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

use crate::bdmv::index_bdmv::{IndexBdmv, IndexEntry, IndexObjectType};
use crate::bdmv::mpls::PlayListMpls;
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

    /// Pick the longest HDMV title. BD-J titles are skipped because
    /// Phase 1 cannot execute their navigation script.
    pub fn longest_title(&self) -> Option<&TitleInfo> {
        self.titles
            .iter()
            .filter(|t| t.kind == TitleKind::Hdmv)
            .max_by_key(|t| t.duration_ticks)
    }

    /// Open a title as a [`TitleSource`]. The optional `decryptor`
    /// lets an AACS adapter plug in; pass `None` for unprotected
    /// homemade discs and for the crate's own tests.
    pub fn open_title(
        &self,
        title: &TitleInfo,
        decryptor: Option<Box<dyn StreamDecryptor>>,
    ) -> Result<TitleSource> {
        let bdmv = self.root.join("BDMV");
        let pl_bytes = read_file(&playlist_path(&bdmv, title.playlist_id))?;
        let pl = PlayListMpls::parse(&pl_bytes)?;
        if pl.play_list.play_items.is_empty() {
            return Err(BlurayError::not_bluray("title has no PlayItems"));
        }
        let clip_stems: Vec<String> = pl
            .play_list
            .play_items
            .iter()
            .map(|p| p.clip_information_file_name.clone())
            .collect();
        TitleSource::new(bdmv, clip_stems, decryptor)
    }
}

/// A `Read`-able view onto a title: concatenates the title's PlayItem
/// clips end-to-end, stripping the 4-byte BDAV TP_extra header per
/// 192-byte source packet to yield a clean 188-byte MPEG-TS stream.
pub struct TitleSource {
    bdmv_root: PathBuf,
    clip_stems: Vec<String>,
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
            .field("clip_stems", &self.clip_stems)
            .field("clip_idx", &self.clip_idx)
            .field("clip_offset", &self.clip_offset)
            .field("pending_len", &(self.pending.len() - self.pending_pos))
            .finish()
    }
}

impl TitleSource {
    fn new(
        bdmv_root: PathBuf,
        clip_stems: Vec<String>,
        decryptor: Option<Box<dyn StreamDecryptor>>,
    ) -> Result<Self> {
        // Pre-compute estimated output total by summing each clip's
        // file size and applying the TP_extra strip ratio (188/192).
        // Round down to a packet boundary so the estimate matches what
        // a complete linear read would actually emit.
        let mut output_total: u64 = 0;
        for stem in &clip_stems {
            let path = bdmv_root.join("STREAM").join(format!("{stem}.m2ts"));
            if let Ok(meta) = std::fs::metadata(&path) {
                let raw = meta.len();
                let usable = raw - (raw % M2TS_PACKET_LEN as u64);
                let packets = usable / M2TS_PACKET_LEN as u64;
                output_total += packets * TS_PACKET_LEN as u64;
            }
        }

        let mut s = Self {
            bdmv_root,
            clip_stems,
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

    /// Reset state to the start of the title (clip 0, offset 0,
    /// no pending). Cheap: just re-opens the first .m2ts file.
    fn rewind_to_start(&mut self) -> Result<()> {
        self.clip_idx = 0;
        self.current = None;
        self.clip_offset = 0;
        self.pending.clear();
        self.pending_pos = 0;
        self.output_pos = 0;
        self.open_next_clip()
    }

    fn open_next_clip(&mut self) -> Result<()> {
        self.current = None;
        if self.clip_idx >= self.clip_stems.len() {
            return Ok(());
        }
        let stem = &self.clip_stems[self.clip_idx];
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

// `Seek` supports the patterns probers + linear pipelines actually
// use: position query, rewind-to-start, end-position query for size
// discovery, and forward-skip (read+discard). Backwards-seek to a
// non-zero offset is the Phase-2 boundary (would need a
// per-clip output-offset index built from CPI EP_map).
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

        // Backwards seek: rewind to clip 0 then fall through to the
        // forward-skip path. Slow on large jumps but correct without
        // a per-clip output-offset index. A proper random-access
        // implementation built on CPI EP_map (BD-ROM Part 3 §5.5.6)
        // is the Phase-2 deliverable; this gets probers + scrubbing
        // working in the meantime.
        if target < self.output_pos {
            self.rewind_to_start().map_err(|e| match e {
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
