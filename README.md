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
  ProgramInfo + CPI EP_map (per-stream-PID entry-point map — coarse +
  fine rows decoded into a flat `(pts_ep_start, spn_ep_start)` list
  ready for I-frame-aligned seek).
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
- **In-place** mid-stream angle switching (`open_title_with_angle`
  fixes the angle at open time; angle-change-point *boundaries* are
  now enumerated via `angle_change_points()` / `next_angle_change_point()`
  so a UI can perform a switch as a stop-current-source +
  `open_title_with_angle(new) + seek_to(boundary_pts)` pair — what's
  still deferred is letting `TitleSource` swap its underlying clip
  reader at the boundary without the open/seek round-trip). Cross-PlayItem
  STC PTS continuity is now exposed as a `pts_continuity_segments`
  map (see Scope above) — the demuxer-side PES reproject is fully
  driven by it; what's still deferred is having `TitleSource::read()`
  *itself* rewrite outgoing PES PTS so a remuxer doesn't even need
  the map.
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
