//! High-level `Disc` API: BDMV walk, title enumeration, and
//! `TitleSource` streaming.
//!
//! The mount path is:
//!
//! 1. The caller passes a directory containing `BDMV/` (the BDMV root)
//!    or a path to a block device.
//! 2. We read `BDMV/index.bdmv` to enumerate titles.
//! 3. For each HDMV title we resolve the referenced
//!    `BDMV/MovieObject.bdmv` movie object to find its initial PlayList
//!    id and parse the corresponding `PLAYLIST/NNNNN.mpls`.
//! 4. `Disc::open_title` streams the title's PlayItems back-to-back
//!    out of `BDMV/STREAM/NNNNN.m2ts`, stripping the 4-byte BDAV
//!    TP_extra header so the consumer sees a clean MPEG-TS byte
//!    stream.
//!
//! Phase 1 limitation: this path uses the *filesystem* view of the
//! disc (mounted directory). The raw-UDF code path in [`crate::udf`]
//! is wired up + tested but the high-level `Disc::mount` only takes
//! a filesystem root. A future `Disc::mount_image` will accept a
//! block-device reader and route through `UdfDisc::open`.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::bdmv::clpi::ClipInformation;
use crate::bdmv::index_bdmv::{IndexBdmv, IndexEntry, IndexObjectType};
use crate::bdmv::mpls::{
    Chapter, ChapterSpan, ConnectionCondition, PlayItem, PlayListMpls, StreamCodingType, SubPath,
};
use crate::decrypt::{StreamDecryptor, AACS_UNIT_LEN};
use crate::error::{BlurayError, Result};
use crate::m2ts::{strip_tp_extra, M2TS_PACKET_LEN, TS_PACKET_LEN};
use crate::source::ChapterSelector;

/// HDMV vs BD-J title classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleKind {
    Hdmv,
    BdJ,
}

/// One playable title.
#[derive(Debug, Clone)]
pub struct TitleInfo {
    /// 1-based disc index.
    pub id: u16,
    pub kind: TitleKind,
    /// The first PlayList associated with the title's entry object.
    /// For HDMV this is resolved from the referenced MovieObject's
    /// first `PLAY_PL` (PlayList) navigation command; if no such
    /// command can be resolved we fall back to PLAYLIST id 0.
    pub playlist_id: u16,
    /// Total duration in 90 kHz ticks.
    pub duration_ticks: u64,
    /// Sorted, deduplicated 3-letter ISO 639-2/T language tags found
    /// across the playlist's STN_table audio + subtitle entries
    /// (BD-ROM Part 3 §5.4.4.4). Empty when the playlist is missing /
    /// unreadable or when every audio + subtitle stream's
    /// `language_code` is the spec sentinel `b"\0\0\0"`. Use
    /// [`Disc::title_streams`] for the full per-track listing.
    pub languages: Vec<String>,
}

/// A mounted BD-ROM (filesystem view).
#[derive(Debug)]
pub struct Disc {
    root: PathBuf,
    titles: Vec<TitleInfo>,
}

