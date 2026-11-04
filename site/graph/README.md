# Code graph (`/graph`)

https://ix.dev/blaze/graph: blaze's symbols and how they relate, as a live force-directed graph in the manner of
Obsidian's graph view. Nodes drift and settle; drag one and its neighbours follow; hover lights a node and its
neighbours; scroll zooms, dragging the background pans; a click opens its details (path, file, symbol, every relation
grouped by kind, each a link); `/` searches; `F` fits; `Esc` clears. Colour is by kind or by crate, size by degree,
line opacity by how often the relation occurs. The force sliders are Obsidian's: centre, repel, link force, link
distance.

It reads the graph `blaze index` writes (the scip lane's work, not landed yet); until then it shows `sample.json`.

## Files

- `schema.ts`: the file format (`blaze-graph`, version 1, a draft) and its loader, `parseGraph`. The one place that
  knows the JSON's shape: when `blaze index` changes the format, change this file and bump `GRAPH_VERSION`. It refuses
  an unknown format, version, field or kind, a dangling or duplicate id, naming where (`$.nodes[12].kind`) and why.
- `model.ts`: the graph as drawn: symbols plus one small connector node per fact (a hyperedge: a call site joining
  caller and callee, an impl joining type, trait and impl block), relations merged per pair of nodes, degree,
  adjacency, colours and sizes.
- `main.ts`: the page: loading, the frame loop, picking, drag, pan and zoom, labels, search, details.
- `graph.html`, `graph.css`: the page, standalone (none of the site's chrome or scripts).
- `sample.ts` writes `sample.json`: 335 symbols from 15 of blaze's real crates, hand-listed, with seeded relations and
  54 facts (`bun graph/sample.ts`).
- `build.ts`: what `gen/build.ts` calls: `dist/graph.html` and `dist/graph/` (the bundle, the CSS, the sample and the
  simulation's `physics.wasm`).
- `serve.ts`: the page alone on localhost, rebuilt on every load, with the worker's CSP.

## Rendering: @codegraphy-dev/graph-renderer

Drawn by [`@codegraphy-dev/graph-renderer`](https://github.com/joesobo/CodeGraphyV4/tree/main/packages/graph-renderer)
(MIT, pinned at 0.4.0): WebGPU instanced nodes and link quads, and a deterministic Barnes-Hut force simulation in a
10 KB wasm module, on typed arrays indexed like ours. It owns drawing and physics; this page owns the graph model,
labels (2D text over the WebGPU canvas), picking, interaction and UI. The page compiles the wasm itself
(`prepareGraphPhysicsFromBytes`), so the worker sends `/graph` a CSP with `'wasm-unsafe-eval'` (`worker/csp.ts`,
`worker/http.ts`); nothing else on the site gets it.

There is no other renderer: without WebGPU (no `navigator.gpu`, no adapter, a lost device) the page says so and draws
nothing. The library itself may take the browser's software WebGPU adapter when there is no hardware one; that is
still WebGPU.

## Viewing it

```
cd site
bun install
bun graph/serve.ts                                          # http://localhost:8790/graph, the sample
bun graph/serve.ts '{"graph": "/path/to/blaze-index.json"}' # a graph file from `blaze index`
```

Or open the page and drop a graph file on it, or pass `?src=<same-origin url>`. The whole site (`bun run build`,
`bun run dev`) builds it too, at `/blaze/graph`. It needs a browser with WebGPU: Chrome or Edge 113+, Safari 26+,
Firefox 141+ on Windows. Headless Chrome draws it with `--enable-unsafe-webgpu`.

`bun run check` type-checks it (`graph/tsconfig.json`).
