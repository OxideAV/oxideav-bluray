//! Windows AACS Volume Identifier reader via SPTI (SCSI Pass-Through
//! Interface) on `\\.\E:` style device paths.
//!
//! Plan:
//!   1. Open `\\.\<DriveLetter>:` via `CreateFileW`.
//!   2. `DeviceIoControl(IOCTL_SCSI_PASS_THROUGH_DIRECT, ...)` with a
//!      `SCSI_PASS_THROUGH_DIRECT` carrying the
//!      `READ DISC STRUCTURE` CDB (opcode `0xAD`, format `0x80`).
//!   3. Parse the response.
//!
//! Not yet implemented — set `OXIDEAV_AACS_VOLUME_ID=<32-hex>`.

use super::DriveError;
use std::path::Path;

pub fn read_volume_id(_disc_root: &Path) -> Result<[u8; 16], DriveError> {
    Err(DriveError::Mmc(
        "Windows SPTI drive query not yet implemented — set \
         OXIDEAV_AACS_VOLUME_ID=<32-hex chars> as a manual override"
            .to_string(),
    ))
}
