# heapprof.sh BRANCH: release blaze of BRANCH (from fix.bundle) with the heap-profile feature into bin/heapprof,
# then a cold ripgrep build with BLAZE_MEMORY and jemalloc profiling; folds the worker's build.prof with jeprof.
# Out: /root/perf/heap/.
export PATH=/nix/var/nix/profiles/default/bin:/run/current-system/sw/bin:$PATH
b=$1
cd /root/perf/blaze && git fetch -q -f ../fix.bundle "$b:$b"
wt=/root/perf/wt-build
cd $wt && git checkout -q --force --detach "$b"
tc=$(cat harness/toolchain)
export CARGO_TARGET_DIR=/root/perf/target-heap CARGO_INCREMENTAL=0
nix develop --command bash -c "export RUSTUP_TOOLCHAIN=$tc PATH=/nix/var/nix/profiles/default/bin:\$PATH; cargo build -j 8 -p blaze-engine --release --locked --features heap-profile" 2>&1 | tail -2
bin=/root/perf/bin/heapprof; rm -rf $bin; mkdir -p $bin
for x in blaze-engine blaze blaze-rustc; do cp $CARGO_TARGET_DIR/release/$x $bin/ 2>/dev/null; done
out=/root/perf/heap; rm -rf $out; mkdir -p $out/tables
p=/root/perf/heap-ws; rm -rf $p; mkdir -p $p
cp -r /root/blaze-projects/ripgrep $p/rg; rm -rf $p/rg/target $p/rg/.git
(
  export BLAZE_HOME=$p/home BLAZE_CACHE=$p/store BLAZE_BACKEND=cranelift RUSTUP_TOOLCHAIN=$tc
  export BLAZE_STD_FROM=/root/perf/work/stdkey-cranelift/std-cranelift CARGO_TERM_COLOR=never
  export BLAZE_MEMORY=$out/tables _RJEM_MALLOC_CONF=prof:true,lg_prof_sample:19
  export PATH=$bin:$PATH
  cd $p/rg
  taskset -c 0-3 blaze build > $out/cold.out 2>&1; echo "cold exit $?"
  for i in $(seq 600); do blaze status 2>&1 | grep -q "$p/rg: ready" && break; sleep 0.5; done
  for w in $BLAZE_HOME/workers/*/; do cp $w/log $out/worker-$(basename $w).log; done
  blaze stop >/dev/null 2>&1
)
ls $out/tables
jeprof=$(nix build --no-link --print-out-paths nixpkgs#jemalloc.bin 2>/dev/null || nix build --no-link --print-out-paths nixpkgs#jemalloc 2>/dev/null)
echo "jeprof from $jeprof"
for prof in $out/tables/*.prof; do
  nix shell nixpkgs#perl nixpkgs#binutils -c perl $(ls $jeprof/bin/jeprof | head -1) --collapsed $bin/blaze-engine $prof > ${prof%.prof}.folded 2> ${prof%.prof}.jeprof.err
  echo "$prof folded: $(wc -l < ${prof%.prof}.folded) stacks"
done
echo HEAP DONE
