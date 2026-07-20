---
id: obs-partial-lazy-reset
title: obs_partial reset lazily on first Point, not on PartialCommit/Ready
created: "2026-07-20"
origin: |
    /refresh-signals scan
kind: finding
severity: nit
review_by: "2026-09-18"
status: open
file: src/app.rs:2038-2100
---

A refresh that produces zero Point messages before erroring could theoretically leak stale partial observation state into a later refresh. Guarded by a refresh id check, so low risk. Consider resetting obs_partial on PartialCommit/Ready instead.
