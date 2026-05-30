//! `bluray://` URI scheme — auto-detect mount points + raw block
//! device paths, and the `oxideav-core` source-registry hook.
//!
//! Supported URI forms:
//!
//! - `bluray://` → auto-detect. Scans OS-specific mount points
//!   (`/Volumes/*` on macOS, `/media/*/*` + `/mnt/*` on Linux) for the
//!   first directory containing a `BDMV/` subdir.
//! - `bluray:///abs/path/to/disc/root` → mount the explicit
//!   filesystem path.
//! - `bluray:///dev/sr0`, `bluray:///dev/disk1` (Phase 2) → would
//!   route through the raw-UDF mounter once we wire `Disc::mount_image`.
//!   Phase 1 returns `Unsupported` for paths that aren't directories.
//!
//! ### Query-string selectors
//!
//! Any of the above can carry `?key=value&...` modifiers:
//!
//! - `?title=N` — open the title with 1-based id `N` instead of the
//!   longest. Range: `[1, disc.titles().len()]`.
//! - `?chapters=A-B` — restrict playback to chapters A through B
//!   inclusive (1-based, matching `Disc::chapters()` order). `A == B`
//!   is one chapter; `A > B` is an error. `chapters=2-` means "from
//!   chapter 2 to the end".
//! - `?chapters=A,B,C` — emit each named chapter as its own segment.
//!   A single integer (`chapters=3`) is the degenerate case.
//!
//! Forgiving on missing keys, strict on malformed values: bad values
//! surface as parse errors rather than being silently dropped.
//!
//! ## Registry hook
//!
//! When the default-on `registry` cargo feature is enabled, the
//! crate's [`crate::register`] re-export wires `bluray` into the
//! [`oxideav_core::SourceRegistry`]. `SourceRegistry::open` dispatches
//! to [`open_bluray`], which mounts the disc, picks the longest
//! HDMV title (autoplay convention), and returns a `TitleSource`
//! boxed as a `BytesSource`.
//!
//! ## Privacy
//!
//! Auto-detect must not leak the user's actual disc identity. The
//! code below only examines directory *existence* (`BDMV/`) — it
//! never reads volume labels, disc IDs, or any other identifying
//! field into a public-facing struct.

use std::path::{Path, PathBuf};

#[cfg(feature = "registry")]
use crate::disc::{Disc, TitleSource};
use crate::error::{BlurayError, Result};

/// Disc-location target carried inside a parsed [`BlurayUri`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlurayUriTarget {
    /// `bluray://` — auto-detect.
    AutoDetect,
    /// `bluray:///abs/path` (no host).
    Path(PathBuf),
}

/// Which chapters of a title to expose downstream.
///
/// Parsed from the optional `?chapters=...` query parameter of a
/// `bluray://` URI. Indices are 1-based and match the order returned by
/// [`crate::Disc::chapters`] (chapter 1 → index 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChapterSelector {
    /// No `chapters=` query — emit one segment per chapter in the title.
    All,
    /// `chapters=A-B` (inclusive) or `chapters=A-` (A through the last
    /// chapter). `start` must be ≥ 1; `end`, when present, must be ≥
    /// `start`.
    Range { start: u32, end: Option<u32> },
    /// `chapters=A,B,C` — non-contiguous chapter ids. A single integer
    /// (`chapters=3`) reaches here as a one-element list. The list is
    /// stored in URI order and is non-empty.
    List(Vec<u32>),
}

/// Parsed `bluray://` URI plus optional title + chapter selectors.
///
/// `target` carries the disc location (auto-detect or filesystem path);
/// `title_id` and `chapter_selector` carry the optional `?title=` /
/// `?chapters=` query params. Pre-query-string callers only need
/// [`Self::target`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlurayUri {
    pub target: BlurayUriTarget,
    /// 1-based title id from `?title=` if present. `None` means "use
    /// the autoplay heuristic" — `Disc::longest_title()`.
    pub title_id: Option<u16>,
    /// Chapter selector from `?chapters=` (defaults to
    /// [`ChapterSelector::All`]).
    pub chapter_selector: ChapterSelector,
}

impl BlurayUri {
    /// Build an auto-detect URI with no query selectors — handy in
    /// tests that only care about the target.
    #[cfg(test)]
    pub(crate) fn auto() -> Self {
        Self {
            target: BlurayUriTarget::AutoDetect,
            title_id: None,
            chapter_selector: ChapterSelector::All,
        }
    }

