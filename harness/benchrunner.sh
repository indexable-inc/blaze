#!/usr/bin/env bash
# benchrunner.sh: makes this machine the self-hosted runner of indexable-inc/blaze that the
# bench-readme and bench-bun workflows run one platform on (harness/benchplatforms.py):
#
#   linux-x86-64    labels self-hosted, linux, x64, bench-x86
#   linux-aarch64   labels self-hosted, linux, ARM64, bench-arm64
#   macos-arm64     labels self-hosted, macOS, ARM64, bench-macos
#
#   scp harness/benchrunner.sh HOST:
#   gh api -X POST repos/indexable-inc/blaze/actions/runners/registration-token --jq .token \
#     | ssh HOST bash benchrunner.sh linux-aarch64 "'AWS m8g.4xlarge'"
#
# Reads the registration token (valid one hour, needs repo admin) from stdin; the arguments are
# the platform (the machine's own, checked) and the name of the machine for the results
# (BLAZE_BENCH_MACHINE). One argument is the name, on linux-x86-64, as before there were
# platforms. Installs the runner in ~/actions-runner and runs it at boot: on Linux from a user
# systemd unit, blaze-bench-runner.service, that lingers; on macOS as the runner's own launchd
# service (svc.sh), which needs the user logged in, and the Mac kept from sleeping.
# Needs curl, tar and Nix with flakes; on NixOS the runner's own binaries run through nix-ld.
# Run again to re-register (--replace keeps the name). Then add the platform to the repository
# variable BENCH_RUNNERS, which turns its jobs on.
set -euo pipefail
export PATH=/run/current-system/sw/bin:/run/wrappers/bin:/nix/var/nix/profiles/default/bin:$PATH
if [ $# -eq 1 ]; then set -- linux-x86-64 "$1"; fi
platform=${1:?name the platform: linux-x86-64, linux-aarch64 or macos-arm64}
machine=${2:?name the machine, e.g. "AWS m8g.4xlarge"}
case "$platform/$(uname -s)/$(uname -m)" in
  linux-x86-64/Linux/x86_64) label=bench-x86 arch=linux-x64 ;;
  linux-aarch64/Linux/aarch64) label=bench-arm64 arch=linux-arm64 ;;
  macos-arm64/Darwin/arm64) label=bench-macos arch=osx-arm64 ;;
  *) echo "this machine is $(uname -s) $(uname -m), not $platform" >&2; exit 1 ;;
esac
read -r token
[ -n "$token" ] || { echo "no registration token on stdin" >&2; exit 1; }
dir=$HOME/actions-runner
version=$(curl -fsSL https://api.github.com/repos/actions/runner/releases/latest | sed -n 's/.*"tag_name": *"v\([^"]*\)".*/\1/p')
mkdir -p "$dir"
cd "$dir"
if [ ! -x ./run.sh ] || [ "$(cat .version 2>/dev/null)" != "$version" ]; then
  curl -fsSL "https://github.com/actions/runner/releases/download/v$version/actions-runner-$arch-$version.tar.gz" | tar xz
  echo "$version" > .version
fi
# The runner is .NET: without ICU on the host it has to be told to do without.
export DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1
./config.sh --unattended --replace --url https://github.com/indexable-inc/blaze --token "$token" \
  --name "$(hostname -s)" --labels "$label" --work _work
cat > .env <<EOF
BLAZE_BENCH_MACHINE=$machine
DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1
EOF
if [ "$(uname -s)" = Darwin ]; then
  # The service's PATH is the runner's .path file: Nix's profile first, for `nix develop`.
  echo "$HOME/.nix-profile/bin:/nix/var/nix/profiles/default/bin:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin" > .path
  ./svc.sh install
  ./svc.sh start
  ./svc.sh status | head -5
  exit 0
fi
mkdir -p "$HOME/.config/systemd/user"
cat > "$HOME/.config/systemd/user/blaze-bench-runner.service" <<EOF
[Unit]
Description=GitHub Actions runner for blaze benchmarks ($label)
After=network-online.target

[Service]
WorkingDirectory=$dir
ExecStart=$dir/run.sh
Environment=PATH=$HOME/.nix-profile/bin:/nix/var/nix/profiles/default/bin:/run/current-system/sw/bin:/run/wrappers/bin
Environment=DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1
Restart=always
RestartSec=10

[Install]
WantedBy=default.target
EOF
systemctl --user daemon-reload
systemctl --user enable --now blaze-bench-runner.service
loginctl enable-linger "$USER" 2>/dev/null || sudo loginctl enable-linger "$USER"
systemctl --user --no-pager status blaze-bench-runner.service | head -5
