//! Integration test for `Disc::title_streams` / `TrackCatalogue` and
//! the `TitleInfo::languages` mount-time population (BD-ROM Part 3
//! §5.4.4.4 STN_table lift).
//!
//! Builds a synthetic 2-PlayItem BDMV tree whose STN_table carries one
//! primary video + two primary audios (English + Japanese) + one PG
//! subtitle (French) — repeated on both PlayItems — and verifies the
//! aggregation dedups by `(PID, kind)` while raising `playitem_count`
//! to 2 for every shared track.
//!
//! All inputs are entirely fabricated by this test; no real disc data
//! anywhere.

use std::fs;
use std::io::Write;
use std::path::Path;

use oxideav_bluray::bdmv::index_bdmv::{AppInfoBdmv, IndexBdmv, IndexEntry, IndexObjectType};
use oxideav_bluray::bdmv::mpls::{
    AppInfoPlayList, ConnectionCondition, PgsSubtitleStream, PlayItem, PlayItemFlags, PlayList,
    PlayListMpls, PrimaryAudioStream, PrimaryVideoStream, StnTable, StreamCodingType,
};
use oxideav_bluray::{Disc, TrackKind, M2TS_PACKET_LEN, TS_PACKET_LEN};

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
        let off = i * M2TS_PACKET_LEN + 4;
        out[off] = 0x47;
        out[off + 1] = (i & 0xFF) as u8;
    }
    out
}

fn shared_stn_table() -> StnTable {
    StnTable {
        primary_video: vec![PrimaryVideoStream {
            elementary_pid: 0x1011,
            coding_type: StreamCodingType::AvcVideo,
            video_format: 0x06,
            frame_rate: 0x03,
            aspect_ratio: 0x03,
        }],
        primary_audio: vec![
            PrimaryAudioStream {
                elementary_pid: 0x1100,
                coding_type: StreamCodingType::DtsHdMaAudio,
                audio_format: 0x06,
                sample_rate: 0x01,
                language_code: *b"eng",
            },
            PrimaryAudioStream {
                elementary_pid: 0x1101,
                coding_type: StreamCodingType::Ac3Audio,
                audio_format: 0x03,
                sample_rate: 0x01,
                language_code: *b"jpn",
            },
        ],
        pg_subtitles: vec![PgsSubtitleStream {
            elementary_pid: 0x1200,
            coding_type: StreamCodingType::PgsSubtitle,
            language_code: *b"FRA", // uppercase — the catalogue must lowercase it
        }],
        ..StnTable::default()
    }
}

fn synth_disc(root: &Path) {
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
                    out_time_ticks: 45_000 * 5,
                    multi_clip_count: 1,
                    angles: Vec::new(),
                    stn_table: shared_stn_table(),
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
                    stn_table: shared_stn_table(),
                    flags: PlayItemFlags::default(),
                },
            ],
            sub_paths: vec![],
        },
        marks: vec![],
    };
    write_file(&bdmv.join("PLAYLIST/00000.mpls"), &pl.encode());

    // Both clips need to exist so Disc::mount succeeds; contents don't
    // matter for this test's assertions.
    let bytes = make_m2ts(16);
    write_file(&bdmv.join("STREAM/00001.m2ts"), &bytes);
    write_file(&bdmv.join("STREAM/00002.m2ts"), &bytes);

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
fn title_streams_dedups_pids_across_playitems() {
    let tmp = tempdir_for_test();
    synth_disc(tmp.path());
    let disc = Disc::mount(tmp.path()).expect("mount synthetic disc");
    let title = &disc.titles()[0];

    let catalogue = disc.title_streams(title);

    // 1 primary video + 2 primary audios + 1 PG = 4 unique tracks.
    assert_eq!(catalogue.len(), 4, "{:?}", catalogue);

    // Canonical STN-class order.
    let kinds: Vec<TrackKind> = catalogue.tracks.iter().map(|t| t.kind).collect();
    assert_eq!(
        kinds,
        vec![
            TrackKind::PrimaryVideo,
            TrackKind::PrimaryAudio,
            TrackKind::PrimaryAudio,
            TrackKind::PgSubtitle,
        ]
    );

    // Every track is shared by both PlayItems — playitem_count must be 2.
    for t in &catalogue.tracks {
        assert_eq!(t.playitem_count, 2, "{:?}", t);
    }

    // Spot-check the per-track contents.
    let video = catalogue.by_pid(0x1011).expect("video PID");
    assert_eq!(video.coding_type, StreamCodingType::AvcVideo);
    assert!(video.language.is_none(), "video tracks carry no language");

    let eng = catalogue.by_pid(0x1100).expect("english audio PID");
    assert_eq!(eng.coding_type, StreamCodingType::DtsHdMaAudio);
    assert_eq!(eng.language.as_deref(), Some("eng"));

    let jpn = catalogue.by_pid(0x1101).expect("japanese audio PID");
    assert_eq!(jpn.kind, TrackKind::PrimaryAudio);
    assert_eq!(jpn.language.as_deref(), Some("jpn"));

    let fra = catalogue.by_pid(0x1200).expect("french PG PID");
    assert_eq!(fra.kind, TrackKind::PgSubtitle);
    // Disc author wrote `FRA` uppercase; catalogue must lowercase.
    assert_eq!(fra.language.as_deref(), Some("fra"));

    // The UI label combines the coding-type display name with the
    // language; video carries no language so no parenthetical.
    assert_eq!(video.label(), "H.264/AVC Video");
    assert_eq!(eng.label(), "DTS-HD Master Audio (eng)");
    assert_eq!(fra.label(), "PGS Subtitle (fra)");
}

