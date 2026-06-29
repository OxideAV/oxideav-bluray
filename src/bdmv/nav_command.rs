//! HDMV navigation-command opcode decode.
//!
//! Decodes the 12-byte HDMV navigation commands carried by
//! `MovieObject.bdmv` (and indirectly executed by the Blu-ray *Movie
//! Object* virtual machine) into structured opcode + operand records.
//!
//! Phase 1 of the crate surfaces commands as opaque
//! [`NavCommand`](super::movie_object::NavCommand) byte arrays; this
//! module adds a *decoder* on top, so callers can introspect the
//! group / sub-group / operation and the two operand words without an
//! execution engine. We do not execute the commands — the VM model
//! (resume-intention, UO-mask, IG button-state machine, arithmetic
//! overflow / rounding) is out of scope and lives in the member-gated
//! BD-ROM Part 3 normative book.
//!
//! ## Wire format (clean-room)
//!
//! Every command is three 4-byte big-endian words:
//!
//! ```text
//!  word0 (operation)   word1 (operand 1 / destination)   word2 (operand 2 / source)
//! ```
//!
//! `word0` bit fields (MSB = bit 31):
//!
//! ```text
//!  bit:  31 30 29 | 28 27 | 26 25 24 | 23 | 22 | 21 .. 4 | 3 2 1 0
//!        | op_cnt |  grp  | sub_grp  |im1 |im2 |  ...    | op code |
//! ```
//!
//! - `op_cnt` (3 bits) — number of operands actually used (0..2).
//! - `grp` (2 bits) — `0`=Branch, `1`=Compare, `2`=Set.
//! - `sub_grp` (3 bits) — sub-group; meaning depends on `grp`.
//! - `imm_op1` / `imm_op2` — operand addressing: `1`=immediate value,
//!   `0`=register reference. They occupy the top two bits of byte 1.
//! - For the **Branch** group the per-operation selector is the low
//!   nibble of byte 1 (`branch_opt`); byte 3 is `0`.
//! - For the **Set** group the per-operation selector is byte 3
//!   (`op code`); the branch nibble is `0`.
//! - For the **Compare** group the per-operation selector is byte **2**.
//!   The clean-room doc's prose says "byte 3", but every Compare worked
//!   hex (`EQ=48C00200`, `48400200`) places the op code in byte 2 with
//!   byte 3 = `0x00`; we follow the worked hex. See the doc-contradiction
//!   note on `decode_operation`.
//!
//! When an operand's immediate flag is clear, the operand word is a
//! register reference whose top two bits select the bank:
//!
//! ```text
//!  bit31=0          → GPR reference (index in low bits)
//!  bit31=1          → PSR reference (index in low bits)
//!  bit31=1,bit30=1  → register reference with the secondary
//!                      addressing flag set
//! ```
//!
//! Clean-room source: `docs/container/bluray/hdmv-navigation-commands.md`.

use super::movie_object::NavCommand;
use super::register_model::{psr_info, PsrInfo};

/// Command group (`grp`, bits 28-27 of the operation word).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandGroup {
    /// Flow control (`grp = 0`).
    Branch,
    /// Conditional compare (`grp = 1`).
    Compare,
    /// Register / system mutation (`grp = 2`).
    Set,
    /// Reserved / unknown group value (`grp = 3`).
    Reserved(u8),
}

impl CommandGroup {
    fn from_bits(grp: u8) -> Self {
        match grp {
            0 => CommandGroup::Branch,
            1 => CommandGroup::Compare,
            2 => CommandGroup::Set,
            other => CommandGroup::Reserved(other),
        }
    }
}

/// The decoded operation selector, identifying the concrete command
/// within its group. `Unknown` carries the raw (`sub_grp`, selector)
/// so a caller can still introspect a command we don't have a name
/// for without losing information.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operation {
    // --- Branch / GOTO (sub_grp 0) ---
    Nop,
    GoTo,
    Break,
    // --- Branch / JUMP (sub_grp 1) ---
    JumpObject,
    JumpTitle,
    CallObject,
    CallTitle,
    Resume,
    // --- Branch / PLAY (sub_grp 2) ---
    PlayPL,
    PlayPLatPlayItem,
    PlayPLatMark,
    TerminatePL,
    LinkPI,
    LinkMK,
    // --- Compare (grp 1) ---
    Bc,
    Eq,
    Ne,
    Ge,
    Gt,
    Le,
    Lt,
    // --- Set / register ops (sub_grp 0) ---
    Move,
    Swap,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Rnd,
    And,
    Or,
    Xor,
    BitSet,
    BitClear,
    ShiftLeft,
    ShiftRight,
    // --- Set / SetSystem ops (sub_grp 1) ---
    SetStream,
    SetNvTimer,
    SetButtonPage,
    EnableButton,
    DisableButton,
    SetSecondaryStream,
    PopUpOff,
    StillOn,
    StillOff,
    SetOutputMode,
    SetStreamSS,
    /// A command whose group/sub-group/selector we don't have a name
    /// for. Carries `(sub_grp, selector)` where `selector` is the
    /// branch nibble for the Branch group or the byte-3 op code for the
    /// Compare/Set groups.
    Unknown {
        sub_grp: u8,
        selector: u8,
    },
}

