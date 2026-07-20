# Map legend — spec

## Goal

A bottom-right panel showing the colour → value mapping for every colour-carrying
layer currently on the map, mirroring the bottom-left layer panel's placement.

## Non-goals

- Interactivity. The legend is not focusable or selectable.
- A legend for geographic layers; border colours are categorical and self-evident.
- Palette changes. This documents the existing ramps.
- Continuous gradients; the ramps are banded and a terminal cell is the smallest
  available unit.

## Success criteria

- [ ] With no colour-carrying layer active, nothing renders and no area is reserved.
- [ ] With radar active, a dBZ scale appears showing the palette's band boundaries.
- [ ] With an observation property active, its scale appears with correct units
      (`°C`, `m/s`, `%`, `hPa`).
- [ ] With both active, both blocks stack.
- [ ] Panel x/y match `layer_area`'s convention reflected horizontally: two
      columns of inset from the right edge, one row of bottom padding, same
      baseline as the layer panel.
- [ ] When height cannot fit every block, whole blocks are dropped — no block
      renders partially — and they drop **least-recently-activated scale first**,
      so the scale the user just switched on is the one that survives.
- [ ] The legend never draws outside the map area or over the footer.
- [ ] Band boundaries come from one shared source; `grep` finds no second
      hardcoded copy of the dBZ or observation thresholds.

## Approach

Stack one block per active scale, bounded by render-mode ownership, with band
data extracted so the renderer and the legend read the same table — see
`docs/design/map-legend.md`.

## Checkpoints

| # | Checkpoint | Files/areas | Agent | Est. files | Verifies |
|---|------------|-------------|-------|------------|----------|
| 1 | Extract the dBZ and observation colour bands into shared band tables; make `dbz_to_color` and `obs_color` read them | `src/providers/meteogate.rs`, `src/ui.rs` | atomic-implementer (mode: feature) | ~2 | Existing colour-threshold tests pass unchanged against the table-driven versions |
| 2 | Add `legend_area` mirroring `layer_area`, and a pure function mapping render-mode state to the ordered list of active scales | `src/ui.rs` | atomic-implementer (mode: surgical) | 1 | Unit tests: placement mirrors the layer panel; scale list follows mode ownership |
| 3 | Render the stacked legend blocks with height-driven whole-block degradation, dropping least-recently-activated first | `src/ui.rs` | atomic-implementer (mode: feature) | 1-2 | Tests: none active renders nothing; both active stacks; short terminal drops whole blocks **and drops the older scale first**; never overdraws the footer |

## Risks

| Risk | Likelihood | Mitigation |
|------|-----------|-----------|
| Extracting bands changes rendered colours subtly | medium | Checkpoint 1 ships with the existing threshold tests unchanged as the guard; it is a refactor with no behaviour change |
| The eleven-band dBZ block is too tall beside a five-band observation block | medium | Open question in the design — resolve by rendering both variants and choosing, before checkpoint 3 |
| Legend overlaps the task-progress overlay, which also occupies the right side | medium | Task overlay is top-right (`render_task_queue`), legend is bottom-right; assert non-overlap in a test at small terminal sizes |
| Legend obscures map content in the bottom-right corner | low | Same tradeoff the layer panel already makes on the left; render each line at its own width so the map shows through trailing cells |

## Change log
