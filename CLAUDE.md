# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`front` (Fancy Radar ObservatioN Tool) is a Rust terminal UI application that renders a live weather radar map in the terminal. It uses ratatui for TUI rendering, tokio for async, and crossterm for terminal control. Data comes from three European weather APIs: MeteoGate (radar tiles via S3), MeteoAlarm (weather warnings), and EUMETNET (surface observations via MeteoGate).

## Commands

```bash
cargo build                    # debug build
cargo build --release          # optimised release build
cargo run                      # run with debug build
cargo run -- --lat 46.0 --lon 14.5 --zoom 6.0   # start at specific location
cargo run -- --no-location     # skip GeoClue location lookup
cargo run -- --clear-cache     # wipe on-disk caches and restart

cargo test                     # run all tests
cargo clippy                   # lint
cargo check                    # type-check without building
```

The `mqtt` feature (enabled by default) adds MQTT live-update support for MeteoAlarm. Build without it: `cargo build --no-default-features`.

## Architecture

### Startup flow

`main.rs` → `App::boot()` → `ui::run()`

`App::boot` constructs providers, resolves the initial viewport (CLI args → GeoClue D-Bus → Europe fallback), spawns background tasks for border pre-loading and the radar frame list, then returns the fully initialised `App`. `ui::run` takes over and drives the event loop.

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

### Providers (`src/providers/`)

| File | Purpose |
|---|---|
| `meteogate.rs` | Radar frame list and tile fetch from MeteoGate S3 + ORD REST API. Implements streaming tile delivery (centre-first spiral). |
| `maps.rs` | Natural Earth GeoJSON download and border tile generation. `BorderResolution` (Low110m → Regional10m) selected by zoom level. |
| `meteoalarm.rs` | MeteoAlarm EDR API + optional MQTT live updates. |
| `eumetnet.rs` | EUMETNET surface observations. Fetches in three phases (capitals → major cities → full viewport) sending `PartialCommit` between phases for progressive display. |
| `geoclue.rs` | One-shot GeoClue2 D-Bus location query used at startup. |

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
