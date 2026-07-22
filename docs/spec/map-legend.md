# Map legend — spec

## Goal

A bottom-right panel showing the colour → value mapping for every colour-carrying
layer currently on the map, rendered as compact horizontal colour bars and
mirroring the bottom-left layer panel's placement.

## Non-goals

- Interactivity. The legend is not focusable or selectable.
- A legend for geographic layers; border colours are categorical and self-evident.
- Palette changes. This documents the existing ramps.
- Interpolated gradients. Each bar is built from the existing discrete band
  colours laid edge-to-edge; no new in-between colours are synthesised. A legend
  must show the colours the map actually paints, and the map paints discrete
  bands. The bar reads as a near-continuous gradient because each band spans
  several cells, not because colours are blended.

## Success criteria

- [ ] With no colour-carrying layer active, nothing renders and no area is reserved.
- [ ] With radar active, a horizontal reflectivity colour bar appears, built from
      the palette's band colours, low → high running left → right.
- [ ] With an observation property active, its bar appears with the correct unit
      (`°C`, `m/s`, `%`, `hPa`).
- [ ] With both active, the bars stack vertically, each a two-row block.
- [ ] Each scale occupies TWO rows. Row 1 (top) carries the `name / unit` title on
      the left (`Reflect / dBZ`, `Temp / °C`, `Wind / m/s`, `Humid / %`,
      `Press / hPa` — a slash, not parentheses) followed inline by the gradient BAR
      (the colour scale sits on the title's row). Row 2 (bottom) carries the scale
      NUMBERS, aligned under the bar. Blocks left-align in a fixed title column so
      the bars begin at the same x.
- [ ] The numbers are an EVENLY-SPACED subset of the band boundaries — every k-th
      boundary at a uniform stride, so the visible gaps between numbers are regular
      (not an irregular greedy subset). The low-end and high-end markers are always
      shown; the stride is the smallest that keeps labels from touching. Each number
      sits under its own band boundary on the bar above it.
- [ ] Each number is drawn in the colour of the band it marks (its "tick colour"),
      so a number reads as belonging to that point on the gradient above it: the
      low marker in the first band's colour, the high marker in the last band's
      colour, an interior number in the colour of the band whose upper bound it is.
- [ ] The bar is compact — wide enough for the evenly-spaced numbers but not
      dominating the map. It stays a modest fraction of the terminal width.
- [ ] The bar is rendered with sub-character (half-block) glyphs at 2× horizontal
      resolution: each terminal cell carries two band colours (foreground +
      background halves), so band edges fall at half-cell granularity and the bar
      reads as a finer, smoother gradient. Colours are still only the discrete band
      colours (no interpolation of new in-between colours); every band occupies the
      same fixed number of half-cells, so the segments stay uniform.
- [ ] Bar colours and band values come from the shared band tables; `grep` finds
      no second hardcoded copy of the dBZ or observation thresholds or colours.
- [ ] When height cannot fit every bar, whole bars are dropped — no bar renders
      partially. The radar (dBZ) bar is kept and observation bars are dropped
      first (fixed priority). When even one bar will not fit, draw nothing.
- [ ] Panel x/y match `layer_area`'s convention reflected horizontally: two
      columns of inset from the right edge, one row of bottom padding, same
      baseline as the layer panel. The legend never draws outside the map area
      or over the footer.

## Approach

One two-row colour-bar block per active scale, stacked, bounded by render-mode
ownership. Row 1 is a `name / unit` title plus fraction-positioned boundary
numbers; row 2 is a sub-character (half-block)
gradient bar at 2× horizontal resolution using the discrete band colours from the
gradient bar of discrete band colours, ~2× widened so the top row's boundary
numbers fit — see `docs/design/map-legend.md`.

## Checkpoints

| # | Checkpoint | Files/areas | Agent | Est. files | Verifies |
|---|------------|-------------|-------|------------|----------|
| 1 | Extract the dBZ and observation colour bands into shared band tables; make `dbz_to_color` and `obs_color` read them | `src/providers/meteogate.rs`, `src/ui.rs` | atomic-implementer (mode: feature) | ~2 | Existing colour-threshold tests pass unchanged against the table-driven versions |
| 2 | Add `legend_area` mirroring `layer_area`, and a pure function mapping render-mode state to the ordered list of active scales | `src/ui.rs` | atomic-implementer (mode: surgical) | 1 | Unit tests: placement mirrors the layer panel; scale list follows mode ownership |
| 3 | Render the stacked two-row colour-bar blocks: a top row with the `name / unit` title and fraction-positioned boundary numbers (min-gap collision drop, low/high kept), a bottom-row sub-character (half-block) 2×-widened gradient bar of discrete band colours, and height-driven whole-block degradation dropping observation bars before the radar bar | `src/ui.rs` | atomic-implementer (mode: feature) | 1-2 | Tests: none active renders nothing; both active stack as two-row blocks; half-block bar carries two band colours per boundary cell; segments uniform; boundary numbers fraction-positioned and never collide; short terminal drops whole blocks **keeping dBZ, dropping obs bars first**; never overdraws the footer |

## Risks

| Risk | Likelihood | Mitigation |
|------|-----------|-----------|
| Extracting bands changes rendered colours subtly | medium | Checkpoint 1 ships with the existing threshold tests unchanged as the guard; it is a refactor with no behaviour change |
| Interior labels collide/merge on a narrow bar | medium | Only start/end are labelled, placed beside the bar (not over it); no interior labels to collide — asserted in a test |
| Legend overlaps the task-progress overlay, which also occupies the right side | medium | Task overlay is top-right (`render_task_queue`), legend is bottom-right; assert non-overlap in a test at small terminal sizes |
| Legend obscures map content in the bottom-right corner | low | Same tradeoff the layer panel already makes on the left; the bars are short (a few rows), leaving most of the corner visible |

## Change log

### 2026-07-22 — Legend reshaped to horizontal colour bars; fixed-priority degradation

**What changed:** Each active scale renders as a compact horizontal colour bar —
cell background colours read from the discrete band tables and laid edge-to-edge,
reading as a near-continuous gradient — carrying a scale name and sparse value
labels (start, end, a few significant interior values). All bars share one fixed
width and align into a column. This replaces the vertical stack of
one-row-per-band blocks. Degradation now keeps the radar (dBZ) bar and drops
observation bars first (fixed priority), replacing least-recently-activated-first
dropping.

**Why:** User visual-design decision at the CP-2 → CP-3 gate (2026-07-22): a
horizontal bar reads better and, being only a few rows tall, largely removes the
height-budget pressure that motivated recency-based dropping. No
activation-recency state exists in the code (`RenderModeState` tracks no
activation order for primary mode slots), so a fixed priority is both simpler and
sufficient for the realistic ≤2-scale case.

**Superseded:** Prior contract stacked one banded row per boundary vertically and
dropped the least-recently-activated whole block first. The dBZ-granularity and
unit-header open questions in the design are resolved by this reshape (the full
scale is shown as one bar; the scale name plus sparse labels replace a per-band
row and a separate header row).

### 2026-07-22 — Single-row bars, quantity+unit names, endpoints-only labels

**What changed:** Each scale now occupies exactly one row instead of two (bar row
+ label row). The left column shows the QUANTITY name with its unit —
`Reflectivity (dBZ)`, `Temperature (°C)`, `Wind (m/s)`, `Humidity (%)`,
`Pressure (hPa)` — replacing the bare unit-as-name (`dBZ`, `Temp`). The bar is a
run of contiguous background-colour cells with a UNIFORM fixed segment width per
band (no remainder distribution, so segments are visually equal). Only the start
and end values are labelled, placed immediately left and right of the bar on the
same row; interior labels are removed.

**Why:** User review of the rendered legend (2026-07-22): the two-row form with
fraction-positioned interior labels produced colliding/merged labels (`560+` from
`55`+`60+`, `2030+` from `20`+`30+`) and unequal segment widths, and `dBZ` named
the unit rather than the quantity. The user asked for a single line per scale with
the unit beside the quantity name and the gradient (contiguous background colours)
carrying the middle.

**Superseded:** The prior contract used two rows per scale (bar + a sparse
label line with start/end plus a few interior values positioned by fraction),
a fixed total bar width with remainder-distributed segment widths, and the unit
as the scale name.

### 2026-07-22 — Two-row blocks, sub-character gradient, `name / unit` headers

**What changed:** Reverted from one row per scale to two: row 1 is a header showing
the abbreviated quantity and unit as `name / unit` (`Reflect / dBZ`, `Temp / °C`,
`Wind / m/s`, `Humid / %`, `Press / hPa`) — a slash, not parentheses, matching the
scientific "quantity / unit" axis convention — and row 2 is the start value, the
bar, and the end value. The bar is now drawn with sub-character half-block glyphs
at 2× horizontal resolution (two band colours per terminal cell via fg/bg halves)
so band edges fall at half-cell granularity and the bar reads as a finer gradient.
Colours remain the discrete band colours (no interpolation).

**Why:** User review of the single-row render (2026-07-22): with no inline text on
the bar, the extra vertical row is cheap and gives the numeric scale more room;
half-block sub-characters make the gradient look smoother than chunky full-cell
segments; and `name / unit` (abbreviated, slash-separated) is the scientifically
correct axis label form rather than `Name (unit)`.

**Superseded:** The prior contract used one row per scale, a contiguous full-cell
background-colour bar with uniform whole-cell segments, and `Name (unit)` labels
with the full quantity name in parentheses.

### 2026-07-22 — Numbers moved to the top row, bar widened ~2×

**What changed:** Row 1 now carries the `name / unit` title AND the scale numbers —
band-boundary values spread across, each positioned by fraction above its boundary
on the bar below (low/high markers always kept, collisions dropped by a minimum-gap
rule). Row 2 is the gradient bar alone. The bar is widened roughly 2× so several
numbers fit. The previous form put only the start/end values inline flanking the
bar; those inline endpoints are removed in favour of the top-row number scale.

**Why:** User review of the two-row render (2026-07-22): they wanted the numeric
scale (several numbers, not just endpoints) on one line with the title, the pure
gradient beneath it, and enough width to fit the numbers without collision.

**Superseded:** The prior contract's row 2 held `start value + bar + end value`
inline; numbers are now on row 1 above the bar and there is no inline start/end.

### 2026-07-22 — Evenly-spaced numbers, narrower bar

**What changed:** The top-row numbers are now an evenly-spaced subset of the band
boundaries (every k-th boundary at a uniform stride, low/high always shown) instead
of a greedy minimum-gap subset, so the gaps between numbers are regular. The bar is
narrowed (fewer half-cells per band) so the legend no longer dominates the map.

**Why:** User review of the rendered legend (2026-07-22): the greedy min-gap number
selection produced irregular, "weirdly placed" spacing, and the ~2× bar was too wide.

**Superseded:** The prior contract widened the bar ~2× and dropped colliding numbers
greedily by minimum gap (irregular subset).

### 2026-07-22 — Bar inline with title, numbers below in tick colour

**What changed:** Swapped the two rows. Row 1 now holds the `name / unit` title AND
the gradient bar inline (the colour scale sits on the title's row); row 2 holds the
numbers, aligned under the bar. Each number is drawn in the colour of the band it
marks (low → first band, high → last band, interior → the band whose upper bound the
number is), so a number visibly belongs to its point on the gradient above it.

**Why:** User review (2026-07-22): with the numbers above a separate bar it was hard
to tell which tick a number belonged to; colouring each number as its band and
placing the colour scale inline with the title makes the association obvious.

**Superseded:** The prior contract put the numbers on the top row (above the bar)
and the gradient bar alone on the bottom row, with numbers in a single dim colour.

## Implementation log

### Shipped (uncommitted — manual review pending) — 2026-07-22

Built across 4 iterations of /subagent-implementation. Per the repo standing rule
(no agent git operations) every checkpoint is left uncommitted in the working
tree for the owner to review and commit. Not bisectable by SHA — the changes land
as one working-tree diff over base `634c6ce`.

- CP-1 — extract dBZ + observation colour bands into shared enumerable tables
  (`DBZ_BANDS`/`DBZ_UNIT` in `providers/meteogate.rs`; `Obs*_BANDS` + `ObsScale` +
  `obs_scale()` in `ui.rs`); `dbz_to_color`/`obs_color` made table-driven. Pure
  refactor, verified byte-identical to base.
- CP-2 — `legend_area()` (bottom-right mirror of `layer_area`) + `LegendScale` +
  pure `active_scales()` (mode-ownership → ordered scale list, dBZ first).
- CP-3 — `render_legend()` drawing horizontal colour bars (cell-background band
  colours, no interpolation), fixed equal bar width (`band_cell_widths`), sparse
  fraction-positioned labels (`legend_labels`), fixed-priority whole-bar
  degradation (`fitting_scales`, keeps dBZ / drops obs first).
- CP-3 fix — `task_queue_reserved_rows()` + a `reserved_top_rows` budget on
  `render_legend` so the bottom-right legend never overlaps the top-right task
  overlay on a short terminal (TDD-confirmed real collision, now guarded + tested).
- CP-3 rework 1 (post-render user review) — single row, quantity name + unit,
  uniform segments, endpoints-only. Second 2026-07-22 change-log entry. 417 tests.
- CP-3 rework 2 — two rows, `▌` half-block sub-character gradient (reusing the radar
  timeline idiom), `name / unit` abbreviated headers. Third change-log entry. 420 tests.
- CP-3 rework 3 — numbers moved to the top row beside the title, gradient alone on
  the bottom row, bar widened, min-gap boundary numbers. Fourth change-log entry.
- CP-3 rework 4 (final) — evenly-spaced (uniform-stride) top-row numbers replacing
  the irregular greedy subset, and a narrower bar (`LEGEND_HALF_CELLS_PER_BAND` 7→3).
  Fifth change-log entry. 422 tests. CLAUDE.md Rendering blurb reflects the final form.

**Out-of-scope work performed during this build:**
- `cargo fmt` applied at the finalize gate — the three checkpoint reviewers ran
  clippy/test/build but not `fmt --check`, so rustfmt reflowed the new band-table
  struct literals. Whitespace only, no behaviour change.

**Unforeseens — surprises that emerged during implementation:**
- The spec's original "least-recently-activated first" degradation was
  unimplementable: `RenderModeState` tracks no activation recency for primary mode
  slots. Surfaced to the user, who reshaped the whole legend to horizontal colour
  bars with fixed-priority degradation (see the 2026-07-22 change-log entry).
- Two transient LSP diagnostics (`E0599 for_test`, `E0425 test_task`) appeared from
  intermediate implementer edit states; neither is in the final tree (`cargo test`
  compiles clean, 415 passing).

**Deferred items still open:** none. Both review nits were fixed in a polish pass:
- F-1 (🔵) — closed: extracted a shared `const MAX_VISIBLE_TASKS = 8` read by both
  `task_queue_reserved_rows` and `render_task_queue`.
- F-2 (🔵) — closed: a single `Instant::now()` per tick is now threaded into both
  `task_queue_reserved_rows` and `render_task_queue` (the latter gained a `now`
  parameter), eliminating the nanosecond-window under-count.
