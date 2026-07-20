---
id: obs-station-identity-is-the-name
title: ObservationPoint identity is the station name, not the WIGOS id
created: "2026-07-20"
origin: |
    plan:observation-tiering
kind: finding
severity: risk
review_by: "2026-09-18"
status: open
file: src/providers/eumetnet.rs
---

`station_id` is set to the human-readable station *name*, falling back to the
WIGOS id only when the name cache is cold (`src/providers/eumetnet.rs:727-733`),
and dedup across fetch phases is `seen_wigos.insert(pt.station_id)`.

Identity therefore changes as `surface-stations.json` warms: the same station
keys by WIGOS id on a cold cache and by name afterwards, within one session.

This is a correctness problem for dedup on its own, and it hard-blocks the
per-station cache in `docs/design/observation-tiering.md` step 3 — that design
needs a stable, geometry-independent key.

Fix: carry the WIGOS id as its own field on `ObservationPoint`
(`src/layers.rs`) and dedup on it; keep the name as display text only.
