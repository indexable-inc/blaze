#!/usr/bin/env bash
# `build.sh DIR KEY`: every script (scripts/src/bin/<name>.rs) built by the blaze on PATH, which
# must be the release .github/blaze-version names (AGENTS.md blaze-builds-blaze), and put in DIR
# as DIR/<name>, with DIR/KEY holding KEY (key.sh's). Run from the checkout's top, whole: blaze
# loads the workspace scripts/ is a member of.
#
# No CARGO_TARGET_DIR: blaze builds into its own build directory under its home, where it places
# each output by cloning the store's file, and refuses a target directory on a file system that
# cannot clone from the store's (tmpfs /tmp, or a $RUNNER_TEMP on another disk; AGENTS.md
# no-fallback). Where each binary is comes from the build itself, the `executable` of its
# `compiler-artifact` line under --message-format=json (crates/blaze-cargo/src/cli/json.rs,
# `artifact`), and it is copied into DIR, which may be on any file system.
#
# `--profile script` (the root Cargo.toml): dev's opt-levels without debug info, so the cached
# binaries are small enough for the Actions cache and a fast restore.
set -euo pipefail
die() { echo "::error::$*" >&2; echo "build.sh: $*" >&2; exit 1; }
[ $# = 2 ] || die "takes DIR and KEY, not: $*"
dir=$1 key=$2

want=$(cat .github/blaze-version)
said=$(blaze-engine --version 2>&1) || die "no blaze on PATH (blaze-engine --version: $said)"
[ "$(echo "$said" | awk '$1 == "blaze" { print $2 }')" = "$want" ] ||
  die "the blaze on PATH says '$said', and .github/blaze-version names $want: the key names that release"

# What cargo or blaze reads from the caller's environment (harness/v2/engine.py, SCRUBBED), so
# the key, which names none of it, covers the build.
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS RUSTDOCFLAGS RUSTC RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER \
  CARGO_BUILD_TARGET CARGO_BUILD_JOBS CARGO_INCREMENTAL BLAZE_BACKEND BLAZE_OPT BLAZE_EMIT
for name in $(env | sed -nE 's/^(CARGO_PROFILE_[A-Z0-9_]*|CARGO_TARGET_[A-Z0-9_]*)=.*/\1/p'); do unset "$name"; done

# Interim (ledger D44, the piece being built): each binary's debug sections are stripped after
# the copy, by the runner's own binutils (`strip --strip-debug` on Linux, `strip -S` on macOS).
# A dev link carries the shipped standard library's DWARF, ~138 MB of each ~145 MB binary, which
# no profile setting drops and blaze-link cannot leave out yet.
# when blaze-link honours strip (ledger D44): remove this and set strip in [profile.script]
case "$(uname -s)" in
  Linux) strip_debug=(strip --strip-debug) ;;
  Darwin) strip_debug=(strip -S) ;;
  *) die "no way to strip debug sections on $(uname -s): only Linux (binutils) and macOS are known" ;;
esac
command -v "${strip_debug[0]}" >/dev/null || die "no \`${strip_debug[0]}\` on PATH, which strips the scripts' debug sections (\`${strip_debug[*]}\`); install binutils"

unset CARGO_TARGET_DIR
said="$dir.build.json"
mkdir -p "$(dirname "$dir")"
blaze build -p scripts --bins --profile script --message-format=json > "$said" || die "blaze build -p scripts --bins --profile script --message-format=json failed (its JSON lines: $said)"

fresh="$dir.new"
rm -rf "$fresh"
mkdir -p "$fresh"
for src in scripts/src/bin/*.rs; do
  name=$(basename "$src" .rs)
  # The executables the build names whose file is `<name>`: exactly one.
  found=$(grep '"reason":"compiler-artifact"' "$said" | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | awk -v n="$name" -F/ '$NF == n' | sort -u)
  [ -n "$found" ] || die "blaze build -p scripts --bins named no executable \`$name\` in a compiler-artifact line ($said)"
  [ "$(echo "$found" | wc -l)" = 1 ] || die "blaze build -p scripts --bins named more than one executable \`$name\`: $(echo "$found" | tr '\n' ' ')"
  [ -x "$found" ] || die "the build named $found for \`$name\`, which is no executable file"
  cp "$found" "$fresh/$name"
  "${strip_debug[@]}" "$fresh/$name" || die "\`${strip_debug[*]} $fresh/$name\` failed"
done
echo "$key" > "$fresh/KEY"
rm -rf "$dir"
mv "$fresh" "$dir"
rm -f "$said"
echo "build.sh: $(ls "$dir" | grep -vx KEY | wc -l | tr -d ' ') scripts in $dir under $key"