    /// Build a path-target URI with no query selectors.
    #[cfg(test)]
    pub(crate) fn path(p: impl Into<PathBuf>) -> Self {
        Self {
            target: BlurayUriTarget::Path(p.into()),
            title_id: None,
            chapter_selector: ChapterSelector::All,
        }
    }
}

/// Parse a `bluray://...` URI string, including optional `?title=` and
/// `?chapters=` query selectors. See the module-level docs for the
/// accepted grammar.
pub fn parse_bluray_uri(uri: &str) -> Result<BlurayUri> {
    let rest = uri
        .strip_prefix("bluray://")
        .or_else(|| uri.strip_prefix("bluray:"))
        .ok_or_else(|| BlurayError::not_bluray(format!("not a bluray:// URI: {uri}")))?;

    // Split off the query string (if any) before we touch the path.
    // We don't honor `#fragment` — there's no spec'd meaning for one
    // on `bluray://` and a path containing `#` would be unusual.
    let (path_part, query_part) = match rest.find('?') {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };

    let target = parse_target(path_part);
    let (title_id, chapter_selector) = match query_part {
        Some(q) => parse_query(q)?,
        None => (None, ChapterSelector::All),
    };

    Ok(BlurayUri {
        target,
        title_id,
        chapter_selector,
    })
}

/// Resolve the path / auto-detect portion of a `bluray://` URI (the
/// part before any `?query`). Mirrors the pre-query-string parser.
fn parse_target(path_part: &str) -> BlurayUriTarget {
    if path_part.is_empty() || path_part == "/" {
        return BlurayUriTarget::AutoDetect;
    }
    // We accept either `bluray:///abs/path` (the empty-host form, post
    // `bluray://` strip the leading `/` belongs to the path) or
    // `bluray://host/path` (treated as a path with the host as the
    // first component — useful for `bluray://localhost/...` style usage
    // even though we don't interpret the host).
    let path = if let Some(p) = path_part.strip_prefix('/') {
        // strip the leading slash separator after the (empty) host.
        if p.starts_with('/') {
            // bluray:////double — preserve absolute root
            PathBuf::from(p)
        } else {
            PathBuf::from(format!("/{p}"))
        }
    } else {
        PathBuf::from(path_part)
    };
    BlurayUriTarget::Path(path)
}

/// Parse the query string (everything after `?`). Forgiving on missing
/// keys; strict on malformed values.
fn parse_query(query: &str) -> Result<(Option<u16>, ChapterSelector)> {
    let mut title_id: Option<u16> = None;
    let mut chapter_selector = ChapterSelector::All;

    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => {
                return Err(BlurayError::malformed(format!(
                    "bluray:// query expects key=value, got `{pair}`"
                )))
            }
        };
        match key {
            "title" => {
                let n: u16 = value.parse().map_err(|_| {
                    BlurayError::malformed(format!(
                        "bluray:// `title=` expects a positive integer, got `{value}`"
                    ))
                })?;
                if n == 0 {
                    return Err(BlurayError::malformed(
                        "bluray:// `title=0` is invalid (titles are 1-based)",
                    ));
                }
                title_id = Some(n);
            }
            "chapters" => {
                chapter_selector = parse_chapter_selector(value)?;
            }
            other => {
                return Err(BlurayError::malformed(format!(
                    "bluray:// unknown query key `{other}`"
                )));
            }
        }
    }
    Ok((title_id, chapter_selector))
}

