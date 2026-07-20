---
id: obs-zoom-cutoff-mismatch
title: CAPITALS_ZOOM_CUTOFF (5.5) and MAJOR_CITIES_ZOOM_CUTOFF (5.0) disagree
created: "2026-07-20"
origin: |
    plan:observation-tiering
kind: finding
severity: risk
review_by: "2026-09-18"
status: open
file: src/providers/eumetnet.rs
---

`CAPITALS_ZOOM_CUTOFF = 5.5` (`src/providers/eumetnet.rs:88`) and
`MAJOR_CITIES_ZOOM_CUTOFF = 5.0` (`src/ui.rs:59`) each carry a comment saying
they must match the other. They do not.

Between zoom 5.0 and 5.5 the fetch tier and the display tier disagree: the
renderer is willing to show major-city stations that the fetch tier has not
been asked to retrieve.

Fix: make it one shared constant rather than two that must be kept in sync.
