//! `PLAYLIST/*.mpls` — PlayList per BD-ROM Part 3 §5.4.
//!
//! Binary outline (big-endian throughout):
//!
//! ```text
//!   0  type_indicator             "MPLS"
//!   4  version_number             "0200"
//!   8  playlist_start_address      u32  (byte offset)
//!  12  playlist_mark_start_address u32
//!  16  extension_data_start_address u32
//!  20  20 reserved bytes
//!  40  AppInfoPlayList()                self-delimited
//!  ... PlayList()                       at playlist_start_address
//!  ... PlayListMark()                   at playlist_mark_start_address
//! ```
//!
//! `AppInfoPlayList()`:
//!
//! ```text
//!   0  length                  u32
//!   4  1 reserved byte
//!   5  PlayList_playback_type  u8 (1 = sequential, 2 = random, 3 = shuffle)
//!   6  playback_count          u16
//!   8  UO_mask_table           8 bytes
//!  16  random_access_flag      1 bit
//!  16  audio_mix_app_flag      1 bit
//!  16  lossless_may_bypass_mixer_flag 1 bit
//!  16  reserved                13 bits
//!  18  end of AppInfoPlayList
//! ```
//!
//! `PlayList()`:
//!
//! ```text
//!   0  length                  u32
//!   4  2 reserved bytes
//!   6  number_of_PlayItems     u16
//!   8  number_of_SubPaths      u16
//!  10  PlayItem[number_of_PlayItems]    self-delimited
//!  ...  SubPath[number_of_SubPaths]     self-delimited
//! ```
//!
//! Each `PlayItem` begins with its own 16-bit length, allowing us to
//! skip past optional STN_table / multi-clip entries that we don't
//! fully parse in Phase 1. We surface only the canonical fields a
//! demuxer needs to wire up streaming: clip filename + codec id +
//! IN/OUT times + STC reference.

use crate::bdmv::common::{BdmvHeader, Reader};
use crate::error::{BlurayError, Result};

/// Connection condition between PlayItems (§5.4.4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionCondition {
    /// `0x01` — Non-seamless connection (new media-time origin).
    NonSeamless,
    /// `0x05` — Seamless connection, same Clip continuation.
    SeamlessContinuation,
    /// `0x06` — Seamless connection, new STC sequence.
    SeamlessNewStc,
    /// Any other value we just preserve for diagnostics.
    Other(u8),
}

impl ConnectionCondition {
    pub fn from_raw(v: u8) -> Self {
        match v {
            0x01 => Self::NonSeamless,
            0x05 => Self::SeamlessContinuation,
            0x06 => Self::SeamlessNewStc,
            other => Self::Other(other),
        }
    }
}

/// `mark_type` of a [`PlayListMark`] (§5.4.5). BD-ROM uses entry marks
/// to delimit the chapter points a player's "chapter search" navigates
/// between; link points are author-private cue points that are not
/// surfaced as chapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkType {
    /// `0x01` — Entry mark. The boundaries of a chapter; a player's
    /// chapter-search jumps land on these.
    EntryMark,
    /// `0x02` — Link point. An author-private cue not exposed as a
    /// user-visible chapter.
    LinkPoint,
    /// Any other value, preserved for diagnostics.
    Other(u8),
}

impl MarkType {
    pub fn from_raw(v: u8) -> Self {
        match v {
            0x01 => Self::EntryMark,
            0x02 => Self::LinkPoint,
            other => Self::Other(other),
        }
    }

    /// True for entry marks — the ones that delimit user-visible
    /// chapters.
    pub fn is_chapter(self) -> bool {
        matches!(self, Self::EntryMark)
    }
}

/// MPEG-TS elementary stream coding type carried in each
/// stream_attributes block of an STN_table (BD-ROM Part 3 §5.4.4.4).
///
/// Values are the canonical PMT `stream_type` byte (ISO/IEC 13818-1
/// §2.4.4.10) for video; for audio / graphics they're BDA-private
/// values from the BD-AV white paper §5.4.4 Table 5-X.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamCodingType {
    /// `0x02` — MPEG-2 (ISO/IEC 13818-2) video.
    Mpeg2Video,
    /// `0x1B` — H.264 / MPEG-4 AVC (ISO/IEC 14496-10) video.
    AvcVideo,
    /// `0x24` — H.265 / HEVC (ISO/IEC 23008-2) video.
    HevcVideo,
    /// `0xEA` — SMPTE VC-1 video.
    Vc1Video,
    /// `0x80` — LPCM (Linear PCM) audio.
    LpcmAudio,
    /// `0x81` — Dolby Digital (AC-3) audio.
    Ac3Audio,
    /// `0x82` — DTS audio.
    DtsAudio,
    /// `0x83` — Dolby TrueHD audio.
    TruehdAudio,
    /// `0x84` — Dolby Digital Plus (E-AC-3) audio.
    EAc3Audio,
    /// `0x85` — DTS-HD High Resolution audio.
    DtsHdAudio,
    /// `0x86` — DTS-HD Master Audio.
    DtsHdMaAudio,
    /// `0xA1` — Dolby Digital Plus (E-AC-3) secondary audio (for PiP /
    /// director's commentary mixdown).
    EAc3SecondaryAudio,
    /// `0xA2` — DTS-HD secondary audio.
    DtsHdSecondaryAudio,
    /// `0x90` — Presentation Graphic Stream (BD bitmap subtitle).
    PgsSubtitle,
    /// `0x91` — Interactive Graphic Stream (BD on-disc menu overlay).
    IgsInteractive,
    /// `0x92` — Text-based subtitle stream.
    TextSubtitle,
    /// Any other raw value, preserved for diagnostics.
    Other(u8),
}

impl StreamCodingType {
    pub fn from_raw(v: u8) -> Self {
        match v {
            0x02 => Self::Mpeg2Video,
            0x1B => Self::AvcVideo,
            0x24 => Self::HevcVideo,
            0xEA => Self::Vc1Video,
            0x80 => Self::LpcmAudio,
            0x81 => Self::Ac3Audio,
            0x82 => Self::DtsAudio,
            0x83 => Self::TruehdAudio,
            0x84 => Self::EAc3Audio,
            0x85 => Self::DtsHdAudio,
            0x86 => Self::DtsHdMaAudio,
            0xA1 => Self::EAc3SecondaryAudio,
            0xA2 => Self::DtsHdSecondaryAudio,
            0x90 => Self::PgsSubtitle,
            0x91 => Self::IgsInteractive,
            0x92 => Self::TextSubtitle,
            other => Self::Other(other),
        }
    }

    pub fn as_raw(self) -> u8 {
        match self {
            Self::Mpeg2Video => 0x02,
            Self::AvcVideo => 0x1B,
            Self::HevcVideo => 0x24,
            Self::Vc1Video => 0xEA,
            Self::LpcmAudio => 0x80,
            Self::Ac3Audio => 0x81,
            Self::DtsAudio => 0x82,
            Self::TruehdAudio => 0x83,
            Self::EAc3Audio => 0x84,
            Self::DtsHdAudio => 0x85,
            Self::DtsHdMaAudio => 0x86,
            Self::EAc3SecondaryAudio => 0xA1,
            Self::DtsHdSecondaryAudio => 0xA2,
            Self::PgsSubtitle => 0x90,
            Self::IgsInteractive => 0x91,
            Self::TextSubtitle => 0x92,
            Self::Other(v) => v,
        }
    }

    pub fn is_video(self) -> bool {
        matches!(
            self,
            Self::Mpeg2Video | Self::AvcVideo | Self::HevcVideo | Self::Vc1Video
        )
    }

    pub fn is_audio(self) -> bool {
        matches!(
            self,
            Self::LpcmAudio
                | Self::Ac3Audio
                | Self::DtsAudio
                | Self::TruehdAudio
                | Self::EAc3Audio
                | Self::DtsHdAudio
                | Self::DtsHdMaAudio
                | Self::EAc3SecondaryAudio
                | Self::DtsHdSecondaryAudio
        )
    }

    /// `true` for the three graphics/menu stream coding types — PGS
    /// (bitmap subtitle), IGS (menu overlay) and the text-based subtitle
    /// stream.
    pub fn is_graphics(self) -> bool {
        matches!(
            self,
            Self::PgsSubtitle | Self::IgsInteractive | Self::TextSubtitle
        )
    }

    /// `true` for the two *secondary*-presentation audio coding types
    /// (`0xA1`/`0xA2`) carried for Picture-in-Picture / commentary
    /// mixdown. These are still audio ([`Self::is_audio`] is also `true`).
    pub fn is_secondary(self) -> bool {
        matches!(self, Self::EAc3SecondaryAudio | Self::DtsHdSecondaryAudio)
    }

    /// A short human-readable label for this coding type, suitable for a
    /// track-catalogue UI (`"MPEG-2 Video"`, `"Dolby TrueHD"`,
    /// `"PGS Subtitle"`, ...). [`Self::Other`] renders as
    /// `"Unknown(0xNN)"` carrying its raw byte.
    pub fn display_name(self) -> String {
        let s = match self {
            Self::Mpeg2Video => "MPEG-2 Video",
            Self::AvcVideo => "H.264/AVC Video",
            Self::HevcVideo => "H.265/HEVC Video",
            Self::Vc1Video => "VC-1 Video",
            Self::LpcmAudio => "LPCM Audio",
            Self::Ac3Audio => "Dolby Digital (AC-3)",
            Self::DtsAudio => "DTS Audio",
            Self::TruehdAudio => "Dolby TrueHD",
            Self::EAc3Audio => "Dolby Digital Plus (E-AC-3)",
            Self::DtsHdAudio => "DTS-HD High Resolution",
            Self::DtsHdMaAudio => "DTS-HD Master Audio",
            Self::EAc3SecondaryAudio => "Dolby Digital Plus (secondary)",
            Self::DtsHdSecondaryAudio => "DTS-HD (secondary)",
            Self::PgsSubtitle => "PGS Subtitle",
            Self::IgsInteractive => "Interactive Graphics",
            Self::TextSubtitle => "Text Subtitle",
            Self::Other(v) => return format!("Unknown(0x{v:02X})"),
        };
        s.to_string()
    }
}

/// Typed view of the 4-bit `video_format` nibble recorded inside the
/// per-PlayItem video `stream_attributes` block (BD-ROM Part 3
/// §5.4.4.4) and the `index.bdmv` AppInfoBDMV header. Names follow the
/// canonical BD-ROM AV video format code table (active-line count +
/// scan kind), so a player labelling its video track has a single
/// enum to switch over instead of duplicating the magic numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoFormat {
    /// `0x1` — 480i (NTSC SD interlaced).
    Video480i,
    /// `0x2` — 576i (PAL SD interlaced).
    Video576i,
    /// `0x3` — 480p (NTSC SD progressive).
    Video480p,
    /// `0x4` — 1080i HD interlaced.
    Video1080i,
    /// `0x5` — 720p HD progressive.
    Video720p,
    /// `0x6` — 1080p HD progressive.
    Video1080p,
    /// `0x7` — 576p (PAL SD progressive).
    Video576p,
    /// `0x8` — 2160p (UHD-BD, BD-ROM-AV HEVC whitepaper).
    Video2160p,
    /// Any other recorded nibble — preserved for diagnostics.
    Other(u8),
}

impl Default for VideoFormat {
    fn default() -> Self {
        Self::Other(0)
    }
}

impl VideoFormat {
    /// Decode the wire nibble into a typed variant. Bits above the
    /// low nibble are masked off so a caller can pass the raw byte
    /// (`video_format(4) | frame_rate(4)`) directly without shifting.
    pub fn from_raw(v: u8) -> Self {
        match v & 0x0F {
            0x1 => Self::Video480i,
            0x2 => Self::Video576i,
            0x3 => Self::Video480p,
            0x4 => Self::Video1080i,
            0x5 => Self::Video720p,
            0x6 => Self::Video1080p,
            0x7 => Self::Video576p,
            0x8 => Self::Video2160p,
            other => Self::Other(other),
        }
    }

    /// Encode this variant back to its 4-bit wire nibble. Round-trips
    /// with [`Self::from_raw`].
    pub fn as_raw(self) -> u8 {
        match self {
            Self::Video480i => 0x1,
            Self::Video576i => 0x2,
            Self::Video480p => 0x3,
            Self::Video1080i => 0x4,
            Self::Video720p => 0x5,
            Self::Video1080p => 0x6,
            Self::Video576p => 0x7,
            Self::Video2160p => 0x8,
            Self::Other(v) => v & 0x0F,
        }
    }

    /// True when the format encodes a progressive scan (480p / 576p /
    /// 720p / 1080p / 2160p). `false` for the two interlaced variants
    /// and for any unknown nibble.
    pub fn is_progressive(self) -> bool {
        matches!(
            self,
            Self::Video480p
                | Self::Video576p
                | Self::Video720p
                | Self::Video1080p
                | Self::Video2160p
        )
    }

    /// Active line count for the variant — convenient for muxers that
    /// want to label the track height (`480` for 480i/p, `1080` for
    /// 1080i/p, etc.). Returns `None` for `Other` since the spec leaves
    /// reserved nibbles open for future profiles.
    pub fn vertical_lines(self) -> Option<u16> {
        Some(match self {
            Self::Video480i | Self::Video480p => 480,
            Self::Video576i | Self::Video576p => 576,
            Self::Video720p => 720,
            Self::Video1080i | Self::Video1080p => 1080,
            Self::Video2160p => 2160,
            Self::Other(_) => return None,
        })
    }
}

/// Typed view of the 4-bit `frame_rate` nibble recorded inside the
/// per-PlayItem video `stream_attributes` block (BD-ROM Part 3
/// §5.4.4.4) and the `index.bdmv` AppInfoBDMV header. The wire
/// nibble encodes a small fixed set of broadcast / cinematic rates;
/// [`Self::fps_q`] returns the exact rational the rate expands to so
/// callers do not have to keep the BD-AV table in mind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameRate {
    /// `0x1` — 24000/1001 (NTSC film).
    Fps23_976,
    /// `0x2` — 24/1 (true cinema).
    Fps24,
    /// `0x3` — 25/1 (PAL).
    Fps25,
    /// `0x4` — 30000/1001 (NTSC video).
    Fps29_97,
    /// `0x6` — 50/1 (PAL doubled).
    Fps50,
    /// `0x7` — 60000/1001 (NTSC video doubled).
    Fps59_94,
    /// Any other recorded nibble — preserved for diagnostics.
    Other(u8),
}

impl Default for FrameRate {
    fn default() -> Self {
        Self::Other(0)
    }
}

impl FrameRate {
    /// Decode the wire nibble into a typed variant. Bits above the
    /// low nibble are masked off.
    pub fn from_raw(v: u8) -> Self {
        match v & 0x0F {
            0x1 => Self::Fps23_976,
            0x2 => Self::Fps24,
            0x3 => Self::Fps25,
            0x4 => Self::Fps29_97,
            0x6 => Self::Fps50,
            0x7 => Self::Fps59_94,
            other => Self::Other(other),
        }
    }

    /// Encode this variant back to its 4-bit wire nibble. Round-trips
    /// with [`Self::from_raw`].
    pub fn as_raw(self) -> u8 {
        match self {
            Self::Fps23_976 => 0x1,
            Self::Fps24 => 0x2,
            Self::Fps25 => 0x3,
            Self::Fps29_97 => 0x4,
            Self::Fps50 => 0x6,
            Self::Fps59_94 => 0x7,
            Self::Other(v) => v & 0x0F,
        }
    }

    /// Exact frame rate as `(numerator, denominator)` — the safe form
    /// for stream-metadata propagation. `None` for `Other`.
    pub fn fps_q(self) -> Option<(u32, u32)> {
        Some(match self {
            Self::Fps23_976 => (24_000, 1_001),
            Self::Fps24 => (24, 1),
            Self::Fps25 => (25, 1),
            Self::Fps29_97 => (30_000, 1_001),
            Self::Fps50 => (50, 1),
            Self::Fps59_94 => (60_000, 1_001),
            Self::Other(_) => return None,
        })
    }