/// Parse the value of a `chapters=` query: a range (`A-B` / `A-`), a
/// comma list (`A,B,C`), or a single id.
fn parse_chapter_selector(value: &str) -> Result<ChapterSelector> {
    if value.is_empty() {
        return Err(BlurayError::malformed(
            "bluray:// `chapters=` is empty (expected N, A-B, or A,B,C)",
        ));
    }
    // List form has precedence: `1,2,3` is unambiguous; a `-` inside a
    // comma-separated entry is rejected as malformed.
    if value.contains(',') {
        let mut ids = Vec::new();
        for tok in value.split(',') {
            if tok.is_empty() {
                return Err(BlurayError::malformed(format!(
                    "bluray:// `chapters=` has an empty list entry in `{value}`"
                )));
            }
            if tok.contains('-') {
                return Err(BlurayError::malformed(format!(
                    "bluray:// `chapters=` mixes range and list syntax in `{value}`"
                )));
            }
            let n: u32 = tok.parse().map_err(|_| {
                BlurayError::malformed(format!(
                    "bluray:// `chapters=` list entry `{tok}` is not a positive integer"
                ))
            })?;
            if n == 0 {
                return Err(BlurayError::malformed(format!(
                    "bluray:// `chapters=` ids are 1-based; got `0` in `{value}`"
                )));
            }
            ids.push(n);
        }
        return Ok(ChapterSelector::List(ids));
    }
    if let Some((a, b)) = value.split_once('-') {
        if a.is_empty() {
            return Err(BlurayError::malformed(format!(
                "bluray:// `chapters=` range missing start: `{value}`"
            )));
        }
        let start: u32 = a.parse().map_err(|_| {
            BlurayError::malformed(format!(
                "bluray:// `chapters=` range start `{a}` is not a positive integer"
            ))
        })?;
        if start == 0 {
            return Err(BlurayError::malformed(format!(
                "bluray:// `chapters=` ids are 1-based; got start `0` in `{value}`"
            )));
        }
        let end = if b.is_empty() {
            None
        } else {
            let end: u32 = b.parse().map_err(|_| {
                BlurayError::malformed(format!(
                    "bluray:// `chapters=` range end `{b}` is not a positive integer"
                ))
            })?;
            if end == 0 {
                return Err(BlurayError::malformed(format!(
                    "bluray:// `chapters=` ids are 1-based; got end `0` in `{value}`"
                )));
            }
            if end < start {
                return Err(BlurayError::malformed(format!(
                    "bluray:// `chapters=` range end `{end}` is before start `{start}`"
                )));
            }
            Some(end)
        };
        return Ok(ChapterSelector::Range { start, end });
    }
    // Bare integer: `chapters=3` → single-element list.
    let n: u32 = value.parse().map_err(|_| {
        BlurayError::malformed(format!(
            "bluray:// `chapters=` value `{value}` is not a positive integer, range, or list"
        ))
    })?;
    if n == 0 {
        return Err(BlurayError::malformed(format!(
            "bluray:// `chapters=` ids are 1-based; got `0` in `{value}`"
        )));
    }
    Ok(ChapterSelector::List(vec![n]))
}

/// Probe the OS for a mounted BD-ROM. Returns the first directory
/// containing `BDMV/` found under one of:
///
/// - `/Volumes/*` (macOS)
/// - `/media/*/*` (Linux user-mount convention)
/// - `/mnt/*`     (Linux historical mount point)
///
/// Errors only on unrecoverable I/O — a missing scan directory is
/// silently skipped.
pub fn detect_disc_root() -> Result<Option<PathBuf>> {
    let mut candidates = Vec::new();
    push_children_with_bdmv(Path::new("/Volumes"), &mut candidates);
    if let Ok(media) = std::fs::read_dir("/media") {
        for entry in media.flatten() {
            push_children_with_bdmv(&entry.path(), &mut candidates);
        }
    }
    push_children_with_bdmv(Path::new("/mnt"), &mut candidates);
    Ok(candidates.into_iter().next())
}

fn push_children_with_bdmv(parent: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.join("BDMV").is_dir() {
            out.push(p);
        }
    }
}