impl Disc {
    /// Mount a BD-ROM at `disc_root`. `disc_root` must be a directory
    /// containing a `BDMV/` subdirectory.
    pub fn mount(disc_root: impl AsRef<Path>) -> Result<Self> {
        let root = disc_root.as_ref().to_path_buf();
        let bdmv = root.join("BDMV");
        if !bdmv.is_dir() {
            return Err(BlurayError::not_bluray(format!(
                "no BDMV/ subdirectory at {}",
                root.display()
            )));
        }

        // Parse index.bdmv.
        let index_bytes = read_file(&bdmv.join("index.bdmv"))?;
        let index = IndexBdmv::parse(&index_bytes)?;

        // For each title, resolve the PlayList id + duration + the
        // sorted-unique language tag list. Each .mpls is read at most
        // once: a successful parse hands the duration AND the language
        // catalogue back via a tuple; a parse failure surfaces (0, [])
        // rather than failing the whole mount, since some titles are
        // intentionally empty placeholders for chapters / menus.
        let mut titles = Vec::with_capacity(index.titles.len());
        for (i, entry) in index.titles.iter().enumerate() {
            let id = (i + 1) as u16;
            let (kind, playlist_id) = resolve_title_playlist(&bdmv, entry)?;
            let (duration_ticks, languages) = read_file(&playlist_path(&bdmv, playlist_id))
                .and_then(|b| {
                    let pl = PlayListMpls::parse(&b)?;
                    Ok((pl.duration_90k(), collect_languages(&pl)))
                })
                .unwrap_or((0, Vec::new()));
            titles.push(TitleInfo {
                id,
                kind,
                playlist_id,
                duration_ticks,
                languages,
            });
        }

        Ok(Self { root, titles })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn titles(&self) -> &[TitleInfo] {
        &self.titles
    }

    /// Best-effort UDF volume label, decoded from the Primary Volume
    /// Descriptor's `volume_identifier` d-string (ECMA-167 §10.1) when
    /// `disc_root` happens to be a raw UDF image / block device.
    ///
    /// Most callers reach `Disc` via [`Self::mount`], which today only
    /// accepts a *mounted filesystem* directory (the BDMV tree already
    /// exposed by the OS). For those mounts the volume identifier lives
    /// in the underlying block device's PVD, which the filesystem
    /// driver has already consumed and discarded — there's nothing left
    /// at `self.root` for us to parse, and this method returns `None`.
    ///
    /// When `self.root` *is* a regular file (an `.iso` / `.img` UDF
    /// image), we open it, walk AVDP → main VDS → PVD, and return the
    /// decoded label. Any I/O or parse error fails over to `None` so a
    /// downstream muxer can fall back to a "untitled disc" label rather
    /// than aborting.
    pub fn volume_label(&self) -> Option<String> {
        let f = File::open(&self.root).ok()?;
        crate::udf::read_volume_label(f).ok()
    }

    /// Pick the longest HDMV title. BD-J titles are skipped because
    /// Phase 1 cannot execute their navigation script.
    pub fn longest_title(&self) -> Option<&TitleInfo> {
        self.titles
            .iter()
            .filter(|t| t.kind == TitleKind::Hdmv)
            .max_by_key(|t| t.duration_ticks)
    }

    /// Deduplicated title list — one entry per unique `playlist_id`,
    /// preserving the lowest-id title that points at each playlist and
    /// dropping placeholder entries.
    ///
    /// Commercial Blu-ray discs routinely ship many `TitleInfo`
    /// entries pointing at the same `.mpls` (anti-rip / menu
    /// navigation artifacts: the "feature" title, the "language
    /// switch" alias, the BD-J entry-point variant, all referencing
    /// the same playlist). A remux pipeline wants exactly one entry
    /// per distinct piece of content; this method returns that view.
    ///
    /// Skips:
    ///
    /// - `playlist_id == 0x4000` — BD-ROM Part 3 §5.2.2 reserves this
    ///   value for "no-op" titles used for BD-J / HDMV menu wiring;
    ///   it never references real playable content.
    ///
    /// Order is by ascending title id, so iterating the result yields
    /// content in disc-author intent order.
    pub fn unique_titles(&self) -> Vec<&TitleInfo> {
        let mut seen: std::collections::HashSet<u16> = std::collections::HashSet::new();
        let mut out = Vec::new();
        for t in &self.titles {
            // BD-ROM Part 3 §5.2.2 reserves playlist_id 0x4000 for
            // "placeholder" titles that wire BD-J / menu nav and never
            // point at real content. Skip them — they're guaranteed to
            // have duration 0 anyway and would clutter a remux selector.
            if t.playlist_id == 0x4000 {
                continue;
            }
            if seen.insert(t.playlist_id) {
                out.push(t);
            }
        }
        out
    }

    /// Disc-level title metadata pulled from
    /// `BDMV/META/DL/bdmt_<lang>.xml` (BD-ROM Part 3 §5.7). Returns
    /// `None` when the META directory is absent, empty, or unreadable —
    /// which is the common case (e.g. the Kite Uncut BD this work
    /// targets ships an empty `META/` directory; most commercial discs
    /// don't author META at all).
    ///
    /// Pulled as a `<di:name>` byte-scan from the first XML file found
    /// under `META/DL/`. The standard library has no XML parser and the
    /// crate intentionally keeps its dep tree tiny; this is a 30-line
    /// regex-free byte scan that's robust against the namespace prefix
    /// variations actually used in the wild (`<di:name>`, `<name>`).
    ///
    /// The returned [`DiscTitleMeta::language`] is the 3-letter ISO
    /// 639-2/T tag carried in the filename suffix (`bdmt_eng.xml` →
    /// `Some("eng")`).
    pub fn title_meta(&self) -> Option<DiscTitleMeta> {
        let meta_dir = self.root.join("BDMV").join("META").join("DL");
        let entries = std::fs::read_dir(&meta_dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name()?.to_str()?.to_string();
            // BD-ROM names META XML files `bdmt_<lang>.xml` per
            // §5.7.4 — match case-insensitively because authoring
            // tools have shipped both spellings.
            let lower = name.to_ascii_lowercase();
            if !lower.starts_with("bdmt_") || !lower.ends_with(".xml") {
                continue;
            }
            let language = lower
                .strip_prefix("bdmt_")
                .and_then(|s| s.strip_suffix(".xml"))
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            let bytes = std::fs::read(&path).ok()?;
            if let Some(title) = extract_di_name(&bytes) {
                return Some(DiscTitleMeta { title, language });
            }
        }
        None
    }

    /// Open a title as a [`TitleSource`] on the primary angle. The
    /// optional `decryptor` lets an AACS adapter plug in; pass `None`
    /// for unprotected homemade discs and for the crate's own tests.
    ///
    /// Equivalent to [`Self::open_title_with_angle`] with `angle = 0`.
    pub fn open_title(
        &self,
        title: &TitleInfo,
        decryptor: Option<Box<dyn StreamDecryptor>>,
    ) -> Result<TitleSource> {
        self.open_title_with_angle(title, 0, decryptor)
    }

    /// Open a title on a specific angle. `angle` is 0-based: `0` is the
    /// primary angle (the clip on each PlayItem itself), `k >= 1` picks
    /// the `k`-th alternate angle from each PlayItem's
    /// [`PlayItem::angles`] list.
    ///
    /// Multi-angle Blu-ray titles store every angle's source packets in
    /// a separate `.m2ts` / `.clpi` pair; only the PlayItem timing
    /// (IN/OUT) is shared. Per BD-ROM AV §5.2.3.3, each angle's clip
    /// list is interleaved on disc so any one angle can be played
    /// end-to-end without seeking against the others.
    ///
    /// Returns an error when `angle` exceeds the smallest
    /// `multi_clip_count` across the title's PlayItems (a single-angle
    /// PlayItem in the middle would leave the seeker without a target
    /// clip for the rest of the title — we surface that conflict at
    /// `open` time rather than mid-stream).
    pub fn open_title_with_angle(
        &self,
        title: &TitleInfo,
        angle: u8,
        decryptor: Option<Box<dyn StreamDecryptor>>,
    ) -> Result<TitleSource> {
        let bdmv = self.root.join("BDMV");
        let pl_bytes = read_file(&playlist_path(&bdmv, title.playlist_id))?;
        let pl = PlayListMpls::parse(&pl_bytes)?;
        if pl.play_list.play_items.is_empty() {
            return Err(BlurayError::not_bluray("title has no PlayItems"));
        }
        // Reject angle values that would leave at least one PlayItem
        // without a target clip — the resulting stream would have a
        // hole in the middle, which is worse than a clean error at
        // open time.
        for (idx, pi) in pl.play_list.play_items.iter().enumerate() {
            if pi.angle_clip(angle).is_none() {
                return Err(BlurayError::not_bluray(format!(
                    "angle {angle} unavailable on PlayItem {idx} (has {} angles)",
                    pi.num_angles()
                )));
            }
        }
        let play_items = pl.play_list.play_items.clone();
        TitleSource::new(bdmv, play_items, angle, decryptor)
    }

    /// Open a title as a stream of per-chapter byte segments selected
    /// by `selector` against the title's chapter table.
    ///
    /// Each yielded [`ChapterSegment`] carries the chapter's title-
    /// relative 1-based id, its start + end title-PTS in 90 kHz ticks,
    /// and the *decrypted, TP_extra-stripped* MPEG-TS bytes for that
    /// chapter. Selectors map to chapter ids as follows:
    ///
    /// - [`ChapterSelector::All`] → one segment per chapter in title order.
    /// - [`ChapterSelector::Range`] → chapters in `[start, end]` (or
    ///   `[start, last]` when `end` is `None`); range is inclusive on
    ///   both ends, 1-based.
    /// - [`ChapterSelector::List`] → the named chapters, in the order
    ///   given by the URI (so `?chapters=3,1` yields chapter 3 *then* 1).
    ///
    /// Returns [`BlurayError::NotBluray`] when the title has no
    /// chapters (so a CLI loop never silently produces zero output),
    /// and [`BlurayError::Malformed`] when `selector` references a
    /// chapter id outside `[1, chapter_count]` — the URI parser cannot
    /// validate the upper bound; we do it here.
    ///
    /// # Boundary caveat
    ///
    /// Per-chapter byte slicing on a Blu-ray is *approximate*. m2ts
    /// streams are contiguous Aligned Units, and a chapter boundary
    /// that falls inside an aligned unit can't be split exactly. The
    /// seeker rounds DOWN to the I-frame at or before the requested
    /// PTS (the only safe landing for a decodable boundary), so each
    /// per-chapter MKV the downstream remuxer produces will have up
    /// to a few hundred ms of *overlap* with the previous chapter at
    /// its head. That's acceptable for the remux use case (the
    /// resulting MKVs play correctly; just one wastes some duplicated
    /// I-frames at the seam). Frame-exact slicing would require a
    /// transcode and is out of scope here.
    pub fn open_title_chapters(
        &self,
        title: &TitleInfo,
        selector: &ChapterSelector,
        decryptor: Option<Box<dyn StreamDecryptor>>,
    ) -> Result<ChapterSegments> {
        let chapters = self.chapters(title);
        if chapters.is_empty() {
            return Err(BlurayError::not_bluray(format!(
                "title {} has no chapters — cannot slice by chapter",
                title.id
            )));
        }
        let chapter_count = chapters.len() as u32;
        let ids: Vec<u32> = match selector {
            ChapterSelector::All => (1..=chapter_count).collect(),
            ChapterSelector::Range { start, end } => {
                let end = end.unwrap_or(chapter_count);
                if *start < 1 || *start > chapter_count {
                    return Err(BlurayError::malformed(format!(
                        "chapter range start {start} is outside [1, {chapter_count}]"
                    )));
                }
                if end < *start || end > chapter_count {
                    return Err(BlurayError::malformed(format!(
                        "chapter range end {end} is outside [{start}, {chapter_count}]"
                    )));
                }
                (*start..=end).collect()
            }
            ChapterSelector::List(list) => {
                for &id in list {
                    if id < 1 || id > chapter_count {
                        return Err(BlurayError::malformed(format!(
                            "chapter id {id} is outside [1, {chapter_count}]"
                        )));
                    }
                }
                list.clone()
            }
        };

        // Pre-compute (chapter_id, start_pts, end_pts) per request.
        // `end_pts` is the next chapter's start; the last chapter in
        // the title carries `end_pts = title_duration` *and* the
        // `ends_at_title_end` flag. The flag is what tells `read_one`
        // to read to EOF rather than to `seek_to(title_duration)` —
        // the latter would round down to the last keyframe in the
        // title, dropping the final GOP of bytes.
        let title_duration = title.duration_ticks;
        let last_chapter_idx = chapters.len() - 1;
        let mut requests = Vec::with_capacity(ids.len());
        for &chapter_id in &ids {
            let idx = chapter_id as usize - 1;
            let start_pts_90k = chapters[idx].start_pts_90k;
            let (end_pts_90k, ends_at_title_end) = if idx == last_chapter_idx {
                (title_duration, true)
            } else {
                (chapters[idx + 1].start_pts_90k, false)
            };
            requests.push(ChapterRequest {
                chapter_id,
                start_pts_90k,
                end_pts_90k,
                ends_at_title_end,
            });
        }

        let source = self.open_title(title, decryptor)?;
        Ok(ChapterSegments { source, requests })
    }

    /// Maximum angle index `k` such that every PlayItem in `title`'s
    /// PlayList offers an angle-`k` clip — i.e. the largest value that
    /// is safe to pass to [`Self::open_title_with_angle`]. Returns 0
    /// when at least one PlayItem is single-clip (only the primary
    /// angle is universally available).
    ///
    /// Reads the title's `.mpls` once. Returns 0 on parse failure
    /// rather than propagating the error — a caller that needs precise
    /// diagnostics can [`PlayListMpls::parse`] directly.
    pub fn max_angle(&self, title: &TitleInfo) -> u8 {
        let bdmv = self.root.join("BDMV");
        let Ok(pl_bytes) = read_file(&playlist_path(&bdmv, title.playlist_id)) else {
            return 0;
        };
        let Ok(pl) = PlayListMpls::parse(&pl_bytes) else {
            return 0;
        };
        pl.play_list
            .play_items
            .iter()
            .map(|pi| pi.num_angles().saturating_sub(1))
            .min()
            .unwrap_or(0)
    }

    /// Title-relative chapter list for `title`, in playback order.
    ///
    /// Reads the title's `.mpls` once and lifts every entry-mark
    /// (§5.4.5) onto the title timeline via
    /// [`PlayListMpls::chapters`]. Each [`Chapter::start_pts_90k`] is
    /// directly seekable with [`TitleSource::seek_to`], so a chapter
    /// menu can jump to the nearest keyframe at a chapter boundary.
    ///
    /// Returns an empty list on read / parse failure rather than
    /// propagating the error — a caller needing precise diagnostics can
    /// [`PlayListMpls::parse`] directly.
    pub fn chapters(&self, title: &TitleInfo) -> Vec<Chapter> {
        let bdmv = self.root.join("BDMV");
        let Ok(pl_bytes) = read_file(&playlist_path(&bdmv, title.playlist_id)) else {
            return Vec::new();
        };
        let Ok(pl) = PlayListMpls::parse(&pl_bytes) else {
            return Vec::new();
        };
        pl.chapters()
    }

    /// Chapter list for `title` with each chapter's derived `[start, end)`
    /// presentation span and duration.
    ///
    /// Same chapters as [`Self::chapters`] but widened via
    /// [`PlayListMpls::chapters_with_duration`]: each chapter ends where
    /// the next begins, and the final chapter ends at the title's total
    /// duration. Returns an empty list on read / parse failure (matches
    /// [`Self::chapters`]).
    pub fn chapter_spans(&self, title: &TitleInfo) -> Vec<ChapterSpan> {
        let bdmv = self.root.join("BDMV");
        let Ok(pl_bytes) = read_file(&playlist_path(&bdmv, title.playlist_id)) else {
            return Vec::new();
        };
        let Ok(pl) = PlayListMpls::parse(&pl_bytes) else {
            return Vec::new();
        };
        pl.chapters_with_duration()
    }

    /// The title's parsed SubPath list (§5.4.4) — the auxiliary
    /// presentation paths (PiP video, out-of-mux audio, text
    /// subtitles, menus) recorded beside the MainPath, each with its
    /// full [`SubPlayItem`](crate::SubPlayItem) list and typed
    /// [`kind()`](crate::SubPath::kind). `TitleSource` streams the
    /// MainPath only; this surface lets a player enumerate what else
    /// the title carries (and resolve each SubPlayItem's clip stem to
    /// its own `.m2ts` / `.clpi` pair).
    ///
    /// Returns an empty list on read / parse failure (matches
    /// [`Self::chapters`] / [`Self::title_streams`]).
    pub fn title_sub_paths(&self, title: &TitleInfo) -> Vec<SubPath> {
        let bdmv = self.root.join("BDMV");
        let Ok(pl_bytes) = read_file(&playlist_path(&bdmv, title.playlist_id)) else {
            return Vec::new();
        };
        let Ok(pl) = PlayListMpls::parse(&pl_bytes) else {
            return Vec::new();
        };
        pl.play_list.sub_paths
    }

    /// Per-track catalogue lifted out of the title's playlist STN_table
    /// (BD-ROM Part 3 §5.4.4.4).
    ///
    /// Every PlayItem in a Blu-ray title carries its own STN_table, but
    /// for a single-angle conformant title the table is identical across
    /// PlayItems — the same primary-video / audio / PG / IG entries
    /// repeat unchanged. The catalogue here merges every PlayItem's
    /// entries by `(elementary_pid, kind)` so a downstream remuxer
    /// gets exactly one [`Track`] per distinct elementary stream, with
    /// `playitem_count` recording how many PlayItems carried that PID.
    ///
    /// The selected angle's PlayItem STN tables are the source of truth.
    /// Multi-angle alternate clips also carry STN tables, but every
    /// angle MUST surface the same elementary PIDs (BD-ROM AV §5.2.3.3
    /// requires this so a mid-stream angle change can keep the same
    /// PMT); reading just the primary PlayItem chain is therefore
    /// sufficient for the per-track listing.
    ///
    /// Tracks are returned in canonical STN class order — primary
    /// video, primary audio, PG subtitles, IG menus, secondary audio,
    /// secondary video, PiP PG — and within each class in the order
    /// the STN_table itself lists them. This is the same order a BD
    /// player applies when assigning per-class user-selector indices,
    /// so a remuxer can label tracks deterministically off `Track::pid`.
    ///
    /// Returns an empty list on read / parse failure rather than
    /// propagating the error (matches [`Self::max_angle`] /
    /// [`Self::chapters`]). A caller needing precise diagnostics can
    /// [`PlayListMpls::parse`] directly.
    /// PlayItem-aligned PTS continuity segments for `title` —
    /// equivalent to opening a [`TitleSource`] on the primary angle
    /// and calling [`TitleSource::pts_continuity_segments`], but
    /// skips the m2ts file-open step (the segment list is derived
    /// entirely from the MPLS + CLPI surface).
    ///
    /// See [`PtsContinuitySegment`] for the per-PlayItem byte / PTS
    /// reproject contract and the
    /// `SeamlessContinuation` / `SeamlessNewStc` / `NonSeamless`
    /// semantics at PlayItem seams.
    ///
    /// Returns an empty list on read / parse failure rather than
    /// propagating the error (matches [`Self::chapters`] /
    /// [`Self::title_streams`]).
    pub fn title_pts_continuity_segments(&self, title: &TitleInfo) -> Vec<PtsContinuitySegment> {
        self.title_pts_continuity_segments_with_angle(title, 0)
    }

    /// Same as [`Self::title_pts_continuity_segments`] but for a
    /// specific 0-based angle. Returns an empty list when `angle`
    /// is unavailable on any PlayItem (matches the
    /// [`Self::open_title_with_angle`] rejection policy, but doesn't
    /// surface the error since this method is used for inspection).
    pub fn title_pts_continuity_segments_with_angle(
        &self,
        title: &TitleInfo,
        angle: u8,
    ) -> Vec<PtsContinuitySegment> {
        let bdmv = self.root.join("BDMV");
        let Ok(pl_bytes) = read_file(&playlist_path(&bdmv, title.playlist_id)) else {
            return Vec::new();
        };
        let Ok(pl) = PlayListMpls::parse(&pl_bytes) else {
            return Vec::new();
        };
        if pl.play_list.play_items.is_empty() {
            return Vec::new();
        }
        for pi in &pl.play_list.play_items {
            if pi.angle_clip(angle).is_none() {
                return Vec::new();
            }
        }
        // The construction mirrors `TitleSource::new` + the public
        // `pts_continuity_segments` body. We can't share the code
        // because `TitleSource::new` does real I/O (open the m2ts +
        // mount the decryptor); this lighter walk only reads CLPI.
        let mut clips: Vec<ClipSeekInfo> = Vec::with_capacity(pl.play_list.play_items.len());
        let mut output_total: u64 = 0;
        let mut title_pts_start: u64 = 0;
        for pi in &pl.play_list.play_items {
            let angle_ref = pi.angle_clip(angle);
            let stem = angle_ref
                .map(|a| a.clip_information_file_name.to_string())
                .unwrap_or_else(|| pi.clip_information_file_name.clone());
            let stc_id_ref = angle_ref.map(|a| a.stc_id_ref).unwrap_or(pi.stc_id_ref);
            let m2ts_path = bdmv.join("STREAM").join(format!("{stem}.m2ts"));
            let packet_count = match std::fs::metadata(&m2ts_path) {
                Ok(meta) => {
                    let raw = meta.len();
                    let usable = raw - (raw % M2TS_PACKET_LEN as u64);
                    usable / M2TS_PACKET_LEN as u64
                }
                Err(_) => 0,
            };
            let (entry_points, stc_origin_pts_90k, angle_change_eps) =
                load_clip_meta(&bdmv, &stem, stc_id_ref);
            clips.push(ClipSeekInfo {
                stem,
                output_start: output_total,
                packet_count,
                title_pts_start,
                in_pts_90k: u64::from(pi.in_time_ticks) * 2,
                entry_points,
                angle_change_eps,
                connection_condition: pi.connection_condition,
                stc_id_ref,
                stc_origin_pts_90k,
            });
            output_total += packet_count * TS_PACKET_LEN as u64;
            title_pts_start += pi.duration_90k();
        }

        // Build the public segment list — same logic as
        // `TitleSource::pts_continuity_segments` but operating on the
        // local `clips` vec rather than `self.clips`.
        let mut out = Vec::with_capacity(clips.len());
        for (idx, c) in clips.iter().enumerate() {
            let cc = if idx == 0 {
                ConnectionCondition::NonSeamless
            } else {
                c.connection_condition
            };
            let next_output_start = clips
                .get(idx + 1)
                .map(|n| n.output_start)
                .unwrap_or(output_total);
            let next_title_pts = clips
                .get(idx + 1)
                .map(|n| n.title_pts_start)
                .unwrap_or_else(|| c.title_pts_start + pl.play_list.play_items[idx].duration_90k());
            let clip_out_pts_90k = c.in_pts_90k + next_title_pts.saturating_sub(c.title_pts_start);
            let mut stem_bytes = [0u8; 5];
            let raw = c.stem.as_bytes();
            let copy_len = raw.len().min(5);
            stem_bytes[..copy_len].copy_from_slice(&raw[..copy_len]);
            out.push(PtsContinuitySegment {
                play_item_index: idx as u16,
                clip_stem: stem_bytes,
                output_byte_start: c.output_start,
                output_byte_end: next_output_start,
                title_pts_start: c.title_pts_start,
                title_pts_end: next_title_pts,
                clip_in_pts_90k: c.in_pts_90k,
                clip_out_pts_90k,
                stc_origin_pts_90k: c.stc_origin_pts_90k,
                stc_id_ref: c.stc_id_ref,
                connection_condition: cc,
            });
        }
        out
    }

    pub fn title_streams(&self, title: &TitleInfo) -> TrackCatalogue {
        let bdmv = self.root.join("BDMV");
        let Ok(pl_bytes) = read_file(&playlist_path(&bdmv, title.playlist_id)) else {
            return TrackCatalogue::default();
        };
        let Ok(pl) = PlayListMpls::parse(&pl_bytes) else {
            return TrackCatalogue::default();
        };
        build_track_catalogue(&pl)
    }

    /// Every mid-stream angle-change boundary for `title` on its
    /// primary angle — file-less peer of
    /// [`TitleSource::angle_change_points`]. Equivalent to opening the
    /// title and calling that method, but skips the `.m2ts` file-open
    /// step (the list is derived entirely from MPLS + CLPI).
    ///
    /// Returns an empty list on read / parse failure (matches
    /// [`Self::chapters`] / [`Self::title_streams`]).
    pub fn title_angle_change_points(&self, title: &TitleInfo) -> Vec<AngleChangePoint> {
        self.title_angle_change_points_with_angle(title, 0)
    }

    /// Same as [`Self::title_angle_change_points`] but for a specific
    /// 0-based angle. Returns an empty list when `angle` is
    /// unavailable on any PlayItem (matches the
    /// [`Self::open_title_with_angle`] rejection policy).
    pub fn title_angle_change_points_with_angle(
        &self,
        title: &TitleInfo,
        angle: u8,
    ) -> Vec<AngleChangePoint> {
        let bdmv = self.root.join("BDMV");
        let Ok(pl_bytes) = read_file(&playlist_path(&bdmv, title.playlist_id)) else {
            return Vec::new();
        };
        let Ok(pl) = PlayListMpls::parse(&pl_bytes) else {
            return Vec::new();
        };
        if pl.play_list.play_items.is_empty() {
            return Vec::new();
        }
        for pi in &pl.play_list.play_items {
            if pi.angle_clip(angle).is_none() {
                return Vec::new();
            }
        }

        // Mirror `TitleSource::new`'s clip walk, but only the fields
        // angle-change-point enumeration needs (stem + output running
        // tally + title-PTS running tally + EP_map). No I/O against
        // STREAM/<stem>.m2ts: we measure that file purely to advance
        // the output running tally.
        let mut out = Vec::new();
        let mut output_total: u64 = 0;
        let mut title_pts_start: u64 = 0;
        for (idx, pi) in pl.play_list.play_items.iter().enumerate() {
            let angle_ref = pi.angle_clip(angle);
            let stem = angle_ref
                .map(|a| a.clip_information_file_name.to_string())
                .unwrap_or_else(|| pi.clip_information_file_name.clone());
            let stc_id_ref = angle_ref.map(|a| a.stc_id_ref).unwrap_or(pi.stc_id_ref);
            let m2ts_path = bdmv.join("STREAM").join(format!("{stem}.m2ts"));
            let packet_count = match std::fs::metadata(&m2ts_path) {
                Ok(meta) => {
                    let raw = meta.len();
                    let usable = raw - (raw % M2TS_PACKET_LEN as u64);
                    usable / M2TS_PACKET_LEN as u64
                }
                Err(_) => 0,
            };
            let (_eps, _stc, angle_change_eps) = load_clip_meta(&bdmv, &stem, stc_id_ref);
            let in_pts_90k = u64::from(pi.in_time_ticks) * 2;

            let mut stem_bytes = [0u8; 5];
            let raw = stem.as_bytes();
            let copy_len = raw.len().min(5);
            stem_bytes[..copy_len].copy_from_slice(&raw[..copy_len]);

            for (pts_ep, spn) in angle_change_eps {
                let clip_pts_64 = u64::from(pts_ep);
                if clip_pts_64 < in_pts_90k {
                    continue;
                }
                let title_pts_90k = title_pts_start + (clip_pts_64 - in_pts_90k);
                let output_byte = output_total + u64::from(spn) * TS_PACKET_LEN as u64;
                out.push(AngleChangePoint {
                    play_item_index: idx as u16,
                    clip_stem: stem_bytes,
                    title_pts_90k,
                    output_byte,
                    clip_pts_90k: pts_ep,
                    spn,
                });
            }

            output_total += packet_count * TS_PACKET_LEN as u64;
            title_pts_start += pi.duration_90k();
        }
        out
    }
}

/// Per-clip seek metadata, computed once at construction. Lets
/// [`TitleSource::seek_to`] map a title-relative 90 kHz PTS to a clip,
/// then to a keyframe-aligned source-packet number via the clip's
/// CPI EP_map (BD-ROM AV §5.7).
#[derive(Debug, Clone)]
struct ClipSeekInfo {
    /// 5-digit clip stem (e.g. `"00001"`); resolves both the `.m2ts`
    /// and `.clpi` paths.
    stem: String,
    /// Absolute output byte offset (post-TP_extra-strip) at which this
    /// clip's bytes begin in the concatenated title stream.
    output_start: u64,
    /// Number of usable 188-byte TS packets this clip contributes
    /// (`file_len / 192`, truncated to a packet boundary).
    packet_count: u64,
    /// Title-relative 90 kHz PTS at which this clip's playback begins
    /// (running sum of preceding PlayItem durations).
    title_pts_start: u64,
    /// Clip-local 90 kHz PTS of the PlayItem's IN point — the EP_map's
    /// `pts_ep_start` values are clip-local, so we offset the seek
    /// target by this before searching. (`PlayItem.in_time_ticks` is
    /// 45 kHz; doubled to 90 kHz.)
    in_pts_90k: u64,
    /// Flat, ascending list of `(pts_ep_start, spn_ep_start)` from the
    /// clip's primary-video EP_map. Empty when the clip ships no CPI
    /// (homemade discs) — seeking then falls back to the clip start.
    entry_points: Vec<(u32, u32)>,
    /// Subset of `entry_points` whose CPI EP_fine row has the
    /// `is_angle_change_point` bit set (BD-ROM AV §5.7 + Part 3
    /// §5.4.4.1 `is_multi_angle`). Mid-stream angle switching is only
    /// safe at one of these source packets — every alternate angle's
    /// clip carries a co-incident I-frame at the matching SPN so the
    /// decoder can resume without re-seeding state. Empty for
    /// single-angle clips (the EP_map row will still exist, but with
    /// the bit cleared).
    angle_change_eps: Vec<(u32, u32)>,
    /// Connection condition advertised by the **owning PlayItem**
    /// (§5.4.4.2): how this clip's STC relates to the previous clip's.
    /// `SeamlessContinuation` (0x05) means the previous clip's PTS axis
    /// continues; the other variants introduce a fresh STC origin and a
    /// downstream demuxer must add a per-clip offset to reproject
    /// clip-local PTS onto the title timeline.
    connection_condition: ConnectionCondition,
    /// `stc_id_ref` from the PlayItem — indexes into the CLPI's
    /// SequenceInfo to pick **which STC sequence inside the clip** the
    /// PlayItem maps to (§5.4.4.1 + §5.5.4.2).
    stc_id_ref: u8,
    /// Clip-local 90 kHz PTS at which the referenced STC sequence
    /// begins, lifted from the CLPI's `SequenceInfo` /
    /// `StcSequence::presentation_start_time` (§5.5.4.2). The
    /// SequenceInfo stores it in 45 kHz units; we double it to 90 kHz
    /// for parity with the rest of the seek pipeline. Falls back to
    /// `in_pts_90k` when the SequenceInfo is missing or the
    /// `stc_id_ref` is out of range (homemade discs).
    stc_origin_pts_90k: u64,
}

/// One PlayItem-aligned continuity segment of a title's output byte
/// stream, ready for a downstream MPEG-TS demuxer to translate raw
/// clip-local PTS values onto a continuous title timeline.
///
/// Background: every PlayItem references its own clip
/// (`STREAM/<stem>.m2ts`), and every clip carries its own STC (System
/// Time Clock) sequence whose origin PTS lives in the CLPI's
/// `SequenceInfo` (BD-ROM AV §5.5.4.2). When [`TitleSource`] stitches
/// PlayItems back-to-back the TS bytes still carry the *clip-local*
/// PTS values — they restart (`NonSeamless` 0x01 / `SeamlessNewStc`
/// 0x06) or continue (`SeamlessContinuation` 0x05) at every PlayItem
/// boundary, but the bytes themselves do not advertise which case
/// applies. Without an out-of-band map a demuxer either sees PTS
/// jumps as legitimate huge skips or assumes monotonic timing and
/// produces a malformed mux at the join.
///
/// One [`PtsContinuitySegment`] per PlayItem says: "between
/// [`output_byte_start`] and [`output_byte_end`] every PES packet's
/// PTS is on the clip's local clock starting at
/// [`clip_in_pts_90k`]; to lift it onto the title timeline add
/// `title_pts_start - clip_in_pts_90k`". The
/// [`connection_condition`] tells the consumer whether the previous
/// segment's PTS axis carries through (no reproject needed across
/// the seam) or restarts (reproject required).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtsContinuitySegment {
    /// 0-based index of this PlayItem in the title's PlayList. Matches
    /// `PlayItem`'s position in `PlayListMpls::play_list.play_items`,
    /// which is also `PlayListMark::ref_play_item_id` for the chapter
    /// marks that point inside this segment.
    pub play_item_index: u16,
    /// 5-digit clip stem (e.g. `"00001"`); resolves both
    /// `STREAM/<stem>.m2ts` and `CLIPINF/<stem>.clpi`.
    pub clip_stem: [u8; 5],
    /// Absolute output-byte offset (post-`TP_extra`-strip) at which
    /// this segment's clean MPEG-TS bytes begin in the title stream.
    /// Equals `0` for the first PlayItem.
    pub output_byte_start: u64,
    /// Exclusive upper bound of the segment's byte range — first byte
    /// of the next PlayItem (or [`TitleSource::output_total`] for the
    /// final PlayItem).
    pub output_byte_end: u64,
    /// Title-relative 90 kHz PTS at which this segment's playback
    /// starts: running sum of the durations of all preceding PlayItems
    /// (§5.4.4.1 IN/OUT pair, doubled from 45 kHz to 90 kHz).
    pub title_pts_start: u64,
    /// Title-relative 90 kHz PTS at which this segment ends —
    /// `title_pts_start + (out_time - in_time) × 2`.
    pub title_pts_end: u64,
    /// Clip-local 90 kHz PTS of the PlayItem's IN point
    /// (`PlayItem::in_time_ticks × 2`). PES packets inside this
    /// segment carry PTS values ≥ `clip_in_pts_90k`; the title-PTS
    /// reproject is `title_pts_start + (pes_pts - clip_in_pts_90k)`.
    pub clip_in_pts_90k: u64,
    /// Clip-local 90 kHz PTS of the PlayItem's OUT point
    /// (`PlayItem::out_time_ticks × 2`). PES packets past this are
    /// out-of-PlayItem and should be discarded by the demuxer.
    pub clip_out_pts_90k: u64,
    /// Clip-local 90 kHz PTS at which the STC sequence indexed by
    /// [`stc_id_ref`] begins (CLPI `SequenceInfo` /
    /// `presentation_start_time` doubled). `0` when the clip ships no
    /// SequenceInfo or `stc_id_ref` is out of range (homemade discs).
    /// A demuxer that uses MPEG-2 33-bit PTS wraparound math can use
    /// this as the segment's STC origin to disambiguate wrap-around.
    pub stc_origin_pts_90k: u64,
    /// Which STC sequence inside the clip this PlayItem maps to
    /// (§5.4.4.1 `stc_id_ref` field). For most clips this is `0`
    /// — single-STC authoring — but some clips ship multiple STC
    /// sequences and the PlayItem picks one.
    pub stc_id_ref: u8,
    /// Connection condition advertised by the PlayItem (§5.4.4.2):
    /// how this segment's PTS axis relates to the previous segment's.
    /// First PlayItem is always treated as a fresh axis regardless of
    /// the recorded byte (there's no "previous" to continue from).
    pub connection_condition: ConnectionCondition,
}

