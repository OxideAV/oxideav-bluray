//! End-to-end integration test: synthesize a minimal BDMV tree in
//! `tempdir`, mount it via `Disc`, pick the longest title, and stream
//! it via `TitleSource`. Verifies the strip-TP_extra pipeline by
//! checking that the bytes pulled from the source match the synthetic
//! TS payload we put into the m2ts file.
//!
//! All inputs are entirely fabricated by this test; no real disc data
//! anywhere.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use oxideav_bluray::bdmv::index_bdmv::{AppInfoBdmv, IndexBdmv, IndexEntry, IndexObjectType};
use oxideav_bluray::bdmv::mpls::{
    AppInfoPlayList, ConnectionCondition, PlayItem, PlayItemFlags, PlayList, PlayListMpls,
    PrimaryAudioStream, PrimaryVideoStream, StnTable, StreamCodingType,
};
use oxideav_bluray::{Disc, TitleKind, M2TS_PACKET_LEN, TS_PACKET_LEN};

fn write_file(path: &Path, bytes: &[u8]) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    let mut f = fs::File::create(path).unwrap();
    f.write_all(bytes).unwrap();
}

/// Build a tiny `.m2ts` file with `n_packets` 192-byte BDAV source
/// packets. Each packet's TS body (188 bytes) is filled with
/// `[0x47, packet_idx_byte, 0xAA, 0xAA, ...]` so the test can verify
/// the strip output.
fn make_m2ts(n_packets: usize) -> Vec<u8> {
    let mut out = vec![0u8; n_packets * M2TS_PACKET_LEN];
    for i in 0..n_packets {
        let pkt_off = i * M2TS_PACKET_LEN;
        let tp_extra = (i as u32).to_be_bytes();
        out[pkt_off..pkt_off + 4].copy_from_slice(&tp_extra);
        let ts_off = pkt_off + 4;
        out[ts_off] = 0x47; // TS sync
        out[ts_off + 1] = (i & 0xFF) as u8;
        for j in 2..TS_PACKET_LEN {
            out[ts_off + j] = 0xAA;
        }
    }
    out
}

#[test]
fn synthetic_bdmv_mount_and_stream() {
    let tmp = tempdir_for_test();
    let root = tmp.path();
    let bdmv = root.join("BDMV");

    // 1. Build a 1-clip / 1-PlayItem playlist.
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
                out_time_ticks: 45_000 * 10, // 10 s
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
            }],
            sub_paths: vec![],
        },
        marks: vec![],
    };
    write_file(&bdmv.join("PLAYLIST/00000.mpls"), &pl.encode());

    // 2. Build the .m2ts file with 64 packets (2 AACS units worth).
    let n_packets = 64;
    let m2ts_bytes = make_m2ts(n_packets);
    write_file(&bdmv.join("STREAM/00001.m2ts"), &m2ts_bytes);

    // 3. Build index.bdmv with a single HDMV title pointing at PLAYLIST 0.
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
                movie_object_id_ref: 0, // → PLAYLIST 00000.mpls
            },
        }],
    };
    write_file(&bdmv.join("index.bdmv"), &idx.encode());

    // 4. Mount.
    let disc = Disc::mount(root).expect("mount synthetic disc");
    let titles = disc.titles();
    assert_eq!(titles.len(), 1);
    assert_eq!(titles[0].kind, TitleKind::Hdmv);
    assert_eq!(titles[0].playlist_id, 0);
    assert_eq!(titles[0].duration_ticks, 10 * 45_000 * 2);

    // 5. Stream the title.
    let title = disc
        .longest_title()
        .expect("at least one HDMV title")
        .clone();
    let mut src = disc.open_title(&title, None).expect("open title");
    let mut got = Vec::new();
    src.read_to_end(&mut got).expect("read title source");

    // Total expected: 64 packets × 188 bytes.
    assert_eq!(got.len(), n_packets * TS_PACKET_LEN);
    // Per-packet check: sync byte + packet index byte fingerprint.
    for i in 0..n_packets {
        let off = i * TS_PACKET_LEN;
        assert_eq!(got[off], 0x47, "TS sync byte at packet {i}");
        assert_eq!(got[off + 1], (i & 0xFF) as u8, "fingerprint at packet {i}");
    }
}

/// Tiny tempdir helper that survives test-process exit cleanup.
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
    let path = std::env::temp_dir().join(format!("oxideav-bluray-test-{pid}-{nonce}-{serial}"));
    fs::create_dir_all(&path).unwrap();
    TestDir { path }
}
