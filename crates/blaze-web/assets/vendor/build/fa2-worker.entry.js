// ForceAtlas2 on the flat matrices graphology-layout-forceatlas2 uses (10 floats a node,
// 3 an edge), one iteration at a time, saying each iteration's mean displacement so the
// caller can tell when it has settled.
//   in:  {nodes: Float32Array, edges: Float32Array, settings, every}
//   out: {iteration, ms, meanStep, diag, x: Float32Array, y: Float32Array}  every `every` iterations
//   in:  "stop"
import iterate from "graphology-layout-forceatlas2/iterate.js";

const PPN = 10;
let running = false;

self.onmessage = (e) => {
  if (e.data === "stop") {
    running = false;
    return;
  }
  const { nodes, edges, settings, every } = e.data;
  const n = nodes.length / PPN;
  const px = new Float32Array(n);
  const py = new Float32Array(n);
  running = true;
  let it = 0;
  const loop = () => {
    const t0 = performance.now();
    let steps = 0;
    let meanStep = 0;
    while (steps < every && running) {
      for (let i = 0; i < n; i++) { px[i] = nodes[i * PPN]; py[i] = nodes[i * PPN + 1]; }
      iterate(settings, nodes, edges);
      let sum = 0;
      for (let i = 0; i < n; i++) sum += Math.hypot(nodes[i * PPN] - px[i], nodes[i * PPN + 1] - py[i]);
      meanStep = sum / n;
      steps++;
      it++;
    }
    let x0 = Infinity, x1 = -Infinity, y0 = Infinity, y1 = -Infinity;
    const x = new Float32Array(n), y = new Float32Array(n);
    for (let i = 0; i < n; i++) {
      x[i] = nodes[i * PPN]; y[i] = nodes[i * PPN + 1];
      if (x[i] < x0) x0 = x[i]; if (x[i] > x1) x1 = x[i];
      if (y[i] < y0) y0 = y[i]; if (y[i] > y1) y1 = y[i];
    }
    self.postMessage({ iteration: it, ms: (performance.now() - t0) / steps, meanStep, diag: Math.hypot(x1 - x0, y1 - y0), x, y }, [x.buffer, y.buffer]);
    if (running) setTimeout(loop, 0);
  };
  loop();
};
