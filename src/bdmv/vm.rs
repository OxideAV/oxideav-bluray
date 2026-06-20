//! HDMV navigation virtual-machine — a minimal command interpreter.
//!
//! The [`nav_command`](super::nav_command) module *decodes* the 12-byte
//! HDMV navigation commands carried by `MovieObject.bdmv` into structured
//! [`DecodedCommand`] records. This module *executes* a decoded command
//! list against a register file — the milestone above bare decode.
//!
//! ## What the VM does
//!
//! A [`HdmvVm`] holds the player register banks ([`Registers`]: 4096 GPRs
//! and 128 PSRs, all 32-bit) plus a program counter, and steps a
//! `&[NavCommand]` one command at a time:
//!
//! - **Set / register ops** ([`Operation::Move`] … [`Operation::ShiftRight`])
//!   mutate the destination register in place: copy, swap, the arithmetic
//!   group (Add/Sub/Mul/Div/Mod), the bitwise group (And/Or/Xor), single-bit
//!   set/clear, and the two shifts. These are fully resolved from the
//!   clean-room operation table; the VM evaluates them directly.
//! - **Compare ops** ([`Operation::Eq`] … [`Operation::Bc`]) evaluate the
//!   comparison of operand 1 against operand 2 and, per the clean-room doc,
//!   **skip the next command** when the comparison is *false* (the
//!   conditional-skip model: "if the comparison holds, the next command
//!   executes; otherwise it is skipped").
//! - **Branch ops** mutate the program counter ([`Operation::GoTo`]) or stop
//!   the list ([`Operation::Break`]); the *navigation* branches that leave
//!   the current command list (JumpTitle, PlayPL, CallObject, Resume, …) do
//!   not have a defined behaviour *inside* a single Movie Object — they hand
//!   control back to the player's title engine. The VM therefore **halts and
//!   yields** a [`NavRequest`] describing the requested transition, so the
//!   surrounding player layer can act on it. This keeps the VM a pure
//!   register/PC machine and the player-state transitions explicit.
//!
//! ## Scope (clean-room boundary)
//!
//! The operation *semantics* implemented here are exactly the ones the
//! clean-room table specifies as wire-format facts: the meaning of each
//! Set/Compare/Branch operation. Numeric edge cases that the public
//! clean-room material does **not** pin down — the rounding direction of
//! `Div`/`Mod` and the overflow behaviour of the arithmetic group — are
//! deferred to the member-gated BD-ROM Part 3 normative book. The VM uses
//! the only behaviour that is self-consistent for a bank of 32-bit unsigned
//! registers: **wrapping** unsigned arithmetic and **truncating** (toward
//! zero) unsigned integer division, with division/modulo by zero producing
//! `0` rather than trapping (a navigation script must not be able to crash
//! the player). These choices are documented per-op and flagged for a
//! future behavioural-RE companion; they affect only out-of-band inputs a
//! conforming authoring tool would not emit.
//!
//! Player-side state the VM does **not** model is reported, not faked:
//! resume-intention handling, the UO mask, and the IG button-state machine
//! all live above the VM. `SetSystem` operations (`SetStream`,
//! `SetButtonPage`, `EnableButton`, …) likewise mutate player/IG state that
//! is outside the register file, so they too are surfaced as a
//! [`NavRequest::SetSystem`] for the player layer rather than executed here.
//!
//! Clean-room source: `docs/container/bluray/hdmv-navigation-commands.md`.

use super::movie_object::NavCommand;
use super::nav_command::{CommandGroup, DecodedCommand, Operand, Operation, RegisterBank};
use super::register_model::{psr_info, GPR_COUNT, PSR_COUNT};

/// The HDMV register file: two banks of 32-bit unsigned registers.
///
/// - **GPRs** — [`GPR_COUNT`] (4096) general-purpose registers, freely
///   read/write by navigation commands.
/// - **PSRs** — [`PSR_COUNT`] (128) player-status registers. The VM lets a
///   navigation command write any PSR the register model marks as mutable
///   ([`super::register_model::PsrClass`]); writes to read-only / reserved
///   PSRs are dropped (the player owns those) rather than rejected, matching
///   the "navigation cannot mutate a Player Setting" rule.
#[derive(Clone)]
pub struct Registers {
    gpr: Box<[u32; GPR_COUNT as usize]>,
    psr: Box<[u32; PSR_COUNT as usize]>,
}

