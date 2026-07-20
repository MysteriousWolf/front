# Zoom-stable caching — spec

## Goal

A zoom change stops discarding rendered radar frames. Zooming out and back
reuses frames already built at the destination zoom instead of re-tiling them at
~140 ms each.

## Non-goals

- Changing what is fetched. Radar already downloads one full-resolution grid per
  timestamp and derives every zoom locally.
- Persisting built tiles to disk. The `.frd` grid is the durable artifact.
- Reworking `RadarFrame`'s internal representation.
- The observation-side cache split — that is `docs/spec/observation-tiering.md`
  checkpoint 4, and shares only the principle, not the code.

## Success criteria

- [ ] Zoom from A to B and back to A re-tiles zero frames that were resident at
      A and never evicted.
- [ ] `frame_cache` is keyed by `(timestamp, tile_zoom)`; no code path clears the
      whole map on a zoom change.
- [ ] Eviction prefers entries at a non-current zoom over any entry at the
      current zoom, regardless of ring distance.
- [ ] With the cache at capacity and entries from three zooms resident, the
      current zoom's playhead window is fully retained.
- [ ] Resident entry count never exceeds `FRAME_CACHE_MAX`, which stays at 48
      for this change — see the Risks table for why raising it is deferred.

## Approach

Key `frame_cache` by `(timestamp, tile_zoom)` and change the eviction ranking
from ring distance alone to `(zoom != current, ring_distance)` — see
`docs/design/zoom-stable-caching.md`.

## Checkpoints

| # | Checkpoint | Files/areas | Agent | Est. files | Verifies |
|---|------------|-------------|-------|------------|----------|
| 1 | Generalise `frames_to_evict` to keyed entries with a current-zoom argument and the two-key ranking | `src/app.rs` | atomic-implementer (mode: surgical) | 1 | Existing eviction tests still pass; new test asserts non-current-zoom entries evict first |
| 2 | Key `frame_cache` by `(timestamp, tile_zoom)`; delete the clear-on-zoom-change branch and update all read/write sites | `src/app.rs`, `src/ui.rs` | atomic-implementer (mode: feature) | ~2 | Test: zoom A → B → A re-tiles nothing; `slot_state` still reports correctly per zoom |
| 3 | Re-examine whether a zoom change should still abort the in-flight preload, given the destination zoom may already be cached | `src/app.rs` | atomic-implementer (mode: surgical) | 1 | Test: preload for an already-cached destination zoom is not aborted needlessly |

## Risks

| Risk | Likelihood | Mitigation |
|------|-----------|-----------|
| Memory grows with the number of zooms visited; entry count is a poor proxy for bytes, since a zoom-7 frame is far larger than a zoom-1 frame | medium | Ranking evicts non-current-zoom entries first, so spares occupy only slack. If it bites, switch the cap from entries to estimated bytes — the design records this as approach B and only the eviction function changes. |
| `FRAME_CACHE_MAX = 48` was sized when only one zoom could be resident; holding it fixed means spare-zoom entries now compete with the live playhead window rather than adding to it | medium | **Deliberately held at 48 for this change.** Raising it trades memory for a benefit that only appears once the new ranking is measurable, and moving both the key and the cap at once would make a regression impossible to attribute. Revisit with the byte-based cap, not before. |
| `slot_state` / timeline rendering reads the cache by timestamp alone and silently reports a frame as resident that exists only at another zoom | medium | Checkpoint 2 covers every read site; the timeline test asserts per-zoom correctness |
| Checkpoint 3 adds a branch to a hot path for a marginal gain | low | Keep it last and independent; drop it if measurement shows no benefit |

## Change log
