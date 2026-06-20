//! HDMV virtual-machine register model — PSR / GPR naming + classes.
//!
//! The HDMV navigation VM (the bytecode in `MovieObject.bdmv`, decoded
//! by [`nav_command`](super::nav_command)) operates against two banks of
//! 32-bit unsigned registers:
//!
//! - **GPRs** — 4096 General Purpose Registers (`GPR0`…`GPR4095`),
//!   freely read/write by navigation commands.
//! - **PSRs** — 128 Player Status Registers (`PSR0`…`PSR127`), several
//!   reserved. Each PSR is one of two classes: **Playback Status**
//!   (tracks current navigation state; mutable by the player and some by
//!   `SetSystem` commands) or **Player Setting** (player capability / user
//!   preference; effectively read-only to ordinary navigation commands).
//!
//! This module surfaces the *register naming/semantics* layer on top of
//! the raw [`Operand::Register`](super::nav_command::Operand) bank+index
//! that [`DecodedCommand`](super::nav_command::DecodedCommand) produces,
//! so a caller can turn a register reference into a human-meaningful
//! name. It does **not** model register *values* or VM execution — see
//! the scope note on [`nav_command`](super::nav_command).
//!
//! Clean-room source: `docs/container/bluray/hdmv-navigation-commands.md`
//! ("Register model" section — PSR0–127 table and GPR conventions). All
//! numbers below are wire-format / public-format facts assembled there
//! from independent public sources (no decoder library source consulted).

/// Number of General Purpose Registers (`GPR0`…`GPR4095`).
pub const GPR_COUNT: u16 = 4096;

/// Number of Player Status Registers (`PSR0`…`PSR127`), reserved
/// entries included.
pub const PSR_COUNT: u8 = 128;

/// The access class of a Player Status Register.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PsrClass {
    /// Tracks current navigation/playback state. Mutable by the player,
    /// and (for some) by `SetSystem` navigation commands.
    PlaybackStatus {
        /// `true` if read-only even to the player's own playback engine
        /// (e.g. `PresentationTime`, `Timer`, the backup of either).
        read_only: bool,
    },
    /// Player capability / user preference. Reflects the player's
    /// configuration; effectively read-only to navigation commands.
    PlayerSetting,
    /// A reserved or system-use PSR index with no defined public name.
    Reserved,
}

impl PsrClass {
    /// `true` for any register navigation commands may *not* mutate
    /// (Player Setting, read-only Playback Status, or Reserved).
    pub fn is_read_only_to_nav(self) -> bool {
        match self {
            PsrClass::PlaybackStatus { read_only } => read_only,
            PsrClass::PlayerSetting => true,
            PsrClass::Reserved => true,
        }
    }
}

/// A named Player Status Register: its index, human-readable name, and
/// access class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PsrInfo {
    /// The PSR index (0..127).
    pub index: u8,
    /// The register's name (e.g. `"Angle"`, `"Title"`). `"reserved"`
    /// for reserved indices.
    pub name: &'static str,
    /// The access class.
    pub class: PsrClass,
}

const PLAYBACK: PsrClass = PsrClass::PlaybackStatus { read_only: false };
const PLAYBACK_RO: PsrClass = PsrClass::PlaybackStatus { read_only: true };
const SETTING: PsrClass = PsrClass::PlayerSetting;

/// Look up the [`PsrInfo`] for a PSR index.
///
/// Indices 0..127 always return an entry; reserved indices carry the
/// name `"reserved"` and class [`PsrClass::Reserved`]. Indices ≥ 128 are
/// outside the register file and return `None`.
pub fn psr_info(index: u8) -> Option<PsrInfo> {
    if index >= PSR_COUNT {
        return None;
    }
    let (name, class) = match index {
        0 => ("InteractiveGraphics", PLAYBACK),
        1 => ("PrimaryAudio", PLAYBACK),
        2 => ("PGPiPPGTextST", PLAYBACK),
        3 => ("Angle", PLAYBACK),
        4 => ("Title", PLAYBACK),
        5 => ("Chapter", PLAYBACK),
        6 => ("PlayList", PLAYBACK),
        7 => ("PlayItem", PLAYBACK),
        8 => ("PresentationTime", PLAYBACK_RO),
        9 => ("Timer", PLAYBACK_RO),
        10 => ("SelectedButton", PLAYBACK),
        11 => ("MenuPage", PLAYBACK),
        12 => ("SelectedStyle", PLAYBACK),
        13 => ("Parental", SETTING),
        14 => ("SecondaryAudioVideo", PLAYBACK),
        15 => ("AudioCapability", SETTING),
        16 => ("AudioLanguage", SETTING),
        17 => ("PGTSLanguage", SETTING),
        18 => ("MenuLanguage", SETTING),
        19 => ("Country", SETTING),
        20 => ("Region", SETTING),
        29 => ("VideoCapability", SETTING),
        30 => ("TextCapability", SETTING),
        31 => ("PlayerProfileVersion", SETTING),
        36 => ("BackupTitle", PLAYBACK),
        37 => ("BackupChapter", PLAYBACK),
        38 => ("BackupPlayList", PLAYBACK),
        39 => ("BackupPlayItem", PLAYBACK),
        40 => ("BackupPresentationTime", PLAYBACK_RO),
        42 => ("BackupSelectedButton", PLAYBACK),
        43 => ("BackupMenuPage", PLAYBACK),
        44 => ("BackupSelectedStyle", PLAYBACK),
        // PSR48..61: per-language characteristic-text-subtitle capability.
        48..=61 => ("CharacteristicTextCapability", SETTING),
        _ => ("reserved", PsrClass::Reserved),
    };
    Some(PsrInfo { index, name, class })
}

