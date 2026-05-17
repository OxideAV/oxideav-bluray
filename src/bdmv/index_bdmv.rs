//! `index.bdmv` — top-level title catalogue per BD-ROM Part 3 §5.2.
//!
//! Binary layout (big-endian throughout):
//!
//! ```text
//!   0    type_indicator         char[4]  "INDX"
//!   4    version_number         char[4]  "0200"
//!   8    indexes_start_address  u32      byte offset to Indexes()
//!  12    extension_data_start   u32      byte offset to ExtensionData()
//!  16    24 reserved bytes (must be 0)
//!  40    AppInfoBDMV()                   self-delimited
//!  ...   Indexes()                       at indexes_start_address
//! ```
//!
//! `AppInfoBDMV()`:
//!
//! ```text
//!   0    length                 u32      byte count of remainder
//!   4    reserved               4 bits
//!   4.4  initial_output_mode_preference  1 bit
//!   4.5  content_exist_flag     1 bit
//!   4.6  reserved               2 bits
//!   5    video_format           4 bits | frame_rate 4 bits
//!   6    user_data              32 bytes
//! ```
//!
//! `Indexes()`:
//!
//! ```text
//!   0    length                 u32      byte count of remainder
//!   4    FirstPlaybackTitle (Object())   sizeof = 12
//!  16    MenuTitle (Object())            sizeof = 12
//!  28    number_of_titles       u16
//!  30    repeat number_of_titles times:
//!           Object()  sizeof = 12
//! ```
//!
//! `Object()`:
//!
//! ```text
//!   0    object_type            2 bits (1 = HDMV, 2 = BD-J)
//!   0.2  reserved               6 bits
//!   1    1 reserved byte
//!   2    HDMV-or-BD-J variant:
//!          HDMV:
//!            playback_type    2 bits
//!            reserved         14 bits
//!            id_ref           u16   (movie_object_id for HDMV)
//!            reserved         u32
//!          BD-J:
//!            playback_type    2 bits
//!            reserved         14 bits
//!            bdjo_file_name   char[5]
//!            reserved         u8
//!   12   end of Object()
//! ```

use crate::bdmv::common::{BdmvHeader, Reader};
use crate::error::{BlurayError, Result};

/// AppInfoBDMV section (fixed structure, no variable extensions in
/// Phase 1 surfacing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppInfoBdmv {
    pub initial_output_mode_preference: u8, // 1 bit
    pub content_exist_flag: u8,             // 1 bit
    pub video_format: u8, // 4 bits (1=480i, 2=576i, 3=480p, 4=1080i, 5=720p, 6=1080p, 7=576p, 8=2160p)
    pub frame_rate: u8,   // 4 bits (1=23.976, 2=24, 3=25, 4=29.97, 6=50, 7=59.94)
}

/// Object referenced by an `IndexEntry` — either an HDMV movie-object
/// id or a BD-J bdjo file-name stem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexObjectType {
    Hdmv {
        playback_type: u8,
        movie_object_id_ref: u16,
    },
    BdJ {
        playback_type: u8,
        /// 5-char ASCII stem (e.g. "00000").
        bdjo_file_name: String,
    },
}

/// One title entry in `index.bdmv`. The first two entries are special
/// (FirstPlaybackTitle and MenuTitle); the remainder are the playable
/// title list addressable as title 1..=N.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub object: IndexObjectType,
}

/// Parsed `index.bdmv`.
#[derive(Debug, Clone)]
pub struct IndexBdmv {
    pub version: [u8; 4],
    pub app_info: AppInfoBdmv,
    pub first_playback_title: IndexEntry,
    pub menu_title: IndexEntry,
    /// Playable titles, in disc order. The on-disc index 1 maps to
    /// `titles[0]`.
    pub titles: Vec<IndexEntry>,
}

