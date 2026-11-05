#!/usr/bin/env bash
# Where a no-op build's time goes, before and after (lane tui): profile.sh BEFORE AFTER OUT
# BEFORE and AFTER are directories with blaze, blaze-engine and blaze-rustc. For each, a fresh
# anim with a BLAZE_HOME of its own, built once and three times more, then: BLAZE_STATS of 5
# no-op builds, the syscalls of one no-op build (the client, and the daemon and its worker
# attached), and BLAZE_STATS of 2 no-op builds with `--events json`.
set -uo pipefail
before=$1 after=$2 out=$3
mkdir -p "$out"
for arm in before after; do
  dir=$before
  [ "$arm" = after ] && dir=$after
  work=$(mktemp -d)
  mkdir -p "$work/anim/src"
  printf '[package]\nname = "anim"\nversion = "0.1.0"\nedition = "2021"\n' >"$work/anim/Cargo.toml"
  printf 'fn main() {\n    println!("anim {}", 6 * 7);\n}\n' >"$work/anim/src/main.rs"
  export BLAZE_HOME="$work/home"
  cd "$work/anim"
  "$dir/blaze" build >"$out/$arm.cold" 2>&1
  for _ in 1 2 3; do "$dir/blaze" build >/dev/null 2>&1; done
  for i in 1 2 3 4 5; do BLAZE_STATS=1 "$dir/blaze" build >"$out/$arm.stats.$i" 2>&1; done
  "$dir/blaze" status >"$out/$arm.status" 2>&1
  pids=$(sed -n 's/^ *\(daemon\|worker\): pid \([0-9]*\).*/\2/p' "$out/$arm.status" | tr '\n' ' ')
  attach=()
  for pid in $pids; do attach+=(-p "$pid"); done
  sudo strace -f -tt -T -o "$out/$arm.daemon.strace" "${attach[@]}" &
  tracer=$!
  sleep 2
  strace -f -tt -T -o "$out/$arm.client.strace" "$dir/blaze" build >/dev/null 2>&1
  sleep 1
  sudo kill -INT "$tracer"
  wait "$tracer"
  for i in 1 2; do BLAZE_STATS=1 "$dir/blaze" build --events json >"$out/$arm.events.$i" 2>&1; done
  "$dir/blaze" stop >/dev/null 2>&1
  cd /
  rm -rf "$work"
done
