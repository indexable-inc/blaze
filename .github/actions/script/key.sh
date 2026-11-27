#!/usr/bin/env bash
# The cache key of the repository's job-scoped built scripts (scripts/,
# docs/v2/plan/design/scripts.md): prints `scripts-<job>-<os>-<arch>-<blaze version>-<sha256>`,
# run from the checkout's top.
#
# Why the key covers every input of what `build.sh` makes (AGENTS.md keys-cover-every-input):
# - the job and exact binary set (`bins.sh <job>`, plus `tools/gate.txt` for `scripts`);
# - the compiler: .github/blaze-cli-version (the pinned v2 release, with its own toolchain),
#   pins/toolchain (the nightly blaze pins), and the runner's OS and architecture;
# - the dependencies: the Cargo.lock blocks of the `scripts` package's whole transitive closure
#   (every registry crate is fixed by its SHA-256 checksum);
# - the sources: EVERY unchecksummed (workspace or `[patch]`) package in `scripts`'s Cargo.lock
#   closure, mapped to its directory on disk (refused if any cannot be located) and hashed in full,
#   plus `pins/`, `.github/actions/script/`, and `.github/actions/blaze/`;
# - the manifests: root `Cargo.toml` filtered to workspace dependencies referenced by any path
#   package in the closure, plus `.cargo/config.toml` whole;
# - the checkout's absolute path (`pwd -P`): scripts that bake in `env!("CARGO_MANIFEST_DIR")`;
# - the environment: `build.sh` scrubs compiler/build env vars so none leaks into the build.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd -P)
sha() { if command -v sha256sum >/dev/null; then sha256sum; else shasum -a 256; fi; }
sha_files() { if command -v sha256sum >/dev/null; then xargs sha256sum; else xargs shasum -a 256; fi; }
die() { echo "::error::$*" >&2; echo "key.sh: $*" >&2; exit 1; }