#[test]
fn title_streams_by_kind_filter_walks_only_one_class() {
    let tmp = tempdir_for_test();
    synth_disc(tmp.path());
    let disc = Disc::mount(tmp.path()).expect("mount synthetic disc");
    let title = &disc.titles()[0];
    let catalogue = disc.title_streams(title);

    let audio: Vec<u16> = catalogue
        .by_kind(TrackKind::PrimaryAudio)
        .map(|t| t.pid)
        .collect();
    assert_eq!(audio, vec![0x1100, 0x1101]);

    let video: Vec<u16> = catalogue
        .by_kind(TrackKind::PrimaryVideo)
        .map(|t| t.pid)
        .collect();
    assert_eq!(video, vec![0x1011]);

    assert!(catalogue
        .by_kind(TrackKind::SecondaryVideo)
        .next()
        .is_none());
}

#[test]
fn title_info_languages_populated_at_mount_sorted_dedup_lowercase() {
    let tmp = tempdir_for_test();
    synth_disc(tmp.path());
    let disc = Disc::mount(tmp.path()).expect("mount synthetic disc");
    let langs = &disc.titles()[0].languages;
    // Set: eng (audio) + jpn (audio) + fra (PG). Sorted, lowercased,
    // deduplicated even though each language appears twice (one entry
    // per PlayItem).
    assert_eq!(langs, &vec!["eng".to_string(), "fra".into(), "jpn".into()]);
}

#[test]
fn title_streams_skips_zero_pid_subpath_entries() {
    // A stream_entry with stream_type != 1 has no in-mux PID; the
    // parser leaves it at 0 and `build_track_catalogue` skips it.
    // Verify by hand-rolling a PlayList whose STN_table puts a single
    // audio stream at PID 0 (the only way to get a non-in-mux entry
    // through the encoder, but works as a degenerate sanity probe).
    let tmp = tempdir_for_test();
    let bdmv = tmp.path().join("BDMV");

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
                out_time_ticks: 45_000,
                multi_clip_count: 1,
                angles: Vec::new(),
                stn_table: StnTable {
                    primary_audio: vec![PrimaryAudioStream {
                        elementary_pid: 0,
                        coding_type: StreamCodingType::Ac3Audio,
                        audio_format: 0x03,
                        sample_rate: 0x01,
                        language_code: *b"eng",
                    }],
                    ..StnTable::default()
                },
                flags: PlayItemFlags::default(),
            }],
            sub_paths: vec![],
        },
        marks: vec![],
    };
    write_file(&bdmv.join("PLAYLIST/00000.mpls"), &pl.encode());
    write_file(&bdmv.join("STREAM/00001.m2ts"), &make_m2ts(4));

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

    let disc = Disc::mount(tmp.path()).expect("mount synthetic disc");
    let title = &disc.titles()[0];
    let catalogue = disc.title_streams(title);
    assert!(
        catalogue.is_empty(),
        "PID 0 must be filtered out: {:?}",
        catalogue
    );
}

// ─────────────────────── tempdir helper ───────────────────────

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

// Monotonic per-process counter so concurrent tests can't collide on
// the same temp-dir name even when their SystemTime::now() readings
// land in the same nanosecond bucket.
static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn tempdir_for_test() -> TestDir {
    let pid = std::process::id();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let serial = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("oxideav-bluray-track-cat-{pid}-{nonce}-{serial}"));
    fs::create_dir_all(&path).unwrap();
    TestDir { path }
}

// Avoid dead-code warnings on the unused `_` import — keep one line
// to silence the helper-not-used lint on per-binary tests.
#[allow(dead_code)]
const _: usize = TS_PACKET_LEN;
