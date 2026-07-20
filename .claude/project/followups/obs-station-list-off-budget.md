---
id: obs-station-list-off-budget
title: fetch_station_list bypasses the request budget
created: "2026-07-20"
origin: |
    plan:observation-tiering
kind: finding
severity: nit
review_by: "2026-09-18"
status: open
file: src/providers/eumetnet.rs
---

Every `/area` query goes through `try_spend`, but `fetch_station_list` issues
its `/locations` request directly without reserving budget.

The budget is therefore not a total: the gateway counts a request that `front`
does not. It is one request per 24 h so the practical impact is small, but it
means the client-side model can under-count, which is the wrong direction for a
safety margin.

Fix: route it through `try_spend` like every other request.