impl std::fmt::Debug for Registers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Avoid dumping 4096+128 zeroes; summarise instead.
        let gpr_set = self.gpr.iter().filter(|&&v| v != 0).count();
        let psr_set = self.psr.iter().filter(|&&v| v != 0).count();
        f.debug_struct("Registers")
            .field("gpr_nonzero", &gpr_set)
            .field("psr_nonzero", &psr_set)
            .finish()
    }
}

impl Default for Registers {
    fn default() -> Self {
        Self::new()
    }
}

impl Registers {
    /// A fresh register file with every register cleared to `0`.
    pub fn new() -> Self {
        Self {
            gpr: Box::new([0u32; GPR_COUNT as usize]),
            psr: Box::new([0u32; PSR_COUNT as usize]),
        }
    }

    /// Read a GPR. Returns `None` for an out-of-range index (≥ 4096).
    pub fn gpr(&self, index: u16) -> Option<u32> {
        self.gpr.get(index as usize).copied()
    }

    /// Read a PSR. Returns `None` for an out-of-range index (≥ 128).
    pub fn psr(&self, index: u8) -> Option<u32> {
        self.psr.get(index as usize).copied()
    }

    /// Write a GPR. Out-of-range indices are ignored.
    pub fn set_gpr(&mut self, index: u16, value: u32) {
        if let Some(slot) = self.gpr.get_mut(index as usize) {
            *slot = value;
        }
    }

    /// Write a PSR *as the player* (bypasses the navigation read-only
    /// guard). Out-of-range indices are ignored. Used to seed initial
    /// player state (e.g. PSR4 Title) before running a script.
    pub fn set_psr_player(&mut self, index: u8, value: u32) {
        if let Some(slot) = self.psr.get_mut(index as usize) {
            *slot = value;
        }
    }

    /// `true` if a navigation command is allowed to write this PSR — i.e.
    /// it is a mutable Playback-Status register, not a read-only one, a
    /// Player Setting, or reserved.
    fn psr_nav_writable(index: u8) -> bool {
        match psr_info(index) {
            Some(info) => !info.class.is_read_only_to_nav(),
            None => false,
        }
    }

    /// Write a PSR as a *navigation command* would: dropped if the target
    /// is read-only to navigation (Player Setting / read-only Playback
    /// Status / reserved), per the register-model class.
    fn set_psr_nav(&mut self, index: u8, value: u32) {
        if Self::psr_nav_writable(index) {
            if let Some(slot) = self.psr.get_mut(index as usize) {
                *slot = value;
            }
        }
    }
}

/// A reference to one register in the file, resolved from an operand word's
/// bank + index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegRef {
    Gpr(u16),
    Psr(u8),
}

impl RegRef {
    /// Resolve a register-reference operand to a concrete bank+index, or
    /// `None` if the operand is an immediate or the index is out of range
    /// for its bank.
    fn from_operand(op: Operand) -> Option<Self> {
        match op {
            Operand::Immediate(_) => None,
            Operand::Register { bank, index } => match bank {
                RegisterBank::Gpr => {
                    if index < GPR_COUNT {
                        Some(RegRef::Gpr(index))
                    } else {
                        None
                    }
                }
                RegisterBank::Psr | RegisterBank::PsrSecondary => u8::try_from(index)
                    .ok()
                    .filter(|&i| i < PSR_COUNT)
                    .map(RegRef::Psr),
            },
        }
    }

    fn read(self, regs: &Registers) -> u32 {
        match self {
            RegRef::Gpr(i) => regs.gpr(i).unwrap_or(0),
            RegRef::Psr(i) => regs.psr(i).unwrap_or(0),
        }
    }

    fn write(self, regs: &mut Registers, value: u32) {
        match self {
            RegRef::Gpr(i) => regs.set_gpr(i, value),
            RegRef::Psr(i) => regs.set_psr_nav(i, value),
        }
    }
}

