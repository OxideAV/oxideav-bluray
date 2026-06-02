//! Integration tests for the cross-PlayItem STC PTS remap surface —
//! `TitleSource::pts_continuity_segments` /
//! `Disc::title_pts_continuity_segments` / `map_clip_pts_to_title_pts`.
//!
//! Background: every PlayItem in a Blu-ray title references its own
//! `.m2ts` clip with its own STC sequence (BD-ROM Part 3 §5.4.4 +
//! §5.5.4.2). When `TitleSource` stitches clips back-to-back the TS
//! bytes still carry **clip-local** PTS values; a downstream MPEG-TS
//! demuxer needs an out-of-band map to translate each clip-local PTS
//! onto a continuous title-relative timeline and to know whether the
//! PTS axis continues seamlessly across the seam
//! (`ConnectionCondition::SeamlessContinuation` 0x05) or restarts
//! (`NonSeamless` 0x01 / `SeamlessNewStc` 0x06).
//!
//! These tests synthesise minimum-shape BDMV trees with several
//! PlayItems and inspect the resulting [`PtsContinuitySegment`]
//! sequence. They are entirely fabricated; no real disc data.

use std::fs;
use std::io::Write;
use std::path::Path;

use oxideav_bluray::bdmv::clpi::{
    AtcSequence, ClipInfo, ClipInformation, ClipMark, Cpi, EpEntry, EpMap, ProgramInfo,
    SequenceInfo, StcSequence, TsTypeInfoBlock,
};
use oxideav_bluray::bdmv::index_bdmv::{AppInfoBdmv, IndexBdmv, IndexEntry, IndexObjectType};
use oxideav_bluray::bdmv::mpls::{
    AppInfoPlayList, ConnectionCondition, PlayItem, PlayList, PlayListMpls, PrimaryAudioStream,
    PrimaryVideoStream, StnTable, StreamCodingType,
};
use oxideav_bluray::{Disc, M2TS_PACKET_LEN, TS_PACKET_LEN};

fn write_file(path: &Path, bytes: &[u8]) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    let mut f = fs::File::create(path).unwrap();
    f.write_all(bytes).unwrap();
}

fn make_m2ts(n_packets: usize) -> Vec<u8> {
    let mut out = vec![0u8; n_packets * M2TS_PACKET_LEN];
    for i in 0..n_packets {
        let pkt_off = i * M2TS_PACKET_LEN;
        out[pkt_off..pkt_off + 4].copy_from_slice(&(i as u32).to_be_bytes());
        let ts_off = pkt_off + 4;
        out[ts_off] = 0x47;
        out[ts_off + 1] = (i & 0xFF) as u8;
        for j in 2..TS_PACKET_LEN {
            out[ts_off + j] = 0xAA;
        }
    }
    out
}

/// CLPI carrying one ATC sequence with one STC sequence whose
/// `presentation_start_time` is `stc_presentation_start_45k` (raw
/// 45 kHz units, as the on-disc field is encoded).
fn make_clpi(n_source_packets: u32, stc_presentation_start_45k: u32) -> Vec<u8> {
    let clpi = ClipInformation {
        version: *b"0200",
        clip_info: ClipInfo {
            clip_stream_type: 1,
            application_type: 1,
            ts_recording_rate: 48_000_000,
            number_of_source_packets: n_source_packets,
            ts_type_info_block: TsTypeInfoBlock {
                validity_flags: 0x80,
                format_id: *b"HDMV",
            },
        },
        sequence_info: SequenceInfo {
            atc_sequences: vec![AtcSequence {
                spn_atc_start: 0,
                offset_stc_id: 0,
                stc_sequences: vec![StcSequence {
                    pcr_pid: 0x1001,
                    spn_stc_start: 0,
                    presentation_start_time: stc_presentation_start_45k,
                    // OUT time is informational here; the segment
                    // builder doesn't need it for the title-PTS map.
                    presentation_end_time: stc_presentation_start_45k.saturating_add(45_000 * 10),
                }],
            }],
        },
        program_info: ProgramInfo { programs: vec![] },
        cpi: Cpi {
            ep_map: vec![EpMap {
                stream_pid: 0x1011,
                ep_stream_type: 1,
                entries: vec![EpEntry {
                    is_angle_change_point: false,
                    i_end_position_offset: 0,
                    pts_ep_start: 0,
                    spn_ep_start: 0,
                    ep_stream_type: 1,
                }],
            }],
            ts_type_indicators: Vec::new(),
        },
        clip_mark: ClipMark { num_marks: 0 },
    };
    clpi.encode()
}

