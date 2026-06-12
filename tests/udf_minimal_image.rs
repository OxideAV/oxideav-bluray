//! Synthesise a minimal UDF 2.50 disc image entirely in memory and
//! verify that [`UdfDisc::open`] walks the volume descriptor sequence,
//! reads the File Set Descriptor, and looks up a file at a known path.
//!
//! Layout we build:
//!
//! ```text
//!   sector   0..15  zero-padded reserved area
//!   sector  16      VRS BEA01
//!   sector  17      VRS NSR03
//!   sector  18      VRS TEA01
//!   sector  19..255 zero
//!   sector 256      AVDP → main VDS at sector 257, length 4*2048
//!   sector 257      Primary Volume Descriptor
//!   sector 258      Partition Descriptor (start=300, length=200)
//!   sector 259      Logical Volume Descriptor (FSD at partition block 0)
//!   sector 260      Terminating Descriptor
//!   sector 300      File Set Descriptor (root dir ICB at partition block 1)
//!   sector 301      Root directory File Entry (embedded FIDs)
//!   sector 302      "TEST" File Entry (embedded payload)
//!   sector 303      "LONG" File Entry (long_ad → partition blocks 4..6)
//!   sector 304..305 "LONG" file body
//!   sector 306      "XPART" File Entry (long_ad naming partition_ref 1)
//!   sector 307      "CHAIN" File Entry (long_ad → block 8, then a
//!                   type-3 continuation pointer → block 9)
//!   sector 308      "CHAIN" body, first extent
//!   sector 309      Allocation Extent Descriptor (§14.5) + one more
//!                   long_ad → block 10
//!   sector 310      "CHAIN" body, second extent
//!   sector 311      "CYCLE" File Entry (type-3 pointer → block 12)
//!   sector 312      AED whose only descriptor is a type-3 pointer
//!                   back to block 12 (a cycle)
//! ```
//!
//! All bytes are entirely fabricated; no real disc data is read.

use std::io::Cursor;

use oxideav_bluray::udf::{
    AllocationExtentDescriptor, AnchorVolumeDescriptorPointer, DescriptorTag, ExtentAd, FileEntry,
    FileSetDescriptor, LbAddr, LogicalVolumeDescriptor, LongAd, PartitionDescriptor,
    PrimaryVolumeDescriptor, ShortAd, TagId, UdfDisc, AVDP_SECTOR, SECTOR_SIZE,
};

const SECTOR: usize = SECTOR_SIZE as usize;

fn place(image: &mut [u8], sector: u64, payload: &[u8]) {
    let off = (sector * SECTOR_SIZE) as usize;
    let end = off + payload.len();
    assert!(
        end <= image.len(),
        "payload at sector {sector} overruns image"
    );
    image[off..end].copy_from_slice(payload);
}

fn tag(id: TagId, location: u32, crc_length: u16) -> DescriptorTag {
    DescriptorTag {
        id,
        descriptor_version: 3,
        serial_number: 1,
        crc: 0,
        crc_length,
        location,
    }
}

/// Build a Primary Volume Descriptor (minimal — only fields used by
/// the parser are filled).
fn pvd_bytes(location: u32) -> Vec<u8> {
    let mut buf = vec![0u8; SECTOR];
    let t = tag(TagId::PrimaryVolume, location, 56);
    buf[0..16].copy_from_slice(&t.encode());
    // volume_descriptor_sequence_number, primary_volume_descriptor_number = 1, 1
    buf[16..20].copy_from_slice(&1u32.to_le_bytes());
    buf[20..24].copy_from_slice(&1u32.to_le_bytes());
    // volume_identifier: 8-bit d-string "SYN", padded, length byte at 31.
    buf[24] = 8; // compression id
    buf[25..28].copy_from_slice(b"SYN");
    buf[55] = 4; // length of payload
    buf
}

