//! STN_table / STN_table_SS / SubPath / multi-clip SubPlayItem
//! coverage against the six self-authored BDMV playlist fixtures staged
//! under `docs/container/bluray/fixtures/` (vendored here under
//! `tests/fixtures/stn/`, renamed by what they exercise).
//!
//! Each fixture ships a `dump.txt`: a mechanical field-by-field dump
//! (`0xOFFSET  name  [raw bytes] = value`) of every `.mpls` / `.clpi`
//! in it. The comparison here is structured, not textual: the parsed
//! [`PlayListMpls`] / [`ClipInformation`] is walked and re-emitted as an
//! ordered `(field name, raw bytes)` list using the dump's own field
//! names, then compared against the dump's lines with only pure
//! framing (lengths, start addresses, reserved bits, undecoded
//! attribute padding) filtered out. Every semantic field of every file
//! must match, in order, byte for byte.

use std::path::{Path, PathBuf};

use oxideav_bluray::bdmv::clpi::ClipInformation;
use oxideav_bluray::{
    PlayItem, PlayListMpls, PrimaryVideoStream, StreamCodingType, StreamEntry, SubPath,
    SubPathList, SubPathType,
};

fn playlist(name: &str) -> PlayListMpls {
    let bytes = std::fs::read(fixture(name).join("BDMV/PLAYLIST/00000.mpls")).unwrap();
    PlayListMpls::parse(&bytes).unwrap()
}

fn clip(name: &str, stem: &str) -> ClipInformation {
    let bytes = std::fs::read(fixture(name).join(format!("BDMV/CLIPINF/{stem}.clpi"))).unwrap();
    ClipInformation::parse(&bytes).unwrap()
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/stn")
        .join(name)
}

pub const FIXTURES: [&str; 6] = [
    "multi-primary",
    "pip-secondary-audio",
    "two-pip",
    "mvc-3d",
    "mvc-3d-split",
    "multiclip-subplayitem",
];

// ───────────────────────────── dump reader ─────────────────────────────

/// One dumped field.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    name: String,
    bytes: Vec<u8>,
}

/// One dumped file: its name and fields in order.
struct DumpFile {
    name: String,
    fields: Vec<Field>,
}

fn parse_hex(s: &str) -> Vec<u8> {
    s.split_whitespace()
        .map(|b| u8::from_str_radix(b, 16).expect("dump hex byte"))
        .collect()
}

/// Parse a `dump.txt`. Field lines are `0xOFFS  <name>  [<hex>] …`;
/// file headers are `# <file>  (<n> bytes)`.
fn read_dump(path: &Path) -> Vec<DumpFile> {
    let text = std::fs::read_to_string(path).unwrap();
    let mut files: Vec<DumpFile> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# ") {
            let name = rest.split_whitespace().next().unwrap().to_string();
            files.push(DumpFile {
                name,
                fields: Vec::new(),
            });
            continue;
        }
        if !line.starts_with("0x") {
            continue;
        }
        let body = line[2..].trim_start_matches(|c: char| c.is_ascii_hexdigit());
        // The raw-bytes bracket is the first " [" whose content is hex
        // and is followed by "]" then end-of-line or " = ".
        let mut found = None;
        let mut search = 0;
        while let Some(i) = body[search..].find(" [") {
            let at = search + i;
            if let Some(j) = body[at + 2..].find(']') {
                let inner = &body[at + 2..at + 2 + j];
                let after = &body[at + 3 + j..];
                if inner.chars().all(|c| c.is_ascii_hexdigit() || c == ' ')
                    && (after.is_empty() || after.starts_with(" = "))
                {
                    found = Some((at, inner));
                    break;
                }
            }
            search = at + 2;
        }
        let (at, inner) = found.unwrap_or_else(|| panic!("unparsed dump line {line:?}"));
        files.last_mut().unwrap().fields.push(Field {
            name: body[..at].trim().to_string(),
            bytes: parse_hex(inner),
        });
    }
    files
}