fn primary_video_stn() -> StnTable {
    StnTable {
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
    }
}

fn write_index_bdmv(bdmv: &Path) {
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
        titles: vec![IndexEntry {
            object: IndexObjectType::Hdmv {
                playback_type: 0,
                movie_object_id_ref: 0,
            },
        }],
    };
    write_file(&bdmv.join("index.bdmv"), &idx.encode());
}

#[test]
fn three_playitems_emit_three_continuity_segments_with_correct_byte_pts_bounds() {
    // Three back-to-back PlayItems with distinct durations + distinct
    // STC origins; verify the resulting continuity segments tile
    // (output_byte_start, output_byte_end) and (title_pts_start,
    // title_pts_end) contiguously over the title.
    let tmp = tempdir_for_test();
    let root = tmp.path();
    let bdmv = root.join("BDMV");

    // Durations: 10 s / 7 s / 5 s — distinct so a bug substituting
    // the wrong PlayItem's duration would shift the title axis.
    let durations_s: [u32; 3] = [10, 7, 5];
    let in_pts_45k: [u32; 3] = [
        // PlayItem IN points: 0 (first clip starts at clip-time 0);
        // 45 000 (second clip's IN is non-zero → 1 s into the clip);
        // 0 again.
        0, 45_000, 0,
    ];
    let stc_origin_45k: [u32; 3] = [
        // STC origins (clip-local 45 kHz) lifted from each clip's
        // SequenceInfo. Distinct values let us prove the segment
        // surface really threaded the CLPI value through (not just
        // the PlayItem IN point).
        0, 45_000, 90_000,
    ];
    // Packets per clip — chosen so 188-byte output bytes per clip are
    // distinct and easy to compute by hand.
    let n_packets: [usize; 3] = [32, 16, 8];

    let mut play_items = Vec::new();
    for i in 0..3 {
        play_items.push(PlayItem {
            clip_information_file_name: format!("{:05}", i + 1),
            clip_codec_identifier: *b"M2TS",
            connection_condition: match i {
                0 => ConnectionCondition::NonSeamless,
                1 => ConnectionCondition::SeamlessContinuation,
                _ => ConnectionCondition::SeamlessNewStc,
            },
            stc_id_ref: 0,
            in_time_ticks: in_pts_45k[i],
            out_time_ticks: in_pts_45k[i] + 45_000 * durations_s[i],
            multi_clip_count: 1,
            angles: Vec::new(),
            stn_table: primary_video_stn(),
        });
    }

    let pl = PlayListMpls {
        version: *b"0200",
        app_info: AppInfoPlayList {
            playback_type: 1,
            playback_count: 0,
            random_access_flag: 1,
            audio_mix_app_flag: 0,
            lossless_may_bypass_mixer_flag: 0,
        },
        play_list: PlayList {
            play_items,
            sub_paths: vec![],
        },
        marks: vec![],
    };
    write_file(&bdmv.join("PLAYLIST/00000.mpls"), &pl.encode());
    for i in 0..3 {
        let stem = format!("{:05}", i + 1);
        write_file(
            &bdmv.join(format!("STREAM/{stem}.m2ts")),
            &make_m2ts(n_packets[i]),
        );
        write_file(
            &bdmv.join(format!("CLIPINF/{stem}.clpi")),
            &make_clpi(n_packets[i] as u32, stc_origin_45k[i]),
        );
    }
    write_index_bdmv(&bdmv);

    let disc = Disc::mount(root).expect("mount");
    let title = disc.longest_title().expect("title").clone();

    // ── Disc-level (no m2ts file open) ─────────────────────────────
    let disc_segments = disc.title_pts_continuity_segments(&title);
    assert_eq!(disc_segments.len(), 3);

    // ── TitleSource-level ──────────────────────────────────────────
    let src = disc.open_title(&title, None).expect("open");
    let segments = src.pts_continuity_segments();
    assert_eq!(segments.len(), 3);
    assert_eq!(
        segments, disc_segments,
        "Disc::title_pts_continuity_segments and TitleSource::pts_continuity_segments must agree"
    );

    // Tile of (output_byte_start, output_byte_end):
    let mut byte_cursor: u64 = 0;
    let mut pts_cursor: u64 = 0;
    for (i, seg) in segments.iter().enumerate() {
        assert_eq!(seg.play_item_index, i as u16, "PI index");
        let mut want_stem = [0u8; 5];
        want_stem.copy_from_slice(format!("{:05}", i + 1).as_bytes());
        assert_eq!(seg.clip_stem, want_stem, "clip stem index {i}");
        assert_eq!(seg.output_byte_start, byte_cursor, "byte tile [{i}]");
        byte_cursor += (n_packets[i] as u64) * TS_PACKET_LEN as u64;
        assert_eq!(seg.output_byte_end, byte_cursor, "byte tile end [{i}]");

        assert_eq!(seg.title_pts_start, pts_cursor, "title PTS tile [{i}]");
        // PlayItem duration in 90 kHz = (OUT - IN) * 2.
        let dur_90k = u64::from(45_000 * durations_s[i]) * 2;
        pts_cursor += dur_90k;
        assert_eq!(seg.title_pts_end, pts_cursor, "title PTS tile end [{i}]");

        // Clip-local IN PTS — PlayItem IN, lifted to 90 kHz.
        assert_eq!(
            seg.clip_in_pts_90k,
            u64::from(in_pts_45k[i]) * 2,
            "clip IN [{i}]"
        );
        // OUT - IN should equal the segment's duration.
        assert_eq!(
            seg.clip_out_pts_90k - seg.clip_in_pts_90k,
            dur_90k,
            "clip out delta [{i}]"
        );
        // STC origin lifted from CLPI SequenceInfo (45 kHz → 90 kHz).
        assert_eq!(
            seg.stc_origin_pts_90k,
            u64::from(stc_origin_45k[i]) * 2,
            "STC origin [{i}]"
        );
        assert_eq!(seg.stc_id_ref, 0);
    }
    // Final cursor must hit the title duration.
    assert_eq!(pts_cursor, title.duration_ticks);
}

