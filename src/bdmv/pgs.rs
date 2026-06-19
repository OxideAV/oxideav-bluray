//! Presentation Graphic Stream (PGS) — the bitmap-subtitle / graphics
//! wire format carried in the Presentation Graphics (PG) elementary
//! stream of a Blu-ray Disc (and the on-disc-equivalent of a standalone
//! `.sup` file).
//!
//! This module parses the PGS *segment wire format*: the shared 13-byte
//! PG segment header, the five segment-type bodies (PCS / WDS / PDS /
//! ODS / END), and the ODS run-length-encoded paletted bitmap. The crate
//! already enumerates PG tracks in its [`crate::TrackCatalogue`]; this
//! turns each PG track's PES payload into structured Display Sets a
//! downstream renderer can composite.
//!
//! A PG elementary stream is a sequence of **Display Sets (DS)**, each
//! one a run of segments framed as:
//!
//! ```text
//! PCS  →  WDS  →  PDS …  →  ODS …  →  END
//! ```
//!
//! All multi-byte fields are big-endian.
//!
//! Clean-room reference: `docs/container/bluray/pgs-segment-syntax.md`
//! (assembled from the BDA's structural disclosures in US patents
//! US 2009/0185789 A1 + US 7,912,305 B1 and the widely-republished
//! community PGS format description — no subtitle-decoder source read).

use crate::error::{BlurayError, Result};

/// The fixed 13-byte PG segment header that prefixes every segment.
///
/// Layout (`docs/container/bluray/pgs-segment-syntax.md` "PG segment
/// header"):
///
/// ```text
///   0  magic         "PG" = 0x50 0x47
///   2  pts           u32  (90 kHz units; ÷90 = ms)
///   6  dts           u32  (90 kHz units; often 0)
///  10  segment_type  u8   (see SegmentType)
///  11  segment_size  u16  (body byte count; excludes these 13 bytes)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentHeader {
    /// Presentation timestamp, 90 kHz units.
    pub pts: u32,
    /// Decoding timestamp, 90 kHz units (often `0`).
    pub dts: u32,
    /// Raw `segment_type` byte.
    pub segment_type: u8,
    /// Length of the segment body that follows the header.
    pub segment_size: u16,
}

impl SegmentHeader {
    /// On-wire size of the header itself.
    pub const SIZE: usize = 13;
    /// The ASCII magic `"PG"`.
    pub const MAGIC: [u8; 2] = *b"PG";

    /// Parse the 13-byte header from the front of `buf`.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < Self::SIZE {
            return Err(BlurayError::malformed("PG segment header truncated"));
        }
        if buf[0..2] != Self::MAGIC {
            return Err(BlurayError::malformed("PG segment header bad magic"));
        }
        let pts = u32::from_be_bytes([buf[2], buf[3], buf[4], buf[5]]);
        let dts = u32::from_be_bytes([buf[6], buf[7], buf[8], buf[9]]);
        let segment_type = buf[10];
        let segment_size = u16::from_be_bytes([buf[11], buf[12]]);
        Ok(Self {
            pts,
            dts,
            segment_type,
            segment_size,
        })
    }

    /// Typed view of the `segment_type` byte.
    pub fn kind(&self) -> SegmentType {
        SegmentType::from_raw(self.segment_type)
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&Self::MAGIC);
        out.extend_from_slice(&self.pts.to_be_bytes());
        out.extend_from_slice(&self.dts.to_be_bytes());
        out.push(self.segment_type);
        out.extend_from_slice(&self.segment_size.to_be_bytes());
    }
}

/// `segment_type` enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentType {
    /// Palette Definition Segment (`0x14`).
    Pds,
    /// Object Definition Segment (`0x15`).
    Ods,
    /// Presentation Composition Segment (`0x16`).
    Pcs,
    /// Window Definition Segment (`0x17`).
    Wds,
    /// END of Display Set Segment (`0x80`).
    End,
    /// Any other / reserved value.
    Other(u8),
}

impl SegmentType {
    pub fn from_raw(v: u8) -> Self {
        match v {
            0x14 => Self::Pds,
            0x15 => Self::Ods,
            0x16 => Self::Pcs,
            0x17 => Self::Wds,
            0x80 => Self::End,
            other => Self::Other(other),
        }
    }

    pub fn as_raw(self) -> u8 {
        match self {
            Self::Pds => 0x14,
            Self::Ods => 0x15,
            Self::Pcs => 0x16,
            Self::Wds => 0x17,
            Self::End => 0x80,
            Self::Other(v) => v,
        }
    }
}

/// `composition_state` of a [`Pcs`] — how the Display Set relates to its
/// predecessor (PGS doc "composition_state values").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositionState {
    /// `0x00` — "display update": only the diff vs the preceding
    /// composition.
    Normal,
    /// `0x40` — "display refresh": a self-sufficient mid-Epoch snapshot
    /// (Acquisition Point).
    AcquisitionPoint,
    /// `0x80` — "new display": begins a new Epoch; carries everything.
    EpochStart,
    /// Any other / reserved value.
    Other(u8),
}

impl CompositionState {
    pub fn from_raw(v: u8) -> Self {
        match v {
            0x00 => Self::Normal,
            0x40 => Self::AcquisitionPoint,
            0x80 => Self::EpochStart,
            other => Self::Other(other),
        }
    }

    pub fn as_raw(self) -> u8 {
        match self {
            Self::Normal => 0x00,
            Self::AcquisitionPoint => 0x40,
            Self::EpochStart => 0x80,
            Self::Other(v) => v,
        }
    }

    /// True for an `Epoch Start` DS — the point at which window geometry
    /// and object buffer are (re)initialised.
    pub fn is_epoch_start(self) -> bool {
        matches!(self, Self::EpochStart)
    }
}

/// One composition-object record inside a [`Pcs`].
///
/// Base size 8 bytes; the four cropping fields are present only when
/// `object_cropped_flag == 0x40` (16 bytes total).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompositionObject {
    /// The ODS object referenced.
    pub object_id: u16,
    /// The WDS window it is drawn into.
    pub window_id: u8,
    /// Raw `object_cropped_flag` byte (`0x40` = cropped / force-display).
    pub object_cropped_flag: u8,
    /// X on the graphics plane.
    pub object_horizontal_position: u16,
    /// Y on the graphics plane.
    pub object_vertical_position: u16,
    /// Cropping rectangle — present iff `object_cropped_flag == 0x40`.
    pub cropping: Option<CompositionObjectCrop>,
}

/// The four cropping fields present when a [`CompositionObject`] is
/// cropped (`object_cropped_flag == 0x40`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompositionObjectCrop {
    pub horizontal_position: u16,
    pub vertical_position: u16,
    pub width: u16,
    pub height: u16,
}

impl CompositionObject {
    /// The sentinel value of `object_cropped_flag` that signals the four
    /// cropping fields follow.
    pub const CROPPED_FLAG: u8 = 0x40;

    /// True when the cropping rectangle is present.
    pub fn is_cropped(&self) -> bool {
        self.cropping.is_some()
    }
}

/// Presentation Composition Segment (`0x16`) body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pcs {
    /// Graphics-plane width (px).
    pub width: u16,
    /// Graphics-plane height (px).
    pub height: u16,
    /// `frame_rate` byte (always `0x10` in practice).
    pub frame_rate: u8,
    /// Increments each Graphics Update.
    pub composition_number: u16,
    /// Raw `composition_state` byte.
    pub composition_state: u8,
    /// `palette_update_flag` byte (`0x80` = palette-only update).
    pub palette_update_flag: u8,
    /// The palette (PDS) used for this composition.
    pub palette_id: u8,
    /// The composition-object records.
    pub composition_objects: Vec<CompositionObject>,
}

impl Pcs {
    /// Value of `palette_update_flag` for a palette-only update.
    pub const PALETTE_UPDATE: u8 = 0x80;

    /// Typed view of the `composition_state` byte.
    pub fn state(&self) -> CompositionState {
        CompositionState::from_raw(self.composition_state)
    }

    /// True when this composition is a palette-only update.
    pub fn is_palette_update(&self) -> bool {
        self.palette_update_flag == Self::PALETTE_UPDATE
    }

    fn parse(body: &[u8]) -> Result<Self> {
        let mut r = BodyReader::new(body);
        let width = r.u16()?;
        let height = r.u16()?;
        let frame_rate = r.u8()?;
        let composition_number = r.u16()?;
        let composition_state = r.u8()?;
        let palette_update_flag = r.u8()?;
        let palette_id = r.u8()?;
        let n = r.u8()? as usize;
        let mut composition_objects = Vec::with_capacity(n);
        for _ in 0..n {
            let object_id = r.u16()?;
            let window_id = r.u8()?;
            let object_cropped_flag = r.u8()?;
            let object_horizontal_position = r.u16()?;
            let object_vertical_position = r.u16()?;
            let cropping = if object_cropped_flag == CompositionObject::CROPPED_FLAG {
                Some(CompositionObjectCrop {
                    horizontal_position: r.u16()?,
                    vertical_position: r.u16()?,
                    width: r.u16()?,
                    height: r.u16()?,
                })
            } else {
                None
            };
            composition_objects.push(CompositionObject {
                object_id,
                window_id,
                object_cropped_flag,
                object_horizontal_position,
                object_vertical_position,
                cropping,
            });
        }
        Ok(Self {
            width,
            height,
            frame_rate,
            composition_number,
            composition_state,
            palette_update_flag,
            palette_id,
            composition_objects,
        })
    }

    fn encode_body(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.width.to_be_bytes());
        out.extend_from_slice(&self.height.to_be_bytes());
        out.push(self.frame_rate);
        out.extend_from_slice(&self.composition_number.to_be_bytes());
        out.push(self.composition_state);
        out.push(self.palette_update_flag);
        out.push(self.palette_id);
        out.push(self.composition_objects.len() as u8);
        for o in &self.composition_objects {
            out.extend_from_slice(&o.object_id.to_be_bytes());
            out.push(o.window_id);
            out.push(o.object_cropped_flag);
            out.extend_from_slice(&o.object_horizontal_position.to_be_bytes());
            out.extend_from_slice(&o.object_vertical_position.to_be_bytes());
            if let Some(c) = o.cropping {
                out.extend_from_slice(&c.horizontal_position.to_be_bytes());
                out.extend_from_slice(&c.vertical_position.to_be_bytes());
                out.extend_from_slice(&c.width.to_be_bytes());
                out.extend_from_slice(&c.height.to_be_bytes());
            }
        }
    }
}

