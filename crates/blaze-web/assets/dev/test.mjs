// node test.mjs [fixture.blzw fixture.edit.cbor]: the page's parsers against good and hostile input.
import { readFileSync } from "node:fs";
import assert from "node:assert/strict";
import { decode, decodeSequence, CborError } from "../cbor.js";
import { parseSnapshot, SnapshotError } from "../snapshot.js";
import { FnColumns, parseFrame, parseRecord, RecordError } from "../records.js";
import { lastName, moduleOf, splitPath } from "../names.js";
import { analyseEdit } from "../edit.js";
import { Model } from "../model.js";
import { KeyIndex, halves } from "../keys.js";
import { Paint } from "../paint.js";
import { crateColor } from "../palette.js";

let passed = 0;
const test = (name, f) => {
  try {
    f();
    passed++;
  } catch (e) {
    console.error(`FAIL ${name}\n${e.stack}`);
    process.exitCode = 1;
  }
};
const bytes = (...xs) => new Uint8Array(xs.flat());
const text = (s) => { const b = new TextEncoder().encode(s); return [0x60 | b.length, ...b]; };

test("cbor: a record", () => {
  const r = decode(bytes(0xa2, text("at_ns"), 0x19, 0x01, 0x00, text("event"), 0xa1, text("Unlisted"), 0xa1, text("count"), 0x05));
  assert.deepEqual({ ...r, event: { Unlisted: { ...r.event.Unlisted } } }, { at_ns: 256, event: { Unlisted: { count: 5 } } });
});
test("cbor: u64 above 2^53 is a BigInt", () => {
  assert.equal(decode(bytes(0x1b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff)), 0xffffffffffffffffn);
  assert.equal(decode(bytes(0x1b, 0, 0x1f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff)), Number.MAX_SAFE_INTEGER);
});
test("cbor: negative ints, bool, null", () => {
  assert.equal(decode(bytes(0x20)), -1);
  assert.equal(decode(bytes(0x38, 0xff)), -256);
  assert.deepEqual(decode(bytes(0x83, 0xf4, 0xf5, 0xf6)), [false, true, null]);
});
for (const [name, b, what] of [
  ["a float", bytes(0xfb, 0, 0, 0, 0, 0, 0, 0, 0), /float/],
  ["a half float", bytes(0xf9, 0, 0), /float/],
  ["a tag", bytes(0xc1, 0x00), /tag/],
  ["bytes", bytes(0x41, 0x00), /byte string/],
  ["indefinite array", bytes(0x9f, 0xff), /indefinite/],
  ["undefined", bytes(0xf7), /simple value 23/],
  ["truncated head", bytes(0x19, 0x01), /runs past the end/],
  ["huge array", bytes(0x9a, 0xff, 0xff, 0xff, 0xff), /longer than the message/],
  ["huge text", bytes(0x7b, 0, 0, 0, 1, 0, 0, 0, 0), /longer than the message/],
  ["non-text key", bytes(0xa1, 0x01, 0x02), /not text/],
  ["duplicate key", bytes(0xa2, text("a"), 1, text("a"), 2), /twice/],
  ["bad utf-8", bytes(0x62, 0xc3, 0x28), /UTF-8/],
  ["deep nesting", new Uint8Array(100).fill(0x81), /deeper/],
]) test(`cbor: refuses ${name}`, () => assert.throws(() => decode(b), (e) => e instanceof CborError && what.test(e.message)));
test("cbor: two items where one is wanted", () => assert.throws(() => decode(bytes(1, 2)), /2 items/));

// What the server sends in one frame: as many records as it had, back to back.
const unlisted = (n) => [0xa2, text("at_ns"), n, text("event"), 0xa1, text("Unlisted"), 0xa1, text("count"), n].flat();
test("frame: every record of it, in order", () => {
  const rs = parseFrame(bytes(unlisted(1), unlisted(2), unlisted(3)));
  assert.deepEqual(rs.map((r) => r.event.count), [1, 2, 3]);
  assert.deepEqual(parseFrame(bytes(unlisted(7))).map((r) => r.at_ns), [7]);
});
test("frame: a record of it that is not one is named by its place", () => {
  const bad = [0xa2, text("at_ns"), 1, text("event"), 0xa1, text("Bogus"), 0xa0].flat();
  assert.throws(() => parseFrame(bytes(unlisted(1), bad)), (e) => e instanceof RecordError && /item 1: .*unknown variant "Bogus"/.test(e.message));
});
test("frame: one cut short is refused", () => {
  assert.throws(() => parseFrame(bytes(unlisted(1), unlisted(2)).subarray(0, 30)), CborError);
});