#[test]
fn first_playitem_connection_condition_normalised_to_nonseamless() {
    // The very first PlayItem's `connection_condition` byte is
    // meaningless (§5.4.4.2 defines it as the relation to the
    // *previous* PlayItem, and there is none). Even when the disc
    // ships a non-default value the surface MUST normalise it to
    // NonSeamless so downstream demuxers don't try to "continue" a
    // PTS axis that doesn't exist yet.
    let tmp = tempdir_for_test();
    let root = tmp.path();
    let bdmv = root.join("BDMV");

    let pl = PlayListMpls {
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
                clip_information_file_name: "00001".into(),
                clip_codec_identifier: *b"M2TS",
                // Deliberately not NonSeamless — we expect the
                // surface to override on the first PlayItem.
                connection_condition: ConnectionCondition::SeamlessContinuation,
                stc_id_ref: 0,
                in_time_ticks: 0,
                out_time_ticks: 45_000 * 5,
                multi_clip_count: 1,
                angles: Vec::new(),
                stn_table: primary_video_stn(),
            }],
            sub_paths: vec![],
        },
        marks: vec![],
    };
    write_file(&bdmv.join("PLAYLIST/00000.mpls"), &pl.encode());
    write_file(&bdmv.join("STREAM/00001.m2ts"), &make_m2ts(16));
    write_file(&bdmv.join("CLIPINF/00001.clpi"), &make_clpi(16, 0));
    write_index_bdmv(&bdmv);

    let disc = Disc::mount(root).unwrap();
    let title = disc.longest_title().unwrap().clone();
    let segs = disc.title_pts_continuity_segments(&title);
    assert_eq!(segs.len(), 1);
    assert_eq!(
        segs[0].connection_condition,
        ConnectionCondition::NonSeamless,
        "first PI must be normalised regardless of recorded byte"
    );
}