/// Pure framing / padding the typed API intentionally does not carry.
fn is_framing(name: &str) -> bool {
    name == "length"
        || (name.starts_with("reserved") && !name.contains('|'))
        || name == "reserved(31)|is_ATC_delta(1)"
        || name.ends_with(".length")
        || name.ends_with("_start_address")
        || name.starts_with("ext_data_start_address")
        || name == "data_block_start_address"
        || name == "(remaining/reserved)"
        || name == "(coding info body)"
        || name.starts_with("(rest of ClipInfo")
        || name == "=== ExtensionData"
}

// ───────────────────────────── emitter ─────────────────────────────

#[derive(Default)]
struct Emit(Vec<Field>);

impl Emit {
    fn f(&mut self, name: impl Into<String>, bytes: &[u8]) {
        self.0.push(Field {
            name: name.into(),
            bytes: bytes.to_vec(),
        });
    }
    fn hdr(&mut self, name: impl Into<String>) {
        self.f(name, &[]);
    }
    fn u8(&mut self, name: &str, v: u8) {
        self.f(name, &[v]);
    }
    fn u16(&mut self, name: &str, v: u16) {
        self.f(name, &v.to_be_bytes());
    }
    fn u32(&mut self, name: &str, v: u32) {
        self.f(name, &v.to_be_bytes());
    }

    fn entry(&mut self, e: &StreamEntry) {
        self.u8("stream_entry.type", e.stream_type());
        match *e {
            StreamEntry::MainClip { pid } => self.u16("ref_to_stream_PID_of_mainClip", pid),
            StreamEntry::SubClip {
                sub_path_id,
                sub_clip_entry_id,
                pid,
            } => {
                self.u8("ref_to_SubPath_id", sub_path_id);
                self.u8("ref_to_subClip_entry_id", sub_clip_entry_id);
                self.u16("ref_to_stream_PID_of_subClip", pid);
            }
            StreamEntry::InMuxSubPath { sub_path_id, pid }
            | StreamEntry::EnhancementLayer { sub_path_id, pid } => {
                self.u8("ref_to_SubPath_id", sub_path_id);
                self.u16("ref_to_stream_PID_of_mainClip", pid);
            }
            _ => panic!("unknown stream entry {e:?}"),
        }
    }

    fn video(&mut self, coding: StreamCodingType, vf: u8, fr: u8) {
        self.u8("stream_coding_type", coding.as_raw());
        self.u8("video_format(4)|frame_rate(4)", (vf << 4) | fr);
    }

    fn audio(&mut self, coding: StreamCodingType, af: u8, sr: u8, lang: &[u8; 3]) {
        self.u8("stream_coding_type", coding.as_raw());
        self.u8("presentation_type(4)|sampling_freq(4)", (af << 4) | sr);
        self.f("language_code", lang);
    }

    fn graphics(&mut self, coding: StreamCodingType, lang: &[u8; 3]) {
        self.u8("stream_coding_type", coding.as_raw());
        self.f("language_code", lang);
    }

    fn primary_video(&mut self, s: &PrimaryVideoStream) {
        self.entry(&s.entry);
        self.video(s.coding_type, s.video_format, s.frame_rate);
    }

