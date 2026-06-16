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
}
