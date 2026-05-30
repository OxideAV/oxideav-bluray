//! AACS [`StreamDecryptor`] adapter — bridges the bluray crate's
//! decryption hook to `oxideav-aacs`.
//!
//! Flow per AACS Common 0.953 + BD-Prerecorded 0.953:
//!   1. Mount the disc.
//!   2. Compute the 20-byte KEYDB.cfg disc-identifier as
//!      `SHA-1(AACS/Unit_Key_RO.inf bytes)` (with fallback to
//!      `AACS/DUPLICATE/Unit_Key_RO.inf`). No drive query, no AACS
//!      host-authentication handshake — the file is plain-text-
//!      readable through the filesystem.
//!   3. Resolve a VUK by walking the cascade in
//!      [`resolve_aacs_entry`]:
//!      (0) Env-var override — `OXIDEAV_AACS_VUK=<32-hex>` short-
//!      circuits with a complete VUK; `OXIDEAV_AACS_MEDIA_KEY=<32-hex>`
//!      combines with VID (from drive or `OXIDEAV_AACS_VOLUME_ID`) to
//!      compute `K_vu = AES-G(K_m, ID_v)`. Either lets a Type-4
//!      MKB whose KCD path isn't wired (or any disc whose `K_m` was
//!      computed externally) play immediately.
//!      (a) Stream-scan KEYDB.cfg line-by-line for the disc ID.
//!      (b) On miss, stream-scan the local VUK cache
//!      (`~/.cache/oxideav/vuk-cache.cfg`) — same line format,
//!      so the cache is shareable with KEYDB.cfg by `cat`.
//!      (c) On miss, attempt *online* derivation (gated behind the
//!      `aacs-online` cargo feature): read the AACS Volume Identifier
//!      via MMC `READ DISC STRUCTURE`
//!      ([`crate::drive::read_volume_id`]) and walk the disc's MKB
//!      Subset-Difference tree with each `| DK |` Device Key parsed
//!      from KEYDB.cfg, via
//!      [`oxideav_aacs::AacsVolume::derive_vuk_from_device_key`]. A
//!      successful derivation is written back to the cache with an
//!      `online-<RFC3339>` provenance stamp.
//!   4. Apply the resolved VUK (or its pre-unwrapped Unit Keys) to a
//!      freshly-opened `AacsVolume`, verify by trial-decrypting the
//!      first `.m2ts` Aligned Unit and checking for the BD-AV TS sync
//!      pattern.

use crate::decrypt::{DecryptError, StreamDecryptor};
use crate::m2ts::M2TS_PACKET_LEN;
#[cfg(feature = "aacs-online")]
use oxideav_aacs::keydb::DeviceKeyRecord;
use oxideav_aacs::keydb::KeyDbEntry;
use oxideav_aacs::vuk::{derive_vuk, disc_id_from_unit_key_file_bytes};
#[cfg(feature = "aacs-online")]
use oxideav_aacs::DeviceKey;
use oxideav_aacs::Vuk;
use oxideav_aacs::{AacsVolume, KeyDb, TitleKey};
#[cfg(any(test, feature = "aacs-online"))]
use std::io::Write;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// AES-128-CBC AACS Aligned Unit length (= 6144 bytes), per AACS
/// Common 0.953 §3.7.
pub const AACS_UNIT_LEN: usize = 6144;

/// Length of the 20-byte KEYDB.cfg disc identifier.
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

