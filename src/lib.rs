//! Read-only Blu-ray Disc (BD-ROM) UDF mount + BDMV walk + playlist
//! (`.mpls`) parsing + `.m2ts` streaming.
//!
//! Phase 1: source plug-in. The crate mounts a BD-ROM disc image (or
//! a directory tree, or a block device), walks the BDMV directory,
//! enumerates titles, and presents each title as a clean 188-byte
//! MPEG-TS byte stream consumable by the existing
//! [`oxideav-pipeline`] + MPEG-TS demuxer.
//!
//! ## Scope
//!
//! - UDF 2.50 read-only mount (ECMA-167 §3–§7).
//! - `index.bdmv` / `MovieObject.bdmv` / `PLAYLIST/*.mpls` /
//!   `CLIPINF/*.clpi` / `STREAM/*.m2ts` parsers (BD-ROM Part 3 §5).
//! - Title listing + longest-title autoplay.
//! - `bluray://` URI handler with auto-detect.
//! - Pluggable `StreamDecryptor` trait (AACS adapter lives elsewhere).
//!
//! Out of scope (deferred or indefinite):
//! - HDMV interactive layer (`MovieObject.bdmv` opcode execution).
//! - BD-J (Java VM).
//! - SubPath PiP / secondary-video streams.
//!
//! ## Clean-room references
//!
//! - `docs/container/bluray/BD-ROM_Part3_V3.2_WhitePaper_180122.pdf`
//! - `docs/container/bluray/BD-ROM_Audio_Visual_Application_Format_Specifications.pdf`
//! - `docs/container/bluray/ECMA-167_3rd_edition_june_1997.pdf`
//!
//! ## Quick start
//!
//! ```no_run
//! use oxideav_bluray::Disc;
//!
//! // Mount a BD-ROM disc root (a directory containing `BDMV/`).
//! let disc = Disc::mount("/Volumes/EXAMPLE_DISC").unwrap();
//!
//! // Pick the longest HDMV title (heuristic for "the main feature").
//! let title = disc.longest_title().expect("disc has at least one title");
//! println!("title {:03} — {} ticks", title.id, title.duration_ticks);
//!
//! // Open the title as a `Read`-able MPEG-TS byte stream.
//! let mut source = disc.open_title(title, None).unwrap();
//! let mut buf = vec![0u8; 188 * 1024];
//! use std::io::Read;
//! let n = source.read(&mut buf).unwrap();
//! eprintln!("read {n} bytes of clean MPEG-TS");
//! ```
//!
//! ## Standalone build
//!
//! `oxideav-core` is gated behind the default-on `registry` feature.
//! Drop the framework dependency entirely with:
//!
//! ```toml
//! oxideav-bluray = { version = "0.0", default-features = false }
//! ```
//!
//! The `Disc` / `TitleSource` / parser surface stays available; only
//! the `bluray://` source-registry plumbing disappears.

// `deny(unsafe_code)` rather than `forbid` so the macOS drive backend
// (`drive::macos`) can opt into raw IOKit / CoreFoundation FFI via a
// module-scoped `#![allow(unsafe_code)]`. Every other module in the
// crate stays unsafe-free.
#![deny(unsafe_code)]
#![warn(missing_debug_implementations)]

#[cfg(feature = "aacs")]
pub mod aacs_adapter;
pub mod bdmv;
pub mod decrypt;
pub mod disc;
#[cfg(feature = "aacs")]
pub mod drive;
pub mod error;
pub mod m2ts;
pub mod source;
pub mod udf;

pub use bdmv::{
    clpi::{ClipInfo, ClipMark, Cpi, EpEntry, EpMap, ProgramInfo, SequenceInfo, StreamCodingInfo},
    index_bdmv::{AppInfoBdmv, IndexBdmv, IndexEntry, IndexObjectType},
    movie_object::{MovieObject, NavCommand},
    mpls::{
        AngleClip, AngleClipRef, AppInfoPlayList, AspectRatio, AudioFormat, Chapter,
        ConnectionCondition, FrameRate, IgsInteractiveStream, MarkType, PgsSubtitleStream,
        PipPgStream, PlayItem, PlayItemFlags, PlayList, PlayListMark, PlayListMpls,
        PlayListPlaybackType, PrimaryAudioStream, PrimaryVideoStream, SampleRate,
        SecondaryAudioStream, SecondaryVideoStream, StnTable, StreamCodingType, SubPath,
        TextSubtitleStream, VideoFormat,
    },
    nav_command::{CommandGroup, DecodedCommand, Operand, Operation, RegisterBank},
    pgs::{
        decode_rle, parse_segments, CompositionObject, CompositionObjectCrop, CompositionState,
        DecodedObject, FragmentFlag, Ods, PaletteEntry, Pcs, Pds, Segment, SegmentBody,
        SegmentHeader, SegmentType, Wds, Window,
    },
};
pub use decrypt::{DecryptError, StreamDecryptor};
pub use disc::{
    AngleChangePoint, ChapterSegment, ChapterSegments, Disc, DiscTitleMeta, PtsContinuitySegment,
    TitleInfo, TitleKind, TitleSource, Track, TrackCatalogue, TrackKind,
};
pub use error::{BlurayError, Result};
pub use m2ts::{
    iter_source_packets, strip_tp_extra, strip_tp_extra_to_vec, M2tsIter, M2tsSourcePacket,
    TpExtraHeader, M2TS_PACKET_LEN, TP_EXTRA_HEADER_LEN, TS_PACKET_LEN,
};
pub use source::{detect_disc_root, parse_bluray_uri, BlurayUri, BlurayUriTarget, ChapterSelector};

#[cfg(feature = "registry")]
pub use source::register;

// Canonical sibling entry point. Registers the `bluray://` source
// driver under `oxideav_core::RuntimeContext::sources`.
#[cfg(feature = "registry")]
oxideav_core::register!("bluray", source::register);
