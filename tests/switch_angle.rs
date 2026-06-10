//! Integration test for in-place mid-stream angle switching.
//!
//! Synthesises a 2-PlayItem × 3-angle BDMV tree where every angle's
//! `.m2ts` carries a distinct fingerprint byte at the TS payload head,
//! and every angle's `.clpi` ships the same EP_map with one
//! `is_angle_change_point = 1` row at SPN 8 of PlayItem 0.
//!
//! Asserts:
//!
//! 1. [`TitleSource::current_angle`] / [`TitleSource::num_angles`]
//!    report the open angle and the smallest-clip-count title-wide
//!    angle count.
//! 2. [`TitleSource::switch_angle_at`] retargets the underlying reader
//!    to the requested angle's clip set at the given title PTS — the
//!    next read returns the destination angle's fingerprint byte and
//!    the absolute output position advances onto the EP_map entry.
//! 3. Out-of-range angle is rejected with [`io::ErrorKind::InvalidInput`]
//!    and the source stays on the previous angle (next read still
//!    yields the previous angle's fingerprint).
//! 4. [`TitleSource::switch_angle`] picks the first angle-change
//!    boundary at or after the current output position.
//! 5. A title with no flagged EP_map rows surfaces
//!    [`io::ErrorKind::NotFound`] from `switch_angle` and the source
//!    stays on the previous angle.
//!
//! Everything here is fabricated by the test; no real disc data.

use std::fs;
use std::io::{self, Read, Write};
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

/// Fingerprinted M2TS: each packet's TS payload starts `0x47 tag tag tag`
/// so the test can identify the source clip by reading the first 4 TS
/// bytes after a seek. Body filled with `0xAA`.
fn make_m2ts(n_packets: usize, tag: u8) -> Vec<u8> {
    let mut out = vec![0u8; n_packets * M2TS_PACKET_LEN];
    for i in 0..n_packets {
        let ts_off = i * M2TS_PACKET_LEN + 4;
        out[ts_off] = 0x47;
        out[ts_off + 1] = tag;
        out[ts_off + 2] = tag;
        out[ts_off + 3] = tag;
        for j in 4..TS_PACKET_LEN {
            out[ts_off + j] = 0xAA;
        }
    }
    out
}

/// Build a `.clpi` whose primary-video EP_map carries the supplied
/// `(pts_ep_start, spn_ep_start, is_angle_change_point)` rows.
fn make_clpi(rows: &[(u32, u32, bool)], n_source_packets: u32, stream_pid: u16) -> Vec<u8> {
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

/// 2-PlayItem × 3-angle synthetic disc.
///
/// PlayItem 0: primary clip 00100 (+ alts 00101, 00102), IN=0,
///   OUT=45_000*10 (10 s). EP_map:
///     (0,       0,  is_acp=false)
///     (45_000,  8,  is_acp=true )  → mid-clip switch boundary
///
/// PlayItem 1: primary clip 00200 (+ alts 00201, 00202), IN=0,
///   OUT=45_000*10 (10 s). EP_map:
///     (0,       0,  is_acp=false)
///     (45_000,  8,  is_acp=true )
///
/// Fingerprints (the first TS byte after the sync 0x47 in every packet):
///
///   00100 → 0xA0    00101 → 0xA1    00102 → 0xA2   (PlayItem 0)
///   00200 → 0xB0    00201 → 0xB1    00202 → 0xB2   (PlayItem 1)
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

    // PlayItem 0 (64 packets each, fingerprint per angle).
    let pi0_eps = [(0u32, 0u32, false), (45_000, 8, true)];
    for (stem, tag) in [("00100", 0xA0), ("00101", 0xA1), ("00102", 0xA2)] {
        write_file(
            &bdmv.join(format!("STREAM/{stem}.m2ts")),
            &make_m2ts(64, tag),
        );
        write_file(
            &bdmv.join(format!("CLIPINF/{stem}.clpi")),
            &make_clpi(&pi0_eps, 64, 0x1011),
        );
    }

    // PlayItem 1 (64 packets each, distinct fingerprint per angle).
    let pi1_eps = [(0u32, 0u32, false), (45_000, 8, true)];
    for (stem, tag) in [("00200", 0xB0), ("00201", 0xB1), ("00202", 0xB2)] {
        write_file(
            &bdmv.join(format!("STREAM/{stem}.m2ts")),
            &make_m2ts(64, tag),
        );
        write_file(
            &bdmv.join(format!("CLIPINF/{stem}.clpi")),
            &make_clpi(&pi1_eps, 64, 0x1011),
        );
    }

    write_file(&bdmv.join("index.bdmv"), &index_bdmv_one_hdmv().encode());
}

