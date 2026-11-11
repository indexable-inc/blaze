#!/usr/bin/env bash
# The cache key of the repository's built scripts (scripts/, docs/v2/plan/design/scripts.md):
# prints `scripts-<os>-<arch>-<blaze version>-<sha256>`, run from the checkout's top. No
# argument, and it reads only the files below, so the `changes` job's sparse checkout has them.
#
# Why the key covers every input of what `build.sh` makes (AGENTS.md keys-cover-every-input):
# the binaries are what the pinned release builds from these, and nothing else is read:
# - the compiler: .github/blaze-version (the release, with its own toolchain; rule own-nightly),
#   harness/toolchain (the nightly blaze pins), and the runner's OS and architecture;
# - the sources: every file under scripts/ and crates/blaze-script/, by content;
# - the dependencies: the Cargo.lock blocks of the scripts package's whole dependency closure
#   (name, version, registry and checksum, so each crate's source is fixed by its checksum); a
#   package in it with no checksum (a path or a [patch]) is refused below, as its source is not
#   hashed here;
# - the manifests: the root Cargo.toml whole but for what cannot reach these packages (the
#   workspace's member and exclude lists, which the lock's closure already resolves; the
#   [workspace.dependencies] entries scripts and blaze-script do not name; [patch] entries, each
#   of which a closure package would show by losing its checksum), and .cargo/config.toml whole;
# - the checkout's absolute path: docs, rustcsites and guards find the tree as
#   `env!("CARGO_MANIFEST_DIR")/..`, baked in at build time, so a binary built in another checkout
#   would read that one;
# - the environment: build.sh unsets every variable cargo or blaze reads from the caller
#   (engine.py's SCRUBBED), so none is an input;
# - this action's own files, which make the binaries.
# Not an input: the `strip` build.sh runs in the meantime (ledger D44) and its version. It only
# removes debug sections, which no script reads, so what a script does is the same whichever
# strip ran; a different strip makes different bytes, never a different behaviour.
set -euo pipefail

sha() { if command -v sha256sum >/dev/null; then sha256sum; else shasum -a 256; fi; }
die() { echo "::error::$*" >&2; echo "key.sh: $*" >&2; exit 1; }

for need in .github/blaze-version harness/toolchain Cargo.toml Cargo.lock .cargo/config.toml scripts/Cargo.toml crates/blaze-script/Cargo.toml; do
  [ -f "$need" ] || die "no $need in $(pwd -P): run from the checkout's top, with this file checked out"
done
export LC_ALL=C

# The workspace dependencies the two path packages name in their dependency tables
# (`x.workspace = true`, `x = { workspace = true ... }`).
direct=$(awk '/^\[/ { deps = $0 ~ /dependencies\]$/ } deps' scripts/Cargo.toml crates/blaze-script/Cargo.toml |
  sed -nE 's/^([A-Za-z0-9_-]+)(\.workspace *= *true| *= *\{.*workspace *= *true).*/\1/p' | sort -u | tr '\n' ' ')

closure=$(awk '
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
      if (c == 0) { print "MISSING " w[1]; exit }
      for (k = 1; k <= c; k++) {
        i = ids[k]
        if (w[2] != "" && ver[i] != w[2]) continue
        if (seen[i]++) continue
        if (!sum[i] && name[i] != "scripts" && name[i] != "blaze-script") { print "UNHASHED " name[i] " " ver[i]; exit }
        printf "%s", text[i]
        m = split(deps[i], ds, "|"); for (j = 2; j <= m; j++) q[++t] = ds[j]
      }
    }
  }' Cargo.lock)
case "$closure" in
  MISSING*) die "Cargo.lock has no package ${closure#MISSING } in the scripts package's dependencies" ;;
  UNHASHED*) die "${closure#UNHASHED } in the scripts package's dependencies has no checksum in Cargo.lock (a path or a [patch]): its source is no input of this key, so add its directory to the sources key.sh hashes" ;;
esac

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

digest=$({
  printf 'blaze-version %s\n' "$(cat .github/blaze-version)"
  printf 'toolchain %s\n' "$(cat harness/toolchain)"
  printf 'host %s %s\n' "$(uname -s)" "$(uname -m)"
  printf 'checkout %s\n' "$(pwd -P)"
  printf 'direct %s\n' "$direct"
  find scripts crates/blaze-script .github/actions/script -type f ! -path '*/target/*' | sort | while read -r f; do
    printf 'file %s %s\n' "$f" "$(sha < "$f" | cut -d' ' -f1)"
  done
  printf '%s\n' "$closure" "$manifest"
  cat .cargo/config.toml
} | sha | cut -d' ' -f1)
echo "scripts-$(uname -s)-$(uname -m)-$(cat .github/blaze-version)-$digest"
