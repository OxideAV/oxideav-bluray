//! End-to-end HDMV navigation pipeline: hand-build a `MovieObject.bdmv`
//! byte image (raw big-endian wire bytes), parse it with the crate's
//! `MovieObjects::parse`, then *execute* the resulting command lists with
//! the `MobjRunner` against a shared PSR/GPR register file.
//!
//! This exercises the whole chain the crate now owns — wire bytes →
//! `MovieObjects` table → per-object `DecodedCommand` decode → VM register
//! mutation + inter-object Jump/Call/Resume — on a stream built from the
//! 12-byte command layout in
//! `docs/container/bluray/hdmv-navigation-commands.md` (not from the
//! crate's own `encode` helper for the command words, which are emitted by
//! hand here so the test pins the on-wire opcode bytes).

use oxideav_bluray::{MobjRunner, MovieObjects, NavRequest, RunOutcome};

/// A 12-byte navigation command from three big-endian words.
fn cmd(word0: u32, word1: u32, word2: u32) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[0..4].copy_from_slice(&word0.to_be_bytes());
    b[4..8].copy_from_slice(&word1.to_be_bytes());
    b[8..12].copy_from_slice(&word2.to_be_bytes());
    b
}

/// Hand-assemble a `MovieObject.bdmv` byte image from a list of objects,
/// each object a `(flags_byte0, commands)` pair.
///
/// Layout (per the crate's `movie_object` module + BD-ROM Part 3 §5.3):
///   "MOBJ" "0200" ext_start(4) reserved(28)
///   MovieObjects(): length(4) reserved(32) n_objects(2)
///     per object: flags(1) reserved(1) n_cmds(2) commands(12·n)
fn build_mobj(objects: &[(u8, Vec<[u8; 12]>)]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8; 32]); // 32 reserved
    body.extend_from_slice(&(objects.len() as u16).to_be_bytes());
    for (flags, cmds) in objects {
        body.push(*flags);
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

// --- Command builders (wire bytes per the clean-room opcode table) ---

/// `Move GPR g, imm v` — op_cnt=2, grp=2, sub_grp=0, op2 immediate.
fn move_gpr_imm(g: u16, v: u32) -> [u8; 12] {
    cmd(0x5040_0001, g as u32, v)
}

/// `Add GPR g, imm v` — Set sub_grp 0, op code 0x03.
fn add_gpr_imm(g: u16, v: u32) -> [u8; 12] {
    cmd(0x5040_0003, g as u32, v)
}

/// `EQ GPR g, imm v` — Compare, op1 register / op2 immediate, op code
/// 0x02 in byte 2 (worked-hex placement).
fn eq_gpr_imm(g: u16, v: u32) -> [u8; 12] {
    cmd(0x4840_0200, g as u32, v)
}

/// `JumpTitle imm t` — Branch, sub_grp=1, branch_opt=1, op_cnt=1, imm.
fn jump_title(t: u32) -> [u8; 12] {
    cmd(0x2181_0000, t, 0)
}

/// `JumpObject imm id` — Branch, sub_grp=1, branch_opt=0, op_cnt=1, imm.
fn jump_object(id: u32) -> [u8; 12] {
    cmd(0x2180_0000, id, 0)
}

/// `CallObject imm id` — Branch, sub_grp=1, branch_opt=2, op_cnt=1, imm.
fn call_object(id: u32) -> [u8; 12] {
    cmd(0x2182_0000, id, 0)
}

#[test]
fn parse_then_execute_arithmetic_object() {
    // One object: GPR0 = 6; GPR0 += 36  → 42.
    let image = build_mobj(&[(0, vec![move_gpr_imm(0, 6), add_gpr_imm(0, 36)])]);
    let table = MovieObjects::parse(&image).expect("parse MovieObject.bdmv");
    assert_eq!(table.movie_objects.len(), 1);
    assert_eq!(table.movie_objects[0].commands.len(), 2);

    let mut runner = MobjRunner::new(&table);
    assert_eq!(runner.run(0), RunOutcome::Finished);
    assert_eq!(runner.vm.registers.gpr(0), Some(42));
}

#[test]
fn parse_then_execute_compare_skip() {
    // GPR0 = 5; EQ GPR0, 9 (false → skip the next); Move GPR1, 1 (skipped);
    // Move GPR2, 2 (runs).
    let image = build_mobj(&[(
        0,
        vec![
            move_gpr_imm(0, 5),
            eq_gpr_imm(0, 9),
            move_gpr_imm(1, 1),
            move_gpr_imm(2, 2),
        ],
    )]);
    let table = MovieObjects::parse(&image).unwrap();
    let mut runner = MobjRunner::new(&table);
    assert_eq!(runner.run(0), RunOutcome::Finished);
    assert_eq!(runner.vm.registers.gpr(0), Some(5));
    assert_eq!(runner.vm.registers.gpr(1), Some(0)); // skipped
    assert_eq!(runner.vm.registers.gpr(2), Some(2));
}

#[test]
fn parse_then_execute_jump_title_yields() {
    // A First-Playback entry object that jumps to title 1.
    let image = build_mobj(&[(0, vec![jump_title(1)])]);
    let table = MovieObjects::parse(&image).unwrap();
    let mut runner = MobjRunner::new(&table);
    assert_eq!(
        runner.run(0),
        RunOutcome::Request(NavRequest::JumpTitle { title: 1 })
    );
}

#[test]
fn parse_then_execute_multi_object_call_chain() {
    // Object 0: Call object 1, then Jump to title 7 once it returns.
    // Object 1: Jump to object 2.
    // Object 2: set GPR5 = 99, run off end → returns to object 0.
    let image = build_mobj(&[
        (0, vec![call_object(1), jump_title(7)]),
        (0, vec![jump_object(2)]),
        (0, vec![move_gpr_imm(5, 99)]),
    ]);
    let table = MovieObjects::parse(&image).unwrap();
    assert_eq!(table.movie_objects.len(), 3);

    let mut runner = MobjRunner::new(&table);
    // Runs 0 → call 1 → jump 2 → set GPR5 → return to 0 → JumpTitle 7.
    assert_eq!(
        runner.run(0),
        RunOutcome::Request(NavRequest::JumpTitle { title: 7 })
    );
    assert_eq!(runner.vm.registers.gpr(5), Some(99));
    // After the call returned, we are back in object 0.
    assert_eq!(runner.current_object(), 0);
    assert_eq!(runner.call_depth(), 0);
    // The player services the JumpTitle; resuming the (now-empty-tail)
    // object 0 finishes the run.
    assert_eq!(runner.resume(), RunOutcome::Finished);
}

#[test]
fn player_seeds_psr_then_script_branches_on_it() {
    // Seed PSR4 (Title) = 2; script: EQ PSR4, 2 (true) → Move GPR0, 1.
    // EQ PSR4, imm 2 : op1 register PSR4 (0x80000004), op2 immediate.
    let eq_psr4_2 = cmd(0x4840_0200, 0x8000_0004, 0x0000_0002);
    let image = build_mobj(&[(0, vec![eq_psr4_2, move_gpr_imm(0, 1)])]);
    let table = MovieObjects::parse(&image).unwrap();

    let mut runner = MobjRunner::new(&table);
    runner.vm.registers.set_psr_player(4, 2);
    assert_eq!(runner.run(0), RunOutcome::Finished);
    assert_eq!(runner.vm.registers.gpr(0), Some(1));

    // With a different seed the compare is false and GPR0 stays 0.
    let mut runner2 = MobjRunner::new(&table);
    runner2.vm.registers.set_psr_player(4, 5);
    assert_eq!(runner2.run(0), RunOutcome::Finished);
    assert_eq!(runner2.vm.registers.gpr(0), Some(0));
}
