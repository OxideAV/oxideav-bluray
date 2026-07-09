//! Integration test for mid-stream angle-change-point enumeration.
//!
//! Synthesises a multi-PlayItem multi-angle BDMV tree where each clip's
//! `.clpi` carries a primary-video EP_map with a mix of plain and
//! angle-change-capable rows (`is_angle_change_point = 1`, BD-ROM AV
//! §5.7), then asserts:
//!
//! 1. [`TitleSource::angle_change_points`] surfaces every flagged row
//!    folded onto the title timeline + output-byte axis.
//! 2. [`TitleSource::next_angle_change_point`] does the right
//!    "next safe angle switch boundary" walk a player UI needs.
//! 3. [`Disc::title_angle_change_points`] / `…_with_angle` give the
//!    same answers without opening any `.m2ts`.
//! 4. A row whose clip-local PTS sits before its PlayItem's IN point
//!    is silently dropped (the streamer can't reach it).
//! 5. A title with no EP_map rows flagged as angle-change yields an
//!    empty list (not a panic / not an error).
//!
//! Everything here is fabricated by the test; no real disc data.

use std::fs;
use std::io::Write;
use std::path::Path;

use oxideav_bluray::bdmv::clpi::{
    ClipInfo, ClipInformation, ClipMark, Cpi, EpEntry, EpMap, ProgramInfo, SequenceInfo,
    TsTypeInfoBlock,
};
use oxideav_bluray::bdmv::index_bdmv::{AppInfoBdmv, IndexBdmv, IndexEntry, IndexObjectType};
use oxideav_bluray::bdmv::mpls::{
    AngleClip, AppInfoPlayList, ConnectionCondition, PlayItem, PlayItemFlags, PlayList,
    PlayListMpls, PrimaryAudioStream, PrimaryVideoStream, StnTable, StreamCodingType,
};
use oxideav_bluray::{Disc, M2TS_PACKET_LEN, TS_PACKET_LEN};

fn write_file(path: &Path, bytes: &[u8]) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    let mut f = fs::File::create(path).unwrap();
    f.write_all(bytes).unwrap();
}

/// Zero-filled `.m2ts` of `n_packets` 192-byte BDAV packets. The body
/// is irrelevant here — this test exercises the EP_map / metadata path
/// only, not the streaming path.
fn make_m2ts(n_packets: usize) -> Vec<u8> {
    vec![0u8; n_packets * M2TS_PACKET_LEN]
}

/// Build a `.clpi` whose primary-video EP_map carries the supplied
/// `(pts_ep_start, spn_ep_start, is_angle_change_point)` rows.
fn make_clpi_with_angle_flags(
    rows: &[(u32, u32, bool)],
    n_source_packets: u32,
    stream_pid: u16,
) -> Vec<u8> {
    let entries: Vec<EpEntry> = rows
        .iter()
        .map(|&(pts, spn, is_acp)| EpEntry {
            is_angle_change_point: is_acp,
            i_end_position_offset: 0,
            pts_ep_start: pts,
            spn_ep_start: spn,
            ep_stream_type: 1,
        })
        .collect();
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
            atc_sequences: vec![],
        },
        program_info: ProgramInfo { programs: vec![] },
        cpi: Cpi {
            ep_map: vec![EpMap {
                stream_pid,
                ep_stream_type: 1,
                entries,
            }],
            ts_type_indicators: Vec::new(),
        },
        clip_mark: ClipMark { num_marks: 0 },
    };
    clpi.encode()
}

fn primary_video() -> Vec<PrimaryVideoStream> {
    vec![PrimaryVideoStream {
        elementary_pid: 0x1011,
        coding_type: StreamCodingType::AvcVideo,
        video_format: 0x06,
        frame_rate: 0x03,
        aspect_ratio: 0x03,
    }]
}

