#!/bin/sh
# Puts the playground of the website into `site/playground/blaze`: the JavaScript that runs
# the compiler and the programs to show first, then, built by `build.sh`, the compiler and
# the source of `core`, gzipped, with `pkg/manifest.json` saying how big they are. The page
# (`site/playground/index.html`) says the demo is unavailable where the manifest is missing,
# which is how a build that failed looks.
set -eu
cd "$(dirname "$0")"
out=../../../site/playground/blaze
rm -rf "$out"
mkdir -p "$out/demos"
cp compiler.js program.js worker.js "$out"
cp demos/*.rs "$out/demos"

./build.sh
node check.mjs

# A build the check does not pass is not shown.
trap 'rm -rf "$out/pkg"' EXIT
mkdir "$out/pkg"
size() { wc -c <"$1" | tr -d ' '; }
for name in blaze_wasm.wasm core.bundle; do
    gzip -9 -n -c "pkg/$name" >"$out/pkg/$name.gz"
done
node check.mjs "$out"
# The page asks for the files under this name, so that a browser never pairs the JavaScript
# of one build with the compiler of another.
build=$(cat "$out"/pkg/*.gz "$out"/*.js | cksum | cut -d ' ' -f 1)
cat >"$out/pkg/manifest.json" <<JSON
{
  "build": "$build",
  "transfer_bytes": $(($(size "$out/pkg/blaze_wasm.wasm.gz") + $(size "$out/pkg/core.bundle.gz"))),
  "compiler_bytes": $(size pkg/blaze_wasm.wasm),
  "core_bytes": $(size pkg/core.bundle)
}
JSON
trap - EXIT
