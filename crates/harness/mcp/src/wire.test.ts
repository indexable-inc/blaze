// The kernel's codec against harness-proto's golden fixtures (written by its fixtures_are_current test): each .cbor
// decodes to its .json, and the .json encodes to exactly the .cbor's bytes. Then the chat log reader, and canonical form.
import { expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { decode, encode } from "./cbor.ts";
import { MAGIC, T_EVENT, frame, messageRows, readLog, toEvent, type Event } from "./wire.ts";

const DIR = `${import.meta.dir}/../../harness-proto/fixtures`;
const names = readdirSync(DIR).filter((f) => f.endsWith(".cbor")).map((f) => f.slice(0, -5)).sort();
const fixture = (n: string) => ({ bytes: new Uint8Array(readFileSync(`${DIR}/${n}.cbor`)), json: JSON.parse(readFileSync(`${DIR}/${n}.json`, "utf8")) });
const hex = (b: Uint8Array) => Buffer.from(b).toString("hex");

test("there are fixtures", () => expect(names.length).toBeGreaterThan(15));

for (const n of names) {
  test(`fixture ${n}`, () => {
    const { bytes, json } = fixture(n);
    expect(decode(bytes)).toEqual(json);
    expect(hex(encode(json))).toBe(hex(bytes));
    if (n.startsWith("event-")) expect(hex(encode(toEvent(decode(bytes))))).toBe(hex(bytes));
  });
}

const events = names.filter((n) => n.startsWith("event-"));
const log = () => {
  const frames = events.map((n) => frame(T_EVENT, fixture(n).bytes));
  return { frames, bytes: new Uint8Array(Buffer.concat([MAGIC, ...frames])) };
};

test("a log's records", () => {
  const { frames, bytes } = log();
  const r = readLog(bytes);
  if ("notALog" in r) throw new Error(r.notALog);
  expect(r.torn).toBeNull();
  expect(r.end).toBe(bytes.length);
  expect(r.records.map((x) => x.n)).toEqual(events.map((_, i) => i + 1));
  expect(r.records.map((x) => x.event)).toEqual(events.map((n) => toEvent(fixture(n).json)));
  // A bad record is skipped; the ones after it stand.
  const bad = new Uint8Array(Buffer.concat([MAGIC, frame(T_EVENT, encode({ ev: "nope", ts: 1 })), ...frames]));
  const b = readLog(bad);
  if ("notALog" in b) throw new Error(b.notALog);
  expect(b.records[0]!.event).toBeNull();
  expect(b.records[0]!.error).toContain("nope");
  expect(b.records.slice(1).every((x) => x.event !== null)).toBe(true);
});

test("a torn last frame is reported, the records before it stand", () => {
  const { frames, bytes } = log();
  const r = readLog(bytes.subarray(0, bytes.length - 1));
  if ("notALog" in r) throw new Error(r.notALog);
  const end = bytes.length - frames.at(-1)!.length;
  expect(r.records.length).toBe(frames.length - 1);
  expect(r.torn).toEqual({ at: end, bytes: frames.at(-1)!.length - 1 });
  expect(r.end).toBe(end);
  const h = readLog(bytes.subarray(0, end + 3)); // inside the header
  if ("notALog" in h) throw new Error(h.notALog);
  expect(h.torn).toEqual({ at: end, bytes: 3 });
});

test("empty, unstarted and foreign logs", () => {
  expect(readLog(new Uint8Array())).toEqual({ records: [], torn: null, end: 0 });
  expect(readLog(MAGIC.subarray(0, 4))).toEqual({ records: [], torn: null, end: 4 });
  expect(readLog(new TextEncoder().encode('{"ev":"user"}\n'))).toHaveProperty("notALog", expect.stringContaining("NDJSON"));
  expect(readLog(new Uint8Array([0, 1, 2]))).toHaveProperty("notALog", expect.stringContaining("not a harness chat log"));
});

test("messages rows", () => {
  const evs: Event[] = [
    { ts: 1, ev: "user", text: "hi" },
    { ts: 2, ev: "block", block: "m:0", kind: "thinking", text: "hm" },
    { ts: 3, ev: "tool_start", block: "t1", name: "ts" },
    { ts: 4, ev: "tool_call", block: "t1", name: "ts", input: { code: "1+1" } },
    { ts: 5, ev: "tool_result", block: "t1", text: "2", error: false },
    { ts: 6, ev: "block", block: "m:1", kind: "text", text: "2." },
    { ts: 7, ev: "turn_end", ok: true, stop: "end_turn", usage: null, text: "2." },
  ];
  expect(messageRows("s1", evs)).toEqual([
    { object: "s1", n: 1, role: "user", kind: "text", tool: null, text: "hi", ts_ms: 1 },
    { object: "s1", n: 2, role: "assistant", kind: "thinking", tool: null, text: "hm", ts_ms: 2 },
    { object: "s1", n: 3, role: "assistant", kind: "tool_call", tool: "ts", text: '{"code":"1+1"}', ts_ms: 4 },
    { object: "s1", n: 4, role: "user", kind: "tool_result", tool: "ts", text: "2", ts_ms: 5 },
    { object: "s1", n: 5, role: "assistant", kind: "text", tool: null, text: "2.", ts_ms: 6 },
    { object: "s1", n: 6, role: "system", kind: "turn_end", tool: null, text: "2.", ts_ms: 7 },
  ]);
});

test("canonical encoding", () => {
  expect(hex(encode({ zz: 500, a: 2.0, bb: 0.5, c: 0.1 }))).toBe("a4" + "616102" + "6163fb3fb999999999999a" + "626262f93800" + "627a7a1901f4");
  expect(hex(encode(0.5))).toBe("f93800");
  expect(hex(encode(2.0))).toBe("02");
  expect(hex(encode(0.1))).toBe("fb3fb999999999999a");
  expect(hex(encode(500))).toBe("1901f4");
  expect(hex(encode(-40000))).toBe("399c3f"); // -1 - 39999, in two bytes (as the up-repl-task fixture has it)
  expect(hex(encode(-70000))).toBe("3a0001116f");
  expect(hex(encode({ "-0": -0 }))).toBe("a1622d3000");
  expect(hex(encode(1.5e-7))).toBe(hex(new Uint8Array([0xfb, ...new Uint8Array(new Float64Array([1.5e-7]).buffer).reverse()])));
  expect(hex(encode(2 ** -24))).toBe("f90001"); // the smallest half subnormal
  expect(hex(encode(65504.5))).toBe("fa477fe080"); // past a half, exact in a single
  expect(hex(encode(2 ** 64 - 2048))).toBe("1bfffffffffffff800");
  expect(hex(encode(-(2 ** 63)))).toBe("3b7fffffffffffffff");
  expect(hex(encode(2 ** 64))).toBe("fa5f800000"); // past a u64: a float
  expect(hex(encode({ a: undefined, b: 1 }))).toBe("a1616201");
  expect(decode(encode(2n ** 63n))).toBe(2n ** 63n);
  expect(decode(encode(-(2n ** 60n)))).toBe(-(2n ** 60n));
  expect(() => decode(new Uint8Array([1, 2]))).toThrow("trailing");
  expect(() => decode(new Uint8Array([0x9f, 0xff]))).toThrow("indefinite");
  expect(() => decode(new Uint8Array([0xc1, 0x00]))).toThrow("tag");
  expect(() => encode({ f: () => 1 })).toThrow("value.f");
  expect(() => encode(new Date())).toThrow("Date");
});