/// One window record inside a [`Wds`] (9 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub window_id: u8,
    pub horizontal_position: u16,
    pub vertical_position: u16,
    pub width: u16,
    pub height: u16,
}

/// Window Definition Segment (`0x17`) body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wds {
    pub windows: Vec<Window>,
}

impl Wds {
    fn parse(body: &[u8]) -> Result<Self> {
        let mut r = BodyReader::new(body);
        let n = r.u8()? as usize;
        let mut windows = Vec::with_capacity(n);
        for _ in 0..n {
            windows.push(Window {
                window_id: r.u8()?,
                horizontal_position: r.u16()?,
                vertical_position: r.u16()?,
                width: r.u16()?,
                height: r.u16()?,
            });
        }
        Ok(Self { windows })
    }

    fn encode_body(&self, out: &mut Vec<u8>) {
        out.push(self.windows.len() as u8);
        for w in &self.windows {
            out.push(w.window_id);
            out.extend_from_slice(&w.horizontal_position.to_be_bytes());
            out.extend_from_slice(&w.vertical_position.to_be_bytes());
            out.extend_from_slice(&w.width.to_be_bytes());
            out.extend_from_slice(&w.height.to_be_bytes());
        }
    }
}

/// One palette (CLUT) entry inside a [`Pds`] (5 bytes). Colour is
/// YCbCr + alpha (BT.709 range), not RGB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaletteEntry {
    /// CLUT index (0–255).
    pub palette_entry_id: u8,
    /// Luminance.
    pub y: u8,
    /// Color Difference Red.
    pub cr: u8,
    /// Color Difference Blue.
    pub cb: u8,
    /// Transparency (`0x00` = fully transparent, `0xFF` = opaque).
    pub alpha: u8,
}

/// Palette Definition Segment (`0x14`) body. The entry count is derived
/// from the body length: `(segment_size − 2) / 5`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pds {
    pub palette_id: u8,
    pub palette_version_number: u8,
    pub entries: Vec<PaletteEntry>,
}

impl Pds {
    fn parse(body: &[u8]) -> Result<Self> {
        let mut r = BodyReader::new(body);
        let palette_id = r.u8()?;
        let palette_version_number = r.u8()?;
        // The remaining body is a whole number of 5-byte entries.
        if r.remaining() % 5 != 0 {
            return Err(BlurayError::malformed(
                "PDS body not a whole number of 5-byte palette entries",
            ));
        }
        let n = r.remaining() / 5;
        let mut entries = Vec::with_capacity(n);
        for _ in 0..n {
            entries.push(PaletteEntry {
                palette_entry_id: r.u8()?,
                y: r.u8()?,
                cr: r.u8()?,
                cb: r.u8()?,
                alpha: r.u8()?,
            });
        }
        Ok(Self {
            palette_id,
            palette_version_number,
            entries,
        })
    }

    fn encode_body(&self, out: &mut Vec<u8>) {
        out.push(self.palette_id);
        out.push(self.palette_version_number);
        for e in &self.entries {
            out.push(e.palette_entry_id);
            out.push(e.y);
            out.push(e.cr);
            out.push(e.cb);
            out.push(e.alpha);
        }
    }
}

/// Fragment-sequence flags of an [`Ods`] (`last_in_sequence_flag`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FragmentFlag {
    /// `0x80` — first fragment in a multi-ODS sequence.
    First,
    /// `0x40` — last fragment in a multi-ODS sequence.
    Last,
    /// `0xC0` — first and last (the whole object in one ODS).
    FirstAndLast,
    /// Any other / reserved value.
    Other(u8),
}

impl FragmentFlag {
    pub fn from_raw(v: u8) -> Self {
        match v {
            0x80 => Self::First,
            0x40 => Self::Last,
            0xC0 => Self::FirstAndLast,
            other => Self::Other(other),
        }
    }

    pub fn as_raw(self) -> u8 {
        match self {
            Self::First => 0x80,
            Self::Last => 0x40,
            Self::FirstAndLast => 0xC0,
            Self::Other(v) => v,
        }
    }

    /// True when this fragment carries the leading `width`/`height`
    /// header (`First` or `FirstAndLast`).
    pub fn is_first(self) -> bool {
        matches!(self, Self::First | Self::FirstAndLast)
    }

    /// True when this fragment closes the object (`Last` or
    /// `FirstAndLast`).
    pub fn is_last(self) -> bool {
        matches!(self, Self::Last | Self::FirstAndLast)
    }
}

/// Object Definition Segment (`0x15`) body — one RLE-compressed paletted
/// bitmap fragment.
///
/// The `width`/`height` fields are present only in the first fragment
/// (the one whose flag is `First` / `FirstAndLast`). Continuation
/// fragments carry header bytes 0–6 (object_id, version, flag, length)
/// followed directly by more RLE bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ods {
    pub object_id: u16,
    pub object_version_number: u8,
    pub last_in_sequence_flag: u8,
    /// `object_data_length`: per the community wire-observation this
    /// counts `width + height + RLE` bytes on the first fragment, so the
    /// raw RLE byte count is `object_data_length − 4` there. Stored
    /// verbatim; [`Ods::rle_data`] returns the bitmap bytes.
    pub object_data_length: u32,
    /// Object width in pixels (first fragment only).
    pub width: Option<u16>,
    /// Object height in pixels (first fragment only).
    pub height: Option<u16>,
    /// The RLE-encoded bitmap bytes for this fragment.
    pub object_data: Vec<u8>,
}

impl Ods {
    /// Typed view of the `last_in_sequence_flag`.
    pub fn fragment(&self) -> FragmentFlag {
        FragmentFlag::from_raw(self.last_in_sequence_flag)
    }

    /// The RLE bitmap bytes of this fragment.
    pub fn rle_data(&self) -> &[u8] {
        &self.object_data
    }

    fn parse(body: &[u8]) -> Result<Self> {
        let mut r = BodyReader::new(body);
        let object_id = r.u16()?;
        let object_version_number = r.u8()?;
        let last_in_sequence_flag = r.u8()?;
        let object_data_length = r.u24()?;
        let flag = FragmentFlag::from_raw(last_in_sequence_flag);
        let (width, height) = if flag.is_first() {
            (Some(r.u16()?), Some(r.u16()?))
        } else {
            (None, None)
        };
        let object_data = r.rest().to_vec();
        Ok(Self {
            object_id,
            object_version_number,
            last_in_sequence_flag,
            object_data_length,
            width,
            height,
            object_data,
        })
    }

    fn encode_body(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.object_id.to_be_bytes());
        out.push(self.object_version_number);
        out.push(self.last_in_sequence_flag);
        let l = self.object_data_length & 0x00FF_FFFF;
        out.push((l >> 16) as u8);
        out.push((l >> 8) as u8);
        out.push(l as u8);
        if let Some(w) = self.width {
            out.extend_from_slice(&w.to_be_bytes());
        }
        if let Some(h) = self.height {
            out.extend_from_slice(&h.to_be_bytes());
        }
        out.extend_from_slice(&self.object_data);
    }
}

/// A parsed PGS segment: the shared header plus its typed body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub header: SegmentHeader,
    pub body: SegmentBody,
}

/// The typed body of a [`Segment`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentBody {
    Pcs(Pcs),
    Wds(Wds),
    Pds(Pds),
    Ods(Ods),
    /// END of Display Set — empty body.
    End,
    /// An unrecognised `segment_type`; the raw body bytes are retained.
    Other {
        segment_type: u8,
        data: Vec<u8>,
    },
}

impl Segment {
    /// Parse one segment from the front of `buf` and return it plus the
    /// total number of bytes consumed (`13 + segment_size`).
    pub fn parse(buf: &[u8]) -> Result<(Self, usize)> {
        let header = SegmentHeader::parse(buf)?;
        let body_len = header.segment_size as usize;
        let total = SegmentHeader::SIZE + body_len;
        if buf.len() < total {
            return Err(BlurayError::malformed("PG segment body truncated"));
        }
        let body_bytes = &buf[SegmentHeader::SIZE..total];
        let body = match header.kind() {
            SegmentType::Pcs => SegmentBody::Pcs(Pcs::parse(body_bytes)?),
            SegmentType::Wds => SegmentBody::Wds(Wds::parse(body_bytes)?),
            SegmentType::Pds => SegmentBody::Pds(Pds::parse(body_bytes)?),
            SegmentType::Ods => SegmentBody::Ods(Ods::parse(body_bytes)?),
            SegmentType::End => {
                if body_len != 0 {
                    return Err(BlurayError::malformed("END segment with non-zero body"));
                }
                SegmentBody::End
            }
            SegmentType::Other(t) => SegmentBody::Other {
                segment_type: t,
                data: body_bytes.to_vec(),
            },
        };
        Ok((Self { header, body }, total))
    }

    /// Encode this segment (header + body) back to bytes, recomputing the
    /// `segment_type` and `segment_size` header fields from the body.
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        let segment_type = match &self.body {
            SegmentBody::Pcs(p) => {
                p.encode_body(&mut body);
                SegmentType::Pcs.as_raw()
            }
            SegmentBody::Wds(w) => {
                w.encode_body(&mut body);
                SegmentType::Wds.as_raw()
            }
            SegmentBody::Pds(p) => {
                p.encode_body(&mut body);
                SegmentType::Pds.as_raw()
            }
            SegmentBody::Ods(o) => {
                o.encode_body(&mut body);
                SegmentType::Ods.as_raw()
            }
            SegmentBody::End => SegmentType::End.as_raw(),
            SegmentBody::Other { segment_type, data } => {
                body.extend_from_slice(data);
                *segment_type
            }
        };
        let header = SegmentHeader {
            pts: self.header.pts,
            dts: self.header.dts,
            segment_type,
            segment_size: body.len() as u16,
        };
        let mut out = Vec::with_capacity(SegmentHeader::SIZE + body.len());
        header.encode_into(&mut out);
        out.extend_from_slice(&body);
        out
    }
}

