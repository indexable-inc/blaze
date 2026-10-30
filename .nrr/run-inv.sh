#!/run/current-system/sw/bin/bash
# Rebuild ra-jit (measurement patch), then inventory.py OUT NAMES... in the bench shell.
export PATH=/run/current-system/sw/bin:$HOME/.cargo/bin:$PATH
R=/dev/shm/nrr
OUT=$1; shift
cd $R/src
CARGO_TARGET_DIR=$R/target nix develop --command cargo build -p ra-jit --release --features llvm --locked 2>&1 | tail -2
cp -f $R/target/release/{ra-jit,blaze,cargo-blaze,cargo-ra-jit,ra-jit-rustc,ra-jit-rustc-first} $R/bin/
export RA_JIT_BIN=$R/bin/ra-jit NRR_HARNESS=$R/src/harness RA_JIT_PROJECTS=$HOME/ra-jit-projects
cd $R
nix develop $R/src#bench --command python3 -I $R/inventory.py "$OUT" "$@"
echo "INVENTORY RC=$?"
