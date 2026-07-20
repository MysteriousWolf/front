---
id: nearest-free-cell-ring-search
title: nearest_free_cell ring search is O(radius^2) per pin per frame
created: "2026-07-20"
origin: |
    /refresh-signals scan
kind: finding
severity: nit
review_by: "2026-09-18"
status: open
file: src/ui.rs:1958-1971
---

Fine at the current PIN_NUDGE_RADIUS = 3, but would not scale if the radius were raised.
