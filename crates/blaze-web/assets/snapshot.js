// The snapshot, the websocket's first message (docs/web.md, "The snapshot"). It comes from
// the peer, so every length and index is checked against what is there.

export class SnapshotError extends Error {
  constructor(offset, what) {
    super(`snapshot at byte ${offset}: ${what}`);
    this.offset = offset;
  }
}

export const HOW = ["Compiled", "Unit", "Unchanged", "Stored"];
export const HOW_COMPILED = 0;
export const HOW_UNIT = 1;
export const HOW_UNCHANGED = 2;
export const HOW_STORED = 3;

const utf8 = new TextDecoder("utf-8", { fatal: true });
const NODE_FIXED = 8 + 4 + 1 + 8 + 8;

export function isSnapshot(bytes) {
  return bytes.length >= 4 && bytes[0] === 0x42 && bytes[1] === 0x4c && bytes[2] === 0x5a && bytes[3] === 0x57;
}

export function parseSnapshot(bytes) {
  const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  let pos = 0;
  const need = (n, what) => {
    if (pos + n > bytes.length) throw new SnapshotError(pos, `${what} runs past the end (${bytes.length} bytes)`);
  };
  const u32 = (what) => { need(4, what); const x = v.getUint32(pos, true); pos += 4; return x; };
  const u64 = (what) => { need(8, what); const x = v.getBigUint64(pos, true); pos += 8; return x; };
  const str = (what) => {
    const n = u32(`the length of ${what}`);
    need(n, what);
    const at = pos;
    pos += n;
    try {
      return utf8.decode(bytes.subarray(at, at + n));
    } catch {
      throw new SnapshotError(at, `${what} is not UTF-8`);
    }
  };
  // A count is bounded by the bytes left, so a hostile count cannot make us allocate.
  const count = (what, minEach) => {
    const at = pos;
    const n = u32(`the count of ${what}`);
    if (n * minEach > bytes.length - pos) throw new SnapshotError(at, `${n} ${what} cannot fit in the ${bytes.length - pos} bytes left`);
    return n;
  };

  if (!isSnapshot(bytes)) throw new SnapshotError(0, 'no magic "BLZW"');
  pos = 4;
  const version = u32("the version");
  if (version !== 1) throw new SnapshotError(4, `version ${version}; this page reads version 1`);
  const root = str("the workspace root");
  const build = Number(u64("the build number"));

  const nCrates = count("crates", 4);
  const crates = new Array(nCrates);
  for (let i = 0; i < nCrates; i++) crates[i] = str(`crate ${i}'s name`);

  const n = count("nodes", NODE_FIXED + 4);
  const key = new BigUint64Array(n);
  const krate = new Uint32Array(n);
  const how = new Uint8Array(n);
  const ns = new Float64Array(n);
  const nodeBuild = new Float64Array(n);
  const names = new Array(n);
  const byKey = new Map();
  for (let i = 0; i < n; i++) {
    const at = pos;
    need(NODE_FIXED, `node ${i}`);
    key[i] = v.getBigUint64(pos, true);
    krate[i] = v.getUint32(pos + 8, true);
    how[i] = v.getUint8(pos + 12);
    const t = v.getBigUint64(pos + 13, true);
    nodeBuild[i] = Number(v.getBigUint64(pos + 21, true));
    pos += NODE_FIXED;
    names[i] = str(`node ${i}'s name`);
    if (krate[i] >= nCrates) throw new SnapshotError(at + 8, `node ${i} names crate ${krate[i]} of ${nCrates}`);
    if (how[i] > 3) throw new SnapshotError(at + 12, `node ${i} has how ${how[i]}, not 0 to 3`);
    if (how[i] !== HOW_COMPILED && t !== 0n) throw new SnapshotError(at + 13, `node ${i} is ${HOW[how[i]]} but has a compile time`);
    ns[i] = Number(t);
    if (byKey.has(key[i])) throw new SnapshotError(at, `node ${i} has the key of node ${byKey.get(key[i])}`);
    byKey.set(key[i], i);
  }

  const m = count("edges", 8);
  need(m * 8, "the edges");
  const edges = new Uint32Array(m * 2);
  for (let e = 0; e < m; e++) {
    const a = v.getUint32(pos, true);
    const b = v.getUint32(pos + 4, true);
    if (a >= n || b >= n) throw new SnapshotError(pos, `edge ${e} is ${a} -> ${b} of ${n} nodes`);
    edges[2 * e] = a;
    edges[2 * e + 1] = b;
    pos += 8;
  }
  if (pos !== bytes.length) throw new SnapshotError(pos, `${bytes.length - pos} bytes after the edges`);
  return { root, build, crates, n, key, krate, how, ns, nodeBuild, names, edges, byKey };
}