/// Which bank a register-reference operand targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegisterBank {
    /// General Purpose Register (bit 31 = 0).
    Gpr,
    /// Player Status Register (bit 31 = 1, bit 30 = 0).
    Psr,
    /// PSR/register reference with the secondary addressing flag set
    /// (bit 31 = 1, bit 30 = 1) — used by e.g. `SetButtonPage` for the
    /// page/button selector operand.
    PsrSecondary,
}

/// A decoded operand word: either an immediate 32-bit value, or a
/// register reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operand {
    /// A 32-bit immediate value (the operand's immediate flag was set).
    Immediate(u32),
    /// A register reference: the bank plus the raw register index from
    /// the low 12 bits.
    Register { bank: RegisterBank, index: u16 },
}

/// A register reference resolved against the HDMV register model: its
/// bank, index, and (for PSRs) the named [`PsrInfo`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedRegister {
    /// The register bank this operand targets.
    pub bank: RegisterBank,
    /// The register index (low 12 bits of the operand word).
    pub index: u16,
    /// For [`RegisterBank::Psr`]/[`RegisterBank::PsrSecondary`] with an
    /// in-range index (0..127), the named PSR; `None` for GPR references
    /// or an out-of-range PSR index.
    pub psr: Option<PsrInfo>,
}

impl Operand {
    /// Decode an operand word given its immediate flag.
    fn decode(word: u32, immediate: bool) -> Self {
        if immediate {
            Operand::Immediate(word)
        } else {
            let bank = match (word >> 30) & 0b11 {
                0b00 | 0b01 => RegisterBank::Gpr, // bit31 = 0
                0b10 => RegisterBank::Psr,        // bit31 = 1, bit30 = 0
                _ => RegisterBank::PsrSecondary,  // bit31 = 1, bit30 = 1
            };
            // Register index lives in the low 12 bits (GPR space is
            // 0..4095, which fits; PSR 0..127 also fits).
            Operand::Register {
                bank,
                index: (word & 0x0FFF) as u16,
            }
        }
    }

    /// Resolve a register-reference operand against the HDMV register
    /// model, attaching the named [`PsrInfo`] for PSR references.
    ///
    /// Returns `None` for an [`Operand::Immediate`] (immediates have no
    /// bank). For a GPR reference the `psr` field of the result is
    /// `None`; for a PSR reference it carries the named register when the
    /// index is in range (0..127).
    pub fn resolve_register(&self) -> Option<ResolvedRegister> {
        match *self {
            Operand::Immediate(_) => None,
            Operand::Register { bank, index } => {
                let psr = match bank {
                    RegisterBank::Gpr => None,
                    RegisterBank::Psr | RegisterBank::PsrSecondary => {
                        u8::try_from(index).ok().and_then(psr_info)
                    }
                };
                Some(ResolvedRegister { bank, index, psr })
            }
        }
    }
}

/// A fully decoded HDMV navigation command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedCommand {
    /// Number of operands the command actually uses (`op_cnt`, 0..2).
    pub operand_count: u8,
    /// Command group (`grp`).
    pub group: CommandGroup,
    /// Raw sub-group bits (`sub_grp`).
    pub sub_group: u8,
    /// Whether operand 1 is an immediate value.
    pub imm_op1: bool,
    /// Whether operand 2 is an immediate value.
    pub imm_op2: bool,
    /// The decoded operation.
    pub operation: Operation,
    /// Operand 1 (destination / branch target). `None` when
    /// `operand_count < 1`.
    pub operand1: Option<Operand>,
    /// Operand 2 (source). `None` when `operand_count < 2`.
    pub operand2: Option<Operand>,
}

impl DecodedCommand {
    /// Decode a raw 12-byte navigation command.
    ///
    /// This never fails: an unrecognised group/op falls back to
    /// [`Operation::Unknown`], preserving the raw selector bits so no
    /// information is lost.
    pub fn decode(bytes: &[u8; 12]) -> Self {
        let word0 = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let word1 = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let word2 = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);

