export PATH=/run/current-system/sw/bin:$PATH
set -x
cd /root/sc
patch -R -p1 < /root/closure.patch || exit 1
patch -p1 < /root/bodyref.patch || exit 1
nix develop --command cargo build -j 24 -p blaze-engine --release --features llvm --locked || exit 1
mkdir -p /root/binC && cp target/release/blaze target/release/blaze-engine target/release/blaze-rustc /root/binC/
timeout 2400 nix develop --command cargo test -j 24 -p hir-ide --lib -- tests::mir::eval:: tests::consteval > /root/ac-test.log 2>&1
tail -6 /root/ac-test.log
for case in ce_sha256 ce_toml ce_inflate const_heap; do
  for arm in A C A C; do
    rm -rf /root/w && cp -r /root/sc/harness/smoke/$case /root/w && cd /root/w
    export BLAZE_HOME=/root/h-$arm; rm -rf $BLAZE_HOME
    s=$(date +%s)
    timeout 2400 /root/bin$arm/blaze run --release > /root/out 2>/root/err; code=$?
    e=$(date +%s)
    /root/bin$arm/blaze stop >/dev/null 2>&1
    ok=$(cmp -s /root/out /root/w/expected.txt && echo correct || echo WRONG)
    echo "AC $case $arm $((e - s)) s exit $code $ok" >> /root/ac.txt
  done
done
echo ACDONE >> /root/ac.txt
