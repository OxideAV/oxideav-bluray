//! Typed decode of the 64-bit `UO_mask_table` (User-Operation
//! prohibition table) carried by `AppInfoPlayList()` (BD-ROM Part 3
//! §5.4.3) and every `PlayItem()` (§5.4.4.1).
//!
//! The wire field is 64 bits, big-endian; **wire bit 0 is the
//! first-transmitted bit, i.e. the MSB of the big-endian word**. A bit
//! set to `1` PROHIBITS the corresponding remote-control user
//! operation; `0` permits it. Only wire bits 0–33 carry assignments in
//! current BD-ROM Part 3; bits 6, 9, 22, 32 and 34–63 are reserved and
//! normally zero.
//!
//! Bit → operation assignments are transcribed from the staged
//! clean-room table `docs/container/bluray/mpls-subpath-uo-mask.md`
//! ("UO_mask_table — 64-bit User Operation mask" section), which
//! reconstructs the member-gated §5.4.3 table from public format
//! documentation. The raw `u64` stays available on
//! [`AppInfoPlayList::uo_mask`](super::mpls::AppInfoPlayList) /
//! [`PlayItemFlags::uo_mask`](super::mpls::PlayItemFlags); this module
//! is the semantic view on top.

use std::fmt;

/// One user operation that a `UO_mask_table` can prohibit.
///
/// The discriminant of each variant **is its wire bit position**
/// (bit 0 = MSB of the big-endian 64-bit field). Reserved wire bits
/// (6, 9, 22, 32, 34–63) have no variant — see
/// [`UoMask::reserved_bits`] for detecting a mask that sets them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum UserOperation {
    /// Wire bit 0 — `menu_call` (jump to the Top Menu title).
    MenuCall = 0,
    /// Wire bit 1 — `title_search` (direct title-number selection).
    TitleSearch = 1,
    /// Wire bit 2 — `chapter_search` (direct chapter-number selection).
    ChapterSearch = 2,
    /// Wire bit 3 — `time_search` (jump to a wall-clock position).
    TimeSearch = 3,
    /// Wire bit 4 — `skip_to_next_point` (next chapter / mark).
    SkipToNextPoint = 4,
    /// Wire bit 5 — `skip_to_prev_point` (previous chapter / mark).
    SkipToPrevPoint = 5,
    /// Wire bit 7 — `stop`.
    Stop = 7,
    /// Wire bit 8 — `pause_on`.
    PauseOn = 8,
    /// Wire bit 10 — `still_off` (leave a still-picture dwell).
    StillOff = 10,
    /// Wire bit 11 — `forward_play` (fast-forward scan).
    ForwardPlay = 11,
    /// Wire bit 12 — `backward_play` (reverse scan).
    BackwardPlay = 12,
    /// Wire bit 13 — `resume` (return from a menu call).
    Resume = 13,
    /// Wire bit 14 — `move_up_selected_button` (IG menu navigation).
    MoveUpSelectedButton = 14,
    /// Wire bit 15 — `move_down_selected_button`.
    MoveDownSelectedButton = 15,
    /// Wire bit 16 — `move_left_selected_button`.
    MoveLeftSelectedButton = 16,
    /// Wire bit 17 — `move_right_selected_button`.
    MoveRightSelectedButton = 17,
    /// Wire bit 18 — `select_button`.
    SelectButton = 18,
    /// Wire bit 19 — `activate_button`.
    ActivateButton = 19,
    /// Wire bit 20 — `select_and_activate_button`.
    SelectAndActivateButton = 20,
    /// Wire bit 21 — `primary_audio_stream_number_change`.
    PrimaryAudioStreamNumberChange = 21,
    /// Wire bit 23 — `angle_number_change`.
    AngleNumberChange = 23,
    /// Wire bit 24 — `popup_on` (pop-up IG menu).
    PopupOn = 24,
    /// Wire bit 25 — `popup_off`.
    PopupOff = 25,
    /// Wire bit 26 — `primary_PG_enable_disable` (PG / TextST on-off).
    PrimaryPgEnableDisable = 26,
    /// Wire bit 27 — `primary_PG_stream_number_change`.
    PrimaryPgStreamNumberChange = 27,
    /// Wire bit 28 — `secondary_video_enable_disable` (PiP on-off).
    SecondaryVideoEnableDisable = 28,
    /// Wire bit 29 — `secondary_video_stream_number_change`.
    SecondaryVideoStreamNumberChange = 29,
    /// Wire bit 30 — `secondary_audio_enable_disable`.
    SecondaryAudioEnableDisable = 30,
    /// Wire bit 31 — `secondary_audio_stream_number_change`.
    SecondaryAudioStreamNumberChange = 31,
    /// Wire bit 33 — `secondary_PG_stream_number_change` (PiP PG /
    /// TextST).
    SecondaryPgStreamNumberChange = 33,
}

