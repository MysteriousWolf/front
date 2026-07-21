---
id: tui-config-editor-f1-inline-table
title: apply_config_edits drops sibling keys in an inline-table section
created: "2026-07-21"
origin: |
    docs/spec/tui-config-editor.md, iter 1 reviewer (CP-1)
kind: finding
severity: nit
review_by: "2026-09-19"
status: open
file: src/config.rs:438
---

apply_config_edits replaces an intermediate segment written as an inline table (location = { ip_fallback = true }) with a fresh Table::new(), dropping any sibling keys inside that inline table. Not hit by the normal [section] config format; no repro in the real file. Fix: detect inline-table intermediates and edit in place, or add a preservation test if inline configs are realistic input.
