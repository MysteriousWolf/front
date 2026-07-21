# Observation tiering — spec

## Goal

Observations resolve viewport-first and importance-ordered, survive a zoom change
without refetching, and stay inside the MeteoGate anonymous quota of 50
requests/hour.

## Non-goals

- Per-city requests at any quota. One pass over 113 cities is 2.3× the anonymous
  hourly allowance; this is settled, not a tuning target.
- A quadtree lattice (approach C in the design). Deferred until an API key is in
  play and coarse cells prove insufficient.
- Palette, thinning-rule, or label-layout changes.
- A new observation data source.

## Success criteria

- [ ] At `zoom >= CUTOFF`, a refresh issues **no** regional backdrop requests —
      only the viewport query. Verified by counting `try_spend` calls in a test
      that drives `fetch_observations` at high zoom.
- [ ] One shared zoom-cutoff constant; `grep` finds no second threshold that the
      renderer and the fetcher must keep in sync by hand.
- [ ] Cell fetch order is `(overlaps viewport desc, best city tier inside asc,
      distance from centre asc)`. Unit-testable as a pure ordering function over
      a fixture cell list.
- [ ] `ObservationPoint` carries the WIGOS id as its own field; dedup uses it,
      and a cold-vs-warm name cache produces identical dedup results for the
      same station set.
- [ ] A zoom change with no viewport movement issues zero new requests when the
      covering cells are within TTL.
- [ ] A reading older than the age cap is dropped, never rendered stale.
- [ ] The station store survives a restart: a second launch within TTL renders
      stations without issuing any request.
- [ ] The station store honours a bounded size: past the cap, the least-recently
      observed stations are evicted and the on-disk payload stops growing.
- [ ] A full cold refresh at `zoom = 4.0` (below `CUTOFF`, the worst case — the
      whole regional backdrop is in play) stays within `budget_limit()` for the
      anonymous quota.

## Approach

Fix the budget-leak bugs, then reorder cells by importance, then split the cache
into a geometry-independent station store plus a geometry-keyed coverage ledger
— see `docs/design/observation-tiering.md`.

## Checkpoints

| # | Checkpoint | Files/areas | Agent | Est. files | Verifies |
|---|------------|-------------|-------|------------|----------|
| 1 | Gate the regional backdrop to `zoom < CUTOFF`; unify the two zoom-cutoff constants into one shared value | `src/providers/eumetnet.rs`, `src/ui.rs` | atomic-implementer (mode: surgical) | 2 | Test: high-zoom refresh issues no backdrop requests; no second cutoff constant exists |
| 2 | Reorder cell fetch by `(viewport overlap, city tier, centre distance)`; extract the ordering as a pure function | `src/providers/eumetnet.rs` | atomic-implementer (mode: surgical) | 1-2 | Unit test over a fixture cell list asserts the three-key ordering |
| 3 | Carry WIGOS id on `ObservationPoint`; dedup on it instead of the display name | `src/layers.rs`, `src/providers/eumetnet.rs` | atomic-implementer (mode: feature) | ~3 | Test: identical dedup with a cold and a warm name cache |
| 4 | Split `location_cache` into a WIGOS-keyed station store and a cell-keyed coverage ledger; fetch decisions read the ledger, rendering reads the store. The store is disk-backed with a bounded size and an LRU eviction by last-observed time — this closes the design's bug 4, where the 6 h TTL silently meant "until quit" | `src/providers/eumetnet.rs`, `src/cache.rs` | atomic-implementer (mode: feature) | ~3 | Tests: zoom change with unchanged viewport issues zero requests; a restart within TTL renders stations with zero requests; the store past its cap evicts and stops growing on disk |
| 5 | Enforce the reading age cap and surface reading age in the UI | `src/providers/eumetnet.rs`, `src/ui.rs`, `src/layers.rs` | atomic-implementer (mode: feature) | ~4 | Test: a reading past the cap is dropped, not rendered |
| 6 | Route `fetch_station_list` through `try_spend` so the budget is a true total | `src/providers/eumetnet.rs` | atomic-implementer (mode: surgical) | 1 | Test: the `/locations` fetch consumes budget |

Checkpoints 1, 2 and 6 are independent and can land in any order. 4 depends on 3.
5 depends on 4.

## Risks

