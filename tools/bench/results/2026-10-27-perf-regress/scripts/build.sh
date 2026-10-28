#!/usr/bin/env bash
# build.sh REV: release blaze (llvm feature) of REV into /root/perf/bin/REV, as CI's build job.
set -euo pipefail
export PATH=/nix/var/nix/profiles/default/bin:/run/current-system/sw/bin:$PATH
rev=$(git -C /root/perf/blaze rev-parse --short=10 "$1")
out=/root/perf/bin/$rev
[ -x "$out/blaze" ] && [ -x "$out/blaze-proc-macro-srv" -o -e "$out/no-server" ] && { echo "have $out"; exit 0; }
wt=/root/perf/wt
if [ ! -d $wt ]; then git -C /root/perf/blaze worktree add -q --detach $wt "$rev"; fi
cd $wt && git checkout -q --force --detach "$rev" && git clean -qfdx -e target
export CARGO_TARGET_DIR=/root/perf/target CARGO_INCREMENTAL=0
start=$(date +%s)
nix develop --command cargo build -j 16 -p blaze-engine --release --features llvm --locked
nix develop --command bash -c 'export CARGO=$(command -v cargo); python3 .github/scripts/build_release.py --server-beside $CARGO_TARGET_DIR/release/blaze-engine' || { echo "no server at $rev"; mkdir -p "$out"; touch "$out/no-server"; }
mkdir -p "$out"
for b in blaze-engine blaze blaze-rustc blaze-proc-macro-srv; do
  [ -f $CARGO_TARGET_DIR/release/$b ] && cp $CARGO_TARGET_DIR/release/$b "$out/"
done
echo "built $rev in $(( $(date +%s) - start ))s"