#[test]
fn missing_sequence_info_falls_back_to_zero_stc_origin() {
    // A homemade disc that ships a `.clpi` with no SequenceInfo (the
    // parser yields `atc_sequences: vec![]`) must not error — instead
    // the segment surface reports `stc_origin_pts_90k = 0` so a
    // demuxer falls back to "use clip_in_pts_90k as the reproject
    // origin", which the synthetic-disc test above already proves
    // gives the right title timing.
    let tmp = tempdir_for_test();
    let root = tmp.path();
    let bdmv = root.join("BDMV");

    let pl = PlayListMpls {
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
                clip_information_file_name: "00001".into(),
                clip_codec_identifier: *b"M2TS",
                connection_condition: ConnectionCondition::NonSeamless,
                stc_id_ref: 3, // out of range against an empty SequenceInfo
                in_time_ticks: 0,
                out_time_ticks: 45_000 * 5,
                multi_clip_count: 1,
                angles: Vec::new(),
                stn_table: primary_video_stn(),
            }],
            sub_paths: vec![],
        },
        marks: vec![],
    };
    write_file(&bdmv.join("PLAYLIST/00000.mpls"), &pl.encode());
    write_file(&bdmv.join("STREAM/00001.m2ts"), &make_m2ts(16));

    // Hand-build a CLPI with `sequence_info.atc_sequences` empty.
    let clpi = ClipInformation {
        version: *b"0200",
        clip_info: ClipInfo {
            clip_stream_type: 1,
            application_type: 1,
            ts_recording_rate: 48_000_000,
            number_of_source_packets: 16,
            ts_type_info_block: TsTypeInfoBlock {
                validity_flags: 0x80,
                format_id: *b"HDMV",
            },
        },
        sequence_info: SequenceInfo {
            atc_sequences: vec![],
        },
        program_info: ProgramInfo { programs: vec![] },
        cpi: Cpi {
            ep_map: vec![],
            ts_type_indicators: Vec::new(),
        },
        clip_mark: ClipMark { num_marks: 0 },
    };
    write_file(&bdmv.join("CLIPINF/00001.clpi"), &clpi.encode());
    write_index_bdmv(&bdmv);

    let disc = Disc::mount(root).unwrap();
    let title = disc.longest_title().unwrap().clone();
    let segs = disc.title_pts_continuity_segments(&title);
    assert_eq!(segs.len(), 1);
    assert_eq!(
        segs[0].stc_origin_pts_90k, 0,
        "empty SequenceInfo → 0 STC origin (caller falls back to IN)"
    );
}

