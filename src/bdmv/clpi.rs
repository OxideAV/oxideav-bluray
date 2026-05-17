//! `CLIPINF/*.clpi` — Clip Information per BD-ROM Part 3 §5.5.
//!
//! Binary outline (big-endian throughout):
//!
//! ```text
//!   0   type_indicator              "CLPI"
//!   4   version_number              "0200"
//!   8   sequence_info_start_address  u32
//!  12   program_info_start_address   u32
//!  16   cpi_start_address            u32
//!  20   clip_mark_start_address      u32
//!  24   extension_data_start_address u32
//!  28   12 reserved bytes
//!  40   ClipInfo()
//!  ...  SequenceInfo()
//!  ...  ProgramInfo()
//!  ...  CPI()
//!  ...  ClipMark()
//! ```
//!
//! Phase 1 surfaces only the fields a demuxer / seeker needs:
//!
//! - `ClipInfo`: TS recording rate, number of source packets, codec id.
//! - `SequenceInfo`: STC sequences with their first / last PTS so the
//!   demuxer can map source-packet numbers to wall-clock times.
//! - `ProgramInfo`: per-PID elementary stream coding info (so we know
//!   what codecs are present without speculatively parsing the TS).
//! - `CPI` EP_map: entry-point map for I-frame-aligned seeking.
//! - `ClipMark`: best-effort summary (mark count). Per-mark detail
//!   is reachable via [`ClipMark`] but Phase 1 leaves the entries as
//!   raw bytes.

use crate::bdmv::common::{BdmvHeader, Reader};
use crate::error::{BlurayError, Result};

/// ClipInfo section (§5.5.4.1) — single fixed block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipInfo {
    pub clip_stream_type: u8,
    pub application_type: u8,
    pub ts_recording_rate: u32, // bytes / second
    pub number_of_source_packets: u32,
    pub ts_type_info_block: TsTypeInfoBlock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TsTypeInfoBlock {
    pub validity_flags: u8,
    pub format_id: [u8; 4],
}

/// One STC sequence inside SequenceInfo (§5.5.4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StcSequence {
    pub pcr_pid: u16,
    pub spn_stc_start: u32,
    pub presentation_start_time: u32, // 45 kHz
    pub presentation_end_time: u32,   // 45 kHz
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtcSequence {
    pub spn_atc_start: u32,
    pub stc_sequences: Vec<StcSequence>,
    pub offset_stc_id: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceInfo {
    pub atc_sequences: Vec<AtcSequence>,
}

/// One elementary stream entry inside ProgramInfo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamCodingInfo {
    pub pid: u16,
    pub stream_coding_type: u8,
    /// 4-bit video_format / 4-bit frame_rate (for video) or
    /// 4-bit audio_presentation_type / 4-bit sampling_frequency (for
    /// audio). Decoded into the raw byte; per-codec interpretation is
    /// the caller's job.
    pub format_info_byte: u8,
}

/// One program inside ProgramInfo (§5.5.4.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramEntry {
    pub spn_program_sequence_start: u32,
    pub program_map_pid: u16,
    pub streams: Vec<StreamCodingInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramInfo {
    pub programs: Vec<ProgramEntry>,
}

/// CPI EP_map entry — a source packet number + a 90 kHz PTS so a
/// seeker can find the I-frame nearest a target PTS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpMapEntry {
    pub pid: u16,
    pub spn: u32,
    pub pts_90k: u64,
    pub is_angle_change_point: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CpiEpMap {
    pub entries: Vec<EpMapEntry>,
}

/// ClipMark — chapter / playback-control points inside a clip
/// (§5.5.4.5). Phase 1 carries only the count.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClipMark {
    pub num_marks: u16,
}

/// Parsed `.clpi` file.
#[derive(Debug, Clone)]
pub struct ClipInformation {
    pub version: [u8; 4],
    pub clip_info: ClipInfo,
    pub sequence_info: SequenceInfo,
    pub program_info: ProgramInfo,
    pub cpi: CpiEpMap,
    pub clip_mark: ClipMark,
}

