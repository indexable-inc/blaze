#!/usr/bin/env bash
# mem.sh REV: cold `blaze build` of ripgrep with BLAZE_MEMORY set (salsa ingredient tables and
# resident/live checkpoints per step), sampling every blaze process's RSS each 200 ms for the peak by
# process kind. Out: /root/perf/mem/REV/.
set -uo pipefail
export PATH=/nix/var/nix/profiles/default/bin:/run/current-system/sw/bin:$PATH
rev=$(git -C /root/perf/blaze rev-parse --short=10 "$1")
bin=/root/perf/bin/$rev
out=/root/perf/mem/$rev; rm -rf "$out"; mkdir -p "$out/tables"
p=/root/perf/mem-ws/$rev; rm -rf "$p"; mkdir -p "$p"
cp -r /root/blaze-projects/ripgrep "$p/rg"; rm -rf "$p/rg/target" "$p/rg/.git"
wt=/root/perf/wt-prof
[ -d $wt ] || git -C /root/perf/blaze worktree add -q --detach $wt "$rev"
git -C $wt checkout -q --force --detach "$rev"
tc=$(cat $wt/harness/toolchain)
std=$(ls -d /root/perf/work/$rev-cranelift/std-cranelift 2>/dev/null | head -1)
export BLAZE_HOME=$p/home BLAZE_CACHE=$p/store BLAZE_BACKEND=cranelift RUSTUP_TOOLCHAIN=$tc
export BLAZE_STD_FROM=$std RUST_BACKTRACE=0 CARGO_TERM_COLOR=never BLAZE_MEMORY=$out/tables
export PATH=$bin:$PATH
cd $p/rg
( while :; do
    ts=$(date +%s.%N)
    for s in /proc/[0-9]*/status; do
      pid=${s#/proc/}; pid=${pid%/status}
      exe=$(readlink /proc/$pid/exe 2>/dev/null) || continue
      case $exe in $bin/*) ;; *) continue ;; esac
      rss=$(awk '/VmRSS/{print $2}' $s 2>/dev/null)
      echo "$ts $pid $rss $(tr '\0' ' ' < /proc/$pid/cmdline 2>/dev/null | cut -c1-160)"
    done
    sleep 0.2
  done ) > $out/rss.samples &
sampler=$!
s=$(date +%s%N)
taskset -c 0-3 blaze build > $out/cold.out 2> $out/cold.err; echo "cold exit $? in $(( ($(date +%s%N) - s) / 1000000 ))ms"
for i in $(seq 600); do blaze status 2>&1 | grep -q "$p/rg: ready" && break; sleep 0.5; done
sleep 3
taskset -c 0-3 blaze build > $out/noop.out 2>&1
kill $sampler
for w in $BLAZE_HOME/workers/*/; do cp $w/log $out/worker-$(basename $w).log 2>/dev/null; done
cp $BLAZE_HOME/*.log $out/ 2>/dev/null
blaze stop >/dev/null 2>&1
echo done
