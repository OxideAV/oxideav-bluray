//! Integration test for `TitleSource::seek_to` — keyframe-aligned PTS
//! seeking on top of the CPI EP_map (BD-ROM AV §5.7).
//!
//! Synthesizes a 2-PlayItem BDMV tree where each clip ships a `.clpi`
//! with a hand-built EP_map, mounts it, and asserts `seek_to(pts_90k)`
//! lands the reader on the correct source-packet (and therefore output
//! byte) for the I-frame at or before the requested presentation time.
//!
//! Everything here is fabricated by the test; no real disc data.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use oxideav_bluray::bdmv::clpi::{
    ClipInfo, ClipInformation, ClipMark, Cpi, EpEntry, EpMap, ProgramInfo, SequenceInfo,
    TsTypeInfoBlock,
};
use oxideav_bluray::bdmv::index_bdmv::{AppInfoBdmv, IndexBdmv, IndexEntry, IndexObjectType};
use oxideav_bluray::bdmv::mpls::{
    AppInfoPlayList, ConnectionCondition, PlayItem, PlayList, PlayListMark, PlayListMpls,
    PrimaryAudioStream, PrimaryVideoStream, StnTable, StreamCodingType,
};
use oxideav_bluray::{Disc, M2TS_PACKET_LEN, TS_PACKET_LEN};

fn write_file(path: &Path, bytes: &[u8]) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    let mut f = fs::File::create(path).unwrap();
    f.write_all(bytes).unwrap();
}

/// `.m2ts` with `n_packets` 192-byte BDAV source packets. The TS body
/// starts `[0x47, low8(global_pkt_idx), ...]` so the test can read the
/// fingerprint back out at any output offset and confirm which source
/// packet it landed on. `base` offsets the fingerprint per clip so the
/// two clips are distinguishable.
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

/// A `.clpi` carrying one primary-video EP_map (PID 0x1011) whose rows
/// are the supplied `(pts_ep_start, spn_ep_start)` pairs.
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

/// Read the next TS packet's 2-byte fingerprint (the source-packet tag)
/// from the current reader position, which must be on a 188-byte
/// TS-packet boundary. Asserts the TS sync byte is present.
fn next_fingerprint(src: &mut impl Read) -> u32 {
    let mut hdr = [0u8; 3];
    src.read_exact(&mut hdr).unwrap();
    assert_eq!(hdr[0], 0x47, "expected TS sync byte at seek landing");
    (u32::from(hdr[1]) << 8) | u32::from(hdr[2])
}

