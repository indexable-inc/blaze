# buildsrc.sh NAME REV: release blaze of REV (no llvm feature, rustup toolchain of the rev) into /root/perf/bin/NAME.
set -euo pipefail
export PATH=/nix/var/nix/profiles/default/bin:/run/current-system/sw/bin:$PATH
name=$1 rev=$2
wt=/root/perf/wt-build
[ -d $wt ] || git -C /root/perf/blaze worktree add -q --detach $wt "$rev"
cd $wt && git checkout -q --force --detach "$rev"
tc=$(cat harness/toolchain)
rustup toolchain install "$tc" --profile minimal -c rust-src >/dev/null 2>&1
export CARGO_TARGET_DIR=/root/perf/target-ab CARGO_INCREMENTAL=0 RUSTUP_TOOLCHAIN=$tc
s=$(date +%s)
nix develop --command bash -c "export RUSTUP_TOOLCHAIN=$tc PATH=/nix/var/nix/profiles/default/bin:\$PATH; cargo build -j 8 -p blaze-engine --release --locked"
out=/root/perf/bin/$name; rm -rf $out; mkdir -p $out
for b in blaze-engine blaze blaze-rustc; do cp $CARGO_TARGET_DIR/release/$b $out/ 2>/dev/null || true; done
echo "built $name ($rev) in $(( $(date +%s) - s ))s"