fn pd_bytes(location: u32, partition_number: u16, start: u32, length_blocks: u32) -> Vec<u8> {
    let mut buf = vec![0u8; SECTOR];
    let t = tag(TagId::Partition, location, 196);
    buf[0..16].copy_from_slice(&t.encode());
    buf[16..20].copy_from_slice(&1u32.to_le_bytes()); // VDS seq num
    buf[20..22].copy_from_slice(&0u16.to_le_bytes()); // flags
    buf[22..24].copy_from_slice(&partition_number.to_le_bytes());
    buf[188..192].copy_from_slice(&start.to_le_bytes());
    buf[192..196].copy_from_slice(&length_blocks.to_le_bytes());
    buf
}

fn lvd_bytes(location: u32, fsd_partition_block: u32, partition_ref: u16) -> Vec<u8> {
    let mut buf = vec![0u8; SECTOR];
    let t = tag(TagId::LogicalVolume, location, 440);
    buf[0..16].copy_from_slice(&t.encode());
    buf[16..20].copy_from_slice(&1u32.to_le_bytes()); // VDS seq num
                                                      // descriptor_character_set 64 bytes at 20..84 — zeros
                                                      // logical_volume_identifier d-string at 84..212 (128 bytes), length byte at 211
    buf[84] = 8;
    buf[85..88].copy_from_slice(b"LVI");
    buf[211] = 4;
    // logical_block_size = 2048
    buf[212..216].copy_from_slice(&2048u32.to_le_bytes());
    // domain_identifier at 216..248
    // FSD long_ad at 248..264
    let fsd = LongAd {
        length: SECTOR_SIZE as u32,
        extent_type: 0,
        location: LbAddr {
            block: fsd_partition_block,
            partition_ref,
        },
        implementation_use: [0u8; 6],
    };
    buf[248..264].copy_from_slice(&fsd.encode());
    // map_table_length / partition_maps deliberately empty.
    buf
}

fn fsd_bytes(location: u32, root_dir_block: u32, partition_ref: u16) -> Vec<u8> {
    let mut buf = vec![0u8; SECTOR];
    let t = tag(TagId::FileSet, location, 416);
    buf[0..16].copy_from_slice(&t.encode());
    // RecordingDateAndTime 12 bytes at 16..28 — zeros
    // InterchangeLevel u16 at 28..30
    buf[28..30].copy_from_slice(&3u16.to_le_bytes());
    buf[30..32].copy_from_slice(&3u16.to_le_bytes());
    // CharacterSetList u32 at 32..36 = 1
    buf[32..36].copy_from_slice(&1u32.to_le_bytes());
    buf[36..40].copy_from_slice(&1u32.to_le_bytes());
    // FileSetNumber + FileSetDescriptorNumber
    buf[40..44].copy_from_slice(&0u32.to_le_bytes());
    buf[44..48].copy_from_slice(&0u32.to_le_bytes());
    // LogicalVolumeIdentifierCharSet (64 bytes) + LVI d-string (128 bytes) — zeros
    // FileSetCharSet + FileSetIdentifier + Copyright + Abstract — zeros
    let root = LongAd {
        length: SECTOR_SIZE as u32,
        extent_type: 0,
        location: LbAddr {
            block: root_dir_block,
            partition_ref,
        },
        implementation_use: [0u8; 6],
    };
    buf[400..416].copy_from_slice(&root.encode());
    buf
}

