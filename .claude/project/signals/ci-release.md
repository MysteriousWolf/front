# ci-release
## What it does
- [`.github/workflows/ci.yml`](../../../.github/workflows/ci.yml) runs on push/PR to `main`: a `quick` gate (fmt check + `cargo check` with all features and with no default features), then `clippy`, `build`, `test`, `docs`, and an independent `audit` job, all gated behind `quick` except `audit`.
- [`.github/workflows/version-guard.yml`](../../../.github/workflows/version-guard.yml) runs on push to `main`, on tags matching `v*`, and on pull requests; it validates [`Cargo.toml`](../../../Cargo.toml)'s `version` against the `YY.N`/`YY.N.P` format, checks the year component matches the current UTC year, checks the release number is greater than the highest existing `v${YY}.*` tag, and (tag pushes only) checks the tag name equals `v` + the release version and that no GitHub release already exists for that tag.
- [`Cargo.toml`](../../../Cargo.toml) declares `version = "26.1.0"` and `edition = "2021"`.

## Artifacts
- [`.cargo/audit.toml`](../../../.cargo/audit.toml) — `cargo-audit` config; ignores four RUSTSEC advisories (`RUSTSEC-2026-0049`, `-0098`, `-0099`, `-0104`) pending a `rumqttc` upgrade to rustls 0.23+.
- [`.zed/tasks.json`](../../../.zed/tasks.json) — Zed editor task definitions: `run` (`cargo run`), `format` ([`.zed/format.sh`](../../../.zed/format.sh)), `test` (`cargo test --all-features`).
- [`.zed/format.sh`](../../../.zed/format.sh) — shell script running `cargo fmt --all` then `cargo clippy --all-targets --all-features -- -D warnings`, `set -e` so it stops on first failure.

## Coupling
- [`Cargo.toml`](../../../Cargo.toml)'s `[features]` block (`default = ["mqtt", "lightning"]`, `mqtt = ["dep:rumqttc"]`, `lightning = ["dep:tokio-tungstenite"]`) is what `ci.yml`'s `--all-features` / `--no-default-features` matrix legs exercise — adding or renaming a feature changes what CI's `quick`, `clippy`, `build`, and `test` jobs cover, coupling this domain to whichever domain owns MQTT (MeteoAlarm live updates, providers domain) and lightning/tile-streaming code (providers domain).
- `version-guard.yml`'s format/year/ordering checks encode the `YY.N` versioning scheme also stated in the repo's root [`CLAUDE.md`](../../../CLAUDE.md); changing one without the other desyncs documented convention from enforced convention.
- [`.cargo/audit.toml`](../../../.cargo/audit.toml)'s ignored advisories are tied to `rumqttc = "0.24"` in [`Cargo.toml`](../../../Cargo.toml); bumping `rumqttc` past 0.24 (to pull rustls 0.23+) is the stated condition for revisiting/removing those ignores.

## Conventions worth knowing
- CI treats `--all-features` and `--no-default-features` as the two feature configurations to validate everywhere (quick check, clippy, build, test) — there is no matrix leg for a single-feature-only build (e.g. `mqtt` without `lightning`).
- `[profile.release]` uses `lto = "fat"` and `codegen-units = 1`, which `ci.yml` calls out in a comment as the reason release builds in the `build` job are slow (~5-10 min cold cache).
- `version-guard.yml` treats format/year/ordering violations as hard errors (`::error::`, exit 1) only when `IS_TAG` or `IS_MAIN` is true; on other PR branches the same violations are only `::warning::` and do not fail the job.
- The version-guard duplicate-release check uses `gh api repos/.../releases/tags/$TAG` (GitHub Releases), not just git tag existence, to detect a duplicate.
- `cargo-audit` is installed via `cargo install cargo-audit --locked` and its binary is cached by version key `cargo-audit-0.22` in `ci.yml`, separate from the crate's own dependency versions in [`Cargo.toml`](../../../Cargo.toml).
</content>