fn primary_audio() -> Vec<PrimaryAudioStream> {
    vec![PrimaryAudioStream {
        elementary_pid: 0x1100,
        coding_type: StreamCodingType::Ac3Audio,
        audio_format: 0x03,
        sample_rate: 0x01,
        language_code: *b"eng",
    }]
}

fn index_bdmv_one_hdmv() -> IndexBdmv {
    IndexBdmv {
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
    }
}

/// Common multi-angle synthetic disc: 2 PlayItems × 3 angles.
///
/// PlayItem 0: primary clip 00100 (+ alts 00101, 00102), IN=0,
///   OUT=45_000*10 (10 s). EP_map:
///     (0,       0,  is_acp=false)  → spn 0
///     (45_000,  8,  is_acp=true )  → mid-clip angle switch boundary
///     (180_000, 24, is_acp=true )  → second one
///
/// PlayItem 1: primary clip 00200 (+ alts 00201, 00202), IN=0,
///   OUT=45_000*10 (10 s). EP_map:
///     (0,       0,  is_acp=false)
///     (90_000,  16, is_acp=true )  → mid-second-clip boundary
fn build_disc(root: &Path) {
    let bdmv = root.join("BDMV");
    let pl = PlayListMpls {
        version: *b"0200",
        app_info: AppInfoPlayList {
            playback_type: 1,
            playback_count: 0,
            random_access_flag: 1,
            audio_mix_app_flag: 0,
            lossless_may_bypass_mixer_flag: 0,
            uo_mask: 0,
        },
        play_list: PlayList {
            play_items: vec![
                PlayItem {
                    clip_information_file_name: "00100".into(),
                    clip_codec_identifier: *b"M2TS",
                    connection_condition: ConnectionCondition::NonSeamless,
                    stc_id_ref: 0,
                    in_time_ticks: 0,
                    out_time_ticks: 45_000 * 10,
                    multi_clip_count: 3,
                    angles: vec![
                        AngleClip {
                            clip_information_file_name: "00101".into(),
                            clip_codec_identifier: *b"M2TS",
                            stc_id_ref: 0,
                        },
                        AngleClip {
                            clip_information_file_name: "00102".into(),
                            clip_codec_identifier: *b"M2TS",
                            stc_id_ref: 0,
                        },
                    ],
                    stn_table: StnTable {
                        primary_video: primary_video(),
                        primary_audio: primary_audio(),
                        ..StnTable::default()
                    },
                    flags: PlayItemFlags::default(),
                },
                PlayItem {
                    clip_information_file_name: "00200".into(),
                    clip_codec_identifier: *b"M2TS",
                    connection_condition: ConnectionCondition::SeamlessContinuation,
                    stc_id_ref: 0,
                    in_time_ticks: 0,
                    out_time_ticks: 45_000 * 10,
                    multi_clip_count: 3,
                    angles: vec![
                        AngleClip {
                            clip_information_file_name: "00201".into(),
                            clip_codec_identifier: *b"M2TS",
                            stc_id_ref: 0,
                        },
                        AngleClip {
                            clip_information_file_name: "00202".into(),
                            clip_codec_identifier: *b"M2TS",
                            stc_id_ref: 0,
                        },
                    ],
                    stn_table: StnTable {
                        primary_video: primary_video(),
                        primary_audio: primary_audio(),
                        ..StnTable::default()
                    },
                    flags: PlayItemFlags::default(),
                },
            ],
            sub_paths: vec![],
        },
        marks: vec![],
    };
    write_file(&bdmv.join("PLAYLIST/00000.mpls"), &pl.encode());

    // PlayItem 0 clips (3 angles, 64 packets each). Each angle's CPI
    // ships the same angle-change pattern — the spec mandates that
    // alternate angles' I-frames sit at the same SPN.
    let pi0_eps = [(0u32, 0u32, false), (45_000, 8, true), (180_000, 24, true)];
    for stem in ["00100", "00101", "00102"] {
        write_file(&bdmv.join(format!("STREAM/{stem}.m2ts")), &make_m2ts(64));
        write_file(
            &bdmv.join(format!("CLIPINF/{stem}.clpi")),
            &make_clpi_with_angle_flags(&pi0_eps, 64, 0x1011),
        );
    }

    // PlayItem 1 clips (3 angles, 64 packets each), one ACP row.
    let pi1_eps = [(0u32, 0u32, false), (90_000, 16, true)];
    for stem in ["00200", "00201", "00202"] {
        write_file(&bdmv.join(format!("STREAM/{stem}.m2ts")), &make_m2ts(64));
        write_file(
            &bdmv.join(format!("CLIPINF/{stem}.clpi")),
            &make_clpi_with_angle_flags(&pi1_eps, 64, 0x1011),
        );
    }

    write_file(&bdmv.join("index.bdmv"), &index_bdmv_one_hdmv().encode());
}

