//! Stream selection over the staged STN fixtures (`tests/fixtures/stn/`,
//! vendored from `docs/container/bluray/fixtures/`): secondary audio,
//! PiP video and the stereoscopic dependent view reach
//! `Disc::title_streams` with their real PIDs and carrier clips; the MVC
//! pair demuxes both views; multi-clip SubPlayItem alternates are
//! selectable by entry id.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use oxideav_bluray::{Disc, StreamEntry, SubPathList, TS_PACKET_LEN, TitleSource, TrackKind};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/stn")
        .join(name)
}

/// Read a whole stream and histogram its TS packets by PID.
fn pid_histogram(mut src: TitleSource) -> BTreeMap<u16, usize> {
    let mut buf = Vec::new();
    src.read_to_end(&mut buf).unwrap();
    assert_eq!(buf.len() % TS_PACKET_LEN, 0);
    let mut h = BTreeMap::new();
    for p in buf.as_chunks::<TS_PACKET_LEN>().0 {
        assert_eq!(p[0], 0x47, "TS sync");
        let pid = (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]);
        *h.entry(pid).or_default() += 1;
    }
    h
}

#[test]
fn mvc_3d_lists_and_demuxes_both_views() {
    let disc = Disc::mount(fixture("mvc-3d")).unwrap();
    let title = disc.titles()[0].clone();
    let cat = disc.title_streams(&title);

    let base = cat.by_kind(TrackKind::PrimaryVideo).next().unwrap();
    assert_eq!(base.pid, 0x1011);
    assert_eq!(base.carrier_clips, vec!["00000".to_string()]);
    assert!(base.is_in_main_path());

    let dv = cat.by_kind(TrackKind::DependentView).next().unwrap();
    assert_eq!(dv.pid, 0x1012);
    assert_eq!(dv.entry, StreamEntry::sub_clip(0, 0, 0x1012));
    assert_eq!(dv.carrier_clips, vec!["00001".to_string()]);
    assert!(!dv.is_in_main_path());
    assert_eq!(dv.label(), "H.264/MVC Dependent View");
    // Catalogue order: dependent view last.
    assert_eq!(cat.tracks.last().unwrap().kind, TrackKind::DependentView);

    // Base view (+ audio + PG) from the main path.
    let main = pid_histogram(disc.open_track(&title, base, None).unwrap());
    assert_eq!(main.get(&0x1011), Some(&130));
    assert_eq!(main.get(&0x1100), Some(&625));
    assert_eq!(main.get(&0x1200), Some(&42));
    assert!(!main.contains_key(&0x1012));

    // Dependent view from the ExtensionData SubPath's clip.
    let dep = pid_histogram(disc.open_track(&title, dv, None).unwrap());
    assert_eq!(dep.get(&0x1012), Some(&196));
    assert!(!dep.contains_key(&0x1011));
    let total: usize = dep.values().sum();
    assert_eq!(total, 224, "every source packet of 00001.m2ts");

    // Same clip set through the explicit SubPath entry point.
    let again = disc
        .open_sub_path(&title, SubPathList::Extension, 0, 0, None)
        .unwrap();
    assert_eq!(pid_histogram(again), dep);
    // The PlayList's own SubPath loop is empty on a 3D title.
    assert!(
        disc.open_sub_path(&title, SubPathList::PlayList, 0, 0, None)
            .is_err()
    );
    assert!(
        disc.open_sub_path(&title, SubPathList::Extension, 0, 1, None)
            .is_err()
    );
}

#[test]
fn split_3d_dependent_view_follows_the_sub_play_item_sequence() {
    let disc = Disc::mount(fixture("mvc-3d-split")).unwrap();
    let title = disc.titles()[0].clone();
    let cat = disc.title_streams(&title);
    let dv = cat.by_kind(TrackKind::DependentView).next().unwrap();
    assert_eq!(dv.playitem_count, 2);
    assert_eq!(
        dv.carrier_clips,
        vec!["00001".to_string(), "00003".to_string()]
    );
    let base = cat.by_kind(TrackKind::PrimaryVideo).next().unwrap();
    assert_eq!(
        base.carrier_clips,
        vec!["00000".to_string(), "00002".to_string()]
    );
}

