# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Marked the internal `bdmv::common` byte-cursor plumbing (`Reader`,
  `BdmvHeader`, `clpi_path`, `m2ts_path`) `#[doc(hidden)]` so
  cargo-semver-checks stops tracking it as stable public API; the
  documented BDMV/UDF/playlist/nav surface is unchanged.

### Added

- **BDMV parser fuzz harness + hostile-input hardening** — a second
  cargo-fuzz target, `bdmv_parsers`, multiplexes the whole BDMV parsing
  surface (`index.bdmv` / `MovieObject.bdmv` / `.mpls` / `.clpi` + the PG
  segment / display-set / RLE decoders) behind a one-byte selector, with
  each `Ok(_)` parse driven through its derived accessors. Two in-CI
  companion suites pin the Ok/Err-never-panic contract without a nightly
  run: `bdmv_hostile_input` (size-lie length/offset fields incl.
  `u32::MAX`, full truncation sweeps, uniform-fill sectors up to 64 KiB,
  ~80k deterministic-PRNG buffers) and `bdmv_structured_mutation` (valid
  encoded `.mpls`/`index.bdmv`/`MovieObject.bdmv` seeds perturbed by every
  single-byte flip, a 32-bit size-lie at each aligned window, and every
  truncation — so corruption lands inside real count-driven bodies). No
  panic found across 5.4M fuzz runs + the full mutation matrix.
- **UO_mask_table + is_repeat_SubPath preservation** (`bdmv::mpls`) — the
  8-byte `UO_mask_table` in `AppInfoPlayList` (§5.4.3) and every
  `PlayItem` (§5.4.4.1) and the `is_repeat_SubPath` flag (§5.4.4) were
  parsed-then-discarded and re-encoded as zeros, so a parse → encode →
  parse cycle silently dropped the disc's User-Operation prohibitions and
  SubPath loop intent. Now surfaced verbatim as `AppInfoPlayList::uo_mask`
  / `PlayItemFlags::uo_mask` (big-endian `u64`, raw — the bit → operation
  assignments are not tabulated in the consulted references) and
  `SubPath::is_repeat_subpath` (`bool`), and round-tripped through both
  parse and encode. 3 new round-trip tests.
- **Stream coding-type labels + track UI labels** (`bdmv::mpls`, `Disc`)
  — `StreamCodingType` gained `is_graphics()` (PGS / IGS / Text),
  `is_secondary()` (the `0xA1`/`0xA2` PiP-commentary audio) and
  `display_name()` (a UI string like `"Dolby TrueHD"` / `"PGS Subtitle"`,
  `"Unknown(0xNN)"` for reserved bytes) — completing the class predicates
  alongside the existing `is_video` / `is_audio`. `Track::label()` builds
  a one-line catalogue label from the coding type's display name + the
  resolved language (`"DTS-HD Master Audio (eng)"`; no parenthetical for
  language-less video). Pure derivation. 3 new tests.
- **EP_map forward-seek helpers** (`bdmv::clpi`) — the keyframe index
  could only resolve the entry point *at or before* a target (the
  backward-safe random-access policy). `EpMap::entry_point_after(pts_90k)`
  /  `next_seek_spn(pts_90k)` add the forward complement (binary search
  for the smallest `pts_ep_start > target`, `None` past the last
  keyframe) — the "skip to next I-frame" / seek-window-end primitive — and
  `entry_point_count()` exposes the indexed keyframe count. Pure
  derivation over the already-parsed EP_map entries. 2 new tests + the
  empty-map guard extended.
- **Chapter presentation spans** (`bdmv::mpls`, `Disc`) — the chapter
  list previously surfaced each chapter's title-relative *start* PTS only;
  a player UI / scrubber also needs each chapter's *end* and *duration*.
  `PlayListMpls::chapters_with_duration()` widens every entry-mark chapter
  to a `[start, end)` window: a chapter ends where the next begins (in
  playback order, after sorting by start so out-of-order authoring is
  tolerated) and the final chapter ends at the title total
  (`duration_90k`). The new `ChapterSpan { index, start_pts_90k,
  end_pts_90k, ref_play_item_id }` adds `duration_90k()` /
  `duration_secs()`. `Disc::chapter_spans(title)` is the file-less peer of
  `Disc::chapters`. Pure derivation over already-parsed marks + PlayItem
  durations — no new wire layout. Re-exported from the crate root. 5 new
  tests (3 unit + 1 disc-level extension + the empty-marks guard).
- **HDMV navigation-command disassembler** (`bdmv::nav_command`,
  `bdmv::movie_object`) — a single-line textual listing for
  decoded HDMV commands, built purely from the already-decoded
  `DecodedCommand` model plus the named PSR register table (no new wire
  knowledge). `Operation::mnemonic()` gives each operation a stable
  uppercase-camel name; `Operation::group()` / `is_branch()` /
  `is_playback()` classify it without re-reading the wire bits.
  `Operand::disassemble()` renders an immediate as `0x..`, a GPR as
  `r<idx>`, and a PSR as `PSR<idx>(<name>)` (with a trailing `'` for a
  secondary-addressing reference). `DecodedCommand::disassemble()` (and
  its `Display` impl) emit `<mnemonic> [op1[, op2]]` — e.g.
  `Eq PSR4(Title), 0xFFFF` / `Move r1, 0x1` / `JumpTitle 0x2`; an
  `Unknown` command keeps its raw `(sub_grp, selector)`. At the table
  level `NavCommand::disassemble()`, `MovieObject::disassemble(index)`
  (a flag header + one indexed line per command) and
  `MovieObjects::disassemble()` (the whole `MovieObject.bdmv` table) give
  a forensic dump of an HDMV script. Diagnostic only — the listing is not
  re-assemblable and nothing is executed (the interpreter stays in
  `bdmv::vm`). 14 new unit tests.
- **CLPI EP_map / CPI seek-index accessors** (`bdmv::clpi`, BD-ROM AV
  §5.7) — the keyframe-seek logic (binary-search the EP_map for the
  largest `pts_ep_start ≤ target`, return its `spn_ep_start`) lived only
  inside `disc.rs::seek_to`; a caller holding a parsed `.clpi` could not
  compute a landing packet without a full `Disc` / `TitleSource`. This
  surfaces it directly on the parsed structures: `EpMap::{
  entry_point_at_or_before(pts_90k), seek_spn(pts_90k), first_pts,
  last_pts, indexed_span_90k}` do the backward-safe binary search
  (clamping to the first entry for a pre-range target, staying on the
  last for a past-range target — the same policy `seek_to` applies), and
  `Cpi::seek_spn(pts_90k)` chains the existing `primary_video_ep_map`
  selection (HEVC main over AVC fallback on UHD-BD, else first known
  video EP_map, else lowest PID) into a one-shot SPN resolve. PTS is the
  clip-local 90 kHz axis the parser already folds the coarse+fine rows
  onto. 4 new unit tests cover the at-or-before snap (exact / between /
  pre-first / past-last), the span helpers, the empty-EP_map `None`
  path, the HEVC-over-AVC selection drive, and a 6-point parity check
  against the `disc.rs::seek_to` largest-at-or-before semantics.
- **CLPI ClipInfo + ProgramInfo demux accessors** (`bdmv::clpi`,
  BD-ROM Part 3 §5.5.4.1 / §5.5.4.3 + AV §3.1) — the second pure-
  derivation layer over already-parsed `.clpi` fields. `ClipInfo`
  grows the byte/SPN index a demuxer needs to turn a CPI EP_map's
  `spn_ep_start` into a file position: `clip_byte_len()`
  (`number_of_source_packets × 192`, the fixed BDAV source-packet
  size), `spn_to_byte(spn)` / `byte_to_spn(byte)` (inverse, range-
  checked against the packet count), and `transfer_duration_secs()`
  (multiplex-rate-derived transfer time `clip_byte_len /
  ts_recording_rate`, `None` on a zero rate). `ProgramInfo` /
  `ProgramEntry` grow the per-PID lookups a demuxer building a flat PID
  table wants: `ProgramEntry::{stream_by_pid, video_streams,
  audio_streams}` and `ProgramInfo::{stream_count, streams (flatten),
  stream_by_pid (cross-program), program_by_pmt_pid, primary_video_pid}`.
  All classification reuses the existing `StreamCodingType::is_video` /
  `is_audio`. 8 new unit tests cover the byte/SPN round trip + range
  boundaries, transfer-duration incl. the zero-rate guard, per-program
  and cross-program PID search, PMT-PID program lookup, and
  first-in-order primary-video-PID selection (incl. the audio-only
  `None` case).
