#!/usr/bin/env bash
# `local.sh CACHE`: the script action's steps (action.yml) outside Actions, with the directory
# CACHE standing in for the Actions cache (CACHE/<key>), to exercise its hit and miss paths on a
# machine with the checkout: run from the checkout's top. A miss needs the pinned blaze on PATH
# and builds; a hit runs no blaze. Prints the key, `hit` or `miss`, and the BLAZE_SCRIPTS line
# the action exports; then, as a run of a script would, `$BLAZE_SCRIPTS/changed selftest`.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd -P)
[ $# = 1 ] || { echo "local.sh: takes the cache directory, not: $*" >&2; exit 1; }
cache=$1
key=$("$here/key.sh")
dir="$cache/$key"
echo "key $key"
if [ -d "$dir" ]; then
  echo hit
  "$here/check.sh" "$dir" "$key"
else
  echo miss
  "$here/build.sh" "$dir" "$key"
fi
echo "BLAZE_SCRIPTS=$dir"
"$dir/changed" selftest
