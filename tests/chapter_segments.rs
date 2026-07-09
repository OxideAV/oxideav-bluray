//! Integration tests for `Disc::open_title_chapters` — the per-chapter
//! byte-segment iterator the CLI wraps a remuxer around.
//!
//! Synthesizes a 3-chapter BDMV tree (two PlayItems with three entry
//! marks across them), mounts it, and asserts each chapter selector
//! variant (`All`, `Range`, `List`) yields the expected sequence of
//! [`ChapterSegment`]s with the right ids, PTS bounds, and byte-range
//! sizes. The byte ranges are *keyframe-rounded* via the seeker, so
//! the assertions look up the EP_map entry at-or-before each chapter
//! mark and bound the expected size by that.
//!
//! Everything here is fabricated by the test; no real disc data.

use oxideav_bluray::bdmv::clpi::{
    ClipInfo, ClipInformation, ClipMark, Cpi, EpEntry, EpMap, ProgramInfo, SequenceInfo,
    TsTypeInfoBlock,
};
use oxideav_bluray::bdmv::index_bdmv::{AppInfoBdmv, IndexBdmv, IndexEntry, IndexObjectType};
use oxideav_bluray::bdmv::mpls::{
    AppInfoPlayList, ConnectionCondition, PlayItem, PlayItemFlags, PlayList, PlayListMark,
    PlayListMpls, PrimaryAudioStream, PrimaryVideoStream, StnTable, StreamCodingType,
};
use oxideav_bluray::{ChapterSelector, Disc, M2TS_PACKET_LEN, TS_PACKET_LEN};
use std::fs;
use std::io::Write;
use std::path::Path;

fn write_file(path: &Path, bytes: &[u8]) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    let mut f = fs::File::create(path).unwrap();
    f.write_all(bytes).unwrap();
}

/// `.m2ts` with `n_packets` 192-byte BDAV source packets. Mirrors the
/// `seek_to_keyframe.rs` helper so the two test suites stay in sync.
fn make_m2ts(n_packets: usize, base: u32) -> Vec<u8> {
    let mut out = vec![0u8; n_packets * M2TS_PACKET_LEN];
    for i in 0..n_packets {
        let pkt_off = i * M2TS_PACKET_LEN;
        out[pkt_off..pkt_off + 4].copy_from_slice(&(i as u32).to_be_bytes());
        let ts_off = pkt_off + 4;
        out[ts_off] = 0x47;
        let tag = base.wrapping_add(i as u32);
        out[ts_off + 1] = (tag >> 8) as u8;
        out[ts_off + 2] = (tag & 0xFF) as u8;
        for j in 3..TS_PACKET_LEN {
            out[ts_off + j] = 0xAA;
        }
    }
    out
}

fn make_clpi(entry_points: &[(u32, u32)], n_source_packets: u32) -> Vec<u8> {
    let entries: Vec<EpEntry> = entry_points
        .iter()
        .map(|&(pts, spn)| EpEntry {
            is_angle_change_point: false,
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
                stream_pid: 0x1011,
                ep_stream_type: 1,
                entries,
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

/// Synthesize a BDMV tree with 2 PlayItems, 3 chapters, and an EP_map
/// per clip that lines up exactly with the chapter marks.
///
/// Layout (title PTS @ 90 kHz):
///   chapter 1 → title 0       (clip A spn 0)
///   chapter 2 → title 5  s    (clip A spn 32) — at the 5 s EP entry
///   chapter 3 → title 12 s    (clip B spn 32) — 7 s into clip B (IN 5 s)
///
/// Clip durations: A = 10 s, B = 10 s; title duration = 20 s.
fn synth_three_chapter_disc(root: &Path) {
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
                    clip_information_file_name: "00001".into(),
                    clip_codec_identifier: *b"M2TS",
                    connection_condition: ConnectionCondition::NonSeamless,
                    stc_id_ref: 0,
                    in_time_ticks: 0,
                    out_time_ticks: 45_000 * 10,
                    multi_clip_count: 1,
                    angles: Vec::new(),
                    stn_table: primary_video_stn(),
                    flags: PlayItemFlags::default(),
                },
                PlayItem {
                    clip_information_file_name: "00002".into(),
                    clip_codec_identifier: *b"M2TS",
                    connection_condition: ConnectionCondition::SeamlessContinuation,
                    stc_id_ref: 0,
                    in_time_ticks: 45_000 * 5,
                    out_time_ticks: 45_000 * 15,
                    multi_clip_count: 1,
                    angles: Vec::new(),
                    stn_table: primary_video_stn(),
                    flags: PlayItemFlags::default(),
                },
            ],
            sub_paths: vec![],
        },
        marks: vec![
            // Chapter 1 — title 0.
            PlayListMark {
                mark_type: 1,
                ref_play_item_id: 0,
                mark_time_ticks: 0,
                entry_es_pid: 0x1011,
                duration_ticks: 0,
            },
            // Chapter 2 — clip-local 5 s in item 0 → title 5 s.
            PlayListMark {
                mark_type: 1,
                ref_play_item_id: 0,
                mark_time_ticks: 45_000 * 5,
                entry_es_pid: 0x1011,
                duration_ticks: 0,
            },
            // Chapter 3 — clip-local 7 s in item 1 (IN 5 s) → title 12 s.
            PlayListMark {
                mark_type: 1,
                ref_play_item_id: 1,
                mark_time_ticks: 45_000 * 7,
                entry_es_pid: 0x1011,
                duration_ticks: 0,
            },
        ],
    };
    write_file(&bdmv.join("PLAYLIST/00000.mpls"), &pl.encode());

    // Clip A: 64 packets. EP entries at PTS 0 (spn 0) and PTS 5 s
    // (spn 32). Clip A is exactly 10 s on-PTS.
    let n_a = 64u32;
    let eps_a = [(0u32, 0u32), (90_000 * 5, 32)];
    write_file(&bdmv.join("STREAM/00001.m2ts"), &make_m2ts(n_a as usize, 0));
    write_file(&bdmv.join("CLIPINF/00001.clpi"), &make_clpi(&eps_a, n_a));

    // Clip B: 64 packets, fingerprint base 0x1000. IN = 5 s clip-local
    // = 450_000; chapter 3 at clip-local 7 s = 630_000. EP entries:
    //   (450_000, spn 0)
    //   (630_000, spn 32)
    let n_b = 64u32;
    let eps_b = [(450_000u32, 0u32), (630_000, 32)];
    write_file(
        &bdmv.join("STREAM/00002.m2ts"),
        &make_m2ts(n_b as usize, 0x1000),
    );
    write_file(&bdmv.join("CLIPINF/00002.clpi"), &make_clpi(&eps_b, n_b));

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