/// A conventional partition of the GPR address space (an *authoring*
/// convention — how tools/templates use the registers — not a
/// hardware-enforced semantic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GprConvention {
    /// Human-readable conventional use of this GPR.
    pub conventional_use: &'static str,
    /// `true` if this register is also reachable from BD-J code per the
    /// doc's interop note (GPR ranges 1000–4005).
    pub bd_j_reachable: bool,
}

/// Return the conventional use of a GPR index, or `None` if the index is
/// outside the 4096-register file.
///
/// The conventions come from the clean-room doc's GPR table. They are
/// *authoring* conventions only — navigation commands may freely
/// read/write any GPR regardless of this partition.
pub fn gpr_convention(index: u16) -> Option<GprConvention> {
    if index >= GPR_COUNT {
        return None;
    }
    let bd_j_reachable = (1000..=4005).contains(&index);
    let conventional_use = match index {
        0..=999 => "free for title programming",
        1000..=1999 => "per-playlist current audio / subtitle / chapter",
        2000..=3999 => "per-playlist resume play-time",
        4000 => "free",
        4001 => "sound-FX on/off flag",
        4002 => "free",
        4003 => "3D-mode flag",
        4004 => "free",
        4005 => "Top Menu pressed flag",
        4006..=4090 => "free for title programming",
        4091..=4095 => "reserved for BD-J interop",
        _ => "free",
    };
    Some(GprConvention {
        conventional_use,
        bd_j_reachable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_match_doc() {
        assert_eq!(GPR_COUNT, 4096);
        assert_eq!(PSR_COUNT, 128);
    }

    #[test]
    fn named_psrs() {
        assert_eq!(psr_info(3).unwrap().name, "Angle");
        assert_eq!(psr_info(4).unwrap().name, "Title");
        assert_eq!(psr_info(31).unwrap().name, "PlayerProfileVersion");
        assert_eq!(psr_info(0).unwrap().name, "InteractiveGraphics");
    }

    #[test]
    fn psr_classes() {
        // Title is mutable Playback Status.
        assert_eq!(psr_info(4).unwrap().class, PLAYBACK);
        assert!(!psr_info(4).unwrap().class.is_read_only_to_nav());
        // PresentationTime is read-only Playback Status.
        assert!(psr_info(8).unwrap().class.is_read_only_to_nav());
        // Parental is a Player Setting (read-only to nav).
        assert_eq!(psr_info(13).unwrap().class, PsrClass::PlayerSetting);
        assert!(psr_info(13).unwrap().class.is_read_only_to_nav());
    }

    #[test]
    fn backup_registers() {
        assert_eq!(psr_info(36).unwrap().name, "BackupTitle");
        assert_eq!(psr_info(40).unwrap().name, "BackupPresentationTime");
        assert!(psr_info(40).unwrap().class.is_read_only_to_nav());
    }

    #[test]
    fn reserved_psrs() {
        // 21-28 reserved.
        assert_eq!(psr_info(25).unwrap().name, "reserved");
        assert_eq!(psr_info(25).unwrap().class, PsrClass::Reserved);
        // 96-111 reserved for BD-system use.
        assert_eq!(psr_info(100).unwrap().name, "reserved");
    }

    #[test]
    fn characteristic_text_range() {
        for i in 48..=61 {
            assert_eq!(psr_info(i).unwrap().name, "CharacteristicTextCapability");
            assert_eq!(psr_info(i).unwrap().class, PsrClass::PlayerSetting);
        }
        assert_eq!(psr_info(47).unwrap().name, "reserved");
        assert_eq!(psr_info(62).unwrap().name, "reserved");
    }

    #[test]
    fn psr_out_of_range() {
        assert!(psr_info(128).is_none());
        assert!(psr_info(200).is_none());
        assert!(psr_info(127).is_some());
    }

    #[test]
    fn gpr_conventions() {
        assert_eq!(
            gpr_convention(0).unwrap().conventional_use,
            "free for title programming"
        );
        assert_eq!(
            gpr_convention(4001).unwrap().conventional_use,
            "sound-FX on/off flag"
        );
        assert_eq!(
            gpr_convention(4003).unwrap().conventional_use,
            "3D-mode flag"
        );
        assert_eq!(
            gpr_convention(4005).unwrap().conventional_use,
            "Top Menu pressed flag"
        );
        assert_eq!(
            gpr_convention(4093).unwrap().conventional_use,
            "reserved for BD-J interop"
        );
    }

    #[test]
    fn gpr_bd_j_reach() {
        assert!(!gpr_convention(500).unwrap().bd_j_reachable);
        assert!(gpr_convention(1500).unwrap().bd_j_reachable);
        assert!(gpr_convention(4005).unwrap().bd_j_reachable);
        assert!(!gpr_convention(4006).unwrap().bd_j_reachable);
    }

    #[test]
    fn gpr_out_of_range() {
        assert!(gpr_convention(4096).is_none());
        assert!(gpr_convention(4095).is_some());
    }
}