/// Resolve AACS by computing the 20-byte KEYDB.cfg disc-identifier
/// from `AACS/Unit_Key_RO.inf` and looking it up in KEYDB.cfg.
/// Returns `Ok(None)` cleanly on every failure path with an
/// actionable stderr line.
pub fn try_resolve_aacs(disc_root: &Path) -> std::io::Result<Option<Box<dyn StreamDecryptor>>> {
    if !disc_root.join("AACS").is_dir() {
        return Ok(None);
    }
    let debug = std::env::var_os("OXIDEAV_AACS_DEBUG").is_some();

    // Step 1 — compute the 20-byte KEYDB.cfg disc-identifier:
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

    // Step 2 — open the AACS volume eagerly so the online fallback can
    // hand its MKB to `AacsVolume::derive_vuk_from_device_key`. Opening
    // also runs the MKB / Unit_Key_RO.inf parsers, so an unreadable disc
    // bails here rather than after we've burned a KEYDB scan.
    let mut volume = match AacsVolume::open(disc_root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("oxideav-bluray: AACS volume open failed: {e}");
            return Ok(None);
        }
    };

    // Step 3 — locate KEYDB.cfg and try the legacy-entry lookup first;
    // miss → cache; miss → online; miss → hard fail.
    let keydb_path = find_keydb_path();
    let resolved_entry = match resolve_aacs_entry(
        keydb_path.as_deref(),
        &disc_id,
        &disc_id_hex,
        disc_root,
        &volume,
        debug,
    )? {
        Some(e) => e,
        None => return Ok(None),
    };
    let entry = resolved_entry.entry;
    if debug {
        eprintln!(
            "oxideav-bluray: matched VUK via {} — vuk {:02X}{:02X}…{:02X}{:02X}, \
             {} pre-unwrapped unit keys",
            resolved_entry.source.label(),
            entry.vuk.as_bytes()[0],
            entry.vuk.as_bytes()[1],
            entry.vuk.as_bytes()[14],
            entry.vuk.as_bytes()[15],
            entry.unit_keys.len()
        );
    }

    // Step 4 — apply keys to the volume.
    apply_entry_to_volume(&entry, &mut volume);

    // Step 5 — verify by trial-decrypting the first .m2ts.
    let first_m2ts = match find_first_m2ts(disc_root) {
        Some(p) => p,
        None => {
            eprintln!("oxideav-bluray: no .m2ts file found under BDMV/STREAM/");
            return Ok(None);
        }
    };
    // Only need the first AACS Aligned Unit (6144 bytes) for the
    // trial decrypt — m2ts files on commercial discs run into tens of
    // gigabytes, so a `std::fs::read` of the whole file would buffer
    // the entire title-1 stream in RAM and (on flaky optical drives)
    // tends to hit a transient sector read error long before the EOF.
    let trial_sample = {
        use std::io::Read;
        let mut f = match std::fs::File::open(&first_m2ts) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("oxideav-bluray: open {} failed: {e}", first_m2ts.display());
                return Ok(None);
            }
        };
        let mut buf = vec![0u8; AACS_UNIT_LEN];
        if let Err(e) = f.read_exact(&mut buf) {
            eprintln!(
                "oxideav-bluray: short read on {}: {e}",
                first_m2ts.display()
            );
            return Ok(None);
        }
        buf
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
            // Write-back: only the online path produces a fresh VUK
            // worth caching; KEYDB / cache hits are already on disk.
            #[cfg(feature = "aacs-online")]
            if matches!(resolved_entry.source, ResolvedSource::Online) {
                if let Err(e) = write_vuk_cache_entry(&disc_id, &entry.vuk, "online") {
                    eprintln!("oxideav-bluray: VUK cache write-back failed (continuing): {e}");
                } else if debug {
                    eprintln!(
                        "oxideav-bluray: wrote VUK for {disc_id_hex} back to {}",
                        default_cache_path().display()
                    );
                }
            }
            return Ok(Some(Box::new(AacsDecryptor { volume })));
        }
    }

    eprintln!(
        "oxideav-bluray: resolved VUK for {disc_id_hex} via {} but no CPS unit's \
         title key produced a valid BD-AV TS sync pattern. Keys may be stale.",
        resolved_entry.source.label()
    );
    Ok(None)
}

// ---------------------------------------------------------------------
// VUK lookup cascade — KEYDB legacy entry → on-disk cache → online
// ---------------------------------------------------------------------

/// Where a resolved [`KeyDbEntry`] came from. Drives diagnostic output
/// and whether to write-back to the cache on a successful trial decrypt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedSource {
    /// `OXIDEAV_AACS_VUK` / `OXIDEAV_AACS_MEDIA_KEY` env-var override.
    EnvOverride,
    /// `KEYDB.cfg` legacy `<DISCID>=V<VUK>` entry.
    Keydb,
    /// Local on-disk cache (`~/.cache/oxideav/vuk-cache.cfg`).
    Cache,
    /// Online derivation via the disc's optical drive (MMC READ DISC
    /// STRUCTURE for the AACS Volume Identifier) + an MKB Subset-
    /// Difference walk against a Device Key parsed from
    /// `KEYDB.cfg`'s extended `| DK |` records. Only reachable with
    /// the `aacs-online` feature on.
    #[cfg(feature = "aacs-online")]
    Online,
}

impl ResolvedSource {
    fn label(self) -> &'static str {
        match self {
            ResolvedSource::EnvOverride => "env-var override",
            ResolvedSource::Keydb => "KEYDB.cfg",
            ResolvedSource::Cache => "local VUK cache",
            #[cfg(feature = "aacs-online")]
            ResolvedSource::Online => "online derivation",
        }
    }
}

