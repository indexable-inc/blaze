#!/usr/bin/env bash
# `bins.sh JOB`: prints the sorted, deduplicated list of `scripts/src/bin/<name>.rs` binaries
# needed by caller job JOB (`scripts`, `lean`, or `release`). One home shared by key.sh,
# build.sh and check.sh so cache keys, builds and cache checks never drift.
set -euo pipefail
die() { echo "::error::$*" >&2; echo "bins.sh: $*" >&2; exit 1; }
[ $# = 1 ] || die "takes JOB (scripts, lean, or release), not: $*"
job=$1

case "$job" in
  lean)
    bins="lean_gate"
    ;;
  release)
    bins="toolchain_cut"
    ;;
  scripts)
    [ -f tools/gate.txt ] || die "no tools/gate.txt in $(pwd -P): run from the checkout's top"
    bins=$(
      {
        echo gate
        sed -nE 's/.*\$BLAZE_SCRIPTS\/([A-Za-z0-9_-]+).*/\1/p' tools/gate.txt
      } | LC_ALL=C sort -u
    )
    ;;
  *)
    die "unknown job '$job' (expected scripts, lean, or release)"
    ;;
esac

[ -n "$bins" ] || die "job '$job' resolved to no scripts"
while IFS= read -r name; do
  [ -f "scripts/src/bin/$name.rs" ] || die "job '$job' requires '$name', which has no scripts/src/bin/$name.rs"
  printf '%s\n' "$name"
done <<< "$bins"
