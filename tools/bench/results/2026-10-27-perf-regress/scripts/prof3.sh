#!/usr/bin/env bash
# prof2.sh REV: warm daemon on ripgrep, then a body edit with (1) strace -f of the client
# (execs, connects), (2) perf record attached to the daemon and its workers. Out: /root/perf/prof2/REV/.
set -uo pipefail
export PATH=/nix/var/nix/profiles/default/bin:/run/current-system/sw/bin:$PATH
rev=$(git -C /root/perf/blaze rev-parse --short=10 "$1")
bin=/root/perf/bin/$rev
out=/root/perf/prof3/$rev${SUFFIX:-}; rm -rf "$out"; mkdir -p "$out"
p=/root/perf/prof3-ws/$rev; rm -rf "$p"; mkdir -p "$p"
cp -r /root/blaze-projects/ripgrep "$p/rg"; rm -rf "$p/rg/target" "$p/rg/.git"
wt=/root/perf/wt-prof
[ -d $wt ] || git -C /root/perf/blaze worktree add -q --detach $wt "$rev"
git -C $wt checkout -q --force --detach "$rev"
tc=$(cat $wt/harness/toolchain)
rustup toolchain install "$tc" --profile minimal -c rust-src >/dev/null 2>&1
std=$(ls -d /root/perf/work/*-cranelift/std-cranelift 2>/dev/null | head -1)
export BLAZE_HOME=$p/home BLAZE_CACHE=$p/store BLAZE_BACKEND=cranelift RUSTUP_TOOLCHAIN=$tc
export BLAZE_STD_FROM=$std BLAZE_STATS=1 RUST_BACKTRACE=0 CARGO_TERM_COLOR=never
export PATH=$bin:$PATH
cd $p/rg
f=crates/core/main.rs; cp $f $f.orig
cat > $p/edit.py <<'EOF'
import sys
p = sys.argv[1]; o = "fn search(args: &HiArgs, mode: SearchMode) -> anyhow::Result<bool> {"
t = open(p).read(); open(p, "w").write(t.replace(o, o + "\n    let _bench_edit = 1u32;", 1))
EOF
edit() { python3 $p/edit.py $f; }
undo() { cp $f.orig $f; }
t() { local s=$(date +%s%N); "$@"; local e=$(date +%s%N); echo $(( (e - s) / 1000000 ))ms; }
echo "== cold"; taskset -c 0-3 blaze build > $out/cold.out 2> $out/cold.err; echo "exit $?"
for i in $(seq 600); do blaze status 2>&1 | grep -q "$p/rg: ready" && break; sleep 0.5; done
blaze status > $out/status.txt 2>&1
for i in 1; do echo "noop $(t taskset -c 0-3 blaze build 2>/dev/null)"; done | tee $out/times.txt
for i in 1 2; do if [ $((i % 2)) = 1 ]; then edit; else undo; fi; echo "body $(t taskset -c 0-3 blaze build 2>/dev/null)"; done | tee -a $out/times.txt
pids=$(pgrep -f "^$bin/blaze-engine" | tr '\n' ' ')
ps -o pid,args -p $pids | cut -c1-250 > $out/pids.txt
sargs=""; for x in $pids; do sargs="$sargs -p $x"; done
strace -f -tt -s 400 -e trace=execve,exit_group -o $out/daemon.strace $sargs &
sp=$!; sleep 1
edit; echo "strace-daemon body $(t taskset -c 0-3 blaze build 2>/dev/null)" | tee -a $out/times.txt
undo; echo "strace-daemon body back $(t taskset -c 0-3 blaze build 2>/dev/null)" | tee -a $out/times.txt
echo "strace-daemon noop $(t taskset -c 0-3 blaze build 2>/dev/null)" | tee -a $out/times.txt
sleep 0.5; kill -INT $sp; wait $sp
for w in $BLAZE_HOME/workers/*/; do cp $w/log $out/worker-$(basename $w).log 2>/dev/null; done
cp $BLAZE_HOME/*.log $out/ 2>/dev/null
ls -la $BLAZE_HOME > $out/home-ls.txt
blaze stop >/dev/null 2>&1
echo done
