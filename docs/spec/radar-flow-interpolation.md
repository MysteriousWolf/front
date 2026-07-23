# Radar frame interpolation (optical-flow smoothing)

## Goal

During `Playing` animation, render continuous constant-velocity motion between
consecutive radar frames using per-pair optical flow, while displaying the real
composites at full 1 km resolution. On by default; flow cost tunable via a
settings choice.

## Non-goals

- No extrapolation / prediction — interpolation strictly between two existing
  frames.
- No morphing across the ring seam (`frame_index 0 → len-1` wrap stays a hard
  cut).
- No smoothing of manual `[`/`]` stepping or the live (non-playing) view.
- No full-resolution flow field; no GPU; no new external dependency.
- No settings keybind — the modal toggle is the only control surface.

## Success criteria

- [ ] A pure-Rust pyramidal Horn–Schunck estimator recovers a known synthetic
      translation (within tolerance) and returns a near-zero field for a
      static input.
- [ ] Interpolated sampling at `t=0` reproduces the `src` frame and at `t=1`
      reproduces the `tgt` frame, cell for cell, versus direct
      `RadarField::sample`.
- [ ] A synthetic translating echo appears at the flow-interpolated position at
      `0 < t < 1` (advected), not merely cross-faded in place.
- [ ] Interpolated display samples the full-resolution grids — no visible
      resolution loss on the ground-truth frames relative to non-smooth
      playback.
- [ ] Flow is computed once per frame-pair (not per rendered frame) and reused
      across all sub-frames of that pair.
- [ ] Flow cache is bounded to a small number of pairs and evicts the furthest
      from the playhead; no flow is ever computed for the `0 → len-1` seam pair.
- [ ] Phase advances off wall-clock (`t = elapsed / pair_duration`) at constant
      velocity; crossing a frame boundary shows no positional jump.
- [ ] The ring seam and any manual step are hard cuts (no morph).
- [ ] `config.toml` gains a `[playback]` section: `smoothing` bool (default
      `true`) and a 3-way flow-resolution choice (default Medium); both
      round-trip through load/save.
- [ ] The settings modal exposes both: an on/off toggle and a cycling
      resolution choice; committing either persists via `apply_config_edits`
      and takes effect live.
- [ ] With smoothing off, playback behaves exactly as before (plain
      `current_field` swap).
- [ ] `cargo test`, `cargo clippy --all-targets --all-features -- -D warnings`,
      and `cargo build --no-default-features` all pass.

## Approach

Pyramidal Horn–Schunck flow + backward-warp-both-and-blend sampler, coarse
per-pair flow cached in a bounded LRU and warmed a pair ahead via the existing
preload hook — see `docs/design/radar-flow-interpolation.md`.

## Checkpoints

| # | Checkpoint | Files/areas | Agent | Est. files | Verifies |
|---|------------|-------------|-------|------------|----------|
| 1 | Pyramidal Horn–Schunck estimator: input two coarse scalar grids (`Vec<f32>` + dims), output a `FlowField` (coarse vectors + dims + scale). No radar coupling. | new `src/flow.rs`, `main.rs`/module decl | atomic-implementer (feature) | ~2 | Unit tests: synthetic translation recovered within tolerance; static input → ~zero field; convergence bounded |
| 2 | Interpolated sampler: coarse-dBZ extraction from `RadarGrid` at a downsample factor; `FlowField` bilinear lookup in grid coords; `InterpolatedField {src, tgt, flow, t}` with `sample(lat,lon)` = backward-warp both + linear-Z blend. | `src/providers/meteogate.rs` | atomic-implementer (feature) | ~1 | Tests: `t=0`≡src.sample, `t=1`≡tgt.sample; translating blob lands at interpolated position; nodata → None |
| 3 | `[playback]` config (smoothing bool default on; flow-resolution enum default Medium) + settings `FieldKind::Choice` (cycling) + two new `SettingsModel` fields + `apply_config_edits`/app apply route. | `src/config.rs`, `src/settings.rs`, `src/app.rs` | atomic-implementer (feature) | ~3 | Tests: config round-trips defaults; choice cycles + yields correct `ConfigEdit`; bool toggle persists; live apply updates state |
| 4 | Per-pair flow cache + JIT compute + warm-ahead: bounded LRU keyed by `(older_ts, newer_ts)`; compute via `flow` + coarse extraction at the configured resolution; warm one pair ahead through the `trigger_field_preload` path; never compute the seam pair. | `src/app.rs` | atomic-implementer (feature) | ~1 | Tests: LRU bound + furthest-from-playhead eviction; correct pair keying; seam pair never keyed; resolution setting honored |
| 5 | Playback timing + render integration: wall-clock phase `t` constant per pair, advance pair at `t≥1`, seam + manual = hard cut, only when `Playing` + smoothing on; mark loop dirty each tick; `raster_radar` selects interpolated vs plain sampler. | `src/ui.rs`, `src/app.rs` | atomic-implementer (feature) | ~2 | Tests: phase progression + pair advance; seam/manual hard cut; smoothing-off = old path; dirty-each-tick while animating |

