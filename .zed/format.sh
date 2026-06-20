#!/usr/bin/env sh
set -e
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