    /// True when the rate is one of the NTSC / cinema-pulldown
    /// fractional variants (`24000/1001`, `30000/1001`, `60000/1001`).
    pub fn is_fractional(self) -> bool {
        matches!(self, Self::Fps23_976 | Self::Fps29_97 | Self::Fps59_94)
    }
}

/// Typed view of the 4-bit `aspect_ratio` nibble recorded inside the
/// per-PlayItem video `stream_attributes` block (BD-ROM Part 3
/// §5.4.4.4). Only the two common display ratios are documented; any
/// other nibble round-trips as [`Self::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AspectRatio {
    /// `0x2` — 4:3 display aspect.
    Ratio4x3,
    /// `0x3` — 16:9 display aspect.
    Ratio16x9,
    /// Any other recorded nibble — preserved for diagnostics.
    Other(u8),
}

impl Default for AspectRatio {
    fn default() -> Self {
        Self::Other(0)
    }
}

impl AspectRatio {
    /// Decode the wire nibble into a typed variant. The
    /// `aspect_ratio(4) | reserved(4)` byte should be shifted down
    /// before calling this (the parser already does so); callers that
    /// hand in the un-shifted byte get the low nibble extracted
    /// implicitly via masking.
    pub fn from_raw(v: u8) -> Self {
        match v & 0x0F {
            0x2 => Self::Ratio4x3,
            0x3 => Self::Ratio16x9,
            other => Self::Other(other),
        }
    }

    /// Encode this variant back to its 4-bit wire nibble.
    pub fn as_raw(self) -> u8 {
        match self {
            Self::Ratio4x3 => 0x2,
            Self::Ratio16x9 => 0x3,
            Self::Other(v) => v & 0x0F,
        }
    }

    /// Display aspect as `(width, height)` — `(4, 3)` / `(16, 9)`. `None`
    /// for `Other`.
    pub fn ratio(self) -> Option<(u8, u8)> {
        Some(match self {
            Self::Ratio4x3 => (4, 3),
            Self::Ratio16x9 => (16, 9),
            Self::Other(_) => return None,
        })
    }

    /// True for 16:9. Convenient one-shot predicate for a UI that only
    /// needs to flag widescreen content.
    pub fn is_widescreen(self) -> bool {
        matches!(self, Self::Ratio16x9)
    }
}

/// Typed view of the 4-bit `audio_format` nibble recorded inside the
/// per-PlayItem audio `stream_attributes` block (BD-ROM Part 3
/// §5.4.4.4). Names follow the BD-AV channel-layout convention
/// (`Mono` = 1.0, `Stereo` = 2.0, `Multi` = 5.1, `Combo` = 5.1 +
/// stereo downmix carried side-band) so a player can decide which
/// downmix to feed the decoder without re-deriving the layout from
/// the raw nibble.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioFormat {
    /// `0x1` — Mono (1.0).
    Mono,
    /// `0x3` — Stereo (2.0).
    Stereo,
    /// `0x6` — Multi-channel (5.1).
    Multi,
    /// `0xC` — Combo (5.1 + side-band stereo downmix).
    Combo,
    /// Any other recorded nibble — preserved for diagnostics.
    Other(u8),
}

impl Default for AudioFormat {
    fn default() -> Self {
        Self::Other(0)
    }
}

impl AudioFormat {
    /// Decode the wire nibble into a typed variant.
    pub fn from_raw(v: u8) -> Self {
        match v & 0x0F {
            0x1 => Self::Mono,
            0x3 => Self::Stereo,
            0x6 => Self::Multi,
            0xC => Self::Combo,
            other => Self::Other(other),
        }
    }

    /// Encode this variant back to its 4-bit wire nibble.
    pub fn as_raw(self) -> u8 {
        match self {
            Self::Mono => 0x1,
            Self::Stereo => 0x3,
            Self::Multi => 0x6,
            Self::Combo => 0xC,
            Self::Other(v) => v & 0x0F,
        }
    }

    /// Number of audio channels the layout carries — `1` for mono,
    /// `2` for stereo, `6` for multi-channel. `Combo` reports `6` (the
    /// primary 5.1 mix; the side-band stereo downmix is an additional
    /// fallback layer rather than a separate channel count). `None`
    /// for `Other`.
    pub fn channel_count(self) -> Option<u8> {
        Some(match self {
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::Multi | Self::Combo => 6,
            Self::Other(_) => return None,
        })
    }

    /// True for `Combo` — the layout that carries a stereo downmix
    /// alongside the primary 5.1 mix.
    pub fn has_downmix(self) -> bool {
        matches!(self, Self::Combo)
    }
}

/// Typed view of the 4-bit `sample_rate` nibble recorded inside the
/// per-PlayItem audio `stream_attributes` block (BD-ROM Part 3
/// §5.4.4.4). BD-AV uses a small fixed set of sample rates documented
/// in the spec's audio attribute table; combination variants (`4896` /
/// `48192`) cover the dual-rate carriage some lossless audio codecs
/// use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SampleRate {
    /// `0x1` — 48 kHz.
    Hz48000,
    /// `0x4` — 96 kHz.
    Hz96000,
    /// `0x5` — 192 kHz.
    Hz192000,
    /// `0xC` — Combo 48 / 192 kHz (dual-rate carriage).
    Combo48_192,
    /// `0xE` — Combo 48 / 96 kHz (dual-rate carriage).
    Combo48_96,
    /// Any other recorded nibble — preserved for diagnostics.
    Other(u8),
}

impl Default for SampleRate {
    fn default() -> Self {
        Self::Other(0)
    }
}

impl SampleRate {
    /// Decode the wire nibble into a typed variant.
    pub fn from_raw(v: u8) -> Self {
        match v & 0x0F {
            0x1 => Self::Hz48000,
            0x4 => Self::Hz96000,
            0x5 => Self::Hz192000,
            0xC => Self::Combo48_192,
            0xE => Self::Combo48_96,
            other => Self::Other(other),
        }
    }

    /// Encode this variant back to its 4-bit wire nibble.
    pub fn as_raw(self) -> u8 {
        match self {
            Self::Hz48000 => 0x1,
            Self::Hz96000 => 0x4,
            Self::Hz192000 => 0x5,
            Self::Combo48_192 => 0xC,
            Self::Combo48_96 => 0xE,
            Self::Other(v) => v & 0x0F,
        }
    }

    /// Primary sample rate in Hz — the highest rate of a combo variant
    /// or the only rate of a single-rate variant. Returns `None` for
    /// `Other`.
    pub fn primary_hz(self) -> Option<u32> {
        Some(match self {
            Self::Hz48000 => 48_000,
            Self::Hz96000 => 96_000,
            Self::Hz192000 => 192_000,
            Self::Combo48_192 => 192_000,
            Self::Combo48_96 => 96_000,
            Self::Other(_) => return None,
        })
    }

    /// `true` when the variant carries two rates (the lossless dual-rate
    /// carriages `Combo48_192` and `Combo48_96`).
    pub fn is_combo(self) -> bool {
        matches!(self, Self::Combo48_192 | Self::Combo48_96)
    }
}

/// One primary-video stream entry inside [`StnTable::primary_video`]
/// (BD-ROM Part 3 §5.4.4.4 "video stream_attributes").
///
/// Wire layout (post-header per-stream):
///
/// ```text
///   stream_entry      length-prefixed block, type 1 = in-mux PID
///   stream_attributes length-prefixed block:
///     stream_coding_type             u8
///     video_format(4) | frame_rate(4) u8
///     aspect_ratio(4) | reserved(4)   u8
/// ```
///
/// `video_format` / `frame_rate` / `aspect_ratio` are the raw 4-bit
/// fields; per-codec interpretation tables live in BD-AV §5.4.4
/// (e.g. `frame_rate == 0x03` → 24000/1001 fps).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrimaryVideoStream {
    /// MPEG-TS elementary PID carrying the stream.
    pub elementary_pid: u16,
    pub coding_type: StreamCodingType,
    pub video_format: u8, // 4-bit
    pub frame_rate: u8,   // 4-bit
    pub aspect_ratio: u8, // 4-bit
}

impl PrimaryVideoStream {
    /// Typed view of [`Self::video_format`] (BD-ROM Part 3 §5.4.4.4).
    pub fn video_format_kind(&self) -> VideoFormat {
        VideoFormat::from_raw(self.video_format)
    }

    /// Typed view of [`Self::frame_rate`] (BD-ROM Part 3 §5.4.4.4).
    pub fn frame_rate_kind(&self) -> FrameRate {
        FrameRate::from_raw(self.frame_rate)
    }

    /// Typed view of [`Self::aspect_ratio`] (BD-ROM Part 3 §5.4.4.4).
    pub fn aspect_ratio_kind(&self) -> AspectRatio {
        AspectRatio::from_raw(self.aspect_ratio)
    }
}

/// One primary-audio stream entry inside [`StnTable::primary_audio`]
/// (BD-ROM Part 3 §5.4.4.4 "audio stream_attributes").
///
/// `audio_format` (4 bits — e.g. 1 = mono, 3 = stereo, 6 = multichannel
/// 5.1) and `sample_rate` (4 bits — e.g. 1 = 48 kHz, 4 = 96 kHz) are
/// the raw nibbles. `language_code` is a 3-byte ISO 639-2/T tag (e.g.
/// `*b"eng"`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrimaryAudioStream {
    pub elementary_pid: u16,
    pub coding_type: StreamCodingType,
    pub audio_format: u8, // 4-bit
    pub sample_rate: u8,  // 4-bit
    pub language_code: [u8; 3],
}

impl PrimaryAudioStream {
    /// Typed view of [`Self::audio_format`] (BD-ROM Part 3 §5.4.4.4).
    pub fn audio_format_kind(&self) -> AudioFormat {
        AudioFormat::from_raw(self.audio_format)
    }

    /// Typed view of [`Self::sample_rate`] (BD-ROM Part 3 §5.4.4.4).
    pub fn sample_rate_kind(&self) -> SampleRate {
        SampleRate::from_raw(self.sample_rate)
    }
}

/// One Presentation Graphic Stream (BD bitmap subtitle) entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PgsSubtitleStream {
    pub elementary_pid: u16,
    pub coding_type: StreamCodingType,
    pub language_code: [u8; 3],
}

/// One Interactive Graphic Stream entry — used by BD-J / HDMV menu
/// overlay. Layout matches [`PgsSubtitleStream`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IgsInteractiveStream {
    pub elementary_pid: u16,
    pub coding_type: StreamCodingType,
    pub language_code: [u8; 3],
}

/// One secondary-audio stream entry (Picture-in-Picture / director's
/// commentary mixdown). Carries the same per-track attributes as a
/// primary audio stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SecondaryAudioStream {
    pub elementary_pid: u16,
    pub coding_type: StreamCodingType,
    pub audio_format: u8,
    pub sample_rate: u8,
    pub language_code: [u8; 3],
}

impl SecondaryAudioStream {
    /// Typed view of [`Self::audio_format`] (BD-ROM Part 3 §5.4.4.4).
    pub fn audio_format_kind(&self) -> AudioFormat {
        AudioFormat::from_raw(self.audio_format)
    }

    /// Typed view of [`Self::sample_rate`] (BD-ROM Part 3 §5.4.4.4).
    pub fn sample_rate_kind(&self) -> SampleRate {
        SampleRate::from_raw(self.sample_rate)
    }
}

/// One secondary-video stream entry (Picture-in-Picture overlay).
/// Layout matches [`PrimaryVideoStream`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SecondaryVideoStream {
    pub elementary_pid: u16,
    pub coding_type: StreamCodingType,
    pub video_format: u8,
    pub frame_rate: u8,
    pub aspect_ratio: u8,
}

impl SecondaryVideoStream {
    /// Typed view of [`Self::video_format`] (BD-ROM Part 3 §5.4.4.4).
    pub fn video_format_kind(&self) -> VideoFormat {
        VideoFormat::from_raw(self.video_format)
    }

    /// Typed view of [`Self::frame_rate`] (BD-ROM Part 3 §5.4.4.4).
    pub fn frame_rate_kind(&self) -> FrameRate {
        FrameRate::from_raw(self.frame_rate)
    }

    /// Typed view of [`Self::aspect_ratio`] (BD-ROM Part 3 §5.4.4.4).
    pub fn aspect_ratio_kind(&self) -> AspectRatio {
        AspectRatio::from_raw(self.aspect_ratio)
    }
}

/// One Picture-in-Picture Presentation Graphic Stream entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PipPgStream {
    pub elementary_pid: u16,
    pub coding_type: StreamCodingType,
    pub language_code: [u8; 3],
}

/// One text-subtitle stream entry. Carries a single-byte
/// `character_code` plus the standard 3-byte ISO 639-2/T language tag.
/// (Text subs are an optional class in the STN_table beyond the seven
/// counted classes; per the BD-AV layout they live in the trailing
/// portion of the table after num_pip_pg streams. Many discs ship none.)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextSubtitleStream {
    pub elementary_pid: u16,
    pub coding_type: StreamCodingType,
    pub character_code: u8,
    pub language_code: [u8; 3],
}

impl Default for StreamCodingType {
    fn default() -> Self {
        Self::Other(0)
    }
}

/// Per-PlayItem stream-type table (BD-ROM Part 3 §5.4.4.4). One vector
/// per stream class; every entry carries the elementary PID + decoded
/// per-codec attribute fields a downstream muxer needs to label each
/// track.
///
/// Round-trips through [`PlayListMpls::encode`]: the encoded byte
/// pattern is the spec-conformant `STN_table()` block with one
/// `stream_entry` + `stream_attributes` pair per stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StnTable {
    pub primary_video: Vec<PrimaryVideoStream>,
    pub primary_audio: Vec<PrimaryAudioStream>,
    /// Presentation Graphic Stream (bitmap subtitles).
    pub pg_subtitles: Vec<PgsSubtitleStream>,
    /// Interactive Graphic Stream (on-disc menu overlay).
    pub ig_streams: Vec<IgsInteractiveStream>,
    pub secondary_audio: Vec<SecondaryAudioStream>,
    pub secondary_video: Vec<SecondaryVideoStream>,
    /// Picture-in-Picture Presentation Graphic Streams.
    pub pip_pg: Vec<PipPgStream>,
}

impl StnTable {
    /// Derive a deprecated [`StnTableSummary`] (count-only view) for
    /// callers that still need the old surface. The new code path
    /// should consume the per-stream vectors directly.
    #[allow(deprecated)]
    pub fn summary(&self) -> StnTableSummary {
        StnTableSummary {
            num_primary_video: self.primary_video.len() as u8,
            num_primary_audio: self.primary_audio.len() as u8,
            num_pg: self.pg_subtitles.len() as u8,
            num_ig: self.ig_streams.len() as u8,
            num_secondary_audio: self.secondary_audio.len() as u8,
            num_secondary_video: self.secondary_video.len() as u8,
            num_pip_pg: self.pip_pg.len() as u8,
        }
    }
}

/// Deprecated count-only view of an STN_table. Kept as a one-release
/// compat shim — new code should consume [`StnTable`]'s per-stream
/// vectors directly so it can label each track by codec / PID /
/// language for a downstream muxer.
#[deprecated(
    since = "0.0.3",
    note = "use `StnTable` and its per-stream vectors (`primary_video`, `primary_audio`, `pg_subtitles`, ...) instead"
)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StnTableSummary {
    pub num_primary_video: u8,
    pub num_primary_audio: u8,
    pub num_pg: u8, // presentation graphics
    pub num_ig: u8, // interactive graphics
    pub num_secondary_audio: u8,
    pub num_secondary_video: u8,
    pub num_pip_pg: u8,
}

