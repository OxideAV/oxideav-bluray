//! Hostile-input hardening for the BDMV metadata / navigation parsers.
//!
//! Every parser in `oxideav_bluray::bdmv` runs on wholly untrusted disc
//! bytes: a length field that lies about the body size, a stream /
//! play-item / entry-point / command count far larger than the bytes
//! that follow, a section offset that points past the end, an all-zero
//! or all-`0xFF` sector, and every truncation of an otherwise-valid
//! file. None of these may panic, overflow in debug, or index out of
//! bounds — the contract is `Ok(_)` or `Err(BlurayError)` for ANY
//! `&[u8]`.
//!
//! This is the in-CI regression companion to the `bdmv_parsers`
//! cargo-fuzz target: the fuzz target explores the space at scale, these
//! cases pin the specific adversarial shapes so a future refactor that
//! reintroduces an unchecked subtraction / cast / index is caught by
//! `cargo test` without a nightly fuzz run.

use oxideav_bluray::bdmv::{clpi, index_bdmv, movie_object, mpls, pgs};

/// A tiny deterministic xorshift PRNG so the sweep is reproducible
/// across runs and machines (no `rand` dep, no cross-crate dev-dep).
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn byte(&mut self) -> u8 {
        (self.next_u64() >> 24) as u8
    }
}

/// Run every BDMV parser over one byte slice. Returning normally means
/// no parser panicked / overflowed / indexed out of bounds — the whole
/// point. Parses that succeed are additionally driven through their
/// derived accessors so a downstream index panic is caught too.
fn drive_all(buf: &[u8]) {
    if let Ok(pl) = mpls::PlayListMpls::parse(buf) {
        let _ = pl.duration_90k();
        let _ = pl.chapters();
        let _ = pl.chapters_with_duration();
        let _ = pl.app_info.playback_kind();
        let _ = pl.app_info.uo_mask_table().prohibited_ops();
        for pi in &pl.play_list.play_items {
            let _ = pi.duration_90k();
            let _ = pi.flags.uo_mask_table().reserved_bits();
        }
        for m in &pl.marks {
            let _ = m.kind();
        }
        for sp in &pl.play_list.sub_paths {
            let _ = sp.kind().display_name();
            for spi in &sp.sub_play_items {
                let _ = spi.duration_90k();
                let _ = spi.sync_start_pts_90k();
                let _ = spi.num_clips();
            }
        }
    }
    if let Ok(clip) = clpi::ClipInformation::parse(buf) {
        let _ = clip.cpi.primary_video_ep_map();
        let _ = clip.sequence_info.presentation_span_90k();
        for ep in &clip.cpi.ep_map {
            let _ = ep.entry_point_count();
            let _ = ep.first_pts();
            let _ = ep.last_pts();
            let _ = ep.indexed_span_90k();
            let _ = ep.seek_spn(0);
            let _ = ep.seek_spn(u32::MAX);
            let _ = ep.entry_point_after(0);
        }
    }
    let _ = index_bdmv::IndexBdmv::parse(buf);
    if let Ok(mobj) = movie_object::MovieObjects::parse(buf) {
        let _ = mobj.disassemble();
    }
    let _ = pgs::parse_segments(buf);
    let _ = pgs::parse_display_sets(buf);
    let _ = pgs::decode_rle(buf, 32, 32);
}

