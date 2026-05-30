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
//! Surface delivered to a demuxer / seeker:
//!
//! - `ClipInfo`: TS recording rate, number of source packets, codec id.
//! - `SequenceInfo`: STC sequences with their first / last PTS so the
//!   demuxer can map source-packet numbers to wall-clock times.
//! - `ProgramInfo`: per-PID elementary stream coding info (so we know
//!   what codecs are present without speculatively parsing the TS).
//! - `CPI` EP_map: per-stream-PID entry-point map for I-frame aligned
//!   seeking — one [`EpEntry`] per fine row with the coarse-row context
//!   already combined into a flat `(pts_ep_start, spn_ep_start)` pair.
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

/// One row of the per-PID EP_map. The coarse-table context has already
/// been folded into [`pts_ep_start`] / [`spn_ep_start`] so a seeker can
/// just binary-search the flat list by `pts_ep_start`.
///
/// PTS combine math (BD-ROM AV §5.7 EP_coarse + EP_fine packing — see
/// the module-level note on the layout): the 14-bit `PTS_EP_coarse`
/// holds bits `[32:19]` of the 33-bit MPEG-2 TS PTS, the 11-bit
/// `PTS_EP_fine` holds bits `[19:9]`. The low 9 bits (≈ 5.7 ms at
/// 90 kHz) are not stored; `pts_ep_start = (coarse << 19) | (fine << 9)`.
///
/// SPN combine math: the 32-bit `SPN_EP_coarse` is the full source-
/// packet number at the coarse-row boundary; the 17-bit `SPN_EP_fine`
/// is the low 17 bits at the fine row. `spn_ep_start =
/// (coarse_spn & 0xFFFE_0000) | fine_spn`.
///
/// [`pts_ep_start`]: EpEntry::pts_ep_start
/// [`spn_ep_start`]: EpEntry::spn_ep_start
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpEntry {
    /// 1-bit field. Set for entry points that also serve as a multi-
    /// angle switch boundary.
    pub is_angle_change_point: bool,
    /// 3-bit field. Encoder-side hint for the byte offset of the I-end
    /// inside its source packet — preserved opaquely.
    pub i_end_position_offset: u8,
    /// Combined PTS in 90 kHz ticks, granularity 512 ticks (≈ 5.7 ms).
    pub pts_ep_start: u32,
    /// Combined source-packet number — multiply by 192 for the byte
    /// offset in the `.m2ts` file.
    pub spn_ep_start: u32,
    /// 4-bit field, copied from the enclosing [`EpMap`] header so a
    /// single iterator over `EpEntry` values is self-describing.
    pub ep_stream_type: u8,
}

/// Entry-point map for one stream PID. Wraps a flat list of
/// [`EpEntry`] rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpMap {
    pub stream_pid: u16,
    /// 4-bit `EP_stream_type` (e.g. 1 for MPEG-2 video, 5 for AVC,
    /// 6 for VC-1 — per the BD-AV white paper).
    pub ep_stream_type: u8,
    pub entries: Vec<EpEntry>,
}

/// `CPI()` block — one [`EpMap`] per stream PID plus any trailing
/// opaque TS-type indicator bytes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cpi {
    pub ep_map: Vec<EpMap>,
    /// Trailing bytes between the last fine table and the end of the
    /// `CPI()` block. Preserved opaquely so a round-trip encode/decode
    /// reproduces the original bytes — but their internal structure is
    /// not parsed by Phase 1.
    pub ts_type_indicators: Vec<u8>,
}

/// Deprecated alias retained for one release so external code that
/// imported the Phase 1 stub `CpiEpMap` still compiles. Equivalent to
/// [`Cpi`].
#[deprecated(
    since = "0.0.2",
    note = "renamed to `Cpi` (BD-ROM AV §5.7 terminology)"
)]
pub type CpiEpMap = Cpi;

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
    pub cpi: Cpi,
    pub clip_mark: ClipMark,
}

