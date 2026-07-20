# Zoom-stable caching — stop discarding work on every zoom change

## Problem

Changing zoom throws away every rendered radar frame:

```rust
// src/app.rs:690
if tile_zoom != self.frame_cache_zoom {
    self.frame_cache.clear();
    self.frame_cache_zoom = tile_zoom;
    if let Some(task) = self.radar_preload_task.take() { task.abort(); }
}
```

Nothing about the *source data* changed — the `.frd` grids are still on disk and
still valid, since they are stored per timestamp with no zoom in the key
(`cache_path`, `src/providers/meteogate.rs`). What is discarded is the derived
tile work. Every frame in the playback window must then be re-read and re-tiled,
and the in-code measurement puts that at ~140 ms per frame. On a 24 h window the
user pays a visible stall for a zoom step, then pays it again if they zoom back.

The same shape appears on the observation side: caches are keyed by fetch
geometry, so a zoom change that alters the queried bbox invalidates entries that
hold data still perfectly good for the new view.

This is one concern with two instances, which is why they share a design.

## Goals / Non-goals

**Goals**

- Zooming out and back must not re-tile frames already built at the destination zoom.
- Bound memory explicitly. The current cap is already generous and multiplying it
  by the number of zoom levels is not acceptable.
- Preserve the existing eviction intent: keep what the playhead is near.

**Non-goals**

- Changing what is fetched from the network. Radar already fetches one
  full-resolution grid per timestamp and derives every zoom locally; that is
  optimal and stays as it is.
- Caching tiles to disk. The `.frd` grid is the durable artifact; tiles are
  cheap enough to rebuild from it that persisting them buys little.
- Reworking `RadarFrame`'s internal representation.

## The memory constraint, stated honestly

`FRAME_CACHE_MAX` is 48 frames, and the existing comment puts a zoom-7 frame at
~5 MB of built tiles — roughly 240 MB already. Keying by `(timestamp, zoom)`
without changing the cap would let that scale with the number of zoom levels
visited, which on a long session is every level from 1 to 7.

So the cap cannot stay a plain count of entries. Two properties matter:

- Frames at low zoom are far smaller than frames at high zoom — fewer tiles
  cover the viewport — so "48 entries" means wildly different memory depending
  on which zooms are resident.
- Only one zoom is being *viewed* at a time. Other zooms are speculative value:
  worth keeping if free, never worth evicting the current view for.

## Approaches

| # | Approach | Pros | Cons |
|---|----------|------|------|
| A | Key by `(ts, zoom)`, evict ranked by *(is current zoom, ring distance)* | Keeps the existing distance intent; other zooms survive only as spare capacity | Entry count still a poor proxy for bytes |
| B | Key by `(ts, zoom)`, cap by estimated bytes rather than entries | Memory bound is the thing actually being bounded | Needs a size estimate per frame; more machinery |
| C | Two-tier: full cache for current zoom, small fixed side-cache (~8 entries) for the previous zoom | Directly targets the observed behaviour — zoom out, look, zoom back | Only helps one step of history; a three-zoom cycle still thrashes |
| D | Keep clearing, but rebuild from the in-memory grid LRU instead of disk | No cache growth at all | `MAX_CACHED_GRIDS` is 2, so 46 of 48 frames still hit disk; does not solve it |

## Recommendation

**Approach A, with the eviction key changed from distance to
`(zoom != current, ring_distance)`.**

The ordering is what does the work: every entry at a non-current zoom sorts
after every entry at the current zoom, so speculative entries are evicted first
and the current view's window is never sacrificed for them. Under steady viewing
the cache behaves exactly as it does today; the retained frames from other zooms
occupy only the slack between the live window and the cap.

B is the theoretically correct bound and should be revisited if memory proves a
problem in practice, but it needs a per-frame byte estimate that does not exist
yet, and the entry cap has been adequate so far. Adopting A does not foreclose
B — the eviction function is the only thing that would change.

C is a narrower special case of A that happens to cover the common gesture; A
subsumes it at the same complexity.

`frames_to_evict` (`src/app.rs:180`) is a pure function with existing tests, and
is the correct and only place this ranking changes. Its signature grows a
"current zoom" argument and its `cached` parameter becomes keyed pairs.

## Observation-side caching

The observation caches key on the fetch geometry — `location_cache` is keyed by
`(lat.to_bits(), lon.to_bits())` of the queried box centre, and the viewport
layer cache by bounds. Cache identity is therefore welded to *how the data was
asked for* rather than *what the data is*, so any change to the query shape
discards results that remain valid.

A station reading is a fact about a station at a time. It does not become less
true because the next query used a different bounding box. The geometry-free
identity is the WIGOS station id, which the provider already parses and already
uses to deduplicate across phases (`seen_wigos`).

Moving the cache to per-station identity with a per-station timestamp would make
zoom changes free by construction: the new view asks which stations it needs,
takes what is fresh, and fetches only the remainder. That is precisely the
"diff, don't refetch" behaviour requested.

This is *recommended in shape only here* — the fetch-unit geometry it depends on
is being settled in `docs/design/observation-tiering.md`, and the two must agree
before either is specced. Recorded here so the shared principle is written down
once: **cache by what the data is, not by how it was requested.**

## Open questions

- Should a zoom change abort the in-flight preload at all? Today it does. If the
  destination zoom's frames are already cached, aborting is pure loss; if they
  are not, the in-flight work is for the wrong zoom and aborting is correct.
  The condition is knowable at abort time but adds a branch to a hot path.
- ~~Is `FRAME_CACHE_MAX = 48` still the right number once entries from several
  zooms coexist?~~ **Resolved: held at 48 for this change.** Moving the key and
  the cap in the same step would make any regression impossible to attribute.
  Revisit together with the byte-based cap (approach B), not before.
