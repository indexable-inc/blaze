#!/usr/bin/env bash
# noopperf2.sh NAME: bin/NAME on ripgrep, cold, wait until the daemon's processes stop using CPU (warm-up done),
# then 30 timed no-ops and `perf record -a -g` over 30 more; reports of the client's time (whole::build down).
# Out: /root/perf/noopperf2/NAME/.
set -uo pipefail
export PATH=/nix/var/nix/profiles/default/bin:/run/current-system/sw/bin:$PATH
name=$1
bin=/root/perf/bin/$name
out=/root/perf/noopperf2/$name; rm -rf "$out"; mkdir -p "$out"
p=/root/perf/noopperf2-ws/$name; rm -rf "$p"; mkdir -p "$p"
cp -r /root/blaze-projects/ripgrep "$p/rg"; rm -rf "$p/rg/target" "$p/rg/.git"
tc=$(cat /root/perf/wt-build/harness/toolchain)
export BLAZE_HOME=$p/home BLAZE_CACHE=$p/store BLAZE_BACKEND=cranelift RUSTUP_TOOLCHAIN=$tc
export BLAZE_STD_FROM=/root/perf/work/stdkey-cranelift/std-cranelift RUST_BACKTRACE=0 CARGO_TERM_COLOR=never
export PATH=$bin:$PATH
cd $p/rg
taskset -c 0-3 blaze build > $out/cold.out 2>&1; echo "cold exit $?"
for i in $(seq 600); do blaze status 2>&1 | grep -q "$p/rg: ready" && break; sleep 0.5; done
cpu() { ps -C blaze -o times= | awk '{s+=$1} END {print s+0}'; }
last=$(cpu); for i in $(seq 120); do sleep 5; now=$(cpu); [ "$now" = "$last" ] && break; last=$now; done
echo "idle after $((i * 5)) s"
for i in 1 2 3; do taskset -c 0-3 blaze build >/dev/null 2>&1; done
for r in 1 2 3; do s=$(date +%s%N); for i in $(seq 30); do taskset -c 0-3 blaze build >/dev/null 2>&1; done; echo "30 noops $(( ($(date +%s%N) - s) / 1000000 ))ms"; done | tee $out/times.txt
perf record -a -C 0-3 -g -F 9999 -o $out/perf.data -- bash -c 'for i in $(seq 30); do taskset -c 0-3 blaze build >/dev/null 2>&1; done' 2> $out/perf-record.err
perf report -i $out/perf.data --no-children --sort comm --stdio 2>/dev/null | grep -v '^#' | grep -v '^$' | head -15 > $out/by-comm.txt
perf report -i $out/perf.data --children --comm blaze --sort sym --stdio -g none --percent-limit 0.7 2>/dev/null | grep -v '^#' | grep -v '^$' | head -150 > $out/blaze-inclusive.txt
perf report -i $out/perf.data --no-children --comm blaze --sort dso,sym --stdio -g none 2>/dev/null | grep -v '^#' | grep -v '^$' | head -60 > $out/blaze-self.txt
rm -f $out/perf.data
blaze stop >/dev/null 2>&1
echo done