#[test]
fn pip_and_secondary_audio_reach_the_catalogue() {
    let disc = Disc::mount(fixture("pip-secondary-audio")).unwrap();
    let title = disc.titles()[0].clone();
    let cat = disc.title_streams(&title);
    let sa = cat.by_kind(TrackKind::SecondaryAudio).next().unwrap();
    assert_eq!(sa.pid, 0x1A00);
    assert_eq!(sa.language.as_deref(), Some("eng"));
    assert!(sa.is_in_main_path());
    let pip = cat.by_kind(TrackKind::SecondaryVideo).next().unwrap();
    assert_eq!(pip.pid, 0x1B00);
    assert_eq!(pip.entry, StreamEntry::in_mux_sub_path(0, 0x1B00));
    assert_eq!(pip.carrier_clips, vec!["00000".to_string()]);
    assert!(pip.is_in_main_path());
    assert_eq!(cat.len(), 5);

    let disc = Disc::mount(fixture("two-pip")).unwrap();
    let title = disc.titles()[0].clone();
    let cat = disc.title_streams(&title);
    let pips: Vec<(u16, Option<u8>)> = cat
        .by_kind(TrackKind::SecondaryVideo)
        .map(|t| (t.pid, t.entry.sub_path_id()))
        .collect();
    assert_eq!(pips.len(), 2);
    assert_eq!(pips[0].1, Some(0));
    assert_eq!(pips[1].1, Some(1));
    assert_eq!(cat.by_kind(TrackKind::SecondaryAudio).count(), 2);
    assert_eq!(cat.by_kind(TrackKind::PrimaryAudio).count(), 2);
}

#[test]
fn multi_primary_catalogue_is_unchanged_in_shape() {
    let disc = Disc::mount(fixture("multi-primary")).unwrap();
    let title = disc.titles()[0].clone();
    let cat = disc.title_streams(&title);
    let langs: Vec<_> = cat
        .by_kind(TrackKind::PrimaryAudio)
        .map(|t| t.language.clone().unwrap())
        .collect();
    assert_eq!(langs, vec!["eng", "fra", "jpn", "deu"]);
    assert_eq!(cat.by_kind(TrackKind::PgSubtitle).count(), 2);
    assert!(cat.tracks.iter().all(|t| t.is_in_main_path()));
    assert_eq!(title.languages, vec!["deu", "eng", "fra", "jpn"]);
}

// ───────────── multi-clip alternates over a synthesised STREAM dir ─────────────

struct TempDisc(PathBuf);