/// One mid-stream angle-switch candidate, lifted from the CPI EP_map's
/// `is_angle_change_point` bit (BD-ROM AV §5.7) of the title's
/// currently-selected angle.
///
/// Multi-angle Blu-ray titles interleave each angle's source packets
/// inside a `PLAYITEM_TYPE == is_multi_angle` block (BD-ROM Part 3
/// §5.4.4.1). A live angle switch is **only** valid at a video access
/// unit where every alternate angle's interleaved clip carries a
/// co-incident I-frame at the matching source-packet number — the
/// authoring tool flags those rows with `is_angle_change_point = 1`
/// in the EP_fine table. A naive switch at any other byte would either
/// resume mid-GOP on the new angle (decoded garbage until the next
/// IDR) or land on the wrong elementary-stream PID.
///
/// One [`AngleChangePoint`] therefore says: "at output-byte
/// [`output_byte`] (title-PTS [`title_pts_90k`]), every alternate
/// angle has an aligned I-frame; an angle switch performed at
/// exactly this byte will produce a clean cut." The caller drives the
/// switch by:
///
/// 1. Reading up to [`output_byte`] on the current
///    [`TitleSource`].
/// 2. Closing the current source.
/// 3. Calling [`Disc::open_title_with_angle`] with the new angle index.
/// 4. Calling [`TitleSource::seek_to`] with `title_pts_90k` to land on
///    the matching I-frame on the new angle.
///
/// The struct exposes both the title-relative PTS / output byte
/// (driver-side scheduling), the clip-local PTS / SPN
/// ([`clip_pts_90k`] / [`spn`] — the raw EP_map values, useful for
/// verification against the per-clip CPI dump), and the owning
/// [`play_item_index`] / [`clip_stem`] (so a UI can label the switch
/// boundary by chapter / clip number).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AngleChangePoint {
    /// 0-based PlayItem index inside the title's PlayList. Matches the
    /// position in `PlayListMpls::play_list.play_items`.
    pub play_item_index: u16,
    /// 5-digit clip stem (e.g. `"00001"`); identifies the `.m2ts` /
    /// `.clpi` pair that owns the EP_map row this point came from.
    pub clip_stem: [u8; 5],
    /// Title-relative 90 kHz PTS at which the angle switch should
    /// land. Directly passable to [`TitleSource::seek_to`] on a
    /// freshly-opened source for the new angle.
    pub title_pts_90k: u64,
    /// Absolute output-byte position (post-`TP_extra`-strip) on the
    /// **current** angle's title stream at which the caller should
    /// stop reading before performing the switch. Equal to
    /// `clip.output_start + spn × 188`.
    pub output_byte: u64,
    /// Clip-local 90 kHz PTS of the angle-change EP_fine row — the raw
    /// `pts_ep_start` value from the EP_map (BD-ROM AV §5.7).
    pub clip_pts_90k: u32,
    /// Source-packet number on the clip's `.m2ts` at which the
    /// I-frame begins. Equal to `output_byte_within_clip / 188` and
    /// to `input_byte_within_clip / 192`.
    pub spn: u32,
}