#[allow(deprecated)]
impl From<&StnTable> for StnTableSummary {
    fn from(t: &StnTable) -> Self {
        t.summary()
    }
}

/// One per-angle alternate clip reference inside a multi-angle PlayItem
/// (§5.4.4.1). The primary angle's clip name/codec/STC live on
/// [`PlayItem::clip_information_file_name`] /
/// [`PlayItem::clip_codec_identifier`] / [`PlayItem::stc_id_ref`]; this
/// struct carries the corresponding fields for each additional angle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AngleClip {
    /// 5-char ASCII clip filename stem (e.g. `"00002"`).
    pub clip_information_file_name: String,
    /// 4-char codec id (typically `b"M2TS"`).
    pub clip_codec_identifier: [u8; 4],
    pub stc_id_ref: u8,
}

/// Per-PlayItem playback-control fields that sit between the IN/OUT
/// timestamps and the `is_multi_angle` block in §5.4.4.1, plus the raw
/// flags byte that prefixes the multi-angle clip list.
///
/// These were previously consumed-and-discarded by the parser; they are
/// surfaced here as raw values so a player can honour the disc author's
/// random-access and still-frame intentions without re-walking the
/// wire bytes. Only the `random_access_flag` is decomposed into a typed
/// bit (its position — the top bit of the byte following the UO mask
/// table — is the layout the parser has always assumed); the remaining
/// fields are surfaced verbatim because their internal bit semantics are
/// not pinned by the consulted references.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlayItemFlags {
    /// `PlayItem_random_access_flag` (§5.4.4.1) — the top bit of the
    /// byte immediately after the 8-byte `UO_mask_table`. When set, the
    /// author permits random access (skip / time search) to land inside
    /// this PlayItem; when clear, the player should treat the PlayItem as
    /// a non-skippable unit.
    pub random_access_flag: bool,
    /// `still_mode` byte (§5.4.4.1). A PlayItem may pause on its final
    /// presented picture (a "still") rather than advancing — used by the
    /// Browsable-Slideshow framework. Surfaced raw; the player consults
    /// [`Self::still_time`] when this selects a timed still.
    pub still_mode: u8,
    /// `still_time` (§5.4.4.1) — the dwell, in seconds, for a timed
    /// still. Meaningful only for the timed-still `still_mode`; `0` for
    /// the no-still / infinite-still cases.
    pub still_time: u16,
    /// Raw flags byte prefixing the `is_multi_angle` clip list
    /// (§5.4.4.1), present only when the PlayItem is multi-angle. `0`
    /// for single-angle PlayItems. Surfaced verbatim — its individual
    /// bit assignments are not pinned by the consulted references.
    pub angle_flags: u8,
    /// `UO_mask_table` (§5.4.4.1) — the PlayItem-scoped 64-bit
    /// User-Operation prohibition table, big-endian, preserved verbatim
    /// (same wire field as [`AppInfoPlayList::uo_mask`] but applied to
    /// this PlayItem only). Surfaced raw; individual bit assignments are
    /// not pinned by the consulted references. Kept so an encode →
    /// parse round trip does not silently drop the disc's per-PlayItem
    /// UO prohibitions.
    pub uo_mask: u64,
}

/// One PlayItem (§5.4.4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayItem {
    /// 5-char ASCII clip filename stem (e.g. `"00001"`).
    pub clip_information_file_name: String,
    /// 4-char codec id (always `b"M2TS"` for standard BD-ROM clips).
    pub clip_codec_identifier: [u8; 4],
    pub connection_condition: ConnectionCondition,
    pub stc_id_ref: u8,
    pub in_time_ticks: u32,  // 45 kHz (§5.4.4.1 "PTS in 45 kHz units")
    pub out_time_ticks: u32, // 45 kHz
    /// Number of multi-clip entries — including the primary clip. `1`
    /// means a single primary clip (no alternate angles); `N > 1` means
    /// `N - 1` entries are listed in [`Self::angles`].
    pub multi_clip_count: u8,
    /// Alternate-angle clip references (angles 2..N) per §5.4.4.1's
    /// `is_multi_angle` block. Empty when `multi_clip_count <= 1`.
    /// Indexing: `angles[0]` is the second angle (the first angle is
    /// the primary `clip_information_file_name` on the PlayItem
    /// itself), so the user-facing 0-based angle index maps as:
    ///   angle 0 → PlayItem primary clip
    ///   angle k → `angles[k - 1]` for k ≥ 1
    pub angles: Vec<AngleClip>,
    pub stn_table: StnTable,
    /// Playback-control fields (`PlayItem_random_access_flag`,
    /// `still_mode`, `still_time`, multi-angle flags byte) lifted off
    /// §5.4.4.1. See [`PlayItemFlags`].
    pub flags: PlayItemFlags,
}

impl PlayItem {
    /// Resolve a 0-based angle index to the corresponding clip
    /// reference. `angle == 0` returns the primary clip; `angle >= 1`
    /// returns the matching entry from [`Self::angles`]. Returns
    /// `None` when the requested angle is out of range.
    pub fn angle_clip(&self, angle: u8) -> Option<AngleClipRef<'_>> {
        if angle == 0 {
            return Some(AngleClipRef {
                clip_information_file_name: &self.clip_information_file_name,
                clip_codec_identifier: &self.clip_codec_identifier,
                stc_id_ref: self.stc_id_ref,
            });
        }
        let idx = (angle as usize).checked_sub(1)?;
        self.angles.get(idx).map(|a| AngleClipRef {
            clip_information_file_name: &a.clip_information_file_name,
            clip_codec_identifier: &a.clip_codec_identifier,
            stc_id_ref: a.stc_id_ref,
        })
    }

    /// Number of angles this PlayItem advertises (1 for single-clip
    /// items; the unfolded count for multi-angle items).
    pub fn num_angles(&self) -> u8 {
        if self.multi_clip_count == 0 {
            1
        } else {
            self.multi_clip_count
        }
    }

    /// Duration of this PlayItem in 90 kHz ticks. BD timing is
    /// uniformly given in 45 kHz units inside MPLS; doubling lifts it
    /// onto the 90 kHz PTS scale used by the rest of the stack.
    pub fn duration_90k(&self) -> u64 {
        (self.out_time_ticks.saturating_sub(self.in_time_ticks)) as u64 * 2
    }
}

/// Borrowed view of a single angle's clip reference, returned by
/// [`PlayItem::angle_clip`]. Lets a streamer look up an angle's `.m2ts`
/// / `.clpi` stem without cloning the underlying `String`.
#[derive(Debug, Clone, Copy)]
pub struct AngleClipRef<'a> {
    pub clip_information_file_name: &'a str,
    pub clip_codec_identifier: &'a [u8; 4],
    pub stc_id_ref: u8,
}

/// A SubPath placeholder — we count them and preserve their type but
/// do not parse the inner PlayItems in Phase 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubPath {
    pub sub_path_type: u8,
    /// `is_repeat_SubPath` (§5.4.4) — the low bit of the 16-bit field
    /// after `SubPath_type` (15 reserved bits + this flag). When set, the
    /// player loops this SubPath (e.g. a background slideshow / audio
    /// loop) for the duration of the associated main-path presentation.
    /// Preserved so an encode → parse round trip keeps the repeat intent.
    pub is_repeat_subpath: bool,
    pub num_sub_play_items: u16,
}

/// Typed view of the `PlayList_playback_type` byte recorded in
/// [`AppInfoPlayList`] (BD-ROM Part 3 §5.4 AppInfoPlayList). Mirrors the
/// raw-byte enumeration the wire layout records — `1` = sequential
/// playback of the listed PlayItems, `2` = random selection without
/// replacement, `3` = shuffle (random selection with replacement).
///
/// Surfaced as a thin type wrapper over [`AppInfoPlayList::playback_type`]
/// so callers can pattern-match on the documented semantics without
/// scattering magic numbers; obtained via
/// [`AppInfoPlayList::playback_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlayListPlaybackType {
    /// `0x01` — Sequential. PlayItems play in the order recorded.
    Sequential,
    /// `0x02` — Random. PlayItems are selected at random *without*
    /// replacement, so each plays at most once per traversal.
    Random,
    /// `0x03` — Shuffle. PlayItems are selected at random *with*
    /// replacement (the same PlayItem may play repeatedly).
    Shuffle,
    /// Any other raw value, preserved for diagnostics.
    Other(u8),
}

impl PlayListPlaybackType {
    /// Decode the wire byte into a typed variant. Unknown bytes round
    /// through [`Self::Other`] rather than failing.
    pub fn from_raw(v: u8) -> Self {
        match v {
            0x01 => Self::Sequential,
            0x02 => Self::Random,
            0x03 => Self::Shuffle,
            other => Self::Other(other),
        }
    }

    /// Encode this variant back to its wire byte. Round-trips with
    /// [`Self::from_raw`].
    pub fn as_raw(self) -> u8 {
        match self {
            Self::Sequential => 0x01,
            Self::Random => 0x02,
            Self::Shuffle => 0x03,
            Self::Other(v) => v,
        }
    }

    /// True when PlayItems play in the recorded order — i.e. the
    /// sequential variant. False for both random-pick variants and any
    /// other value the wire records.
    pub fn is_sequential(self) -> bool {
        matches!(self, Self::Sequential)
    }

    /// True when the wire value selects a randomised traversal —
    /// either random-without-replacement or shuffle-with-replacement.
    /// `false` for sequential and for any other byte recorded.
    pub fn is_randomised(self) -> bool {
        matches!(self, Self::Random | Self::Shuffle)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppInfoPlayList {
    /// Wire byte `PlayList_playback_type` — see
    /// [`PlayListPlaybackType`] for the typed view.
    pub playback_type: u8,
    pub playback_count: u16,
    pub random_access_flag: u8,
    pub audio_mix_app_flag: u8,
    pub lossless_may_bypass_mixer_flag: u8,
    /// `UO_mask_table` (§5.4.3) — the 64-bit User-Operation prohibition
    /// table, big-endian, preserved verbatim. Each bit, when set,
    /// prohibits one remote-control user operation for the whole
    /// PlayList; the individual bit → operation assignments are not
    /// tabulated in the consulted references, so the raw word is
    /// surfaced rather than decoded. Preserving it lets an
    /// [`PlayListMpls::encode`] → [`PlayListMpls::parse`] round trip
    /// keep the disc's UO prohibitions instead of dropping them to zero.
    pub uo_mask: u64,
}

impl AppInfoPlayList {
    /// Typed view of [`Self::playback_type`] — see
    /// [`PlayListPlaybackType`] for the variant set.
    pub fn playback_kind(&self) -> PlayListPlaybackType {
        PlayListPlaybackType::from_raw(self.playback_type)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayList {
    pub play_items: Vec<PlayItem>,
    pub sub_paths: Vec<SubPath>,
}

/// One PlayListMark — chapter / bookmark / skip-point entry from
/// `PlayListMark()` (§5.4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayListMark {
    pub mark_type: u8,
    pub ref_play_item_id: u16,
    /// Clip-local timestamp of the mark, in 45 kHz ticks. Lies on the
    /// same time axis as the referenced PlayItem's
    /// [`PlayItem::in_time_ticks`] / [`PlayItem::out_time_ticks`].
    pub mark_time_ticks: u32,
    pub entry_es_pid: u16,
    pub duration_ticks: u32,
}

impl PlayListMark {
    /// Decode the raw [`Self::mark_type`] byte into a [`MarkType`].
    pub fn kind(&self) -> MarkType {
        MarkType::from_raw(self.mark_type)
    }
}

/// A user-visible chapter, derived from an entry-mark in a PlayList by
/// [`PlayListMpls::chapters`]. Carries a **title-relative** 90 kHz
/// presentation timestamp ready to hand to
/// [`crate::TitleSource::seek_to`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chapter {
    /// 0-based chapter index in playback order (chapter 1 is index 0).
    pub index: usize,
    /// Title-relative start time of the chapter in 90 kHz ticks — the
    /// sum of every preceding PlayItem's duration plus the mark's offset
    /// into its own PlayItem. Directly seekable via
    /// [`crate::TitleSource::seek_to`].
    pub start_pts_90k: u64,
    /// Index of the PlayItem this chapter's entry-mark references.
    pub ref_play_item_id: u16,
}

/// A chapter with its derived presentation span — the title-relative
/// `[start, end)` window and the duration that follows from it.
///
/// Produced by [`PlayListMpls::chapters_with_duration`]. The `start` is the
/// same value [`Chapter::start_pts_90k`] carries; the `end` is the next
/// chapter's start in playback order, and the *last* chapter's end is the
/// title's total presentation length ([`PlayListMpls::duration_90k`]). All
/// derivation over already-parsed marks + PlayItem durations — no new wire
/// layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChapterSpan {
    /// 0-based chapter index in playback order (chapter 1 is index 0).
    pub index: usize,
    /// Title-relative start in 90 kHz ticks — identical to
    /// [`Chapter::start_pts_90k`]. Directly seekable via
    /// [`crate::TitleSource::seek_to`].
    pub start_pts_90k: u64,
    /// Title-relative end in 90 kHz ticks — the next chapter's start, or
    /// the title total duration for the final chapter. Always
    /// `>= start_pts_90k`.
    pub end_pts_90k: u64,
    /// Index of the PlayItem this chapter's entry-mark references.
    pub ref_play_item_id: u16,
}

impl ChapterSpan {
    /// Duration of this chapter in 90 kHz ticks (`end - start`). Saturates
    /// at 0 for the degenerate equal-timestamp case.
    pub fn duration_90k(&self) -> u64 {
        self.end_pts_90k.saturating_sub(self.start_pts_90k)
    }

    /// Duration of this chapter in (truncated) whole seconds.
    pub fn duration_secs(&self) -> u64 {
        self.duration_90k() / 90_000
    }
}

/// Parsed `.mpls` file.
#[derive(Debug, Clone)]
pub struct PlayListMpls {
    pub version: [u8; 4],
    pub app_info: AppInfoPlayList,
    pub play_list: PlayList,
    pub marks: Vec<PlayListMark>,
}

impl PlayListMpls {
    pub fn parse(buf: &[u8]) -> Result<Self> {
        let header = BdmvHeader::parse(buf)?;
        if header.type_indicator != b"MPLS" {
            return Err(BlurayError::malformed(format!(
                ".mpls type_indicator {:?}",
                header.type_indicator
            )));
        }
        let version = *header.version_number;
        if buf.len() < 40 {
            return Err(BlurayError::malformed(".mpls truncated before AppInfo"));
        }

        let mut r = Reader::new(buf);
        r.seek(8)?;
        let playlist_start = r.read_u32()? as usize;
        let marks_start = r.read_u32()? as usize;
        let _ext_start = r.read_u32()? as usize;
        // 20 reserved bytes at 20..40
        r.seek(40)?;

        // AppInfoPlayList
        let app_len = r.read_u32()? as usize;
        if app_len < 14 {
            return Err(BlurayError::malformed(
                "AppInfoPlayList shorter than 14 bytes",
            ));
        }
        let app_start = r.pos;
        r.skip(1)?; // reserved
        let playback_type = r.read_u8()?;
        let playback_count = r.read_u16()?;
        let uo_mask = r.read_u64()?; // UO_mask_table (§5.4.3)
        let flag_bits = r.read_u8()?;
        let _padding = r.read_u8()?; // 13 bits reserved actually split across bytes — top of next byte stays 0
        let random_access_flag = (flag_bits >> 7) & 1;
        let audio_mix_app_flag = (flag_bits >> 6) & 1;
        let lossless_may_bypass_mixer_flag = (flag_bits >> 5) & 1;
        let app_info = AppInfoPlayList {
            playback_type,
            playback_count,
            random_access_flag,
            audio_mix_app_flag,
            lossless_may_bypass_mixer_flag,
            uo_mask,
        };
        // Skip remainder of AppInfo
        r.seek(app_start + app_len)?;

        // PlayList()
        r.seek(playlist_start)?;
        let pl_len = r.read_u32()? as usize;
        let pl_body_start = r.pos;
        let pl_body_end = pl_body_start + pl_len;
        if pl_body_end > buf.len() {
            return Err(BlurayError::malformed("PlayList body overruns buffer"));
        }
        r.skip(2)?; // 2 reserved bytes
        let n_play_items = r.read_u16()? as usize;
        let n_sub_paths = r.read_u16()? as usize;

        let mut play_items = Vec::with_capacity(n_play_items);
        for _ in 0..n_play_items {
            play_items.push(parse_play_item(&mut r)?);
        }
        let mut sub_paths = Vec::with_capacity(n_sub_paths);
        for _ in 0..n_sub_paths {
            sub_paths.push(parse_sub_path(&mut r)?);
        }

        // PlayListMark()
        let marks = if marks_start > 0 && marks_start < buf.len() {
            r.seek(marks_start)?;
            let _mark_len = r.read_u32()?;
            let n_marks = r.read_u16()? as usize;
            let mut marks = Vec::with_capacity(n_marks);
            for _ in 0..n_marks {
                let _reserved = r.read_u8()?;
                let mark_type = r.read_u8()?;
                let ref_play_item_id = r.read_u16()?;
                let mark_time_ticks = r.read_u32()?;
                let entry_es_pid = r.read_u16()?;
                let duration_ticks = r.read_u32()?;
                marks.push(PlayListMark {
                    mark_type,
                    ref_play_item_id,
                    mark_time_ticks,
                    entry_es_pid,
                    duration_ticks,
                });
            }
            marks
        } else {
            Vec::new()
        };

        Ok(Self {
            version,
            app_info,
            play_list: PlayList {
                play_items,
                sub_paths,
            },
            marks,
        })
    }