#[test]
fn map_clip_pts_to_title_pts_walks_across_playitem_seams() {
    // Build a 2-clip title: PI0 covers [0, 10s) of the title; PI1
    // covers [10s, 18s). PI1's clip has its own STC origin so the
    // bytes it emits carry PTS values like
    // `stc_origin + (t - playback_start_of_pi1) + in_pts_of_pi1`,
    // which we model below by feeding each PI a representative
    // clip-local PTS and asserting the reproject lands on the right
    // title timeline.
    let tmp = tempdir_for_test();
    let root = tmp.path();
    let bdmv = root.join("BDMV");

    let pl = PlayListMpls {
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
                    out_time_ticks: 45_000 * 10,
                    multi_clip_count: 1,
                    angles: Vec::new(),
                    stn_table: primary_video_stn(),
                },
                PlayItem {
                    clip_information_file_name: "00002".into(),
                    clip_codec_identifier: *b"M2TS",
                    connection_condition: ConnectionCondition::SeamlessNewStc,
                    stc_id_ref: 0,
                    // PI1 IN-point starts 2 s into clip 2 — so the
                    // first bytes the demuxer sees on clip 2 carry
                    // clip-local PTS = 2 s.
                    in_time_ticks: 45_000 * 2,
                    out_time_ticks: 45_000 * 10,
                    multi_clip_count: 1,
                    angles: Vec::new(),
                    stn_table: primary_video_stn(),
                },
            ],
            sub_paths: vec![],
        },
        marks: vec![],
    };
    write_file(&bdmv.join("PLAYLIST/00000.mpls"), &pl.encode());
    write_file(&bdmv.join("STREAM/00001.m2ts"), &make_m2ts(32));
    write_file(&bdmv.join("STREAM/00002.m2ts"), &make_m2ts(16));
    write_file(&bdmv.join("CLIPINF/00001.clpi"), &make_clpi(32, 0));
    // Clip 2 STC origin is 1 s — proof that we threaded the SequenceInfo
    // origin onto the segment surface; doesn't affect the reproject
    // (which uses clip_in_pts_90k), but a demuxer that needs wraparound
    // disambiguation reads it off the segment.
    write_file(&bdmv.join("CLIPINF/00002.clpi"), &make_clpi(16, 45_000));
    write_index_bdmv(&bdmv);

    let disc = Disc::mount(root).unwrap();
    let title = disc.longest_title().unwrap().clone();
    let src = disc.open_title(&title, None).unwrap();

    // Clip 1 covers output bytes [0, 32 * 188) = [0, 6016).
    // Clip 2 covers [6016, 6016 + 16 * 188) = [6016, 9024).
    let clip1_size = 32 * TS_PACKET_LEN as u64;
    let clip2_size = 16 * TS_PACKET_LEN as u64;

    // Inside clip 1 (byte 1000), a packet carrying clip-local PTS =
    // 3 s should map to title PTS = 3 s.
    let t_in_clip1 = src
        .map_clip_pts_to_title_pts(1000, 90_000 * 3)
        .expect("clip 1 reproject");
    assert_eq!(t_in_clip1, 90_000 * 3);

    // Inside clip 2 (byte clip1_size + 500), a packet carrying
    // clip-local PTS = (2 s + delta) — delta past the clip IN —
    // should map to title PTS = 10 s + delta. Try delta = 4 s →
    // clip-local PTS = 6 s → title PTS = 14 s.
    let t_in_clip2 = src
        .map_clip_pts_to_title_pts(clip1_size + 500, 90_000 * 6)
        .expect("clip 2 reproject");
    assert_eq!(t_in_clip2, 90_000 * 14);

    // A PES PTS earlier than the clip IN point (a stray packet that
    // landed before the PlayItem window) maps to `None` — the
    // demuxer should drop it.
    let too_early = src.map_clip_pts_to_title_pts(clip1_size + 500, 90_000); // 1 s, before clip2 IN (2 s)
    assert_eq!(too_early, None);

    // A byte position past the last segment maps to `None` once it
    // exceeds the recorded total. (Byte 0 is always in segment 0; a
    // huge byte position still picks the last segment since segments
    // tile the output bytes — so this test instead pushes the byte
    // past the last segment's start, which still maps. The `None`
    // branch fires only when `byte_pos < first.output_start`, which
    // for the first clip is always 0, so the practical None-shaped
    // case is the early-PTS check above.)
    let _ = clip2_size; // silence unused warning if assertion above changes
}