/// Resolve an operand to its 32-bit value: an immediate directly, or the
/// current contents of a referenced register.
fn operand_value(op: Option<Operand>, regs: &Registers) -> u32 {
    match op {
        Some(Operand::Immediate(v)) => v,
        Some(reg @ Operand::Register { .. }) => {
            RegRef::from_operand(reg).map(|r| r.read(regs)).unwrap_or(0)
        }
        None => 0,
    }
}

/// A navigation transition the VM cannot perform itself — it hands one of
/// these back to the player's title engine, which owns the playback state.
///
/// The two operand fields carry the *resolved* operand values (immediates
/// taken literally, register references read from the file at the moment of
/// the branch), so the player layer does not need to re-resolve operands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavRequest {
    /// `JumpObject` — switch to another Movie Object by id (operand 1).
    JumpObject { mobj_id: u32 },
    /// `JumpTitle` — switch to a Title by number (operand 1).
    JumpTitle { title: u32 },
    /// `CallObject` — save resume state, then switch to a Movie Object.
    CallObject { mobj_id: u32 },
    /// `CallTitle` — save resume state, then switch to a Title.
    CallTitle { title: u32 },
    /// `Resume` — restore previously suspended playback.
    Resume,
    /// `PlayPL` — start playback of a PlayList.
    PlayPlayList { playlist: u32 },
    /// `PlayPLatPlayItem` — start a PlayList at a PlayItem.
    PlayPlayListAtPlayItem { playlist: u32, play_item: u32 },
    /// `PlayPLatMark` — start a PlayList at a PlayListMark.
    PlayPlayListAtMark { playlist: u32, mark: u32 },
    /// `TerminatePL` — stop playback of the current PlayList.
    TerminatePlayList,
    /// `LinkPI` — jump to a PlayItem within the current PlayList.
    LinkPlayItem { play_item: u32 },
    /// `LinkMK` — jump to a PlayListMark within the current PlayList.
    LinkMark { mark: u32 },
    /// A `SetSystem` operation: player/IG state mutation outside the
    /// register file. Carries the decoded [`Operation`] plus the two
    /// resolved operand values so the IG / player layer can act on it.
    SetSystem {
        op: Operation,
        operand1: u32,
        operand2: u32,
    },
}

/// The outcome of stepping the VM one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The command executed (register/PC mutation, or a no-op) and the VM
    /// is ready to step the next command. Carries the new program counter.
    Continue,
    /// A Compare command evaluated *false*: the next command was skipped.
    /// (The PC has already advanced past the skipped command.)
    Skipped,
    /// The command list ended cleanly — a `Break`, or the program counter
    /// ran off the end of the list.
    Halted,
    /// The command requested a navigation transition the player must
    /// perform. The VM has stopped; the program counter points at the
    /// command *after* the branch so the player can resume the list there
    /// if it chooses (e.g. after a `CallObject` returns).
    Branch(NavRequest),
}

/// A minimal HDMV navigation virtual machine: a register file plus a
/// program counter, stepping a decoded command list.
#[derive(Debug, Clone)]
pub struct HdmvVm {
    /// The player register banks (GPR + PSR).
    pub registers: Registers,
    /// The program counter: index of the next command to execute.
    pc: usize,
    /// Budget on executed steps, to bound a pathological `GoTo` loop.
    step_budget: u64,
    /// Steps executed so far against the budget.
    steps_taken: u64,
}

/// Default per-`run` step budget — generous for any real Movie Object but
/// finite, so a script that `GoTo`s itself in a tight loop terminates.
pub const DEFAULT_STEP_BUDGET: u64 = 1_000_000;

impl Default for HdmvVm {
    fn default() -> Self {
        Self::new()
    }
}

impl HdmvVm {
    /// A fresh VM with a cleared register file, PC at 0, and the default
    /// step budget.
    pub fn new() -> Self {
        Self {
            registers: Registers::new(),
            pc: 0,
            step_budget: DEFAULT_STEP_BUDGET,
            steps_taken: 0,
        }
    }

    /// The current program counter (index of the next command).
    pub fn pc(&self) -> usize {
        self.pc
    }

    /// Set the program counter (e.g. to enter a Movie Object at a command
    /// other than 0, or to resume after a returned `CallObject`).
    pub fn set_pc(&mut self, pc: usize) {
        self.pc = pc;
    }