fn make_root_directory_file_entry(location: u32, fid_payload: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; SECTOR];
    // For the root directory we use EmbeddedInIcb: the FIDs live
    // directly in the FE's allocation area.
    let t = tag(TagId::FileEntry, location, 200);
    buf[0..16].copy_from_slice(&t.encode());

    // ICB tag: file_type = 4 (directory), flags ad_type = 3 (embedded).
    // strategy_type = 4
    buf[16..20].copy_from_slice(&0u32.to_le_bytes()); // prior_recorded_entries
    buf[20..22].copy_from_slice(&4u16.to_le_bytes()); // strategy_type
    buf[22..24].copy_from_slice(&0u16.to_le_bytes()); // strategy_parameter
    buf[24..26].copy_from_slice(&1u16.to_le_bytes()); // max_entries
                                                      // byte 26 reserved, byte 27 file_type = 4
    buf[27] = 4;
    // parent_icb 6 bytes at 28..34 — zero
    // flags = embedded(3) | strategy stuff at 0
    buf[34..36].copy_from_slice(&3u16.to_le_bytes());

    // uid/gid/perm/file_link_count at 36..50
    buf[44..48].copy_from_slice(&0u32.to_le_bytes()); // permissions
    buf[48..50].copy_from_slice(&1u16.to_le_bytes()); // file_link_count
                                                      // record_format/disp_attr/record_length at 50..56 — zero
                                                      // information_length u64 at 56
    buf[56..64].copy_from_slice(&(fid_payload.len() as u64).to_le_bytes());
    // logical_blocks_recorded u64 at 64 — zero
    // access/mod/attribute times at 72..108 — zero
    // checkpoint u32 at 108..112 — zero
    // extended_attribute_icb long_ad at 112..128 — zero
    // implementation_identifier 32 bytes at 128..160 — zero
    // unique_id u64 at 160..168 — zero
    // length_of_extended_attributes u32 at 168..172 = 0
    // length_of_allocation_descriptors u32 at 172..176 = fid_payload.len()
    buf[172..176].copy_from_slice(&(fid_payload.len() as u32).to_le_bytes());
    // ad area starts at offset 176 (after 0 EA bytes).
    let ad_off = 176;
    buf[ad_off..ad_off + fid_payload.len()].copy_from_slice(fid_payload);
    buf
}

/// Build a File Identifier Descriptor with the given name, ICB
/// (block, partition_ref), and whether it's a directory.
fn make_fid(name: &str, block: u32, partition_ref: u16, is_dir: bool) -> Vec<u8> {
    let mut out = Vec::new();
    // tag (16 bytes) — we'll backfill after computing crc_length
    let mut header = [0u8; 16];
    let icb = LongAd {
        length: SECTOR_SIZE as u32,
        extent_type: 0,
        location: LbAddr {
            block,
            partition_ref,
        },
        implementation_use: [0u8; 6],
    };
    // file_version_number u16 = 1
    let mut body = Vec::new();
    body.extend_from_slice(&1u16.to_le_bytes());
    // file_characteristics: bit 1 = directory
    let chars = if is_dir { 0x02u8 } else { 0x00u8 };
    body.push(chars);
    // length_of_file_identifier
    let id_len = 1 + name.len(); // 1 byte compression-id prefix + ascii bytes
    body.push(id_len as u8);
    // ICB long_ad 16 bytes
    body.extend_from_slice(&icb.encode());
    // length_of_implementation_use u16 = 0
    body.extend_from_slice(&0u16.to_le_bytes());
    // identifier: 8-bit d-string
    body.push(8); // compression id
    body.extend_from_slice(name.as_bytes());
    // pad to 4-byte boundary
    while (body.len() + 16) % 4 != 0 {
        body.push(0);
    }
    // crc_length is total length minus 16.
    let crc_length = (body.len()) as u16;
    let t = tag(TagId::FileIdentifier, 0, crc_length);
    header.copy_from_slice(&t.encode());
    out.extend_from_slice(&header);
    out.extend_from_slice(&body);
    out
}

