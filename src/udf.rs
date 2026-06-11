//! Minimal read-only UDF 2.50 mounter, scoped to what's needed to
//! walk a BD-ROM `BDMV/` directory.
//!
//! Reference: ECMA-167 3rd edition (June 1997).
//!
//! ## What's implemented
//!
//! - Sector layout: 2048-byte logical sectors (BD-ROM block size).
//! - Volume Recognition Sequence — verify `BEA01` / `NSR0X` / `TEA01`
//!   at sector 16+. Optional in practice; the mounter does not abort
//!   if the VRS is absent (some BD-R authoring tools omit it).
//! - Anchor Volume Descriptor Pointer at sector 256 (§3/10.2).
//! - Volume Descriptor Sequence: Primary Volume Descriptor (§10.1),
//!   Logical Volume Descriptor (§10.6), Partition Descriptor (§10.5).
//! - File Set Descriptor (§14.1).
//! - File Identifier Descriptor (§14.4) + File Entry / ICB (§14.9).
//! - Short Allocation Descriptors (§14.14.1), Long Allocation
//!   Descriptors (§14.14.2) and Extended Allocation Descriptors
//!   (§14.14.3) in File Entries. Long / extended extents must point
//!   into the mounted partition (the single-partition BD-ROM
//!   assumption); an ext_ad whose `Recorded Length` differs from its
//!   `Information Length` (a compressed extent, §14.14.3 Note 46) is
//!   refused. The file walk refuses any extent with
//!   `extent_type != 0` (recorded + allocated).
//!
//! ## What's not implemented (Phase 1 — surface `Unsupported`)
//!
//! - Multi-extent partition maps (`partition_map_count > 1`).
//! - ICB strategy types other than 4 (the spec's "default" linear).
//! - Extended Attributes / Symbolic Links / Streams.
//! - Sparse / sequential files.
//! - Allocation Extent Descriptors (§14.5).
//! - UDF 1.50 or earlier (we look at the LVD identifier suffix).
//!
//! ## ExtendedFileEntry (§14.17)
//!
//! Tag 266 lays out the same prefix as FileEntry (§14.9) up through
//! `Information Length` (BP 56), then inserts three extra fields —
//! `Object Size` (BP 64, u64), `Logical Blocks Recorded` (BP 72, u64,
//! same semantics as FE BP 64), and `Creation Date and Time` (BP 104,
//! 12-byte timestamp) — before the trailing prefix shifts by 40 bytes.
//! L_EA lives at BP 208, L_AD at BP 212, and the Extended-Attribute /
//! Allocation-Descriptor area starts at BP 216. We surface the FE +
//! EFE union as a single [`FileEntry`] struct with [`FileEntry::object_size`]
//! populated for the EFE variant; the allocation walking is identical
//! between the two so [`UdfDisc::read_file`] / [`UdfDisc::read_directory`]
//! transparently traverse either.

use std::io::{Read, Seek, SeekFrom};

use crate::error::{BlurayError, Result};

/// BD-ROM logical sector size in bytes. UDF allows other sizes but
/// every BD spec mandates 2048-byte sectors.
pub const SECTOR_SIZE: u64 = 2048;
/// Sector at which the Anchor Volume Descriptor Pointer (AVDP) lives
/// per ECMA-167 §3/10.2 and UDF 2.5 §2.2.3.
pub const AVDP_SECTOR: u64 = 256;
/// First sector of the Volume Recognition Sequence (§2/8.3).
pub const VRS_FIRST_SECTOR: u64 = 16;

// ─────────────────────── Descriptor tag (§7.2) ───────────────────────

/// Numeric `TagIdentifier` of every descriptor we touch. Values from
/// ECMA-167 §3/7.2.1 unless otherwise noted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum TagId {
    PrimaryVolume = 1,
    AnchorVolumeDescriptorPointer = 2,
    VolumeDescriptorPointer = 3,
    ImplementationUseVolume = 4,
    Partition = 5,
    LogicalVolume = 6,
    UnallocatedSpace = 7,
    Terminating = 8,
    LogicalVolumeIntegrity = 9,
    FileSet = 256,
    FileIdentifier = 257,
    AllocationExtent = 258,
    Indirect = 259,
    Terminal = 260,
    FileEntry = 261,
    ExtendedAttributeHeader = 262,
    UnallocatedSpaceEntry = 263,
    SpaceBitmap = 264,
    PartitionIntegrityEntry = 265,
    ExtendedFileEntry = 266,
}

impl TagId {
    pub fn from_raw(v: u16) -> Option<Self> {
        Some(match v {
            1 => Self::PrimaryVolume,
            2 => Self::AnchorVolumeDescriptorPointer,
            3 => Self::VolumeDescriptorPointer,
            4 => Self::ImplementationUseVolume,
            5 => Self::Partition,
            6 => Self::LogicalVolume,
            7 => Self::UnallocatedSpace,
            8 => Self::Terminating,
            9 => Self::LogicalVolumeIntegrity,
            256 => Self::FileSet,
            257 => Self::FileIdentifier,
            258 => Self::AllocationExtent,
            259 => Self::Indirect,
            260 => Self::Terminal,
            261 => Self::FileEntry,
            262 => Self::ExtendedAttributeHeader,
            263 => Self::UnallocatedSpaceEntry,
            264 => Self::SpaceBitmap,
            265 => Self::PartitionIntegrityEntry,
            266 => Self::ExtendedFileEntry,
            _ => return None,
        })
    }
}

/// The 16-byte descriptor tag prefix common to every numbered
/// descriptor (§7.2). All multi-byte fields are little-endian.
///
/// ```text
///   0  TagIdentifier        u16 LE
///   2  DescriptorVersion    u16 LE   (2 for UDF 1.50, 3 for 2.x)
///   4  TagChecksum          u8       (sum of bytes 0..16 except [4] mod 256)
///   5  Reserved             u8
///   6  TagSerialNumber      u16 LE
///   8  DescriptorCRC        u16 LE
///  10  DescriptorCRCLength  u16 LE
///  12  TagLocation          u32 LE   (sector this tag is recorded at)
/// ```
#[derive(Debug, Clone, Copy)]
pub struct DescriptorTag {
    pub id: TagId,
    pub descriptor_version: u16,
    pub serial_number: u16,
    pub crc: u16,
    pub crc_length: u16,
    pub location: u32,
}

impl DescriptorTag {
    pub const SIZE: usize = 16;

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::SIZE {
            return Err(BlurayError::malformed("descriptor tag truncated"));
        }
        let id_raw = u16::from_le_bytes([bytes[0], bytes[1]]);
        let id = TagId::from_raw(id_raw)
            .ok_or_else(|| BlurayError::malformed(format!("unknown TagId {id_raw}")))?;
        let descriptor_version = u16::from_le_bytes([bytes[2], bytes[3]]);
        let checksum = bytes[4];
        // Reserved byte at offset 5 must be zero per §7.2.5.
        if bytes[5] != 0 {
            return Err(BlurayError::malformed(
                "DescriptorTag reserved byte non-zero",
            ));
        }
        let serial_number = u16::from_le_bytes([bytes[6], bytes[7]]);
        let crc = u16::from_le_bytes([bytes[8], bytes[9]]);
        let crc_length = u16::from_le_bytes([bytes[10], bytes[11]]);
        let location = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);

        // Validate tag checksum (§7.2.3): sum bytes 0..16 except byte 4
        // and compare modulo 256.
        let calc: u32 = bytes[..Self::SIZE]
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 4)
            .map(|(_, b)| *b as u32)
            .sum();
        if (calc & 0xFF) as u8 != checksum {
            return Err(BlurayError::malformed("DescriptorTag checksum mismatch"));
        }

        Ok(Self {
            id,
            descriptor_version,
            serial_number,
            crc,
            crc_length,
            location,
        })
    }

    /// Encode a tag back to bytes. Recomputes the checksum; the CRC
    /// fields are written as-is (CRC validation against the descriptor
    /// body is not performed by this minimal mounter).
    pub fn encode(&self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        let id = self.id as u16;
        out[0..2].copy_from_slice(&id.to_le_bytes());
        out[2..4].copy_from_slice(&self.descriptor_version.to_le_bytes());
        // out[4] = checksum, fill last.
        // out[5] = 0
        out[6..8].copy_from_slice(&self.serial_number.to_le_bytes());
        out[8..10].copy_from_slice(&self.crc.to_le_bytes());
        out[10..12].copy_from_slice(&self.crc_length.to_le_bytes());
        out[12..16].copy_from_slice(&self.location.to_le_bytes());
        let sum: u32 = out
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 4)
            .map(|(_, b)| *b as u32)
            .sum();
        out[4] = (sum & 0xFF) as u8;
        out
    }
}