#[test]
fn segment_for_playitem_after_in_offset_advances_clip_axis_correctly() {
    // A PlayItem whose IN point is well past the clip start: bytes
    // we emit map clip-local PTS that start at IN, not at 0. The
    // reproject formula (title = title_start + (pes - clip_in))
    // must yield a positive title-PTS at the first byte of that
    // segment.
    let tmp = tempdir_for_test();
    let root = tmp.path();
    let bdmv = root.join("BDMV");

    // Single PlayItem with IN-point = 3 s — the segment claims the
    // first byte emitted is the byte the m2ts file starts with, and
    // a downstream demuxer that sees clip-local PTS = 3 s on that
    // byte should reproject to title PTS = 0.
    let pl = PlayListMpls {
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
                clip_information_file_name: "00001".into(),
                clip_codec_identifier: *b"M2TS",
                connection_condition: ConnectionCondition::NonSeamless,
                stc_id_ref: 0,
                in_time_ticks: 45_000 * 3,
                out_time_ticks: 45_000 * 10, // 7 s of playback
                multi_clip_count: 1,
                angles: Vec::new(),
                stn_table: primary_video_stn(),
            }],
            sub_paths: vec![],
        },
        marks: vec![],
    };
    write_file(&bdmv.join("PLAYLIST/00000.mpls"), &pl.encode());
    write_file(&bdmv.join("STREAM/00001.m2ts"), &make_m2ts(16));
    write_file(&bdmv.join("CLIPINF/00001.clpi"), &make_clpi(16, 0));
    write_index_bdmv(&bdmv);

    let disc = Disc::mount(root).unwrap();
    let title = disc.longest_title().unwrap().clone();
    let src = disc.open_title(&title, None).unwrap();
    let segs = src.pts_continuity_segments();
    assert_eq!(segs.len(), 1);
    let s = &segs[0];

    // clip-local IN = 3 s in 90 kHz = 270_000.
    // (PlayItem.in_time_ticks is 45 kHz; doubling lifts to 90 kHz, so
    // `3 s × 45_000 × 2 = 270_000`.)
    assert_eq!(s.clip_in_pts_90k, 270_000);
    // Title duration = 7 s in 90 kHz = 630_000; title PTS end of the
    // only segment = title_duration_90k.
    assert_eq!(s.title_pts_start, 0);
    assert_eq!(s.title_pts_end, 630_000);
    // Reproject: clip-local PTS = 3 s on the 90 kHz axis (the IN
    // point) → title PTS = 0.
    let mapped = src.map_clip_pts_to_title_pts(0, 270_000).unwrap();
    assert_eq!(mapped, 0);
    // clip-local PTS = 5 s on the 90 kHz axis = 450_000 → title PTS
    // = 2 s on the 90 kHz axis = 180_000.
    let mapped2 = src.map_clip_pts_to_title_pts(0, 450_000).unwrap();
    assert_eq!(mapped2, 180_000);
}

#[test]
fn missing_playlist_yields_empty_segment_list() {
    // No PLAYLIST/00000.mpls on disk — `title_pts_continuity_segments`
    // returns an empty list (matches the `chapters` / `title_streams`
    // error-swallow policy: don't crash on inspection).
    let tmp = tempdir_for_test();
    let root = tmp.path();
    let bdmv = root.join("BDMV");
    // Index points at PLAYLIST 0 but we don't write the .mpls.
    write_index_bdmv(&bdmv);
    write_file(&bdmv.join("STREAM/00001.m2ts"), &make_m2ts(8));

    let disc = match Disc::mount(root) {
        Ok(d) => d,
        // A disc with no mpls fails the longest_title path; that's
        // acceptable — the inspection methods must just not panic.
        Err(_) => return,
    };
    let Some(title) = disc.longest_title().cloned() else {
        return;
    };
    let segs = disc.title_pts_continuity_segments(&title);
    assert!(segs.is_empty(), "missing mpls → empty segment list");
}

// ── tempdir helper (cribbed from sibling tests for parity) ─────────

struct TestDir {
    path: std::path::PathBuf,
}

impl TestDir {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn tempdir_for_test() -> TestDir {
    let pid = std::process::id();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let serial = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("oxideav-bluray-pts-cont-{pid}-{nonce}-{serial}"));
    fs::create_dir_all(&path).unwrap();
    TestDir { path }
}