| Risk | Likelihood | Mitigation |
|------|-----------|-----------|
| `/area` responses over a 12° cell are capped or paginated, silently dropping the small stations this design promises to tier | medium | **Measure before checkpoint 4.** One probe request against a 12° cell, comparing station count and payload size against a 3° cell over the same area. If capped, the recommendation inverts toward finer cells and a mandatory API key. |
| Store/ledger split degrades freshness invisibly — stations render everywhere with hours-old readings because the ledger says "covered" | high if unmitigated | Checkpoint 5 is not optional polish; the age cap and age display ship with the split |
| Per-station store grows without bound where the single-file cache did not | medium | Size cap and bounded on-disk format as part of checkpoint 4 |
| WIGOS ids turn out not to be stable across responses | low | Checkpoint 3 lands independently; if identity proves unstable, checkpoints 4-5 are abandoned and 1-2 still deliver the ordering requirement |

## Change log

- **2026-07-20 — checkpoint 2 landed** (uncommitted). Cell fetch order is now
  `(overlaps viewport desc, best city tier asc, centre distance asc)`, extracted
  as pure `order_cells` alongside `CityTier`, `cell_tier` and `cell_bounds`
  (`eumetnet.rs:152-235`). This matters because the budget can be exhausted
  partway through a refresh, so fetch order decides what the user actually
  receives; the previous nearest-centre-first sort was arbitrary with respect to
  what was on screen.

  Tier lookup takes the capitals and majors lists as slices rather than reading
  the `EUROPEAN_*` constants directly, so tests can inject fixtures that vary
  each sort key independently — which is what makes the precedence testable at
  all. Overlap reuses the existing `Bounds::intersects` rather than a new
  intersection test. When the viewport is unknown (`bounds: None`) the ordering
  falls back to a whole-world box, so every cell overlaps and the order degrades
  to tier-then-distance instead of behaving arbitrarily.

- **2026-07-20 — checkpoint 3 landed** (uncommitted). `ObservationPoint` now
  carries `wigos_id` (`src/layers.rs`) and all four dedup sites key on it
  instead of the display name. The two construction sites were collapsed into
  one pure `station_to_point` helper, which is what made the identity test
  possible without an HTTP mock.

  **`#[serde(default)]` on the new field required a second fix.**
  `ObservationPoint` is persisted inside `DiskCacheEntry`, so a cache entry
  written by a build predating `wigos_id` deserializes with every id defaulted
  to `""`. Keying those through a plain `HashSet::insert` collapses them all
  onto the single empty key — measured: a 3-station pre-upgrade layer survived
  dedup as **1 station**, and would render that way until the 600 s viewport
  TTL expired. Dedup now goes through `admit_point`, which admits an empty id
  unconditionally instead of inserting it: a point with no identity cannot be
  shown to duplicate anything, so the safe reading is "keep it". Ids repopulate
  on the next successful fetch.

  Closes follow-up `obs-station-identity-is-the-name`. Unblocks checkpoints 4
  and 5, which still need the `/area` cap probe before they can start.

- **2026-07-20 — checkpoint 6 landed** (uncommitted). `fetch_station_list` now
  reserves through `try_spend` before its `/locations` GET, between the
  fresh-cache early return and the network call; a refusal falls back to the
  stale cache, matching the five existing degrade-gracefully returns in that
  function. The budget is now a true total.

  **The spec's own test recipe was wrong and was corrected during
  implementation.** It proposed draining the budget to zero and asserting
  `budget_remaining()` was unchanged. That is not falsifiable:
  `RequestBudget::try_spend` only decrements on success, so a refused
  reservation leaves the count at zero whether or not the reservation is
  attempted — the assertion passes on the unfixed code. The test instead starts
  with budget *available*, points at an unroutable endpoint, and asserts the
  count drops by exactly 1 even though the request itself fails. Verified to
  fail on the pre-fix code (`left: 40, right: 39`). Closes follow-up
  `obs-station-list-off-budget`.

- **2026-07-20 — checkpoint 1 landed** (`4d8bba2`). The two zoom cutoffs are now
  one shared `OBS_TIER_ZOOM_CUTOFF = 5.5` in `src/geo.rs`; the regional backdrop
  is gated behind the pure predicate `should_fetch_backdrop(zoom)` in
  `src/providers/eumetnet.rs`. 5.5 was chosen over 5.0 because the viewport
  query is the expensive one — deferring the switch to it keeps the anonymous
  quota intact longer, at the cost of major-city stations rendering one display
  tier later. `ALL_OBS_ZOOM_CUTOFF` (6.5) and `STATION_NAMES_ZOOM` (5.5) were
  deliberately left alone: they gate display density, not the fetch/display tier
  boundary. Closes follow-ups `obs-zoom-cutoff-mismatch` and
  `obs-backdrop-not-zoom-gated`.
