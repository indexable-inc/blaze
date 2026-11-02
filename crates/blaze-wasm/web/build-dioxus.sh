#!/bin/sh
# Builds the page, after `build.sh`: the playground into `pkg/dioxus` and `assets`, and into
# `pkg/dioxus.pack` the workspace of the Dioxus program for blaze to compile in the page.
# Needs the toolchain of `dioxus/rust-toolchain.toml`, a `clang` that targets wasm32 for the
# parser the highlighting uses, and `wasm-bindgen` of the version `dioxus/Cargo.lock` has
# (`cargo install wasm-bindgen-cli --version 0.2.122`).
set -eu
# The page's Dioxus is the module rustc builds, and the program was linked into it by rustc's
# symbols: blaze runs no rustc, so the page's Dioxus has to be built by blaze and the pack made
# by `blaze serve`, which neither does yet.
echo "build-dioxus.sh: the Dioxus page needs its playground built by blaze for wasm32 and its pack made by blaze serve; neither exists yet" >&2
exit 1
cd "$(dirname "$0")/.."
target=../../target
tool="$PWD/$target/release/blaze-wasm"
cargo build --release --target-dir "$target"

cd web/dioxus
cargo build --release -p playground
out=target/wasm32-unknown-unknown/release
# Everything of every crate, exported under its symbol: the program links against any of it.
# rustc gives its own arguments to the linker after the libraries, where `--whole-archive`
# does nothing, so the linker runs here.
"$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/host: //p')/bin/rust-lld" -flavor wasm \
    --whole-archive "$out/libplayground.a" --no-entry --no-gc-sections --allow-undefined \
    --export-all --export-table --growable-table --export=__stack_pointer --emit-relocs \
    -o "$out/playground.wasm"
rm -rf ../pkg/dioxus
wasm-bindgen "$out/playground.wasm" --target web --keep-lld-exports --out-dir ../pkg/dioxus --out-name playground

# The pack's `core` is the source of the std this toolchain built the program with, which it
# links: that toolchain's rust-src, named as blaze's library is (BLAZE_LIBRARY), since blaze's
# front end reads no rust-src of its own accord: it is read here because plain cargo built the
# program.
BLAZE_LIBRARY="$(rustc --print sysroot)/lib/rustlib/src/rust/library" "$tool" pack . app ../pkg/dioxus.pack
"$tool" host "$out/playground.wasm" ../pkg/dioxus.pack

# Two things wasm-bindgen's JavaScript needs for a module nothing was removed from: the names
# of functions LLVM made module-private (`name.llvm.123`) do not parse with their dots, and
# the functions describing bindings, which it normally deletes once read, keep their import.
sed -i -E -e 's/wasm\.(_R[A-Za-z0-9_]+\.llvm\.[0-9]+)/wasm["\1"]/g' \
    -e 's/(^|[^"A-Za-z0-9_.])(_R[A-Za-z0-9_]+)\.llvm\.([0-9]+)/\1\2$llvm$\3/g' \
    -e 's/^( *)"\.\/playground_bg\.js": import0,$/&\n\1__wbindgen_placeholder__: { __wbindgen_describe() {}, __wbindgen_describe_cast() {} },/' \
    ../pkg/dioxus/playground.js

# The style sheets of the editor. `dx` would write into the module where it put each; built
# without it the editor asks for all of them under the one name below, so `index.html` asks
# for them instead and that name gets an empty sheet.
rm -rf ../assets
mkdir ../assets
manifest() {
    cargo metadata --format-version 1 --locked |
        grep -o "\"manifest_path\":\"[^\"]*/$1-[0-9.]*/Cargo.toml\"" | sed 's|.*:"\(.*\)/Cargo.toml"|\1|'
}
code="$(manifest dioxus-code)/assets"
themes="$code/generated/arborium-themes"
cp "$code/dioxus-code.css" "$themes/dioxus-code-theme.css" "$themes/tokyo-night.css" \
    "$themes/github-light.css" \
    "$(manifest dioxus-code-editor)/assets/dioxus-code-editor.css" ../assets
sed -n 's/.*PLACEHOLDER_HASH: &str = "\(.*\)";/\1/p' "$(manifest manganis-core)/src/asset.rs" |
    while read -r name; do
        echo "/* see build-dioxus.sh */" >"../assets/$name"
    done
