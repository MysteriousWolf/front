# Backlog cleanup — spec

## Goal

Clear the standing follow-up backlog that the three feature specs do not already
absorb: one documentation-vs-reality risk, two per-frame raster scans, and two
structural nits.

## Non-goals

- The observation follow-ups (`obs-backdrop-not-zoom-gated`,
  `obs-zoom-cutoff-mismatch`, `obs-station-identity-is-the-name`,
  `obs-station-list-off-budget`, `obs-location-cache-memory-only`) — those are
  checkpoints in `docs/spec/observation-tiering.md`.
- Implementing MQTT. See the decision gate below; this spec covers the *decision*
  and whichever branch follows, not a commitment to build it.

## Success criteria

- [ ] `grep -rn 'rumqttc\|feature = "mqtt"' src/` and CLAUDE.md's claim about the
      `mqtt` feature agree with each other.
- [ ] `raster_pin`'s label work does not scan the full cell grid per pin per frame.
- [ ] `nearest_free_cell` does not re-scan inner rings it has already rejected.
- [ ] Pin rendering behaviour is unchanged: the existing `pin_label_*` and
      `nearest_free_cell` tests pass untouched.
- [ ] `obs_partial` is reset on `PartialCommit`/`Ready` rather than lazily on the
      first `Point`.

## Approach

Each item is independently small; no design doc. Approaches are stated per
checkpoint below since there is no design to hold them.

### Decision gate — MQTT

`CLAUDE.md` states the default `mqtt` feature "adds MQTT live-update support for
MeteoAlarm". Verified: `rumqttc` is an optional dependency and the feature is
declared, `config.rs` carries `meteoalarm.mqtt_broker`, but `src/` contains
**zero** references to `rumqttc`, `cfg(feature = "mqtt")`, or `mqtt_broker`. By
contrast the comparable optional `lightning` feature has 4. The feature is
manifest- and config-only, and the documentation claims otherwise.

Two honest resolutions, and the user picks:

| Option | Effect | Cost |
|---|---|---|
| **Correct the docs** | CLAUDE.md and the generated `config.toml` stop advertising a capability that does not exist; the feature flag and config key are removed or marked "reserved" | small |
| **Implement it** | MeteoAlarm warnings update live over MQTT instead of on the poll interval | substantial — a new long-lived connection, reconnect/backoff, and a fifth async source through `App`'s channel plumbing |

Default recommendation: correct the docs. A documented-but-absent feature is a
trust problem now; live warning updates are a feature request that should be
justified on its own merits, not inherited from a stale doc line.

### Raster scans

Both were introduced by the pin-labelling work and are the same shape — work
proportional to the whole grid, repeated per pin per frame:

- `recolor_existing_label` scans every cell of the grid looking for the label
  text. The full-grid scan is *deliberate* (the pin and the capital label are
  anchored to different points and can be far apart), so the fix is to make the
  scan cheap — a first-character prefilter before the full comparison, or
  restricting the scan to rows that contain any glyph at all — not to
  reintroduce a proximity window.
- `nearest_free_cell` re-walks every cell within radius `r` on each ring
  iteration instead of only the ring at distance `r`, making it O(radius²) where
  the ring itself is O(radius).

## Checkpoints

| # | Checkpoint | Files/areas | Agent | Est. files | Verifies |
|---|------------|-------------|-------|------------|----------|
| 1 | Resolve the MQTT gate: apply the user's chosen branch | `CLAUDE.md`, `src/config.rs`, `Cargo.toml` (docs branch) | atomic-implementer (mode: surgical) | 1-3 | `grep` for mqtt symbols agrees with the documented claim |
| 2 | Prefilter `recolor_existing_label` so the common no-match case does not pay a full-grid character comparison | `src/ui.rs` | atomic-implementer (mode: surgical) | 1 | Existing `pin_label_*` tests pass unchanged; new test asserts a no-match grid is not fully compared |
| 3 | Walk only the ring at distance `r` in `nearest_free_cell` | `src/ui.rs` | atomic-implementer (mode: surgical) | 1 | Existing nudge tests pass unchanged; new test asserts inner rings are visited once |
| 4 | Reset `obs_partial` on `PartialCommit`/`Ready` instead of lazily on first `Point` | `src/app.rs` | atomic-implementer (mode: surgical) | 1 | Test: a refresh producing zero `Point`s before erroring leaves no stale partial state |
| 5 | *(deferred)* Extract the repeated channel + `drain_*` pattern in `App` behind a small trait | `src/app.rs` | atomic-implementer (mode: feature) | ~2 | Every existing drain test passes unchanged |

Checkpoints 1-4 are independent. Checkpoint 5 is deliberately last and should
not be started until the feature specs land — they add and reshape async
sources, and abstracting the pattern before it has settled would abstract the
wrong thing.

## Risks

| Risk | Likelihood | Mitigation |
|------|-----------|-----------|
| Removing the `mqtt` feature flag breaks an existing `--no-default-features` build invocation or a user's `config.toml` | low | Keep the config key parsing (unknown keys already tolerated via `serde(default)`); only the doc claim and the dead flag change |
| A `recolor_existing_label` prefilter changes match semantics — e.g. case-insensitivity is lost in the fast path | medium | The prefilter must use the same case-insensitive comparison as the full path; the existing case-insensitivity test is the guard |
| Checkpoint 5 abstracts a pattern that the feature work is about to change | high | Explicitly deferred until the feature specs land |

## Change log

## Change log

- **2026-07-21 — checkpoint 4 landed** (uncommitted). `obs_partial` (and
  `obs_incoming`/`obs_incoming_id`) now reset via `reset_obs_accumulator` at the
  observation-refresh kickoff (`app.rs:1189`), once per refresh unconditionally,
  instead of lazily on the first `Point`. A refresh that errors before producing
  any `Point` can no longer leave the prior refresh's data lingering in the
  accumulator.

  The lazy reset block in the `Point` handler was removed as genuinely dead:
  `obs_incoming_id` is now written only at init and at kickoff (with the same id
  as `obs_refresh_id`), and `drain_obs_results` only processes messages whose id
  equals the current `obs_refresh_id` — so an accepted `Point` can never satisfy
  the old `result.id != obs_incoming_id` condition. Trace verified against every
  write site before deletion.

  Test seam: a full `App` is only constructible via `async fn boot` with real
  I/O, so the reset logic was extracted as a free function and unit-tested
  directly on the accumulator fields. Note this covers the reset *logic* but not
  the *wiring* — if the kickoff call were removed, no test would catch it. Left
  as-is: the wiring is one line, verified by reading, and adding an integration
  test would require an App test-harness that does not exist.

  Closes follow-up `obs-partial-lazy-reset`.

  CP-1 (MQTT) remains parked; CP-5 (`App` channel plumbing) remains deferred by
  design until the task-system work settles the async-source pattern.
