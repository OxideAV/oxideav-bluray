# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
