# providers
## What it does
- [`src/providers/`](../../../src/providers) holds one module per external weather/geodata API: `meteogate.rs` (radar), `maps.rs` (borders), `meteoalarm.rs` (warnings), `eumetnet.rs` (surface obs), `geocode.rs` (place search), `lightning.rs` (strike stream, feature-gated).
- [`src/providers/mod.rs`](../../../src/providers/mod.rs) declares seven submodules: `eumetnet`, `geocode`, `lightning` (behind `#[cfg(feature = "lightning")]`), `location`, `maps`, `meteoalarm`, `meteogate`.
- Every provider struct is constructed once in `App::boot` ([`src/app.rs`](../../../src/app.rs)) and held on the `App` struct (`maps`, `meteogate`, `meteoalarm`, `eumetnet`, `geocode: Arc<GeocodeProvider>`).

## Artifacts
None.

## CLI code
- [`examples/geocode_probe.rs`](../../../examples/geocode_probe.rs) — manual probe that runs real Nominatim queries via `GeocodeProvider` and prints results plus elapsed time, exercising the 1 req/s throttle end to end. Run with `cargo run --example geocode_probe -- "Ljubljana" "Mount Fuji"`.

## Docs
- [`CLAUDE.md`](../../../CLAUDE.md) — "Providers ([`src/providers/`](../../../src/providers))" table names each file's purpose (meteogate.rs, maps.rs, meteoalarm.rs, eumetnet.rs, geocode.rs) matching the module split verified above.
- [`CLAUDE.md`](../../../CLAUDE.md) — mentions "MQTT live-update support for MeteoAlarm" gated by the `mqtt` Cargo feature; no `rumqttc` usage or `mqtt` cfg was found anywhere under [`src/`](../../../src) (see Concerns).

## Coupling
- `meteogate.rs`, `maps.rs`, `meteoalarm.rs`, `eumetnet.rs` all import types from `crate::layers` (`RadarFrame`/`RadarTile`/`RadarRun`/`Rgb8`, `BorderLayer`/`BorderLine`/`BorderLineKind`/`BorderResolution`/`SpatialGrid`, `WarningFeature`/`WarningLayer`, `ObservationLayer`/`ObservationPoint`) — changing those shapes forces changes here (rendering/layers domain).
- `eumetnet.rs` imports `EUROPEAN_CAPITALS`, `EUROPEAN_MAJOR_CITIES`, `EUROPE_LAT`, `EUROPE_LON`, `Bounds`, `GeoPoint`, `WorldPoint`, `lat_lon_to_world`, `world_to_lat_lon` from `crate::geo`; `maps.rs` and `lightning.rs` use `WorldPoint`/`GeoPoint`/`lat_lon_to_world` too — the geo domain's coordinate types are a shared dependency.
- `geocode.rs` returns `Place { point: GeoPoint, display_name: String }` consumed by the [`/`](../../..) search flow in `app.rs`/`ui.rs`.
- `lightning.rs::connect_and_stream` is spawned directly from `src/app.rs:1958`; it pushes `(WorldPoint, i8)` pairs (and a `CONNECTED_SENTINEL`) into an mpsc channel that `app.rs` drains.
- `meteogate.rs::frame_streamed` and `eumetnet.rs::fetch_observations` both take an `UnboundedSender` the caller in `app.rs` drains per tick (`drain_refresh_results`, `drain_obs_results` per CLAUDE.md); `eumetnet.rs`'s `flush_tx` signal drives `ObsRefreshPayload::PartialCommit`, a type defined in [`src/app.rs`](../../../src/app.rs) (line 2691), not in the provider module itself.
- All five HTTP-backed providers (`meteogate`, `maps`, `meteoalarm`, `eumetnet`, `geocode`) take a `dirs: FrontDirs` and use `crate::cache::{read_if_exists, write_atomic, write_log}` — cache layout changes in [`src/cache.rs`](../../../src/cache.rs) ripple into every provider file.
- Config structs consumed here (`MeteoGateConfig`, `EumetnetConfig`, `MeteoAlarmConfig`, `GeocodeConfig`) are defined in [`src/config.rs`](../../../src/config.rs); field renames there require matching edits in the corresponding provider.
- `location.rs` submodule (GeoClue/IP/Windows/macOS backends) is declared alongside these in `mod.rs` but is a separate domain per task instructions — not covered here.