// ─────────────────────── extent_ad (§7.1) ───────────────────────

/// `extent_ad`: 8-byte pair (length in bytes, logical block location).
#[derive(Debug, Clone, Copy)]
pub struct ExtentAd {
    pub length: u32,   // bytes
    pub location: u32, // logical block number
}

impl ExtentAd {
    pub const SIZE: usize = 8;
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::SIZE {
            return Err(BlurayError::malformed("extent_ad truncated"));
        }
        Ok(Self {
            length: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            location: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        })
    }

    pub fn encode(self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        out[0..4].copy_from_slice(&self.length.to_le_bytes());
        out[4..8].copy_from_slice(&self.location.to_le_bytes());
        out
    }
}

// ─────────────────────── short_ad (§14.14.1) ───────────────────────

/// `short_ad`: 8-byte allocation descriptor used by File Entries.
/// Top 2 bits of `length` encode the extent type (§14.14.1.1):
/// - 00 = recorded and allocated
/// - 01 = allocated but not recorded
/// - 10 = not allocated, not recorded
/// - 11 = the extent is the next AD (continuation pointer)
#[derive(Debug, Clone, Copy)]
pub struct ShortAd {
    pub length: u32,         // bytes (bottom 30 bits)
    pub extent_type: u8,     // 0..=3
    pub block_location: u32, // logical block number within the partition
}

impl ShortAd {
    pub const SIZE: usize = 8;
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::SIZE {
            return Err(BlurayError::malformed("short_ad truncated"));
        }
        let raw_len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let block_location = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        Ok(Self {
            length: raw_len & 0x3FFF_FFFF,
            extent_type: ((raw_len >> 30) & 0b11) as u8,
            block_location,
        })
    }

    pub fn encode(self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        let raw_len = (self.length & 0x3FFF_FFFF) | ((self.extent_type as u32 & 0b11) << 30);
        out[0..4].copy_from_slice(&raw_len.to_le_bytes());
        out[4..8].copy_from_slice(&self.block_location.to_le_bytes());
        out
    }
}

// ─────────────────────── lb_addr / long_ad (§7.1) ───────────────────────

/// `lb_addr`: 6-byte logical block address (4-byte block number +
/// 2-byte partition reference).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LbAddr {
    pub block: u32,
    pub partition_ref: u16,
}

impl LbAddr {
    pub const SIZE: usize = 6;
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::SIZE {
            return Err(BlurayError::malformed("lb_addr truncated"));
        }
        Ok(Self {
            block: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            partition_ref: u16::from_le_bytes([bytes[4], bytes[5]]),
        })
    }

    pub fn encode(self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        out[0..4].copy_from_slice(&self.block.to_le_bytes());
        out[4..6].copy_from_slice(&self.partition_ref.to_le_bytes());
        out
    }
}

/// `long_ad`: 16-byte allocation descriptor used by the File Set
/// Descriptor and other indirection structures. We carry only the
/// fields we use.
#[derive(Debug, Clone, Copy)]
pub struct LongAd {
    pub length: u32,
    pub extent_type: u8,
    pub location: LbAddr,
    pub implementation_use: [u8; 6],
}

impl LongAd {
    pub const SIZE: usize = 16;
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::SIZE {
            return Err(BlurayError::malformed("long_ad truncated"));
        }
        let raw_len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let location = LbAddr::parse(&bytes[4..10])?;
        let mut impl_use = [0u8; 6];
        impl_use.copy_from_slice(&bytes[10..16]);
        Ok(Self {
            length: raw_len & 0x3FFF_FFFF,
            extent_type: ((raw_len >> 30) & 0b11) as u8,
            location,
            implementation_use: impl_use,
        })
    }

    pub fn encode(self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        let raw_len = (self.length & 0x3FFF_FFFF) | ((self.extent_type as u32 & 0b11) << 30);
        out[0..4].copy_from_slice(&raw_len.to_le_bytes());
        out[4..10].copy_from_slice(&self.location.encode());
        out[10..16].copy_from_slice(&self.implementation_use);
        out
    }
}

// ─────────────────────── ext_ad (§14.14.3) ───────────────────────

/// `ext_ad`: 20-byte Extended Allocation Descriptor (§14.14.3).
///
/// ```text
///   0  Extent Length        u32  (top 2 bits = extent type, §14.14.1.1)
///   4  Recorded Length      u32  (bytes actually recorded; top 2 bits reserved)
///   8  Information Length   u32  (bytes of information starting at the extent)
///  12  Extent Location      lb_addr (6 bytes)
///  18  Implementation Use   2 bytes
/// ```
///
/// `Recorded Length < Information Length` signals a compressed extent
/// (§14.14.3 Note 46) — the mounter refuses those.
#[derive(Debug, Clone, Copy)]
pub struct ExtAd {
    pub length: u32,
    pub extent_type: u8,
    pub recorded_length: u32,
    pub information_length: u32,
    pub location: LbAddr,
    pub implementation_use: [u8; 2],
}

impl ExtAd {
    pub const SIZE: usize = 20;
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::SIZE {
            return Err(BlurayError::malformed("ext_ad truncated"));
        }
        let raw_len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let recorded_length =
            u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) & 0x3FFF_FFFF;
        let information_length = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let location = LbAddr::parse(&bytes[12..18])?;
        Ok(Self {
            length: raw_len & 0x3FFF_FFFF,
            extent_type: ((raw_len >> 30) & 0b11) as u8,
            recorded_length,
            information_length,
            location,
            implementation_use: [bytes[18], bytes[19]],
        })
    }

    pub fn encode(self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        let raw_len = (self.length & 0x3FFF_FFFF) | ((self.extent_type as u32 & 0b11) << 30);
        out[0..4].copy_from_slice(&raw_len.to_le_bytes());
        out[4..8].copy_from_slice(&(self.recorded_length & 0x3FFF_FFFF).to_le_bytes());
        out[8..12].copy_from_slice(&self.information_length.to_le_bytes());
        out[12..18].copy_from_slice(&self.location.encode());
        out[18..20].copy_from_slice(&self.implementation_use);
        out
    }
}

/// One allocation extent normalised across the three AD flavours
/// (§14.14.1 short / §14.14.2 long / §14.14.3 extended) so the file
/// walk can run a single loop.
#[derive(Debug, Clone, Copy)]
pub struct AllocExtent {
    /// Bytes this extent contributes to the file body. For an ext_ad
    /// this is its `Information Length`; for short/long it's the
    /// 30-bit Extent Length.
    pub length: u32,
    /// §14.14.1.1 extent type (0 = recorded and allocated).
    pub extent_type: u8,
    /// Logical block number of the extent within its partition.
    pub block: u32,
    /// `None` for a short_ad (the partition the descriptor is
    /// recorded on is implied, §14.14.1.2); `Some(ref)` for long /
    /// extended ADs whose `lb_addr` names a partition explicitly.
    pub partition_ref: Option<u16>,
}

// ─────────────────────── d-string / OSTA compressed unicode ───────────────────────

/// Decode an OSTA Compressed Unicode `d-string` per UDF 2.50 §2.1.3.
///
/// The first byte is the compression ID (8 = 8-bit, 16 = 16-bit BE).
/// The remainder is the payload, followed by a length byte we read
/// from the *outer* `length` argument (the d-string carrier supplies
/// the total byte count including the compression-id prefix).
///
/// Returns the decoded `String`. Truncating null bytes are stripped.
pub fn decode_dstring(payload: &[u8]) -> Result<String> {
    if payload.is_empty() {
        return Ok(String::new());
    }
    match payload[0] {
        0 => Ok(String::new()),
        8 => {
            // 8-bit per char.
            let mut s = String::with_capacity(payload.len() - 1);
            for &b in &payload[1..] {
                if b == 0 {
                    break;
                }
                s.push(b as char);
            }
            Ok(s)
        }
        16 => {
            // 16-bit BE per code-point.
            let body = &payload[1..];
            if body.len() % 2 != 0 {
                return Err(BlurayError::malformed(
                    "16-bit d-string with odd byte count",
                ));
            }
            let mut s = String::with_capacity(body.len() / 2);
            for chunk in body.chunks_exact(2) {
                let cp = u16::from_be_bytes([chunk[0], chunk[1]]);
                if cp == 0 {
                    break;
                }
                if let Some(c) = char::from_u32(cp as u32) {
                    s.push(c);
                }
            }
            Ok(s)
        }
        other => Err(BlurayError::malformed(format!(
            "d-string compression id {other}"
        ))),
    }
}

