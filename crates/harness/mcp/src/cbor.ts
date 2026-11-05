// Deterministic CBOR, the only encoding harness speaks (crates/harness/harness-proto/src/cbor.rs is the reference):
// the same value gives the same bytes here and in Rust. Map keys sorted by their encoded bytes (text keys: shorter
// first, then bytewise UTF-8); shortest ints and lengths; definite lengths only; an integral number in [-2^63, 2^64)
// is an integer (a bigint always is), any other number the shortest of f16, f32, f64 that holds it exactly; -0 is 0.
// null, booleans, strings (text), Uint8Array (bytes), arrays, and plain objects (maps; a key whose value is undefined
// is left out, as JSON.stringify does). Anything else (a function, a Date, a class instance) is refused by path.

const TWO_63 = 2 ** 63;
const TWO_64 = 2 ** 64;
const utf8 = new TextEncoder();

class Out {
  buf = new Uint8Array(256);
  len = 0;
  room(n: number) {
    if (this.len + n <= this.buf.length) return;
    const next = new Uint8Array(Math.max(this.buf.length * 2, this.len + n));
    next.set(this.buf.subarray(0, this.len));
    this.buf = next;
  }
  byte(b: number) { this.room(1); this.buf[this.len++] = b; }
  bytes(b: Uint8Array) { this.room(b.length); this.buf.set(b, this.len); this.len += b.length; }
  view(n: number) { this.room(n); const v = new DataView(this.buf.buffer, this.len, n); this.len += n; return v; }
}

/** A head: major type and argument, the argument in its shortest form. */
function head(o: Out, major: number, n: number | bigint) {
  const m = major << 5;
  if (typeof n === "bigint" && n <= BigInt(Number.MAX_SAFE_INTEGER)) n = Number(n);
  if (typeof n === "bigint") { o.byte(m | 27); o.view(8).setBigUint64(0, n); return; }
  if (n < 24) o.byte(m | n);
  else if (n < 0x100) { o.byte(m | 24); o.byte(n); }
  else if (n < 0x10000) { o.byte(m | 25); o.view(2).setUint16(0, n); }
  else if (n < 0x100000000) { o.byte(m | 26); o.view(4).setUint32(0, n); }
  else { o.byte(m | 27); o.view(8).setBigUint64(0, BigInt(n)); }
}

function int(o: Out, n: number | bigint) {
  if (typeof n === "bigint" ? n >= 0n : n >= 0) head(o, 0, n);
  else head(o, 1, typeof n === "bigint" ? -1n - n : -1 - n);
}

/** `x` as a half's bits if a half holds it exactly, else null (what ciborium checks by converting there and back). */
function half(x: number): number | null {
  if (Number.isNaN(x)) return 0x7e00;
  const sign = x < 0 || Object.is(x, -0) ? 0x8000 : 0;
  const a = Math.abs(x);
  if (a === Infinity) return sign | 0x7c00;
  if (a < 2 ** -14) { // subnormal: m * 2^-24
    const m = a * 2 ** 24; // exact: a power-of-two scale
    return Number.isInteger(m) ? sign | m : null;
  }
  if (a > 65504) return null;
  const v = new DataView(new ArrayBuffer(8));
  v.setFloat64(0, a);
  const hi = v.getUint32(0), lo = v.getUint32(4);
  const e = ((hi >>> 20) & 0x7ff) - 1023;
  const mant = hi & 0xfffff; // top 20 of the 52 mantissa bits
  if (lo !== 0 || (mant & 0x3ff) !== 0) return null; // a half keeps only the top 10
  return sign | ((e + 15) << 10) | (mant >>> 10);
}

function float(o: Out, x: number) {
  const h = half(x);
  if (h !== null) { o.byte(0xf9); o.view(2).setUint16(0, h); }
  else if (Math.fround(x) === x) { o.byte(0xfa); o.view(4).setFloat32(0, x); }
  else { o.byte(0xfb); o.view(8).setFloat64(0, x); }
}

const plainObject = (v: object) => { const p = Object.getPrototypeOf(v); return p === Object.prototype || p === null; };

