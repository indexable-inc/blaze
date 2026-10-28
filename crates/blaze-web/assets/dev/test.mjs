// node test.mjs [fixture.blzw fixture.edit.cbor]: the page's parsers against good and hostile input.
import { readFileSync } from "node:fs";
import assert from "node:assert/strict";
import { decode, decodeSequence, CborError } from "../cbor.js";
import { parseSnapshot, SnapshotError } from "../snapshot.js";
import { parseRecord, RecordError } from "../records.js";
import { lastName, moduleOf, splitPath } from "../names.js";
import { analyseEdit } from "../edit.js";
import { Model } from "../model.js";

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
    { Moved: { id: 1, state: "Running" } }, { Critical: { path: [0, 1] } }, { Cores: { busy: 790, of: 8 } },
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
]) test(`records: refuses ${name}`, () => assert.throws(() => parseRecord(r), (e) => e instanceof RecordError && what.test(e.message)));

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
  m.mergeFns(5, [
    { id: 0, key: 10, krate: "anim", name: "anim::main", calls: [], how: { type: "Compiled", ns: 5 } },
    { id: 1, key: 21, krate: "anim", name: "anim::main::{closure#0}", calls: [0], how: { type: "Compiled", ns: 5 } },
  ], 11);
  const a = analyseEdit(m, [{ name: "main", krate: "anim", cause: { type: "Edited", file: "src/main.rs" } }], 11);
  assert.equal(a.entries[0].node, m.names.indexOf("anim::main"));
  assert.deepEqual(a.roots, [m.names.indexOf("anim::main")]);
});

test("model: Functions merges by key, resolves calls across records, adds nodes", () => {
  const m = new Model(parseSnapshot(snap()));
  const r1 = m.mergeFns(4, [{ id: 0, key: 10, krate: "anim", name: "anim::main", calls: [1, 7], how: { type: "Compiled", ns: 5 } }], 10);
  assert.deepEqual(r1.added, []);
  assert.equal(m.unresolvedCalls, 2);
  const r2 = m.mergeFns(4, [{ id: 1, key: 99n, krate: "newcrate", name: "newcrate::f", calls: [0], how: { type: "Unit" } }], 10);
  assert.deepEqual(r2.added, [3]);
  assert.equal(m.unresolvedCalls, 1);
  assert.ok(m.hasEdge(0, 3) && m.hasEdge(3, 0));
  assert.equal(m.crates[m.krate[3]], "newcrate");
  assert.equal(m.nodeBuild[0], 10);
  assert.throws(() => m.mergeFns(4, [{ id: 0, key: 11, krate: "core", name: "core::fmt::write", calls: [], how: { type: "Stored" } }], 10), /id 0 is said for two/);
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
