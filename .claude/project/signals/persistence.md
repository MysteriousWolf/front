# persistence
## What it does
- Persists app configuration to `~/.config/front/config.toml` and runtime state (viewport, layers) to a TOML state file, both loaded/written via `Config` and `StateConfig` in [`src/config.rs`](../../../src/config.rs).
- Manages on-disk cache directories (map tiles, radar tiles, log file) through `FrontDirs` in [`src/cache.rs`](../../../src/cache.rs).
- `FrontDirs::prune_radar_cache` deletes cached radar files older than a caller-supplied `max_age` duration, recursing into subdirectories.

## Artifacts
- [`src/cache.rs`](../../../src/cache.rs) — `FrontDirs` (config/cache/maps/radar/log path resolution via `directories::ProjectDirs::from("", "", "front")`), `read_if_exists`, `write_atomic` (write-to-`.tmp`-then-rename), `write_log` (timestamped append, safe during raw terminal mode).
- [`src/config.rs`](../../../src/config.rs) — `Config` (top-level app config: `viewport`, `meteogate`, `meteoalarm`, `eumetnet`, `location`, `geocode` sections), `StateConfig` (persisted runtime state: center_lat/lon, zoom, `enabled_layers`, `known_layers`, `selected_layer`, `render_modes`, legacy `braille_layer`/`color_layer`/`text_layer`, `lightning_trail_minutes`, `history_hours`).

## Coupling
- [`src/config.rs`](../../../src/config.rs) imports `LayerId` and `RenderMode` from [`src/layers.rs`](../../../src/layers.rs) (rendering domain) — adding/renaming a `LayerId` or `RenderMode` variant changes what `StateConfig` can serialize/deserialize, and `layers.rs`'s `known_layers`/`LEGACY_KNOWN_LAYERS` handling depends on `StateConfig`'s shape.
- [`src/config.rs`](../../../src/config.rs) imports `EUROPE_LAT`, `EUROPE_LON`, `EUROPE_ZOOM` from [`src/geo.rs`](../../../src/geo.rs) (geo domain) as viewport defaults.
- [`src/config.rs`](../../../src/config.rs) uses `write_atomic` from [`src/cache.rs`](../../../src/cache.rs), so `StateConfig::save` depends on cache.rs's atomic-write implementation.
- `MeteoGateConfig`, `MeteoAlarmConfig`, `EumetnetConfig`, `LocationConfig`, `GeocodeConfig` define endpoint/key fields consumed by the providers domain ([`src/providers/meteogate.rs`](../../../src/providers/meteogate.rs), `meteoalarm.rs`, `eumetnet.rs`, `geocode.rs`, `location/`) — renaming or removing a field there breaks those providers' config reads.
- `EumetnetConfig::hourly_quota` returns `ANON_HOURLY_QUOTA` (50) or `AUTH_HOURLY_QUOTA` (500) based on whether `api_key` is set; the EUMETNET provider budgets requests against this value.

## Conventions worth knowing
- `Config::load` auto-writes a fully-commented default TOML file (`write_default`) when the target path doesn't exist, so the file is always present after first boot and documents every key inline.
- Most `Config` sub-section fields use `#[serde(default = "...")]` per field, with a few (e.g. `MeteoGateConfig::api_key`, `MeteoAlarmConfig::token`, `EumetnetConfig::api_key`) using bare `#[serde(default)]` since empty-string is an acceptable derived default — either way every field defaults independently, so old config files missing an entire new section (e.g. pre-`[location]`, pre-`[geocode]` files) still parse via `Default` impls — covered by `test_config_without_location_section_still_loads` and `test_config_without_geocode_section_still_loads`.
- `StateConfig` keeps legacy scalar fields (`braille_layer`, `color_layer`, `text_layer`) alongside the newer `render_modes: Vec<LayerRenderMode>` for backward-compatible loading; per the doc comment, when `render_modes` is non-empty the legacy scalars are ignored on load (enforced by callers, not by `config.rs` itself, per `test_state_config_legacy_scalar_fields_roundtrip`).
- `known_layers` and `render_modes` use `#[serde(default, skip_serializing_if = "Vec::is_empty")]` so old state files without these fields still deserialize (empty vec) and new files omit the key entirely when empty.
- `write_atomic` (used by `StateConfig::save`) writes to `path.with_extension("tmp")` then renames over the target, avoiding partial/corrupt writes if interrupted.
- `FrontDirs::new` calls `fs::create_dir_all` for config/maps/radar dirs at construction time — directories always exist after a successful `FrontDirs::new()`, never lazily created later.
- `prune_radar_cache` is described in its doc comment as being invoked on boot (per CLAUDE.md: "pruned to 24 hours on boot"), but `cache.rs` itself defines the mechanism only — the `max_age` value and the call site are decided elsewhere (app-core domain).
- `write_log` silently ignores failures to create the parent directory or open the file (`let _ =`), so logging never panics or blocks the raw-mode terminal.
</content>
