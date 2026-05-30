//! Linux AACS Volume Identifier reader via `ioctl(CDROM_SEND_PACKET)`
//! on a `/dev/sr*` optical-disc device.
//!
//! Why CDROM_SEND_PACKET and not SG_IO: SG_IO is the SCSI Generic v3
//! transport — fine for SCSI / USB-mass-storage / ATAPI-bridge
//! optical drives, but not exposed for legacy IDE optical drives that
//! the kernel registers only through its CD-ROM subsystem.
//! `CDROM_SEND_PACKET` (defined in `<linux/cdrom.h>`,
//! [`CDROM_SEND_PACKET`] = `0x5393`) is the older, more portable
//! interface: it takes a 12-byte MMC CDB plus a `cdrom_generic_command`
//! envelope and the kernel's CD-ROM driver routes it to whatever
//! transport backs the device. libaacs uses this for the same reason.
//!
//! Flow:
//!
//! 1. Resolve `disc_root` (a filesystem mount point like
//!    `/run/media/<user>/<label>`) → the backing block device path
//!    (`/dev/sr0`) by walking `/proc/self/mountinfo`. The mountinfo
//!    line format is documented in the Linux kernel
//!    `Documentation/filesystems/proc.rst` §3.5 (Table 3-5); we want
//!    the mount-point field (#5) and the source field (the first
//!    token after the ` - ` separator).
//! 2. Open the block device with `O_RDONLY | O_NONBLOCK | O_CLOEXEC`.
//! 3. Build a 12-byte MMC `READ DISC STRUCTURE` CDB (opcode `0xAD`)
//!    with Media Type = `0x01` (BD), Format = `0x80`
//!    (AACS Volume Identifier), Allocation Length = `0x0024` (36),
//!    AGID = 0. The CDB layout matches AACS Common §4.14.3 / MMC-6
//!    Table 381 — exactly the bytes the `mmc::ReadDiscStructure`
//!    builder in `oxideav-aacs` emits.
//! 4. Wrap the CDB in a `cdrom_generic_command` struct and issue
//!    `ioctl(fd, CDROM_SEND_PACKET, &cgc)`. The kernel returns the
//!    SCSI status byte in `cgc.stat` and writes any sense data to
//!    `cgc.sense`.
//! 5. Parse the 36-byte response via
//!    `oxideav_aacs::mmc::parse_volume_id_response` — 4-byte header
//!    + 16-byte Volume Identifier + 16-byte MAC.
//!
//! The transport-level [`CdromDrive`] implements `oxideav_aacs::mmc::
//! DriveCommand` so the AKE flow can reuse the same handle for the
//! REPORT KEY ↔ SEND KEY ↔ READ DISC STRUCTURE round-trips.
//!
//! Reference material (publicly licensed): Linux kernel
//! `include/uapi/linux/cdrom.h` for the ioctl + struct layout;
//! T10 MMC-6 r02g for the CDB; AACS LA Common Final 0.953 §4.14.3
//! for the response.

// Lib-root lint is `deny(unsafe_code)` — this module needs raw
// `ioctl(SG_IO)` + `close(fd)`, so opt the whole module in.
#![allow(unsafe_code)]

use super::DriveError;
use oxideav_aacs::mmc::{
    parse_volume_id_response, DataDirection, DriveCommand, ReadDiscStructure, ScsiResponse,
    MMC_CDB_LEN,
};
use oxideav_aacs::AacsError;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::io::{IntoRawFd, RawFd};
use std::path::{Path, PathBuf};

/// `CDROM_SEND_PACKET` ioctl number — defined in `<linux/cdrom.h>`.
const CDROM_SEND_PACKET: libc::c_ulong = 0x5393;

/// `data_direction` constants from `<linux/cdrom.h>`.
const CGC_DATA_WRITE: u8 = 1;
const CGC_DATA_READ: u8 = 2;
const CGC_DATA_NONE: u8 = 3;

/// Default per-command timeout (milliseconds).
const SG_TIMEOUT_MS: u32 = 10_000;

/// Sense-buffer size we hand the kernel. SPC-4 caps fixed-format sense
/// at 18 bytes and descriptor-format at 252; 32 covers both with
/// margin and matches `<scsi/sg.h>` convention.
const SENSE_BUF_LEN: usize = 32;