/// Build the synthetic UDF image used by both the mount/walk test and
/// the volume-label extraction test. The PVD encodes the 8-bit
/// d-string `"SYN"` as `volume_identifier`.
fn build_synthetic_udf_image() -> Vec<u8> {
    // Image: 512 sectors × 2048 bytes = 1 MiB.
    let mut image = vec![0u8; 512 * SECTOR];

    // VRS (optional in our impl but include for completeness).
    let mut vrs = vec![0u8; SECTOR];
    vrs[0] = 0;
    vrs[1..6].copy_from_slice(b"BEA01");
    vrs[6] = 1; // structure_version
    place(&mut image, 16, &vrs);
    let mut nsr = vec![0u8; SECTOR];
    nsr[1..6].copy_from_slice(b"NSR03");
    nsr[6] = 1;
    place(&mut image, 17, &nsr);
    let mut tea = vec![0u8; SECTOR];
    tea[1..6].copy_from_slice(b"TEA01");
    tea[6] = 1;
    place(&mut image, 18, &tea);

    // AVDP at sector 256.
    let main_vds = ExtentAd {
        length: (4 * SECTOR_SIZE) as u32,
        location: 257,
    };
    let reserve_vds = ExtentAd {
        length: 0,
        location: 0,
    };
    let mut avdp = vec![0u8; SECTOR];
    let avdp_tag = tag(TagId::AnchorVolumeDescriptorPointer, AVDP_SECTOR as u32, 32);
    avdp[0..16].copy_from_slice(&avdp_tag.encode());
    avdp[16..24].copy_from_slice(&main_vds.encode());
    avdp[24..32].copy_from_slice(&reserve_vds.encode());
    place(&mut image, AVDP_SECTOR, &avdp);

    // Place PVD / PD / LVD / Terminating
    place(&mut image, 257, &pvd_bytes(257));
    place(&mut image, 258, &pd_bytes(258, 0, 300, 200));
    place(&mut image, 259, &lvd_bytes(259, 0, 0));
    let mut term = vec![0u8; SECTOR];
    let t_term = tag(TagId::Terminating, 260, 16);
    term[0..16].copy_from_slice(&t_term.encode());
    place(&mut image, 260, &term);

    // FSD at partition_start (300) + 0 = 300.
    place(&mut image, 300, &fsd_bytes(300, 1, 0));

    // Root directory FE at partition_start + 1 = 301 with two FIDs:
    // "..", then a child file "TEST" pointing at partition block 2 (302).
    let mut fids = Vec::new();
    // "..": parent FID with characteristics 0x08
    let parent_fid = {
        let mut tmp = Vec::new();
        let mut header = [0u8; 16];
        let body = {
            let mut body = Vec::new();
            body.extend_from_slice(&1u16.to_le_bytes()); // file_version_number
            body.push(0x0A); // parent (0x08) | directory (0x02)
            body.push(0); // length_of_file_identifier
            let icb = LongAd {
                length: SECTOR_SIZE as u32,
                extent_type: 0,
                location: LbAddr {
                    block: 1,
                    partition_ref: 0,
                },
                implementation_use: [0u8; 6],
            };
            body.extend_from_slice(&icb.encode());
            body.extend_from_slice(&0u16.to_le_bytes()); // length_of_impl_use
            while (body.len() + 16) % 4 != 0 {
                body.push(0);
            }
            body
        };
        let t = tag(TagId::FileIdentifier, 0, body.len() as u16);
        header.copy_from_slice(&t.encode());
        tmp.extend_from_slice(&header);
        tmp.extend_from_slice(&body);
        tmp
    };
    fids.extend_from_slice(&parent_fid);
    fids.extend_from_slice(&make_fid("TEST", 2, 0, false));
    fids.extend_from_slice(&make_fid("LONG", 3, 0, false));
    fids.extend_from_slice(&make_fid("XPART", 6, 0, false));
    fids.extend_from_slice(&make_fid("CHAIN", 7, 0, false));
    fids.extend_from_slice(&make_fid("CYCLE", 11, 0, false));
    place(&mut image, 301, &make_root_directory_file_entry(301, &fids));

    // The "TEST" file's FE at partition block 2 / sector 302. Embedded
    // payload b"HELLO".
    let mut test_fe = vec![0u8; SECTOR];
    let t = tag(TagId::FileEntry, 302, 200);
    test_fe[0..16].copy_from_slice(&t.encode());
    test_fe[20..22].copy_from_slice(&4u16.to_le_bytes()); // strategy_type
    test_fe[24..26].copy_from_slice(&1u16.to_le_bytes()); // max_entries
    test_fe[27] = 5; // file_type = file (5)
    test_fe[34..36].copy_from_slice(&3u16.to_le_bytes()); // flags ad_type = embedded
    test_fe[48..50].copy_from_slice(&1u16.to_le_bytes()); // link count
    test_fe[56..64].copy_from_slice(&5u64.to_le_bytes()); // info_len
    test_fe[172..176].copy_from_slice(&5u32.to_le_bytes()); // l_ad
    test_fe[176..181].copy_from_slice(b"HELLO");
    place(&mut image, 302, &test_fe);

    // The "LONG" file's FE at partition block 3 / sector 303 records a
    // single long_ad (§14.14.2) covering partition blocks 4..6
    // (sectors 304..306); information_length 2500 spans into the
    // second block.
    let long_body: Vec<u8> = (0..LONG_FILE_LEN).map(|i| (i % 251) as u8).collect();
    place(&mut image, 304, &long_body);
    let long_extent = LongAd {
        length: 2 * SECTOR_SIZE as u32,
        extent_type: 0,
        location: LbAddr {
            block: 4,
            partition_ref: 0,
        },
        implementation_use: [0u8; 6],
    };
    place(
        &mut image,
        303,
        &make_long_ad_file_entry(303, LONG_FILE_LEN as u64, &[long_extent]),
    );

    // The "XPART" file's FE at partition block 6 / sector 306 records
    // a long_ad whose lb_addr names partition_ref 1 — a partition this
    // single-partition volume doesn't have.
    let foreign_extent = LongAd {
        length: SECTOR_SIZE as u32,
        extent_type: 0,
        location: LbAddr {
            block: 4,
            partition_ref: 1,
        },
        implementation_use: [0u8; 6],
    };
    place(
        &mut image,
        306,
        &make_long_ad_file_entry(306, SECTOR_SIZE, &[foreign_extent]),
    );

    // The "CHAIN" file's FE at partition block 7 / sector 307 records
    // one data long_ad (block 8) and a type-3 continuation pointer
    // (§14.14.1.1) to block 9, where an Allocation Extent Descriptor
    // (§14.5) carries one further data long_ad (block 10). The body
    // spans both data extents; information_length (3048) trims the
    // second one.
    let chain_body: Vec<u8> = (0..CHAIN_FILE_LEN)
        .map(|i| ((i * 7 + 3) % 253) as u8)
        .collect();
    place(&mut image, 308, &chain_body[..SECTOR]);
    place(&mut image, 310, &chain_body[SECTOR..]);
    let chain_first = LongAd {
        length: SECTOR_SIZE as u32,
        extent_type: 0,
        location: LbAddr {
            block: 8,
            partition_ref: 0,
        },
        implementation_use: [0u8; 6],
    };
    place(
        &mut image,
        307,
        &make_long_ad_file_entry(
            307,
            CHAIN_FILE_LEN as u64,
            &[chain_first, continuation_long_ad(9)],
        ),
    );
    let chain_second = LongAd {
        length: SECTOR_SIZE as u32,
        extent_type: 0,
        location: LbAddr {
            block: 10,
            partition_ref: 0,
        },
        implementation_use: [0u8; 6],
    };
    place(&mut image, 309, &make_aed_block(309, &[chain_second]));

    // The "CYCLE" file's FE at partition block 11 / sector 311 records
    // only a type-3 pointer to block 12, where the AED's single
    // descriptor is a type-3 pointer back to block 12 — a cyclic AED
    // chain the mounter must refuse rather than walk forever.
    place(
        &mut image,
        311,
        &make_long_ad_file_entry(311, 0, &[continuation_long_ad(12)]),
    );
    place(
        &mut image,
        312,
        &make_aed_block(312, &[continuation_long_ad(12)]),
    );

    image
}