function item(o: Out, v: unknown, path: string) {
  switch (typeof v) {
    case "boolean": o.byte(v ? 0xf5 : 0xf4); return;
    case "bigint":
      if (v < -(1n << 63n) || v >= 1n << 64n) throw new Error(`cbor: ${path} = ${v} is past a 64-bit integer`);
      int(o, v); return;
    case "number":
      if (Number.isInteger(v) && v >= -TWO_63 && v < TWO_64) int(o, Number.isSafeInteger(v) ? (v === 0 ? 0 : v) : BigInt(v));
      else float(o, v);
      return;
    case "string": { const b = utf8.encode(v); head(o, 3, b.length); o.bytes(b); return; }
    case "object":
      if (v === null) { o.byte(0xf6); return; }
      if (v instanceof Uint8Array) { head(o, 2, v.length); o.bytes(v); return; }
      if (Array.isArray(v)) {
        head(o, 4, v.length);
        v.forEach((x, i) => item(o, x, `${path}[${i}]`));
        return;
      }
      if (plainObject(v)) {
        const entries: [Uint8Array, Uint8Array][] = [];
        for (const [k, x] of Object.entries(v)) {
          if (x === undefined) continue;
          const ko = new Out(); item(ko, k, path);
          const vo = new Out(); item(vo, x, `${path}.${k}`);
          entries.push([ko.buf.subarray(0, ko.len), vo.buf.subarray(0, vo.len)]);
        }
        entries.sort(([a], [b]) => cmp(a, b));
        head(o, 5, entries.length);
        for (const [k, x] of entries) { o.bytes(k); o.bytes(x); }
        return;
      }
      throw new Error(`cbor: ${path} is a ${v.constructor?.name ?? "object"}, not a plain object, array, Uint8Array or scalar`);
    default:
      throw new Error(`cbor: ${path} is a ${typeof v}, which CBOR cannot hold`);
  }
}

/** Encoded keys' order: shorter first, then bytewise. */
function cmp(a: Uint8Array, b: Uint8Array): number {
  if (a.length !== b.length) return a.length - b.length;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return a[i]! - b[i]!;
  return 0;
}

/** `v` as deterministic CBOR. */
export function encode(v: unknown): Uint8Array {
  const o = new Out();
  item(o, v, "value");
  return o.buf.slice(0, o.len);
}

const text = new TextDecoder("utf-8", { fatal: true });

function f16(h: number): number {
  const s = h & 0x8000 ? -1 : 1, e = (h >> 10) & 0x1f, m = h & 0x3ff;
  if (e === 0) return s * m * 2 ** -24;
  if (e === 31) return m ? NaN : s * Infinity;
  return s * (1 + m / 1024) * 2 ** (e - 15);
}

/** One whole item of `b`, refusing trailing bytes, indefinite lengths and tags. Integers past 2^53 are bigints. */
export function decode(b: Uint8Array): unknown {
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  let at = 0;
  const need = (n: number) => { if (at + n > b.length) throw new Error(`cbor: ends at byte ${b.length}, inside an item at ${at}`); };
  const arg = (info: number): number | bigint => {
    if (info < 24) return info;
    switch (info) {
      case 24: need(1); return b[at++]!;
      case 25: need(2); at += 2; return v.getUint16(at - 2);
      case 26: need(4); at += 4; return v.getUint32(at - 4);
      case 27: { need(8); at += 8; const n = v.getBigUint64(at - 8); return n <= BigInt(Number.MAX_SAFE_INTEGER) ? Number(n) : n; }
      case 31: throw new Error(`cbor: an indefinite length at byte ${at - 1} (harness writes definite lengths only)`);
      default: throw new Error(`cbor: reserved additional info ${info} at byte ${at - 1}`);
    }
  };
  const len = (info: number) => {
    const n = arg(info);
    if (typeof n === "bigint" || n > b.length - at) throw new Error(`cbor: a length of ${n} at byte ${at} is past the input's end`);
    return n;
  };
  const one = (): unknown => {
    need(1);
    const ib = b[at++]!, major = ib >> 5, info = ib & 0x1f;
    switch (major) {
      case 0: return arg(info);
      case 1: {
        const n = arg(info);
        if (typeof n === "number" && n < Number.MAX_SAFE_INTEGER) return -1 - n;
        const neg = -1n - BigInt(n);
        return neg >= BigInt(Number.MIN_SAFE_INTEGER) ? Number(neg) : neg;
      }
      case 2: { const n = len(info); at += n; return b.slice(at - n, at); }
      case 3: { const n = len(info); at += n; return text.decode(b.subarray(at - n, at)); }
      case 4: { const n = len(info); const a: unknown[] = []; for (let i = 0; i < n; i++) a.push(one()); return a; }
      case 5: {
        const n = len(info);
        const entries: [string, unknown][] = [];
        for (let i = 0; i < n; i++) {
          const k = one();
          if (typeof k !== "string") throw new Error(`cbor: a map key at byte ${at} is a ${typeof k}, not text`);
          entries.push([k, one()]);
        }
        return Object.fromEntries(entries); // own data properties, so a "__proto__" key stays a key
      }
      case 6: throw new Error(`cbor: a tag at byte ${at - 1} (harness writes none)`);
      default:
        switch (info) {
          case 20: return false;
          case 21: return true;
          case 22: return null;
          case 25: need(2); at += 2; return f16(v.getUint16(at - 2));
          case 26: need(4); at += 4; return v.getFloat32(at - 4);
          case 27: need(8); at += 8; return v.getFloat64(at - 8);
          default: throw new Error(`cbor: simple value ${info} at byte ${at - 1} is not in harness's data model`);
        }
    }
  };
  const out = one();
  if (at !== b.length) throw new Error(`cbor: ${b.length - at} trailing bytes after one item`);
  return out;
}
