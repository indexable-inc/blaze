#!/usr/bin/env bash
# `check.sh DIR KEY`: DIR, restored from the cache under KEY, is what build.sh made under that key:
# DIR/KEY says KEY and every script of scripts/src/bin has its binary. A miss names what is
# wrong and stops; nothing is rebuilt in its place (AGENTS.md no-fallback).
set -euo pipefail
die() { echo "::error::$*" >&2; echo "check.sh: $*" >&2; exit 1; }
[ $# = 2 ] || die "takes DIR and KEY, not: $*"
dir=$1 key=$2
[ -f "$dir/KEY" ] || die "the built scripts restored at $dir have no KEY file"
[ "$(cat "$dir/KEY")" = "$key" ] || die "the built scripts at $dir are of key $(cat "$dir/KEY"), not $key"
for src in scripts/src/bin/*.rs; do
  name=$(basename "$src" .rs)
  [ -x "$dir/$name" ] || die "the built scripts at $dir (key $key) have no $name, which scripts/src/bin has"
done
