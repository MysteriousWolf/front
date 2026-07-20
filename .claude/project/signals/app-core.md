# app-core
## What it does
- `main.rs` builds a manually-configured multi-thread tokio runtime (not `#[tokio::main]`) so `tune_allocator()` runs before any worker thread is spawned, then calls `App::boot(&cli).await` and hands the result to `front::ui::run(app)`.
- `App` ([`src/app.rs`](../../../src/app.rs)) is a single struct holding all app state: viewport, layer registry, radar/border/observation/warning caches, and one `UnboundedSender`/`UnboundedReceiver` pair plus an `Option<JoinHandle<()>>` per background task family (radar refresh, border refresh, obs refresh, warning refresh, radar preload, search, location, location-label, lightning, frame list, task-progress).
- Every refresh family carries its own monotonic `*_refresh_id: u64`; each `drain_*` method discards any received result whose `id` does not match the current counter, so a superseded/aborted task's late result cannot corrupt state.
- `keys.rs` declares every keybinding once in a static `BINDINGS: &[Binding]` table; `keys::resolve(KeyEvent) -> Option<Action>` is the sole lookup function, used by the event loop in [`src/ui.rs`](../../../src/ui.rs).
- `cli.rs` defines `Cli` (clap `Parser`) with fields `lat`, `lon`, `zoom` (all `Option<f64>`), `no_location: bool`, and `clear_cache: bool`.

## CLI code
- [`src/main.rs`](../../../src/main.rs) — process entry point; builds the tokio runtime by hand, calls `tune_allocator()` (Linux/glibc only, gated by `#[cfg(target_env = "gnu")]`) which sets `M_MMAP_THRESHOLD` to 128 KiB and `M_ARENA_MAX` to 2 via `libc::mallopt`, then runs `App::boot` and `ui::run`.
- [`src/cli.rs`](../../../src/cli.rs) — `Cli` struct parsed with `clap::Parser`; no subcommands, only top-level flags (`--lat`, `--lon`, `--zoom`, `--no-location`, `--clear-cache`).

## Docs
- CLAUDE.md documents the startup flow (`main.rs` → `App::boot()` → `ui::run()`), the `App` struct's channel/drain architecture, and the [`/`](../../..) search-prompt key-routing takeover.

## Coupling
- `App::boot` constructs `MeteoGateProvider`, `MeteoAlarmProvider`, `EumetnetProvider`, and `NaturalEarthProvider` directly (`src/providers/*.rs`) — provider constructor signature changes force edits in `App::boot`.
- `App` methods reference `LayerRegistry`, `LayerId`, `LayerStatus`, `RenderMode`, and `BorderLayer`/`BorderLineKind` from [`src/layers.rs`](../../../src/layers.rs) throughout (`set_status`, `mode_state_mut`, `enabled`, `overlay_modes` indirectly via save/load) — layer-registry API changes ripple into `App`'s drain/request methods.
- `App::save_state` / `App::load_state` serialize to/from `StateConfig` in [`src/config.rs`](../../../src/config.rs) (`~/.config/front/state.toml`); adding a field to `App` that should persist requires a matching `StateConfig` field and load/save wiring.
- `App` methods use `Viewport`, `Bounds`, `WorldPoint`, `GeoPoint`, `world_to_lat_lon`, `haversine_m` from [`src/geo.rs`](../../../src/geo.rs) for viewport bounds, drag/search jumps, and label-refresh distance checks — geo API changes affect `App::request_meteogate_refresh`, `submit_search`, `request_location_label`, etc.
- `App` holds `location: LocationArbiter` and an `Option<UnboundedReceiver<LocationFix>>` from [`src/providers/location/`](../../../src/providers/location); `initial_viewport()` (in `app.rs`) spawns the location backend stream and races it against `INITIAL_FIX_TIMEOUT` (2 s) before falling back to the Europe viewport — changes to `LocationArbiter::offer`/`current` affect both boot and `drain_location_updates`.
- [`src/ui.rs`](../../../src/ui.rs)'s event loop is the sole caller of `keys::resolve`; when `app.search_is_open()` is true, `ui.rs` routes every printable key press into `app.search_push_char`/`search_backspace`/`submit_search`/`cancel_search` *before* `keys::resolve` runs, so a new `Action` variant does not by itself change key routing while the prompt is open (Ctrl+C is special-cased to still quit).
- `Action::OpenSearch`, `ToggleLayer`, `ModeBraille`/`ModeColor`/`ModeText`, `SelectPrevious`/`SelectNext`, `EnterGroup`/`ExitGroup` resolved by `keys::resolve` are matched and handled inside [`src/ui.rs`](../../../src/ui.rs)'s event loop, not in `app.rs` — adding an `Action` variant to `keys.rs` requires a corresponding match arm in `ui.rs`.

## Conventions worth knowing
- Background work follows one repeated pattern across all five refresh families (radar, border, obs, warning, search): bump `*_refresh_id`, clone provider/channel handles, `tokio::spawn`, send results back through an `UnboundedSender`, and drain on the next tick via a dedicated `drain_*` method that filters on `id`.
- `App::boot` sends `TaskMsg::Start`/`Progress`/`Complete`/`Error` through `task_tx` for every long-running fetch (frame list, radar frame, border download/tile-gen, warnings, observations); `drain_task_messages` upserts `active_tasks` by `TaskKind` so the overlay shows at most one row per kind, with smoothstep-eased `display_fraction` animation and pruning after completed rows have been visible 1 s.
- `keys::normalize` strips terminal state bits (KEYPAD, NUM_LOCK) and drops Shift from character-key chords (since terminals already fold Shift into the character), while Shift is preserved for non-character keys like arrows.
- `keys.rs` has a `#[cfg(test)]` module asserting invariants about the table itself: every footer-hint rank is unique, every chord resolves to exactly one action, every `Category` has at least one help row, and documentation-only rows (`action: None`) carry no chords.
- `App::load_state` treats `state.toml`'s `known_layers` as authoritative for any layer it lists: those layers' modes are cleared and rebuilt from `render_modes` before applying, so a layer switched off in a previous session stays off; a layer the saved file never knew about keeps its constructor default (see `LEGACY_KNOWN_LAYERS` handling referenced in CLAUDE.md).
- `--lat`/`--lon` on the CLI short-circuits `initial_viewport`: it produces a `LocationSource::Manual` fix and returns `None` for the location receiver, so no location backend task is ever spawned.
- Radar refresh, obs refresh, and warning refresh methods all check `self.layers.enabled(LayerId::X)` and bail early with no-op if the corresponding layer is off, before touching any channel or task state.
