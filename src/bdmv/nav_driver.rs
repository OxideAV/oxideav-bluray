//! Title-level HDMV navigation driver — the disc's title engine.
//!
//! [`MobjRunner`](super::mobj_runner) drives a single `MovieObject.bdmv`
//! table, following the *intra-table* branches (`JumpObject`, `CallObject`,
//! `Resume`). What it cannot do alone is the *inter-title* navigation that
//! needs `index.bdmv`: a `JumpTitle`/`CallTitle` names a **title number**,
//! and only `index.bdmv` knows which Movie Object implements that title.
//! This module is the layer above the runner that closes that loop.
//!
//! ## What the driver owns
//!
//! A [`NavDriver`] holds the parsed [`IndexBdmv`], a [`MobjRunner`] over the
//! parsed [`MovieObjects`] table (which carries the shared register file),
//! and a title-call stack. Starting from an entry point
//! ([`TitleEntry::FirstPlayback`] / [`TitleEntry::TopMenu`] / a numbered
//! [`TitleEntry::Title`]) it:
//!
//! 1. Resolves the entry to a Movie Object id via the index
//!    ([`IndexBdmv::resolve_movie_object`]).
//! 2. Seeds **PSR4 (Title)** with the title number it entered, so a script
//!    that compares against PSR4 sees the title it is running in (per the
//!    register model: PSR4 = "title number, b15–b0").
//! 3. Runs the [`MobjRunner`] over the object table.
//! 4. When the runner yields a navigation request the *title engine* owns —
//!    `JumpTitle`, `CallTitle`, and a title-context `Resume` — the driver
//!    **services it itself**: it re-resolves the named title through the
//!    index, seeds PSR4, and re-enters the runner at the new object. This is
//!    the inter-title transition the runner surfaces but does not perform.
//! 5. Requests that need the *disc / streaming* layer (`PlayPL*`,
//!    `TerminatePL`, `LinkPI`/`LinkMK`, and `SetSystem`) are surfaced to the
//!    caller as a [`DriveOutcome::Play`] / [`DriveOutcome::Request`] — those
//!    start or alter actual A/V playback, which lives outside the navigation
//!    model.
//!
//! The shared register file (inside the runner) persists across title
//! transitions, exactly as a player's single global GPR/PSR bank does — a
//! `CallTitle` that sets a GPR is visible to the caller title after the
//! called title returns.
//!
//! ## Title-call stack
//!
//! `CallTitle` (like `CallObject`, one level up) saves a resume point and
//! switches title; a later title-context `Resume` returns to the calling
//! title at the command after the `CallTitle`. The driver keeps its own
//! **title** resume stack for this, separate from the runner's per-table
//! object resume stack.
//!
//! Clean-room source: `docs/container/bluray/hdmv-navigation-commands.md`
//! (Branch group JumpTitle / CallTitle / Resume + the "entered from
//! index.bdmv First-Playback / Top-Menu / Title table" placement note) plus
//! the PSR4 = Title assignment in the register model.

use super::index_bdmv::{IndexBdmv, TitleEntry};
use super::mobj_runner::{MobjRunner, RunOutcome};
use super::movie_object::MovieObjects;
use super::vm::{NavRequest, Registers};

/// PSR index of the Title register (per the register model: PSR4 = Title
/// number, `0xFFFF` = top menu).
const PSR_TITLE: u8 = 4;
/// Title value flagging the Top-Menu entry (`0xFFFF`).
const TITLE_VALUE_TOP_MENU: u32 = 0xFFFF;

/// One frame of the driver's *title* resume stack: the entry to return to
/// (so PSR4 re-seeds correctly) plus the runner state — the object and the
/// program counter to resume the calling title at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TitleFrame {
    entry: TitleEntry,
    object: usize,
    pc: usize,
}

