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

/// Strip BDAV TP_extra_headers and return a freshly-allocated `Vec`
/// holding the resulting 188-byte TS packets back-to-back. Convenience
/// wrapper around [`strip_tp_extra`] for callers that don't have a
/// pre-allocated output buffer.
///
/// # Panics
///
/// Same as [`strip_tp_extra`]: panics if `input.len()` is not a
/// multiple of 192.
pub fn strip_tp_extra_to_vec(input: &[u8]) -> Vec<u8> {
    assert!(
        input.len() % M2TS_PACKET_LEN == 0,
        "input not 192-byte aligned",
    );
    let n_pkts = input.len() / M2TS_PACKET_LEN;
    let mut out = vec![0u8; n_pkts * TS_PACKET_LEN];
    let _ = strip_tp_extra(input, &mut out);
    out
}

/// One BDAV source packet, borrowing into a buffer of contiguous
/// 192-byte packets.
///
/// The 4-byte [`TpExtraHeader`] is parsed eagerly; the 188-byte TS
/// payload is exposed as an opaque borrowed slice — its internal
/// structure (`sync_byte`, PID, adaptation-field, payload) belongs to
/// ISO/IEC 13818-1 and is the downstream MPEG-TS demuxer's job, not
/// this crate's.
#[derive(Debug, Clone, Copy)]
pub struct M2tsSourcePacket<'a> {
    /// Parsed `TP_extra_header` (CCI + 27 MHz arrival timestamp).
    pub tp_extra: TpExtraHeader,
    /// 188-byte TS payload borrowed from the source buffer.
    pub ts_payload: &'a [u8; TS_PACKET_LEN],
}

/// Iterator over BDAV source packets in a buffer of contiguous
/// 192-byte source packets.
///
/// Constructed via [`iter_source_packets`]. Yields one
/// [`M2tsSourcePacket`] per 192-byte chunk; stops cleanly at the end
/// of the buffer. The buffer length must be a multiple of 192 — this
/// is checked once at iterator construction.
#[derive(Debug, Clone)]
pub struct M2tsIter<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> M2tsIter<'a> {
    /// Number of source packets still pending in this iterator.
    pub fn remaining(&self) -> usize {
        (self.buf.len() - self.pos) / M2TS_PACKET_LEN
    }
}

impl<'a> Iterator for M2tsIter<'a> {
    type Item = M2tsSourcePacket<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos + M2TS_PACKET_LEN > self.buf.len() {
            return None;
        }
        let hdr_bytes: &[u8; TP_EXTRA_HEADER_LEN] = self.buf
            [self.pos..self.pos + TP_EXTRA_HEADER_LEN]
            .try_into()
            .expect("4-byte slice");
        let tp_extra = TpExtraHeader::parse(hdr_bytes);
        let ts_start = self.pos + TP_EXTRA_HEADER_LEN;
        let ts_payload: &[u8; TS_PACKET_LEN] = self.buf[ts_start..ts_start + TS_PACKET_LEN]
            .try_into()
            .expect("188-byte slice");
        self.pos += M2TS_PACKET_LEN;
        Some(M2tsSourcePacket {
            tp_extra,
            ts_payload,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let r = self.remaining();
        (r, Some(r))
    }
}

impl ExactSizeIterator for M2tsIter<'_> {}

/// Iterate over a buffer of contiguous 192-byte BDAV source packets.
///
/// Each yielded [`M2tsSourcePacket`] borrows into the input buffer —
/// the iterator does not allocate. The 4-byte [`TpExtraHeader`] is
/// decoded eagerly; the 188-byte TS payload is left opaque.
///
/// # Panics
///
/// Panics if `input.len()` is not a multiple of 192 (a programmer
/// error at every internal call site).
pub fn iter_source_packets(input: &[u8]) -> M2tsIter<'_> {
    assert!(
        input.len() % M2TS_PACKET_LEN == 0,
        "input not 192-byte aligned",
    );
    M2tsIter { buf: input, pos: 0 }
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

    #[test]
    fn strip_to_vec_matches_strip_in_place() {
        // Build 5 synthetic source packets with distinguishable TS payloads
        // and TP_extra_headers so we can compare both helpers byte-for-byte.
        const N: usize = 5;
        let mut input = vec![0u8; N * M2TS_PACKET_LEN];
        for i in 0..N {
            let arrival = 0x0011_2233u32.wrapping_add(i as u32 * 1000);
            let hdr = TpExtraHeader {
                copy_permission: (i & 0b11) as u8,
                arrival_time: arrival & 0x3FFF_FFFF,
            };
            input[i * M2TS_PACKET_LEN..i * M2TS_PACKET_LEN + TP_EXTRA_HEADER_LEN]
                .copy_from_slice(&hdr.encode());
            // TS sync byte then a deterministic body.
            input[i * M2TS_PACKET_LEN + TP_EXTRA_HEADER_LEN] = 0x47;
            for j in 1..TS_PACKET_LEN {
                input[i * M2TS_PACKET_LEN + TP_EXTRA_HEADER_LEN + j] = ((i * j) & 0xFF) as u8;
            }
        }
        // Reference: existing in-place helper.
        let mut reference = vec![0u8; N * TS_PACKET_LEN];
        let n_ref = strip_tp_extra(&input, &mut reference);
        assert_eq!(n_ref, N * TS_PACKET_LEN);

        // New convenience wrapper must produce byte-identical output.
        let from_vec = strip_tp_extra_to_vec(&input);
        assert_eq!(from_vec, reference);
    }

