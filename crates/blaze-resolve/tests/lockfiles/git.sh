#!/usr/bin/env bash
# Makes git-*.lock with cargo 1.99.0, the oracle: a workspace depending on one local git
# repository at a branch, a tag, a rev and its default branch, with an unused [patch]
# (versions 2 and 4) or a [replace] (versions 1 and 3), at the encoding its rust-version
# picks. Only version 4 encodes references, so only it is given ones that need encoding:
# cargo reads `?branch=a+b` of the older versions back as `a b`. Nothing is fetched from the
# network.
#
#   crates/blaze-resolve/tests/lockfiles/git.sh
set -euo pipefail
out=$(cd "$(dirname "$0")" && pwd)
g=/tmp/blaze-resolve-gen
rm -rf "$g" && mkdir -p "$g/repo"
export CARGO_HOME=$g/home

cd "$g/repo"
git init -q -b main .
mkdir -p dep1/src dep2/src && touch dep1/src/lib.rs dep2/src/lib.rs
printf '[package]\nname = "dep1"\nversion = "0.5.0"\nedition = "2018"\n' > dep1/Cargo.toml
printf '[package]\nname = "dep2"\nversion = "0.1.0"\nedition = "2018"\n' > dep2/Cargo.toml
printf '[workspace]\nmembers = ["dep1", "dep2"]\n' > Cargo.toml
git add -A
GIT_AUTHOR_DATE=2026-10-25T00:00:00Z GIT_COMMITTER_DATE=2026-10-25T00:00:00Z \
  git -c user.name=blaze -c user.email=blaze@example.com commit -qm dep
git branch 'feat/x'
git branch 'feat+1/x'
git tag 'v1'
git tag 'v1#$'
sha=$(git rev-parse HEAD)
repo="file://$g/repo"

lock() { # name, rust-version line, patch|replace, branch, tag
  local w=$g/$1
  mkdir -p "$w"/{app,lib,patch/dep1,rep/dep2}/src
  touch "$w"/{app,lib,patch/dep1,rep/dep2}/src/lib.rs
  printf '[package]\nname = "dep1"\nversion = "0.6.0"\nedition = "2018"\n' > "$w/patch/dep1/Cargo.toml"
  printf '[package]\nname = "dep2"\nversion = "0.1.0"\nedition = "2018"\n' > "$w/rep/dep2/Cargo.toml"
  printf '[package]\nname = "lib"\nversion = "0.2.0"\nedition = "2018"\n%s\n[dependencies]\ndep1 = { version = "0.5", git = "%s", tag = "%s" }\n' \
    "$2" "$repo" "$5" > "$w/lib/Cargo.toml"
  printf '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2018"\n%s\n[dependencies]\nlib = { path = "../lib" }\ndep1 = { version = "0.5", git = "%s", branch = "%s" }\ndep2 = { git = "%s", rev = "%s" }\nd1 = { package = "dep1", version = "0.5", git = "%s" }\n' \
    "$2" "$repo" "$4" "$repo" "${sha:0:10}" "$repo" > "$w/app/Cargo.toml"
  if [ "$3" = patch ]; then
    extra=$(printf '[patch."%s"]\ndep1 = { path = "patch/dep1" }' "$repo")
  else
    extra=$(printf '[replace]\n"%s#dep2@0.1.0" = { path = "rep/dep2" }' "$repo")
  fi
  printf '[workspace]\nmembers = ["app", "lib"]\nresolver = "1"\n\n%s\n' "$extra" > "$w/Cargo.toml"
  (cd "$w" && cargo +1.99.0 generate-lockfile --quiet)
  cp "$w/Cargo.lock" "$out/git-$3-$1.lock"
}

lock v1 'rust-version = "1.37"' replace feat/x v1
lock v2 'rust-version = "1.50"' patch feat/x v1
lock v3 'rust-version = "1.81"' replace feat/x v1
lock v4 '' patch 'feat+1/x' 'v1#$'
rm -rf "$g"
