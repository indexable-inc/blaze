// node licenses.mjs <bundle> > ../LICENSE: the license of every npm package esbuild puts into the bundle.
import { build } from "esbuild";
import { readFileSync, readdirSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";

const entries = { cosmos: "cosmos.entry.js", sigma: "sigma.entry.js", fa2: "fa2-worker.entry.js" };
const [which] = process.argv.slice(2);
if (!entries[which]) throw new Error("usage: node licenses.mjs cosmos|sigma|fa2");

const { metafile } = await build({
  entryPoints: [entries[which]],
  bundle: true,
  write: false,
  metafile: true,
  format: "esm",
  platform: "browser",
  define: { "process.env.NODE_ENV": '"production"' },
  logLevel: "warning",
});
const used = new Map();
for (const [path, input] of Object.entries(metafile.outputs[Object.keys(metafile.outputs)[0]].inputs)) {
  if (input.bytesInOutput === 0) continue;
  const m = path.match(/^(.*node_modules\/(?:@[^/]+\/)?[^/]+)\//);
  if (m) used.set(m[1], (used.get(m[1]) ?? 0) + input.bytesInOutput);
}
// Packages without a license file, and the heading in their README.md the license text follows (to the end).
const inReadme = { seedrandom: /^LICENSE \(MIT\)\r?\n-+\r?\n/m };
const out = [];
const missing = [];
for (const dir of [...used.keys()].sort()) {
  const pkg = JSON.parse(readFileSync(join(dir, "package.json"), "utf8"));
  const file = readdirSync(dir).find((f) => /^(licen[cs]e|copying)(\.(md|txt))?$/i.test(f));
  let text;
  if (file) text = readFileSync(join(dir, file), "utf8");
  else if (inReadme[pkg.name]) {
    const readme = readFileSync(join(dir, "README.md"), "utf8");
    const m = readme.match(inReadme[pkg.name]);
    if (!m) throw new Error(`${pkg.name}'s README.md no longer has the heading ${inReadme[pkg.name]}`);
    text = readme.slice(m.index + m[0].length).replace(/\r\n/g, "\n");
  } else {
    missing.push(`${pkg.name}@${pkg.version} (${pkg.license})`);
    continue;
  }
  out.push(`${"=".repeat(78)}\n${pkg.name}@${pkg.version} (${pkg.license})\n${"=".repeat(78)}\n\n${text.trim()}\n`);
}
if (missing.length) throw new Error(`no license text found for: ${missing.join(", ")}; name where it is in inReadme`);
process.stdout.write(`The npm packages bundled into ${which === "fa2" ? "fa2-worker" : which}.js, and their licenses.\n\n${out.join("\n")}`);