    /// Replace the per-run step budget (number of commands `run` will
    /// execute before bailing with [`Step::Halted`]).
    pub fn set_step_budget(&mut self, budget: u64) {
        self.step_budget = budget;
    }

    /// Execute one command from `commands` at the current PC.
    ///
    /// Advances the PC. Returns the [`Step`] outcome. A PC past the end of
    /// the list yields [`Step::Halted`] without touching the registers.
    pub fn step(&mut self, commands: &[NavCommand]) -> Step {
        let Some(raw) = commands.get(self.pc) else {
            return Step::Halted;
        };
        let cmd = raw.decode();
        // PC advances to the following command by default; branches that
        // redirect override this.
        self.pc += 1;
        self.exec(&cmd, commands.len())
    }

    /// Run the command list from the current PC until it halts or requests
    /// a navigation transition, bounded by the step budget.
    ///
    /// Returns the terminal [`Step`]: [`Step::Halted`] (ran off the end /
    /// `Break` / budget exhausted) or [`Step::Branch`].
    pub fn run(&mut self, commands: &[NavCommand]) -> Step {
        self.steps_taken = 0;
        loop {
            if self.steps_taken >= self.step_budget {
                return Step::Halted;
            }
            self.steps_taken += 1;
            match self.step(commands) {
                Step::Continue | Step::Skipped => continue,
                terminal => return terminal,
            }
        }
    }

    /// Execute a single decoded command; `len` is the command-list length
    /// (for `GoTo` bounds + skip clamping).
    fn exec(&mut self, cmd: &DecodedCommand, len: usize) -> Step {
        match cmd.group {
            CommandGroup::Branch => self.exec_branch(cmd, len),
            CommandGroup::Compare => self.exec_compare(cmd, len),
            CommandGroup::Set => self.exec_set(cmd),
            CommandGroup::Reserved(_) => Step::Continue,
        }
    }

    /// Branch group: flow control. `GoTo`/`Break`/`Nop` are handled in the
    /// VM; the playback-leaving branches yield a [`NavRequest`].
    fn exec_branch(&mut self, cmd: &DecodedCommand, len: usize) -> Step {
        let op1 = operand_value(cmd.operand1, &self.registers);
        let op2 = operand_value(cmd.operand2, &self.registers);
        match cmd.operation {
            Operation::Nop => Step::Continue,
            Operation::GoTo => {
                // Jump to command index op1 within this object. An
                // out-of-range target halts the list.
                let target = op1 as usize;
                if target < len {
                    self.pc = target;
                    Step::Continue
                } else {
                    Step::Halted
                }
            }
            Operation::Break => Step::Halted,
            Operation::JumpObject => Step::Branch(NavRequest::JumpObject { mobj_id: op1 }),
            Operation::JumpTitle => Step::Branch(NavRequest::JumpTitle { title: op1 }),
            Operation::CallObject => Step::Branch(NavRequest::CallObject { mobj_id: op1 }),
            Operation::CallTitle => Step::Branch(NavRequest::CallTitle { title: op1 }),
            Operation::Resume => Step::Branch(NavRequest::Resume),
            Operation::PlayPL => Step::Branch(NavRequest::PlayPlayList { playlist: op1 }),
            Operation::PlayPLatPlayItem => Step::Branch(NavRequest::PlayPlayListAtPlayItem {
                playlist: op1,
                play_item: op2,
            }),
            Operation::PlayPLatMark => Step::Branch(NavRequest::PlayPlayListAtMark {
                playlist: op1,
                mark: op2,
            }),
            Operation::TerminatePL => Step::Branch(NavRequest::TerminatePlayList),
            Operation::LinkPI => Step::Branch(NavRequest::LinkPlayItem { play_item: op1 }),
            Operation::LinkMK => Step::Branch(NavRequest::LinkMark { mark: op1 }),
            // Any other operation reaching the Branch arm is an unknown
            // selector — treat as a no-op rather than trap.
            _ => Step::Continue,
        }
    }

