#!/run/current-system/sw/bin/bash
# Build branch norustc-refused in /dev/shm/nrr/me and count the std unit's refusals.
export PATH=/run/current-system/sw/bin:$HOME/.cargo/bin:$PATH
R=/dev/shm/nrr/me; mkdir -p $R; cd $R
[ -d src ] || git clone -q https://github.com/indexable-inc/blaze src
cd src && git fetch -q origin norustc-refused && git checkout -q -f --detach FETCH_HEAD
# measurement-only: RA_JIT_SKIPPED appends
python3 - <<'PY'
p="crates/ra-jit/src/main.rs"; s=open(p).read()
old="""        if let Err(error) = std::fs::write(&path, all) {"""
new="""        let appended = std::fs::OpenOptions::new().append(true).create(true).open(&path);
        if let Err(error) = appended.and_then(|mut it| std::io::Write::write_all(&mut it, all.as_bytes())) {"""
if old in s: open(p,"w").write(s.replace(old,new))
PY
CARGO_TARGET_DIR=$R/target nix develop --command cargo build -p ra-jit --release --features llvm --locked 2>&1 | tail -2
O=$R/std; rm -rf $O; mkdir -p $O
export RUSTUP_TOOLCHAIN=$(cat harness/toolchain) RA_JIT_HOME=$O/home RA_JIT_CACHE=$O/store RA_JIT_SKIPPED=$O/../std.skipped RUST_BACKTRACE=0
rm -f $O/../std.skipped
nix develop --command $R/target/release/ra-jit std $O/remote "$(rustc --print sysroot)" > $O/log 2>&1
echo "STD RC=$?"