struct ResolvedEntry {
    entry: KeyDbEntry,
    source: ResolvedSource,
}

/// Try, in order: KEYDB.cfg legacy line scan, local VUK cache,
/// online derivation. Returns the first hit (without trial-decrypt
/// verification — the caller still has to walk every CPS Unit). Emits
/// `Ok(None)` only when every cascade step has been exhausted with a
/// stderr diagnostic explaining why.
fn resolve_aacs_entry(
    keydb_path: Option<&Path>,
    disc_id: &[u8; DISC_ID_LEN],
    disc_id_hex: &str,
    disc_root: &Path,
    volume: &AacsVolume,
    debug: bool,
) -> std::io::Result<Option<ResolvedEntry>> {
    // (0) Env-var override. Lets a caller plug in a complete VUK or a
    //     Media Key (K_m) computed externally (libaacs, another tool,
    //     a Type-4 KCD-Mark reader). Unblocks Type-4 BD-Prerecorded
    //     discs whose Subset-Difference walk yields the Media Key
    //     *precursor* K_mp rather than K_m, since the cascade's online
    //     derivation expects K_m to verify directly against the MKB's
    //     Verify-Media-Key record.
    if let Some(entry) = entry_from_env(disc_id, disc_root, debug)? {
        return Ok(Some(ResolvedEntry {
            entry,
            source: ResolvedSource::EnvOverride,
        }));
    }

    // (1) Legacy KEYDB.cfg `<DISCID>=V<VUK>` line scan.
    if let Some(path) = keydb_path {
        if let Some(line) = scan_keydb_for_line(path, disc_id, debug)? {
            match parse_single_keydb_line(&line) {
                Some(e) => {
                    return Ok(Some(ResolvedEntry {
                        entry: e,
                        source: ResolvedSource::Keydb,
                    }))
                }
                None => eprintln!(
                    "oxideav-bluray: matched KEYDB.cfg line for {disc_id_hex} did \
                     not parse cleanly. Line: {line}"
                ),
            }
        } else if debug {
            eprintln!(
                "oxideav-bluray: KEYDB.cfg miss for {disc_id_hex} ({}), falling \
                 through to cache + online cascade",
                path.display()
            );
        }
    } else if debug {
        eprintln!(
            "oxideav-bluray: KEYDB.cfg not found; falling through to cache + \
             online cascade"
        );
    }

    // (2) Local on-disk VUK cache.
    let cache_path = default_cache_path();
    if cache_path.exists() {
        if let Some(line) = scan_keydb_for_line(&cache_path, disc_id, debug)? {
            if let Some(e) = parse_single_keydb_line(&line) {
                return Ok(Some(ResolvedEntry {
                    entry: e,
                    source: ResolvedSource::Cache,
                }));
            } else {
                eprintln!(
                    "oxideav-bluray: matched cache line for {disc_id_hex} did \
                     not parse cleanly. Line: {line}"
                );
            }
        } else if debug {
            eprintln!(
                "oxideav-bluray: cache miss for {disc_id_hex} ({})",
                cache_path.display()
            );
        }
    } else if debug {
        eprintln!(
            "oxideav-bluray: no cache file at {} yet",
            cache_path.display()
        );
    }

    // (3) Online derivation — gated by `aacs-online`. Requires the full
    // KEYDB.cfg (for `| DK |` records) + a drive query for the AACS
    // Volume Identifier.
    #[cfg(feature = "aacs-online")]
    {
        let _ = (disc_root, volume); // touched below
        if let Some(path) = keydb_path {
            match try_online_vuk(path, disc_id_hex, disc_root, volume, debug)? {
                Some(vuk) => {
                    let entry = KeyDbEntry {
                        disc_id: *disc_id,
                        vuk,
                        label: Some("online-derived".to_string()),
                        unit_keys: Vec::new(),
                    };
                    return Ok(Some(ResolvedEntry {
                        entry,
                        source: ResolvedSource::Online,
                    }));
                }
                None => {
                    eprintln!(
                        "oxideav-bluray: online VUK derivation failed for \
                         {disc_id_hex} (no Device Key in KEYDB.cfg produced a \
                         VUK that satisfies the MKB Verify-Media-Key record). \
                         See OXIDEAV_AACS_DEBUG=1 for per-DK try-count."
                    );
                }
            }
        } else {
            eprintln!(
                "oxideav-bluray: online VUK derivation skipped — KEYDB.cfg not \
                 found so no | DK | Device Keys are available to walk the MKB."
            );
        }
    }
    #[cfg(not(feature = "aacs-online"))]
    {
        let _ = (disc_root, volume);
        eprintln!(
            "oxideav-bluray: online VUK derivation skipped — built without \
             the `aacs-online` feature."
        );
    }

    eprintln!(
        "oxideav-bluray: no AACS VUK for disc ID {disc_id_hex} — exhausted \
         KEYDB.cfg + cache{}. Add an entry to KEYDB.cfg, or build with \
         `--features aacs-online` and provide a `| DK | ...` Device Key \
         record so the disc's MKB can be walked online.",
        if cfg!(feature = "aacs-online") {
            " + online derivation"
        } else {
            ""
        }
    );
    Ok(None)
}

