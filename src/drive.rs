//! Drive-level access to the AACS Volume Identifier.
//!
//! The Volume Identifier is stored in the BD-ROM Mark — a physically-
//! protected region not accessible through the filesystem. The only way
//! to retrieve it is to send an MMC `READ DISC STRUCTURE` command (SCSI
//! opcode `0xAD`) with `Media Specific = 0x80` (AACS Volume Identifier)
//! to the optical drive.
//!
//! This module owns that drive query. The platform implementations live
//! under `drive/<os>.rs` and are loaded lazily via `libloading` against
//! the system framework / kernel interface, matching the workspace's
//! existing convention (see oxideplay's SDL2 loader).
//!
//! ## Override
//!
//! For testing / when the drive doesn't permit MMC AACS queries (e.g.
//! macOS exclusive-access conflicts with Finder having the volume
//! mounted), the env var `OXIDEAV_AACS_VOLUME_ID=<32-hex chars>`
//! short-circuits the drive query and supplies the 16-byte Volume ID
//! directly. Useful for validating the rest of the pipeline before the
//! native drive path lands per platform.
//!
//! ## AKE-authenticated read
//!
//! Most consumer Blu-ray drives refuse a raw `READ DISC STRUCTURE`
//! Format `0x80` query with sense `KCQ 05/6f/02` ("KEY NOT
//! ESTABLISHED") — they require the AACS Drive-Host Authentication +
//! Key Exchange handshake first. [`read_volume_id_with_ake`] runs the
//! full §4.3 AKE against the drive using the host certificate +
//! private key from `KEYDB.cfg`'s `| HC |` record and the bundled
//! AACS LA root public key ([`oxideav_aacs::aacs_la_pub_point`]),
//! then issues the VID query under the bus-key-encrypted session.

use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum DriveError {
    #[error("OXIDEAV_AACS_VOLUME_ID env override is not 32 hex characters: {0:?}")]
    BadEnvOverride(String),
    #[error("AACS Volume Identifier query not yet implemented on this platform")]
    Unimplemented,
    #[error("MMC error: {0}")]
    Mmc(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Read the 16-byte AACS Volume Identifier for the disc rooted at
/// `disc_root` (the mount-point path, e.g. `/Volumes/My Disc` on macOS).
///
/// Resolution order:
///   1. `$OXIDEAV_AACS_VOLUME_ID` env override (32 hex chars). Useful
///      for testing the rest of the pipeline.
///   2. Platform-native MMC `READ DISC STRUCTURE` command via the
///      drive backing `disc_root`.
pub fn read_volume_id(disc_root: &Path) -> Result<[u8; 16], DriveError> {
    if let Ok(s) = std::env::var("OXIDEAV_AACS_VOLUME_ID") {
        let s = s
            .strip_prefix("0x")
            .or_else(|| s.strip_prefix("0X"))
            .unwrap_or(&s);
        if s.len() != 32 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(DriveError::BadEnvOverride(s.to_string()));
        }
        let mut out = [0u8; 16];
        for i in 0..16 {
            out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
                .map_err(|_| DriveError::BadEnvOverride(s.to_string()))?;
        }
        return Ok(out);
    }
    platform::read_volume_id(disc_root)
}

/// Read the 16-byte AACS Volume Identifier through an AKE-authenticated
/// drive session. Mirrors the AACS Common 0.953 §4.3 host-side
/// handshake exactly:
///
/// 1. `REPORT KEY` Key Format `0x00` → AGID.
/// 2. `SEND KEY` Key Format `0x01` → `Hn || Host_Cert`.
/// 3. `REPORT KEY` Key Format `0x01` → `Dn || Drive_Cert`. Host
///    verifies the Drive Certificate against `AACS_LApub`.
/// 4. `REPORT KEY` Key Format `0x02` → `Dv || Dsig`. Host verifies
///    `AACS_Verify(Drive_pub, Dsig, Hn || Dv)`.
/// 5. Host computes `Hk·G = Hv`, signs `(Dn || Hv)`, and sends
///    `Hv || Hsig` via `SEND KEY` `0x02`.
/// 6. Bus Key `BK = lsb_128(x-coord of Hk·Dv)`.
/// 7. `READ DISC STRUCTURE` Format `0x80` with the granted AGID;
///    response includes a CMAC under BK over the Volume ID that the
///    host verifies before returning.
///
/// Currently wired only for the Linux SG_IO transport. macOS already
/// reads VID without AKE via IOKit's `MMCDeviceInterface`; Windows
/// (SPTI) returns `Unimplemented` for now.
#[cfg(target_os = "linux")]
pub fn read_volume_id_with_ake(
    disc_root: &Path,
    host_cert: &[u8; 92],
    host_priv_key: &[u8; 20],
) -> Result<[u8; 16], DriveError> {
    platform::read_volume_id_with_ake(disc_root, host_cert, host_priv_key)
}

/// Stub on non-Linux platforms — AKE wiring is only Linux-side for
/// now (macOS uses IOKit's `ReadDiscStructure` which doesn't need
/// AKE; Windows SPTI plumbing isn't yet in this crate).
#[cfg(not(target_os = "linux"))]
pub fn read_volume_id_with_ake(
    _disc_root: &Path,
    _host_cert: &[u8; 92],
    _host_priv_key: &[u8; 20],
) -> Result<[u8; 16], DriveError> {
    Err(DriveError::Unimplemented)
}

#[cfg(target_os = "macos")]
#[path = "drive/macos.rs"]
mod platform;

#[cfg(target_os = "linux")]
#[path = "drive/linux.rs"]
mod platform;

#[cfg(target_os = "windows")]
#[path = "drive/windows.rs"]
mod platform;

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod platform {
    use super::DriveError;
    use std::path::Path;
    pub fn read_volume_id(_disc_root: &Path) -> Result<[u8; 16], DriveError> {
        Err(DriveError::Unimplemented)
    }
}