#[test]
fn title_source_lists_every_angle_change_point() {
    let tmp = tempdir_for_test();
    build_disc(tmp.path());

    let disc = Disc::mount(tmp.path()).expect("mount");
    let title = disc.longest_title().expect("title").clone();
    let src = disc.open_title(&title, None).expect("open title");

    let acps = src.angle_change_points();
    assert_eq!(acps.len(), 3, "two ACPs in PI0 + one in PI1 = 3 total");

    // PI0 spans clip-local PTS [0, ?]; title PTS equals clip-local
    // because IN=0. The CPI encoder truncates `pts_ep_start` to its
    // top 23 bits (the low 9 bits are not stored — §5.7 fine-row
    // packing), so 45_000 round-trips as `(45_000 >> 9) << 9 = 44_544`
    // and 180_000 as `(180_000 >> 9) << 9 = 179_712`.
    let truncate = |p: u32| (p >> 9) << 9;
    assert_eq!(acps[0].play_item_index, 0);
    assert_eq!(&acps[0].clip_stem, b"00100");
    assert_eq!(acps[0].clip_pts_90k, truncate(45_000));
    assert_eq!(acps[0].title_pts_90k, u64::from(truncate(45_000)));
    assert_eq!(acps[0].spn, 8);
    assert_eq!(acps[0].output_byte, 8 * TS_PACKET_LEN as u64);

    assert_eq!(acps[1].play_item_index, 0);
    assert_eq!(acps[1].clip_pts_90k, truncate(180_000));
    assert_eq!(acps[1].title_pts_90k, u64::from(truncate(180_000)));
    assert_eq!(acps[1].spn, 24);
    assert_eq!(acps[1].output_byte, 24 * TS_PACKET_LEN as u64);

    // PI1: clip-local PTS round-trips as 89_600, but title_pts is
    // offset by PI0's duration (10 s = 900_000 ticks).
    assert_eq!(acps[2].play_item_index, 1);
    assert_eq!(&acps[2].clip_stem, b"00200");
    assert_eq!(acps[2].clip_pts_90k, truncate(90_000));
    assert_eq!(acps[2].title_pts_90k, 900_000 + u64::from(truncate(90_000)));
    assert_eq!(acps[2].spn, 16);
    // Output offset: PI0 contributed 64 packets, PI1 SPN 16 sits 16
    // packets in.
    let pi0_output = 64 * TS_PACKET_LEN as u64;
    assert_eq!(acps[2].output_byte, pi0_output + 16 * TS_PACKET_LEN as u64);
}

