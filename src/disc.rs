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
use crate::bdmv::mpls::{Chapter, PlayItem, PlayListMpls};
use crate::decrypt::{StreamDecryptor, AACS_UNIT_LEN};
use crate::error::{BlurayError, Result};
use crate::m2ts::{strip_tp_extra, M2TS_PACKET_LEN, TS_PACKET_LEN};

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
    /// Soft list of language tags found in the playlist's STN_table.
    /// Phase 1: empty (STN_table per-stream entries aren't surfaced
    /// yet); the field stays for forward compatibility so the API
    /// doesn't break later.
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

        // For each title, resolve the PlayList id + duration.
        let mut titles = Vec::with_capacity(index.titles.len());
        for (i, entry) in index.titles.iter().enumerate() {
            let id = (i + 1) as u16;
            let (kind, playlist_id) = resolve_title_playlist(&bdmv, entry)?;
            // Parse the playlist for the duration. If parsing fails
            // (missing / corrupt), surface duration 0 rather than
            // failing the whole mount — some titles are intentionally
            // empty placeholders for chapters / menus.
            let duration_ticks = read_file(&playlist_path(&bdmv, playlist_id))
                .and_then(|b| {
                    let pl = PlayListMpls::parse(&b)?;
                    Ok(pl.duration_90k())
                })
                .unwrap_or(0);
            titles.push(TitleInfo {
                id,
                kind,
                playlist_id,
                duration_ticks,
                languages: Vec::new(),
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
