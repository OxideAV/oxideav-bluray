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

impl NavCommand {
    /// A single-line disassembly of this command, e.g.
    /// `"JumpTitle 0x1"`. Convenience wrapper over `self.decode()` +
    /// [`super::nav_command::DecodedCommand::disassemble`]. Diagnostic
    /// only — never re-assembled to bytes nor executed.
    pub fn disassemble(&self) -> String {
        self.decode().disassemble()
    }
}

impl MovieObject {
    /// Disassemble this MovieObject's command list into a multi-line
    /// listing — one `<index>: <command>` line per navigation command,
    /// prefixed by a header naming the object's playback-control flags.
    ///
    /// `index` is the object's position in `MovieObject.bdmv` (used only
    /// for the header label). The output is a forensic / diagnostic dump
    /// of the HDMV script; it is not re-assemblable and the commands are
    /// not executed (see `super::vm` for the interpreter).
    pub fn disassemble(&self, index: usize) -> String {
        let mut s = format!(
            "MovieObject[{index}] (resume={}, menu_mask={}, title_mask={}, {} cmd{})",
            self.resume_intention_flag,
            self.menu_call_mask,
            self.title_search_mask,
            self.commands.len(),
            if self.commands.len() == 1 { "" } else { "s" },
        );
        for (i, cmd) in self.commands.iter().enumerate() {
            s.push_str(&format!("\n  {i:>3}: {}", cmd.disassemble()));
        }
        s
    }
}

impl MovieObjects {
    /// Disassemble the entire `MovieObject.bdmv` table into a listing —
    /// every [`MovieObject`] rendered by [`MovieObject::disassemble`],
    /// separated by blank lines. The leading line records the table
    /// version. Diagnostic only; the script is never executed here.
    pub fn disassemble(&self) -> String {
        let mut s = format!(
            "MovieObject.bdmv version={} ({} object{})",
            String::from_utf8_lossy(&self.version),
            self.movie_objects.len(),
            if self.movie_objects.len() == 1 {
                ""
            } else {
                "s"
            },
        );
        for (i, mo) in self.movie_objects.iter().enumerate() {
            s.push('\n');
            s.push('\n');
            s.push_str(&mo.disassemble(i));
        }
        s
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

    /// Build a NavCommand from three big-endian words.
    fn nav(word0: u32, word1: u32, word2: u32) -> NavCommand {
        let mut bytes = [0u8; 12];
        bytes[0..4].copy_from_slice(&word0.to_be_bytes());
        bytes[4..8].copy_from_slice(&word1.to_be_bytes());
        bytes[8..12].copy_from_slice(&word2.to_be_bytes());
        NavCommand { bytes }
    }

    #[test]
    fn nav_command_disassemble() {
        // JumpTitle 1.
        assert_eq!(nav(0x2181_0000, 1, 0).disassemble(), "JumpTitle 0x1");
    }

    #[test]
    fn movie_object_disassemble_listing() {
        let mo = MovieObject {
            resume_intention_flag: 1,
            menu_call_mask: 0,
            title_search_mask: 1,
            commands: vec![
                nav(0x5040_0001, 0x0000_0001, 0x0000_0001), // Move r1, 0x1
                nav(0x2181_0000, 0x0000_0002, 0),           // JumpTitle 0x2
            ],
        };
        let listing = mo.disassemble(0);
        // Each command line is `\n` + two-space indent + a 3-wide
        // right-aligned index + ": " + the disassembly.
        let expected = "MovieObject[0] (resume=1, menu_mask=0, title_mask=1, 2 cmds)\n    \
                        0: Move r1, 0x1\n    \
                        1: JumpTitle 0x2";
        assert_eq!(listing, expected);
    }

    #[test]
    fn movie_object_singular_count_label() {
        let mo = MovieObject {
            resume_intention_flag: 0,
            menu_call_mask: 0,
            title_search_mask: 0,
            commands: vec![nav(0x0000_0000, 0, 0)], // Nop
        };
        let listing = mo.disassemble(3);
        assert!(listing.starts_with("MovieObject[3] (resume=0, menu_mask=0, title_mask=0, 1 cmd)"));
        assert!(listing.contains("\n    0: Nop"));
    }

    #[test]
    fn movie_objects_table_disassemble() {
        let mo = MovieObjects {
            version: *b"0200",
            movie_objects: vec![
                MovieObject {
                    resume_intention_flag: 0,
                    menu_call_mask: 0,
                    title_search_mask: 0,
                    commands: vec![nav(0x0203_0000, 0, 0)], // TerminatePL
                },
                MovieObject {
                    resume_intention_flag: 0,
                    menu_call_mask: 0,
                    title_search_mask: 0,
                    commands: vec![],
                },
            ],
        };
        let dump = mo.disassemble();
        assert!(dump.starts_with("MovieObject.bdmv version=0200 (2 objects)"));
        assert!(dump.contains("MovieObject[0]"));
        assert!(dump.contains("0: TerminatePL"));
        assert!(dump.contains("MovieObject[1] (resume=0, menu_mask=0, title_mask=0, 0 cmds)"));
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