/// `bluray://` source-registry entry point. Mounts the disc,
/// auto-picks the longest HDMV title (or honors `?title=N`), and
/// returns a streaming `BytesSource`.
///
/// `?chapters=...` is parsed and validated, but the BytesSource path
/// currently surfaces the whole title — chapter slicing happens
/// downstream via [`crate::Disc::open_title_chapters`] when callers
/// want one segment per chapter rather than one stream.
#[cfg(feature = "registry")]
pub fn open_bluray(uri: &str) -> oxideav_core::Result<Box<dyn oxideav_core::BytesSource>> {
    use oxideav_core::Error as CoreError;
    let parsed = parse_bluray_uri(uri).map_err(|e| CoreError::invalid(e.to_string()))?;
    let root = match parsed.target {
        BlurayUriTarget::AutoDetect => detect_disc_root()
            .map_err(|e| CoreError::invalid(e.to_string()))?
            .ok_or_else(|| CoreError::invalid("no BD-ROM mount found"))?,
        BlurayUriTarget::Path(p) => {
            if !p.is_dir() {
                return Err(CoreError::invalid(format!(
                    "bluray:// path {} is not a directory (raw-device support is Phase 2)",
                    p.display()
                )));
            }
            p
        }
    };
    let disc = Disc::mount(&root).map_err(|e| CoreError::invalid(e.to_string()))?;
    let title = match parsed.title_id {
        Some(id) => {
            let idx = (id as usize).checked_sub(1).ok_or_else(|| {
                CoreError::invalid(format!("bluray://?title={id} — titles are 1-based"))
            })?;
            disc.titles()
                .get(idx)
                .ok_or_else(|| {
                    CoreError::invalid(format!(
                        "bluray://?title={id} — disc only has {} title(s)",
                        disc.titles().len()
                    ))
                })?
                .clone()
        }
        None => disc
            .longest_title()
            .ok_or_else(|| CoreError::invalid("disc has no playable HDMV titles"))?
            .clone(),
    };
    // Auto-resolve AACS decryption when the `aacs` feature is on
    // (default-on). Fail LOUDLY for AACS-protected discs whose VUK
    // isn't in KEYDB.cfg — silently falling back to Identity would
    // hand encrypted bytes to the demuxer, which then loops forever
    // hunting for an MPEG-TS sync byte that never appears.
    let decryptor: Option<Box<dyn crate::StreamDecryptor>> = {
        #[cfg(feature = "aacs")]
        {
            let has_aacs_dir = root.join("AACS").is_dir();
            let resolved = crate::aacs_adapter::try_resolve_aacs(&root)
                .map_err(|e| CoreError::invalid(format!("AACS resolve: {e}")))?;
            if has_aacs_dir && resolved.is_none() {
                return Err(CoreError::invalid(format!(
                    "disc at {} is AACS-protected but no matching VUK \
                     was found in KEYDB.cfg — add a line of the form \
                     `<40-hex disc id> = V <32-hex VUK> | <label>` \
                     under $OXIDEAV_AACS_KEYDB, ~/Library/Preferences/aacs/ \
                     (macOS), $XDG_CONFIG_HOME/aacs/, or ~/.config/aacs/",
                    root.display()
                )));
            }
            resolved
        }
        #[cfg(not(feature = "aacs"))]
        {
            None
        }
    };
    let src: TitleSource = disc
        .open_title(&title, decryptor)
        .map_err(|e| CoreError::invalid(e.to_string()))?;
    Ok(Box::new(src))
}