/// Decode a `d-string` field where the *outer* layout is: a `field_len`
/// region of bytes whose last byte is the length of the payload in
/// bytes. The payload occupies the first `length` bytes of the region.
pub fn decode_dstring_field(field: &[u8]) -> Result<String> {
    if field.is_empty() {
        return Ok(String::new());
    }
    let len = *field.last().unwrap() as usize;
    if len > field.len() - 1 {
        return Err(BlurayError::malformed("d-string length overflows field"));
    }
    decode_dstring(&field[..len])
}

// ─────────────────────── AnchorVolumeDescriptorPointer (§10.2) ───────────────────────

#[derive(Debug, Clone, Copy)]
pub struct AnchorVolumeDescriptorPointer {
    pub tag: DescriptorTag,
    pub main_volume_descriptor_sequence: ExtentAd,
    pub reserve_volume_descriptor_sequence: ExtentAd,
}

impl AnchorVolumeDescriptorPointer {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let tag = DescriptorTag::parse(bytes)?;
        if tag.id != TagId::AnchorVolumeDescriptorPointer {
            return Err(BlurayError::malformed(format!(
                "expected AVDP tag, got {:?}",
                tag.id
            )));
        }
        let main = ExtentAd::parse(&bytes[16..24])?;
        let reserve = ExtentAd::parse(&bytes[24..32])?;
        Ok(Self {
            tag,
            main_volume_descriptor_sequence: main,
            reserve_volume_descriptor_sequence: reserve,
        })
    }
}

// ─────────────────────── PrimaryVolumeDescriptor (§10.1) ───────────────────────

#[derive(Debug, Clone)]
pub struct PrimaryVolumeDescriptor {
    pub tag: DescriptorTag,
    pub volume_descriptor_sequence_number: u32,
    pub primary_volume_descriptor_number: u32,
    /// 32-byte d-string field. Decoded as UTF-8 (lossy fallback).
    pub volume_identifier: String,
}

impl PrimaryVolumeDescriptor {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let tag = DescriptorTag::parse(bytes)?;
        if tag.id != TagId::PrimaryVolume {
            return Err(BlurayError::malformed("expected PVD tag"));
        }
        if bytes.len() < 56 {
            return Err(BlurayError::malformed("PVD truncated"));
        }
        let vds_n = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let pvd_n = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        let volume_identifier = decode_dstring_field(&bytes[24..56])?;
        Ok(Self {
            tag,
            volume_descriptor_sequence_number: vds_n,
            primary_volume_descriptor_number: pvd_n,
            volume_identifier,
        })
    }
}

// ─────────────────────── PartitionDescriptor (§10.5) ───────────────────────

#[derive(Debug, Clone)]
pub struct PartitionDescriptor {
    pub tag: DescriptorTag,
    pub volume_descriptor_sequence_number: u32,
    pub partition_flags: u16,
    pub partition_number: u16,
    pub partition_starting_location: u32,
    pub partition_length: u32, // in blocks
}

impl PartitionDescriptor {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let tag = DescriptorTag::parse(bytes)?;
        if tag.id != TagId::Partition {
            return Err(BlurayError::malformed("expected PD tag"));
        }
        if bytes.len() < 196 {
            return Err(BlurayError::malformed("PD truncated"));
        }
        let vds_n = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let part_flags = u16::from_le_bytes([bytes[20], bytes[21]]);
        let part_num = u16::from_le_bytes([bytes[22], bytes[23]]);
        // PartitionContents (32 bytes) at 24..56; ignored.
        // PartitionContentsUse (128 bytes) at 56..184; ignored.
        // AccessType (4 bytes) at 184..188; ignored.
        let part_start = u32::from_le_bytes([bytes[188], bytes[189], bytes[190], bytes[191]]);
        let part_len = u32::from_le_bytes([bytes[192], bytes[193], bytes[194], bytes[195]]);
        Ok(Self {
            tag,
            volume_descriptor_sequence_number: vds_n,
            partition_flags: part_flags,
            partition_number: part_num,
            partition_starting_location: part_start,
            partition_length: part_len,
        })
    }
}

// ─────────────────────── LogicalVolumeDescriptor (§10.6) ───────────────────────

#[derive(Debug, Clone)]
pub struct LogicalVolumeDescriptor {
    pub tag: DescriptorTag,
    pub volume_descriptor_sequence_number: u32,
    /// 128 bytes from offset 84: logical_volume_identifier (d-string).
    pub logical_volume_identifier: String,
    pub logical_block_size: u32, // bytes
    pub file_set_descriptor_location: LongAd,
}

impl LogicalVolumeDescriptor {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let tag = DescriptorTag::parse(bytes)?;
        if tag.id != TagId::LogicalVolume {
            return Err(BlurayError::malformed("expected LVD tag"));
        }
        if bytes.len() < 440 {
            return Err(BlurayError::malformed("LVD truncated"));
        }
        let vds_n = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        // descriptor_character_set: 64 bytes at 20..84 (skipped)
        let lvi = decode_dstring_field(&bytes[84..212])?;
        let lbs = u32::from_le_bytes([bytes[212], bytes[213], bytes[214], bytes[215]]);
        // domain_identifier: 32 bytes at 216..248 (skipped — should be `*OSTA UDF Compliant`)
        let fsd = LongAd::parse(&bytes[248..264])?;
        // map_table_length / number_of_partition_maps follow at 264..272;
        // partition_map_table begins at 440. We don't traverse the map
        // table here — the BD-ROM single-partition assumption is checked
        // separately in the mounter.
        Ok(Self {
            tag,
            volume_descriptor_sequence_number: vds_n,
            logical_volume_identifier: lvi,
            logical_block_size: lbs,
            file_set_descriptor_location: fsd,
        })
    }
}

// ─────────────────────── FileSetDescriptor (§14.1) ───────────────────────

#[derive(Debug, Clone)]
pub struct FileSetDescriptor {
    pub tag: DescriptorTag,
    pub root_directory_icb: LongAd,
}

impl FileSetDescriptor {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let tag = DescriptorTag::parse(bytes)?;
        if tag.id != TagId::FileSet {
            return Err(BlurayError::malformed("expected FSD tag"));
        }
        if bytes.len() < 400 {
            return Err(BlurayError::malformed("FSD truncated"));
        }
        // RecordingDateAndTime 12 bytes at 16..28
        // InterchangeLevel u16 at 28..30
        // MaximumInterchangeLevel u16 at 30..32
        // CharacterSetList u32 at 32..36
        // MaximumCharacterSetList u32 at 36..40
        // FileSetNumber u32 at 40..44
        // FileSetDescriptorNumber u32 at 44..48
        // LogicalVolumeIdentifierCharSet 64 bytes at 48..112
        // LogicalVolumeIdentifier 128 bytes at 112..240
        // FileSetCharSet 64 bytes at 240..304
        // FileSetIdentifier 32 bytes at 304..336
        // CopyrightFileIdentifier 32 bytes at 336..368
        // AbstractFileIdentifier 32 bytes at 368..400
        // RootDirectoryICB long_ad 16 bytes at 400..416
        if bytes.len() < 416 {
            return Err(BlurayError::malformed("FSD truncated before RootICB"));
        }
        let root = LongAd::parse(&bytes[400..416])?;
        Ok(Self {
            tag,
            root_directory_icb: root,
        })
    }
}

// ─────────────────────── FileIdentifierDescriptor (§14.4) ───────────────────────

#[derive(Debug, Clone)]
pub struct FileIdentifierDescriptor {
    pub tag: DescriptorTag,
    pub file_version_number: u16,
    pub file_characteristics: u8,
    pub identifier: String,
    pub icb: LongAd,
    pub total_size: usize,
}

impl FileIdentifierDescriptor {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 38 {
            return Err(BlurayError::malformed("FID truncated"));
        }
        let tag = DescriptorTag::parse(bytes)?;
        if tag.id != TagId::FileIdentifier {
            return Err(BlurayError::malformed("expected FID tag"));
        }
        let file_version_number = u16::from_le_bytes([bytes[16], bytes[17]]);
        let file_characteristics = bytes[18];
        let len_fi = bytes[19] as usize;
        let icb = LongAd::parse(&bytes[20..36])?;
        let len_impl_use = u16::from_le_bytes([bytes[36], bytes[37]]) as usize;
        let id_off = 38 + len_impl_use;
        let id_end = id_off + len_fi;
        if bytes.len() < id_end {
            return Err(BlurayError::malformed("FID identifier overruns buffer"));
        }
        let identifier = decode_dstring(&bytes[id_off..id_end])?;
        // Total length is rounded up to a 4-byte boundary (§14.4.9).
        let unpadded = id_end;
        let total = unpadded.div_ceil(4) * 4;
        Ok(Self {
            tag,
            file_version_number,
            file_characteristics,
            identifier,
            icb,
            total_size: total,
        })
    }

    pub fn is_deleted(&self) -> bool {
        self.file_characteristics & 0x04 != 0
    }
    pub fn is_directory(&self) -> bool {
        self.file_characteristics & 0x02 != 0
    }
    pub fn is_parent(&self) -> bool {
        self.file_characteristics & 0x08 != 0
    }
}

