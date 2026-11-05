// Functions by key (a u64, as its high and low 32 bits): open addressing over typed arrays, so
// finding one makes no BigInt and the table is three arrays, not a Map entry per function.

const EMPTY = -1;

function mix(hi, lo) {
  return Math.imul(lo ^ Math.imul(hi, 0x9e3779b1), 0x85ebca6b) ^ (hi >>> 15);
}

export class KeyIndex {
  constructor(expected = 0) {
    let size = 16;
    while (size < 2 * expected) size *= 2;
    this.alloc(size);
    this.size = 0;
  }

  alloc(size) {
    this.mask = size - 1;
    this.hi = new Uint32Array(size);
    this.lo = new Uint32Array(size);
    this.node = new Int32Array(size).fill(EMPTY);
  }

  slot(hi, lo) {
    let s = mix(hi, lo) & this.mask;
    while (this.node[s] !== EMPTY && (this.hi[s] !== hi || this.lo[s] !== lo)) s = (s + 1) & this.mask;
    return s;
  }

  // The node of the key, or -1.
  get(hi, lo) {
    return this.node[this.slot(hi, lo)];
  }

  set(hi, lo, node) {
    if (2 * (this.size + 1) > this.mask + 1) this.grow();
    const s = this.slot(hi, lo);
    if (this.node[s] === EMPTY) this.size++;
    this.hi[s] = hi;
    this.lo[s] = lo;
    this.node[s] = node;
  }

  grow() {
    const { hi, lo, node } = this;
    this.alloc(2 * (this.mask + 1));
    for (let s = 0; s < node.length; s++) {
      if (node[s] === EMPTY) continue;
      const t = this.slot(hi[s], lo[s]);
      this.hi[t] = hi[s];
      this.lo[t] = lo[s];
      this.node[t] = node[s];
    }
  }
}

// A u64 given as a number (a safe integer) or a BigInt, as its two halves.
export function halves(key) {
  if (typeof key === "bigint") return [Number(key >> 32n) >>> 0, Number(key & 0xffffffffn) >>> 0];
  return [Math.floor(key / 2 ** 32) >>> 0, key >>> 0];
}
