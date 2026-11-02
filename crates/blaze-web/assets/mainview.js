// The main view: every function a point, every call a line, laid out and drawn on the GPU by
// cosmos.gl. Colour is the crate (dim when the build shown did not compile it), size the compile time.
import { Graph } from "./vendor/cosmos.js";
import { hierarchicalLayout } from "./layout.js";
import { FLASH_MS, Paint } from "./paint.js";

export const SPACE = 4096;
const SPREAD_STEP_MS = 420;

export function webgl2Missing() {
  const c = document.createElement("canvas");
  return c.getContext("webgl2") ? null : "This page draws the program with WebGL2, and this browser has no WebGL2 context (is hardware acceleration off?).";
}

export class MainView {
  // `slow` stretches the live animations' timing, for recording them on a software renderer.
  constructor(container, model, handlers, slow = 1) {
    this.container = container;
    this.model = model;
    this.handlers = handlers;
    this.slow = slow;
    this.paint = new Paint(model, slow);
    this.outlined = new Set();
    this.spreadLinks = new Set();
    this.spreadTimers = [];
    this.selection = null;
    this.animFrame = 0;
    this.labels = [];
    this.labelLayer = document.createElement("div");
    this.labelLayer.className = "crate-labels";
    container.appendChild(this.labelLayer);
    this.graph = new Graph(container, {
      spaceSize: SPACE,
      backgroundColor: "#0b0f14",
      attribution: "",
      rescalePositions: false,
      fitViewOnInit: true,
      fitViewDelay: 0,
      fitViewPadding: 0.06,
      pointDefaultSize: 2,
      pointSizeScale: 1,
      scalePointsOnZoom: false,
      pointGreyoutOpacity: 0.2,
      renderHoveredPointRing: true,
      hoveredPointRingColor: "#e6edf3",
      focusedPointRingColor: "#f0b72f",
      outlinedPointRingColor: "#f0b72f",
      linkDefaultColor: [0.55, 0.62, 0.75, 1],
      linkOpacity: 0.1,
      linkGreyoutOpacity: 0.02,
      linkDefaultWidth: 0.6,
      linkVisibilityDistanceRange: [40, 220],
      linkVisibilityMinTransparency: 0.04,
      simulationCluster: 0.4,
      simulationLinkSpring: 0.4,
      simulationLinkDistance: 4,
      simulationRepulsion: 0.4,
      simulationGravity: 0.05,
      simulationFriction: 0.85,
      simulationDecay: 1500,
      randomSeed: 1,
      onSimulationEnd: () => { this.updateCrateCentres(); this.handlers.onLayout?.(false); },
      onSimulationTick: () => this.maybeUpdateCentres(),
      onZoom: () => this.placeLabels(),
      onPointClick: (i) => this.handlers.onSelect?.(i),
      onBackgroundClick: () => this.handlers.onSelect?.(null),
      onPointMouseOver: (i, _p, ev) => this.handlers.onHover?.(i, ev),
      onPointMouseOut: () => this.handlers.onHover?.(null),
    });
    this.lastCentres = 0;
    this.load();
  }

  // Positions, colours, sizes, calls and clusters from the model, all at once.
  load() {
    const m = this.model;
    this.lay = hierarchicalLayout(m.n, m.group, m.modules.length, m.moduleCrate, m.crates.length, SPACE);
    this.packedFor = m.n;
    this.positions = this.lay.pos;
    this.restyleAll(false);
    this.graph.setPointPositions(this.positions);
    this.graph.setLinks(new Float32Array(m.edges.subarray(0, 2 * m.m)));
    this.graph.setPointClusters(Array.from(m.group.subarray(0, m.n)));
    this.graph.setClusterPositions(Array.from(this.lay.centres));
    this.graph.render(1);
    this.crateCentres = this.lay.crateCentres.slice();
    this.buildLabels();
    this.handlers.onLayout?.(true);
  }

  // cosmos.gl takes whole arrays, and uploads all of each it is given: that is O(n) a frame on
  // the GPU side, whatever changed.
  upload(render) {
    const { colors, sizes } = this.paint.points();
    this.graph.setPointColors(colors);
    this.graph.setPointSizes(sizes);
    if (render) this.graph.render(undefined, 0);
  }

  restyleAll(upload = true) {
    this.paint.all(performance.now());
    this.upload(upload);
  }

  // The points whose functions changed, and with `relit` the build shown is another.
  restyle(points, relit, upload = true) {
    this.paint.some(points, relit, performance.now());
    this.upload(upload);
  }