/// `cdrom_generic_command` from `<linux/cdrom.h>`. The kernel only
/// reads the fields we set and writes back `stat`, the buffer, and
/// the sense bytes; `quiet` suppresses the kernel's printk on a
/// CHECK CONDITION reply.
#[repr(C)]
struct CdromGenericCommand {
    cmd: [u8; 12],
    buffer: *mut u8,
    buflen: u32,
    stat: i32,
    sense: *mut u8,
    data_direction: u8,
    quiet: i32,
    timeout: i32,
    unused: *mut libc::c_void,
}

/// Linux optical-drive transport for MMC commands.
///
/// Owns an open file descriptor on `/dev/sr*` and implements
/// [`DriveCommand`] by translating each `execute` call into a single
/// `ioctl(CDROM_SEND_PACKET)` round-trip. Works on SCSI / USB /
/// ATAPI-bridge / IDE drives transparently — the kernel's CD-ROM
/// driver routes the packet to whichever transport backs the device.
pub struct CdromDrive {
    fd: RawFd,
}

/// Alias kept for callers that still reference `SgDrive`. The
/// transport is `CDROM_SEND_PACKET` now (more portable than SG_IO);
/// the rename is the only visible difference.
pub type SgDrive = CdromDrive;

impl CdromDrive {
    /// Open the given block device for SG_IO traffic. The path is
    /// typically `/dev/sr0`.
    pub fn open(dev_path: &Path) -> Result<Self, DriveError> {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(dev_path)
            .map_err(|e| {
                DriveError::Mmc(format!(
                    "open({}) for SG_IO failed: {e}",
                    dev_path.display()
                ))
            })?;
        Ok(Self {
            fd: f.into_raw_fd(),
        })
    }
}

impl Drop for CdromDrive {
    fn drop(&mut self) {
        // Safety: we own the fd from open(); ignore close errors.
        unsafe {
            libc::close(self.fd);
        }
    }
}

impl DriveCommand for CdromDrive {
    fn execute(
        &mut self,
        cdb: &[u8; MMC_CDB_LEN],
        direction: DataDirection,
        data_out: &[u8],
        allocation_length: u16,
    ) -> Result<ScsiResponse, AacsError> {
        let data_direction = match direction {
            DataDirection::None => CGC_DATA_NONE,
            DataDirection::FromDevice => CGC_DATA_READ,
            DataDirection::ToDevice => CGC_DATA_WRITE,
        };

        let mut data_buf: Vec<u8> = match direction {
            DataDirection::None => Vec::new(),
            DataDirection::FromDevice => vec![0u8; allocation_length as usize],
            DataDirection::ToDevice => data_out.to_vec(),
        };
        let mut sense = [0u8; SENSE_BUF_LEN];

        let buffer = if data_buf.is_empty() {
            std::ptr::null_mut()
        } else {
            data_buf.as_mut_ptr()
        };

        let mut cmd = [0u8; 12];
        cmd.copy_from_slice(cdb);

        let mut cgc = CdromGenericCommand {
            cmd,
            buffer,
            buflen: data_buf.len() as u32,
            stat: 0,
            sense: sense.as_mut_ptr(),
            data_direction,
            quiet: 1,
            timeout: SG_TIMEOUT_MS as i32,
            unused: std::ptr::null_mut(),
        };

        // Safety: `cgc` is a valid `cdrom_generic_command` with stable
        // pointers (`buffer`, `sense`) that outlive the ioctl call.
        let rc = unsafe {
            libc::ioctl(
                self.fd,
                CDROM_SEND_PACKET,
                &mut cgc as *mut CdromGenericCommand,
            )
        };
        if rc != 0 {
            // The kernel returns -1 + errno on transport failure AND on
            // CHECK CONDITION (it stuffs sense into the buffer and
            // returns EIO). Distinguish by inspecting `cgc.stat`:
            // non-zero status means the drive replied (just not GOOD)
            // and we should pass the response up rather than report a
            // transport error.
            let errno = std::io::Error::last_os_error();
            if cgc.stat == 0 {
                return Err(AacsError::Io(format!(
                    "ioctl(CDROM_SEND_PACKET) failed: {errno} (opcode=0x{:02x})",
                    cdb[0]
                )));
            }
        }

        // CHECK CONDITION reply — return sense bytes as the data
        // payload so callers can decode KCQ codes.
        if cgc.stat != 0 {
            return Ok(ScsiResponse {
                status: cgc.stat as u8,
                data: sense.to_vec(),
            });
        }

        if matches!(direction, DataDirection::ToDevice) {
            data_buf.clear();
        }
        Ok(ScsiResponse::good(data_buf))
    }
}

