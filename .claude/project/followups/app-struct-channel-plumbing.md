---
id: app-struct-channel-plumbing
title: App is a 60+ field struct with hand-inlined channel/drain plumbing
created: "2026-07-20"
origin: |
    /refresh-signals scan
kind: finding
severity: nit
review_by: "2026-09-18"
status: open
file: src/app.rs
---

Every new async data source means manually repeating the channel + drain_* pattern. Candidate for a small trait or macro once the pattern has a third or fourth instance.
