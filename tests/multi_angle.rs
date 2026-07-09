//! Multi-angle BDMV mount + per-angle streaming.
//!
//! Synthesises a single-PlayItem title with three angles (primary clip
//! `00100`, alt clips `00101` / `00102`), each angle's `.m2ts`
//! fingerprinted so the test can read the byte stream back and assert
//! that [`Disc::open_title_with_angle`] selected the right `.m2ts`.
//!
//! Spec basis: BD-ROM Part 3 §5.4.4.1 `is_multi_angle` block (each
//! PlayItem may list `N` per-angle clip references — selecting an angle
//! just means swapping which `.m2ts` the streamer reads at each
//! PlayItem). Hand-built; no real disc data.
//!
//! Also exercises [`Disc::max_angle`] and the at-open angle-range
//! rejection in [`Disc::open_title_with_angle`].

use std::fs;
use std::io::{Read, Write};
use std::path::Path;

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

/// `.m2ts` with `n_packets` source packets. The TS body's first 4
/// bytes are `[0x47, tag, tag, tag]` so the test can identify which
/// angle's clip it just read.
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
            play_items: vec![PlayItem {
                clip_information_file_name: "00100".into(),
                clip_codec_identifier: *b"M2TS",
                connection_condition: ConnectionCondition::NonSeamless,
                stc_id_ref: 0,
                in_time_ticks: 0,
                out_time_ticks: 45_000 * 5,
                multi_clip_count: 3,
                angles: vec![
                    AngleClip {
                        clip_information_file_name: "00101".into(),
                        clip_codec_identifier: *b"M2TS",
                        stc_id_ref: 1,
                    },
                    AngleClip {
                        clip_information_file_name: "00102".into(),
                        clip_codec_identifier: *b"M2TS",
                        stc_id_ref: 2,
                    },
                ],
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
            }],
            sub_paths: vec![],
        },
        marks: vec![],
    };
    write_file(&bdmv.join("PLAYLIST/00000.mpls"), &pl.encode());

    // One .m2ts per angle, fingerprinted with a distinct tag so the
    // test can verify which clip the streamer landed on.
    write_file(&bdmv.join("STREAM/00100.m2ts"), &make_m2ts(8, 0xA0));
    write_file(&bdmv.join("STREAM/00101.m2ts"), &make_m2ts(8, 0xB0));
    write_file(&bdmv.join("STREAM/00102.m2ts"), &make_m2ts(8, 0xC0));

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

/// Read the first packet's fingerprint byte from the freshly-opened
/// `TitleSource`. The fingerprint identifies which `.m2ts` (and
/// therefore which angle) the streamer is feeding.
fn first_fingerprint(src: &mut impl Read) -> u8 {
    let mut hdr = [0u8; 4];
    src.read_exact(&mut hdr).unwrap();
    assert_eq!(hdr[0], 0x47, "expected TS sync byte at stream head");
    // The fingerprint occupies bytes 1..=3 (all the same value); pick
    // any of them.
    assert_eq!(hdr[1], hdr[2]);
    assert_eq!(hdr[2], hdr[3]);
    hdr[1]
}

#[test]
fn primary_angle_streams_primary_clip() {
    let tmp = tempdir_for_test();
    build_disc(tmp.path());

    let disc = Disc::mount(tmp.path()).expect("mount");
    let title = disc.longest_title().expect("title").clone();

    // angle = 0 → primary clip 00100 (fingerprint 0xA0).
    let mut src = disc
        .open_title_with_angle(&title, 0, None)
        .expect("open angle 0");
    assert_eq!(first_fingerprint(&mut src), 0xA0);
}

#[test]
fn alt_angle_one_streams_first_alt_clip() {
    let tmp = tempdir_for_test();
    build_disc(tmp.path());

    let disc = Disc::mount(tmp.path()).expect("mount");
    let title = disc.longest_title().expect("title").clone();

    // angle = 1 → first alt clip 00101 (fingerprint 0xB0).
    let mut src = disc
        .open_title_with_angle(&title, 1, None)
        .expect("open angle 1");
    assert_eq!(first_fingerprint(&mut src), 0xB0);
}

#[test]
fn alt_angle_two_streams_second_alt_clip() {
    let tmp = tempdir_for_test();
    build_disc(tmp.path());

    let disc = Disc::mount(tmp.path()).expect("mount");
    let title = disc.longest_title().expect("title").clone();

    let mut src = disc
        .open_title_with_angle(&title, 2, None)
        .expect("open angle 2");
    assert_eq!(first_fingerprint(&mut src), 0xC0);
}

#[test]
fn open_title_defaults_to_primary_angle() {
    let tmp = tempdir_for_test();
    build_disc(tmp.path());

    let disc = Disc::mount(tmp.path()).expect("mount");
    let title = disc.longest_title().expect("title").clone();

    // `open_title` is documented to equal `open_title_with_angle(0)`.
    let mut src = disc.open_title(&title, None).expect("open default");
    assert_eq!(first_fingerprint(&mut src), 0xA0);
}

#[test]
fn out_of_range_angle_is_rejected_at_open_time() {
    let tmp = tempdir_for_test();
    build_disc(tmp.path());

    let disc = Disc::mount(tmp.path()).expect("mount");
    let title = disc.longest_title().expect("title").clone();

    // 3 angles total → max valid is 2. Requesting 3 must fail
    // *before* any read, surfacing the conflict cleanly.
    let err = disc
        .open_title_with_angle(&title, 3, None)
        .expect_err("angle 3 unavailable");
    let msg = err.to_string();
    assert!(
        msg.contains("angle 3"),
        "diagnostic mentions the rejected angle: {msg}"
    );
}

#[test]
fn max_angle_reports_smallest_play_item_angle_count() {
    let tmp = tempdir_for_test();
    build_disc(tmp.path());

    let disc = Disc::mount(tmp.path()).expect("mount");
    let title = disc.longest_title().expect("title").clone();
    // Single PlayItem with 3 angles → max safely-openable angle is 2.
    assert_eq!(disc.max_angle(&title), 2);
}

/// Tempdir helper (mirrors the other integration tests' pattern).
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
    let path =
        std::env::temp_dir().join(format!("oxideav-bluray-multiangle-{pid}-{nonce}-{serial}"));
    fs::create_dir_all(&path).unwrap();
    TestDir { path }
}
