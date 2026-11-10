//! `ExtensionData()` — the trailing, ID-tagged extension block shared by
//! `.mpls` and `.clpi` files.
//!
//! Layout (observed on every staged fixture,
//! `docs/container/bluray/fixtures/*/dump.txt`; big-endian):
//!
//! ```text
//!   length                       u32   bytes after this field (0 = no data)
//!   data_block_start_address     u32   relative to the `length` field
//!   reserved                     24 bits
//!   number_of_ext_data_entries   u8
//!   for each entry:
//!     ID1                        u16
//!     ID2                        u16
//!     ext_data_start_address     u32   relative to the `length` field
//!     ext_data_length            u32
//!   data blocks
//! ```
//!
//! Known IDs (`docs/container/bluray/stn-table-observed.md` §3):
//!
//! | file | ID1, ID2 | content |
//! |---|---|---|
//! | `.mpls` | 1, 1 | PiP metadata (kept raw) |
//! | `.mpls` | 2, 1 | `STN_table_SS`, one per PlayItem — [`crate::StnTableSs`] |
//! | `.mpls` | 2, 2 | SubPath extension list (stereoscopic SubPaths) |
//! | `.clpi` | 2, 5 | `ProgramInfo_SS` of a dependent-view clip |
//!
//! The parser is lenient: an entry whose address range falls outside
//! the file is dropped rather than failing the whole file parse (the
//! extension block is auxiliary to playback).

use crate::bdmv::common::Reader;

/// `(ID1, ID2)` of the PlayList PiP metadata block.
pub const EXT_ID_PIP_METADATA: (u16, u16) = (1, 1);
/// `(ID1, ID2)` of the PlayList `STN_table_SS` block.
pub const EXT_ID_STN_TABLE_SS: (u16, u16) = (2, 1);
/// `(ID1, ID2)` of the PlayList SubPath-extension block.
pub const EXT_ID_SUB_PATH_EXTENSION: (u16, u16) = (2, 2);
/// `(ID1, ID2)` of the Clip `ProgramInfo_SS` block.
pub const EXT_ID_PROGRAM_INFO_SS: (u16, u16) = (2, 5);

/// One `ExtensionData()` entry: its `(ID1, ID2)` tag and the raw data
/// block it addresses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExtensionDataEntry {
    /// `ID1`.
    pub id1: u16,
    /// `ID2`.
    pub id2: u16,
    /// The `ext_data_length` bytes the entry addresses.
    pub data: Vec<u8>,
}

impl ExtensionDataEntry {
    /// Build an entry.
    pub fn new(id1: u16, id2: u16, data: Vec<u8>) -> Self {
        Self { id1, id2, data }
    }

    /// `(ID1, ID2)` pair.
    pub fn id(&self) -> (u16, u16) {
        (self.id1, self.id2)
    }
}

/// Parse the `ExtensionData()` block starting at `start` in `buf`
/// (`start` is the file's `ExtensionData_start_address`). Returns an
/// empty list for `start == 0`, a zero `length`, or a block that does
/// not fit in `buf`.
#[doc(hidden)]
pub fn parse_extension_data(buf: &[u8], start: usize) -> Vec<ExtensionDataEntry> {
    if start == 0 || start >= buf.len() {
        return Vec::new();
    }
    let mut r = Reader::new(buf);
    let parse = |r: &mut Reader<'_>| -> crate::error::Result<Vec<ExtensionDataEntry>> {
        r.seek(start)?;
        let length = r.read_u32()? as usize;
        if length == 0 {
            return Ok(Vec::new());
        }
        let end = start
            .saturating_add(4)
            .saturating_add(length)
            .min(buf.len());
        let _data_block_start = r.read_u32()?;
        r.skip(3)?;
        let n = r.read_u8()? as usize;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let id1 = r.read_u16()?;
            let id2 = r.read_u16()?;
            let addr = r.read_u32()? as usize;
            let len = r.read_u32()? as usize;
            let s = start.saturating_add(addr);
            let e = s.saturating_add(len);
            if addr == 0 || e > end {
                continue;
            }
            out.push(ExtensionDataEntry::new(id1, id2, buf[s..e].to_vec()));
        }
        Ok(out)
    };
    parse(&mut r).unwrap_or_default()
}

/// Serialise `entries` as an `ExtensionData()` block (header, entry
/// table, then the data blocks back to back). An empty list encodes as
/// a zero `length` word.
#[doc(hidden)]
pub fn encode_extension_data(entries: &[ExtensionDataEntry]) -> Vec<u8> {
    if entries.is_empty() {
        return vec![0, 0, 0, 0];
    }
    let header = 12 + 12 * entries.len();
    let mut out = Vec::with_capacity(header + entries.iter().map(|e| e.data.len()).sum::<usize>());
    out.extend_from_slice(&[0u8; 4]); // length placeholder
    out.extend_from_slice(&(header as u32).to_be_bytes());
    out.extend_from_slice(&[0u8; 3]);
    out.push(entries.len() as u8);
    let mut addr = header;
    for e in entries {
        out.extend_from_slice(&e.id1.to_be_bytes());
        out.extend_from_slice(&e.id2.to_be_bytes());
        out.extend_from_slice(&(addr as u32).to_be_bytes());
        out.extend_from_slice(&(e.data.len() as u32).to_be_bytes());
        addr += e.data.len();
    }
    for e in entries {
        out.extend_from_slice(&e.data);
    }
    let length = (out.len() - 4) as u32;
    out[..4].copy_from_slice(&length.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_two_entries() {
        let entries = vec![
            ExtensionDataEntry::new(2, 1, vec![1, 2, 3]),
            ExtensionDataEntry::new(2, 2, vec![9; 7]),
        ];
        let mut file = vec![0xAAu8; 16];
        let start = file.len();
        file.extend_from_slice(&encode_extension_data(&entries));
        assert_eq!(parse_extension_data(&file, start), entries);
    }

    #[test]
    fn empty_and_out_of_range() {
        assert!(parse_extension_data(&[0u8; 8], 0).is_empty());
        assert!(parse_extension_data(&[0u8; 8], 4).is_empty());
        assert!(parse_extension_data(&[0u8; 8], 99).is_empty());
        // Entry addressing past the block is dropped, not a panic.
        let mut file = vec![0u8; 4];
        file.extend_from_slice(&encode_extension_data(&[
            ExtensionDataEntry::new(1, 1, vec![5; 4]),
            ExtensionDataEntry::new(1, 2, vec![6; 2]),
        ]));
        // First entry's ext_data_length → 1000.
        file[4 + 20..4 + 24].copy_from_slice(&1000u32.to_be_bytes());
        let got = parse_extension_data(&file, 4);
        assert_eq!(got, vec![ExtensionDataEntry::new(1, 2, vec![6; 2])]);
    }
}
