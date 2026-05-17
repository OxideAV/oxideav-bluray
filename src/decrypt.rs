//! Pluggable stream decryptor trait.
//!
//! AACS lives in a separate sibling crate (`oxideav-aacs`). To avoid
//! holding a hard dep on a brand-new sibling (and the publish-cascade
//! trouble that brings), this crate ships only the *trait* — the AACS
//! crate ships an adapter implementing it.
//!
//! ## Unit size
//!
//! Per BD-ROM Part 3 §5.6 the AACS encryption unit is 6144 bytes — 32
//! BDAV source packets of 192 bytes each, equivalently 32 standard
//! MPEG-TS packets of 188 bytes. We accept a `&[u8]` of *any* length
//! that is an exact multiple of 6144; the trait impl is responsible
//! for fan-out across the constituent units.
//!
//! ## Identity decryptor
//!
//! [`Identity`] passes data through unchanged. It exists so the
//! synthetic test fixtures (which are never encrypted) can exercise
//! the same code path as a real AACS-protected disc would.

use std::fmt;

/// AACS-style alignment for the decryption unit: 32 × 192-byte
/// BDAV source packets — equivalently 3 × 2048-byte logical sectors.
/// On-disc bytes are encrypted at this granularity (per AACS
/// Pre-Recorded Video Book §3.2.5.2); decryption happens *before*
/// `TP_extra_header` stripping, so the buffer the trait sees still
/// carries 192-byte packets.
pub const AACS_UNIT_LEN: usize = 6144;

/// Error returned by a [`StreamDecryptor`] implementation.
///
/// Kept opaque + non-exhaustive so the trait can evolve without
/// breaking downstream impls; the AACS adapter is responsible for
/// mapping its own error vocabulary into `DecryptError::new("...")`.
#[derive(Debug)]
#[non_exhaustive]
pub struct DecryptError {
    msg: String,
}

impl DecryptError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self { msg: msg.into() }
    }
}

impl fmt::Display for DecryptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for DecryptError {}

/// Plug-in trait for AACS / BD+ / any other on-disc cipher.
///
/// `decrypt_units` is called with a buffer whose length is a multiple
/// of [`AACS_UNIT_LEN`] (6144 bytes) and an absolute *byte offset*
/// from the start of the originating `.m2ts` clip — implementations
/// that need a per-unit counter (CTR / OFB modes) can derive it from
/// the offset.
///
/// The decryptor mutates the buffer in place. Returning `Err` aborts
/// the title stream and surfaces a [`crate::BlurayError::Decrypt`] to
/// the caller.
pub trait StreamDecryptor: Send {
    /// Decrypt `buf` in place. `clip_offset` is the absolute byte
    /// offset of `buf[0]` within the `.m2ts` clip that produced it.
    ///
    /// `buf.len()` is always a multiple of [`AACS_UNIT_LEN`].
    fn decrypt_units(&mut self, buf: &mut [u8], clip_offset: u64) -> Result<(), DecryptError>;
}

/// A no-op decryptor: copies input to output unchanged. Useful for
/// homemade / unprotected BD-R discs and for the crate's own test
/// suite, which never produces encrypted bytes.
#[derive(Debug, Default, Clone, Copy)]
pub struct Identity;

impl StreamDecryptor for Identity {
    fn decrypt_units(&mut self, _buf: &mut [u8], _clip_offset: u64) -> Result<(), DecryptError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aacs_unit_len_matches_spec() {
        // 32 BDAV source packets × 192 bytes = 6144 bytes = 3 × 2048
        // logical sectors. AACS Pre-Recorded Video Book §3.2.5.2.
        assert_eq!(AACS_UNIT_LEN, 32 * 192);
        assert_eq!(AACS_UNIT_LEN, 3 * 2048);
    }

    #[test]
    fn identity_pass_through() {
        let mut d = Identity;
        let mut buf = vec![0xAAu8; AACS_UNIT_LEN];
        d.decrypt_units(&mut buf, 0).unwrap();
        assert!(buf.iter().all(|&b| b == 0xAA));
    }
}
