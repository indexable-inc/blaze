// Compile time as a squarified treemap, crate > module > function. Tiles too small to see are
// one "n more" tile per parent, so 80k functions draw as a few thousand rectangles.
import { crateColor, css } from "./palette.js";
import { el, fitCanvas, fmtNs, fmtCount } from "./ui.js";
import { shortName } from "./names.js";

const MIN_AREA = 40;
const MAX_CHILDREN = 800;
const HEADER = 15;

export class Treemap {
  constructor(root, model, handlers, tooltip) {
    this.root = root;
    this.handlers = handlers;
    this.tooltip = tooltip;
    this.crumbs = el("div", "crumbs");
    this.note = el("div", "note");
    this.canvas = el("canvas", "treemap");
    root.replaceChildren(this.crumbs, this.note, this.canvas);
    this.canvas.addEventListener("mousemove", (e) => this.hover(e));
    this.canvas.addEventListener("mouseleave", () => { this.tooltip.hide(); this.handlers.onHoverNodes?.(null); });
    this.canvas.addEventListener("click", (e) => this.click(e));
    new ResizeObserver(() => this.visible && this.draw()).observe(root);
    this.setModel(model);
  }

  setModel(model) {
    this.model = model;
    this.path = [];
    this.invalidate();
  }

  invalidate() {
    this.index = null;
    if (this.visible) this.draw();
  }

  show() {
    this.visible = true;
    this.draw();
  }

  hide() {
    this.visible = false;
  }

  build() {
    const m = this.model;
    const fnsOf = Array.from({ length: m.modules.length }, () => []);
    const modsOf = Array.from({ length: m.crates.length }, () => []);
    const modNs = new Float64Array(m.modules.length);
    let untimed = 0;
    for (let i = 0; i < m.n; i++) {
      if (m.ns[i] > 0) {
        fnsOf[m.group[i]].push(i);
        modNs[m.group[i]] += m.ns[i];
      } else untimed++;
    }
    const crateNs = new Float64Array(m.crates.length);
    for (let g = 0; g < m.modules.length; g++) {
      if (modNs[g] > 0) {
        modsOf[m.moduleCrate[g]].push(g);
        crateNs[m.moduleCrate[g]] += modNs[g];
      }
    }
    const fnItem = (i) => ({ kind: "fn", node: i, crate: m.krate[i], value: m.ns[i], label: shortName(m.names[i]), full: m.names[i] });
    const modItem = (g) => ({ kind: "module", group: g, crate: m.moduleCrate[g], value: modNs[g], label: m.modules[g], children: () => fnsOf[g].map(fnItem) });
    const crateItem = (c) => ({ kind: "crate", crate: c, value: crateNs[c], label: m.crates[c], children: () => modsOf[c].map(modItem) });
    this.index = {
      untimed,
      top: { kind: "all", label: "all", value: crateNs.reduce((a, b) => a + b, 0), children: () => m.crates.map((_, c) => c).filter((c) => crateNs[c] > 0).map(crateItem) },
      crateItem, modItem,
    };
  }

  current() {
    let it = this.index.top;
    for (const p of this.path) it = p.kind === "crate" ? this.index.crateItem(p.crate) : this.index.modItem(p.group);
    return it;
  }

  draw() {
    if (!this.index) this.build();
    const box = this.root.getBoundingClientRect();
    const w = Math.max(100, box.width - 2);
    const h = Math.max(100, box.height - this.crumbs.offsetHeight - this.note.offsetHeight - 6);
    const ctx = fitCanvas(this.canvas, w, h);
    ctx.fillStyle = "#0b0f14";
    ctx.fillRect(0, 0, w, h);
    ctx.font = "11px ui-sans-serif, system-ui, sans-serif";
    ctx.textBaseline = "top";
    this.hits = [];
    const cur = this.current();
    this.layoutLevel(ctx, cur, 0, 0, w, h, 0);

    const m = this.model;
    const all = [{ label: "all", path: [] }];
    this.path.forEach((p, k) => all.push({ label: p.kind === "crate" ? m.crates[p.crate] : m.modules[p.group], path: this.path.slice(0, k + 1) }));
    this.crumbs.replaceChildren(...all.flatMap((c, k) => {
      const b = el("button", k === all.length - 1 ? "crumb current" : "crumb", c.label);
      b.addEventListener("click", () => { this.path = c.path; this.draw(); });
      return k ? [el("span", "crumb-sep", "›"), b] : [b];
    }));
    this.note.textContent = `${fmtNs(cur.value)} of compile time. ${fmtCount(this.index.untimed)} functions with no time of their own (compiled in an LLVM unit, unchanged or from the store) are not drawn. Click to zoom in.`;
  }

