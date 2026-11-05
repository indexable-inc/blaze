#!/bin/sh
# Unpacks a crates.io crate (name-version) into harness/vendor/<name> as its own workspace.
set -e
here=$(cd "$(dirname "$0")" && pwd)
# The version starts at the first `-<digits>.`: it may hold more hyphens (`1.1.2+spec-1.1.0`).
name=$(printf %s "$1" | sed -E 's/-[0-9]+\.[0-9]+\.[0-9]+.*$//')
version=${1#"$name"-}
dest="$here/vendor/$name"
[ -d "$dest" ] && exit 0
mkdir -p "$here/vendor"
src=$(ls -d "$HOME"/.cargo/registry/src/*/"$1" 2>/dev/null | head -1)
if [ -z "$src" ]; then
    tmp=$(mktemp -d)
    (cd "$tmp" && cargo init -q --name fetch && cargo add -q "$name@=$version" && cargo fetch -q)
    src=$(ls -d "$HOME"/.cargo/registry/src/*/"$1" | head -1)
fi
cp -r "$src" "$dest"
chmod -R u+w "$dest"
rm -f "$dest/Cargo.lock"
# The manifest cargo published is self-contained; the original may lean on a workspace.
grep -q '^\[workspace\]' "$dest/Cargo.toml" || printf '\n[workspace]\n' >> "$dest/Cargo.toml"
# A crate's own profiles (LTO, optimization) change what there is to link against.
awk '/^\[/ { skip = /^\[profile\./ } !skip' "$dest/Cargo.toml" > "$dest/Cargo.toml.new"
mv "$dest/Cargo.toml.new" "$dest/Cargo.toml"
