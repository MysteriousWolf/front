---
id: obs-backdrop-not-zoom-gated
title: Regional obs backdrop is fetched at every zoom, ungated
created: "2026-07-20"
origin: |
    plan:observation-tiering
kind: finding
severity: risk
review_by: "2026-09-18"
status: open
file: src/providers/eumetnet.rs
---

The viewport `/area` query is gated to `zoom >= CAPITALS_ZOOM_CUTOFF`
(`src/providers/eumetnet.rs:390`), but the regional backdrop batch at `:438`
has no zoom gate at all.

Zoomed in, the layer therefore pays for ~16 continental cell queries whose
stations are entirely off-screen, *on top of* the viewport query. Against an
anonymous quota of 50 requests/hour this is the single largest source of waste.

Fix: gate the backdrop to `zoom < CUTOFF`, or restrict it to cells intersecting
the expanded viewport. Planned as step 1 in `docs/design/observation-tiering.md`.
