#!/usr/bin/env bash
# build.sh NAME...: blaze-engine --release of each shipped branch astral-ship-NAME (from /root/ax/ship.bundle),
# built one after another in /root/ax/blaze so each is incremental; binaries in /root/ax/bin/NAME/.
set -euo pipefail
export PATH=/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:$PATH HOME=/root RUST_BACKTRACE=0
[ -d /root/ax/blaze/.git ] || git clone -q /root/ax/ship.bundle /root/ax/blaze
cd /root/ax/blaze
git fetch -q -u /root/ax/ship.bundle '+refs/heads/*:refs/heads/*'
for name in "$@"; do
  git checkout -q -f astral-ship-$name
  s=$(date +%s)
  nix --extra-experimental-features 'nix-command flakes' develop -c cargo build -q -j12 -p blaze-engine --release
  mkdir -p /root/ax/bin/$name; cp target/release/blaze-engine /root/ax/bin/$name/
  if [ -f target/release/blaze ]; then cp target/release/blaze /root/ax/bin/$name/; else ln -sf blaze-engine /root/ax/bin/$name/blaze; fi
  echo "BUILT $name in $(( $(date +%s) - s ))s"
done