    #[test]
    #[should_panic(expected = "192-byte aligned")]
    fn strip_to_vec_panics_on_misaligned_input() {
        let input = vec![0u8; 191];
        let _ = strip_tp_extra_to_vec(&input);
    }

    #[test]
    fn iter_yields_one_packet_with_parsed_header_and_borrowed_payload() {
        let mut input = vec![0u8; M2TS_PACKET_LEN];
        let hdr = TpExtraHeader {
            copy_permission: 0b01,
            arrival_time: 0x0A0B_0C0D,
        };
        input[..TP_EXTRA_HEADER_LEN].copy_from_slice(&hdr.encode());
        input[TP_EXTRA_HEADER_LEN] = 0x47;
        input[TP_EXTRA_HEADER_LEN + 1] = 0xCA;
        input[M2TS_PACKET_LEN - 1] = 0xFE;

        let mut it = iter_source_packets(&input);
        assert_eq!(it.remaining(), 1);
        let pkt = it.next().expect("first packet");
        assert_eq!(pkt.tp_extra, hdr);
        assert_eq!(pkt.ts_payload[0], 0x47);
        assert_eq!(pkt.ts_payload[1], 0xCA);
        assert_eq!(pkt.ts_payload[TS_PACKET_LEN - 1], 0xFE);
        assert!(it.next().is_none());
        assert_eq!(it.remaining(), 0);
    }

    #[test]
    fn iter_walks_many_packets_in_order_with_distinct_arrival_times() {
        const N: usize = 13;
        let mut input = vec![0u8; N * M2TS_PACKET_LEN];
        for i in 0..N {
            let hdr = TpExtraHeader {
                copy_permission: 0,
                arrival_time: ((i as u32) * 27_000) & 0x3FFF_FFFF, // 1 ms steps at 27 MHz
            };
            input[i * M2TS_PACKET_LEN..i * M2TS_PACKET_LEN + TP_EXTRA_HEADER_LEN]
                .copy_from_slice(&hdr.encode());
            input[i * M2TS_PACKET_LEN + TP_EXTRA_HEADER_LEN] = 0x47;
            // Pack the iteration index into the TS payload tail so we
            // can verify ordering without parsing TS structure.
            input[i * M2TS_PACKET_LEN + M2TS_PACKET_LEN - 1] = i as u8;
        }

        let collected: Vec<_> = iter_source_packets(&input).collect();
        assert_eq!(collected.len(), N);
        for (i, pkt) in collected.iter().enumerate() {
            assert_eq!(pkt.tp_extra.arrival_time, (i as u32) * 27_000);
            assert_eq!(pkt.ts_payload[0], 0x47);
            assert_eq!(pkt.ts_payload[TS_PACKET_LEN - 1], i as u8);
        }
    }

    #[test]
    fn iter_size_hint_and_exact_size() {
        let input = vec![0u8; 7 * M2TS_PACKET_LEN];
        let mut it = iter_source_packets(&input);
        assert_eq!(it.size_hint(), (7, Some(7)));
        assert_eq!(it.len(), 7);
        // Advance and re-check.
        for expected in (0..7).rev() {
            let _ = it.next().unwrap();
            assert_eq!(it.len(), expected);
        }
        assert!(it.next().is_none());
    }

    #[test]
    fn iter_on_empty_buffer_yields_nothing() {
        let input: Vec<u8> = Vec::new();
        let mut it = iter_source_packets(&input);
        assert_eq!(it.remaining(), 0);
        assert!(it.next().is_none());
    }

    #[test]
    #[should_panic(expected = "192-byte aligned")]
    fn iter_panics_on_misaligned_input() {
        let input = vec![0u8; M2TS_PACKET_LEN + 1];
        let _ = iter_source_packets(&input);
    }

    #[test]
    fn iter_payload_matches_strip_in_place_output() {
        // Iterator-visible payloads must equal the linearised
        // `strip_tp_extra` output packet-for-packet — they are two
        // views of the same source bytes.
        const N: usize = 4;
        let mut input = vec![0u8; N * M2TS_PACKET_LEN];
        for i in 0..N {
            // TP_extra arbitrary
            input[i * M2TS_PACKET_LEN] = i as u8;
            input[i * M2TS_PACKET_LEN + TP_EXTRA_HEADER_LEN] = 0x47;
            for j in 1..TS_PACKET_LEN {
                input[i * M2TS_PACKET_LEN + TP_EXTRA_HEADER_LEN + j] = ((i + j) & 0xFF) as u8;
            }
        }
        let stripped = strip_tp_extra_to_vec(&input);
        for (i, pkt) in iter_source_packets(&input).enumerate() {
            let want = &stripped[i * TS_PACKET_LEN..(i + 1) * TS_PACKET_LEN];
            assert_eq!(&pkt.ts_payload[..], want);
        }
    }
}