impl UserOperation {
    /// Every assigned user operation, in ascending wire-bit order.
    pub const ALL: [UserOperation; 30] = [
        Self::MenuCall,
        Self::TitleSearch,
        Self::ChapterSearch,
        Self::TimeSearch,
        Self::SkipToNextPoint,
        Self::SkipToPrevPoint,
        Self::Stop,
        Self::PauseOn,
        Self::StillOff,
        Self::ForwardPlay,
        Self::BackwardPlay,
        Self::Resume,
        Self::MoveUpSelectedButton,
        Self::MoveDownSelectedButton,
        Self::MoveLeftSelectedButton,
        Self::MoveRightSelectedButton,
        Self::SelectButton,
        Self::ActivateButton,
        Self::SelectAndActivateButton,
        Self::PrimaryAudioStreamNumberChange,
        Self::AngleNumberChange,
        Self::PopupOn,
        Self::PopupOff,
        Self::PrimaryPgEnableDisable,
        Self::PrimaryPgStreamNumberChange,
        Self::SecondaryVideoEnableDisable,
        Self::SecondaryVideoStreamNumberChange,
        Self::SecondaryAudioEnableDisable,
        Self::SecondaryAudioStreamNumberChange,
        Self::SecondaryPgStreamNumberChange,
    ];

    /// The operation's wire bit position (0 = MSB of the big-endian
    /// 64-bit field, i.e. the first-transmitted bit).
    pub fn wire_bit(self) -> u8 {
        self as u8
    }

    /// The single-bit mask this operation occupies inside the raw
    /// big-endian `u64` (`1 << (63 - wire_bit)`).
    pub fn raw_mask(self) -> u64 {
        1u64 << (63 - self.wire_bit())
    }

    /// Resolve a wire bit position back to its operation. Returns
    /// `None` for the reserved positions (6, 9, 22, 32, 34–63) and for
    /// any bit ≥ 64.
    pub fn from_wire_bit(bit: u8) -> Option<Self> {
        Self::ALL.iter().copied().find(|op| op.wire_bit() == bit)
    }

    /// The operation's field name as recorded in the staged
    /// `UO_mask_table` bit map (snake_case).
    pub fn name(self) -> &'static str {
        match self {
            Self::MenuCall => "menu_call",
            Self::TitleSearch => "title_search",
            Self::ChapterSearch => "chapter_search",
            Self::TimeSearch => "time_search",
            Self::SkipToNextPoint => "skip_to_next_point",
            Self::SkipToPrevPoint => "skip_to_prev_point",
            Self::Stop => "stop",
            Self::PauseOn => "pause_on",
            Self::StillOff => "still_off",
            Self::ForwardPlay => "forward_play",
            Self::BackwardPlay => "backward_play",
            Self::Resume => "resume",
            Self::MoveUpSelectedButton => "move_up_selected_button",
            Self::MoveDownSelectedButton => "move_down_selected_button",
            Self::MoveLeftSelectedButton => "move_left_selected_button",
            Self::MoveRightSelectedButton => "move_right_selected_button",
            Self::SelectButton => "select_button",
            Self::ActivateButton => "activate_button",
            Self::SelectAndActivateButton => "select_and_activate_button",
            Self::PrimaryAudioStreamNumberChange => "primary_audio_stream_number_change",
            Self::AngleNumberChange => "angle_number_change",
            Self::PopupOn => "popup_on",
            Self::PopupOff => "popup_off",
            Self::PrimaryPgEnableDisable => "primary_PG_enable_disable",
            Self::PrimaryPgStreamNumberChange => "primary_PG_stream_number_change",
            Self::SecondaryVideoEnableDisable => "secondary_video_enable_disable",
            Self::SecondaryVideoStreamNumberChange => "secondary_video_stream_number_change",
            Self::SecondaryAudioEnableDisable => "secondary_audio_enable_disable",
            Self::SecondaryAudioStreamNumberChange => "secondary_audio_stream_number_change",
            Self::SecondaryPgStreamNumberChange => "secondary_PG_stream_number_change",
        }
    }

    /// True for the seven IG-menu button-navigation operations
    /// (move / select / activate) plus the two pop-up toggles — the
    /// class a purely presentational player without an IG decoder can
    /// ignore wholesale.
    pub fn is_menu_navigation(self) -> bool {
        matches!(
            self,
            Self::MoveUpSelectedButton
                | Self::MoveDownSelectedButton
                | Self::MoveLeftSelectedButton
                | Self::MoveRightSelectedButton
                | Self::SelectButton
                | Self::ActivateButton
                | Self::SelectAndActivateButton
                | Self::PopupOn
                | Self::PopupOff
        )
    }

    /// True for the stream-selection operations (audio / PG / angle /
    /// secondary audio-video enable + number change) — the class that
    /// maps onto a player's track-selection UI.
    pub fn is_stream_selection(self) -> bool {
        matches!(
            self,
            Self::PrimaryAudioStreamNumberChange
                | Self::AngleNumberChange
                | Self::PrimaryPgEnableDisable
                | Self::PrimaryPgStreamNumberChange
                | Self::SecondaryVideoEnableDisable
                | Self::SecondaryVideoStreamNumberChange
                | Self::SecondaryAudioEnableDisable
                | Self::SecondaryAudioStreamNumberChange
                | Self::SecondaryPgStreamNumberChange
        )
    }
}