/// Why a [`NavDriver::run`] / [`NavDriver::resume`] returned.
///
/// The driver resolves every transition the *title engine* owns
/// (`JumpTitle`/`CallTitle`/title `Resume`) internally; it only returns
/// when the script either finishes, asks to start/alter **playback**, or
/// hits a structural problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveOutcome {
    /// The navigation script finished: the entry title (and any titles on
    /// the title-call stack) ran to completion with nothing left to do and
    /// no playback was requested.
    Finished,
    /// The script requested A/V playback of a PlayList. The streaming layer
    /// opens the title's `.m2ts` and plays it; calling [`NavDriver::resume`]
    /// continues the script after playback (or a player-driven stop).
    Play(PlayRequest),
    /// The script requested a player/streaming action the navigation model
    /// does not own — `TerminatePL`, an in-PlayList `LinkPI`/`LinkMK`, or a
    /// `SetSystem` (stream select, button page, still, output mode, …). The
    /// caller services it and may [`NavDriver::resume`].
    Request(NavRequest),
    /// A `JumpTitle`/`CallTitle` named a title the index does not resolve to
    /// a runnable HDMV Movie Object (out of range, or a BD-J title). Carries
    /// the offending title number.
    BadTitle(u16),
    /// A branch named a Movie Object id outside the `MovieObject.bdmv` table.
    BadObject(u32),
    /// A transition budget (title transitions or per-table object
    /// transitions) was exhausted — a pathological navigation cycle. The run
    /// stopped to stay bounded.
    BudgetExhausted,
}

/// A resolved request to start PlayList playback, surfaced by
/// [`DriveOutcome::Play`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayRequest {
    /// The PlayList id to play (the `.mpls` stem).
    pub playlist: u32,
    /// An optional PlayItem to start at (`PlayPLatPlayItem`).
    pub play_item: Option<u32>,
    /// An optional PlayListMark to start at (`PlayPLatMark`).
    pub mark: Option<u32>,
}

/// Default budget on *title transitions* in one `run`/`resume`, bounding a
/// pathological JumpTitle/CallTitle cycle across the index.
pub const DEFAULT_TITLE_BUDGET: u64 = 10_000;

/// Drives the disc's HDMV title structure — `index.bdmv` plus
/// `MovieObject.bdmv` and a shared register file — resolving inter-title
/// navigation itself and surfacing only playback / player requests.
#[derive(Debug, Clone)]
pub struct NavDriver<'a> {
    index: &'a IndexBdmv,
    /// The per-table runner. Owns the shared register file
    /// ([`MobjRunner::vm`]) carried across title transitions.
    runner: MobjRunner<'a>,
    /// The title resume stack for `CallTitle` / title `Resume`.
    title_stack: Vec<TitleFrame>,
    /// The entry currently executing (for PSR4 re-seeding on resume).
    current_entry: TitleEntry,
    /// Title-transition budget for one drive.
    title_budget: u64,
    /// Pending resume point set when the drive yielded a Play / Request:
    /// `true` once a yield captured the runner mid-title so [`Self::resume`]
    /// continues it.
    pending: bool,
}

impl<'a> NavDriver<'a> {
    /// A driver over a disc's parsed `index.bdmv` and `MovieObject.bdmv`,
    /// with a cleared register file.
    pub fn new(index: &'a IndexBdmv, objects: &'a MovieObjects) -> Self {
        Self {
            index,
            runner: MobjRunner::new(objects),
            title_stack: Vec::new(),
            current_entry: TitleEntry::FirstPlayback,
            title_budget: DEFAULT_TITLE_BUDGET,
            pending: false,
        }
    }

    /// Replace the title-transition budget.
    pub fn set_title_budget(&mut self, budget: u64) {
        self.title_budget = budget;
    }

    /// Borrow the shared register file (e.g. to inspect results, or seed
    /// player settings such as PSR13 Parental / PSR19 Country before running
    /// via [`Registers::set_psr_player`]).
    pub fn registers(&self) -> &Registers {
        &self.runner.vm.registers
    }

    /// Mutable access to the shared register file.
    pub fn registers_mut(&mut self) -> &mut Registers {
        &mut self.runner.vm.registers
    }

    /// The title entry currently selected.
    pub fn current_entry(&self) -> TitleEntry {
        self.current_entry
    }

    /// Depth of the title-call (`CallTitle`) stack.
    pub fn title_call_depth(&self) -> usize {
        self.title_stack.len()
    }

    /// Enter `entry` (resolving it to its Movie Object via the index) and
    /// drive the navigation until the script finishes or asks for playback /
    /// a player action.
    pub fn run(&mut self, entry: TitleEntry) -> DriveOutcome {
        self.title_stack.clear();
        self.pending = false;
        let Some(object) = self.resolve(entry) else {
            return Self::bad_title(entry);
        };
        self.current_entry = entry;
        self.seed_psr_title(entry);
        match self.runner.run(object) {
            RunOutcome::BadObject(id) => DriveOutcome::BadObject(id),
            outcome => self.drive(outcome),
        }
    }

