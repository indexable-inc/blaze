# Vendored renderer

`cosmos.js` is [cosmos.gl](https://github.com/cosmosgl/graph) (`@cosmos.gl/graph` 3.5.1, MIT): the main view's force layout and points/links renderer, both on the GPU through WebGL2. It is a single minified ES module built from npm with esbuild. The page never fetches anything else. `LICENSE` holds the license text of every npm package bundled into it:

| package | version | license |
|---|---|---|
| @cosmos.gl/graph | 3.5.1 | MIT |
| @luma.gl/core, engine, shadertools, webgl | 9.3.6 | MIT |
| @math.gl/types | 4.1.0 | MIT |
| @probe.gl/env, log, stats | 4.1.2 | MIT |
| d3-array 3.2.4, d3-color 3.1.0, d3-dispatch 3.0.1, d3-drag 3.0.0, d3-format 3.1.2, d3-interpolate 3.0.1, d3-scale 4.0.2, d3-selection 3.0.0, d3-timer 3.0.1, d3-transition 3.0.1, d3-zoom 3.0.0 | | ISC |
| d3-ease | 3.0.1 | BSD-3-Clause |
| dompurify | 3.4.17 | MPL-2.0 OR Apache-2.0 (used under Apache-2.0) |
| gl-bench | 1.0.42 | MIT |
| gl-matrix | 3.4.4 | MIT |
| random | 4.1.0 | MIT |
| seedrandom | 3.0.5 | MIT |

```
cosmos.js  744,762 bytes  sha256 baae2a4759db42791001e4f52b9d1730db110a441d762f882c00bad50e3b0444
```

The choice of cosmos.gl over Sigma.js is in `bench/results/2026-10-28-web-graph/README.md`.

## Rebuilding

`build/` pins every version: `package.json` names exact versions and `package-lock.json` pins the whole tree. With Node 24 and npm, from `build/`:

```
npm ci
node build.mjs .. cosmos          # writes ../cosmos.js; byte-identical to the checked-in file
node licenses.mjs cosmos > ../LICENSE
```

To upgrade, change the version in `package.json`, run `npm install` to update the lock file, rebuild both files, and update the table and hash above.

`build.mjs` can also build `sigma` (sigma 4.0.4 and graphology 0.26.0) and `fa2` (a ForceAtlas2 worker around graphology-layout-forceatlas2 0.10.1). Those are only for the renderer benchmark, which builds them itself; the page doesn't ship them.
