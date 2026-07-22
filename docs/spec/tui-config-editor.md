# In-TUI config editor with live verification

## Goal

A TUI modal (opened by a dedicated key, same visual family as the help overlay)
that edits the quota-raising `eumetnet.api_key` secret and the `ip_fallback`
preference in place, verifies the secret against its live service, persists edits
to `config.toml` without disturbing comments, and applies a saved key immediately
by rebuilding the affected provider — no restart.

## Non-goals

- No editing of service endpoints, S3 endpoint/bucket, or broker URLs.
- No `meteoalarm` section (token or `mqtt_broker`) in v1.
- No `meteogate.api_key` — it is a dead config field (no code sends it; the radar
  S3 bucket is anonymous). Editing it would change nothing and there is no live
  endpoint to verify it against. Excluded until the key is actually wired into
  radar/ORD requests (separate follow-on work).
- No `state.toml` editing.
- No startup viewport (`lat/lon/zoom`) editing — set by panning, saved to `state.toml`.
- No *new* preference settings beyond `ip_fallback` (e.g. auto-recenter-on-location). The field model must accommodate future bool preferences as data-only additions, but wiring a new setting's runtime behavior is follow-on work.
- No new general-purpose settings framework beyond the typed-field set.
- No restart-to-apply path.

## Success criteria

- [ ] A dedicated key opens/closes a centered modal in the help visual family;
      while open it owns the keyboard (printable keys never trigger global
      actions), matching the search-prompt takeover.
- [ ] The modal edits `eumetnet.api_key` (secret string) and
      `location.ip_fallback` (bool) via a focus list; the secret renders masked
      (`set ••••1234` / `unset`), never in full in logs.
- [ ] The focused secret can be toggled to reveal its full value in place
      (paste confirmation); un-focusing re-masks. Other fields stay masked.
- [ ] Edits stage in the modal; a single confirm applies all changed fields at
      once (Esc discards unsaved edits). Before applying, a minimal diff of
      pending changes is shown for review (changed fields only, secrets masked).
- [ ] Saving writes only the changed keys to `config.toml`; a file with
      hand-written comments and extra keys keeps them after a save (verified by
      test).
- [ ] Each secret can be verified against its service on its staged value,
      classifying into Valid / Invalid / Unreachable, surfaced as a task-overlay
      row (indeterminate marquee), with no secret value logged.
- [ ] A saved secret takes effect without restart: the affected provider is
      rebuilt from the new config and its refresh re-kicked; in-flight results
      from the old provider are discarded via the existing `refresh_id` check.
- [ ] Empty secret is a valid, non-error state (anonymous access); verify is
      offered only when a value is present.
- [ ] `cargo test`, `cargo clippy --all-targets --all-features -- -D warnings`,
      and `cargo fmt --check` pass.

## Approach

Modal reusing the help-overlay render pattern and the search-prompt keyboard
takeover; `toml_edit` surgical write-back; live provider rebuild via the
`refresh_id` staleness mechanism — see `docs/design/tui-config-editor.md`.

## Checkpoints

