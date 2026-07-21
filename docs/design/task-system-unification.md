# Task system unification — design

## The ask

Two related things:

1. Every background activity should report through the task system, so the
   top-right overlay shows progress for *everything* rather than a subset.
2. The scheduling patterns those activities need should live in the task system
   too, instead of being hand-rolled per provider.

## Where things stand

### The overlay covers roughly half the background work

`TaskMsg` (`app.rs:2832`) is sent by seven call sites. Verified by reading every
`tokio::spawn` in `app.rs` and `providers/location/mod.rs`:

| Reports progress | Silent |
|---|---|
| frame list, radar refresh, border download, border tile gen, warnings, observations, radar preload (partial) | `location_label_task` (`app.rs:1597`), `search_task` (`:1699`), `lightning_task` (`:1957`), `radar_preload_task` (`:2293`), all four location backends (`location/mod.rs:180-213`) |

So a user pressing `/` and waiting on a geocode, or waiting for a location fix,
gets no indication anything is happening. Those are exactly the cases where the
app looks frozen, because they are user-initiated and slow.

### Rendering constraint

`render_task_queue` (`ui.rs:3305`) upserts by `TaskKind` — at most one row per
kind — and caps at `max_visible = 8`. There are 6 kinds today. Adding one kind
per silent source would reach 10-11 and start truncating. **The cap is a real
design input, not an implementation detail:** either kinds get grouped, or the
overlay needs a policy for what to drop.

### Three copies of exponential backoff

| Location | Shape |
|---|---|
| `providers/location/ip.rs:52-68` | `backoff = (backoff * 2).min(MAX_RETRY_INTERVAL)`, reset on success |
| `providers/meteogate.rs:653` | `delay = (delay * 2).min(MAX_RETRY_AFTER)`, fixed attempt count |
| `app.rs` `FrameRetry` (`:87`, `:98-103`) | `attempts` + `next_at`, base 2 s ceiling 90 s |

Same algorithm, three implementations, three sets of constants, three sets of
bugs-if-any. Alongside these sit ~20 free-standing `Duration` constants for
intervals and TTLs spread across six files.

This is the part of the ask with hard evidence behind it. The coverage gap is a
feature request; the backoff duplication is existing debt.

## Scope decision

The user chose: **consolidate what already exists** — one retry/backoff policy
plus fixed-interval polling. Not a general scheduler.

That is the right call, and worth writing down *why*, because the pull toward a
richer abstraction here is strong:

- Every scheduling mode built is one already in use at three call sites. Nothing
  is designed against a hypothetical caller.
- A general scheduler's unused modes are untested by construction. The three
  backoff copies at least run.
- The TTL constants (`VIEWPORT_DATA_TTL`, `CAPITAL_DATA_TTL`, `STATION_LIST_TTL`,
  …) are *cache* lifetimes, not schedules. Folding them into a scheduler
  conflates "how long is this datum good for" with "when do I next run" — those
  diverge as soon as one cache is shared by two tasks.

Long-lived streams (lightning WS, GeoClue D-Bus) are explicitly **out** of this
pass. They need supervision and restart semantics, which is a different problem
from "retry this fallible one-shot", and merging them early would produce a
policy type with mutually-exclusive fields.

## Approaches considered

### A — Report-only, leave scheduling alone

Add `TaskMsg` emission to the silent sources; keep the three backoff copies.

Delivers the visible half of the ask immediately, and is nearly risk-free. But
it leaves the debt, and the user asked for both.

### B — One `RetryPolicy` value type, adopted by the three sites *(chosen)*

A small policy struct describing base delay, ceiling, and giving-up rule, plus a
helper that computes the next delay. Each of the three sites keeps its own
constants but stops implementing the arithmetic. Task reporting is added
alongside.

Chosen because it is the smallest change that removes the duplication without
inventing a scheduler. The policy is a *value*, not a runtime — callers still own
their loops, so nothing has to be restructured around a new executor.

### C — A scheduler owning the tasks

A central runtime that owns handles, drives intervals, and reports. Rejected:
`App` already owns task handles and cancellation via `refresh_id`, and inserting
a scheduler underneath means rewriting working cancellation logic for no user
benefit. Revisit only if a third async source needs coordination the current
`refresh_id` scheme cannot express.

## Resolved questions

### 1. Overlay capacity — group, then prioritise

Grouping first: the four location backends race to produce *one* answer, so they
share a single `Location` kind. The geocode search and the reverse-geocode label
are both Nominatim round-trips and share a `Geocode` kind. `radar_preload_task`
reuses the existing `RadarFrame` kind.

That yields 9 kinds against `max_visible = 8`:

> border · tiles · radar · obs · warn · frames · location · geocode · lightning

Still one over, and raising the cap is the wrong reflex — the overlay sits in
the top-right corner over the map, and a 9-row panel starts eating the view it
is annotating.

So: keep the cap and make the drop **deterministic and deliberate** rather than
"whatever `[..n]` happens to slice". Sort before truncating:

1. `Running` before terminal states (`Completed`/`Error`/`Superseded`)
2. user-initiated (`Geocode`, `Location`) before background
3. oldest start first, so rows do not reshuffle as fractions update

A finished row is the one that disappears when space runs out, which is the
correct answer — it has already told the user what it had to say. In practice
all 9 are rarely live at once; boot is 6.

Rejected: raising `max_visible`. Rejected: dropping by arrival order, which
would hide a running task behind a completed one.

### 2. Visibility threshold — 150 ms

A task renders only once it has been running for 150 ms. Below ~100 ms a change
reads as instantaneous, so a row that appears and vanishes inside that window is
pure flicker; 150 ms clears it with margin while staying well under the
threshold where a user starts wondering whether anything happened.

`Error` is exempt — a failure always renders regardless of how fast it arrived.
A geocode that 400s in 30 ms must still say so.

This is what makes instrumenting the fast paths safe: a warm-cache geocode stays
invisible, a slow one appears.

### 3. Indeterminate progress — `Option<f64>`, no faked fraction

`TaskMsg::Progress.fraction` and `ActiveTask.fraction` become `Option<f64>`.
`None` renders as an animated marquee rather than a filled bar.

A geocode is one request: it is 0 % until it is 100 %, and any intermediate
number is a lie. Faking `0.5` would make the bar assert progress that is not
being measured — the overlay's whole value is that its bars mean something.

Cost: existing `Progress` senders wrap their value in `Some`. Mechanical, and
the compiler finds every site.

Rejected: a separate `Pulse` variant, which would leave two ways to say
"working" and force the renderer to handle both.
