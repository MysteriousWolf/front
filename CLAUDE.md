# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Git — standing rules

**No agent-initiated git operations.** The agent must never run `git commit`, `git push`, `git branch`, `git checkout -b`, `git tag`, or any command that creates, modifies, or publishes git state. No PRs may be opened by the agent. All git operations — committing, tagging, pushing, branching, opening PRs — are performed manually by the repo owner after manual review. This applies to every task in this repo, not just individual requests.

### Versioning scheme

Version format: `YY.N` — two-digit year, dot, sequential release number starting at 1 for that year.

| Example | Meaning |
|---|---|
| `26.1` | First release of 2026 |
| `26.5` | Fifth release of 2026 |
| `28.1` | First release of 2028 (resets to 1) |

- `Cargo.toml` `version` field uses this format directly (e.g. `version = "26.1"`).
- Git tags use the prefix `v` followed by the version (e.g. `v26.1`). Tag and `Cargo.toml` must always match.
- The sequential counter resets to 1 on each new calendar year.
- Bumping the version and creating the tag are manual steps only — never automated by the agent.
- CI (`version-guard.yml`) validates the format, year, tag–Cargo.toml match, and uniqueness on every tag push but never creates or modifies versions.

## What this is

`front` (Fancy Radar ObservatioN Tool) is a Rust terminal UI application that renders a live weather radar map in the terminal. It uses ratatui for TUI rendering, tokio for async, and crossterm for terminal control. Data comes from three European weather APIs: MeteoGate (radar tiles via S3), MeteoAlarm (weather warnings), and EUMETNET (surface observations via MeteoGate).

## Commands

```bash
cargo build                    # debug build
cargo build --release          # optimised release build
cargo run                      # run with debug build
cargo run -- --lat 46.0 --lon 14.5 --zoom 6.0   # start at specific location
cargo run -- --no-location     # disable all location acquisition
cargo run -- --clear-cache     # wipe on-disk caches and restart

cargo test                     # run all tests
cargo clippy                   # lint
cargo check                    # type-check without building
```

The `mqtt` feature (enabled by default) adds MQTT live-update support for MeteoAlarm. Build without it: `cargo build --no-default-features`.

## Architecture

### Startup flow

`main.rs` → `App::boot()` → `ui::run()`

`App::boot` constructs providers, resolves the initial viewport (CLI args → first location fix, 2 s timeout → Europe fallback), spawns background tasks for border pre-loading and the radar frame list, then returns the fully initialised `App`. `ui::run` takes over and drives the event loop.

### App struct (`src/app.rs`)

The central state machine. Owns all data (viewport, radar frame, border layers, observation cache, warning layer) and all background task handles. Background work is fully async; results are delivered through unbounded `tokio::sync::mpsc` channels and drained on each event-loop tick by:

- `drain_refresh_results()` — radar tiles and border layer data
- `drain_obs_results()` — observation points (streamed progressively)
- `drain_warning_results()` — MeteoAlarm warning polygons
- `drain_frame_list()` — radar timestamp list
- `drain_task_messages()` — progress overlay updates

Each background task is identified by a monotonic `refresh_id`; stale results (id mismatch) are silently discarded so task cancellation never corrupts state.

### Rendering (`src/ui.rs`)

Pure ratatui rendering; called on every tick. The map area is drawn using braille characters (2×4 dot grid per terminal cell) to maximise spatial resolution. Three mutually exclusive render modes exist: `Braille`, `Color`, and `Text` — each can be assigned to at most one layer at a time (`RenderModeState` in `src/layers.rs`).

Border lines are rasterised into a `BorderMask` (a flat cell grid) which is cached and invalidated only when the viewport or border data changes. The mask supports a `fallback_mask_cache` to avoid a blank flash during resolution transitions.

### Layers (`src/layers.rs`)

`LayerRegistry` manages which layers are visible and their current `LayerStatus` (Loading / Ready / Error). Layers:
- `Radar` — radar reflectivity tiles from MeteoGate S3
- `MapBorders` — Natural Earth GeoJSON country/region/road borders
- `MeteoAlarm` — warning polygons
- `SurfTemp` / `SurfWind` / `SurfHumidity` / `SurfPressure` — EUMETNET surface obs
- `Location` — "you are here" marker (`Text` = red `x`, `Color` = red cell background)
- `SearchPin` — where the `/` search landed; identical rendering in `Rgb8::BLUE`

Most layers are driven by the render-mode system, where "enabled" means "owns a
render mode". `LayerId::is_simple_toggle()` marks the exceptions — the
geographic layers — which use a plain on/off `enabled` flag and expose no
render-mode options.

#### Overlay modes — the exception to "one layer per mode"

`RenderModeState` holds one exclusive primary slot per mode plus an
`overlays: Vec<(RenderMode, LayerId)>` **list**. `overlay_modes(id)` declares
which layers draw as overlays: they render on top of the primary owner instead
of evicting it, and — because the overlays are a list, not one slot per mode —
any number of them can share a mode without evicting *each other*. The location
marker and the search pin both overlay `Text` and must both stay visible.