  // New nodes from a Functions record: each starts at its module's centre, or its crate's for a
  // new module. `touched` and `relit` are restyled as `restyle` does.
  addNodes(added, newEdges, touched, relit) {
    const m = this.model;
    // The packing is of the program as it was: one grown by a quarter since (a cold build's,
    // which starts from nothing) is packed again.
    if (m.n > 1.25 * this.packedFor + 64) {
      this.load();
      this.fit();
      return;
    }
    const old = this.graph.getPointPositionsArray();
    const pos = new Float32Array(2 * m.n);
    pos.set(old.subarray(0, Math.min(old.length, pos.length)));
    const centres = this.lay.centres;
    const grownCentres = new Float32Array(2 * m.modules.length);
    grownCentres.set(centres.subarray(0, Math.min(centres.length, grownCentres.length)));
    for (let g = centres.length / 2; g < m.modules.length; g++) {
      const c = m.moduleCrate[g];
      const cc = this.crateCentres.length > 2 * c ? [this.crateCentres[2 * c], this.crateCentres[2 * c + 1]] : [SPACE / 2, SPACE / 2];
      grownCentres[2 * g] = cc[0];
      grownCentres[2 * g + 1] = cc[1];
    }
    this.lay.centres = grownCentres;
    for (const i of added) {
      const g = m.group[i];
      pos[2 * i] = grownCentres[2 * g] + (Math.random() - 0.5) * 20;
      pos[2 * i + 1] = grownCentres[2 * g + 1] + (Math.random() - 0.5) * 20;
    }
    this.positions = pos;
    this.graph.setPointPositions(pos, true);
    if (added.length || newEdges) this.graph.setLinks(new Float32Array(m.edges.subarray(0, 2 * m.m)));
    this.graph.setPointClusters(Array.from(m.group.subarray(0, m.n)));
    this.graph.setClusterPositions(Array.from(grownCentres));
    this.restyle(touched, relit, false);
    this.graph.render(added.length ? 0.15 : undefined, 0);
    if (m.crates.length * 2 > this.crateCentres.length) this.buildLabels();
  }

  // ------------------------------------------------------------------ live
  flash(nodes) {
    const t = performance.now();
    for (const i of nodes) this.paint.flashes.set(i, t);
    this.animate();
  }

  animate() {
    if (this.animFrame) return;
    const tick = () => {
      this.animFrame = 0;
      this.paint.tick(performance.now());
      this.upload(true);
      if (this.paint.flashes.size) this.animFrame = requestAnimationFrame(tick);
    };
    this.animFrame = requestAnimationFrame(tick);
  }

  get animating() {
    return this.paint.flashes.size > 0 || this.spreadTimers.length > 0;
  }

  // An edit's spread: level by level from the edited functions out along the calls, each
  // level flashing and ringed, the calls between levels lit while it goes. The rings stay until
  // clearSpread (the next build).
  spread(levels, follow) {
    this.clearSpread();
    const all = levels.flatMap((l) => l.nodes);
    if (all.length === 0) return;
    const fitMs = follow ? 700 * this.slow : 0;
    if (follow) {
      // With what it calls and what calls it in view, so the spread is seen where it sits.
      const m = this.model;
      const around = new Set(all);
      for (const i of all) {
        for (const e of m.out[i]) around.add(m.edges[2 * e + 1]);
        for (const e of m.inn[i]) around.add(m.edges[2 * e]);
      }
      this.graph.fitViewByPointIndices(Array.from(around), fitMs, 0.1, false);
    }
    levels.forEach((level, d) => {
      this.spreadTimers.push(setTimeout(() => {
        for (const i of level.nodes) this.outlined.add(i);
        for (const e of level.links) this.spreadLinks.add(e);
        this.flash(level.nodes);
        this.applyHighlight();
      }, fitMs + d * SPREAD_STEP_MS * this.slow));
    });
    this.spreadTimers.push(setTimeout(() => {
      this.spreadTimers = [];
      this.spreadLinks.clear();
      this.applyHighlight();
    }, fitMs + levels.length * SPREAD_STEP_MS * this.slow + FLASH_MS * this.slow + 2500));
  }

  clearSpread() {
    for (const t of this.spreadTimers) clearTimeout(t);
    this.spreadTimers = [];
    this.outlined.clear();
    this.spreadLinks.clear();
    this.applyHighlight();
  }

  // ------------------------------------------------------------------ selection
  // `nodes` highlighted (the rest greyed), `links` lit, `focus` ringed; null clears it.
  highlight(sel) {
    this.selection = sel;
    this.applyHighlight();
  }

