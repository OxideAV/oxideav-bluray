//! BDAV M2TS source-packet layer.
//!
//! Blu-ray stores each MPEG-2 TS packet as a 192-byte *BDAV source
//! packet*: a 4-byte `TP_extra_header` followed by the standard
//! 188-byte TS packet (BD-ROM Part 3 §5.6.2.1).
//!
//! ```text
//!  byte 0           4                                          192
//!  ┌──────────────┐ ┌───────────────────────────────────────────┐
//!  │ TP_extra (4) │ │       MPEG-TS packet 188-byte             │
//!  └──────────────┘ └───────────────────────────────────────────┘
//! ```
//!
//! `TP_extra_header` is two big-endian bit-fields:
//! - bit 31..30 — `copy_permission_indicator` (2 bits)
//! - bit 29..0  — `arrival_time_stamp` (30 bits, 27 MHz clock truncated
//!   to mod 2³⁰)
//!
//! For Phase 1 we only need to *strip* the header — the existing
//! MPEG-TS demuxer takes it from there. The arrival-time field is
//! parsed and surfaced via [`TpExtraHeader`] for callers that want it
//! (e.g. a future jitter-aware buffer fill estimator); the basic
//! [`strip_tp_extra`] helper discards it.

/// MPEG-TS packet size (ISO/IEC 13818-1).
pub const TS_PACKET_LEN: usize = 188;
/// BDAV TP_extra_header size.
pub const TP_EXTRA_HEADER_LEN: usize = 4;
/// Full BDAV source packet size (header + TS packet).
pub const M2TS_PACKET_LEN: usize = TP_EXTRA_HEADER_LEN + TS_PACKET_LEN;

/// Parsed BDAV `TP_extra_header`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TpExtraHeader {
    /// 2-bit `copy_permission_indicator` (CCI). Values per AACS — we
    /// surface the raw 0..=3.
    pub copy_permission: u8,
    /// 30-bit `arrival_time_stamp` (27 MHz clock truncated mod 2³⁰).
    pub arrival_time: u32,
}

impl TpExtraHeader {
    /// Parse the 4-byte header. Always succeeds — every 4-byte pattern
    /// is a valid header (the field layout is just bit-packed).
    pub fn parse(bytes: &[u8; TP_EXTRA_HEADER_LEN]) -> Self {
        let raw = u32::from_be_bytes(*bytes);
        Self {
            copy_permission: ((raw >> 30) & 0b11) as u8,
            arrival_time: raw & 0x3FFF_FFFF,
        }
    }

    /// Inverse of [`Self::parse`] — used only in tests to forge
    /// synthetic input.
    pub fn encode(self) -> [u8; TP_EXTRA_HEADER_LEN] {
        let raw = ((self.copy_permission as u32 & 0b11) << 30) | (self.arrival_time & 0x3FFF_FFFF);
        raw.to_be_bytes()
    }
}

/// Strip BDAV TP_extra_headers from a buffer of contiguous 192-byte
/// source packets, writing the resulting 188-byte TS packets back
/// into the same allocation in `out`.
///
/// Returns the number of bytes written to `out`. `out` must be large
/// enough to hold `input.len() * 188 / 192` bytes.
///
/// # Errors / panics
///
/// Panics if `input.len()` is not a multiple of 192, or if `out` is
/// smaller than the required output capacity. Both are programmer
/// errors at the call sites inside this crate.
pub fn strip_tp_extra(input: &[u8], out: &mut [u8]) -> usize {
    assert!(
        input.len() % M2TS_PACKET_LEN == 0,
        "input not 192-byte aligned",
    );
    let n_pkts = input.len() / M2TS_PACKET_LEN;
    let need = n_pkts * TS_PACKET_LEN;
    assert!(
        out.len() >= need,
        "output buffer too small: need {need} have {}",
        out.len(),
    );
    for i in 0..n_pkts {
        let src_off = i * M2TS_PACKET_LEN + TP_EXTRA_HEADER_LEN;
        let dst_off = i * TS_PACKET_LEN;
        out[dst_off..dst_off + TS_PACKET_LEN]
            .copy_from_slice(&input[src_off..src_off + TS_PACKET_LEN]);
    }
    need
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        let h = TpExtraHeader {
            copy_permission: 0b10,
            arrival_time: 0x1234_5678,
        };
        let bytes = h.encode();
        let parsed = TpExtraHeader::parse(&bytes);
        assert_eq!(parsed, h);
    }

    #[test]
    fn header_max_values() {
        let h = TpExtraHeader {
            copy_permission: 0b11,
            arrival_time: 0x3FFF_FFFF,
        };
        let parsed = TpExtraHeader::parse(&h.encode());
        assert_eq!(parsed, h);
    }

    #[test]
    fn header_truncates_excess_arrival_time() {
        // 0xFFFF_FFFF should parse as copy=0b11, arrival=0x3FFF_FFFF.
        let parsed = TpExtraHeader::parse(&[0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(parsed.copy_permission, 0b11);
        assert_eq!(parsed.arrival_time, 0x3FFF_FFFF);
    }

    #[test]
    fn strip_one_packet() {
        let mut input = vec![0xAA; M2TS_PACKET_LEN];
        // TS sync byte at offset 4.
        input[4] = 0x47;
        // Fingerprint at offset 8 so we know we copied the right bytes.
        input[8] = 0xCD;
        let mut out = vec![0u8; TS_PACKET_LEN];
        let n = strip_tp_extra(&input, &mut out);
        assert_eq!(n, TS_PACKET_LEN);
        assert_eq!(out[0], 0x47);
        assert_eq!(out[4], 0xCD);
        assert_eq!(out[187], 0xAA);
    }

    #[test]
    fn strip_many_packets() {
        const N: usize = 17;
        let mut input = vec![0u8; N * M2TS_PACKET_LEN];
        for i in 0..N {
            // TP_extra: arbitrary
            input[i * M2TS_PACKET_LEN] = i as u8;
            // TS sync
            input[i * M2TS_PACKET_LEN + TP_EXTRA_HEADER_LEN] = 0x47;
            // Marker
            input[i * M2TS_PACKET_LEN + TP_EXTRA_HEADER_LEN + 1] = i as u8;
        }
        let mut out = vec![0u8; N * TS_PACKET_LEN];
        let n = strip_tp_extra(&input, &mut out);
        assert_eq!(n, N * TS_PACKET_LEN);
        for i in 0..N {
            assert_eq!(out[i * TS_PACKET_LEN], 0x47);
            assert_eq!(out[i * TS_PACKET_LEN + 1], i as u8);
        }
    }

    #[test]
    #[should_panic(expected = "192-byte aligned")]
    fn strip_panics_on_misaligned_input() {
        let input = vec![0u8; 191];
        let mut out = vec![0u8; 188];
        strip_tp_extra(&input, &mut out);
    }

    #[test]
    #[should_panic(expected = "output buffer too small")]
    fn strip_panics_on_short_output() {
        let input = vec![0u8; M2TS_PACKET_LEN];
        let mut out = vec![0u8; 187];
        strip_tp_extra(&input, &mut out);
    }
}