/// Union of every assigned operation's [`UserOperation::raw_mask`];
/// the complement is the reserved-bit set (wire bits 6, 9, 22, 32,
/// 34–63).
fn assigned_mask() -> u64 {
    UserOperation::ALL
        .iter()
        .fold(0u64, |acc, op| acc | op.raw_mask())
}

/// Typed view of one 64-bit `UO_mask_table` word.
///
/// Wraps the raw big-endian `u64` exactly as surfaced by
/// [`AppInfoPlayList::uo_mask`](super::mpls::AppInfoPlayList) /
/// [`PlayItemFlags::uo_mask`](super::mpls::PlayItemFlags): a set bit
/// **prohibits** its operation. `UoMask::default()` prohibits nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct UoMask(pub u64);

impl UoMask {
    /// A mask that permits every user operation (all bits clear).
    pub const PERMIT_ALL: UoMask = UoMask(0);

    /// Wrap a raw big-endian `UO_mask_table` word.
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw big-endian word (round-trips with [`Self::from_raw`]).
    pub fn as_raw(self) -> u64 {
        self.0
    }

    /// True when the disc prohibits `op` (its wire bit is `1`).
    pub fn is_prohibited(self, op: UserOperation) -> bool {
        self.0 & op.raw_mask() != 0
    }

    /// True when the disc permits `op` (its wire bit is `0`).
    pub fn permits(self, op: UserOperation) -> bool {
        !self.is_prohibited(op)
    }

    /// Every prohibited operation, in ascending wire-bit order.
    /// Reserved bits do not contribute (see [`Self::reserved_bits`]).
    pub fn prohibited_ops(self) -> Vec<UserOperation> {
        UserOperation::ALL
            .iter()
            .copied()
            .filter(|op| self.is_prohibited(*op))
            .collect()
    }

    /// True when no assigned operation is prohibited (reserved bits
    /// are ignored — a mask that only sets reserved bits still
    /// "permits all" in terms of defined operations).
    pub fn permits_all(self) -> bool {
        self.0 & assigned_mask() == 0
    }

    /// The reserved wire-bit positions (6, 9, 22, 32, 34–63) that are
    /// set in this mask, ascending. Non-empty output means the disc
    /// recorded a bit current BD-ROM Part 3 leaves unassigned —
    /// harmless to playback (there is no operation to prohibit) but a
    /// useful authoring-forensics signal.
    pub fn reserved_bits(self) -> Vec<u8> {
        let reserved = self.0 & !assigned_mask();
        (0u8..64)
            .filter(|bit| reserved & (1u64 << (63 - bit)) != 0)
            .collect()
    }

    /// Builder: return a copy with `op` prohibited.
    #[must_use]
    pub fn with_prohibited(self, op: UserOperation) -> Self {
        Self(self.0 | op.raw_mask())
    }

    /// Builder: return a copy with `op` permitted.
    #[must_use]
    pub fn with_permitted(self, op: UserOperation) -> Self {
        Self(self.0 & !op.raw_mask())
    }
}

