#!/usr/bin/env bash
# bench.sh OUT: main's blaze and the branch's, alternating (ABAB), on ripgrep with 8 cores: a cold
# build with a store of its own, 5 edits and 10 no-ops each round. One JSON line per run.
set -uo pipefail
out=$1
: >"$out"
unset BLAZE_CACHE
export TMPDIR=/root/tui/tmp
now() { python3 -c "import time; print(time.time())"; }
edit() {
  python3 - "$1" <<'PY'
import sys
path = "crates/core/main.rs"
opening = "fn search(args: &HiArgs, mode: SearchMode) -> anyhow::Result<bool> {"
text = open(path).read().replace(opening + "\n    let _tui_edit = 0u32;", opening)
for n in range(1, 100):
    text = text.replace(opening + f"\n    let _tui_edit = {n}u32;", opening)
open(path, "w").write(text.replace(opening, opening + f"\n    let _tui_edit = {sys.argv[1]}u32;"))
PY
}
run() {
  local side=$1 round=$2 kind=$3 i=$4 s e
  s=$(now); taskset -c 0-7 "$blaze" build >/dev/null 2>&1; local code=$?; e=$(now)
  python3 -c "import json; print(json.dumps({'side': '$side', 'round': $round, 'kind': '$kind', 'i': $i, 'exit': $code, 's': round($e - $s, 4)}))" >>"$out"
}
for round in 1 2; do
  for side in main branch; do
    case $side in
      main) blaze=/root/tui/main/target/release/blaze ;;
      branch) blaze=/root/tui/blaze/target/release/blaze ;;
    esac
    work=$(mktemp -d)
    cp -r /root/tui/ripgrep "$work/ripgrep"
    export BLAZE_HOME="$work/home" XDG_CACHE_HOME="$work/cache"
    cd "$work/ripgrep"
    run $side $round cold 0
    for i in 1 2 3 4 5; do edit $((round * 10 + i)); run $side $round edit $i; done
    for i in $(seq 1 10); do run $side $round noop $i; done
    "$blaze" stop >/dev/null 2>&1 || true
    cd /; rm -rf "$work"
  done
done
echo DONE >>"$out"
