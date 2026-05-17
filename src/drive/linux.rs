//! Linux AACS Volume Identifier reader via `SG_IO` ioctl on /dev/sr*.
//!
//! Plan:
//!   1. Resolve `disc_root` → `/dev/sr*` via `statfs` + `/proc/mounts`.
//!   2. Open `/dev/srN` with `O_RDONLY | O_NONBLOCK`.
//!   3. Build the `READ DISC STRUCTURE` CDB (opcode `0xAD`,
//!      format `0x80` AACS Volume Identifier).
//!   4. Wrap in `sg_io_hdr` and ioctl(`SG_IO`).
//!   5. Parse 4-byte header + 16-byte Volume ID from the buffer.
//!
//! Not yet implemented — set `OXIDEAV_AACS_VOLUME_ID=<32-hex>` to
//! drive the rest of the pipeline.

use super::DriveError;
use std::path::Path;

pub fn read_volume_id(_disc_root: &Path) -> Result<[u8; 16], DriveError> {
    Err(DriveError::Mmc(
        "Linux SG_IO drive query not yet implemented — set \
         OXIDEAV_AACS_VOLUME_ID=<32-hex chars> as a manual override"
            .to_string(),
    ))
}