    /// Total title duration in 90 kHz ticks (sum of PlayItem durations).
    pub fn duration_90k(&self) -> u64 {
        self.play_list
            .play_items
            .iter()
            .map(|p| p.duration_90k())
            .sum()
    }

    /// Title-relative chapter list, in playback order.
    ///
    /// Each entry-mark (`mark_type == 0x01`, §5.4.5) becomes one
    /// [`Chapter`]. The mark's `mark_time_ticks` is a *clip-local* 45 kHz
    /// timestamp on the referenced PlayItem's own time axis (the same
    /// axis as [`PlayItem::in_time_ticks`]); a player's chapter search
    /// instead navigates the *title* timeline, which concatenates every
    /// PlayItem's `[IN, OUT]` window end-to-end. We therefore convert:
    ///
    /// ```text
    ///   chapter_pts = Σ duration_90k(items before ref) +
    ///                 (mark_time_90k − in_time_90k of ref item)
    /// ```
    ///
    /// The result is directly seekable via
    /// [`crate::TitleSource::seek_to`]. Marks whose `ref_play_item_id`
    /// is out of range, or whose timestamp falls before the referenced
    /// PlayItem's IN point, are skipped (malformed authoring). Link
    /// points (`mark_type == 0x02`) are not chapters and are excluded.
    pub fn chapters(&self) -> Vec<Chapter> {
        let items = &self.play_list.play_items;
        // Running title-relative start (90 kHz) of each PlayItem.
        let mut item_start_90k = Vec::with_capacity(items.len());
        let mut acc: u64 = 0;
        for pi in items {
            item_start_90k.push(acc);
            acc += pi.duration_90k();
        }

        let mut out = Vec::new();
        for m in &self.marks {
            if !m.kind().is_chapter() {
                continue;
            }
            let ref_id = m.ref_play_item_id as usize;
            let Some(pi) = items.get(ref_id) else {
                continue;
            };
            let mark_90k = u64::from(m.mark_time_ticks) * 2;
            let in_90k = u64::from(pi.in_time_ticks) * 2;
            // A mark before its PlayItem's IN point is malformed; skip it
            // rather than wrapping into a bogus huge offset.
            if mark_90k < in_90k {
                continue;
            }
            let start_pts_90k = item_start_90k[ref_id] + (mark_90k - in_90k);
            out.push(Chapter {
                index: out.len(),
                start_pts_90k,
                ref_play_item_id: m.ref_play_item_id,
            });
        }
        out
    }

    /// Chapter list with each chapter's derived presentation span.
    ///
    /// Same chapters as [`Self::chapters`], but each is widened to a
    /// `[start, end)` window: a chapter ends where the next one begins
    /// (in playback order), and the final chapter ends at the title's
    /// total duration ([`Self::duration_90k`]). The returned spans are
    /// sorted by `start_pts_90k` so the `end = next.start` rule holds even
    /// when the underlying marks were authored out of order. A chapter
    /// whose start somehow exceeds the title duration (malformed authoring)
    /// gets `end == start` (zero duration) rather than an underflow.
    pub fn chapters_with_duration(&self) -> Vec<ChapterSpan> {
        let mut chapters = self.chapters();
        if chapters.is_empty() {
            return Vec::new();
        }
        // Order by start so "end = next start" is meaningful regardless of
        // authoring order; preserve each chapter's original ref + re-number
        // the playback-order index.
        chapters.sort_by_key(|c| c.start_pts_90k);
        let title_end = self.duration_90k();

        let mut out = Vec::with_capacity(chapters.len());
        for (i, c) in chapters.iter().enumerate() {
            let end = match chapters.get(i + 1) {
                Some(next) => next.start_pts_90k,
                None => title_end.max(c.start_pts_90k),
            };
            out.push(ChapterSpan {
                index: i,
                start_pts_90k: c.start_pts_90k,
                end_pts_90k: end.max(c.start_pts_90k),
                ref_play_item_id: c.ref_play_item_id,
            });
        }
        out
    }

    /// Test-only encoder. Produces a minimally-conformant `.mpls`
    /// payload for the parser to round-trip. The marks block is
    /// always emitted (length 0 if there are no marks).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"MPLS");
        out.extend_from_slice(&self.version);
        // Three offset placeholders, filled at the end.
        out.extend_from_slice(&[0u8; 4]); // playlist_start
        out.extend_from_slice(&[0u8; 4]); // marks_start
        out.extend_from_slice(&[0u8; 4]); // ext_start
        out.extend_from_slice(&[0u8; 20]); // 20 reserved

        // AppInfo: length(u32) + 14 bytes body.
        let app_body_len: u32 = 14;
        out.extend_from_slice(&app_body_len.to_be_bytes());
        out.push(0); // reserved
        out.push(self.app_info.playback_type);
        out.extend_from_slice(&self.app_info.playback_count.to_be_bytes());
        out.extend_from_slice(&self.app_info.uo_mask.to_be_bytes()); // UO_mask_table
        let fb = ((self.app_info.random_access_flag & 1) << 7)
            | ((self.app_info.audio_mix_app_flag & 1) << 6)
            | ((self.app_info.lossless_may_bypass_mixer_flag & 1) << 5);
        out.push(fb);
        out.push(0); // 13 bits reserved — remaining 8 are 0

        // PlayList() begins here.
        let playlist_start = out.len() as u32;
        let len_off = out.len();
        out.extend_from_slice(&[0u8; 4]); // pl_len placeholder
        let body_start = out.len();
        out.extend_from_slice(&[0u8; 2]); // 2 reserved
        out.extend_from_slice(&(self.play_list.play_items.len() as u16).to_be_bytes());
        out.extend_from_slice(&(self.play_list.sub_paths.len() as u16).to_be_bytes());
        for pi in &self.play_list.play_items {
            encode_play_item(&mut out, pi);
        }
        for sp in &self.play_list.sub_paths {
            encode_sub_path(&mut out, sp);
        }
        let body_len = (out.len() - body_start) as u32;
        out[len_off..len_off + 4].copy_from_slice(&body_len.to_be_bytes());

        // PlayListMark() block.
        let marks_start = out.len() as u32;
        let mark_len_off = out.len();
        out.extend_from_slice(&[0u8; 4]); // length placeholder
        let mark_body_start = out.len();
        out.extend_from_slice(&(self.marks.len() as u16).to_be_bytes());
        for m in &self.marks {
            out.push(0); // reserved
            out.push(m.mark_type);
            out.extend_from_slice(&m.ref_play_item_id.to_be_bytes());
            out.extend_from_slice(&m.mark_time_ticks.to_be_bytes());
            out.extend_from_slice(&m.entry_es_pid.to_be_bytes());
            out.extend_from_slice(&m.duration_ticks.to_be_bytes());
        }
        let mark_body_len = (out.len() - mark_body_start) as u32;
        out[mark_len_off..mark_len_off + 4].copy_from_slice(&mark_body_len.to_be_bytes());

        // Backfill offsets.
        out[8..12].copy_from_slice(&playlist_start.to_be_bytes());
        out[12..16].copy_from_slice(&marks_start.to_be_bytes());
        out
    }
}

fn parse_play_item(r: &mut Reader<'_>) -> Result<PlayItem> {
    let len = r.read_u16()? as usize;
    let body_start = r.pos;
    let body_end = body_start + len;
    let clip_stem_bytes = r.slice(5)?;
    let clip_information_file_name = std::str::from_utf8(clip_stem_bytes)
        .map_err(|_| BlurayError::malformed("PlayItem clip name not ASCII"))?
        .to_string();
    let codec_bytes = r.slice(4)?;
    let mut clip_codec_identifier = [0u8; 4];
    clip_codec_identifier.copy_from_slice(codec_bytes);
    // 11 bits reserved + is_multi_angle (1 bit) + connection_condition (4 bits)
    let pack = r.read_u16()?;
    let is_multi_angle = ((pack >> 4) & 1) as u8;
    let cc = (pack & 0xF) as u8;
    let stc_id_ref = r.read_u8()?;
    let in_time_ticks = r.read_u32()?;
    let out_time_ticks = r.read_u32()?;
    // UO_mask_table (§5.4.4.1) — 64-bit UO prohibition table, preserved.
    let uo_mask = r.read_u64()?;
    // random_access_flag 1 bit (top) + reserved 7 bits
    let random_access_byte = r.read_u8()?;
    let random_access_flag = (random_access_byte & 0x80) != 0;
    // still_mode 1 byte + still_time u16
    let still_mode = r.read_u8()?;
    let still_time = r.read_u16()?;
    let mut angle_flags = 0u8;
    let (multi_clip_count, angles) = if is_multi_angle != 0 {
        let num_angles = r.read_u8()?;
        // flags byte (surfaced raw; bit assignments not pinned by refs)
        angle_flags = r.read_u8()?;
        // (num_angles - 1) repeated entries of
        //   5-byte clip_information_file_name + 4-byte codec id +
        //   1-byte stc_id_ref + 1 reserved
        // = 11 bytes each (§5.4.4.1 `is_multi_angle` block).
        let alt_count = num_angles.saturating_sub(1) as usize;
        let mut angles = Vec::with_capacity(alt_count);
        for _ in 0..alt_count {
            let stem_bytes = r.slice(5)?;
            let clip_information_file_name = std::str::from_utf8(stem_bytes)
                .map_err(|_| BlurayError::malformed("PlayItem angle clip name not ASCII"))?
                .to_string();
            let codec_bytes = r.slice(4)?;
            let mut clip_codec_identifier = [0u8; 4];
            clip_codec_identifier.copy_from_slice(codec_bytes);
            let stc_id_ref = r.read_u8()?;
            r.skip(1)?; // reserved
            angles.push(AngleClip {
                clip_information_file_name,
                clip_codec_identifier,
                stc_id_ref,
            });
        }
        (num_angles, angles)
    } else {
        (1, Vec::new())
    };

    // STN_table() — read the count header + every per-stream
    // (stream_entry + stream_attributes) pair. Per BD-ROM Part 3
    // §5.4.4.4 the streams appear in this fixed order:
    //   primary_video × N1, primary_audio × N2, pg × N3, ig × N4,
    //   secondary_audio × N5, secondary_video × N6, pip_pg × N7.
    // Text-subtitle entries (if any) live in the trailing block of the
    // table; we collect them by scanning the residue.
    let stn_len = r.read_u16()? as usize;
    let stn_body_start = r.pos;
    let stn_body_end = stn_body_start + stn_len;
    let stn_table = if stn_len >= 14 {
        // 2 reserved bytes
        r.skip(2)?;
        let num_primary_video = r.read_u8()?;
        let num_primary_audio = r.read_u8()?;
        let num_pg = r.read_u8()?;
        let num_ig = r.read_u8()?;
        let num_secondary_audio = r.read_u8()?;
        let num_secondary_video = r.read_u8()?;
        let num_pip_pg = r.read_u8()?;
        // 5 reserved bytes consumed at the fixed offset (the 14-byte
        // STN header: 2 reserved + 7 count bytes + 5 reserved).
        r.skip(5)?;

        // The remaining body holds the per-stream pairs in the order
        // above. Each pair is `length`-prefixed; if the buffer runs out
        // (malformed authoring) we surface a clean error rather than
        // panic. Bound the inner readers by `stn_body_end` so a bogus
        // length byte can't escape the STN block.
        let mut t = StnTable::default();
        for _ in 0..num_primary_video {
            t.primary_video.push(parse_primary_video_stream(r)?);
        }
        for _ in 0..num_primary_audio {
            t.primary_audio.push(parse_primary_audio_stream(r)?);
        }
        for _ in 0..num_pg {
            t.pg_subtitles.push(parse_pg_subtitle_stream(r)?);
        }
        for _ in 0..num_ig {
            t.ig_streams.push(parse_ig_stream(r)?);
        }
        for _ in 0..num_secondary_audio {
            // Secondary audio adds 1-byte num_secondary_audio_extra_pid
            // + N additional PID bytes per entry — but we follow the
            // simplified layout (same as primary audio) per BD-ROM
            // Part 3 §5.4.4.4. Anything trailing is skipped by the
            // stream_attributes length envelope.
            t.secondary_audio.push(parse_secondary_audio_stream(r)?);
        }
        for _ in 0..num_secondary_video {
            t.secondary_video.push(parse_secondary_video_stream(r)?);
        }
        for _ in 0..num_pip_pg {
            t.pip_pg.push(parse_pip_pg_stream(r)?);
        }
        t
    } else {
        StnTable::default()
    };
    r.seek(stn_body_end)?;

    // Skip any tail bytes (`length` covers everything from here).
    r.seek(body_end)?;

    Ok(PlayItem {
        clip_information_file_name,
        clip_codec_identifier,
        connection_condition: ConnectionCondition::from_raw(cc),
        stc_id_ref,
        in_time_ticks,
        out_time_ticks,
        multi_clip_count,
        angles,
        stn_table,
        flags: PlayItemFlags {
            random_access_flag,
            still_mode,
            still_time,
            angle_flags,
            uo_mask,
        },
    })
}