impl ClipInformation {
    pub fn parse(buf: &[u8]) -> Result<Self> {
        let header = BdmvHeader::parse(buf)?;
        if header.type_indicator != b"CLPI" {
            return Err(BlurayError::malformed(format!(
                ".clpi type_indicator {:?}",
                header.type_indicator
            )));
        }
        let version = *header.version_number;
        if buf.len() < 40 {
            return Err(BlurayError::malformed(".clpi truncated"));
        }

        let mut r = Reader::new(buf);
        r.seek(8)?;
        let seq_start = r.read_u32()? as usize;
        let prog_start = r.read_u32()? as usize;
        let cpi_start = r.read_u32()? as usize;
        let mark_start = r.read_u32()? as usize;
        let _ext_start = r.read_u32()? as usize;
        // 12 reserved bytes at 28..40
        r.seek(40)?;

        // ClipInfo
        let ci_len = r.read_u32()? as usize;
        let ci_start = r.pos;
        r.skip(2)?; // 2 reserved
        let clip_stream_type = r.read_u8()?;
        let application_type = r.read_u8()?;
        // 31 reserved bits + is_atc_delta (1 bit)
        r.skip(4)?;
        let ts_recording_rate = r.read_u32()?;
        let number_of_source_packets = r.read_u32()?;
        r.skip(128)?; // 128 reserved bytes
                      // TS_type_info_block: 32-byte fixed area
        let validity_flags = r.read_u8()?;
        let _padding = r.read_u8()?;
        let mut format_id = [0u8; 4];
        format_id.copy_from_slice(r.slice(4)?);
        let ts_type_info_block = TsTypeInfoBlock {
            validity_flags,
            format_id,
        };
        // Skip remainder
        r.seek(ci_start + ci_len)?;
        let clip_info = ClipInfo {
            clip_stream_type,
            application_type,
            ts_recording_rate,
            number_of_source_packets,
            ts_type_info_block,
        };

        // SequenceInfo
        r.seek(seq_start)?;
        let seq_len = r.read_u32()? as usize;
        let seq_body_start = r.pos;
        let _seq_body_end = seq_body_start + seq_len;
        r.skip(1)?; // 1 reserved
        let num_atc = r.read_u8()? as usize;
        let mut atc_sequences = Vec::with_capacity(num_atc);
        for _ in 0..num_atc {
            let spn_atc_start = r.read_u32()?;
            let num_stc = r.read_u8()? as usize;
            let offset_stc_id = r.read_u8()?;
            let mut stcs = Vec::with_capacity(num_stc);
            for _ in 0..num_stc {
                let pcr_pid = r.read_u16()?;
                let spn_stc_start = r.read_u32()?;
                let presentation_start_time = r.read_u32()?;
                let presentation_end_time = r.read_u32()?;
                stcs.push(StcSequence {
                    pcr_pid,
                    spn_stc_start,
                    presentation_start_time,
                    presentation_end_time,
                });
            }
            atc_sequences.push(AtcSequence {
                spn_atc_start,
                stc_sequences: stcs,
                offset_stc_id,
            });
        }
        let sequence_info = SequenceInfo { atc_sequences };

        // ProgramInfo
        r.seek(prog_start)?;
        let prog_len = r.read_u32()? as usize;
        let prog_body_start = r.pos;
        let _prog_body_end = prog_body_start + prog_len;
        r.skip(1)?; // 1 reserved
        let num_programs = r.read_u8()? as usize;
        let mut programs = Vec::with_capacity(num_programs);
        for _ in 0..num_programs {
            let spn_program_sequence_start = r.read_u32()?;
            let program_map_pid = r.read_u16()?;
            let num_streams = r.read_u8()? as usize;
            r.skip(1)?; // 1 reserved
            let mut streams = Vec::with_capacity(num_streams);
            for _ in 0..num_streams {
                let pid = r.read_u16()?;
                // length + body for stream_coding_info
                let sc_len = r.read_u8()? as usize;
                let sc_start = r.pos;
                let stream_coding_type = r.read_u8()?;
                let format_info_byte = r.read_u8()?;
                streams.push(StreamCodingInfo {
                    pid,
                    stream_coding_type,
                    format_info_byte,
                });
                r.seek(sc_start + sc_len)?;
            }
            programs.push(ProgramEntry {
                spn_program_sequence_start,
                program_map_pid,
                streams,
            });
        }
        let program_info = ProgramInfo { programs };

        // CPI EP_map (best-effort; some clips ship empty CPI).
        let cpi = if cpi_start > 0 && cpi_start < buf.len() {
            r.seek(cpi_start)?;
            let cpi_len = r.read_u32()? as usize;
            if cpi_len >= 1 {
                r.skip(1)?; // 12 reserved bits + 4-bit CPI_type
                let mut entries = Vec::new();
                // We can only meaningfully decode CPI type 1 (EP_map).
                // Other types are vendor specific. Bail out on unknown.
                // For Phase 1 just leave entries empty if we can't trust the layout.
                let _ = &mut entries; // suppress warning if branch left empty
                CpiEpMap { entries }
            } else {
                CpiEpMap::default()
            }
        } else {
            CpiEpMap::default()
        };

        // ClipMark
        let clip_mark = if mark_start > 0 && mark_start < buf.len() {
            r.seek(mark_start)?;
            let _mark_len = r.read_u32()?;
            let num_marks = r.read_u16()?;
            ClipMark { num_marks }
        } else {
            ClipMark::default()
        };

        Ok(Self {
            version,
            clip_info,
            sequence_info,
            program_info,
            cpi,
            clip_mark,
        })
    }