// A snapshot of two crates, three nodes, two edges.
function snap({ version = 1, krate1 = 1, how2 = 3, ns2 = 0n, edgeTo = 2, trailing = 0, dupKey = false } = {}) {
  const parts = [];
  const u32 = (x) => { const b = new Uint8Array(4); new DataView(b.buffer).setUint32(0, x, true); parts.push(b); };
  const u64 = (x) => { const b = new Uint8Array(8); new DataView(b.buffer).setBigUint64(0, BigInt(x), true); parts.push(b); };
  const str = (s) => { const b = new TextEncoder().encode(s); u32(b.length); parts.push(b); };
  parts.push(new TextEncoder().encode("BLZW"));
  u32(version); str("/w"); u64(9);
  u32(2); str("anim"); str("core");
  u32(3);
  const node = (key, k, how, ns, name) => { u64(key); u32(k); parts.push(new Uint8Array([how])); u64(ns); u64(9); str(name); };
  node(10, 0, 0, 1234, "anim::main");
  node(dupKey ? 10 : 11, krate1, 0, 99, "core::fmt::write");
  node(12, 1, how2, ns2, "<core::fmt::Arguments as core::fmt::Display>::fmt");
  u32(2); u32(0); u32(1); u32(1); u32(edgeTo);
  parts.push(new Uint8Array(trailing));
  const out = new Uint8Array(parts.reduce((s, p) => s + p.length, 0));
  let o = 0;
  for (const p of parts) { out.set(p, o); o += p.length; }
  return out;
}
test("snapshot: parses", () => {
  const s = parseSnapshot(snap());
  assert.equal(s.root, "/w");
  assert.equal(s.build, 9);
  assert.deepEqual(s.crates, ["anim", "core"]);
  assert.equal(s.n, 3);
  assert.deepEqual([...s.edges], [0, 1, 1, 2]);
  assert.equal(s.ns[0], 1234);
});
for (const [name, b, what] of [
  ["no magic", new Uint8Array([1, 2, 3, 4]), /magic/],
  ["version 2", snap({ version: 2 }), /version 2/],
  ["crate out of range", snap({ krate1: 5 }), /crate 5 of 2/],
  ["how 4", snap({ how2: 4 }), /how 4/],
  ["time on a stored node", snap({ ns2: 5n }), /Stored but has a compile time/],
  ["edge out of range", snap({ edgeTo: 3 }), /1 -> 3 of 3/],
  ["trailing bytes", snap({ trailing: 3 }), /3 bytes after/],
  ["a key twice", snap({ dupKey: true }), /key of node 0/],
  ["truncated", snap().subarray(0, 60), /runs past the end|cannot fit/],
]) test(`snapshot: refuses ${name}`, () => assert.throws(() => parseSnapshot(b), (e) => e instanceof SnapshotError && what.test(e.message)));
test("snapshot: a huge count is refused before allocating", () => {
  const b = snap().slice(0, 4 + 4 + 4 + 2 + 8 + 4);
  const v = new DataView(b.buffer);
  v.setUint32(b.length - 4, 0xffffffff, true);
  assert.throws(() => parseSnapshot(b), /cannot fit/);
});

// Ids 2 and 3: "std" waiting for 0, said before, and "ab" waiting for 2.
const NODES = { first: 2, kinds: ["Std", "Names"], names: "stdab", name_at: [0, 3, 5], deps: [0, 2], deps_at: [0, 1, 2] };

