#!/bin/sh
# Builds what the page loads into `pkg/`: the compiler as wasm32, and the source of `core`.
# `core` comes from blaze's own library/ at the repo root. Serve this directory afterwards, with any static server:
#
#     python3 -m http.server -d crates/blaze-wasm/web 8377
set -eu
cd "$(dirname "$0")/.."
target=../../target
mkdir -p web/pkg

cargo build --release --lib --target-dir "$target" --target wasm32-unknown-unknown
cp "$target/wasm32-unknown-unknown/release/blaze_wasm.wasm" web/pkg/

cargo run --release --target-dir "$target" -- \
    bundle ../../library web/pkg/core.bundle