/// Read 4 bytes from `src` and return the fingerprint byte
/// (`bytes[1]` — bytes 1..=3 of every packet are the tag).
fn fingerprint(src: &mut impl Read) -> u8 {
    let mut hdr = [0u8; 4];
    src.read_exact(&mut hdr).unwrap();
    assert_eq!(hdr[0], 0x47, "expected TS sync byte");
    assert_eq!(hdr[1], hdr[2]);
    assert_eq!(hdr[2], hdr[3]);
    hdr[1]
}

#[test]
fn current_angle_and_num_angles_report_open_state() {
    let tmp = tempdir_for_test();
    build_disc(tmp.path());

    let disc = Disc::mount(tmp.path()).expect("mount");
    let title = disc.longest_title().expect("title").clone();

    let src0 = disc
        .open_title_with_angle(&title, 0, None)
        .expect("open angle 0");
    assert_eq!(src0.current_angle(), 0);
    assert_eq!(src0.num_angles(), 3, "PI0 and PI1 both advertise 3 angles");

    let src2 = disc
        .open_title_with_angle(&title, 2, None)
        .expect("open angle 2");
    assert_eq!(src2.current_angle(), 2);
    assert_eq!(src2.num_angles(), 3);
}

#[test]
fn switch_angle_at_retargets_clip_reader() {
    let tmp = tempdir_for_test();
    build_disc(tmp.path());

    let disc = Disc::mount(tmp.path()).expect("mount");
    let title = disc.longest_title().expect("title").clone();
    let mut src = disc
        .open_title_with_angle(&title, 0, None)
        .expect("open angle 0");

    // Sanity: angle 0 streams the primary clip 00100 (fingerprint 0xA0).
    assert_eq!(fingerprint(&mut src), 0xA0);

    // Switch to angle 1 at title PTS 0 → should land on 00101's start.
    let new_pos = src.switch_angle_at(1, 0).expect("switch to angle 1");
    assert_eq!(new_pos, 0, "PTS 0 lands on the first EP_map row");
    assert_eq!(src.current_angle(), 1);
    assert_eq!(fingerprint(&mut src), 0xA1);

    // Switch to angle 2 at the ACP boundary (clip-local PTS 45_000;
    // CPI fine row stores top 23 bits so the round-tripped value is
    // truncated to (45_000 >> 9) << 9 = 44_544). Title PTS == clip PTS
    // because IN=0.
    let trunc_45k: u64 = ((45_000u32 >> 9) << 9) as u64;
    let new_pos = src
        .switch_angle_at(2, trunc_45k)
        .expect("switch to angle 2");
    // SPN 8 on the new angle's clip → output position = 8 × 188 bytes.
    let expected = 8 * TS_PACKET_LEN as u64;
    assert_eq!(new_pos, expected, "ACP at SPN 8 lifts to 8 × 188 bytes");
    assert_eq!(src.current_angle(), 2);
    assert_eq!(fingerprint(&mut src), 0xA2);
}