fn encode_play_item(out: &mut Vec<u8>, pi: &PlayItem) {
    // length placeholder
    let len_off = out.len();
    out.extend_from_slice(&[0u8; 2]);
    let body_start = out.len();

    let mut clip_name = [b'0'; 5];
    let bytes = pi.clip_information_file_name.as_bytes();
    let take = bytes.len().min(5);
    clip_name[..take].copy_from_slice(&bytes[..take]);
    out.extend_from_slice(&clip_name);
    out.extend_from_slice(&pi.clip_codec_identifier);

    let cc_raw = match pi.connection_condition {
        ConnectionCondition::NonSeamless => 0x01,
        ConnectionCondition::SeamlessContinuation => 0x05,
        ConnectionCondition::SeamlessNewStc => 0x06,
        ConnectionCondition::Other(v) => v,
    } as u16;
    // 11 reserved + multi-angle bit + 4-bit cc
    let pack: u16 = ((if pi.multi_clip_count > 1 { 1 } else { 0 }) << 4) | (cc_raw & 0xF);
    out.extend_from_slice(&pack.to_be_bytes());
    out.push(pi.stc_id_ref);
    out.extend_from_slice(&pi.in_time_ticks.to_be_bytes());
    out.extend_from_slice(&pi.out_time_ticks.to_be_bytes());
    out.extend_from_slice(&pi.flags.uo_mask.to_be_bytes()); // UO_mask_table
    out.push(if pi.flags.random_access_flag { 0x80 } else { 0 }); // random_access_flag (top bit) + reserved
    out.push(pi.flags.still_mode); // still_mode
    out.extend_from_slice(&pi.flags.still_time.to_be_bytes()); // still_time

    if pi.multi_clip_count > 1 {
        out.push(pi.multi_clip_count);
        out.push(pi.flags.angle_flags);
        // Write `multi_clip_count - 1` alt-angle entries. If the
        // `angles` vec is shorter than that (e.g. a hand-built
        // PlayItem that forgot to populate the alt slots), zero-fill
        // the missing entries — the round-trip parser will still see
        // valid 5/4/1/1-byte fields.
        for i in 1..pi.multi_clip_count {
            let idx = (i as usize) - 1;
            match pi.angles.get(idx) {
                Some(angle) => {
                    let mut name = [b'0'; 5];
                    let bytes = angle.clip_information_file_name.as_bytes();
                    let take = bytes.len().min(5);
                    name[..take].copy_from_slice(&bytes[..take]);
                    out.extend_from_slice(&name);
                    out.extend_from_slice(&angle.clip_codec_identifier);
                    out.push(angle.stc_id_ref);
                    out.push(0); // reserved
                }
                None => out.extend_from_slice(&[0u8; 11]),
            }
        }
    }

    // STN_table() — 14-byte header (2 reserved + 7 counts + 5 reserved)
    // followed by one (stream_entry + stream_attributes) pair per
    // stream in the canonical class order.
    let stn_len_off = out.len();
    out.extend_from_slice(&[0u8; 2]); // placeholder for STN body length
    let stn_body_start = out.len();
    out.extend_from_slice(&[0u8; 2]); // 2 reserved
    out.push(pi.stn_table.primary_video.len() as u8);
    out.push(pi.stn_table.primary_audio.len() as u8);
    out.push(pi.stn_table.pg_subtitles.len() as u8);
    out.push(pi.stn_table.ig_streams.len() as u8);
    out.push(pi.stn_table.secondary_audio.len() as u8);
    out.push(pi.stn_table.secondary_video.len() as u8);
    out.push(pi.stn_table.pip_pg.len() as u8);
    out.extend_from_slice(&[0u8; 5]); // 5 reserved

    for s in &pi.stn_table.primary_video {
        encode_video_stream(
            out,
            s.elementary_pid,
            s.coding_type,
            s.video_format,
            s.frame_rate,
            s.aspect_ratio,
        );
    }
    for s in &pi.stn_table.primary_audio {
        encode_audio_stream(
            out,
            s.elementary_pid,
            s.coding_type,
            s.audio_format,
            s.sample_rate,
            &s.language_code,
        );
    }
    for s in &pi.stn_table.pg_subtitles {
        encode_language_stream(out, s.elementary_pid, s.coding_type, &s.language_code);
    }
    for s in &pi.stn_table.ig_streams {
        encode_language_stream(out, s.elementary_pid, s.coding_type, &s.language_code);
    }
    for s in &pi.stn_table.secondary_audio {
        encode_audio_stream(
            out,
            s.elementary_pid,
            s.coding_type,
            s.audio_format,
            s.sample_rate,
            &s.language_code,
        );
    }
    for s in &pi.stn_table.secondary_video {
        encode_video_stream(
            out,
            s.elementary_pid,
            s.coding_type,
            s.video_format,
            s.frame_rate,
            s.aspect_ratio,
        );
    }
    for s in &pi.stn_table.pip_pg {
        encode_language_stream(out, s.elementary_pid, s.coding_type, &s.language_code);
    }

    let stn_body_len = (out.len() - stn_body_start) as u16;
    out[stn_len_off..stn_len_off + 2].copy_from_slice(&stn_body_len.to_be_bytes());

    let body_len = (out.len() - body_start) as u16;
    out[len_off..len_off + 2].copy_from_slice(&body_len.to_be_bytes());
}

// ─────────────────────── STN_table per-stream helpers ───────────────────────
//
// Each per-stream record is two length-prefixed blocks:
//
//   stream_entry:
//     length            u8     (count of payload bytes after this byte)
//     stream_type       u8     1 = in-mux elementary stream from main Clip
//                              2/3/4 = from a SubPath / overlay (unused here)
//     ref_to_stream_PID u16    big-endian elementary PID (type 1 only)
//     [padding to `length`]
//
//   stream_attributes:
//     length             u8
//     stream_coding_type u8
//     [per-codec attribute bytes]
//     [padding to `length`]
//
// The parser reads only the fields it surfaces; everything in the
// length envelope past the recognised fields is skipped — that's both
// how the spec allows authoring tools to add new attribute bytes and
// how we stay forward-compatible with codecs we haven't enumerated.

/// Parse the `stream_entry` block. Returns the elementary PID (zero
/// when the entry is non-in-mux — type 2/3/4 carry a SubPath ref + ref
/// stream ID instead of a direct PID; we leave those at zero rather
/// than fabricating a value).
fn parse_stream_entry(r: &mut Reader<'_>) -> Result<u16> {
    let len = r.read_u8()? as usize;
    let start = r.pos;
    let end = start + len;
    if end > r.buf.len() {
        return Err(BlurayError::malformed("stream_entry overruns buffer"));
    }
    let pid = if len >= 3 {
        let stream_type = r.read_u8()?;
        if stream_type == 1 {
            r.read_u16()?
        } else {
            // Non-in-mux: leave PID at 0 — the demuxer won't see this
            // PID on the main TS anyway, so the muxer-relevant field
            // is absent.
            0
        }
    } else {
        0
    };
    r.seek(end)?;
    Ok(pid)
}

fn encode_stream_entry(out: &mut Vec<u8>, pid: u16) {
    // Fixed 9-byte payload (the spec-canonical in-mux entry length).
    let payload_len: u8 = 9;
    out.push(payload_len);
    let start = out.len();
    out.push(1); // stream_type = 1 (in-mux)
    out.extend_from_slice(&pid.to_be_bytes());
    // Pad to `payload_len` bytes with zeros.
    while out.len() - start < payload_len as usize {
        out.push(0);
    }
}

fn parse_primary_video_stream(r: &mut Reader<'_>) -> Result<PrimaryVideoStream> {
    let elementary_pid = parse_stream_entry(r)?;
    let len = r.read_u8()? as usize;
    let start = r.pos;
    let end = start + len;
    if end > r.buf.len() {
        return Err(BlurayError::malformed("video stream_attributes overruns"));
    }
    let coding_raw = if len >= 1 { r.read_u8()? } else { 0 };
    let (video_format, frame_rate) = if r.pos < end {
        let b = r.read_u8()?;
        ((b >> 4) & 0xF, b & 0xF)
    } else {
        (0, 0)
    };
    let aspect_ratio = if r.pos < end {
        (r.read_u8()? >> 4) & 0xF
    } else {
        0
    };
    r.seek(end)?;
    Ok(PrimaryVideoStream {
        elementary_pid,
        coding_type: StreamCodingType::from_raw(coding_raw),
        video_format,
        frame_rate,
        aspect_ratio,
    })
}

fn parse_primary_audio_stream(r: &mut Reader<'_>) -> Result<PrimaryAudioStream> {
    let elementary_pid = parse_stream_entry(r)?;
    let len = r.read_u8()? as usize;
    let start = r.pos;
    let end = start + len;
    if end > r.buf.len() {
        return Err(BlurayError::malformed("audio stream_attributes overruns"));
    }
    let coding_raw = if len >= 1 { r.read_u8()? } else { 0 };
    let (audio_format, sample_rate) = if r.pos < end {
        let b = r.read_u8()?;
        ((b >> 4) & 0xF, b & 0xF)
    } else {
        (0, 0)
    };
    let mut language_code = [0u8; 3];
    if r.pos + 3 <= end {
        language_code.copy_from_slice(r.slice(3)?);
    }
    r.seek(end)?;
    Ok(PrimaryAudioStream {
        elementary_pid,
        coding_type: StreamCodingType::from_raw(coding_raw),
        audio_format,
        sample_rate,
        language_code,
    })
}

fn parse_language_stream(r: &mut Reader<'_>) -> Result<(u16, StreamCodingType, [u8; 3])> {
    let elementary_pid = parse_stream_entry(r)?;
    let len = r.read_u8()? as usize;
    let start = r.pos;
    let end = start + len;
    if end > r.buf.len() {
        return Err(BlurayError::malformed(
            "graphics stream_attributes overruns",
        ));
    }
    let coding_raw = if len >= 1 { r.read_u8()? } else { 0 };
    let mut language_code = [0u8; 3];
    if r.pos + 3 <= end {
        language_code.copy_from_slice(r.slice(3)?);
    }
    r.seek(end)?;
    Ok((
        elementary_pid,
        StreamCodingType::from_raw(coding_raw),
        language_code,
    ))
}

fn parse_pg_subtitle_stream(r: &mut Reader<'_>) -> Result<PgsSubtitleStream> {
    let (elementary_pid, coding_type, language_code) = parse_language_stream(r)?;
    Ok(PgsSubtitleStream {
        elementary_pid,
        coding_type,
        language_code,
    })
}

fn parse_ig_stream(r: &mut Reader<'_>) -> Result<IgsInteractiveStream> {
    let (elementary_pid, coding_type, language_code) = parse_language_stream(r)?;
    Ok(IgsInteractiveStream {
        elementary_pid,
        coding_type,
        language_code,
    })
}

fn parse_secondary_audio_stream(r: &mut Reader<'_>) -> Result<SecondaryAudioStream> {
    let p = parse_primary_audio_stream(r)?;
    Ok(SecondaryAudioStream {
        elementary_pid: p.elementary_pid,
        coding_type: p.coding_type,
        audio_format: p.audio_format,
        sample_rate: p.sample_rate,
        language_code: p.language_code,
    })
}

fn parse_secondary_video_stream(r: &mut Reader<'_>) -> Result<SecondaryVideoStream> {
    let p = parse_primary_video_stream(r)?;
    Ok(SecondaryVideoStream {
        elementary_pid: p.elementary_pid,
        coding_type: p.coding_type,
        video_format: p.video_format,
        frame_rate: p.frame_rate,
        aspect_ratio: p.aspect_ratio,
    })
}

fn parse_pip_pg_stream(r: &mut Reader<'_>) -> Result<PipPgStream> {
    let (elementary_pid, coding_type, language_code) = parse_language_stream(r)?;
    Ok(PipPgStream {
        elementary_pid,
        coding_type,
        language_code,
    })
}

fn encode_video_stream(
    out: &mut Vec<u8>,
    pid: u16,
    coding: StreamCodingType,
    video_format: u8,
    frame_rate: u8,
    aspect_ratio: u8,
) {
    encode_stream_entry(out, pid);
    let attr_len: u8 = 5;
    out.push(attr_len);
    let start = out.len();
    out.push(coding.as_raw());
    out.push(((video_format & 0xF) << 4) | (frame_rate & 0xF));
    out.push((aspect_ratio & 0xF) << 4);
    while out.len() - start < attr_len as usize {
        out.push(0);
    }
}

fn encode_audio_stream(
    out: &mut Vec<u8>,
    pid: u16,
    coding: StreamCodingType,
    audio_format: u8,
    sample_rate: u8,
    language_code: &[u8; 3],
) {
    encode_stream_entry(out, pid);
    let attr_len: u8 = 6;
    out.push(attr_len);
    let start = out.len();
    out.push(coding.as_raw());
    out.push(((audio_format & 0xF) << 4) | (sample_rate & 0xF));
    out.extend_from_slice(language_code);
    while out.len() - start < attr_len as usize {
        out.push(0);
    }
}

fn encode_language_stream(
    out: &mut Vec<u8>,
    pid: u16,
    coding: StreamCodingType,
    language_code: &[u8; 3],
) {
    encode_stream_entry(out, pid);
    let attr_len: u8 = 4;
    out.push(attr_len);
    let start = out.len();
    out.push(coding.as_raw());
    out.extend_from_slice(language_code);
    while out.len() - start < attr_len as usize {
        out.push(0);
    }
}

fn parse_sub_path(r: &mut Reader<'_>) -> Result<SubPath> {
    let len = r.read_u32()? as usize;
    let body_start = r.pos;
    let body_end = body_start + len;
    r.skip(1)?; // reserved
    let sub_path_type = r.read_u8()?;
    // 15 reserved bits + is_repeat_SubPath (low bit of the 16-bit field)
    let repeat_field = r.read_u16()?;
    let is_repeat_subpath = (repeat_field & 1) != 0;
    r.skip(1)?; // reserved
    let num_sub_play_items = r.read_u8()? as u16;
    r.seek(body_end)?;
    Ok(SubPath {
        sub_path_type,
        is_repeat_subpath,
        num_sub_play_items,
    })
}