| # | Checkpoint | Files/areas | Agent | Est. files | Verifies |
|---|------------|-------------|-------|------------|----------|
| 1 | Surgical config write-back: update named keys in `config.toml` via `toml_edit`, preserving comments/order/extra keys; add `toml_edit` as a direct dep. | `src/config.rs`, `Cargo.toml` | atomic-implementer (feature) | ~2 | Unit test: editing one key preserves surrounding comments + a hand-added key; round-trips the new value. |
| 2 | Verify probe for `eumetnet.api_key`: async check classifying HTTP outcome → `Valid`/`Invalid`/`Unreachable` (shared pure classifier); runs through `task_tx` as an indeterminate task; no secret logged. | `src/providers/verify.rs`, `src/providers/eumetnet.rs`, `src/app.rs` | atomic-implementer (feature) | ~3 | Unit test: status→outcome classification (200→Valid, 401/403→Invalid, 429/5xx/network→Unreachable). |
| 3 | Live provider rebuild: `App` method that mutates `self.config` for an edited section, reassigns the provider field from it, bumps the relevant `refresh_id`, and re-kicks its refresh. | `src/app.rs` | atomic-implementer (feature) | ~1 | Unit test: rebuild swaps provider config and increments the refresh id so stale results are discarded. |
| 4 | Settings field model: typed fields (secret string, bool) with staged edits, dirty tracking, mask formatting (`set ••••1234`/`unset`) with a per-field reveal flag, focus navigation, a pending-changes diff (changed fields only), and apply/discard — as testable pure logic separate from rendering. Bool fields are data-only additions so future preferences drop in without new machinery. | `src/config.rs` or new `src/settings.rs`, `src/app.rs` | atomic-implementer (feature) | ~2 | Unit tests: mask shows only last 4; reveal flag unmasks focused field only; staged edit applies on confirm, dropped on discard; bool toggles; pending diff lists only changed fields. |
| 5 | Modal render + key wiring: draw the settings modal (help visual family) from the field model, including the reveal toggle and the pending-changes diff shown before apply; add the open key + keyboard takeover before `keys::resolve`; a help row for it; confirm→diff→write-back→rebuild→verify wiring; conflict-check the open key. | `src/ui.rs`, `src/keys.rs`, `src/app.rs` | atomic-implementer (feature) | ~3 | Full suite green; manual: open modal, edit a key, reveal it, verify, review pending diff, confirm, provider refresh uses new key. |

## Risks

| Risk | Likelihood | Mitigation |
|------|-----------|-----------|
| Live provider swap races an in-flight refresh, mixing old/new-key results. | med | Reassign field then bump `refresh_id` before re-kick; the existing id check drops stale results. Cover with the CP-3 test. |
| A secret leaks into `~/.cache/front/front.log` via an error/debug path. | med | No secret in any `write_log`/probe error string; assert masked formatting in CP-2/CP-4 tests; probe logs outcome, not value. |
| `toml_edit` write on a malformed/partial user file loses data. | low | Surgical edit of a parsed document, atomic write via the existing path; on parse failure, refuse to save and report rather than regenerate. |
| Open key collides with an existing binding. | low | CP-5 conflict-checks `keys.rs` before fixing the key; `s` is the proposed default only. |
| Verify probe hits a rate limit and reports `Unreachable` on a valid key. | low | Distinct `Unreachable` outcome is honest ("couldn't confirm"), never shown as `Invalid`; probe is a single cheap request. |

## Change log

### 2026-07-21 — Drop meteogate.api_key from scope

**What changed:** Removed `meteogate.api_key` from the editable field set and from
the CP-2 verify probe. The editor now covers exactly one verifiable secret
(`eumetnet.api_key`) plus the `ip_fallback` bool.

**Why:** CP-2 implementation discovered `meteogate.api_key` is never sent by any
request — the radar S3 bucket is anonymous and no ORD REST endpoint exists in the
codebase. Editing it changes nothing and there is no live endpoint to verify it
against. Confirmed by grep: the field is referenced only in config/doc comments.

**Superseded:** Prior contract had both `meteogate.api_key` and `eumetnet.api_key`
as editable, verifiable secrets and CP-2 building a probe for each.

## Implementation log

### Shipped (uncommitted, awaiting manual review) — 2026-07-21

Built across 6 iterations of /subagent-implementation. Per repo standing rule
(no agent git operations), nothing was committed — every checkpoint is left in
the working tree for the repo owner's manual review and commit. Not bisectable by
checkpoint; reviewers diffed the working tree against loop base `99363b1f`.

Checkpoints (all reviewer-PASS):

- CP-1 — `toml_edit` surgical config write-back (`apply_config_edits`), comment/
  order/extra-key preserving; malformed file refused. `src/config.rs`, `Cargo.toml`.
- CP-2 — `eumetnet.api_key` verify probe: shared pure classifier
  (`src/providers/verify.rs`), authed `/locations` probe, indeterminate task-overlay
  marquee, no secret logged. `src/providers/verify.rs`, `eumetnet.rs`, `app.rs`.
