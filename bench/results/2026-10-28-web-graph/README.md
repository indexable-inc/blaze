# The build page's graph renderer: cosmos.gl vs Sigma.js

**Pick: cosmos.gl 3.5.1.** It is the page's renderer (`crates/blaze-web/assets/vendor/cosmos.js`).

The candidates were [cosmos.gl](https://github.com/cosmosgl/graph) 3.5.1, which runs both the force layout and the drawing on the GPU through WebGL2, and [Sigma.js](https://www.sigmajs.org) 4.0.4 on graphology 0.26.0, with graphology-layout-forceatlas2 0.10.1 running ForceAtlas2 in a worker. Each drew the same graph: 80,000 functions and 300,000 calls from `crates/blaze-web/assets/dev/fixture.py`, starting from the page's own crate/module layout. Both ran in headless Chromium.

**Every number here comes from SwiftShader, Chromium's software GPU, on a VM with no GPU. That understates a real GPU badly, and by different amounts for different things.** Software rasterisation dominates every frame: one frame at 80k takes 4–5.5 s, so the fps columns are close to meaningless as absolute numbers. Section 4 lists the measurements that don't depend on the GPU, which are the ones the choice rests on.

## 1. Machine

| | |
|---|---|
| VM | ix VM, 16 vCPU ARM Neoverse-V3 (aarch64), one NUMA node |
| browser | Chromium 155.0.8059.84, headless, `--use-angle=swiftshader --enable-unsafe-swiftshader --ignore-gpu-blocklist --enable-precise-memory-info` |
| WebGL renderer | ANGLE (Google, Vulkan 1.3.0 (SwiftShader Device (LLVM 10.0.0) (0x0000C0DE)), SwiftShader driver) |
| bundles | esbuild 0.27.2 on Node 24.21.0, from `crates/blaze-web/assets/vendor/build` (exact versions in its lock file) |

`raw/*/all.json` records the same details as the runs saw them.

## 2. Method

`run.py` serves `bench.html` under the page's CSP (`default-src 'self'; worker-src 'self' blob:; connect-src 'self'`). For every run it starts a fresh browser, alternating the arms (cosmos, sigma, cosmos, sigma, …: ABAB), with 3 runs per arm and condition. `vm-bench.sh` is the exact script that ran.

Each run goes through these phases in order:

1. **Setup.** Parse the snapshot, compute the page's crate > module > function circle packing (`layout.js`), and give it to the renderer. "First frame" is the time from the start of setup to the second animation frame after the first draw.
2. **Settle.** The force layout runs until it's settled (footnote 1) or the cap is reached (240 s, or 120 s for the 800x500 and 20k conditions). After this phase cosmos's simulation is paused, so the next phases measure drawing only.
3. **Pan/zoom.** 10 s of a scripted camera path (pan plus 1×–4× zoom), with a camera change every frame.
4. **Live.** 10 s with a static camera while a `setInterval` changes the colour and size of 1,000 random functions every 100 ms, the way the page lights up functions as a build goes.
5. **Live while moving.** The live updates and the camera path together.
6. **Layout step alone.** Footnote 2.
7. **Memory.** JS heap via `performance.memory`, and the RSS of the browser and all its processes, read from `/proc`.

There were three conditions:

- **80k-1600x1000:** the brief's graph and window.
- **80k-800x500:** a quarter of the pixels. Frame times stayed the same, so SwiftShader's cost here is geometry (300k links), not pixel fill.
- **20k-1600x1000:** a quarter of the graph (20,000 functions, 75,000 calls). Frame times fell about 4×, so the cost scales with the graph.

The fixtures were made by `fixture.py` with seed 1, before it gained dependency ranks: same counts, but its cross-crate calls went to random crates. Cost per frame depends on the counts, and the layout may take a little longer or shorter with the ranked structure.

## 3. Results

Each cell gives the median of the runs in bold, then every run's value in order. `summary.py '{"raw": "raw"}'` prints these tables from `raw/`.

### 80k-1600x1000 (the brief's condition)

80,000 functions, 300,000 calls, 7,071 modules; 3 runs of each arm.

| | cosmos.gl 3.5.1 | Sigma.js 4.0.4 + FA2 worker |
|---|---|---|
| first frame (ms) | **1152** (1137, 1152, 1158) | **2762** (2762, 2542, 2769) |
| layout settled (s)¹ | **145** (144, 145, 146) | **70** (72, 66, 70) |
| layout steps to settle | **34** (34, 34, 34) | **137** (137, 137, 137) |
| ms per layout step, alone² | **330** (326, 330, 331) | **445** (453, 445, 432) |
| pan/zoom fps | **0.49** (0.48, 0.50, 0.49) | **0.36** (0.36, 0.36, 0.36) |
| pan/zoom frame p50 (ms) | **4060** (4114, 4007, 4060) | **5565** (5565, 5560, 5578) |
| renderer main thread per frame, pan/zoom (ms)³ | **0.24** (0.28, 0.21, 0.24) | **0.30** (0.40, 0.30, 0.30) |
| live: fps | **0.41** (0.41, 0.41, 0.49) | **0.18** (0.18, 0.18, 0.18) |
| live: ms per update call⁴ | **589.0** (599.2, 580.1, 589.0) | **3.6** (3.8, 3.6, 3.0) |
| live: updates done per 100 asked for | **15** (15, 15, 15) | **100** (100, 100, 99) |
| live: renderer main thread per frame (ms)³ | **0.8** (0.8, 1.0, 0.7) | **90.6** (95.6, 90.6, 89.8) |
| live while moving: fps | **0.25** (0.25, 0.25, 0.33) | **0.18** (0.18, 0.18, 0.18) |
| JS heap after layout (MB) | **73** (73, 73, 73) | **458** (468, 400, 458) |
| browser RSS after layout (MB)⁵ | **1501** (1501, 1464, 1515) | **1839** (1877, 1721, 1839) |
| browser RSS at end (MB)⁵ | **1668** (1686, 1615, 1668) | **1750** (1784, 1686, 1750) |

The settled layouts are in `raw/80k-1600x1000/cosmos-1-settled.png` and `sigma-1-settled.png`. Sigma's edges are drawn at its default opacity, which I didn't tune, so they wash out its picture.

### 80k-800x500

| | cosmos.gl 3.5.1 | Sigma.js 4.0.4 + FA2 worker |
|---|---|---|
| first frame (ms) | **1171** (1174, 1171, 1170) | **2771** (2771, 2905, 2622) |
| layout settled (s)¹ | **143** (146, 143, 141) | **70** (70, 70, 65) |
| layout steps to settle | **34** (34, 34, 34) | **137** (137, 137, 137) |
| ms per layout step, alone² | **329** (323, 329, 335) | **438** (441, 436, 438) |
| pan/zoom fps | **0.50** (0.49, 0.58, 0.50) | **0.36** (0.36, 0.36, 0.36) |
| pan/zoom frame p50 (ms) | **3915** (4052, 16, 3915) | **5572** (5592, 5558, 5572) |
| renderer main thread per frame, pan/zoom (ms)³ | **0.21** (0.24, 0.20, 0.21) | **0.33** (0.33, 0.28, 0.35) |
| live: fps | **0.43** (0.50, 0.41, 0.43) | **0.18** (0.18, 0.18, 0.18) |
| live: ms per update call⁴ | **583.8** (583.8, 592.6, 557.8) | **3.0** (2.9, 3.0, 3.7) |
| live: updates done per 100 asked for | **15** (15, 15, 15) | **99** (99, 99, 100) |
| live: renderer main thread per frame (ms)³ | **0.7** (0.7, 1.1, 0.5) | **98.2** (93.7, 98.2, 111.0) |
| live while moving: fps | **0.25** (0.25, 0.25, 0.26) | **0.18** (0.18, 0.18, 0.18) |
| JS heap after layout (MB) | **73** (73, 73, 73) | **459** (459, 513, 427) |
| browser RSS after layout (MB)⁵ | **1393** (1385, 1404, 1393) | **1768** (1734, 1768, 1845) |
| browser RSS at end (MB)⁵ | **1614** (1600, 1622, 1614) | **1641** (1641, 1627, 1742) |

The 16 ms median in cosmos's second pan/zoom run is real: that run's animation frames mostly came back without waiting on the GPU process. Its fps, which counts frames over time, stayed close to the other runs.

### 20k-1600x1000

20,000 functions, 75,000 calls, 2,407 modules.

| | cosmos.gl 3.5.1 | Sigma.js 4.0.4 + FA2 worker |
|---|---|---|
| first frame (ms) | **1144** (1133, 1149, 1144) | **1362** (1362, 1374, 1361) |
| layout settled (s)¹ | **36** (36, 36, 37) | **17** (17, 17, 17) |
| layout steps to settle | **34** (34, 34, 34) | **156** (156, 156, 156) |
| ms per layout step, alone² | **72** (72, 71, 72) | **88** (88, 88, 88) |
| pan/zoom fps | **1.29** (1.29, 1.28, 1.39) | **0.89** (0.88, 0.97, 0.89) |
| pan/zoom frame p50 (ms) | **1005** (1005, 1012, 998) | **1407** (1419, 1407, 1388) |
| renderer main thread per frame, pan/zoom (ms)³ | **0.20** (0.20, 0.23, 0.18) | **0.29** (0.29, 0.59, 0.28) |
| live: fps | **1.17** (1.17, 1.14, 1.17) | **1.06** (1.14, 0.70, 1.06) |
| live: ms per update call⁴ | **1.4** (1.4, 1.4, 1.2) | **2.6** (2.6, 2.7, 2.1) |
| live: updates done per 100 asked for | **100** (99, 100, 100) | **99** (99, 99, 99) |
| live: renderer main thread per frame (ms)³ | **0.6** (0.6, 0.6, 0.5) | **22.3** (22.5, 22.3, 22.2) |
| live while moving: fps | **1.12** (1.12, 1.28, 1.11) | **0.80** (0.80, 0.73, 0.80) |
| JS heap after layout (MB) | **24** (24, 24, 24) | **122** (122, 114, 129) |
| browser RSS after layout (MB)⁵ | **1310** (1345, 1297, 1310) | **1479** (1479, 1477, 1486) |
| browser RSS at end (MB)⁵ | **1486** (1480, 1486, 1501) | **1426** (1426, 1436, 1408) |

Every cosmos run logged one "ResizeObserver loop completed with undelivered notifications" error, which is benign. It also logged up to 256 `clientWaitSync: attempt to use a deleted object` WebGL warnings: cosmos.gl's asynchronous GPU readbacks give up on their fences while a SwiftShader frame takes seconds. No run had a CSP violation or a failure.

Footnotes:

1. **Settled** means the mean distance a point moves per step is below 0.0005 of the layout's diagonal. For cosmos it's checked every 30 frames; for FA2, every iteration. The two arms reach this in different ways, so it isn't the same quality bar:
   - cosmos.gl cools its simulation (alpha decay) and crosses the line at 34 ticks whatever the graph's size. Its own end, alpha < 0.001, would take an estimated ~1,900 ticks at its default decay, and no run got there. Under SwiftShader cosmos ticks once per drawn frame, so 145 s is 34 frames of about 4 s each.
   - FA2 doesn't cool; it gets there when its steps converge.
2. **Layout step alone.**
   - cosmos.gl: 10 `step()`s of a headless `Graph(null)`. This is GPU work, done here by SwiftShader on the CPU, with nothing drawn.
   - FA2: its median iteration in the worker during the settle phase. This is CPU work on one core.
3. **Renderer main thread per frame** is the time the renderer's own `requestAnimationFrame` callbacks take on the main thread, per callback. `bench.js` wraps `requestAnimationFrame` before the renderer loads. This is a frame's CPU cost without the GPU's.
4. **Update call.** Each one sets the colour and size of 1,000 random functions:
   - cosmos.gl: `setPointColors`, `setPointSizes`, then `render()`.
   - Sigma: 1,000 `mergeNodeAttributes`. Sigma redoes its node data on the next frame, which is the main-thread cost in footnote 3.

   When a call takes longer than 100 ms, `setInterval` drops the ticks it missed, so the row below counts the updates that actually happened.
5. **Browser RSS** is the resident memory of Chromium and every process under it, SwiftShader's buffers in the GPU process included.

## 4. Why cosmos.gl

These measurements don't depend on the GPU:

- **Main thread during a build.** This is the page's main case, since functions light up as they're compiled.
  - cosmos.gl's own frame cost stays under 1 ms at 80k in every phase.
  - Sigma's renderer spends 90–98 ms of the main thread on every frame while nodes change at 80k, and 22 ms at 20k: linear in the graph. That's CPU work a real GPU doesn't change. It holds Sigma to about 10 fps at 80k on this CPU, and while it runs the page's panels (search, treemap, DAG) don't respond.
- **Memory.** The JS heap after layout is 73 MB against 458 MB at 80k, and 24 MB against 122 MB at 20k. Graphology keeps an object per node and per edge, while cosmos keeps typed arrays and GPU textures. Browser RSS is 1.50 GB against 1.84 GB.
- **First frame.** 1.15 s against 2.76 s at 80k. Building the graphology graph alone takes 0.7 s. The page starts from its crate/module packing, so the first frame is already readable before any force layout runs.

These measurements do depend on the GPU, and SwiftShader stands in for it:

- **Frames.** cosmos.gl is ahead in every condition: 0.49 against 0.36 fps at 80k, and 1.29 against 0.89 at 20k. Both are bound by SwiftShader's geometry work, which is why the 800x500 window didn't help. The absolute numbers say nothing about a real GPU.
- **Layout.** Here FA2 settles in half the time: 70 s against 145 s at 80k. That's because SwiftShader runs cosmos's GPU simulation on the CPU, and only one cosmos tick runs per drawn frame. Per step, cosmos is still cheaper: 330 ms against 445 ms. FA2's iterations are CPU work in a worker, so they won't get faster on a real GPU; cosmos's should, but this bench can't show it. This is the one place where cosmos.gl is measured behind, and I expect it to reverse on real hardware, though that is untested.
- **The live-update stall.** At 80k only 15 of 100 cosmos updates happened, each call taking about 590 ms. `probeLive` timed the parts of an update one by one, one per animation frame, after `gl.finish()`. That run is in `raw/probe-80k` and `raw/probe-20k`, made by `vm-probe.sh` on a second ix VM; only its `probe_live` entries count, because its other phases were shortened.
  - At 80k the update itself took about 7.6 ms: `render()` took a median of 7.4 ms (3.5–22.3 ms) and setting the colours 0.2 ms, while `gl.finish()` took 0 ms. At 20k it took about 1.8 ms, 1.6 ms of it `render()`.
  - So the 590 ms is the update waiting behind SwiftShader's 4-second frame, not work of its own.

Against cosmos.gl:

- Its bundle is bigger: 744,762 bytes (196 KB gzipped), against 459,324 + 5,974 bytes for Sigma with graphology and the FA2 worker.
- It needs WebGL2, which the page checks for and refuses to run without.
- Its asynchronous readbacks warn under a slow GPU.

## 5. What this does not measure

- **A real GPU.** Frame rates, the length of the cosmos.gl layout, and how much the live updates stall on one.
- **Layout quality.** Beyond the settled screenshots, nothing compares the layouts. The two "settled" criteria are not equivalent (footnote 1).
- **Tuning.** Neither arm was tuned beyond the page's settings. Sigma's default edge program, a `hideEdgesOnMove` setting, or a lower FA2 `barnesHutTheta` might each change its numbers.
- **Other browsers.** Only Chromium was tried; Firefox and Safari were not.

## 6. Running it

From a checkout, in a Linux VM with Nix:

```
cd crates/blaze-web/assets/vendor/build && npm ci && node build.mjs ../../../../../vb/out cosmos sigma fa2 && cd -
python3 crates/blaze-web/assets/dev/fixture.py '{"nodes": 80000, "edges": 300000, "seed": 1, "out": "fx/anim-like.blzw"}'
bash bench/results/2026-10-28-web-graph/vm-bench.sh     # about 1.5 h; writes raw/<condition>/
python3 bench/results/2026-10-28-web-graph/summary.py '{"raw": "raw"}'
```

`run.py` takes one JSON value: `assets`, `vendor`, `fixture`, `out`, `chromium`, and optionally `runs`, `arms`, `settle_cap_s`, `panzoom_s`, `live_s`, `width`, `height`, `probe`.

## 7. The page's screenshots

The page's own screenshots, on the 80k fixture with its edit records, are in `screens/`. They're 1600x1000 under the same CSP and SwiftShader, made by `vm-screens.sh` (`screens.py`):

- `cold.png`: the cold build.
- `cold-treemap.png`: compile time.
- `edit-mid.png`: the edit's spread under way. It was taken with `slow=20`, which stretches the animation 20× so a 4-second frame can catch it.
- `edit-settled.png`: after the spread.
- `phases.png`, `crates.png`, `crates-open.png` (the anim crate opened), `search.png`.

## 8. Screenshots of anim

The page served by `blaze ui` while blaze builds anim (69 crates, 79,696 functions, 149,538 calls), in `screens/anim/`. They're 1024x640 in headless Chromium on the lane's x86_64 VM, taken by `live.py`. The page was open before the cold build started and stayed open through two edits of `anim::main`:

- `cold.png`: the cold build finished (218 s), every function lit in its crate's colour.
- `edit.png`: the second edit's build finished. What it didn't compile is dim. The camera is on the 60 functions the edit reached (at the bottom left) and on what they call, with the calls between them lit.
- `edit-tab.png`: the Edit tab after the same build. It lists `anim::main` in `src/main.rs`, the one function compiled again in an edited file.

The daemon's RSS after the cold build and both edits was 121 MB in this run, and 167 MB in the run before it. Each REST query of the world (`Crate,(Calls,*)`, `Module`, `Function`) took 1 to 1.5 ms.

`check.py` (`vm-check.sh`) loads the page against hostile input and a live websocket, and checks that each one gives the right error panel or works.
