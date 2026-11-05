// node build.mjs <outdir> <bundle...>: the renderer bundles, minified ES modules.
//   cosmos     cosmos.js     the page's renderer, @cosmos.gl/graph
//   sigma      sigma.js      sigma and graphology (the benchmark's other arm)
//   fa2        fa2-worker.js ForceAtlas2 in a worker (the benchmark's sigma layout)
import { build } from "esbuild";
import { mkdirSync } from "node:fs";

const [outdir, ...which] = process.argv.slice(2);
if (!outdir || which.length === 0) throw new Error("usage: node build.mjs <outdir> cosmos|sigma|fa2 ...");
const bundles = {
  cosmos: { entry: "cosmos.entry.js", out: "cosmos.js", format: "esm" },
  sigma: { entry: "sigma.entry.js", out: "sigma.js", format: "esm" },
  fa2: { entry: "fa2-worker.entry.js", out: "fa2-worker.js", format: "iife" },
};
mkdirSync(outdir, { recursive: true });
for (const name of which) {
  const b = bundles[name];
  if (!b) throw new Error(`no bundle ${name}`);
  await build({
    entryPoints: [b.entry],
    outfile: `${outdir}/${b.out}`,
    bundle: true,
    minify: true,
    format: b.format,
    target: "es2022",
    platform: "browser",
    legalComments: "none",
    define: { "process.env.NODE_ENV": '"production"' },
    logLevel: "warning",
  });
}