test("records: every variant", () => {
  const evs = [
    { Began: { build: 1, command: "build", root: "/w" } },
    { Started: { step: "Codegen", of: "anim" } }, { Ended: { step: "Codegen", of: null } },
    { Crate: { name: "anim", state: "Built", fns: { total: 3, done: 3, kept: 1 } } },
    { Rebuilt: { name: "anim::f", krate: "anim", cause: { Edited: { file: "src/lib.rs" } } } },
    { Rebuilt: { name: "anim::g", krate: "anim", cause: "Signature" } },
    { Progress: { step: "Codegen", done: 3, total: 9, longest: null } }, { Cache: { cache: { found: 1, fetched: 0, compiled: 2, cold: null } } },
    { Unlisted: { count: 4 } }, { Refused: { name: "x", why: "y" } }, { Said: { level: "Warning", text: "t" } },
    { Finished: { ok: true, summary: { took_ns: 5, crates: 1, fns: { total: 1, done: 1, kept: 0 }, rebuilt: [["anim::f", "Body"]], more: 0, error: null, cache: null, slowest: [{ step: "Codegen", of: "anim", took_ns: 5 }] } } },
    { Node: { id: 1, kind: "Codegen", name: "anim", deps: [0] } }, { Moved: { id: 1, state: { Cached: { remote: false } } } },
    { Nodes: { first: 2, kinds: ["Std", "Names"], names: "stdab", name_at: [0, 3, 5], deps: [0, 2], deps_at: [0, 1, 2] } },
    { Moved: { id: 1, state: "Running" } }, { Moved: { id: 1, state: "Cancelled" } }, { Critical: { path: [0, 1] } }, { Cores: { busy: 790, of: 8 } },
    { Compiling: { functions: [{ krate: "anim", name: "anim::f", ns: 300000000 }] } }, { Slowest: { functions: [] } },
    { Functions: { object: 0, whole: true, functions: [{ id: 0, key: 2n ** 63n, krate: "anim", name: "anim::f", calls: [1], how: { Compiled: { ns: 7 } } }] } },
    "LoadWorkspace",
  ];
  for (const ev of evs.slice(0, -1)) parseRecord({ at_ns: 1, event: ev });
  assert.throws(() => parseRecord({ at_ns: 1, event: evs[evs.length - 1] }), /unknown variant "LoadWorkspace"/);
});
for (const [name, r, what] of [
  ["unknown variant", { at_ns: 1, event: { Bogus: {} } }, /unknown variant "Bogus"/],
  ["missing field", { at_ns: 1, event: { Began: { build: 1, command: "b" } } }, /no field "root"/],
  ["extra field", { at_ns: 1, event: { Unlisted: { count: 1, more: 2 } } }, /unknown field "more"/],
  ["wrong type", { at_ns: 1, event: { Unlisted: { count: "1" } } }, /not a u64/],
  ["bad step", { at_ns: 1, event: { Started: { step: "Bake", of: null } } }, /not a Step/],
  ["unit with fields", { at_ns: 1, event: { Rebuilt: { name: "a", krate: "b", cause: { Cold: {} } } } }, /unit variant Cold with fields/],
  ["struct without fields", { at_ns: 1, event: { Rebuilt: { name: "a", krate: "b", cause: "Edited" } } }, /without its fields/],
  ["id above u32", { at_ns: 1, event: { Critical: { path: [2 ** 32] } } }, /more than a u32/],
  ["huge at_ns", { at_ns: 2n ** 60n, event: { Unlisted: { count: 1 } } }, /counts exactly/],
  ...[
    ["nodes with offsets too few", { name_at: [0, 3] }, /name_at: has 2 offsets for 2 nodes and 5 entries/],
    ["nodes with offsets going back", { deps_at: [0, 2, 1] }, /deps_at: has 3 offsets for 2 nodes and 2 entries/],
    ["nodes with offsets going back inside", { deps_at: [0, 3, 2], deps: [0, 2] }, /deps_at: goes back at node 1/],
    ["nodes cutting a character", { names: "st\u00e9", name_at: [0, 3, 4] }, /name_at: cuts a character at node 0/],
    ["nodes past a u32", { first: 0xffffffff }, /2 ids from 4294967295, past a u32/],
    ["nodes waiting for each other", { deps: [3, 2] }, /nodes 2, 3 waiting for each other/],
    ["a node waiting for itself", { deps: [0, 3] }, /nodes 3 waiting for each other/],
  ].map(([name, edit, what]) => [name, { at_ns: 1, event: { Nodes: { ...NODES, ...edit } } }, what]),
]) test(`records: refuses ${name}`, () => assert.throws(() => parseRecord(r), (e) => e instanceof RecordError && what.test(e.message)));

