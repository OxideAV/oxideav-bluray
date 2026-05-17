//! AACS [`StreamDecryptor`] adapter — bridges the bluray crate's
//! decryption hook to `oxideav-aacs`. Built behind the `aacs` cargo
//! feature so consumers that only need BD-R / unprotected playback
//! can skip the dep.
//!
//! Usage flow per AACS Common 0.953 + BD-Prerecorded 0.953:
//!   1. Mount the disc + locate `AACS/` directory.
//!   2. Open `AacsVolume` (parses `MKB_RO.inf` + `Unit_Key_RO.inf`).
//!   3. Resolve a VUK from `KEYDB.cfg`. The KEYDB format keys entries
//!      by the 20-byte Content-Certificate disc ID, which we don't
//!      parse yet (the `.cer` file is out of scope for the current
//!      aacs round). Instead we try each KeyDb entry's VUK in turn,
//!      unwrap the first CPS Unit's title key with it, attempt to
//!      decrypt the first Aligned Unit of the first .m2ts clip, and
//!      check for the BD-AV TS sync-byte pattern (`0x47` every 192
//!      bytes within the unit, post-decryption). The first VUK that
//!      produces a valid sync pattern wins.
//!   4. Wrap the resulting `AacsVolume` (with unwrapped title keys)
//!      as an `AacsDecryptor` and hand it to `Disc::open_title`.

use crate::decrypt::{DecryptError, StreamDecryptor};
use crate::m2ts::M2TS_PACKET_LEN;
use oxideav_aacs::{AacsVolume, KeyDb};
use std::path::Path;

/// AES-128-CBC AACS Aligned Unit length (= 6144 bytes), per AACS
/// Common 0.953 §3.7. Mirrors the constant our `m2ts` module + the
/// `oxideav_aacs` crate define independently.
pub const AACS_UNIT_LEN: usize = 6144;

/// [`StreamDecryptor`] backed by an [`AacsVolume`] whose CPS Unit
/// title keys have already been unwrapped. Phase-1 BD-ROM playback
/// uses CPS Unit 1 (the "main" feature unit) for every clip; multi-
/// CPS-Unit titles (e.g. studio bonus content with separate keys
/// per playlist) will surface as a Phase-2 follow-up that needs the
/// playlist's CPS Unit assignments threaded through `TitleSource`.
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

/// Try every VUK in `keydb` against the first CPS unit; pick the
/// first whose decryption of one Aligned Unit of `STREAM/<first_clip>.m2ts`
/// produces a valid BD-AV TS sync pattern (`0x47` every 192 bytes).
///
/// Returns `Ok(Some(decryptor))` on success, `Ok(None)` if no VUK
/// matched (caller falls back to Identity, which usually means the
/// pipeline will fail to find packet sync later — that's the user's
/// cue to fix their KEYDB.cfg). `Err` only on AACS-file-parse errors.
pub fn try_resolve_aacs(disc_root: &Path) -> std::io::Result<Option<Box<dyn StreamDecryptor>>> {
    // Disc has no AACS/ directory ⇒ not protected; skip cleanly.
    if !disc_root.join("AACS").is_dir() {
        return Ok(None);
    }

    let keydb = match KeyDb::load_default() {
        Ok(k) if !k.is_empty() => k,
        _ => return Ok(None),
    };

    // Early-bail: confirm AACS metadata at least parses before we
    // spin up the per-VUK trial loop. Discarded; each loop iteration
    // re-opens because unwrap_title_keys mutates the volume in place
    // and AacsVolume isn't Clone.
    if AacsVolume::open(disc_root).is_err() {
        return Ok(None);
    }

    // First .m2ts clip on disc — used as the trial-decrypt oracle.
    let first_m2ts = match find_first_m2ts(disc_root) {
        Some(p) => p,
        None => return Ok(None),
    };
    let trial_sample = match std::fs::read(&first_m2ts) {
        Ok(b) if b.len() >= AACS_UNIT_LEN => b[..AACS_UNIT_LEN].to_vec(),
        _ => return Ok(None),
    };

    for entry in keydb.entries() {
        let mut v_try = match AacsVolume::open(disc_root) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        if v_try.unwrap_title_keys(&entry.vuk).is_err() {
            continue;
        }
        let cps = match v_try.cps_units.first() {
            Some(c) => c,
            None => continue,
        };
        let mut buf = trial_sample.clone();
        let decrypted = match v_try.decrypt_unit(cps, &buf) {
            Ok(d) => d,
            Err(_) => continue,
        };
        buf.copy_from_slice(&decrypted);
        if looks_like_bdav_ts(&buf) {
            return Ok(Some(Box::new(AacsDecryptor { volume: v_try })));
        }
    }

    Ok(None)
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

/// Heuristic: in a decrypted BD-AV Aligned Unit, each 192-byte
/// source packet is `4-byte TP_extra_header + 188-byte TS packet`,
/// and a TS packet begins with the `0x47` sync byte. Check that
/// every packet boundary within the 6144-byte unit (32 packets)
/// carries the sync byte at offset `4 + i * 192`.
fn looks_like_bdav_ts(unit: &[u8]) -> bool {
    if unit.len() < AACS_UNIT_LEN {
        return false;
    }
    const SYNC: u8 = 0x47;
    let n_packets = AACS_UNIT_LEN / M2TS_PACKET_LEN;
    (0..n_packets).all(|i| unit[i * M2TS_PACKET_LEN + 4] == SYNC)
}
