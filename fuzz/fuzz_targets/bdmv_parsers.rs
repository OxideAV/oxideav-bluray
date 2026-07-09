#![no_main]

//! Coverage-guided fuzz harness for the BDMV metadata / navigation
//! parsers in `oxideav_bluray::bdmv`.
//!
//! These parsers turn the raw on-disc bytes of `index.bdmv`,
//! `MovieObject.bdmv`, `PLAYLIST/*.mpls`, `CLIPINF/*.clpi` and the
//! Presentation-Graphics elementary stream into typed structures. Every
//! one runs on wholly untrusted input — a size field that lies about the
//! body length, a stream / play-item / entry-point count far larger than
//! the bytes that follow, a section offset that points past the end, a
//! truncated tail — and MUST surface `Err(BlurayError)` rather than
//! panic, overflow in debug, or index out of bounds.
//!
//! The first byte selects which parser the remaining bytes are fed to,
//! so cargo-fuzz minimises a single corpus across the whole BDMV
//! surface. Parsers that return `Ok(_)` are additionally exercised
//! through their derived accessors so a downstream-index panic is caught
//! too.

use libfuzzer_sys::fuzz_target;
use oxideav_bluray::bdmv::{clpi, index_bdmv, movie_object, mpls, pgs};

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let sel = data[0];
    let body = &data[1..];

    match sel % 7 {
        0 => {
            if let Ok(pl) = mpls::PlayListMpls::parse(body) {
                // Derived accessors run over attacker-shaped field values.
                let _ = pl.duration_90k();
                let _ = pl.chapters();
                let _ = pl.chapters_with_duration();
                for pi in &pl.play_list.play_items {
                    let _ = pi.duration_90k();
                }
                for m in &pl.marks {
                    let _ = m.kind();
                }
                let _ = pl.app_info.playback_kind();
            }
        }
        1 => {
            if let Ok(clip) = clpi::ClipInformation::parse(body) {
                let _ = clip.cpi.primary_video_ep_map();
                for ep in &clip.cpi.ep_map {
                    let _ = ep.entry_point_count();
                    let _ = ep.first_pts();
                    let _ = ep.last_pts();
                    let _ = ep.indexed_span_90k();
                    let _ = ep.seek_spn(0);
                    let _ = ep.seek_spn(u32::MAX);
                }
                let _ = clip.sequence_info.presentation_span_90k();
            }
        }
        2 => {
            let _ = index_bdmv::IndexBdmv::parse(body);
        }
        3 => {
            if let Ok(mobj) = movie_object::MovieObjects::parse(body) {
                let _ = mobj.disassemble();
            }
        }
        4 => {
            let _ = pgs::parse_segments(body);
        }
        5 => {
            if let Ok(sets) = pgs::parse_display_sets(body) {
                for ds in &sets {
                    let _ = ds.reassemble_objects();
                }
            }
        }
        _ => {
            let _ = pgs::decode_rle(body, 16, 16);
        }
    }
});
