// Deterministic CBOR (RFC 8949 section 4.2.1) for the gateway's bodies: unsigned integers, byte
// and text strings, arrays, maps with text keys, booleans and null. The decoder takes only that
// subset, definite lengths and shortest forms, and nothing after the top item.

export type Cbor = number | Uint8Array | string | Cbor[] | { [key: string]: Cbor } | boolean | null;

export class CborError extends Error {}

export function encode(value: Cbor): Uint8Array {
  const out: number[] = [];
  write(out, value);
  return new Uint8Array(out);
}

function head(out: number[], major: number, n: number) {
  const m = major << 5;
  if (n < 24) out.push(m | n);
  else if (n < 0x100) out.push(m | 24, n);
  else if (n < 0x10000) out.push(m | 25, n >> 8, n & 0xff);
  else if (n < 0x100000000) out.push(m | 26, n >>> 24, (n >>> 16) & 0xff, (n >>> 8) & 0xff, n & 0xff);
  else {
    const hi = Math.floor(n / 0x100000000);
    const lo = n >>> 0;
    out.push(m | 27, hi >>> 24, (hi >>> 16) & 0xff, (hi >>> 8) & 0xff, hi & 0xff, lo >>> 24, (lo >>> 16) & 0xff, (lo >>> 8) & 0xff, lo & 0xff);
  }
}

function write(out: number[], v: Cbor) {
  if (typeof v === "number") {
    if (!Number.isSafeInteger(v) || v < 0) throw new CborError(`cannot encode ${v}: only unsigned integers`);
    head(out, 0, v);
  } else if (v instanceof Uint8Array) {
    head(out, 2, v.length);
    for (const b of v) out.push(b);
  } else if (typeof v === "string") {
    const bytes = new TextEncoder().encode(v);
    head(out, 3, bytes.length);
    for (const b of bytes) out.push(b);
  } else if (Array.isArray(v)) {
    head(out, 4, v.length);
    for (const item of v) write(out, item);
  } else if (v === false) out.push(0xf4);
  else if (v === true) out.push(0xf5);
  else if (v === null) out.push(0xf6);
  else {
    const entries = Object.entries(v).map(([k, val]) => [encode(k), val] as const);
    entries.sort(([a], [b]) => compareBytes(a, b));
    head(out, 5, entries.length);
    for (const [k, val] of entries) {
      for (const b of k) out.push(b);
      write(out, val);
    }
  }
}

function compareBytes(a: Uint8Array, b: Uint8Array): number {
  for (let i = 0; i < Math.min(a.length, b.length); i++) if (a[i] !== b[i]) return a[i] - b[i];
  return a.length - b.length;
}

const MAX_DEPTH = 16;

export function decode(bytes: Uint8Array): Cbor {
  const r = { bytes, pos: 0 };
  const v = read(r, 0);
  if (r.pos !== bytes.length) throw new CborError("bytes after the top item");
  return v;
}

function need(r: { bytes: Uint8Array; pos: number }, n: number) {
  if (r.pos + n > r.bytes.length) throw new CborError("truncated");
}

function readArg(r: { bytes: Uint8Array; pos: number }, info: number): number {
  if (info < 24) return info;
  const size = info === 24 ? 1 : info === 25 ? 2 : info === 26 ? 4 : info === 27 ? 8 : 0;
  if (size === 0) throw new CborError("indefinite lengths and reserved forms are not accepted");
  need(r, size);
  let n = 0;
  for (let i = 0; i < size; i++) n = n * 256 + r.bytes[r.pos + i];
  r.pos += size;
  if (!Number.isSafeInteger(n)) throw new CborError("integer too large");
  const min = size === 1 ? 24 : size === 2 ? 0x100 : size === 4 ? 0x10000 : 0x100000000;
  if (n < min) throw new CborError("integer not in its shortest form");
  return n;
}

function read(r: { bytes: Uint8Array; pos: number }, depth: number): Cbor {
  if (depth > MAX_DEPTH) throw new CborError("nested too deeply");
  need(r, 1);
  const ib = r.bytes[r.pos++];
  const major = ib >> 5;
  const info = ib & 0x1f;
  switch (major) {
    case 0:
      return readArg(r, info);
    case 2: {
      const n = readArg(r, info);
      need(r, n);
      const v = r.bytes.slice(r.pos, r.pos + n);
      r.pos += n;
      return v;
    }
    case 3: {
      const n = readArg(r, info);
      need(r, n);
      const v = new TextDecoder("utf-8", { fatal: true, ignoreBOM: false }).decode(r.bytes.subarray(r.pos, r.pos + n));
      r.pos += n;
      return v;
    }
    case 4: {
      const n = readArg(r, info);
      if (n > r.bytes.length - r.pos) throw new CborError("truncated");
      const v: Cbor[] = [];
      for (let i = 0; i < n; i++) v.push(read(r, depth + 1));
      return v;
    }
    case 5: {
      const n = readArg(r, info);
      if (n > r.bytes.length - r.pos) throw new CborError("truncated");
      const v: { [key: string]: Cbor } = Object.create(null);
      let prev: Uint8Array | null = null;
      for (let i = 0; i < n; i++) {
        const start = r.pos;
        const k = read(r, depth + 1);
        if (typeof k !== "string") throw new CborError("map keys must be text");
        const kb = r.bytes.subarray(start, r.pos);
        if (prev !== null && compareBytes(prev, kb) >= 0) throw new CborError("map keys not in deterministic order");
        prev = kb;
        v[k] = read(r, depth + 1);
      }
      return v;
    }
    case 7:
      if (info === 20) return false;
      if (info === 21) return true;
      if (info === 22) return null;
      throw new CborError("unsupported simple value or float");
    default:
      throw new CborError(`unsupported major type ${major}`);
  }
}
