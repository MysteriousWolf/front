---
id: recolor-label-full-grid-scan
title: recolor_existing_label scans the full cell grid per pin per frame
created: "2026-07-20"
origin: |
    /refresh-signals scan
kind: finding
severity: nit
review_by: "2026-09-18"
status: open
file: src/ui.rs:1850-1885
---

Case-insensitive substring match across width x height x name_len with no early exit beyond a length pre-check.
