//! End-to-end integration over the PGS (Presentation Graphic Stream)
//! subtitle pipeline and the HDMV navigation-command + register-model
//! layers.
//!
//! These tests assemble a synthetic `.sup`-style PG byte stream **by
//! hand** (raw big-endian bytes, not via the crate's own `encode`
//! helpers) so the *parse* path is exercised on real wire bytes, then
//! drive it all the way through: `parse_segments` → `group_display_sets`
//! → `DisplaySet::reassemble_objects` / `render` → RGBA graphics plane.
//! A second test builds a `MovieObject.bdmv` image and decodes its
//! navigation commands, resolving register operands against the PSR/GPR
//! model.
//!
//! Wire format: `docs/container/bluray/pgs-segment-syntax.md` and
//! `docs/container/bluray/hdmv-navigation-commands.md`.

use oxideav_bluray::bdmv::movie_object::MovieObjects;
use oxideav_bluray::{
    group_display_sets, parse_segments, CompositionState, FragmentFlag, Operand, Operation,
    PsrClass, RegisterBank, SegmentBody, SegmentType,
};
use oxideav_bluray::{MovieObject, NavCommand};

/// Push a 13-byte PG segment header followed by `body`, computing
/// `segment_size` from the body length.
fn push_segment(out: &mut Vec<u8>, pts: u32, seg_type: u8, body: &[u8]) {
    out.extend_from_slice(b"PG");
    out.extend_from_slice(&pts.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // dts
    out.push(seg_type);
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(body);
}

/// Build a minimal one-Display-Set `.sup` stream:
///
/// - plane 16×8, Epoch Start, palette_id 0, one composition object
///   (object 0) drawn at (2, 1) into window 0.
/// - WDS: one 8×4 window at (2, 1).
/// - PDS palette 0: index 1 → opaque, index 2 → opaque (different
///   colours), index 0 left transparent.
/// - ODS object 0: a 4×2 bitmap, single FirstAndLast fragment, with
///   row 0 = pixels [1, 1, 2, 2] (four literals) and row 1 =
///   pixels [2, 2, 2, 2] (a short colour-2 run).
fn build_sup_stream() -> Vec<u8> {
    let mut out = Vec::new();
    let pts = 90_000; // 1s in 90 kHz units

    // --- PCS (0x16) ---
    let mut pcs = Vec::new();
    pcs.extend_from_slice(&16u16.to_be_bytes()); // width
    pcs.extend_from_slice(&8u16.to_be_bytes()); // height
    pcs.push(0x10); // frame_rate
    pcs.extend_from_slice(&0u16.to_be_bytes()); // composition_number
    pcs.push(0x80); // composition_state = Epoch Start
    pcs.push(0x00); // palette_update_flag
    pcs.push(0x00); // palette_id
    pcs.push(1); // number_of_composition_objects
                 // composition object 0:
    pcs.extend_from_slice(&0u16.to_be_bytes()); // object_id
    pcs.push(0); // window_id
    pcs.push(0x00); // object_cropped_flag (not cropped)
    pcs.extend_from_slice(&2u16.to_be_bytes()); // object_horizontal_position
    pcs.extend_from_slice(&1u16.to_be_bytes()); // object_vertical_position
    push_segment(&mut out, pts, 0x16, &pcs);

    // --- WDS (0x17) ---
    let mut wds = Vec::new();
    wds.push(1); // number_of_windows
    wds.push(0); // window_id
    wds.extend_from_slice(&2u16.to_be_bytes()); // window_horizontal_position
    wds.extend_from_slice(&1u16.to_be_bytes()); // window_vertical_position
    wds.extend_from_slice(&8u16.to_be_bytes()); // window_width
    wds.extend_from_slice(&4u16.to_be_bytes()); // window_height
    push_segment(&mut out, pts, 0x17, &wds);

    // --- PDS (0x14) ---
    let mut pds = Vec::new();
    pds.push(0); // palette_id
    pds.push(0); // palette_version_number
                 // entry 1: opaque, Y=235 Cr=128 Cb=128 (≈ white), alpha 255
    pds.extend_from_slice(&[1, 235, 128, 128, 255]);
    // entry 2: opaque, Y=81 Cr=90 Cb=240 (≈ a blue), alpha 255
    pds.extend_from_slice(&[2, 81, 90, 240, 255]);
    push_segment(&mut out, pts, 0x14, &pds);

    // --- ODS (0x15) ---
    // RLE bytes per scanline, terminated by 00 00 end-of-line.
    //   row 0 [1,1,2,2]: literals 01 01 02 02, then EOL 00 00
    //   row 1 [2,2,2,2]: short run of colour 2, length 4 ->
    //                    escape 00, byte2 = 10LLLLLL (0x80|4)=0x84, colour 02
    //                    then EOL 00 00
    let mut rle = Vec::new();
    rle.extend_from_slice(&[0x01, 0x01, 0x02, 0x02, 0x00, 0x00]); // row 0
    rle.extend_from_slice(&[0x00, 0x84, 0x02, 0x00, 0x00]); // row 1 run + EOL

    let mut ods = Vec::new();
    ods.extend_from_slice(&0u16.to_be_bytes()); // object_id
    ods.push(0); // object_version_number
    ods.push(0xC0); // last_in_sequence_flag = FirstAndLast
                    // object_data_length = width(2) + height(2) + rle.len()
    let data_len = (4 + rle.len()) as u32;
    ods.push((data_len >> 16) as u8);
    ods.push((data_len >> 8) as u8);
    ods.push(data_len as u8);
    ods.extend_from_slice(&4u16.to_be_bytes()); // width
    ods.extend_from_slice(&2u16.to_be_bytes()); // height
    ods.extend_from_slice(&rle);
    push_segment(&mut out, pts, 0x15, &ods);

    // --- END (0x80) ---
    push_segment(&mut out, pts, 0x80, &[]);

    out
}

#[test]
fn sup_stream_parses_into_one_display_set() {
    let stream = build_sup_stream();
    let segments = parse_segments(&stream).expect("parse PG segments");

    // Five segments, in canonical PCS → WDS → PDS → ODS → END order.
    assert_eq!(segments.len(), 5);
    assert_eq!(segments[0].header.kind(), SegmentType::Pcs);
    assert_eq!(segments[1].header.kind(), SegmentType::Wds);
    assert_eq!(segments[2].header.kind(), SegmentType::Pds);
    assert_eq!(segments[3].header.kind(), SegmentType::Ods);
    assert_eq!(segments[4].header.kind(), SegmentType::End);

    // PTS propagated from the header.
    assert_eq!(segments[0].header.pts, 90_000);

    let display_sets = group_display_sets(&segments).expect("group into display sets");
    assert_eq!(display_sets.len(), 1);
    let ds = &display_sets[0];
    assert_eq!(ds.state(), CompositionState::EpochStart);
    assert_eq!(ds.pts, 90_000);
    assert_eq!(ds.pcs.width, 16);
    assert_eq!(ds.pcs.height, 8);
    assert_eq!(ds.pcs.composition_objects.len(), 1);
    assert!(ds.wds.is_some());
    assert_eq!(ds.palettes.len(), 1);
    assert_eq!(ds.objects.len(), 1);
    assert_eq!(ds.objects[0].fragment(), FragmentFlag::FirstAndLast);
}

#[test]
fn ods_rle_decodes_to_expected_indices() {
    let stream = build_sup_stream();
    let segments = parse_segments(&stream).unwrap();
    let display_sets = group_display_sets(&segments).unwrap();
    let ds = &display_sets[0];

    let objects = ds.reassemble_objects().expect("reassemble ODS fragments");
    assert_eq!(objects.len(), 1);
    let decoded = objects[0].decode().expect("RLE decode");
    assert_eq!(decoded.width, 4);
    assert_eq!(decoded.height, 2);
    // Row 0 literals, row 1 run-of-2.
    assert_eq!(decoded.pixels, vec![1, 1, 2, 2, 2, 2, 2, 2]);
}

#[test]
fn render_composites_object_onto_plane() {
    let stream = build_sup_stream();
    let segments = parse_segments(&stream).unwrap();
    let display_sets = group_display_sets(&segments).unwrap();
    let ds = &display_sets[0];

    let rendered = ds.render().expect("render display set");
    assert_eq!(rendered.pts, 90_000);
    let plane = &rendered.plane;
    assert_eq!(plane.width, 16);
    assert_eq!(plane.height, 8);

    // The object is painted at (2, 1). Plane (0,0) is outside it → fully
    // transparent.
    assert_eq!(plane.pixel(0, 0).unwrap().a, 0);

    // Object pixel (0,0) = index 1 (opaque) lands at plane (2, 1).
    let p_idx1 = plane.pixel(2, 1).expect("plane pixel in bounds");
    assert_eq!(p_idx1.a, 255, "index 1 is opaque");
    // index 1 is ≈ white (Y=235, neutral chroma).
    assert!(p_idx1.r > 200 && p_idx1.g > 200 && p_idx1.b > 200);

    // Object pixel (2,0) = index 2 (opaque blue) lands at plane (4, 1).
    let p_idx2 = plane.pixel(4, 1).expect("plane pixel in bounds");
    assert_eq!(p_idx2.a, 255, "index 2 is opaque");
    // The two palette colours differ.
    assert_ne!(p_idx1.to_array(), p_idx2.to_array());

    // Object row 1 is a full run of index 2 → plane row 2 cols 2..6 opaque.
    for x in 2..6 {
        assert_eq!(plane.pixel(x, 2).unwrap().a, 255);
    }

    // Just past the 4×2 object (plane (6, 1)) is back to transparent.
    assert_eq!(plane.pixel(6, 1).unwrap().a, 0);
}

#[test]
fn second_display_set_is_a_separate_pcs_boundary() {
    // Two DSs back to back: the grouper must split on the second PCS.
    let mut stream = build_sup_stream();
    let second = build_sup_stream();
    stream.extend_from_slice(&second);

    let segments = parse_segments(&stream).unwrap();
    assert_eq!(segments.len(), 10);
    let display_sets = group_display_sets(&segments).unwrap();
    assert_eq!(display_sets.len(), 2);
    for ds in &display_sets {
        assert_eq!(ds.state(), CompositionState::EpochStart);
        let _ = ds.render().expect("each DS renders");
    }
}

#[test]
fn pds_palette_entries_round_trip_through_parse() {
    let stream = build_sup_stream();
    let segments = parse_segments(&stream).unwrap();
    let pds = segments
        .iter()
        .find_map(|s| match &s.body {
            SegmentBody::Pds(p) => Some(p),
            _ => None,
        })
        .expect("PDS present");
    assert_eq!(pds.palette_id, 0);
    assert_eq!(pds.entries.len(), 2);
    assert_eq!(pds.entries[0].palette_entry_id, 1);
    assert_eq!(pds.entries[0].alpha, 255);
    assert_eq!(pds.entries[1].palette_entry_id, 2);
    assert_eq!(pds.entries[1].y, 81);
}

// --- HDMV navigation-command + register-model end-to-end ---

/// Build a `MovieObject.bdmv` image holding one Movie Object whose
/// command list is:
///   1. JumpTitle 1                     (21 81 00 00  00 00 00 01  …)
///   2. Move GPR1 ← 5                   (50 40 00 01  00 00 00 01  00 00 00 05)
///   3. EQ PSR4 == 0xFFFF (cmp PSR4)    (48 40 02 00  80 00 00 04  00 00 FF FF)
fn build_movie_object_bdmv() -> Vec<u8> {
    fn cmd(w0: u32, w1: u32, w2: u32) -> NavCommand {
        let mut bytes = [0u8; 12];
        bytes[0..4].copy_from_slice(&w0.to_be_bytes());
        bytes[4..8].copy_from_slice(&w1.to_be_bytes());
        bytes[8..12].copy_from_slice(&w2.to_be_bytes());
        NavCommand { bytes }
    }

    let mo = MovieObjects {
        version: *b"0200",
        movie_objects: vec![MovieObject {
            resume_intention_flag: 0,
            menu_call_mask: 0,
            title_search_mask: 0,
            commands: vec![
                cmd(0x2181_0000, 0x0000_0001, 0x0000_0000), // JumpTitle 1
                cmd(0x5040_0001, 0x0000_0001, 0x0000_0005), // Move GPR1 ← 5
                cmd(0x4840_0200, 0x8000_0004, 0x0000_FFFF), // EQ PSR4 == 0xFFFF
            ],
        }],
    };
    mo.encode()
}

#[test]
fn movie_object_commands_decode_and_resolve_registers() {
    let image = build_movie_object_bdmv();
    let mo = MovieObjects::parse(&image).expect("parse MovieObject.bdmv");
    assert_eq!(mo.version, *b"0200");
    assert_eq!(mo.movie_objects.len(), 1);
    let cmds = &mo.movie_objects[0].commands;
    assert_eq!(cmds.len(), 3);

    // 1. JumpTitle 1 — branch, immediate operand 1.
    let jump = cmds[0].decode();
    assert_eq!(jump.operation, Operation::JumpTitle);
    assert_eq!(jump.operand1, Some(Operand::Immediate(1)));

    // 2. Move GPR1 ← 5 — set op; dst is GPR1 (register), src immediate 5.
    let mov = cmds[1].decode();
    assert_eq!(mov.operation, Operation::Move);
    let dst = mov.operand1.unwrap();
    let dst_reg = dst.resolve_register().expect("dst is a register");
    assert_eq!(dst_reg.bank, RegisterBank::Gpr);
    assert_eq!(dst_reg.index, 1);
    assert!(dst_reg.psr.is_none(), "GPR ref has no PsrInfo");
    assert_eq!(mov.operand2, Some(Operand::Immediate(5)));

    // 3. EQ comparing PSR4 (Title) against 0xFFFF — compare op; operand 1
    //    is a PSR register reference that resolves to the named "Title".
    let eq = cmds[2].decode();
    assert_eq!(eq.operation, Operation::Eq);
    let cmp_reg = eq.operand1.unwrap().resolve_register().unwrap();
    assert_eq!(cmp_reg.bank, RegisterBank::Psr);
    assert_eq!(cmp_reg.index, 4);
    let psr = cmp_reg.psr.expect("PSR4 has a named entry");
    assert_eq!(psr.name, "Title");
    // Title is mutable Playback Status — a nav command may write it.
    assert!(!psr.class.is_read_only_to_nav());
    assert!(matches!(psr.class, PsrClass::PlaybackStatus { .. }));
    assert_eq!(eq.operand2, Some(Operand::Immediate(0xFFFF)));
}
