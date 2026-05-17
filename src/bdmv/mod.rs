//! BDMV directory parsers — per BD-ROM Part 3 §5.
//!
//! Layout (Phase 1 — relevant subset):
//!
//! ```text
//! BDMV/
//!   index.bdmv          (§5.2)
//!   MovieObject.bdmv    (§5.3)
//!   PLAYLIST/
//!     00000.mpls        (§5.4)
//!     ...
//!   CLIPINF/
//!     00000.clpi        (§5.5)
//!     ...
//!   STREAM/
//!     00000.m2ts        (§5.6 — BDAV source packet stream)
//!     ...
//!   AUXDATA/  META/  BDJO/  JAR/   (out of Phase 1 scope)
//! ```
//!
//! Every BDMV file uses big-endian integers and starts with an
//! 8-byte identifier (`type_indicator`) + 4-byte
//! `version_number`.

pub mod clpi;
pub mod common;
pub mod index_bdmv;
pub mod movie_object;
pub mod mpls;

pub use common::{BdmvHeader, Reader};