#[test]
fn seek_to_lands_on_keyframe_packet() {
    let tmp = tempdir_for_test();
    let root = tmp.path();
    let bdmv = root.join("BDMV");

    // Two PlayItems, each 10 s @ 45 kHz IN/OUT, IN-point 0.
    let in_a = 0u32;
    let out_a = 45_000 * 10;
    let in_b = 0u32;
    let out_b = 45_000 * 10;

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
                    in_time_ticks: in_a,
                    out_time_ticks: out_a,
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
                },
                PlayItem {
                    clip_information_file_name: "00002".into(),
                    clip_codec_identifier: *b"M2TS",
                    connection_condition: ConnectionCondition::SeamlessContinuation,
                    stc_id_ref: 0,
                    in_time_ticks: in_b,
                    out_time_ticks: out_b,
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
                },
            ],
            sub_paths: vec![],
        },
        marks: vec![],
    };
    write_file(&bdmv.join("PLAYLIST/00000.mpls"), &pl.encode());

    // Clip A: 64 packets. EP_map entries at clip-local PTS (90 kHz):
    //   (0,    spn 0)
    //   (90000, spn 16)   ~1 s
    //   (180000, spn 40)  ~2 s
    let n_a = 64u32;
    let eps_a = [(0u32, 0u32), (90_000, 16), (180_000, 40)];
    write_file(&bdmv.join("STREAM/00001.m2ts"), &make_m2ts(n_a as usize, 0));
    write_file(&bdmv.join("CLIPINF/00001.clpi"), &make_clpi(&eps_a, n_a));

    // Clip B: 64 packets, fingerprint base 0x1000 so it can't be
    // confused with clip A. EP entries at (0, spn 0) and (90000, spn 24).
    let n_b = 64u32;
    let eps_b = [(0u32, 0u32), (90_000, 24)];
    write_file(
        &bdmv.join("STREAM/00002.m2ts"),
        &make_m2ts(n_b as usize, 0x1000),
    );
    write_file(&bdmv.join("CLIPINF/00002.clpi"), &make_clpi(&eps_b, n_b));

    // index.bdmv → one HDMV title → PLAYLIST 0.
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

    let disc = Disc::mount(root).expect("mount synthetic disc");
    let title = disc.longest_title().expect("title").clone();
    let mut src = disc.open_title(&title, None).expect("open title");

    let clip_a_out = n_a as u64 * TS_PACKET_LEN as u64; // clip B output start

    // --- Seek inside clip A, target between the 1 s and 2 s entries.
    // clip-local PTS target 150_000 → largest entry ≤ it is (90000, 16).
    let pos = src.seek_to(150_000).unwrap();
    assert_eq!(
        pos,
        16 * TS_PACKET_LEN as u64,
        "seek to 1.67s lands on EP spn=16"
    );
    let tag = next_fingerprint(&mut src);
    assert_eq!(tag, 16, "fingerprint confirms source packet 16 of clip A");

    // --- Seek exactly onto the 2 s entry (180_000) → spn 40.
    let pos = src.seek_to(180_000).unwrap();
    assert_eq!(pos, 40 * TS_PACKET_LEN as u64);
    let tag = next_fingerprint(&mut src);
    assert_eq!(tag, 40, "exact-match entry lands on spn=40");

    // --- Seek before the first entry → spn 0 of clip A.
    let pos = src.seek_to(0).unwrap();
    assert_eq!(pos, 0);
    let tag = next_fingerprint(&mut src);
    assert_eq!(tag, 0);

    // --- Seek into clip B. Clip A duration is 10 s = 900_000 ticks.
    // Title PTS 990_000 → clip B, clip-local 90_000 → entry (90000, 24).
    let pos = src.seek_to(990_000).unwrap();
    assert_eq!(
        pos,
        clip_a_out + 24 * TS_PACKET_LEN as u64,
        "title-PTS into clip B maps through that clip's EP_map"
    );
    let tag = next_fingerprint(&mut src);
    assert_eq!(tag, 0x1000 + 24, "lands on source packet 24 of clip B");

    // --- Seek past the title end stays in the last clip and lands on
    // its last entry-point (the keyframe at-or-before the huge target).
    let pos = src.seek_to(u64::MAX).unwrap();
    assert_eq!(
        pos,
        clip_a_out + 24 * TS_PACKET_LEN as u64,
        "past-end seek lands on the last EP of the final clip"
    );
    let tag = next_fingerprint(&mut src);
    assert_eq!(tag, 0x1000 + 24, "lands on source packet 24 of clip B");
}

#[test]
fn seek_to_without_cpi_falls_back_to_clip_start() {
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
            }],
            sub_paths: vec![],
        },
        marks: vec![],
    };
    write_file(&bdmv.join("PLAYLIST/00000.mpls"), &pl.encode());

    // .m2ts present, but NO .clpi → no EP_map → seek falls back to start.
    write_file(&bdmv.join("STREAM/00001.m2ts"), &make_m2ts(32, 0));

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

    let disc = Disc::mount(root).expect("mount");
    let title = disc.longest_title().expect("title").clone();
    let mut src = disc.open_title(&title, None).expect("open");

    // Any seek_to without a CPI EP_map lands at the clip start (0).
    let pos = src.seek_to(450_000).unwrap();
    assert_eq!(pos, 0, "no EP_map → clip start");
    assert_eq!(next_fingerprint(&mut src), 0, "packet 0 fingerprint");

    // The byte-exact `Seek` contract still works alongside seek_to.
    let p = src.seek(SeekFrom::Start(5 * TS_PACKET_LEN as u64)).unwrap();
    assert_eq!(p, 5 * TS_PACKET_LEN as u64);
    assert_eq!(
        next_fingerprint(&mut src),
        5,
        "byte-exact seek landed on packet 5"
    );
}

