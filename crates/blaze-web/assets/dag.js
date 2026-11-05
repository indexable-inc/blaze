// The crate DAG: crates and the calls between them, a crate opening to its modules and a
// module to its functions. Calls from generic code instantiated for a caller can point back up,
// so the graph may have cycles: those calls are drawn, but do not order the layers.
import { crateColor, css } from "./palette.js";
import { fmtNs, fmtCount, el, svg } from "./ui.js";

const MAX_FNS = 250;
const ROW = 64;
const MAX_ROW = 1400;
const H = 30;

export class CrateDag {
  constructor(root, model, handlers) {
    this.root = root;
    this.model = model;
    this.handlers = handlers;
    this.openCrates = new Set();
    this.openModules = new Set();
    this.chips = el("div", "chips");
    this.note = el("div", "note");
    this.scroller = el("div", "dag-scroll");
    root.replaceChildren(this.chips, this.note, this.scroller);
    this.dirty = true;
  }

  setModel(model) {
    this.model = model;
    this.openCrates.clear();
    this.openModules.clear();
    this.dirty = true;
  }

  // A cold build adds functions in hundreds of records: what is shown is drawn again at most
  // once a second.
  invalidate() {
    this.dirty = true;
    if (this.visible && !this.timer) {
      this.timer = setTimeout(() => {
        this.timer = 0;
        if (this.visible) this.show();
      }, 1000);
    }
  }

  show() {
    this.visible = true;
    if (this.dirty) this.render();
  }

  hide() { this.visible = false; }

  // Which drawn unit each function falls in.
  units() {
    const m = this.model;
    const unitOf = new Int32Array(m.n);
    const units = [];
    const index = new Map();
    const unit = (id, make) => {
      let u = index.get(id);
      if (u === undefined) {
        u = units.length;
        index.set(id, u);
        units.push({ ...make(), count: 0, ns: 0, members: [] });
      }
      return u;
    };
    const capped = new Map();
    for (const g of this.openModules) {
      const fns = [];
      for (let i = 0; i < m.n; i++) if (m.group[i] === g) fns.push(i);
      fns.sort((a, b) => m.ns[b] - m.ns[a]);
      capped.set(g, new Set(fns.slice(0, MAX_FNS)));
    }
    for (let i = 0; i < m.n; i++) {
      const c = m.krate[i], g = m.group[i];
      let u;
      if (!this.openCrates.has(c)) u = unit(`c${c}`, () => ({ kind: "crate", crate: c, label: m.crates[c] }));
      else if (!this.openModules.has(g)) u = unit(`m${g}`, () => ({ kind: "module", crate: c, group: g, label: m.modules[g] }));
      else if (capped.get(g).has(i)) u = unit(`f${i}`, () => ({ kind: "fn", crate: c, node: i, label: m.names[i] }));
      else u = unit(`r${g}`, () => ({ kind: "rest", crate: c, group: g, label: `${m.modules[g]}: the rest` }));
      unitOf[i] = u;
      units[u].count++;
      units[u].ns += m.ns[i];
      units[u].members.push(i);
    }
    const edges = new Map();
    for (let e = 0; e < m.m; e++) {
      const a = unitOf[m.edges[2 * e]], b = unitOf[m.edges[2 * e + 1]];
      if (a === b) continue;
      const k = a * units.length + b;
      edges.set(k, (edges.get(k) ?? 0) + 1);
    }
    return { units, edges: [...edges].map(([k, w]) => ({ a: Math.floor(k / units.length), b: k % units.length, w })) };
  }