    fn play_item(&mut self, i: usize, pi: &PlayItem) {
        self.hdr(format!("=== PlayItem[{i}]"));
        self.f(
            "Clip_Information_file_name",
            pi.clip_information_file_name.as_bytes(),
        );
        self.f("Clip_codec_identifier", &pi.clip_codec_identifier);
        let multi = u16::from(pi.multi_clip_count > 1);
        self.u16(
            "reserved(11)|is_multi_angle(1)|connection_condition(4)",
            (multi << 4) | u16::from(pi.connection_condition.as_raw()),
        );
        self.u8("ref_to_STC_id", pi.stc_id_ref);
        self.u32("IN_time", pi.in_time_ticks);
        self.u32("OUT_time", pi.out_time_ticks);
        self.f("UO_mask_table", &pi.flags.uo_mask.to_be_bytes());
        self.u8(
            "random_access_flag(1)|reserved(7)",
            u8::from(pi.flags.random_access_flag) << 7,
        );
        self.u8("still_mode", pi.flags.still_mode);
        self.u16("still_time", pi.flags.still_time);
        let t = &pi.stn_table;
        self.u8(
            "number_of_primary_video_stream_entries",
            t.primary_video.len() as u8,
        );
        self.u8(
            "number_of_primary_audio_stream_entries",
            t.primary_audio.len() as u8,
        );
        self.u8(
            "number_of_PG_textST_stream_entries",
            t.pg_subtitles.len() as u8,
        );
        self.u8("number_of_IG_stream_entries", t.ig_streams.len() as u8);
        self.u8(
            "number_of_secondary_audio_stream_entries",
            t.secondary_audio.len() as u8,
        );
        self.u8(
            "number_of_secondary_video_stream_entries",
            t.secondary_video.len() as u8,
        );
        self.u8(
            "number_of_PiP_PG_textST_plus_stream_entries",
            t.pip_pg.len() as u8,
        );
        self.u8(
            "number_of_DV(wiki)/reserved(Sony)_stream_entries",
            t.eighth_count,
        );
        for (k, s) in t.primary_video.iter().enumerate() {
            self.hdr(format!("--- primary_video[{k}]"));
            self.primary_video(s);
        }
        for (k, s) in t.primary_audio.iter().enumerate() {
            self.hdr(format!("--- primary_audio[{k}]"));
            self.entry(&s.entry);
            self.audio(
                s.coding_type,
                s.audio_format,
                s.sample_rate,
                &s.language_code,
            );
        }
        for (k, s) in t.pg_subtitles.iter().enumerate() {
            self.hdr(format!("--- PG_textST[{k}]"));
            self.entry(&s.entry);
            self.graphics(s.coding_type, &s.language_code);
        }
        for (k, s) in t.pip_pg.iter().enumerate() {
            self.hdr(format!("--- PG_textST[{}]", t.pg_subtitles.len() + k));
            self.entry(&s.entry);
            self.graphics(s.coding_type, &s.language_code);
        }
        for (k, s) in t.ig_streams.iter().enumerate() {
            self.hdr(format!("--- IG[{k}]"));
            self.entry(&s.entry);
            self.graphics(s.coding_type, &s.language_code);
        }
        for (k, s) in t.secondary_audio.iter().enumerate() {
            self.hdr(format!("--- secondary_audio[{k}]"));
            self.entry(&s.entry);
            self.audio(
                s.coding_type,
                s.audio_format,
                s.sample_rate,
                &s.language_code,
            );
            self.u8(
                "number_of_primary_audio_ref_entries",
                s.primary_audio_refs.len() as u8,
            );
            for &r in &s.primary_audio_refs {
                self.u8("primary_audio_stream_id_ref", r);
            }
        }
        for (k, s) in t.secondary_video.iter().enumerate() {
            self.hdr(format!("--- secondary_video[{k}]"));
            self.entry(&s.entry);
            self.video(s.coding_type, s.video_format, s.frame_rate);
            self.u8(
                "number_of_secondary_audio_ref_entries",
                s.secondary_audio_refs.len() as u8,
            );
            for &r in &s.secondary_audio_refs {
                self.u8("secondary_audio_stream_id_ref", r);
            }
            self.u8(
                "number_of_PiP_PG_textST_ref_entries",
                s.pip_pg_refs.len() as u8,
            );
            for &r in &s.pip_pg_refs {
                self.u8("PiP_PG_textST_stream_id_ref", r);
            }
        }
    }

