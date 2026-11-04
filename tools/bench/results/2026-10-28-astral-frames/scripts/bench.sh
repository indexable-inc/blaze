#!/usr/bin/env bash
# bench.sh OUT PROJECT BIN...: bench.py of PROJECT with each blaze binary in turn, twice, interleaved.
set -uo pipefail
export PATH=/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:$PATH HOME=/root TMPDIR=/root/ax/tmp/bench CARGO_BUILD_JOBS=8
mkdir -p $TMPDIR
out=/root/ax/out/$1 project=$2; shift 2
mkdir -p $out
cd /root/ax/trees/langtests
for round in 1 2; do
  for bin in "$@"; do
    echo "== $bin round $round $(date +%T)"
    work=/root/ax/bench/$bin-$round; rm -rf $work
    BLAZE_BIN=/root/ax/bin/$bin/blaze-engine BLAZE_BENCH_MACHINE=ix-astral-arm64-16 timeout 7200 \
      nix --extra-experimental-features 'nix-command flakes' develop -c \
      python3 harness/bench.py $project --backend cranelift --runs 5 --out $out/$bin-$round.json --work $work --keep \
      < /dev/null > $out/$bin-$round.log 2>&1
    echo "== $bin round $round exit $? $(date +%T)"
  done
done
