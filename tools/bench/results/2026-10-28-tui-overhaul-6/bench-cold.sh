#!/usr/bin/env bash
# coldbench.sh OUT: before's blaze and after's, a cold ripgrep build each in turn (fresh home and
# store, 8 cores), 3 each: one JSON line per build with the worker's time and its steps' times.
set -uo pipefail
out=$1
: >"$out"
unset BLAZE_CACHE
export TMPDIR=/root/tui/tmp
declare -A bin=([main]=/root/tui/main/target/release/blaze [branch]=/root/tui/blaze/target/release/blaze)
run() {
  local side=$1 i=$2 work
  work=$(mktemp -d)
  cp -r /root/tui/ripgrep "$work/ripgrep"
  cd "$work/ripgrep"
  BLAZE_HOME="$work/home" XDG_CACHE_HOME="$work/cache" taskset -c 0-7 "${bin[$side]}" build --events json >"$work/ev" 2>/dev/null
  local code=$?
  python3 - "$work/ev" "$side" "$i" "$code" >>"$out" <<'PY'
import json, sys
path, side, i, code = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
records = [json.loads(l) for l in open(path) if l.startswith("{")]
steps, open_, slow, flying = {}, {}, 0, 0
for r in records:
    (kind, body), = r["event"].items()
    if kind == "Started":
        open_[(body["step"], body["of"])] = r["at_ns"]
    elif kind == "Ended":
        key = (body["step"], body["of"])
        if key in open_:
            steps[body["step"]] = steps.get(body["step"], 0) + r["at_ns"] - open_.pop(key)
    elif kind == "Slowest":
        slow = len(body["functions"])
    elif kind == "Compiling":
        flying += 1
finished = [r["at_ns"] for r in records if "Finished" in r["event"]]
print(json.dumps({"side": side, "i": i, "exit": code, "build_ns": finished[-1] if finished else 0,
                  "steps": steps, "slowest": slow, "compiling_records": flying}))
PY
  BLAZE_HOME="$work/home" "${bin[$side]}" stop >/dev/null 2>&1
  cd /; rm -rf "$work"
}
for i in 1 2 3; do
  if (( i % 2 )); then run main $i; run branch $i; else run branch $i; run main $i; fi
done
echo DONE >>"$out"
