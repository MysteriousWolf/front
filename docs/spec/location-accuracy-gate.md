# Location accuracy gate — spec

## Goal

The "you are here" marker and its place-name label appear only when the current
fix is accurate to better than 5 km. A coarse fix still centres the viewport on
boot; it just does not draw a dot claiming a precision the fix does not have.

## Non-goals

- Changing `LocationArbiter`'s ranking rules. The gate is a *display* predicate
  applied to whatever fix the arbiter has already picked.
- Changing which backends run, or their order.
- Gating the initial viewport jump. A 10 km fix is still far better than the
  Europe fallback for deciding where to start.
- A user-facing toggle to override the threshold. Revisit if asked.

## Motivating measurement

`/usr/lib/geoclue-2.0/demos/where-am-i` on the dev box, 2026-07-20:

| Stage | Accuracy | Arrives |
|---|---|---|
| `GeoIP (ichnaea)` | 10 000 m | immediately |
| `WiFi` | 25 m | ~5 s later, then refreshes |

So the gate is not theoretical: GeoClue really does deliver a coarse fix first
and refine it. A 5 km threshold hides the GeoIP stage and reveals the marker
when WiFi lands. The IP fallback's hardcoded 25 km (`ip.rs:21`) never passes,
which is the intended outcome — a city-level guess should not draw a dot.

## Success criteria

- [ ] A fix with `accuracy_m = Some(x)`, `x < 5000.0`, renders the marker.
- [ ] A fix with `accuracy_m = Some(x)`, `x >= 5000.0`, renders no marker and no
      label.
- [ ] A fix with `accuracy_m = None` renders no marker and no label.
- [ ] The first fix still moves the viewport regardless of accuracy; a test
      drives a 10 km first fix and asserts the viewport moved and the marker did
      not draw.
- [ ] `--lat/--lon` (a `Manual` fix) always renders. It is a user assertion, not
      a measurement, and `LocationFix::new` gives it `accuracy_m: None` — the
      one case where unknown must not mean hidden.
- [ ] Toggling the `Location` layer off still hides the marker; the gate is an
      additional condition, never a replacement for the layer flag.

## Approach

One predicate on `LocationFix`, consulted at the render sites.

```rust
/// Largest horizontal error at which the marker still means something.
/// Above this the dot would imply a precision the fix does not have.
pub const DISPLAY_ACCURACY_M: f64 = 5_000.0;

impl LocationFix {
    /// Whether this fix is precise enough to draw. `None` accuracy fails —
    /// consistent with the arbiter treating unknown as worse than any known
    /// value — except for `Manual`, which is asserted, not measured.
    pub fn is_displayable(&self) -> bool { … }
}
```

`Manual` is the one carve-out. `--lat/--lon` produces `accuracy_m: None`
(`LocationFix::new`), so a naive "None means hide" rule would break the explicit
CLI flag — the user typed the coordinates.

## Checkpoints

| # | Checkpoint | Files/areas | Agent | Est. files | Verifies |
|---|------------|-------------|-------|------------|----------|
| 1 | Add `DISPLAY_ACCURACY_M` and `LocationFix::is_displayable()`, with the `Manual` carve-out | `src/providers/location/mod.rs` | atomic-implementer (mode: surgical) | 1 | Unit tests over the threshold boundary, `None`, and `Manual` |
| 2 | Gate the marker and label render on the predicate | `src/ui.rs`, `src/app.rs` | atomic-implementer (mode: surgical) | 2 | Test: a 10 km fix draws no pin; a 25 m fix does |
| 3 | Confirm the viewport jump is unaffected | `src/app.rs` | atomic-implementer (mode: surgical) | 1 | Test: a 10 km first fix still moves the viewport |

## Risks

| Risk | Likelihood | Mitigation |
|------|-----------|-----------|
| Marker flickers as accuracy oscillates across the threshold — the arbiter accepts a same-source refresh even when it is *less* accurate, so WiFi 25 m → GeoIP 10 km would make the dot vanish | medium | Measure first. If flicker is observed in practice, add hysteresis (require two consecutive failing fixes before hiding) rather than pre-building it |
| A user on a machine with only the IP fallback never sees a marker and reads it as a bug | high | The `Location` layer status line already carries the fix summary (`GeoClue ±12 m`). Extend it to say why the marker is suppressed rather than showing nothing |
| `Manual` carve-out forgotten, breaking `--lat/--lon` | medium | Explicit success criterion and a dedicated test |

## Change log

- **2026-07-20 — all three checkpoints landed** (uncommitted).
  `DISPLAY_ACCURACY_M = 5_000.0` and `LocationFix::is_displayable()` in
  `location/mod.rs`; the render site now goes through `raster_location_marker`
  (`ui.rs:1776`), which gates marker and label together because both are drawn
  by the same `raster_pin` call.

  `viewport_for_fix` (`app.rs:2796`) was extracted *without* consulting the
  gate — verified: a coarse first fix still moves the viewport. Extracting it
  was necessary because `initial_viewport` is a private async fn coupled to
  real backend spawning, so there was no seam to test the "viewport moves,
  marker does not draw" property against.

  All three seams were falsified before being accepted: flipping `<` to `<=`
  fails the threshold test; removing the render guard fails the coarse-fix
  tests; making `viewport_for_fix` gate on accuracy fails the viewport test.
  Removing the `Manual` carve-out fails both manual tests — re-checked
  independently, since that was the requirement most likely to be silently
  dropped.