[ $# = 1 ] || die "takes JOB (scripts, lean, or release), not: $*"
job=$1

for need in .github/blaze-cli-version pins/toolchain Cargo.toml Cargo.lock .cargo/config.toml scripts/Cargo.toml; do
  [ -f "$need" ] || die "no $need in $(pwd -P): run from the checkout's top, with this file checked out"
done
export LC_ALL=C

bins=$("$here/bins.sh" "$job")

# Walk Cargo.lock from `scripts` to extract both the full lock closure and every unchecksummed
# (workspace member or `[patch]`) package name in that closure.
walk=$(awk '
  function flush() { if (n) text[n] = block }
  /^\[\[package\]\]/ { flush(); n++; block = $0 "\n"; next }
  n { block = block $0 "\n" }
  n && /^name = / { v = $3; gsub(/"/, "", v); name[n] = v; byname[v] = byname[v] " " n }
  n && /^version = / { v = $3; gsub(/"/, "", v); ver[n] = v }
  n && /^checksum = / { sum[n] = 1 }
  n && /^dependencies = \[/ { indeps = 1; next }
  indeps && /^\]/ { indeps = 0; next }
  indeps { d = $0; gsub(/[",]/, "", d); sub(/^ +/, "", d); deps[n] = deps[n] "|" d }
  END {
    flush()
    q[1] = "scripts"; h = 1; t = 1
    while (h <= t) {
      split(q[h++], w, " ")
      c = split(byname[w[1]], ids, " ")
      if (c == 0) { print "__ERR__ MISSING " w[1]; exit }
      for (k = 1; k <= c; k++) {
        i = ids[k]
        if (w[2] != "" && ver[i] != w[2]) continue
        if (seen[i]++) continue
        if (!sum[i]) path_pkgs[name[i]] = 1
        out = out text[i]
        m = split(deps[i], ds, "|"); for (j = 2; j <= m; j++) q[++t] = ds[j]
      }
    }
    for (p in path_pkgs) print "__PATH__ " p
    print "__CLOSURE__"
    printf "%s", out
  }' Cargo.lock)

err_line=$(printf '%s\n' "$walk" | grep '^__ERR__ ' || true)
if [ -n "$err_line" ]; then
  die "Cargo.lock has no package ${err_line#__ERR__ MISSING } in the scripts package's dependencies"
fi

path_pkg_names=$(printf '%s\n' "$walk" | sed -n 's/^__PATH__ //p' | sort -u)
closure=$(printf '%s\n' "$walk" | awk 'seen { print } /^__CLOSURE__$/ { seen = 1 }')

# Map every package name in `scripts/`, `crates/*/`, and `vendor/*/` to its directory.
pkg_map=$(awk '
  FNR == 1 { in_pkg = 0 }
  /^\[/ { in_pkg = ($0 == "[package]") }
  in_pkg && /^name[ \t]*=/ {
    v = $0
    sub(/^[^"]*"/, "", v)
    sub(/".*$/, "", v)
    dir = FILENAME
    sub(/\/Cargo\.toml$/, "", dir)
    print v "\t" dir
    nextfile
  }' scripts/Cargo.toml crates/*/Cargo.toml vendor/*/Cargo.toml)

source_dirs=()
manifest_files=()
while IFS= read -r pkg; do
  [ -n "$pkg" ] || continue
  dir=$(printf '%s\n' "$pkg_map" | awk -F'\t' -v p="$pkg" '$1 == p { print $2; exit }')
  [ -n "$dir" ] && [ -d "$dir" ] || die "unchecksummed package '$pkg' in scripts's Cargo.lock closure has no Cargo.toml under scripts/, crates/*/, or vendor/*/"
  source_dirs+=("$dir")
  manifest_files+=("$dir/Cargo.toml")
done <<< "$path_pkg_names"

# All workspace dependencies named by any path package in scripts's closure.
direct=$(awk '/^\[/ { deps = $0 ~ /dependencies\]$/ } deps' "${manifest_files[@]}" |
  sed -nE 's/^([A-Za-z0-9_-]+)(\.workspace *= *true| *= *\{.*workspace *= *true).*/\1/p' | sort -u | tr '\n' ' ')

manifest=$(awk -v direct=" $direct " '
  /^\[/ { table = $0; skip = 0; depth = 0 }
  table == "[workspace]" && /^(members|exclude|default-members) *= *\[/ { skip = 1 }
  table ~ /^\[patch[.]/ && !/^\[/ { next }
  table == "[workspace.dependencies]" && depth == 0 && /^[A-Za-z0-9_-]+ *=/ { key = $1; sub(/=.*/, "", key); drop = index(direct, " " key " ") == 0 }
  table == "[workspace.dependencies]" && !/^\[/ {
    line = $0; o = gsub(/[[{]/, "", line); line = $0; c = gsub(/[]}]/, "", line); depth += o - c
    if (drop) next
  }
  skip { line = $0; if (line ~ /\]/) skip = 0; next }
  { print }' Cargo.toml)

extra_inputs=(pins .github/actions/script .github/actions/blaze)
if [ "$job" = scripts ]; then
  extra_inputs+=(tools/gate.txt)
fi

blaze_ver=$(tr -d '[:space:]' < .github/blaze-cli-version)
digest=$({
  printf 'job %s\n' "$job"
  printf 'bins\n%s\n' "$bins"
  printf 'blaze-cli-version %s\n' "$blaze_ver"
  printf 'toolchain %s\n' "$(cat pins/toolchain)"
  printf 'host %s %s\n' "$(uname -s)" "$(uname -m)"
  printf 'checkout %s\n' "$(pwd -P)"
  printf 'direct %s\n' "$direct"
  find "${source_dirs[@]}" "${extra_inputs[@]}" -type f ! -path '*/target/*' | sort -u | sha_files
  printf '%s\n' "$closure" "$manifest"
  cat .cargo/config.toml
} | sha | cut -d' ' -f1)
echo "scripts-$job-$(uname -s)-$(uname -m)-$blaze_ver-$digest"