/// Parse a whole PG elementary byte stream (e.g. a `.sup` file or a PG
/// PES payload) into a flat list of segments, in stream order.
pub fn parse_segments(buf: &[u8]) -> Result<Vec<Segment>> {
    let mut segments = Vec::new();
    let mut pos = 0;
    while pos < buf.len() {
        let (seg, used) = Segment::parse(&buf[pos..])?;
        if used == 0 {
            return Err(BlurayError::malformed("PG segment zero-length advance"));
        }
        pos += used;
        segments.push(seg);
    }
    Ok(segments)
}

/// A decoded paletted bitmap: `width × height` CLUT indices, row-major.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedObject {
    pub width: u16,
    pub height: u16,
    /// `width × height` palette indices, row-major (top-left first).
    pub pixels: Vec<u8>,
}

/// Decode the ODS run-length-encoded bitmap into `width × height` CLUT
/// indices.
///
/// The encoding is byte-oriented, per scanline (each line ends with an
/// all-zeros 2-byte end-of-line code). Code words are 1–4 bytes; the
/// four run branches are selected by the top two bits of the byte
/// following a `0x00` escape (PGS doc "ODS run-length encoding"):
///
/// ```text
///   CCCCCCCC (C≠0)                       1B  one pixel of colour C
///   00000000 00LLLLLL                    2B  L pixels of colour 0  (L 1..63)
///   00000000 01LLLLLL LLLLLLLL           3B  L pixels of colour 0  (14-bit L)
///   00000000 10LLLLLL CCCCCCCC           3B  L pixels of colour C  (L 3..63)
///   00000000 11LLLLLL LLLLLLLL CCCCCCCC  4B  L pixels of colour C  (14-bit L)
///   00000000 00000000                    2B  end of line
/// ```
///
/// Each line must decode to exactly `width` pixels and there must be
/// exactly `height` lines; otherwise the data is rejected as malformed.
pub fn decode_rle(data: &[u8], width: u16, height: u16) -> Result<DecodedObject> {
    let w = width as usize;
    let h = height as usize;
    let mut pixels = Vec::with_capacity(w.saturating_mul(h));
    let mut i = 0;
    let mut line_len = 0usize;
    let mut lines = 0usize;

    // Push a run while keeping the per-line width within bounds.
    let push_run =
        |pixels: &mut Vec<u8>, line_len: &mut usize, color: u8, count: usize| -> Result<()> {
            if *line_len + count > w {
                return Err(BlurayError::malformed("PGS RLE run overruns object width"));
            }
            pixels.resize(pixels.len() + count, color);
            *line_len += count;
            Ok(())
        };

    while i < data.len() {
        let b0 = data[i];
        if b0 != 0 {
            // Single literal pixel.
            push_run(&mut pixels, &mut line_len, b0, 1)?;
            i += 1;
            continue;
        }
        // Escape: need at least one more byte.
        if i + 1 >= data.len() {
            return Err(BlurayError::malformed("PGS RLE truncated after escape"));
        }
        let b1 = data[i + 1];
        if b1 == 0 {
            // End of line.
            if line_len != w {
                return Err(BlurayError::malformed("PGS RLE short scanline"));
            }
            lines += 1;
            if lines > h {
                return Err(BlurayError::malformed("PGS RLE too many scanlines"));
            }
            line_len = 0;
            i += 2;
            continue;
        }
        let branch = b1 >> 6;
        match branch {
            0b00 => {
                // Short run of colour 0; length in low 6 bits.
                let len = (b1 & 0x3F) as usize;
                push_run(&mut pixels, &mut line_len, 0, len)?;
                i += 2;
            }
            0b01 => {
                // Long run of colour 0; 14-bit length across b1(low6)+b2.
                if i + 2 >= data.len() {
                    return Err(BlurayError::malformed("PGS RLE truncated long run"));
                }
                let len = (((b1 & 0x3F) as usize) << 8) | data[i + 2] as usize;
                push_run(&mut pixels, &mut line_len, 0, len)?;
                i += 3;
            }
            0b10 => {
                // Short run of colour C; length low 6 bits of b1, colour b2.
                if i + 2 >= data.len() {
                    return Err(BlurayError::malformed("PGS RLE truncated short colour run"));
                }
                let len = (b1 & 0x3F) as usize;
                let color = data[i + 2];
                push_run(&mut pixels, &mut line_len, color, len)?;
                i += 3;
            }
            _ => {
                // 0b11: long run of colour C; 14-bit length b1(low6)+b2, colour b3.
                if i + 3 >= data.len() {
                    return Err(BlurayError::malformed("PGS RLE truncated long colour run"));
                }
                let len = (((b1 & 0x3F) as usize) << 8) | data[i + 2] as usize;
                let color = data[i + 3];
                push_run(&mut pixels, &mut line_len, color, len)?;
                i += 4;
            }
        }
    }

    // A bitmap may omit the final end-of-line code; accept a complete
    // trailing line.
    if line_len == w && w != 0 {
        lines += 1;
        line_len = 0;
    }
    if line_len != 0 {
        return Err(BlurayError::malformed("PGS RLE trailing partial scanline"));
    }
    if lines != h {
        return Err(BlurayError::malformed("PGS RLE scanline count mismatch"));
    }
    Ok(DecodedObject {
        width,
        height,
        pixels,
    })
}

/// A **Display Set (DS)** — one screen-composition unit of a PG stream.
///
/// The PGS doc ("Stream framing") orders a DS as
/// `PCS → WDS → PDS … → ODS … → END`: exactly one PCS, an optional WDS
/// (present in `Epoch Start` / `Acquisition Point` DSs), zero or more
/// PDS, zero or more ODS (a large bitmap split across several ODS
/// fragments sharing one `object_id`), and a terminating END.
///
/// [`group_display_sets`] slices a flat segment list (from
/// [`parse_segments`]) into Display Sets on each PCS boundary;
/// [`DisplaySet::reassemble_objects`] then folds each ODS fragment chain
/// back into whole RLE objects ready for [`decode_rle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplaySet {
    /// The Presentation Composition Segment that opens the DS.
    pub pcs: Pcs,
    /// The Window Definition Segment, if present (Epoch Start /
    /// Acquisition Point DSs carry one; Normal-Case updates may omit it).
    pub wds: Option<Wds>,
    /// The Palette Definition Segments, in stream order.
    pub palettes: Vec<Pds>,
    /// The Object Definition Segment fragments, in stream order.
    pub objects: Vec<Ods>,
    /// The presentation timestamp of the opening PCS (90 kHz units).
    pub pts: u32,
}

impl DisplaySet {
    /// The composition state of this DS (Epoch Start / Acquisition
    /// Point / Normal Case).
    pub fn state(&self) -> CompositionState {
        self.pcs.state()
    }

    /// Reassemble the ODS fragment chains into whole RLE objects.
    ///
    /// Fragments sharing one `object_id` are concatenated in stream
    /// order: the chain must open with a `First` (or `FirstAndLast`)
    /// fragment carrying the `width`/`height`, continue through zero or
    /// more middle fragments, and close with a `Last` (or the same
    /// `FirstAndLast`). The first fragment's `object_data_length` counts
    /// the `width` + `height` (4 bytes) plus all RLE bytes across every
    /// fragment, so the expected RLE byte total is
    /// `object_data_length − 4` (the PGS doc's wire-observation caveat);
    /// the concatenated fragment payloads are validated against it.
    ///
    /// Returns one [`ReassembledObject`] per `object_id`, in first-seen
    /// order. Malformed chains (a continuation without an open `First`,
    /// a second `First` for the same id, a missing dimension, or a
    /// declared-length mismatch) are rejected.
    pub fn reassemble_objects(&self) -> Result<Vec<ReassembledObject>> {
        // (object_id, accumulator) in first-seen order; `open` marks a
        // chain still awaiting its `Last` fragment.
        let mut order: Vec<u16> = Vec::new();
        let mut acc: Vec<ReassembleAcc> = Vec::new();

        for ods in &self.objects {
            let flag = ods.fragment();
            let slot = acc.iter_mut().find(|a| a.object_id == ods.object_id);

            if flag.is_first() {
                if let Some(s) = slot {
                    if !s.closed {
                        return Err(BlurayError::malformed(
                            "PGS ODS second First fragment for an open object_id",
                        ));
                    }
                    // A later DS reusing the id would arrive in a
                    // different DisplaySet; within one DS a repeated
                    // First is malformed authoring.
                    return Err(BlurayError::malformed(
                        "PGS ODS duplicate object_id within a Display Set",
                    ));
                }
                let width = ods.width.ok_or_else(|| {
                    BlurayError::malformed("PGS ODS First fragment missing width")
                })?;
                let height = ods.height.ok_or_else(|| {
                    BlurayError::malformed("PGS ODS First fragment missing height")
                })?;
                order.push(ods.object_id);
                acc.push(ReassembleAcc {
                    object_id: ods.object_id,
                    version: ods.object_version_number,
                    width,
                    height,
                    declared_len: ods.object_data_length,
                    data: ods.object_data.clone(),
                    closed: flag.is_last(),
                });
            } else {
                // Continuation (Last / Other): must extend an open chain.
                let s = slot.ok_or_else(|| {
                    BlurayError::malformed("PGS ODS continuation fragment with no open object_id")
                })?;
                if s.closed {
                    return Err(BlurayError::malformed(
                        "PGS ODS continuation after the chain was already closed",
                    ));
                }
                s.data.extend_from_slice(&ods.object_data);
                if flag.is_last() {
                    s.closed = true;
                }
            }
        }

        let mut out = Vec::with_capacity(order.len());
        for s in acc {
            if !s.closed {
                return Err(BlurayError::malformed(
                    "PGS ODS object chain never received a Last fragment",
                ));
            }
            // declared_len counts width(2) + height(2) + RLE bytes.
            if (s.declared_len as usize) < 4 {
                return Err(BlurayError::malformed(
                    "PGS ODS object_data_length shorter than the 4 dimension bytes",
                ));
            }
            let expected_rle = s.declared_len as usize - 4;
            if s.data.len() != expected_rle {
                return Err(BlurayError::malformed(
                    "PGS ODS reassembled RLE length disagrees with object_data_length",
                ));
            }
            out.push(ReassembledObject {
                object_id: s.object_id,
                object_version_number: s.version,
                width: s.width,
                height: s.height,
                rle_data: s.data,
            });
        }
        Ok(out)
    }
}

/// Per-object accumulator used while reassembling ODS fragment chains.
struct ReassembleAcc {
    object_id: u16,
    version: u8,
    width: u16,
    height: u16,
    declared_len: u32,
    data: Vec<u8>,
    closed: bool,
}