/// Resolve a VUK from environment variables, bypassing the rest of
/// the cascade. Two shapes are accepted, in priority order:
///
/// * `OXIDEAV_AACS_VUK=<32-hex>` — supplies the 16-byte Volume Unique
///   Key directly. Same shape KEYDB.cfg's legacy `V` token holds.
///
/// * `OXIDEAV_AACS_MEDIA_KEY=<32-hex>` — supplies the 16-byte Media
///   Key `K_m` (post-KCD). Combined with the Volume Identifier (from
///   `OXIDEAV_AACS_VOLUME_ID` if set, otherwise read from the optical
///   drive via [`crate::drive::read_volume_id`]) to compute
///   `K_vu = AES-G(K_m, ID_v)`.
///
/// Both override paths skip the disc-id-keyed lookups in KEYDB.cfg /
/// cache and the MKB walk — so they unblock Type-4 BD-Prerecorded
/// discs whose Subset-Difference walk yields `K_mp` rather than `K_m`
/// (the KCD-Mark drive read for `K_m = AES-G(K_mp, KCD)` is not yet
/// wired), and any disc whose `K_m` was computed by an external tool
/// (libaacs, etc.).
///
/// `disc_id` is accepted for symmetry with the keydb lookups but is
/// not used — the env override applies to whatever disc is mounted.
fn entry_from_env(
    disc_id: &[u8; DISC_ID_LEN],
    disc_root: &Path,
    debug: bool,
) -> std::io::Result<Option<KeyDbEntry>> {
    let _ = disc_root; // silence unused on builds without aacs-online
    if let Ok(s) = std::env::var("OXIDEAV_AACS_VUK") {
        let vuk_bytes = match parse_hex16_env("OXIDEAV_AACS_VUK", &s) {
            Some(b) => b,
            None => return Ok(None),
        };
        if debug {
            eprintln!(
                "oxideav-bluray: OXIDEAV_AACS_VUK env override applied — vuk \
                 {:02X}{:02X}…{:02X}{:02X}",
                vuk_bytes[0], vuk_bytes[1], vuk_bytes[14], vuk_bytes[15]
            );
        }
        return Ok(Some(KeyDbEntry {
            disc_id: *disc_id,
            vuk: Vuk::from_bytes(vuk_bytes),
            label: Some("OXIDEAV_AACS_VUK env override".to_string()),
            unit_keys: Vec::new(),
        }));
    }

    if let Ok(km_s) = std::env::var("OXIDEAV_AACS_MEDIA_KEY") {
        let km = match parse_hex16_env("OXIDEAV_AACS_MEDIA_KEY", &km_s) {
            Some(b) => b,
            None => return Ok(None),
        };
        // VID source: env override first, then drive query. The drive
        // query is only available under the `aacs-online` feature; on
        // a feature-free build, fall through if the env override
        // isn't set.
        let vid_opt: Option<[u8; 16]> = if let Ok(vid_s) = std::env::var("OXIDEAV_AACS_VOLUME_ID") {
            parse_hex16_env("OXIDEAV_AACS_VOLUME_ID", &vid_s)
        } else {
            #[cfg(feature = "aacs-online")]
            {
                match crate::drive::read_volume_id(disc_root) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        eprintln!(
                            "oxideav-bluray: OXIDEAV_AACS_MEDIA_KEY env set but \
                             drive Volume Identifier query failed: {e}. Set \
                             OXIDEAV_AACS_VOLUME_ID=<32-hex chars> to supply it \
                             manually."
                        );
                        None
                    }
                }
            }
            #[cfg(not(feature = "aacs-online"))]
            {
                eprintln!(
                    "oxideav-bluray: OXIDEAV_AACS_MEDIA_KEY env set without \
                     OXIDEAV_AACS_VOLUME_ID, and `aacs-online` feature is off \
                     so no drive query is available. Either set the env var \
                     or rebuild with --features aacs-online."
                );
                None
            }
        };
        let Some(vid) = vid_opt else {
            return Ok(None);
        };
        let vuk = derive_vuk(&km, &vid);
        if debug {
            let vb = vuk.as_bytes();
            eprintln!(
                "oxideav-bluray: OXIDEAV_AACS_MEDIA_KEY env override — \
                 K_vu = AES-G(K_m, ID_v) = {:02X}{:02X}…{:02X}{:02X}",
                vb[0], vb[1], vb[14], vb[15]
            );
        }
        return Ok(Some(KeyDbEntry {
            disc_id: *disc_id,
            vuk,
            label: Some("OXIDEAV_AACS_MEDIA_KEY env override".to_string()),
            unit_keys: Vec::new(),
        }));
    }
    Ok(None)
}

