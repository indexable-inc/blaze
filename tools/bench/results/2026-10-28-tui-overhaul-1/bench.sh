#!/usr/bin/env bash
# bench3.sh OUT: on a terminal (script, 110 columns), so the live view draws: main's blaze and the branch's, each with a warm daemon of its own on its own
# copy of ripgrep (8 cores), alternated run by run: 10 edits and 30 no-ops each. One JSON
# line per run.
set -uo pipefail
out=$1
: >"$out"
unset BLAZE_CACHE
export TMPDIR=/root/tui/tmp
now() { python3 -c "import time; print(time.time())"; }
declare -A bin=([main]=/root/tui/main/target/release/blaze [branch]=/root/tui/blaze/target/release/blaze)
declare -A dir
for side in main branch; do
  work=$(mktemp -d)
  cp -r /root/tui/ripgrep "$work/ripgrep"
  dir[$side]=$work
  (cd "$work/ripgrep" && BLAZE_HOME="$work/home" XDG_CACHE_HOME="$work/cache" taskset -c 0-7 "${bin[$side]}" build >/dev/null 2>&1)
done
edit() {
  python3 - "$1" <<'PY'
import re, sys
path = "crates/core/main.rs"
opening = "fn search(args: &HiArgs, mode: SearchMode) -> anyhow::Result<bool> {"
text = re.sub(re.escape(opening) + r"\n    let _tui_edit = \d+u32;", opening, open(path).read())
open(path, "w").write(text.replace(opening, opening + f"\n    let _tui_edit = {sys.argv[1]}u32;"))
PY
}
run() {
  local side=$1 kind=$2 i=$3 s e work=${dir[$1]}
  cd "$work/ripgrep"
  [ "$kind" = edit ] && edit "$i"
  s=$(now); BLAZE_HOME="$work/home" XDG_CACHE_HOME="$work/cache" script -qfec "stty cols 110 rows 40; taskset -c 0-7 ${bin[$side]} build" /dev/null >/dev/null 2>&1; local code=$?; e=$(now)
  python3 -c "import json; print(json.dumps({'side': '$side', 'kind': '$kind', 'i': $i, 'exit': $code, 's': round($e - $s, 5)}))" >>"$out"
}
for i in $(seq 1 20); do
  if (( i % 2 )); then run main edit $i; run branch edit $i; else run branch edit $i; run main edit $i; fi
done
for i in $(seq 1 30); do
  if (( i % 2 )); then run main noop $i; run branch noop $i; else run branch noop $i; run main noop $i; fi
done
for side in main branch; do
  (cd "${dir[$side]}/ripgrep" && BLAZE_HOME="${dir[$side]}/home" "${bin[$side]}" stop >/dev/null 2>&1)
done
echo DONE >>"$out"