test("records: nodes said together read as their nodes", () => {
  const { event } = parseRecord({ at_ns: 1, event: { Nodes: { ...NODES, names: "\u00e9ab", name_at: [0, 2, 4], deps: [3, 0] } } });
  assert.deepEqual(event.nodes, [
    { id: 2, kind: "Std", name: "\u00e9", deps: [3] },
    { id: 3, kind: "Names", name: "ab", deps: [0] },
  ]);
});

for (const [name, krate, want] of [
  ["anim::scene::render", "anim", "anim::scene"],
  ["<alloc::vec::Vec<T> as core::ops::drop::Drop>::drop", "alloc", "alloc::vec::Vec"],
  ["core::ptr::drop_in_place::<anim::Thing>", "anim", "core::ptr"],
  ["anim::main::{closure#0}", "anim", "anim"],
  ["anim::main::{closure#0}::{closure#1}", "anim", "anim"],
  ["<anim::Mesh>::new", "anim", "anim::Mesh"],
  ["<&[u8] as core::fmt::Debug>::fmt", "core", "core"],
  ["<&mut anim::W as core::fmt::Write>::write_str", "core", "anim::W"],
  ["<dyn core::any::Any as core::fmt::Debug>::fmt", "core", "core::any::Any"],
  ["<fn() -> u8 as core::fmt::Debug>::fmt", "core", "core"],
  ["<(A, B) as core::clone::Clone>::clone", "core", "core"],
  ["<<anim::A as anim::T>::Out as core::fmt::Debug>::fmt", "anim", "anim::A"],
  ["main", "anim", "anim"],
  ["anim::Mesh::{impl#2}::build", "anim", "anim::Mesh"],
]) test(`names: ${name}`, () => assert.equal(moduleOf(name, krate), want));
test("names: split keeps generics", () => assert.deepEqual(splitPath("a::<b::c>::d"), ["a", "<b::c>", "d"]));
for (const [name, want] of [
  ["anim::main", "main"],
  ["<alloc::vec::Vec<T>>::push", "push"],
  ["<wgpu::Texture as core::ops::drop::Drop>::drop", "drop"],
  ["core::ptr::drop_in_place::<anim::Thing>", "drop_in_place"],
  ["anim::main::{closure#0}", "{closure#0}"],
  ["main", "main"],
]) test(`names: own name of ${name}`, () => assert.equal(lastName(name), want));

test("edit: a Rebuilt is found by its crate and own name, among what the build compiled", () => {
  const m = new Model(parseSnapshot(snap()));
  m.mergeFns(5, FnColumns.of([
    { id: 0, key: 10, krate: "anim", name: "anim::main", calls: [], how: { type: "Compiled", ns: 5 } },
    { id: 1, key: 21, krate: "anim", name: "anim::main::{closure#0}", calls: [0], how: { type: "Compiled", ns: 5 } },
  ]), 11);
  const a = analyseEdit(m, [{ name: "main", krate: "anim", cause: { type: "Edited", file: "src/main.rs" } }], 11);
  assert.equal(a.entries[0].node, m.names.indexOf("anim::main"));
  assert.deepEqual(a.roots, [m.names.indexOf("anim::main")]);
});

