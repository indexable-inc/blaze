export PATH=/run/current-system/sw/bin:$PATH
mkdir -p /root/np/bench
export HOME=/root/hr
if [ ! -x $HOME/.cargo/bin/rustup ]; then
  curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --default-toolchain nightly-2026-10-27 --profile minimal -c rust-src
  $HOME/.cargo/bin/rustup toolchain install 1.80.0 --profile minimal
fi
$HOME/.cargo/bin/rustup show
echo "== rustup"
PATH=$HOME/.cargo/bin:/run/current-system/sw/bin python3 /root/np/bench.py rustup
echo "== norust"
export HOME=/root/hn
mkdir -p $HOME
unshare -m bash -c '
for f in $(which -a rustc cargo rustdoc rustup 2>/dev/null | sort -u); do mount --bind /dev/null "$f"; done
which -a rustc cargo; rustc --version; echo "(rustc hidden above)"
PATH=/run/current-system/sw/bin python3 /root/np/bench.py norust'
echo BENCHED
