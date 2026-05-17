//! AACS [`StreamDecryptor`] adapter — bridges the bluray crate's
//! decryption hook to `oxideav-aacs`.
//!
//! Flow per AACS Common 0.953 + BD-Prerecorded 0.953:
//!   1. Mount the disc.
//!   2. Compute the libbluray-style 20-byte disc ID as
//!      `SHA-1(AACS/Unit_Key_RO.inf bytes)` per libaacs's
//!      `_calc_title_hash` / `aacs_get_disc_id` (with fallback to
//!      `AACS/DUPLICATE/Unit_Key_RO.inf`). No drive query, no AACS
//!      host-authentication handshake — the file is plain-text-
//!      readable through the filesystem.
//!   3. Stream-scan KEYDB.cfg line-by-line for that exact disc ID —
//!      no full-file parse, no memory blow-up.
//!   4. Parse only the matching line via `KeyDb::parse(single_line)`,
//!      apply its VUK or pre-unwrapped Unit Keys to a freshly-opened
//!      `AacsVolume`, verify by trial-decrypting the first .m2ts
//!      Aligned Unit and checking for the BD-AV TS sync pattern.

use crate::decrypt::{DecryptError, StreamDecryptor};
use crate::m2ts::M2TS_PACKET_LEN;
use oxideav_aacs::keydb::KeyDbEntry;
use oxideav_aacs::vuk::disc_id_from_unit_key_file_bytes;
use oxideav_aacs::{AacsVolume, KeyDb, TitleKey};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// AES-128-CBC AACS Aligned Unit length (= 6144 bytes), per AACS
/// Common 0.953 §3.7.
pub const AACS_UNIT_LEN: usize = 6144;

/// Length of the libbluray-style 20-byte KEYDB.cfg disc identifier.
/// Derived as `SHA-1(AACS Volume Identifier)` per
/// [`oxideav_aacs::vuk::disc_id_from_unit_key_file_bytes`].
const DISC_ID_LEN: usize = 20;

/// [`StreamDecryptor`] backed by an [`AacsVolume`] whose CPS Unit
/// title keys have already been unwrapped.
pub struct AacsDecryptor {
    volume: AacsVolume,
}

impl std::fmt::Debug for AacsDecryptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AacsDecryptor")
            .field("cps_units", &self.volume.cps_units.len())
            .finish()
    }
}

impl StreamDecryptor for AacsDecryptor {
    fn decrypt_units(&mut self, buf: &mut [u8], _clip_offset: u64) -> Result<(), DecryptError> {
        if buf.len() % AACS_UNIT_LEN != 0 {
            return Err(DecryptError::new(format!(
                "AacsDecryptor: buffer length {} is not a multiple of {AACS_UNIT_LEN}",
                buf.len()
            )));
        }
        let cps_unit = self
            .volume
            .cps_units
            .first()
            .ok_or_else(|| DecryptError::new("AACS volume has no CPS Units"))?;
        let mut off = 0;
        while off < buf.len() {
            let unit = &mut buf[off..off + AACS_UNIT_LEN];
            let dec = self
                .volume
                .decrypt_unit(cps_unit, unit)
                .map_err(|e| DecryptError::new(e.to_string()))?;
            unit.copy_from_slice(&dec);
            off += AACS_UNIT_LEN;
        }
        Ok(())
    }
}