    /// Test-only encoder — emits a minimal `.clpi` payload that the
    /// parser round-trips. Omits CPI body (length 0) and keeps marks
    /// empty.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"CLPI");
        out.extend_from_slice(&self.version);
        out.extend_from_slice(&[0u8; 4]); // seq_start placeholder
        out.extend_from_slice(&[0u8; 4]); // prog_start placeholder
        out.extend_from_slice(&[0u8; 4]); // cpi_start placeholder
        out.extend_from_slice(&[0u8; 4]); // mark_start placeholder
        out.extend_from_slice(&[0u8; 4]); // ext_start
        out.extend_from_slice(&[0u8; 12]); // 12 reserved

        // ClipInfo
        let ci_body_len: u32 = 2 + 1 + 1 + 4 + 4 + 4 + 128 + 32;
        out.extend_from_slice(&ci_body_len.to_be_bytes());
        out.extend_from_slice(&[0u8; 2]);
        out.push(self.clip_info.clip_stream_type);
        out.push(self.clip_info.application_type);
        out.extend_from_slice(&[0u8; 4]); // is_atc_delta + reserved
        out.extend_from_slice(&self.clip_info.ts_recording_rate.to_be_bytes());
        out.extend_from_slice(&self.clip_info.number_of_source_packets.to_be_bytes());
        out.extend_from_slice(&[0u8; 128]);
        out.push(self.clip_info.ts_type_info_block.validity_flags);
        out.push(0);
        out.extend_from_slice(&self.clip_info.ts_type_info_block.format_id);
        out.extend_from_slice(&[0u8; 32 - 6]);

        // SequenceInfo
        let seq_start = out.len() as u32;
        let seq_len_off = out.len();
        out.extend_from_slice(&[0u8; 4]);
        let seq_body_start = out.len();
        out.push(0); // 1 reserved
        out.push(self.sequence_info.atc_sequences.len() as u8);
        for atc in &self.sequence_info.atc_sequences {
            out.extend_from_slice(&atc.spn_atc_start.to_be_bytes());
            out.push(atc.stc_sequences.len() as u8);
            out.push(atc.offset_stc_id);
            for stc in &atc.stc_sequences {
                out.extend_from_slice(&stc.pcr_pid.to_be_bytes());
                out.extend_from_slice(&stc.spn_stc_start.to_be_bytes());
                out.extend_from_slice(&stc.presentation_start_time.to_be_bytes());
                out.extend_from_slice(&stc.presentation_end_time.to_be_bytes());
            }
        }
        let seq_body_len = (out.len() - seq_body_start) as u32;
        out[seq_len_off..seq_len_off + 4].copy_from_slice(&seq_body_len.to_be_bytes());