    fn sub_path(&mut self, i: usize, sp: &SubPath) {
        self.hdr(format!("=== SubPath[{i}]"));
        self.u8("SubPath_type", sp.sub_path_type);
        self.u16(
            "reserved(15)|is_repeat_SubPath(1)",
            u16::from(sp.is_repeat_subpath),
        );
        self.u8("number_of_SubPlayItems", sp.sub_play_items.len() as u8);
        for (j, spi) in sp.sub_play_items.iter().enumerate() {
            self.hdr(format!("=== SubPlayItem[{j}]"));
            self.f(
                "Clip_Information_file_name[0]",
                spi.clip_information_file_name.as_bytes(),
            );
            self.f("Clip_codec_identifier[0]", &spi.clip_codec_identifier);
            let multi = u32::from(!spi.multi_clips.is_empty());
            self.u32(
                "reserved(27)|connection_condition(4)|is_multi_clip_entries(1)",
                (u32::from(spi.connection_condition.as_raw()) << 1) | multi,
            );
            self.u8("ref_to_STC_id[0]", spi.stc_id_ref);
            self.u32("SubPlayItem_IN_time", spi.in_time_ticks);
            self.u32("SubPlayItem_OUT_time", spi.out_time_ticks);
            self.u16("sync_PlayItem_id", spi.sync_play_item_id);
            self.u32("sync_start_PTS_of_PlayItem", spi.sync_start_pts_ticks);
            if multi == 1 {
                self.u8("number_of_multi_clip_entries", spi.num_clips() as u8);
                for (k, c) in spi.multi_clips.iter().enumerate() {
                    self.f(
                        format!("Clip_Information_file_name[{}]", k + 1),
                        c.clip_information_file_name.as_bytes(),
                    );
                    self.f("Clip_codec_identifier", &c.clip_codec_identifier);
                    self.u8("ref_to_STC_id", c.stc_id_ref);
                }
            }
        }
    }

    fn ext_table(&mut self, entries: &[oxideav_bluray::ExtensionDataEntry]) {
        if entries.is_empty() {
            return;
        }
        self.u8("number_of_ext_data_entries", entries.len() as u8);
        for e in entries {
            self.u16("ID1", e.id1);
            self.u16("ID2", e.id2);
            self.u32("ext_data_length", e.data.len() as u32);
        }
    }

    fn mpls(&mut self, pl: &PlayListMpls) {
        self.f("type_indicator", b"MPLS");
        self.f("version_number", &pl.version);
        self.hdr("=== AppInfoPlayList");
        let a = &pl.app_info;
        let mut body = vec![0, a.playback_type];
        body.extend_from_slice(&a.playback_count.to_be_bytes());
        body.extend_from_slice(&a.uo_mask.to_be_bytes());
        body.push(
            (a.random_access_flag << 7)
                | (a.audio_mix_app_flag << 6)
                | (a.lossless_may_bypass_mixer_flag << 5),
        );
        body.push(0);
        self.f("(AppInfoPlayList body)", &body);
        self.hdr("=== PlayList");
        self.u16("number_of_PlayItems", pl.play_list.play_items.len() as u16);
        self.u16("number_of_SubPaths", pl.play_list.sub_paths.len() as u16);
        for (i, pi) in pl.play_list.play_items.iter().enumerate() {
            self.play_item(i, pi);
        }
        for (i, sp) in pl.play_list.sub_paths.iter().enumerate() {
            self.sub_path(i, sp);
        }
        self.hdr("=== PlayListMark");
        let mut marks = (pl.marks.len() as u16).to_be_bytes().to_vec();
        for m in &pl.marks {
            marks.push(0);
            marks.push(m.mark_type);
            marks.extend_from_slice(&m.ref_play_item_id.to_be_bytes());
            marks.extend_from_slice(&m.mark_time_ticks.to_be_bytes());
            marks.extend_from_slice(&m.entry_es_pid.to_be_bytes());
            marks.extend_from_slice(&m.duration_ticks.to_be_bytes());
        }
        self.f("(marks)", &marks);
        let ext = &pl.extension;
        self.ext_table(&ext.entries);
        for e in &ext.entries {
            self.hdr(format!(
                "=== ext data ID1={} ID2={} len={}",
                e.id1,
                e.id2,
                e.data.len()
            ));
            match e.id() {
                (1, 1) => self.f("(PiP metadata block, raw)", &e.data),
                (2, 1) => {
                    for (k, ss) in ext.stn_table_ss.iter().enumerate() {
                        self.hdr(format!("--- STN_table_SS for PlayItem[{k}]"));
                        self.u16("fixed_offset_during_PopUp(1)|reserved(15)", ss.flags);
                        let dv = ss.dependent_view.as_ref().expect("dependent view");
                        self.primary_video(dv);
                        self.f("(remaining STN_SS bytes, not decoded)", &ss.trailing);
                    }
                }
                (2, 2) => {
                    self.u16("number_of_SubPath_extensions", ext.sub_paths.len() as u16);
                    for (i, sp) in ext.sub_paths.iter().enumerate() {
                        self.sub_path(i, sp);
                    }
                }
                _ => self.f("(raw)", &e.data),
            }
        }
    }