        let operand_count = ((word0 >> 29) & 0b111) as u8;
        let grp_bits = ((word0 >> 27) & 0b11) as u8;
        let sub_group = ((word0 >> 24) & 0b111) as u8;
        let imm_op1 = ((word0 >> 23) & 1) == 1;
        let imm_op2 = ((word0 >> 22) & 1) == 1;
        // Branch group: selector is the low nibble of byte 1
        // (bits 19-16). The per-operation selector byte for the
        // Compare and Set groups differs by group — see the
        // doc-contradiction note on `decode_operation`:
        //   - Set group  → byte 3 (bits 7-0).
        //   - Compare group → byte 2 (bits 15-8) per the worked hex.
        let branch_opt = ((word0 >> 16) & 0x0F) as u8;
        let set_op_code = (word0 & 0xFF) as u8;
        let compare_op_code = ((word0 >> 8) & 0xFF) as u8;

        let group = CommandGroup::from_bits(grp_bits);
        let operation =
            decode_operation(group, sub_group, branch_opt, compare_op_code, set_op_code);

        let operand1 = if operand_count >= 1 {
            Some(Operand::decode(word1, imm_op1))
        } else {
            None
        };
        let operand2 = if operand_count >= 2 {
            Some(Operand::decode(word2, imm_op2))
        } else {
            None
        };

        DecodedCommand {
            operand_count,
            group,
            sub_group,
            imm_op1,
            imm_op2,
            operation,
            operand1,
            operand2,
        }
    }
}

impl NavCommand {
    /// Decode this command's opcode + operands. See [`DecodedCommand`].
    pub fn decode(&self) -> DecodedCommand {
        DecodedCommand::decode(&self.bytes)
    }
}

// Doc-contradiction note (`docs/container/bluray/hdmv-navigation-commands.md`):
// the bit-field prose (lines 87/93/163) says the Compare/Set per-operation
// op code is "carried in byte 3" (bits 7-0). That holds for the **Set**
// group's worked hex (`Move=50400001`, `SetButtonPage=51000003` — op code
// in byte 3). It does **not** hold for the **Compare** group: both Compare
// worked-hex examples (`EQ(imm,imm)=48C00200`, `compare PSR4 = 48400200`)
// place the op-code value `0x02` in byte **2** (bits 15-8), with byte 3 =
// `0x00`. The worked hex is the primary S1-sourced data the bit diagram
// (S7) was *derived* from, and both Compare examples agree with each other,
// so we resolve the contradiction in favour of the worked hex: Compare op
// code = byte 2, Set op code = byte 3. Flagged for a doc patch.
fn decode_operation(
    group: CommandGroup,
    sub_grp: u8,
    branch_opt: u8,
    compare_op_code: u8,
    set_op_code: u8,
) -> Operation {
    match group {
        CommandGroup::Branch => match (sub_grp, branch_opt) {
            // GOTO sub-group.
            (0, 0) => Operation::Nop,
            (0, 1) => Operation::GoTo,
            (0, 2) => Operation::Break,
            // JUMP sub-group.
            (1, 0) => Operation::JumpObject,
            (1, 1) => Operation::JumpTitle,
            (1, 2) => Operation::CallObject,
            (1, 3) => Operation::CallTitle,
            (1, 4) => Operation::Resume,
            // PLAY sub-group.
            (2, 0) => Operation::PlayPL,
            (2, 1) => Operation::PlayPLatPlayItem,
            (2, 2) => Operation::PlayPLatMark,
            (2, 3) => Operation::TerminatePL,
            (2, 4) => Operation::LinkPI,
            (2, 5) => Operation::LinkMK,
            _ => Operation::Unknown {
                sub_grp,
                selector: branch_opt,
            },
        },
        CommandGroup::Compare => match compare_op_code {
            0x01 => Operation::Bc,
            0x02 => Operation::Eq,
            0x03 => Operation::Ne,
            0x04 => Operation::Ge,
            0x05 => Operation::Gt,
            0x06 => Operation::Le,
            0x07 => Operation::Lt,
            _ => Operation::Unknown {
                sub_grp,
                selector: compare_op_code,
            },
        },
        CommandGroup::Set => match sub_grp {
            // Register / arithmetic / bitwise ops.
            0 => match set_op_code {
                0x01 => Operation::Move,
                0x02 => Operation::Swap,
                0x03 => Operation::Add,
                0x04 => Operation::Sub,
                0x05 => Operation::Mul,
                0x06 => Operation::Div,
                0x07 => Operation::Mod,
                0x08 => Operation::Rnd,
                0x09 => Operation::And,
                0x0A => Operation::Or,
                0x0B => Operation::Xor,
                0x0C => Operation::BitSet,
                0x0D => Operation::BitClear,
                0x0E => Operation::ShiftLeft,
                0x0F => Operation::ShiftRight,
                _ => Operation::Unknown {
                    sub_grp,
                    selector: set_op_code,
                },
            },
            // SetSystem ops.
            1 => match set_op_code {
                0x01 => Operation::SetStream,
                0x02 => Operation::SetNvTimer,
                0x03 => Operation::SetButtonPage,
                0x04 => Operation::EnableButton,
                0x05 => Operation::DisableButton,
                0x06 => Operation::SetSecondaryStream,
                0x07 => Operation::PopUpOff,
                0x08 => Operation::StillOn,
                0x09 => Operation::StillOff,
                0x0A => Operation::SetOutputMode,
                0x0B => Operation::SetStreamSS,
                _ => Operation::Unknown {
                    sub_grp,
                    selector: set_op_code,
                },
            },
            _ => Operation::Unknown {
                sub_grp,
                selector: set_op_code,
            },
        },
        CommandGroup::Reserved(_) => Operation::Unknown {
            sub_grp,
            selector: set_op_code,
        },
    }
}

