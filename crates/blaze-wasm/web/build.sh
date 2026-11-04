#!/bin/sh
# Builds what the page loads into `pkg/`: the compiler as wasm32, and the source of `core`.
# `core` comes from blaze's own library/ at the repo root. `check.mjs` runs the page's demos
# without a browser; `build-site.sh` puts the page into the website.
set -eu
cd "$(dirname "$0")/.."
target=../../target
mkdir -p web/pkg

cargo build --release --lib --target-dir "$target" --target wasm32-unknown-unknown
cp "$target/wasm32-unknown-unknown/release/blaze_wasm.wasm" web/pkg/

cargo run --release --target-dir "$target" -- \
    bundle ../../library web/pkg/core.bundle
