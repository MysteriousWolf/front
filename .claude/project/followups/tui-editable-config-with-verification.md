---
id: tui-editable-config-with-verification
title: In-TUI editor for config.toml keys/tokens with live verification
created: "2026-07-21"
origin: |
    user sidenote 2026-07-21
kind: plan
review_by: "2026-09-19"
status: open
file: src/config.rs, src/ui.rs:901
---

**Feature request (captured, not scoped).** All API keys / tokens / settings in
`config.toml` should be editable from inside the TUI, with a way to verify a
value was entered correctly — presented "in a nice way", like the help modal.

## User's shape

- A key (they suggested `s`) opens a modal, same visual family as the help modal
  (`render_help`, `src/ui.rs:901` — `Clear` + `Block`/`Paragraph`, toggled by the
  `ToggleHelp` action in `src/keys.rs`).
- Fields are editable in-place.
- Each secret can be *verified* — i.e. actively checked against its service, not
  just format-linted — so the user knows the key works before relying on it.

## What's editable (the secrets that matter)

From `src/config.rs`:
- `meteogate.api_key` (`:108`) — radar tiles + observations
- `meteoalarm.token` (`:123`) and `meteoalarm.mqtt_broker` (`:128`)
- `eumetnet.api_key` (`:145`) — gated by `api_key.trim().is_empty()` at `:153`

Anonymous access works today (empty keys → the 50/hr quota this whole
observation-tiering effort exists to live within). A key raises the quota, which
is exactly why making it easy to enter and confirm is worth something.

## Why this is a real project, not a toggle

1. **Text input in a TUI.** The `/` search prompt already routes printable keys
   into a buffer (`App::search_input`) — that is the reusable pattern, but a
   multi-field form needs focus management the single-line prompt does not have.
2. **"Verify" means an async probe per provider.** Each service needs its own
   check: MeteoGate a cheap authed request, MeteoAlarm the EDR collections
   endpoint, EUMETNET a `/locations` hit. That is new network code with
   valid/invalid/unreachable states — and it should go through the task system
   (see `task-system-unification`) so the overlay shows the check running,
   rather than a fourth ad-hoc spinner. Depends on that work landing.
3. **Persistence.** Edits must write back to `config.toml` via the existing
   atomic-write path (`src/cache.rs` / `src/config.rs`), preserving the
   hand-written comments the generated default carries (`config.rs:336+`) — a
   naive re-serialize would strip them. That is its own non-trivial constraint.
4. **Secret display.** Keys are secrets; the modal should mask them (show
   set/unset + last 4, not the full value) and never log them.

## Dependencies / ordering

- Best sequenced AFTER `task-system-unification` (verification probes ride the
  task overlay) and after the config surface stops advertising dead options
  (the parked MQTT doc issue, `mqtt-documented-not-implemented` — do not build a
  verify button for a broker that nothing connects to).
- Needs its own design doc before a spec: input/focus model, the per-provider
  verification contract, comment-preserving TOML write-back. Not a
  drop-in-and-go checkpoint.