    fn clpi(&mut self, c: &ClipInformation) {
        self.f("type_indicator", b"HDMV");
        self.f("version_number", &c.version);
        self.hdr("=== ClipInfo");
        self.u8("Clip_stream_type", c.clip_info.clip_stream_type);
        self.u8("application_type", c.clip_info.application_type);
        self.u32("TS_recording_rate", c.clip_info.ts_recording_rate);
        self.u32(
            "number_of_source_packets",
            c.clip_info.number_of_source_packets,
        );
        self.hdr("=== ProgramInfo");
        self.u8(
            "number_of_program_sequences",
            c.program_info.programs.len() as u8,
        );
        for p in &c.program_info.programs {
            self.u32("SPN_program_sequence_start", p.spn_program_sequence_start);
            self.u16("program_map_PID", p.program_map_pid);
            self.u8("number_of_streams_in_ps", p.streams.len() as u8);
            for (k, s) in p.streams.iter().enumerate() {
                self.hdr(format!("--- stream[{k}]"));
                self.u16("stream_PID", s.pid);
                self.u8("stream_coding_type", s.stream_coding_type);
            }
        }
        self.ext_table(&c.extension.entries);
        for e in &c.extension.entries {
            self.hdr(format!(
                "=== ext data ID1={} ID2={} len={}",
                e.id1,
                e.id2,
                e.data.len()
            ));
            self.f("(raw)", &e.data);
        }
    }
}

fn file_path(fixture_dir: &Path, dump_name: &str) -> PathBuf {
    let bdmv = fixture_dir.join("BDMV");
    if dump_name.ends_with(".clpi") {
        return bdmv.join("CLIPINF").join(dump_name);
    }
    let direct = bdmv.join("PLAYLIST").join(dump_name);
    if direct.exists() {
        return direct;
    }
    // The multi-clip dump names its playlist after the writer's scratch
    // file; the fixture holds exactly one playlist.
    bdmv.join("PLAYLIST").join("00000.mpls")
}

fn compare(fixture_name: &str) -> usize {
    let dir = fixture(fixture_name);
    let dumps = read_dump(&dir.join("dump.txt"));
    assert!(!dumps.is_empty());
    let mut checked = 0;
    for d in &dumps {
        let bytes = std::fs::read(file_path(&dir, &d.name)).unwrap();
        let mut e = Emit::default();
        if d.name.ends_with(".mpls") {
            e.mpls(&PlayListMpls::parse(&bytes).unwrap());
        } else {
            e.clpi(&ClipInformation::parse(&bytes).unwrap());
        }
        let want: Vec<&Field> = d.fields.iter().filter(|f| !is_framing(&f.name)).collect();
        let got: Vec<&Field> = e.0.iter().collect();
        for (i, (w, g)) in want.iter().zip(got.iter()).enumerate() {
            assert_eq!(
                (w.name.as_str(), &w.bytes),
                (g.name.as_str(), &g.bytes),
                "{fixture_name}/{}: field #{i} differs",
                d.name
            );
        }
        assert_eq!(
            want.len(),
            got.len(),
            "{fixture_name}/{}: field count (dump vs parsed)",
            d.name
        );
        checked += want.len();
    }
    checked
}