/// Body length of the synthetic "CHAIN" file (first extent full, the
/// AED-continued second extent trimmed to 1000 bytes).
const CHAIN_FILE_LEN: usize = SECTOR + 1000;

/// Body length of the synthetic "LONG" file (spans two blocks).
const LONG_FILE_LEN: usize = 2500;

/// Build a plain File Entry (file_type 5) whose allocation area holds
/// the given long_ads (ICB-tag ad_type flags = 1, §14.6.8).
fn make_long_ad_file_entry(location: u32, info_len: u64, extents: &[LongAd]) -> Vec<u8> {
    let mut buf = vec![0u8; SECTOR];
    let t = tag(TagId::FileEntry, location, 200);
    buf[0..16].copy_from_slice(&t.encode());
    buf[20..22].copy_from_slice(&4u16.to_le_bytes()); // strategy_type
    buf[24..26].copy_from_slice(&1u16.to_le_bytes()); // max_entries
    buf[27] = 5; // file_type = file (5)
    buf[34..36].copy_from_slice(&1u16.to_le_bytes()); // flags ad_type = long (1)
    buf[48..50].copy_from_slice(&1u16.to_le_bytes()); // link count
    buf[56..64].copy_from_slice(&info_len.to_le_bytes()); // info_len
    let l_ad = (extents.len() * 16) as u32;
    buf[172..176].copy_from_slice(&l_ad.to_le_bytes());
    let mut off = 176;
    for extent in extents {
        buf[off..off + 16].copy_from_slice(&extent.encode());
        off += 16;
    }
    buf
}

