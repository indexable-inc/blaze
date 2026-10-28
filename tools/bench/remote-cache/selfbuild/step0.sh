#!/bin/bash
# Bootstrap: cargo builds main's blaze (also arm 1's warm target dir), its unit tests, and the
# proc-macro server; the binaries are copied to /root/rb/bin.
export PATH=/run/current-system/sw/bin:/usr/bin:$PATH HOME=/root
set -u
cd /root/rb
[ -d c ] || cp -a src c
cd c
t() { local s=$(date +%s.%N); "$@"; local rc=$?; echo "STEP $1 ${*:2} rc=$rc secs=$(awk "BEGIN { print $(date +%s.%N) - $s }")"; return $rc; }
t nix develop --command true
t nix develop --command cargo build -p blaze-engine --release --features llvm --locked || exit 1
t nix develop --command python3 .github/scripts/build_release.py --server-beside target/release/blaze-engine || exit 1
mkdir -p /root/rb/bin
for b in blaze-engine blaze blaze-rustc blaze-proc-macro-srv; do
  cp target/release/$b /root/rb/bin/ || echo "missing $b"
done
ls -la /root/rb/bin
# 16 jobs of test binaries outgrew the VM's memory.
CARGO_BUILD_JOBS=4 t nix develop --command python3 harness/unittests.py
echo STEP0-DONE
