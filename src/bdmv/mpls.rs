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
    pub num_sub_play_items: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppInfoPlayList {
    /// 1 = sequential, 2 = random, 3 = shuffle.
    pub playback_type: u8,
    pub playback_count: u16,
    pub random_access_flag: u8,
    pub audio_mix_app_flag: u8,
    pub lossless_may_bypass_mixer_flag: u8,
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
        r.skip(8)?; // UO_mask_table
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
        out.extend_from_slice(&[0u8; 8]); // UO_mask_table
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
    // UO_mask_table 8 bytes
    r.skip(8)?;
    // random_access_flag 1 bit + reserved 7 bits
    r.skip(1)?;
    // still_mode 1 byte + still_time u16
    r.skip(3)?;
    let (multi_clip_count, angles) = if is_multi_angle != 0 {
        let num_angles = r.read_u8()?;
        // flags byte
        r.skip(1)?;
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
            // simplified layout (same as primary audio) that matches
            // libbluray's clean-room read path. Anything trailing is
            // skipped by the stream_attributes length envelope.
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
    out.extend_from_slice(&[0u8; 8]); // UO_mask_table
    out.push(0); // random_access_flag + reserved
    out.push(0); // still_mode
    out.extend_from_slice(&[0u8; 2]); // still_time

    if pi.multi_clip_count > 1 {
        out.push(pi.multi_clip_count);
        out.push(0);
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
    // 15 reserved bits + is_repeat_SubPath
    r.skip(2)?;
    r.skip(1)?; // reserved
    let num_sub_play_items = r.read_u8()? as u16;
    r.seek(body_end)?;
    Ok(SubPath {
        sub_path_type,
        num_sub_play_items,
    })
}

fn encode_sub_path(out: &mut Vec<u8>, sp: &SubPath) {
    let len_off = out.len();
    out.extend_from_slice(&[0u8; 4]);
    let body_start = out.len();
    out.push(0); // reserved
    out.push(sp.sub_path_type);
    out.extend_from_slice(&[0u8; 2]); // 15 reserved + is_repeat
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
                    },
                ],
                sub_paths: vec![SubPath {
                    sub_path_type: 5,
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
                            video_format: 0x06, // 1080
                            frame_rate: 0x03,   // 24000/1001
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
                                audio_format: 0x06, // multichannel
                                sample_rate: 0x05,  // 96 kHz combo
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
}