/// A `Read`-able view onto a title: concatenates the title's PlayItem
/// clips end-to-end, stripping the 4-byte BDAV TP_extra header per
/// 192-byte source packet to yield a clean 188-byte MPEG-TS stream.
pub struct TitleSource {
    bdmv_root: PathBuf,
    /// Per-clip metadata + EP_map seek index, in playback order.
    clips: Vec<ClipSeekInfo>,
    clip_idx: usize,
    /// Open file for the current clip, or `None` if EOF / between clips.
    current: Option<File>,
    /// Absolute byte offset within the current clip (for decryption).
    clip_offset: u64,
    /// Decryptor — replaced by `Identity` if `None` was passed.
    decryptor: Box<dyn StreamDecryptor>,
    /// Pending 188-byte TS bytes already produced from the most recent
    /// read; drained first on the next `read()`.
    pending: Vec<u8>,
    pending_pos: usize,
    /// Total bytes already emitted to the caller across the whole
    /// title — i.e. the absolute output position. Tracked separately
    /// from `clip_offset` (input-side) because the TP_extra-strip
    /// changes byte counts (192 → 188 per packet).
    output_pos: u64,
    /// Estimated total output bytes for the entire title (sum of
    /// each .m2ts file size * 188 / 192). Computed at construction
    /// so `seek(End(0))`-style probes don't have to walk the clips.
    output_total: u64,
    /// Total title duration in 90 kHz ticks — sum of every
    /// PlayItem's `(OUT - IN) × 2`. Cached at construction so
    /// [`Self::pts_continuity_segments`] can derive the final
    /// segment's `title_pts_end` without re-walking the PlayItem
    /// list.
    title_duration_90k: u64,
    /// PlayList PlayItems backing the title, retained so an in-place
    /// angle switch ([`Self::switch_angle_at`] / [`Self::switch_angle`])
    /// can rebuild the per-clip seek index for a different angle's
    /// `.m2ts` / `.clpi` pair without re-reading the source `.mpls`.
    play_items: Vec<PlayItem>,
    /// 0-based angle index currently driving [`Self::clips`]. `0` is
    /// the primary angle (clip name from the PlayItem itself); `k ≥ 1`
    /// references entry `k - 1` of the PlayItem's
    /// [`crate::bdmv::mpls::PlayItem::angles`] list (§5.4.4.1
    /// `is_multi_angle` block).
    current_angle: u8,
}

impl std::fmt::Debug for TitleSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TitleSource")
            .field("bdmv_root", &self.bdmv_root)
            .field("num_clips", &self.clips.len())
            .field("clip_idx", &self.clip_idx)
            .field("clip_offset", &self.clip_offset)
            .field("output_pos", &self.output_pos)
            .field("output_total", &self.output_total)
            .field("pending_len", &(self.pending.len() - self.pending_pos))
            .finish()
    }
}

impl TitleSource {
    fn new(
        bdmv_root: PathBuf,
        play_items: Vec<PlayItem>,
        angle: u8,
        decryptor: Option<Box<dyn StreamDecryptor>>,
    ) -> Result<Self> {
        let (clips, output_total, title_duration_90k) =
            build_clip_seek_index(&bdmv_root, &play_items, angle);

        let mut s = Self {
            bdmv_root,
            clips,
            clip_idx: 0,
            current: None,
            clip_offset: 0,
            decryptor: decryptor.unwrap_or_else(|| Box::new(crate::decrypt::Identity)),
            pending: Vec::new(),
            pending_pos: 0,
            output_pos: 0,
            output_total,
            title_duration_90k,
            play_items,
            current_angle: angle,
        };
        s.open_next_clip()?;
        Ok(s)
    }

    /// 0-based index of the angle currently driving this source.
    ///
    /// `0` is the primary clip set; `k ≥ 1` references the `k`-th
    /// entry of each PlayItem's `is_multi_angle` block (BD-ROM Part 3
    /// §5.4.4.1). Single-angle titles always report `0`.
    pub fn current_angle(&self) -> u8 {
        self.current_angle
    }

    /// Number of angles available across the title: the smallest
    /// `multi_clip_count` across every PlayItem (so `switch_angle_at`
    /// is guaranteed never to land on a PlayItem with fewer angles).
    /// Always `≥ 1`; equals `1` for single-angle titles.
    pub fn num_angles(&self) -> u8 {
        self.play_items
            .iter()
            .map(|pi| pi.num_angles())
            .min()
            .unwrap_or(1)
    }

