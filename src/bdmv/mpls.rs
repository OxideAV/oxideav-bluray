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

/// Lightweight summary of an STN_table — Phase 1 only needs the
/// stream-type counts and the elementary PIDs so the demuxer knows
/// what's on the wire.
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
    pub stn_table: StnTableSummary,
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

    // STN_table() — read length + skip body (we summarise from the
    // counts inside).
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
        // 5 reserved bytes — already consumed in fixed-prefix accounting.
        // We don't parse the per-stream entries; just leap to body end.
        StnTableSummary {
            num_primary_video,
            num_primary_audio,
            num_pg,
            num_ig,
            num_secondary_audio,
            num_secondary_video,
            num_pip_pg,
        }
    } else {
        StnTableSummary::default()
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

    // STN_table() — write a 14-byte fixed body so we can round-trip
    // the summary counts.
    let stn_body_len: u16 = 14;
    out.extend_from_slice(&stn_body_len.to_be_bytes());
    out.extend_from_slice(&[0u8; 2]); // 2 reserved
    out.push(pi.stn_table.num_primary_video);
    out.push(pi.stn_table.num_primary_audio);
    out.push(pi.stn_table.num_pg);
    out.push(pi.stn_table.num_ig);
    out.push(pi.stn_table.num_secondary_audio);
    out.push(pi.stn_table.num_secondary_video);
    out.push(pi.stn_table.num_pip_pg);
    out.extend_from_slice(&[0u8; 5]); // 5 reserved (fills 14 total)

    let body_len = (out.len() - body_start) as u16;
    out[len_off..len_off + 2].copy_from_slice(&body_len.to_be_bytes());
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
                        stn_table: StnTableSummary {
                            num_primary_video: 1,
                            num_primary_audio: 1,
                            ..StnTableSummary::default()
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
                        stn_table: StnTableSummary {
                            num_primary_video: 1,
                            num_primary_audio: 2,
                            num_pg: 1,
                            ..StnTableSummary::default()
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
                    stn_table: StnTableSummary {
                        num_primary_video: 1,
                        num_primary_audio: 1,
                        ..StnTableSummary::default()
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
}