impl fmt::Display for UoMask {
    /// `permit-all`, or a comma-separated list of the prohibited
    /// operation names (plus `+reserved` when unassigned bits are
    /// set): `prohibit: menu_call, angle_number_change`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ops = self.prohibited_ops();
        let has_reserved = !self.reserved_bits().is_empty();
        if ops.is_empty() && !has_reserved {
            return write!(f, "permit-all");
        }
        write!(f, "prohibit:")?;
        let mut first = true;
        for op in ops {
            if first {
                write!(f, " {}", op.name())?;
                first = false;
            } else {
                write!(f, ", {}", op.name())?;
            }
        }
        if has_reserved {
            write!(f, " +reserved")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_bit_anchors_match_staged_table() {
        // The staged table's two cross-check anchor points plus the
        // first-transmitted-bit convention: wire bit 0 = MSB.
        assert_eq!(UserOperation::MenuCall.raw_mask(), 0x8000_0000_0000_0000);
        // angle_number_change is wire bit 23 → 1 << (63 - 23).
        assert_eq!(UserOperation::AngleNumberChange.wire_bit(), 23);
        assert_eq!(UserOperation::AngleNumberChange.raw_mask(), 1u64 << 40);
        // secondary_PG_stream_number_change is wire bit 33.
        assert_eq!(UserOperation::SecondaryPgStreamNumberChange.wire_bit(), 33);
        assert_eq!(
            UserOperation::SecondaryPgStreamNumberChange.raw_mask(),
            1u64 << 30
        );
        // stop sits at wire bit 7 (after the reserved bit 6).
        assert_eq!(UserOperation::Stop.wire_bit(), 7);
    }

    #[test]
    fn from_wire_bit_round_trips_and_rejects_reserved() {
        for op in UserOperation::ALL {
            assert_eq!(UserOperation::from_wire_bit(op.wire_bit()), Some(op));
        }
        for reserved in [6u8, 9, 22, 32, 34, 40, 63, 64, 200] {
            assert_eq!(UserOperation::from_wire_bit(reserved), None, "{reserved}");
        }
    }

    #[test]
    fn all_is_ascending_and_covers_thirty_ops() {
        assert_eq!(UserOperation::ALL.len(), 30);
        for w in UserOperation::ALL.windows(2) {
            assert!(w[0].wire_bit() < w[1].wire_bit());
        }
        // Assigned bits = 0..=33 minus the four reserved holes.
        let assigned: Vec<u8> = UserOperation::ALL.iter().map(|o| o.wire_bit()).collect();
        let expect: Vec<u8> = (0u8..=33).filter(|b| ![6, 9, 22, 32].contains(b)).collect();
        assert_eq!(assigned, expect);
    }

    #[test]
    fn prohibit_permit_inverse() {
        let m = UoMask::PERMIT_ALL
            .with_prohibited(UserOperation::MenuCall)
            .with_prohibited(UserOperation::AngleNumberChange);
        assert!(m.is_prohibited(UserOperation::MenuCall));
        assert!(m.is_prohibited(UserOperation::AngleNumberChange));
        assert!(m.permits(UserOperation::Stop));
        assert!(!m.permits_all());
        assert_eq!(
            m.prohibited_ops(),
            vec![UserOperation::MenuCall, UserOperation::AngleNumberChange]
        );
        let m2 = m
            .with_permitted(UserOperation::MenuCall)
            .with_permitted(UserOperation::AngleNumberChange);
        assert_eq!(m2, UoMask::PERMIT_ALL);
        assert!(m2.permits_all());
    }

    #[test]
    fn reserved_bits_detected_and_ignored_by_ops() {
        // Set the four low reserved holes + one deep reserved bit.
        let raw = (1u64 << (63 - 6))
            | (1u64 << (63 - 9))
            | (1u64 << (63 - 22))
            | (1u64 << (63 - 32))
            | (1u64 << (63 - 50));
        let m = UoMask::from_raw(raw);
        assert_eq!(m.reserved_bits(), vec![6, 9, 22, 32, 50]);
        assert!(m.prohibited_ops().is_empty());
        assert!(m.permits_all(), "reserved-only mask defines no prohibition");
        assert_eq!(m.as_raw(), raw, "raw word survives the typed view");
    }

    #[test]
    fn classification_predicates() {
        assert!(UserOperation::SelectButton.is_menu_navigation());
        assert!(UserOperation::PopupOn.is_menu_navigation());
        assert!(!UserOperation::Stop.is_menu_navigation());
        assert!(UserOperation::AngleNumberChange.is_stream_selection());
        assert!(UserOperation::SecondaryPgStreamNumberChange.is_stream_selection());
        assert!(!UserOperation::MenuCall.is_stream_selection());
    }

    #[test]
    fn display_lists_prohibited_names() {
        assert_eq!(UoMask::PERMIT_ALL.to_string(), "permit-all");
        let m = UoMask::PERMIT_ALL
            .with_prohibited(UserOperation::MenuCall)
            .with_prohibited(UserOperation::TitleSearch);
        assert_eq!(m.to_string(), "prohibit: menu_call, title_search");
        let with_reserved = UoMask::from_raw(m.as_raw() | (1u64 << (63 - 50)));
        assert_eq!(
            with_reserved.to_string(),
            "prohibit: menu_call, title_search +reserved"
        );
        let reserved_only = UoMask::from_raw(1u64 << (63 - 50));
        assert_eq!(reserved_only.to_string(), "prohibit: +reserved");
    }

    #[test]
    fn every_named_op_round_trips_through_a_mask() {
        for op in UserOperation::ALL {
            let m = UoMask::PERMIT_ALL.with_prohibited(op);
            assert!(m.is_prohibited(op), "{}", op.name());
            assert_eq!(m.prohibited_ops(), vec![op]);
            assert_eq!(m.as_raw().count_ones(), 1);
            assert!(m.reserved_bits().is_empty());
        }
    }
}