// ---------------------------------------------------------------------------
// Disassembly: a human-readable single-line listing of a decoded command.
//
// This is pure derivation over the already-decoded [`DecodedCommand`] model
// plus the named [`PsrInfo`] / [`GprConvention`] register tables — it adds no
// new wire knowledge. The output is a diagnostic / forensic listing in the
// shape an authoring tool prints (e.g. `Move r1, 0x1` / `JumpTitle 1`), not a
// re-assemblable binary form. It is intended for log lines, dumps and tests,
// never for execution (the VM lives in `super::vm`).
// ---------------------------------------------------------------------------

impl Operation {
    /// A short uppercase-camel mnemonic for this operation, matching the
    /// variant name (`"Nop"`, `"JumpTitle"`, `"SetButtonPage"`, ...).
    ///
    /// [`Operation::Unknown`] renders as `"Unknown"` — its raw
    /// `(sub_grp, selector)` is surfaced separately by the command-level
    /// disassembler so the mnemonic stays a stable identifier.
    pub fn mnemonic(self) -> &'static str {
        match self {
            Operation::Nop => "Nop",
            Operation::GoTo => "GoTo",
            Operation::Break => "Break",
            Operation::JumpObject => "JumpObject",
            Operation::JumpTitle => "JumpTitle",
            Operation::CallObject => "CallObject",
            Operation::CallTitle => "CallTitle",
            Operation::Resume => "Resume",
            Operation::PlayPL => "PlayPL",
            Operation::PlayPLatPlayItem => "PlayPLatPlayItem",
            Operation::PlayPLatMark => "PlayPLatMark",
            Operation::TerminatePL => "TerminatePL",
            Operation::LinkPI => "LinkPI",
            Operation::LinkMK => "LinkMK",
            Operation::Bc => "Bc",
            Operation::Eq => "Eq",
            Operation::Ne => "Ne",
            Operation::Ge => "Ge",
            Operation::Gt => "Gt",
            Operation::Le => "Le",
            Operation::Lt => "Lt",
            Operation::Move => "Move",
            Operation::Swap => "Swap",
            Operation::Add => "Add",
            Operation::Sub => "Sub",
            Operation::Mul => "Mul",
            Operation::Div => "Div",
            Operation::Mod => "Mod",
            Operation::Rnd => "Rnd",
            Operation::And => "And",
            Operation::Or => "Or",
            Operation::Xor => "Xor",
            Operation::BitSet => "BitSet",
            Operation::BitClear => "BitClear",
            Operation::ShiftLeft => "ShiftLeft",
            Operation::ShiftRight => "ShiftRight",
            Operation::SetStream => "SetStream",
            Operation::SetNvTimer => "SetNvTimer",
            Operation::SetButtonPage => "SetButtonPage",
            Operation::EnableButton => "EnableButton",
            Operation::DisableButton => "DisableButton",
            Operation::SetSecondaryStream => "SetSecondaryStream",
            Operation::PopUpOff => "PopUpOff",
            Operation::StillOn => "StillOn",
            Operation::StillOff => "StillOff",
            Operation::SetOutputMode => "SetOutputMode",
            Operation::SetStreamSS => "SetStreamSS",
            Operation::Unknown { .. } => "Unknown",
        }
    }

    /// The group this operation belongs to (`Branch` / `Compare` / `Set`),
    /// derived from the operation alone. Returns `None` for
    /// [`Operation::Unknown`] (its group is not implied by the variant).
    pub fn group(self) -> Option<CommandGroup> {
        use Operation::*;
        let g = match self {
            Nop | GoTo | Break | JumpObject | JumpTitle | CallObject | CallTitle | Resume
            | PlayPL | PlayPLatPlayItem | PlayPLatMark | TerminatePL | LinkPI | LinkMK => {
                CommandGroup::Branch
            }
            Bc | Eq | Ne | Ge | Gt | Le | Lt => CommandGroup::Compare,
            Move | Swap | Add | Sub | Mul | Div | Mod | Rnd | And | Or | Xor | BitSet
            | BitClear | ShiftLeft | ShiftRight | SetStream | SetNvTimer | SetButtonPage
            | EnableButton | DisableButton | SetSecondaryStream | PopUpOff | StillOn | StillOff
            | SetOutputMode | SetStreamSS => CommandGroup::Set,
            Unknown { .. } => return None,
        };
        Some(g)
    }

    /// `true` when this operation, executed, can transfer control away
    /// from the current command stream (the Branch group) — i.e. it is a
    /// jump, call, resume, play or link rather than a register/compare op.
    pub fn is_branch(self) -> bool {
        matches!(self.group(), Some(CommandGroup::Branch))
    }

    /// `true` when this operation leaves HDMV command execution to begin
    /// (or stop) PlayList playback — the PLAY sub-group. A VM yields control
    /// to the player layer on these.
    pub fn is_playback(self) -> bool {
        use Operation::*;
        matches!(
            self,
            PlayPL | PlayPLatPlayItem | PlayPLatMark | TerminatePL | LinkPI | LinkMK
        )
    }
}