#[test]
fn every_fixture_matches_its_dump_field_by_field() {
    let mut total = 0;
    for name in FIXTURES {
        let n = compare(name);
        eprintln!("{name}: {n} semantic fields match dump.txt");
        assert!(n > 20, "{name}: only {n} fields compared");
        total += n;
    }
    // Guard against the filter silently swallowing the corpus.
    assert!(total > 600, "only {total} semantic fields compared");
}

#[test]
fn every_playlist_reencodes_byte_identically() {
    for name in FIXTURES {
        let path = fixture(name).join("BDMV/PLAYLIST/00000.mpls");
        let bytes = std::fs::read(&path).unwrap();
        let pl = PlayListMpls::parse(&bytes).unwrap();
        assert_eq!(pl.encode(), bytes, "{name}: encode(parse(x)) != x");
    }
}

// ───────────────────────── semantic spot checks ─────────────────────────

#[test]
fn multi_primary_lists_four_audio_two_pg_all_type_1() {
    let pl = playlist("multi-primary");
    let t = &pl.play_list.play_items[0].stn_table;
    let audio: Vec<(u16, u8, [u8; 3])> = t
        .primary_audio
        .iter()
        .map(|a| (a.elementary_pid, a.coding_type.as_raw(), a.language_code))
        .collect();
    assert_eq!(
        audio,
        vec![
            (0x1100, 0x81, *b"eng"),
            (0x1101, 0x81, *b"fra"),
            (0x1102, 0x80, *b"jpn"),
            (0x1103, 0x82, *b"deu"),
        ]
    );
    assert_eq!(t.primary_audio[1].audio_format, 6); // multi-channel
    let pg: Vec<u16> = t.pg_subtitles.iter().map(|p| p.elementary_pid).collect();
    assert_eq!(pg, vec![0x1200, 0x1201]);
    assert!(t.pip_pg.is_empty());
    for e in t
        .primary_video
        .iter()
        .map(|s| s.entry)
        .chain(t.primary_audio.iter().map(|s| s.entry))
        .chain(t.pg_subtitles.iter().map(|s| s.entry))
    {
        assert_eq!(e.stream_type(), 1);
        assert!(e.is_in_main_clip());
    }
    assert!(pl.play_list.sub_paths.is_empty());
    assert!(pl.extension.is_empty());
}

#[test]
fn pip_entry_is_type_3_through_in_mux_sub_path() {
    let pl = playlist("pip-secondary-audio");
    let t = &pl.play_list.play_items[0].stn_table;
    assert_eq!(t.secondary_audio.len(), 1);
    let sa = &t.secondary_audio[0];
    assert_eq!(sa.entry, StreamEntry::main_clip(0x1A00));
    assert_eq!(sa.coding_type, StreamCodingType::EAc3SecondaryAudio);
    assert!(sa.primary_audio_refs.is_empty());
    let sv = &t.secondary_video[0];
    assert_eq!(sv.entry, StreamEntry::in_mux_sub_path(0, 0x1B00));
    assert_eq!(sv.elementary_pid, 0x1B00);
    assert_eq!((sv.video_format, sv.frame_rate), (5, 1));
    assert_eq!(sv.secondary_audio_refs, vec![0]);
    assert!(sv.pip_pg_refs.is_empty());
    assert_eq!(pl.play_list.sub_paths[0].kind(), SubPathType::InMuxSyncPip);

    let r = pl
        .resolve_stream_entry(0, &sv.entry, SubPathList::PlayList)
        .unwrap();
    assert!(r.is_main_clip());
    assert_eq!(r.clip_information_file_name, "00000");
    assert_eq!(r.pid, 0x1B00);
    assert_eq!(r.sub_path_index, Some(0));
    assert_eq!(r.sub_play_item_index, Some(0));
    // PiP metadata stays raw under (1, 1).
    assert_eq!(pl.extension.entry(1, 1).unwrap().data.len(), 30);
}