/// Resolve a filesystem mount point to its backing block device by
/// walking `/proc/self/mountinfo`.
///
/// Each line has the format (from `Documentation/filesystems/proc.rst`
/// §3.5, Table 3-5):
///
/// ```text
/// mount-id parent-id major:minor root mount-point options - fstype source super-options
/// ```
///
/// We canonicalize both sides before comparing, so `/run/media/foo`
/// matches even if the caller passed a path with a trailing slash or
/// `.` segment.
fn block_device_for_mount(mount_point: &Path) -> Result<PathBuf, DriveError> {
    let canon = std::fs::canonicalize(mount_point).map_err(|e| {
        DriveError::Mmc(format!(
            "canonicalize({}) failed: {e}",
            mount_point.display()
        ))
    })?;
    let mounts = std::fs::read("/proc/self/mountinfo")
        .map_err(|e| DriveError::Mmc(format!("read /proc/self/mountinfo failed: {e}")))?;
    for raw_line in mounts.split(|&b| b == b'\n') {
        if raw_line.is_empty() {
            continue;
        }
        // Split on ' - ' first to separate the variable-length prefix
        // from the suffix (fstype source super-options).
        let line = match std::str::from_utf8(raw_line) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let (prefix, suffix) = match line.split_once(" - ") {
            Some((p, s)) => (p, s),
            None => continue,
        };
        let pre_fields: Vec<&str> = prefix.split_whitespace().collect();
        if pre_fields.len() < 5 {
            continue;
        }
        // Field 5 (0-indexed 4) is the mount point. mountinfo escapes
        // spaces / tabs / newlines as octal `\NNN`; un-escape so paths
        // with whitespace ("Kite Uncut BD") compare cleanly.
        let mount_str = unescape_mountinfo(pre_fields[4]);
        if Path::new(&mount_str) != canon {
            continue;
        }
        let post_fields: Vec<&str> = suffix.split_whitespace().collect();
        if post_fields.len() < 2 {
            continue;
        }
        let src = unescape_mountinfo(post_fields[1]);
        return Ok(PathBuf::from(src));
    }
    Err(DriveError::Mmc(format!(
        "no /proc/self/mountinfo entry for mount point {}",
        canon.display()
    )))
}

