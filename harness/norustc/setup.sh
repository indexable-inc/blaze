#!/usr/bin/env bash
# Prepares $NR_ROOT (default ~/no-rustc) on a Linux dev host for count.py: blaze at a
# commit, built with the real toolchain, and a copy of the pinned nightly (harness/toolchain)
# whose rustc, rustdoc and cargo log every run (wrap.sh), linked as the rustup toolchain nr-pin.
# Remove with `rustup toolchain uninstall nr-pin` and rm -rf.
set -euo pipefail
L=${NR_ROOT:-$HOME/no-rustc}
REV=${REV:-origin/main}
HERE=$(cd "${0%/*}" && pwd)
TC=$(cat "$HERE/../toolchain")
pin=()
[ -n "${NR_CPUS:-}" ] && pin=(taskset -c "$NR_CPUS")
mkdir -p "$L/logs" "$L/apps"
cd "$L"
[ -d src ] || git clone -q https://github.com/indexable-inc/blaze src
git -C src fetch -q origin
git -C src checkout -q --detach "$REV"

counted() { # toolchain-dir name
  local from=$HOME/.rustup/toolchains/$1 to=$L/tc-$2
  if [ ! -d "$to" ]; then
    cp -a "$from" "$to"
    for t in rustc rustdoc cargo; do
      mv "$to/bin/$t" "$to/bin/$t.real"
      cp "$HERE/wrap.sh" "$to/bin/$t"
      chmod +x "$to/bin/$t"
    done
  fi
  rustup toolchain link "nr-$2" "$to"
}
counted "$TC-x86_64-unknown-linux-gnu" pin

cd "$L/src"
CARGO_TARGET_DIR=$L/target "${pin[@]}" cargo "+$TC" build -p blaze-engine --release --bins 2>&1 | tail -3
ls "$L/target/release/blaze-engine"