#[test]
fn next_angle_change_point_walks_in_title_order() {
    let tmp = tempdir_for_test();
    build_disc(tmp.path());

    let disc = Disc::mount(tmp.path()).expect("mount");
    let title = disc.longest_title().expect("title").clone();
    let src = disc.open_title(&title, None).expect("open title");

    // CPI encoder truncates `pts_ep_start` to its top 23 bits
    // (§5.7), so 45_000 round-trips as 44_544, 180_000 as 179_712,
    // 90_000 as 89_600.
    let trunc_45k: u64 = (45_000u32 >> 9 << 9) as u64;
    let trunc_180k: u64 = (180_000u32 >> 9 << 9) as u64;
    let trunc_90k: u64 = (90_000u32 >> 9 << 9) as u64;

    // Before the first ACP → the first ACP.
    let p = src.next_angle_change_point(0).expect("ACP available");
    assert_eq!(p.title_pts_90k, trunc_45k);

    // Exactly on the first ACP → the first ACP (at-or-after).
    let p = src.next_angle_change_point(trunc_45k).expect("ACP");
    assert_eq!(p.title_pts_90k, trunc_45k);

    // One tick past → the second ACP.
    let p = src.next_angle_change_point(trunc_45k + 1).expect("ACP");
    assert_eq!(p.title_pts_90k, trunc_180k);

    // Across the PlayItem seam → PI1's first ACP.
    let p = src.next_angle_change_point(500_000).expect("ACP in PI1");
    assert_eq!(p.title_pts_90k, 900_000 + trunc_90k);

    // Past every ACP → None.
    assert!(src.next_angle_change_point(u64::MAX).is_none());
}

#[test]
fn disc_title_angle_change_points_matches_source() {
    let tmp = tempdir_for_test();
    build_disc(tmp.path());

    let disc = Disc::mount(tmp.path()).expect("mount");
    let title = disc.longest_title().expect("title").clone();

    let from_source = disc
        .open_title(&title, None)
        .expect("open title")
        .angle_change_points();
    let from_disc = disc.title_angle_change_points(&title);

    assert_eq!(from_source.len(), from_disc.len());
    for (a, b) in from_source.iter().zip(from_disc.iter()) {
        assert_eq!(a, b);
    }
}

#[test]
fn disc_title_angle_change_points_with_angle_matches_alt_angle() {
    let tmp = tempdir_for_test();
    build_disc(tmp.path());

    let disc = Disc::mount(tmp.path()).expect("mount");
    let title = disc.longest_title().expect("title").clone();

    // Alt-angle CPI carries the same ACP rows (the test deliberately
    // gives every angle's `.clpi` the same EP_map). A picky reading of
    // the title timeline is the right invariant: the boundaries don't
    // shift when the user switches angles.
    let from_angle_0 = disc.title_angle_change_points_with_angle(&title, 0);
    let from_angle_1 = disc.title_angle_change_points_with_angle(&title, 1);
    let from_angle_2 = disc.title_angle_change_points_with_angle(&title, 2);

    assert_eq!(from_angle_0.len(), 3);
    assert_eq!(from_angle_0.len(), from_angle_1.len());
    assert_eq!(from_angle_0.len(), from_angle_2.len());

    for (a, b) in from_angle_0.iter().zip(from_angle_1.iter()) {
        // Different clip stems, but the title-timeline coordinates
        // line up exactly.
        assert_eq!(a.title_pts_90k, b.title_pts_90k);
        assert_eq!(a.spn, b.spn);
        assert_eq!(a.output_byte, b.output_byte);
    }
    // Out-of-range angle → empty list (matches open_title_with_angle).
    let out_of_range = disc.title_angle_change_points_with_angle(&title, 9);
    assert!(out_of_range.is_empty());
}

