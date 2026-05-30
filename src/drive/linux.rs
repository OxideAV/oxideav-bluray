//! Linux AACS Volume Identifier reader via `ioctl(SG_IO)` on a
//! `/dev/sr*` SCSI generic device.
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
//!    `O_NONBLOCK` is required by the SG_IO ioctl interface so the
//!    kernel doesn't block waiting for the device to spin up.
//! 3. Build a 12-byte MMC `READ DISC STRUCTURE` CDB (opcode `0xAD`)
//!    with Media Type = `0x01` (BD), Format = `0x80`
//!    (AACS Volume Identifier), Allocation Length = `0x0024` (36),
//!    AGID = 0. The CDB layout matches AACS Common §4.14.3 / MMC-6
//!    Table 381 — exactly the bytes the `mmc::ReadDiscStructure`
//!    builder in `oxideav-aacs` emits.
//! 4. Wrap the CDB in a `sg_io_hdr_v3` struct (`<scsi/sg.h>`) and
//!    issue `ioctl(fd, SG_IO, &hdr)`. SG_IO is the Linux SCSI
//!    Generic v3 interface — a single round-trip command that
//!    takes a CDB, optional data-in/data-out buffer, and returns
//!    SCSI status + sense data.
//! 5. Parse the 36-byte response via
//!    `oxideav_aacs::mmc::parse_volume_id_response` — 4-byte header
//!    + 16-byte Volume Identifier + 16-byte MAC.
//!
//! The transport-level [`SgDrive`] implements `oxideav_aacs::mmc::
//! DriveCommand` so an AACS-AKE-authenticated flow can layer on top
//! later (`oxideav_aacs::ake::read_verified_volume_id`) by reusing
//! the same handle.
//!
//! Reference material (publicly licensed / spec): Linux kernel
//! `Documentation/scsi/scsi-generic.rst` for SG_IO; T10 MMC-6 r02g
//! for the CDB; AACS LA Common Final 0.953 §4.14.3 for the response.

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

/// `SG_IO` ioctl number — defined as `_IOWR('S', 0x85, sg_io_hdr_t)` in
/// `<scsi/sg.h>`. On every Linux architecture this resolves to
/// `0x2285`.
const SG_IO: libc::c_ulong = 0x2285;

/// `dxfer_direction` constants from `<scsi/sg.h>`.
const SG_DXFER_NONE: libc::c_int = -1;
const SG_DXFER_TO_DEV: libc::c_int = -2;
const SG_DXFER_FROM_DEV: libc::c_int = -3;

/// Default per-command timeout (milliseconds).
const SG_TIMEOUT_MS: u32 = 10_000;

/// Maximum sense-data length we'll accept from the drive (`<scsi/sg.h>`
/// historically uses 32 bytes; SPC-4 caps fixed-format sense at 18 and
/// descriptor-format at 252 — 32 is the documented Linux convention).
const SG_SENSE_BUF_LEN: usize = 32;

/// `sg_io_hdr_v3` from `<scsi/sg.h>` — the v3 ABI is stable since
/// 2.4 and is what SG_IO consumes. Layout matches the C struct on
/// 64-bit Linux; #[repr(C)] keeps field order + alignment in sync.
#[repr(C)]
struct SgIoHdr {
    interface_id: libc::c_int,
    dxfer_direction: libc::c_int,
    cmd_len: libc::c_uchar,
    mx_sb_len: libc::c_uchar,
    iovec_count: libc::c_ushort,
    dxfer_len: libc::c_uint,
    dxferp: *mut libc::c_void,
    cmdp: *const libc::c_uchar,
    sbp: *mut libc::c_uchar,
    timeout: libc::c_uint,
    flags: libc::c_uint,
    pack_id: libc::c_int,
    usr_ptr: *mut libc::c_void,
    status: libc::c_uchar,
    masked_status: libc::c_uchar,
    msg_status: libc::c_uchar,
    sb_len_wr: libc::c_uchar,
    host_status: libc::c_ushort,
    driver_status: libc::c_ushort,
    resid: libc::c_int,
    duration: libc::c_uint,
    info: libc::c_uint,
}

/// Linux SCSI-Generic SG_IO transport for MMC commands.
///
/// Owns an open file descriptor on `/dev/sr*` and implements
/// [`DriveCommand`] by translating each `execute` call into a single
/// `ioctl(SG_IO)` round-trip.
pub struct SgDrive {
    fd: RawFd,
}