#[test]
fn two_pips_reference_their_own_sub_path_and_secondary_audio() {
    let pl = playlist("two-pip");
    let t = &pl.play_list.play_items[0].stn_table;
    assert_eq!(t.primary_audio.len(), 2);
    assert_eq!(t.secondary_audio.len(), 2);
    assert_eq!(t.secondary_video.len(), 2);
    for (i, sv) in t.secondary_video.iter().enumerate() {
        assert_eq!(sv.entry.stream_type(), 3);
        assert_eq!(sv.entry.sub_path_id(), Some(i as u8));
        assert_eq!(sv.secondary_audio_refs, vec![i as u8]);
        let r = pl
            .resolve_stream_entry(0, &sv.entry, SubPathList::PlayList)
            .unwrap();
        assert_eq!(r.sub_path_index, Some(i));
        assert_eq!(r.clip_information_file_name, "00000");
    }
    assert_ne!(
        t.secondary_video[0].elementary_pid,
        t.secondary_video[1].elementary_pid
    );
    assert_eq!(pl.play_list.sub_paths.len(), 2);
}

#[test]
fn mvc_dependent_view_lives_in_stn_table_ss_and_extension_sub_path() {
    let pl = playlist("mvc-3d");
    assert!(pl.play_list.sub_paths.is_empty());
    let ext = &pl.extension;
    assert_eq!(ext.stn_table_ss.len(), 1);
    let ss = &ext.stn_table_ss[0];
    assert!(!ss.fixed_offset_during_popup());
    assert_eq!(ss.trailing, vec![0x00, 0x01, 0xFF, 0x00]);
    let dv = ss.dependent_view.unwrap();
    assert_eq!(dv.entry, StreamEntry::sub_clip(0, 0, 0x1012));
    assert_eq!(dv.coding_type, StreamCodingType::MvcDependentVideo);
    assert!(dv.coding_type.is_video());
    assert_eq!(ext.sub_paths.len(), 1);
    assert_eq!(ext.sub_paths[0].kind(), SubPathType::StereoscopicVideo);

    let r = pl
        .resolve_stream_entry(0, &dv.entry, SubPathList::Extension)
        .unwrap();
    assert_eq!(r.clip_information_file_name, "00001");
    assert_eq!(r.pid, 0x1012);
    assert_eq!(r.sub_clip_entry_id, Some(0));
    // Against the (empty) PlayList SubPath loop the entry cannot resolve.
    assert!(
        pl.resolve_stream_entry(0, &dv.entry, SubPathList::PlayList)
            .is_none()
    );

    // The dependent-view clip: application_type 8, empty ProgramInfo,
    // the PID listed in ProgramInfo_SS (2, 5).
    let c = clip("mvc-3d", "00001");
    assert_eq!(c.clip_info.application_type, 8);
    assert_eq!(c.program_info.stream_count(), 0);
    let ss_pi = c.extension.program_info_ss.as_ref().unwrap();
    let s = ss_pi.stream_by_pid(0x1012).unwrap();
    assert_eq!(s.coding_type(), StreamCodingType::MvcDependentVideo);
    assert_eq!(s.format_info_byte, 0x73);
    let base = clip("mvc-3d", "00000");
    assert!(base.extension.program_info_ss.is_none());
    assert!(base.extension.entry(2, 4).is_some());
}