  // A highlight while something elsewhere is hovered; null goes back to the selection's.
  preview(sel) {
    this.previewSel = sel;
    this.applyHighlight();
  }

  // Without a selection, a spread under way greys out all but what it has reached; once it is
  // over, its rings give way to a selection.
  applyHighlight() {
    const sel = this.previewSel ?? this.selection;
    const links = new Set(this.spreadLinks);
    if (sel?.links) for (const e of sel.links) links.add(e);
    const spreading = this.spreadTimers.length > 0 && this.outlined.size > 0;
    this.graph.setConfigPartial({
      highlightedPointIndices: sel?.nodes ? Array.from(sel.nodes) : spreading ? Array.from(this.outlined) : undefined,
      highlightedLinkIndices: links.size ? Array.from(links) : undefined,
      focusedPointIndex: sel?.focus ?? undefined,
      outlinedPointIndices: this.outlined.size && (spreading || !sel) ? Array.from(this.outlined) : undefined,
    });
  }

  // Centres on `i` with the nodes `around` it in view (its callers and callees), or zooms to it alone.
  centreOn(i, around) {
    if (around && around.length > 1) this.graph.fitViewByPointIndices(around.slice(0, 400), 600, 0.25, false);
    else this.graph.zoomToPointByIndex(i, 600, 3, true, false);
  }

  fit(nodes) {
    if (nodes?.length) this.graph.fitViewByPointIndices(nodes, 600, 0.2, false);
    else this.graph.fitView(600, 0.06, false);
  }

  setLayoutRunning(on) {
    if (on) this.graph.start(0.3);
    else this.graph.pause();
  }

  get layoutRunning() {
    return this.graph.isSimulationRunning;
  }

  // ------------------------------------------------------------------ crate labels
  buildLabels() {
    this.labelLayer.replaceChildren();
    const m = this.model;
    const counts = new Float64Array(m.crates.length);
    for (let i = 0; i < m.n; i++) counts[m.krate[i]]++;
    this.labels = m.crates.map((name, c) => {
      const el = document.createElement("div");
      el.className = "crate-label";
      el.textContent = name;
      el.style.color = `rgb(${this.paint.paletteOf(c).map((x) => Math.round(x * 255)).join(",")})`;
      this.labelLayer.appendChild(el);
      return { el, c, count: counts[c] };
    });
    this.labelCounts = counts;
    this.updateCrateCentres();
  }

  maybeUpdateCentres() {
    const t = performance.now();
    if (t - this.lastCentres > 1500) {
      this.lastCentres = t;
      this.updateCrateCentres(true);
    }
  }

  async updateCrateCentres(async = false) {
    const m = this.model;
    if (async && this.reading) return;
    this.reading = async;
    const pos = async ? await this.graph.getPointPositionsAsync() : this.graph.getPointPositionsArray();
    this.reading = false;
    if (pos.length < 2 * m.n) return;
    const sx = new Float64Array(m.crates.length), sy = new Float64Array(m.crates.length), k = new Float64Array(m.crates.length);
    for (let i = 0; i < m.n; i++) {
      const c = m.krate[i];
      sx[c] += pos[2 * i];
      sy[c] += pos[2 * i + 1];
      k[c]++;
    }
    this.crateCentres = new Float32Array(2 * m.crates.length);
    for (let c = 0; c < m.crates.length; c++) {
      this.crateCentres[2 * c] = k[c] ? sx[c] / k[c] : NaN;
      this.crateCentres[2 * c + 1] = k[c] ? sy[c] / k[c] : NaN;
    }
    this.placeLabels();
  }

  // Bigger crates' labels always, smaller ones once zoomed in far enough to have room.
  placeLabels() {
    if (!this.crateCentres) return;
    const zoom = this.graph.getZoomLevel();
    const total = this.model.n;
    const w = this.container.clientWidth, h = this.container.clientHeight;
    for (const l of this.labels) {
      const x = this.crateCentres[2 * l.c], y = this.crateCentres[2 * l.c + 1];
      const big = l.count / total;
      if (!Number.isFinite(x) || big * zoom * 40 < 0.15) {
        l.el.hidden = true;
        continue;
      }
      const [sx, sy] = this.graph.spaceToScreenPosition([x, y]);
      l.el.hidden = sx < -100 || sy < -20 || sx > w + 100 || sy > h + 20;
      l.el.style.transform = `translate(${sx.toFixed(1)}px, ${sy.toFixed(1)}px) translate(-50%, -50%)`;
      l.el.style.fontSize = `${Math.round(11 + Math.min(9, Math.sqrt(big) * 30))}px`;
    }
  }
}