/// Build a continuation extent of allocation descriptors per §12
/// figure 7: an Allocation Extent Descriptor (§14.5) followed by the
/// given long_ads.
fn make_aed_block(location: u32, extents: &[LongAd]) -> Vec<u8> {
    let mut buf = vec![0u8; SECTOR];
    let l_ad = (extents.len() * 16) as u32;
    let aed = AllocationExtentDescriptor {
        tag: tag(TagId::AllocationExtent, location, (8 + l_ad) as u16),
        previous_allocation_extent_location: 0,
        length_of_allocation_descriptors: l_ad,
    };
    buf[0..24].copy_from_slice(&aed.encode());
    let mut off = AllocationExtentDescriptor::HEADER_SIZE;
    for extent in extents {
        buf[off..off + 16].copy_from_slice(&extent.encode());
        off += 16;
    }
    buf
}

/// A type-3 ("next extent of allocation descriptors", §14.14.1.1)
/// long_ad pointing at one logical block.
fn continuation_long_ad(block: u32) -> LongAd {
    LongAd {
        length: SECTOR_SIZE as u32,
        extent_type: 3,
        location: LbAddr {
            block,
            partition_ref: 0,
        },
        implementation_use: [0u8; 6],
    }
}

#[test]
fn mount_synthetic_udf_and_walk_root() {
    let image = build_synthetic_udf_image();

    // Mount it.
    let mut disc = UdfDisc::open(Cursor::new(image)).expect("mount synthetic UDF");
    assert_eq!(disc.partition_start_sector, 300);
    assert_eq!(disc.logical_block_size, 2048);

    // Walk the root and find "TEST".
    let root_icb = disc.root_directory_icb;
    let entries = disc.read_directory(root_icb).expect("read root");
    assert_eq!(entries.len(), 5);
    let test = entries
        .iter()
        .find(|e| e.name == "TEST")
        .expect("TEST entry");
    assert!(!test.is_directory);

    let content = disc.read_file(test.icb).expect("read TEST");
    assert_eq!(content, b"HELLO");

    // Path lookup also works.
    let by_path = disc.read_path("TEST").expect("lookup by path");
    assert_eq!(by_path, b"HELLO");

    // Suppress "unused import" warnings for re-exports we don't
    // exercise in this minimal test.
    let _ = (
        std::any::type_name::<AnchorVolumeDescriptorPointer>(),
        std::any::type_name::<FileEntry>(),
        std::any::type_name::<FileSetDescriptor>(),
        std::any::type_name::<LogicalVolumeDescriptor>(),
        std::any::type_name::<PartitionDescriptor>(),
        std::any::type_name::<PrimaryVolumeDescriptor>(),
        std::any::type_name::<ShortAd>(),
    );
}

#[test]
fn long_ad_file_reads_across_blocks() {
    // The "LONG" file's body is recorded through a long_ad (§14.14.2)
    // covering two partition blocks; information_length (2500) trims
    // the block-rounded extent.
    let image = build_synthetic_udf_image();
    let mut disc = UdfDisc::open(Cursor::new(image)).expect("mount synthetic UDF");
    assert_eq!(disc.partition_number, 0);

    let content = disc.read_path("LONG").expect("read LONG via long_ad");
    assert_eq!(content.len(), LONG_FILE_LEN);
    let expected: Vec<u8> = (0..LONG_FILE_LEN).map(|i| (i % 251) as u8).collect();
    assert_eq!(content, expected);
}