impl Operand {
    /// Render this operand in the single-line listing form:
    ///
    /// - an immediate as `0x..` (hex, the form authoring tools print);
    /// - a GPR reference as `r<index>` (e.g. `r12`);
    /// - a PSR reference as `PSR<index>` with the named register appended
    ///   when known (e.g. `PSR4(Title)`); a secondary-addressing PSR
    ///   reference gains a trailing `'` (e.g. `PSR4087'`).
    pub fn disassemble(&self) -> String {
        match *self {
            Operand::Immediate(v) => format!("0x{v:X}"),
            Operand::Register { bank, index } => match bank {
                RegisterBank::Gpr => format!("r{index}"),
                RegisterBank::Psr | RegisterBank::PsrSecondary => {
                    let tick = if bank == RegisterBank::PsrSecondary {
                        "'"
                    } else {
                        ""
                    };
                    match self.resolve_register().and_then(|r| r.psr) {
                        Some(p) if p.name != "reserved" => {
                            format!("PSR{index}{tick}({})", p.name)
                        }
                        _ => format!("PSR{index}{tick}"),
                    }
                }
            },
        }
    }
}

impl DecodedCommand {
    /// A single-line disassembly of this command.
    ///
    /// The form is `<mnemonic> [operand1[, operand2]]`. A
    /// [`Operation::Unknown`] additionally carries its raw selector, e.g.
    /// `Unknown(sub_grp=0,sel=0x7)`. Operands are rendered by
    /// [`Operand::disassemble`]. This is a diagnostic listing only — it is
    /// not a re-assemblable binary representation.
    pub fn disassemble(&self) -> String {
        let mut s = match self.operation {
            Operation::Unknown { sub_grp, selector } => {
                format!("Unknown(sub_grp={sub_grp},sel=0x{selector:X})")
            }
            op => op.mnemonic().to_string(),
        };
        if let Some(op1) = &self.operand1 {
            s.push(' ');
            s.push_str(&op1.disassemble());
            if let Some(op2) = &self.operand2 {
                s.push_str(", ");
                s.push_str(&op2.disassemble());
            }
        }
        s
    }
}