- CP-3 — live provider rebuild: `App` stores the HTTP client;
  `rebuild_eumetnet_provider` + pure `eumetnet_rebuild_needed`; re-kicks obs refresh
  via the `obs_refresh_id` staleness mechanism. `src/app.rs`.
- CP-4 — pure settings field model (`src/settings.rs`): typed Secret/Bool fields,
  masking, focused reveal, staged edits, dirty tracking, pending-diff, apply/discard;
  19 tests.
- CP-5 — modal render + key wiring (`s` opens; Ctrl+R reveal, Ctrl+V verify, Enter
  apply, Esc back/discard, arrows navigate); edit→verify→review-diff→apply→write-back
  →live-rebuild; masked-`Debug` hardening. `src/keys.rs`, `app.rs`, `ui.rs`, `settings.rs`.
- Polish — cleared the stale verify badge (`last_verify` reset on edit/focus/open/
  apply), plus two doc/legend nits.

Verification at finalize: 392 tests pass (44 added across the feature), `cargo clippy
--all-targets --all-features -- -D warnings` clean, feature files fmt-clean. (Repo
carries pre-existing `cargo fmt` drift in unrelated files, left untouched.)

**Out-of-scope work performed during this build:**
- Dropped `meteogate.api_key` from the editor mid-build (see Change log) — it is a
  dead config field (never sent; radar S3 is anonymous). A surgical removal iteration
  cleaned up the stubbed probe.

**Unforeseens — surprises during implementation:**
- `meteogate.api_key` is vestigial with no live endpoint to verify — discovered in
  CP-2, escalated to the owner, scope amended.
- Verify task rendered as a frozen 0% bar until an explicit `Progress{fraction:None}`
  was sent (the overlay's `Start` arm defaults to a determinate 0%). Fixed in CP-2.
- Derived `Debug` on the secret-carrying field leaked the raw key transitively
  through `App`'s derived `Debug`; replaced with a masked manual impl in CP-5.

**Deferred items still open:**
- `tui-config-editor-f1-inline-table` — `apply_config_edits` drops sibling keys when
  a section is written as an inline table. `.claude/project/followups/`.
- `tui-config-editor-f4-unreachable-invariant` — `pending_changes` `unreachable!()`
  arm relies on a construction-enforced invariant. `.claude/project/followups/`.
- Follow-on (not filed): wire `meteogate.api_key` into real ORD requests + a live
  verify endpoint, if/when that API is used.
- `ip_fallback` edits persist + update live config but do not hot-restart location
  backends (out of v1 scope; applies on next launch).

### Follow-up batch (uncommitted, awaiting manual review) — 2026-07-22

One /subagent-implementation iteration (reviewer-PASS, 0 findings) resolving the two
config-editor follow-ups above:

- `tui-config-editor-f1-inline-table` — **fixed.** `apply_config_edits` now walks
  intermediate segments via `Item::as_table_like_mut()` (`&mut dyn TableLike`), so an
  intermediate written as an inline table (`location = { ip_fallback = true }`) is
  navigated into rather than clobbered with a fresh `Table::new()`; sibling keys
  survive. Final-segment scalar write switched to `entry(seg).or_insert(Item::None)`
  (byte-equivalent to `Table`'s own `IndexMut`, so the normal `[section]` path is
  unchanged). Regression test `test_apply_config_edits_preserves_inline_table_siblings`
  added — confirmed failing on pre-fix code. `src/config.rs`. Closed 2026-07-22.
- `tui-config-editor-f4-unreachable-invariant` — **moot, closed no-change.** The
  shipped `settings.rs` has no `unreachable!()` and no `pending_changes` fn; the final
  refactor matches exhaustively on `FieldValue` (2 variants). The reviewer finding was
  against an intermediate iteration that never shipped. Closed 2026-07-22.

Per the repo standing rule, nothing was committed — the working-tree diff
(`src/config.rs`, +52/-8) awaits the owner's manual commit.
