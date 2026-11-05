#!/usr/bin/env bash
# bench.sh REV [BACKEND]: build REV, then bench.py ripgrep at REV (its own harness and toolchain),
# pinned to cores 0-3 (the CI runner's 4), into /root/perf/res/REV-BACKEND.json. Cached.
set -euo pipefail
export PATH=/nix/var/nix/profiles/default/bin:/run/current-system/sw/bin:$PATH
rev=$(git -C /root/perf/blaze rev-parse --short=10 "$1")
backend=${2:-cranelift}
tag=${TAG:-$rev}
res=/root/perf/res/$tag-$backend.json
mkdir -p /root/perf/res /root/perf/logs
[ -s "$res" ] && { echo "have $res"; exit 0; }
bin=${BIN:-/root/perf/bin/$tag}
if [ ! -x $bin/blaze-engine ] && [ -s /root/perf/tars/$rev.tar ]; then mkdir -p $bin && tar -C $bin -xf /root/perf/tars/$rev.tar; fi
[ -x $bin/blaze-engine ] || { echo "no binary for $rev"; exit 125; }
[ -d /root/perf/wt ] || git -C /root/perf/blaze worktree add -q --detach /root/perf/wt "$rev"
cd /root/perf/wt && git checkout -q --force --detach "$rev"
export BLAZE_PROJECTS=/root/blaze-projects
export BLAZE_BIN=$bin/blaze-engine BLAZE_LLVM_BIN=$bin/blaze-engine
work=/root/perf/work/$tag-$backend
rm -rf "$work"; mkdir -p "$work"
cd harness
tc=$(cat /root/perf/wt/harness/toolchain)
rustup toolchain install "$tc" --profile minimal -c rust-src >/dev/null 2>&1
taskset -c 0-3 python3 bench.py ripgrep --backend $backend --out $res.tmp --work $work \
  > /root/perf/logs/$tag-$backend.log 2>&1 || echo "bench exit $? (see log)"
pkill -f "^$bin/" || true
find "$work" -mindepth 1 -maxdepth 1 ! -name "std-*" -exec rm -rf {} +
[ -s "$res.tmp" ] && mv "$res.tmp" "$res"
python3 /root/perf/summ.py "$res" || true
