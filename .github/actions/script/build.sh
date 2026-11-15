#!/usr/bin/env bash
# `build.sh DIR KEY`: every script (scripts/src/bin/<name>.rs) built by cargo with the nightly
# harness/toolchain names, and put in DIR as DIR/<name>, with DIR/KEY holding KEY (key.sh's). Run
# from the checkout's top, whole: cargo loads the workspace scripts/ is a member of.
#
# cargo, not blaze: no release builds today's tree, and the tree's own blaze (stage 0) is built by
# cargo too (.github/actions/blaze, docs/contributing.md "CI"), so cargo building the scripts
# keeps the cache keyed by the nightly and the scripts' own inputs alone, where a stage-0 key would
# change with every commit of blaze. Where each binary is comes from the build itself, the
# `executable` of its `compiler-artifact` line under --message-format=json, and it is copied into
# DIR, which may be on any file system. The release profile, as `cargo build --release` in the
# checkout's target/ builds stage 0 beside them.
set -euo pipefail
die() { echo "::error::$*" >&2; echo "build.sh: $*" >&2; exit 1; }
[ $# = 2 ] || die "takes DIR and KEY, not: $*"
dir=$1 key=$2
tc=$(cat harness/toolchain)

# What cargo or blaze reads from the caller's environment (harness/v2/engine.py, SCRUBBED), so
# the key, which names none of it, covers the build.
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS RUSTDOCFLAGS RUSTC RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER \
  CARGO_BUILD_TARGET CARGO_BUILD_JOBS CARGO_INCREMENTAL BLAZE_BACKEND BLAZE_OPT BLAZE_EMIT
for name in $(env | sed -nE 's/^(CARGO_PROFILE_[A-Z0-9_]*|CARGO_TARGET_[A-Z0-9_]*)=.*/\1/p'); do unset "$name"; done

# cargo's own target directory in CI, as .github/actions/blaze's stage 0 has it: blaze writes
# target/release in a format cargo refuses, and an ix runner's snapshot keeps it.
export CARGO_TARGET_DIR=target/cargo
said="$dir.build.json"
mkdir -p "$(dirname "$dir")"
timeout 600 rustup toolchain install "$tc" --profile minimal >&2 || die "rustup could not install $tc"
timeout 1800 cargo +"$tc" build --release --locked -p scripts --bins --message-format=json > "$said" ||
  die "cargo +$tc build --release --locked -p scripts --bins --message-format=json failed (its JSON lines: $said)"

fresh="$dir.new"
rm -rf "$fresh"
mkdir -p "$fresh"
for src in scripts/src/bin/*.rs; do
  name=$(basename "$src" .rs)
  # The executables the build names whose file is `<name>`: exactly one.
  found=$(grep '"reason":"compiler-artifact"' "$said" | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | awk -v n="$name" -F/ '$NF == n' | sort -u)
  [ -n "$found" ] || die "the build of the scripts named no executable \`$name\` in a compiler-artifact line ($said)"
  [ "$(echo "$found" | wc -l)" = 1 ] || die "the build of the scripts named more than one executable \`$name\`: $(echo "$found" | tr '\n' ' ')"
  [ -x "$found" ] || die "the build named $found for \`$name\`, which is no executable file"
  cp "$found" "$fresh/$name"
done
echo "$key" > "$fresh/KEY"
rm -rf "$dir"
mv "$fresh" "$dir"
rm -f "$said"
echo "build.sh: $(ls "$dir" | grep -vx KEY | wc -l | tr -d ' ') scripts in $dir under $key"