// Per-chapter expected byte-range — derived from the EP_map entries
// above. `chap` 1 starts at clip A spn 0; chap 2 at clip A spn 32; chap
// 3 at clip B spn 32. Clip A contributes 64 * 188 output bytes total.
const PACKET_OUT: u64 = TS_PACKET_LEN as u64;
const CLIP_A_OUT: u64 = 64 * PACKET_OUT;
const CHAP_1_START_BYTE: u64 = 0;
const CHAP_2_START_BYTE: u64 = 32 * PACKET_OUT;
const CHAP_3_START_BYTE: u64 = CLIP_A_OUT + 32 * PACKET_OUT;
const TITLE_TOTAL_OUT: u64 = 128 * PACKET_OUT;

#[test]
fn open_title_chapters_all_yields_three_segments_in_order() {
    let tmp = tempdir_for_test();
    synth_three_chapter_disc(tmp.path());
    let disc = Disc::mount(tmp.path()).expect("mount");
    let title = disc.longest_title().expect("title").clone();

    let segs: Vec<_> = disc
        .open_title_chapters(&title, &ChapterSelector::All, None)
        .expect("open chapters")
        .collect::<Result<_, _>>()
        .expect("read segments");

    assert_eq!(segs.len(), 3, "All → one segment per chapter");
    assert_eq!(segs[0].chapter_id, 1);
    assert_eq!(segs[1].chapter_id, 2);
    assert_eq!(segs[2].chapter_id, 3);

    assert_eq!(segs[0].start_pts_90k, 0);
    assert_eq!(segs[1].start_pts_90k, 5 * 90_000);
    assert_eq!(segs[2].start_pts_90k, 12 * 90_000);

    // Chapter byte ranges land on the EP_map boundaries.
    let chap_1_len = (CHAP_2_START_BYTE - CHAP_1_START_BYTE) as usize;
    let chap_2_len = (CHAP_3_START_BYTE - CHAP_2_START_BYTE) as usize;
    let chap_3_len = (TITLE_TOTAL_OUT - CHAP_3_START_BYTE) as usize;
    assert_eq!(segs[0].bytes.len(), chap_1_len);
    assert_eq!(segs[1].bytes.len(), chap_2_len);
    assert_eq!(segs[2].bytes.len(), chap_3_len);

    // The first byte of each chapter is the TS sync byte at the
    // keyframe boundary. Fingerprint tags below confirm we landed on
    // the *right* keyframe rather than just any 0x47 byte.
    assert_eq!(segs[0].bytes[0], 0x47, "chapter 1 starts on TS sync");
    let chap_1_tag = (u32::from(segs[0].bytes[1]) << 8) | u32::from(segs[0].bytes[2]);
    assert_eq!(chap_1_tag, 0, "clip A packet 0 fingerprint");

    assert_eq!(segs[1].bytes[0], 0x47, "chapter 2 starts on TS sync");
    let chap_2_tag = (u32::from(segs[1].bytes[1]) << 8) | u32::from(segs[1].bytes[2]);
    assert_eq!(chap_2_tag, 32, "clip A packet 32 fingerprint");

    assert_eq!(segs[2].bytes[0], 0x47, "chapter 3 starts on TS sync");
    let chap_3_tag = (u32::from(segs[2].bytes[1]) << 8) | u32::from(segs[2].bytes[2]);
    assert_eq!(chap_3_tag, 0x1000 + 32, "clip B packet 32 fingerprint");
}