/// Parse a 16-byte (32-hex) env-var value with `0x` prefix tolerated.
/// Emits a stderr diagnostic and returns `None` on a malformed value
/// so the caller can fall through to the next cascade step.
fn parse_hex16_env(name: &str, s: &str) -> Option<[u8; 16]> {
    let trimmed = s
        .trim()
        .strip_prefix("0x")
        .or_else(|| s.trim().strip_prefix("0X"))
        .unwrap_or(s.trim());
    if trimmed.len() != 32 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        eprintln!(
            "oxideav-bluray: {name} env override is not 32 hex characters: {s:?} — \
             ignoring"
        );
        return None;
    }
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = u8::from_str_radix(&trimmed[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Parse a single `<DISCID>=V<VUK>` KEYDB.cfg line into a
/// [`KeyDbEntry`]. Returns `None` if the line fails to parse cleanly.
fn parse_single_keydb_line(line: &str) -> Option<KeyDbEntry> {
    let mini = KeyDb::parse(line).ok()?;
    let out = mini.entries().next().cloned();
    out
}

// ---------------------------------------------------------------------
// Online VUK derivation
// ---------------------------------------------------------------------

/// Walk every `| DK |` Device Key in `keydb_path` against the disc's
/// MKB Subset-Difference tree until one produces a Media Key whose
/// `Verify-Media-Key` record matches.
///
/// This is the standard offline-walk part of the AACS "online" VUK
/// derivation: AACS LA-issued Device Keys are static once burned into
/// a player, so the "online" qualifier refers to the fact that we
/// have to *talk to the drive* for the Volume Identifier — the MKB
/// walk itself runs entirely locally against the disc's own
/// `AACS/MKB_RO.inf` file. The drive query is gated by the same
/// `aacs-online` cargo feature so headless / CI builds without an
/// optical drive can opt out (the offline KEYDB.cfg + cache cascade
/// stays available).
#[cfg(feature = "aacs-online")]
fn try_online_vuk(
    keydb_path: &Path,
    disc_id_hex: &str,
    disc_root: &Path,
    volume: &AacsVolume,
    debug: bool,
) -> std::io::Result<Option<Vuk>> {
    // Parse the full KEYDB.cfg up-front so we can iterate `| DK |`
    // records. Streaming the file line-by-line again here wouldn't
    // help: each DK is on one line, but the parser also folds in
    // surrounding `; comments` and depends on whole-line `|`-leader
    // dispatch logic that's much cleaner to reuse via `KeyDb::parse`.
    let kdb_text = std::fs::read_to_string(keydb_path)?;
    let kdb = KeyDb::parse(&kdb_text).map_err(std::io::Error::other)?;
    let dks = kdb.device_keys();
    if dks.is_empty() {
        if debug {
            eprintln!(
                "oxideav-bluray: KEYDB.cfg has no `| DK | ...` records; \
                 nothing to walk against the MKB for {disc_id_hex}"
            );
        }
        return Ok(None);
    }

    // Drive-side: fetch the 16-byte AACS Volume Identifier via MMC
    // READ DISC STRUCTURE (or the `OXIDEAV_AACS_VOLUME_ID` env
    // override for testing). The drive query is what makes this path
    // "online".
    let volume_id = match crate::drive::read_volume_id(disc_root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "oxideav-bluray: online VUK derivation: drive Volume \
                 Identifier query failed for {disc_id_hex}: {e}"
            );
            return Ok(None);
        }
    };
    if debug {
        eprintln!(
            "oxideav-bluray: online VUK derivation — Volume ID = {}, walking \
             MKB with {} candidate Device Key(s)",
            volume_id
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<String>(),
            dks.len()
        );
    }

    // Walk each DK against the MKB. The first one whose Media Key
    // verifies wins; the rest are tried only if earlier DKs are
    // revoked by the MKB's subset-difference records.
    for (i, dk) in dks.iter().enumerate() {
        let device_key = match device_key_from_record(dk) {
            Some(k) => k,
            None => {
                if debug {
                    eprintln!("oxideav-bluray: DK[{i}] skipped — unparseable uv/mask");
                }
                continue;
            }
        };
        match volume.derive_vuk_from_device_key(&device_key, &volume_id) {
            Ok(vuk) => {
                if debug {
                    eprintln!(
                        "oxideav-bluray: DK[{i}] produced a VUK that survived \
                         the MKB Verify-Media-Key check"
                    );
                }
                return Ok(Some(vuk));
            }
            Err(e) => {
                if debug {
                    eprintln!("oxideav-bluray: DK[{i}] rejected: {e}");
                }
            }
        }
    }
    Ok(None)
}