#[test]
fn switch_angle_at_into_second_playitem() {
    let tmp = tempdir_for_test();
    build_disc(tmp.path());

    let disc = Disc::mount(tmp.path()).expect("mount");
    let title = disc.longest_title().expect("title").clone();
    let mut src = disc
        .open_title_with_angle(&title, 0, None)
        .expect("open angle 0");

    // PI0 duration is 10 s = 900_000 ticks; PI1's start sits at title
    // PTS 900_000. Switch to angle 2 right at the seam — the seek
    // should land on PI1's first EP_map row (PTS 0 on PI1's clip, SPN 0).
    let new_pos = src.switch_angle_at(2, 900_000).expect("switch at seam");
    assert_eq!(src.current_angle(), 2);
    // PI1 (angle 2 → clip 00202) starts at 64 × 188 bytes (PI0 angle 2
    // contributed 64 packets to the new total). seek_to landing on
    // SPN 0 puts the cursor at PI1's start = 64 × 188.
    let expected = 64 * TS_PACKET_LEN as u64;
    assert_eq!(new_pos, expected);
    assert_eq!(fingerprint(&mut src), 0xB2, "PI1 angle 2 fingerprint");
}

#[test]
fn out_of_range_angle_is_rejected_and_source_unchanged() {
    let tmp = tempdir_for_test();
    build_disc(tmp.path());

    let disc = Disc::mount(tmp.path()).expect("mount");
    let title = disc.longest_title().expect("title").clone();
    let mut src = disc
        .open_title_with_angle(&title, 0, None)
        .expect("open angle 0");

    // Angle 7 doesn't exist — should fail with InvalidInput and leave
    // the source on angle 0.
    let err = src
        .switch_angle_at(7, 0)
        .expect_err("angle 7 must be rejected");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(src.current_angle(), 0, "rejection preserves current angle");

    // Confirm the source still streams angle 0's clip.
    assert_eq!(fingerprint(&mut src), 0xA0);
}

#[test]
fn switch_angle_uses_next_boundary_at_or_after_position() {
    let tmp = tempdir_for_test();
    build_disc(tmp.path());

    let disc = Disc::mount(tmp.path()).expect("mount");
    let title = disc.longest_title().expect("title").clone();
    let mut src = disc
        .open_title_with_angle(&title, 0, None)
        .expect("open angle 0");

    // The first ACP boundary sits at PI0 SPN 8 = output byte 8 × 188.
    // At the very start of the title, `switch_angle(1)` picks that
    // boundary and lands on 00101 at output byte 8 × 188.
    let new_pos = src.switch_angle(1).expect("first boundary");
    let expected = 8 * TS_PACKET_LEN as u64;
    assert_eq!(new_pos, expected);
    assert_eq!(src.current_angle(), 1);
    assert_eq!(fingerprint(&mut src), 0xA1);
}

#[test]
fn switch_angle_yields_not_found_when_no_boundary_remains() {
    let tmp = tempdir_for_test();
    let root = tmp.path();
    let bdmv = root.join("BDMV");

    // Single-PlayItem single-angle disc with three plain (non-ACP)
    // EP rows. `switch_angle` has no boundary to land on so it MUST
    // surface NotFound and leave the source untouched.
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
                in_time_ticks: 0,
                out_time_ticks: 45_000 * 5,
                multi_clip_count: 1,
                angles: vec![],
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
    write_file(&bdmv.join("STREAM/00001.m2ts"), &make_m2ts(64, 0xA0));
    write_file(
        &bdmv.join("CLIPINF/00001.clpi"),
        &make_clpi(&rows, 64, 0x1011),
    );
    write_file(&bdmv.join("index.bdmv"), &index_bdmv_one_hdmv().encode());

    let disc = Disc::mount(root).expect("mount");
    let title = disc.longest_title().expect("title").clone();
    let mut src = disc.open_title(&title, None).expect("open");

    // Single-angle title → `num_angles()` reports 1.
    assert_eq!(src.num_angles(), 1);

    // No flagged row → switch_angle has nowhere to land.
    let err = src.switch_angle(0).expect_err("no boundary");
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert_eq!(src.current_angle(), 0);
}

/// Tempdir helper (mirrors the angle_change_points.rs pattern).
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
    let path = std::env::temp_dir().join(format!("oxideav-bluray-switch-{pid}-{nonce}-{serial}"));
    fs::create_dir_all(&path).unwrap();
    TestDir { path }
}
