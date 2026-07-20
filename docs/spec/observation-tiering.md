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