    /// Continue a drive that yielded a [`DriveOutcome::Play`] or
    /// [`DriveOutcome::Request`], after the caller serviced it. Resumes the
    /// current title's object table at the command after the branch. A
    /// no-op returning [`DriveOutcome::Finished`] if nothing is pending.
    pub fn resume(&mut self) -> DriveOutcome {
        if !std::mem::take(&mut self.pending) {
            return DriveOutcome::Finished;
        }
        let outcome = self.runner.resume();
        self.drive(outcome)
    }

    /// Resolve a title entry to its Movie Object id, or `None` if the index
    /// has no runnable HDMV object for it.
    fn resolve(&self, entry: TitleEntry) -> Option<usize> {
        self.index.resolve_movie_object(entry).map(|id| id as usize)
    }

    /// Map a failed title resolution to the right outcome variant.
    fn bad_title(entry: TitleEntry) -> DriveOutcome {
        match entry {
            TitleEntry::Title { number } => DriveOutcome::BadTitle(number),
            // First-Playback / Top-Menu with no HDMV object: report the
            // 0xFFFF top-menu sentinel so the caller can tell it apart from
            // a numbered title.
            _ => DriveOutcome::BadTitle(TITLE_VALUE_TOP_MENU as u16),
        }
    }

    /// Seed PSR4 (Title) with the number for the entry the driver just
    /// entered: the numbered title for a `Title`, the `0xFFFF` top-menu
    /// sentinel for the menu, and `0` for First-Playback (which precedes any
    /// title selection).
    fn seed_psr_title(&mut self, entry: TitleEntry) {
        let value = match entry {
            TitleEntry::Title { number } => number as u32,
            TitleEntry::TopMenu => TITLE_VALUE_TOP_MENU,
            TitleEntry::FirstPlayback => 0,
        };
        self.runner.vm.registers.set_psr_player(PSR_TITLE, value);
    }

    /// Service the runner's terminal [`RunOutcome`], following title-level
    /// transitions until the run finishes or yields a playback / player
    /// request the caller must handle.
    fn drive(&mut self, mut outcome: RunOutcome) -> DriveOutcome {
        let mut transitions = 0u64;
        loop {
            match outcome {
                RunOutcome::Finished => {
                    // The current title finished. Pop a calling title if any.
                    match self.title_stack.pop() {
                        Some(frame) => {
                            if !self.charge(&mut transitions) {
                                return DriveOutcome::BudgetExhausted;
                            }
                            self.current_entry = frame.entry;
                            self.seed_psr_title(frame.entry);
                            self.runner.set_object_pc(frame.object, frame.pc);
                            outcome = self.runner.resume();
                        }
                        None => return DriveOutcome::Finished,
                    }
                }
                RunOutcome::BadObject(id) => return DriveOutcome::BadObject(id),
                RunOutcome::BudgetExhausted => return DriveOutcome::BudgetExhausted,
                RunOutcome::Request(req) => match req {
                    NavRequest::JumpTitle { title } => {
                        if !self.charge(&mut transitions) {
                            return DriveOutcome::BudgetExhausted;
                        }
                        let entry = TitleEntry::Title {
                            number: title as u16,
                        };
                        let Some(next) = self.resolve(entry) else {
                            return DriveOutcome::BadTitle(title as u16);
                        };
                        self.current_entry = entry;
                        self.seed_psr_title(entry);
                        outcome = self.enter(next);
                    }
                    NavRequest::CallTitle { title } => {
                        if !self.charge(&mut transitions) {
                            return DriveOutcome::BudgetExhausted;
                        }
                        let entry = TitleEntry::Title {
                            number: title as u16,
                        };
                        let Some(next) = self.resolve(entry) else {
                            return DriveOutcome::BadTitle(title as u16);
                        };
                        // Save the return point: the calling title's entry,
                        // its object, and the PC after the CallTitle.
                        self.title_stack.push(TitleFrame {
                            entry: self.current_entry,
                            object: self.runner.current_object(),
                            pc: self.runner.vm.pc(),
                        });
                        self.current_entry = entry;
                        self.seed_psr_title(entry);
                        outcome = self.enter(next);
                    }
                    NavRequest::Resume => {
                        // Title-context Resume: pop the title-call stack.
                        match self.title_stack.pop() {
                            Some(frame) => {
                                if !self.charge(&mut transitions) {
                                    return DriveOutcome::BudgetExhausted;
                                }
                                self.current_entry = frame.entry;
                                self.seed_psr_title(frame.entry);
                                self.runner.set_object_pc(frame.object, frame.pc);
                                outcome = self.runner.resume();
                            }
                            None => return DriveOutcome::Finished,
                        }
                    }
                    NavRequest::PlayPlayList { playlist } => {
                        self.pending = true;
                        return DriveOutcome::Play(PlayRequest {
                            playlist,
                            play_item: None,
                            mark: None,
                        });
                    }
                    NavRequest::PlayPlayListAtPlayItem {
                        playlist,
                        play_item,
                    } => {
                        self.pending = true;
                        return DriveOutcome::Play(PlayRequest {
                            playlist,
                            play_item: Some(play_item),
                            mark: None,
                        });
                    }
                    NavRequest::PlayPlayListAtMark { playlist, mark } => {
                        self.pending = true;
                        return DriveOutcome::Play(PlayRequest {
                            playlist,
                            play_item: None,
                            mark: Some(mark),
                        });
                    }
                    // Everything else (TerminatePL, Link*, SetSystem) is a
                    // player/streaming request. JumpObject / CallObject never
                    // surface here — the runner resolves those itself.
                    other => {
                        self.pending = true;
                        return DriveOutcome::Request(other);
                    }
                },
            }
        }
    }