#[test]
fn aed_continuation_chain_reads_full_file() {
    // "CHAIN"'s File Entry ends its AD field in a type-3 pointer
    // (§14.14.1.1) to an Allocation Extent Descriptor block (§14.5);
    // the mounter must follow the chain and read the body across both
    // the in-entry and the continued extent.
    let image = build_synthetic_udf_image();
    let mut disc = UdfDisc::open(Cursor::new(image)).expect("mount synthetic UDF");

    let content = disc.read_path("CHAIN").expect("read CHAIN via AED chain");
    assert_eq!(content.len(), CHAIN_FILE_LEN);
    let expected: Vec<u8> = (0..CHAIN_FILE_LEN)
        .map(|i| ((i * 7 + 3) % 253) as u8)
        .collect();
    assert_eq!(content, expected);
}

#[test]
fn cyclic_aed_chain_is_refused() {
    // "CYCLE"'s AED chain points back at itself; the depth-capped walk
    // must surface Malformed instead of looping forever.
    let image = build_synthetic_udf_image();
    let mut disc = UdfDisc::open(Cursor::new(image)).expect("mount synthetic UDF");
    let err = disc.read_path("CYCLE").expect_err("cyclic AED chain");
    assert!(matches!(err, oxideav_bluray::BlurayError::Malformed(_)));
}

#[test]
fn cross_partition_long_ad_is_refused() {
    // "XPART"'s long_ad names partition_ref 1 — not the mounted
    // partition. The single-partition mounter must refuse rather than
    // misresolve the block against the wrong partition base.
    let image = build_synthetic_udf_image();
    let mut disc = UdfDisc::open(Cursor::new(image)).expect("mount synthetic UDF");
    let err = disc.read_path("XPART").expect_err("cross-partition extent");
    assert!(matches!(err, oxideav_bluray::BlurayError::Unsupported(_)));
}

#[test]
fn read_volume_label_extracts_pvd_identifier() {
    // The synthetic image's PVD encodes the d-string "SYN".
    let image = build_synthetic_udf_image();
    let label = oxideav_bluray::udf::read_volume_label(Cursor::new(image))
        .expect("read_volume_label on synthetic image");
    assert_eq!(label, "SYN");
}

#[test]
fn disc_volume_label_returns_none_for_filesystem_root() {
    // `Disc::volume_label()` returns `None` when `self.root` is a
    // directory (the canonical filesystem-mount case) — the underlying
    // block device's PVD has already been consumed by the OS driver,
    // there's nothing for us to parse. Build a minimal BDMV/ tree so
    // `Disc::mount` succeeds, then assert the label probe falls through
    // cleanly to `None` instead of erroring.
    use std::fs;
    use std::io::Write;

    use oxideav_bluray::bdmv::index_bdmv::{AppInfoBdmv, IndexBdmv, IndexEntry, IndexObjectType};

    let pid = std::process::id();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp = std::env::temp_dir().join(format!("oxideav-bluray-vol-label-{pid}-{nonce}"));
    fs::create_dir_all(tmp.join("BDMV")).unwrap();

    let idx = IndexBdmv {
        version: *b"0200",
        app_info: AppInfoBdmv {
            initial_output_mode_preference: 0,
            content_exist_flag: 1,
            video_format: 6,
            frame_rate: 4,
        },
        first_playback_title: IndexEntry {
            object: IndexObjectType::Hdmv {
                playback_type: 0,
                movie_object_id_ref: 0,
            },
        },
        menu_title: IndexEntry {
            object: IndexObjectType::Hdmv {
                playback_type: 1,
                movie_object_id_ref: 0,
            },
        },
        titles: vec![],
    };
    let mut f = fs::File::create(tmp.join("BDMV/index.bdmv")).unwrap();
    f.write_all(&idx.encode()).unwrap();
    drop(f);

    let disc = oxideav_bluray::Disc::mount(&tmp).expect("mount filesystem-style BDMV");
    assert_eq!(disc.volume_label(), None);

    let _ = fs::remove_dir_all(&tmp);
}