test("model: Functions merges by key, resolves calls across records, adds nodes", () => {
  const m = new Model(parseSnapshot(snap()));
  const r1 = m.mergeFns(4, FnColumns.of([{ id: 0, key: 10, krate: "anim", name: "anim::main", calls: [1, 7], how: { type: "Compiled", ns: 5 } }]), 10);
  assert.deepEqual(r1.added, []);
  assert.equal(m.unresolvedCalls, 2);
  const r2 = m.mergeFns(4, FnColumns.of([{ id: 1, key: 99n, krate: "newcrate", name: "newcrate::f", calls: [0], how: { type: "Unit" } }]), 10);
  assert.deepEqual(r2.added, [3]);
  assert.equal(m.unresolvedCalls, 1);
  assert.ok(m.hasEdge(0, 3) && m.hasEdge(3, 0));
  assert.equal(m.crates[m.krate[3]], "newcrate");
  assert.equal(m.nodeBuild[0], 10);
  assert.throws(() => m.mergeFns(4, FnColumns.of([{ id: 0, key: 11, krate: "core", name: "core::fmt::write", calls: [], how: { type: "Stored" } }]), 10), /id 0 is said for two/);
});

// CBOR as blaze_protocol::cbor writes it (shortest heads, definite lengths), with what a
// hostile frame needs: `Raw` bytes spliced in and `Pairs` for a map with a key twice.
class Raw { constructor(bytes) { this.bytes = bytes; } }
class Pairs { constructor(pairs) { this.pairs = pairs; } }
function cbor(...items) {
  const out = [];
  const head = (major, n) => {
    n = BigInt(n);
    const width = n < 24n ? 0 : n < 0x100n ? 1 : n < 0x10000n ? 2 : n < 0x100000000n ? 4 : 8;
    out.push((major << 5) | (width === 0 ? Number(n) : { 1: 24, 2: 25, 4: 26, 8: 27 }[width]));
    for (let s = BigInt(8 * (width - 1)); s >= 0n; s -= 8n) out.push(Number((n >> s) & 0xffn));
  };
  const go = (v) => {
    if (v === null) out.push(0xf6);
    else if (typeof v === "boolean") out.push(v ? 0xf5 : 0xf4);
    else if (typeof v === "number" || typeof v === "bigint") v < 0 ? head(1, -BigInt(v) - 1n) : head(0, v);
    else if (typeof v === "string") { const b = new TextEncoder().encode(v); head(3, b.length); for (const x of b) out.push(x); }
    else if (v instanceof Raw) for (const x of v.bytes) out.push(x);
    else if (Array.isArray(v)) { head(4, v.length); v.forEach(go); }
    else {
      const pairs = v instanceof Pairs ? v.pairs : Object.entries(v);
      head(5, pairs.length);
      for (const [k, x] of pairs) { go(k); go(x); }
    }
  };
  items.forEach(go);
  return new Uint8Array(out);
}
function rng(seed) {
  return () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 2 ** 32;
  };
}
const shuffled = (rand, xs) => {
  const a = xs.slice();
  for (let i = a.length - 1; i > 0; i--) {
    const j = Math.floor(rand() * (i + 1));
    [a[i], a[j]] = [a[j], a[i]];
  }
  return a;
};
const shuffledMap = (rand, o) => Object.fromEntries(shuffled(rand, Object.entries(o)));

// parseFrame before it read columns, kept to hold the new one to.
function parseFrameByItems(bytes) {
  return decodeSequence(bytes).map((v, i) => {
    try {
      return parseRecord(v);
    } catch (e) {
      throw new RecordError(`item ${i}: ${e.message}`);
    }
  });
}
// Functions as columns, each as the page reads it.
const fnRows = (c) => Array.from({ length: c.n }, (_, k) => ({
  id: c.id[k], key: (BigInt(c.keyHi[k]) << 32n) | BigInt(c.keyLo[k]), krate: c.crates[c.krate[k]], name: c.name[k],
  how: c.how[k], ns: c.ns[k], calls: Array.from(c.calls.subarray(c.callsAt[k], c.callsAt[k + 1])),
}));
const plain = (records) => records.map((r) => r.event.type === "Functions" ? { ...r, event: { ...r.event, functions: fnRows(r.event.functions) } } : r);
const outcome = (f) => {
  try {
    return { ok: plain(f()) };
  } catch (e) {
    return { error: e.constructor.name, message: e.message };
  }
};
const HOW_CODE = { Compiled: 0, Unit: 1, Unchanged: 2, Stored: 3 };
const randomFns = (rand, n) => {
  const pick = (xs) => xs[Math.floor(rand() * xs.length)];
  const u32 = () => Math.floor(rand() * 2 ** 32);
  return Array.from({ length: n }, (_, k) => ({
    id: k === 0 ? 0xffffffff : 3 * k + 1,
    key: pick([() => u32(), () => Math.floor(rand() * 2 ** 53), () => (BigInt(u32()) << 32n) | BigInt(u32()), () => 0, () => 2n ** 64n - 1n])(),
    krate: pick(["anim", "core", "anim", "zürich", "c".repeat(30)]),
    name: `${pick(["anim::main", "<alloc::vec::Vec<T> as core::ops::drop::Drop>::drop", "naïve::ƒ", "", "x".repeat(300)])}#${k}`,
    calls: Array.from({ length: pick([0, 1, 3, 30]) }, () => pick([0, 0xffffffff, Math.floor(rand() * 3 * n)])),
    how: pick([() => "Unit", () => "Unchanged", () => "Stored", () => ({ Compiled: { ns: pick([0, 1, Math.floor(rand() * 1e12), Number.MAX_SAFE_INTEGER]) } })])(),
  }));
};
const functionsRecord = (rand, functions, at_ns = 7) =>
  shuffledMap(rand, { at_ns, event: { Functions: shuffledMap(rand, { object: 3, whole: rand() < 0.5, functions }) } });