impl Drop for TempDisc {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// One 192-byte source packet: 4-byte TP_extra + TS packet on `pid`
/// whose first payload byte is `tag`.
fn source_packet(pid: u16, tag: u8) -> [u8; 192] {
    let mut p = [0xFFu8; 192];
    p[..4].copy_from_slice(&[0, 0, 0, 0]);
    p[4] = 0x47;
    p[5] = 0x40 | ((pid >> 8) as u8 & 0x1F);
    p[6] = pid as u8;
    p[7] = 0x10;
    p[8] = tag;
    p
}

fn write_clip(dir: &Path, stem: &str, pid: u16, tag: u8, packets: usize) {
    let mut out = Vec::new();
    for _ in 0..packets {
        out.extend_from_slice(&source_packet(pid, tag));
    }
    fs::write(dir.join(format!("{stem}.m2ts")), out).unwrap();
}

/// The multi-clip playlist is structural (its clips do not exist), so
/// build a disc around it: the index / movie objects of another
/// fixture, the playlist itself, and one synthetic clip per referenced
/// stem, each tagged so the test can tell which one was streamed.
fn multiclip_disc() -> TempDisc {
    static SERIAL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "oxideav-bluray-stn-{}-{}",
        std::process::id(),
        SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let bdmv = root.join("BDMV");
    for d in ["PLAYLIST", "CLIPINF", "STREAM"] {
        fs::create_dir_all(bdmv.join(d)).unwrap();
    }
    let donor = fixture("pip-secondary-audio").join("BDMV");
    for f in ["index.bdmv", "MovieObject.bdmv"] {
        fs::copy(donor.join(f), bdmv.join(f)).unwrap();
    }
    fs::copy(
        fixture("multiclip-subplayitem").join("BDMV/PLAYLIST/00000.mpls"),
        bdmv.join("PLAYLIST/00000.mpls"),
    )
    .unwrap();
    let stream = bdmv.join("STREAM");
    write_clip(&stream, "00000", 0x1011, 0xA0, 4);
    write_clip(&stream, "00010", 0x1A00, 0x10, 3);
    write_clip(&stream, "00011", 0x1A00, 0x11, 5);
    write_clip(&stream, "00012", 0x1A00, 0x12, 7);
    TempDisc(root)
}

fn tags(mut src: TitleSource) -> Vec<u8> {
    let mut buf = Vec::new();
    src.read_to_end(&mut buf).unwrap();
    buf.as_chunks::<TS_PACKET_LEN>()
        .0
        .iter()
        .map(|p| p[4])
        .collect()
}

#[test]
fn multi_clip_alternates_are_selectable_by_entry_id() {
    let tmp = multiclip_disc();
    let disc = Disc::mount(&tmp.0).unwrap();
    let title = disc.titles()[0].clone();
    let cat = disc.title_streams(&title);
    let sa = cat.by_kind(TrackKind::SecondaryAudio).next().unwrap();
    assert_eq!(sa.entry, StreamEntry::sub_clip(1, 2, 0x1A00));
    assert_eq!(sa.carrier_clips, vec!["00012".to_string()]);

    // The track's own entry id (2) → only clip 00012, never the
    // concatenation of the alternates.
    assert_eq!(
        tags(disc.open_track(&title, sa, None).unwrap()),
        vec![0x12; 7]
    );
    // Any alternate by explicit entry id.
    for (id, tag, n) in [(0u8, 0x10u8, 3usize), (1, 0x11, 5), (2, 0x12, 7)] {
        let src = disc
            .open_sub_path(&title, SubPathList::PlayList, 1, id, None)
            .unwrap();
        assert_eq!(tags(src), vec![tag; n], "entry {id}");
    }
    assert!(
        disc.open_sub_path(&title, SubPathList::PlayList, 1, 3, None)
            .is_err()
    );
    // SubPath 0 is the in-mux PiP path: its SubPlayItem names the main
    // clip.
    let pip = disc
        .open_sub_path(&title, SubPathList::PlayList, 0, 0, None)
        .unwrap();
    assert_eq!(tags(pip), vec![0xA0; 4]);
}

#[cfg(feature = "registry")]
#[test]
fn bluray_uri_stream_selector_opens_the_dependent_view() {
    use oxideav_bluray::source::{open_bluray, open_bluray_multi_title};
    let root = fixture("mvc-3d");
    let read_pids = |mut src: Box<dyn oxideav_core::BytesSource>| {
        let mut buf = Vec::new();
        src.read_to_end(&mut buf).unwrap();
        let mut h: BTreeMap<u16, usize> = BTreeMap::new();
        for p in buf.as_chunks::<TS_PACKET_LEN>().0 {
            *h.entry((u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]))
                .or_default() += 1;
        }
        h
    };
    let uri = |q: &str| format!("bluray://{}{q}", root.display());

    let dep = read_pids(open_bluray(&uri("?stream=0x1012")).unwrap());
    assert_eq!(dep.get(&0x1012), Some(&196));
    assert!(!dep.contains_key(&0x1011));
    // An in-mux stream selects the main path.
    let main = read_pids(open_bluray(&uri("?stream=0x1100")).unwrap());
    assert_eq!(main.get(&0x1011), Some(&130));
    // A PID the title does not list is an error, not a silent fallback.
    assert!(open_bluray(&uri("?stream=0x1FFE")).is_err());

    let mut multi = open_bluray_multi_title(&uri("?stream=0x1012")).unwrap();
    assert_eq!(multi.title_count(), 1);
    let dep2 = read_pids(multi.open_title(0).unwrap());
    assert_eq!(dep2, dep);
    // Chapter slicing is main-path only.
    assert!(open_bluray_multi_title(&uri("?stream=0x1012&chapters=1")).is_err());
    assert!(open_bluray_multi_title(&uri("?stream=0x1100&chapters=1")).is_ok());
}