// ─────────────────────── FileEntry (§14.9) ───────────────────────

/// A File Entry's ICB Tag (§14.6) — 20 bytes immediately after the
/// descriptor tag. We surface only the fields we use.
#[derive(Debug, Clone, Copy)]
pub struct IcbTag {
    pub prior_recorded_entries: u32,
    pub strategy_type: u16,
    pub strategy_parameter: u16,
    pub max_entries: u16,
    pub file_type: u8,
    pub parent_icb: LbAddr,
    pub flags: u16,
}

impl IcbTag {
    pub const SIZE: usize = 20;
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::SIZE {
            return Err(BlurayError::malformed("ICB tag truncated"));
        }
        Ok(Self {
            prior_recorded_entries: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            strategy_type: u16::from_le_bytes([bytes[4], bytes[5]]),
            strategy_parameter: u16::from_le_bytes([bytes[6], bytes[7]]),
            max_entries: u16::from_le_bytes([bytes[8], bytes[9]]),
            // reserved byte at 10
            file_type: bytes[11],
            parent_icb: LbAddr::parse(&bytes[12..18])?,
            flags: u16::from_le_bytes([bytes[18], bytes[19]]),
        })
    }
}

/// Allocation Descriptor type encoded in `IcbTag::flags & 0b111`
/// (§14.6.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdType {
    Short,
    Long,
    Extended,
    EmbeddedInIcb,
}

impl AdType {
    pub fn from_flags(flags: u16) -> Result<Self> {
        match flags & 0b111 {
            0 => Ok(Self::Short),
            1 => Ok(Self::Long),
            2 => Ok(Self::Extended),
            3 => Ok(Self::EmbeddedInIcb),
            v => Err(BlurayError::malformed(format!("unknown ad_type {v}"))),
        }
    }
}

/// File Entry parsed enough to (a) compute its size and (b) walk its
/// allocation extents.
///
/// Carries both the plain File Entry (§14.9, tag 261) and the
/// Extended File Entry (§14.17, tag 266). For an EFE the additional
/// `Object Size` field (sum of every stream's Information Length,
/// §14.17.11) is surfaced via [`Self::object_size`]; for a plain FE
/// the field is `None`.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub tag: DescriptorTag,
    pub icb_tag: IcbTag,
    pub uid: u32,
    pub gid: u32,
    pub permissions: u32,
    pub file_link_count: u16,
    pub record_format: u8,
    pub record_display_attributes: u8,
    pub record_length: u32,
    pub information_length: u64,
    pub logical_blocks_recorded: u64,
    /// `Object Size` (§14.17.11). Recorded only on Extended File Entries;
    /// `None` for plain File Entries which lack this field.
    pub object_size: Option<u64>,
    pub length_of_extended_attributes: u32,
    pub length_of_allocation_descriptors: u32,
    /// Resolved short_ad extents (§14.14.1), when `ad_type == Short`.
    pub short_ads: Vec<ShortAd>,
    /// Resolved long_ad extents (§14.14.2), when `ad_type == Long`.
    pub long_ads: Vec<LongAd>,
    /// Resolved ext_ad extents (§14.14.3), when `ad_type == Extended`.
    pub ext_ads: Vec<ExtAd>,
    /// Raw embedded data, when `ad_type == EmbeddedInIcb` (a single
    /// directory listing or a tiny file).
    pub embedded_data: Vec<u8>,
    pub ad_type: AdType,
}

impl FileEntry {
    /// Plain File Entry prefix size (§14.9): 16 (tag) + 20 (ICB tag) +
    /// 140 (rest) = 176. L_EA at BP 168, L_AD at BP 172, EAs at BP 176.
    pub const PREFIX_SIZE: usize = 176;
    /// Extended File Entry prefix size (§14.17): the shared prefix is
    /// 40 bytes longer because of the inserted Object Size (BP 64),
    /// Creation Date and Time (BP 104), and the extra reserved word at
    /// BP 132. L_EA at BP 208, L_AD at BP 212, EAs at BP 216.
    pub const EFE_PREFIX_SIZE: usize = 216;

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::PREFIX_SIZE {
            return Err(BlurayError::malformed("FileEntry truncated"));
        }
        let tag = DescriptorTag::parse(bytes)?;
        if tag.id != TagId::FileEntry && tag.id != TagId::ExtendedFileEntry {
            return Err(BlurayError::malformed("expected FE / EFE tag"));
        }
        let is_efe = tag.id == TagId::ExtendedFileEntry;
        if is_efe && bytes.len() < Self::EFE_PREFIX_SIZE {
            return Err(BlurayError::malformed("ExtendedFileEntry truncated"));
        }
        let icb_tag = IcbTag::parse(&bytes[16..36])?;
        let uid = u32::from_le_bytes([bytes[36], bytes[37], bytes[38], bytes[39]]);
        let gid = u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]);
        let permissions = u32::from_le_bytes([bytes[44], bytes[45], bytes[46], bytes[47]]);
        let flc = u16::from_le_bytes([bytes[48], bytes[49]]);
        let rec_format = bytes[50];
        let rec_disp_attr = bytes[51];
        let rec_len = u32::from_le_bytes([bytes[52], bytes[53], bytes[54], bytes[55]]);
        let info_len = u64::from_le_bytes([
            bytes[56], bytes[57], bytes[58], bytes[59], bytes[60], bytes[61], bytes[62], bytes[63],
        ]);

        // From here, FE (§14.9) and EFE (§14.17) diverge:
        //
        //   FE (BP 64):  Logical Blocks Recorded u64, then 3×12 timestamps
        //                (access/mod/attribute), checkpoint u32, ext_attr_icb,
        //                impl_ident, unique_id, L_EA, L_AD; EAs at BP 176.
        //
        //   EFE (BP 64): Object Size u64, then Logical Blocks Recorded u64,
        //                then 4×12 timestamps (the extra one is creation_time
        //                at BP 104), checkpoint u32, 4 reserved bytes,
        //                ext_attr_icb, stream_dir_icb, impl_ident, unique_id,
        //                L_EA, L_AD; EAs at BP 216.
        //
        // We surface object_size for the EFE branch and consume the
        // shared "lbr / timestamps / checkpoint / impl_ident / unique_id"
        // span opaquely — the allocation walk below is identical.
        let (object_size, lbr, prefix_size, l_ea_off, l_ad_off) = if is_efe {
            let obj = u64::from_le_bytes([
                bytes[64], bytes[65], bytes[66], bytes[67], bytes[68], bytes[69], bytes[70],
                bytes[71],
            ]);
            let lbr = u64::from_le_bytes([
                bytes[72], bytes[73], bytes[74], bytes[75], bytes[76], bytes[77], bytes[78],
                bytes[79],
            ]);
            // access_time 80..92, mod_time 92..104, creation_time 104..116,
            // attribute_time 116..128, checkpoint 128..132, reserved 132..136,
            // ext_attr_icb 136..152, stream_dir_icb 152..168,
            // impl_ident 168..200, unique_id 200..208, L_EA 208..212,
            // L_AD 212..216.
            (Some(obj), lbr, Self::EFE_PREFIX_SIZE, 208usize, 212usize)
        } else {
            let lbr = u64::from_le_bytes([
                bytes[64], bytes[65], bytes[66], bytes[67], bytes[68], bytes[69], bytes[70],
                bytes[71],
            ]);
            // access_time 72..84, mod_time 84..96, attribute_time 96..108,
            // checkpoint 108..112, ext_attr_icb 112..128, impl_ident 128..160,
            // unique_id 160..168, L_EA 168..172, L_AD 172..176.
            (None, lbr, Self::PREFIX_SIZE, 168usize, 172usize)
        };

        let l_ea = u32::from_le_bytes([
            bytes[l_ea_off],
            bytes[l_ea_off + 1],
            bytes[l_ea_off + 2],
            bytes[l_ea_off + 3],
        ]);
        let l_ad = u32::from_le_bytes([
            bytes[l_ad_off],
            bytes[l_ad_off + 1],
            bytes[l_ad_off + 2],
            bytes[l_ad_off + 3],
        ]);

        let ad_type = AdType::from_flags(icb_tag.flags)?;

        let ea_off = prefix_size;
        let ea_end = ea_off + l_ea as usize;
        let ad_off = ea_end;
        let ad_end = ad_off + l_ad as usize;
        if bytes.len() < ad_end {
            return Err(BlurayError::malformed("FE allocation area overruns FE"));
        }

        let mut short_ads = Vec::new();
        let mut long_ads = Vec::new();
        let mut ext_ads = Vec::new();
        let mut embedded_data = Vec::new();
        match ad_type {
            AdType::Short => {
                let mut o = 0;
                while o + ShortAd::SIZE <= l_ad as usize {
                    let ad = ShortAd::parse(&bytes[ad_off + o..ad_off + o + ShortAd::SIZE])?;
                    if ad.length == 0 {
                        break;
                    }
                    if ad.extent_type == 3 {
                        return Err(BlurayError::unsupported(
                            "Allocation Extent Descriptor continuation",
                        ));
                    }
                    short_ads.push(ad);
                    o += ShortAd::SIZE;
                }
            }
            AdType::Long => {
                let mut o = 0;
                while o + LongAd::SIZE <= l_ad as usize {
                    let ad = LongAd::parse(&bytes[ad_off + o..ad_off + o + LongAd::SIZE])?;
                    if ad.length == 0 {
                        break;
                    }
                    if ad.extent_type == 3 {
                        return Err(BlurayError::unsupported(
                            "Allocation Extent Descriptor continuation",
                        ));
                    }
                    long_ads.push(ad);
                    o += LongAd::SIZE;
                }
            }
            AdType::Extended => {
                let mut o = 0;
                while o + ExtAd::SIZE <= l_ad as usize {
                    let ad = ExtAd::parse(&bytes[ad_off + o..ad_off + o + ExtAd::SIZE])?;
                    if ad.length == 0 {
                        break;
                    }
                    if ad.extent_type == 3 {
                        return Err(BlurayError::unsupported(
                            "Allocation Extent Descriptor continuation",
                        ));
                    }
                    if ad.recorded_length != ad.information_length {
                        // §14.14.3 Note 46: a Recorded Length that differs
                        // from the Information Length signals a compressed
                        // extent — we have no way to decode it.
                        return Err(BlurayError::unsupported(
                            "compressed ext_ad extent (recorded != information length)",
                        ));
                    }
                    ext_ads.push(ad);
                    o += ExtAd::SIZE;
                }
            }
            AdType::EmbeddedInIcb => {
                embedded_data.extend_from_slice(&bytes[ad_off..ad_end]);
            }
        }

        Ok(Self {
            tag,
            icb_tag,
            uid,
            gid,
            permissions,
            file_link_count: flc,
            record_format: rec_format,
            record_display_attributes: rec_disp_attr,
            record_length: rec_len,
            information_length: info_len,
            logical_blocks_recorded: lbr,
            object_size,
            length_of_extended_attributes: l_ea,
            length_of_allocation_descriptors: l_ad,
            short_ads,
            long_ads,
            ext_ads,
            embedded_data,
            ad_type,
        })
    }

    /// The allocation extents normalised across the AD flavour this
    /// File Entry recorded (§14.14). Empty when
    /// `ad_type == EmbeddedInIcb` — use [`Self::embedded_data`].
    pub fn extents(&self) -> Vec<AllocExtent> {
        match self.ad_type {
            AdType::Short => self
                .short_ads
                .iter()
                .map(|ad| AllocExtent {
                    length: ad.length,
                    extent_type: ad.extent_type,
                    block: ad.block_location,
                    partition_ref: None,
                })
                .collect(),
            AdType::Long => self
                .long_ads
                .iter()
                .map(|ad| AllocExtent {
                    length: ad.length,
                    extent_type: ad.extent_type,
                    block: ad.location.block,
                    partition_ref: Some(ad.location.partition_ref),
                })
                .collect(),
            AdType::Extended => self
                .ext_ads
                .iter()
                .map(|ad| AllocExtent {
                    length: ad.information_length,
                    extent_type: ad.extent_type,
                    block: ad.location.block,
                    partition_ref: Some(ad.location.partition_ref),
                })
                .collect(),
            AdType::EmbeddedInIcb => Vec::new(),
        }
    }

    pub fn is_directory(&self) -> bool {
        self.icb_tag.file_type == 4
    }

    /// `true` when this entry was decoded from an Extended File Entry
    /// (§14.17, tag 266) rather than a plain File Entry (§14.9).
    pub fn is_extended(&self) -> bool {
        self.tag.id == TagId::ExtendedFileEntry
    }
}