  render() {
    this.dirty = false;
    const m = this.model;
    const { units, edges } = this.units();
    const layers = layer(units.length, edges);

    this.chips.replaceChildren(
      ...[...this.openCrates].map((c) => chip(`${m.crates[c]}`, () => { this.openCrates.delete(c); for (const g of [...this.openModules]) if (m.moduleCrate[g] === c) this.openModules.delete(g); this.render(); })),
      ...[...this.openModules].map((g) => chip(m.modules[g], () => { this.openModules.delete(g); this.render(); })),
    );
    const capped = [...this.openModules].filter((g) => units.some((u) => u.kind === "rest" && u.group === g));
    this.note.textContent = `${units.length} shown, ${fmtCount(edges.length)} call bundles. Click a crate to open its modules, a module its functions.` +
      (capped.length ? ` Open modules show their ${MAX_FNS} slowest functions; the rest are one box.` : "");

    const widths = units.map((u) => Math.min(220, 14 + 6.6 * Math.min(u.label.length, 32)));
    const x = new Float64Array(units.length), y = new Float64Array(units.length);
    const rows = [];
    for (const row of layers) {
      let cur = [], cx = 10;
      for (const u of row) {
        if (cur.length && cx + widths[u] > MAX_ROW) { rows.push(cur); cur = []; cx = 10; }
        cur.push(u);
        x[u] = cx;
        cx += widths[u] + 12;
      }
      if (cur.length) rows.push(cur);
    }
    let width = 0;
    for (const row of rows) { const l = row[row.length - 1]; width = Math.max(width, x[l] + widths[l] + 12); }
    rows.forEach((row, r) => {
      const last = row[row.length - 1];
      const shift = (width - (x[last] + widths[last] + 12)) / 2;
      for (const u of row) { x[u] += shift; y[u] = 14 + r * ROW; }
    });
    const height = 28 + rows.length * ROW;
    const s = svg("svg", { class: "dag", width, height, viewBox: `0 0 ${width} ${height}` });
    const maxW = Math.max(1, ...edges.map((e) => e.w));
    const gEdges = svg("g", { class: "dag-edges" });
    for (const e of edges.sort((p, q) => p.w - q.w)) {
      const x1 = x[e.a] + widths[e.a] / 2, y1 = y[e.a] + H, x2 = x[e.b] + widths[e.b] / 2, y2 = y[e.b];
      const up = y2 <= y1;
      const my = (y1 + y2) / 2;
      const d = up
        ? `M${x1},${y1} C${x1},${y1 + 40} ${x2},${y2 - 40} ${x2},${y2}`
        : `M${x1},${y1} C${x1},${my} ${x2},${my} ${x2},${y2}`;
      const p = svg("path", { d, class: up ? "dag-edge back" : "dag-edge", "stroke-width": (0.6 + 3 * Math.log1p(e.w) / Math.log1p(maxW)).toFixed(2) });
      p.appendChild(svg("title", {}, `${units[e.a].label} → ${units[e.b].label}: ${fmtCount(e.w)} calls`));
      gEdges.appendChild(p);
    }
    s.appendChild(gEdges);
    units.forEach((u, i) => {
      const g = svg("g", { class: `dag-node ${u.kind}`, transform: `translate(${x[i]},${y[i]})` });
      const col = crateColor(m.crates[u.crate]);
      g.appendChild(svg("rect", { width: widths[i], height: H, rx: 5, fill: css(col, u.kind === "crate" ? 0.22 : 0.14), stroke: css(col, 0.9) }));
      const label = u.kind === "module" && this.openCrates.has(u.crate) ? u.label.slice(m.crates[u.crate].length).replace(/^::/, "") || m.crates[u.crate] : u.label;
      g.appendChild(svg("text", { x: 7, y: 13, class: "dag-label" }, ellipsis(label, 32)));
      g.appendChild(svg("text", { x: 7, y: 25, class: "dag-sub" }, `${fmtCount(u.count)} fn · ${fmtNs(u.ns)}`));
      g.appendChild(svg("title", {}, `${u.label}\n${fmtCount(u.count)} functions, ${fmtNs(u.ns)} compiling`));
      g.addEventListener("click", () => this.click(u));
      g.addEventListener("mouseenter", () => this.handlers.onHoverUnit?.(u.members));
      g.addEventListener("mouseleave", () => this.handlers.onHoverUnit?.(null));
      s.appendChild(g);
    });
    this.scroller.replaceChildren(s);
  }

  click(u) {
    if (u.kind === "crate") {
      this.openCrates.add(u.crate);
      this.render();
    } else if (u.kind === "module") {
      this.openModules.add(u.group);
      this.render();
    } else if (u.kind === "fn") {
      this.handlers.onSelect?.(u.node);
    }
    this.handlers.onFocusUnit?.(u.members);
  }
}