/// Resolve AACS by querying the drive for the Volume Identifier,
/// hashing it to the libbluray disc_id, and looking up KEYDB.cfg.
/// Returns `Ok(None)` cleanly on every failure path with an
/// actionable stderr line.
pub fn try_resolve_aacs(disc_root: &Path) -> std::io::Result<Option<Box<dyn StreamDecryptor>>> {
    if !disc_root.join("AACS").is_dir() {
        return Ok(None);
    }
    let debug = std::env::var_os("OXIDEAV_AACS_DEBUG").is_some();

    // Step 1 — compute the libbluray disc_id per libaacs's
    // `_calc_title_hash` / `aacs_get_disc_id` convention:
    //
    //   disc_id = SHA-1(bytes_of(AACS/Unit_Key_RO.inf))
    //
    // No drive query, no AACS host-authentication handshake — the
    // BD-ROM Mark Volume Identifier is irrelevant for this lookup.
    // Fallback path is `AACS/DUPLICATE/Unit_Key_RO.inf` (BD-Prerecorded
    // §2.1 — the duplicate copy mirrors content). The other content-
    // type variants (BD-Recordable BDMV / BDAV, HD-DVD) use different
    // files; we don't bother probing them for BD-ROM playback.
    let unit_key_bytes = {
        let primary = disc_root.join("AACS").join("Unit_Key_RO.inf");
        let dup = disc_root
            .join("AACS")
            .join("DUPLICATE")
            .join("Unit_Key_RO.inf");
        match std::fs::read(&primary).or_else(|_| std::fs::read(&dup)) {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "oxideav-bluray: AACS Unit_Key_RO.inf read failed: {e} \
                     (tried {} and {})",
                    primary.display(),
                    dup.display()
                );
                return Ok(None);
            }
        }
    };
    let disc_id = disc_id_from_unit_key_file_bytes(&unit_key_bytes);
    let disc_id_hex = hex_id(&disc_id);
    if debug {
        eprintln!(
            "oxideav-bluray: disc_id = SHA-1(AACS/Unit_Key_RO.inf, {} bytes) = {disc_id_hex}",
            unit_key_bytes.len()
        );
    }

    // Step 2 — locate KEYDB.cfg and stream-scan for ONE matching line.
    let keydb_path = match find_keydb_path() {
        Some(p) => p,
        None => {
            eprintln!(
                "oxideav-bluray: AACS resolution skipped — KEYDB.cfg not found. \
                 Checked: $OXIDEAV_AACS_KEYDB, \
                 ~/Library/Preferences/aacs/KEYDB.cfg (macOS), \
                 $XDG_CONFIG_HOME/aacs/KEYDB.cfg, \
                 $XDG_CONFIG_DIRS/<dir>/aacs/KEYDB.cfg, \
                 ~/.config/aacs/KEYDB.cfg."
            );
            return Ok(None);
        }
    };
    let line = match scan_keydb_for_line(&keydb_path, &disc_id, debug)? {
        Some(l) => l,
        None => {
            eprintln!(
                "oxideav-bluray: no KEYDB.cfg entry for disc ID {disc_id_hex} \
                 (searched {}). Either the disc-ID derivation we use is wrong \
                 for this AACS version (please report the cert head[0..64] \
                 shown with OXIDEAV_AACS_DEBUG=1), or KEYDB.cfg simply lacks \
                 a line for this disc.",
                keydb_path.display()
            );
            return Ok(None);
        }
    };

    // Step 3 — parse the matched line.
    let mini = KeyDb::parse(&line).map_err(std::io::Error::other)?;
    let entry: KeyDbEntry = match mini.entries().next() {
        Some(e) => e.clone(),
        None => {
            eprintln!(
                "oxideav-bluray: matched KEYDB.cfg line for {disc_id_hex} did \
                 not parse cleanly. Line: {line}"
            );
            return Ok(None);
        }
    };
    if debug {
        eprintln!(
            "oxideav-bluray: matched KEYDB entry — vuk {:02X}{:02X}…{:02X}{:02X}, \
             {} pre-unwrapped unit keys",
            entry.vuk.as_bytes()[0],
            entry.vuk.as_bytes()[1],
            entry.vuk.as_bytes()[14],
            entry.vuk.as_bytes()[15],
            entry.unit_keys.len()
        );
    }

    // Step 4 — open AACS volume, apply keys.
    let mut volume = match AacsVolume::open(disc_root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("oxideav-bluray: AACS volume open failed: {e}");
            return Ok(None);
        }
    };
    apply_entry_to_volume(&entry, &mut volume);

    // Step 5 — verify by trial-decrypting the first .m2ts.
    let first_m2ts = match find_first_m2ts(disc_root) {
        Some(p) => p,
        None => {
            eprintln!("oxideav-bluray: no .m2ts file found under BDMV/STREAM/");
            return Ok(None);
        }
    };
    let trial_sample = match std::fs::read(&first_m2ts) {
        Ok(b) if b.len() >= AACS_UNIT_LEN => b[..AACS_UNIT_LEN].to_vec(),
        Ok(b) => {
            eprintln!(
                "oxideav-bluray: {} too short ({} bytes) for one AACS Aligned Unit",
                first_m2ts.display(),
                b.len()
            );
            return Ok(None);
        }
        Err(e) => {
            eprintln!(
                "oxideav-bluray: failed to read {}: {e}",
                first_m2ts.display()
            );
            return Ok(None);
        }
    };

    for cps_idx in 0..volume.cps_units.len() {
        let cps = volume.cps_units[cps_idx];
        if cps.title_key.is_none() {
            continue;
        }
        let mut buf = trial_sample.clone();
        let decrypted = match volume.decrypt_unit(&cps, &buf) {
            Ok(d) => d,
            Err(_) => continue,
        };
        buf.copy_from_slice(&decrypted);
        if looks_like_bdav_ts(&buf) {
            if debug {
                eprintln!(
                    "oxideav-bluray: AACS verified with CPS Unit {} ({} CPS units total)",
                    cps.id,
                    volume.cps_units.len()
                );
            }
            return Ok(Some(Box::new(AacsDecryptor { volume })));
        }
    }

    eprintln!(
        "oxideav-bluray: matched KEYDB entry for {disc_id_hex} but no CPS unit's \
         title key produced a valid BD-AV TS sync pattern. Keys may be stale."
    );
    Ok(None)
}

