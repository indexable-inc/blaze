#!/usr/bin/env bash
# `check.sh DIR KEY JOB`: verifies DIR, restored from the cache under KEY, holds KEY and every
# binary `bins.sh JOB` requires. A miss names what is wrong and stops; nothing is rebuilt in its
# place (AGENTS.md no-fallback).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd -P)
die() { echo "::error::$*" >&2; echo "check.sh: $*" >&2; exit 1; }
[ $# = 3 ] || die "takes DIR, KEY, and JOB, not: $*"
dir=$1 key=$2 job=$3
[ -f "$dir/KEY" ] || die "the built scripts restored at $dir have no KEY file"
[ "$(cat "$dir/KEY")" = "$key" ] || die "the built scripts at $dir are of key $(cat "$dir/KEY"), not $key"
bins=$("$here/bins.sh" "$job")
while IFS= read -r name; do
  [ -n "$name" ] || continue
  [ -x "$dir/$name" ] || die "the built scripts at $dir (key $key, job $job) have no $name"
done <<< "$bins"