test("frame: functions read as columns are those read as items", () => {
  for (let seed = 1; seed <= 300; seed++) {
    const rand = rng(seed);
    const fns = randomFns(rand, Math.floor(rand() * 40));
    const frame = cbor(...shuffled(rand, [functionsRecord(rand, fns.map((f) => shuffledMap(rand, f))), ...[0, 1].map(() => ({ at_ns: 1, event: { Unlisted: { count: 2 } } }))]));
    const got = outcome(() => parseFrame(frame));
    assert.deepEqual(got, outcome(() => parseFrameByItems(frame)), `seed ${seed}`);
    const want = fns.map((f) => ({
      id: f.id, key: BigInt(f.key), krate: f.krate, name: f.name, calls: f.calls,
      how: HOW_CODE[typeof f.how === "string" ? f.how : "Compiled"], ns: typeof f.how === "string" ? 0 : f.how.Compiled.ns,
    }));
    assert.deepEqual(got.ok.find((r) => r.event.type === "Functions").event.functions, want, `seed ${seed}`);
  }
});

const GOOD_FN = { id: 1, key: 10, krate: "anim", name: "anim::f", calls: [2], how: "Unit" };
const without = (k) => Object.fromEntries(Object.entries(GOOD_FN).filter(([f]) => f !== k));
for (const [name, bad] of [
  ["an unknown seventh field", { ...GOOD_FN, more: 1 }],
  ["an unknown field for a known one", { ...without("calls"), cals: [2] }],
  ["a missing field", without("how")],
  ["a field twice", new Pairs([...Object.entries(without("name")), ["id", 1]])],
  ["a text id", { ...GOOD_FN, id: "1" }],
  ["an id above a u32", { ...GOOD_FN, id: 2 ** 32 }],
  ["a negative key", { ...GOOD_FN, key: -1 }],
  ["a key cut short", { ...GOOD_FN, key: new Raw([0x1b, 0, 0]) }],
  ["a key of reserved info", { ...GOOD_FN, key: new Raw([0x1c]) }],
  ["a name not UTF-8", { ...GOOD_FN, name: new Raw([0x62, 0xc3, 0x28]) }],
  ["a crate not UTF-8", { ...GOOD_FN, krate: new Raw([0x62, 0xc3, 0x28]) }],
  ["a crate that is not text", { ...GOOD_FN, krate: 5 }],
  ["a call that is text", { ...GOOD_FN, calls: ["2"] }],
  ["calls that are a map", { ...GOOD_FN, calls: {} }],
  ["Compiled without its fields", { ...GOOD_FN, how: "Compiled" }],
  ["Unit with fields", { ...GOOD_FN, how: { Unit: {} } }],
  ["Compiled with a field more", { ...GOOD_FN, how: { Compiled: { ns: 1, more: 2 } } }],
  ["a time past what the page counts", { ...GOOD_FN, how: { Compiled: { ns: 2n ** 60n } } }],
  ["an unknown how", { ...GOOD_FN, how: "Fresh" }],
  ["a function that is an array", [1, 2]],
  ["a function that is a float", new Raw([0xf9, 0, 0])],
]) test(`frame: a Functions record with ${name} is refused as reading it by items does`, () => {
  const rand = rng(name.length);
  for (const [records, cut] of [
    [[functionsRecord(rand, [GOOD_FN, bad, GOOD_FN])], 0],
    [[{ at_ns: 1, event: { Unlisted: { count: 2 } } }, functionsRecord(rand, [bad])], 0],
    // A record wrong before its functions, and one cut short after: the first error said is
    // the same.
    [[functionsRecord(rand, [bad], 2n ** 60n)], 0],
    [[functionsRecord(rand, [bad]), { at_ns: 1, event: { Unlisted: { count: 2 } } }], 3],
  ]) {
    const whole = cbor(...records);
    const frame = whole.subarray(0, whole.length - cut);
    const got = outcome(() => parseFrame(frame));
    assert.ok(got.error, `${name} is taken`);
    assert.deepEqual(got, outcome(() => parseFrameByItems(frame)));
  }
});

