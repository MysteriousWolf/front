# Radar: direct source-grid resampling

## Goal

Render radar by bilinearly sampling the in-RAM 1 km source grid directly per
screen sub-cell, and delete the Web-Mercator tile pipeline. Radar becomes smooth
at every zoom, never rebuilds or blanks on pan/zoom, and costs less RAM/CPU.

## Non-goals

- No change to the S3 download path or the `.frd` on-disk format.
- No on-disk *resampled* cache — rebuild live; RAM `.frd`-byte cache only.
- No change to borders, observations, warnings, or timeline/playback UX.
- No new tunable render resolution — screen resolution is the sample rate.

## Success criteria

- [ ] Zoomed-in radar shows smooth gradients, not flat rectangular band blocks.
- [ ] Pan and zoom never rebuild or blank radar — only the sampled lat/lons change; the field stays put until a *different timeline frame* is selected.
- [ ] Radar RAM (help-modal readout) at a full playback window is ≤ the tile-based figure it replaces.
- [ ] Radar render stays within a per-frame time budget (a `raster_radar` bench asserts it) across braille/color/text modes.
- [ ] `build_tile`/tile streaming/preload/`frame_cache`/`RadarFrame`/`RadarTile`/`RadarRun`/`covers_bounds`/`trim_to_bounds` are gone; `cargo build`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test` are clean.

## Approach

Direct source→screen bilinear resample; the source grid is the base, the raster
row-bands do the parallel sampling — see `docs/design/radar-direct-resample.md`.

## Checkpoints

| # | Checkpoint | Files/areas | Agent | Est. files | Verifies |
|---|------------|-------------|-------|------------|----------|
| 1 | Expose a samplable radar field: a public handle wrapping `Arc<RadarGrid>` with a `sample(lat,lon) -> Option<(Rgb8,u8)>` (bilinear + `dbz_to_color`); a provider entry that fetches/decodes/caches a frame's grid. | `providers/meteogate.rs` | atomic-implementer (feature) | ~1 | Unit test: field sample returns interpolated band colours on a synthetic grid; on-cell reproduces the cell, mid-cell interpolates. |
| 2 | Render from the field: rewire `raster_radar` to sample the current field per sub-cell (screen→world→lat/lon→sample) inside the existing parallel row-bands, for braille/color/text; `App` holds `current_field` instead of a tile frame. | `ui.rs`, `app.rs` | atomic-implementer (feature) | ~3 | Reference test rebuilt from a synthetic field passes for all three modes; radar draws from the grid (no tile lookups remain in the raster). |
| 3 | Frame loading + playback on grids: cache `.frd` bytes in RAM for the playback window + LRU of decoded grids; `request_*` ensures current + window grids are present; timeline stepping swaps `current_field` with no refetch. | `app.rs`, `providers/meteogate.rs` | atomic-implementer (feature) | ~2 | Cache/LRU unit test; stepping frames switches the field without re-decoding an already-warm frame. |
| 4 | Delete the tile pipeline: remove tile build/stream/spiral, preload, `frame_cache`, `RadarFrame`/`RadarTile`/`RadarRun`, `covers_bounds`/`trim_to_bounds`, and dead drain/refresh branches. | `providers/meteogate.rs`, `app.rs`, `layers.rs`, `ui.rs`, `geo.rs` | atomic-implementer (feature) | ~5 | `cargo build` + `clippy -D warnings` clean (no dead code); full `cargo test` green. |

## Risks

| Risk | Likelihood | Mitigation |
|------|-----------|-----------|
| Per-sub-cell double projection (mercator⁻¹ + LAEA) slows render. | med | Parallel row-bands (existing); precompute per-row/col mappings if needed; the bench in the success criteria gates it. |
| Playback RAM regresses if grids (16.7 MB) are cached instead of `.frd` bytes. | med | Cache compact `.frd` bytes for the window; decode to grids on demand behind a small LRU. |
| Reference/bench tests assume tile inputs. | high | CP-2/CP-4 rewrite the fixtures to build a synthetic grid/field. |
| Progressive-fill lost (tiles streamed centre-first). | low | Whole-grid decode is ~3 ms from disk; the frame appears at once rather than filling in — acceptable and simpler. |

## Change log

<!-- Empty on creation. First entry logs the first amendment after approval. -->

## Implementation log

### shipped (uncommitted working tree) — 2026-07-23

Built across 4 checkpoints via /subagent-implementation. No commits — repo rule
forbids agent-initiated git; the whole change lands as one working-tree diff for
manual review. Checkpoints (each reviewer-PASSed; orchestrator re-verified
build/test/clippy/`--no-default-features`):

- CP-1 — `RadarField` + `sample(lat,lon)->Option<(Rgb8,u8)>` (bilinear + band
  colour) + `MeteoGateProvider::field(ts)`, additive. `providers/meteogate.rs`.
- CP-2 — `raster_radar` samples a `Fn(lat,lon)->Option<(Rgb8,u8)>` closure per
  sub-cell (parallel row-bands kept); `App.current_field`. `ui.rs`, `app.rs`.
- CP-3 — warm cache: RAM `.frd` bytes window-wide + bounded decoded-grid LRU
  (`FrdCache`, `warm_field`/`field_warm`/`prune_field_window`), field preload
  over the playback window, stepping serves warm with no refetch; F-1 retry fix.
  `providers/meteogate.rs`, `app.rs`, `ui.rs`.
- CP-4 — deleted the radar tile pipeline (build/stream/spiral, preload,
  `frame_cache`, `RadarFrame`/`RadarTile`/`RadarRun`, `covers_bounds`/
  `trim_to_bounds`, `radar_zoom`/`RADAR_TILE_ZOOM`, tile retry maps). Split
  `drain_refresh_results`→`drain_border_results`; repointed
  `frame_label`/`slot_state`/`timeline_line` to `timestamps`/`field_is_warm`;
  rewired Radar `LayerStatus` into the field path. All 5 core files + `geo.rs`.

Final state: `cargo fmt --check`, `cargo test` (405 pass), `cargo clippy
--all-targets --all-features -- -D warnings` (zero warnings), `cargo check
--no-default-features` all clean.

**Out-of-scope work performed:** `geo.rs` tile helpers (`TileCoord`,
`visible_tiles`, `tile_bounds`, `tiles_spiral_from`, `tile_for_world`,
`TILE_SIZE`, world/tile-pixel converters) removed — proved radar-only once the
pipeline went (borders never used them). `disk_frames` was on the CP-4 DELETE
list but kept: repurposed as the on-disk `.frd` presence check feeding the
timeline warm/disk indicator, not tile-cache dead code.

**Unforeseens:**
- CP-3 reviewer caught a stale-result race — `FieldRefreshResult` carried no ts,
  so a fetch for frame A could clobber `current_field` after a warm step to B.
  Fixed in CP-3 by threading the result's own ts and gating on `field_result_wanted`.
- CP-4's raw source grid is 1 km (3800×4400 @ 1000 m); z=7 tiles were ~786 m, so
  the pre-refactor blockiness was the source cells sampled nearest-neighbour, not
  a tile-zoom cap — direct bilinear sampling is what actually resolves it.

**Deferred items still open:** none. F-1 (field-retry wedge) closed in CP-3;
`RadarBatch` dead struct removed in finalize.