impl IndexBdmv {
    pub fn parse(buf: &[u8]) -> Result<Self> {
        let header = BdmvHeader::parse(buf)?;
        if header.type_indicator != b"INDX" {
            return Err(BlurayError::malformed(format!(
                "index.bdmv type_indicator {:?}",
                header.type_indicator
            )));
        }
        let version = *header.version_number;

        if buf.len() < 40 {
            return Err(BlurayError::malformed(
                "index.bdmv truncated before AppInfo",
            ));
        }
        let mut r = Reader::new(buf);
        r.seek(8)?;
        let indexes_start = r.read_u32()? as usize;
        let _extension_data_start = r.read_u32()? as usize;
        // 24 reserved bytes at offset 16..40 (we don't enforce zero — some
        // real-world authoring tools leave debris).
        r.seek(40)?;

        // AppInfoBDMV
        let app_len = r.read_u32()? as usize;
        if app_len < 2 {
            return Err(BlurayError::malformed("AppInfoBDMV length < 2"));
        }
        let app_start = r.pos;
        let app_end = app_start + app_len;
        let b0 = r.read_u8()?;
        let b1 = r.read_u8()?;
        let app_info = AppInfoBdmv {
            initial_output_mode_preference: (b0 >> 3) & 1,
            content_exist_flag: (b0 >> 2) & 1,
            video_format: (b1 >> 4) & 0xF,
            frame_rate: b1 & 0xF,
        };
        // Skip remainder of AppInfo (32 bytes of user_data + any future
        // extensions absorbed by `length`).
        r.seek(app_end)?;

        // Indexes()
        r.seek(indexes_start)?;
        let idx_len = r.read_u32()? as usize;
        let idx_end = r.pos + idx_len;
        if idx_end > buf.len() {
            return Err(BlurayError::malformed("Indexes overruns buffer"));
        }
        let first_playback = parse_object(&mut r)?;
        let menu = parse_object(&mut r)?;
        let n = r.read_u16()? as usize;
        let mut titles = Vec::with_capacity(n);
        for _ in 0..n {
            titles.push(parse_object(&mut r)?);
        }

        Ok(Self {
            version,
            app_info,
            first_playback_title: IndexEntry {
                object: first_playback,
            },
            menu_title: IndexEntry { object: menu },
            titles: titles
                .into_iter()
                .map(|object| IndexEntry { object })
                .collect(),
        })
    }

    /// Encode an `IndexBdmv` back to bytes. Only used by tests to
    /// forge synthetic input — production decoders never call this.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"INDX");
        out.extend_from_slice(&self.version);
        // Placeholder for indexes_start (filled below).
        out.extend_from_slice(&[0u8; 4]);
        // extension_data_start = 0
        out.extend_from_slice(&[0u8; 4]);
        // 24 reserved bytes
        out.extend_from_slice(&[0u8; 24]);

        // AppInfo
        let app_payload_len = 2 /* flag bytes */ + 32 /* user_data */;
        out.extend_from_slice(&(app_payload_len as u32).to_be_bytes());
        let b0 = ((self.app_info.initial_output_mode_preference & 1) << 3)
            | ((self.app_info.content_exist_flag & 1) << 2);
        let b1 = ((self.app_info.video_format & 0xF) << 4) | (self.app_info.frame_rate & 0xF);
        out.push(b0);
        out.push(b1);
        out.extend_from_slice(&[0u8; 32]); // user_data

        // Record Indexes() start.
        let indexes_start = out.len();
        // Length placeholder (re-filled below).
        out.extend_from_slice(&[0u8; 4]);
        let indexes_body_start = out.len();
        encode_object(&mut out, &self.first_playback_title.object);
        encode_object(&mut out, &self.menu_title.object);
        out.extend_from_slice(&(self.titles.len() as u16).to_be_bytes());
        for t in &self.titles {
            encode_object(&mut out, &t.object);
        }
        let indexes_body_len = (out.len() - indexes_body_start) as u32;
        out[indexes_start..indexes_start + 4].copy_from_slice(&indexes_body_len.to_be_bytes());

        // Backfill indexes_start at offset 8.
        out[8..12].copy_from_slice(&(indexes_start as u32).to_be_bytes());
        out
    }
}

