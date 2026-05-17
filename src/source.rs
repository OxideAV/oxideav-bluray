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

/// Parsed `bluray://` URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlurayUri {
    /// `bluray://` — auto-detect.
    AutoDetect,
    /// `bluray:///abs/path` (no host).
    Path(PathBuf),
}

/// Parse a `bluray://...` URI string.
pub fn parse_bluray_uri(uri: &str) -> Result<BlurayUri> {
    let rest = uri
        .strip_prefix("bluray://")
        .or_else(|| uri.strip_prefix("bluray:"))
        .ok_or_else(|| BlurayError::not_bluray(format!("not a bluray:// URI: {uri}")))?;
    if rest.is_empty() || rest == "/" {
        return Ok(BlurayUri::AutoDetect);
    }
    // We accept either `bluray:///abs/path` (the empty-host form, post
    // `bluray://` strip the leading `/` belongs to the path) or
    // `bluray://host/path` (treated as a path with the host as the
    // first component — useful for `bluray://localhost/...` style usage
    // even though we don't interpret the host).
    let path = if let Some(p) = rest.strip_prefix('/') {
        // strip the leading slash separator after the (empty) host.
        if p.starts_with('/') {
            // bluray:////double — preserve absolute root
            PathBuf::from(p)
        } else {
            PathBuf::from(format!("/{p}"))
        }
    } else {
        PathBuf::from(rest)
    };
    Ok(BlurayUri::Path(path))
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
/// auto-picks the longest HDMV title, and returns a streaming
/// `BytesSource`.
#[cfg(feature = "registry")]
pub fn open_bluray(uri: &str) -> oxideav_core::Result<Box<dyn oxideav_core::BytesSource>> {
    use oxideav_core::Error as CoreError;
    let parsed = parse_bluray_uri(uri).map_err(|e| CoreError::invalid(e.to_string()))?;
    let root = match parsed {
        BlurayUri::AutoDetect => detect_disc_root()
            .map_err(|e| CoreError::invalid(e.to_string()))?
            .ok_or_else(|| CoreError::invalid("no BD-ROM mount found"))?,
        BlurayUri::Path(p) => {
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
    let title = disc
        .longest_title()
        .ok_or_else(|| CoreError::invalid("disc has no playable HDMV titles"))?
        .clone();
    let src: TitleSource = disc
        .open_title(&title, None)
        .map_err(|e| CoreError::invalid(e.to_string()))?;
    Ok(Box::new(src))
}

/// Register the `bluray` scheme with a [`oxideav_core::RuntimeContext`].
#[cfg(feature = "registry")]
pub fn register(ctx: &mut oxideav_core::RuntimeContext) {
    ctx.sources.register_bytes("bluray", open_bluray);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_auto_detect() {
        assert_eq!(
            parse_bluray_uri("bluray://").unwrap(),
            BlurayUri::AutoDetect
        );
        assert_eq!(parse_bluray_uri("bluray:").unwrap(), BlurayUri::AutoDetect);
        assert_eq!(
            parse_bluray_uri("bluray:///").unwrap(),
            BlurayUri::AutoDetect
        );
    }

    #[test]
    fn parse_absolute_path() {
        assert_eq!(
            parse_bluray_uri("bluray:///tmp/disc-root").unwrap(),
            BlurayUri::Path(PathBuf::from("/tmp/disc-root"))
        );
        assert_eq!(
            parse_bluray_uri("bluray:///dev/sr0").unwrap(),
            BlurayUri::Path(PathBuf::from("/dev/sr0"))
        );
    }

    #[test]
    fn rejects_wrong_scheme() {
        assert!(parse_bluray_uri("file:///x").is_err());
        assert!(parse_bluray_uri("http://example/").is_err());
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
