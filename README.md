# oxideav-bluray

Read-only Blu-ray Disc (BD-ROM) source plug-in: UDF 2.50 mount +
BDMV directory walk + playlist (`.mpls`) / clip-info (`.clpi`)
parsing + `.m2ts` streaming. Pure-Rust, clean-room per the
publicly-distributed BDA whitepapers and ECMA-167.

```text
bluray:///Volumes/EXAMPLE          → mount + autoplay longest title
bluray:///path/to/disc/root        → mount explicit BDMV root
bluray://                          → auto-detect first BD-ROM mount
```

## Scope (Phase 1)

- UDF 2.50 read-only mount (sector layout, Volume Descriptor
  Sequence, File Set Descriptor, File Entry / ICB allocation walks).
  The File Entry parser accepts both Tag 261 (plain FE §14.9) and
  Tag 266 (ExtendedFileEntry §14.17); EFE carries the extra
  `Object Size` field (§14.17.11) surfaced through
  `FileEntry::object_size`, and the 40-byte-longer prefix
  (creation_time + stream_directory_icb + extra reserved word) is
  decoded transparently so authoring tools that emit EFE for the BDMV
  root mount without an `unsupported` bail. All three ECMA-167
  allocation-descriptor flavours are walked: short_ad (§14.14.1),
  long_ad (§14.14.2) and ext_ad (§14.14.3), normalised through
  `FileEntry::extents() -> Vec<AllocExtent>` so the file walk runs one
  loop regardless of flavour. Long/extended extents must reference
  the mounted partition (`UdfDisc::partition_number`, the
  single-partition BD-ROM assumption) — a cross-partition `lb_addr`
  is refused rather than misresolved against the wrong partition
  base; an ext_ad whose `Recorded Length` differs from its
  `Information Length` (a compressed extent, §14.14.3 Note 46) is
  refused at parse time, and an ext_ad extent contributes its
  `Information Length` bytes (not the block-rounded Extent Length)
  to the file body. Allocation Extent Descriptor continuation chains
  (§14.5 / §12 figure 7) are followed: an AD whose extent type is 3
  ("the extent is the next extent of allocation descriptors",
  §14.14.1.1) terminates its field per §12 and names the extent
  holding an AED (Tag 258) + further descriptors of the same
  flavour; `UdfDisc::read_file_entry` walks the chain (depth-capped
  at 32 to refuse cyclic chains, each continuation extent bounded to
  1 MiB) so `FileEntry::extents()` always sees the full flattened
  sequence. A standalone `FileEntry::parse` (no disc reader)
  surfaces the unresolved pointer via `FileEntry::continuation`.