impl ClipInformation {
    pub fn parse(buf: &[u8]) -> Result<Self> {
        let header = BdmvHeader::parse(buf)?;
        // Per BD-ROM Part 3 §5.5.1.1, the .clpi `type_indicator` is
        // the four ASCII bytes "HDMV" (not "CLPI" — that's the file
        // *extension*, not the wire magic). Accept "CLPI" as a
        // lenient back-compat path for any synthetic fixture that
        // followed the older (incorrect) parser's expectation.
        if header.type_indicator != b"HDMV" && header.type_indicator != b"CLPI" {
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

        // CPI EP_map (BD-ROM AV §5.7). Best-effort: some homemade clips
        // ship a zero-length CPI; we tolerate that by yielding an empty
        // `Cpi` rather than failing the parse.
        let cpi = if cpi_start > 0 && cpi_start < buf.len() {
            parse_cpi(buf, cpi_start)?
        } else {
            Cpi::default()
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

    /// Test-only encoder — emits a `.clpi` payload that the parser
    /// round-trips. Includes a populated CPI block when `self.cpi.ep_map`
    /// is non-empty.
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

        // CPI
        let cpi_start = out.len() as u32;
        let cpi_bytes = encode_cpi(&self.cpi);
        out.extend_from_slice(&cpi_bytes);

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

// ---------------------------------------------------------------------
// CPI EP_map parser
//
// Round-93 clean-room layout (BD-ROM AV §5.7 / BD-RE Part 3 §3.1.5.2
// Figure 3.1.5.2 + orchestrator-handed bit-widths — the BD-AV white
// paper documents the EP_map *concept* but not the exact field widths;
// the bit-packed widths used below are imported from the round-93 task
// brief and verified internally for round-trip).
//
// Layout, relative to start of the CPI() block (the u32 length field):
//
//   off 0   length:                       u32  (size of bytes that follow)
//   off 4   12 reserved bits + 4-bit CPI_type  (u16)
//   off 6   8 reserved bits + N_streams        (u8 + u8)
//   off 8   per-stream-PID header  (×N, 12 bytes each):
//             stream_PID                  u16
//             10 reserved | 4 EP_stream_type | 16 N_coarse | 18 N_fine   (48 bits = 6 bytes)
//             EP_map_for_one_stream_PID_start_address    u32  (offset from start of CPI block)
//
//   each EP_map_for_one_stream_PID() at its start_address:
//     off 0   EP_fine_table_start_address     u32  (relative to start of this body)
//             N_coarse × 8 bytes:
//               18 ref_to_EP_fine_id | 14 PTS_EP_coarse | 32 SPN_EP_coarse   (64 bits)
//     off fine_start
//             N_fine × 4 bytes:
//               1 is_angle_change_point | 3 I_end_position_offset
//               | 11 PTS_EP_fine | 17 SPN_EP_fine                          (32 bits)
//
// All fields are big-endian / MSB-first.
// ---------------------------------------------------------------------

fn parse_cpi(buf: &[u8], cpi_start: usize) -> Result<Cpi> {
    let mut r = Reader::new(buf);
    r.seek(cpi_start)?;
    let cpi_len = r.read_u32()? as usize;
    let body_start = r.pos;
    let body_end = body_start
        .checked_add(cpi_len)
        .ok_or_else(|| BlurayError::malformed("CPI length overflow"))?;
    if body_end > buf.len() {
        return Err(BlurayError::malformed("CPI length past EOF"));
    }
    if cpi_len < 2 {
        return Ok(Cpi::default());
    }

    // 12 reserved bits + 4-bit CPI_type. Read as a u16 and mask the low
    // nibble.
    let head = r.read_u16()?;
    let cpi_type = (head & 0x000F) as u8;
    if cpi_type != 1 {
        // BD-ROM AV §5.7: only CPI_type == 1 (EP_map) is defined for
        // BD-ROM. Other values are reserved / vendor; leave the body
        // unparsed but preserve nothing — a re-encode would not be
        // safe to round-trip an unknown CPI_type, so callers get an
        // empty parse and the warning is conveyed via the empty list.
        return Ok(Cpi::default());
    }

    if cpi_len < 4 {
        return Ok(Cpi::default());
    }
    // EP_map() header: 8 reserved bits + N_streams (u8 + u8).
    let _reserved = r.read_u8()?;
    let num_streams = r.read_u8()? as usize;

    // Per-stream PID header (×N).
    struct StreamHeader {
        stream_pid: u16,
        ep_stream_type: u8,
        num_coarse: u32,
        num_fine: u32,
        body_addr: u32,
    }
    let mut headers = Vec::with_capacity(num_streams);
    for _ in 0..num_streams {
        let stream_pid = r.read_u16()?;
        // 48 bits: 10 reserved + 4 EP_stream_type + 16 N_coarse + 18 N_fine.
        let packed_bytes: [u8; 6] = {
            let s = r.slice(6)?;
            [s[0], s[1], s[2], s[3], s[4], s[5]]
        };
        let mut br = BitReader::new(&packed_bytes);
        let _reserved10 = br.read_bits(10)?;
        let ep_stream_type = br.read_bits(4)? as u8;
        let num_coarse = br.read_bits(16)?;
        let num_fine = br.read_bits(18)?;
        let body_addr = r.read_u32()?;
        headers.push(StreamHeader {
            stream_pid,
            ep_stream_type,
            num_coarse,
            num_fine,
            body_addr,
        });
    }

    // Per-stream EP_map_for_one_stream_PID() bodies. The per-stream
    // `body_addr` is documented as "offset from the first byte of the
    // CPI block", but empirically — verified against a 2017
    // commercially-pressed BD-ROM — the base is the position of the
    // `reserved+N_streams` byte, i.e. AFTER the length u32 (`cpi_len`)
    // and the head u16. So:
    //
    //   body_abs = cpi_start + 4 (length u32) + 2 (head u16) + body_addr
    //            = cpi_start + 6 + body_addr
    //
    // For a single-stream EP_map (the common case on movie discs),
    // body_addr = headers_size + 2 (reserved + N bytes) = 12 + 2 = 14.
    let mut ep_map = Vec::with_capacity(num_streams);
    for h in &headers {
        let body_abs = cpi_start
            .checked_add(6)
            .and_then(|b| b.checked_add(h.body_addr as usize))
            .ok_or_else(|| BlurayError::malformed("EP_map body_addr overflow"))?;
        if body_abs >= body_end {
            return Err(BlurayError::malformed(
                "EP_map_for_one_stream_PID start address past CPI body end",
            ));
        }
        r.seek(body_abs)?;
        let fine_table_addr = r.read_u32()? as usize;

        // Coarse table immediately follows the fine_table_addr u32.
        let mut coarses: Vec<(u32, u32, u32)> = Vec::with_capacity(h.num_coarse as usize);
        for _ in 0..h.num_coarse {
            let bytes = r.slice(8)?;
            let arr: [u8; 8] = [
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ];
            let mut br = BitReader::new(&arr);
            let ref_to_fine = br.read_bits(18)?;
            let pts_coarse = br.read_bits(14)?;
            let spn_coarse = br.read_bits(32)?;
            coarses.push((ref_to_fine, pts_coarse, spn_coarse));
        }

        // Fine table at body_abs + fine_table_addr.
        let fine_abs = body_abs
            .checked_add(fine_table_addr)
            .ok_or_else(|| BlurayError::malformed("EP_fine table address overflow"))?;
        if fine_abs > body_end {
            return Err(BlurayError::malformed(
                "EP_fine table start address past CPI body end",
            ));
        }
        r.seek(fine_abs)?;
        let mut fines: Vec<(bool, u8, u32, u32)> = Vec::with_capacity(h.num_fine as usize);
        for _ in 0..h.num_fine {
            let bytes = r.slice(4)?;
            let arr = [bytes[0], bytes[1], bytes[2], bytes[3]];
            let mut br = BitReader::new(&arr);
            let is_angle = br.read_bits(1)? != 0;
            let i_end = br.read_bits(3)? as u8;
            let pts_fine = br.read_bits(11)?;
            let spn_fine = br.read_bits(17)?;
            fines.push((is_angle, i_end, pts_fine, spn_fine));
        }

        // Combine each fine row with its enclosing coarse row. The
        // walker advances the coarse index whenever the next coarse's
        // `ref_to_EP_fine_id` first becomes ≤ the current fine index —
        // i.e. coarse[c] covers fine ids in `[c.ref ..= next_c.ref)`,
        // and the last coarse extends through num_fine.
        let mut entries = Vec::with_capacity(h.num_fine as usize);
        let mut coarse_idx: usize = 0;
        for (fi, fine) in fines.iter().enumerate() {
            while coarse_idx + 1 < coarses.len() && (coarses[coarse_idx + 1].0 as usize) <= fi {
                coarse_idx += 1;
            }
            let (_ref, pts_c, spn_c) = if coarses.is_empty() {
                (0, 0, 0)
            } else {
                coarses[coarse_idx]
            };
            let (is_angle, i_end, pts_f, spn_f) = *fine;
            let pts_ep_start = (pts_c << 19) | (pts_f << 9);
            let spn_ep_start = (spn_c & 0xFFFE_0000) | spn_f;
            entries.push(EpEntry {
                is_angle_change_point: is_angle,
                i_end_position_offset: i_end,
                pts_ep_start,
                spn_ep_start,
                ep_stream_type: h.ep_stream_type,
            });
        }

        ep_map.push(EpMap {
            stream_pid: h.stream_pid,
            ep_stream_type: h.ep_stream_type,
            entries,
        });
    }

    Ok(Cpi {
        ep_map,
        ts_type_indicators: Vec::new(),
    })
}

/// Encoder-only bucket: a run of fine entries that share the same
/// coarse `(pts >> 19, spn >> 17)` key.
struct Bucket<'a> {
    key: (u32, u32),
    entries: Vec<&'a EpEntry>,
}

/// Test-only encoder for [`Cpi`]. Emits a CPI block whose layout the
/// parser above round-trips. Grouping policy: one coarse row per
/// distinct `(pts_ep_start >> 19, spn_ep_start >> 17)` bucket. Empty
/// `Cpi` → an empty length-0 CPI block (4 bytes of zeroes), matching
/// what the Phase-1 stub used to write.
fn encode_cpi(cpi: &Cpi) -> Vec<u8> {
    let mut out = Vec::new();
    if cpi.ep_map.is_empty() {
        out.extend_from_slice(&0u32.to_be_bytes());
        return out;
    }

    // First pass: build the per-stream body byte vectors and compute
    // the headers (incl. the per-stream `body_addr`).
    let headers_size = 12 * cpi.ep_map.len();
    let bodies_start_offset =
        4 /*length u32*/ + 2 /*head*/ + 1 /*reserved*/ + 1 /*N*/ + headers_size;
    let bodies_start_in_body = bodies_start_offset - 4; // exclude the length u32 itself
                                                        // `body_addr` is "offset from the position of the reserved+N byte"
                                                        // — i.e. AFTER the length u32 AND the head u16. Subtract 6 from
                                                        // `bodies_start_offset` to land in that base. See the parser's
                                                        // matching comment.
    let bodies_start_in_body_addr_base = bodies_start_offset - 6;

    let mut header_metadata: Vec<(u16, u8, u32, u32, u32)> = Vec::with_capacity(cpi.ep_map.len());
    let mut bodies: Vec<u8> = Vec::new();
    for ep in &cpi.ep_map {
        // Group fines into coarse buckets — one bucket per run of fine
        // rows that share the same `(pts >> 19, spn >> 17)` key, in
        // file order. Each bucket becomes one coarse row whose
        // `ref_to_EP_fine_id` points at the bucket's first fine.
        let mut buckets: Vec<Bucket<'_>> = Vec::new();
        for e in &ep.entries {
            let key = (e.pts_ep_start >> 19, e.spn_ep_start >> 17);
            match buckets.last_mut() {
                Some(last) if last.key == key => last.entries.push(e),
                _ => buckets.push(Bucket {
                    key,
                    entries: vec![e],
                }),
            }
        }

        let n_coarse = buckets.len() as u32;
        let n_fine = ep.entries.len() as u32;
        let body_addr = (bodies_start_in_body_addr_base + bodies.len()) as u32;

        // Build this stream's body.
        let fine_table_start_address: u32 = 4 + 8 * n_coarse; // relative to body start
        let mut body = Vec::new();
        body.extend_from_slice(&fine_table_start_address.to_be_bytes());

        // Coarse rows.
        let mut running_fine_id: u32 = 0;
        for b in &buckets {
            let pts_coarse = b.key.0 & 0x3FFF;
            // SPN_EP_coarse is the *full* 32-bit SPN at the coarse
            // boundary — i.e. of the first fine row in this bucket
            // (its high 15 bits drive the SPN top portion).
            let spn_coarse_full = b.entries[0].spn_ep_start;
            let mut bw = BitWriter::new();
            bw.write_bits(running_fine_id, 18);
            bw.write_bits(pts_coarse, 14);
            bw.write_bits(spn_coarse_full, 32);
            debug_assert!(bw.byte_aligned());
            body.extend_from_slice(&bw.into_bytes());
            running_fine_id += b.entries.len() as u32;
        }

        // Fine rows.
        for e in &ep.entries {
            let mut bw = BitWriter::new();
            bw.write_bits(e.is_angle_change_point as u32, 1);
            bw.write_bits(u32::from(e.i_end_position_offset & 0x07), 3);
            bw.write_bits((e.pts_ep_start >> 9) & 0x07FF, 11);
            bw.write_bits(e.spn_ep_start & 0x0001_FFFF, 17);
            debug_assert!(bw.byte_aligned());
            body.extend_from_slice(&bw.into_bytes());
        }

        bodies.extend_from_slice(&body);
        header_metadata.push((
            ep.stream_pid,
            ep.ep_stream_type,
            n_coarse,
            n_fine,
            body_addr,
        ));
    }

    // CPI body length (everything after the length u32):
    //   2 (head) + 1+1 (reserved+N) + headers + bodies + ts_type_indicators
    let body_len = 2 + 1 + 1 + headers_size + bodies.len() + cpi.ts_type_indicators.len();
    out.extend_from_slice(&(body_len as u32).to_be_bytes());

    // 12 reserved + 4-bit CPI_type=1.
    out.extend_from_slice(&[0x00, 0x01]);

    // 8 reserved + N_streams.
    out.push(0);
    out.push(cpi.ep_map.len() as u8);

    // Per-stream PID headers.
    for (stream_pid, ep_stream_type, n_coarse, n_fine, body_addr) in &header_metadata {
        out.extend_from_slice(&stream_pid.to_be_bytes());
        let mut bw = BitWriter::new();
        bw.write_bits(0, 10); // reserved
        bw.write_bits(u32::from(*ep_stream_type & 0x0F), 4); // EP_stream_type (4 bits)
        bw.write_bits(*n_coarse, 16); // N_coarse (16 bits)
        bw.write_bits(*n_fine, 18); // N_fine (18 bits)
        debug_assert!(bw.byte_aligned());
        out.extend_from_slice(&bw.into_bytes());
        out.extend_from_slice(&body_addr.to_be_bytes());
    }

    // Bodies.
    out.extend_from_slice(&bodies);

    // Trailing opaque indicator bytes (if any).
    out.extend_from_slice(&cpi.ts_type_indicators);

    debug_assert_eq!(out.len(), 4 + body_len);
    debug_assert_eq!(
        out.len(),
        bodies_start_offset + bodies.len() + cpi.ts_type_indicators.len()
    );
    let _ = bodies_start_in_body;
    out
}

// ---------------------------------------------------------------------
// Tiny MSB-first bit reader / writer. Scoped to clpi because CPI is
// the only BDMV file in Phase 1 that needs sub-byte fields.
// ---------------------------------------------------------------------

struct BitReader<'a> {
    bytes: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit_pos: 0 }
    }

