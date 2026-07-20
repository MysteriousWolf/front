# Project signals

## Framework & runtime

Rust 2021 edition, package `front` (version `26.1.0`). Terminal UI via `ratatui` 0.30 + `crossterm` 0.29 (event-stream feature), async runtime `tokio` 1 (manually-built multi-thread runtime in [`src/main.rs`](../../src/main.rs), not `#[tokio::main]`). HTTP via `reqwest` 0.12 (rustls-tls, json). Optional features: `mqtt` (`rumqttc`, MeteoAlarm live updates) and `lightning` (`tokio-tungstenite`, Blitzortung WS stream) — both in `default = ["mqtt", "lightning"]`. Per-platform location deps: `zbus` (Linux D-Bus), `windows` crate (`Devices_Geolocation`), `objc2`/`objc2-core-location` (macOS).

## Build / test / lint

| Purpose | Command | Source |
|---------|---------|--------|
| Debug build | `cargo build` | [`Cargo.toml`](../../Cargo.toml) |
| Release build | `cargo build --release` | [`Cargo.toml`](../../Cargo.toml) (`[profile.release]`: `lto = "fat"`, `codegen-units = 1`, `strip = "symbols"`) |
| Run | `cargo run` / `cargo run -- --lat 46.0 --lon 14.5 --zoom 6.0` / `cargo run -- --no-location` / `cargo run -- --clear-cache` | [`src/cli.rs`](../../src/cli.rs) |
| Build without MQTT | `cargo build --no-default-features` | [`Cargo.toml`](../../Cargo.toml) `[features]` |
| Test | `cargo test` | workspace-wide `#[cfg(test)]` modules |
| Lint | `cargo clippy` | [`.zed/format.sh`](../../.zed/format.sh) runs `cargo clippy --all-targets --all-features -- -D warnings` |
| Type-check | `cargo check` | — |
| Manual location probe | `cargo run --example location_probe` | [`examples/location_probe.rs`](../../examples/location_probe.rs) |
| Manual geocode probe | `cargo run --example geocode_probe -- "Ljubljana"` | [`examples/geocode_probe.rs`](../../examples/geocode_probe.rs) |

CI ([`.github/workflows/ci.yml`](../../.github/workflows/ci.yml)) gates on a `quick` job (fmt check + `cargo check` with `--all-features` and `--no-default-features`), then `clippy`, `build`, `test`, `docs`, and an independent `audit` job (`cargo-audit`, ignoring 4 RUSTSEC advisories pinned in [`.cargo/audit.toml`](../../.cargo/audit.toml) pending a `rumqttc` upgrade). [`.github/workflows/version-guard.yml`](../../.github/workflows/version-guard.yml) validates the `YY.N` version scheme (see repo-root [`CLAUDE.md`](../../CLAUDE.md)) against [`Cargo.toml`](../../Cargo.toml) and git tags on every push to `main` and every `v*` tag push.

## Language breakdown

| Language | LOC | Files | % |
|----------|-----|-------|---|
| Rust | 19566 | 24 | 97% |
| Markdown | 246 | 2 | 1% |
| YAML | 243 | 2 | 1% |
| TOML | 73 | 2 | 0% |
| JSON | 26 | 1 | 0% |
| Shell | 4 | 1 | 0% |

## DevOps & CI

GitHub Actions is the CI provider ([`.github/workflows/ci.yml`](../../.github/workflows/ci.yml), `version-guard.yml`). No deploy/release automation exists beyond `version-guard.yml`'s validation — version bumps and git tags are manual steps per repo-root [`CLAUDE.md`](../../CLAUDE.md). [`.cargo/audit.toml`](../../.cargo/audit.toml) configures `cargo-audit` with four ignored RUSTSEC advisories tied to the pinned `rumqttc` version.

## Domains

| Domain | Repo paths | One-liner | Detail |
|--------|------------|-----------|--------|
| app-core | [`src/app.rs`](../../src/app.rs), [`src/main.rs`](../../src/main.rs), [`src/cli.rs`](../../src/cli.rs), [`src/keys.rs`](../../src/keys.rs) | Boot flow, central `App` state machine, background-task channels, CLI args, key resolution | [`.claude/project/signals/app-core.md`](signals/app-core.md) |
| rendering | [`src/ui.rs`](../../src/ui.rs), [`src/layers.rs`](../../src/layers.rs) | ratatui rendering, `LayerRegistry`/`RenderModeState`, border rasterisation, overlay-mode exceptions | [`.claude/project/signals/rendering.md`](signals/rendering.md) |
| providers | [`src/providers/mod.rs`](../../src/providers/mod.rs), `meteogate.rs`, `maps.rs`, `meteoalarm.rs`, `eumetnet.rs`, `geocode.rs`, `lightning.rs` | Radar/border/warning/obs/geocode/lightning HTTP+WS clients, each with client-side rate-limit guards | [`.claude/project/signals/providers.md`](signals/providers.md) |
| location | [`src/providers/location/`](../../src/providers/location) | Platform-agnostic location backends (GeoClue, Windows, macOS, IP fallback) racing into a shared arbiter | [`.claude/project/signals/location.md`](signals/location.md) |
| geo | [`src/geo.rs`](../../src/geo.rs) | Normalised `[0,1]²` world-space coordinate system, Mercator conversions, tile addressing | [`.claude/project/signals/geo.md`](signals/geo.md) |
| persistence | [`src/cache.rs`](../../src/cache.rs), [`src/config.rs`](../../src/config.rs) | Config/state TOML persistence, cache directory management, atomic writes | [`.claude/project/signals/persistence.md`](signals/persistence.md) |
| ci-release | [`.github/workflows/ci.yml`](../../.github/workflows/ci.yml), [`.github/workflows/version-guard.yml`](../../.github/workflows/version-guard.yml), [`.cargo/audit.toml`](../../.cargo/audit.toml), [`Cargo.toml`](../../Cargo.toml), [`.zed/`](../../.zed) | CI pipeline, version-scheme enforcement, cargo-audit config, editor dev tooling | [`.claude/project/signals/ci-release.md`](signals/ci-release.md) |

## Cross-cutting

- Domain partitioning basis: vertical slices by functional concern, corroborated by the existing architecture write-up in repo-root [`CLAUDE.md`](../../CLAUDE.md) (which already documents App struct, rendering, layers, providers, location, coordinate system, persistence as separate sections). No `.claude/.atomic-index/atomic.db` code-intel index was present for this run — partitioning relied on the deterministic file tree plus direct source reads, not call-graph queries.
- Deterministic substrate: [`.claude/project/deterministic-signals.md`](deterministic-signals.md) (whole-repo scan, 24 Rust files / 19566 LOC).
- Test layout: no dedicated `tests/` directory — all tests are inline `#[cfg(test)] mod tests` blocks within their source file (e.g. [`src/geo.rs`](../../src/geo.rs), [`src/layers.rs`](../../src/layers.rs), [`src/keys.rs`](../../src/keys.rs), [`src/config.rs`](../../src/config.rs)).
- No `.signalsignore` file exists at repo root, so no paths were flagged `[generated]` in the deterministic scan.
- All 7 domain files passed reviewer validation within budget (no domain required a 3rd iteration).
</content>
