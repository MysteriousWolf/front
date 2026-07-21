---
id: tui-config-editor-f4-unreachable-invariant
title: pending_changes unreachable arm relies on construction-enforced invariant
created: "2026-07-21"
origin: |
    docs/spec/tui-config-editor.md, iter 4 reviewer (CP-4)
kind: finding
severity: nit
review_by: "2026-09-19"
status: open
file: src/settings.rs:214
---

The unreachable!() arm in SettingsModel::pending_changes holds only because nothing replaces a field kind after construction — enforced by discipline, not the type system. If field kinds ever become mutable this becomes a real panic. Low value now; noted for awareness.