/// Map a KEYDB.cfg `| DK |` record into the
/// [`oxideav_aacs::DeviceKey`] shape `AacsVolume::derive_vuk_from_device_key`
/// expects.
///
/// `DeviceKeyRecord` carries the `(key_uv, key_u_mask_shift)` pair as
/// raw bytes; `DeviceKey` wants `uv: u32` plus separate `u_mask_zero_bits`
/// and `v_mask_zero_bits` counters. We synthesise the `v_mask` count
/// from the trailing zeros of `uv` (`v` mask is at most as deep as the
/// node), matching what
/// `AacsVolume::derive_vuk_from_device_key`'s subset-difference walker
/// reads back out (`d_node = (uv << 1) | 1`; `target_v_mask_zero_bits
/// = sd.uv.trailing_zeros()` per the MKB record). Returns `None` if
/// the record's `key_uv` bytes don't fit a `u32`.
#[cfg(feature = "aacs-online")]
fn device_key_from_record(rec: &DeviceKeyRecord) -> Option<DeviceKey> {
    let uv = u32::from_be_bytes(rec.key_uv);
    let u_mask_zero_bits = rec.key_u_mask_shift;
    // `v_mask` zero-bit count = trailing zeros of `uv`, since the v
    // half of the node identifier is whatever bits remain below the
    // u-mask boundary (BD-AACS Common §3.2.3 uv encoding). Capped at
    // the spec's 32-bit field width.
    let v_mask_zero_bits = uv.trailing_zeros().min(32) as u8;
    Some(DeviceKey {
        key: rec.device_key,
        uv,
        u_mask_zero_bits,
        v_mask_zero_bits,
    })
}

// ---------------------------------------------------------------------
// VUK cache (`~/.cache/oxideav/vuk-cache.cfg`)
// ---------------------------------------------------------------------

/// Resolve the on-disk path of the local VUK cache. Override with
/// `OXIDEAV_AACS_VUK_CACHE=<file>`; otherwise
/// `${XDG_CACHE_HOME:-${HOME}/.cache}/oxideav/vuk-cache.cfg`.
fn default_cache_path() -> PathBuf {
    if let Ok(p) = std::env::var("OXIDEAV_AACS_VUK_CACHE") {
        return PathBuf::from(p);
    }
    let base = if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".cache")
    } else {
        // Last-ditch — write to CWD-local hidden file. Better than
        // panicking; the cache is an optimisation, not a hard
        // dependency.
        PathBuf::from(".oxideav-cache")
    };
    base.join("oxideav").join("vuk-cache.cfg")
}

