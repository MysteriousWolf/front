# geo
## What it does
- [`src/geo.rs`](../../../src/geo.rs) defines the normalised `[0,1]²` "world" coordinate space (a Web-Mercator-style projection), the `GeoPoint` / `WorldPoint` / `Viewport` / `Bounds` / `TileCoord` types, and conversions between lat/lon and world space (`lat_lon_to_world`, `world_to_lat_lon`).
- It also holds static reference data: `EUROPEAN_CAPITALS` / `EUROPEAN_CAPITAL_NAMES` (45 entries, same order) and `EUROPEAN_MAJOR_CITIES` (68 entries), plus `haversine_m` (great-circle distance) and `near_european_capital` (proximity test against `CITY_MATCH_KM` = 100.0 km).
- Zoom is clamped to `[MIN_VIEW_ZOOM, MAX_VIEW_ZOOM]` = `[1.0, 12.0]`; latitude is clamped to `±MAX_LAT` = `85.05112878` (standard Web Mercator cutoff); default view is `EUROPE_LAT/EUROPE_LON/EUROPE_ZOOM` = `46.05, 14.51, 4.0`.

## Artifacts
None.

## CLI code
None.

## Docs
- [`CLAUDE.md`](../../../CLAUDE.md) — "Coordinate system ([`src/geo.rs`](../../../src/geo.rs))" section states all internal geometry uses the normalised `[0,1]²` world space and names `WorldPoint`, `Viewport`, `Bounds`, `TileCoord`, `lat_lon_to_world`, `world_to_lat_lon`.
- [`CLAUDE.md`](../../../CLAUDE.md) — "City name labels" section documents that capital names are drawn at hardcoded lat/lon from this module's `EUROPEAN_CAPITALS`/`EUROPEAN_CAPITAL_NAMES`, never at a station location, and that `CITY_MATCH_KM` only gates visibility, not naming.

## Coupling
- `Viewport::bounds`, `world_at_screen`, `zoom_around_screen`, and the tile functions (`visible_tiles`, `tiles_spiral_from`, `tile_bounds`, `tile_pixel_to_world`, `world_to_tile_pixel`) are consumed by rendering ([`src/ui.rs`](../../../src/ui.rs)) and by [`src/app.rs`](../../../src/app.rs) for viewport state — changing `Bounds`/`WorldPoint` field layout or the span/zoom formulas forces changes in rendering (border rasterisation, braille grid math) and app-core (viewport persistence, pan/zoom handlers).
- `tile_for_world`, `tile_bounds`, `visible_tiles`, and `tiles_spiral_from` define the tile addressing scheme consumed by providers ([`src/providers/meteogate.rs`](../../../src/providers/meteogate.rs) radar tiles, [`src/providers/maps.rs`](../../../src/providers/maps.rs) border tiles) — changing tile math forces changes in providers.
- `EUROPEAN_CAPITALS`, `EUROPEAN_CAPITAL_NAMES`, and `near_european_capital` are consumed by rendering's capital-name raster and by providers ([`src/providers/eumetnet.rs`](../../../src/providers/eumetnet.rs) phased fetch: capitals → major cities → full viewport) — changing the capitals list or `CITY_MATCH_KM` forces changes in both rendering and providers.
- `GeoPoint`, `WorldPoint`, and `Bounds` derive `Serialize`/`Deserialize`, but the persisted viewport in `state.toml`/`config.toml` stores plain `lat`/`lon`/`zoom: f64` fields (`src/app.rs::save_state`, `src/config.rs::ViewportConfig`) rather than a serialized `WorldPoint`/`GeoPoint`/`Bounds` — `world_to_lat_lon` is the conversion point between this module's types and the persisted plain fields.
- `--lat`/`--lon` CLI args (location domain) construct a `Viewport` via `Viewport::from_lat_lon`, coupling this module to location's manual-fix path.

## Conventions worth knowing
- World-space `y` increases southward (Mercator north-up convention): `y=0` is the north pole boundary (`MAX_LAT`), `y=1` is the south pole boundary.
- `Viewport::centre_range` (private) computes clamped center bounds so viewports at high zoom (small span) cannot pan the center past `[span, 1-span]`, while low zoom (span ≥ 0.5) allows the full `[0,1]` range — pan/zoom methods call this before and after mutating `center` to avoid directional jumps from out-of-range saved state.
- `Bounds::expanded(fraction)` inflates a bounds rectangle centered on itself, clamped to `[0,1]²`, used for prefetch margins.
- Tile addressing follows the standard slippy-map scheme: `TileCoord { z, x, y }`, `2^z` tiles per axis, `tile_bounds`/`tile_for_world` convert between tile index and world-space fractions.
- `tiles_spiral_from` returns tiles ordered center-first in clockwise concentric rings (not row-major), so consumers relying on tile order for progressive/streamed rendering get nearest-to-center tiles first.
- The module has 20 unit tests (`#[cfg(test)] mod tests`) covering roundtrip conversion, clamping, bounds containment/intersection, tile math, spiral ordering, and cursor-anchored zoom.
