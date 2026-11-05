#!/usr/bin/env bash
# Writes the fixtures of tests/workspace_resolve.rs from cargo 1.99.0, for the workspace it
# builds, its path as /ws: fresh.json, the `resolve` of `cargo metadata` without a lock file;
# patched.lock, the lock file that writes; and locked.json, the `resolve` with it.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
ws=$(cd "$(mktemp -d)" && pwd -P)/ws
for p in t tt b; do mkdir -p "$ws/$p/src"; touch "$ws/$p/src/lib.rs"; done
cd "$ws"
cat > Cargo.toml <<'TOML'
[workspace]
members = ["t", "tt"]
resolver = "2"

[patch.crates-io]
zz-blaze-t = { path = "t" }
TOML
cat > t/Cargo.toml <<'TOML'
[package]
name = "zz-blaze-t"
version = "1.0.0"
edition = "2021"

[features]
f = []

[dependencies]
b = { path = "../b" }
TOML
cat > tt/Cargo.toml <<'TOML'
[package]
name = "tt"
version = "0.1.0"
edition = "2021"

[dependencies]
zz-blaze-t = { version = "1", features = ["f"] }
TOML
printf '[package]\nname = "b"\nversion = "0.1.0"\nedition = "2021"\n' > b/Cargo.toml
resolve() {
  cargo +1.99.0 metadata --format-version 1 --offline |
    python3 -c "import json, sys; print(json.dumps(json.load(sys.stdin)['resolve'], separators=(',', ':')).replace('file://$ws', 'file:///ws'), end='')"
}
resolve > "$here/fresh.json"
cp Cargo.lock "$here/patched.lock"
resolve > "$here/locked.json"
