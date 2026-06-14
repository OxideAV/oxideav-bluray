#![no_main]

//! Coverage-guided fuzz harness for the UDF / ECMA-167 descriptor
//! parsers in `oxideav_bluray::udf`.
//!
//! These parsers turn raw on-disc bytes into typed descriptors during a
//! BD-ROM mount. They run before any structural validation has
//! succeeded, so a malformed sector — truncated descriptor, a length
//! field that overruns its buffer, an out-of-range OSTA d-string
//! compression id, a fixed-offset field on a too-short slice — must
//! surface as `Err(BlurayError)`, never as a panic, a debug-mode
//! integer overflow, or an index-out-of-bounds.
//!
//! Contract under test for every entry point: any `&[u8]` yields either
//! `Ok(_)` or `Err(_)`. The first byte selects which parser the rest of
//! the slice is fed to, so cargo-fuzz minimises a single corpus across
//! the whole descriptor surface.

use libfuzzer_sys::fuzz_target;
use oxideav_bluray::udf;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let sel = data[0];
    let body = &data[1..];

    match sel % 17 {
        0 => {
            let _ = udf::DescriptorTag::parse(body);
        }
        1 => {
            let _ = udf::ExtentAd::parse(body);
        }
        2 => {
            let _ = udf::ShortAd::parse(body);
        }
        3 => {
            let _ = udf::LbAddr::parse(body);
        }
        4 => {
            let _ = udf::LongAd::parse(body);
        }
        5 => {
            let _ = udf::ExtAd::parse(body);
        }
        6 => {
            let _ = udf::AllocationExtentDescriptor::parse(body);
        }
        7 => {
            let _ = udf::AnchorVolumeDescriptorPointer::parse(body);
        }
        8 => {
            let _ = udf::PrimaryVolumeDescriptor::parse(body);
        }
        9 => {
            let _ = udf::PartitionDescriptor::parse(body);
        }
        10 => {
            let _ = udf::LogicalVolumeDescriptor::parse(body);
        }
        11 => {
            let _ = udf::FileSetDescriptor::parse(body);
        }
        12 => {
            let _ = udf::FileIdentifierDescriptor::parse(body);
        }
        13 => {
            let _ = udf::IcbTag::parse(body);
        }
        14 => {
            let _ = udf::FileEntry::parse(body);
        }
        15 => {
            let _ = udf::decode_dstring(body);
        }
        _ => {
            let _ = udf::decode_dstring_field(body);
        }
    }
});