#[test]
fn split_3d_sub_play_items_are_a_sequence_selected_by_sync_play_item() {
    let pl = playlist("mvc-3d-split");
    assert_eq!(pl.play_list.play_items.len(), 2);
    let ext = &pl.extension;
    assert_eq!(ext.stn_table_ss.len(), 2);
    let sp = &ext.sub_paths[0];
    assert_eq!(sp.sub_play_items.len(), 2);
    let want = [("00001", 0usize), ("00003", 1usize)];
    for (pi_idx, ss) in ext.stn_table_ss.iter().enumerate() {
        let dv = ss.dependent_view.unwrap();
        // Same entry on both PlayItems — entry id 0 indexes within a
        // SubPlayItem, not across them.
        assert_eq!(dv.entry, StreamEntry::sub_clip(0, 0, 0x1012));
        assert_eq!(ss.trailing, vec![0x00, 0x01, 0x00, 0x00]);
        let r = pl
            .resolve_stream_entry(pi_idx, &dv.entry, SubPathList::Extension)
            .unwrap();
        assert_eq!(
            (r.clip_information_file_name.as_str(), r.sub_play_item_index),
            (want[pi_idx].0, Some(want[pi_idx].1))
        );
    }
    assert_eq!(
        sp.sub_play_items[1].connection_condition.as_raw(),
        6,
        "second SubPlayItem is a seamless new-STC join"
    );
    // The sequence tiles the title timeline: SubPlayItem k starts where
    // PlayItem k starts.
    let pi0 = pl.play_list.play_items[0].duration_90k();
    let w0 = pl
        .sub_play_item_sync_window_in(SubPathList::Extension, 0, 0)
        .unwrap();
    let w1 = pl
        .sub_play_item_sync_window_in(SubPathList::Extension, 0, 1)
        .unwrap();
    assert_eq!((w0.title_start_pts_90k, w0.title_end_pts_90k), (0, pi0));
    assert_eq!(w1.title_start_pts_90k, pi0);
    assert_eq!(w1.title_end_pts_90k, pl.duration_90k());
    assert!(pl.sub_play_item_sync_window(0, 0).is_none());
    for stem in ["00001", "00003"] {
        let c = clip("mvc-3d-split", stem);
        assert_eq!(c.clip_info.application_type, 8);
        assert!(
            c.extension
                .program_info_ss
                .as_ref()
                .and_then(|p| p.stream_by_pid(0x1012))
                .is_some(),
            "{stem}"
        );
    }
}

#[test]
fn multi_clip_alternates_are_selected_by_sub_clip_entry_id() {
    let pl = playlist("multiclip-subplayitem");
    let sa = &pl.play_list.play_items[0].stn_table.secondary_audio[0];
    assert_eq!(sa.entry, StreamEntry::sub_clip(1, 2, 0x1A00));
    let sp = &pl.play_list.sub_paths[1];
    assert_eq!(sp.kind(), SubPathType::OutOfMuxSynchronous);
    let spi = &sp.sub_play_items[0];
    assert_eq!(spi.num_clips(), 3);
    let names: Vec<(String, u8)> = (0..3)
        .map(|id| {
            let c = spi.clip_entry(id).unwrap();
            (c.clip_information_file_name, c.stc_id_ref)
        })
        .collect();
    assert_eq!(
        names,
        vec![
            ("00010".to_string(), 0),
            ("00011".to_string(), 1),
            ("00012".to_string(), 2),
        ]
    );
    assert!(spi.clip_entry(3).is_none());

    let r = pl
        .resolve_stream_entry(0, &sa.entry, SubPathList::PlayList)
        .unwrap();
    assert_eq!(r.clip_information_file_name, "00012");
    assert_eq!(r.stc_id_ref, 2);
    assert_eq!(r.pid, 0x1A00);
    assert_eq!(r.sub_path_index, Some(1));
    // Every alternate shares the one IN/OUT window.
    assert_eq!(
        (spi.in_time_ticks, spi.out_time_ticks),
        (27_000_000, 27_180_180)
    );
    // Selecting another alternate is just a different entry id.
    let alt = StreamEntry::sub_clip(1, 1, 0x1A00);
    let r1 = pl
        .resolve_stream_entry(0, &alt, SubPathList::PlayList)
        .unwrap();
    assert_eq!(r1.clip_information_file_name, "00011");
    let bad = StreamEntry::sub_clip(1, 3, 0x1A00);
    assert!(
        pl.resolve_stream_entry(0, &bad, SubPathList::PlayList)
            .is_none()
    );
}