- **CLPI SequenceInfo demux accessors** (`bdmv::clpi`, BD-ROM Part 3
  §5.5.4.2) — the SequenceInfo / AtcSequence / StcSequence structs the
  `.clpi` parser already populated carried zero accessor methods; the
  module doc's promise that they let a demuxer "map source-packet
  numbers to wall-clock times" was unfulfilled. This round wires that
  up, all pure derivation over already-parsed fields (no new wire
  layout): `StcSequence::{duration_45k, duration_90k, start_pts_90k,
  end_pts_90k, contains_spn_start}` lift the recorded 45 kHz
  presentation times onto the 90 kHz axis the rest of the stack uses;
  `AtcSequence::{stc_sequence(stc_id), stc_sequence_for_spn(spn)}`
  resolve a PlayItem's `stc_id_ref` (offset by the ATC's
  `offset_stc_id`) and the STC sequence owning a given source-packet
  number (last `spn_stc_start` at or before `spn`); `SequenceInfo::{
  stc_sequence_count, stc_sequences, stc_sequence_by_id,
  first_stc_sequence, presentation_span_90k}` flatten across ATC
  sequences, search a global `stc_id`, and compute the clip's total
  presentation span (latest end − earliest start, 90 kHz). 11 new unit
  tests cover the duration/PTS lift, inverted-time saturation,
  SPN-ownership boundaries (incl. None before the first start), the
  offset-`stc_id` resolution, cross-ATC flattening / span, and the
  empty-SequenceInfo case.
- Title-level HDMV **navigation driver** (`bdmv::nav_driver::NavDriver`):
  the disc title engine that ties `index.bdmv` to `MovieObject.bdmv`. The
  `MobjRunner` drives one object table and follows intra-table
  `JumpObject`/`CallObject`/`Resume`, but `JumpTitle`/`CallTitle` name a
  *title number* only `index.bdmv` can resolve to a Movie Object. The
  driver closes that loop: a `TitleEntry` (FirstPlayback / TopMenu /
  numbered Title N) resolves to its HDMV Movie Object via the index, PSR4
  (Title) is seeded on entry (`0xFFFF` for the menu), the `MobjRunner` runs
  with a shared register file, and the inter-title
  `JumpTitle`/`CallTitle`/title-`Resume` transitions are serviced by the
  driver itself — its own title-call stack (separate from the runner's
  object stack) keeps a `CallTitle`'s register writes visible to the caller
  after the called title returns. `PlayPL*` surfaces as a resolved
  `DriveOutcome::Play(PlayRequest { playlist, play_item, mark })` and the
  remaining player ops (`TerminatePL`/`Link*`/`SetSystem`) as
  `DriveOutcome::Request`; both are resumable with `NavDriver::resume` once
  serviced. Bad titles (out-of-range / BD-J) and pathological inter-title
  cycles are bounded (`DriveOutcome::BadTitle`/`BadObject`/`BudgetExhausted`).
  Adds `IndexBdmv::resolve_movie_object` / `entry` / `title_count` +
  `TitleEntry` + `IndexObjectType::hdmv_movie_object_id`/`is_hdmv`, and
  `MobjRunner::set_object_pc` for the driver's mid-table re-entry. Re-exported
  from the crate root (`NavDriver`, `DriveOutcome`, `PlayRequest`,
  `TitleEntry`, `DEFAULT_TITLE_BUDGET`).
