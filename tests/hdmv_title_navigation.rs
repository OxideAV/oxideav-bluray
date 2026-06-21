//! End-to-end disc-title HDMV navigation: hand-assemble both an
//! `index.bdmv` and a `MovieObject.bdmv` byte image (raw big-endian wire
//! bytes), parse them with the crate's public parsers, then drive the
//! whole title structure with `NavDriver` — the layer that resolves
//! inter-title `JumpTitle`/`CallTitle` through the index and surfaces only
//! actual playback requests.
//!
//! This exercises the full chain the crate now owns on the navigation side
//! (`index.bdmv` bytes into `IndexBdmv`, title-entry resolution,
//! `MovieObject.bdmv` bytes into `MovieObjects`, per-object `DecodedCommand`
//! decode, `MobjRunner` VM execution, then `NavDriver` inter-title
//! transition with PSR4 seeding and `PlayPL` surfacing). The command words
//! and the `index.bdmv` Object() records are emitted by hand here from the
//! layouts in `docs/container/bluray/hdmv-navigation-commands.md` and
//! BD-ROM Part 3 §5.2/§5.3, so the test pins the on-wire bytes.

use oxideav_bluray::{
    DriveOutcome, IndexBdmv, MovieObjects, NavDriver, NavRequest, PlayRequest, TitleEntry,
};

/// A 12-byte navigation command from three big-endian words.
fn cmd(word0: u32, word1: u32, word2: u32) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[0..4].copy_from_slice(&word0.to_be_bytes());
    b[4..8].copy_from_slice(&word1.to_be_bytes());
    b[8..12].copy_from_slice(&word2.to_be_bytes());
    b
}

// --- command builders (wire bytes per the clean-room opcode table) ---

/// `Move GPR g, imm v`.
fn move_gpr_imm(g: u16, v: u32) -> [u8; 12] {
    cmd(0x5040_0001, g as u32, v)
}

/// `JumpTitle imm t` — Branch sub_grp 1, branch_opt 1.
fn jump_title(t: u32) -> [u8; 12] {
    cmd(0x2181_0000, t, 0)
}

/// `CallTitle imm t` — Branch sub_grp 1, branch_opt 3.
fn call_title(t: u32) -> [u8; 12] {
    cmd(0x2183_0000, t, 0)
}

/// `PlayPL imm playlist` — Branch sub_grp 2, branch_opt 0, op_cnt 1, imm.
fn play_pl(playlist: u32) -> [u8; 12] {
    let word0 = (1u32 << 29) | (2u32 << 24) | (1 << 23);
    cmd(word0, playlist, 0)
}

/// `EQ PSR4, imm v` — Compare, op1 register PSR4 (0x80000004), op2 immediate,
/// op code 0x02 in byte 2.
fn eq_psr4_imm(v: u32) -> [u8; 12] {
    cmd(0x4840_0200, 0x8000_0004, v)
}

/// Hand-assemble a `MovieObject.bdmv` byte image from a list of command
/// lists (one per object).
fn build_mobj(objects: &[Vec<[u8; 12]>]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8; 32]); // 32 reserved
    body.extend_from_slice(&(objects.len() as u16).to_be_bytes());
    for cmds in objects {
        body.push(0); // flags byte0
        body.push(0); // reserved low byte
        body.extend_from_slice(&(cmds.len() as u16).to_be_bytes());
        for c in cmds {
            body.extend_from_slice(c);
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"MOBJ");
    out.extend_from_slice(b"0200");
    out.extend_from_slice(&[0u8; 4]); // extension_data_start
    out.extend_from_slice(&[0u8; 28]); // reserved
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

/// One `index.bdmv` Object() — HDMV variant (12 bytes): object_type=1 in
/// the top 2 bits of byte 0, then a reserved byte, then playback_type(2
/// bits) + 14 reserved, then the u16 movie_object_id_ref, then 6 reserved.
fn hdmv_object(movie_object_id: u16) -> [u8; 12] {
    let mut o = [0u8; 12];
    o[0] = 0b01_000000; // object_type = 1 (HDMV)
    o[4..6].copy_from_slice(&movie_object_id.to_be_bytes());
    o
}

/// Hand-assemble an `index.bdmv` byte image: a fixed header + AppInfoBDMV +
/// Indexes() (FirstPlayback, MenuTitle, N titles), per BD-ROM Part 3 §5.2.
fn build_index(first: u16, menu: u16, titles: &[u16]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"INDX");
    out.extend_from_slice(b"0200");
    let indexes_start_off = out.len();
    out.extend_from_slice(&[0u8; 4]); // indexes_start (backfilled)
    out.extend_from_slice(&[0u8; 4]); // extension_data_start = 0
    out.extend_from_slice(&[0u8; 24]); // 24 reserved

    // AppInfoBDMV: length(4) + 2 flag bytes + 32 user_data.
    out.extend_from_slice(&(2u32 + 32).to_be_bytes());
    out.push(0); // flags
    out.push((6 << 4) | 4); // video_format=6 (1080p) | frame_rate=4 (29.97)
    out.extend_from_slice(&[0u8; 32]); // user_data

    // Indexes(): length(4) + FirstPlayback + Menu + n_titles(2) + titles.
    let indexes_start = out.len();
    out.extend_from_slice(&[0u8; 4]); // indexes body length (backfilled)
    let body_start = out.len();
    out.extend_from_slice(&hdmv_object(first));
    out.extend_from_slice(&hdmv_object(menu));
    out.extend_from_slice(&(titles.len() as u16).to_be_bytes());
    for &t in titles {
        out.extend_from_slice(&hdmv_object(t));
    }
    let body_len = (out.len() - body_start) as u32;
    out[indexes_start..indexes_start + 4].copy_from_slice(&body_len.to_be_bytes());
    out[indexes_start_off..indexes_start_off + 4]
        .copy_from_slice(&(indexes_start as u32).to_be_bytes());
    out
}

