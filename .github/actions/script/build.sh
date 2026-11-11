#!/usr/bin/env bash
# `build.sh DIR KEY`: every script (scripts/src/bin/<name>.rs) built by the blaze on PATH, which
# must be the release .github/blaze-version names (AGENTS.md blaze-builds-blaze), and put in DIR
# as DIR/<name>, with DIR/KEY holding KEY (key.sh's). Run from the checkout's top, whole: blaze
# loads the workspace scripts/ is a member of.
set -euo pipefail
die() { echo "::error::$*" >&2; echo "build.sh: $*" >&2; exit 1; }
[ $# = 2 ] || die "takes DIR and KEY, not: $*"
dir=$1 key=$2

want=$(cat .github/blaze-version)
said=$(blaze-engine --version 2>&1) || die "no blaze on PATH (blaze-engine --version: $said)"
[ "$(echo "$said" | awk '$1 == "blaze" { print $2 }')" = "$want" ] ||
  die "the blaze on PATH says '$said', and .github/blaze-version names $want: the key names that release"

# What cargo or blaze reads from the caller's environment (harness/v2/engine.py, SCRUBBED), so
# the key, which names none of it, covers the build.
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS RUSTDOCFLAGS RUSTC RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER \
  CARGO_BUILD_TARGET CARGO_BUILD_JOBS CARGO_INCREMENTAL BLAZE_BACKEND BLAZE_OPT BLAZE_EMIT
for name in $(env | sed -nE 's/^(CARGO_PROFILE_[A-Z0-9_]*|CARGO_TARGET_[A-Z0-9_]*)=.*/\1/p'); do unset "$name"; done

target="$dir.target"
export CARGO_TARGET_DIR="$target"
blaze build -p scripts --bins

fresh="$dir.new"
rm -rf "$fresh"
mkdir -p "$fresh"
for src in scripts/src/bin/*.rs; do
  name=$(basename "$src" .rs)
  [ -x "$target/debug/$name" ] || die "blaze build -p scripts --bins left no $target/debug/$name"
  cp "$target/debug/$name" "$fresh/$name"
done
echo "$key" > "$fresh/KEY"
rm -rf "$dir"
mv "$fresh" "$dir"
rm -rf "$target"
echo "build.sh: $(ls "$dir" | grep -vx KEY | wc -l | tr -d ' ') scripts in $dir under $key"
