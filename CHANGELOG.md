# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