- BDMV parsers — `index.bdmv` titles, `MovieObject.bdmv` nav-command
  enumeration **+ opcode decode** (`NavCommand::decode` →
  `DecodedCommand`: the 3×4-byte command word split into `op_cnt` /
  `grp` (Branch/Compare/Set) / `sub_grp` / the named `Operation`
  (Nop, GoTo, JumpTitle, PlayPL, the seven Compare ops, the fifteen
  register Set ops, the eleven SetSystem ops), plus the two operand
  words decoded into immediate values or GPR/PSR register references;
  all worked-hex examples from the clean-room table round-trip) **+ the
  PSR/GPR register model** (`bdmv::register_model`: `psr_info` names the
  128 Player Status Registers with their Playback-Status / Player-Setting
  access class, `gpr_convention` the 4096-GPR authoring partition,
  `Operand::resolve_register` joining a register operand to its named
  `PsrInfo`) **+ a minimal HDMV navigation VM** (`bdmv::vm::HdmvVm`:
  executes the decoded commands against a 4096-GPR / 128-PSR register
  file — Move/Swap, the arithmetic group (Add/Sub/Mul wrapping, Div/Mod
  truncating with ÷0→0), bitwise And/Or/Xor, single-bit Set/Clear, the
  two shifts, the seven Compare ops as a conditional-skip of the next
  command, and `GoTo`/`Break`/`Nop` flow control; the playback-leaving
  branches and `SetSystem` ops halt-and-yield a typed `NavRequest`, and
  nav writes to read-only/Player-Setting PSRs are dropped) **driven over
  the whole MOBJ table** by `bdmv::mobj_runner::MobjRunner`, which
  follows `JumpObject`/`CallObject`/`Resume` with a resume stack and one
  shared register file,
  `PLAYLIST/*.mpls` PlayList + PlayItem + STN_table
  summary + ClipMark, `CLIPINF/*.clpi` ClipInfo + SequenceInfo +
  ProgramInfo + CPI EP_map (per-stream-PID entry-point map — coarse +
  fine rows decoded into a flat `(pts_ep_start, spn_ep_start)` list
  ready for I-frame-aligned seek). The 4-bit `EP_stream_type` header
  field is surfaced through a typed `EpStreamType` view
  (`EpMap::kind()` / `EpEntry::kind()`); `Cpi::primary_video_ep_map()`
  uses it to pick the HEVC EP_map over a co-resident AVC fallback on
  UHD-BD authoring patterns, falling back to lowest-PID when every
  EP_map carries an unknown 4-bit code. The CLPI ProgramInfo
  `stream_coding_info()` block (§5.5.4.3) is decoded with the same
  typed surface as the MPLS STN_table (§5.4.4.4): `StreamCodingInfo`
  now reads the full `sc_len` body — exposing `aspect_ratio_nibble` +
  the 3-byte ISO 639-2/T `language_code` the parser previously
  discarded — and offers `coding_type()`, `video_format_kind()` /
  `frame_rate_kind()` / `aspect_ratio_kind()` (video),
  `audio_format_kind()` / `sample_rate_kind()` (audio), and
  `language()` accessors reusing the `StreamCodingType` / `VideoFormat`
  / `FrameRate` / `AspectRatio` / `AudioFormat` / `SampleRate` enums, so
  a remuxer reads each clip's per-stream codec + resolution + channel
  layout + language straight off its own ProgramInfo without a matching
  `.mpls` open.
- `.m2ts` stream → strip the 4-byte BDAV `TP_extra_header` per
  192-byte source packet, deliver clean 188-byte MPEG-TS bytes.
  Three call shapes: the in-place `strip_tp_extra(input, &mut out)`
  (zero-alloc, used internally by `TitleSource`), the convenience
  `strip_tp_extra_to_vec(input) -> Vec<u8>` for one-shot callers, and
  a borrowing `iter_source_packets(input) -> M2tsIter` that yields
  one `M2tsSourcePacket { tp_extra, ts_payload: &[u8; 188] }` per
  192-byte chunk — letting callers consume the 27 MHz arrival
  timestamps + CCI bits without re-parsing the 4-byte header out of
  band. The TS payload stays opaque; downstream MPEG-TS demuxers own
  ISO/IEC 13818-1 parsing.
- **Presentation Graphic Stream (PGS) segment parser** (`bdmv::pgs`) —
  the bitmap-subtitle / graphics wire format carried in each PG
  elementary stream (and the on-disc-equivalent of a `.sup` file). The
  shared 13-byte PG segment header (`SegmentHeader`: `"PG"` magic +
  90 kHz PTS/DTS + `segment_type` + `segment_size`) frames the five
  typed bodies: `Pcs` (composition objects + optional cropping rect +
  `CompositionState` Epoch-Start / Acquisition-Point / Normal +
  palette-update flag), `Wds` (window geometry list), `Pds` (the
  YCbCr+alpha CLUT, entry count derived from the body length), `Ods`
  (the fragmented RLE bitmap with `FragmentFlag` First / Last /
  FirstAndLast and first-fragment-only `width`/`height`), and the empty
  `END`. `parse_segments` walks a whole PG / `.sup` byte stream into a
  flat `Vec<Segment>`; `decode_rle` expands the ODS byte-oriented,
  per-scanline run-length code (single-pixel literals + the four
  short/long × colour-0/colour-C run branches + the all-zeros
  end-of-line) into `width × height` palette indices, rejecting runs
  that overrun the width or a scanline count that misses `height`.
  Every segment round-trips through `Segment::encode` (which recomputes
  `segment_size` from the body); malformed inputs (bad magic, truncated
  body, ragged PDS, non-empty END) surface `BlurayError::Malformed`
  rather than panicking. Clean-room from
  `docs/container/bluray/pgs-segment-syntax.md`. Re-exported from the
  crate root.
