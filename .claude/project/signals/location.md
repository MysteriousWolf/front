# location
## What it does
Acquires the device's position from platform-specific backends plus an IP-based fallback, each running as an independent task that pushes `LocationFix` values into one shared `tokio::sync::mpsc::unbounded_channel`; `LocationArbiter::offer` decides which competing fix is current.
`spawn()` in [`src/providers/location/mod.rs`](../../../src/providers/location/mod.rs) conditionally starts `geoclue::run` (Linux, `cfg(target_os = "linux")`), `windows::run` (`cfg(windows)`), `macos::run` (`cfg(target_os = "macos")`), and `ip::run` (all platforms, gated by `config.ip_fallback`); a backend erroring only logs via `write_log` and does not stop the others.
`LocationSource` is `Ip < Platform < Manual` (derives `PartialOrd`/`Ord` on declaration order) and `is_better` in `mod.rs` breaks accuracy ties by source rank; a fix older than `STALE_AFTER` (5 minutes) always loses to a fresh one regardless of source.

## CLI code
- `src/app.rs:2729-2787` (`initial_viewport`) — reads `cli.lat`/`cli.lon`; if both are `Some`, builds a `LocationFix` with `LocationSource::Manual` and returns with no backend stream started (`None` receiver), so no backend task exists to override it. `cli.no_location` (checked after the manual-fix branch) also returns early with no stream.

## Docs
- [`CLAUDE.md`](../../../CLAUDE.md) — "Location ([`src/providers/location/`](../../../src/providers/location))" section documents the shared-channel/backend-independence architecture, the `LocationArbiter` selection rule, and the `--lat/--lon` manual-fix override.
- `README.md:33-43` — usage flags (`front`, `--lat --lon --zoom`, `--no-location`) and the "Requirements" section noting GeoClue2 D-Bus is used for location on Linux, with `--no-location` as the documented way to skip it.

## Coupling
- [`src/app.rs`](../../../src/app.rs) (app-core): owns the `LocationArbiter` field, calls `initial_viewport` at boot, and `drain_location_updates` (`src/app.rs:1774`) drains the mpsc receiver each tick — a new `LocationSource` variant or `LocationFix` field requires updating both drain and boot-time construction there.
- `geo` domain: `LocationFix.point` is a `crate::geo::GeoPoint`; every backend constructs fixes via `GeoPoint::new(lon, lat)`, so changes to `GeoPoint`'s constructor signature or field order touch all four backend files.
- `persistence` domain: `LocationConfig` ([`src/config.rs`](../../../src/config.rs)) supplies `ip_fallback` (bool) and `ip_endpoint` (String) consumed by `spawn()`; every backend logs failures through `crate::cache::write_log(log_path, ...)` to the shared log file, but fixes themselves are never cached to disk.

## Conventions worth knowing
- Every backend module has an identical `run(tx: UnboundedSender<LocationFix>, log_path: &Path) -> Result<()>` signature (windows/macos/geoclue) or `run(tx, endpoint, log_path)` for `ip.rs`, and each is spawned via `tokio::spawn` wrapping the call in `if let Err(e) = ... { write_log(...) }` in `mod.rs`'s `spawn()`.
- `windows.rs` and `macos.rs` carry an explicit top-of-file NOTE that they are type-checked only, never run on real hardware, because development happens on Linux; the auth-prompt and event-delegate code paths are called out as unverified.
- `macos.rs` runs `CLLocationManager` on a dedicated `std::thread` driving an `NSRunLoop` (not a tokio task) because CoreLocation delegate callbacks require a live run loop; the async `run()` function parks on `tx.closed().await` to mirror other backends' task lifetime.
- `windows.rs` similarly parks on `tx.closed().await` after subscribing to `PositionChanged`, and calls `locator.RemovePositionChanged(token)` on shutdown.
- `geoclue.rs` wraps `GetClient`/`Start` D-Bus calls in a 5-second `with_timeout` (`SETUP_TIMEOUT`) because the daemon can park those calls indefinitely when no GeoClue authorization agent is running.
- `ip.rs` uses `RETRY_INTERVAL` (60s) with exponential backoff up to `MAX_RETRY_INTERVAL` (= `REFRESH_INTERVAL`, 30 minutes) on lookup failure, and always reports `ASSUMED_ACCURACY_M` = 25_000.0 regardless of what the service returns.
- `ip.rs`'s `IpApiResponse` accepts both `lat`/`lon` and `latitude`/`longitude` field names via `#[serde(alias = ...)]`, and `parse()` is factored out from the HTTP call specifically so it is unit-testable without a network request.
- [`examples/location_probe.rs`](../../../examples/location_probe.rs) is a manual probe (`cargo run --example location_probe`) that runs `spawn()` plus a `LocationArbiter` for 15 seconds and prints every fix and whether the arbiter accepted it; [`CLAUDE.md`](../../../CLAUDE.md) points to it as the way to check a backend on a new platform.
</content>