impl core::fmt::Display for DecodedCommand {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.disassemble())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(word0: u32, word1: u32, word2: u32) -> DecodedCommand {
        let mut bytes = [0u8; 12];
        bytes[0..4].copy_from_slice(&word0.to_be_bytes());
        bytes[4..8].copy_from_slice(&word1.to_be_bytes());
        bytes[8..12].copy_from_slice(&word2.to_be_bytes());
        DecodedCommand::decode(&bytes)
    }

    // --- Worked hex examples straight from the clean-room doc ---

    #[test]
    fn nop() {
        let d = cmd(0x0000_0000, 0, 0);
        assert_eq!(d.group, CommandGroup::Branch);
        assert_eq!(d.sub_group, 0);
        assert_eq!(d.operation, Operation::Nop);
        assert_eq!(d.operand_count, 0);
        assert_eq!(d.operand1, None);
        assert_eq!(d.operand2, None);
    }

    #[test]
    fn goto() {
        let d = cmd(0x2001_0000, 0x0000_000C, 0);
        assert_eq!(d.group, CommandGroup::Branch);
        assert_eq!(d.operation, Operation::GoTo);
        assert_eq!(d.operand_count, 1);
        // imm_op1 = 0 in 0x2001_0000 (byte1 = 0x01, top bit clear) →
        // operand 1 is a register reference (GPR target / command id).
        assert!(!d.imm_op1);
        assert_eq!(
            d.operand1,
            Some(Operand::Register {
                bank: RegisterBank::Gpr,
                index: 0x00C,
            })
        );
    }

    #[test]
    fn break_op() {
        let d = cmd(0x0002_0000, 0, 0);
        assert_eq!(d.group, CommandGroup::Branch);
        assert_eq!(d.operation, Operation::Break);
        assert_eq!(d.operand_count, 0);
    }

    #[test]
    fn jump_object() {
        let d = cmd(0x2180_0000, 0x0000_0001, 0);
        assert_eq!(d.operation, Operation::JumpObject);
        assert_eq!(d.sub_group, 1);
        assert!(d.imm_op1);
        assert_eq!(d.operand1, Some(Operand::Immediate(1)));
    }

    #[test]
    fn jump_title() {
        let d = cmd(0x2181_0000, 0x0000_0001, 0);
        assert_eq!(d.operand_count, 1);
        assert_eq!(d.group, CommandGroup::Branch);
        assert_eq!(d.sub_group, 1);
        assert!(d.imm_op1);
        assert!(!d.imm_op2);
        assert_eq!(d.operation, Operation::JumpTitle);
        assert_eq!(d.operand1, Some(Operand::Immediate(1)));
        assert_eq!(d.operand2, None);
    }

    #[test]
    fn call_object_and_title() {
        assert_eq!(cmd(0x2182_0000, 1, 0).operation, Operation::CallObject);
        assert_eq!(cmd(0x2183_0000, 1, 0).operation, Operation::CallTitle);
    }

    #[test]
    fn resume() {
        let d = cmd(0x0104_0000, 0, 0);
        assert_eq!(d.operation, Operation::Resume);
        assert_eq!(d.sub_group, 1);
        assert_eq!(d.operand_count, 0);
    }

    #[test]
    fn terminate_pl() {
        let d = cmd(0x0203_0000, 0, 0);
        assert_eq!(d.operation, Operation::TerminatePL);
        assert_eq!(d.sub_group, 2);
    }

    #[test]
    fn link_pi_and_mk() {
        let pi = cmd(0x2284_0000, 0x0000_0001, 0);
        assert_eq!(pi.operation, Operation::LinkPI);
        assert_eq!(pi.sub_group, 2);
        assert!(pi.imm_op1);
        assert_eq!(pi.operand1, Some(Operand::Immediate(1)));

        let mk = cmd(0x2285_0000, 0x0000_0001, 0);
        assert_eq!(mk.operation, Operation::LinkMK);
        assert_eq!(mk.sub_group, 2);
    }

    #[test]
    fn eq_immediate_immediate() {
        // 48C00200 00000001 00000001 = EQ of 1 vs 1.
        let d = cmd(0x48C0_0200, 0x0000_0001, 0x0000_0001);
        assert_eq!(d.group, CommandGroup::Compare);
        assert_eq!(d.operation, Operation::Eq);
        assert_eq!(d.operand_count, 2);
        assert!(d.imm_op1);
        assert!(d.imm_op2);
        assert_eq!(d.operand1, Some(Operand::Immediate(1)));
        assert_eq!(d.operand2, Some(Operand::Immediate(1)));
    }

    #[test]
    fn compare_register_immediate() {
        // 48400200 80000004 0000FFFF = compare PSR4 against 0xFFFF.
        // (op_cnt=2, grp=1, 40 = op2 immediate / op1 register.)
        let d = cmd(0x4840_0200, 0x8000_0004, 0x0000_FFFF);
        assert_eq!(d.operation, Operation::Eq);
        assert!(!d.imm_op1);
        assert!(d.imm_op2);
        assert_eq!(
            d.operand1,
            Some(Operand::Register {
                bank: RegisterBank::Psr,
                index: 4,
            })
        );
        assert_eq!(d.operand2, Some(Operand::Immediate(0xFFFF)));
    }

    #[test]
    fn all_compare_opcodes() {
        for (code, op) in [
            (0x01u8, Operation::Bc),
            (0x02, Operation::Eq),
            (0x03, Operation::Ne),
            (0x04, Operation::Ge),
            (0x05, Operation::Gt),
            (0x06, Operation::Le),
            (0x07, Operation::Lt),
        ] {
            // grp=1, op_cnt=2, both immediate (0xC0). Compare op code
            // lives in byte 2 (bits 15-8), per the worked hex.
            let word0 = 0x4800_0000 | 0x00C0_0000 | ((code as u32) << 8);
            assert_eq!(cmd(word0, 0, 0).operation, op);
        }
    }

    #[test]
    fn move_reg_immediate() {
        // 50400001 00000001 00000001 = Move GPR1 ← 1.
        let d = cmd(0x5040_0001, 0x0000_0001, 0x0000_0001);
        assert_eq!(d.group, CommandGroup::Set);
        assert_eq!(d.sub_group, 0);
        assert_eq!(d.operation, Operation::Move);
        assert!(!d.imm_op1);
        assert!(d.imm_op2);
        assert_eq!(
            d.operand1,
            Some(Operand::Register {
                bank: RegisterBank::Gpr,
                index: 1,
            })
        );
        assert_eq!(d.operand2, Some(Operand::Immediate(1)));
    }

    #[test]
    fn all_set_register_opcodes() {
        for (code, op) in [
            (0x01u8, Operation::Move),
            (0x02, Operation::Swap),
            (0x03, Operation::Add),
            (0x04, Operation::Sub),
            (0x05, Operation::Mul),
            (0x06, Operation::Div),
            (0x07, Operation::Mod),
            (0x08, Operation::Rnd),
            (0x09, Operation::And),
            (0x0A, Operation::Or),
            (0x0B, Operation::Xor),
            (0x0C, Operation::BitSet),
            (0x0D, Operation::BitClear),
            (0x0E, Operation::ShiftLeft),
            (0x0F, Operation::ShiftRight),
        ] {
            // grp=2, sub_grp=0, op_cnt=2.
            let word0 = 0x5000_0000 | (code as u32);
            assert_eq!(cmd(word0, 0, 0).operation, op);
        }
    }

    #[test]
    fn set_button_page() {
        // 51000003 80000FF6 C0000FF7.
        let d = cmd(0x5100_0003, 0x8000_0FF6, 0xC000_0FF7);
        assert_eq!(d.group, CommandGroup::Set);
        assert_eq!(d.sub_group, 1);
        assert_eq!(d.operation, Operation::SetButtonPage);
        assert_eq!(d.operand_count, 2);
        // Both operands register references; op2 has the secondary
        // addressing flag set (0xC...).
        assert_eq!(
            d.operand1,
            Some(Operand::Register {
                bank: RegisterBank::Psr,
                index: 0xFF6,
            })
        );
        assert_eq!(
            d.operand2,
            Some(Operand::Register {
                bank: RegisterBank::PsrSecondary,
                index: 0xFF7,
            })
        );
    }

    #[test]
    fn all_setsystem_opcodes() {
        for (code, op) in [
            (0x01u8, Operation::SetStream),
            (0x02, Operation::SetNvTimer),
            (0x03, Operation::SetButtonPage),
            (0x04, Operation::EnableButton),
            (0x05, Operation::DisableButton),
            (0x06, Operation::SetSecondaryStream),
            (0x07, Operation::PopUpOff),
            (0x08, Operation::StillOn),
            (0x09, Operation::StillOff),
            (0x0A, Operation::SetOutputMode),
            (0x0B, Operation::SetStreamSS),
        ] {
            // grp=2, sub_grp=1.
            let word0 = 0x5100_0000 | (code as u32);
            assert_eq!(cmd(word0, 0, 0).operation, op);
        }
    }

    #[test]
    fn unknown_branch_op_preserves_selector() {
        // Branch, GOTO sub-group, unrecognised selector 7. The branch
        // selector is the low nibble of byte 1 (bits 19-16), so a
        // selector of 7 is word0 = 0x0007_0000.
        let d = cmd(0x0007_0000, 0, 0);
        assert_eq!(d.group, CommandGroup::Branch);
        assert_eq!(
            d.operation,
            Operation::Unknown {
                sub_grp: 0,
                selector: 7,
            }
        );
    }

    #[test]
    fn unknown_set_opcode_preserves_selector() {
        // Set, sub_grp 0, op code 0xFF (not a defined op).
        let d = cmd(0x5000_00FF, 0, 0);
        assert_eq!(d.group, CommandGroup::Set);
        assert_eq!(
            d.operation,
            Operation::Unknown {
                sub_grp: 0,
                selector: 0xFF,
            }
        );
    }

    #[test]
    fn reserved_group_three() {
        // grp = 3 (bits 28-27 set).
        let d = cmd(0x1800_0000, 0, 0);
        assert_eq!(d.group, CommandGroup::Reserved(3));
        assert!(matches!(d.operation, Operation::Unknown { .. }));
    }

    #[test]
    fn nav_command_decode_passthrough() {
        let nc = NavCommand {
            bytes: {
                let mut b = [0u8; 12];
                b[0..4].copy_from_slice(&0x2181_0000u32.to_be_bytes());
                b[4..8].copy_from_slice(&1u32.to_be_bytes());
                b
            },
        };
        assert_eq!(nc.decode().operation, Operation::JumpTitle);
    }

    // --- Disassembly ---

    #[test]
    fn disasm_nop_no_operands() {
        let d = cmd(0x0000_0000, 0, 0);
        assert_eq!(d.disassemble(), "Nop");
        assert_eq!(d.to_string(), "Nop");
    }

    #[test]
    fn disasm_goto_gpr_operand() {
        // GoTo with a register (GPR) operand 1.
        let d = cmd(0x2001_0000, 0x0000_000C, 0);
        assert_eq!(d.disassemble(), "GoTo r12");
    }

    #[test]
    fn disasm_jump_title_immediate() {
        let d = cmd(0x2181_0000, 0x0000_0001, 0);
        assert_eq!(d.disassemble(), "JumpTitle 0x1");
    }

    #[test]
    fn disasm_move_reg_immediate() {
        // Move GPR1 ← 1.
        let d = cmd(0x5040_0001, 0x0000_0001, 0x0000_0001);
        assert_eq!(d.disassemble(), "Move r1, 0x1");
    }

    #[test]
    fn disasm_compare_named_psr() {
        // compare PSR4 (Title) against 0xFFFF.
        let d = cmd(0x4840_0200, 0x8000_0004, 0x0000_FFFF);
        assert_eq!(d.disassemble(), "Eq PSR4(Title), 0xFFFF");
    }

    #[test]
    fn disasm_secondary_psr_tick() {
        // SetButtonPage with a primary + secondary-addressing PSR operand.
        // Indices 0xFF6/0xFF7 are out of the named 0..127 PSR range, so no
        // (name) suffix; the secondary operand carries the trailing tick.
        let d = cmd(0x5100_0003, 0x8000_0FF6, 0xC000_0FF7);
        assert_eq!(d.disassemble(), "SetButtonPage PSR4086, PSR4087'");
    }

    #[test]
    fn disasm_unknown_carries_selector() {
        let d = cmd(0x0007_0000, 0, 0);
        assert_eq!(d.disassemble(), "Unknown(sub_grp=0,sel=0x7)");
    }

    #[test]
    fn operation_group_and_predicates() {
        assert_eq!(Operation::JumpTitle.group(), Some(CommandGroup::Branch));
        assert_eq!(Operation::Eq.group(), Some(CommandGroup::Compare));
        assert_eq!(Operation::Move.group(), Some(CommandGroup::Set));
        assert_eq!(
            Operation::Unknown {
                sub_grp: 0,
                selector: 0
            }
            .group(),
            None
        );

        assert!(Operation::JumpTitle.is_branch());
        assert!(!Operation::Move.is_branch());
        assert!(Operation::PlayPL.is_playback());
        assert!(Operation::LinkMK.is_playback());
        assert!(!Operation::JumpTitle.is_playback());
        assert!(!Operation::Add.is_playback());
    }

    #[test]
    fn mnemonic_matches_decode_group() {
        // Every decoded operation's `group()` (when Some) must agree with
        // the `CommandGroup` the decoder assigned, so the disassembler's
        // group view never contradicts the wire decode.
        for code in 0x01u8..=0x07 {
            let word0 = 0x4800_0000 | 0x00C0_0000 | ((code as u32) << 8);
            let d = cmd(word0, 0, 0);
            assert_eq!(d.operation.group(), Some(d.group));
        }
        for code in 0x01u8..=0x0F {
            let d = cmd(0x5000_0000 | code as u32, 0, 0);
            assert_eq!(d.operation.group(), Some(d.group));
        }
    }

    #[test]
    fn gpr_reference_high_index() {
        // GPR 4095 (max) as a register operand: bit31=0, low 12 bits.
        let d = cmd(0x5040_0001, 0x0000_0FFF, 0x0000_0001);
        assert_eq!(
            d.operand1,
            Some(Operand::Register {
                bank: RegisterBank::Gpr,
                index: 0x0FFF,
            })
        );
    }
}