/// A whole Graphics Object reassembled from one or more ODS fragments:
/// the dimensions plus the concatenated RLE byte stream. Decode the
/// `rle_data` with [`decode_rle`] (using `width`/`height`) to get the
/// paletted bitmap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReassembledObject {
    pub object_id: u16,
    pub object_version_number: u8,
    pub width: u16,
    pub height: u16,
    /// The concatenated RLE bytes across every fragment of the chain.
    pub rle_data: Vec<u8>,
}

impl ReassembledObject {
    /// Decode this object's reassembled RLE bytes into a paletted
    /// [`DecodedObject`].
    pub fn decode(&self) -> Result<DecodedObject> {
        decode_rle(&self.rle_data, self.width, self.height)
    }
}

/// Slice a flat PG segment list (from [`parse_segments`]) into
/// [`DisplaySet`]s, one per PCS.
///
/// A Display Set opens with a PCS and runs through the segments that
/// follow up to (and including) the next END, per the PGS doc framing
/// `PCS → WDS → PDS … → ODS … → END`. Within a DS the WDS / PDS / ODS
/// segments are bucketed by type; the END terminates the set. The first
/// segment of the stream must be a PCS, more than one PCS or more than
/// one WDS in a single DS is rejected, and any segment appearing before
/// the opening PCS is malformed.
pub fn group_display_sets(segments: &[Segment]) -> Result<Vec<DisplaySet>> {
    let mut sets = Vec::new();
    let mut cur: Option<DisplaySetBuilder> = None;

    for seg in segments {
        match &seg.body {
            SegmentBody::Pcs(pcs) => {
                if let Some(b) = cur.take() {
                    // A new PCS before the previous DS saw its END:
                    // close the previous set implicitly is unsafe, so
                    // reject — a well-formed stream ends each DS with END.
                    let _ = b;
                    return Err(BlurayError::malformed(
                        "PGS Display Set opened a second PCS before END",
                    ));
                }
                cur = Some(DisplaySetBuilder {
                    pcs: pcs.clone(),
                    pts: seg.header.pts,
                    wds: None,
                    palettes: Vec::new(),
                    objects: Vec::new(),
                });
            }
            SegmentBody::Wds(wds) => {
                let b = cur
                    .as_mut()
                    .ok_or_else(|| BlurayError::malformed("PGS WDS before any PCS"))?;
                if b.wds.is_some() {
                    return Err(BlurayError::malformed(
                        "PGS Display Set with more than one WDS",
                    ));
                }
                b.wds = Some(wds.clone());
            }
            SegmentBody::Pds(pds) => {
                let b = cur
                    .as_mut()
                    .ok_or_else(|| BlurayError::malformed("PGS PDS before any PCS"))?;
                b.palettes.push(pds.clone());
            }
            SegmentBody::Ods(ods) => {
                let b = cur
                    .as_mut()
                    .ok_or_else(|| BlurayError::malformed("PGS ODS before any PCS"))?;
                b.objects.push(ods.clone());
            }
            SegmentBody::End => {
                let b = cur
                    .take()
                    .ok_or_else(|| BlurayError::malformed("PGS END before any PCS"))?;
                sets.push(b.finish());
            }
            SegmentBody::Other { .. } => {
                // Unknown segment types inside a DS are ignored for
                // grouping (retained at the flat-segment layer); a stray
                // one before a PCS is still malformed.
                if cur.is_none() {
                    return Err(BlurayError::malformed("PGS unknown segment before any PCS"));
                }
            }
        }
    }

    if cur.is_some() {
        return Err(BlurayError::malformed(
            "PGS trailing Display Set without an END segment",
        ));
    }
    Ok(sets)
}

/// In-progress [`DisplaySet`] used by [`group_display_sets`].
struct DisplaySetBuilder {
    pcs: Pcs,
    pts: u32,
    wds: Option<Wds>,
    palettes: Vec<Pds>,
    objects: Vec<Ods>,
}

impl DisplaySetBuilder {
    fn finish(self) -> DisplaySet {
        DisplaySet {
            pcs: self.pcs,
            wds: self.wds,
            palettes: self.palettes,
            objects: self.objects,
            pts: self.pts,
        }
    }
}

/// Parse a whole PG elementary byte stream and group it into Display
/// Sets in one call — [`parse_segments`] followed by
/// [`group_display_sets`].
pub fn parse_display_sets(buf: &[u8]) -> Result<Vec<DisplaySet>> {
    let segments = parse_segments(buf)?;
    group_display_sets(&segments)
}

/// Tiny big-endian reader over a segment body. Mirrors
/// [`crate::bdmv::common::Reader`] but adds a 3-byte read and stays local
/// so PGS parsing keeps its own bounds-checking surface.
struct BodyReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> BodyReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos.min(self.buf.len())
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            return Err(BlurayError::malformed("PGS segment body read past end"));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let s = self.take(2)?;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }

    fn u24(&mut self) -> Result<u32> {
        let s = self.take(3)?;
        Ok(((s[0] as u32) << 16) | ((s[1] as u32) << 8) | s[2] as u32)
    }

    fn rest(&mut self) -> &'a [u8] {
        let s = &self.buf[self.pos..];
        self.pos = self.buf.len();
        s
    }
}

// ===========================================================================
// Renderer: palette resolution + window compositing
// ===========================================================================
//
// The layers above turn a PG / `.sup` byte stream into Display Sets, fold ODS
// fragments into whole RLE objects, and expand the RLE into `width × height`
// CLUT *indices* ([`DecodedObject`]). This section completes the decode by
// applying the PDS palette (YCbCr+alpha → RGBA) and compositing each
// composition object into the graphics plane at the position the PCS gives
// it — yielding the actual subtitle bitmap a player would alpha-blend onto
// the video plane.
//
// Colour conversion: PGS palette entries are **BT.709 limited-range** YCbCr
// (`docs/container/bluray/pgs-segment-syntax.md`, "Palette entry": *"Color is
// YCbCr + alpha (BT.709 range as used on BD), not RGB"*). The inverse matrix
// below is the standard BT.709 limited→full-range conversion (Y scaled from
// the 16–235 studio range, Cb/Cr from 16–240 around the 128 neutral point);
// the resulting R'G'B' are clamped to 0–255. Alpha passes through unchanged
// (`0x00` transparent … `0xFF` opaque).

/// A single straight-alpha RGBA pixel (8 bits per channel).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rgba8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba8 {
    /// A fully transparent pixel (all channels `0`).
    pub const TRANSPARENT: Rgba8 = Rgba8 {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    /// This pixel's four channels as `[r, g, b, a]`.
    pub fn to_array(self) -> [u8; 4] {
        [self.r, self.g, self.b, self.a]
    }
}

/// Convert one BT.709 limited-range `(Y, Cb, Cr)` triple to full-range
/// `(R, G, B)`, clamped to `0..=255`.
///
/// Studio range: `Y` in 16–235, `Cb`/`Cr` in 16–240 centred on 128. The
/// coefficients are the BT.709 inverse matrix; integer math is done in
/// fixed point (×256) to stay deterministic and `no_std`-friendly.
fn ycbcr709_to_rgb(y: u8, cb: u8, cr: u8) -> (u8, u8, u8) {
    // De-bias and scale out of the studio range into full range.
    let yf = (y as f32 - 16.0) * (255.0 / 219.0);
    let cbf = cb as f32 - 128.0;
    let crf = cr as f32 - 128.0;

    // BT.709 limited-range inverse coefficients. Chroma is carried in the
    // 16–240 (224-wide) studio range, so each chroma term is pre-scaled by
    // 255/224 folded into the published constants below.
    let r = yf + 1.5748 * crf * (255.0 / 224.0);
    let g = yf - 0.1873 * cbf * (255.0 / 224.0) - 0.4681 * crf * (255.0 / 224.0);
    let b = yf + 1.8556 * cbf * (255.0 / 224.0);

    let clamp = |v: f32| -> u8 {
        if v <= 0.0 {
            0
        } else if v >= 255.0 {
            255
        } else {
            (v + 0.5) as u8
        }
    };
    (clamp(r), clamp(g), clamp(b))
}

impl PaletteEntry {
    /// Resolve this CLUT entry to a straight-alpha [`Rgba8`] pixel, applying
    /// the BT.709 limited-range YCbCr→RGB conversion and passing the alpha
    /// (`T`) through unchanged.
    pub fn to_rgba(&self) -> Rgba8 {
        let (r, g, b) = ycbcr709_to_rgb(self.y, self.cr, self.cb);
        Rgba8 {
            r,
            g,
            b,
            a: self.alpha,
        }
    }
}

/// A resolved 256-entry CLUT (Colour Look-Up Table).
///
/// Built from one or more [`Pds`] segments. Entries a PDS does not mention
/// keep their previous value (palettes are incrementally updatable — the
/// PGS doc's *"Entries not present in a PDS keep their previous value"*); a
/// freshly-[`Palette::new`]'d table starts fully transparent, matching the
/// convention that an unwritten index (notably 255) is transparent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    entries: [Rgba8; 256],
}

impl Default for Palette {
    fn default() -> Self {
        Self::new()
    }
}

impl Palette {
    /// A fully-transparent 256-entry CLUT.
    pub fn new() -> Self {
        Self {
            entries: [Rgba8::TRANSPARENT; 256],
        }
    }

    /// Look up the resolved colour for a CLUT index.
    pub fn get(&self, index: u8) -> Rgba8 {
        self.entries[index as usize]
    }

    /// Apply one PDS's entries on top of the current table (incremental
    /// update): each `palette_entry_id` is overwritten, untouched indices
    /// keep their prior value.
    pub fn apply(&mut self, pds: &Pds) {
        for e in &pds.entries {
            self.entries[e.palette_entry_id as usize] = e.to_rgba();
        }
    }

    /// Build a CLUT from a slice of PDS applied in order (later entries win
    /// for a repeated index). Convenience for a Display Set's `palettes`.
    pub fn from_palettes(palettes: &[Pds]) -> Self {
        let mut p = Self::new();
        for pds in palettes {
            p.apply(pds);
        }
        p
    }

