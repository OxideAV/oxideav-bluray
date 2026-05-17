//! macOS AACS Volume Identifier reader via IOKit + SCSITaskDevice.
//!
//! Loads IOKit at runtime via [`libloading`] (same pattern oxideplay
//! uses for SDL2). The plan:
//!
//!   1. Resolve `IOServiceMatching`, `IOServiceGetMatchingServices`,
//!      `IOIteratorNext`, `IOObjectRelease`,
//!      `IOCreatePlugInInterfaceForService` from
//!      `/System/Library/Frameworks/IOKit.framework/IOKit`.
//!   2. Walk the IORegistry for `IOBDMedia` or `IODVDMedia` matching
//!      the `disc_root` BSD name (via `statfs` → `f_mntfromname`).
//!   3. Open the SCSITaskDeviceInterface plugin, claim exclusive
//!      access (this requires the volume to be unmounted from
//!      Finder — `diskutil unmount /Volumes/...` first).
//!   4. Build a `READ DISC STRUCTURE` (opcode `0xAD`) CDB with
//!      `Media = 0x01` (BD), `Format = 0x80` (AACS Volume Identifier),
//!      `Alloc Len = 0x0024` (36 bytes).
//!   5. Execute and parse: 4-byte header + 16-byte Volume ID.
//!
//! **Not yet implemented**: this round adds the architectural seam and
//! ships the env-override path. The IOKit/MMC dispatch goes in the
//! next commit (it's ~500 LOC of vtable-style FFI that warrants its
//! own focused PR). Set `OXIDEAV_AACS_VOLUME_ID=<32-hex>` to drive
//! the rest of the pipeline in the meantime.

use super::DriveError;
use std::path::Path;

pub fn read_volume_id(_disc_root: &Path) -> Result<[u8; 16], DriveError> {
    Err(DriveError::Mmc(
        "macOS IOKit/MMC drive query not yet implemented — set \
         OXIDEAV_AACS_VOLUME_ID=<32-hex chars> as a manual override \
         until the IOKit FFI lands"
            .to_string(),
    ))
}