- End-to-end HDMV navigation pipeline test (`tests/hdmv_vm_pipeline.rs`):
  hand-assembles a `MovieObject.bdmv` byte image (raw big-endian wire
  bytes — the 12-byte command words emitted by hand from the clean-room
  opcode table, not the crate's `encode` helper), parses it with
  `MovieObjects::parse`, then *executes* the command lists with
  `MobjRunner` against a shared register file. Covers arithmetic, the
  Compare conditional-skip, a `JumpTitle` yield, a multi-object
  `CallObject → JumpObject → return → JumpTitle` chain (asserting the
  callee's GPR write survives the return), and a player-seeded PSR4
  (Title) the script branches on.
- HDMV **Movie Object runner** (`bdmv::mobj_runner`): drives the whole
  `MovieObject.bdmv` table with one shared `HdmvVm` register file,
  following the inter-object Branch semantics the VM yields. `JumpObject`
  switches object (PC 0); `CallObject` pushes a `(object, pc)` resume
  frame then switches; `Resume` (and an object's list running off the end
  or hitting `Break`) pops the frame and continues the caller after its
  call — so GPRs a callee sets are visible to the caller, matching the
  single global register file on a player. Everything needing disc/player
  state the BDMV table alone does not carry (`JumpTitle`, `CallTitle`,
  the `PlayPL*` family, `TerminatePL`, `Link*`, `SetSystem`) is yielded
  as a `RunOutcome::Request(NavRequest)` and the run is resumable with
  `MobjRunner::resume` once the player services it. Bad object ids and a
  pathological inter-object Jump/Call cycle are bounded
  (`RunOutcome::BadObject` / `BudgetExhausted`). Re-exported from the
  crate root (`MobjRunner`, `RunOutcome`).
- HDMV navigation **virtual machine** (`bdmv::vm`): a minimal command
  interpreter that *executes* the decoded 12-byte nav-commands against a
  register file, the milestone above bare decode. `Registers` holds the
  two banks (4096 GPRs + 128 PSRs, 32-bit); `HdmvVm::step` / `run`
  evaluate each command. **Set / register ops** mutate the destination
  in place — Move, Swap, the arithmetic group (Add/Sub/Mul wrapping,
  Div/Mod truncating with ÷0→0), the bitwise group (And/Or/Xor), single
  bit Set/Clear, and the two shifts (≥32 → 0). **Compare ops**
  (BC/EQ/NE/GE/GT/LE/LT) implement the conditional-skip model — a false
  comparison skips the next command (`Step::Skipped`). **Branch ops**
  GoTo (PC redirect, out-of-range halts) / Break / Nop run in-VM; the
  navigation branches that leave the command list (JumpTitle, JumpObject,
  Call*, Resume, PlayPL*, TerminatePL, LinkPI/MK) and the SetSystem ops
  halt-and-yield a typed `NavRequest` with resolved operand values, so
  the surrounding player layer owns the title/IG transition. Navigation
  writes to read-only / Player-Setting / reserved PSRs are dropped per
  the register-model class; `Registers::set_psr_player` seeds player
  state. An infinite `GoTo` loop is bounded by a step budget. Clean-room
  from `docs/container/bluray/hdmv-navigation-commands.md`; re-exported
  from the crate root (`HdmvVm`, `Registers`, `NavRequest`, `Step`).
- End-to-end PGS / HDMV integration test (`tests/pgs_sup_pipeline.rs`):
  assembles a synthetic `.sup`-style PG byte stream by hand (raw
  big-endian wire bytes, not the crate's `encode` helpers) and drives it
  through `parse_segments` → `group_display_sets` →
  `DisplaySet::reassemble_objects` / `render` to an RGBA graphics plane,
  asserting segment ordering, ODS RLE expansion, palette resolution and
  composite placement; plus a hand-built `MovieObject.bdmv` whose
  navigation commands are decoded and whose register operands resolve
  against the PSR/GPR model (`JumpTitle`, `Move GPR1`, `EQ PSR4 (Title)`).

- Presentation Graphic Stream (PGS) segment parser in
  `bdmv::pgs`: the shared 13-byte PG segment header (`SegmentHeader`),
  the five typed segment bodies — `Pcs` (composition objects + cropping
  + `CompositionState` Epoch-Start / Acquisition-Point / Normal +
  palette-update flag), `Wds` (window list), `Pds` (YCbCr+alpha CLUT,
  entry count derived from body length), `Ods` (fragmented RLE bitmap
  with `FragmentFlag` First / Last / FirstAndLast), and the empty `END`
  — plus `parse_segments` for a whole PG / `.sup` byte stream and
  `decode_rle` which expands the ODS byte-oriented per-scanline
  run-length encoding (the four colour-run branches + end-of-line) into
  `width × height` paletted indices. Every segment round-trips through
  `Segment::encode` (which recomputes `segment_size`); malformed inputs
  (bad magic, truncated body, ragged PDS, RLE width / scanline-count
  mismatch) are rejected, never panic. Clean-room from
  `docs/container/bluray/pgs-segment-syntax.md`; re-exported from the
  crate root.
- HDMV navigation-command opcode decode: `NavCommand::decode` /
  `DecodedCommand` split the 12-byte command word into group
  (Branch/Compare/Set), sub-group, the named `Operation` (all GOTO /
  JUMP / PLAY branch ops, the seven Compare ops, the fifteen register
  Set ops, the eleven SetSystem ops), and the two operand words decoded
  into immediate values or GPR/PSR register references. Clean-room from
  `docs/container/bluray/hdmv-navigation-commands.md`; every worked-hex
  example in the table round-trips.
- HDMV register model in `bdmv::register_model` — the PSR / GPR naming
  layer above the raw `Operand::Register` bank+index. `psr_info(index)`
  maps a Player Status Register index to a `PsrInfo` (name + `PsrClass`
  Playback-Status / read-only Playback-Status / Player-Setting /
  Reserved), covering the named PSR0–20/29–31/36–44 set, the
  characteristic-text-capability range PSR48–61, and every reserved hole;
  `PsrClass::is_read_only_to_nav` tells whether a navigation command may
  mutate it. `gpr_convention(index)` returns the authoring-convention use
  of a GPR plus a `bd_j_reachable` flag (GPR1000–4005). `GPR_COUNT` (4096)
  / `PSR_COUNT` (128) constants. `Operand::resolve_register` joins a
  decoded register-reference operand to its `ResolvedRegister`
  (bank + index + named `PsrInfo` for PSR refs). Clean-room from the
  "Register model" section of
  `docs/container/bluray/hdmv-navigation-commands.md`; re-exported from
  the crate root. 11 new unit tests.
- PGS Display Set grouping + ODS fragment reassembly in `bdmv::pgs`,
  the layer above the flat segment list: `group_display_sets` /
  `parse_display_sets` slice a segment list into `DisplaySet`s on each
  PCS boundary (`{ pcs, wds, palettes, objects, pts }`, framing
  `PCS -> WDS -> PDS ... -> ODS ... -> END`), and
  `DisplaySet::reassemble_objects` folds each ODS fragment chain
  (fragments sharing one `object_id`, opened by `First`/`FirstAndLast`
  carrying `width`/`height`, closed by `Last`) into a
  `ReassembledObject` whose concatenated RLE bytes are validated against
  the first fragment's `object_data_length - 4` (width+height+RLE
  wire-observation); `ReassembledObject::decode` runs `decode_rle` for
  the paletted bitmap. Malformed framing (segment before PCS, second PCS
  before END, two WDS, trailing DS without END) and malformed chains
  (continuation with no open chain, duplicate `object_id`, never-closed
  chain, declared-length mismatch) surface `BlurayError::Malformed`.
  `DisplaySet` / `ReassembledObject` / `group_display_sets` /
  `parse_display_sets` re-exported from the crate root. Clean-room from
  `docs/container/bluray/pgs-segment-syntax.md`. 14 new unit tests.
- PGS renderer in `bdmv::pgs` — the layer that turns parsed Display Sets
  into the actual subtitle bitmap. `PaletteEntry::to_rgba` /
  `ycbcr709_to_rgb` apply the BT.709 limited-range YCbCr→RGB conversion
  (alpha passed through); `Palette` is a 256-entry CLUT built from one or
  more `Pds` with incremental-update semantics (`apply`,
  `from_palettes`, `from_palettes_with_id` selecting by the PCS's
  `palette_id`; unwritten indices stay transparent).
  `DecodedObject::to_rgba` resolves CLUT indices to an `RgbaImage`
  (straight-alpha pixels, `to_rgba_bytes` for a packed RGBA8888 buffer);
  `DisplaySet::render` composites every composition object — decoded,
  palette-resolved, cropped to its `object_cropping_*` sub-rectangle when
  `object_cropped_flag == 0x40`, and clipped to the plane — into a
  `RenderedDisplaySet` graphics plane (`pcs.width × pcs.height`) at each
  object's `(object_horizontal_position, object_vertical_position)`. A
  composition object referencing an absent `object_id` is rejected.
  `Rgba8` / `RgbaImage` / `Palette` / `RenderedDisplaySet` re-exported
  from the crate root. Clean-room from
  `docs/container/bluray/pgs-segment-syntax.md` (the *"Color is YCbCr +
  alpha (BT.709 range as used on BD)"* palette-entry note). 10 new unit
  tests.

## [0.0.3](https://github.com/OxideAV/oxideav-bluray/compare/v0.0.2...v0.0.3) - 2026-06-15

### Other

- typed stream_coding_info() accessors on ProgramInfo StreamCodingInfo
- add cargo-fuzz harness over descriptor parsers; fix AVDP truncation panic
- follow Allocation Extent Descriptor continuation chains (ECMA-167 §14.5 / §12)
- long_ad + ext_ad allocation descriptors in File Entry walks (ECMA-167 §14.14.2/§14.14.3)
- surface PlayItem playback-control fields via PlayItemFlags
- typed nibble accessors for video/audio attribute fields
- drop release-plz.toml — use release-plz defaults across the workspace
- typed PlayList_playback_type accessor + AppInfoPlayList::playback_kind
- typed EP_stream_type accessor + HEVC-aware EP_map selector
- parse ExtendedFileEntry per ECMA-167 §14.17
- in-place mid-stream angle switching on TitleSource
- enumerate mid-stream angle-change boundaries from CPI EP_map
- expose cross-PlayItem STC PTS continuity map (§5.4.4.2 + §5.5.4.2)
- per-title TrackCatalogue + populate TitleInfo::languages
- stream chapter bytes through TitleSource instead of buffering
- fix EP_map body_addr base — skip length u32 + head u16
- accept 'HDMV' type_indicator (BD-ROM Part 3 §5.5.1.1)
- implement MultiTitleSource for bluray://
- Disc::open_title_chapters per-chapter byte-segment iterator
- ?title=N + ?chapters=A-B|A,B,C query selectors on bluray://
- add unique_titles() dedup + optional title_meta() from BDMV/META
- Disc::volume_label() from the UDF Primary Volume Descriptor
- keep per-stream STN_table detail (PID / codec / language)
- map KEYDB.cfg DK fields to the spec-compliant DeviceKey
- switch transport from SG_IO to CDROM_SEND_PACKET
- Type-4 KCD support + clear diagnostic for unwireable path
- AACS Drive-Host AKE fallback for the Linux SG_IO Volume ID read
- env-var override for Vuk / Media Key

### Added

- **CLPI ProgramInfo `stream_coding_info()` typed attribute accessors
  (BD-ROM Part 3 §5.5.4.3)** — the per-PID `StreamCodingInfo` block in
  a `.clpi` ProgramInfo carries the same length-prefixed
  `stream_coding_info()` structure as an MPLS STN_table stream entry
  (§5.4.4.4): `stream_coding_type` + the packed attribute nibbles +
  (for audio / graphics) a 3-byte ISO 639-2/T language tag. The parser
  previously kept only `stream_coding_type` + the first attribute byte
  and discarded the rest. It now reads every byte inside the recorded
  `sc_len` body, surfacing `StreamCodingInfo::aspect_ratio_nibble` and
  `language_code`, plus typed accessors `coding_type()`,
  `video_format_kind()` / `frame_rate_kind()` / `aspect_ratio_kind()`
  (video), `audio_format_kind()` / `sample_rate_kind()` (audio), and
  `language()` (lowercased, `None` when absent) — reusing the
  `StreamCodingType` / `VideoFormat` / `FrameRate` / `AspectRatio` /
  `AudioFormat` / `SampleRate` enums the MPLS surface already exposes.
  A remuxer can now read a clip's per-stream codec + resolution +
  channel layout + language from its own ProgramInfo without a matching
  `.mpls` open. The encode path reconstructs the variable-length body
  (video aspect-ratio nibble vs audio/graphics language tag) so the
  extra fields round-trip. 4 new unit tests.
- **Allocation Extent Descriptor continuation chains (ECMA-167 §14.5 /
  §12 figure 7)** — an allocation descriptor whose §14.14.1.1 extent
  type is 3 ("the extent is the next extent of allocation
  descriptors") previously made `FileEntry::parse` bail `Unsupported`.
  The pointer now terminates its AD field per §12 (a descriptor
  recorded after it is not consumed) and is surfaced as
  `FileEntry::continuation: Option<AdContinuation>` (length / block /
  optional partition_ref — `None` for the short_ad flavour whose
  partition is implied per §14.14.1.2; the ext_ad flavour is exempt
  from the §14.14.3 Note 46 compressed-extent check since the pointer
  carries no file data). `UdfDisc::read_file_entry` resolves the
  chain: each continuation extent starts with an Allocation Extent
  Descriptor (new `AllocationExtentDescriptor` parse/encode — Tag 258,
  Previous Allocation Extent Location §14.5.2, L_AD §14.5.3) followed
  by `L_AD` bytes of further descriptors of the File Entry's flavour,
  appended to the entry's AD vectors so `FileEntry::extents()` sees
  the full flattened sequence. The walk is depth-capped at 32 (a
  cyclic chain is refused `Malformed` instead of looping forever),
  refuses a continuation extent shorter than the 24-byte AED header
  or larger than 1 MiB, and keeps the cross-partition refusal. The
  per-flavour AD field parser is factored out of `FileEntry::parse`
  so the in-entry field and every AED body decode identically. 6 new
  tests: continuation surfaced + field-terminated for long_ad,
  implied-partition short_ad pointer, compressed-check-exempt ext_ad
  pointer, AED header round-trip + wrong-tag rejection, plus two
  synthetic-image end-to-end cases (a file whose AD field chains
  through an AED block reads back byte-exact across both extents, and
  a self-pointing cyclic AED chain refused `Malformed`).
- **Long + Extended Allocation Descriptors in File Entries (ECMA-167
  §14.14.2 / §14.14.3)** — the UDF File Entry allocation walk
  previously refused any ICB-tag ad-type other than short_ad /
  embedded. The parser now decodes `long_ad` (16-byte: Extent Length
  with the §14.14.1.1 type bits, `lb_addr` Extent Location,
  6 implementation-use bytes) and the new `ExtAd` (20-byte ext_ad:
  Extent Length, Recorded Length, Information Length, `lb_addr`,
  2 implementation-use bytes) areas into `FileEntry::long_ads` /
  `FileEntry::ext_ads`, and a new `FileEntry::extents() ->
  Vec<AllocExtent>` normalises all three flavours (length /
  extent_type / block / optional partition_ref) so
  `UdfDisc::read_file` runs a single extent loop. Per the
  single-partition BD-ROM assumption, a long/extended extent whose
  `lb_addr` names a partition other than the mounted one
  (`UdfDisc::partition_number`, newly surfaced from the Partition
  Descriptor §10.5) is refused `Unsupported` instead of being
  misresolved against the wrong partition base. An ext_ad whose
  Recorded Length differs from its Information Length — a compressed
  extent per §14.14.3 Note 46 — is refused at parse time; an
  accepted ext_ad contributes its Information Length bytes (not the
  block-rounded Extent Length) to the file body. Extent-type-3
  continuation pointers (Allocation Extent Descriptor chains) stay
  refused for every flavour. 7 new tests: ext_ad round-trip,
  two-long_ad File Entry normalisation, uncompressed ext_ad
  normalisation, compressed-ext_ad rejection, continuation-long_ad
  rejection, plus two synthetic-image end-to-end cases (a two-block
  long_ad file reading back byte-exact through `read_path`, and a
  cross-partition long_ad refused). (HDMV navigation-command opcode
  decode was investigated for this round and is blocked on docs
  staging: the BDA whitepapers cover the HDMV programming model only
  at the overview level — §2.2.1.5.1, three operation groups +
  register file — and `docs/container/bluray/`'s own README lists
  "all MOBJ opcodes" among the member-gated gaps, so
  `MovieObject.bdmv` commands stay opaque 12-byte `NavCommand`
  records; see README "Deferred".)

- **PlayItem playback-control fields (`PlayItemFlags`)** — the
  `PlayItem_random_access_flag`, `still_mode` byte, `still_time`
  word, and the raw multi-angle flags byte (BD-ROM Part 3 §5.4.4.1)
  were previously consumed-and-discarded by the `.mpls` parser; they
  are now surfaced through a new `PlayItemFlags` struct on
  `PlayItem::flags`. `random_access_flag` is decomposed into a typed
  `bool` (the top bit of the byte following the 8-byte UO mask
  table — the layout the parser has always assumed); `still_mode`,
  `still_time`, and `angle_flags` are surfaced verbatim because their
  internal bit semantics are not pinned by the consulted references.
  Both the `parse` and `encode` paths now read/write these fields
  instead of treating them as fixed zeros, so a hand-built PlayItem's
  random-access intent and still-frame dwell survive an
  encode → parse round trip. Re-exported from the crate root. Five
  new unit tests cover the default-all-clear state, a
  random-access + still-mode + still-time round trip, the multi-angle
  flags byte round trip on a multi-angle PlayItem, and the
  top-bit-only isolation of the random-access flag.

- **STN_table video / audio attribute typed accessors
  (`VideoFormat` / `FrameRate` / `AspectRatio` / `AudioFormat` /
  `SampleRate`)** — five new enums covering the 4-bit nibbles
  BD-ROM Part 3 §5.4.4.4 records inside each PlayItem's per-stream
  `stream_attributes` block, plus the matching disc-wide-default
  nibbles in `index.bdmv` AppInfoBDMV §5.3. Variants follow the
  documented BD-AV codes: `VideoFormat` = 480i / 576i / 480p /
  1080i / 720p / 1080p / 576p / 2160p; `FrameRate` = 23.976 / 24 /
  25 / 29.97 / 50 / 59.94; `AspectRatio` = 4:3 / 16:9;
  `AudioFormat` = Mono / Stereo / Multi (5.1) / Combo (5.1 + stereo
  downmix); `SampleRate` = 48 kHz / 96 kHz / 192 kHz / 48-192 combo /
  48-96 combo. Each enum carries an `Other(u8)` catch-all so a
  reserved nibble round-trips losslessly. Methods on each:
  `from_raw(u8) -> Self` (masks the low nibble — callers can pass
  the un-shifted wire byte `video_format(4) | frame_rate(4)`
  directly), `as_raw(self) -> u8` (round-trips through the 4-bit
  field), plus per-enum helpers — `VideoFormat::is_progressive()` +
  `vertical_lines() -> Option<u16>`, `FrameRate::fps_q() ->
  Option<(u32, u32)>` (exact rational for safe metadata
  propagation) + `is_fractional()`, `AspectRatio::ratio() ->
  Option<(u8, u8)>` + `is_widescreen()`,
  `AudioFormat::channel_count() -> Option<u8>` + `has_downmix()`,
  `SampleRate::primary_hz() -> Option<u32>` + `is_combo()`. Typed
  accessors `video_format_kind()` / `frame_rate_kind()` /
  `aspect_ratio_kind()` land directly on `PrimaryVideoStream` and
  `SecondaryVideoStream`; `audio_format_kind()` /
  `sample_rate_kind()` land on `PrimaryAudioStream` and
  `SecondaryAudioStream`; `AppInfoBdmv` exposes
  `video_format_kind()` / `frame_rate_kind()` for the disc-wide
  defaults (sharing the same enum lets a player run one switch
  over both the disc default and the per-stream view). The raw
  `u8` fields stay public so existing consumers compile unchanged.
  15 new unit tests cover the named-variant round-trips, the
  `Other` catch-all for reserved nibbles, nibble masking on the
  un-shifted wire byte, the per-enum helper predicates, the
  per-stream accessors on `PrimaryVideoStream` /
  `PrimaryAudioStream` / `SecondaryVideoStream` /
  `SecondaryAudioStream`, the `AppInfoBdmv` accessors with full
  variant coverage, and end-to-end MPLS + `index.bdmv` encode →
  parse round-trips so the typed view stays consistent with the
  wire bit-packing `PlayListMpls::encode` / `IndexBdmv::encode`
  already exercise. Re-exported from the crate root next to the
  existing `StreamCodingType` / `PlayListPlaybackType`. Spec basis:
  BD-ROM Part 3 §5.4.4.4 (per-stream `stream_attributes` table) +
  AppInfoBDMV §5.3 (disc-wide default nibbles).
- **PlayList `playback_type` typed accessor (`PlayListPlaybackType`)**
  — new enum parallel to `MarkType` / `ConnectionCondition` /
  `StreamCodingType`, covering the documented values that the
  `PlayList_playback_type` byte recorded in `AppInfoPlayList()` carries
  (BD-ROM Part 3 §5.4 AppInfoPlayList). Variants: `Sequential` (0x01),
  `Random` (0x02 — random pick without replacement), `Shuffle` (0x03 —
  random pick with replacement), `Other(u8)` catch-all preserved for
  forward-compatibility. Methods: `from_raw(u8) -> Self`,
  `as_raw(self) -> u8` (round-trips through the wire byte),
  `is_sequential(self)` (true only for the recorded-order variant),
  `is_randomised(self)` (true for both random-pick variants — useful
  for a UI that wants a single "non-sequential" indicator).
  `AppInfoPlayList::playback_kind() -> PlayListPlaybackType` exposes
  the typed view directly on the already-parsed AppInfo. The raw
  `playback_type: u8` field stays public so existing consumers
  continue to compile, the typed accessor sits alongside as the
  pattern-match-friendly surface. Six new unit tests cover the named
  variants, `Other` round-trips for five sentinel bytes, the
  `is_sequential` / `is_randomised` helpers, the `playback_kind()`
  accessor on a constructed `AppInfoPlayList`, and an end-to-end
  encode → parse round-trip through `PlayListMpls` for every variant
  (so the typed view stays consistent with the wire layout
  `PlayListMpls::encode` / `parse` already exercise). `PlayListPlaybackType`
  re-exported from the crate root next to `AppInfoPlayList`.
- **CPI EP_stream_type typed accessor (`EpStreamType`)** — new enum
  parallel to `StreamCodingType` (BD-ROM AV §5.7 4-bit
  `EP_stream_type` field). Variants: `Reserved` (0x0),
  `Mpeg2Video` (0x1), `AvcVideo` (0x5), `Vc1Video` (0x6),
  `HevcVideo` (0x8 — UHD-BD per BD-ROM-AV HEVC whitepaper),
  `Other(u8)` (raw 4-bit catch-all), `Unset` (`Default`). Methods:
  `from_raw(u8) -> Self` (masks the high nibble so a stale byte
  still classifies), `as_raw(self) -> u8` (round-trips through the
  wire format; `Unset` encodes as `Reserved` for determinism on
  re-encode), `is_video()` (true for the four known BD video
  codes), `is_hevc()`, `label() -> &'static str` (short
  UI-friendly token). `EpMap::kind()` / `EpEntry::kind()` /
  `EpMap::is_video()` expose the typed view directly on the
  already-parsed CPI structures, so a flat iterator over EP_map
  entries is self-describing without re-fetching the parent header.
- **`Cpi::primary_video_ep_map() -> Option<&EpMap>`** — principled
  selector for the EP_map a keyframe-seeker should drive against.
  HEVC wins over AVC / MPEG-2 / VC-1 on UHD-BD authoring patterns
  that ship both an HEVC main EP_map and an AVC fallback EP_map
  inside the same CPI block; the older `min_by_key(stream_pid)`
  heuristic became the fallback when every EP_map carries an
  unknown `EP_stream_type` byte (legacy fixtures, future BD
  profiles). Two companion accessors land alongside:
  `Cpi::ep_map_by_kind(EpStreamType) -> Option<&EpMap>` for callers
  that want a specific track (e.g. forcing AVC fallback on a UHD
  title), and `Cpi::video_ep_maps()` for a UI listing of every
  seekable video EP_map. `disc::load_clip_meta` now drives off
  `primary_video_ep_map()` instead of the lowest-PID heuristic, so
  UHD-BD titles whose AVC fallback EP_map sits at a numerically
  smaller PID than the HEVC main now seek against the HEVC entry
  points — the only EP_map whose keyframes a UHD-BD player would
  actually decode. Synthetic regression fixture covers the
  authoring pattern (`primary_video_selector_picks_hevc_over_avc_on_uhd_layout`).
  16 new unit tests in `bdmv::clpi::tests`. Spec basis: BD-ROM AV
  §5.7 (EP_map 4-bit `EP_stream_type` header field) + BD-ROM-AV
  HEVC whitepaper (UHD-BD HEVC profile mapping to code 0x8).

- **ExtendedFileEntry (§14.17) parsing** — `FileEntry::parse` now
  recognises Tag 266 alongside Tag 261 and decodes the full §14.17
  layout: 40-byte-longer prefix (216 vs 176), `Object Size` (u64 at
  BP 64) surfaced through a new `FileEntry::object_size: Option<u64>`
  field, shifted `Logical Blocks Recorded` (BP 72) / `L_EA` (BP 208) /
  `L_AD` (BP 212) offsets, the fourth `Creation Date and Time`
  timestamp slot (BP 104), the additional reserved word (BP 132), and
  the new `Stream Directory ICB` long_ad slot (BP 152) which is read
  but not followed (named-stream walking stays Phase-2). Plain FE
  parsing continues to report `object_size = None` and lives at the
  same 176-byte prefix offsets it always did, so existing callers
  (`UdfDisc::read_file` / `read_directory` / every `tests/` synthesised
  image) decode unchanged. A new `FileEntry::is_extended()` selector
  reports the source tag. Replaces the previous early-bail
  `BlurayError::Unsupported("ExtendedFileEntry")` — UDF 2.50 / 2.60
  discs that author their root directory or named-stream files using
  Extended File Entries now mount through. Allocation descriptor walk
  is identical between the two variants (Short / Long / Extended /
  EmbeddedInIcb codes still source from the ICB Tag flags), so the
  same `short_ads: Vec<ShortAd>` / `embedded_data: Vec<u8>`
  out-parameters land for either tag. Spec basis: ECMA-167 §14.17 (the
  EFE figure 4/48 byte map) + §14.9 (the shared FE prefix this builds
  on). No new spec dependency. Four new unit tests in `udf::tests`
  cover (a) an EFE-embedded directory round-trip with non-trivial
  `object_size` distinct from `information_length`; (b) an EFE with a
  single short_ad pointing at a file extent; (c) a regression that a
  plain FE still reports `object_size == None`; (d) a truncated EFE
  buffer rejected as `BlurayError::Malformed` rather than mis-decoded
  as a plain FE.

- **In-place mid-stream angle switching** — new
  `TitleSource::switch_angle_at(new_angle, title_pts_90k) -> io::Result<u64>`
  retargets an open source to a different angle's `.m2ts` / `.clpi`
  pair at a keyframe-aligned title PTS, without dropping the
  decryptor or recreating the source. Every PlayItem is revalidated
  against `new_angle` before any state mutation — an out-of-range
  angle surfaces `io::ErrorKind::InvalidInput` and the source stays on
  the previous angle. On success the per-clip seek index is rebuilt
  (each PlayItem's clip stem is reselected via
  `PlayItem::angle_clip`, the new `.m2ts` is re-measured, the new
  `.clpi` is re-parsed for EP_map + STC origin + angle-change rows),
  the reader is opened on the destination angle's `.m2ts` at the
  6144-byte AACS-unit-aligned source-packet that contains the chosen
  EP_map entry, and `current_angle` is updated. The convenience
  `TitleSource::switch_angle(new_angle)` finds the first
  `AngleChangePoint` at or after the current output position and
  delegates — the typical "user pressed the angle button mid-playback"
  UI path. When the title's CPI carries no flagged rows, or the
  reader is past the last boundary, the convenience wrapper returns
  `io::ErrorKind::NotFound` and leaves the source untouched.
  `TitleSource::current_angle() -> u8` reports the angle currently
  driving the reader; `TitleSource::num_angles() -> u8` reports the
  smallest PlayItem-angle-count across the title (so any value
  `< num_angles()` is guaranteed safe to pass to `switch_angle_at`).
  The output-byte axis becomes a new physical stream after a switch —
  alternate angles' clip bytes live in different `.m2ts` files, so
  even when their per-clip packet counts match, the absolute output
  position is renumbered against the destination angle's stream.
  Callers tracking position by byte should re-anchor against the
  returned `u64`. Spec basis: BD-ROM Part 3 §5.4.4.1 `is_multi_angle`
  block + AV §5.2.3.3 interleaved-clip layout + §5.7 angle-change EP
  flag. No new spec dependency. Internally the per-clip seek-index
  build that `TitleSource::new` used was extracted into a free
  function `build_clip_seek_index(&Path, &[PlayItem], u8) -> (clips,
  output_total, title_duration_90k)` so `new()` and `switch_angle_at`
  share the same code path. `TitleSource` grew two fields:
  `play_items: Vec<PlayItem>` (kept for the rebuild) and
  `current_angle: u8`.
- **Six new integration tests** (`tests/switch_angle.rs`): a
  2-PlayItem × 3-angle synthetic title with fingerprinted `.m2ts`
  bytes per angle covers `current_angle` / `num_angles` reporting;
  `switch_angle_at` to an intra-PlayItem ACP boundary (next read
  returns the destination angle's fingerprint, output position lands
  on the EP_map row's SPN × 188); `switch_angle_at` across the
  PlayItem seam (lands on PI1's start on the new angle); out-of-range
  angle returns `InvalidInput` and the source still streams the
  previous angle; `switch_angle` finds the first boundary at-or-after
  the current output position; a single-angle title with no flagged
  rows returns `NotFound` from `switch_angle` and leaves the source
  untouched.

- **Mid-stream angle-change-point enumeration** — new
  `TitleSource::angle_change_points() -> Vec<AngleChangePoint>` plus
  the convenience `TitleSource::next_angle_change_point(pts_90k)`
  and the file-less peer
  `Disc::title_angle_change_points(title)` + angle-aware
  `…_with_angle(title, angle)` surface every CPI EP_fine row whose
  `is_angle_change_point = 1` bit is set (BD-ROM AV §5.7), folded
  onto the title timeline + output-byte axis. Each
  `AngleChangePoint { play_item_index, clip_stem, title_pts_90k,
  output_byte, clip_pts_90k, spn }` is a video access unit at which
  a mid-stream angle switch is guaranteed clean — every alternate
  angle's interleaved clip (Part 3 §5.4.4.1 `is_multi_angle` block)
  carries a co-incident I-frame at the matching source-packet
  number, so a player UI does the switch as `read up to output_byte
  + close + open_title_with_angle(new) + seek_to(title_pts_90k)`.
  ACPs whose clip-local `pts_ep_start` sits before the owning
  PlayItem's IN point are silently dropped (the streamer never
  reaches them). The `title_angle_change_points` peer matches the
  `chapters` / `title_streams` / `title_pts_continuity_segments`
  swallow-error policy: empty list on `.mpls` / `.clpi` read /
  parse failure, empty on out-of-range angle. The new
  `AngleChangePoint` type is re-exported at the crate root.
  Internally `load_clip_meta` grew a third return slot
  (`angle_change_eps: Vec<(u32, u32)>`) feeding a new
  `ClipSeekInfo::angle_change_eps` field — same CLPI parse cost as
  before. Six new integration tests
  (`tests/angle_change_points.rs`): 2-PlayItem × 3-angle title with
  three flagged rows yields every ACP at the correct title-PTS +
  output-byte; `next_angle_change_point` walks rows
  at-or-after a cursor in title order; `Disc::title_angle_change_points`
  matches the source's view; alt-angle CPI yields the same
  title-timeline coordinates as the primary; pre-IN-point ACP
  dropped; flag-clear CPI returns an empty list.

- **Cross-PlayItem STC PTS continuity map** — new
  `TitleSource::pts_continuity_segments()` plus the file-less peer
  `Disc::title_pts_continuity_segments(title)` (and an angle-aware
  variant `…_with_angle(title, angle)`) return one
  `PtsContinuitySegment` per PlayItem in playback order: a record of
  where that PlayItem's bytes live in the output stream
  (`output_byte_start` / `output_byte_end`), where the segment sits on
  the title timeline (`title_pts_start` / `title_pts_end` in 90 kHz),
  the PlayItem's IN / OUT lifted onto the same 90 kHz axis
  (`clip_in_pts_90k` / `clip_out_pts_90k`, doubled from §5.4.4.1's
  45 kHz fields), the clip-local STC origin lifted from the CLPI
  `SequenceInfo` (§5.5.4.2 — `presentation_start_time` of the STC
  sequence picked by the PlayItem's `stc_id_ref`), the `stc_id_ref`
  itself, and the seam's `ConnectionCondition` (§5.4.4.2 —
  `NonSeamless` / `SeamlessContinuation` / `SeamlessNewStc`). A
  downstream MPEG-TS demuxer reprojects each PES packet inside a
  segment as `title_pts = title_pts_start + (pes_pts - clip_in_pts_90k)`;
  the convenience `TitleSource::map_clip_pts_to_title_pts(byte_pos,
  pes_pts)` does the binary search for one-shot callers and returns
  `None` when the requested PES PTS sits before the segment's IN
  (leading bytes that the demuxer should drop). The first PlayItem's
  recorded `connection_condition` byte is meaningless (defined as the
  relation to the *previous* PlayItem, of which there is none) so the
  surface always normalises it to `NonSeamless`. Internally the same
  walk feeds a renamed `load_clip_meta` helper that now returns both
  the primary EP_map (was `load_entry_points`) and the
  CLPI-resolved STC origin in one CLPI parse — no extra I/O per clip.
  `ClipSeekInfo` gained three fields (`connection_condition`,
  `stc_id_ref`, `stc_origin_pts_90k`) and `TitleSource` caches the
  title's 90 kHz duration so the final segment's `title_pts_end`
  doesn't need a separate MPLS walk.
- **`PtsContinuitySegment` re-exported at the crate root.**
- **Six new integration tests** (`tests/pts_continuity.rs`): a 3-PlayItem
  tile that verifies byte / PTS bounds contiguity + per-clip STC origin
  threading; first-PlayItem `connection_condition` normalisation; empty
  `SequenceInfo` falls back to a 0 STC origin; the `map_clip_pts_to_title_pts`
  walk across a NonSeamless / SeamlessNewStc seam; a PlayItem with a
  non-zero IN-point reprojects correctly; missing MPLS yields an empty
  segment list (matches the `chapters` / `title_streams` swallow-error
  policy).
- **`Disc::title_streams(title) -> TrackCatalogue` — per-title track
  catalogue** (BD-ROM Part 3 §5.4.4.4 STN_table lift). The MPLS parser
  already decoded every PlayItem's `StnTable` into typed per-class
  vectors (`primary_video`, `primary_audio`, `pg_subtitles`, …), but no
  high-level helper aggregated those across a title's PlayItems for a
  remuxer to consume as a flat track list. The new catalogue merges
  every PlayItem's entries by `(elementary_pid, kind)` (PIDs are stable
  across PlayItems per AV §5.2.3.3) and returns one `Track { pid, kind,
  coding_type, language, playitem_count }` per distinct elementary
  stream, in canonical STN class order: `PrimaryVideo` → `PrimaryAudio`
  → `PgSubtitle` → `IgMenu` → `SecondaryAudio` → `SecondaryVideo` →
  `PipPgSubtitle`. `language` is the lowercased 3-letter ISO 639-2/T
  tag from the per-stream attributes block (`None` for video / IG / PiP
  PG which carry no `language_code`, for the spec sentinel `b"\0\0\0"`,
  and for non-ASCII raw bytes). `playitem_count` records how many
  PlayItems in the title's PlayList carried that PID — equals
  `play_items.len()` for the common single-angle case, less when a clip
  drops a commentary track. `TrackCatalogue::by_pid` / `by_kind` give
  O(n) lookups for downstream selectors. Returns an empty catalogue on
  read / parse failure (matches `Disc::max_angle` /
  `Disc::chapters`). The new types `Track` / `TrackKind` /
  `TrackCatalogue` are re-exported at the crate root.
- **`TitleInfo::languages` populated at mount time** (was a documented
  Phase-1 stub returning `Vec::new()`). Populated from every audio
  (primary + secondary) + subtitle (PG + PiP PG) + IG entry's
  `language_code` field on the title's playlist, sorted and
  deduplicated through a `BTreeSet<String>` and lowercased so a disc
  shipping `ENG` and `jpn` returns `vec!["eng", "jpn"]`. Empty when the
  playlist is missing / unreadable or when every entry's language is
  the spec sentinel `b"\0\0\0"`. Reuses the same MPLS parse as the
  existing duration calculation — no extra I/O per title.
- **Four new integration tests** (`tests/track_catalogue.rs`):
  multi-PlayItem deduplication keeps `playitem_count == 2` for every
  shared PID; `by_kind` walks only the requested class; mount-time
  `TitleInfo::languages` is sorted-lowercased-deduplicated against a
  mixed-case 3-tag synthetic disc; PID-0 (non-in-mux SubPath) entries
  are filtered out of the catalogue. Three new unit tests cover
  `decode_lang_code` (sentinel rejection, non-ASCII rejection,
  lowercasing) and `class_order` (matches §5.4.4.4 declaration order).

- **`cargo-fuzz` harness over the UDF / ECMA-167 descriptor parsers**
  (`fuzz/fuzz_targets/udf_descriptors.rs`) — a libFuzzer target that
  multiplexes every public `oxideav_bluray::udf` `parse(&[u8])` entry
  point (descriptor tag, the §7.1/§14.14 allocation descriptors, the
  §14.5 Allocation Extent Descriptor, the §10 volume descriptors, the
  §14 File Set / File Identifier / File Entry / ICB Tag, and the OSTA
  §2.1.3 d-string decoders) behind a one-byte selector. ~31M
  executions crash-free. Built against the crate with
  `default-features = false` so the fuzz binary pulls in only the
  parser code.

### Fixed

- **`AnchorVolumeDescriptorPointer::parse` panic on a checksum-valid
  but truncated buffer** (ECMA-167 §10.2) — the parser read its two
  fixed-offset `extent_ad` fields at BP 16/24 without first checking
  the buffer held the full 32-byte descriptor, so a 16-byte slice
  carrying a valid AVDP descriptor tag (passing the §7.2.3 checksum)
  panicked on the slice instead of returning an error. It now reports
  `Malformed` for any buffer shorter than 32 bytes. Found by the new
  `udf_descriptors` fuzz target; covered by regression test
  `avdp_truncated_after_valid_tag_is_rejected`.

### Changed

- **Tempdir helpers across `tests/` now use an atomic monotonic counter
  alongside `SystemTime`-derived nonce** — bare nanoseconds collided
  when several integration tests ran concurrently in the same
  test binary, causing intermittent `BDMV header truncated` failures
  when one test's `Disc::mount` raced with another's
  `fs::create_dir_all`.
- Scrubbed two clean-room attribution leaks from doc comments: a
  "matches libbluray's clean-room read path" tag in
  `src/bdmv/mpls.rs` and a "libaacs uses this for the same reason"
  tag in `src/drive/linux.rs`. Both replaced with spec citations
  (BD-ROM Part 3 §5.4.4.4 / kernel-userspace MMC CDB rationale).

## [0.0.2](https://github.com/OxideAV/oxideav-bluray/compare/v0.0.1...v0.0.2) - 2026-05-29

### Other

- title-relative chapter list from PlayListMark entry marks
- multi-angle PlayItem parsing + Disc::open_title_with_angle
- add iter_source_packets + strip_tp_extra_to_vec
- scrub libaacs/libbluray function-name citations from doc-comments
- VUK lookup cascade — KEYDB → on-disk cache → online derivation
- keyframe-aligned TitleSource::seek_to over CPI EP_map (Phase 2b)
- decode CPI EP_map per BD-ROM AV §5.7
- derive disc_id by hashing AACS/Unit_Key_RO.inf, not the drive Volume ID
- macos drive: add MMCDeviceInterface fallback when SCSITaskUserClient is unavailable
- macos drive: walk parent chain when SCSITaskUserClient plugin isn't on the matched service
- real IOKit/MMC AACS Volume Identifier reader
- add platform module + rewire AACS adapter for drive→disc_id flow
- hard EOF at output_total to bound runaway readers
- oxideav-aacs 0.1 (was 0.0 — release-plz bumped aacs to 0.1.0)
- surface every early-bail path with stderr diagnostic
- rustfmt 1.95 collapse on aacs_adapter
- dump KEYDB entries + AACS/ contents + cert head on failure
- trial every CPS Unit + consume pre-unwrapped Unit Keys
- hard-fail when AACS/ dir present but no VUK matches
- integrate decryption into bluray:// source driver
- support backwards seek via rewind + forward-skip
- seek for rewind / end-position / forward-skip

### Added

- **Title-relative chapter list from PlayListMark entry marks** (`mpls`
  + `disc`). The `.mpls` parser already read each `PlayListMark`
  (`mark_type`, `ref_play_item_id`, clip-local `mark_time_ticks` …, BD-ROM
  Part 3 §5.4.5), but nothing turned them into a navigable chapter list:
  a mark's timestamp lies on its referenced PlayItem's *clip-local* time
  axis, while a player's chapter search navigates the *title* timeline
  (every PlayItem's `[IN, OUT]` window concatenated). New
  `PlayListMpls::chapters() -> Vec<Chapter>` lifts every entry mark
  (`mark_type == 0x01`) onto the title timeline via
  `Σ duration_90k(items before ref) + (mark_time_90k − in_time_90k)`,
  yielding a `Chapter { index, start_pts_90k, ref_play_item_id }` whose
  `start_pts_90k` is directly seekable with `TitleSource::seek_to`. Link
  points (`mark_type == 0x02`) are excluded; out-of-range PlayItem refs
  and marks before their PlayItem's IN point are skipped as malformed
  authoring. A new `MarkType` enum (`EntryMark` / `LinkPoint` /
  `Other(u8)`, with `from_raw` + `is_chapter`) and a
  `PlayListMark::kind()` decode the raw `mark_type` byte. `Disc::chapters(title)`
  reads the title's `.mpls` once and returns the same list (empty on
  read/parse failure, mirroring `max_angle`). New types `Chapter` /
  `MarkType` are re-exported at the crate root. Verified end-to-end: a
  synthetic 2-clip disc with two entry marks (one offset into the second
  PlayItem past its non-zero IN point) plus a link point yields exactly
  two chapters whose PTS round-trip through `seek_to` onto the correct
  keyframe source packets.
- **Multi-angle PlayItem parsing + per-angle title open** (`mpls` +
  `disc`). Per BD-ROM Part 3 §5.4.4.1, a PlayItem with the
  `is_multi_angle` bit set carries an `(N - 1) × 11`-byte block of
  alternate-angle clip references (5-byte clip stem + 4-byte codec id +
  1-byte `stc_id_ref` + 1 reserved per alt entry). The parser used to
  read `multi_clip_count` and then `r.skip(11 * (N-1))` past the alt
  entries — the round trip preserved the count but discarded every alt
  clip's name. This round captures the lot: `PlayItem` grows an
  `angles: Vec<AngleClip>` field (each `AngleClip` carrying
  `{ clip_information_file_name, clip_codec_identifier, stc_id_ref }`),
  the encoder writes those entries back at exactly the same byte
  offsets, and a new `PlayItem::angle_clip(angle: u8) -> Option<AngleClipRef>`
  selector maps a 0-based angle index to the right clip reference
  (angle 0 → the primary clip on the PlayItem itself; angle k ≥ 1 →
  `angles[k - 1]`). A new `PlayItem::num_angles()` returns the unfolded
  count (1 for single-clip items). The new types `AngleClip` /
  `AngleClipRef` are re-exported at the crate root.
- **`Disc::open_title_with_angle(title, angle, decryptor)` —
  angle-selecting title open**, plus `Disc::max_angle(title)` for the
  largest safely-openable angle across every PlayItem (the smallest
  PlayItem-angle-count minus one). `open_title` now delegates to
  `open_title_with_angle(title, 0, decryptor)` — same observable
  behaviour, so existing callers (including `bluray://` URI handler +
  every test) stay byte-for-byte identical. The new entry point rejects
  an out-of-range `angle` *at open time* by checking every PlayItem's
  `angle_clip(angle)` up front: surfacing the mismatch cleanly is
  strictly better than leaving the streamer to discover mid-clip that
  one PlayItem in the middle of the title has fewer angles than the
  caller asked for. Internally `TitleSource::new` now takes the angle
  index and selects each PlayItem's `.m2ts` / `.clpi` stem via
  `angle_clip(angle)`, so the resulting source-packet seek index,
  EP_map lookup, and AACS-unit-aligned seek path all operate on the
  selected angle's bytes.
- **9 new tests** (3 unit + 6 integration in `tests/multi_angle.rs`):
  multi-angle MPLS round trip preserves the two alt-angle clip stems
  with the right `stc_id_ref`s; the `angle_clip` selector maps 0/1/2
  to the correct refs and returns `None` for out-of-range; a single-
  angle PlayItem keeps the empty `angles` vec + `num_angles() == 1`
  invariant; end-to-end mount + open_title_with_angle(0/1/2) streams
  the right `.m2ts` (verified via per-clip 4-byte fingerprints at the
  TS-payload head); `open_title` defaults to the primary angle;
  out-of-range angle is rejected with a diagnostic naming the angle;
  `max_angle` reports 2 for the 3-angle synthetic title. Spec basis:
  BD-ROM Part 3 §5.4.4.1 (is_multi_angle block) + AV §5.2.3.3 (per-
  angle interleaved clip layout on disc). No new spec dependency.
- **`iter_source_packets` — borrowing M2TS source-packet iterator
  + `strip_tp_extra_to_vec` convenience wrapper** (`m2ts`). Adds two
  call shapes complementing the existing in-place
  `strip_tp_extra(input, &mut out)` worker: `strip_tp_extra_to_vec`
  is a one-shot allocation-returning wrapper, and
  `iter_source_packets` yields one
  `M2tsSourcePacket { tp_extra: TpExtraHeader, ts_payload: &[u8; 188] }`
  per 192-byte chunk — the iterator borrows into the input buffer (no
  copy), exposes `ExactSizeIterator`, and lets callers consume the
  27 MHz arrival timestamps + CCI bits without re-decoding the 4-byte
  `TP_extra_header` out of band. The 188-byte TS payload stays opaque
  per the crate's container/codec split — ISO/IEC 13818-1 parsing
  belongs to the downstream MPEG-TS demuxer, not here. Both new
  entry points panic on a non-192-byte-multiple input matching the
  existing helper's contract. Eight new unit tests cover the
  one-packet borrow, multi-packet ordering against deterministic
  arrival-time + payload-tail markers, `ExactSizeIterator::len`
  decreasing on each `next()`, the empty-buffer case, the
  misalignment panic on both new helpers, and a byte-for-byte
  equivalence check between iterator-exposed payloads and the
  linearised `strip_tp_extra_to_vec` output (the two views are
  guaranteed to agree). Grounded in BD-ROM Part 3 §5.6.2.1 source-
  packet layout — no new spec dependency.
- **AACS VUK lookup cascade: KEYDB.cfg → on-disk cache → online
  derivation** (`aacs_adapter::try_resolve_aacs`). On a KEYDB.cfg
  legacy-entry miss for a given disc ID, the resolver now falls
  through to a local cache at
  `${XDG_CACHE_HOME:-${HOME}/.cache}/oxideav/vuk-cache.cfg`
  (override with `OXIDEAV_AACS_VUK_CACHE=<file>`) using the same
  `<40-hex DISCID> = V <32-hex VUK> | <label>` format as KEYDB.cfg —
  so the cache stays human-readable and `cat`-shareable. A second
  miss triggers an **online** VUK derivation (gated by the new
  `aacs-online` cargo feature, default-on): the disc's AACS Volume
  Identifier is read via MMC `READ DISC STRUCTURE` through the
  existing `drive::read_volume_id` backend, and each `| DK |` Device
  Key parsed from KEYDB.cfg's extended format is walked through the
  disc's MKB Subset-Difference tree via
  `oxideav_aacs::AacsVolume::derive_vuk_from_device_key`. The first
  Device Key whose derived Media Key satisfies the MKB
  Verify-Media-Key record wins; revoked DKs are skipped. A
  successful derivation is appended back to the cache with an
  `online-<RFC3339-UTC>` provenance stamp so subsequent mounts skip
  the drive query (idempotent — re-running for the same disc ID
  doesn't duplicate the line). The new `aacs-online` cargo feature
  cleanly disables the drive-query path for headless / CI builds
  that have no optical hardware; the KEYDB.cfg + cache fallbacks
  stay available without it. Six unit tests cover the cache
  round-trip, idempotency, the civil-from-Unix date helper, and the
  `DeviceKeyRecord` → `DeviceKey` adapter.
- **`TitleSource::seek_to(pts_90k)` — keyframe-aligned random access**
  (BD-ROM AV §5.7 + §3.1). Builds a per-clip seek index at
  `open_title` time: each PlayItem's `.clpi` is parsed (best-effort)
  and its primary-video EP_map (lowest stream PID) lifted into an
  ascending `(pts_ep_start, spn_ep_start)` list, alongside the clip's
  running output-byte start + title-relative 90 kHz PTS start.
  `seek_to` maps a title-relative PTS → clip → clip-local PTS (via the
  PlayItem IN-point) → binary-search for the I-frame at or before the
  target → `spn_ep_start × 192` byte offset (AV §3.1: 192-byte source
  packets). Positioning opens the `.m2ts` at the enclosing 6144-byte
  AACS unit boundary then drains the residual packets, so decryption
  stays unit-aligned. Clips with no CPI fall back to the clip start; a
  past-end target lands on the final clip's last entry point. The
  byte-exact `Seek` impl now reuses the same per-clip output-offset
  index to jump straight to the containing clip on a rewind instead of
  rewinding to clip 0. New `tests/seek_to_keyframe.rs` covers both
  paths against synthetic 2-clip titles with hand-built EP_maps.
- **CPI EP_map decode** (BD-ROM AV §5.7 / BD-RE Part 3 §3.1.5.2). The
  `CLIPINF/*.clpi` parser now produces a typed `Cpi { ep_map:
  Vec<EpMap>, ts_type_indicators: Vec<u8> }` instead of the Phase-1
  empty stub. Each `EpMap` carries the stream PID + 4-bit
  `EP_stream_type` + a flat list of `EpEntry` rows; the coarse-table
  context (14-bit `PTS_EP_coarse`, 32-bit `SPN_EP_coarse`) is folded
  into each entry's `pts_ep_start` / `spn_ep_start` so a seeker can
  binary-search directly. PTS combine: `(coarse << 19) | (fine << 9)`
  (low 9 bits truncated, ≈ 5.7 ms granularity); SPN combine:
  `(coarse_spn & 0xFFFE_0000) | fine_spn`. Five new tests cover the
  MSB-first bit reader/writer, an empty CPI block, an unknown-CPI-type
  fallback to empty, and a two-stream multi-coarse round trip.
- **`Cpi` / `EpMap` / `EpEntry` re-exported** at the crate root.
  `CpiEpMap` stays as a deprecated alias for one release.

### Changed

- `ClipInformation.cpi` is now `Cpi` (was `CpiEpMap` — same field
  position, new struct layout). External code that pattern-matches on
  `Cpi.entries` must migrate to walking `cpi.ep_map[i].entries`.

### Added (previous unreleased entries)

- **macOS AACS Volume Identifier reader** — `drive::macos` now drives a
  real MMC `READ DISC STRUCTURE` (opcode `0xAD`, format `0x80`) against
  the optical drive via IOKit's SCSITaskDeviceInterface. The
  `IOKit.framework` + `CoreFoundation.framework` are dlopen'd at runtime
  via [`libloading`] (same pattern as oxideplay's SDL2 loader) — no
  `io-kit-sys`, `core-foundation`, `iokit-rs`, or `mach2` dep. Flow:
  `statfs(disc_root)` → BSD whole-disk name → IORegistry walk against
  `IOBDServices` / `IODVDServices` matching by child-plane `BSD Name` →
  `IOCreatePlugInInterfaceForService` →
  `QueryInterface(kIOSCSITaskDeviceInterfaceID)` →
  `ObtainExclusiveAccess` → `CreateSCSITask` →
  `SetCommandDescriptorBlock` + `SetScatterGatherEntries` +
  `SetTimeoutDuration(5000ms)` → `ExecuteTaskSync`. `kIOReturnExclusiveAccess`
  gives an actionable error message pointing at `diskutil unmount`.
  Clean-room from Apple's public SDK headers (Xcode CommandLineTools);
  no libbluray / libaacs / makemkv / AnyDVD source consulted.

- Bootstrap (Phase 1 — source plug-in): clean-room read-only Blu-ray
  Disc (BD-ROM) support per the BDA whitepapers + ECMA-167 UDF spec.
  - **UDF 2.50 mount** — `udf::UdfDisc` reads the Volume Recognition
    Sequence, Anchor Volume Descriptor Pointer at sector 256, Volume
    Descriptor Sequence (PVD / LVD / PD), File Set Descriptor, and
    walks `FileIdentifierDescriptor` + `FileEntry` ICBs over short
    allocation descriptors. Read-only, sector-cached lazily on demand.
  - **BDMV walk** — `bdmv::index_bdmv` parses `index.bdmv` (header +
    AppInfoBDMV + Indexes); `bdmv::movie_object` parses
    `MovieObject.bdmv`; `bdmv::mpls` parses `PLAYLIST/*.mpls`
    (AppInfoPlayList + PlayList + PlayItems + STN_table summary,
    SubPath count); `bdmv::clpi` parses `CLIPINF/*.clpi` (ClipInfo +
    SequenceInfo + ProgramInfo + CPI EP_map).
  - **M2TS source** — `m2ts::TitleSource` streams a title's
    concatenated `.m2ts` clips, stripping the 4-byte BDAV TP_extra
    header per packet and presenting a clean 188-byte MPEG-TS byte
    stream to the existing demuxer.
  - **Title selection** — `Disc::titles()` enumerates HDMV + BD-J
    titles with duration in 90 kHz ticks; `Disc::longest_title()`
    returns the longest HDMV title for autoplay.
  - **bluray:// URI handler** — `source::open_bluray` handles
    `bluray://` (auto-detect mount points), `bluray:///path/to/disc`,
    and `bluray:///dev/sr0` (raw block device); registered with
    `oxideav_core::SourceRegistry` under the default-on `registry`
    cargo feature via the `register!` macro.
  - **AACS decoupling** — `StreamDecryptor` trait (6144-byte aligned
    unit boundary) so `oxideav-aacs` can plug in without a hard dep;
    no `oxideav-aacs = "..."` in `[dependencies]`.
- Tests (all synthetic, no real disc data): UDF descriptor round-trips,
  BDMV parser round-trips against hand-crafted byte sequences,
  TP_extra header stripping, synthetic BDMV tree mount, `bluray://`
  URI parser, mount auto-detect against a `/tmp/` synthetic BDMV tree.
