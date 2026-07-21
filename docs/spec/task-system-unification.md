# Task system unification — spec

Design: `docs/design/task-system-unification.md`

## Goal

Every background activity reports through the task system so the top-right
overlay reflects all of it, and the three hand-rolled exponential-backoff
implementations become one shared policy.

## Non-goals

- A general scheduler abstraction. Scope decision recorded in the design: build
  only the modes already in use at real call sites.
- Folding cache TTLs (`VIEWPORT_DATA_TTL`, `CAPITAL_DATA_TTL`,
  `STATION_LIST_TTL`, …) into scheduling. A cache lifetime is not a schedule.
- Long-lived stream supervision (lightning WS, GeoClue D-Bus). Different problem
  — restart semantics, not retry-a-one-shot.
- Replacing `refresh_id`-based cancellation. It works; a scheduler that owned
  task handles would mean rewriting it for no user-visible gain.
- Changing the overlay's visual design beyond what capacity forces.

## Success criteria

- [ ] No `tokio::spawn` in `src/` performs user-visible work without a
      corresponding `TaskMsg::Start` and a terminal `Complete`/`Error`.
      Verifiable by enumerating spawns and their task ids.
- [ ] Exponential backoff is computed in exactly one place; `grep` finds no
      second `* 2).min(` retry-delay expression.
- [ ] `ip.rs`, `meteogate.rs` and `app.rs`'s `FrameRetry` all reach their
      existing delay sequences through the shared policy — each keeps its own
      constants, and existing retry-timing tests pass unmodified.
- [ ] A task that completes faster than the visibility threshold never renders a
      row.
- [ ] The overlay never truncates a running task silently: with more active
      kinds than `max_visible`, the drop policy is deliberate and tested.

## Approach

Two independent strands. Strand A (reporting) delivers the visible benefit;
strand B (backoff) pays down the duplication. They touch different code and can
land in either order.

Backoff becomes a **value type**, not a runtime — callers keep their own loops:

```rust
/// How a fallible operation spaces its retries.
pub struct RetryPolicy { base: Duration, ceiling: Duration, give_up_after: Option<u32> }

impl RetryPolicy {
    /// Delay before attempt `n` (0-based), doubling from `base`, clamped to `ceiling`.
    pub fn delay_for(&self, attempt: u32) -> Duration { … }
    pub fn exhausted(&self, attempt: u32) -> bool { … }
}
```

Deriving the delay from an attempt *number* rather than mutating a running
`backoff` variable is what makes it testable: the sequence for a given policy is
a pure function, so each existing call site's current sequence can be asserted
directly against its replacement.

## Design decisions this spec depends on

All three previously-open questions are resolved in
`docs/design/task-system-unification.md`. In summary:

- **Kinds are grouped**: four location backends → one `Location`; search +
  reverse-geocode → one `Geocode`; radar preload reuses `RadarFrame`. Nine kinds
  total against `max_visible = 8`, so overflow drops by an explicit sort —
  running before terminal, user-initiated before background, oldest first. The
  user-initiated tier discriminates on the `Geocode`/`Location` kinds, which are
  introduced in checkpoint 6; that tier therefore lands with them (checkpoint 5
  implements running-before-terminal and oldest-first, both testable with the
  existing kinds).
- **150 ms visibility threshold**, with `Error` exempt.
- **`fraction` becomes `Option<f64>`**; `None` renders as a marquee. No faked
  values.

## Checkpoints

| # | Checkpoint | Files/areas | Agent | Est. files | Verifies |
|---|------------|-------------|-------|------------|----------|
| 1 | Add `RetryPolicy` with `delay_for` / `exhausted` as a pure value type | new small module, or `src/app.rs` | atomic-implementer (mode: surgical) | 1 | Unit tests: doubling, ceiling clamp, give-up boundary |
| 2 | Adopt it at all three backoff sites, preserving each one's existing delay sequence | `src/providers/location/ip.rs`, `src/providers/meteogate.rs`, `src/app.rs` | atomic-implementer (mode: feature) | 3 | Per-site test asserting the delay sequence is byte-identical to today's constants; existing retry tests pass unmodified |
| 3 | `fraction` → `Option<f64>` end to end; marquee render for `None` | `src/app.rs`, `src/ui.rs` | atomic-implementer (mode: feature) | 2 | Test: a `None` task renders a marquee, not a filled bar; existing determinate bars unchanged |
| 4 | 150 ms visibility threshold, `Error` exempt | `src/app.rs`, `src/ui.rs` | atomic-implementer (mode: surgical) | 2 | Test: a task completing in 50 ms never renders; a 50 ms *error* does |
| 5 | Deterministic overflow sort before truncation: running before terminal, then oldest first | `src/ui.rs` | atomic-implementer (mode: surgical) | 1 | Test: with more active kinds than `max_visible`, a completed row drops before a running one, and ties break by start order |
| 6 | Add the grouped kinds (`Geocode`, one `Location`) and their user-initiated sort tier, and instrument the silent sources: geocode search, location label, location backends, lightning, radar preload | `src/app.rs`, `src/ui.rs`, `src/providers/location/mod.rs` | atomic-implementer (mode: feature) | 3-4 | Test: each newly-instrumented spawn emits Start and a terminal message; a user-initiated kind sorts ahead of a background one at equal running-state; enumerating spawns shows none unreported |