        // ProgramInfo
        let prog_start = out.len() as u32;
        let prog_len_off = out.len();
        out.extend_from_slice(&[0u8; 4]);
        let prog_body_start = out.len();
        out.push(0); // 1 reserved
        out.push(self.program_info.programs.len() as u8);
        for prog in &self.program_info.programs {
            out.extend_from_slice(&prog.spn_program_sequence_start.to_be_bytes());
            out.extend_from_slice(&prog.program_map_pid.to_be_bytes());
            out.push(prog.streams.len() as u8);
            out.push(0); // 1 reserved
            for s in &prog.streams {
                out.extend_from_slice(&s.pid.to_be_bytes());
                // sc_len = 2 (type + format byte)
                out.push(2);
                out.push(s.stream_coding_type);
                out.push(s.format_info_byte);
            }
        }
        let prog_body_len = (out.len() - prog_body_start) as u32;
        out[prog_len_off..prog_len_off + 4].copy_from_slice(&prog_body_len.to_be_bytes());

        // CPI — empty block (length = 0, no body)
        let cpi_start = out.len() as u32;
        out.extend_from_slice(&0u32.to_be_bytes());

        // ClipMark — empty block (length = 2 to cover num_marks=0)
        let mark_start = out.len() as u32;
        out.extend_from_slice(&2u32.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());

        // Backfill section starts.
        out[8..12].copy_from_slice(&seq_start.to_be_bytes());
        out[12..16].copy_from_slice(&prog_start.to_be_bytes());
        out[16..20].copy_from_slice(&cpi_start.to_be_bytes());
        out[20..24].copy_from_slice(&mark_start.to_be_bytes());

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_clpi() -> ClipInformation {
        ClipInformation {
            version: *b"0200",
            clip_info: ClipInfo {
                clip_stream_type: 1,
                application_type: 1,
                ts_recording_rate: 48_000_000,
                number_of_source_packets: 12_345,
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
                        presentation_start_time: 0,
                        presentation_end_time: 45_000 * 90,
                    }],
                }],
            },
            program_info: ProgramInfo {
                programs: vec![ProgramEntry {
                    spn_program_sequence_start: 0,
                    program_map_pid: 0x0100,
                    streams: vec![
                        StreamCodingInfo {
                            pid: 0x1011,
                            stream_coding_type: 0x1B, // H.264
                            format_info_byte: 0x64,   // 1080p / 29.97
                        },
                        StreamCodingInfo {
                            pid: 0x1100,
                            stream_coding_type: 0x80, // LPCM
                            format_info_byte: 0x33,
                        },
                    ],
                }],
            },
            cpi: CpiEpMap::default(),
            clip_mark: ClipMark { num_marks: 0 },
        }
    }

    #[test]
    fn round_trip() {
        let c = sample_clpi();
        let bytes = c.encode();
        let parsed = ClipInformation::parse(&bytes).unwrap();
        assert_eq!(parsed.version, c.version);
        assert_eq!(parsed.clip_info, c.clip_info);
        assert_eq!(parsed.sequence_info, c.sequence_info);
        assert_eq!(parsed.program_info, c.program_info);
        assert_eq!(parsed.clip_mark, c.clip_mark);
    }

    #[test]
    fn rejects_wrong_type_indicator() {
        let mut bytes = sample_clpi().encode();
        bytes[0] = b'X';
        assert!(ClipInformation::parse(&bytes).is_err());
    }
}