    /// Switch the underlying clip reader to a different angle at the
    /// keyframe-aligned title PTS `title_pts_90k`, without dropping
    /// the title's decryptor / output-state metadata.
    ///
    /// The spec's interleaved-clip constraint (BD-ROM AV §5.2.3.3 +
    /// §5.7 `is_angle_change_point`) guarantees that every alternate
    /// angle's `.m2ts` carries a co-incident I-frame at the matching
    /// source-packet number, so a switch at one of
    /// [`Self::angle_change_points`] is decoder-safe — the next packet
    /// served after this call is the same access unit on a different
    /// camera angle.
    ///
    /// Mechanics:
    ///
    /// 1. Validate `new_angle` against every PlayItem (matches the
    ///    upfront check in [`Disc::open_title_with_angle`]) — an
    ///    out-of-range angle is rejected before any I/O so the source
    ///    stays usable on the previous angle.
    /// 2. Rebuild the per-clip seek index ([`ClipSeekInfo`] list) for
    ///    `new_angle` — each PlayItem's clip stem is reselected via
    ///    [`crate::bdmv::mpls::PlayItem::angle_clip`], its `.m2ts`
    ///    size is re-measured, and its `.clpi` is re-parsed for the
    ///    EP_map + STC origin + angle-change rows. The
    ///    interleaved-clip constraint makes the per-clip packet
    ///    counts (and therefore [`Self::output_total`]) almost always
    ///    identical across angles, but the surface doesn't assume so;
    ///    `output_total` is recomputed.
    /// 3. Land the reader on the chosen `title_pts_90k`:
    ///    keyframe-rounded to the new angle's EP_map (same
    ///    [`Self::seek_to`] semantics — the entry at or before the
    ///    target). `current_angle` is updated; the decryptor + the
    ///    title-duration cache are preserved.
    ///
    /// Returns the new absolute output position the next [`Self::read`]
    /// will resume from.
    ///
    /// The pre-call `output_pos` is invalidated — alternate angles' bytes
    /// live in different `.m2ts` files, so even if the packet counts
    /// match, the output-byte axis is a new physical stream. Callers
    /// who tracked their position by byte should re-anchor against the
    /// returned value.
    pub fn switch_angle_at(&mut self, new_angle: u8, title_pts_90k: u64) -> io::Result<u64> {
        // Validate angle against every PlayItem before mutating any
        // state — preserve the previous-angle source on rejection.
        for (idx, pi) in self.play_items.iter().enumerate() {
            if pi.angle_clip(new_angle).is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "angle {new_angle} unavailable on PlayItem {idx} (has {} angles)",
                        pi.num_angles()
                    ),
                ));
            }
        }

        // Rebuild per-clip seek index for the new angle. The title's
        // total duration in 90 kHz ticks is a function of PlayItem IN /
        // OUT only (angle-independent), but we let the helper recompute
        // it for parity with `new()` and to keep the helper a pure
        // function of (bdmv_root, play_items, angle).
        let (clips, output_total, title_duration_90k) =
            build_clip_seek_index(&self.bdmv_root, &self.play_items, new_angle);
        self.clips = clips;
        self.output_total = output_total;
        self.title_duration_90k = title_duration_90k;
        self.current_angle = new_angle;
        // The previous angle's open file handle now references the
        // wrong stream — drop it and clear all pending bytes before
        // the seek opens the new angle's clip.
        self.current = None;
        self.pending.clear();
        self.pending_pos = 0;
        self.clip_offset = 0;
        self.output_pos = 0;
        self.clip_idx = 0;

        self.seek_to(title_pts_90k)
    }

    /// Switch to `new_angle` at the next safe boundary at or after the
    /// current output position.
    ///
    /// "Safe" boundary = an [`AngleChangePoint`] — a CPI EP_fine row
    /// with `is_angle_change_point = 1`. The spec guarantees these are
    /// the only source packets where every alternate angle's clip
    /// carries a co-incident I-frame, so the decoder can resume on the
    /// new angle without re-seeding state (BD-ROM AV §5.7).
    ///
    /// Returns the new absolute output position. Errors:
    ///
    /// - [`io::ErrorKind::InvalidInput`] if `new_angle` exceeds at
    ///   least one PlayItem's `multi_clip_count` (no rebuild
    ///   happens — the source stays on the previous angle).
    /// - [`io::ErrorKind::NotFound`] if no `AngleChangePoint` exists at
    ///   or after the current output position (e.g. the title carries
    ///   no flagged rows, or the reader is past the last boundary).
    ///   The source stays on the previous angle.
    pub fn switch_angle(&mut self, new_angle: u8) -> io::Result<u64> {
        // Find the first angle-change boundary at or after the current
        // output position. `angle_change_points` returns boundaries in
        // title order; pick the first whose `output_byte >= output_pos`.
        let current_output = self.output_pos;
        let target_pts = self
            .angle_change_points()
            .into_iter()
            .find(|p| p.output_byte >= current_output)
            .map(|p| p.title_pts_90k)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "no angle-change boundary at or after current position",
                )
            })?;
        self.switch_angle_at(new_angle, target_pts)
    }

    /// Total output bytes the title will produce when read to EOF
    /// (sum of each `.m2ts` size × 188 / 192, truncated to packet
    /// boundaries). Computed once at construction; constant for the
    /// lifetime of the source.
    pub fn output_total(&self) -> u64 {
        self.output_total
    }

    /// PlayItem-aligned continuity segments for the title — one
    /// [`PtsContinuitySegment`] per PlayItem in playback order.
    ///
    /// A downstream MPEG-TS demuxer streams the bytes [`Self::read`]
    /// emits and consults this list to reproject each PES packet's
    /// clip-local PTS onto the continuous title timeline. The mapping
    /// for a packet seen between `output_byte_start` and
    /// `output_byte_end` of segment `s` is:
    ///
    /// ```text
    ///   title_pts = s.title_pts_start + (pes_pts - s.clip_in_pts_90k)
    /// ```
    ///
    /// (where `pes_pts` is the raw 90 kHz PES PTS). The fact that
    /// each segment's PTS axis can be **completely unrelated** to the
    /// previous one — `connection_condition == NonSeamless` (0x01) or
    /// `SeamlessNewStc` (0x06) — is exactly the case a remuxer
    /// previously had no signal for. With this list in hand the
    /// remuxer can:
    ///
    /// * apply a per-segment offset to map clip-local PTS → title PTS;
    /// * detect that a new STC sequence starts at the seam (the bytes
    ///   already carry a different PCR value but a demuxer that
    ///   tracks PCR continuity needs to be told the boundary is
    ///   intentional, not corruption);
    /// * preserve `SeamlessContinuation` (0x05) seams as a single
    ///   continuous axis (no reproject across that seam).
    ///
    /// The returned vec is empty only when the title has no
    /// PlayItems — every successfully-mounted title has at least one
    /// segment.
    pub fn pts_continuity_segments(&self) -> Vec<PtsContinuitySegment> {
        let mut out = Vec::with_capacity(self.clips.len());
        for (idx, c) in self.clips.iter().enumerate() {
            // The first PlayItem's connection condition is meaningless
            // (§5.4.4.2 defines it as "between this PlayItem and the
            // previous one") so normalise it to `NonSeamless` — the
            // segment is the seed of its own axis, no previous to
            // continue from.
            let cc = if idx == 0 {
                ConnectionCondition::NonSeamless
            } else {
                c.connection_condition
            };
            let next_output_start = self
                .clips
                .get(idx + 1)
                .map(|n| n.output_start)
                .unwrap_or(self.output_total);
            // Title-PTS end of this segment: next clip's start for
            // every clip but the last; for the last clip, the cached
            // total title duration (sum of every PlayItem's
            // `duration_90k`, captured at construction).
            let next_title_pts = self
                .clips
                .get(idx + 1)
                .map(|n| n.title_pts_start)
                .unwrap_or(self.title_duration_90k);
            let clip_out_pts_90k = c.in_pts_90k + next_title_pts.saturating_sub(c.title_pts_start);
            let mut stem_bytes = [0u8; 5];
            let raw = c.stem.as_bytes();
            let copy_len = raw.len().min(5);
            stem_bytes[..copy_len].copy_from_slice(&raw[..copy_len]);
            out.push(PtsContinuitySegment {
                play_item_index: idx as u16,
                clip_stem: stem_bytes,
                output_byte_start: c.output_start,
                output_byte_end: next_output_start,
                title_pts_start: c.title_pts_start,
                title_pts_end: next_title_pts,
                clip_in_pts_90k: c.in_pts_90k,
                clip_out_pts_90k,
                stc_origin_pts_90k: c.stc_origin_pts_90k,
                stc_id_ref: c.stc_id_ref,
                connection_condition: cc,
            });
        }
        out
    }

    /// Reproject a clip-local 90 kHz PES PTS at output-byte position
    /// `byte_pos` onto the title timeline. Returns `None` when
    /// `byte_pos` lies past the last continuity segment or when the
    /// clip-local PTS sits before its segment's IN point (out-of-
    /// PlayItem leading bytes that the demuxer should discard).
    ///
    /// This is the convenience companion to [`Self::pts_continuity_segments`]
    /// for one-shot remap calls. A demuxer that already cached the
    /// segment list per-output-byte should reproject inline; this is
    /// the slower binary-search path for callers that don't.
    pub fn map_clip_pts_to_title_pts(&self, byte_pos: u64, clip_pts_90k: u64) -> Option<u64> {
        // Pick the segment whose [start, end) contains byte_pos.
        let seg_idx = self
            .clips
            .iter()
            .rposition(|c| c.output_start <= byte_pos)?;
        let seg = &self.clips[seg_idx];
        if clip_pts_90k < seg.in_pts_90k {
            return None;
        }
        Some(seg.title_pts_start + (clip_pts_90k - seg.in_pts_90k))
    }

    /// Every mid-stream angle-change boundary in the title, in playback
    /// order. One [`AngleChangePoint`] per CPI EP_fine row with
    /// `is_angle_change_point = 1` (BD-ROM AV §5.7), folded onto the
    /// title timeline + output-byte axis exactly as
    /// [`Self::pts_continuity_segments`] folds PlayItem seams.
    ///
    /// Empty when:
    /// * the title is single-angle (no CPI row sets the bit);
    /// * every clip ships an empty / missing CPI (homemade discs);
    /// * the row's clip-local PTS lies before its PlayItem's IN point
    ///   (out-of-PlayItem byte that the demuxer would discard anyway).
    ///
    /// Sorted ascending by `title_pts_90k`. A player UI typically pairs
    /// this with the multi-angle picker:
    /// [`Self::next_angle_change_point`] gives the boundary the user
    /// can switch *into*; calling [`Disc::open_title_with_angle`] +
    /// [`Self::seek_to`] on the new angle's source lands at the
    /// matching I-frame.
    ///
    /// The title's currently-selected angle is the source of truth —
    /// alternate angles' CPI EP_maps MUST advertise the same
    /// angle-change rows (the spec's interleaved-clip constraint
    /// guarantees one IDR per row in every angle), so a switch from
    /// angle 0 → 1 → 2 walks the same boundary set.
    pub fn angle_change_points(&self) -> Vec<AngleChangePoint> {
        let mut out = Vec::new();
        for (idx, c) in self.clips.iter().enumerate() {
            let mut stem_bytes = [0u8; 5];
            let raw = c.stem.as_bytes();
            let copy_len = raw.len().min(5);
            stem_bytes[..copy_len].copy_from_slice(&raw[..copy_len]);
            for &(pts_ep, spn) in &c.angle_change_eps {
                let clip_pts_64 = u64::from(pts_ep);
                // Out-of-PlayItem rows: an angle-change EP whose PTS
                // sits before the PlayItem's IN point would never be
                // read by the streamer (the IN point clips it off);
                // drop it so a UI doesn't surface an unreachable
                // boundary.
                if clip_pts_64 < c.in_pts_90k {
                    continue;
                }
                let title_pts_90k = c.title_pts_start + (clip_pts_64 - c.in_pts_90k);
                let output_byte = c.output_start + u64::from(spn) * TS_PACKET_LEN as u64;
                out.push(AngleChangePoint {
                    play_item_index: idx as u16,
                    clip_stem: stem_bytes,
                    title_pts_90k,
                    output_byte,
                    clip_pts_90k: pts_ep,
                    spn,
                });
            }
        }
        out
    }

    /// First angle-change boundary at or after `pts_90k` (title
    /// timeline, 90 kHz), or `None` if no remaining boundary exists.
    ///
    /// Convenience wrapper around [`Self::angle_change_points`] for the
    /// common "user pressed the angle button, give me the next safe
    /// switch boundary" UI path. Performs a binary search rather than
    /// returning the iterator + a `find` call so the typical N (≤ 1
    /// per chapter) stays O(log N).
    pub fn next_angle_change_point(&self, pts_90k: u64) -> Option<AngleChangePoint> {
        let pts_list = self.angle_change_points();
        let idx = pts_list.partition_point(|p| p.title_pts_90k < pts_90k);
        pts_list.get(idx).copied()
    }

    /// Seek to the keyframe-aligned entry point at or before `pts_90k`
    /// (a title-relative presentation timestamp in 90 kHz ticks) and
    /// return the absolute output byte position the next `read()` will
    /// resume from.
    ///
    /// The mapping (BD-ROM AV §5.7 + §3.1):
    ///
    /// 1. Locate the PlayItem/clip whose title-relative time window
    ///    contains `pts_90k`.
    /// 2. Convert to a clip-local PTS by subtracting the clip's
    ///    title-start and adding the PlayItem IN-point PTS (the EP_map's
    ///    `pts_ep_start` values are clip-local).
    /// 3. Binary-search the clip's EP_map for the largest
    ///    `pts_ep_start ≤ clip-local target` — i.e. the I-frame at or
    ///    before the requested time (seeks land on a decodable boundary,
    ///    never mid-GOP).
    /// 4. The chosen entry's `spn_ep_start` is a source-packet number;
    ///    the `.m2ts` byte offset is `spn_ep_start × 192` (§3.1: each
    ///    source packet is 192 bytes). Output position is
    ///    `spn_ep_start × 188` within the clip.
    ///
    /// Clips with no CPI (homemade discs) seek to the clip start. A
    /// `pts_90k` past the title end clamps to the final clip's start.
    pub fn seek_to(&mut self, pts_90k: u64) -> io::Result<u64> {
        if self.clips.is_empty() {
            return Ok(0);
        }
        // 1. Pick the clip: the last clip whose title window starts at
        //    or before the target. (Windows are contiguous; a target
        //    past the last clip's start stays on the last clip.)
        let clip_idx = self
            .clips
            .iter()
            .rposition(|c| c.title_pts_start <= pts_90k)
            .unwrap_or(0);
        let clip = &self.clips[clip_idx];

        // 2. Title-relative → clip-local PTS.
        let into_clip = pts_90k.saturating_sub(clip.title_pts_start);
        let clip_local_target = clip.in_pts_90k.saturating_add(into_clip);

        // 3. Binary-search the EP_map for the entry at or before target.
        let spn = match clip.entry_points.as_slice() {
            [] => 0u32,
            eps => {
                let target = clip_local_target.min(u64::from(u32::MAX)) as u32;
                // Largest index with pts_ep_start <= target.
                let idx = match eps.binary_search_by(|&(pts, _)| pts.cmp(&target)) {
                    Ok(i) => i,
                    Err(0) => 0,
                    Err(i) => i - 1,
                };
                eps[idx].1
            }
        };

        // 4. Land the reader on source-packet `spn` of `clip_idx`.
        self.position_at(clip_idx, spn).map_err(|e| match e {
            BlurayError::Io(e) => e,
            other => io::Error::other(other.to_string()),
        })
    }

    /// Position the reader at source-packet `spn` of clip `clip_idx`,
    /// keeping decryption unit-aligned. Returns the new absolute output
    /// position.
    ///
    /// Decryption happens per 6144-byte AACS unit, so we open the file
    /// at the unit boundary at or below `spn × 192`, then drain the
    /// residual packets (output side) up to the exact entry point. This
    /// keeps the `clip_offset` the decryptor sees on a unit boundary.
    fn position_at(&mut self, clip_idx: usize, spn: u32) -> Result<u64> {
        // Copy the fields we need before taking `&mut self` for reads.
        let (stem, output_start, packet_count) = {
            let clip = &self.clips[clip_idx];
            (clip.stem.clone(), clip.output_start, clip.packet_count)
        };
        // Clamp the requested packet to the clip's usable range so a
        // stale / over-large EP_map entry can't seek past EOF.
        let spn = (spn as u64).min(packet_count.saturating_sub(1));

        // AACS-unit-aligned input byte where we begin reading.
        let target_byte = spn * M2TS_PACKET_LEN as u64;
        let unit_byte_start = target_byte - (target_byte % AACS_UNIT_LEN as u64);
        let unit_packet_start = unit_byte_start / M2TS_PACKET_LEN as u64;
        let residual_packets = spn - unit_packet_start;

        // Open the clip and seek the file to the unit boundary.
        self.clip_idx = clip_idx;
        let path = self.bdmv_root.join("STREAM").join(format!("{stem}.m2ts"));
        let mut f = File::open(&path).map_err(BlurayError::Io)?;
        f.seek(SeekFrom::Start(unit_byte_start))
            .map_err(BlurayError::Io)?;
        self.current = Some(f);
        self.clip_offset = unit_byte_start;
        self.pending.clear();
        self.pending_pos = 0;
        // Output position at the unit boundary, then advance past the
        // residual packets to land exactly on the entry point.
        self.output_pos = output_start + unit_packet_start * TS_PACKET_LEN as u64;

        let mut to_skip = residual_packets * TS_PACKET_LEN as u64;
        let mut sink = [0u8; 8192];
        while to_skip > 0 {
            let want = (to_skip as usize).min(sink.len());
            let n = Read::read(self, &mut sink[..want]).map_err(BlurayError::Io)?;
            if n == 0 {
                break;
            }
            to_skip -= n as u64;
        }
        Ok(self.output_pos)
    }

    fn open_next_clip(&mut self) -> Result<()> {
        self.current = None;
        if self.clip_idx >= self.clips.len() {
            return Ok(());
        }
        let stem = &self.clips[self.clip_idx].stem;
        let path = self.bdmv_root.join("STREAM").join(format!("{stem}.m2ts"));
        let f = File::open(&path).map_err(BlurayError::Io)?;
        self.current = Some(f);
        self.clip_offset = 0;
        Ok(())
    }

    /// Pull one AACS-unit worth (6144 bytes) of source packets from
    /// the current clip, decrypt it, strip TP_extra, and stage the
    /// resulting 188-byte TS bytes into `self.pending`.
    ///
    /// Returns Ok(true) if it produced output, Ok(false) at clip-list
    /// end-of-stream.
    fn refill(&mut self) -> Result<bool> {
        loop {
            if self.current.is_none() {
                return Ok(false);
            }
            let f = self.current.as_mut().unwrap();
            let mut unit = vec![0u8; AACS_UNIT_LEN];
            let mut filled = 0;
            while filled < AACS_UNIT_LEN {
                match f.read(&mut unit[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(BlurayError::Io(e)),
                }
            }
            if filled == 0 {
                // EOF on this clip — advance.
                self.clip_idx += 1;
                self.open_next_clip()?;
                continue;
            }

            // We need an exact multiple of 192 to strip TP_extra cleanly.
            // The .m2ts file should always be packet-aligned; if not,
            // truncate to the nearest packet boundary. (Padding the
            // tail with zeros would produce malformed TS packets.)
            let usable = filled - (filled % M2TS_PACKET_LEN);
            if usable == 0 {
                self.clip_idx += 1;
                self.open_next_clip()?;
                continue;
            }
            let unit_to_decrypt = &mut unit[..usable];
            // Decrypt: AACS expects 6144-byte alignment. If the
            // current chunk is less than that (final unit of the
            // clip), the trait impl is expected to ignore the tail.
            // We round down to the AACS unit boundary for the
            // decryption call and pass the residue through untouched.
            let dec_len = usable - (usable % AACS_UNIT_LEN);
            if dec_len > 0 {
                self.decryptor
                    .decrypt_units(&mut unit_to_decrypt[..dec_len], self.clip_offset)
                    .map_err(|e| BlurayError::Decrypt(e.to_string()))?;
            }
            self.clip_offset += usable as u64;

            // Strip TP_extra into pending.
            let n_pkts = usable / M2TS_PACKET_LEN;
            self.pending.resize(n_pkts * TS_PACKET_LEN, 0);
            self.pending_pos = 0;
            strip_tp_extra(unit_to_decrypt, &mut self.pending);
            return Ok(true);
        }
    }
}

