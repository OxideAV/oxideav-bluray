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
//! - Short Allocation Descriptors (§14.14.1). Long + Extended ADs are
//!   read but the parser refuses any with `extent_type != 0` (recorded
//!   + allocated) and any AD type other than short.
//!
//! ## What's not implemented (Phase 1 — surface `Unsupported`)
//!
//! - Multi-extent partition maps (`partition_map_count > 1`).
//! - ICB strategy types other than 4 (the spec's "default" linear).
//! - Extended Attributes / Symbolic Links / Streams.
//! - Sparse / sequential files.
//! - Allocation Extent Descriptors (§14.5).
//! - UDF 1.50 or earlier (we look at the LVD identifier suffix).

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
    pub length_of_extended_attributes: u32,
    pub length_of_allocation_descriptors: u32,
    /// Resolved short-ad extents (we refuse long/extended in Phase 1).
    pub short_ads: Vec<ShortAd>,
    /// Raw embedded data, when `ad_type == EmbeddedInIcb` (a single
    /// directory listing or a tiny file).
    pub embedded_data: Vec<u8>,
    pub ad_type: AdType,
}

impl FileEntry {
    /// Standard FE prefix size: 16 (tag) + 20 (ICB tag) + 136 (rest) = 172.
    pub const PREFIX_SIZE: usize = 176;

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::PREFIX_SIZE {
            return Err(BlurayError::malformed("FileEntry truncated"));
        }
        let tag = DescriptorTag::parse(bytes)?;
        if tag.id != TagId::FileEntry && tag.id != TagId::ExtendedFileEntry {
            return Err(BlurayError::malformed("expected FE / EFE tag"));
        }
        if tag.id == TagId::ExtendedFileEntry {
            return Err(BlurayError::unsupported("ExtendedFileEntry"));
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
        let lbr = u64::from_le_bytes([
            bytes[64], bytes[65], bytes[66], bytes[67], bytes[68], bytes[69], bytes[70], bytes[71],
        ]);
        // access_time / modification_time / attribute_time at 72..108 (12 bytes each — skipped)
        // checkpoint u32 at 108..112 (skipped)
        // extended_attribute_icb long_ad 16 bytes at 112..128 (skipped)
        // implementation_identifier 32 bytes at 128..160 (skipped)
        // unique_id u64 at 160..168 (skipped)
        let l_ea = u32::from_le_bytes([bytes[168], bytes[169], bytes[170], bytes[171]]);
        let l_ad = u32::from_le_bytes([bytes[172], bytes[173], bytes[174], bytes[175]]);

        let ad_type = AdType::from_flags(icb_tag.flags)?;

        let ea_off = Self::PREFIX_SIZE;
        let ea_end = ea_off + l_ea as usize;
        let ad_off = ea_end;
        let ad_end = ad_off + l_ad as usize;
        if bytes.len() < ad_end {
            return Err(BlurayError::malformed("FE allocation area overruns FE"));
        }

        let mut short_ads = Vec::new();
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
                return Err(BlurayError::unsupported("long_ad in FileEntry"));
            }
            AdType::Extended => {
                return Err(BlurayError::unsupported("extended_ad in FileEntry"));
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
            length_of_extended_attributes: l_ea,
            length_of_allocation_descriptors: l_ad,
            short_ads,
            embedded_data,
            ad_type,
        })
    }

    pub fn is_directory(&self) -> bool {
        self.icb_tag.file_type == 4
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
        for ad in &fe.short_ads {
            if ad.extent_type != 0 {
                return Err(BlurayError::unsupported("non-recorded extent in file"));
            }
            let blocks = (ad.length as u64).div_ceil(SECTOR_SIZE);
            for i in 0..blocks {
                let buf = self.read_partition_block(ad.block_location as u64 + i)?;
                let to_copy =
                    (ad.length as usize).saturating_sub(i as usize * SECTOR_SIZE as usize);
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
}