/// `bluray://` opener that returns a [`oxideav_core::MultiTitleSource`]
/// — one "title" per emitted chapter (when `?chapters=...` is set)
/// or a single title carrying the whole BD title bytes (when only
/// `?title=N` or nothing is set).
///
/// This is the registry opener `bluray://` is registered under. The
/// older byte-shape [`open_bluray`] stays available for callers that
/// just want one byte stream and don't go through the registry.
#[cfg(feature = "registry")]
pub fn open_bluray_multi_title(
    uri: &str,
) -> oxideav_core::Result<Box<dyn oxideav_core::MultiTitleSource>> {
    use oxideav_core::Error as CoreError;
    let parsed = parse_bluray_uri(uri).map_err(|e| CoreError::invalid(e.to_string()))?;
    let root = match parsed.target {
        BlurayUriTarget::AutoDetect => detect_disc_root()
            .map_err(|e| CoreError::invalid(e.to_string()))?
            .ok_or_else(|| CoreError::invalid("no BD-ROM mount found"))?,
        BlurayUriTarget::Path(p) => {
            if !p.is_dir() {
                return Err(CoreError::invalid(format!(
                    "bluray:// path {} is not a directory (raw-device support is Phase 2)",
                    p.display()
                )));
            }
            p
        }
    };
    let disc = Disc::mount(&root).map_err(|e| CoreError::invalid(e.to_string()))?;
    let title = match parsed.title_id {
        Some(id) => {
            let idx = (id as usize).checked_sub(1).ok_or_else(|| {
                CoreError::invalid(format!("bluray://?title={id} — titles are 1-based"))
            })?;
            disc.titles()
                .get(idx)
                .ok_or_else(|| {
                    CoreError::invalid(format!(
                        "bluray://?title={id} — disc only has {} title(s)",
                        disc.titles().len()
                    ))
                })?
                .clone()
        }
        None => disc
            .longest_title()
            .ok_or_else(|| CoreError::invalid("disc has no playable HDMV titles"))?
            .clone(),
    };

    // Resolve which chapter indices we emit. `ChapterSelector::All`
    // collapses to "one segment = whole title"; the other variants
    // produce one segment per chapter id.
    let chapter_list = match &parsed.chapter_selector {
        ChapterSelector::All => None,
        ChapterSelector::Range { start, end } => {
            let total = disc.chapters(&title).len() as u32;
            let end = end.unwrap_or(total);
            if *start == 0 || end == 0 {
                return Err(CoreError::invalid(
                    "bluray://?chapters= — chapter ids are 1-based",
                ));
            }
            if *start > end {
                return Err(CoreError::invalid(format!(
                    "bluray://?chapters={start}-{end} — start > end"
                )));
            }
            Some((*start..=end).collect::<Vec<u32>>())
        }
        ChapterSelector::List(ids) => Some(ids.clone()),
    };

    // Disc-level metadata — surface the volume label and the BDMT
    // `<di:name>` when present so a downstream menu can use them.
    let mut metadata: Vec<(String, String)> = Vec::new();
    if let Some(label) = disc.volume_label() {
        metadata.push(("volume_label".to_string(), label));
    }
    if let Some(m) = disc.title_meta() {
        metadata.push(("disc_title".to_string(), m.title));
        if let Some(lang) = m.language {
            metadata.push(("disc_title_language".to_string(), lang));
        }
    }

    Ok(Box::new(BluRayMultiTitleSource {
        disc_root: root,
        title,
        chapter_list,
        metadata,
    }))
}

/// Concrete implementor of [`oxideav_core::MultiTitleSource`] for the
/// `bluray://` scheme. Holds the parsed disc root + BD title +
/// resolved chapter-emit plan. AACS decryption is re-resolved on
/// each `open_title` call (the trait method is `&mut self` but the
/// decryptor isn't cloneable, so we can't cache one).
#[cfg(feature = "registry")]
struct BluRayMultiTitleSource {
    disc_root: std::path::PathBuf,
    title: crate::TitleInfo,
    /// `None` → emit one segment = whole title.
    /// `Some(ids)` → emit one segment per chapter id, in the given order.
    chapter_list: Option<Vec<u32>>,
    metadata: Vec<(String, String)>,
}

#[cfg(feature = "registry")]
impl BluRayMultiTitleSource {
    /// Mount the disc fresh + resolve a decryptor — done once per
    /// `open_title` call. Keeps the trait method's lifetime simple
    /// (no shared mutable Disc state).
    fn fresh_disc_and_decryptor(
        &self,
    ) -> oxideav_core::Result<(crate::Disc, Option<Box<dyn crate::StreamDecryptor>>)> {
        use oxideav_core::Error as CoreError;
        let disc =
            crate::Disc::mount(&self.disc_root).map_err(|e| CoreError::invalid(e.to_string()))?;
        let decryptor: Option<Box<dyn crate::StreamDecryptor>> = {
            #[cfg(feature = "aacs")]
            {
                crate::aacs_adapter::try_resolve_aacs(&self.disc_root)
                    .map_err(|e| CoreError::invalid(format!("AACS resolve: {e}")))?
            }
            #[cfg(not(feature = "aacs"))]
            {
                None
            }
        };
        Ok((disc, decryptor))
    }
}

#[cfg(feature = "registry")]
impl oxideav_core::MultiTitleSource for BluRayMultiTitleSource {
    fn title_count(&self) -> usize {
        match &self.chapter_list {
            None => 1,
            Some(ids) => ids.len(),
        }
    }

