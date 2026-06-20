//! Movie Object runner — drive the whole `MovieObject.bdmv` table.
//!
//! [`vm::HdmvVm`](super::vm) steps a *single* command list. A real disc's
//! `MovieObject.bdmv` holds an *array* of Movie Objects, and control flows
//! between them: `JumpObject` switches object, `CallObject` switches object
//! after pushing a resume point, and `Resume` pops back. This module ties
//! the VM to the [`MovieObjects`](super::movie_object::MovieObjects) table
//! so a caller can run from an entry Movie Object and let the runner follow
//! the inter-object branches itself, sharing one register file across the
//! whole run.
//!
//! ## What the runner resolves vs yields
//!
//! The runner owns the part of the navigation model that is *intra-table*
//! and fully determined by the clean-room command semantics:
//!
//! - **`JumpObject id`** — switch to Movie Object `id`, PC at 0.
//! - **`CallObject id`** — push the current `(object, pc)` onto a resume
//!   stack, then switch to Movie Object `id`.
//! - **`Resume`** — pop the resume stack and continue the caller object at
//!   the command after its `CallObject`.
//! - An object's command list **running off the end** (or `Break`) acts as
//!   an implicit return: if the resume stack is non-empty the runner pops
//!   and continues the caller; otherwise the run ends.
//!
//! Everything that needs *disc / player* state the BDMV table alone does
//! not carry — `JumpTitle`, `CallTitle`, the `PlayPL*` family,
//! `TerminatePL`, `LinkPI`/`LinkMK`, and the `SetSystem` operations — is
//! surfaced to the caller as a [`RunOutcome::Request`] carrying the
//! [`NavRequest`]. The player layer (which owns `index.bdmv`, the PlayList
//! table, and the IG state machine) acts on it and may resume the runner.
//!
//! The shared register file ([`MobjRunner::vm`]) is what makes a multi-object
//! run meaningful: GPRs a called object sets are visible to the caller after
//! `Resume`, exactly as the single global register file on a player.
//!
//! Clean-room source: `docs/container/bluray/hdmv-navigation-commands.md`
//! (the Branch group's JumpObject / CallObject / Resume semantics + the
//! 12-byte command structure).

use super::movie_object::MovieObjects;
use super::vm::{HdmvVm, NavRequest, Step};

/// One frame of the resume stack: the Movie Object id to return to and the
/// program counter to resume at (the command after the `CallObject`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResumeFrame {
    object: usize,
    pc: usize,
}

/// Why a [`MobjRunner::run`] / [`MobjRunner::resume`] returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// The run completed: the entry object's command list (and any callers
    /// on the resume stack) finished, with the resume stack empty.
    Finished,
    /// The run paused on a navigation request the player layer must service
    /// (`JumpTitle`, a `PlayPL*`, `TerminatePL`, a `Link*`, or a
    /// `SetSystem`). Call [`MobjRunner::resume`] to continue after the
    /// player has acted on it.
    Request(NavRequest),
    /// A branch named a Movie Object id outside the table — the run stopped
    /// rather than indexing out of bounds. Carries the offending id.
    BadObject(u32),
    /// The per-run object-transition budget was exhausted (a pathological
    /// Jump/Call cycle across objects). The run stopped to stay bounded.
    BudgetExhausted,
}

/// Default budget on *object transitions* in a single `run`/`resume`, to
/// bound a pathological inter-object Jump/Call cycle. (Each object's own
/// command list is separately bounded by the VM's per-list step budget.)
pub const DEFAULT_OBJECT_BUDGET: u64 = 100_000;

/// Drives a [`MovieObjects`] table with a shared [`HdmvVm`] register file,
/// following inter-object Jump/Call/Resume branches.
#[derive(Debug, Clone)]
pub struct MobjRunner<'a> {
    objects: &'a MovieObjects,
    /// The navigation VM (register file + per-list PC). Public so a caller
    /// can seed PSRs (e.g. PSR4 Title) before running and inspect GPR/PSR
    /// results after.
    pub vm: HdmvVm,
    /// Index of the Movie Object currently executing.
    current: usize,
    /// Resume stack for `CallObject` / `Resume`.
    resume: Vec<ResumeFrame>,
    /// Object-transition budget for one `run`.
    object_budget: u64,
}

