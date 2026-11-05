// Each point's colour and size as the main view draws them: colour the crate's (dim when the
// build shown did not compile it), size the compile time, a flash over both. Kept from frame to
// frame, so a frame restyles only the points that changed.
import { crateColor } from "./palette.js";
import { HOW_COMPILED, HOW_UNIT } from "./snapshot.js";

const DIM = 0.32;
const GLOW = [1, 0.86, 0.45];
export const FLASH_MS = 1400;

export function nodeSize(ns) {
  return ns > 0 ? 2 + 1.6 * Math.log10(1 + ns / 20000) : 1.6;
}

export class Paint {
  // `slow` stretches a flash, as MainView's.
  constructor(model, slow = 1) {
    this.model = model;
    this.slow = slow;
    this.flashes = new Map();
    // Room for more than `n` points; `points()` is the view of them that is drawn.
    this.colors = new Float32Array(0);
    this.sizes = new Float32Array(0);
    this.palette = [];
    // The points drawn lit, each once: what a new build shown may dim.
    this.lit = [];
    this.litMark = new Uint8Array(0);
  }

  paletteOf(c) {
    const crates = this.model.crates;
    while (this.palette.length <= c) this.palette.push(crateColor(crates[this.palette.length]));
    return this.palette[c];
  }

  baseColor(i) {
    const m = this.model;
    const c = this.paletteOf(m.krate[i]);
    const lit = (m.how[i] === HOW_COMPILED || m.how[i] === HOW_UNIT) && m.nodeBuild[i] === m.current;
    const k = lit ? 1 : DIM;
    const at = 4 * i;
    this.colors[at] = c[0] * k;
    this.colors[at + 1] = c[1] * k;
    this.colors[at + 2] = c[2] * k;
    this.colors[at + 3] = lit ? 1 : 0.7;
    if (lit && !this.litMark[i]) {
      this.litMark[i] = 1;
      this.lit.push(i);
    }
  }

  ensure(n) {
    if (this.sizes.length >= n) return;
    const room = Math.max(n, 2 * this.sizes.length);
    const colors = new Float32Array(4 * room), sizes = new Float32Array(room), litMark = new Uint8Array(room);
    colors.set(this.colors);
    sizes.set(this.sizes);
    litMark.set(this.litMark);
    [this.colors, this.sizes, this.litMark] = [colors, sizes, litMark];
  }

  style(i) {
    this.baseColor(i);
    this.sizes[i] = nodeSize(this.model.ns[i]);
  }

  // Returns whether the flash is still going at `t`.
  paintFlash(i, t0, t) {
    const f = Math.max(0, 1 - (t - t0) / (FLASH_MS * this.slow));
    const e = f * f * (3 - 2 * f);
    const at = 4 * i;
    this.baseColor(i);
    for (let k = 0; k < 3; k++) this.colors[at + k] += (GLOW[k] - this.colors[at + k]) * e;
    this.colors[at + 3] = 1;
    this.sizes[i] = nodeSize(this.model.ns[i]) * (1 + 1.8 * e);
    return f > 0;
  }

  // Every point, at `now`.
  all(now) {
    const n = this.model.n;
    this.ensure(n);
    this.lit = [];
    this.litMark.fill(0);
    for (let i = 0; i < n; i++) this.style(i);
    for (const [i, t0] of this.flashes) this.paintFlash(i, t0, now);
  }

  // What `all` would leave, given `points` are every point whose function changed since, and
  // `relit` whether the build shown did: a point only turns lit when its function changes, and
  // only turns dim when that or the build shown does.
  some(points, relit, now) {
    this.ensure(this.model.n);
    if (relit) {
      const was = this.lit;
      this.lit = [];
      for (const i of was) this.litMark[i] = 0;
      for (const i of was) this.style(i);
    }
    for (const i of points) this.style(i);
    for (const [i, t0] of this.flashes) this.paintFlash(i, t0, now);
  }

  // The flashes at `t`, those over dropped.
  tick(t) {
    for (const [i, t0] of this.flashes) if (!this.paintFlash(i, t0, t)) this.flashes.delete(i);
  }

  points() {
    const n = this.model.n;
    return { colors: this.colors.subarray(0, 4 * n), sizes: this.sizes.subarray(0, n) };
  }
}