impl Read for TitleSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        // Hard byte cap: stop reading once we've emitted more than
        // `output_total` (the precomputed title length). A consumer
        // that keeps asking past EOF — e.g. an mpeg-ts demuxer
        // unsuccessfully hunting for the 0x47 sync byte on
        // AES-scrambled bytes because AACS resolution failed —
        // would otherwise spin the optical drive reading the entire
        // title before giving up, putting the kernel I/O in
        // uninterruptible sleep for minutes.
        if self.output_pos >= self.output_total {
            return Ok(0);
        }
        if self.pending_pos >= self.pending.len() {
            match self.refill() {
                Ok(true) => {}
                Ok(false) => return Ok(0),
                Err(BlurayError::Io(e)) => return Err(e),
                Err(e) => return Err(io::Error::other(e.to_string())),
            }
        }
        let avail = self.pending.len() - self.pending_pos;
        let take = avail.min(out.len());
        out[..take].copy_from_slice(&self.pending[self.pending_pos..self.pending_pos + take]);
        self.pending_pos += take;
        self.output_pos += take as u64;
        Ok(take)
    }
}

// ─────────────────────── chapter byte segments ─────────────────────

/// One chapter's worth of decrypted, TP_extra-stripped MPEG-TS bytes,
/// emitted by [`ChapterSegments`].
///
/// `chapter_id` is 1-based and matches `Disc::chapters()` index + 1.
/// `start_pts_90k` / `end_pts_90k` are the title-relative PTS values
/// that bounded the request — keep in mind the *actual* byte range is
/// keyframe-rounded (see [`Disc::open_title_chapters`] for the
/// boundary caveat).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChapterSegment {
    /// 1-based chapter id (matches `Disc::chapters()` order + 1).
    pub chapter_id: u32,
    /// Title-relative start PTS in 90 kHz ticks (the chapter's mark).
    pub start_pts_90k: u64,
    /// Title-relative end PTS in 90 kHz ticks — next chapter's mark,
    /// or the title duration for the final chapter.
    pub end_pts_90k: u64,
    /// Decrypted M2TS bytes for this chapter — already TP-extra
    /// stripped by the underlying [`TitleSource`], so this is clean
    /// 188-byte MPEG-TS ready for a remuxer.
    pub bytes: Vec<u8>,
}

/// Internal request record: each item in this list becomes one
/// [`ChapterSegment`] on the iterator's output.
#[derive(Debug, Clone, Copy)]
struct ChapterRequest {
    chapter_id: u32,
    start_pts_90k: u64,
    end_pts_90k: u64,
    /// `true` when this request is the title's final chapter — read
    /// to EOF rather than seek to `end_pts_90k` (which would round
    /// DOWN to the last keyframe and drop the final GOP).
    ends_at_title_end: bool,
}

/// Iterator yielding one [`ChapterSegment`] per chapter requested via
/// [`Disc::open_title_chapters`].
///
/// Lazy: each `next()` call seeks the underlying [`TitleSource`] to
/// the chapter's start PTS, then reads bytes until reaching the next
/// chapter's start PTS (or the end of the title). The seeker is
/// keyframe-aligned per BD-ROM AV §5.7 — see
/// [`Disc::open_title_chapters`] for the resulting frame-boundary
/// caveat.
pub struct ChapterSegments {
    source: TitleSource,
    requests: Vec<ChapterRequest>,
}

impl std::fmt::Debug for ChapterSegments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChapterSegments")
            .field("remaining", &self.requests.len())
            .field("source", &self.source)
            .finish()
    }
}

impl ChapterSegments {
    /// Number of segments left to emit (handy for progress UIs).
    pub fn remaining(&self) -> usize {
        self.requests.len()
    }

    /// Read one chapter — resolve start + end byte offsets via
    /// [`TitleSource::seek_to`], then read the byte range. We
    /// intentionally re-use the same seek primitive for both ends so
    /// the boundary is keyframe-rounded the same way at the seam.
    ///
    /// For the title's final chapter (`ends_at_title_end == true`),
    /// the end byte is the title's EOF rather than the seeker output:
    /// `seek_to(title_duration)` rounds DOWN to the last keyframe and
    /// would drop the closing GOP.
    fn read_one(&mut self, req: &ChapterRequest) -> Result<ChapterSegment> {
        // End byte: either the title's total output size (last
        // chapter) or the keyframe at-or-before the next chapter's
        // mark. Reading `output_total()` rather than `seek(End(0))`
        // avoids consuming bytes just to discover the EOF offset on
        // a real (multi-GB) title.
        let end_byte = if req.ends_at_title_end {
            self.source.output_total()
        } else {
            self.source
                .seek_to(req.end_pts_90k)
                .map_err(BlurayError::Io)?
        };
        let start_byte = self
            .source
            .seek_to(req.start_pts_90k)
            .map_err(BlurayError::Io)?;

        // Defensive: if start >= end (would happen on a degenerate
        // chapter whose mark happens to land exactly at the title end),
        // emit an empty segment rather than read backwards.
        if start_byte >= end_byte {
            return Ok(ChapterSegment {
                chapter_id: req.chapter_id,
                start_pts_90k: req.start_pts_90k,
                end_pts_90k: req.end_pts_90k,
                bytes: Vec::new(),
            });
        }
        let want = (end_byte - start_byte) as usize;
        let mut bytes = Vec::with_capacity(want);
        let mut taken: usize = 0;
        let mut buf = [0u8; 64 * 1024];
        while taken < want {
            let n = (want - taken).min(buf.len());
            let got = self.source.read(&mut buf[..n]).map_err(BlurayError::Io)?;
            if got == 0 {
                // Short read: the source hit EOF before reaching the
                // resolved end byte. Could happen if the end seek
                // returned an over-estimate at the title boundary;
                // we just truncate the chapter to what we got.
                break;
            }
            bytes.extend_from_slice(&buf[..got]);
            taken += got;
        }
        Ok(ChapterSegment {
            chapter_id: req.chapter_id,
            start_pts_90k: req.start_pts_90k,
            end_pts_90k: req.end_pts_90k,
            bytes,
        })
    }
}

impl Iterator for ChapterSegments {
    type Item = Result<ChapterSegment>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.requests.is_empty() {
            return None;
        }
        let req = self.requests.remove(0);
        Some(self.read_one(&req))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.requests.len(), Some(self.requests.len()))
    }
}

/// Disc-level title metadata returned by [`Disc::title_meta`].
///
/// Optional fields stay `Option<_>` because META XML authoring is
/// inconsistent across publishers — many discs omit alternate names,
/// thumbnails, etc., and the `<di:name>` element itself is the only
/// universally-shipped field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscTitleMeta {
    /// The disc's display title — content of the `<di:name>` element
    /// (the BD-ROM spec's "DiscInfo name").
    pub title: String,
    /// 3-letter ISO 639-2/T language tag carried in the META filename
    /// suffix (e.g. `bdmt_eng.xml` → `Some("eng")`).
    pub language: Option<String>,
}

/// Extract the contents of the first `<di:name>` (or `<name>`) element
/// out of a BDMV META XML byte buffer. Pure byte scan — keeps the
/// crate dep-free of any XML parser. Robust against the two namespace
/// spellings actually authored in the wild; trims surrounding
/// whitespace.
///
/// Returns `None` if the buffer doesn't contain a recognisable name
/// element. Stops at the first match — META files only carry one
/// `<di:name>` at the top level per BD-ROM Part 3 §5.7.4.
fn extract_di_name(bytes: &[u8]) -> Option<String> {
    // Try both spellings the wild-type authoring tools have emitted.
    for open in [b"<di:name".as_slice(), b"<name".as_slice()] {
        let Some(start) = find_subsequence(bytes, open) else {
            continue;
        };
        // Skip past `<di:name` then scan for the `>` that closes the
        // start tag (allowing for attributes like `xml:lang="en"`).
        let after_open = start + open.len();
        let Some(rel) = bytes[after_open..].iter().position(|&b| b == b'>') else {
            continue;
        };
        let body_start = after_open + rel + 1;
        // Find the matching close tag. We match the exact byte
        // sequence so `<di:name>` pairs with `</di:name>` and `<name>`
        // pairs with `</name>` — mixing the two would be malformed XML
        // and we reject it by returning None.
        let close = match open {
            b"<di:name" => b"</di:name>".as_slice(),
            b"<name" => b"</name>".as_slice(),
            _ => unreachable!(),
        };
        let Some(rel_end) = find_subsequence(&bytes[body_start..], close) else {
            continue;
        };
        let body = &bytes[body_start..body_start + rel_end];
        let s = std::str::from_utf8(body).ok()?.trim().to_string();
        if s.is_empty() {
            return None;
        }
        return Some(s);
    }
    None
}

/// O(n·m) byte-substring search — tiny inputs (META XML files are
/// kilobytes), so the naïve scan is fine and saves a regex dep.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ─────────────────────── track catalogue ───────────────────────

/// Stream class for one entry in a [`TrackCatalogue`].
///
/// Mirrors the seven counted STN_table classes (BD-ROM Part 3
/// §5.4.4.4) — every elementary stream a title can carry maps onto
/// exactly one of these. We collapse the parser's per-class struct
/// types (`PrimaryVideoStream`, `PrimaryAudioStream`, …) into a single
/// enum so a remuxer can iterate one flat list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrackKind {
    /// Primary video — the title's main video stream.
    PrimaryVideo,
    /// Primary audio — the title's main audio mix.
    PrimaryAudio,
    /// Presentation Graphic Stream (BD bitmap subtitle).
    PgSubtitle,
    /// Interactive Graphic Stream (on-disc menu overlay).
    IgMenu,
    /// Secondary audio (PiP / commentary mixdown).
    SecondaryAudio,
    /// Secondary video (Picture-in-Picture overlay).
    SecondaryVideo,
    /// Picture-in-Picture Presentation Graphic Stream.
    PipPgSubtitle,
}

/// One elementary stream a title carries, as listed in every PlayItem's
/// STN_table. Returned by [`Disc::title_streams`].
///
/// `language_code` is `None` for video / IG / PiP PG entries (the
/// stream class doesn't carry one) and for audio / PG / text-subtitle
/// entries whose `language_code` field is the spec sentinel `b"\0\0\0"`
/// (the "language not specified" pad many discs ship for the primary
/// audio mix).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    /// MPEG-TS elementary PID carrying this stream. Stable across the
    /// whole title (BD-ROM AV §5.2.3.3 requires the PID assignment to
    /// be consistent across all PlayItems and all angles).
    pub pid: u16,
    pub kind: TrackKind,
    /// Spec coding type — `Mpeg2Video` / `AvcVideo` / `Ac3Audio` /
    /// `DtsHdMaAudio` / `PgsSubtitle` etc. (BD-ROM Part 3 §5.4.4.4
    /// `stream_coding_type`).
    pub coding_type: StreamCodingType,
    /// ISO 639-2/T 3-letter language tag. `None` when the stream class
    /// doesn't carry one, when the field is unset, or when the raw
    /// bytes don't decode as ASCII.
    pub language: Option<String>,
    /// Number of PlayItems in the title's PlayList that listed this
    /// PID in their STN_table. For a single-angle conformant title
    /// this equals the PlayItem count (every PI lists the same PIDs);
    /// a value lower than the PlayItem count indicates a per-PlayItem
    /// override (e.g. a clip that drops a commentary track).
    pub playitem_count: u32,
}

