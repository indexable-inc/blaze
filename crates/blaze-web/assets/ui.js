const SVG_NS = "http://www.w3.org/2000/svg";

export function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

export function svg(tag, attrs = {}, text) {
  const e = document.createElementNS(SVG_NS, tag);
  for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, String(v));
  if (text !== undefined) e.textContent = text;
  return e;
}

export function fmtNs(ns) {
  if (!ns) return "0";
  if (ns < 1e3) return `${Math.round(ns)} ns`;
  if (ns < 1e6) return `${(ns / 1e3).toFixed(ns < 1e4 ? 1 : 0)} µs`;
  if (ns < 1e9) return `${(ns / 1e6).toFixed(ns < 1e7 ? 1 : 0)} ms`;
  return `${(ns / 1e9).toFixed(ns < 1e10 ? 2 : 1)} s`;
}

export function fmtCount(n) {
  return Number(n).toLocaleString("en-US");
}

// Canvas sized to its box in device pixels; returns the 2D context scaled to CSS pixels.
export function fitCanvas(canvas, width, height) {
  const r = window.devicePixelRatio || 1;
  canvas.width = Math.max(1, Math.round(width * r));
  canvas.height = Math.max(1, Math.round(height * r));
  canvas.style.width = `${width}px`;
  canvas.style.height = `${height}px`;
  const ctx = canvas.getContext("2d");
  ctx.setTransform(r, 0, 0, r, 0, 0);
  return ctx;
}

export class Tooltip {
  constructor() {
    this.el = el("div", "tooltip");
    this.el.hidden = true;
    document.body.appendChild(this.el);
  }

  show(x, y, lines) {
    this.el.replaceChildren(...lines.map((l, i) => el("div", i === 0 ? "tt-title" : "tt-line", l)));
    this.el.hidden = false;
    const w = this.el.offsetWidth, h = this.el.offsetHeight;
    const left = Math.min(x + 14, window.innerWidth - w - 8);
    const top = y + 14 + h > window.innerHeight ? y - h - 10 : y + 14;
    this.el.style.transform = `translate(${Math.max(4, left)}px, ${Math.max(4, top)}px)`;
  }

  hide() {
    this.el.hidden = true;
  }
}