    /// Enter Movie Object `id` (PC 0) and run it, mapping a bad id.
    fn enter(&mut self, id: usize) -> RunOutcome {
        self.runner.run(id)
    }

    /// Charge one title transition against the budget; `false` if exhausted.
    fn charge(&self, n: &mut u64) -> bool {
        *n += 1;
        *n <= self.title_budget
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bdmv::index_bdmv::{AppInfoBdmv, IndexBdmv, IndexEntry, IndexObjectType};
    use crate::bdmv::movie_object::{MovieObject, NavCommand};

    // --- builders ---

    fn nc(word0: u32, word1: u32, word2: u32) -> NavCommand {
        let mut b = [0u8; 12];
        b[0..4].copy_from_slice(&word0.to_be_bytes());
        b[4..8].copy_from_slice(&word1.to_be_bytes());
        b[8..12].copy_from_slice(&word2.to_be_bytes());
        NavCommand { bytes: b }
    }

    /// `Move GPR g, imm v`.
    fn move_gpr_imm(g: u16, v: u32) -> NavCommand {
        nc(0x5040_0001, g as u32, v)
    }

    /// `JumpTitle imm t`.
    fn jump_title(t: u32) -> NavCommand {
        nc(0x2181_0000, t, 0)
    }

    /// `CallTitle imm t`.
    fn call_title(t: u32) -> NavCommand {
        nc(0x2183_0000, t, 0)
    }

    /// `Resume` (Branch sub_grp 1, branch_opt 4).
    fn resume_cmd() -> NavCommand {
        nc(0x0104_0000, 0, 0)
    }

    /// `PlayPL imm playlist` (Branch sub_grp 2, branch_opt 0, op_cnt 1, imm).
    fn play_pl(playlist: u32) -> NavCommand {
        let word0 = (1u32 << 29) | (2u32 << 24) | (1 << 23);
        nc(word0, playlist, 0)
    }

    fn obj(commands: Vec<NavCommand>) -> MovieObject {
        MovieObject {
            resume_intention_flag: 0,
            menu_call_mask: 0,
            title_search_mask: 0,
            commands,
        }
    }

    fn objects(list: Vec<MovieObject>) -> MovieObjects {
        MovieObjects {
            version: *b"0200",
            movie_objects: list,
        }
    }

    fn hdmv(id: u16) -> IndexEntry {
        IndexEntry {
            object: IndexObjectType::Hdmv {
                playback_type: 0,
                movie_object_id_ref: id,
            },
        }
    }

    fn bdj() -> IndexEntry {
        IndexEntry {
            object: IndexObjectType::BdJ {
                playback_type: 0,
                bdjo_file_name: "00000".into(),
            },
        }
    }

    fn index(first: IndexEntry, menu: IndexEntry, titles: Vec<IndexEntry>) -> IndexBdmv {
        IndexBdmv {
            version: *b"0200",
            app_info: AppInfoBdmv {
                initial_output_mode_preference: 0,
                content_exist_flag: 1,
                video_format: 6,
                frame_rate: 4,
            },
            first_playback_title: first,
            menu_title: menu,
            titles,
        }
    }

    // --- tests ---

    #[test]
    fn first_playback_runs_resolved_object() {
        // FirstPlayback → object 0 which sets GPR0 = 7.
        let idx = index(hdmv(0), hdmv(1), vec![hdmv(2)]);
        let objs = objects(vec![
            obj(vec![move_gpr_imm(0, 7)]),
            obj(vec![]),
            obj(vec![]),
        ]);
        let mut d = NavDriver::new(&idx, &objs);
        assert_eq!(d.run(TitleEntry::FirstPlayback), DriveOutcome::Finished);
        assert_eq!(d.registers().gpr(0), Some(7));
    }

    #[test]
    fn jump_title_is_resolved_through_index() {
        // FirstPlayback object jumps to Title 1; Title 1's object (id 2)
        // sets GPR1 = 9 then finishes. The driver resolves the title→object
        // mapping itself (Title 1 = titles[0] = object 2).
        let idx = index(hdmv(0), hdmv(9), vec![hdmv(2)]);
        let objs = objects(vec![
            obj(vec![jump_title(1)]),      // object 0 (FirstPlayback)
            obj(vec![]),                   // object 1
            obj(vec![move_gpr_imm(1, 9)]), // object 2 (Title 1)
        ]);
        let mut d = NavDriver::new(&idx, &objs);
        assert_eq!(d.run(TitleEntry::FirstPlayback), DriveOutcome::Finished);
        assert_eq!(d.registers().gpr(1), Some(9));
        // PSR4 (Title) was seeded to 1 when the driver entered Title 1.
        assert_eq!(d.registers().psr(PSR_TITLE), Some(1));
        assert_eq!(d.current_entry(), TitleEntry::Title { number: 1 });
    }

    #[test]
    fn call_title_then_resume_shares_registers() {
        // FirstPlayback (object 0): CallTitle 1, then set GPR0 = 100.
        // Title 1 (object 1): set GPR1 = 50, then Resume → returns to caller.
        let idx = index(hdmv(0), hdmv(9), vec![hdmv(1)]);
        let objs = objects(vec![
            obj(vec![call_title(1), move_gpr_imm(0, 100)]),
            obj(vec![move_gpr_imm(1, 50), resume_cmd()]),
        ]);
        let mut d = NavDriver::new(&idx, &objs);
        assert_eq!(d.run(TitleEntry::FirstPlayback), DriveOutcome::Finished);
        // Both registers set: callee's GPR1 visible to the caller; caller's
        // GPR0 set after the call returned.
        assert_eq!(d.registers().gpr(1), Some(50));
        assert_eq!(d.registers().gpr(0), Some(100));
        assert_eq!(d.title_call_depth(), 0);
        // Back in the FirstPlayback entry; PSR4 re-seeded to 0.
        assert_eq!(d.current_entry(), TitleEntry::FirstPlayback);
        assert_eq!(d.registers().psr(PSR_TITLE), Some(0));
    }

    #[test]
    fn call_title_implicit_return_on_end_of_list() {
        // Title 1's object runs off the end instead of an explicit Resume.
        let idx = index(hdmv(0), hdmv(9), vec![hdmv(1)]);
        let objs = objects(vec![
            obj(vec![call_title(1), move_gpr_imm(0, 7)]),
            obj(vec![move_gpr_imm(1, 8)]),
        ]);
        let mut d = NavDriver::new(&idx, &objs);
        assert_eq!(d.run(TitleEntry::FirstPlayback), DriveOutcome::Finished);
        assert_eq!(d.registers().gpr(1), Some(8));
        assert_eq!(d.registers().gpr(0), Some(7));
    }

    #[test]
    fn play_pl_yields_then_resumes() {
        // FirstPlayback: PlayPL 5, then set GPR0 = 1 after playback.
        let idx = index(hdmv(0), hdmv(9), vec![]);
        let objs = objects(vec![obj(vec![play_pl(5), move_gpr_imm(0, 1)])]);
        let mut d = NavDriver::new(&idx, &objs);
        assert_eq!(
            d.run(TitleEntry::FirstPlayback),
            DriveOutcome::Play(PlayRequest {
                playlist: 5,
                play_item: None,
                mark: None,
            })
        );
        // GPR0 not set yet — the script paused at the PlayPL.
        assert_eq!(d.registers().gpr(0), Some(0));
        // Player serviced playback; resume runs the tail.
        assert_eq!(d.resume(), DriveOutcome::Finished);
        assert_eq!(d.registers().gpr(0), Some(1));
    }

    #[test]
    fn top_menu_seeds_sentinel_title() {
        // TopMenu entry → object 1; PSR4 should be the 0xFFFF top-menu value.
        let idx = index(hdmv(0), hdmv(1), vec![]);
        let objs = objects(vec![obj(vec![]), obj(vec![move_gpr_imm(0, 1)])]);
        let mut d = NavDriver::new(&idx, &objs);
        assert_eq!(d.run(TitleEntry::TopMenu), DriveOutcome::Finished);
        assert_eq!(d.registers().psr(PSR_TITLE), Some(0xFFFF));
        assert_eq!(d.registers().gpr(0), Some(1));
    }

    #[test]
    fn jump_to_bdj_title_is_bad_title() {
        // Title 1 is a BD-J title; a JumpTitle 1 cannot be run by the HDMV VM.
        let idx = index(hdmv(0), hdmv(9), vec![bdj()]);
        let objs = objects(vec![obj(vec![jump_title(1)])]);
        let mut d = NavDriver::new(&idx, &objs);
        assert_eq!(d.run(TitleEntry::FirstPlayback), DriveOutcome::BadTitle(1));
    }

    #[test]
    fn jump_to_out_of_range_title_is_bad_title() {
        let idx = index(hdmv(0), hdmv(9), vec![hdmv(1)]);
        let objs = objects(vec![obj(vec![jump_title(5)]), obj(vec![])]);
        let mut d = NavDriver::new(&idx, &objs);
        assert_eq!(d.run(TitleEntry::FirstPlayback), DriveOutcome::BadTitle(5));
    }

    #[test]
    fn run_bad_entry_bdj_first_playback() {
        // FirstPlayback is itself a BD-J title → not runnable.
        let idx = index(bdj(), hdmv(0), vec![]);
        let objs = objects(vec![obj(vec![])]);
        let mut d = NavDriver::new(&idx, &objs);
        assert!(matches!(
            d.run(TitleEntry::FirstPlayback),
            DriveOutcome::BadTitle(_)
        ));
    }

    #[test]
    fn jump_to_movie_object_out_of_table_is_bad_object() {
        // Title resolves to object id 9 but the table only has one object.
        let idx = index(hdmv(9), hdmv(0), vec![]);
        let objs = objects(vec![obj(vec![])]);
        let mut d = NavDriver::new(&idx, &objs);
        assert_eq!(d.run(TitleEntry::FirstPlayback), DriveOutcome::BadObject(9));
    }

    #[test]
    fn inter_title_jump_cycle_is_bounded() {
        // Title 1 ↔ Title 2 JumpTitle each other forever.
        let idx = index(hdmv(0), hdmv(9), vec![hdmv(1), hdmv(2)]);
        let objs = objects(vec![
            obj(vec![jump_title(1)]),
            obj(vec![jump_title(2)]),
            obj(vec![jump_title(1)]),
        ]);
        let mut d = NavDriver::new(&idx, &objs);
        d.set_title_budget(50);
        assert_eq!(
            d.run(TitleEntry::FirstPlayback),
            DriveOutcome::BudgetExhausted
        );
    }

    #[test]
    fn script_compares_seeded_psr4_title() {
        // FirstPlayback jumps to Title 2; Title 2's object compares PSR4 == 2
        // (the seeded title number) and on true sets GPR0 = 1.
        // EQ PSR4, imm 2 : op1 register PSR4 (0x80000004), op2 immediate.
        let eq_psr4 = nc(0x4840_0200, 0x8000_0004, 0x0000_0002);
        let idx = index(hdmv(0), hdmv(9), vec![hdmv(3), hdmv(1)]);
        let objs = objects(vec![
            obj(vec![jump_title(2)]),               // object 0 FirstPlayback
            obj(vec![eq_psr4, move_gpr_imm(0, 1)]), // object 1 = Title 2
            obj(vec![]),
            obj(vec![]), // object 3 = Title 1
        ]);
        let mut d = NavDriver::new(&idx, &objs);
        assert_eq!(d.run(TitleEntry::FirstPlayback), DriveOutcome::Finished);
        assert_eq!(d.registers().gpr(0), Some(1));
    }
}
