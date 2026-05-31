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
use crate::bdmv::mpls::{Chapter, PlayItem, PlayListMpls, StreamCodingType};
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
        // Build the per-clip seek index in playback order. For each
        // PlayItem we:
        //   - select the requested angle's clip stem (caller has
        //     already validated `angle` against every PlayItem);
        //   - measure the `.m2ts` size → usable packet count → the
        //     output bytes it contributes (188/192 ratio);
        //   - parse its `.clpi` (best-effort) to lift the primary
        //     EP_map into a flat ascending `(pts, spn)` list;
        //   - record the running output-byte start + title-relative
        //     90 kHz PTS start so a seek can locate the clip.
        let mut clips = Vec::with_capacity(play_items.len());
        let mut output_total: u64 = 0;
        let mut title_pts_start: u64 = 0;
        for pi in &play_items {
            let stem = pi
                .angle_clip(angle)
                .map(|a| a.clip_information_file_name.to_string())
                .unwrap_or_else(|| pi.clip_information_file_name.clone());
            let m2ts_path = bdmv_root.join("STREAM").join(format!("{stem}.m2ts"));
            let packet_count = match std::fs::metadata(&m2ts_path) {
                Ok(meta) => {
                    let raw = meta.len();
                    let usable = raw - (raw % M2TS_PACKET_LEN as u64);
                    usable / M2TS_PACKET_LEN as u64
                }
                Err(_) => 0,
            };

            let entry_points = load_entry_points(&bdmv_root, &stem);

            clips.push(ClipSeekInfo {
                stem,
                output_start: output_total,
                packet_count,
                title_pts_start,
                in_pts_90k: u64::from(pi.in_time_ticks) * 2,
                entry_points,
            });

            output_total += packet_count * TS_PACKET_LEN as u64;
            title_pts_start += pi.duration_90k();
        }

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
        };
        s.open_next_clip()?;
        Ok(s)
    }

    /// Total output bytes the title will produce when read to EOF
    /// (sum of each `.m2ts` size × 188 / 192, truncated to packet
    /// boundaries). Computed once at construction; constant for the
    /// lifetime of the source.
    pub fn output_total(&self) -> u64 {
        self.output_total
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

/// Best-effort: parse `CLIPINF/<stem>.clpi`, pick the primary EP_map,
/// and lift it into an ascending `(pts_ep_start, spn_ep_start)` list a
/// seeker can binary-search. A missing / corrupt / CPI-less `.clpi`
/// yields an empty list — seeking then falls back to the clip start
/// rather than failing the whole title.
///
/// "Primary" EP_map: the EP_map with the lowest `stream_pid` (the
/// primary video PID is always the lowest-numbered video elementary
/// stream on a conformant BD-ROM, so its EP_map carries the I-frame
/// boundaries we want to land on). The rows are sorted by
/// `pts_ep_start` defensively — they're already monotonic on a
/// conformant disc, but a stable sort makes the binary search correct
/// even on a malformed table.
fn load_entry_points(bdmv_root: &Path, stem: &str) -> Vec<(u32, u32)> {
    let path = bdmv_root.join("CLIPINF").join(format!("{stem}.clpi"));
    let Ok(bytes) = read_file(&path) else {
        return Vec::new();
    };
    let Ok(clpi) = ClipInformation::parse(&bytes) else {
        return Vec::new();
    };
    let Some(ep) = clpi.cpi.ep_map.iter().min_by_key(|m| m.stream_pid) else {
        return Vec::new();
    };
    let mut eps: Vec<(u32, u32)> = ep
        .entries
        .iter()
        .map(|e| (e.pts_ep_start, e.spn_ep_start))
        .collect();
    eps.sort_by_key(|&(pts, _)| pts);
    eps
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