    fn open_title(
        &mut self,
        index: usize,
    ) -> oxideav_core::Result<Box<dyn oxideav_core::BytesSource>> {
        use oxideav_core::Error as CoreError;
        let (disc, decryptor) = self.fresh_disc_and_decryptor()?;
        match &self.chapter_list {
            None => {
                if index != 0 {
                    return Err(CoreError::invalid(format!(
                        "bluray:// MultiTitleSource: index {index} out of range (have 1 title)"
                    )));
                }
                let src = disc
                    .open_title(&self.title, decryptor)
                    .map_err(|e| CoreError::invalid(e.to_string()))?;
                Ok(Box::new(src))
            }
            Some(ids) => {
                let id = *ids.get(index).ok_or_else(|| {
                    CoreError::invalid(format!(
                        "bluray:// MultiTitleSource: index {index} out of range (have {} titles)",
                        ids.len()
                    ))
                })?;
                let chapters = disc.chapters(&self.title);
                if chapters.is_empty() {
                    return Err(CoreError::invalid(format!(
                        "title {} has no chapters",
                        self.title.id
                    )));
                }
                let chapter_count = chapters.len() as u32;
                if id < 1 || id > chapter_count {
                    return Err(CoreError::invalid(format!(
                        "chapter id {id} outside [1, {chapter_count}]"
                    )));
                }
                let idx = id as usize - 1;
                let start_pts_90k = chapters[idx].start_pts_90k;
                let last_chapter = idx == chapters.len() - 1;
                let next_pts_90k = if last_chapter {
                    self.title.duration_ticks
                } else {
                    chapters[idx + 1].start_pts_90k
                };
                let mut title_source = disc
                    .open_title(&self.title, decryptor)
                    .map_err(|e| CoreError::invalid(e.to_string()))?;
                // Compute end byte first (last chapter → EOF; else
                // keyframe-aligned next-chapter byte) THEN seek back to
                // the start.  Mirrors ChapterSegments::read_one — the
                // seek is keyframe-rounded so two seeks at the seam
                // line up on the same packet boundary.
                let end_byte = if last_chapter {
                    title_source.output_total()
                } else {
                    title_source.seek_to(next_pts_90k).map_err(CoreError::Io)?
                };
                let start_byte = title_source.seek_to(start_pts_90k).map_err(CoreError::Io)?;
                let remaining = end_byte.saturating_sub(start_byte);
                Ok(Box::new(BoundedTitleSource {
                    inner: title_source,
                    remaining,
                }))
            }
        }
    }

    fn title_label(&self, index: usize) -> String {
        match &self.chapter_list {
            None => format!("t{:02}", self.title.id),
            Some(ids) => format!("c{:02}", ids[index]),
        }
    }

    fn title_display_name(&self, index: usize) -> Option<String> {
        match &self.chapter_list {
            None => Some(format!("Title {}", self.title.id)),
            Some(ids) => Some(format!("Chapter {}", ids[index])),
        }
    }

    fn title_container_hint(&self, _index: usize) -> Option<&'static str> {
        // Every BD title is BDAV-wrapped MPEG-TS. The chapter-segment
        // bytes are already TP_extra-stripped clean MPEG-TS; the
        // whole-title byte stream is a sequence of 192-byte source
        // packets that downstream callers strip themselves.
        Some("mpegts")
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }
}

/// Streaming wrapper around [`TitleSource`] that bounds the number of
/// bytes the demuxer can read from the underlying title.  Used by the
/// chapter branch of [`BluRayMultiTitleSource::open_title`] so a CLI
/// pipeline sees `.ts` output growing on disk as the bluray is read,
/// instead of waiting for a full chapter to land in RAM first.
#[cfg(feature = "registry")]
struct BoundedTitleSource {
    inner: TitleSource,
    remaining: u64,
}

#[cfg(feature = "registry")]
impl std::fmt::Debug for BoundedTitleSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundedTitleSource")
            .field("remaining", &self.remaining)
            .finish()
    }
}

#[cfg(feature = "registry")]
impl std::io::Read for BoundedTitleSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let cap = (self.remaining as usize).min(buf.len());
        let n = self.inner.read(&mut buf[..cap])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

#[cfg(feature = "registry")]
impl std::io::Seek for BoundedTitleSource {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        // The mpegts demuxer never seeks on the input.  Forward to
        // preserve trait shape; do not adjust `remaining` since byte
        // accounting after an arbitrary seek is undefined.
        self.inner.seek(pos)
    }
}

