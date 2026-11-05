#!/usr/bin/env bash
# Writes host.json and windows.json: the `resolve` of cargo 1.99.0's `cargo metadata` for the
# workspace tests/metadata.rs builds, without and with `--filter-platform`, its path as /ws.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
ws=$(cd "$(mktemp -d)" && pwd -P)/ws
mkdir -p "$ws/src"
cd "$ws"
cat > Cargo.toml <<'TOML'
[package]
name = "foo"
version = "0.1.0"
edition = "2021"

[dependencies]
baz-x = { path = "bar", package = "bar" }
common = { path = "common", features = ["f1"] }

[dev-dependencies]
common = { path = "common" }

[build-dependencies]
cc = { path = "cc" }

[target.'cfg(unix)'.dependencies]
unixy = { path = "unixy" }
TOML
touch src/lib.rs
for p in bar common cc unixy; do mkdir -p $p/src; touch $p/src/lib.rs; done
printf '[package]\nname = "bar"\nversion = "0.2.0"\nedition = "2021"\n\n[lib]\nname = "bar_lib"\n' > bar/Cargo.toml
printf '[package]\nname = "common"\nversion = "1.0.0"\nedition = "2021"\n\n[features]\nf1 = []\nf2 = []\n' > common/Cargo.toml
for p in cc unixy; do printf '[package]\nname = "%s"\nversion = "1.0.0"\nedition = "2021"\n' $p > $p/Cargo.toml; done
resolve() {
  cargo +1.99.0 metadata --format-version 1 --offline "$@" |
    python3 -c "import json, sys; print(json.dumps(json.load(sys.stdin)['resolve'], separators=(',', ':')).replace('file://$ws', 'file:///ws'), end='')"
}
resolve > "$here/host.json"
resolve --filter-platform x86_64-pc-windows-msvc > "$here/windows.json"
