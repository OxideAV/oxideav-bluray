# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