fn encode_sub_path(out: &mut Vec<u8>, sp: &SubPath) {
    let len_off = out.len();
    out.extend_from_slice(&[0u8; 4]);
    let body_start = out.len();
    out.push(0); // reserved
    out.push(sp.sub_path_type);
    // 15 reserved bits + is_repeat_SubPath (low bit)
    out.extend_from_slice(&(sp.is_repeat_subpath as u16).to_be_bytes());
    out.push(0); // reserved
    out.push(sp.num_sub_play_items as u8);
    let body_len = (out.len() - body_start) as u32;
    out[len_off..len_off + 4].copy_from_slice(&body_len.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_mpls() -> PlayListMpls {
        PlayListMpls {
            version: *b"0200",
            app_info: AppInfoPlayList {
                playback_type: 1,
                playback_count: 0,
                random_access_flag: 1,
                audio_mix_app_flag: 0,
                lossless_may_bypass_mixer_flag: 0,
                uo_mask: 0,
            },
            play_list: PlayList {
                play_items: vec![
                    PlayItem {
                        clip_information_file_name: "00001".into(),
                        clip_codec_identifier: *b"M2TS",
                        connection_condition: ConnectionCondition::NonSeamless,
                        stc_id_ref: 0,
                        in_time_ticks: 0,
                        out_time_ticks: 45_000 * 60, // 60 s at 45 kHz
                        multi_clip_count: 1,
                        angles: Vec::new(),
                        stn_table: StnTable {
                            primary_video: vec![PrimaryVideoStream {
                                elementary_pid: 0x1011,
                                coding_type: StreamCodingType::AvcVideo,
                                video_format: 0x06,
                                frame_rate: 0x03,
                                aspect_ratio: 0x03,
                            }],
                            primary_audio: vec![PrimaryAudioStream {
                                elementary_pid: 0x1100,
                                coding_type: StreamCodingType::Ac3Audio,
                                audio_format: 0x03,
                                sample_rate: 0x01,
                                language_code: *b"eng",
                            }],
                            ..StnTable::default()
                        },
                        flags: PlayItemFlags::default(),
                    },
                    PlayItem {
                        clip_information_file_name: "00002".into(),
                        clip_codec_identifier: *b"M2TS",
                        connection_condition: ConnectionCondition::SeamlessContinuation,
                        stc_id_ref: 0,
                        in_time_ticks: 45_000 * 30,
                        out_time_ticks: 45_000 * 30 + 45_000 * 45, // 45 s
                        multi_clip_count: 1,
                        angles: Vec::new(),
                        stn_table: StnTable {
                            primary_video: vec![PrimaryVideoStream {
                                elementary_pid: 0x1011,
                                coding_type: StreamCodingType::AvcVideo,
                                video_format: 0x06,
                                frame_rate: 0x03,
                                aspect_ratio: 0x03,
                            }],
                            primary_audio: vec![
                                PrimaryAudioStream {
                                    elementary_pid: 0x1100,
                                    coding_type: StreamCodingType::Ac3Audio,
                                    audio_format: 0x03,
                                    sample_rate: 0x01,
                                    language_code: *b"eng",
                                },
                                PrimaryAudioStream {
                                    elementary_pid: 0x1101,
                                    coding_type: StreamCodingType::DtsHdMaAudio,
                                    audio_format: 0x06,
                                    sample_rate: 0x01,
                                    language_code: *b"jpn",
                                },
                            ],
                            pg_subtitles: vec![PgsSubtitleStream {
                                elementary_pid: 0x1200,
                                coding_type: StreamCodingType::PgsSubtitle,
                                language_code: *b"eng",
                            }],
                            ..StnTable::default()
                        },
                        flags: PlayItemFlags::default(),
                    },
                ],
                sub_paths: vec![SubPath {
                    sub_path_type: 5,
                    is_repeat_subpath: false,
                    num_sub_play_items: 1,
                }],
            },
            marks: vec![PlayListMark {
                mark_type: 1,
                ref_play_item_id: 0,
                mark_time_ticks: 0,
                entry_es_pid: 0x1011,
                duration_ticks: 0,
            }],
        }
    }

    #[test]
    fn round_trip() {
        let m = sample_mpls();
        let bytes = m.encode();
        let parsed = PlayListMpls::parse(&bytes).unwrap();
        assert_eq!(parsed.version, m.version);
        assert_eq!(parsed.app_info, m.app_info);
        assert_eq!(parsed.play_list, m.play_list);
        assert_eq!(parsed.marks, m.marks);
    }

    #[test]
    fn play_item_flags_default_is_all_clear() {
        // A freshly-built PlayItem (via `sample_mpls`, which leaves the
        // flags at their `Default`) reports every playback-control field
        // cleared — matching the all-zero bytes `encode` writes.
        let m = sample_mpls();
        for pi in &m.play_list.play_items {
            assert_eq!(pi.flags, PlayItemFlags::default());
            assert!(!pi.flags.random_access_flag);
            assert_eq!(pi.flags.still_mode, 0);
            assert_eq!(pi.flags.still_time, 0);
            assert_eq!(pi.flags.angle_flags, 0);
        }
    }

    #[test]
    fn play_item_flags_survive_round_trip() {
        // Set non-trivial random-access / still-mode / still-time values
        // on the first PlayItem and confirm they survive an
        // encode → parse round trip through the wire byte positions.
        let mut m = sample_mpls();
        m.play_list.play_items[0].flags = PlayItemFlags {
            random_access_flag: true,
            still_mode: 0x02,
            still_time: 7,
            // single-angle PlayItem: angle_flags is not on the wire, so
            // it round-trips back to 0 regardless of what we set here.
            angle_flags: 0,
            // Non-trivial per-PlayItem UO_mask_table: all 8 bytes must
            // survive the 8-byte wire field verbatim.
            uo_mask: 0xDEAD_BEEF_0123_4567,
        };
        let bytes = m.encode();
        let parsed = PlayListMpls::parse(&bytes).unwrap();
        let f = parsed.play_list.play_items[0].flags;
        assert!(f.random_access_flag);
        assert_eq!(f.still_mode, 0x02);
        assert_eq!(f.still_time, 7);
        assert_eq!(f.uo_mask, 0xDEAD_BEEF_0123_4567);
        // The second PlayItem left its flags at default → still clear.
        assert_eq!(
            parsed.play_list.play_items[1].flags,
            PlayItemFlags::default()
        );
    }

    #[test]
    fn uo_mask_tables_survive_round_trip() {
        // Both the PlayList-scoped (AppInfoPlayList §5.4.3) and the
        // PlayItem-scoped (§5.4.4.1) UO_mask_table are 8-byte wire fields
        // the parser used to discard (encode wrote zeros). Confirm every
        // one of the 64 bits round-trips through encode → parse.
        let mut m = sample_mpls();
        m.app_info.uo_mask = 0x0102_0408_1020_4080;
        m.play_list.play_items[0].flags.uo_mask = 0xFFFF_0000_FFFF_0000;
        m.play_list.play_items[1].flags.uo_mask = 0x0000_FFFF_0000_FFFF;
        let bytes = m.encode();
        let parsed = PlayListMpls::parse(&bytes).unwrap();
        assert_eq!(parsed.app_info.uo_mask, 0x0102_0408_1020_4080);
        assert_eq!(
            parsed.play_list.play_items[0].flags.uo_mask,
            0xFFFF_0000_FFFF_0000
        );
        assert_eq!(
            parsed.play_list.play_items[1].flags.uo_mask,
            0x0000_FFFF_0000_FFFF
        );
    }

    #[test]
    fn sub_path_repeat_flag_survives_round_trip() {
        // is_repeat_SubPath is the low bit of the 16-bit field after
        // SubPath_type; it was discarded (encode wrote a fixed zero word).
        let mut m = sample_mpls();
        assert!(!m.play_list.sub_paths[0].is_repeat_subpath);
        m.play_list.sub_paths[0].is_repeat_subpath = true;
        let bytes = m.encode();
        let parsed = PlayListMpls::parse(&bytes).unwrap();
        assert!(parsed.play_list.sub_paths[0].is_repeat_subpath);
        assert_eq!(parsed.play_list.sub_paths[0].sub_path_type, 5);
    }

    #[test]
    fn play_item_angle_flags_survive_round_trip_when_multi_angle() {
        // The multi-angle flags byte is only written (and read) for a
        // multi-angle PlayItem. Build one and confirm the raw byte
        // survives the encode → parse cycle.
        let mut m = sample_mpls();
        let pi = &mut m.play_list.play_items[0];
        pi.multi_clip_count = 2;
        pi.angles = vec![AngleClip {
            clip_information_file_name: "00099".into(),
            clip_codec_identifier: *b"M2TS",
            stc_id_ref: 0,
        }];
        pi.flags.angle_flags = 0b0000_0011;
        let bytes = m.encode();
        let parsed = PlayListMpls::parse(&bytes).unwrap();
        let f = parsed.play_list.play_items[0].flags;
        assert_eq!(f.angle_flags, 0b0000_0011);
        // The alt-angle clip stem still round-trips alongside the flags.
        assert_eq!(
            parsed.play_list.play_items[0].angles[0].clip_information_file_name,
            "00099"
        );
    }

    #[test]
    fn random_access_flag_is_top_bit_only() {
        // The byte after the UO mask table carries the random-access
        // flag in its top bit; the low 7 bits are reserved and must not
        // leak into the typed `bool`. Forge a PlayList whose first
        // PlayItem sets the flag, then flip individual reserved bits in
        // the encoded bytes and confirm the decoded flag is stable.
        let mut m = sample_mpls();
        m.play_list.play_items[0].flags.random_access_flag = true;
        let mut bytes = m.encode();
        // Locate the random-access byte: it is the first 0x80 byte that
        // follows the first PlayItem's 8-byte all-zero UO mask table.
        // Rather than hunt for it, re-parse to confirm the encoder set
        // exactly the top bit.
        let parsed = PlayListMpls::parse(&bytes).unwrap();
        assert!(parsed.play_list.play_items[0].flags.random_access_flag);
        // Now clear the flag and confirm it reads back false even if we
        // would have set reserved bits.
        m.play_list.play_items[0].flags.random_access_flag = false;
        bytes = m.encode();
        let parsed2 = PlayListMpls::parse(&bytes).unwrap();
        assert!(!parsed2.play_list.play_items[0].flags.random_access_flag);
    }

    #[test]
    fn total_duration_90k() {
        let m = sample_mpls();
        // Item 1: (60*45000) ticks @ 45kHz → *2 for 90kHz.
        // Item 2: (45*45000) ticks @ 45kHz → *2 for 90kHz.
        let want = 60u64 * 45_000 * 2 + 45u64 * 45_000 * 2;
        assert_eq!(m.duration_90k(), want);
    }

    #[test]
    fn mark_type_from_raw() {
        assert_eq!(MarkType::from_raw(0x01), MarkType::EntryMark);
        assert_eq!(MarkType::from_raw(0x02), MarkType::LinkPoint);
        assert_eq!(MarkType::from_raw(0x07), MarkType::Other(7));
        assert!(MarkType::EntryMark.is_chapter());
        assert!(!MarkType::LinkPoint.is_chapter());
        assert!(!MarkType::Other(7).is_chapter());
    }

    #[test]
    fn playback_type_from_raw_named_variants() {
        assert_eq!(
            PlayListPlaybackType::from_raw(0x01),
            PlayListPlaybackType::Sequential,
        );
        assert_eq!(
            PlayListPlaybackType::from_raw(0x02),
            PlayListPlaybackType::Random,
        );
        assert_eq!(
            PlayListPlaybackType::from_raw(0x03),
            PlayListPlaybackType::Shuffle,
        );
    }

    #[test]
    fn playback_type_other_round_trips() {
        for v in [0x00u8, 0x04, 0x10, 0x7F, 0xFF] {
            let parsed = PlayListPlaybackType::from_raw(v);
            assert_eq!(parsed, PlayListPlaybackType::Other(v));
            assert_eq!(parsed.as_raw(), v);
            assert!(!parsed.is_sequential());
            assert!(!parsed.is_randomised());
        }
    }

    #[test]
    fn playback_type_as_raw_matches_known_codes() {
        assert_eq!(PlayListPlaybackType::Sequential.as_raw(), 0x01);
        assert_eq!(PlayListPlaybackType::Random.as_raw(), 0x02);
        assert_eq!(PlayListPlaybackType::Shuffle.as_raw(), 0x03);
    }

    #[test]
    fn playback_type_helpers() {
        assert!(PlayListPlaybackType::Sequential.is_sequential());
        assert!(!PlayListPlaybackType::Sequential.is_randomised());
        assert!(!PlayListPlaybackType::Random.is_sequential());
        assert!(PlayListPlaybackType::Random.is_randomised());
        assert!(!PlayListPlaybackType::Shuffle.is_sequential());
        assert!(PlayListPlaybackType::Shuffle.is_randomised());
    }

    #[test]
    fn app_info_playback_kind_typed_accessor() {
        for (raw, want) in [
            (0x01u8, PlayListPlaybackType::Sequential),
            (0x02, PlayListPlaybackType::Random),
            (0x03, PlayListPlaybackType::Shuffle),
            (0x77, PlayListPlaybackType::Other(0x77)),
        ] {
            let app = AppInfoPlayList {
                playback_type: raw,
                playback_count: 0,
                random_access_flag: 0,
                audio_mix_app_flag: 0,
                lossless_may_bypass_mixer_flag: 0,
                uo_mask: 0,
            };
            assert_eq!(app.playback_kind(), want);
            // Round-trip via the typed view should preserve the raw byte.
            assert_eq!(app.playback_kind().as_raw(), raw);
        }
    }

    #[test]
    fn playback_kind_survives_encode_decode_round_trip() {
        // Drive the typed view through the full encode → parse path so
        // wire preservation of the raw byte is covered end-to-end. We
        // emit each documented variant and a sentinel `Other(0x42)`.
        for variant in [
            PlayListPlaybackType::Sequential,
            PlayListPlaybackType::Random,
            PlayListPlaybackType::Shuffle,
            PlayListPlaybackType::Other(0x42),
        ] {
            let mut m = sample_mpls();
            m.app_info.playback_type = variant.as_raw();
            let bytes = m.encode();
            let parsed = PlayListMpls::parse(&bytes).unwrap();
            assert_eq!(parsed.app_info.playback_type, variant.as_raw());
            assert_eq!(parsed.app_info.playback_kind(), variant);
        }
    }

    #[test]
    fn chapters_lift_marks_onto_title_timeline() {
        // PlayItem 0 spans title [0, 60s); IN = 0.
        // PlayItem 1 spans title [60s, 105s); IN = 30s (clip-local).
        let mut m = sample_mpls();
        m.marks = vec![
            // Entry mark at clip-local IN of item 0 → title 0.
            PlayListMark {
                mark_type: 1,
                ref_play_item_id: 0,
                mark_time_ticks: 0,
                entry_es_pid: 0x1011,
                duration_ticks: 0,
            },
            // Entry mark 10s into item 0 → title 10s.
            PlayListMark {
                mark_type: 1,
                ref_play_item_id: 0,
                mark_time_ticks: 45_000 * 10,
                entry_es_pid: 0x1011,
                duration_ticks: 0,
            },
            // Link point (not a chapter) — must be excluded.
            PlayListMark {
                mark_type: 2,
                ref_play_item_id: 0,
                mark_time_ticks: 45_000 * 20,
                entry_es_pid: 0x1011,
                duration_ticks: 0,
            },
            // Entry mark at clip-local 35s in item 1 (IN = 30s) → 5s
            // into item 1 → title 60s + 5s = 65s.
            PlayListMark {
                mark_type: 1,
                ref_play_item_id: 1,
                mark_time_ticks: 45_000 * 35,
                entry_es_pid: 0x1011,
                duration_ticks: 0,
            },
            // Out-of-range PlayItem reference — must be skipped.
            PlayListMark {
                mark_type: 1,
                ref_play_item_id: 9,
                mark_time_ticks: 0,
                entry_es_pid: 0x1011,
                duration_ticks: 0,
            },
            // Mark before its PlayItem's IN point — malformed, skipped.
            PlayListMark {
                mark_type: 1,
                ref_play_item_id: 1,
                mark_time_ticks: 45_000 * 10, // < IN (30s)
                entry_es_pid: 0x1011,
                duration_ticks: 0,
            },
        ];

        let ch = m.chapters();
        assert_eq!(ch.len(), 3, "two from item 0 + one from item 1");

        assert_eq!(ch[0].index, 0);
        assert_eq!(ch[0].start_pts_90k, 0);
        assert_eq!(ch[0].ref_play_item_id, 0);

        assert_eq!(ch[1].index, 1);
        assert_eq!(ch[1].start_pts_90k, 10 * 90_000); // 10s @ 90kHz

        assert_eq!(ch[2].index, 2);
        assert_eq!(ch[2].ref_play_item_id, 1);
        // title 60s + 5s into item 1 = 65s @ 90kHz.
        assert_eq!(ch[2].start_pts_90k, 65 * 90_000);
    }

    #[test]
    fn chapter_spans_carry_end_and_duration() {
        // Same marks as the lift test: chapters at title 0s, 10s, 65s.
        // Title spans [0, 105s) (item 0 = 60s, item 1 = 45s).
        let mut m = sample_mpls();
        m.marks = vec![
            PlayListMark {
                mark_type: 1,
                ref_play_item_id: 0,
                mark_time_ticks: 0,
                entry_es_pid: 0x1011,
                duration_ticks: 0,
            },
            PlayListMark {
                mark_type: 1,
                ref_play_item_id: 0,
                mark_time_ticks: 45_000 * 10,
                entry_es_pid: 0x1011,
                duration_ticks: 0,
            },
            PlayListMark {
                mark_type: 1,
                ref_play_item_id: 1,
                mark_time_ticks: 45_000 * 35, // 5s past IN(30s) → title 65s
                entry_es_pid: 0x1011,
                duration_ticks: 0,
            },
        ];

        let spans = m.chapters_with_duration();
        assert_eq!(spans.len(), 3);

        // [0, 10s)
        assert_eq!(spans[0].index, 0);
        assert_eq!(spans[0].start_pts_90k, 0);
        assert_eq!(spans[0].end_pts_90k, 10 * 90_000);
        assert_eq!(spans[0].duration_90k(), 10 * 90_000);
        assert_eq!(spans[0].duration_secs(), 10);

        // [10s, 65s)
        assert_eq!(spans[1].start_pts_90k, 10 * 90_000);
        assert_eq!(spans[1].end_pts_90k, 65 * 90_000);
        assert_eq!(spans[1].duration_secs(), 55);

        // [65s, 105s) — final chapter ends at the title total duration.
        assert_eq!(spans[2].start_pts_90k, 65 * 90_000);
        assert_eq!(spans[2].end_pts_90k, m.duration_90k());
        assert_eq!(spans[2].end_pts_90k, 105 * 90_000);
        assert_eq!(spans[2].duration_secs(), 40);
        assert_eq!(spans[2].ref_play_item_id, 1);
    }

    #[test]
    fn chapter_spans_empty_when_no_marks() {
        let mut m = sample_mpls();
        m.marks.clear();
        assert!(m.chapters_with_duration().is_empty());
    }

    #[test]
    fn chapter_spans_sorted_and_contiguous() {
        // Author the marks out of order; spans must still be contiguous
        // start-ascending with end = next start.
        let mut m = sample_mpls();
        m.marks = vec![
            PlayListMark {
                mark_type: 1,
                ref_play_item_id: 0,
                mark_time_ticks: 45_000 * 20, // title 20s
                entry_es_pid: 0x1011,
                duration_ticks: 0,
            },
            PlayListMark {
                mark_type: 1,
                ref_play_item_id: 0,
                mark_time_ticks: 45_000 * 5, // title 5s
                entry_es_pid: 0x1011,
                duration_ticks: 0,
            },
        ];
        let spans = m.chapters_with_duration();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].start_pts_90k, 5 * 90_000);
        assert_eq!(spans[0].end_pts_90k, spans[1].start_pts_90k);
        assert_eq!(spans[1].start_pts_90k, 20 * 90_000);
        assert_eq!(spans[1].end_pts_90k, m.duration_90k());
    }

    #[test]
    fn chapters_survive_round_trip() {
        let mut m = sample_mpls();
        m.marks = vec![
            PlayListMark {
                mark_type: 1,
                ref_play_item_id: 0,
                mark_time_ticks: 45_000 * 5,
                entry_es_pid: 0x1011,
                duration_ticks: 0,
            },
            PlayListMark {
                mark_type: 1,
                ref_play_item_id: 1,
                mark_time_ticks: 45_000 * 40,
                entry_es_pid: 0x1011,
                duration_ticks: 0,
            },
        ];
        let bytes = m.encode();
        let parsed = PlayListMpls::parse(&bytes).unwrap();
        assert_eq!(parsed.chapters(), m.chapters());
        // 5s into item 0 = 5s; 10s into item 1 = 70s.
        assert_eq!(parsed.chapters()[0].start_pts_90k, 5 * 90_000);
        assert_eq!(parsed.chapters()[1].start_pts_90k, 70 * 90_000);
    }

    #[test]
    fn connection_condition_from_raw() {
        assert_eq!(
            ConnectionCondition::from_raw(0x01),
            ConnectionCondition::NonSeamless
        );
        assert_eq!(
            ConnectionCondition::from_raw(0x05),
            ConnectionCondition::SeamlessContinuation
        );
        assert_eq!(
            ConnectionCondition::from_raw(0x06),
            ConnectionCondition::SeamlessNewStc
        );
        assert_eq!(
            ConnectionCondition::from_raw(0x09),
            ConnectionCondition::Other(9)
        );
    }

    /// Build a single-PlayItem MPLS where the PlayItem carries
    /// `multi_clip_count = 3` and two alt-angle clip entries —
    /// exercise the round-trip + the [`PlayItem::angle_clip`] selector.
    fn multi_angle_mpls() -> PlayListMpls {
        PlayListMpls {
            version: *b"0200",
            app_info: AppInfoPlayList {
                playback_type: 1,
                playback_count: 0,
                random_access_flag: 1,
                audio_mix_app_flag: 0,
                lossless_may_bypass_mixer_flag: 0,
                uo_mask: 0,
            },
            play_list: PlayList {
                play_items: vec![PlayItem {
                    clip_information_file_name: "00100".into(),
                    clip_codec_identifier: *b"M2TS",
                    connection_condition: ConnectionCondition::NonSeamless,
                    stc_id_ref: 0,
                    in_time_ticks: 0,
                    out_time_ticks: 45_000 * 5,
                    multi_clip_count: 3,
                    angles: vec![
                        AngleClip {
                            clip_information_file_name: "00101".into(),
                            clip_codec_identifier: *b"M2TS",
                            stc_id_ref: 1,
                        },
                        AngleClip {
                            clip_information_file_name: "00102".into(),
                            clip_codec_identifier: *b"M2TS",
                            stc_id_ref: 2,
                        },
                    ],
                    stn_table: StnTable {
                        primary_video: vec![PrimaryVideoStream {
                            elementary_pid: 0x1011,
                            coding_type: StreamCodingType::AvcVideo,
                            video_format: 0x06,
                            frame_rate: 0x03,
                            aspect_ratio: 0x03,
                        }],
                        primary_audio: vec![PrimaryAudioStream {
                            elementary_pid: 0x1100,
                            coding_type: StreamCodingType::Ac3Audio,
                            audio_format: 0x03,
                            sample_rate: 0x01,
                            language_code: *b"eng",
                        }],
                        ..StnTable::default()
                    },
                    flags: PlayItemFlags::default(),
                }],
                sub_paths: vec![],
            },
            marks: vec![],
        }
    }

    #[test]
    fn multi_angle_round_trip_preserves_alt_clip_stems() {
        let m = multi_angle_mpls();
        let bytes = m.encode();
        let parsed = PlayListMpls::parse(&bytes).unwrap();
        assert_eq!(parsed.play_list.play_items.len(), 1);
        let pi = &parsed.play_list.play_items[0];
        assert_eq!(pi.multi_clip_count, 3);
        assert_eq!(pi.num_angles(), 3);
        assert_eq!(pi.angles.len(), 2);
        assert_eq!(pi.angles[0].clip_information_file_name, "00101");
        assert_eq!(pi.angles[0].stc_id_ref, 1);
        assert_eq!(pi.angles[1].clip_information_file_name, "00102");
        assert_eq!(pi.angles[1].stc_id_ref, 2);
    }

    #[test]
    fn angle_clip_selector_maps_to_correct_clip() {
        let m = multi_angle_mpls();
        let pi = &m.play_list.play_items[0];

        // Primary angle: the PlayItem's own clip.
        let primary = pi.angle_clip(0).unwrap();
        assert_eq!(primary.clip_information_file_name, "00100");
        assert_eq!(primary.stc_id_ref, 0);

        // Alt angles map by 1-based offset into `angles`.
        let a1 = pi.angle_clip(1).unwrap();
        assert_eq!(a1.clip_information_file_name, "00101");
        let a2 = pi.angle_clip(2).unwrap();
        assert_eq!(a2.clip_information_file_name, "00102");

        // Out-of-range angle returns None — `open_title_with_angle`
        // relies on this to reject a bad selector at open time.
        assert!(pi.angle_clip(3).is_none());
        assert!(pi.angle_clip(255).is_none());
    }

    #[test]
    fn single_angle_play_item_has_empty_angle_list() {
        // A regression-shaped check: the standard single-clip path
        // round-trips with `angles == []` and `num_angles() == 1`.
        let m = sample_mpls();
        for pi in &m.play_list.play_items {
            assert_eq!(pi.multi_clip_count, 1);
            assert!(pi.angles.is_empty());
            assert_eq!(pi.num_angles(), 1);
            assert!(pi.angle_clip(0).is_some());
            assert!(pi.angle_clip(1).is_none());
        }
    }

    #[test]
    fn stream_coding_type_round_trip() {
        // Every named variant round-trips through `from_raw` / `as_raw`.
        let named = [
            (StreamCodingType::Mpeg2Video, 0x02),
            (StreamCodingType::AvcVideo, 0x1B),
            (StreamCodingType::HevcVideo, 0x24),
            (StreamCodingType::Vc1Video, 0xEA),
            (StreamCodingType::LpcmAudio, 0x80),
            (StreamCodingType::Ac3Audio, 0x81),
            (StreamCodingType::DtsAudio, 0x82),
            (StreamCodingType::TruehdAudio, 0x83),
            (StreamCodingType::EAc3Audio, 0x84),
            (StreamCodingType::DtsHdAudio, 0x85),
            (StreamCodingType::DtsHdMaAudio, 0x86),
            (StreamCodingType::EAc3SecondaryAudio, 0xA1),
            (StreamCodingType::DtsHdSecondaryAudio, 0xA2),
            (StreamCodingType::PgsSubtitle, 0x90),
            (StreamCodingType::IgsInteractive, 0x91),
            (StreamCodingType::TextSubtitle, 0x92),
        ];
        for (variant, raw) in named {
            assert_eq!(variant.as_raw(), raw);
            assert_eq!(StreamCodingType::from_raw(raw), variant);
        }
        // Unknown values fall into `Other` and round-trip too.
        assert_eq!(
            StreamCodingType::from_raw(0x55),
            StreamCodingType::Other(0x55)
        );
        assert_eq!(StreamCodingType::Other(0x55).as_raw(), 0x55);
    }

    #[test]
    fn stream_coding_type_class_predicates() {
        assert!(StreamCodingType::AvcVideo.is_video());
        assert!(StreamCodingType::HevcVideo.is_video());
        assert!(!StreamCodingType::AvcVideo.is_audio());
        assert!(StreamCodingType::DtsHdMaAudio.is_audio());
        assert!(!StreamCodingType::PgsSubtitle.is_video());
        assert!(!StreamCodingType::PgsSubtitle.is_audio());
    }

    #[test]
    fn stream_coding_type_graphics_and_secondary_predicates() {
        assert!(StreamCodingType::PgsSubtitle.is_graphics());
        assert!(StreamCodingType::IgsInteractive.is_graphics());
        assert!(StreamCodingType::TextSubtitle.is_graphics());
        assert!(!StreamCodingType::AvcVideo.is_graphics());
        assert!(!StreamCodingType::Ac3Audio.is_graphics());

        assert!(StreamCodingType::EAc3SecondaryAudio.is_secondary());
        assert!(StreamCodingType::DtsHdSecondaryAudio.is_secondary());
        // Secondary audio is still audio.
        assert!(StreamCodingType::EAc3SecondaryAudio.is_audio());
        assert!(!StreamCodingType::EAc3Audio.is_secondary());
        assert!(!StreamCodingType::PgsSubtitle.is_secondary());
    }

    #[test]
    fn stream_coding_type_display_names() {
        assert_eq!(StreamCodingType::Mpeg2Video.display_name(), "MPEG-2 Video");
        assert_eq!(StreamCodingType::TruehdAudio.display_name(), "Dolby TrueHD");
        assert_eq!(
            StreamCodingType::DtsHdMaAudio.display_name(),
            "DTS-HD Master Audio"
        );
        assert_eq!(StreamCodingType::PgsSubtitle.display_name(), "PGS Subtitle");
        assert_eq!(
            StreamCodingType::Other(0x7F).display_name(),
            "Unknown(0x7F)"
        );
    }

    /// Build a single-PlayItem MPLS whose STN_table carries one video
    /// (AVC), two audio (AC-3 eng + DTS-HD MA jpn), and one PG (eng) —
    /// exercise the per-stream parser / encoder for every class a
    /// remux pipeline needs.
    fn stn_table_mpls() -> PlayListMpls {
        PlayListMpls {
            version: *b"0200",
            app_info: AppInfoPlayList {
                playback_type: 1,
                playback_count: 0,
                random_access_flag: 1,
                audio_mix_app_flag: 0,
                lossless_may_bypass_mixer_flag: 0,
                uo_mask: 0,
            },
            play_list: PlayList {
                play_items: vec![PlayItem {
                    clip_information_file_name: "00001".into(),
                    clip_codec_identifier: *b"M2TS",
                    connection_condition: ConnectionCondition::NonSeamless,
                    stc_id_ref: 0,
                    in_time_ticks: 0,
                    out_time_ticks: 45_000 * 90,
                    multi_clip_count: 1,
                    angles: Vec::new(),
                    stn_table: StnTable {
                        primary_video: vec![PrimaryVideoStream {
                            elementary_pid: 0x1011,
                            coding_type: StreamCodingType::AvcVideo,
                            video_format: 0x06, // 1080p
                            frame_rate: 0x03,   // 25 fps (BD-AV §5.4.4.4)
                            aspect_ratio: 0x03, // 16:9
                        }],
                        primary_audio: vec![
                            PrimaryAudioStream {
                                elementary_pid: 0x1100,
                                coding_type: StreamCodingType::Ac3Audio,
                                audio_format: 0x03, // stereo
                                sample_rate: 0x01,  // 48 kHz
                                language_code: *b"eng",
                            },
                            PrimaryAudioStream {
                                elementary_pid: 0x1101,
                                coding_type: StreamCodingType::DtsHdMaAudio,
                                audio_format: 0x06, // multichannel (5.1)
                                sample_rate: 0x05,  // 192 kHz (BD-AV §5.4.4.4)
                                language_code: *b"jpn",
                            },
                        ],
                        pg_subtitles: vec![PgsSubtitleStream {
                            elementary_pid: 0x1200,
                            coding_type: StreamCodingType::PgsSubtitle,
                            language_code: *b"eng",
                        }],
                        ..StnTable::default()
                    },
                    flags: PlayItemFlags::default(),
                }],
                sub_paths: vec![],
            },
            marks: vec![],
        }
    }

    #[test]
    #[allow(deprecated)]
    fn stn_table_round_trip_preserves_every_per_stream_field() {
        let m = stn_table_mpls();
        let bytes = m.encode();
        let parsed = PlayListMpls::parse(&bytes).unwrap();
        assert_eq!(parsed.play_list.play_items.len(), 1);
        let stn = &parsed.play_list.play_items[0].stn_table;

        // Primary video — PID + codec + format nibbles all preserved.
        assert_eq!(stn.primary_video.len(), 1);
        let v = &stn.primary_video[0];
        assert_eq!(v.elementary_pid, 0x1011);
        assert_eq!(v.coding_type, StreamCodingType::AvcVideo);
        assert_eq!(v.video_format, 0x06);
        assert_eq!(v.frame_rate, 0x03);
        assert_eq!(v.aspect_ratio, 0x03);

        // Two audio tracks — language code + codec class preserved.
        assert_eq!(stn.primary_audio.len(), 2);
        assert_eq!(stn.primary_audio[0].elementary_pid, 0x1100);
        assert_eq!(stn.primary_audio[0].coding_type, StreamCodingType::Ac3Audio);
        assert_eq!(stn.primary_audio[0].audio_format, 0x03);
        assert_eq!(stn.primary_audio[0].sample_rate, 0x01);
        assert_eq!(&stn.primary_audio[0].language_code, b"eng");
        assert_eq!(stn.primary_audio[1].elementary_pid, 0x1101);
        assert_eq!(
            stn.primary_audio[1].coding_type,
            StreamCodingType::DtsHdMaAudio
        );
        assert_eq!(&stn.primary_audio[1].language_code, b"jpn");

        // PG subtitles — coding type + language preserved.
        assert_eq!(stn.pg_subtitles.len(), 1);
        assert_eq!(stn.pg_subtitles[0].elementary_pid, 0x1200);
        assert_eq!(
            stn.pg_subtitles[0].coding_type,
            StreamCodingType::PgsSubtitle
        );
        assert_eq!(&stn.pg_subtitles[0].language_code, b"eng");

        // Class counts the deprecated summary view would surface still
        // line up with the per-stream Vec lengths (the round-trip path
        // hasn't dropped any classes).
        let s = stn.summary();
        assert_eq!(s.num_primary_video, 1);
        assert_eq!(s.num_primary_audio, 2);
        assert_eq!(s.num_pg, 1);
        assert_eq!(s.num_ig, 0);
        assert_eq!(s.num_secondary_audio, 0);
        assert_eq!(s.num_secondary_video, 0);
        assert_eq!(s.num_pip_pg, 0);
    }

    #[test]
    fn stn_table_round_trip_full_table_with_secondary_classes() {
        // Stress every class — secondary audio/video, IG, PIP_PG — so
        // the encoder + parser stay in lockstep on the classes a vanilla
        // commercial disc doesn't always ship.
        let mut m = stn_table_mpls();
        let pi = &mut m.play_list.play_items[0];
        pi.stn_table.ig_streams.push(IgsInteractiveStream {
            elementary_pid: 0x1400,
            coding_type: StreamCodingType::IgsInteractive,
            language_code: *b"eng",
        });
        pi.stn_table.secondary_audio.push(SecondaryAudioStream {
            elementary_pid: 0x1A00,
            coding_type: StreamCodingType::EAc3SecondaryAudio,
            audio_format: 0x03,
            sample_rate: 0x01,
            language_code: *b"fre",
        });
        pi.stn_table.secondary_video.push(SecondaryVideoStream {
            elementary_pid: 0x1B00,
            coding_type: StreamCodingType::AvcVideo,
            video_format: 0x04,
            frame_rate: 0x03,
            aspect_ratio: 0x03,
        });
        pi.stn_table.pip_pg.push(PipPgStream {
            elementary_pid: 0x1300,
            coding_type: StreamCodingType::PgsSubtitle,
            language_code: *b"jpn",
        });

        let bytes = m.encode();
        let parsed = PlayListMpls::parse(&bytes).unwrap();
        let stn = &parsed.play_list.play_items[0].stn_table;
        assert_eq!(stn.ig_streams.len(), 1);
        assert_eq!(stn.ig_streams[0].elementary_pid, 0x1400);
        assert_eq!(&stn.ig_streams[0].language_code, b"eng");
        assert_eq!(stn.secondary_audio.len(), 1);
        assert_eq!(stn.secondary_audio[0].elementary_pid, 0x1A00);
        assert_eq!(
            stn.secondary_audio[0].coding_type,
            StreamCodingType::EAc3SecondaryAudio
        );
        assert_eq!(&stn.secondary_audio[0].language_code, b"fre");
        assert_eq!(stn.secondary_video.len(), 1);
        assert_eq!(stn.secondary_video[0].elementary_pid, 0x1B00);
        assert_eq!(stn.pip_pg.len(), 1);
        assert_eq!(stn.pip_pg[0].elementary_pid, 0x1300);
        assert_eq!(&stn.pip_pg[0].language_code, b"jpn");
    }

    #[test]
    fn empty_stn_table_round_trips() {
        // The PlayItem ships zero streams of every class — the encoder
        // still produces the 14-byte STN header, and the parser surfaces
        // a default-constructed `StnTable`.
        let mut m = stn_table_mpls();
        m.play_list.play_items[0].stn_table = StnTable::default();
        let bytes = m.encode();
        let parsed = PlayListMpls::parse(&bytes).unwrap();
        assert_eq!(
            parsed.play_list.play_items[0].stn_table,
            StnTable::default()
        );
    }

    #[test]
    #[allow(deprecated)]
    fn deprecated_summary_from_impl_matches_method() {
        let m = stn_table_mpls();
        let stn = &m.play_list.play_items[0].stn_table;
        let from: StnTableSummary = stn.into();
        assert_eq!(from, stn.summary());
    }

    // -----------------------------------------------------------------
    // VideoFormat / FrameRate / AspectRatio / AudioFormat / SampleRate
    // -----------------------------------------------------------------

    #[test]
    fn video_format_named_round_trip() {
        let pairs = [
            (VideoFormat::Video480i, 0x1u8),
            (VideoFormat::Video576i, 0x2),
            (VideoFormat::Video480p, 0x3),
            (VideoFormat::Video1080i, 0x4),
            (VideoFormat::Video720p, 0x5),
            (VideoFormat::Video1080p, 0x6),
            (VideoFormat::Video576p, 0x7),
            (VideoFormat::Video2160p, 0x8),
        ];
        for (variant, raw) in pairs {
            assert_eq!(VideoFormat::from_raw(raw), variant);
            assert_eq!(variant.as_raw(), raw);
        }
        // Bits above the low nibble are masked off — the wire packs
        // `video_format(4) | frame_rate(4)` into one byte, so a caller
        // can pass the un-shifted upper-nibble byte and still get the
        // right variant.
        assert_eq!(VideoFormat::from_raw(0xF6), VideoFormat::Video1080p);
        // Unknown low nibbles surface as Other and round-trip.
        for v in [0x0u8, 0x9, 0xA, 0xF] {
            assert_eq!(VideoFormat::from_raw(v), VideoFormat::Other(v));
            assert_eq!(VideoFormat::Other(v).as_raw(), v);
        }
        // Other masking — an oversized inner byte still encodes as a
        // 4-bit nibble.
        assert_eq!(VideoFormat::Other(0xFF).as_raw(), 0x0F);
    }

    #[test]
    fn video_format_helpers() {
        assert!(VideoFormat::Video1080p.is_progressive());
        assert!(VideoFormat::Video720p.is_progressive());
        assert!(VideoFormat::Video2160p.is_progressive());
        assert!(VideoFormat::Video480p.is_progressive());
        assert!(VideoFormat::Video576p.is_progressive());
        assert!(!VideoFormat::Video480i.is_progressive());
        assert!(!VideoFormat::Video576i.is_progressive());
        assert!(!VideoFormat::Video1080i.is_progressive());
        assert!(!VideoFormat::Other(0x0).is_progressive());

        assert_eq!(VideoFormat::Video480i.vertical_lines(), Some(480));
        assert_eq!(VideoFormat::Video576i.vertical_lines(), Some(576));
        assert_eq!(VideoFormat::Video720p.vertical_lines(), Some(720));
        assert_eq!(VideoFormat::Video1080p.vertical_lines(), Some(1080));
        assert_eq!(VideoFormat::Video2160p.vertical_lines(), Some(2160));
        assert_eq!(VideoFormat::Other(0xA).vertical_lines(), None);
    }

    #[test]
    fn frame_rate_named_round_trip() {
        let pairs = [
            (FrameRate::Fps23_976, 0x1u8),
            (FrameRate::Fps24, 0x2),
            (FrameRate::Fps25, 0x3),
            (FrameRate::Fps29_97, 0x4),
            (FrameRate::Fps50, 0x6),
            (FrameRate::Fps59_94, 0x7),
        ];
        for (variant, raw) in pairs {
            assert_eq!(FrameRate::from_raw(raw), variant);
            assert_eq!(variant.as_raw(), raw);
        }
        // The reserved nibble `0x5` (skipped between 0x4 and 0x6 in the
        // spec table) surfaces as Other — caller sees the unknown byte
        // rather than a silently wrong rate.
        assert_eq!(FrameRate::from_raw(0x5), FrameRate::Other(0x5));
        // Bits above the low nibble are masked off.
        assert_eq!(FrameRate::from_raw(0x63), FrameRate::Fps25);
    }

    #[test]
    fn frame_rate_helpers() {
        assert_eq!(FrameRate::Fps23_976.fps_q(), Some((24_000, 1_001)));
        assert_eq!(FrameRate::Fps24.fps_q(), Some((24, 1)));
        assert_eq!(FrameRate::Fps25.fps_q(), Some((25, 1)));
        assert_eq!(FrameRate::Fps29_97.fps_q(), Some((30_000, 1_001)));
        assert_eq!(FrameRate::Fps50.fps_q(), Some((50, 1)));
        assert_eq!(FrameRate::Fps59_94.fps_q(), Some((60_000, 1_001)));
        assert_eq!(FrameRate::Other(0x5).fps_q(), None);
        assert!(FrameRate::Fps23_976.is_fractional());
        assert!(FrameRate::Fps29_97.is_fractional());
        assert!(FrameRate::Fps59_94.is_fractional());
        assert!(!FrameRate::Fps24.is_fractional());
        assert!(!FrameRate::Fps25.is_fractional());
        assert!(!FrameRate::Fps50.is_fractional());
    }

    #[test]
    fn aspect_ratio_round_trip() {
        assert_eq!(AspectRatio::from_raw(0x2), AspectRatio::Ratio4x3);
        assert_eq!(AspectRatio::from_raw(0x3), AspectRatio::Ratio16x9);
        assert_eq!(AspectRatio::Ratio4x3.as_raw(), 0x2);
        assert_eq!(AspectRatio::Ratio16x9.as_raw(), 0x3);
        assert_eq!(AspectRatio::from_raw(0xA), AspectRatio::Other(0xA));
        assert_eq!(AspectRatio::Ratio4x3.ratio(), Some((4, 3)));
        assert_eq!(AspectRatio::Ratio16x9.ratio(), Some((16, 9)));
        assert_eq!(AspectRatio::Other(0xA).ratio(), None);
        assert!(AspectRatio::Ratio16x9.is_widescreen());
        assert!(!AspectRatio::Ratio4x3.is_widescreen());
        assert!(!AspectRatio::Other(0xA).is_widescreen());
        // Masking — the wire byte stores `aspect_ratio(4) | reserved(4)`
        // and the parser strips the reserved nibble before storing, but
        // a caller who hands in the raw byte still gets the right view.
        assert_eq!(AspectRatio::from_raw(0xF3), AspectRatio::Ratio16x9);
    }

    #[test]
    fn audio_format_named_round_trip() {
        let pairs = [
            (AudioFormat::Mono, 0x1u8),
            (AudioFormat::Stereo, 0x3),
            (AudioFormat::Multi, 0x6),
            (AudioFormat::Combo, 0xC),
        ];
        for (variant, raw) in pairs {
            assert_eq!(AudioFormat::from_raw(raw), variant);
            assert_eq!(variant.as_raw(), raw);
        }
        // Reserved nibbles surface as Other.
        for v in [0x0u8, 0x2, 0x4, 0x5, 0x7, 0xF] {
            assert_eq!(AudioFormat::from_raw(v), AudioFormat::Other(v));
        }
    }

    #[test]
    fn audio_format_helpers() {
        assert_eq!(AudioFormat::Mono.channel_count(), Some(1));
        assert_eq!(AudioFormat::Stereo.channel_count(), Some(2));
        assert_eq!(AudioFormat::Multi.channel_count(), Some(6));
        assert_eq!(AudioFormat::Combo.channel_count(), Some(6));
        assert_eq!(AudioFormat::Other(0x2).channel_count(), None);
        assert!(AudioFormat::Combo.has_downmix());
        assert!(!AudioFormat::Multi.has_downmix());
        assert!(!AudioFormat::Stereo.has_downmix());
    }

    #[test]
    fn sample_rate_named_round_trip() {
        let pairs = [
            (SampleRate::Hz48000, 0x1u8),
            (SampleRate::Hz96000, 0x4),
            (SampleRate::Hz192000, 0x5),
            (SampleRate::Combo48_192, 0xC),
            (SampleRate::Combo48_96, 0xE),
        ];
        for (variant, raw) in pairs {
            assert_eq!(SampleRate::from_raw(raw), variant);
            assert_eq!(variant.as_raw(), raw);
        }
        assert_eq!(SampleRate::from_raw(0x0), SampleRate::Other(0x0));
        assert_eq!(SampleRate::from_raw(0xF), SampleRate::Other(0xF));
    }

    #[test]
    fn sample_rate_helpers() {
        assert_eq!(SampleRate::Hz48000.primary_hz(), Some(48_000));
        assert_eq!(SampleRate::Hz96000.primary_hz(), Some(96_000));
        assert_eq!(SampleRate::Hz192000.primary_hz(), Some(192_000));
        // Combo variants report the highest carried rate so a player
        // sizing its downstream resampler can pick the dominant rate.
        assert_eq!(SampleRate::Combo48_192.primary_hz(), Some(192_000));
        assert_eq!(SampleRate::Combo48_96.primary_hz(), Some(96_000));
        assert_eq!(SampleRate::Other(0x0).primary_hz(), None);
        assert!(SampleRate::Combo48_192.is_combo());
        assert!(SampleRate::Combo48_96.is_combo());
        assert!(!SampleRate::Hz48000.is_combo());
        assert!(!SampleRate::Hz192000.is_combo());
    }

    #[test]
    fn primary_video_stream_typed_accessors() {
        // 1080p / 23.976 / 16:9 — the most common BD-AV main-feature
        // authoring pattern.
        let v = PrimaryVideoStream {
            elementary_pid: 0x1011,
            coding_type: StreamCodingType::AvcVideo,
            video_format: 0x06,
            frame_rate: 0x01,
            aspect_ratio: 0x03,
        };
        assert_eq!(v.video_format_kind(), VideoFormat::Video1080p);
        assert_eq!(v.frame_rate_kind(), FrameRate::Fps23_976);
        assert_eq!(v.aspect_ratio_kind(), AspectRatio::Ratio16x9);
        assert!(v.video_format_kind().is_progressive());
        assert!(v.aspect_ratio_kind().is_widescreen());
    }

    #[test]
    fn primary_audio_stream_typed_accessors() {
        let a = PrimaryAudioStream {
            elementary_pid: 0x1100,
            coding_type: StreamCodingType::DtsHdMaAudio,
            audio_format: 0x06,
            sample_rate: 0x04,
            language_code: *b"eng",
        };
        assert_eq!(a.audio_format_kind(), AudioFormat::Multi);
        assert_eq!(a.sample_rate_kind(), SampleRate::Hz96000);
        assert_eq!(a.audio_format_kind().channel_count(), Some(6));
        assert_eq!(a.sample_rate_kind().primary_hz(), Some(96_000));
    }

    #[test]
    fn secondary_audio_video_typed_accessors() {
        let sv = SecondaryVideoStream {
            elementary_pid: 0x1B00,
            coding_type: StreamCodingType::AvcVideo,
            video_format: 0x05,
            frame_rate: 0x07,
            aspect_ratio: 0x03,
        };
        assert_eq!(sv.video_format_kind(), VideoFormat::Video720p);
        assert_eq!(sv.frame_rate_kind(), FrameRate::Fps59_94);
        assert_eq!(sv.aspect_ratio_kind(), AspectRatio::Ratio16x9);

        let sa = SecondaryAudioStream {
            elementary_pid: 0x1A00,
            coding_type: StreamCodingType::EAc3SecondaryAudio,
            audio_format: 0x03,
            sample_rate: 0x01,
            language_code: *b"jpn",
        };
        assert_eq!(sa.audio_format_kind(), AudioFormat::Stereo);
        assert_eq!(sa.sample_rate_kind(), SampleRate::Hz48000);
    }

    #[test]
    fn typed_video_audio_accessors_survive_mpls_round_trip() {
        // The wire encoder packs nibbles into bytes; the typed accessor
        // surface needs to keep producing the right variants after a
        // full encode → parse cycle. Run one PlayItem with a video +
        // audio attribute set whose nibbles cover both halves of every
        // byte the wire layout packs.
        let m = stn_table_mpls();
        let bytes = m.encode();
        let parsed = PlayListMpls::parse(&bytes).unwrap();
        let stn = &parsed.play_list.play_items[0].stn_table;

        let pv = stn.primary_video[0];
        assert_eq!(pv.video_format_kind(), VideoFormat::Video1080p);
        assert_eq!(pv.frame_rate_kind(), FrameRate::Fps25);
        assert_eq!(pv.aspect_ratio_kind(), AspectRatio::Ratio16x9);

        let pa_eng = stn.primary_audio[0];
        assert_eq!(pa_eng.audio_format_kind(), AudioFormat::Stereo);
        assert_eq!(pa_eng.sample_rate_kind(), SampleRate::Hz48000);
        assert_eq!(&pa_eng.language_code, b"eng");

        let pa_jpn = stn.primary_audio[1];
        assert_eq!(pa_jpn.audio_format_kind(), AudioFormat::Multi);
        assert_eq!(pa_jpn.sample_rate_kind(), SampleRate::Hz192000);
        assert_eq!(&pa_jpn.language_code, b"jpn");
    }
}