#[test]
fn drive_first_playback_jumps_to_title_then_plays() {
    // index.bdmv: FirstPlayback → object 0, Menu → object 1,
    //   Title 1 → object 2, Title 2 → object 3.
    let index_bytes = build_index(0, 1, &[2, 3]);
    let index = IndexBdmv::parse(&index_bytes).expect("parse index.bdmv");

    // MovieObject.bdmv:
    //   object 0 (FirstPlayback): JumpTitle 2.
    //   object 1 (Menu):          (unused here)
    //   object 2 (Title 1):       (unused here)
    //   object 3 (Title 2):       EQ PSR4 == 2 (true, seeded) → PlayPL 7.
    let mobj_bytes = build_mobj(&[
        vec![jump_title(2)],
        vec![],
        vec![],
        vec![eq_psr4_imm(2), play_pl(7)],
    ]);
    let objects = MovieObjects::parse(&mobj_bytes).expect("parse MovieObject.bdmv");

    let mut driver = NavDriver::new(&index, &objects);
    // FirstPlayback jumps to Title 2; the driver re-resolves Title 2 → object
    // 3 through the index, seeds PSR4 = 2, the EQ holds, and PlayPL 7 is
    // surfaced as a resolved playback request.
    assert_eq!(
        driver.run(TitleEntry::FirstPlayback),
        DriveOutcome::Play(PlayRequest {
            playlist: 7,
            play_item: None,
            mark: None,
        })
    );
    assert_eq!(driver.current_entry(), TitleEntry::Title { number: 2 });

    // The streaming layer plays PlayList 7; resuming the (now-empty-tail)
    // object 3 finishes the navigation.
    assert_eq!(driver.resume(), DriveOutcome::Finished);
}

#[test]
fn drive_call_title_returns_with_shared_registers() {
    // FirstPlayback (object 0): CallTitle 1, then Move GPR0 = 100 after return.
    // Title 1 (object 1): Move GPR1 = 50, run off end → returns to caller.
    let index_bytes = build_index(0, 9, &[1]);
    let index = IndexBdmv::parse(&index_bytes).unwrap();
    let mobj_bytes = build_mobj(&[
        vec![call_title(1), move_gpr_imm(0, 100)],
        vec![move_gpr_imm(1, 50)],
    ]);
    let objects = MovieObjects::parse(&mobj_bytes).unwrap();

    let mut driver = NavDriver::new(&index, &objects);
    assert_eq!(
        driver.run(TitleEntry::FirstPlayback),
        DriveOutcome::Finished
    );
    // Callee's GPR1 write survived the return; caller's GPR0 ran after.
    assert_eq!(driver.registers().gpr(1), Some(50));
    assert_eq!(driver.registers().gpr(0), Some(100));
    assert_eq!(driver.title_call_depth(), 0);
}

#[test]
fn drive_top_menu_surfaces_setsystem() {
    // Menu object issues a SetSystem (SetButtonPage) the driver surfaces.
    // SetButtonPage: grp=2, sub_grp=1, op code 0x03, both operands immediate.
    let set_button_page = {
        let word0 = (2u32 << 29) | (2u32 << 27) | (1u32 << 24) | (1 << 23) | (1 << 22) | 0x03;
        cmd(word0, 0x0000_0001, 0x0000_0002)
    };
    let index_bytes = build_index(0, 1, &[]);
    let index = IndexBdmv::parse(&index_bytes).unwrap();
    let mobj_bytes = build_mobj(&[vec![], vec![set_button_page]]);
    let objects = MovieObjects::parse(&mobj_bytes).unwrap();

    let mut driver = NavDriver::new(&index, &objects);
    assert_eq!(
        driver.run(TitleEntry::TopMenu),
        DriveOutcome::Request(NavRequest::SetSystem {
            op: oxideav_bluray::Operation::SetButtonPage,
            operand1: 1,
            operand2: 2,
        })
    );
    // PSR4 carried the top-menu sentinel value while the menu ran.
    assert_eq!(driver.registers().psr(4), Some(0xFFFF));
    // After the player services the button-page change, the menu finishes.
    assert_eq!(driver.resume(), DriveOutcome::Finished);
}
