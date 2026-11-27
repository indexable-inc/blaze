#!/usr/bin/env bash
# `local.sh CACHE [JOB]`: exercises the script action's steps (action.yml) outside Actions, with
# CACHE standing in for the Actions cache (CACHE/<key>), run from the checkout's top. JOB defaults
# to `scripts` (`scripts`, `lean`, or `release`). A miss needs the pinned blaze on PATH and builds;
# a hit runs no blaze.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd -P)
[ $# = 1 ] || [ $# = 2 ] || { echo "local.sh: takes CACHE [JOB], not: $*" >&2; exit 1; }
cache=$1
job=${2:-scripts}
key=$("$here/key.sh" "$job")
dir="$cache/$key"
echo "key $key"
if [ -f "$dir/KEY" ]; then
  echo hit
else
  echo miss
  "$here/build.sh" "$dir" "$key" "$job" || { echo "local.sh: the build failed" >&2; exit 1; }
fi
"$here/check.sh" "$dir" "$key" "$job" || exit 1
echo "BLAZE_SCRIPTS=$dir"
echo "size $(du -sh "$dir" | cut -f1) in $dir"
case "$job" in
  scripts) "$dir/changed" selftest ;;
  lean) "$dir/lean_gate" selftest ;;
  release) [ -x "$dir/toolchain_cut" ] ;;
esac