function chip(text, onClose) {
  const c = el("button", "chip", `${text} ✕`);
  c.title = "close";
  c.addEventListener("click", onClose);
  return c;
}

function ellipsis(s, n) {
  return s.length > n ? "…" + s.slice(s.length - n + 1) : s;
}

// Longest-path layers over the graph without its back edges, then a few barycentre sweeps to
// untangle each layer's order. The back edges are those against an order found by Eades, Lin
// and Smyth's greedy feedback arc set, weighted by calls, so the heavier direction wins.
function layer(n, edges) {
  const out = Array.from({ length: n }, () => []);
  const inn = Array.from({ length: n }, () => []);
  for (const e of edges) { out[e.a].push(e); inn[e.b].push(e); }
  const rank = feedbackOrder(n, out, inn);
  const forward = Array.from({ length: n }, () => []);
  for (const e of edges) if (rank[e.a] < rank[e.b]) forward[e.a].push(e.b);
  const depth = new Int32Array(n);
  const indeg = new Int32Array(n);
  for (let v = 0; v < n; v++) for (const w of forward[v]) indeg[w]++;
  const queue = [];
  for (let v = 0; v < n; v++) if (!indeg[v]) queue.push(v);
  for (let q = 0; q < queue.length; q++) {
    const v = queue[q];
    for (const w of forward[v]) {
      depth[w] = Math.max(depth[w], depth[v] + 1);
      if (--indeg[w] === 0) queue.push(w);
    }
  }
  const layers = [];
  for (let v = 0; v < n; v++) (layers[depth[v]] ??= []).push(v);
  for (let i = 0; i < layers.length; i++) layers[i] ??= [];
  const pos = new Float64Array(n);
  const place = () => layers.forEach((row) => row.forEach((v, i) => { pos[v] = i; }));
  place();
  for (let sweep = 0; sweep < 4; sweep++) {
    for (let d = 1; d < layers.length; d++) {
      const bary = (v) => (inn[v].length ? inn[v].reduce((s, e) => s + pos[e.a], 0) / inn[v].length : pos[v]);
      layers[d].sort((a, b) => bary(a) - bary(b));
      place();
    }
  }
  return layers;
}

// Each unit's place in an order with few calls (by weight) going backwards: sinks go to the
// end, sources to the front, and otherwise the unit with the most more calls out than in.
function feedbackOrder(n, out, inn) {
  const gone = new Uint8Array(n);
  const wOut = new Float64Array(n), wIn = new Float64Array(n);
  const dOut = new Int32Array(n), dIn = new Int32Array(n);
  for (let v = 0; v < n; v++) {
    for (const e of out[v]) { wOut[v] += e.w; dOut[v]++; }
    for (const e of inn[v]) { wIn[v] += e.w; dIn[v]++; }
  }
  const front = [], back = [];
  const remove = (v) => {
    gone[v] = 1;
    for (const e of out[v]) if (!gone[e.b]) { wIn[e.b] -= e.w; dIn[e.b]--; }
    for (const e of inn[v]) if (!gone[e.a]) { wOut[e.a] -= e.w; dOut[e.a]--; }
  };
  let left = n;
  while (left) {
    let moved = true;
    while (moved) {
      moved = false;
      for (let v = 0; v < n; v++) {
        if (gone[v]) continue;
        if (dOut[v] === 0) { back.push(v); remove(v); left--; moved = true; }
        else if (dIn[v] === 0) { front.push(v); remove(v); left--; moved = true; }
      }
    }
    if (!left) break;
    let best = -1, score = -Infinity;
    for (let v = 0; v < n; v++) if (!gone[v] && wOut[v] - wIn[v] > score) { score = wOut[v] - wIn[v]; best = v; }
    front.push(best);
    remove(best);
    left--;
  }
  const rank = new Int32Array(n);
  [...front, ...back.reverse()].forEach((v, i) => { rank[v] = i; });
  return rank;
}
