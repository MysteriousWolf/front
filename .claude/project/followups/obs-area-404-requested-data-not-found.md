---
id: obs-area-404-requested-data-not-found
title: Every /area request in the log returns 404 'Requested data not found' — zero successes
created: "2026-07-20"
origin: |
    subagent-implementation CP-probe
kind: finding
severity: risk
review_by: "2026-09-18"
status: open
file: src/providers/eumetnet.rs
---

`grep 'eumetnet: area HTTP' ~/.cache/front/front.log` returns **1935 hits, all
404**, body `{"detail":"Requested data not found."}`. Not one 200, and not one
429 — the rate limiting previously assumed to be the problem never appears.

404 here is an application-level "no data matched this query" from the EDR
service, distinct from the 502 the whole route is returning at the time of
writing (the upstream is currently down; gateway root answers, MeteoAlarm and
the radar S3 bucket are both healthy).

**Unresolved and important:** this may be caused by the 12 degree region cells
introduced by `region_cells` / `REGION_CELL_DEG`. A 12x12 degree polygon may
exceed what the EDR `/area` endpoint will service, and it may answer 404 rather
than 413. If so, the clustering change traded a rate-limit failure for a
total-failure mode.

Cannot currently be distinguished from the upstream outage: while the route
502s, no polygon of any size can be tested (1 degree, 3 degree and 12 degree
boxes all 502).

**Next step when the service recovers:** re-run the cap probe — 1 degree vs
3 degree vs 12 degree boxes over the same area, comparing status, station count
and payload size. That single measurement resolves both this finding and the
top risk in `docs/spec/observation-tiering.md`, and determines whether
checkpoints 4-5 proceed as specced or invert toward finer cells.