#[test]
fn in_point_drops_pre_in_acp_rows() {
    // Build a single-PlayItem disc whose CPI advertises an ACP at
    // clip-local PTS 30_000 — but the PlayItem's IN point is 60_000.
    // The streamer never emits bytes before IN, so an ACP advertised
    // ahead of IN is unreachable and must be filtered out.
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
            uo_mask: 0,
        },
        play_list: PlayList {
            play_items: vec![PlayItem {
                clip_information_file_name: "00001".into(),
                clip_codec_identifier: *b"M2TS",
                connection_condition: ConnectionCondition::NonSeamless,
                stc_id_ref: 0,
                // IN is 30_000 in 45 kHz units → 60_000 in 90 kHz.
                in_time_ticks: 30_000,
                out_time_ticks: 45_000 * 10,
                multi_clip_count: 1,
                angles: Vec::new(),
                stn_table: StnTable {
                    primary_video: primary_video(),
                    primary_audio: primary_audio(),
                    ..StnTable::default()
                },
                flags: PlayItemFlags::default(),
            }],
            sub_paths: vec![],
        },
        marks: vec![],
    };
    write_file(&bdmv.join("PLAYLIST/00000.mpls"), &pl.encode());

    // ACP at clip-local 30_000 (< 60_000 IN, must be dropped) plus a
    // reachable ACP at clip-local 120_000.
    let rows = [(30_000u32, 5u32, true), (120_000, 20, true)];
    write_file(&bdmv.join("STREAM/00001.m2ts"), &make_m2ts(64));
    write_file(
        &bdmv.join("CLIPINF/00001.clpi"),
        &make_clpi_with_angle_flags(&rows, 64, 0x1011),
    );
    write_file(&bdmv.join("index.bdmv"), &index_bdmv_one_hdmv().encode());

    let disc = Disc::mount(root).expect("mount");
    let title = disc.longest_title().expect("title").clone();
    let acps = disc
        .open_title(&title, None)
        .expect("open title")
        .angle_change_points();
    assert_eq!(acps.len(), 1, "pre-IN ACP dropped, one reachable left");
    // 120_000 truncates through the CPI encoder to 119_808.
    let trunc_120k: u32 = 120_000u32 >> 9 << 9;
    assert_eq!(acps[0].clip_pts_90k, trunc_120k);
    // title PTS = clip-local 119_808 − IN 60_000 = 59_808.
    assert_eq!(acps[0].title_pts_90k, u64::from(trunc_120k) - 60_000);
    assert_eq!(acps[0].spn, 20);
}

#[test]
fn no_acp_rows_yields_empty_list() {
    // Single PlayItem, every EP_map row plain (`is_angle_change_point =
    // false`). `angle_change_points()` must be empty, `next_…` must be
    // `None` — neither a panic nor a fallback.
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
            uo_mask: 0,
        },
        play_list: PlayList {
            play_items: vec![PlayItem {
                clip_information_file_name: "00001".into(),
                clip_codec_identifier: *b"M2TS",
                connection_condition: ConnectionCondition::NonSeamless,
                stc_id_ref: 0,
                in_time_ticks: 0,
                out_time_ticks: 45_000 * 10,
                multi_clip_count: 1,
                angles: Vec::new(),
                stn_table: StnTable {
                    primary_video: primary_video(),
                    primary_audio: primary_audio(),
                    ..StnTable::default()
                },
                flags: PlayItemFlags::default(),
            }],
            sub_paths: vec![],
        },
        marks: vec![],
    };
    write_file(&bdmv.join("PLAYLIST/00000.mpls"), &pl.encode());

    let rows = [
        (0u32, 0u32, false),
        (45_000, 10, false),
        (90_000, 20, false),
    ];
    write_file(&bdmv.join("STREAM/00001.m2ts"), &make_m2ts(64));
    write_file(
        &bdmv.join("CLIPINF/00001.clpi"),
        &make_clpi_with_angle_flags(&rows, 64, 0x1011),
    );
    write_file(&bdmv.join("index.bdmv"), &index_bdmv_one_hdmv().encode());

    let disc = Disc::mount(root).expect("mount");
    let title = disc.longest_title().expect("title").clone();
    let src = disc.open_title(&title, None).expect("open title");
    assert!(src.angle_change_points().is_empty());
    assert!(src.next_angle_change_point(0).is_none());
    assert!(disc.title_angle_change_points(&title).is_empty());
}

/// Tempdir helper (mirrors the seek_to_keyframe.rs pattern).
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
    let path = std::env::temp_dir().join(format!("oxideav-bluray-acps-{pid}-{nonce}-{serial}"));
    fs::create_dir_all(&path).unwrap();
    TestDir { path }
}