/// Register the `bluray` scheme with a [`oxideav_core::RuntimeContext`].
///
/// Registers as a [`oxideav_core::MultiTitleSource`] opener — the
/// scheme can emit one byte stream per chapter (or one for the whole
/// title when `?chapters=` is absent). Single-byte-stream callers
/// can still use [`open_bluray`] directly to get a [`TitleSource`]
/// without going through the registry.
#[cfg(feature = "registry")]
pub fn register(ctx: &mut oxideav_core::RuntimeContext) {
    ctx.sources
        .register_multi_title("bluray", open_bluray_multi_title);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_auto_detect() {
        assert_eq!(parse_bluray_uri("bluray://").unwrap(), BlurayUri::auto());
        assert_eq!(parse_bluray_uri("bluray:").unwrap(), BlurayUri::auto());
        assert_eq!(parse_bluray_uri("bluray:///").unwrap(), BlurayUri::auto());
    }

    #[test]
    fn parse_absolute_path() {
        assert_eq!(
            parse_bluray_uri("bluray:///tmp/disc-root").unwrap(),
            BlurayUri::path("/tmp/disc-root")
        );
        assert_eq!(
            parse_bluray_uri("bluray:///dev/sr0").unwrap(),
            BlurayUri::path("/dev/sr0")
        );
    }

    #[test]
    fn rejects_wrong_scheme() {
        assert!(parse_bluray_uri("file:///x").is_err());
        assert!(parse_bluray_uri("http://example/").is_err());
    }

    // ─── ?title= ───────────────────────────────────────────────

    #[test]
    fn parse_title_query_on_path() {
        let u = parse_bluray_uri("bluray:///tmp/disc?title=2").unwrap();
        assert_eq!(u.target, BlurayUriTarget::Path(PathBuf::from("/tmp/disc")));
        assert_eq!(u.title_id, Some(2));
        assert_eq!(u.chapter_selector, ChapterSelector::All);
    }

    #[test]
    fn parse_title_query_on_autodetect() {
        let u = parse_bluray_uri("bluray://?title=1").unwrap();
        assert_eq!(u.target, BlurayUriTarget::AutoDetect);
        assert_eq!(u.title_id, Some(1));
    }

    #[test]
    fn rejects_title_zero() {
        // titles are 1-based; `?title=0` is a definite user error rather
        // than a silently-recovered "use the longest" case.
        assert!(parse_bluray_uri("bluray:///d?title=0").is_err());
    }

    #[test]
    fn rejects_title_non_integer() {
        assert!(parse_bluray_uri("bluray:///d?title=abc").is_err());
        assert!(parse_bluray_uri("bluray:///d?title=-1").is_err());
    }

    #[test]
    fn rejects_title_out_of_u16_range() {
        // u16 caps at 65535; anything past that overflows the parser
        // and surfaces a Malformed error rather than silently wrapping.
        assert!(parse_bluray_uri("bluray:///d?title=70000").is_err());
    }

    // ─── ?chapters= range ──────────────────────────────────────

    #[test]
    fn parse_chapter_range_inclusive() {
        let u = parse_bluray_uri("bluray:///d?chapters=2-5").unwrap();
        assert_eq!(
            u.chapter_selector,
            ChapterSelector::Range {
                start: 2,
                end: Some(5)
            }
        );
    }

    #[test]
    fn parse_chapter_range_single_chapter() {
        let u = parse_bluray_uri("bluray:///d?chapters=3-3").unwrap();
        assert_eq!(
            u.chapter_selector,
            ChapterSelector::Range {
                start: 3,
                end: Some(3)
            }
        );
    }

    #[test]
    fn parse_chapter_range_open_ended() {
        let u = parse_bluray_uri("bluray:///d?chapters=2-").unwrap();
        assert_eq!(
            u.chapter_selector,
            ChapterSelector::Range {
                start: 2,
                end: None
            }
        );
    }

    #[test]
    fn rejects_chapter_range_inverted() {
        assert!(parse_bluray_uri("bluray:///d?chapters=5-2").is_err());
    }

    #[test]
    fn rejects_chapter_range_missing_start() {
        // `-3` would mean "start from somewhere" — ambiguous; surface
        // it as a parse error rather than silently treating as `1-3`.
        assert!(parse_bluray_uri("bluray:///d?chapters=-3").is_err());
    }

    #[test]
    fn rejects_chapter_range_zero() {
        assert!(parse_bluray_uri("bluray:///d?chapters=0-3").is_err());
        assert!(parse_bluray_uri("bluray:///d?chapters=2-0").is_err());
    }

    // ─── ?chapters= list ───────────────────────────────────────

    #[test]
    fn parse_chapter_list_multi() {
        let u = parse_bluray_uri("bluray:///d?chapters=2,4,7").unwrap();
        assert_eq!(u.chapter_selector, ChapterSelector::List(vec![2, 4, 7]));
    }

    #[test]
    fn parse_chapter_single_int_lists_one() {
        // `chapters=3` is the degenerate single-element list; matches
        // the user's mental model of "give me chapter 3 only".
        let u = parse_bluray_uri("bluray:///d?chapters=3").unwrap();
        assert_eq!(u.chapter_selector, ChapterSelector::List(vec![3]));
    }

    #[test]
    fn rejects_chapter_list_with_range_token() {
        // `1,2-3,4` mixes syntax — refuse rather than guess which
        // semantics the user meant.
        assert!(parse_bluray_uri("bluray:///d?chapters=1,2-3,4").is_err());
    }

    #[test]
    fn rejects_chapter_list_empty_token() {
        assert!(parse_bluray_uri("bluray:///d?chapters=1,,3").is_err());
        assert!(parse_bluray_uri("bluray:///d?chapters=,1").is_err());
    }

    #[test]
    fn rejects_chapter_list_non_integer() {
        assert!(parse_bluray_uri("bluray:///d?chapters=1,x,3").is_err());
    }

    // ─── Composition ───────────────────────────────────────────

    #[test]
    fn parse_title_and_chapters_compose() {
        let u = parse_bluray_uri("bluray:///vol/movie?title=1&chapters=2-5").unwrap();
        assert_eq!(u.target, BlurayUriTarget::Path(PathBuf::from("/vol/movie")));
        assert_eq!(u.title_id, Some(1));
        assert_eq!(
            u.chapter_selector,
            ChapterSelector::Range {
                start: 2,
                end: Some(5)
            }
        );
    }

    #[test]
    fn parse_order_of_query_params_does_not_matter() {
        let a = parse_bluray_uri("bluray://?title=2&chapters=1,3").unwrap();
        let b = parse_bluray_uri("bluray://?chapters=1,3&title=2").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn rejects_unknown_query_key() {
        // Strict on malformed: silently ignoring would let typos
        // (`?titl=1`) silently fall back to "longest title".
        assert!(parse_bluray_uri("bluray://?titl=1").is_err());
        assert!(parse_bluray_uri("bluray://?chapter=2-5").is_err());
    }

    #[test]
    fn rejects_query_without_equals() {
        assert!(parse_bluray_uri("bluray://?title").is_err());
    }

    #[test]
    fn rejects_empty_chapters_value() {
        assert!(parse_bluray_uri("bluray://?chapters=").is_err());
    }

    #[test]
    fn parse_empty_query_string_is_default() {
        // `?` with nothing after it is equivalent to no query at all.
        let u = parse_bluray_uri("bluray:///d?").unwrap();
        assert_eq!(u, BlurayUri::path("/d"));
    }

    #[test]
    fn parse_skips_consecutive_ampersands() {
        // `a=1&&b=2` shouldn't fail — leading/trailing `&` separators
        // are common and harmless.
        let u = parse_bluray_uri("bluray:///d?title=1&&chapters=3").unwrap();
        assert_eq!(u.title_id, Some(1));
        assert_eq!(u.chapter_selector, ChapterSelector::List(vec![3]));
    }

    #[test]
    fn detect_with_synthetic_root_under_tmp() {
        // Build a fake mount point under /tmp/oxideav-bluray-detect-<pid>/<vol>/BDMV.
        // We can't write into /Volumes or /media from a test, so we
        // exercise the inner push helper directly with our synthetic
        // parent.
        let pid = std::process::id();
        let parent = std::env::temp_dir().join(format!("oxideav-bluray-detect-{pid}"));
        let vol = parent.join("FAKEVOL");
        let bdmv = vol.join("BDMV");
        std::fs::create_dir_all(&bdmv).unwrap();

        let mut out = Vec::new();
        push_children_with_bdmv(&parent, &mut out);
        assert_eq!(out, vec![vol.clone()]);

        std::fs::remove_dir_all(&parent).ok();
    }
}
