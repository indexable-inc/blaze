#!/usr/bin/env bash
# abnoop.sh NAME REV (binary bin/NAME, harness toolchain of REV): warm daemon on ripgrep, then where a no-op `blaze build` spends its time:
# timed runs, strace -f of the client (execve/connect/openat/wait, with times). Out: /root/perf/noop/REV/.
set -uo pipefail
export PATH=/nix/var/nix/profiles/default/bin:/run/current-system/sw/bin:$PATH
name=$1
rev=$(git -C /root/perf/blaze rev-parse --short=10 "$2")
bin=/root/perf/bin/$name
out=/root/perf/abnoop/$name; rm -rf "$out"; mkdir -p "$out"
p=/root/perf/abnoop-ws/$name; rm -rf "$p"; mkdir -p "$p"
cp -r /root/blaze-projects/ripgrep "$p/rg"; rm -rf "$p/rg/target" "$p/rg/.git"
wt=/root/perf/wt-prof
[ -d $wt ] || git -C /root/perf/blaze worktree add -q --detach $wt "$rev"
git -C $wt checkout -q --force --detach "$rev"
tc=$(cat $wt/harness/toolchain)
rustup toolchain install "$tc" --profile minimal -c rust-src >/dev/null 2>&1
std=$(ls -d /root/perf/work/$rev-cranelift/std-cranelift 2>/dev/null | head -1)
export BLAZE_HOME=$p/home BLAZE_CACHE=$p/store BLAZE_BACKEND=cranelift RUSTUP_TOOLCHAIN=$tc
export BLAZE_STD_FROM=$std RUST_BACKTRACE=0 CARGO_TERM_COLOR=never
export PATH=$bin:$PATH
cd $p/rg
t() { local s=$(date +%s%N); "$@"; local e=$(date +%s%N); echo $(( (e - s) / 1000000 ))ms; }
echo "== cold"; taskset -c 0-3 blaze build > $out/cold.out 2> $out/cold.err; echo "exit $?"
for i in $(seq 600); do blaze status 2>&1 | grep -q "$p/rg: ready" && break; sleep 0.5; done
for i in 1 2 3; do taskset -c 0-3 blaze build >/dev/null 2>&1; done
for r in 1 2 3; do s=$(date +%s%N); for i in $(seq 30); do taskset -c 0-3 blaze build >/dev/null 2>&1; done; echo "30 noops $(( ($(date +%s%N) - s) / 1000000 ))ms"; done | tee $out/times.txt
for r in 1 2 3; do s=$(date +%s%N); for i in $(seq 30); do taskset -c 0-3 sh -c "cargo --version" >/dev/null 2>&1; done; echo "30 rustup cargo --version $(( ($(date +%s%N) - s) / 1000000 ))ms"; done | tee -a $out/times.txt
blaze stop >/dev/null 2>&1
echo done