impl Track {
    /// A one-line UI label for this track: the coding type's
    /// [`StreamCodingType::display_name`] with the language appended in
    /// parentheses when known — e.g. `"Dolby TrueHD (eng)"`,
    /// `"PGS Subtitle (jpn)"`, or just `"H.265/HEVC Video"` for a video
    /// track (which carries no language). Pure derivation over the
    /// already-resolved `coding_type` + `language`.
    pub fn label(&self) -> String {
        match &self.language {
            Some(lang) => format!("{} ({lang})", self.coding_type.display_name()),
            None => self.coding_type.display_name(),
        }
    }
}

/// Aggregated per-track listing for a Blu-ray title — one entry per
/// distinct (PID, [`TrackKind`]) pair across every PlayItem in the
/// PlayList.
///
/// Order matches the canonical STN class order documented on
/// [`Disc::title_streams`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackCatalogue {
    pub tracks: Vec<Track>,
}

impl TrackCatalogue {
    /// Number of tracks in the catalogue.
    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    /// True when the catalogue is empty (parse failure or
    /// genuinely-track-free placeholder title).
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// Iterate over tracks of a given kind, in catalogue order.
    pub fn by_kind(&self, kind: TrackKind) -> impl Iterator<Item = &Track> {
        self.tracks.iter().filter(move |t| t.kind == kind)
    }

    /// First track with the given PID, if any. PIDs are unique within
    /// a Blu-ray title's PlayList (per AV §5.2.3.3), so this returns
    /// the canonical Track for that elementary stream.
    pub fn by_pid(&self, pid: u16) -> Option<&Track> {
        self.tracks.iter().find(|t| t.pid == pid)
    }
}

/// Decode a 3-byte ISO 639-2/T language code into a 3-letter String.
/// Returns `None` for the spec sentinel `b"\0\0\0"` (the "unset"
/// pad) and for any byte sequence that isn't printable ASCII (the
/// spec allows author-private extension here).
fn decode_lang_code(raw: [u8; 3]) -> Option<String> {
    if raw == [0, 0, 0] {
        return None;
    }
    if !raw.iter().all(|&b| b.is_ascii_graphic()) {
        return None;
    }
    // Lowercase per ISO 639-2/T convention so a downstream remuxer
    // emits `eng` / `jpn` regardless of whether the disc author wrote
    // `ENG` / `JPN` (some do).
    let s: String = raw
        .iter()
        .map(|&b| (b as char).to_ascii_lowercase())
        .collect();
    Some(s)
}

/// Lift every PlayItem's STN_table into a single deduplicated
/// [`TrackCatalogue`]. See [`Disc::title_streams`] for details.
fn build_track_catalogue(pl: &PlayListMpls) -> TrackCatalogue {
    // Track records pushed in first-seen order; the canonical STN
    // class order falls out of the inner walk order below.
    let mut tracks: Vec<Track> = Vec::new();
    let mut index_by_key: std::collections::HashMap<(u16, TrackKind), usize> =
        std::collections::HashMap::new();

    let bump = |tracks: &mut Vec<Track>,
                index_by_key: &mut std::collections::HashMap<(u16, TrackKind), usize>,
                pid: u16,
                kind: TrackKind,
                coding_type: StreamCodingType,
                language: Option<String>| {
        if pid == 0 {
            // Non-in-mux entries (stream_type 2/3/4) carry a SubPath
            // reference rather than a direct PID; the parser leaves
            // the PID at 0 for those. Skip — there's no main-TS PID
            // for a remuxer to label.
            return;
        }
        let key = (pid, kind);
        if let Some(&idx) = index_by_key.get(&key) {
            tracks[idx].playitem_count += 1;
        } else {
            let idx = tracks.len();
            tracks.push(Track {
                pid,
                kind,
                coding_type,
                language,
                playitem_count: 1,
            });
            index_by_key.insert(key, idx);
        }
    };

    for pi in &pl.play_list.play_items {
        for s in &pi.stn_table.primary_video {
            bump(
                &mut tracks,
                &mut index_by_key,
                s.elementary_pid,
                TrackKind::PrimaryVideo,
                s.coding_type,
                None,
            );
        }
        for s in &pi.stn_table.primary_audio {
            bump(
                &mut tracks,
                &mut index_by_key,
                s.elementary_pid,
                TrackKind::PrimaryAudio,
                s.coding_type,
                decode_lang_code(s.language_code),
            );
        }
        for s in &pi.stn_table.pg_subtitles {
            bump(
                &mut tracks,
                &mut index_by_key,
                s.elementary_pid,
                TrackKind::PgSubtitle,
                s.coding_type,
                decode_lang_code(s.language_code),
            );
        }
        for s in &pi.stn_table.ig_streams {
            bump(
                &mut tracks,
                &mut index_by_key,
                s.elementary_pid,
                TrackKind::IgMenu,
                s.coding_type,
                decode_lang_code(s.language_code),
            );
        }
        for s in &pi.stn_table.secondary_audio {
            bump(
                &mut tracks,
                &mut index_by_key,
                s.elementary_pid,
                TrackKind::SecondaryAudio,
                s.coding_type,
                decode_lang_code(s.language_code),
            );
        }
        for s in &pi.stn_table.secondary_video {
            bump(
                &mut tracks,
                &mut index_by_key,
                s.elementary_pid,
                TrackKind::SecondaryVideo,
                s.coding_type,
                None,
            );
        }
        for s in &pi.stn_table.pip_pg {
            bump(
                &mut tracks,
                &mut index_by_key,
                s.elementary_pid,
                TrackKind::PipPgSubtitle,
                s.coding_type,
                decode_lang_code(s.language_code),
            );
        }
    }

    // Sort by class (canonical STN order) then by first-seen index.
    // Currently the input order already matches class order across
    // PlayItems, but if a downstream parser ever introduced an
    // out-of-order STN class the explicit sort keeps the invariant.
    tracks.sort_by_key(|t| (class_order(t.kind), t.pid));
    TrackCatalogue { tracks }
}

/// Canonical ordering rank for [`TrackKind`] — matches the STN_table
/// class declaration order in BD-ROM Part 3 §5.4.4.4.
fn class_order(kind: TrackKind) -> u8 {
    match kind {
        TrackKind::PrimaryVideo => 0,
        TrackKind::PrimaryAudio => 1,
        TrackKind::PgSubtitle => 2,
        TrackKind::IgMenu => 3,
        TrackKind::SecondaryAudio => 4,
        TrackKind::SecondaryVideo => 5,
        TrackKind::PipPgSubtitle => 6,
    }
}

/// Sorted, deduplicated set of 3-letter language tags pulled from a
/// PlayList's STN_table audio + PG + IG + PiP PG entries. Used by
/// [`Disc::mount`] to populate [`TitleInfo::languages`] without re-
/// reading the .mpls per title.
fn collect_languages(pl: &PlayListMpls) -> Vec<String> {
    let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for pi in &pl.play_list.play_items {
        for s in &pi.stn_table.primary_audio {
            if let Some(l) = decode_lang_code(s.language_code) {
                set.insert(l);
            }
        }
        for s in &pi.stn_table.pg_subtitles {
            if let Some(l) = decode_lang_code(s.language_code) {
                set.insert(l);
            }
        }
        for s in &pi.stn_table.ig_streams {
            if let Some(l) = decode_lang_code(s.language_code) {
                set.insert(l);
            }
        }
        for s in &pi.stn_table.secondary_audio {
            if let Some(l) = decode_lang_code(s.language_code) {
                set.insert(l);
            }
        }
        for s in &pi.stn_table.pip_pg {
            if let Some(l) = decode_lang_code(s.language_code) {
                set.insert(l);
            }
        }
    }
    set.into_iter().collect()
}

// ─────────────────────── helpers ───────────────────────

fn playlist_path(bdmv: &Path, id: u16) -> PathBuf {
    bdmv.join("PLAYLIST").join(format!("{id:05}.mpls"))
}

