// The CBOR a Record is (RFC 8949): unsigned and negative integers, text, arrays, maps, false,
// true, null. Anything else (bytes, tags, floats, indefinite lengths, undefined) is refused,
// since a Record never has it.

export class CborError extends Error {
  constructor(offset, what) {
    super(`CBOR at byte ${offset}: ${what}`);
    this.offset = offset;
  }
}

const MAX_DEPTH = 64;
const utf8 = new TextDecoder("utf-8", { fatal: true });

// Integers come out as numbers when they are safe ones and as BigInt otherwise (a u64 key).
export function decode(bytes) {
  const items = decodeSequence(bytes);
  if (items.length !== 1) throw new CborError(0, `${items.length} items where one was expected`);
  return items[0];
}

export function decodeSequence(bytes) {
  const r = new Reader(bytes);
  const out = [];
  while (r.pos < bytes.length) out.push(r.item(0));
  return out;
}

class Reader {
  constructor(bytes) {
    this.b = bytes;
    this.v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    this.pos = 0;
  }

  need(n, what) {
    if (this.pos + n > this.b.length) throw new CborError(this.pos, `${what} runs past the end (${this.b.length} bytes)`);
  }

  // The argument of a head: its count, length or value.
  arg(info, at) {
    if (info < 24) return info;
    switch (info) {
      case 24: this.need(1, "a 1-byte argument"); return this.b[this.pos++];
      case 25: { this.need(2, "a 2-byte argument"); const x = this.v.getUint16(this.pos); this.pos += 2; return x; }
      case 26: { this.need(4, "a 4-byte argument"); const x = this.v.getUint32(this.pos); this.pos += 4; return x; }
      case 27: {
        this.need(8, "an 8-byte argument");
        const x = this.v.getBigUint64(this.pos);
        this.pos += 8;
        return x <= BigInt(Number.MAX_SAFE_INTEGER) ? Number(x) : x;
      }
      default: throw new CborError(at, info === 31 ? "an indefinite length" : `reserved additional info ${info}`);
    }
  }

  length(info, at, what) {
    const n = this.arg(info, at);
    if (typeof n === "bigint" || n > this.b.length - this.pos) throw new CborError(at, `${what} of ${n} is longer than the message`);
    return n;
  }

  item(depth) {
    if (depth > MAX_DEPTH) throw new CborError(this.pos, `nested deeper than ${MAX_DEPTH}`);
    const at = this.pos;
    this.need(1, "an item");
    const head = this.b[this.pos++];
    const major = head >> 5;
    const info = head & 31;
    switch (major) {
      case 0: return this.arg(info, at);
      case 1: {
        const n = this.arg(info, at);
        return typeof n === "bigint" || n >= Number.MAX_SAFE_INTEGER ? -1n - BigInt(n) : -1 - n;
      }
      case 2: throw new CborError(at, "a byte string, which no Record has");
      case 3: {
        const n = this.length(info, at, "a text string");
        const s = this.b.subarray(this.pos, this.pos + n);
        this.pos += n;
        try {
          return utf8.decode(s);
        } catch {
          throw new CborError(at, "a text string that is not UTF-8");
        }
      }
      case 4: {
        const n = this.length(info, at, "an array");
        const a = new Array(n);
        for (let i = 0; i < n; i++) a[i] = this.item(depth + 1);
        return a;
      }
      case 5: {
        const n = this.length(info, at, "a map");
        const m = Object.create(null);
        for (let i = 0; i < n; i++) {
          const kat = this.pos;
          const k = this.item(depth + 1);
          if (typeof k !== "string") throw new CborError(kat, "a map key that is not text");
          if (k in m) throw new CborError(kat, `the map key ${JSON.stringify(k)} twice`);
          m[k] = this.item(depth + 1);
        }
        return m;
      }
      case 6: throw new CborError(at, "a tag, which no Record has");
      case 7:
        if (info === 20) return false;
        if (info === 21) return true;
        if (info === 22) return null;
        throw new CborError(at, info >= 25 && info <= 27 ? "a float, which no Record has" : `simple value ${info}`);
    }
    throw new CborError(at, `major type ${major}`);
  }
}