    /// Compare group: evaluate op1 ⋈ op2; on *false*, skip the next command.
    fn exec_compare(&mut self, cmd: &DecodedCommand, len: usize) -> Step {
        let a = operand_value(cmd.operand1, &self.registers);
        let b = operand_value(cmd.operand2, &self.registers);
        let holds = match cmd.operation {
            Operation::Bc => (a & b) != 0,
            Operation::Eq => a == b,
            Operation::Ne => a != b,
            Operation::Ge => a >= b,
            Operation::Gt => a > b,
            Operation::Le => a <= b,
            Operation::Lt => a < b,
            // Unknown compare selector: treat as "holds" (don't skip).
            _ => true,
        };
        if holds {
            Step::Continue
        } else {
            // Skip the next command: advance the PC past it (clamped to the
            // list end).
            self.pc = (self.pc + 1).min(len);
            Step::Skipped
        }
    }

    /// Set group: register-mutation ops (sub-group 0) and SetSystem
    /// (sub-group 1, surfaced as a [`NavRequest`]).
    fn exec_set(&mut self, cmd: &DecodedCommand) -> Step {
        // SetSystem ops mutate player/IG state outside the register file.
        if cmd.sub_group == 1 {
            let op1 = operand_value(cmd.operand1, &self.registers);
            let op2 = operand_value(cmd.operand2, &self.registers);
            return Step::Branch(NavRequest::SetSystem {
                op: cmd.operation,
                operand1: op1,
                operand2: op2,
            });
        }

        // Register-mutation ops: dst = operand 1, src = operand 2.
        let Some(dst) = cmd.operand1.and_then(RegRef::from_operand) else {
            // No writable destination (immediate / out-of-range): no-op.
            return Step::Continue;
        };
        let cur = dst.read(&self.registers);
        let src = operand_value(cmd.operand2, &self.registers);

        match cmd.operation {
            Operation::Move => dst.write(&mut self.registers, src),
            Operation::Swap => {
                // Exchange dst and src. Only meaningful when src is also a
                // register; for an immediate src the swap degenerates to a
                // Move into dst (the immediate has nowhere to receive).
                if let Some(src_ref) = cmd.operand2.and_then(RegRef::from_operand) {
                    let tmp = src_ref.read(&self.registers);
                    src_ref.write(&mut self.registers, cur);
                    dst.write(&mut self.registers, tmp);
                } else {
                    dst.write(&mut self.registers, src);
                }
            }
            Operation::Add => dst.write(&mut self.registers, cur.wrapping_add(src)),
            Operation::Sub => dst.write(&mut self.registers, cur.wrapping_sub(src)),
            Operation::Mul => dst.write(&mut self.registers, cur.wrapping_mul(src)),
            Operation::Div => {
                // Truncating unsigned division; ÷0 → 0 (must not trap).
                dst.write(&mut self.registers, cur.checked_div(src).unwrap_or(0));
            }
            Operation::Mod => {
                // Modulo; mod 0 → 0 (must not trap).
                dst.write(&mut self.registers, cur.checked_rem(src).unwrap_or(0));
            }
            Operation::Rnd => {
                // rand(1..=src). Without a clean-room RNG spec the VM uses a
                // deterministic value (1) so a script's control flow is
                // reproducible; the actual entropy source is player-defined.
                let v = if src == 0 { 0 } else { 1 };
                dst.write(&mut self.registers, v);
            }
            Operation::And => dst.write(&mut self.registers, cur & src),
            Operation::Or => dst.write(&mut self.registers, cur | src),
            Operation::Xor => dst.write(&mut self.registers, cur ^ src),
            Operation::BitSet => {
                let bit = src & 31;
                dst.write(&mut self.registers, cur | (1u32 << bit));
            }
            Operation::BitClear => {
                let bit = src & 31;
                dst.write(&mut self.registers, cur & !(1u32 << bit));
            }
            Operation::ShiftLeft => {
                let v = if src >= 32 { 0 } else { cur << src };
                dst.write(&mut self.registers, v);
            }
            Operation::ShiftRight => {
                let v = if src >= 32 { 0 } else { cur >> src };
                dst.write(&mut self.registers, v);
            }
            // Unknown / non-register Set op: no-op.
            _ => {}
        }
        Step::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 12-byte NavCommand from three big-endian words.
    fn nc(word0: u32, word1: u32, word2: u32) -> NavCommand {
        let mut bytes = [0u8; 12];
        bytes[0..4].copy_from_slice(&word0.to_be_bytes());
        bytes[4..8].copy_from_slice(&word1.to_be_bytes());
        bytes[8..12].copy_from_slice(&word2.to_be_bytes());
        NavCommand { bytes }
    }

    /// `Move dst, src` where dst is GPR `g` and src is an immediate.
    fn move_gpr_imm(g: u16, v: u32) -> NavCommand {
        // 50400001 <gpr g> <v> : op_cnt=2, grp=2, sub_grp=0, op2 immediate.
        nc(0x5040_0001, g as u32, v)
    }

    /// A Set/register op with dst = GPR `g`, src = immediate.
    fn set_op_gpr_imm(op_code: u32, g: u16, v: u32) -> NavCommand {
        nc(0x5040_0000 | op_code, g as u32, v)
    }

    #[test]
    fn registers_default_zero() {
        let r = Registers::new();
        assert_eq!(r.gpr(0), Some(0));
        assert_eq!(r.gpr(4095), Some(0));
        assert_eq!(r.gpr(4096), None);
        assert_eq!(r.psr(0), Some(0));
        assert_eq!(r.psr(127), Some(0));
        assert_eq!(r.psr(128), None);
    }

    #[test]
    fn move_immediate_into_gpr() {
        let mut vm = HdmvVm::new();
        let prog = [move_gpr_imm(1, 0x1234)];
        assert_eq!(vm.step(&prog), Step::Continue);
        assert_eq!(vm.registers.gpr(1), Some(0x1234));
        assert_eq!(vm.pc(), 1);
        // Stepping past the end halts.
        assert_eq!(vm.step(&prog), Step::Halted);
    }

    #[test]
    fn arithmetic_group() {
        let mut vm = HdmvVm::new();
        let prog = [
            move_gpr_imm(0, 10),
            set_op_gpr_imm(0x03, 0, 5), // Add → 15
            set_op_gpr_imm(0x04, 0, 3), // Sub → 12
            set_op_gpr_imm(0x05, 0, 4), // Mul → 48
            set_op_gpr_imm(0x06, 0, 5), // Div → 9 (truncating)
            set_op_gpr_imm(0x07, 0, 4), // Mod → 1
        ];
        assert_eq!(vm.run(&prog), Step::Halted);
        assert_eq!(vm.registers.gpr(0), Some(1));
    }

    #[test]
    fn div_mod_by_zero_is_zero() {
        let mut vm = HdmvVm::new();
        let prog = [
            move_gpr_imm(0, 7),
            set_op_gpr_imm(0x06, 0, 0), // Div by 0 → 0
        ];
        vm.run(&prog);
        assert_eq!(vm.registers.gpr(0), Some(0));

        let mut vm2 = HdmvVm::new();
        let prog2 = [
            move_gpr_imm(1, 7),
            set_op_gpr_imm(0x07, 1, 0), // Mod by 0 → 0
        ];
        vm2.run(&prog2);
        assert_eq!(vm2.registers.gpr(1), Some(0));
    }

    #[test]
    fn bitwise_and_shift_and_bitops() {
        let mut vm = HdmvVm::new();
        let prog = [
            move_gpr_imm(0, 0b1010),
            set_op_gpr_imm(0x09, 0, 0b1100), // And → 0b1000
            set_op_gpr_imm(0x0A, 0, 0b0001), // Or  → 0b1001
            set_op_gpr_imm(0x0B, 0, 0b1111), // Xor → 0b0110
            set_op_gpr_imm(0x0C, 0, 0),      // BitSet bit0 → 0b0111
            set_op_gpr_imm(0x0D, 0, 1),      // BitClear bit1 → 0b0101
            set_op_gpr_imm(0x0E, 0, 1),      // Shl 1 → 0b1010
            set_op_gpr_imm(0x0F, 0, 2),      // Shr 2 → 0b0010
        ];
        vm.run(&prog);
        assert_eq!(vm.registers.gpr(0), Some(0b0010));
    }

    #[test]
    fn shift_by_32_or_more_is_zero() {
        let mut vm = HdmvVm::new();
        let prog = [
            move_gpr_imm(0, 0xFFFF_FFFF),
            set_op_gpr_imm(0x0E, 0, 32), // Shl 32 → 0
        ];
        vm.run(&prog);
        assert_eq!(vm.registers.gpr(0), Some(0));
    }

    #[test]
    fn add_wraps() {
        let mut vm = HdmvVm::new();
        let prog = [
            move_gpr_imm(0, 0xFFFF_FFFF),
            set_op_gpr_imm(0x03, 0, 1), // Add 1 → wraps to 0
        ];
        vm.run(&prog);
        assert_eq!(vm.registers.gpr(0), Some(0));
    }

    #[test]
    fn swap_two_gprs() {
        let mut vm = HdmvVm::new();
        // Swap GPR0, GPR1 : 50400002 <gpr0> <gpr1>, both register refs.
        let prog = [
            move_gpr_imm(0, 100),
            move_gpr_imm(1, 200),
            nc(0x5000_0002, 0x0000_0000, 0x0000_0001), // Swap GPR0 ↔ GPR1
        ];
        vm.run(&prog);
        assert_eq!(vm.registers.gpr(0), Some(200));
        assert_eq!(vm.registers.gpr(1), Some(100));
    }

    #[test]
    fn compare_true_runs_next_false_skips() {
        // EQ(1,1) true → next Move runs; EQ(1,2) false → next Move skipped.
        let mut vm = HdmvVm::new();
        let prog = [
            nc(0x48C0_0200, 1, 1), // EQ imm 1,1 → true
            move_gpr_imm(0, 0xAA), // runs
            nc(0x48C0_0200, 1, 2), // EQ imm 1,2 → false
            move_gpr_imm(1, 0xBB), // skipped
            move_gpr_imm(2, 0xCC), // runs
        ];
        vm.run(&prog);
        assert_eq!(vm.registers.gpr(0), Some(0xAA));
        assert_eq!(vm.registers.gpr(1), Some(0)); // never written
        assert_eq!(vm.registers.gpr(2), Some(0xCC));
    }

    #[test]
    fn compare_skip_reported() {
        let mut vm = HdmvVm::new();
        let prog = [
            nc(0x48C0_0300, 1, 1), // NE imm 1,1 → false
            move_gpr_imm(0, 1),
        ];
        assert_eq!(vm.step(&prog), Step::Skipped);
        // PC skipped past the Move.
        assert_eq!(vm.pc(), 2);
    }

    #[test]
    fn compare_against_register() {
        let mut vm = HdmvVm::new();
        let prog = [
            move_gpr_imm(0, 5),
            // GE GPR0, 5 : op_cnt=2, grp=1, op1 register / op2 immediate
            // 44400400 <gpr0> <5> — compare op code 0x04 (GE) in byte 2.
            nc(0x4440_0400, 0x0000_0000, 0x0000_0005),
            move_gpr_imm(1, 0x77), // runs iff GPR0 >= 5 (true)
        ];
        vm.run(&prog);
        assert_eq!(vm.registers.gpr(1), Some(0x77));
    }

    #[test]
    fn infinite_goto_loop_is_bounded_by_budget() {
        // GoTo 0 (immediate) at index 0 loops forever; the step budget
        // bounds it so `run` terminates with Halted instead of hanging.
        let mut vm = HdmvVm::new();
        vm.set_step_budget(50);
        let prog = [nc(0x2081_0000, 0x0000_0000, 0)]; // GoTo 0
        assert_eq!(vm.run(&prog), Step::Halted);
    }

    #[test]
    fn goto_redirects_pc() {
        let mut vm = HdmvVm::new();
        // GoTo immediate target 2 (skip the Move at index 1).
        // 20810000 <2> : imm_op1=1 (0x81 byte1 top bit), GoTo selector 1.
        let prog = [
            nc(0x2081_0000, 0x0000_0002, 0), // GoTo 2
            move_gpr_imm(0, 0xDEAD),         // skipped
            move_gpr_imm(1, 0xBEEF),         // runs
        ];
        vm.run(&prog);
        assert_eq!(vm.registers.gpr(0), Some(0));
        assert_eq!(vm.registers.gpr(1), Some(0xBEEF));
    }

    #[test]
    fn goto_out_of_range_halts() {
        let mut vm = HdmvVm::new();
        let prog = [nc(0x2081_0000, 0x0000_0063, 0)]; // GoTo 99, len 1
        assert_eq!(vm.run(&prog), Step::Halted);
    }

    #[test]
    fn break_halts() {
        let mut vm = HdmvVm::new();
        let prog = [
            nc(0x0002_0000, 0, 0), // Break
            move_gpr_imm(0, 1),    // never reached
        ];
        assert_eq!(vm.run(&prog), Step::Halted);
        assert_eq!(vm.registers.gpr(0), Some(0));
    }

    #[test]
    fn jump_title_yields_request() {
        let mut vm = HdmvVm::new();
        // JumpTitle 1 : 21810000 00000001.
        let prog = [nc(0x2181_0000, 0x0000_0001, 0)];
        assert_eq!(
            vm.run(&prog),
            Step::Branch(NavRequest::JumpTitle { title: 1 })
        );
        // PC points past the branch so the player could resume.
        assert_eq!(vm.pc(), 1);
    }

    #[test]
    fn play_pl_at_play_item_carries_both_operands() {
        // PlayPLatPlayItem playlist=5, playitem=2.
        // op_cnt=2, grp=0 (Branch), sub_grp=2 (Play), branch_opt=1, both
        // operands immediate (imm_op1+imm_op2).
        let word0 = (2u32 << 29) | (2u32 << 24) | (1 << 23) | (1 << 22) | (1 << 16);
        let prog = [nc(word0, 0x0000_0005, 0x0000_0002)];
        let mut vm = HdmvVm::new();
        assert_eq!(
            vm.run(&prog),
            Step::Branch(NavRequest::PlayPlayListAtPlayItem {
                playlist: 5,
                play_item: 2,
            })
        );
    }

    #[test]
    fn setsystem_yields_request_with_resolved_operands() {
        // SetButtonPage (op 3, sub_grp 1) with immediate operands.
        // op_cnt=2, grp=2, sub_grp=1, both immediate, op code 0x03 in byte3.
        let word0 = (2u32 << 29) | (2u32 << 27) | (1u32 << 24) | (1 << 23) | (1 << 22) | 0x03;
        let prog = [nc(word0, 0x0000_0007, 0x0000_0009)];
        let mut vm = HdmvVm::new();
        assert_eq!(
            vm.run(&prog),
            Step::Branch(NavRequest::SetSystem {
                op: Operation::SetButtonPage,
                operand1: 7,
                operand2: 9,
            })
        );
    }

    #[test]
    fn nav_write_to_readonly_psr_is_dropped() {
        let mut vm = HdmvVm::new();
        // PSR8 (PresentationTime) is read-only to nav. Move PSR8 ← 42.
        // dst = PSR8 register ref (0x80000008), src immediate 42.
        // 50400001 80000008 0000002A : op1 register, op2 immediate.
        let prog = [nc(0x5040_0001, 0x8000_0008, 42)];
        vm.run(&prog);
        // Write dropped.
        assert_eq!(vm.registers.psr(8), Some(0));
    }

    #[test]
    fn nav_write_to_mutable_psr_succeeds() {
        let mut vm = HdmvVm::new();
        // PSR4 (Title) is mutable Playback Status. Move PSR4 ← 3.
        let prog = [nc(0x5040_0001, 0x8000_0004, 3)];
        vm.run(&prog);
        assert_eq!(vm.registers.psr(4), Some(3));
    }

    #[test]
    fn player_seed_then_compare() {
        // Player seeds PSR4 (Title)=2; script compares PSR4 == 2 and on
        // true sets GPR0.
        let mut vm = HdmvVm::new();
        vm.registers.set_psr_player(4, 2);
        // EQ PSR4, 2 : op1 register, op2 immediate, op code 0x02 byte2.
        let prog = [
            nc(0x4840_0200, 0x8000_0004, 0x0000_0002),
            move_gpr_imm(0, 1),
        ];
        vm.run(&prog);
        assert_eq!(vm.registers.gpr(0), Some(1));
    }
}
