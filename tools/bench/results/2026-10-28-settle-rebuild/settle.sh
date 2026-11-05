#!/usr/bin/env bash
# The settle swap of blaze #1299 on the anim stand-in, one engine: a cold build, an edit on
# the worker that built it, the swap, edits on the worker started from the store; then a
# fresh daemon on the warm store for reference. OUT gets each phase's output and summary.txt.
set -u
OUT=${OUT:-$PWD/out}
ENGINE=${ENGINE:?engine under test}
REL=${REL:?release whose blaze client is used}
mkdir -p "$OUT"
export RUST_BACKTRACE=0 BLAZE_STATS=1
export BLAZE_HOME=$OUT/../home BLAZE_CACHE=$OUT/../store
SRC=$(cd "$(dirname "$0")/anim-deps" && pwd)
P=$OUT/../anim
rm -rf "$P" && cp -r "$SRC" "$P"
curl -fsSL https://ix.dev/blaze | sh -s -- --version "$REL" >/dev/null 2>&1 || { echo "install failed"; exit 1; }
cp "$ENGINE" "$HOME/.cargo/bin/blaze-engine"
blaze --version | head -1 | tee -a "$OUT/summary.txt"
S=$OUT/summary.txt
rss() {
  for pid in $(pgrep -f blaze-engine); do
    s=/proc/$pid/status
    [ -r $s ] || continue
    echo "  $1 pid=$pid rss_mb=$(( $(awk '/VmRSS/{print $2}' $s) / 1024 )) hwm_mb=$(( $(awk '/VmHWM/{print $2}' $s) / 1024 )) $(tr '\0' ' ' < /proc/$pid/cmdline | cut -c1-60)"
  done | tee -a "$S"
}
n=0
build() {
  n=$((n+1))
  local s e
  s=$(date +%s%N)
  (cd "$P" && blaze build) > "$OUT/$n-$1.out" 2>&1
  local rc=$?
  e=$(date +%s%N)
  echo "$n $1 rc=$rc wall_ms=$(( (e - s) / 1000000 ))" | tee -a "$S"
  grep -E "Loaded|Resolved|Took|Cache|Finished|Rebuilt|Compiled .* fns|Fresh" "$OUT/$n-$1.out" | sed 's/^/    /' | tee -a "$S"
}
edit() { sed -i "s/let _edit = [0-9]*u32;/let _edit = $1u32;/" "$P/src/main.rs"; }
save() {
  for log in "$BLAZE_HOME"/workers/*/log* "$BLAZE_HOME"/spares/*/log; do
    [ -s "$log" ] && cp "$log" "$OUT/$1.$(basename "$(dirname "$(dirname "$log")")")-$(basename "$(dirname "$log")")-$(basename "$log")"
  done
  cp "$BLAZE_HOME"/daemon.log "$OUT/$1.daemon.log" 2>/dev/null
}
stop() {
  save "$1"
  blaze stop >/dev/null 2>&1
  sleep 5
  pkill -9 -f blaze-engine; sleep 1
  rm -rf "$BLAZE_HOME/workers" "$BLAZE_HOME/spares"
}
# Waits for the daemon to say a worker was started from the store and, after that, ready.
swapped() {
  local s; s=$(date +%s)
  while [ $(( $(date +%s) - s )) -lt 900 ]; do
    if grep -q "from the store\$" "$BLAZE_HOME/daemon.log" 2>/dev/null; then
      local after; after=$(sed -n '/from the store$/,$p' "$BLAZE_HOME/daemon.log" | grep -c ": ready")
      if [ "$after" -ge 1 ]; then
        echo "  swapped after $(( $(date +%s) - s ))s" | tee -a "$S"
        return 0
      fi
    fi
    sleep 2
  done
  echo "  no swap in 900s" | tee -a "$S"
  return 1
}

echo "== cold, then the swap" | tee -a "$S"
rm -rf "$BLAZE_CACHE"
build cold; rss cold
edit 1; build cold-edit1; rss cold-edit1
edit 2; build cold-edit2
save before-swap
swapped; sleep 10; rss settled
edit 3; build settled-edit3; rss settled-edit3
edit 4; build settled-edit4
edit 5; build settled-edit5; rss settled-edit5
edit 0
stop settled
echo "== fresh daemon, warm store" | tee -a "$S"
build warm; sleep 30; rss warm
edit 1; build warm-edit1; rss warm-edit1
edit 2; build warm-edit2
edit 3; build warm-edit3
edit 0
sleep 20; rss warm-after
stop warm
echo DONE | tee -a "$S"
