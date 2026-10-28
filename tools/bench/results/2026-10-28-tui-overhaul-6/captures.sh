#!/usr/bin/env bash
# cap.sh OUT BLAZE: ripgrep cold, an edit, and a second checkout over the same store, on a
# terminal (script, 110 columns); piped, why, status, events.
set -uo pipefail
out=$1
blaze=$2
unset BLAZE_CACHE
export TMPDIR=/root/tui/tmp
rm -rf "$out"; mkdir -p "$out"
work=$(mktemp -d)
cp -r /root/tui/ripgrep "$work/ripgrep"
export BLAZE_HOME="$work/home" XDG_CACHE_HOME="$work/cache"
cd "$work/ripgrep"
tty() { script -qfec "stty cols 110 rows 40; $*" "$out/$name.tty" >/dev/null; echo "$name: exit $?" >>"$out/exits"; }
said() { name=$1; shift; "$@" >"$out/$name" 2>&1; echo "$name: exit $?" >>"$out/exits"; }
now() { python3 -c "import time; print(time.time())"; }
timed() { local s; s=$(now); "$@"; echo "$name wall: $(python3 -c "print(round($(now) - $s, 3))")" >>"$out/exits"; }
edit() {
  python3 - "$1" <<'PY'
import sys
path = "crates/core/main.rs"
opening = "fn search(args: &HiArgs, mode: SearchMode) -> anyhow::Result<bool> {"
text = open(path).read().replace(opening + "\n    let _tui_edit = 0u32;", opening)
open(path, "w").write(text.replace(opening, opening + f"\n    let _tui_edit = {sys.argv[1]}u32;"))
PY
}
name=cold timed tty taskset -c 0-7 "$blaze" build
said why-cold.txt "$blaze" why
edit 1
name=edit timed tty taskset -c 0-7 "$blaze" build
edit 2
said edit.piped "$blaze" build
said why.txt "$blaze" why
said status.txt "$blaze" status
said noop-events.jsonl "$blaze" build --events json
for i in 1 2 3 4 5; do name=noop timed said noop.$i "$blaze" build; done
cp "$BLAZE_HOME"/workers/*/log "$out/worker.log" 2>/dev/null
"$blaze" stop >/dev/null 2>&1 || true
cp -r /root/tui/ripgrep "$work/again"
cd "$work/again"
name=warm timed tty taskset -c 0-7 "$blaze" build
"$blaze" stop >/dev/null 2>&1 || true
echo DONE >>"$out/exits"