## Conventions worth knowing
- Every provider that hits a rate-limited or quota-limited external API implements its own client-side guard rather than relying on the server: `geocode.rs` throttles to 1 req/s via a `Mutex<Option<Instant>>` timestamp (`GeocodeProvider::throttle`); `eumetnet.rs` implements a sliding-window `RequestBudget` (`try_spend`/`remaining`) capped at `BUDGET_UTILISATION` (0.8) of `EumetnetConfig::hourly_quota()`, with the gateway's `X-RateLimit-Reset` header driving cooldown (`RATE_LIMIT_DEFAULT_COOLDOWN` 60s, capped at `RATE_LIMIT_MAX_COOLDOWN` 20 min); `meteogate.rs` caches negative/positive S3 HEAD probes (`ProbeResult::Exists`/`Missing`/`Unreachable`) with different retry windows (`MISSING_RETRY` 60s vs `TRANSIENT_RETRY` 5s).
- `eumetnet.rs::fetch_observations` runs a three-phase fetch: (1) station-name list from a 24h disk cache (`STATION_LIST_TTL`), refetched in the background if stale; (2) a batched `fetch_location_batch` over capitals+major cities snapped onto a `REGION_CELL_DEG` (12°) grid to cut ~113 individual queries down to ~16 regional cells; (3) `fetch_viewport_points`, a single `/area` bbox query for the visible viewport, only issued when `zoom >= CAPITALS_ZOOM_CUTOFF` (5.5). All phases dedupe stations by WIGOS id via a shared `seen_wigos: HashSet<String>`.
- `meteogate.rs` never persists the source GeoTIFF: `frame_impl`/`load_grid` decode it once and store a custom `.frd` grid format (magic `FRD1`, one `u8` code per 0.5 dBZ step, zstd level 1) — decode is ~3ms vs ~198ms for the deflate GeoTIFF, per the file's own measurement comments.
- `meteogate.rs::frame_streamed` builds tiles in centre-first spiral order (`crate::geo::tiles_spiral_from`) and streams each completed `RadarTile` through a channel as soon as it's built, bounded by a `Semaphore` (`MAX_CONCURRENT_TILES` = 16); `frame` (non-streaming) returns all tiles at once instead.
- `maps.rs::NaturalEarthProvider` downloads GeoJSON from a primary CDN URL first, falling back to a raw GitHub URL on failure (`download_first` tries `country_urls`/`detail_urls` in order); each resolution's source parse is deduplicated and written to a compact `.dedup` cache file plus a `.generated` marker so a second boot skips re-parsing entirely.
- `maps.rs` runs GeoJSON parsing and Ramer-Douglas-Peucker simplification inside `tokio::task::spawn_blocking`, with per-resolution epsilon values (`simplification_epsilon`) tuned so visual detail is proportional to zoom band.
- `meteoalarm.rs::warnings()` checks a 5-minute-TTL disk cache (mtime-based) before hitting the MeteoAlarm EDR API at `{api_endpoint}/collections/warnings/locations/ALL`, appending `?token=` only when `MeteoAlarmConfig::token` is non-empty; GeoJSON decoding happens in `task::spawn_blocking` and only `Polygon` geometries are kept (other geometry types are skipped).
- `lightning.rs::connect_and_stream` round-robins across three hardcoded Blitzortung WS endpoints (`WS_SERVERS`), reconnecting on drop (5s backoff on connect failure, 3s on disconnect); incoming frames are decoded with a custom LZW-variant `decode()` ported from the `akeamc/blitzortung` project (comment cites MIT license) before JSON parsing.
- `geocode.rs` derives the `/reverse` endpoint from the configured `/search` endpoint via string replacement (`reverse_endpoint`), so `config.toml` only stores one Nominatim URL.
- `geocode.rs::NominatimAddress::settlement()` walks address fields smallest-first (`hamlet`, `village`, `town`, `municipality`, `city`, `suburb`, `county`) to avoid rolling a village up to its containing city.
- Every outbound `reqwest::Client` builder that talks to Nominatim sets a custom `User-Agent` (`USER_AGENT` const built from `CARGO_PKG_VERSION`) per that service's usage policy; `meteogate.rs`/`maps.rs`/`meteoalarm.rs`/`eumetnet.rs` reuse one shared `reqwest::Client` passed in from `app.rs` instead of building their own.
