#!/usr/bin/env bash
# `build.sh DIR KEY`: every script (scripts/src/bin/<name>.rs) exported by the blaze on PATH
# (.github/blaze-cli-version; AGENTS.md rules no-rustc, blaze-builds-blaze) and put in DIR as
# DIR/<name>, with DIR/KEY holding KEY (key.sh's). Run from the checkout's top, whole.
set -euo pipefail
die() { echo "::error::$*" >&2; echo "build.sh: $*" >&2; exit 1; }
[ $# = 2 ] || die "takes DIR and KEY, not: $*"
dir=$1 key=$2

command -v blaze >/dev/null 2>&1 || die "no blaze on PATH (.github/blaze-cli-version names $(cat .github/blaze-cli-version))"

# Scrub caller environment variables so the cache key covers the build.
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS RUSTDOCFLAGS RUSTC RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER \
  CARGO_BUILD_TARGET CARGO_BUILD_JOBS CARGO_INCREMENTAL BLAZE_BACKEND BLAZE_OPT BLAZE_EMIT
for name in $(env | sed -nE 's/^(CARGO_PROFILE_[A-Z0-9_]*|CARGO_TARGET_[A-Z0-9_]*)=.*/\1/p'); do unset "$name"; done

fresh="$dir.new"
rm -rf "$fresh"
mkdir -p "$fresh"
for src in scripts/src/bin/*.rs; do
  name=$(basename "$src" .rs)
  if ! blaze export . -p scripts --bin "$name" -o "$fresh/$name"; then
    die "blaze export . -p scripts --bin $name failed"
  fi
  [ -x "$fresh/$name" ] || die "blaze export produced no executable at $fresh/$name"
done
echo "$key" > "$fresh/KEY"
rm -rf "$dir"
mv "$fresh" "$dir"
echo "build.sh: $(ls "$dir" | grep -vx KEY | wc -l | tr -d ' ') scripts in $dir under $key"
