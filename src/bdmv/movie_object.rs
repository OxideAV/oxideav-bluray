//! `MovieObject.bdmv` — HDMV navigation script per BD-ROM Part 3 §5.3.
//!
//! Phase 1: enumerate, do not execute. Each `MovieObject` carries a
//! sequence of 12-byte navigation commands; we surface them as
//! opaque [`NavCommand`] records.
//!
//! Binary layout:
//!
//! ```text
//!   0    type_indicator         "MOBJ"
//!   4    version_number         "0200"
//!   8    extension_data_start   u32      (offset to ExtensionData, 0 if absent)
//!  12    28 reserved bytes
//!  40    MovieObjects()                  self-delimited
//! ```
//!
//! `MovieObjects()`:
//!
//! ```text
//!   0    length                 u32   (byte count of remainder)
//!   4    32 reserved bytes
//!  36    number_of_movie_objects u16
//!  38    repeat number_of_movie_objects times:
//!           MovieObject {
//!             resume_intention_flag       1 bit
//!             menu_call_mask              1 bit
//!             title_search_mask           1 bit
//!             reserved                    13 bits
//!             number_of_navigation_commands u16
//!             NavigationCommand[] (12 bytes each)
//!           }
//! ```

use crate::bdmv::common::{BdmvHeader, Reader};
use crate::error::{BlurayError, Result};

/// One HDMV navigation command. We surface the raw 12-byte payload
/// instead of decoding the opcode (which we wouldn't execute anyway
/// in Phase 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavCommand {
    pub bytes: [u8; 12],
}

/// One `MovieObject` — a flat list of navigation commands plus three
/// playback-control flag bits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovieObject {
    pub resume_intention_flag: u8, // 1 bit
    pub menu_call_mask: u8,        // 1 bit
    pub title_search_mask: u8,     // 1 bit
    pub commands: Vec<NavCommand>,
}

/// Parsed `MovieObject.bdmv`.
#[derive(Debug, Clone)]
pub struct MovieObjects {
    pub version: [u8; 4],
    pub movie_objects: Vec<MovieObject>,
}

impl MovieObjects {
    pub fn parse(buf: &[u8]) -> Result<Self> {
        let header = BdmvHeader::parse(buf)?;
        if header.type_indicator != b"MOBJ" {
            return Err(BlurayError::malformed(format!(
                "MovieObject.bdmv type_indicator {:?}",
                header.type_indicator
            )));
        }
        let version = *header.version_number;

        let mut r = Reader::new(buf);
        r.seek(40)?;

        let body_len = r.read_u32()? as usize;
        if body_len + r.pos > buf.len() {
            return Err(BlurayError::malformed("MovieObjects body overruns buffer"));
        }
        let _body_end = r.pos + body_len;

        r.skip(32)?; // 32 reserved
        let n_objects = r.read_u16()? as usize;

        let mut movie_objects = Vec::with_capacity(n_objects);
        for _ in 0..n_objects {
            let b0 = r.read_u8()?;
            let _b1 = r.read_u8()?; // remaining flag/reserved bits
            let resume = (b0 >> 7) & 1;
            let menu = (b0 >> 6) & 1;
            let title = (b0 >> 5) & 1;
            let n_cmds = r.read_u16()? as usize;
            let mut commands = Vec::with_capacity(n_cmds);
            for _ in 0..n_cmds {
                let s = r.slice(12)?;
                let mut bytes = [0u8; 12];
                bytes.copy_from_slice(s);
                commands.push(NavCommand { bytes });
            }
            movie_objects.push(MovieObject {
                resume_intention_flag: resume,
                menu_call_mask: menu,
                title_search_mask: title,
                commands,
            });
        }

        Ok(Self {
            version,
            movie_objects,
        })
    }

    /// Encode back to bytes (test-only).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"MOBJ");
        out.extend_from_slice(&self.version);
        out.extend_from_slice(&[0u8; 4]); // extension_data_start
        out.extend_from_slice(&[0u8; 28]); // reserved

        // Placeholder length.
        let len_off = out.len();
        out.extend_from_slice(&[0u8; 4]);
        let body_start = out.len();

        out.extend_from_slice(&[0u8; 32]); // 32 reserved
        out.extend_from_slice(&(self.movie_objects.len() as u16).to_be_bytes());

        for mo in &self.movie_objects {
            let b0 = ((mo.resume_intention_flag & 1) << 7)
                | ((mo.menu_call_mask & 1) << 6)
                | ((mo.title_search_mask & 1) << 5);
            out.push(b0);
            out.push(0);
            out.extend_from_slice(&(mo.commands.len() as u16).to_be_bytes());
            for cmd in &mo.commands {
                out.extend_from_slice(&cmd.bytes);
            }
        }
        let body_len = (out.len() - body_start) as u32;
        out[len_off..len_off + 4].copy_from_slice(&body_len.to_be_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_empty() {
        let mo = MovieObjects {
            version: *b"0200",
            movie_objects: vec![],
        };
        let bytes = mo.encode();
        let parsed = MovieObjects::parse(&bytes).unwrap();
        assert!(parsed.movie_objects.is_empty());
    }

    #[test]
    fn round_trip_three_objects() {
        let mo = MovieObjects {
            version: *b"0200",
            movie_objects: vec![
                MovieObject {
                    resume_intention_flag: 1,
                    menu_call_mask: 0,
                    title_search_mask: 1,
                    commands: vec![NavCommand { bytes: [0x12; 12] }],
                },
                MovieObject {
                    resume_intention_flag: 0,
                    menu_call_mask: 1,
                    title_search_mask: 0,
                    commands: vec![],
                },
                MovieObject {
                    resume_intention_flag: 1,
                    menu_call_mask: 1,
                    title_search_mask: 1,
                    commands: vec![
                        NavCommand { bytes: [0x01; 12] },
                        NavCommand { bytes: [0xFF; 12] },
                    ],
                },
            ],
        };
        let bytes = mo.encode();
        let parsed = MovieObjects::parse(&bytes).unwrap();
        assert_eq!(parsed.movie_objects, mo.movie_objects);
    }

    #[test]
    fn rejects_wrong_type_indicator() {
        let mut bytes = MovieObjects {
            version: *b"0200",
            movie_objects: vec![],
        }
        .encode();
        bytes[0] = b'X';
        assert!(MovieObjects::parse(&bytes).is_err());
    }
}
