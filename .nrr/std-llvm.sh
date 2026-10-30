#!/run/current-system/sw/bin/bash
export PATH=/run/current-system/sw/bin:$HOME/.cargo/bin:$PATH
R=/dev/shm/nrr; O=$R/stdllvm; rm -rf $O; mkdir -p $O
export RUSTUP_TOOLCHAIN=$(cat $R/src/harness/toolchain) RA_JIT_HOME=$O/home RA_JIT_CACHE=$O/store RA_JIT_BACKEND=llvm RA_JIT_SKIPPED=$O/std.skipped RUST_BACKTRACE=0
cd $R/src && nix develop --command $R/bin/ra-jit std $O/remote "$(rustc --print sysroot)" > $O/log 2>&1
echo "RC=$?" >> $O/log
