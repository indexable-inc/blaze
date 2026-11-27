#!/usr/bin/env bash
# `build.sh DIR KEY JOB`: builds the job-scoped Rust scripts (`bins.sh JOB`) as standalone AOT
# binaries with the pinned v2 `blaze` on PATH (.github/blaze-cli-version; zero cargo, zero rustc)
# and places them at DIR/<name> with DIR/KEY holding KEY.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd -P)
die() { echo "::error::$*" >&2; echo "build.sh: $*" >&2; exit 1; }
[ $# = 3 ] || die "takes DIR, KEY, and JOB, not: $*"
dir=$1 key=$2 job=$3

command -v blaze >/dev/null 2>&1 || die "no blaze on PATH (.github/blaze-cli-version names $(cat .github/blaze-cli-version))"

# Scrub caller environment variables so the cache key covers the build.
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS RUSTDOCFLAGS RUSTC RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER \
  CARGO_BUILD_TARGET CARGO_BUILD_JOBS CARGO_INCREMENTAL BLAZE_BACKEND BLAZE_OPT BLAZE_EMIT
for name in $(env | sed -nE 's/^(CARGO_PROFILE_[A-Z0-9_]*|CARGO_TARGET_[A-Z0-9_]*)=.*/\1/p'); do unset "$name"; done

run_bounded() {
  local secs=$1; shift
  if command -v timeout >/dev/null 2>&1; then
    timeout -k 30 "$secs" "$@"
  elif command -v gtimeout >/dev/null 2>&1; then
    gtimeout -k 30 "$secs" "$@"
  else
    "$@"
  fi
}

bins=$("$here/bins.sh" "$job")
fresh="$dir.new"
rm -rf "$fresh"
mkdir -p "$fresh"
while IFS= read -r name; do
  [ -n "$name" ] || continue
  if ! run_bounded 600 blaze build --standalone -p scripts --bin "$name" -o "$fresh/$name"; then
    die "blaze build --standalone -p scripts --bin $name failed"
  fi
  [ -x "$fresh/$name" ] || die "blaze build --standalone produced no executable at $fresh/$name"
done <<< "$bins"
echo "$key" > "$fresh/KEY"
rm -rf "$dir"
mv "$fresh" "$dir"
echo "build.sh: $(ls "$dir" | grep -vx KEY | wc -l | tr -d ' ') scripts for job '$job' in $dir under $key"