/// Every BDMV file that carries a length-prefixed body reads a 4-byte
/// big-endian body length near a fixed offset. Build a valid magic +
/// version, then set that length to a value that overruns the real
/// buffer, and to `u32::MAX`. Must be refused, never panic.
#[test]
fn size_lies_do_not_panic() {
    for magic in [b"MPLS", b"CLPI", b"INDX", b"MOBJ"] {
        // Header + a body just long enough to reach the count fields,
        // with every length / offset field set to a lie.
        for &lie in &[0xFFFF_FFFFu32, 0x7FFF_FFFF, 0x0001_0000, u32::MAX - 3] {
            let mut buf = Vec::new();
            buf.extend_from_slice(magic);
            buf.extend_from_slice(b"0200");
            // Offsets at 8, 12, 16... all set to the lie.
            for _ in 0..12 {
                buf.extend_from_slice(&lie.to_be_bytes());
            }
            drive_all(&buf);

            // Same, but pad out to a plausible file size so section
            // offsets land inside the buffer and the count fields are
            // reached before the lie is caught.
            let mut padded = buf.clone();
            padded.resize(512, 0xFF);
            // Sprinkle count-sized lies deeper in.
            for off in (40..padded.len().saturating_sub(4)).step_by(37) {
                padded[off] = 0xFF;
                padded[off + 1] = 0xFF;
            }
            drive_all(&padded);
        }
    }
}

/// Truncation sweep: take a buffer that reaches into each parser's body
/// and feed every prefix length `0..=N`. A parser that reads a field
/// without a bounds check trips here.
#[test]
fn truncation_sweep_does_not_panic() {
    // A generic 300-byte "looks like a BDMV file" buffer per magic:
    // valid magic + version, then structured-ish bytes (small counts,
    // in-range offsets) so truncation cuts across real read sites.
    for magic in [b"MPLS", b"CLPI", b"INDX", b"MOBJ", b"PG\0\0"] {
        let mut full = Vec::new();
        full.extend_from_slice(&magic[..]);
        full.resize(300, 0);
        // Plant a handful of small counts + offsets so an untruncated
        // parse would actually walk a body.
        for (i, off) in (8..300).step_by(4).enumerate() {
            let v = ((i as u32) % 7 + 1) * if i % 3 == 0 { 0x20 } else { 1 };
            if off + 4 <= full.len() {
                full[off..off + 4].copy_from_slice(&v.to_be_bytes());
            }
        }
        for len in 0..=full.len() {
            drive_all(&full[..len]);
        }
    }
}

/// Uniform-fill sectors: all-zero and all-`0xFF` of many lengths. These
/// are the degenerate cases a raw sector read hands the parser when it
/// lands on padding or an erased block.
#[test]
fn uniform_fill_does_not_panic() {
    for fill in [0x00u8, 0xFF, 0xAA, 0x55] {
        for len in [0usize, 1, 7, 8, 13, 39, 40, 41, 100, 512, 2048, 65_540] {
            let buf = vec![fill; len];
            drive_all(&buf);
        }
    }
}

/// Valid magic + version, then a fully random tail. Repeats a large
/// number of deterministic PRNG draws so a rare unchecked path is likely
/// to be hit; a failure is reproducible from the fixed seed.
#[test]
fn random_tails_after_valid_magic_do_not_panic() {
    let mut rng = Rng::new(0x0BAD_C0DE_1234_5678);
    let magics: [&[u8]; 5] = [b"MPLS", b"CLPI", b"INDX", b"MOBJ", b"PG\0\0"];
    for iter in 0..40_000u32 {
        let magic = magics[(iter as usize) % magics.len()];
        let len = 8 + (rng.next_u64() as usize % 600);
        let mut buf = Vec::with_capacity(len);
        buf.extend_from_slice(magic);
        buf.extend_from_slice(b"0200");
        while buf.len() < len {
            buf.push(rng.byte());
        }
        drive_all(&buf);
    }
}

/// Fully random buffers with no magic bias — the outer envelope check
/// itself must be panic-safe on garbage.
#[test]
fn pure_random_does_not_panic() {
    let mut rng = Rng::new(0xDEAD_BEEF_CAFE_F00D);
    for _ in 0..40_000u32 {
        let len = rng.next_u64() as usize % 300;
        let mut buf = Vec::with_capacity(len);
        for _ in 0..len {
            buf.push(rng.byte());
        }
        drive_all(&buf);
    }
}