/// Apply a [`KeyDbEntry`]'s keys to an [`AacsVolume`]. If the entry
/// supplied pre-unwrapped Unit Keys (libbluray extended format), use
/// those directly. Otherwise AES-128-ECB-unwrap each CPS Unit's
/// encrypted title key with the VUK.
fn apply_entry_to_volume(entry: &KeyDbEntry, volume: &mut AacsVolume) {
    if !entry.unit_keys.is_empty() {
        for cps in volume.cps_units.iter_mut() {
            cps.title_key = entry
                .unit_keys
                .iter()
                .find(|(id, _)| *id == cps.id)
                .map(|(_, k)| TitleKey(*k));
        }
    } else {
        let _ = volume.unwrap_title_keys(&entry.vuk);
    }
}

/// Stream-scan `keydb_path` line-by-line for a leading 40-hex disc ID
/// equal to `disc_id`. Streaming = at most one line in memory at a
/// time; the KEYDB file size is unbounded by design.
fn scan_keydb_for_line(
    keydb_path: &Path,
    disc_id: &[u8; DISC_ID_LEN],
    debug: bool,
) -> std::io::Result<Option<String>> {
    let target_hex = hex_id(disc_id);
    let f = std::fs::File::open(keydb_path)?;
    let reader = BufReader::new(f);
    let mut scanned = 0usize;
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with(';') {
            continue;
        }
        scanned += 1;
        let leading = trimmed
            .strip_prefix("0x")
            .or_else(|| trimmed.strip_prefix("0X"))
            .unwrap_or(trimmed);
        if leading.len() < 40 {
            continue;
        }
        if leading[..40].eq_ignore_ascii_case(&target_hex) {
            if debug {
                eprintln!("oxideav-bluray: KEYDB line match after scanning {scanned} lines");
            }
            return Ok(Some(line));
        }
    }
    if debug {
        eprintln!(
            "oxideav-bluray: scanned all {scanned} candidate lines in {}; no match",
            keydb_path.display()
        );
    }
    Ok(None)
}

/// Returns the first KEYDB-search path that exists. Mirrors
/// `KeyDb::load_default`'s order but stops before reading.
fn find_keydb_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("OXIDEAV_AACS_KEYDB") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    #[cfg(target_os = "macos")]
    if let Ok(home) = std::env::var("HOME") {
        let pb = PathBuf::from(home).join("Library/Preferences/aacs/KEYDB.cfg");
        if pb.exists() {
            return Some(pb);
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        let pb = PathBuf::from(xdg).join("aacs/KEYDB.cfg");
        if pb.exists() {
            return Some(pb);
        }
    }
    if let Ok(dirs) = std::env::var("XDG_CONFIG_DIRS") {
        for d in dirs.split(':') {
            if d.is_empty() {
                continue;
            }
            let pb = PathBuf::from(d).join("aacs/KEYDB.cfg");
            if pb.exists() {
                return Some(pb);
            }
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let pb = PathBuf::from(home).join(".config/aacs/KEYDB.cfg");
        if pb.exists() {
            return Some(pb);
        }
    }
    None
}

fn hex_id(id: &[u8; DISC_ID_LEN]) -> String {
    id.iter().map(|b| format!("{b:02X}")).collect()
}

/// Walk `BDMV/STREAM/` for the lowest-numbered `*.m2ts` file.
fn find_first_m2ts(disc_root: &Path) -> Option<std::path::PathBuf> {
    let stream_dir = disc_root.join("BDMV").join("STREAM");
    let mut entries: Vec<_> = std::fs::read_dir(&stream_dir).ok()?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    entries
        .into_iter()
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("m2ts"))
}

/// Heuristic: in a decrypted BD-AV Aligned Unit, each 192-byte source
/// packet is `4-byte TP_extra_header + 188-byte TS packet`, and a TS
/// packet begins with `0x47`. Check every packet boundary within the
/// 6144-byte unit (32 packets).
fn looks_like_bdav_ts(unit: &[u8]) -> bool {
    if unit.len() < AACS_UNIT_LEN {
        return false;
    }
    const SYNC: u8 = 0x47;
    let n_packets = AACS_UNIT_LEN / M2TS_PACKET_LEN;
    (0..n_packets).all(|i| unit[i * M2TS_PACKET_LEN + 4] == SYNC)
}