**Order matters.** 1-2 are independent of the rest and can land any time. 3, 4
and 5 all reshape the overlay contract and must precede 6 — instrumenting nine
sources first, then changing `fraction`'s type under them, means touching every
new call site twice.

## Risks

| Risk | Likelihood | Mitigation |
|------|-----------|-----------|
| Adopting a shared policy silently changes one site's retry timing, altering behaviour against a live API under failure — the hardest kind of regression to notice | medium | Checkpoint 2 asserts each site's *existing* sequence explicitly before switching, so a change of timing fails a test rather than shipping |
| Instrumenting the four racing location backends produces four overlay rows for one user-visible activity | high | Design's preferred answer is grouping them under one row; settle in checkpoint 3 before writing checkpoint 4 |
| Fast tasks flash a row and vanish, making the overlay feel noisy — a net UX regression from a feature meant to improve feedback | high | Visibility threshold is a checkpoint-3 decision and a success criterion, not an afterthought |
| `TaskMsg::Progress` requires a `fraction`, but a geocode has no meaningful one | high | Checkpoint 3 decides whether an indeterminate variant is needed; do not fake `0.5` |

## Change log

- **2026-07-21 — checkpoint 1 landed** (uncommitted). `RetryPolicy` added as a
  pure value type in the new `src/retry.rs`, wired via `pub mod retry;` in
  `lib.rs`. `delay_for(attempt)` is `base * 2^attempt` saturating at `ceiling`;
  `exhausted(attempt)` honours an optional give-up count. 0-based attempt
  indexing, documented in the module.

  Overflow-safe by construction: multiplies in `u128` nanos with `checked_mul`
  and caps the shift at 127, so `delay_for(u32::MAX)` returns `ceiling` rather
  than panicking — which matters because the `ip.rs` site it will replace
  retries forever with an unbounded attempt count. The old `1u32 << attempts`
  form panics at attempt 32.

  Orchestrator cross-checked the reproduced sequences against the *real* site
  formulas (not just the agent's hand table): `RetryPolicy(2s, 90s)` matches
  `app.rs`'s `BASE.saturating_mul(1<<n.min(6)).min(90)` for attempts 0-9, and
  `RetryPolicy(60s, 1800s)` matches `ip.rs`'s mutating `(backoff*2).min(MAX)`.
  The app.rs shift-cap (6) and RetryPolicy's (127) differ but converge because
  the ceiling dominates before either cap bites.

  **Carried to CP-2:** meteogate's download loop is NOT a clean swap. At
  `meteogate.rs:635` it overrides `delay` from a server `Retry-After` header
  (`delay = after.min(MAX_RETRY_AFTER)`) — a server-driven value a pure
  `delay_for` cannot produce. CP-2's meteogate adoption must preserve that
  path: use `RetryPolicy` for the *default* doubling but let a present
  `Retry-After` still win. Do not delete the header handling.

- **2026-07-21 — checkpoint 2 landed** (uncommitted). `RetryPolicy` adopted at
  all three backoff sites, each preserving its exact current sequence. Per-site
  attempt conventions differ and are honoured: `ip.rs` 0-based
  (`delay_for(failures)`), `app.rs` pre-incremented 1-based
  (`delay_for(entry.attempts)`, first failure = 4s), `meteogate.rs` 0-based. No
  `*2).min(` or `<<`-shift retry arithmetic survives outside `retry.rs` (grep
  verified). Each site carries a test asserting its sequence is unchanged.

  **Correction applied during review.** The first cut of the meteogate site
  changed behaviour: it consumed a `Retry-After` override for only the sleep it
  preceded, then snapped back to the base-derived sequence. The original
  semantics carry the override forward — the doubling continues *from* the
  server value. Divergent case (verified): a 30s `Retry-After` on attempt 1
  followed by a header-less failure on attempt 2 gave 800ms under the first cut
  versus 60s originally — 75× less deferential to a server that asked to back
  off. Fixed by adding `RetryPolicy::double(current)` (an overflow-safe
  saturating double for externally-redirected baselines) and seeding
  meteogate's `delay` from it, exactly reproducing the original mutating loop
  while keeping the arithmetic in `retry.rs`. Regression test
  `retry_after_redirects_the_doubling_baseline` pins the 60s case.

  **Carried to CP-6:** the meteogate `Retry-After` path is now correct and must
  not regress when that site is later instrumented for the task overlay.

- **2026-07-21 — checkpoint 3 landed** (uncommitted). `TaskMsg::Progress.fraction`
  and `ActiveTask.fraction` are now `Option<f64>`; `None` renders as a bouncing
  `braille_marquee` with `···` in place of a percentage — no faked value. The
  four determinate senders wrap in `Some`; `Start` stays `Some(0.0)` and
  `Complete` always sets `Some(1.0)`, so all six existing kinds render
  byte-identically (the `Some(_)` render arm is the original
  `braille_bar(display_fraction)` + percent expression verbatim; pinned by
  `braille_bar_determinate_output_is_unchanged`).

  Progress/Complete transitions were extracted to `ActiveTask::apply_progress`
  / `apply_complete` (no cheap `App` test seam exists), handling every `Option`
  transition: `Some→Some` keeps today's diff-check, `None→Some` resets the
  animation, `*→None` sets indeterminate without a reset (the marquee runs off
  wall-clock). The smoothstep loop `continue`s past `None` tasks.

  Redraw for a marquee needs no new `changed` flag: the `animation_interval`
  trigger (`ui.rs:295`, `!active_tasks.is_empty()`) already redraws every 50ms
  while any task is active, independent of state. A `None` task is always
  `Running` until terminal, so it can never become unprunable. Marquee is
  width-safe by construction (`pos ≤ width - block`).

  Scope held: no source sends `None` yet — the marquee path is exercised only by
  tests. Which sources go indeterminate is CP-6.

### 2026-07-21 — CP-5 scope narrowed, user-initiated tier relocated to CP-6

**What changed:** Checkpoint 5 now covers only the running-before-terminal and
oldest-first sort keys. The third key — user-initiated before background — moved
to checkpoint 6.

**Why:** that key discriminates on the `Geocode` and `Location` kinds, which do
not exist until checkpoint 6 introduces them. Implementing it in checkpoint 5
would be untested-by-construction (no current kind is user-initiated). Landing
it alongside the kinds it sorts on keeps every checkpoint's behaviour testable
when written.

**Superseded:** CP-5's prior verify ("a completed background row drops before a
running user-initiated one") assumed user-initiated kinds existed at CP-5; that
assertion now lives in CP-6.

- **2026-07-21 — checkpoints 4 & 5 landed** (uncommitted). **CP-4:** new
  `started_at: Instant` on `ActiveTask` (set on both Start branches, reset on
  upsert) and `ActiveTask::is_visible(now)`: `Error` always renders, `Running`
  gates on `now - started_at >= 150ms`, `Completed`/`Superseded` gate on
  **run duration** `completed_at - started_at >= 150ms`. Gating completed tasks
  on run duration rather than wall-clock age is the load-bearing distinction —
  otherwise a 50ms task would surface ~150ms after starting, mid post-complete
  linger. Orchestrator injected the wall-clock-age bug and confirmed
  `fast_completed_task_never_becomes_visible` catches it (fixture: 50ms run,
  `now` at start+200ms). It is a display filter only; state pruning is untouched.

  **CP-5:** `sort_visible_tasks` orders running-before-terminal, then
  oldest-first by `started_at`, then `id` (total, stable). `render_task_queue`
  now does filter(`is_visible`) → sort → `take(max_visible)`, with panel height
  from the final list and an early return when nothing is visible; the row loop
  iterates the sorted result, not the old `active_tasks[..n]` slice. The
  user-initiated tier is deferred to CP-6 with the kinds it discriminates on.
  Agent falsified the sort by dropping the running key; orchestrator
  re-verified the full render wiring.