fn read_file(path: &Path) -> Result<Vec<u8>> {
    let mut f = File::open(path)?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Best-effort `CLIPINF/<stem>.clpi` lift. Returns:
///
/// * **`entry_points`** — the primary EP_map (BD-ROM AV §5.7), flattened
///   into an ascending `(pts_ep_start, spn_ep_start)` list. "Primary" =
///   the EP_map with the lowest `stream_pid`; on a conformant BD-ROM
///   the primary video PID is always the lowest video PID. Sorted
///   defensively against a malformed (non-monotonic) table.
/// * **`stc_origin_pts_90k`** — the 90 kHz clip-local PTS at which the
///   STC sequence indexed by `stc_id_ref` begins, lifted from
///   `SequenceInfo` (§5.5.4.2 `presentation_start_time`, stored in
///   45 kHz units, doubled here for parity with the seek pipeline).
/// * **`angle_change_eps`** — subset of `entry_points` whose CPI
///   EP_fine row had its `is_angle_change_point` bit set. These are
///   the source packets at which a mid-stream angle switch can land
///   without resetting decoder state (BD-ROM AV §5.7 + Part 3
///   §5.4.4.1 `is_multi_angle` block). Ascending by PTS. Empty for
///   single-angle clips.
///
/// Both fall back gracefully:
///
/// - missing / corrupt `.clpi` → empty EP list + `stc_origin_pts_90k = 0`
///   + empty `angle_change_eps`;
/// - clip with an EP_map but no SequenceInfo → empty EP list passes
///   through unchanged, `stc_origin_pts_90k = 0`;
/// - `stc_id_ref` past the end of the first ATC sequence's STC list →
///   `stc_origin_pts_90k = 0` (no usable origin advertised).
///
/// The caller compares the resulting `stc_origin_pts_90k` against the
/// PlayItem's `in_pts_90k` to decide whether the clip really uses the
/// SequenceInfo origin or whether to fall back to the PlayItem IN
/// point — see [`TitleSource::new`] for the policy.
/// `(entry_points, stc_origin_pts_90k, angle_change_eps)` — return
/// shape of [`load_clip_meta`]. Aliased so the clippy
/// `type_complexity` lint stays happy without dropping back onto an
/// inline struct (the three fields are independent units of data, so
/// a tuple is the most ergonomic call shape).
type ClipMetaTriple = (Vec<(u32, u32)>, u64, Vec<(u32, u32)>);

/// Build the per-clip seek index for `play_items` at angle `angle`.
///
/// Returns `(clips, output_total, title_duration_90k)` where:
///
/// - `clips` is one [`ClipSeekInfo`] per PlayItem in playback order,
///   with each clip's `.m2ts` size measured, its `.clpi` parsed for
///   EP_map + STC origin + angle-change rows, and its running
///   output-byte / title-PTS starts threaded in.
/// - `output_total` sums each clip's `packet_count × 188` (post
///   TP_extra strip) — used as the title's hard byte cap.
/// - `title_duration_90k` sums every PlayItem's `(OUT − IN) × 2`
///   (§5.4.4.1 IN/OUT pair) — used by
///   [`TitleSource::pts_continuity_segments`] to land the final
///   segment's `title_pts_end`.
///
/// Used by [`TitleSource::new`] (on title open) and by
/// [`TitleSource::switch_angle_at`] (rebuild for a different angle —
/// the clips' `stem` / `packet_count` / EP_map are angle-specific, the
/// running starts are not). Callers must validate `angle` against
/// every PlayItem before invoking — this helper falls back to the
/// PlayItem's primary clip when an out-of-range angle is passed,
/// because that's the safest available output for a partially-built
/// state, but the calling surface should never let that path trip.
fn build_clip_seek_index(
    bdmv_root: &Path,
    play_items: &[PlayItem],
    angle: u8,
) -> (Vec<ClipSeekInfo>, u64, u64) {
    let mut clips = Vec::with_capacity(play_items.len());
    let mut output_total: u64 = 0;
    let mut title_pts_start: u64 = 0;
    for pi in play_items {
        let angle_ref = pi.angle_clip(angle);
        let stem = angle_ref
            .map(|a| a.clip_information_file_name.to_string())
            .unwrap_or_else(|| pi.clip_information_file_name.clone());
        // The angle clip carries its own `stc_id_ref` (§5.4.4.1's
        // is_multi_angle block); fall back to the primary-PlayItem
        // value when the angle list is empty.
        let stc_id_ref = angle_ref.map(|a| a.stc_id_ref).unwrap_or(pi.stc_id_ref);
        let m2ts_path = bdmv_root.join("STREAM").join(format!("{stem}.m2ts"));
        let packet_count = match std::fs::metadata(&m2ts_path) {
            Ok(meta) => {
                let raw = meta.len();
                let usable = raw - (raw % M2TS_PACKET_LEN as u64);
                usable / M2TS_PACKET_LEN as u64
            }
            Err(_) => 0,
        };

        let (entry_points, stc_origin_pts_90k, angle_change_eps) =
            load_clip_meta(bdmv_root, &stem, stc_id_ref);

        clips.push(ClipSeekInfo {
            stem,
            output_start: output_total,
            packet_count,
            title_pts_start,
            in_pts_90k: u64::from(pi.in_time_ticks) * 2,
            entry_points,
            angle_change_eps,
            connection_condition: pi.connection_condition,
            stc_id_ref,
            stc_origin_pts_90k,
        });

        output_total += packet_count * TS_PACKET_LEN as u64;
        title_pts_start += pi.duration_90k();
    }
    (clips, output_total, title_pts_start)
}

fn load_clip_meta(bdmv_root: &Path, stem: &str, stc_id_ref: u8) -> ClipMetaTriple {
    let path = bdmv_root.join("CLIPINF").join(format!("{stem}.clpi"));
    let Ok(bytes) = read_file(&path) else {
        return (Vec::new(), 0, Vec::new());
    };
    let Ok(clpi) = ClipInformation::parse(&bytes) else {
        return (Vec::new(), 0, Vec::new());
    };
    // Principled pick: prefer the EP_map whose 4-bit EP_stream_type
    // names a known BD video bitstream (HEVC > MPEG-2 / AVC / VC-1)
    // before falling back to the lowest-PID heuristic — covers UHD-BD
    // titles that interleave an AVC fallback EP_map alongside the
    // HEVC main on a single clip.
    let primary_ep = clpi.cpi.primary_video_ep_map();
    let mut eps: Vec<(u32, u32)> = match primary_ep {
        Some(ep) => ep
            .entries
            .iter()
            .map(|e| (e.pts_ep_start, e.spn_ep_start))
            .collect(),
        None => Vec::new(),
    };
    eps.sort_by_key(|&(pts, _)| pts);

    // Angle-change EP_fine rows (§5.7 bit field). A row's
    // `is_angle_change_point` advertises that the corresponding source
    // packet is the head of a video access unit at which a mid-stream
    // angle switch can land without resetting decoder state. Tracked
    // off the same primary-video EP_map; sorted by PTS for predictable
    // search.
    let mut angle_change_eps: Vec<(u32, u32)> = match primary_ep {
        Some(ep) => ep
            .entries
            .iter()
            .filter(|e| e.is_angle_change_point)
            .map(|e| (e.pts_ep_start, e.spn_ep_start))
            .collect(),
        None => Vec::new(),
    };
    angle_change_eps.sort_by_key(|&(pts, _)| pts);

    // SequenceInfo: §5.5.4.2 — one ATC sequence per clip in the
    // overwhelming majority of authoring patterns; the per-PlayItem
    // `stc_id_ref` indexes the STC sequence list inside that ATC entry.
    // A malformed clip with zero ATC sequences, or a `stc_id_ref` past
    // the last STC slot, yields the safe `0` fallback — downstream code
    // in `TitleSource::new` then defaults to the PlayItem's IN point.
    let stc_origin_pts_90k = clpi
        .sequence_info
        .atc_sequences
        .first()
        .and_then(|atc| atc.stc_sequences.get(stc_id_ref as usize))
        .map(|stc| u64::from(stc.presentation_start_time) * 2)
        .unwrap_or(0);

    (eps, stc_origin_pts_90k, angle_change_eps)
}

/// Resolve an `IndexEntry` to (kind, playlist_id). For HDMV we use
/// the entry's `movie_object_id_ref` as the playlist id heuristic
/// (this matches the very-common case where movie objects are
/// numbered to align with PlayLists; if it doesn't match we fall
/// back to id = 0 silently — the consumer will see duration 0 and
/// can skip the title).
fn resolve_title_playlist(_bdmv: &Path, entry: &IndexEntry) -> Result<(TitleKind, u16)> {
    match &entry.object {
        IndexObjectType::Hdmv {
            movie_object_id_ref,
            ..
        } => Ok((TitleKind::Hdmv, *movie_object_id_ref)),
        IndexObjectType::BdJ { .. } => Ok((TitleKind::BdJ, 0)),
    }
}

// `Seek` is the *byte-exact* contract demuxers expect: position query,
// rewind-to-start, end-position query for size discovery, forward-skip,
// and (via rewind + forward-skip) backwards-seek to any byte offset.
// For *keyframe-aligned* time seeks — landing on a decodable I-frame
// boundary rather than an arbitrary byte — use [`TitleSource::seek_to`],
// which consults the CPI EP_map (BD-ROM AV §5.7).
impl Seek for TitleSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        // Resolve every variant to an absolute output offset.
        let target: i128 = match pos {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(d) => self.output_pos as i128 + d as i128,
            SeekFrom::End(d) => self.output_total as i128 + d as i128,
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "TitleSource: seek before start",
            ));
        }
        let target = target as u64;

        // No-op seek (common probe pattern).
        if target == self.output_pos {
            return Ok(self.output_pos);
        }

        // Backwards seek (or a forward seek into an earlier clip than
        // the cursor): jump straight to the start of the clip that
        // contains `target` using the per-clip output-offset index,
        // then fall through to the bounded forward-skip below. This
        // avoids re-reading every preceding clip on a large rewind —
        // the linear scan is now at most one clip's worth of packets.
        if target < self.output_pos {
            let clip_idx = self
                .clips
                .iter()
                .rposition(|c| c.output_start <= target)
                .unwrap_or(0);
            self.clip_idx = clip_idx;
            self.current = None;
            self.clip_offset = 0;
            self.pending.clear();
            self.pending_pos = 0;
            self.output_pos = self.clips[clip_idx].output_start;
            self.open_next_clip().map_err(|e| match e {
                BlurayError::Io(e) => e,
                other => io::Error::other(other.to_string()),
            })?;
        }

        // Forward: read + discard until we reach target. Bounded by
        // the precomputed total so a runaway seek-past-end caps out
        // at the end-of-stream rather than spinning forever.
        let cap = target.min(self.output_total);
        let mut sink = [0u8; 8192];
        while self.output_pos < cap {
            let want = ((cap - self.output_pos) as usize).min(sink.len());
            let n = Read::read(self, &mut sink[..want])?;
            if n == 0 {
                break;
            }
        }
        Ok(self.output_pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn title(id: u16, playlist_id: u16, duration_ticks: u64) -> TitleInfo {
        TitleInfo {
            id,
            kind: TitleKind::Hdmv,
            playlist_id,
            duration_ticks,
            languages: Vec::new(),
        }
    }

    #[test]
    fn unique_titles_dedup_by_playlist_id() {
        // Multiple TitleInfo entries pointing at the same playlist (the
        // commercial-disc anti-rip pattern). `unique_titles` keeps the
        // lowest-id title per playlist.
        let disc = Disc {
            root: PathBuf::from("/dev/null"),
            titles: vec![
                title(1, 100, 90_000 * 60),
                title(2, 100, 90_000 * 60), // same playlist as title 1 → dropped
                title(3, 200, 90_000 * 30),
                title(4, 100, 90_000 * 60), // dup of title 1 again → dropped
                title(5, 300, 90_000 * 10),
            ],
        };
        let unique = disc.unique_titles();
        assert_eq!(unique.len(), 3);
        assert_eq!(unique[0].id, 1);
        assert_eq!(unique[0].playlist_id, 100);
        assert_eq!(unique[1].id, 3);
        assert_eq!(unique[1].playlist_id, 200);
        assert_eq!(unique[2].id, 5);
        assert_eq!(unique[2].playlist_id, 300);
    }

    #[test]
    fn unique_titles_skips_placeholder_4000() {
        // playlist_id == 0x4000 is the BD-ROM "no-op" sentinel (BD-J /
        // menu wiring). `unique_titles` must skip it even when it's the
        // first / only entry pointing at that id.
        let disc = Disc {
            root: PathBuf::from("/dev/null"),
            titles: vec![
                title(1, 0x4000, 0),
                title(2, 100, 90_000 * 30),
                title(3, 0x4000, 0),
                title(4, 100, 90_000 * 30),
            ],
        };
        let unique = disc.unique_titles();
        assert_eq!(unique.len(), 1);
        assert_eq!(unique[0].id, 2);
        assert_eq!(unique[0].playlist_id, 100);
    }

    #[test]
    fn unique_titles_empty_list_yields_empty_result() {
        let disc = Disc {
            root: PathBuf::from("/dev/null"),
            titles: vec![],
        };
        assert!(disc.unique_titles().is_empty());
    }

    #[test]
    fn extract_di_name_handles_namespaced_form() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<disclibrary xmlns:di="urn:BDA:bdmv;discinfo">
  <di:discinfo>
    <di:title>
      <di:name>My Movie</di:name>
      <di:numSets>1</di:numSets>
    </di:title>
  </di:discinfo>
</disclibrary>"#;
        assert_eq!(extract_di_name(xml), Some("My Movie".to_string()));
    }

    #[test]
    fn extract_di_name_handles_unqualified_name() {
        let xml = br#"<root><name>Plain</name></root>"#;
        assert_eq!(extract_di_name(xml), Some("Plain".to_string()));
    }

    #[test]
    fn extract_di_name_returns_none_when_absent() {
        let xml = br#"<root><title>Not a name</title></root>"#;
        assert_eq!(extract_di_name(xml), None);
    }

    #[test]
    fn extract_di_name_trims_whitespace() {
        let xml = b"<di:name>   Padded   </di:name>";
        assert_eq!(extract_di_name(xml), Some("Padded".to_string()));
    }

    #[test]
    fn extract_di_name_empty_element_yields_none() {
        let xml = b"<di:name></di:name>";
        assert_eq!(extract_di_name(xml), None);
    }

    #[test]
    fn extract_di_name_skips_attributes_on_start_tag() {
        // Authors frequently ship `<di:name xml:lang="en">...</di:name>`.
        let xml = br#"<di:name xml:lang="en">Localised</di:name>"#;
        assert_eq!(extract_di_name(xml), Some("Localised".to_string()));
    }

    fn make_test_dir(suffix: &str) -> PathBuf {
        let pid = std::process::id();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let p = std::env::temp_dir().join(format!("oxideav-bluray-{suffix}-{pid}-{nonce}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn empty_disc(root: PathBuf) -> Disc {
        Disc {
            root,
            titles: vec![],
        }
    }

    #[test]
    fn title_meta_returns_none_when_meta_directory_absent() {
        let root = make_test_dir("meta-none");
        let disc = empty_disc(root.clone());
        assert_eq!(disc.title_meta(), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn title_meta_returns_none_when_meta_directory_empty() {
        let root = make_test_dir("meta-empty");
        std::fs::create_dir_all(root.join("BDMV/META/DL")).unwrap();
        let disc = empty_disc(root.clone());
        assert_eq!(disc.title_meta(), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn title_meta_reads_bdmt_en_xml_with_language_suffix() {
        let root = make_test_dir("meta-en");
        let dl = root.join("BDMV/META/DL");
        std::fs::create_dir_all(&dl).unwrap();
        let xml = br#"<?xml version="1.0"?>
<disclibrary xmlns:di="urn:BDA:bdmv;discinfo">
  <di:discinfo>
    <di:title>
      <di:name>Kite Uncut</di:name>
    </di:title>
  </di:discinfo>
</disclibrary>"#;
        std::fs::write(dl.join("bdmt_eng.xml"), xml).unwrap();
        let disc = empty_disc(root.clone());
        let meta = disc.title_meta().expect("META present");
        assert_eq!(meta.title, "Kite Uncut");
        assert_eq!(meta.language.as_deref(), Some("eng"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn decode_lang_code_rejects_unset_sentinel() {
        assert_eq!(decode_lang_code([0, 0, 0]), None);
    }

    #[test]
    fn decode_lang_code_rejects_non_ascii() {
        assert_eq!(decode_lang_code([0xFF, 0xFE, 0xFD]), None);
        assert_eq!(decode_lang_code([0, b'e', b'n']), None);
    }

    #[test]
    fn decode_lang_code_lowercases_uppercase_authoring() {
        assert_eq!(decode_lang_code(*b"ENG"), Some("eng".into()));
        assert_eq!(decode_lang_code(*b"jpn"), Some("jpn".into()));
        assert_eq!(decode_lang_code(*b"FrA"), Some("fra".into()));
    }

    #[test]
    fn class_order_is_monotonic_and_matches_spec_order() {
        // BD-ROM Part 3 §5.4.4.4 declaration order: primary video,
        // primary audio, PG, IG, secondary audio, secondary video, PiP PG.
        let order = [
            TrackKind::PrimaryVideo,
            TrackKind::PrimaryAudio,
            TrackKind::PgSubtitle,
            TrackKind::IgMenu,
            TrackKind::SecondaryAudio,
            TrackKind::SecondaryVideo,
            TrackKind::PipPgSubtitle,
        ];
        for (i, k) in order.iter().enumerate() {
            assert_eq!(class_order(*k) as usize, i);
        }
    }

    #[test]
    fn title_meta_ignores_non_matching_filenames() {
        // A stray `.xml` that isn't a `bdmt_*.xml` must not be parsed.
        let root = make_test_dir("meta-stray");
        let dl = root.join("BDMV/META/DL");
        std::fs::create_dir_all(&dl).unwrap();
        std::fs::write(dl.join("README.xml"), b"<root><name>nope</name></root>").unwrap();
        let disc = empty_disc(root.clone());
        assert_eq!(disc.title_meta(), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