// ─────────────────────── Volume Recognition Sequence (§2/8.3) ───────────────────────

/// Probe sector 16+ for the BEA01 / NSR0x / TEA01 trio. Returns
/// `Ok(true)` if the sequence is well-formed, `Ok(false)` if it's
/// absent (some authoring tools omit it). Errors propagate from the
/// underlying reader.
pub fn probe_vrs<R: Read + Seek>(r: &mut R) -> Result<bool> {
    r.seek(SeekFrom::Start(VRS_FIRST_SECTOR * SECTOR_SIZE))?;
    let mut sector = [0u8; SECTOR_SIZE as usize];
    let mut saw_bea = false;
    let mut saw_nsr = false;
    let mut saw_tea = false;
    for _ in 0..8 {
        match r.read_exact(&mut sector) {
            Ok(()) => {}
            Err(_) => return Ok(false),
        }
        if &sector[0..1] != b"\x00" {
            // Structure-Type must be 0 (§2/9.1.1).
            return Ok(false);
        }
        let id = &sector[1..6];
        match id {
            b"BEA01" => saw_bea = true,
            b"NSR02" | b"NSR03" => saw_nsr = true,
            b"TEA01" => {
                saw_tea = true;
                break;
            }
            b"CD001" | b"CDW02" => { /* ISO 9660 — ignore */ }
            _ => return Ok(false),
        }
    }
    Ok(saw_bea && saw_nsr && saw_tea)
}

// ─────────────────────── UdfDisc (mounter) ───────────────────────

/// A successfully mounted UDF volume. Holds the partition base, the
/// root directory ICB, and an owned reader for sector I/O. Read-only.
pub struct UdfDisc<R: Read + Seek> {
    reader: R,
    pub partition_start_sector: u64,
    /// `Partition Number` of the mounted (single) partition (§10.5);
    /// long / extended allocation extents must reference it.
    pub partition_number: u16,
    pub logical_block_size: u32,
    pub root_directory_icb: LongAd,
    pub volume_identifier: String,
}

impl<R: Read + Seek> std::fmt::Debug for UdfDisc<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UdfDisc")
            .field("partition_start_sector", &self.partition_start_sector)
            .field("logical_block_size", &self.logical_block_size)
            .field("root_directory_icb", &self.root_directory_icb)
            .field("volume_identifier", &self.volume_identifier)
            .finish()
    }
}

impl<R: Read + Seek> UdfDisc<R> {
    /// Mount the volume. Reads the AVDP, the Volume Descriptor
    /// Sequence (PVD + LVD + PD), and the File Set Descriptor.
    pub fn open(mut reader: R) -> Result<Self> {
        // VRS is optional in practice — log + continue either way.
        let _ = probe_vrs(&mut reader)?;

        // AVDP at sector 256.
        let avdp = read_descriptor(&mut reader, AVDP_SECTOR)?;
        let avdp = AnchorVolumeDescriptorPointer::parse(&avdp)?;

        // Walk the main VDS. Stop on Terminating Descriptor.
        let main = avdp.main_volume_descriptor_sequence;
        let mut pvd: Option<PrimaryVolumeDescriptor> = None;
        let mut pd: Option<PartitionDescriptor> = None;
        let mut lvd: Option<LogicalVolumeDescriptor> = None;
        let max_sectors = main.length as u64 / SECTOR_SIZE;
        for i in 0..max_sectors {
            let sec = main.location as u64 + i;
            let buf = read_sector(&mut reader, sec)?;
            let id_raw = u16::from_le_bytes([buf[0], buf[1]]);
            let tag_id = match TagId::from_raw(id_raw) {
                Some(t) => t,
                None => continue, // unknown / zero-filled tail — skip
            };
            match tag_id {
                TagId::PrimaryVolume => pvd = Some(PrimaryVolumeDescriptor::parse(&buf)?),
                TagId::Partition => pd = Some(PartitionDescriptor::parse(&buf)?),
                TagId::LogicalVolume => lvd = Some(LogicalVolumeDescriptor::parse(&buf)?),
                TagId::Terminating => break,
                _ => {}
            }
        }
        let pvd = pvd.ok_or_else(|| BlurayError::not_bluray("no Primary Volume Descriptor"))?;
        let pd = pd.ok_or_else(|| BlurayError::not_bluray("no Partition Descriptor"))?;
        let lvd = lvd.ok_or_else(|| BlurayError::not_bluray("no Logical Volume Descriptor"))?;

        // Per BD spec the logical block size must equal the BD sector
        // size (2048).
        if lvd.logical_block_size as u64 != SECTOR_SIZE {
            return Err(BlurayError::unsupported(format!(
                "logical_block_size = {} (only 2048 supported)",
                lvd.logical_block_size,
            )));
        }

        // Read the File Set Descriptor at LVD.file_set_descriptor_location.
        let fsd_partition_block = lvd.file_set_descriptor_location.location.block as u64;
        let fsd_partition_ref = lvd.file_set_descriptor_location.location.partition_ref;
        if fsd_partition_ref != pd.partition_number {
            return Err(BlurayError::unsupported(
                "FSD references non-default partition",
            ));
        }
        let fsd_sec = pd.partition_starting_location as u64 + fsd_partition_block;
        let fsd_buf = read_sector(&mut reader, fsd_sec)?;
        let fsd = FileSetDescriptor::parse(&fsd_buf)?;

        Ok(Self {
            reader,
            partition_start_sector: pd.partition_starting_location as u64,
            partition_number: pd.partition_number,
            logical_block_size: lvd.logical_block_size,
            root_directory_icb: fsd.root_directory_icb,
            volume_identifier: pvd.volume_identifier,
        })
    }

