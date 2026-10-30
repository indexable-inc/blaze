#!/run/current-system/sw/bin/bash
# norustc-refused lane: clone blaze at REV, release build with LLVM, and blaze-proc-macro-srv.
set -uo pipefail
export PATH=/run/current-system/sw/bin:$HOME/.cargo/bin:$PATH
R=/dev/shm/nrr
REV=${REV:-origin/main}
mkdir -p $R/logs
cd $R
[ -d src ] || git clone -q https://github.com/indexable-inc/blaze src
git -C src fetch -q origin && git -C src checkout -q -f --detach "$REV"
cd src
start=$(date +%s)
CARGO_TARGET_DIR=$R/target nix develop --command cargo build -p ra-jit --release --features llvm --locked
rc=$?
echo "BUILD RC=$rc secs=$(( $(date +%s)-start ))"
[ $rc = 0 ] || exit $rc
mkdir -p $R/bin && cp -f $R/target/release/{ra-jit,blaze,cargo-blaze,cargo-ra-jit,ra-jit-rustc,ra-jit-rustc-first} $R/bin/ 2>/dev/null
cd vendor/proc-macro-srv && nix develop $R/src --command $R/bin/ra-jit . --crate rust_analyzer_proc_macro_srv --crate proc_macro --out $R/work-server.o --exe $R/bin/blaze-proc-macro-srv
echo "SERVER RC=$?"