## Risks

| Risk | Likelihood | Mitigation |
|------|-----------|-----------|
| Coarse flow smears fast cells embedded in slow stratiform | med | Resolution is a one-constant knob (Medium→Fine); tune against real weather post-merge, no arch change |
| Per-pair JIT compute stutters on first reach of a pair | med | Warm one pair ahead via existing preload hook; Medium flow is a few ms cache-resident |
| Horn–Schunck fails brightness-constancy on convective growth | med | Temporal Z cross-fade in the sampler absorbs in-place intensity change (design's core mechanism) |
| Holding both decoded grids of a pair inflates the grid LRU | low | Flow estimation forces both decodes, which then serve rendering; ensure grid LRU ≥ 2 resident |
| New `[playback]` keys break old `config.toml` | low | `#[serde(default)]` on the section and fields; on-disk absence → coded defaults |
| Dirty-each-tick raises idle CPU | low | Gate continuous redraw on `Playing && smoothing`; 33 ms `MIN_FRAME_INTERVAL` caps at 30 fps |

## Change log

<!-- No amendments — the body as written was implemented without a decision change. -->

## Implementation log

### shipped (uncommitted) — 2026-07-23

Built across 6 iterations of /subagent-implementation (implement→review, fresh-context subagents each pass). Per repo policy (`CLAUDE.md`: no agent git operations), **nothing was committed** — all changes are uncommitted in the working tree on `main` for manual owner review + commit. No worktree, no per-iteration commits.

Checkpoints (each reviewer-PASSed):

- CP1 — `src/flow.rs`: pure pyramidal Horn–Schunck optical-flow estimator (`FlowField`, `estimate_flow`, `vector_at`); wired via `pub mod flow;` in `src/lib.rs`.
- CP2 — `src/providers/meteogate.rs`: `InterpolatedField` (backward-warp both frames + linear-Z blend), `coarse_dbz` extraction, `sample_z_at` linear-Z refactor, `same_geometry` guard.
- CP3 — `src/config.rs` / `src/settings.rs` / `src/app.rs` (+1-line `src/ui.rs`): `[playback]` config (`smoothing` bool default on, `flow_resolution` Coarse/Medium/Fine default Medium, `downsample_factor` 8/4/2), cycling `FieldKind::Choice`, apply route.
- CP4 — `src/app.rs` (+`meteogate.rs` split): `FlowCache` (bounded, furthest-from-playhead eviction), seam-aware `active_flow_pair`, JIT compute + warm-ahead via `trigger_field_preload`; `compute_flow`/`with_flow` split so flow is computed once per pair.
- CP5 — `src/ui.rs` / `src/app.rs`: wall-clock phase clock (`playback_phase`, `morph_phase`), `interpolated_field_for_render` (field_warm-derived), sampler selection, 30 fps dirty-each-tick, seam + manual hard cuts, smoothing-off regression guard.
- Polish — F-4 fix + nits: `normalize_coarse_dbz` clamps normalized coarse dBZ to `[0,1]` (bounds intense cells that previously reached ~1.9); flow.rs `pub`→`pub(crate)`; test/comment tidy-ups.

Final state: 440 tests pass (all-features + default), `cargo clippy --all-targets --all-features -D warnings` clean, `cargo build --no-default-features` clean. Diff: ~1164 insertions across 6 tracked files + new `src/flow.rs`.

**Out-of-scope work performed during this build:**
- CP2 introduced `FLOW_DBZ_NORM` normalization (not in the original plan) — raw dBZ amplitude (0–70) diverged against `flow.rs`'s fixed `ALPHA_SQUARED` (tuned for ~[0,1]); caught by a failing translating-blob test. Later bounded by the F-4 clamp.
- CP4 required a `meteogate.rs` `new`→`compute_flow`+`with_flow` split (enabling refactor) so flow could be cached per-pair rather than recomputed per render frame.

**Unforeseens — surprises that emerged during implementation:**
- The crate has a `src/lib.rs` target (not only `main.rs`); the flow module is declared there.
- `App` has no lightweight test constructor and `RadarGrid` is private, so the config→`compute_flow` resolution wiring and the warm-grid tail of `interpolated_field_for_render` are inspection-verified, not unit-tested. Predicate gates (smoothing-off / paused / seam) were extracted into pure functions specifically so they *are* unit-tested.

**Deferred items still open:**
- None. F-1 mitigated in CP2. F-4 fixed in the polish pass. F-2/F-3/F-5/F-6 fixed in the polish pass. No tracked issues outstanding.
- Standing tuning note (recorded in `## Risks` row 1): flow resolution and the `FLOW_DBZ_NORM`/`ALPHA_SQUARED` amplitude coupling should be eyeballed against real OPERA radar once running; the clamp bounds the failure mode but the Medium-default and constant are provisional until validated on live weather.
