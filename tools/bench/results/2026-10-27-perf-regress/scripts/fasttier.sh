# fasttier.sh BRANCH...: for each branch (from /root/perf/fix.bundle): cargo fmt --check, then
# `cargo test --release -p blaze-engine` (docs/contributing.md "What a change needs") on the rev's toolchain.
export PATH=/nix/var/nix/profiles/default/bin:/run/current-system/sw/bin:$PATH
cd /root/perf/blaze && for b in "$@"; do git fetch -q -f ../fix.bundle "$b:$b"; done
wt=/root/perf/wt-test
for b in "$@"; do
  [ -d $wt ] || git -C /root/perf/blaze worktree add -q --detach $wt "$b"
  cd $wt && git checkout -q --force --detach "$b"
  tc=$(cat harness/toolchain)
  rustup toolchain install "$tc" --profile minimal -c rust-src -c rustfmt >/dev/null 2>&1
  export CARGO_TARGET_DIR=/root/perf/target-test CARGO_INCREMENTAL=0 RUSTUP_TOOLCHAIN=$tc
  s=$(date +%s)
  echo "== $b $(git rev-parse --short=10 HEAD)"
  nix develop --command bash -c "export RUSTUP_TOOLCHAIN=$tc PATH=/nix/var/nix/profiles/default/bin:\$PATH CARGO=\$(PATH=/nix/var/nix/profiles/default/bin:\$PATH command -v cargo)
    cargo fmt --all --check && echo 'fmt ok'
    cargo test -j 8 --release -p blaze-engine --locked 2>&1 | grep -E '^(test result|failures:|    [a-z_:]+$|error)' "
  echo "== $b exit $? in $(( $(date +%s) - s ))s"
done
echo FASTTIER DONE