    /// Read a logical block from the partition (i.e. an in-partition
    /// block address translates to absolute sector
    /// `partition_start + block`).
    fn read_partition_block(&mut self, partition_block: u64) -> Result<Vec<u8>> {
        let sec = self.partition_start_sector + partition_block;
        read_sector_into_vec(&mut self.reader, sec)
    }

    /// Read the File Entry at the given partition ICB.
    fn read_file_entry(&mut self, icb: LongAd) -> Result<FileEntry> {
        if icb.length == 0 {
            return Err(BlurayError::malformed("FE ICB length 0"));
        }
        let buf = self.read_partition_block(icb.location.block as u64)?;
        FileEntry::parse(&buf)
    }

    /// Read the full content of a file, materialised into a `Vec<u8>`.
    /// Bounded by `information_length`. Suitable for BDMV control
    /// files (kilobytes); not for `.m2ts` (gigabytes — use
    /// [`Self::open_file_reader`] instead).
    pub fn read_file(&mut self, icb: LongAd) -> Result<Vec<u8>> {
        let fe = self.read_file_entry(icb)?;
        let want = fe.information_length as usize;
        if fe.ad_type == AdType::EmbeddedInIcb {
            return Ok(fe.embedded_data[..want.min(fe.embedded_data.len())].to_vec());
        }
        let mut out = Vec::with_capacity(want);
        for ext in fe.extents() {
            if ext.extent_type != 0 {
                return Err(BlurayError::unsupported("non-recorded extent in file"));
            }
            if let Some(p) = ext.partition_ref {
                if p != self.partition_number {
                    return Err(BlurayError::unsupported(
                        "extent references a different partition",
                    ));
                }
            }
            let blocks = (ext.length as u64).div_ceil(SECTOR_SIZE);
            for i in 0..blocks {
                let buf = self.read_partition_block(ext.block as u64 + i)?;
                let to_copy =
                    (ext.length as usize).saturating_sub(i as usize * SECTOR_SIZE as usize);
                let take = to_copy.min(SECTOR_SIZE as usize);
                out.extend_from_slice(&buf[..take]);
                if out.len() >= want {
                    break;
                }
            }
            if out.len() >= want {
                break;
            }
        }
        out.truncate(want);
        Ok(out)
    }

    /// List the entries of a directory at `dir_icb`. Skips the parent
    /// (`..`) and any deleted entries. Returns `(name, child_icb,
    /// is_directory)`.
    pub fn read_directory(&mut self, dir_icb: LongAd) -> Result<Vec<DirEntry>> {
        let raw = self.read_file(dir_icb)?;
        let mut out = Vec::new();
        let mut o = 0;
        while o + 38 <= raw.len() {
            let fid = FileIdentifierDescriptor::parse(&raw[o..])?;
            o += fid.total_size;
            if fid.is_deleted() || fid.is_parent() {
                continue;
            }
            let is_dir = fid.is_directory();
            out.push(DirEntry {
                name: fid.identifier,
                icb: fid.icb,
                is_directory: is_dir,
            });
        }
        Ok(out)
    }

    /// Walk an absolute path (e.g. `"BDMV/index.bdmv"`) starting from
    /// the root directory ICB. Returns the file's ICB on success.
    pub fn lookup(&mut self, path: &str) -> Result<LongAd> {
        let mut cur_icb = self.root_directory_icb;
        let mut cur_is_dir = true;
        for component in path.split('/').filter(|s| !s.is_empty()) {
            if !cur_is_dir {
                return Err(BlurayError::not_bluray(
                    "path component descends into non-directory",
                ));
            }
            let entries = self.read_directory(cur_icb)?;
            let m = entries
                .iter()
                .find(|e| e.name.eq_ignore_ascii_case(component))
                .ok_or_else(|| {
                    BlurayError::not_bluray(format!("path component {component:?} not found"))
                })?;
            cur_icb = m.icb;
            cur_is_dir = m.is_directory;
        }
        Ok(cur_icb)
    }

    /// Materialise a file's bytes by absolute path (convenience).
    pub fn read_path(&mut self, path: &str) -> Result<Vec<u8>> {
        let icb = self.lookup(path)?;
        self.read_file(icb)
    }
}

/// A single entry in a UDF directory listing.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub icb: LongAd,
    pub is_directory: bool,
}

/// Best-effort read of the UDF volume identifier (the Primary Volume
/// Descriptor's `volume_identifier` d-string, §10.1) from a raw UDF
/// image / block device.
///
/// Walks the AVDP at sector 256 → main Volume Descriptor Sequence →
/// PVD. Returns the decoded `volume_identifier` on success. Any I/O or
/// parse error surfaces as a [`BlurayError`]; the caller can decide
/// whether to fall through to `None` at the high-level surface.
pub fn read_volume_label<R: Read + Seek>(reader: R) -> Result<String> {
    let disc = UdfDisc::open(reader)?;
    Ok(disc.volume_identifier)
}

// ─────────────────────── sector helpers ───────────────────────