impl<'a> MobjRunner<'a> {
    /// A runner over `objects`, ready to enter a Movie Object by id.
    pub fn new(objects: &'a MovieObjects) -> Self {
        Self {
            objects,
            vm: HdmvVm::new(),
            current: 0,
            resume: Vec::new(),
            object_budget: DEFAULT_OBJECT_BUDGET,
        }
    }

    /// Replace the object-transition budget.
    pub fn set_object_budget(&mut self, budget: u64) {
        self.object_budget = budget;
    }

    /// The Movie Object index currently selected.
    pub fn current_object(&self) -> usize {
        self.current
    }

    /// Depth of the resume (Call) stack.
    pub fn call_depth(&self) -> usize {
        self.resume.len()
    }

    /// Enter Movie Object `entry` (PC 0, empty resume stack) and run until
    /// the table finishes or a player-serviced request is yielded.
    pub fn run(&mut self, entry: usize) -> RunOutcome {
        if entry >= self.objects.movie_objects.len() {
            return RunOutcome::BadObject(entry as u32);
        }
        self.current = entry;
        self.vm.set_pc(0);
        self.resume.clear();
        self.drive()
    }

    /// Continue a run that previously yielded a [`RunOutcome::Request`],
    /// after the player layer has serviced the request. Resumes the current
    /// object at the command after the branch.
    pub fn resume(&mut self) -> RunOutcome {
        self.drive()
    }

    /// The inner loop: step the current object's VM, handle the terminal
    /// `Step`, and follow inter-object branches.
    fn drive(&mut self) -> RunOutcome {
        let mut transitions = 0u64;
        loop {
            let commands = &self.objects.movie_objects[self.current].commands;
            match self.vm.run(commands) {
                Step::Halted => {
                    // Implicit return: pop a caller if any, else finished.
                    match self.resume.pop() {
                        Some(frame) => {
                            transitions += 1;
                            if transitions > self.object_budget {
                                return RunOutcome::BudgetExhausted;
                            }
                            self.current = frame.object;
                            self.vm.set_pc(frame.pc);
                        }
                        None => return RunOutcome::Finished,
                    }
                }
                Step::Branch(req) => match req {
                    NavRequest::JumpObject { mobj_id } => {
                        transitions += 1;
                        if transitions > self.object_budget {
                            return RunOutcome::BudgetExhausted;
                        }
                        match self.enter(mobj_id) {
                            Ok(()) => {}
                            Err(bad) => return RunOutcome::BadObject(bad),
                        }
                    }
                    NavRequest::CallObject { mobj_id } => {
                        transitions += 1;
                        if transitions > self.object_budget {
                            return RunOutcome::BudgetExhausted;
                        }
                        // Push the return point (current object, PC after
                        // the CallObject — the VM already advanced it).
                        self.resume.push(ResumeFrame {
                            object: self.current,
                            pc: self.vm.pc(),
                        });
                        match self.enter(mobj_id) {
                            Ok(()) => {}
                            Err(bad) => {
                                self.resume.pop();
                                return RunOutcome::BadObject(bad);
                            }
                        }
                    }
                    NavRequest::Resume => {
                        // Explicit Resume: pop the call stack. With nothing
                        // to resume, the run finishes.
                        match self.resume.pop() {
                            Some(frame) => {
                                transitions += 1;
                                if transitions > self.object_budget {
                                    return RunOutcome::BudgetExhausted;
                                }
                                self.current = frame.object;
                                self.vm.set_pc(frame.pc);
                            }
                            None => return RunOutcome::Finished,
                        }
                    }
                    // Everything else needs disc/player state: yield it.
                    other => return RunOutcome::Request(other),
                },
                // `run` only ever returns Halted or Branch.
                Step::Continue | Step::Skipped => unreachable!("run returns terminal Step"),
            }
        }
    }

