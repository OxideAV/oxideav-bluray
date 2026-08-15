//! Structured-mutation hardening for the BDMV parsers.
//!
//! `bdmv_hostile_input` throws random / uniform / truncated garbage at
//! the parsers; that mostly exercises the outer envelope checks. This
//! suite instead starts from a *valid* encoded `.mpls` / `index.bdmv` /
//! `MovieObject.bdmv` (built through the crate's own public encoders) and
//! perturbs it — every single-byte flip, a length/offset field rewritten
//! to a lie, and every truncation — so the mutation lands *inside* a real
//! body the parser walks. That reaches the deep count-driven loops
//! (per-PlayItem / per-stream / per-command / per-entry-point) a random
//! buffer almost never gets past the magic check to see.
//!
//! Contract, as everywhere: any `&[u8]` yields `Ok(_)` or
//! `Err(BlurayError)` — never a panic, debug overflow, or out-of-bounds.
//! Every mutated buffer is fed to *all* BDMV parsers, because a mutation
//! of one file may accidentally resemble another and each must stay
//! panic-safe on it.

use oxideav_bluray::bdmv::index_bdmv::{AppInfoBdmv, IndexBdmv, IndexEntry, IndexObjectType};
use oxideav_bluray::bdmv::movie_object::{MovieObject, MovieObjects, NavCommand};
use oxideav_bluray::bdmv::mpls::{
    AppInfoPlayList, ConnectionCondition, PlayItem, PlayItemFlags, PlayList, PlayListMark,
    PlayListMpls, PrimaryAudioStream, PrimaryVideoStream, StnTable, StreamCodingType, SubPath,
    SubPlayItem, SubPlayItemClip,
};
use oxideav_bluray::bdmv::{clpi, index_bdmv, movie_object, mpls, pgs};

/// Feed one buffer to every BDMV parser + derived accessor. Returning
/// normally is the whole assertion.
fn drive_all(buf: &[u8]) {
    if let Ok(pl) = mpls::PlayListMpls::parse(buf) {
        let _ = pl.duration_90k();
        let _ = pl.chapters_with_duration();
        let _ = pl.app_info.uo_mask_table().prohibited_ops();
        for pi in &pl.play_list.play_items {
            let _ = pi.duration_90k();
            let _ = pi.flags.uo_mask_table().reserved_bits();
        }
        for sp in &pl.play_list.sub_paths {
            let _ = sp.kind().display_name();
            for spi in &sp.sub_play_items {
                let _ = spi.duration_90k();
                let _ = spi.num_clips();
            }
        }
    }
    if let Ok(clip) = clpi::ClipInformation::parse(buf) {
        let _ = clip.cpi.primary_video_ep_map();
        for ep in &clip.cpi.ep_map {
            let _ = ep.indexed_span_90k();
            let _ = ep.seek_spn(u32::MAX);
        }
    }
    let _ = index_bdmv::IndexBdmv::parse(buf);
    if let Ok(mobj) = movie_object::MovieObjects::parse(buf) {
        let _ = mobj.disassemble();
    }
    let _ = pgs::parse_segments(buf);
    let _ = pgs::parse_display_sets(buf);
}

/// Every single-byte flip (xor a few masks), plus a length/offset lie at
/// each 4-byte-aligned window, plus every truncation. All must survive.
fn mutate_and_drive(seed: &[u8]) {
    // Baseline: the untouched seed is well-formed enough that at least
    // one parser accepts it (guards against a builder that silently
    // produces garbage — which would make the whole sweep meaningless).
    drive_all(seed);

    // 1. Single-byte perturbations across the whole buffer.
    for i in 0..seed.len() {
        for mask in [0xFFu8, 0x80, 0x01, 0x7F] {
            let mut m = seed.to_vec();
            m[i] ^= mask;
            drive_all(&m);
        }
    }

    // 2. 32-bit "size lie": overwrite each aligned 4-byte window with a
    //    huge / near-max value so a length or section offset claims far
    //    more than the buffer holds.
    for off in 0..seed.len().saturating_sub(4) {
        for &lie in &[0xFFFF_FFFFu32, 0x7FFF_FFFF, 0x0010_0000] {
            let mut m = seed.to_vec();
            m[off..off + 4].copy_from_slice(&lie.to_be_bytes());
            drive_all(&m);
        }
    }

    // 3. Truncation sweep.
    for len in 0..=seed.len() {
        drive_all(&seed[..len]);
    }
}