- **PGS renderer — palette resolution + window compositing**
  (`bdmv::pgs`) — the layer that turns parsed Display Sets into the
  actual subtitle bitmap. `PaletteEntry::to_rgba` applies the BT.709
  limited-range YCbCr→RGB conversion (the doc's *"Color is YCbCr + alpha
  (BT.709 range as used on BD)"* palette-entry note), passing the alpha
  through; `Palette` is a 256-entry CLUT built from one or more `Pds`
  with incremental-update semantics (`apply` / `from_palettes` /
  `from_palettes_with_id`, the last selecting the CLUT the PCS names via
  `palette_id`; unwritten indices, including 255, stay transparent).
  `DecodedObject::to_rgba` resolves the CLUT indices to an `RgbaImage`
  (straight-alpha `Rgba8` pixels, `to_rgba_bytes` for a packed RGBA8888
  buffer), and `DisplaySet::render` composites every composition object —
  decoded, palette-resolved, cropped to its `object_cropping_*`
  sub-rectangle when `object_cropped_flag == 0x40`, and clipped to the
  plane — into a `RenderedDisplaySet` graphics plane
  (`pcs.width × pcs.height`) at each object's
  `(object_horizontal_position, object_vertical_position)`. A
  composition object referencing an `object_id` absent from the DS is
  rejected. `Rgba8` / `RgbaImage` / `Palette` / `RenderedDisplaySet`
  re-exported from the crate root. (Alpha-blend onto the decoded video
  plane is the downstream player's job; this yields the straight-alpha
  overlay it blends.) 10 new unit tests.
- **PGS Display Set grouping + ODS fragment reassembly** (`bdmv::pgs`) —
  the layer above the flat segment list. `group_display_sets` (and the
  one-shot `parse_display_sets`) slice a `Vec<Segment>` into
  `DisplaySet`s on each PCS boundary, bucketing the `PCS -> WDS -> PDS
  ... -> ODS ... -> END` run into `{ pcs, wds, palettes, objects, pts }`
  and rejecting malformed framing (a segment before the opening PCS, a
  second PCS before END, two WDS in one DS, a trailing DS with no END).
  `DisplaySet::reassemble_objects` then folds each ODS fragment chain
  (fragments sharing one `object_id`, opened by a `First` /
  `FirstAndLast` carrying `width`/`height` and closed by a `Last`) back
  into a `ReassembledObject` whose concatenated `rle_data` is validated
  against the first fragment's `object_data_length - 4` byte count (the
  doc's width+height+RLE wire-observation); `ReassembledObject::decode`
  runs `decode_rle` to yield the paletted bitmap. A continuation with no
  open chain, a duplicate `object_id`, a never-closed chain, or a length
  mismatch surfaces `BlurayError::Malformed`. This bridges the parsed
  segments into renderable display sets for a downstream compositor.
  14 new unit tests.
- `TitleSource::seek_to(pts_90k)` — keyframe-aligned random access.
  A title-relative 90 kHz PTS is mapped to a PlayItem/clip, converted
  to clip-local time, binary-searched against that clip's CPI EP_map
  for the I-frame at or before the target, and the chosen
  `spn_ep_start` is turned into a byte offset (`× 192`, per AV §3.1)
  with AACS-unit-aligned positioning so decryption stays correct.
  Clips with no CPI fall back to the clip start. The byte-exact `Seek`
  impl now uses the same per-clip output-offset index to jump straight
  to the containing clip on a rewind.
- `Disc::longest_title()` heuristic for autoplay (longest HDMV
  title).
- **Multi-angle PlayItem parsing + per-angle title open** — `PlayItem`
  carries an `angles: Vec<AngleClip>` list of the per-angle clip
  references the disc lists for the `is_multi_angle` block (§5.4.4.1);
  `Disc::open_title_with_angle(title, angle, decryptor)` streams a
  chosen angle's `.m2ts` chain; `Disc::max_angle(title)` reports the
  largest angle that's available on every PlayItem.
- **Chapter list from PlayListMark entry marks** (§5.4.5) —
  `PlayListMpls::chapters()` / `Disc::chapters(title)` lift every entry
  mark (`mark_type == 0x01`) off its clip-local time axis onto the title
  timeline (summing preceding PlayItem durations + the mark's offset
  past its PlayItem IN point), returning a `Chapter { index,
  start_pts_90k, ref_play_item_id }` whose 90 kHz PTS feeds straight into
  `TitleSource::seek_to`. Link points (`mark_type == 0x02`) and
  malformed refs are excluded. A `MarkType` enum + `PlayListMark::kind()`
  classify the raw `mark_type` byte.
- **Cross-PlayItem STC PTS continuity map** — `TitleSource::pts_continuity_segments()`
  (and the file-less peer `Disc::title_pts_continuity_segments(title)`) returns
  one [`PtsContinuitySegment`] per PlayItem in playback order, telling a downstream
  MPEG-TS demuxer where each PlayItem's bytes live in the output stream and how to
  reproject its clip-local PTS onto the title timeline. Each segment carries
  `output_byte_start` / `output_byte_end`, `title_pts_start` / `title_pts_end`
  (90 kHz), `clip_in_pts_90k` / `clip_out_pts_90k` (PlayItem IN/OUT lifted from
  §5.4.4.1, doubled from 45 kHz), the `stc_origin_pts_90k` lifted from the clip's
  CLPI `SequenceInfo` / `presentation_start_time` (§5.5.4.2, picked by the
  PlayItem's `stc_id_ref`), and the seam's `connection_condition` (§5.4.4.2 —
  `NonSeamless` / `SeamlessContinuation` / `SeamlessNewStc`). A demuxer reprojects
  each PES packet inside a segment as
  `title_pts = title_pts_start + (pes_pts - clip_in_pts_90k)`; the convenience
  `TitleSource::map_clip_pts_to_title_pts(byte_pos, pes_pts)` does the binary
  search for one-shot callers. The first PlayItem's recorded
  `connection_condition` byte is meaningless (defined as the relation to the
  *previous* PlayItem) so the surface always normalises it to `NonSeamless`. The
  reproject closes the long-standing gap where a downstream remuxer either had to
  flatten every clip's PTS into a single axis or interpret natural BD-AV STC
  restarts as stream corruption.
- **Mid-stream angle-change-point enumeration** —
  `TitleSource::angle_change_points() -> Vec<AngleChangePoint>` and
  the convenience `next_angle_change_point(pts_90k)` lift every CPI
  EP_fine row with `is_angle_change_point = 1` (BD-ROM AV §5.7) onto
  the title timeline + output-byte axis. Each
  `AngleChangePoint { play_item_index, clip_stem, title_pts_90k,
  output_byte, clip_pts_90k, spn }` is the I-frame at which a live
  angle switch can land cleanly — the spec's interleaved-clip
  constraint guarantees every alternate angle carries a co-incident
  I-frame at the matching SPN, so a player UI reads up to
  `output_byte`, calls `open_title_with_angle(new_angle)`, then
  `seek_to(title_pts_90k)` on the new source. ACPs whose clip-local
  PTS sits before their PlayItem's IN point are dropped (unreachable
  by the streamer). The file-less peer `Disc::title_angle_change_points(title)`
  + angle-aware `…_with_angle(title, angle)` mirror the
  `title_pts_continuity_segments` / `chapters` pattern (no `.m2ts`
  open; empty list on parse failure or out-of-range angle).
- **In-place mid-stream angle switching** — `TitleSource::switch_angle_at(new_angle, title_pts_90k)`
  retargets an open `TitleSource` to a different angle's `.m2ts` /
  `.clpi` pair at a keyframe-aligned title PTS, without dropping the
  decryptor or recreating the source. The convenience
  `TitleSource::switch_angle(new_angle)` lands on the next
  [`angle_change_points`](#) boundary at or after the current output
  position — the typical "user pressed the angle button mid-playback"
  UI path. Both validate `new_angle` against every PlayItem up front
  and leave the source untouched on rejection
  (`io::ErrorKind::InvalidInput`); `switch_angle` additionally returns
  `io::ErrorKind::NotFound` when no flagged boundary remains.
  `TitleSource::current_angle()` reports the angle currently driving
  the reader; `TitleSource::num_angles()` reports the smallest
  PlayItem-angle-count across the title (so any value `< num_angles()`
  is safe to pass to `switch_angle_at`). The output-byte axis is a
  new physical stream after a switch — callers who tracked their
  position by byte should re-anchor against the returned value.
- **Per-title track catalogue** — `Disc::title_streams(title) ->
  TrackCatalogue` aggregates every PlayItem's STN_table (§5.4.4.4) into
  a flat per-track listing, deduplicated by `(elementary_pid, kind)` so
  a remuxer sees exactly one `Track { pid, kind, coding_type, language,
  playitem_count }` per distinct elementary stream. Tracks are emitted
  in canonical STN class order (primary video → primary audio → PG → IG
  → secondary audio → secondary video → PiP PG). `TrackCatalogue::by_pid`
  / `by_kind` give O(n) PID lookup + class filter for downstream label
  emission. `TitleInfo::languages` is now populated at mount time from
  every audio + subtitle entry's 3-byte ISO 639-2/T tag (sorted,
  deduplicated, lowercased — disc authors ship a mix of `ENG` / `eng`).
- **PlayList `playback_type` typed accessor** —
  `AppInfoPlayList::playback_kind()` returns a `PlayListPlaybackType`
  (the typed view of the `PlayList_playback_type` byte recorded in
  `AppInfoPlayList()` per BD-ROM Part 3 §5.4) so callers can pattern-match
  on `Sequential` / `Random` / `Shuffle` / `Other(u8)` instead of
  comparing against magic numbers. `from_raw` / `as_raw` round-trip
  through the wire byte; `is_sequential` / `is_randomised` cover the
  common UI predicate (one indicator for both random-pick variants).
- **PlayItem playback-control fields (`PlayItemFlags`)** — the
  `PlayItem_random_access_flag`, `still_mode` byte, `still_time` word,
  and the raw multi-angle flags byte (BD-ROM Part 3 §5.4.4.1) — fields
  the `.mpls` parser previously consumed and discarded — are now
  surfaced through `PlayItem::flags`. `random_access_flag` decomposes
  into a typed `bool` (the top bit of the byte after the 8-byte UO
  mask table — the layout the parser has always assumed); `still_mode`,
  `still_time`, and `angle_flags` are surfaced verbatim. The `parse`
  and `encode` paths read/write these fields instead of fixed zeros, so
  a PlayItem's random-access intent + still-frame dwell survive an
  encode → parse round trip. Re-exported from the crate root.
- **STN_table video / audio attribute typed accessors** — five new
  enums (`VideoFormat`, `FrameRate`, `AspectRatio`, `AudioFormat`,
  `SampleRate`) cover the 4-bit nibbles BD-ROM Part 3 §5.4.4.4 packs
  into each PlayItem's per-stream `stream_attributes` block plus the
  matching nibbles in `index.bdmv` AppInfoBDMV §5.3 (disc-wide
  defaults). `VideoFormat` covers 480i / 576i / 480p / 1080i / 720p /
  1080p / 576p / 2160p with `is_progressive()` + `vertical_lines()`;
  `FrameRate` covers 23.976 / 24 / 25 / 29.97 / 50 / 59.94 with
  `fps_q() -> (num, den)` for safe rational propagation +
  `is_fractional()`; `AspectRatio` covers 4:3 / 16:9 with
  `ratio() -> (w, h)` + `is_widescreen()`; `AudioFormat` covers Mono /
  Stereo / Multi (5.1) / Combo (5.1 + downmix) with `channel_count()`
  + `has_downmix()`; `SampleRate` covers 48k / 96k / 192k plus the
  dual-rate combos `48/192` and `48/96` with `primary_hz()` +
  `is_combo()`. `from_raw` masks the low nibble so callers can pass
  the un-shifted wire byte directly; `as_raw` round-trips through the
  4-bit field. Surfaces as typed methods (`video_format_kind` /
  `frame_rate_kind` / `aspect_ratio_kind` / `audio_format_kind` /
  `sample_rate_kind`) on `PrimaryVideoStream` /
  `SecondaryVideoStream` / `PrimaryAudioStream` / `SecondaryAudioStream`
  and `AppInfoBdmv`. 15 new unit tests cover the named round-trips,
  the helper predicates, the `Other` catch-all for reserved nibbles,
  nibble masking on the un-shifted wire byte, the per-stream
  accessors, and a full MPLS encode → parse roundtrip end-to-end so
  the typed view stays consistent with the wire packing
  `PlayListMpls::encode` / `parse` already exercise. Re-exported
  from the crate root.
- `bluray://` URI handler registered with `oxideav_core::SourceRegistry`
  under the default-on `registry` cargo feature.
- Pluggable [`StreamDecryptor`] trait so `oxideav-aacs` can plug in
  without a hard dep.
- macOS AACS Volume Identifier reader (`drive::macos`): IOKit
  SCSITaskDeviceInterface dlopen'd via `libloading`, MMC `READ DISC
  STRUCTURE` (opcode `0xAD`, format `0x80`) against the BSD whole-disk
  node resolved via `statfs(disc_root)`. Surfaces an actionable error
  when Finder still has the volume mounted (run `diskutil unmount`
  first). Linux SG_IO + Windows SPTI backends still stubbed; use
  `OXIDEAV_AACS_VOLUME_ID=<32-hex>` as an override there.
- **AACS VUK lookup cascade** (`aacs_adapter::try_resolve_aacs`):
  KEYDB.cfg legacy entry → on-disk cache
  (`${XDG_CACHE_HOME:-${HOME}/.cache}/oxideav/vuk-cache.cfg`, same
  line format as KEYDB.cfg, override with `OXIDEAV_AACS_VUK_CACHE`)
  → **online** derivation gated by the default-on `aacs-online`
  cargo feature. The online path reads the drive's AACS Volume
  Identifier and walks every `| DK |` Device Key from KEYDB.cfg
  through the disc's MKB via
  `oxideav_aacs::AacsVolume::derive_vuk_from_device_key`; on success
  the derived VUK is written back to the cache with an
  `online-<RFC3339>` provenance stamp. Headless / CI builds opt out
  with `--no-default-features --features registry,aacs`.

## Deferred

- HDMV interactive layer (`MovieObject.bdmv` opcode execution).
- BD-J (Java ME).
- SubPath PiP / secondary-video streams.
- Raw-block-device mount (`bluray:///dev/sr0`) — UDF mounter exists,
  high-level routing in `Disc::mount_image` is Phase 2.
- Cross-PlayItem STC PTS continuity is exposed as a
  `pts_continuity_segments` map (see Scope above) — the demuxer-side
  PES reproject is fully driven by it; what's still deferred is having
  `TitleSource::read()` *itself* rewrite outgoing PES PTS so a remuxer
  doesn't even need the map.
- ICB strategy types other than 4, multi-extent partition maps.
  (ExtendedFileEntry §14.17 parsing, long/extended allocation
  descriptors §14.14.2–3, and Allocation Extent Descriptor
  continuation chains §14.5 now landed — listed in Scope above.)
- HDMV nav-command *execution* now landed (see Scope above): the
  `bdmv::vm::HdmvVm` interpreter runs the decoded Set / Compare / Branch
  commands against the GPR/PSR register file, and `bdmv::mobj_runner`
  drives the whole `MovieObject.bdmv` table across
  `JumpObject`/`CallObject`/`Resume`, all from the
  `docs/container/bluray/hdmv-navigation-commands.md` clean-room table.
  What stays deferred is the *player-side* model the BDMV table alone
  does not carry: resume-intention handling, the UO mask, and the IG
  button-state machine — surfaced as a yielded `NavRequest` for the
  player layer rather than executed. The exact rounding of `Div`/`Mod`
  on out-of-band operands and the precise arithmetic-overflow rule are
  not pinned down by the public clean-room material (the VM uses
  wrapping unsigned arithmetic, truncating division, ÷0→0); those edge
  cases live in the member-gated BD-ROM Part 3 normative book. The
  `SetStream` / `SetSecondaryStream` operand-word sub-field packing
  (which bits pick audio vs PG vs angle, the change/keep flags) is also
  not yet tabulated in `docs/` and is surfaced only as the raw operand
  words on the yielded `NavRequest::SetSystem`.

## Standalone build

`oxideav-core` is gated behind the default-on `registry` feature.
Drop the framework dependency entirely with:

```toml
oxideav-bluray = { version = "0.0", default-features = false }
```

The `Disc` / `TitleSource` / parser surface stays available; only
the `bluray://` registry plumbing disappears.

## Tests

All fixtures are synthetic — the crate ships zero bytes from real
Blu-ray discs and references no real titles. Test categories:

- UDF descriptor round-trips (tag checksum, lb_addr, short_ad,
  ext_ad, d-string).
- Allocation-descriptor flavours (§14.14): a File Entry recording two
  long_ads parses + normalises through `extents()`; an uncompressed
  ext_ad contributes its Information Length; a compressed ext_ad
  (recorded ≠ information) is rejected `Unsupported`; end-to-end, a
  synthetic-image file recorded through a two-block long_ad reads
  back byte-exact and a long_ad naming a foreign `partition_ref` is
  refused (`tests/udf_minimal_image.rs`).
- AED continuation chains (§14.5 / §12): a type-3 AD terminates its
  field (a trailing AD after it is not consumed) and surfaces through
  `FileEntry::continuation` for all three flavours (short_ad with
  implied partition; ext_ad pointer exempt from the compressed-extent
  check); the 24-byte AED header round-trips and a non-258 tag is
  rejected; end-to-end, a synthetic-image file whose AD field chains
  through an AED block reads back byte-exact across both extents, and
  a cyclic AED chain (an AED pointing at itself) is refused
  `Malformed` by the depth cap instead of looping
  (`tests/udf_minimal_image.rs`).
- ExtendedFileEntry (Tag 266 / §14.17) parsing: an embedded-directory
  EFE with non-trivial `object_size`, a single-extent EFE pointing at
  block 42, a regression that plain FE reports `object_size == None`,
  and a truncated EFE rejected as `Malformed`.
- BDMV parser round-trips (`index.bdmv`, `MovieObject.bdmv`,
  `.mpls`, `.clpi`).
- PGS segment round-trips (`bdmv::pgs` unit tests): every segment
  type encodes → parses byte-identically (PCS cropped + uncropped,
  WDS, PDS with derived entry count, ODS first/continuation fragments,
  empty END); a full PCS→WDS→PDS→ODS→END Display Set parses via
  `parse_segments` and re-encodes to the same bytes; the four ODS RLE
  branches (short/long × colour-0/colour-C) + end-of-line + a
  no-trailing-EOL final line decode to the expected paletted indices;
  and malformed inputs (bad magic, truncated body, ragged PDS,
  non-empty END, RLE width overrun / short scanline / wrong line count
  / truncated escape) are all rejected.
- `TP_extra_header` strip (1 packet, 17 packets, alignment panics).
- AACS-unit-length sanity check (32 × 192 = 6144 = 3 × 2048).
- `bluray://` URI parser (auto-detect, absolute path, scheme reject).
- Mount auto-detect with a synthetic `/tmp/` BDMV tree.
- End-to-end mount + stream of a hand-rolled BDMV directory (`tests/synthetic_disc.rs`).
- Keyframe-aligned `seek_to` over a 2-clip title with hand-built CPI
  EP_maps, plus the no-CPI fallback + byte-exact `Seek` co-existence,
  and a chapter test (`Disc::chapters` lifts entry marks to title PTS,
  excludes a link point, and each chapter PTS seeks onto the right
  keyframe — including a mark offset past a non-zero PlayItem IN point)
  (`tests/seek_to_keyframe.rs`).
- End-to-end mount of a synthesised in-memory UDF image (`tests/udf_minimal_image.rs`).
- Multi-angle BDMV mount + per-angle streaming (`tests/multi_angle.rs`):
  primary + two alt angles each streamed from their own fingerprinted
  `.m2ts`, out-of-range angle rejected at open time, `max_angle`
  reporting the smallest PlayItem-angle-count minus one.
- Mid-stream angle-change-point enumeration (`tests/angle_change_points.rs`):
  ACP rows lift onto the title timeline with byte/PTS bounds; the
  `next_angle_change_point(pts)` walk visits each row at-or-after a
  cursor; `Disc::title_angle_change_points` matches the source's
  view; ACPs whose clip-local PTS lies before the PlayItem's IN
  point are dropped; a CPI with no ACP rows yields an empty list.
- In-place mid-stream angle switching (`tests/switch_angle.rs`):
  `current_angle` / `num_angles` report the open state; `switch_angle_at`
  retargets the underlying clip reader to a different angle at the
  requested title PTS (verified end-to-end via per-clip fingerprint
  bytes for both an intra-PlayItem ACP boundary and a PlayItem-seam
  switch); out-of-range angle returns `InvalidInput` and the source
  stays on the previous angle (next read still yields the previous
  angle's fingerprint); the boundary-finding `switch_angle` lands on
  the first flagged row at-or-after the current output position; a
  title with no flagged rows returns `NotFound` from `switch_angle`.

## Fuzzing

`fuzz/` carries a `cargo-fuzz` (libFuzzer) harness. The
`udf_descriptors` target multiplexes every public
`oxideav_bluray::udf` descriptor parser behind a one-byte selector —
`DescriptorTag`, the §7.1/§14.14 allocation descriptors (`extent_ad`,
`short_ad`, `long_ad`, `ext_ad`, `lb_addr`), the §14.5 Allocation
Extent Descriptor, the §10 volume descriptors (AVDP / PVD / PD / LVD),
the §14 File Set / File Identifier / File Entry / ICB Tag, and the
OSTA §2.1.3 d-string decoders — so a single corpus exercises the whole
parser surface. The contract is that any byte slice yields `Ok(_)` or
`Err(BlurayError)` and never panics, overflows, or indexes out of
bounds. Built against the crate with `default-features = false` (no
registry / AACS deps). Run with:

```sh
cargo +nightly fuzz run udf_descriptors
```

The contract is that any byte slice yields `Ok(_)` or
`Err(BlurayError)` and never panics, overflows, or indexes out of
bounds (a regression test `avdp_truncated_after_valid_tag_is_rejected`
pins the one historically-reachable panic, an `AnchorVolumeDescriptorPointer::parse`
slice without a length check, as fixed).

## Clean-room references

Only these documents are consulted; no third-party Blu-ray / UDF
library or disc-ripping tool source has been read.

- `docs/container/bluray/BD-ROM_Part3_V3.2_WhitePaper_180122.pdf`
- `docs/container/bluray/BD-ROM_Audio_Visual_Application_Format_Specifications.pdf`
- `docs/container/bluray/BD-ROM-AV-WhitePaper_HEVC.pdf`
- `docs/container/bluray/ECMA-167_3rd_edition_june_1997.pdf`
- `docs/container/bluray/pgs-segment-syntax.md`

## Privacy

Auto-detect probes directory existence only. The crate never reads
volume labels, disc IDs, or any other identifying field into a
public-facing struct, and contains zero references to specific
commercial titles, studios, or hashes in code, tests, or docs.

## License

MIT. Copyright (c) 2026 Karpelès Lab Inc.
