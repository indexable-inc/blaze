// One arm of the renderer bench: bench.html?renderer=cosmos|sigma&fixture=<url>. run.py
// calls the phases on window.bench in order and reads what each returns.
import { parseSnapshot, HOW_COMPILED } from "/assets/snapshot.js";
import { moduleOf } from "/assets/names.js";
import { hierarchicalLayout } from "/assets/layout.js";
import { crateColor } from "/assets/palette.js";

const params = new URLSearchParams(location.search);
const RENDERER = params.get("renderer");
const FIXTURE = params.get("fixture");
const SPACE = 4096;

const violations = [];
document.addEventListener("securitypolicyviolation", (e) =>
  violations.push(`${e.violatedDirective} blocked ${e.blockedURI} at ${e.sourceFile}:${e.lineNumber}`));
const errors = [];
window.addEventListener("error", (e) => errors.push(String(e.message)));
window.addEventListener("unhandledrejection", (e) => errors.push(String(e.reason?.stack ?? e.reason)));

const now = () => performance.now();

// Every animation-frame callback timed: the renderer's main-thread work per frame, which a
// real GPU does not take away (SwiftShader's raster work happens in the GPU process).
const raf = { ms: 0, calls: 0 };
const nativeRaf = window.requestAnimationFrame.bind(window);
window.requestAnimationFrame = (cb) => nativeRaf((t) => {
  const s = performance.now();
  try {
    cb(t);
  } finally {
    raf.ms += performance.now() - s;
    raf.calls++;
  }
});
const frame = () => new Promise((r) => nativeRaf(r));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function rng(seed) {
  let s = seed >>> 0;
  return () => {
    s = (s + 0x6d2b79f5) >>> 0;
    let t = s;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

// What both arms draw: the same positions to start from, colours and sizes.
async function prepare() {
  const t0 = now();
  const bytes = new Uint8Array(await (await fetch(FIXTURE)).arrayBuffer());
  const t1 = now();
  const s = parseSnapshot(bytes);
  const t2 = now();
  const moduleIndex = new Map();
  const group = new Uint32Array(s.n);
  const groupCrate = [];
  for (let i = 0; i < s.n; i++) {
    const m = s.crates[s.krate[i]] + "\u0000" + moduleOf(s.names[i], s.crates[s.krate[i]]);
    let g = moduleIndex.get(m);
    if (g === undefined) {
      g = moduleIndex.size;
      moduleIndex.set(m, g);
      groupCrate.push(s.krate[i]);
    }
    group[i] = g;
  }
  const lay = hierarchicalLayout(s.n, group, moduleIndex.size, groupCrate, s.crates.length, SPACE);
  const t3 = now();
  const colors = new Float32Array(4 * s.n);
  const sizes = new Float32Array(s.n);
  const palette = s.crates.map(crateColor);
  for (let i = 0; i < s.n; i++) {
    const c = palette[s.krate[i]];
    const dim = s.how[i] === HOW_COMPILED ? 1 : 0.35;
    colors.set([c[0] * dim, c[1] * dim, c[2] * dim, 1], 4 * i);
    sizes[i] = s.ns[i] > 0 ? 1.5 + 1.2 * Math.log10(1 + s.ns[i] / 10000) : 1.5;
  }
  return {
    s, group, groupCrate, lay, colors, sizes, nGroups: moduleIndex.size,
    prep: { fetchMs: t1 - t0, parseMs: t2 - t1, layoutMs: t3 - t2, nodes: s.n, edges: s.edges.length / 2, modules: moduleIndex.size },
  };
}

function glInfo(canvas) {
  const gl = canvas?.getContext("webgl2");
  if (!gl) return "no webgl2 context on the canvas";
  const ext = gl.getExtension("WEBGL_debug_renderer_info");
  return ext ? gl.getParameter(ext.UNMASKED_RENDERER_WEBGL) : gl.getParameter(gl.RENDERER);
}

// The camera path both arms follow for the pan/zoom phases: around the middle, zooming 1x to 4x.
function cameraAt(t) {
  const a = (t / 3000) * 2 * Math.PI;
  const zoom = 1 + 1.5 * (1 - Math.cos((t / 4000) * 2 * Math.PI));
  return { u: 0.5 + 0.2 * Math.cos(a), v: 0.5 + 0.2 * Math.sin(a), zoom };
}

async function measureFrames(ms, moveCamera, onTick) {
  const intervals = [];
  let last = now();
  const start = last;
  const raf0 = { ...raf };
  let tickMs = 0, ticks = 0;
  const timer = onTick ? setInterval(() => { const t = now(); onTick(); tickMs += now() - t; ticks++; }, 100) : null;
  while (now() - start < ms) {
    moveCamera(now() - start);
    await frame();
    const t = now();
    intervals.push(t - last);
    last = t;
  }
  if (timer) clearInterval(timer);
  const elapsed = now() - start;
  const sorted = intervals.slice().sort((a, b) => a - b);
  const q = (p) => sorted[Math.min(sorted.length - 1, Math.floor(p * sorted.length))];
  return {
    frames: intervals.length, seconds: elapsed / 1000, fps: intervals.length / (elapsed / 1000),
    p50ms: q(0.5), p95ms: q(0.95), maxMs: sorted[sorted.length - 1],
    rendererRafCalls: raf.calls - raf0.calls,
    rendererRafMsPerCall: (raf.ms - raf0.ms) / Math.max(1, raf.calls - raf0.calls),
    ...(onTick ? { updates: ticks, updateMeanMs: tickMs / Math.max(ticks, 1) } : {}),
  };
}

function liveUpdater(d, apply) {
  const r = rng(7);
  let flip = false;
  return () => {
    flip = !flip;
    const idx = new Uint32Array(1000);
    for (let k = 0; k < 1000; k++) idx[k] = Math.floor(r() * d.s.n);
    apply(idx, flip);
  };
}

const SETTLE_RATIO = 0.0005;

// ---------------------------------------------------------------- cosmos.gl
async function cosmosArm(d, stage) {
  const { Graph } = await import("/vendor/cosmos.js");
  let ended = null, ticks = 0;
  const t0 = now();
  const graph = new Graph(stage, {
    spaceSize: SPACE, pixelRatio: 1, backgroundColor: "#0d1117", attribution: "",
    pointDefaultSize: 2, scalePointsOnZoom: true, rescalePositions: false, fitViewOnInit: false,
    linkDefaultColor: [0.6, 0.65, 0.75, 1], linkOpacity: 0.12, linkDefaultWidth: 0.5,
    simulationCluster: 0.4, simulationLinkSpring: 0.4, simulationLinkDistance: 4,
    simulationRepulsion: 0.4, simulationGravity: 0.05, simulationFriction: 0.85,
    onSimulationTick: () => { ticks++; },
    onSimulationEnd: () => { ended ??= now(); },
  });
  await graph.ready;
  const tReady = now();
  graph.setPointPositions(d.lay.pos);
  graph.setPointColors(d.colors);
  graph.setPointSizes(d.sizes);
  graph.setLinks(new Float32Array(d.s.edges));
  graph.setPointClusters(Array.from(d.group));
  graph.setClusterPositions(Array.from(d.lay.centres));
  graph.render(1);
  graph.fitView(0);
  await frame();
  await frame();
  const tFirst = now();
  const canvas = stage.querySelector("canvas");
  const info = glInfo(canvas);
  const colors = d.colors.slice();
  const sizes = d.sizes.slice();
  const k0 = Math.min(stage.clientWidth, stage.clientHeight) / (SPACE * 0.92);

  return {
    graph,
    setup: { readyMs: tReady - t0, firstFrameMs: tFirst - t0, renderer: info },
    async settle(capMs) {
      const tS = now();
      let prev = await graph.getPointPositionsAsync();
      let prevTicks = ticks;
      let settledAt = null, settledTicks = null, lastStep = null;
      const trace = [];
      while (now() - tS < capMs && ended === null) {
        for (let f = 0; f < 30 && ended === null; f++) await frame();
        const cur = await graph.getPointPositionsAsync();
        const dt = ticks - prevTicks;
        if (dt > 0 && cur.length === prev.length) {
          let sum = 0, x0 = Infinity, x1 = -Infinity, y0 = Infinity, y1 = -Infinity;
          for (let i = 0; i < cur.length; i += 2) {
            sum += Math.hypot(cur[i] - prev[i], cur[i + 1] - prev[i + 1]);
            x0 = Math.min(x0, cur[i]); x1 = Math.max(x1, cur[i]); y0 = Math.min(y0, cur[i + 1]); y1 = Math.max(y1, cur[i + 1]);
          }
          const step = sum / (cur.length / 2) / dt;
          const diag = Math.hypot(x1 - x0, y1 - y0);
          lastStep = step / diag;
          trace.push([Math.round(now() - tS), ticks, +lastStep.toExponential(3)]);
          if (settledAt === null && lastStep < SETTLE_RATIO) { settledAt = now(); settledTicks = ticks; }
        }
        prev = cur;
        prevTicks = ticks;
      }
      const progress = graph.progress;
      // The later phases measure drawing alone, as sigma's do once FA2 has stopped.
      if (ended === null) graph.pause();
      return {
        definition: `alpha < 0.001 (cosmos onSimulationEnd); and mean step per tick < ${SETTLE_RATIO} of the layout diagonal`,
        alphaEndMs: ended === null ? null : ended - tS, ticks, progress,
        ticksToAlphaEndEstimate: progress > 0 ? Math.round(ticks / progress) : null,
        stepSettledMs: settledAt === null ? null : settledAt - tS, stepSettledTicks: settledTicks,
        capped: ended === null, lastStepRatio: lastStep, trace,
      };
    },
    // The simulation without drawing: a headless instance stepped by hand, the GPU drained by
    // a position read after the steps.
    async simOnly(steps) {
      const h = new Graph(null, { spaceSize: SPACE, rescalePositions: false, simulationCluster: 0.4, simulationLinkSpring: 0.4,
        simulationLinkDistance: 4, simulationRepulsion: 0.4, simulationGravity: 0.05, simulationFriction: 0.85 });
      await h.ready;
      h.setPointPositions(d.lay.pos);
      h.setLinks(new Float32Array(d.s.edges));
      h.setPointClusters(Array.from(d.group));
      h.setClusterPositions(Array.from(d.lay.centres));
      h.render(1);
      h.step();
      h.getPointPositionsArray();
      const t = now();
      for (let k = 0; k < steps; k++) h.step();
      h.getPointPositionsArray();
      const ms = (now() - t) / steps;
      h.destroy();
      return { steps, msPerStep: ms };
    },
    moveCamera(t) {
      const { u, v, zoom } = cameraAt(t);
      graph.setZoomTransformByPointPositions(new Float32Array([u * SPACE, v * SPACE]), 0, k0 * zoom);
    },
    reset() { graph.fitView(0); },
    applyLive(idx, flip) {
      for (const i of idx) {
        if (flip) colors.set([1, 0.95, 0.7, 1], 4 * i); else colors.set(d.colors.subarray(4 * i, 4 * i + 4), 4 * i);
        sizes[i] = flip ? d.sizes[i] * 1.8 : d.sizes[i];
      }
      graph.setPointColors(colors);
      graph.setPointSizes(sizes);
      graph.render(undefined, 0);
    },
    // applyLive's parts timed one by one, after draining the GPU: whether its cost is waiting
    // for the frame in flight or work of its own.
    probeLive(idx, flip) {
      const gl = canvas.getContext("webgl2");
      const t0 = now();
      gl.finish();
      const t1 = now();
      for (const i of idx) {
        if (flip) colors.set([1, 0.95, 0.7, 1], 4 * i); else colors.set(d.colors.subarray(4 * i, 4 * i + 4), 4 * i);
        sizes[i] = flip ? d.sizes[i] * 1.8 : d.sizes[i];
      }
      graph.setPointColors(colors);
      const t2 = now();
      graph.setPointSizes(sizes);
      const t3 = now();
      graph.render(undefined, 0);
      const t4 = now();
      return { finishMs: t1 - t0, colorsMs: t2 - t1, sizesMs: t3 - t2, renderMs: t4 - t3 };
    },
  };
}

// ---------------------------------------------------------------- sigma + FA2
async function sigmaArm(d, stage) {
  const { Sigma, Graphology, inferSettings } = await import("/vendor/sigma.js");
  const t0 = now();
  const g = new Graphology({ type: "directed", multi: false, allowSelfLoops: false });
  const hex = (c) => "#" + [0, 1, 2].map((k) => Math.round(c[k] * 255).toString(16).padStart(2, "0")).join("");
  const baseColor = new Array(d.s.n);
  for (let i = 0; i < d.s.n; i++) {
    baseColor[i] = hex(d.colors.subarray(4 * i, 4 * i + 3));
    g.addNode(String(i), { x: d.lay.pos[2 * i], y: d.lay.pos[2 * i + 1], size: d.sizes[i], color: baseColor[i] });
  }
  for (let e = 0; e < d.s.edges.length; e += 2) g.addEdge(String(d.s.edges[e]), String(d.s.edges[e + 1]), { color: "rgba(153,166,191,0.12)", size: 0.5 });
  const tGraph = now();
  const sigma = new Sigma(g, stage, { settings: { renderLabels: false, renderEdgeLabels: false, hideEdgesOnMove: false, stagePadding: 20 } });
  await frame();
  await frame();
  const tFirst = now();
  const canvas = stage.querySelector("canvas");
  const info = glInfo(canvas);
  const camera = sigma.getCamera();

  return {
    graph: g,
    setup: { graphologyMs: tGraph - t0, firstFrameMs: tFirst - t0, renderer: info },
    async settle(capMs) {
      const n = d.s.n;
      const nodes = new Float32Array(10 * n);
      const deg = new Uint32Array(n);
      for (let e = 0; e < d.s.edges.length; e++) deg[d.s.edges[e]]++;
      for (let i = 0; i < n; i++) {
        nodes[10 * i] = d.lay.pos[2 * i];
        nodes[10 * i + 1] = d.lay.pos[2 * i + 1];
        nodes[10 * i + 6] = 1 + deg[i];
        nodes[10 * i + 7] = 1;
        nodes[10 * i + 8] = 1;
      }
      const m = d.s.edges.length / 2;
      const edges = new Float32Array(3 * m);
      for (let e = 0; e < m; e++) {
        edges[3 * e] = 10 * d.s.edges[2 * e];
        edges[3 * e + 1] = 10 * d.s.edges[2 * e + 1];
        edges[3 * e + 2] = 1;
      }
      const settings = {
        linLogMode: false, outboundAttractionDistribution: false, adjustSizes: false,
        edgeWeightInfluence: 1, barnesHutTheta: 0.5, ...inferSettings(g), barnesHutOptimize: true,
      };
      const worker = new Worker("/vendor/fa2-worker.js");
      const tS = now();
      let latest = null, settledAt = null, settledIt = null, lastMsg = null, msPerIt = [];
      const trace = [];
      const done = new Promise((resolve) => {
        worker.onmessage = (e) => {
          const msg = e.data;
          latest = msg;
          lastMsg = msg;
          msPerIt.push(msg.ms);
          const ratio = msg.meanStep / msg.diag;
          if (msg.iteration % 10 === 0) trace.push([Math.round(now() - tS), msg.iteration, +ratio.toExponential(3)]);
          if (settledAt === null && ratio < SETTLE_RATIO) { settledAt = now(); settledIt = msg.iteration; resolve(); }
          if (now() - tS > capMs) resolve();
        };
        worker.onerror = (e) => { errors.push(`fa2 worker: ${e.message}`); resolve(); };
      });
      worker.postMessage({ nodes, edges, settings, every: 1 });
      let applying = true;
      (async () => {
        while (applying) {
          await frame();
          if (latest) {
            const { x, y } = latest;
            latest = null;
            g.updateEachNodeAttributes((key, attr) => { const i = +key; attr.x = x[i]; attr.y = y[i]; return attr; }, { attributes: ["x", "y"] });
          }
        }
      })();
      await done;
      worker.postMessage("stop");
      worker.terminate();
      applying = false;
      await frame();
      msPerIt.sort((a, b) => a - b);
      return {
        definition: `ForceAtlas2 (Barnes-Hut, inferred settings) until mean step per iteration < ${SETTLE_RATIO} of the layout diagonal`,
        stepSettledMs: settledAt === null ? null : settledAt - tS, stepSettledIterations: settledIt,
        iterations: lastMsg?.iteration ?? 0, iterationMedianMs: msPerIt[Math.floor(msPerIt.length / 2)] ?? null,
        capped: settledAt === null, lastStepRatio: lastMsg ? lastMsg.meanStep / lastMsg.diag : null, settings, trace,
      };
    },
    moveCamera(t) {
      const { u, v, zoom } = cameraAt(t);
      camera.setState({ x: u, y: v, ratio: 1 / zoom });
    },
    reset() { camera.setState({ x: 0.5, y: 0.5, ratio: 1 }); },
    applyLive(idx, flip) {
      for (const i of idx) {
        g.mergeNodeAttributes(String(i), flip ? { color: "#fff2b3", size: d.sizes[i] * 1.8 } : { color: baseColor[i], size: d.sizes[i] });
      }
    },
  };
}

let data = null, arm = null;
window.bench = {
  async setup() {
    data = await prepare();
    const stage = document.getElementById("stage");
    if (RENDERER === "cosmos") arm = await cosmosArm(data, stage);
    else if (RENDERER === "sigma") arm = await sigmaArm(data, stage);
    else throw new Error(`renderer ${RENDERER}: not cosmos or sigma`);
    return { prep: data.prep, ...arm.setup };
  },
  async settle(capMs) { return arm.settle(capMs); },
  async simOnly(steps) { return arm.simOnly ? arm.simOnly(steps) : null; },
  async panzoom(ms) {
    const r = await measureFrames(ms, (t) => arm.moveCamera(t));
    arm.reset();
    return r;
  },
  async live(ms) {
    const r = await measureFrames(ms, () => {}, liveUpdater(data, (idx, flip) => arm.applyLive(idx, flip)));
    await sleep(200);
    return r;
  },
  async liveMoving(ms) {
    const r = await measureFrames(ms, (t) => arm.moveCamera(t), liveUpdater(data, (idx, flip) => arm.applyLive(idx, flip)));
    arm.reset();
    await sleep(200);
    return r;
  },
  async probeLive(count) {
    if (!arm.probeLive) return null;
    const r = rng(7);
    const out = [];
    for (let k = 0; k < count; k++) {
      await frame();
      const idx = new Uint32Array(1000);
      for (let j = 0; j < 1000; j++) idx[j] = Math.floor(r() * data.s.n);
      out.push(arm.probeLive(idx, k % 2 === 0));
    }
    return out;
  },
  memory() {
    const m = performance.memory;
    return m ? { usedJSHeapMB: m.usedJSHeapSize / 2 ** 20, totalJSHeapMB: m.totalJSHeapSize / 2 ** 20 } : null;
  },
  problems() { return { violations, errors }; },
};