/// Replace mountinfo octal escapes (`\040` etc.) with the literal byte.
fn unescape_mountinfo(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let d0 = bytes[i + 1];
            let d1 = bytes[i + 2];
            let d2 = bytes[i + 3];
            if (b'0'..=b'7').contains(&d0)
                && (b'0'..=b'7').contains(&d1)
                && (b'0'..=b'7').contains(&d2)
            {
                let v = ((d0 - b'0') as u32) * 64 + ((d1 - b'0') as u32) * 8 + (d2 - b'0') as u32;
                if v <= 0xff {
                    out.push(v as u8);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // mountinfo paths are bytes, but for the PathBuf we want a String;
    // lossy is fine because mount-point strings are filesystem paths.
    String::from_utf8_lossy(&out).into_owned()
}

/// Public entry point used by `drive::read_volume_id`. Sends one
/// `READ DISC STRUCTURE` (Format `0x80`, AACS Volume Identifier) to the
/// `/dev/sr*` backing `disc_root` and returns the 16-byte Volume ID.
///
/// AGID = 0; no AKE handshake is run. The macOS backend takes the
/// same unauthenticated shortcut (see `drive/macos.rs`); virtually
/// every consumer Blu-ray drive serves Format 0x80 without prior
/// authentication. If a drive rejects with CHECK CONDITION, callers
/// can layer `oxideav_aacs::ake::host_authenticate` + the same
/// [`SgDrive`] handle and re-issue via
/// `oxideav_aacs::ake::read_verified_volume_id`.
pub fn read_volume_id(disc_root: &Path) -> Result<[u8; 16], DriveError> {
    let dev_path = block_device_for_mount(disc_root)?;
    let mut drive = SgDrive::open(&dev_path)?;

    let rds = ReadDiscStructure::aacs_volume_id(0);
    let cdb = rds.cdb();
    let resp = drive
        .execute(&cdb, DataDirection::FromDevice, &[], rds.allocation_length)
        .map_err(|e| DriveError::Mmc(format!("READ DISC STRUCTURE: {e}")))?;

    if resp.status != 0 {
        let sense_summary = format_sense(&resp.data);
        return Err(DriveError::Mmc(format!(
            "READ DISC STRUCTURE returned SCSI status 0x{:02x} ({sense_summary}) — \
             drive likely requires AACS Drive-Host AKE for Format 0x80 on this \
             medium. Override with OXIDEAV_AACS_VOLUME_ID=<32-hex chars> if you \
             have it from another source.",
            resp.status
        )));
    }
    let parsed = parse_volume_id_response(&resp.data)
        .map_err(|e| DriveError::Mmc(format!("parse Volume ID response: {e}")))?;
    Ok(parsed.volume_id)
}

/// AKE-authenticated variant of [`read_volume_id`]. Runs the full
/// AACS Common 0.953 §4.3 Drive-Host AKE handshake against the
/// drive, then issues `READ DISC STRUCTURE` Format `0x80` under the
/// established bus-key session and verifies the drive's CMAC over
/// the Volume Identifier.
///
/// `host_cert` / `host_priv_key` come from the keydb's `| HC |`
/// record. The AACS LA root public key is taken from
/// [`oxideav_aacs::aacs_la_pub_point`] — a spec-defined constant
/// every compliant licensee carries.
///
/// Ephemeral host material — the 20-byte nonce `Hn` and the 20-byte
/// scalar `Hk` — is drawn from `/dev/urandom`. The AACS spec calls
/// out §2.2 RNG requirements for production callers; `/dev/urandom`
/// on a modern kernel meets the practical bar.
pub fn read_volume_id_with_ake(
    disc_root: &Path,
    host_cert: &[u8; 92],
    host_priv_key: &[u8; 20],
) -> Result<[u8; 16], DriveError> {
    use oxideav_aacs::ake::{
        aacs_la_pub_point, host_authenticate, read_verified_volume_id, HostCredentials,
    };
    use oxideav_aacs::ec::U160;
    use std::io::Read;

    let aacs_la_pub = aacs_la_pub_point();

    let dev_path = block_device_for_mount(disc_root)?;
    let mut drive = SgDrive::open(&dev_path)?;

    // Draw 40 random bytes for Hn (20) || Hk (20).
    let mut rng_bytes = [0u8; 40];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut rng_bytes))
        .map_err(|e| DriveError::Mmc(format!("/dev/urandom read failed: {e}")))?;
    let mut host_nonce = [0u8; 20];
    host_nonce.copy_from_slice(&rng_bytes[..20]);
    let mut hk_bytes = [0u8; 20];
    hk_bytes.copy_from_slice(&rng_bytes[20..]);
    let hk = U160::from_be_bytes(&hk_bytes);

    let creds = HostCredentials {
        host_cert: *host_cert,
        host_priv: U160::from_be_bytes(host_priv_key),
        aacs_la_pub,
    };

    let ake_result = host_authenticate(&mut drive, &creds, &host_nonce, &hk)
        .map_err(|e| DriveError::Mmc(format!("AKE handshake failed: {e}")))?;

    read_verified_volume_id(&mut drive, &ake_result.bus_key, ake_result.agid)
        .map_err(|e| DriveError::Mmc(format!("AKE-verified Volume ID read failed: {e}")))
}

/// Render a sense buffer as a short KCQ triple (Key/ASC/ASCQ) for
/// error messages. Accepts both fixed-format and descriptor-format
/// sense.
fn format_sense(sense: &[u8]) -> String {
    if sense.len() < 14 {
        return "no sense".to_string();
    }
    // Fixed-format (response code 0x70/0x71): sense key in byte 2 low
    // nibble; ASC at byte 12; ASCQ at byte 13.
    // Descriptor-format (0x72/0x73): sense key in byte 1; ASC at 2;
    // ASCQ at 3.
    let rc = sense[0] & 0x7f;
    let (key, asc, ascq) = if rc == 0x70 || rc == 0x71 {
        (sense[2] & 0x0f, sense[12], sense[13])
    } else if rc == 0x72 || rc == 0x73 {
        (sense[1] & 0x0f, sense[2], sense[3])
    } else {
        return format!("unknown sense rc=0x{rc:02x}");
    };
    format!("KCQ {key:02x}/{asc:02x}/{ascq:02x}")
}