#[test]
fn disc_chapters_map_to_seekable_keyframes() {
    let tmp = tempdir_for_test();
    let root = tmp.path();
    let bdmv = root.join("BDMV");

    // Two PlayItems each spanning 10 s @ 45 kHz. Item 0 IN = 0; item 1
    // IN = 5 s (clip-local), so a mark at clip-local 7 s in item 1 sits
    // 2 s into the PlayItem → title 10 s + 2 s = 12 s.
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
                },
            ],
            sub_paths: vec![],
        },
        marks: vec![
            // Chapter 1: item 0 IN → title 0.
            PlayListMark {
                mark_type: 1,
                ref_play_item_id: 0,
                mark_time_ticks: 0,
                entry_es_pid: 0x1011,
                duration_ticks: 0,
            },
            // Link point (excluded): clip-local 3 s in item 0.
            PlayListMark {
                mark_type: 2,
                ref_play_item_id: 0,
                mark_time_ticks: 45_000 * 3,
                entry_es_pid: 0x1011,
                duration_ticks: 0,
            },
            // Chapter 2: clip-local 7 s in item 1 (IN 5 s) → title 12 s.
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

    // Clip A: 64 packets, EP at title-0 spn 0.
    let n_a = 64u32;
    let eps_a = [(0u32, 0u32), (90_000, 16)];
    write_file(&bdmv.join("STREAM/00001.m2ts"), &make_m2ts(n_a as usize, 0));
    write_file(&bdmv.join("CLIPINF/00001.clpi"), &make_clpi(&eps_a, n_a));

    // Clip B: 64 packets, fingerprint base 0x1000. IN = 5 s = 450_000
    // clip-local ticks; the 12 s chapter is clip-local 7 s = 630_000.
    // EP entry at (630_000, spn 32) is the keyframe at-or-before it.
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

    let disc = Disc::mount(root).expect("mount synthetic disc");
    let title = disc.longest_title().expect("title").clone();

    // Two entry marks → two chapters; the link point is excluded.
    let chapters = disc.chapters(&title);
    assert_eq!(chapters.len(), 2, "link point excluded from chapters");
    assert_eq!(chapters[0].index, 0);
    assert_eq!(chapters[0].start_pts_90k, 0);
    assert_eq!(chapters[1].index, 1);
    assert_eq!(chapters[1].ref_play_item_id, 1);
    assert_eq!(chapters[1].start_pts_90k, 12 * 90_000, "title 12 s");

    // Each chapter's PTS is directly seekable to the right keyframe.
    let mut src = disc.open_title(&title, None).expect("open title");
    let clip_a_out = n_a as u64 * TS_PACKET_LEN as u64;

    let pos = src.seek_to(chapters[0].start_pts_90k).unwrap();
    assert_eq!(pos, 0, "chapter 1 lands at clip A start");
    assert_eq!(next_fingerprint(&mut src), 0);

    let pos = src.seek_to(chapters[1].start_pts_90k).unwrap();
    assert_eq!(
        pos,
        clip_a_out + 32 * TS_PACKET_LEN as u64,
        "chapter 2 lands on clip B keyframe spn=32"
    );
    assert_eq!(next_fingerprint(&mut src), 0x1000 + 32);
}

/// Tempdir helper (mirrors synthetic_disc.rs).
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
    let path = std::env::temp_dir().join(format!("oxideav-bluray-seek-{pid}-{nonce}-{serial}"));
    fs::create_dir_all(&path).unwrap();
    TestDir { path }
}