test("keys: KeyIndex finds what a Map does", () => {
  const rand = rng(9);
  const keys = new KeyIndex();
  const want = new Map();
  for (let i = 0; i < 20000; i++) {
    const hi = rand() < 0.5 ? 0 : Math.floor(rand() * 2 ** 32), lo = Math.floor(rand() * (rand() < 0.5 ? 64 : 2 ** 32));
    const k = (BigInt(hi) << 32n) | BigInt(lo);
    if (rand() < 0.6) {
      keys.set(hi, lo, i);
      want.set(k, i);
    }
    assert.equal(keys.get(hi, lo), want.get(k) ?? -1);
  }
  for (const [k, node] of want) assert.equal(keys.get(...halves(k)), node);
  assert.equal(keys.size, want.size);
  assert.deepEqual(halves(2n ** 64n - 1n), [0xffffffff, 0xffffffff]);
  assert.deepEqual(halves(2 ** 52 + 3), [2 ** 20, 3]);
});

test("model: merged functions and their counts are those of merging one by one", () => {
  const rand = rng(4);
  const m = new Model(parseSnapshot(snap()));
  // key -> { krate, name, how, ns, build }, from the snapshot's three, then each merge.
  const want = new Map([10n, 11n, 12n].map((k, i) => [k, { krate: m.crates[m.krate[i]], name: m.names[i], how: m.how[i], ns: m.ns[i], build: m.nodeBuild[i] }]));
  for (let round = 0; round < 60; round++) {
    const fns = randomFns(rand, 1 + Math.floor(rand() * 30)).map((f, k) => ({
      ...f, id: k, key: rand() < 0.3 ? [10, 11, 12][k % 3] : f.key, calls: [],
      how: typeof f.how === "string" ? { type: f.how } : { type: "Compiled", ns: f.how.Compiled.ns },
    }));
    // A key said twice in a record is the later's.
    const byKey = new Map(fns.map((f) => [BigInt(f.key), f]));
    const fresh = [...byKey.keys()].filter((k) => !want.has(k)).length;
    const n = m.n;
    const res = m.mergeFns(round, FnColumns.of([...fns]), 20 + round);
    assert.equal(res.added.length, fresh);
    assert.equal(m.n, n + fresh);
    for (const [k, f] of byKey) want.set(k, { krate: f.krate, name: f.name, how: HOW_CODE[f.how.type], ns: f.how.ns ?? 0, build: 20 + round });
    assert.equal(m.n, want.size);
    for (let i = 0; i < m.n; i++) {
      const k = (BigInt(m.keyHi[i]) << 32n) | BigInt(m.keyLo[i]);
      assert.deepEqual({ krate: m.crates[m.krate[i]], name: m.names[i], how: m.how[i], ns: m.ns[i], build: m.nodeBuild[i] }, want.get(k));
      assert.equal(m.keys.get(m.keyHi[i], m.keyLo[i]), i);
    }
    const scan = [0, 0, 0, 0];
    for (let i = 0; i < m.n; i++) scan[m.how[i]]++;
    assert.deepEqual(m.counts(), { compiled: scan[0], unit: scan[1], unchanged: scan[2], stored: scan[3] });
  }
});