fn parse_object(r: &mut Reader<'_>) -> Result<IndexObjectType> {
    let b0 = r.read_u8()?;
    let object_type = (b0 >> 6) & 0b11;
    // skip reserved byte
    let _ = r.read_u8()?;
    let pb_bits = r.read_u16()?;
    let playback_type = ((pb_bits >> 14) & 0b11) as u8;
    let rest = r.slice(8)?; // 8 bytes of HDMV / BD-J variant
    match object_type {
        1 => {
            let movie_object_id_ref = u16::from_be_bytes([rest[0], rest[1]]);
            // rest[2..8] = reserved u32 + 16-bit reserved padding (total 6)
            Ok(IndexObjectType::Hdmv {
                playback_type,
                movie_object_id_ref,
            })
        }
        2 => {
            let name = std::str::from_utf8(&rest[0..5])
                .map_err(|_| BlurayError::malformed("BD-J bdjo_file_name not ASCII"))?;
            Ok(IndexObjectType::BdJ {
                playback_type,
                bdjo_file_name: name.to_string(),
            })
        }
        v => Err(BlurayError::malformed(format!(
            "unknown object_type {v} in index.bdmv"
        ))),
    }
}

fn encode_object(out: &mut Vec<u8>, obj: &IndexObjectType) {
    match obj {
        IndexObjectType::Hdmv {
            playback_type,
            movie_object_id_ref,
        } => {
            let b0 = 0b01_000000;
            out.push(b0);
            out.push(0);
            let pb_bits: u16 = (*playback_type as u16 & 0b11) << 14;
            out.extend_from_slice(&pb_bits.to_be_bytes());
            out.extend_from_slice(&movie_object_id_ref.to_be_bytes());
            // 6 bytes of reserved
            out.extend_from_slice(&[0u8; 6]);
        }
        IndexObjectType::BdJ {
            playback_type,
            bdjo_file_name,
        } => {
            let b0 = 0b10_000000;
            out.push(b0);
            out.push(0);
            let pb_bits: u16 = (*playback_type as u16 & 0b11) << 14;
            out.extend_from_slice(&pb_bits.to_be_bytes());
            let mut name = [0u8; 5];
            let bytes = bdjo_file_name.as_bytes();
            let take = bytes.len().min(5);
            name[..take].copy_from_slice(&bytes[..take]);
            out.extend_from_slice(&name);
            // 3 bytes reserved
            out.extend_from_slice(&[0u8; 3]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_index() -> IndexBdmv {
        IndexBdmv {
            version: *b"0200",
            app_info: AppInfoBdmv {
                initial_output_mode_preference: 0,
                content_exist_flag: 1,
                video_format: 6, // 1080p
                frame_rate: 4,   // 29.97
            },
            first_playback_title: IndexEntry {
                object: IndexObjectType::Hdmv {
                    playback_type: 0,
                    movie_object_id_ref: 0,
                },
            },
            menu_title: IndexEntry {
                object: IndexObjectType::Hdmv {
                    playback_type: 1,
                    movie_object_id_ref: 1,
                },
            },
            titles: vec![
                IndexEntry {
                    object: IndexObjectType::Hdmv {
                        playback_type: 0,
                        movie_object_id_ref: 2,
                    },
                },
                IndexEntry {
                    object: IndexObjectType::BdJ {
                        playback_type: 0,
                        bdjo_file_name: "00000".into(),
                    },
                },
            ],
        }
    }

    #[test]
    fn round_trip() {
        let idx = sample_index();
        let bytes = idx.encode();
        let parsed = IndexBdmv::parse(&bytes).unwrap();
        assert_eq!(parsed.version, idx.version);
        assert_eq!(parsed.app_info, idx.app_info);
        assert_eq!(parsed.first_playback_title, idx.first_playback_title);
        assert_eq!(parsed.menu_title, idx.menu_title);
        assert_eq!(parsed.titles, idx.titles);
    }

    #[test]
    fn rejects_wrong_type_indicator() {
        let mut bytes = sample_index().encode();
        bytes[0] = b'X';
        assert!(IndexBdmv::parse(&bytes).is_err());
    }
}
