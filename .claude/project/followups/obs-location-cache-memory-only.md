---
id: obs-location-cache-memory-only
title: location_cache is memory-only, so the 6h backdrop TTL dies at exit
created: "2026-07-20"
origin: |
    plan:observation-tiering
kind: finding
severity: nit
review_by: "2026-09-18"
status: open
file: src/providers/eumetnet.rs
---

`location_cache` is constructed as an empty in-memory map
(`src/providers/eumetnet.rs:226`, `:253`) and is never loaded from or written
to disk, unlike the viewport layer cache and the station list.

`CAPITAL_DATA_TTL` is 6 h, but in practice it means "6 h or until quit". Every
launch re-pays the full regional backdrop against a 50/hour quota — a
per-launch cost nobody chose.

Fix: persist it, or state the in-memory lifetime in the constant's doc comment
so the 6 h figure stops being misleading. See the open question in
`docs/design/observation-tiering.md`.