    /// Build a CLUT from the single PDS matching `palette_id` within a
    /// Display Set's palette list (the PCS names which palette to use via
    /// `palette_id`). Returns a fully-transparent palette if none matches.
    pub fn from_palettes_with_id(palettes: &[Pds], palette_id: u8) -> Self {
        let mut p = Self::new();
        for pds in palettes.iter().filter(|p| p.palette_id == palette_id) {
            p.apply(pds);
        }
        p
    }
}

/// A decoded, palette-resolved RGBA bitmap: `width × height` straight-alpha
/// pixels, row-major (top-left first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbaImage {
    pub width: u16,
    pub height: u16,
    /// `width × height` pixels, row-major.
    pub pixels: Vec<Rgba8>,
}

impl RgbaImage {
    /// A fully-transparent image of the given size.
    pub fn transparent(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            pixels: vec![Rgba8::TRANSPARENT; (width as usize) * (height as usize)],
        }
    }

    /// The pixel at `(x, y)`, or [`None`] if out of bounds.
    pub fn pixel(&self, x: u16, y: u16) -> Option<Rgba8> {
        if x >= self.width || y >= self.height {
            return None;
        }
        Some(self.pixels[(y as usize) * (self.width as usize) + (x as usize)])
    }

    /// Flatten to a tightly-packed `RGBA8888` byte buffer
    /// (`width × height × 4` bytes, row-major).
    pub fn to_rgba_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.pixels.len() * 4);
        for p in &self.pixels {
            out.extend_from_slice(&p.to_array());
        }
        out
    }
}

impl DecodedObject {
    /// Resolve this paletted bitmap's CLUT indices through `palette` to a
    /// straight-alpha [`RgbaImage`].
    pub fn to_rgba(&self, palette: &Palette) -> RgbaImage {
        let pixels = self.pixels.iter().map(|&idx| palette.get(idx)).collect();
        RgbaImage {
            width: self.width,
            height: self.height,
            pixels,
        }
    }
}

/// A fully composited Display Set: every composition object decoded,
/// palette-resolved, and painted into the PCS-declared graphics plane at the
/// position (and crop) the PCS gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedDisplaySet {
    /// The graphics-plane bitmap (`pcs.width × pcs.height`).
    pub plane: RgbaImage,
    /// Presentation timestamp of the opening PCS (90 kHz units).
    pub pts: u32,
}

impl DisplaySet {
    /// Decode, palette-resolve and composite this Display Set into a single
    /// graphics-plane [`RgbaImage`].
    ///
    /// The plane is sized `pcs.width × pcs.height` and starts fully
    /// transparent. The PCS's `palette_id` selects the active CLUT from this
    /// DS's [`palettes`](Self::palettes); each composition object's ODS is
    /// reassembled, RLE-decoded, palette-resolved, and painted at
    /// `(object_horizontal_position, object_vertical_position)`. When a
    /// composition object carries a cropping rectangle (`object_cropped_flag
    /// == 0x40`) only the cropped sub-rectangle of the object is painted, at
    /// the same plane position. Pixels that fall outside the plane are
    /// clipped.
    ///
    /// A `palette_update_flag` (palette-only) PCS still composites against
    /// whatever objects it references; callers tracking an Epoch's object
    /// buffer across DSs should drive the buffer themselves — this method
    /// renders strictly from the objects present in *this* DS.
    ///
    /// Errors propagate from [`reassemble_objects`](Self::reassemble_objects)
    /// (malformed fragment chains) and [`decode_rle`] (malformed RLE), plus a
    /// `Malformed` if a composition object references an `object_id` with no
    /// matching ODS in this DS.
    pub fn render(&self) -> Result<RenderedDisplaySet> {
        let palette = Palette::from_palettes_with_id(&self.palettes, self.pcs.palette_id);

        // Decode every object in this DS once, keyed by object_id.
        let reassembled = self.reassemble_objects()?;
        let mut images: Vec<(u16, RgbaImage)> = Vec::with_capacity(reassembled.len());
        for obj in &reassembled {
            let decoded = obj.decode()?;
            images.push((obj.object_id, decoded.to_rgba(&palette)));
        }

        let mut plane = RgbaImage::transparent(self.pcs.width, self.pcs.height);

        for co in &self.pcs.composition_objects {
            let img = images
                .iter()
                .find(|(id, _)| *id == co.object_id)
                .map(|(_, img)| img)
                .ok_or_else(|| {
                    BlurayError::malformed(
                        "PGS composition object references an object_id absent from the Display Set",
                    )
                })?;

            // Source sub-rectangle: the whole object, or the crop window.
            let (src_x, src_y, src_w, src_h) = match co.cropping {
                Some(c) => (
                    c.horizontal_position,
                    c.vertical_position,
                    c.width,
                    c.height,
                ),
                None => (0, 0, img.width, img.height),
            };

            blit(
                &mut plane,
                img,
                co.object_horizontal_position,
                co.object_vertical_position,
                src_x,
                src_y,
                src_w,
                src_h,
            );
        }

        Ok(RenderedDisplaySet {
            plane,
            pts: self.pts,
        })
    }
}