/// Append `<DISCID>=V<VUK> | <provenance>-<RFC3339>` to the VUK
/// cache. Creates the parent directory if necessary. Idempotent: if
/// the same `<DISCID>=` line already exists in the cache, this is a
/// no-op (we don't dedupe by VUK — a re-derivation that produces a
/// different VUK is much more interesting as a fresh line than a
/// silent overwrite).
#[cfg(any(test, feature = "aacs-online"))]
fn write_vuk_cache_entry(
    disc_id: &[u8; DISC_ID_LEN],
    vuk: &Vuk,
    provenance: &str,
) -> std::io::Result<()> {
    let path = default_cache_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let disc_hex = hex_id(disc_id);

    // Idempotency: skip if the file already has a line beginning with
    // this disc id.
    if path.exists() {
        let f = std::fs::File::open(&path)?;
        for line in BufReader::new(f).lines() {
            let line = line?;
            let trimmed = line.trim_start();
            let stripped = trimmed
                .strip_prefix("0x")
                .or_else(|| trimmed.strip_prefix("0X"))
                .unwrap_or(trimmed);
            if stripped.len() >= 40 && stripped[..40].eq_ignore_ascii_case(&disc_hex) {
                return Ok(());
            }
        }
    }

    let vuk_hex: String = vuk.as_bytes().iter().map(|b| format!("{b:02X}")).collect();
    let stamp = rfc3339_now();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{disc_hex} = V {vuk_hex} | {provenance}-{stamp}")?;
    Ok(())
}

/// Tiny RFC 3339 / ISO-8601 "now" formatter — no `chrono` /
/// `time` dep just for one provenance stamp. Granularity = seconds,
/// always UTC ("Z" suffix).
#[cfg(any(test, feature = "aacs-online"))]
fn rfc3339_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Howard Hinnant's days-from-civil inversion, narrowed to Unix
/// seconds → (y, mo, d, h, mi, s) tuple. Civil calendar Gregorian
/// proleptic; valid from 1970-01-01 onwards (overflows in year ~5870
/// AD, well past any reasonable cache stamp).
#[cfg(any(test, feature = "aacs-online"))]
fn civil_from_unix(unix_secs: u64) -> (i32, u8, u8, u8, u8, u8) {
    let secs_per_day = 86_400u64;
    let days = (unix_secs / secs_per_day) as i64;
    let sod = unix_secs % secs_per_day;
    let h = (sod / 3600) as u8;
    let mi = ((sod % 3600) / 60) as u8;
    let s = (sod % 60) as u8;
    // Howard Hinnant — `date.h` civil_from_days, public domain.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y_int = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u8;
    let y = (y_int + if m <= 2 { 1 } else { 0 }) as i32;
    (y, m, d, h, mi, s)
}

/// Apply a [`KeyDbEntry`]'s keys to an [`AacsVolume`]. If the entry
/// supplied pre-unwrapped Unit Keys (KEYDB.cfg extended format), use
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

#[cfg(test)]
mod tests {
    //! These tests cover the lookup-cascade plumbing in isolation —
    //! the legacy line scanner, the VUK cache reader / writer, and
    //! the civil-from-unix helper. We deliberately don't synthesise a
    //! complete encrypted BD-ROM fixture here: that work lives in
    //! `tests/synthetic_disc.rs` and the AACS-crate's own round-trip
    //! suite. What we *do* verify is:
    //!
    //!   * a cache hit produces a parseable `KeyDbEntry`;
    //!   * cache write-back is idempotent (re-writing the same disc
    //!     ID does not append a duplicate line);
    //!   * the date stamp formatter agrees with the Unix epoch +
    //!     a handful of round-number anchors;
    //!   * the DeviceKeyRecord → DeviceKey adapter preserves the
    //!     `uv` / mask-shift fields the MKB walker reads back out.
    use super::*;
    use std::sync::Mutex;