fn valid_mpls() -> Vec<u8> {
    let pl = PlayListMpls {
        version: *b"0200",
        app_info: AppInfoPlayList {
            playback_type: 1,
            playback_count: 0,
            random_access_flag: 1,
            audio_mix_app_flag: 0,
            lossless_may_bypass_mixer_flag: 0,
            uo_mask: 0x0102_0304_0506_0708,
        },
        play_list: PlayList {
            play_items: vec![
                PlayItem {
                    clip_information_file_name: "00001".into(),
                    clip_codec_identifier: *b"M2TS",
                    connection_condition: ConnectionCondition::NonSeamless,
                    stc_id_ref: 0,
                    in_time_ticks: 0,
                    out_time_ticks: 45_000 * 10,
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
                    flags: PlayItemFlags::default(),
                },
                PlayItem {
                    clip_information_file_name: "00002".into(),
                    clip_codec_identifier: *b"M2TS",
                    connection_condition: ConnectionCondition::SeamlessContinuation,
                    stc_id_ref: 0,
                    in_time_ticks: 0,
                    out_time_ticks: 45_000 * 5,
                    multi_clip_count: 1,
                    angles: Vec::new(),
                    stn_table: StnTable::default(),
                    flags: PlayItemFlags::default(),
                },
            ],
            sub_paths: vec![SubPath {
                sub_path_type: 5,
                is_repeat_subpath: true,
                // A real SubPlayItem (multi-clip, so the mutation matrix
                // reaches the multi-clip entry walk too) — corruption
                // lands inside genuine count-driven SubPath bodies.
                sub_play_items: vec![SubPlayItem {
                    clip_information_file_name: "00007".into(),
                    clip_codec_identifier: *b"M2TS",
                    connection_condition: ConnectionCondition::SeamlessNewStc,
                    stc_id_ref: 1,
                    in_time_ticks: 90,
                    out_time_ticks: 45_000 * 3,
                    sync_play_item_id: 1,
                    sync_start_pts_ticks: 1234,
                    multi_clips: vec![SubPlayItemClip {
                        clip_information_file_name: "00008".into(),
                        clip_codec_identifier: *b"M2TS",
                        stc_id_ref: 2,
                    }],
                }],
            }],
        },
        marks: vec![PlayListMark {
            mark_type: 0x01,
            ref_play_item_id: 0,
            mark_time_ticks: 0,
            entry_es_pid: 0,
            duration_ticks: 0,
        }],
    };
    pl.encode()
}

fn valid_index() -> Vec<u8> {
    let idx = IndexBdmv {
        version: *b"0200",
        app_info: AppInfoBdmv {
            initial_output_mode_preference: 0,
            content_exist_flag: 1,
            video_format: 6,
            frame_rate: 4,
        },
        first_playback_title: IndexEntry {
            object: IndexObjectType::Hdmv {
                playback_type: 0,
                movie_object_id_ref: 0,
            },
        },
        menu_title: IndexEntry {
            object: IndexObjectType::Hdmv {
                playback_type: 1,
                movie_object_id_ref: 0,
            },
        },
        titles: vec![
            IndexEntry {
                object: IndexObjectType::Hdmv {
                    playback_type: 0,
                    movie_object_id_ref: 0,
                },
            },
            IndexEntry {
                object: IndexObjectType::Hdmv {
                    playback_type: 0,
                    movie_object_id_ref: 1,
                },
            },
        ],
    };
    idx.encode()
}

fn valid_mobj() -> Vec<u8> {
    // Two objects; the second carries a couple of 12-byte command words
    // so the per-command loop is exercised under mutation.
    let mobj = MovieObjects {
        version: *b"0200",
        movie_objects: vec![
            MovieObject {
                resume_intention_flag: 1,
                menu_call_mask: 0,
                title_search_mask: 0,
                commands: vec![NavCommand {
                    bytes: [
                        0x51, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    ],
                }],
            },
            MovieObject {
                resume_intention_flag: 0,
                menu_call_mask: 1,
                title_search_mask: 1,
                commands: vec![
                    NavCommand { bytes: [0x00; 12] },
                    NavCommand {
                        bytes: [
                            0x21, 0x82, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
                        ],
                    },
                ],
            },
        ],
    };
    mobj.encode()
}

#[test]
fn mutated_mpls_never_panics() {
    let seed = valid_mpls();
    assert!(
        PlayListMpls::parse(&seed).is_ok(),
        "builder produced invalid .mpls"
    );
    mutate_and_drive(&seed);
}

#[test]
fn mutated_index_never_panics() {
    let seed = valid_index();
    assert!(
        IndexBdmv::parse(&seed).is_ok(),
        "builder produced invalid index.bdmv"
    );
    mutate_and_drive(&seed);
}

#[test]
fn mutated_mobj_never_panics() {
    let seed = valid_mobj();
    assert!(
        MovieObjects::parse(&seed).is_ok(),
        "builder produced invalid MovieObject.bdmv"
    );
    mutate_and_drive(&seed);
}