  layoutLevel(ctx, parent, x, y, w, h, depth) {
    const kids = parent.children().sort((a, b) => b.value - a.value);
    if (!kids.length || parent.value <= 0) return;
    const area = w * h;
    const shown = [];
    let rest = 0, restCount = 0;
    for (const k of kids) {
      if (shown.length < MAX_CHILDREN && (k.value / parent.value) * area >= MIN_AREA) shown.push(k);
      else { rest += k.value; restCount++; }
    }
    if (restCount) shown.push({ kind: "rest", crate: parent.crate ?? kids[0].crate, value: rest, label: `${restCount} more`, count: restCount, parent });
    const rects = squarify(shown.map((k) => k.value), x, y, w, h);
    shown.forEach((k, i) => {
      const [rx, ry, rw, rh] = rects[i];
      if (rw < 1 || rh < 1) return;
      const col = crateColor(this.model.crates[k.crate]);
      const leaf = !k.children || k.kind === "rest";
      ctx.fillStyle = css(col, k.kind === "rest" ? 0.12 : leaf ? 0.55 : 0.16 + depth * 0.08);
      ctx.fillRect(rx + 0.5, ry + 0.5, rw - 1, rh - 1);
      ctx.strokeStyle = css(col, leaf ? 0.9 : 0.55);
      ctx.lineWidth = 1;
      ctx.strokeRect(rx + 0.5, ry + 0.5, rw - 1, rh - 1);
      this.hits.push({ x: rx, y: ry, w: rw, h: rh, item: k });
      const room = rw > 36 && rh > 13;
      if (room) {
        ctx.fillStyle = leaf ? "#0b0f14" : "#e6edf3";
        const label = fit(k.label, rw - 6);
        ctx.fillText(label, rx + 3, ry + 2);
      }
      if (!leaf && depth < 2 && rw > 30 && rh > HEADER + 14) {
        this.layoutLevel(ctx, k, rx + 2, ry + HEADER, rw - 4, rh - HEADER - 2, depth + 1);
      }
    });
  }

  hit(e) {
    const r = this.canvas.getBoundingClientRect();
    const px = e.clientX - r.left, py = e.clientY - r.top;
    for (let i = this.hits.length - 1; i >= 0; i--) {
      const h = this.hits[i];
      if (px >= h.x && px < h.x + h.w && py >= h.y && py < h.y + h.h) return h.item;
    }
    return null;
  }

  hover(e) {
    const it = this.hit(e);
    if (!it) { this.tooltip.hide(); return; }
    const m = this.model;
    const lines = [it.full ?? it.label, `${fmtNs(it.value)}`, it.kind === "fn" ? m.crates[it.crate] : it.kind === "rest" ? "too small to draw one by one" : `${it.kind}`];
    this.tooltip.show(e.clientX, e.clientY, lines);
  }

  click(e) {
    const it = this.hit(e);
    if (!it) return;
    if (it.kind === "fn") this.handlers.onSelect?.(it.node);
    else if (it.kind === "crate") { this.path = [{ kind: "crate", crate: it.crate }]; this.draw(); }
    else if (it.kind === "module") {
      this.path = [{ kind: "crate", crate: it.crate }, { kind: "module", group: it.group }];
      this.draw();
    } else if (it.kind === "rest" && it.parent.kind !== "all" && it.parent.kind !== "module") {
      const p = it.parent;
      this.path = p.kind === "crate" ? [{ kind: "crate", crate: p.crate }] : this.path;
      this.draw();
    }
  }
}

function fit(s, px) {
  const n = Math.floor(px / 6.2);
  return s.length <= n ? s : n <= 1 ? "" : s.slice(0, n - 1) + "…";
}

// Bruls, Huizing and van Wijk's squarified layout of `values` (largest first) into a rectangle.
function squarify(values, x, y, w, h) {
  const total = values.reduce((a, b) => a + b, 0);
  const scale = (w * h) / total;
  const areas = values.map((v) => v * scale);
  const out = new Array(values.length);
  let i = 0;
  while (i < areas.length) {
    const side = Math.min(w, h);
    let j = i, sum = 0, best = Infinity;
    while (j < areas.length) {
      const s2 = sum + areas[j];
      const lo = areas[j], hi = areas[i];
      const worst = Math.max((side * side * hi) / (s2 * s2), (s2 * s2) / (side * side * lo));
      if (worst > best) break;
      best = worst;
      sum = s2;
      j++;
    }
    const thick = sum / side;
    let off = 0;
    for (let k = i; k < j; k++) {
      const len = areas[k] / thick;
      out[k] = w >= h ? [x, y + off, thick, len] : [x + off, y, len, thick];
      off += len;
    }
    if (w >= h) { x += thick; w -= thick; } else { y += thick; h -= thick; }
    i = j;
  }
  return out;
}
