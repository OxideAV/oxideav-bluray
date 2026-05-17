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
  Sequence, File Set Descriptor, File Entry / ICB short-allocation
  walks).
- BDMV parsers — `index.bdmv` titles, `MovieObject.bdmv` nav-command
  enumeration, `PLAYLIST/*.mpls` PlayList + PlayItem + STN_table
  summary + ClipMark, `CLIPINF/*.clpi` ClipInfo + SequenceInfo +
  ProgramInfo + CPI placeholder.
- `.m2ts` stream → strip the 4-byte BDAV `TP_extra_header` per
  192-byte source packet, deliver clean 188-byte MPEG-TS bytes.
- `Disc::longest_title()` heuristic for autoplay (longest HDMV
  title).
- `bluray://` URI handler registered with `oxideav_core::SourceRegistry`
  under the default-on `registry` cargo feature.
- Pluggable [`StreamDecryptor`] trait so `oxideav-aacs` can plug in
  without a hard dep.

## Deferred

- HDMV interactive layer (`MovieObject.bdmv` opcode execution).
- BD-J (Java ME).
- SubPath PiP / secondary-video streams.
- Raw-block-device mount (`bluray:///dev/sr0`) — UDF mounter exists,
  high-level routing in `Disc::mount_image` is Phase 2.
- CPI EP_map full decode (the entries Vec stays empty in Phase 1;
  seek hints will land in Phase 2).
- ICB strategy types other than 4, ExtendedFileEntry, long/extended
  allocation descriptors, multi-extent partition maps.

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

- UDF descriptor round-trips (tag checksum, lb_addr, short_ad, d-string).
- BDMV parser round-trips (`index.bdmv`, `MovieObject.bdmv`,
  `.mpls`, `.clpi`).
- `TP_extra_header` strip (1 packet, 17 packets, alignment panics).
- AACS-unit-length sanity check (32 × 192 = 6144 = 3 × 2048).
- `bluray://` URI parser (auto-detect, absolute path, scheme reject).
- Mount auto-detect with a synthetic `/tmp/` BDMV tree.
- End-to-end mount + stream of a hand-rolled BDMV directory (`tests/synthetic_disc.rs`).
- End-to-end mount of a synthesised in-memory UDF image (`tests/udf_minimal_image.rs`).

## Clean-room references

Only these documents are consulted; no `libbluray` / `libudf` /
`udfclient` / VLC bluray-module / makemkv / AnyDVD source has been
read.

- `docs/container/bluray/BD-ROM_Part3_V3.2_WhitePaper_180122.pdf`
- `docs/container/bluray/BD-ROM_Audio_Visual_Application_Format_Specifications.pdf`
- `docs/container/bluray/BD-ROM-AV-WhitePaper_HEVC.pdf`
- `docs/container/bluray/ECMA-167_3rd_edition_june_1997.pdf`

## Privacy

Auto-detect probes directory existence only. The crate never reads
volume labels, disc IDs, or any other identifying field into a
public-facing struct, and contains zero references to specific
commercial titles, studios, or hashes in code, tests, or docs.

## License

MIT. Copyright (c) 2026 Karpelès Lab Inc.