impl SgDrive {
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

impl Drop for SgDrive {
    fn drop(&mut self) {
        // Safety: we own the fd from open(); ignore close errors.
        unsafe {
            libc::close(self.fd);
        }
    }
}

impl DriveCommand for SgDrive {
    fn execute(
        &mut self,
        cdb: &[u8; MMC_CDB_LEN],
        direction: DataDirection,
        data_out: &[u8],
        allocation_length: u16,
    ) -> Result<ScsiResponse, AacsError> {
        let dxfer_direction = match direction {
            DataDirection::None => SG_DXFER_NONE,
            DataDirection::FromDevice => SG_DXFER_FROM_DEV,
            DataDirection::ToDevice => SG_DXFER_TO_DEV,
        };

        let mut data_buf: Vec<u8> = match direction {
            DataDirection::None => Vec::new(),
            DataDirection::FromDevice => vec![0u8; allocation_length as usize],
            DataDirection::ToDevice => data_out.to_vec(),
        };
        let mut sense = [0u8; SG_SENSE_BUF_LEN];

        let dxferp = if data_buf.is_empty() {
            std::ptr::null_mut()
        } else {
            data_buf.as_mut_ptr() as *mut libc::c_void
        };

        let mut hdr = SgIoHdr {
            interface_id: b'S' as libc::c_int,
            dxfer_direction,
            cmd_len: MMC_CDB_LEN as u8,
            mx_sb_len: SG_SENSE_BUF_LEN as u8,
            iovec_count: 0,
            dxfer_len: data_buf.len() as u32,
            dxferp,
            cmdp: cdb.as_ptr(),
            sbp: sense.as_mut_ptr(),
            timeout: SG_TIMEOUT_MS,
            flags: 0,
            pack_id: 0,
            usr_ptr: std::ptr::null_mut(),
            status: 0,
            masked_status: 0,
            msg_status: 0,
            sb_len_wr: 0,
            host_status: 0,
            driver_status: 0,
            resid: 0,
            duration: 0,
            info: 0,
        };

        // Safety: `hdr` is a valid `sg_io_hdr_v3` with stable pointers
        // (`cdb`, `data_buf`, `sense`) that all outlive the ioctl call.
        let rc = unsafe { libc::ioctl(self.fd, SG_IO, &mut hdr as *mut SgIoHdr) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return Err(AacsError::Io(format!(
                "ioctl(SG_IO) failed: {err} (opcode=0x{:02x})",
                cdb[0]
            )));
        }

        // SG_IO v3 packs three different status fields:
        //   * host_status: host-adapter transport errors (e.g.
        //     selection timeout, unexpected disconnect).
        //   * driver_status: driver-layer diagnostic. The low nibble
        //     carries the error code (`DRIVER_TIMEOUT=6`,
        //     `DRIVER_SENSE=8`, etc.). DRIVER_SENSE alone just means
        //     "sense data accompanies a non-GOOD SCSI status" — it's
        //     the normal way the kernel surfaces a drive's CHECK
        //     CONDITION reply, not a transport failure. The high bits
        //     carry DRIVER_SUGGEST_* advisories that we ignore.
        //   * status: the SCSI status byte itself.
        //
        // Treat only host_status != 0 and non-SENSE driver-status
        // codes as transport errors; a CHECK CONDITION reply (status
        // = 0x02, driver_status = 0x08) flows through to the sense
        // path below.
        const DRIVER_SENSE: u16 = 0x08;
        let driver_code = hdr.driver_status & 0x0f;
        if hdr.host_status != 0 || (driver_code != 0 && driver_code != DRIVER_SENSE) {
            return Err(AacsError::Io(format!(
                "SG_IO transport error: host_status=0x{:04x} driver_status=0x{:04x} \
                 (opcode=0x{:02x})",
                hdr.host_status, hdr.driver_status, cdb[0]
            )));
        }

        // Truncate read buffer to actually-transferred bytes (resid is
        // requested minus actual, can be 0 or positive).
        if matches!(direction, DataDirection::FromDevice) {
            let actual = (data_buf.len() as i32 - hdr.resid).max(0) as usize;
            data_buf.truncate(actual);
        } else {
            data_buf.clear();
        }

        // If the drive reported a non-GOOD status, hand back the sense
        // bytes instead of the (typically empty / undefined) data
        // buffer so callers can diagnose.
        if hdr.status != 0 {
            let sb_len = (hdr.sb_len_wr as usize).min(SG_SENSE_BUF_LEN);
            return Ok(ScsiResponse {
                status: hdr.status,
                data: sense[..sb_len].to_vec(),
            });
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