| Layer | Overlay modes | Why |
|---|---|---|
| `Lightning` | `Braille` | Strike dots coexist with radar braille. |
| `Location` / `SearchPin` | `Text`, `Color` | One annotated cell must not cost the map its temperature readings or radar colour. |

Both pins render through `raster_pin`, differing only in colour and which layer
owns the modes (`LayerId::is_pin()`). The `Text` glyph is nudged to the nearest
free cell (`nearest_free_cell`) so it never blanks a city name or a reading;
the `Color` background only tints, so it stays on the true cell.

`toggle()` and `restore()` route by `overlay_modes`, so callers never pick a
slot themselves. Overlays persist to `state.toml` with the same mode tag as a
primary and are routed back on load by layer identity.

**Adding a layer:** `state.toml` records `known_layers`. On load, layers listed
there are cleared and re-applied from the file (so "off" persists); a layer the
file never knew keeps its constructor default. That is what stops a newly added
layer from silently booting up disabled for existing users —
`LEGACY_KNOWN_LAYERS` covers files written before the field existed.

### Providers (`src/providers/`)

| File | Purpose |
|---|---|
| `meteogate.rs` | Radar frame list and tile fetch from MeteoGate S3 + ORD REST API. Implements streaming tile delivery (centre-first spiral). |
| `maps.rs` | Natural Earth GeoJSON download and border tile generation. `BorderResolution` (Low110m → Regional10m) selected by zoom level. |
| `meteoalarm.rs` | MeteoAlarm EDR API + optional MQTT live updates. |
| `eumetnet.rs` | EUMETNET surface observations. Fetches in three phases (capitals → major cities → full viewport) sending `PartialCommit` between phases for progressive display. |
| `geocode.rs` | Place-name search via OSM Nominatim, backing the `/` prompt. Enforces the service's 1 req/s policy and identifying `User-Agent` internally. `cargo run --example geocode_probe` runs real queries. |
| `location/` | Platform-agnostic location. See below. |

### Location (`src/providers/location/`)

Every backend is an independent task pushing `LocationFix` values into one
shared mpsc channel; `App::drain_location_updates` drains it each tick. A
backend failing (no GeoClue daemon, denied permission, offline) never stops the
others — the app simply falls back to the Europe view.

| Backend | Platform | Notes |
|---|---|---|
| `geoclue.rs` | Linux | Subscribes to the `LocationUpdated` D-Bus signal, so refinements stream in. |
| `windows.rs` | Windows | `Geolocator` + `PositionChanged`. Type-checked only — never run on hardware. |
| `macos.rs` | macOS | `CLLocationManager` on a dedicated thread (CoreLocation needs a run loop). Type-checked only. |
| `ip.rs` | any | Coarse city-level fallback over HTTP. Opt-out via `location.ip_fallback`. |

`LocationArbiter` picks the winner: a fix is accepted when it is strictly more
accurate, refreshes the same source, or the incumbent has gone stale (5 min).
`--lat/--lon` produces a `Manual` fix that nothing can override, and starts no
backend at all. Only the **first** fix moves the viewport — later ones move the
marker only, so a refinement never yanks the map away from a user who has
panned.

`cargo run --example location_probe` prints every fix as it arrives and shows
which one the arbiter picks — useful for checking a backend on a new platform.

### Place search (`/`)

`/` opens a prompt that takes over the footer row. While `App::search_input` is
`Some`, the event loop routes **every** printable key into the buffer before
`keys::resolve` runs — otherwise typing "quit" would quit. Enter geocodes via
`providers::geocode`, jumps the viewport to the hit (min zoom
`SEARCH_MIN_ZOOM`), and turns on `SearchPin`. Esc closes the prompt but leaves
an existing pin alone. Toggling the `SearchPin` layer off is how you clear a
pin — that calls `App::clear_search_pin`, which drops the point rather than
just hiding it.

### City name labels

Capital names are drawn by `raster_capital_names` at the city's own hardcoded
lat/lon, **never** at a nearby weather station. Anchoring them to stations put
names up to 100 km off and made them vanish when the closest station reported no
data; upstream station metadata is also unreliable (Tallinn's nearest station is
named "Abidjan Plateau Mairie"). Readings stay at their stations; the two are
independent. `CITY_MATCH_KM` (100 km) now only gates *visibility* at low zoom.

### Coordinate system (`src/geo.rs`)

All internal geometry uses a normalised [0,1]² "world" space (Mercator projection). `WorldPoint`, `Viewport`, `Bounds`, and `TileCoord` all operate in this space. `lat_lon_to_world` / `world_to_lat_lon` convert to/from WGS-84.

### Persistence (`src/cache.rs`, `src/config.rs`)

- Config: `~/.config/front/config.toml` (auto-generated with defaults on first run)
- State (viewport, enabled layers): `~/.config/front/state.toml`
- Map cache: `~/.cache/front/maps/` — GeoJSON and border tile files per resolution
- Radar cache: `~/.cache/front/radar/` — pruned to 24 hours on boot
- Log: `~/.cache/front/front.log`

### Background task progress overlay

Tasks send `TaskMsg` (Start / Progress / Complete / Error) through `task_tx`. `drain_task_messages` upserts by `TaskKind` so the overlay shows at most one row per kind. Progress bars animate with smoothstep easing over ~0.25 s.