// MainView's colours and sizes before Paint kept them: every point, every frame.
function paintEveryPoint(m, flashes, now, slow) {
  const palette = m.crates.map(crateColor);
  const size = (ns) => (ns > 0 ? 2 + 1.6 * Math.log10(1 + ns / 20000) : 1.6);
  const colors = new Float32Array(4 * m.n), sizes = new Float32Array(m.n);
  const base = (i) => {
    const c = palette[m.krate[i]];
    const lit = (m.how[i] === 0 || m.how[i] === 1) && m.nodeBuild[i] === m.current;
    const k = lit ? 1 : 0.32;
    colors.set([c[0] * k, c[1] * k, c[2] * k, lit ? 1 : 0.7], 4 * i);
  };
  for (let i = 0; i < m.n; i++) {
    base(i);
    sizes[i] = size(m.ns[i]);
  }
  for (const [i, t0] of flashes) {
    const f = Math.max(0, 1 - (now - t0) / (1400 * slow));
    const e = f * f * (3 - 2 * f);
    base(i);
    const glow = [1, 0.86, 0.45];
    for (let k = 0; k < 3; k++) colors[4 * i + k] += (glow[k] - colors[4 * i + k]) * e;
    colors[4 * i + 3] = 1;
    sizes[i] = size(m.ns[i]) * (1 + 1.8 * e);
  }
  return { colors, sizes };
}

test("paint: restyling the points that changed draws what restyling all of them did", () => {
  for (let seed = 1; seed <= 40; seed++) {
    const rand = rng(seed);
    const pick = (n) => Math.floor(rand() * n);
    const m = { crates: ["anim", "core"], n: 0, krate: [], how: [], ns: [], nodeBuild: [], current: 1 };
    const paint = new Paint(m, seed % 2 ? 1 : 3);
    paint.all(0);
    for (let frame = 1; frame <= 80; frame++) {
      const now = 16 * frame;
      const touched = [];
      const relit = rand() < 0.15;
      if (relit) m.current++;
      for (let a = pick(rand() < 0.2 ? 200 : 4); a > 0; a--) {
        const i = m.n++;
        m.krate.push(0); m.how.push(0); m.ns.push(0); m.nodeBuild.push(0);
        touched.push(i);
      }
      for (let c = pick(12); c > 0 && m.n; c--) touched.push(pick(m.n));
      if (rand() < 0.1) m.crates.push(`crate${m.crates.length}`);
      for (const i of touched) {
        m.krate[i] = pick(m.crates.length);
        m.how[i] = pick(4);
        m.ns[i] = rand() < 0.3 ? 0 : pick(1e9);
        m.nodeBuild[i] = m.current - pick(2);
      }
      for (let f = pick(3); f > 0 && m.n; f--) paint.flashes.set(pick(m.n), now - pick(1500));
      if (rand() < 0.3) paint.tick(now);
      paint.some(touched, relit, now);
      const want = paintEveryPoint(m, paint.flashes, now, paint.slow);
      const got = paint.points();
      assert.deepEqual([got.colors, got.sizes], [want.colors, want.sizes], `seed ${seed} frame ${frame}`);
    }
  }
});

const [fixture, records] = process.argv.slice(2);
if (fixture) {
  test("fixture: snapshot and records", () => {
    const s = parseSnapshot(new Uint8Array(readFileSync(fixture)));
    const m = new Model(s);
    const rs = decodeSequence(new Uint8Array(readFileSync(records))).map(parseRecord);
    assert.equal(rs[0].event.type, "Began");
    const fns = rs.find((r) => r.event.type === "Functions").event;
    const res = m.mergeFns(fns.object, fns.functions, rs[0].event.build);
    assert.equal(res.added.length, 1);
    assert.equal(m.unresolvedCalls, 0);
  });
}
console.log(`${passed} passed${process.exitCode ? ", some FAILED" : ""}`);
