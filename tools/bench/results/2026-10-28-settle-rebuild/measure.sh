#!/usr/bin/env bash
# The anim stand-in's warm load and the worker's memory, cold and warm (lane relax-images,
# blaze #1295 and #1299). OUT gets each phase's client output, the worker logs and summary.txt.
set -u
OUT=${OUT:-$PWD/out}
OLD=${OLD:?release before}
NEW=${NEW:?release under test}
ENGINE=${ENGINE:-}
mkdir -p "$OUT"
export RUST_BACKTRACE=0 BLAZE_STATS=1
export BLAZE_HOME=$OUT/../home BLAZE_CACHE=$OUT/../store
SRC=$(cd "$(dirname "$0")/anim-deps" && pwd)
P=$OUT/../anim
rm -rf "$P" && cp -r "$SRC" "$P"
install() {
  curl -fsSL https://ix.dev/blaze | sh -s -- --version "$1" >/dev/null 2>&1 || { echo "install $1 failed"; exit 1; }
  if [ -n "$ENGINE" ] && [ "$1" = "$NEW" ]; then cp "$ENGINE" "$HOME/.cargo/bin/blaze-engine"; fi
  blaze --version | head -1
}
rss() {
  for pid in $(pgrep -f blaze-engine); do
    s=/proc/$pid/status
    [ -r $s ] || continue
    echo "  $1 pid=$pid rss_mb=$(( $(awk '/VmRSS/{print $2}' $s) / 1024 )) hwm_mb=$(( $(awk '/VmHWM/{print $2}' $s) / 1024 )) $(tr '\0' ' ' < /proc/$pid/cmdline | cut -c1-60)"
  done
}
n=0
build() { # phase-name
  n=$((n+1))
  local s e
  s=$(date +%s%N)
  (cd "$P" && blaze build) > "$OUT/$n-$1.out" 2>&1
  local rc=$?
  e=$(date +%s%N)
  echo "$n $1 rc=$rc wall_ms=$(( (e - s) / 1000000 ))" | tee -a "$OUT/summary.txt"
  grep -E "Loaded|Resolved|Took|Cache|Built from|Finished|Rebuilt|Compiled .* fns" "$OUT/$n-$1.out" | sed 's/^/    /' | tee -a "$OUT/summary.txt"
}
settle() { # phase-name
  sleep 30
  rss "$1" | tee -a "$OUT/summary.txt"
}
edit() {
  sed -i "s/let _edit = [0-9]*u32;/let _edit = $1u32;/" "$P/src/main.rs"
}
stop() {
  for log in "$BLAZE_HOME"/workers/*/log "$BLAZE_HOME"/spares/*/log; do
    [ -s "$log" ] && cp "$log" "$OUT/$n-$1.$(basename "$(dirname "$(dirname "$log")")")-$(basename "$(dirname "$log")").log"
  done
  cp "$BLAZE_HOME"/daemon.log "$OUT/$n-$1.daemon.log" 2>/dev/null
  blaze stop >/dev/null 2>&1
  sleep 5
  pkill -9 -f blaze-engine; sleep 1
  rm -rf "$BLAZE_HOME/workers" "$BLAZE_HOME/spares"
}
round() { # label: a fresh daemon on what the store has, then a no-op and two edits
  build "$1"; settle "$1"
  build "$1-noop"
  edit 1; build "$1-edit1"; settle "$1-edit1"
  edit 2; build "$1-edit2"
  edit 0
  stop "$1"
}

echo "== $OLD, cold (empty store)" | tee -a "$OUT/summary.txt"
install "$OLD" | tee -a "$OUT/summary.txt"
rm -rf "$BLAZE_CACHE"
round old-cold
echo "== $NEW over $OLD's store: cold after upgrade" | tee -a "$OUT/summary.txt"
install "$NEW" | tee -a "$OUT/summary.txt"
round upgrade
for i in 1 2; do
  echo "== $NEW, fresh daemon, warm store ($i)" | tee -a "$OUT/summary.txt"
  round warm$i
done
echo "== $NEW, cold (empty store)" | tee -a "$OUT/summary.txt"
rm -rf "$BLAZE_CACHE"
round cold
echo "== $NEW, fresh daemon, warm store, BLAZE_MEMORY" | tee -a "$OUT/summary.txt"
export BLAZE_MEMORY=$OUT/memory-warm
round warm-memory
echo "== $NEW, cold, BLAZE_MEMORY" | tee -a "$OUT/summary.txt"
export BLAZE_MEMORY=$OUT/memory-cold
rm -rf "$BLAZE_CACHE"
round cold-memory
echo DONE | tee -a "$OUT/summary.txt"