    /// The cache path resolver reads `OXIDEAV_AACS_VUK_CACHE` /
    /// `XDG_CACHE_HOME` / `HOME` from the process environment, so the
    /// tests that exercise it have to serialise themselves to avoid
    /// trampling each other's overrides.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn fresh_tmp_cache() -> (tempdir_lite::TempDir, PathBuf) {
        let dir = tempdir_lite::TempDir::new();
        let path = dir.path().join("vuk-cache.cfg");
        (dir, path)
    }

    #[test]
    fn cache_roundtrip_parses_back_via_keydb() {
        let _g = ENV_LOCK.lock().unwrap();
        let (_dir, path) = fresh_tmp_cache();
        let _restore = EnvOverride::set("OXIDEAV_AACS_VUK_CACHE", path.to_str().unwrap());

        let disc_id = [0xAAu8; DISC_ID_LEN];
        let vuk = Vuk::from_bytes([0x42u8; 16]);
        write_vuk_cache_entry(&disc_id, &vuk, "online").expect("write");
        assert!(path.exists());

        // Stream-scan finds the line we just wrote.
        let line = scan_keydb_for_line(&path, &disc_id, false)
            .expect("scan")
            .expect("hit");
        let entry = parse_single_keydb_line(&line).expect("parse");
        assert_eq!(entry.disc_id, disc_id);
        assert_eq!(entry.vuk.as_bytes(), vuk.as_bytes());
    }

    #[test]
    fn cache_write_is_idempotent_per_disc_id() {
        let _g = ENV_LOCK.lock().unwrap();
        let (_dir, path) = fresh_tmp_cache();
        let _restore = EnvOverride::set("OXIDEAV_AACS_VUK_CACHE", path.to_str().unwrap());

        let disc_id = [0xCDu8; DISC_ID_LEN];
        let vuk = Vuk::from_bytes([0x11u8; 16]);
        write_vuk_cache_entry(&disc_id, &vuk, "online").expect("first write");
        write_vuk_cache_entry(&disc_id, &vuk, "online").expect("second write (idempotent)");

        let body = std::fs::read_to_string(&path).expect("read back");
        let lines = body.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(lines, 1, "duplicate line should not be appended");
    }

    #[test]
    fn cache_miss_for_different_disc_id_returns_none() {
        let _g = ENV_LOCK.lock().unwrap();
        let (_dir, path) = fresh_tmp_cache();
        let _restore = EnvOverride::set("OXIDEAV_AACS_VUK_CACHE", path.to_str().unwrap());
        // Pre-populate with one disc id.
        std::fs::write(
            &path,
            "11223344556677889900AABBCCDDEEFF11223344 = V 00112233445566778899AABBCCDDEEFF | seed\n",
        )
        .unwrap();
        let target = [0xFFu8; DISC_ID_LEN];
        let hit = scan_keydb_for_line(&path, &target, false).expect("scan");
        assert!(hit.is_none());
    }

    #[test]
    fn civil_from_unix_anchors() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        // 2000-01-01 00:00:00 UTC = 946684800 seconds.
        assert_eq!(civil_from_unix(946_684_800), (2000, 1, 1, 0, 0, 0));
        // 2026-05-25 12:34:56 UTC = 1779712496 seconds.
        // (Anchor checks the date format + leap-year handling.)
        let (y, mo, d, h, mi, s) = civil_from_unix(1_779_712_496);
        assert_eq!((y, mo, d, h, mi, s), (2026, 5, 25, 12, 34, 56));
    }

    #[test]
    fn rfc3339_now_is_well_formed() {
        let s = rfc3339_now();
        // YYYY-MM-DDTHH:MM:SSZ — 20 chars.
        assert_eq!(s.len(), 20);
        assert!(s.ends_with('Z'));
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
        assert_eq!(&s[10..11], "T");
        assert_eq!(&s[13..14], ":");
        assert_eq!(&s[16..17], ":");
    }

    #[cfg(feature = "aacs-online")]
    #[test]
    fn device_key_from_record_preserves_uv_and_mask_fields() {
        let rec = DeviceKeyRecord {
            device_key: [0x33u8; 16],
            device_node: [0x12, 0x34],
            // 0x0000_0400 → uv = 1024, trailing_zeros = 10 → v_mask_zero_bits = 10.
            key_uv: [0x00, 0x00, 0x04, 0x00],
            key_u_mask_shift: 23,
            comment: None,
        };
        let dk = device_key_from_record(&rec).expect("conversion");
        assert_eq!(dk.key, [0x33u8; 16]);
        assert_eq!(dk.uv, 0x0000_0400);
        assert_eq!(dk.u_mask_zero_bits, 23);
        assert_eq!(dk.v_mask_zero_bits, 10);
    }

    /// Save / restore an environment variable for the duration of a
    /// test (so an `OXIDEAV_AACS_VUK_CACHE` override doesn't leak
    /// into sibling tests). Held inside ENV_LOCK so writes from
    /// parallel tests don't race.
    struct EnvOverride {
        key: &'static str,
        prev: Option<String>,
    }
    impl EnvOverride {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, prev }
        }
    }
    impl Drop for EnvOverride {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// Tiny ephemeral-directory helper so we don't add a `tempfile`
    /// dev-dep just for these tests. Uses the platform tempdir +
    /// PID + a Mutex-derived counter for uniqueness.
    mod tempdir_lite {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);

        pub struct TempDir(PathBuf);
        impl TempDir {
            pub fn new() -> Self {
                let n = SEQ.fetch_add(1, Ordering::SeqCst);
                let pid = std::process::id();
                let base = std::env::temp_dir().join(format!("oxideav-bluray-r127-{pid}-{n}"));
                std::fs::create_dir_all(&base).expect("mkdir tempdir");
                Self(base)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