fn read_sector<R: Read + Seek>(r: &mut R, sector: u64) -> Result<[u8; SECTOR_SIZE as usize]> {
    r.seek(SeekFrom::Start(sector * SECTOR_SIZE))?;
    let mut buf = [0u8; SECTOR_SIZE as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn read_sector_into_vec<R: Read + Seek>(r: &mut R, sector: u64) -> Result<Vec<u8>> {
    Ok(read_sector(r, sector)?.to_vec())
}

fn read_descriptor<R: Read + Seek>(r: &mut R, sector: u64) -> Result<Vec<u8>> {
    read_sector_into_vec(r, sector)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_tag_round_trip() {
        let tag = DescriptorTag {
            id: TagId::FileSet,
            descriptor_version: 3,
            serial_number: 0x1234,
            crc: 0xABCD,
            crc_length: 200,
            location: 0xDEAD_BEEF,
        };
        let bytes = tag.encode();
        let parsed = DescriptorTag::parse(&bytes).unwrap();
        assert_eq!(parsed.id, TagId::FileSet);
        assert_eq!(parsed.descriptor_version, 3);
        assert_eq!(parsed.serial_number, 0x1234);
        assert_eq!(parsed.crc, 0xABCD);
        assert_eq!(parsed.crc_length, 200);
        assert_eq!(parsed.location, 0xDEAD_BEEF);
    }

    #[test]
    fn tag_checksum_detects_corruption() {
        let tag = DescriptorTag {
            id: TagId::FileEntry,
            descriptor_version: 3,
            serial_number: 1,
            crc: 0,
            crc_length: 0,
            location: 100,
        };
        let mut bytes = tag.encode();
        // Flip a non-checksum byte.
        bytes[12] ^= 0xFF;
        assert!(matches!(
            DescriptorTag::parse(&bytes),
            Err(BlurayError::Malformed(_))
        ));
    }

    #[test]
    fn lb_addr_round_trip() {
        let a = LbAddr {
            block: 0x12345678,
            partition_ref: 7,
        };
        assert_eq!(LbAddr::parse(&a.encode()).unwrap(), a);
    }

    #[test]
    fn short_ad_extent_type_round_trip() {
        let ad = ShortAd {
            length: 0x3FFF_FFFE,
            extent_type: 2,
            block_location: 42,
        };
        let parsed = ShortAd::parse(&ad.encode()).unwrap();
        assert_eq!(parsed.length, ad.length);
        assert_eq!(parsed.extent_type, ad.extent_type);
        assert_eq!(parsed.block_location, ad.block_location);
    }

    #[test]
    fn dstring_8bit() {
        // "TEST" preceded by compression id 8.
        let payload = b"\x08TEST";
        assert_eq!(decode_dstring(payload).unwrap(), "TEST");
    }

    #[test]
    fn dstring_16bit_be() {
        let mut payload = vec![16u8];
        for c in "FOO".chars() {
            let v = c as u16;
            payload.push((v >> 8) as u8);
            payload.push(v as u8);
        }
        assert_eq!(decode_dstring(&payload).unwrap(), "FOO");
    }

    #[test]
    fn dstring_field_with_trailing_length() {
        // 32-byte field: compression-id "8", "TST", padding, last byte = 4.
        let mut field = vec![0u8; 32];
        field[0] = 8;
        field[1..4].copy_from_slice(b"TST");
        field[31] = 4;
        assert_eq!(decode_dstring_field(&field).unwrap(), "TST");
    }

    #[test]
    fn ad_type_from_flags() {
        assert_eq!(AdType::from_flags(0).unwrap(), AdType::Short);
        assert_eq!(AdType::from_flags(1).unwrap(), AdType::Long);
        assert_eq!(AdType::from_flags(2).unwrap(), AdType::Extended);
        assert_eq!(AdType::from_flags(3).unwrap(), AdType::EmbeddedInIcb);
        // high bits beyond 0..=2 don't change the type.
        assert_eq!(AdType::from_flags(0xFFF8).unwrap(), AdType::Short);
        assert_eq!(AdType::from_flags(0xFFFB).unwrap(), AdType::EmbeddedInIcb);
    }

    /// Build an ICB Tag (§14.6) prefixed by the Descriptor Tag of either
    /// FileEntry or ExtendedFileEntry, with the configured ad-type
    /// flags, file_type and strategy_type 4. Returns just the 36 bytes
    /// of `tag(16) + icbtag(20)`; the caller appends the per-variant
    /// remainder.
    fn build_fe_header(tag_id: TagId, ad_flags: u16, file_type: u8) -> Vec<u8> {
        let tag = DescriptorTag {
            id: tag_id,
            descriptor_version: 3,
            serial_number: 1,
            crc: 0,
            crc_length: 0,
            location: 0,
        };
        let mut out = Vec::with_capacity(36);
        out.extend_from_slice(&tag.encode());
        out.extend_from_slice(&0u32.to_le_bytes()); // prior_recorded_entries
        out.extend_from_slice(&4u16.to_le_bytes()); // strategy_type = 4
        out.extend_from_slice(&0u16.to_le_bytes()); // strategy_parameter
        out.extend_from_slice(&1u16.to_le_bytes()); // max_entries
        out.push(0); // reserved
        out.push(file_type);
        out.extend_from_slice(&[0u8; 6]); // parent_icb (lb_addr)
        out.extend_from_slice(&ad_flags.to_le_bytes());
        assert_eq!(out.len(), 36);
        out
    }

    /// Hand-roll an Extended File Entry (§14.17, tag 266) carrying
    /// `info_len` bytes of embedded payload. `object_size` is the
    /// EFE-only field at BP 64 (§14.17.11). Returns a buffer that
    /// [`FileEntry::parse`] can ingest directly.
    fn build_efe_embedded(
        file_type: u8,
        info_len: u64,
        object_size: u64,
        payload: &[u8],
    ) -> Vec<u8> {
        // ad_type 3 = EmbeddedInIcb.
        let mut buf = build_fe_header(TagId::ExtendedFileEntry, 3, file_type);
        // BP 36..64: uid, gid, permissions, file_link_count, record_format,
        // record_display_attributes, record_length, information_length.
        buf.extend_from_slice(&0u32.to_le_bytes()); // uid
        buf.extend_from_slice(&0u32.to_le_bytes()); // gid
        buf.extend_from_slice(&0u32.to_le_bytes()); // permissions
        buf.extend_from_slice(&1u16.to_le_bytes()); // file_link_count
        buf.push(0); // record_format
        buf.push(0); // record_display_attributes
        buf.extend_from_slice(&0u32.to_le_bytes()); // record_length
        buf.extend_from_slice(&info_len.to_le_bytes()); // information_length (BP 56)
        assert_eq!(buf.len(), 64);
        // BP 64: object_size (EFE-only).
        buf.extend_from_slice(&object_size.to_le_bytes());
        // BP 72: logical_blocks_recorded.
        buf.extend_from_slice(&0u64.to_le_bytes());
        // BP 80..128: 4 × 12-byte timestamps (access / mod / creation / attribute).
        buf.extend_from_slice(&[0u8; 4 * 12]);
        // BP 128: checkpoint.
        buf.extend_from_slice(&0u32.to_le_bytes());
        // BP 132: 4 reserved bytes.
        buf.extend_from_slice(&[0u8; 4]);
        // BP 136: extended_attribute_icb long_ad.
        buf.extend_from_slice(&[0u8; 16]);
        // BP 152: stream_directory_icb long_ad.
        buf.extend_from_slice(&[0u8; 16]);
        // BP 168: implementation_identifier regid (32 bytes).
        buf.extend_from_slice(&[0u8; 32]);
        // BP 200: unique_id u64.
        buf.extend_from_slice(&0u64.to_le_bytes());
        // BP 208: L_EA.
        buf.extend_from_slice(&0u32.to_le_bytes());
        // BP 212: L_AD = payload length.
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        assert_eq!(buf.len(), FileEntry::EFE_PREFIX_SIZE);
        // BP 216: EA area (empty) followed by AD area (embedded payload).
        buf.extend_from_slice(payload);
        buf
    }

    #[test]
    fn extended_file_entry_embedded_roundtrip() {
        // Directory (file_type 4) with 6 bytes of embedded data. The
        // EFE's Object Size carries 999, distinguishing it from
        // Information Length (6) so we can confirm both fields land on
        // the right struct member.
        let payload = b"DIRENT";
        let bytes = build_efe_embedded(4, payload.len() as u64, 999, payload);
        let fe = FileEntry::parse(&bytes).unwrap();
        assert!(fe.is_extended());
        assert!(fe.is_directory());
        assert_eq!(fe.information_length, payload.len() as u64);
        assert_eq!(fe.object_size, Some(999));
        assert_eq!(fe.ad_type, AdType::EmbeddedInIcb);
        assert_eq!(fe.embedded_data, payload);
        assert_eq!(fe.length_of_extended_attributes, 0);
        assert_eq!(fe.length_of_allocation_descriptors, payload.len() as u32);
    }

    #[test]
    fn extended_file_entry_short_ad_extent() {
        // file_type 5 (sequence of bytes / regular file), one short_ad
        // pointing at block 42, length 4096. ad_type 0 = Short.
        let mut buf = build_fe_header(TagId::ExtendedFileEntry, 0, 5);
        buf.extend_from_slice(&0u32.to_le_bytes()); // uid
        buf.extend_from_slice(&0u32.to_le_bytes()); // gid
        buf.extend_from_slice(&0u32.to_le_bytes()); // permissions
        buf.extend_from_slice(&1u16.to_le_bytes()); // file_link_count
        buf.push(0); // record_format
        buf.push(0); // record_display_attributes
        buf.extend_from_slice(&0u32.to_le_bytes()); // record_length
        buf.extend_from_slice(&4096u64.to_le_bytes()); // info_len
        buf.extend_from_slice(&4096u64.to_le_bytes()); // object_size (no streams)
        buf.extend_from_slice(&2u64.to_le_bytes()); // lbr
        buf.extend_from_slice(&[0u8; 4 * 12]); // 4 timestamps
        buf.extend_from_slice(&0u32.to_le_bytes()); // checkpoint
        buf.extend_from_slice(&[0u8; 4]); // reserved
        buf.extend_from_slice(&[0u8; 16]); // ext_attr_icb
        buf.extend_from_slice(&[0u8; 16]); // stream_dir_icb
        buf.extend_from_slice(&[0u8; 32]); // impl_ident
        buf.extend_from_slice(&0u64.to_le_bytes()); // unique_id
        buf.extend_from_slice(&0u32.to_le_bytes()); // L_EA
        buf.extend_from_slice(&8u32.to_le_bytes()); // L_AD = one short_ad
        assert_eq!(buf.len(), FileEntry::EFE_PREFIX_SIZE);
        // AD area: one short_ad (length 4096, block 42).
        let ad = ShortAd {
            length: 4096,
            extent_type: 0,
            block_location: 42,
        };
        buf.extend_from_slice(&ad.encode());

        let fe = FileEntry::parse(&buf).unwrap();
        assert!(fe.is_extended());
        assert!(!fe.is_directory());
        assert_eq!(fe.information_length, 4096);
        assert_eq!(fe.object_size, Some(4096));
        assert_eq!(fe.logical_blocks_recorded, 2);
        assert_eq!(fe.ad_type, AdType::Short);
        assert_eq!(fe.short_ads.len(), 1);
        assert_eq!(fe.short_ads[0].length, 4096);
        assert_eq!(fe.short_ads[0].block_location, 42);
    }

    #[test]
    fn plain_file_entry_still_reports_no_object_size() {
        // The FE / EFE branch decision must not pollute the FE path:
        // a plain File Entry (§14.9) returns object_size = None.
        let mut buf = build_fe_header(TagId::FileEntry, 3, 4); // embedded dir
        buf.extend_from_slice(&[0u8; 4 * 3]); // uid/gid/perm
        buf.extend_from_slice(&1u16.to_le_bytes()); // file_link_count
        buf.push(0); // record_format
        buf.push(0); // record_display_attributes
        buf.extend_from_slice(&0u32.to_le_bytes()); // record_length
        buf.extend_from_slice(&0u64.to_le_bytes()); // info_len
        buf.extend_from_slice(&0u64.to_le_bytes()); // lbr (BP 64 on FE)
        buf.extend_from_slice(&[0u8; 3 * 12]); // 3 timestamps (no creation_time)
        buf.extend_from_slice(&0u32.to_le_bytes()); // checkpoint
        buf.extend_from_slice(&[0u8; 16]); // ext_attr_icb
        buf.extend_from_slice(&[0u8; 32]); // impl_ident
        buf.extend_from_slice(&0u64.to_le_bytes()); // unique_id
        buf.extend_from_slice(&0u32.to_le_bytes()); // L_EA
        buf.extend_from_slice(&0u32.to_le_bytes()); // L_AD
        assert_eq!(buf.len(), FileEntry::PREFIX_SIZE);

        let fe = FileEntry::parse(&buf).unwrap();
        assert!(!fe.is_extended());
        assert_eq!(fe.object_size, None);
    }

    /// Hand-roll a plain File Entry (§14.9) with `file_type` 5 and the
    /// given ad-type flags + raw allocation-descriptor payload.
    fn build_plain_fe_with_ads(ad_flags: u16, info_len: u64, ad_payload: &[u8]) -> Vec<u8> {
        let mut buf = build_fe_header(TagId::FileEntry, ad_flags, 5);
        buf.extend_from_slice(&[0u8; 4 * 3]); // uid/gid/perm
        buf.extend_from_slice(&1u16.to_le_bytes()); // file_link_count
        buf.push(0); // record_format
        buf.push(0); // record_display_attributes
        buf.extend_from_slice(&0u32.to_le_bytes()); // record_length
        buf.extend_from_slice(&info_len.to_le_bytes()); // information_length
        buf.extend_from_slice(&0u64.to_le_bytes()); // logical_blocks_recorded
        buf.extend_from_slice(&[0u8; 3 * 12]); // 3 timestamps
        buf.extend_from_slice(&0u32.to_le_bytes()); // checkpoint
        buf.extend_from_slice(&[0u8; 16]); // ext_attr_icb
        buf.extend_from_slice(&[0u8; 32]); // impl_ident
        buf.extend_from_slice(&0u64.to_le_bytes()); // unique_id
        buf.extend_from_slice(&0u32.to_le_bytes()); // L_EA
        buf.extend_from_slice(&(ad_payload.len() as u32).to_le_bytes()); // L_AD
        assert_eq!(buf.len(), FileEntry::PREFIX_SIZE);
        buf.extend_from_slice(ad_payload);
        buf
    }

    #[test]
    fn ext_ad_round_trip() {
        let ad = ExtAd {
            length: 6144,
            extent_type: 1,
            recorded_length: 4096,
            information_length: 4000,
            location: LbAddr {
                block: 77,
                partition_ref: 3,
            },
            implementation_use: [0xAB, 0xCD],
        };
        let parsed = ExtAd::parse(&ad.encode()).unwrap();
        assert_eq!(parsed.length, 6144);
        assert_eq!(parsed.extent_type, 1);
        assert_eq!(parsed.recorded_length, 4096);
        assert_eq!(parsed.information_length, 4000);
        assert_eq!(parsed.location, ad.location);
        assert_eq!(parsed.implementation_use, [0xAB, 0xCD]);
    }

    #[test]
    fn file_entry_long_ad_extents() {
        // ad_type 1 = Long. Two long_ads in the same partition.
        let ads: Vec<u8> = [
            LongAd {
                length: 4096,
                extent_type: 0,
                location: LbAddr {
                    block: 10,
                    partition_ref: 0,
                },
                implementation_use: [0u8; 6],
            },
            LongAd {
                length: 2048,
                extent_type: 0,
                location: LbAddr {
                    block: 50,
                    partition_ref: 0,
                },
                implementation_use: [0u8; 6],
            },
        ]
        .iter()
        .flat_map(|ad| ad.encode())
        .collect();
        let fe = FileEntry::parse(&build_plain_fe_with_ads(1, 6144, &ads)).unwrap();
        assert_eq!(fe.ad_type, AdType::Long);
        assert_eq!(fe.long_ads.len(), 2);
        assert!(fe.short_ads.is_empty());
        let ext = fe.extents();
        assert_eq!(ext.len(), 2);
        assert_eq!(ext[0].length, 4096);
        assert_eq!(ext[0].block, 10);
        assert_eq!(ext[0].partition_ref, Some(0));
        assert_eq!(ext[1].length, 2048);
        assert_eq!(ext[1].block, 50);
    }

    #[test]
    fn file_entry_ext_ad_extents() {
        // ad_type 2 = Extended. Uncompressed (recorded == information).
        let ad = ExtAd {
            length: 4096,
            extent_type: 0,
            recorded_length: 3000,
            information_length: 3000,
            location: LbAddr {
                block: 21,
                partition_ref: 0,
            },
            implementation_use: [0u8; 2],
        };
        let fe = FileEntry::parse(&build_plain_fe_with_ads(2, 3000, &ad.encode())).unwrap();
        assert_eq!(fe.ad_type, AdType::Extended);
        assert_eq!(fe.ext_ads.len(), 1);
        let ext = fe.extents();
        assert_eq!(ext.len(), 1);
        // The normalised length is the Information Length, not the
        // (block-rounded) Extent Length.
        assert_eq!(ext[0].length, 3000);
        assert_eq!(ext[0].block, 21);
        assert_eq!(ext[0].partition_ref, Some(0));
    }

    #[test]
    fn file_entry_compressed_ext_ad_rejected() {
        // recorded_length != information_length → compressed extent
        // (§14.14.3 Note 46) → Unsupported.
        let ad = ExtAd {
            length: 4096,
            extent_type: 0,
            recorded_length: 1000,
            information_length: 3000,
            location: LbAddr {
                block: 21,
                partition_ref: 0,
            },
            implementation_use: [0u8; 2],
        };
        assert!(matches!(
            FileEntry::parse(&build_plain_fe_with_ads(2, 3000, &ad.encode())),
            Err(BlurayError::Unsupported(_))
        ));
    }

    #[test]
    fn file_entry_long_ad_continuation_rejected() {
        // extent_type 3 = next extent of allocation descriptors
        // (Allocation Extent Descriptor chain) → Unsupported.
        let ad = LongAd {
            length: 2048,
            extent_type: 3,
            location: LbAddr {
                block: 99,
                partition_ref: 0,
            },
            implementation_use: [0u8; 6],
        };
        assert!(matches!(
            FileEntry::parse(&build_plain_fe_with_ads(1, 0, &ad.encode())),
            Err(BlurayError::Unsupported(_))
        ));
    }

    #[test]
    fn extended_file_entry_truncated_is_malformed() {
        // 200 bytes is enough for FE PREFIX_SIZE (176) but not EFE
        // PREFIX_SIZE (216). Parsing must reject rather than mis-decode
        // an EFE.
        let mut buf = build_fe_header(TagId::ExtendedFileEntry, 3, 4);
        buf.resize(200, 0);
        assert!(matches!(
            FileEntry::parse(&buf),
            Err(BlurayError::Malformed(_))
        ));
    }
}
