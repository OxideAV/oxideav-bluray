//! Shared BDMV-file plumbing: big-endian byte reader, header layout
//! verification, and small helpers used by every parser in the
//! `bdmv` module.

use crate::error::{BlurayError, Result};

/// Standard 12-byte BDMV file header: 4-byte ASCII type indicator
/// (e.g. `b"INDX"`, `b"MPLS"`, `b"CLPI"`, `b"MOBJ"`) + 4-byte ASCII
/// version (`b"0200"` for BD-ROM 2.0, `b"0100"` for legacy). Followed
/// by a 4-byte big-endian offset to the next major section, which
/// varies per file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BdmvHeader<'a> {
    pub type_indicator: &'a [u8; 4],
    pub version_number: &'a [u8; 4],
}

impl<'a> BdmvHeader<'a> {
    pub const SIZE: usize = 8;

    pub fn parse(buf: &'a [u8]) -> Result<Self> {
        if buf.len() < Self::SIZE {
            return Err(BlurayError::malformed("BDMV header truncated"));
        }
        let type_indicator: &[u8; 4] = buf[0..4].try_into().unwrap();
        let version_number: &[u8; 4] = buf[4..8].try_into().unwrap();
        Ok(Self {
            type_indicator,
            version_number,
        })
    }
}

/// Big-endian byte reader. Holds a borrowed slice and a cursor.
/// Bounds-checks every read and surfaces an [`BlurayError::Malformed`]
/// on overruns.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    pub buf: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos.min(self.buf.len())
    }

    pub fn seek(&mut self, pos: usize) -> Result<()> {
        if pos > self.buf.len() {
            return Err(BlurayError::malformed(format!(
                "seek to {pos} past end {}",
                self.buf.len()
            )));
        }
        self.pos = pos;
        Ok(())
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.seek(self.pos + n)
    }

    pub fn slice(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            return Err(BlurayError::malformed("slice past end"));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn read_u8(&mut self) -> Result<u8> {
        Ok(self.slice(1)?[0])
    }

    pub fn read_u16(&mut self) -> Result<u16> {
        let s = self.slice(2)?;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }

    pub fn read_u32(&mut self) -> Result<u32> {
        let s = self.slice(4)?;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }

    pub fn read_u64(&mut self) -> Result<u64> {
        let s = self.slice(8)?;
        Ok(u64::from_be_bytes([
            s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
        ]))
    }
}

/// Map a clip filename in PLAYLIST `clip_information_file_name` (e.g.
/// `"00001"`) to the absolute BDMV path of the CLIPINF file
/// (`"BDMV/CLIPINF/00001.clpi"`). The `codec_id` argument is the
/// 4-byte clip type identifier (`b"M2TS"` for standard m2ts clips).
pub fn clpi_path(stem: &str, codec_id: &[u8; 4]) -> Result<String> {
    if stem.len() != 5 || !stem.chars().all(|c| c.is_ascii_digit()) {
        return Err(BlurayError::malformed(format!(
            "clip name {stem:?} is not 5 digits"
        )));
    }
    if codec_id != b"M2TS" {
        return Err(BlurayError::unsupported(format!(
            "non-M2TS clip codec {:?}",
            std::str::from_utf8(codec_id).unwrap_or("???")
        )));
    }
    Ok(format!("BDMV/CLIPINF/{stem}.clpi"))
}

/// Like [`clpi_path`] but returns the `.m2ts` path under `STREAM/`.
pub fn m2ts_path(stem: &str) -> Result<String> {
    if stem.len() != 5 || !stem.chars().all(|c| c.is_ascii_digit()) {
        return Err(BlurayError::malformed(format!(
            "clip name {stem:?} is not 5 digits"
        )));
    }
    Ok(format!("BDMV/STREAM/{stem}.m2ts"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_parse() {
        let buf = b"INDX0200";
        let h = BdmvHeader::parse(buf).unwrap();
        assert_eq!(h.type_indicator, b"INDX");
        assert_eq!(h.version_number, b"0200");
    }

    #[test]
    fn header_truncated() {
        assert!(BdmvHeader::parse(b"INDX").is_err());
    }

    #[test]
    fn reader_u32_bounds() {
        let buf = [0u8, 0, 0, 1, 0, 0, 0, 2];
        let mut r = Reader::new(&buf);
        assert_eq!(r.read_u32().unwrap(), 1);
        assert_eq!(r.read_u32().unwrap(), 2);
        assert!(r.read_u8().is_err());
    }

    #[test]
    fn clpi_and_m2ts_paths() {
        assert_eq!(
            clpi_path("00012", b"M2TS").unwrap(),
            "BDMV/CLIPINF/00012.clpi"
        );
        assert_eq!(m2ts_path("00012").unwrap(), "BDMV/STREAM/00012.m2ts");
        assert!(clpi_path("12", b"M2TS").is_err());
        assert!(clpi_path("00012", b"FMTS").is_err());
    }
}