    /// Switch to Movie Object `id` at PC 0, or report a bad id.
    fn enter(&mut self, id: u32) -> Result<(), u32> {
        let idx = id as usize;
        if idx >= self.objects.movie_objects.len() {
            return Err(id);
        }
        self.current = idx;
        self.vm.set_pc(0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bdmv::movie_object::{MovieObject, NavCommand};

    fn nc(word0: u32, word1: u32, word2: u32) -> NavCommand {
        let mut bytes = [0u8; 12];
        bytes[0..4].copy_from_slice(&word0.to_be_bytes());
        bytes[4..8].copy_from_slice(&word1.to_be_bytes());
        bytes[8..12].copy_from_slice(&word2.to_be_bytes());
        NavCommand { bytes }
    }

    fn move_gpr_imm(g: u16, v: u32) -> NavCommand {
        nc(0x5040_0001, g as u32, v)
    }

    fn obj(commands: Vec<NavCommand>) -> MovieObject {
        MovieObject {
            resume_intention_flag: 0,
            menu_call_mask: 0,
            title_search_mask: 0,
            commands,
        }
    }

    fn table(objects: Vec<MovieObject>) -> MovieObjects {
        MovieObjects {
            version: *b"0200",
            movie_objects: objects,
        }
    }

    #[test]
    fn single_object_runs_to_finish() {
        let t = table(vec![obj(vec![move_gpr_imm(0, 7)])]);
        let mut r = MobjRunner::new(&t);
        assert_eq!(r.run(0), RunOutcome::Finished);
        assert_eq!(r.vm.registers.gpr(0), Some(7));
    }

    #[test]
    fn jump_object_switches_and_shares_registers() {
        // Object 0 sets GPR0=1 then JumpObject 1; object 1 sets GPR1=2.
        // JumpObject 1 : grp=0, sub_grp=1, branch_opt=0, op_cnt=1, imm.
        let jump_obj_1 = nc(0x2180_0000, 0x0000_0001, 0);
        let t = table(vec![
            obj(vec![move_gpr_imm(0, 1), jump_obj_1]),
            obj(vec![move_gpr_imm(1, 2)]),
        ]);
        let mut r = MobjRunner::new(&t);
        assert_eq!(r.run(0), RunOutcome::Finished);
        assert_eq!(r.vm.registers.gpr(0), Some(1));
        assert_eq!(r.vm.registers.gpr(1), Some(2));
        assert_eq!(r.current_object(), 1);
    }

    #[test]
    fn call_object_then_implicit_return() {
        // Object 0: CallObject 1, then set GPR0=9 (runs after return).
        // Object 1: set GPR1=5, list ends → implicit return.
        // CallObject 1 : grp=0, sub_grp=1, branch_opt=2, op_cnt=1, imm.
        let call_obj_1 = nc(0x2182_0000, 0x0000_0001, 0);
        let t = table(vec![
            obj(vec![call_obj_1, move_gpr_imm(0, 9)]),
            obj(vec![move_gpr_imm(1, 5)]),
        ]);
        let mut r = MobjRunner::new(&t);
        assert_eq!(r.run(0), RunOutcome::Finished);
        assert_eq!(r.vm.registers.gpr(1), Some(5)); // callee ran
        assert_eq!(r.vm.registers.gpr(0), Some(9)); // caller resumed
        assert_eq!(r.call_depth(), 0);
        assert_eq!(r.current_object(), 0);
    }

    #[test]
    fn call_object_then_explicit_resume() {
        // Object 1 ends with an explicit Resume instead of running off end.
        // Resume : grp=0, sub_grp=1, branch_opt=4, op_cnt=0.
        let call_obj_1 = nc(0x2182_0000, 0x0000_0001, 0);
        let resume = nc(0x0104_0000, 0, 0);
        let t = table(vec![
            obj(vec![call_obj_1, move_gpr_imm(0, 3)]),
            obj(vec![move_gpr_imm(1, 4), resume]),
        ]);
        let mut r = MobjRunner::new(&t);
        assert_eq!(r.run(0), RunOutcome::Finished);
        assert_eq!(r.vm.registers.gpr(1), Some(4));
        assert_eq!(r.vm.registers.gpr(0), Some(3));
    }

    #[test]
    fn jump_title_is_yielded_then_resumable() {
        // Object 0: JumpTitle 2 (yields), then set GPR0=1 on resume.
        let jump_title_2 = nc(0x2181_0000, 0x0000_0002, 0);
        let t = table(vec![obj(vec![jump_title_2, move_gpr_imm(0, 1)])]);
        let mut r = MobjRunner::new(&t);
        assert_eq!(
            r.run(0),
            RunOutcome::Request(NavRequest::JumpTitle { title: 2 })
        );
        // Player serviced it; resume continues the object after the branch.
        assert_eq!(r.resume(), RunOutcome::Finished);
        assert_eq!(r.vm.registers.gpr(0), Some(1));
    }

    #[test]
    fn play_pl_is_yielded() {
        // PlayPL playlist=3 : grp=0, sub_grp=2, branch_opt=0, op_cnt=1, imm.
        let play_pl = {
            let word0 =
                (1u32 << 29) | (2u32 << 24) | (1 << 23) /* imm_op1 */;
            nc(word0, 0x0000_0003, 0)
        };
        let t = table(vec![obj(vec![play_pl])]);
        let mut r = MobjRunner::new(&t);
        assert_eq!(
            r.run(0),
            RunOutcome::Request(NavRequest::PlayPlayList { playlist: 3 })
        );
    }

    #[test]
    fn bad_jump_object_id_reported() {
        let jump_obj_5 = nc(0x2180_0000, 0x0000_0005, 0);
        let t = table(vec![obj(vec![jump_obj_5])]);
        let mut r = MobjRunner::new(&t);
        assert_eq!(r.run(0), RunOutcome::BadObject(5));
    }

    #[test]
    fn run_bad_entry_reported() {
        let t = table(vec![obj(vec![])]);
        let mut r = MobjRunner::new(&t);
        assert_eq!(r.run(3), RunOutcome::BadObject(3));
    }

    #[test]
    fn inter_object_jump_cycle_is_bounded() {
        // Two objects that JumpObject each other forever.
        let jump_0 = nc(0x2180_0000, 0x0000_0000, 0);
        let jump_1 = nc(0x2180_0000, 0x0000_0001, 0);
        let t = table(vec![obj(vec![jump_1]), obj(vec![jump_0])]);
        let mut r = MobjRunner::new(&t);
        r.set_object_budget(100);
        assert_eq!(r.run(0), RunOutcome::BudgetExhausted);
    }

    #[test]
    fn nested_calls_unwind_in_order() {
        // 0 calls 1; 1 calls 2; 2 sets GPR2; unwinds 2→1→0 setting markers.
        let call1 = nc(0x2182_0000, 0x0000_0001, 0);
        let call2 = nc(0x2182_0000, 0x0000_0002, 0);
        let t = table(vec![
            obj(vec![call1, move_gpr_imm(0, 10)]),
            obj(vec![call2, move_gpr_imm(1, 20)]),
            obj(vec![move_gpr_imm(2, 30)]),
        ]);
        let mut r = MobjRunner::new(&t);
        assert_eq!(r.run(0), RunOutcome::Finished);
        assert_eq!(r.vm.registers.gpr(2), Some(30));
        assert_eq!(r.vm.registers.gpr(1), Some(20));
        assert_eq!(r.vm.registers.gpr(0), Some(10));
        assert_eq!(r.call_depth(), 0);
    }
}