/// Copy the `src_w × src_h` sub-rectangle of `src` (top-left at
/// `(src_x, src_y)` in `src`) onto `dst` with its top-left at
/// `(dst_x, dst_y)`. Pixels falling outside either image are skipped
/// (clipped); this is a straight copy (overwrite), not an alpha blend — the
/// graphics plane starts transparent and PGS objects do not overlap within a
/// single DS in well-formed streams.
#[allow(clippy::too_many_arguments)]
fn blit(
    dst: &mut RgbaImage,
    src: &RgbaImage,
    dst_x: u16,
    dst_y: u16,
    src_x: u16,
    src_y: u16,
    src_w: u16,
    src_h: u16,
) {
    for row in 0..src_h {
        let sy = match src_y.checked_add(row) {
            Some(v) if v < src.height => v,
            _ => continue,
        };
        let dy = match dst_y.checked_add(row) {
            Some(v) if v < dst.height => v,
            _ => continue,
        };
        for col in 0..src_w {
            let sx = match src_x.checked_add(col) {
                Some(v) if v < src.width => v,
                _ => continue,
            };
            let dx = match dst_x.checked_add(col) {
                Some(v) if v < dst.width => v,
                _ => continue,
            };
            let sp = src.pixels[(sy as usize) * (src.width as usize) + (sx as usize)];
            dst.pixels[(dy as usize) * (dst.width as usize) + (dx as usize)] = sp;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(seg: &Segment) -> Segment {
        let bytes = seg.encode();
        let (parsed, used) = Segment::parse(&bytes).unwrap();
        assert_eq!(used, bytes.len());
        parsed
    }

    #[test]
    fn header_magic_and_fields() {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"PG");
        buf.extend_from_slice(&0x0001_2345u32.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.push(0x16);
        buf.extend_from_slice(&7u16.to_be_bytes());
        let h = SegmentHeader::parse(&buf).unwrap();
        assert_eq!(h.pts, 0x0001_2345);
        assert_eq!(h.dts, 0);
        assert_eq!(h.kind(), SegmentType::Pcs);
        assert_eq!(h.segment_size, 7);
    }

    #[test]
    fn header_bad_magic_rejected() {
        let mut buf = vec![b'X', b'G'];
        buf.extend_from_slice(&[0u8; 11]);
        assert!(SegmentHeader::parse(&buf).is_err());
    }

    #[test]
    fn segment_type_roundtrip() {
        for raw in [0x14u8, 0x15, 0x16, 0x17, 0x80, 0x00, 0xAB] {
            assert_eq!(SegmentType::from_raw(raw).as_raw(), raw);
        }
        assert_eq!(SegmentType::from_raw(0x16), SegmentType::Pcs);
        assert_eq!(SegmentType::from_raw(0x99), SegmentType::Other(0x99));
    }

    #[test]
    fn pcs_roundtrip_uncropped_and_cropped() {
        let pcs = Pcs {
            width: 1920,
            height: 1080,
            frame_rate: 0x10,
            composition_number: 3,
            composition_state: 0x80,
            palette_update_flag: 0x00,
            palette_id: 1,
            composition_objects: vec![
                CompositionObject {
                    object_id: 5,
                    window_id: 0,
                    object_cropped_flag: 0x00,
                    object_horizontal_position: 100,
                    object_vertical_position: 900,
                    cropping: None,
                },
                CompositionObject {
                    object_id: 6,
                    window_id: 1,
                    object_cropped_flag: CompositionObject::CROPPED_FLAG,
                    object_horizontal_position: 10,
                    object_vertical_position: 20,
                    cropping: Some(CompositionObjectCrop {
                        horizontal_position: 1,
                        vertical_position: 2,
                        width: 30,
                        height: 40,
                    }),
                },
            ],
        };
        let seg = Segment {
            header: SegmentHeader {
                pts: 90_000,
                dts: 0,
                segment_type: SegmentType::Pcs.as_raw(),
                segment_size: 0,
            },
            body: SegmentBody::Pcs(pcs.clone()),
        };
        let parsed = roundtrip(&seg);
        assert_eq!(parsed.body, SegmentBody::Pcs(pcs));
        if let SegmentBody::Pcs(p) = parsed.body {
            assert_eq!(p.state(), CompositionState::EpochStart);
            assert!(p.state().is_epoch_start());
            assert!(!p.is_palette_update());
            assert!(p.composition_objects[1].is_cropped());
            assert!(!p.composition_objects[0].is_cropped());
        }
    }

    #[test]
    fn pcs_palette_update_flag() {
        let pcs = Pcs {
            width: 1920,
            height: 1080,
            frame_rate: 0x10,
            composition_number: 0,
            composition_state: 0x00,
            palette_update_flag: Pcs::PALETTE_UPDATE,
            palette_id: 2,
            composition_objects: vec![],
        };
        assert!(pcs.is_palette_update());
        assert_eq!(pcs.state(), CompositionState::Normal);
    }

    #[test]
    fn wds_roundtrip() {
        let wds = Wds {
            windows: vec![
                Window {
                    window_id: 0,
                    horizontal_position: 10,
                    vertical_position: 20,
                    width: 300,
                    height: 100,
                },
                Window {
                    window_id: 1,
                    horizontal_position: 0,
                    vertical_position: 0,
                    width: 1920,
                    height: 1080,
                },
            ],
        };
        let seg = Segment {
            header: SegmentHeader {
                pts: 1,
                dts: 0,
                segment_type: SegmentType::Wds.as_raw(),
                segment_size: 0,
            },
            body: SegmentBody::Wds(wds.clone()),
        };
        assert_eq!(roundtrip(&seg).body, SegmentBody::Wds(wds));
    }

    #[test]
    fn pds_roundtrip_and_entry_count() {
        let pds = Pds {
            palette_id: 0,
            palette_version_number: 7,
            entries: vec![
                PaletteEntry {
                    palette_entry_id: 1,
                    y: 235,
                    cr: 128,
                    cb: 128,
                    alpha: 0xFF,
                },
                PaletteEntry {
                    palette_entry_id: 255,
                    y: 16,
                    cr: 128,
                    cb: 128,
                    alpha: 0x00,
                },
            ],
        };
        let seg = Segment {
            header: SegmentHeader {
                pts: 1,
                dts: 0,
                segment_type: SegmentType::Pds.as_raw(),
                segment_size: 0,
            },
            body: SegmentBody::Pds(pds.clone()),
        };
        let bytes = seg.encode();
        // body = 2 head + 2 entries × 5 = 12.
        assert_eq!(bytes.len(), SegmentHeader::SIZE + 12);
        assert_eq!(roundtrip(&seg).body, SegmentBody::Pds(pds));
    }

    #[test]
    fn pds_rejects_ragged_body() {
        // 2 head + 6 bytes is not a whole number of 5-byte entries.
        let mut buf = Vec::new();
        buf.extend_from_slice(b"PG");
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.push(0x14);
        buf.extend_from_slice(&8u16.to_be_bytes());
        buf.extend_from_slice(&[0u8; 8]);
        assert!(Segment::parse(&buf).is_err());
    }

    #[test]
    fn ods_roundtrip_first_and_last() {
        let ods = Ods {
            object_id: 9,
            object_version_number: 1,
            last_in_sequence_flag: FragmentFlag::FirstAndLast.as_raw(),
            object_data_length: 4 + 3,
            width: Some(8),
            height: Some(2),
            object_data: vec![0x01, 0x02, 0x03],
        };
        let seg = Segment {
            header: SegmentHeader {
                pts: 1,
                dts: 0,
                segment_type: SegmentType::Ods.as_raw(),
                segment_size: 0,
            },
            body: SegmentBody::Ods(ods.clone()),
        };
        let parsed = roundtrip(&seg);
        assert_eq!(parsed.body, SegmentBody::Ods(ods));
        if let SegmentBody::Ods(o) = parsed.body {
            assert_eq!(o.fragment(), FragmentFlag::FirstAndLast);
            assert!(o.fragment().is_first());
            assert!(o.fragment().is_last());
            assert_eq!(o.width, Some(8));
            assert_eq!(o.rle_data(), &[0x01, 0x02, 0x03]);
        }
    }

    #[test]
    fn ods_continuation_has_no_dimensions() {
        let ods = Ods {
            object_id: 9,
            object_version_number: 1,
            last_in_sequence_flag: FragmentFlag::Last.as_raw(),
            object_data_length: 0,
            width: None,
            height: None,
            object_data: vec![0xAA, 0xBB],
        };
        let seg = Segment {
            header: SegmentHeader {
                pts: 1,
                dts: 0,
                segment_type: SegmentType::Ods.as_raw(),
                segment_size: 0,
            },
            body: SegmentBody::Ods(ods.clone()),
        };
        let parsed = roundtrip(&seg);
        if let SegmentBody::Ods(o) = parsed.body {
            assert!(o.fragment().is_last());
            assert!(!o.fragment().is_first());
            assert_eq!(o.width, None);
            assert_eq!(o.height, None);
            assert_eq!(o.object_data, vec![0xAA, 0xBB]);
        } else {
            panic!("expected ODS");
        }
    }

    #[test]
    fn fragment_flag_roundtrip() {
        for raw in [0x80u8, 0x40, 0xC0, 0x00, 0x12] {
            assert_eq!(FragmentFlag::from_raw(raw).as_raw(), raw);
        }
    }

    #[test]
    fn end_segment_roundtrip_and_nonzero_rejected() {
        let seg = Segment {
            header: SegmentHeader {
                pts: 42,
                dts: 0,
                segment_type: SegmentType::End.as_raw(),
                segment_size: 0,
            },
            body: SegmentBody::End,
        };
        let bytes = seg.encode();
        assert_eq!(bytes.len(), SegmentHeader::SIZE);
        assert_eq!(roundtrip(&seg).body, SegmentBody::End);

        // END with a non-zero body is malformed.
        let mut bad = bytes.clone();
        bad[11..13].copy_from_slice(&2u16.to_be_bytes());
        bad.extend_from_slice(&[0u8; 2]);
        assert!(Segment::parse(&bad).is_err());
    }

    #[test]
    fn parse_full_display_set() {
        // PCS → WDS → PDS → ODS → END
        let segs = vec![
            Segment {
                header: SegmentHeader {
                    pts: 100,
                    dts: 0,
                    segment_type: SegmentType::Pcs.as_raw(),
                    segment_size: 0,
                },
                body: SegmentBody::Pcs(Pcs {
                    width: 1920,
                    height: 1080,
                    frame_rate: 0x10,
                    composition_number: 1,
                    composition_state: 0x80,
                    palette_update_flag: 0,
                    palette_id: 0,
                    composition_objects: vec![CompositionObject {
                        object_id: 0,
                        window_id: 0,
                        object_cropped_flag: 0,
                        object_horizontal_position: 0,
                        object_vertical_position: 0,
                        cropping: None,
                    }],
                }),
            },
            Segment {
                header: SegmentHeader {
                    pts: 100,
                    dts: 0,
                    segment_type: SegmentType::Wds.as_raw(),
                    segment_size: 0,
                },
                body: SegmentBody::Wds(Wds {
                    windows: vec![Window {
                        window_id: 0,
                        horizontal_position: 0,
                        vertical_position: 0,
                        width: 1920,
                        height: 1080,
                    }],
                }),
            },
            Segment {
                header: SegmentHeader {
                    pts: 100,
                    dts: 0,
                    segment_type: SegmentType::Pds.as_raw(),
                    segment_size: 0,
                },
                body: SegmentBody::Pds(Pds {
                    palette_id: 0,
                    palette_version_number: 0,
                    entries: vec![PaletteEntry {
                        palette_entry_id: 1,
                        y: 128,
                        cr: 128,
                        cb: 128,
                        alpha: 0xFF,
                    }],
                }),
            },
            Segment {
                header: SegmentHeader {
                    pts: 100,
                    dts: 0,
                    segment_type: SegmentType::Ods.as_raw(),
                    segment_size: 0,
                },
                body: SegmentBody::Ods(Ods {
                    object_id: 0,
                    object_version_number: 0,
                    last_in_sequence_flag: FragmentFlag::FirstAndLast.as_raw(),
                    object_data_length: 0,
                    width: Some(2),
                    height: Some(1),
                    object_data: vec![0x01, 0x02],
                }),
            },
            Segment {
                header: SegmentHeader {
                    pts: 100,
                    dts: 0,
                    segment_type: SegmentType::End.as_raw(),
                    segment_size: 0,
                },
                body: SegmentBody::End,
            },
        ];
        let mut stream = Vec::new();
        for s in &segs {
            stream.extend_from_slice(&s.encode());
        }
        let parsed = parse_segments(&stream).unwrap();
        assert_eq!(parsed.len(), 5);
        // `encode` recomputes each `segment_size` header field from the
        // body, so compare bodies (and the surviving header fields)
        // rather than the placeholder sizes the `segs` literals carry.
        for (p, s) in parsed.iter().zip(segs.iter()) {
            assert_eq!(p.body, s.body);
            assert_eq!(p.header.pts, s.header.pts);
            assert_eq!(p.header.dts, s.header.dts);
            assert_eq!(p.header.segment_type, s.header.segment_type);
        }
        // The parsed stream re-encodes to the identical bytes.
        let mut reencoded = Vec::new();
        for p in &parsed {
            reencoded.extend_from_slice(&p.encode());
        }
        assert_eq!(reencoded, stream);
    }

    #[test]
    fn rle_literals_and_eol() {
        // Two-pixel line: colour 1, colour 2; end of line.
        let data = [0x01, 0x02, 0x00, 0x00];
        let obj = decode_rle(&data, 2, 1).unwrap();
        assert_eq!(obj.pixels, vec![1, 2]);
    }

    #[test]
    fn rle_short_run_color0() {
        // 00 00 LLLLLL → 5 pixels of colour 0; then EOL.
        let data = [0x00, 0x05, 0x00, 0x00];
        let obj = decode_rle(&data, 5, 1).unwrap();
        assert_eq!(obj.pixels, vec![0, 0, 0, 0, 0]);
    }

    #[test]
    fn rle_long_run_color0() {
        // 00 01LLLLLL LLLLLLLL → 300 pixels of colour 0.
        let len = 300u16;
        let b1 = 0x40 | ((len >> 8) as u8 & 0x3F);
        let b2 = (len & 0xFF) as u8;
        let mut data = vec![0x00, b1, b2];
        data.extend_from_slice(&[0x00, 0x00]); // EOL
        let obj = decode_rle(&data, 300, 1).unwrap();
        assert_eq!(obj.pixels.len(), 300);
        assert!(obj.pixels.iter().all(|&p| p == 0));
    }

    #[test]
    fn rle_short_run_colorc() {
        // 00 10LLLLLL CCCCCCCC → 4 pixels of colour 7.
        let data = [0x00, 0x80 | 4, 7, 0x00, 0x00];
        let obj = decode_rle(&data, 4, 1).unwrap();
        assert_eq!(obj.pixels, vec![7, 7, 7, 7]);
    }

    #[test]
    fn rle_long_run_colorc() {
        // 00 11LLLLLL LLLLLLLL CCCCCCCC → 200 pixels of colour 9.
        let len = 200u16;
        let b1 = 0xC0 | ((len >> 8) as u8 & 0x3F);
        let b2 = (len & 0xFF) as u8;
        let mut data = vec![0x00, b1, b2, 9];
        data.extend_from_slice(&[0x00, 0x00]); // EOL
        let obj = decode_rle(&data, 200, 1).unwrap();
        assert_eq!(obj.pixels.len(), 200);
        assert!(obj.pixels.iter().all(|&p| p == 9));
    }

    #[test]
    fn rle_two_lines() {
        // line 0: colour 1 ×2; line 1: short run colour 0 ×2.
        let data = [0x01, 0x01, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00];
        let obj = decode_rle(&data, 2, 2).unwrap();
        assert_eq!(obj.pixels, vec![1, 1, 0, 0]);
    }

    #[test]
    fn rle_trailing_line_without_eol() {
        // A complete final line without a trailing EOL marker is accepted.
        let data = [0x01, 0x02];
        let obj = decode_rle(&data, 2, 1).unwrap();
        assert_eq!(obj.pixels, vec![1, 2]);
    }

    #[test]
    fn rle_overrun_rejected() {
        // Run longer than width.
        let data = [0x00, 0x80 | 10, 3];
        assert!(decode_rle(&data, 4, 1).is_err());
    }

    #[test]
    fn rle_short_scanline_rejected() {
        // EOL before width is reached.
        let data = [0x01, 0x00, 0x00];
        assert!(decode_rle(&data, 4, 1).is_err());
    }

    #[test]
    fn rle_wrong_line_count_rejected() {
        // One line decoded but height = 2 expected.
        let data = [0x01, 0x01, 0x00, 0x00];
        assert!(decode_rle(&data, 2, 2).is_err());
    }

    #[test]
    fn rle_truncated_after_escape_rejected() {
        let data = [0x00];
        assert!(decode_rle(&data, 4, 1).is_err());
    }

    #[test]
    fn segment_truncated_body_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"PG");
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.push(0x14);
        buf.extend_from_slice(&100u16.to_be_bytes()); // claims 100-byte body
        buf.extend_from_slice(&[0u8; 4]); // but only 4 present
        assert!(Segment::parse(&buf).is_err());
    }

    // --- Display Set grouping + ODS fragment reassembly ---

    fn seg(pts: u32, body: SegmentBody) -> Segment {
        Segment {
            header: SegmentHeader {
                pts,
                dts: 0,
                segment_type: 0, // recomputed by encode; ignored by grouping
                segment_size: 0,
            },
            body,
        }
    }

    fn pcs(state: u8) -> Pcs {
        Pcs {
            width: 1920,
            height: 1080,
            frame_rate: 0x10,
            composition_number: 1,
            composition_state: state,
            palette_update_flag: 0,
            palette_id: 0,
            composition_objects: vec![],
        }
    }

    fn ods(
        object_id: u16,
        flag: FragmentFlag,
        len: u32,
        wh: Option<(u16, u16)>,
        data: &[u8],
    ) -> Ods {
        Ods {
            object_id,
            object_version_number: 0,
            last_in_sequence_flag: flag.as_raw(),
            object_data_length: len,
            width: wh.map(|x| x.0),
            height: wh.map(|x| x.1),
            object_data: data.to_vec(),
        }
    }

    #[test]
    fn group_single_display_set() {
        let segs = vec![
            seg(100, SegmentBody::Pcs(pcs(0x80))),
            seg(100, SegmentBody::Wds(Wds { windows: vec![] })),
            seg(
                100,
                SegmentBody::Pds(Pds {
                    palette_id: 0,
                    palette_version_number: 0,
                    entries: vec![],
                }),
            ),
            seg(
                100,
                SegmentBody::Ods(ods(
                    0,
                    FragmentFlag::FirstAndLast,
                    4 + 2,
                    Some((2, 1)),
                    &[1, 2],
                )),
            ),
            seg(100, SegmentBody::End),
        ];
        let sets = group_display_sets(&segs).unwrap();
        assert_eq!(sets.len(), 1);
        let ds = &sets[0];
        assert_eq!(ds.pts, 100);
        assert!(ds.state().is_epoch_start());
        assert!(ds.wds.is_some());
        assert_eq!(ds.palettes.len(), 1);
        assert_eq!(ds.objects.len(), 1);
    }

    #[test]
    fn group_two_display_sets() {
        let segs = vec![
            seg(100, SegmentBody::Pcs(pcs(0x80))),
            seg(100, SegmentBody::End),
            seg(200, SegmentBody::Pcs(pcs(0x00))),
            seg(200, SegmentBody::End),
        ];
        let sets = group_display_sets(&segs).unwrap();
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].pts, 100);
        assert!(sets[0].state().is_epoch_start());
        assert_eq!(sets[1].pts, 200);
        assert_eq!(sets[1].state(), CompositionState::Normal);
    }

    #[test]
    fn group_rejects_segment_before_pcs() {
        let segs = vec![seg(0, SegmentBody::Wds(Wds { windows: vec![] }))];
        assert!(group_display_sets(&segs).is_err());
    }

    #[test]
    fn group_rejects_second_pcs_before_end() {
        let segs = vec![
            seg(0, SegmentBody::Pcs(pcs(0x80))),
            seg(0, SegmentBody::Pcs(pcs(0x00))),
        ];
        assert!(group_display_sets(&segs).is_err());
    }

    #[test]
    fn group_rejects_two_wds() {
        let segs = vec![
            seg(0, SegmentBody::Pcs(pcs(0x80))),
            seg(0, SegmentBody::Wds(Wds { windows: vec![] })),
            seg(0, SegmentBody::Wds(Wds { windows: vec![] })),
            seg(0, SegmentBody::End),
        ];
        assert!(group_display_sets(&segs).is_err());
    }

    #[test]
    fn group_rejects_trailing_set_without_end() {
        let segs = vec![seg(0, SegmentBody::Pcs(pcs(0x80)))];
        assert!(group_display_sets(&segs).is_err());
    }

    #[test]
    fn reassemble_single_fragment_object() {
        // FirstAndLast: object_data_length = 4 (w+h) + 3 (RLE).
        let ds = DisplaySet {
            pcs: pcs(0x80),
            wds: None,
            palettes: vec![],
            objects: vec![ods(
                7,
                FragmentFlag::FirstAndLast,
                4 + 3,
                Some((8, 1)),
                &[0x01, 0x02, 0x03],
            )],
            pts: 0,
        };
        let objs = ds.reassemble_objects().unwrap();
        assert_eq!(objs.len(), 1);
        assert_eq!(objs[0].object_id, 7);
        assert_eq!(objs[0].width, 8);
        assert_eq!(objs[0].height, 1);
        assert_eq!(objs[0].rle_data, vec![0x01, 0x02, 0x03]);
    }

    #[test]
    fn reassemble_multi_fragment_chain() {
        // A 6-byte RLE bitmap split First(3) + middle(2) + Last(1).
        // declared length on the First fragment = 4 + 6 = 10.
        let ds = DisplaySet {
            pcs: pcs(0x80),
            wds: None,
            palettes: vec![],
            objects: vec![
                ods(
                    3,
                    FragmentFlag::First,
                    4 + 6,
                    Some((6, 1)),
                    &[0x11, 0x22, 0x33],
                ),
                ods(3, FragmentFlag::Other(0x00), 0, None, &[0x44, 0x55]),
                ods(3, FragmentFlag::Last, 0, None, &[0x66]),
            ],
            pts: 0,
        };
        let objs = ds.reassemble_objects().unwrap();
        assert_eq!(objs.len(), 1);
        assert_eq!(objs[0].rle_data, vec![0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
        assert_eq!(objs[0].width, 6);
    }

    #[test]
    fn reassemble_two_objects_first_seen_order() {
        let ds = DisplaySet {
            pcs: pcs(0x80),
            wds: None,
            palettes: vec![],
            objects: vec![
                ods(9, FragmentFlag::FirstAndLast, 4 + 1, Some((1, 1)), &[0xAA]),
                ods(5, FragmentFlag::FirstAndLast, 4 + 1, Some((1, 1)), &[0xBB]),
            ],
            pts: 0,
        };
        let objs = ds.reassemble_objects().unwrap();
        assert_eq!(objs.iter().map(|o| o.object_id).collect::<Vec<_>>(), [9, 5]);
    }

    #[test]
    fn reassemble_continuation_without_first_rejected() {
        let ds = DisplaySet {
            pcs: pcs(0x80),
            wds: None,
            palettes: vec![],
            objects: vec![ods(1, FragmentFlag::Last, 0, None, &[0xAA])],
            pts: 0,
        };
        assert!(ds.reassemble_objects().is_err());
    }

    #[test]
    fn reassemble_never_closed_rejected() {
        let ds = DisplaySet {
            pcs: pcs(0x80),
            wds: None,
            palettes: vec![],
            objects: vec![ods(1, FragmentFlag::First, 4 + 3, Some((3, 1)), &[0x01])],
            pts: 0,
        };
        // First fragment present, Last never arrives → length never met
        // and chain never closed.
        assert!(ds.reassemble_objects().is_err());
    }

    #[test]
    fn reassemble_length_mismatch_rejected() {
        // declared 4 + 3 = 7 (RLE 3) but only 2 RLE bytes delivered.
        let ds = DisplaySet {
            pcs: pcs(0x80),
            wds: None,
            palettes: vec![],
            objects: vec![ods(
                1,
                FragmentFlag::FirstAndLast,
                4 + 3,
                Some((3, 1)),
                &[0x01, 0x02],
            )],
            pts: 0,
        };
        assert!(ds.reassemble_objects().is_err());
    }

    #[test]
    fn reassemble_then_decode_end_to_end() {
        // First fragment carries the full RLE for a 2x1 bitmap:
        // colours 1, 2, then EOL. declared = 4 + 4.
        let rle = [0x01u8, 0x02, 0x00, 0x00];
        let ds = DisplaySet {
            pcs: pcs(0x80),
            wds: None,
            palettes: vec![],
            objects: vec![ods(
                0,
                FragmentFlag::FirstAndLast,
                4 + rle.len() as u32,
                Some((2, 1)),
                &rle,
            )],
            pts: 0,
        };
        let objs = ds.reassemble_objects().unwrap();
        let bitmap = objs[0].decode().unwrap();
        assert_eq!(bitmap.width, 2);
        assert_eq!(bitmap.pixels, vec![1, 2]);
    }

    #[test]
    fn parse_display_sets_end_to_end() {
        // Build a real byte stream and round-trip it through the
        // top-level parse_display_sets entry point.
        let rle = [0x01u8, 0x02, 0x00, 0x00];
        let segs = vec![
            seg(100, SegmentBody::Pcs(pcs(0x80))),
            seg(100, SegmentBody::Wds(Wds { windows: vec![] })),
            seg(
                100,
                SegmentBody::Ods(ods(
                    0,
                    FragmentFlag::FirstAndLast,
                    4 + rle.len() as u32,
                    Some((2, 1)),
                    &rle,
                )),
            ),
            seg(100, SegmentBody::End),
        ];
        let mut stream = Vec::new();
        for s in &segs {
            stream.extend_from_slice(&s.encode());
        }
        let sets = parse_display_sets(&stream).unwrap();
        assert_eq!(sets.len(), 1);
        let objs = sets[0].reassemble_objects().unwrap();
        assert_eq!(objs[0].decode().unwrap().pixels, vec![1, 2]);
    }

    // --- Renderer: palette resolution + compositing ---

    fn palette_entry(id: u8, y: u8, cr: u8, cb: u8, a: u8) -> PaletteEntry {
        PaletteEntry {
            palette_entry_id: id,
            y,
            cr,
            cb,
            alpha: a,
        }
    }

    #[test]
    fn ycbcr_white_black_and_neutral() {
        // Studio-white Y=235, neutral chroma → ~full white.
        let (r, g, b) = ycbcr709_to_rgb(235, 128, 128);
        assert!(r >= 254 && g >= 254 && b >= 254, "white = {r},{g},{b}");
        // Studio-black Y=16, neutral chroma → black.
        let (r, g, b) = ycbcr709_to_rgb(16, 128, 128);
        assert_eq!((r, g, b), (0, 0, 0));
        // Mid grey stays grey (r≈g≈b).
        let (r, g, b) = ycbcr709_to_rgb(126, 128, 128);
        assert!(r == g && g == b, "grey {r},{g},{b}");
        assert!((120..=135).contains(&r), "mid grey ~128, got {r}");
    }

    #[test]
    fn palette_entry_alpha_passthrough_and_transparent_default() {
        let e = palette_entry(5, 235, 128, 128, 0x80);
        let px = e.to_rgba();
        assert_eq!(px.a, 0x80);
        assert!(px.r >= 254 && px.g >= 254 && px.b >= 254);

        // A fresh palette is fully transparent everywhere (incl. index 255).
        let p = Palette::new();
        assert_eq!(p.get(0), Rgba8::TRANSPARENT);
        assert_eq!(p.get(255), Rgba8::TRANSPARENT);
    }

    #[test]
    fn palette_incremental_update_keeps_untouched() {
        let mut p = Palette::new();
        p.apply(&Pds {
            palette_id: 0,
            palette_version_number: 0,
            entries: vec![palette_entry(1, 235, 128, 128, 0xFF)],
        });
        let one = p.get(1);
        assert_eq!(one.a, 0xFF);
        // A second PDS that only writes index 2 must leave index 1 intact.
        p.apply(&Pds {
            palette_id: 0,
            palette_version_number: 1,
            entries: vec![palette_entry(2, 16, 128, 128, 0xFF)],
        });
        assert_eq!(p.get(1), one);
        assert_eq!(p.get(2).a, 0xFF);
        // Index 2 is studio-black → RGB (0,0,0) opaque.
        assert_eq!(
            p.get(2),
            Rgba8 {
                r: 0,
                g: 0,
                b: 0,
                a: 0xFF
            }
        );
    }

    #[test]
    fn from_palettes_with_id_selects_matching() {
        let pals = vec![
            Pds {
                palette_id: 0,
                palette_version_number: 0,
                entries: vec![palette_entry(1, 235, 128, 128, 0xFF)],
            },
            Pds {
                palette_id: 1,
                palette_version_number: 0,
                entries: vec![palette_entry(1, 16, 128, 128, 0xFF)],
            },
        ];
        // palette_id 1 → index 1 is black-opaque, not white.
        let p = Palette::from_palettes_with_id(&pals, 1);
        assert_eq!(
            p.get(1),
            Rgba8 {
                r: 0,
                g: 0,
                b: 0,
                a: 0xFF
            }
        );
        // A palette_id with no matching PDS → all transparent.
        let none = Palette::from_palettes_with_id(&pals, 7);
        assert_eq!(none.get(1), Rgba8::TRANSPARENT);
    }

    #[test]
    fn decoded_object_to_rgba_and_bytes() {
        let mut p = Palette::new();
        p.apply(&Pds {
            palette_id: 0,
            palette_version_number: 0,
            entries: vec![
                palette_entry(1, 235, 128, 128, 0xFF), // white opaque
                palette_entry(2, 16, 128, 128, 0xFF),  // black opaque
            ],
        });
        // 2x1 bitmap: index 1 then index 2.
        let obj = DecodedObject {
            width: 2,
            height: 1,
            pixels: vec![1, 2],
        };
        let img = obj.to_rgba(&p);
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 1);
        assert_eq!(img.pixel(0, 0).unwrap().a, 0xFF);
        assert_eq!(
            img.pixel(1, 0).unwrap(),
            Rgba8 {
                r: 0,
                g: 0,
                b: 0,
                a: 0xFF
            }
        );
        assert!(img.pixel(2, 0).is_none());
        // RGBA8888 byte layout: 2 px × 4 = 8 bytes.
        let bytes = img.to_rgba_bytes();
        assert_eq!(bytes.len(), 8);
        assert_eq!(&bytes[4..8], &[0, 0, 0, 0xFF]);
    }

    /// Build a one-DS PG byte stream with a single 2×2 object placed at
    /// `(px, py)` on a `pw × ph` plane, using a PDS that maps index 1 to
    /// opaque white and leaves 0 transparent. Returns the parsed DisplaySet.
    fn render_fixture(
        pw: u16,
        ph: u16,
        px: u16,
        py: u16,
        crop: Option<CompositionObjectCrop>,
    ) -> DisplaySet {
        // 2x2 all-index-1 bitmap: each line = [0x00,0x82,0x01] (run of 2 of
        // colour 1) then EOL [0x00,0x00].
        let rle = [0x00u8, 0x82, 0x01, 0x00, 0x00, 0x00, 0x82, 0x01, 0x00, 0x00];
        let pcs = Pcs {
            width: pw,
            height: ph,
            frame_rate: 0x10,
            composition_number: 1,
            composition_state: 0x80,
            palette_update_flag: 0,
            palette_id: 0,
            composition_objects: vec![CompositionObject {
                object_id: 7,
                window_id: 0,
                object_cropped_flag: if crop.is_some() { 0x40 } else { 0x00 },
                object_horizontal_position: px,
                object_vertical_position: py,
                cropping: crop,
            }],
        };
        let pds = Pds {
            palette_id: 0,
            palette_version_number: 0,
            entries: vec![palette_entry(1, 235, 128, 128, 0xFF)],
        };
        let segs = vec![
            seg(500, SegmentBody::Pcs(pcs)),
            seg(500, SegmentBody::Wds(Wds { windows: vec![] })),
            seg(500, SegmentBody::Pds(pds)),
            seg(
                500,
                SegmentBody::Ods(ods(
                    7,
                    FragmentFlag::FirstAndLast,
                    4 + rle.len() as u32,
                    Some((2, 2)),
                    &rle,
                )),
            ),
            seg(500, SegmentBody::End),
        ];
        group_display_sets(&segs).unwrap().pop().unwrap()
    }

    #[test]
    fn render_places_object_on_plane() {
        let ds = render_fixture(8, 4, 2, 1, None);
        let r = ds.render().unwrap();
        assert_eq!(r.pts, 500);
        assert_eq!(r.plane.width, 8);
        assert_eq!(r.plane.height, 4);
        // Object occupies (2..4, 1..3); white-opaque inside, transparent out.
        for y in 0..4u16 {
            for x in 0..8u16 {
                let px = r.plane.pixel(x, y).unwrap();
                let inside = (2..4).contains(&x) && (1..3).contains(&y);
                if inside {
                    assert_eq!(px.a, 0xFF, "({x},{y}) should be opaque");
                    assert!(px.r >= 254, "({x},{y}) should be white");
                } else {
                    assert_eq!(px, Rgba8::TRANSPARENT, "({x},{y}) should be clear");
                }
            }
        }
    }

    #[test]
    fn render_clips_object_overrunning_plane() {
        // Place the 2x2 object so its right/bottom edge falls off a 3x2 plane.
        let ds = render_fixture(3, 2, 2, 1, None);
        let r = ds.render().unwrap();
        // Only the (2,1) pixel of the object lands on the plane.
        assert_eq!(r.plane.pixel(2, 1).unwrap().a, 0xFF);
        assert_eq!(r.plane.pixel(0, 0).unwrap(), Rgba8::TRANSPARENT);
        assert_eq!(r.plane.pixel(1, 0).unwrap(), Rgba8::TRANSPARENT);
    }

    #[test]
    fn render_honours_cropping_rectangle() {
        // Crop to a 1x1 sub-rect at object-(1,1); placed at plane-(0,0).
        let crop = CompositionObjectCrop {
            horizontal_position: 1,
            vertical_position: 1,
            width: 1,
            height: 1,
        };
        let ds = render_fixture(4, 4, 0, 0, Some(crop));
        let r = ds.render().unwrap();
        // Exactly one opaque pixel at the plane origin.
        assert_eq!(r.plane.pixel(0, 0).unwrap().a, 0xFF);
        assert_eq!(r.plane.pixel(1, 0).unwrap(), Rgba8::TRANSPARENT);
        assert_eq!(r.plane.pixel(0, 1).unwrap(), Rgba8::TRANSPARENT);
    }

    #[test]
    fn render_rejects_missing_object() {
        let mut ds = render_fixture(8, 4, 0, 0, None);
        // Drop the ODS so the composition object dangles.
        ds.objects.clear();
        assert!(ds.render().is_err());
    }

    #[test]
    fn render_palette_only_pcs_with_no_objects() {
        // A composition with zero objects renders an all-transparent plane.
        let pcs = Pcs {
            width: 4,
            height: 2,
            frame_rate: 0x10,
            composition_number: 0,
            composition_state: 0x00,
            palette_update_flag: Pcs::PALETTE_UPDATE,
            palette_id: 0,
            composition_objects: vec![],
        };
        let ds = DisplaySet {
            pcs,
            wds: None,
            palettes: vec![],
            objects: vec![],
            pts: 9,
        };
        let r = ds.render().unwrap();
        assert!(r.plane.pixels.iter().all(|p| *p == Rgba8::TRANSPARENT));
        assert_eq!(r.plane.pixels.len(), 8);
    }
}
