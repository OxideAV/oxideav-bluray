//! Crate-local error type.
//!
//! Kept deliberately small and `oxideav-core`-free so the standalone
//! build path stays decoupled from the framework error type. A small
//! `From<std::io::Error>` lets `?` work over the UDF I/O layer.

use std::fmt;

/// Result alias for the crate.
pub type Result<T> = std::result::Result<T, BlurayError>;

/// What can go wrong when mounting a disc, walking BDMV, or streaming
/// a title.
#[derive(Debug)]
pub enum BlurayError {
    /// Underlying I/O error (sector read, file open).
    Io(std::io::Error),
    /// A descriptor or BDMV file has a malformed header / tag / length
    /// that does not match the spec. The `String` carries a short
    /// human-readable hint for the diagnostic; no user data is ever
    /// embedded.
    Malformed(String),
    /// The disc root does not look like a BD-ROM: no `BDMV/` subdir,
    /// or no readable `index.bdmv`.
    NotBluray(String),
    /// A feature the parser cannot handle in Phase 1 (e.g. ICB strategy
    /// 4 hierarchical, sparse files, encrypted sub-paths). Carries the
    /// short name of the unsupported construct.
    Unsupported(String),
    /// Decryption failure surfaced from a plugged-in [`crate::StreamDecryptor`].
    Decrypt(String),
}

impl fmt::Display for BlurayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Malformed(s) => write!(f, "malformed Blu-ray structure: {s}"),
            Self::NotBluray(s) => write!(f, "not a Blu-ray disc: {s}"),
            Self::Unsupported(s) => write!(f, "unsupported Blu-ray construct: {s}"),
            Self::Decrypt(s) => write!(f, "decryption failed: {s}"),
        }
    }
}

impl std::error::Error for BlurayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for BlurayError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl BlurayError {
    pub(crate) fn malformed(msg: impl Into<String>) -> Self {
        Self::Malformed(msg.into())
    }
    pub(crate) fn not_bluray(msg: impl Into<String>) -> Self {
        Self::NotBluray(msg.into())
    }
    pub(crate) fn unsupported(msg: impl Into<String>) -> Self {
        Self::Unsupported(msg.into())
    }
}
