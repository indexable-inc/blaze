// The page without a browser: `node check.mjs`, after `build.sh`. Builds the demo, edits it
// twice, and checks that what it draws follows the edits while its balls stay.

import assert from "node:assert";
import { existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { gunzipSync } from "node:zlib";
import { Compiler } from "./compiler.js";
import { Program } from "./program.js";

// `node check.mjs <directory>` checks what `build-site.sh` put there, gzipped, instead.
const root = process.argv[2] ? pathToFileURL(`${resolve(process.argv[2])}/`) : new URL(".", import.meta.url);
const read = (name) => {
  const plain = new URL(name, root);
  return existsSync(plain) ? readFileSync(plain) : gunzipSync(readFileSync(new URL(`${name}.gz`, root)));
};
const compiler = await Compiler.load(read("pkg/blaze_wasm.wasm"), read("pkg/core.bundle"));
let calls = [];
const program = new Program({
  fill: (...color) => calls.push(["fill", ...color]),
  rect: (...rect) => calls.push(["rect", ...rect]),
  circle: (...circle) => calls.push(["circle", ...circle]),
  text: (pointer, length, x, y) => calls.push(["text", program.text(pointer, length), x, y]),
});

function build(source) {
  const start = performance.now();
  compiler.setFile("/app/main.rs", source);
  const { module, report } = compiler.build();
  program.apply(module, report);
  const took = (performance.now() - start).toFixed(1);
  console.log(
    `${took} ms: ${report.functions.length} functions in ${module.length} bytes, ` +
      `${report.compiled} of ${report.bodies} bodies compiled, ` +
      `compiler memory ${compiler.memoryBytes() >> 20} MiB`,
  );
  calls = [];
  program.call("frame", 400, 300, 0.016);
  return { report, calls };
}

const source = read("demos/balls.rs").toString();
const circles = (calls) => calls.filter(([name]) => name === "circle");

let { report, calls: drawn } = build(source);
assert.equal(circles(drawn).length, 0);
program.call("click", 100, 50);

const bigger = source.replace("RADIUS: f64 = 14.0", "RADIUS: f64 = 30.0");
({ report, calls: drawn } = build(bigger));
assert.ok(report.functions.length < 5, "only what uses the constant is compiled again");
assert.deepEqual(circles(drawn).map((it) => it[3]), [30], "the ball is still there, and bigger");

({ report, calls: drawn } = build(bigger.replace("click to drop a ball", "hello")));
assert.deepEqual(report.functions, ["frame"]);
assert.equal(drawn.at(-1)[1], "hello");
assert.equal(circles(drawn).length, 1);

// Long enough for the ball to reach the floor and a wall, which run code a first frame does not.
for (let frame = 0; frame < 3000; frame++) program.call("frame", 400, 300, 0.016);

// The other demos build whole and run, clicks and all.
for (const name of ["fountain", "life", "sort", "mandelbrot"]) {
  const compiler = await Compiler.load(read("pkg/blaze_wasm.wasm"), read("pkg/core.bundle"));
  let drawn = 0;
  const draw = () => (drawn += 1);
  const program = new Program({ fill: draw, rect: draw, circle: draw, text: draw });
  compiler.setFile("/app/main.rs", read(`demos/${name}.rs`).toString());
  const { module, report } = compiler.build();
  program.apply(module, report);
  for (let frame = 0; frame < 600; frame++) {
    program.call("frame", 400, 300, 0.016);
    if (frame % 100 === 50) {
      program.call("click", 20 + frame / 2, 30 + frame / 3);
    }
  }
  assert.ok(drawn > 600, name);
  console.log(`${name}: ${report.functions.length} functions in ${module.length} bytes`);
}
console.log("ok");