#[test]
fn open_title_chapters_range_single_yields_one_segment() {
    // Task-spec test: Range { start: 2, end: Some(2) } → exactly one
    // segment, chapter_id 2, starting at the title-5 s mark.
    let tmp = tempdir_for_test();
    synth_three_chapter_disc(tmp.path());
    let disc = Disc::mount(tmp.path()).expect("mount");
    let title = disc.longest_title().expect("title").clone();

    let segs: Vec<_> = disc
        .open_title_chapters(
            &title,
            &ChapterSelector::Range {
                start: 2,
                end: Some(2),
            },
            None,
        )
        .expect("open chapter 2")
        .collect::<Result<_, _>>()
        .expect("read");

    assert_eq!(segs.len(), 1, "single-chapter range → one segment");
    assert_eq!(segs[0].chapter_id, 2);
    assert_eq!(segs[0].start_pts_90k, 5 * 90_000);
    assert_eq!(segs[0].end_pts_90k, 12 * 90_000);
    assert_eq!(
        segs[0].bytes.len(),
        (CHAP_3_START_BYTE - CHAP_2_START_BYTE) as usize
    );
    assert_eq!(segs[0].bytes[0], 0x47);
    let tag = (u32::from(segs[0].bytes[1]) << 8) | u32::from(segs[0].bytes[2]);
    assert_eq!(tag, 32, "clip A packet 32 fingerprint");
}

#[test]
fn open_title_chapters_range_open_ended_runs_to_title_end() {
    // Range { start: 2, end: None } → chapters 2..=3, ending at EOF.
    let tmp = tempdir_for_test();
    synth_three_chapter_disc(tmp.path());
    let disc = Disc::mount(tmp.path()).expect("mount");
    let title = disc.longest_title().expect("title").clone();

    let segs: Vec<_> = disc
        .open_title_chapters(
            &title,
            &ChapterSelector::Range {
                start: 2,
                end: None,
            },
            None,
        )
        .expect("open chapters 2-")
        .collect::<Result<_, _>>()
        .expect("read");

    assert_eq!(segs.len(), 2);
    assert_eq!(segs[0].chapter_id, 2);
    assert_eq!(segs[1].chapter_id, 3);

    // Last chapter must run to EOF — not stop at seek_to(title_duration).
    let chap_3_len = (TITLE_TOTAL_OUT - CHAP_3_START_BYTE) as usize;
    assert_eq!(
        segs[1].bytes.len(),
        chap_3_len,
        "last chapter reads to title EOF, not to the last keyframe"
    );
}

#[test]
fn open_title_chapters_list_preserves_uri_order() {
    // `?chapters=3,1` → emit chapter 3 *then* chapter 1. The CLI
    // produces filenames in URI order so the chapter selector iterator
    // must echo it back.
    let tmp = tempdir_for_test();
    synth_three_chapter_disc(tmp.path());
    let disc = Disc::mount(tmp.path()).expect("mount");
    let title = disc.longest_title().expect("title").clone();

    let segs: Vec<_> = disc
        .open_title_chapters(&title, &ChapterSelector::List(vec![3, 1]), None)
        .expect("open list")
        .collect::<Result<_, _>>()
        .expect("read");

    assert_eq!(segs.len(), 2);
    assert_eq!(segs[0].chapter_id, 3);
    assert_eq!(segs[1].chapter_id, 1);
}

#[test]
fn open_title_chapters_rejects_out_of_range_id() {
    // A list with id 99 (only 3 chapters exist) must surface as
    // Malformed at open time, not silently truncate or hand back an
    // empty iterator.
    let tmp = tempdir_for_test();
    synth_three_chapter_disc(tmp.path());
    let disc = Disc::mount(tmp.path()).expect("mount");
    let title = disc.longest_title().expect("title").clone();

    assert!(disc
        .open_title_chapters(&title, &ChapterSelector::List(vec![99]), None)
        .is_err());
    assert!(disc
        .open_title_chapters(
            &title,
            &ChapterSelector::Range {
                start: 1,
                end: Some(99),
            },
            None,
        )
        .is_err());
}

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
    let path = std::env::temp_dir().join(format!("oxideav-bluray-chap-{pid}-{nonce}-{serial}"));
    fs::create_dir_all(&path).unwrap();
    TestDir { path }
}