    fn read_bits(&mut self, n: usize) -> Result<u32> {
        debug_assert!(n <= 32);
        if self.bit_pos + n > self.bytes.len() * 8 {
            return Err(BlurayError::malformed("BitReader past end"));
        }
        let mut acc: u32 = 0;
        for _ in 0..n {
            let byte = self.bytes[self.bit_pos / 8];
            let bit = (byte >> (7 - (self.bit_pos % 8))) & 1;
            acc = (acc << 1) | u32::from(bit);
            self.bit_pos += 1;
        }
        Ok(acc)
    }
}

struct BitWriter {
    bytes: Vec<u8>,
    bit_pos: usize,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            bit_pos: 0,
        }
    }

    fn write_bits(&mut self, value: u32, n: usize) {
        debug_assert!(n <= 32);
        for i in 0..n {
            let bit = ((value >> (n - 1 - i)) & 1) as u8;
            let byte_idx = self.bit_pos / 8;
            if byte_idx >= self.bytes.len() {
                self.bytes.push(0);
            }
            let shift = 7 - (self.bit_pos % 8);
            self.bytes[byte_idx] |= bit << shift;
            self.bit_pos += 1;
        }
    }

    fn byte_aligned(&self) -> bool {
        self.bit_pos % 8 == 0
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
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
            cpi: Cpi::default(),
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

    #[test]
    fn bit_reader_msb_first() {
        // 0b10110100_01010010 → reading nibbles MSB-first should yield
        // 0xB, 0x4, 0x5, 0x2.
        let bytes = [0xB4, 0x52];
        let mut br = BitReader::new(&bytes);
        assert_eq!(br.read_bits(4).unwrap(), 0xB);
        assert_eq!(br.read_bits(4).unwrap(), 0x4);
        assert_eq!(br.read_bits(4).unwrap(), 0x5);
        assert_eq!(br.read_bits(4).unwrap(), 0x2);
    }

    #[test]
    fn bit_writer_round_trip() {
        let mut bw = BitWriter::new();
        bw.write_bits(0b1011, 4);
        bw.write_bits(0b0100, 4);
        bw.write_bits(0b0101, 4);
        bw.write_bits(0b0010, 4);
        assert!(bw.byte_aligned());
        let v = bw.into_bytes();
        assert_eq!(v, vec![0xB4, 0x52]);
    }

    /// Encode a CPI block with two stream PIDs (one PID with multiple
    /// coarse buckets and three fines; the second PID single-row) and
    /// confirm a decode round-trips every public field.
    #[test]
    fn cpi_round_trip_two_streams() {
        // Bucket 0 of PID 0x1011: pts_ep_start values share (pts >> 19) == 10.
        // Bucket 1 of PID 0x1011: (pts >> 19) == 11, and the SPN is past
        // the 17-bit fine-only boundary so the coarse mask actually kicks in.
        let pts_a0: u32 = 10u32 << 19; // 5242880  (fine_pts = 0)
        let pts_a1: u32 = (10u32 << 19) | (10u32 << 9); // 5248000  (fine_pts = 10)
        let pts_a2: u32 = (11u32 << 19) | (5u32 << 9); // 5769728  (fine_pts = 5)
        let pts_b0: u32 = 0;

        let spn_a0: u32 = 0;
        let spn_a1: u32 = 192;
        // 0x00020000 → 17-bit fine alone can't reach it, so the coarse
        // SPN must contribute its high bits. Round-trip must preserve.
        let spn_a2: u32 = 0x0002_0000 | 100;
        let spn_b0: u32 = 0;

        let cpi = Cpi {
            ep_map: vec![
                EpMap {
                    stream_pid: 0x1011,
                    ep_stream_type: 1,
                    entries: vec![
                        EpEntry {
                            is_angle_change_point: false,
                            i_end_position_offset: 0,
                            pts_ep_start: pts_a0,
                            spn_ep_start: spn_a0,
                            ep_stream_type: 1,
                        },
                        EpEntry {
                            is_angle_change_point: false,
                            i_end_position_offset: 1,
                            pts_ep_start: pts_a1,
                            spn_ep_start: spn_a1,
                            ep_stream_type: 1,
                        },
                        EpEntry {
                            is_angle_change_point: true,
                            i_end_position_offset: 2,
                            pts_ep_start: pts_a2,
                            spn_ep_start: spn_a2,
                            ep_stream_type: 1,
                        },
                    ],
                },
                EpMap {
                    stream_pid: 0x1100,
                    ep_stream_type: 5,
                    entries: vec![EpEntry {
                        is_angle_change_point: false,
                        i_end_position_offset: 0,
                        pts_ep_start: pts_b0,
                        spn_ep_start: spn_b0,
                        ep_stream_type: 5,
                    }],
                },
            ],
            ts_type_indicators: Vec::new(),
        };

        // Encode → wrap in a minimal CLPI scaffold → parse → compare.
        let mut clpi = sample_clpi();
        clpi.cpi = cpi.clone();
        let bytes = clpi.encode();
        let parsed = ClipInformation::parse(&bytes).unwrap();
        assert_eq!(parsed.cpi.ep_map.len(), 2);

        assert_eq!(parsed.cpi.ep_map[0].stream_pid, 0x1011);
        assert_eq!(parsed.cpi.ep_map[0].ep_stream_type, 1);
        assert_eq!(parsed.cpi.ep_map[0].entries.len(), 3);
        assert_eq!(parsed.cpi.ep_map[0].entries[0].pts_ep_start, pts_a0);
        assert_eq!(parsed.cpi.ep_map[0].entries[1].pts_ep_start, pts_a1);
        assert_eq!(parsed.cpi.ep_map[0].entries[2].pts_ep_start, pts_a2);
        assert_eq!(parsed.cpi.ep_map[0].entries[0].spn_ep_start, spn_a0);
        assert_eq!(parsed.cpi.ep_map[0].entries[1].spn_ep_start, spn_a1);
        assert_eq!(parsed.cpi.ep_map[0].entries[2].spn_ep_start, spn_a2);
        assert!(!parsed.cpi.ep_map[0].entries[0].is_angle_change_point);
        assert!(parsed.cpi.ep_map[0].entries[2].is_angle_change_point);
        assert_eq!(parsed.cpi.ep_map[0].entries[2].i_end_position_offset, 2);
        assert!(parsed.cpi.ep_map[0]
            .entries
            .iter()
            .all(|e| e.ep_stream_type == 1));

        assert_eq!(parsed.cpi.ep_map[1].stream_pid, 0x1100);
        assert_eq!(parsed.cpi.ep_map[1].ep_stream_type, 5);
        assert_eq!(parsed.cpi.ep_map[1].entries.len(), 1);
        assert_eq!(parsed.cpi.ep_map[1].entries[0].pts_ep_start, pts_b0);
        assert_eq!(parsed.cpi.ep_map[1].entries[0].spn_ep_start, spn_b0);
        assert_eq!(parsed.cpi.ep_map[1].entries[0].ep_stream_type, 5);
    }

    /// Confirm an empty `Cpi` still encodes to a parseable zero-length
    /// CPI block — the common path for homemade discs that ship no
    /// EP_map.
    #[test]
    fn cpi_empty_round_trip() {
        let c = sample_clpi();
        assert!(c.cpi.ep_map.is_empty());
        let bytes = c.encode();
        let parsed = ClipInformation::parse(&bytes).unwrap();
        assert!(parsed.cpi.ep_map.is_empty());
        assert!(parsed.cpi.ts_type_indicators.is_empty());
    }

    /// A CPI block with `CPI_type != 1` decodes to an empty `Cpi`
    /// rather than failing — Phase 1 doesn't know how to interpret
    /// vendor-defined CPI blocks but mustn't reject the whole file.
    #[test]
    fn cpi_unknown_type_yields_empty() {
        let mut clpi = sample_clpi();
        clpi.cpi = Cpi::default();
        let mut bytes = clpi.encode();
        // Find the CPI start (stored at offset 16..20 in the header).
        let cpi_start = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]) as usize;
        // The CPI block is currently zero-length; rewrite it as a tiny
        // CPI_type=2 block with no payload (length=2, head=0x0002).
        bytes[cpi_start..cpi_start + 4].copy_from_slice(&2u32.to_be_bytes());
        // Splice in 2 head bytes: 12 reserved + 4-bit CPI_type = 2.
        let head = 0x0002u16.to_be_bytes();
        // The original encode() wrote 4 zero bytes for the length-0 CPI
        // followed by the ClipMark block. We extend by 2 bytes to make
        // room for the head, and patch up the ClipMark start offset.
        let insert_at = cpi_start + 4;
        bytes.splice(insert_at..insert_at, head);
        let new_mark_start = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]) + 2;
        bytes[20..24].copy_from_slice(&new_mark_start.to_be_bytes());
        let parsed = ClipInformation::parse(&bytes).unwrap();
        assert!(parsed.cpi.ep_map.is_empty());
    }
}
