// The kernel's codec against harness-proto's golden fixtures (written by its fixtures_are_current test): each .cbor
// decodes to its .json, and the .json encodes to exactly the .cbor's bytes. Then the chat log reader, and canonical form.
import { afterAll, expect, test } from "bun:test";
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

// genui's Shader and Video (ui/shader.ts, ui/element.ts): cells run through the kernel as the MCP ts tool runs them, with no
// harnessd (a socket that is never there) and no live ~/.harness, and the tree the app would be sent is checked against the wire.
// The Shader tests compile with the slang fork (shader.ts `slangc`), so they need it built.
const scratch = (await import("node:fs")).mkdtempSync(`${(await import("node:os")).tmpdir()}/genui-test-`);
process.env.HARNESS_SOCK = `${scratch}/never.sock`;
process.env.HARNESS_HOME = scratch;
afterAll(async () => (await import("node:fs")).rmSync(scratch, { recursive: true, force: true }));
await Bun.write(`${scratch}/pic.png`, new Uint8Array([0x89, 0x50, 0x4e, 0x47]));
await Bun.write(`${scratch}/clip.mp4`, new Uint8Array([0, 0, 0, 0x18]));
await Bun.write(`${scratch}/glow.png`, new Uint8Array([0x89, 0x50, 0x4e, 0x47]));
const { Repl } = await import("./repl.ts");
const { dispatch } = await import("./ui/index.ts");
const repl = new Repl({});
type Node = { type: string; props: Record<string, unknown>; children: unknown[] };
const cell = async (code: string) => repl.run(code).done;
const nodes = (ui: unknown): Node[] => {
  const all: Node[] = [];
  const walk = (n: unknown) => { if (n && typeof n === "object" && "type" in n) { all.push(n as Node); (n as Node).children.forEach(walk); } else if (Array.isArray(n)) n.forEach(walk); };
  walk(ui);
  return all;
};
const one = (ui: unknown, type: string) => nodes(ui).filter((n) => n.type === type);

test("a Shader on the wire: msl, uniforms and textures in slot order, '@id's resolved over the whole tree", async () => {
  const t = await cell(`<Column>
  <Image id="pic" src="${scratch}/pic.png" />
  <Video id="clip" src="${scratch}/clip.mp4" />
  <Shader id="fx" aspect="4/3" textures={{ v: '@clip', p: '@pic', g: '${scratch}/glow.png', w: 'https://example.com/a.mp4', me: '@fx' }}
    uniforms={{ z: 0.5, g: '@gain', on: '@on', v: [1, 2, 3], col: '#ff0000', flag: true }}>
float4 main(ShaderInput in) {
    float4 a = texture(v, in.uv) + texture(p, in.uv) + g.Sample(den_linear, in.uv) + texture(w, in.uv) * texture(me, in.uv);
    return a * in.u.z * in.u.g * in.u.on + float4(in.u.v, in.u.flag) * in.u.col;
}
  </Shader>
  <Slider id="gain" value={2} />
  <Toggle id="on" value={true} />
</Column>`);
  expect(t.result).toBe("UI shown (6 elements)");
  const [shader] = one(t.ui, "Shader");
  const { msl, key, ...props } = shader!.props;
  expect(String(msl)).toContain(" den_fs(");
  expect(String(msl)).toContain(" den_vs(");
  expect(String(msl)).toContain("[[texture(4)]]");
  expect(String(key)).toMatch(/^[0-9a-f]{16}$/);
  expect(props).toEqual({
    uniforms: [
      { name: "z", lanes: 1, value: [0.5, 0, 0, 0] },
      { name: "g", lanes: 1, value: [2, 0, 0, 0], from: "gain" },
      { name: "on", lanes: 1, value: [1, 0, 0, 0], from: "on" },
      { name: "v", lanes: 3, value: [1, 2, 3, 0] },
      { name: "col", lanes: 4, value: [1, 0, 0, 1] },
      { name: "flag", lanes: 1, value: [1, 0, 0, 0] },
    ],
    textures: [
      { name: "v", kind: "ref", ref: "clip" },
      { name: "p", kind: "image", src: `${scratch}/pic.png` },
      { name: "g", kind: "image", src: `${scratch}/glow.png` },
      { name: "w", kind: "video", src: "https://example.com/a.mp4" },
      { name: "me", kind: "ref", ref: "fx" },
    ],
    aspect: 1.333333, animate: false, pointer: false, id: "fx",
  });
  expect(shader!.children).toEqual([]);
  expect(one(t.ui, "Slider")[0]!.props.id).toBe("gain");
  expect(one(t.ui, "Image")[0]!.props.id).toBe("pic");
});

test("a Shader that reads time and the pointer animates and follows it; a Shadertoy mainImage compiles", async () => {
  const t = await cell(`<Column>
  <Shader source={"float4 main(ShaderInput in) { return float4(in.uv * in.hovered, sin(in.time), 1); }"} />
  <Shader source={"void mainImage(out float4 c, in float2 p) { c = float4(p / iResolution.xy, iMouse.z, 1); }"} animate />
</Column>`);
  expect(t.status).toBe("done");
  expect(one(t.ui, "Shader").map((s) => [s.props.animate, s.props.pointer])).toEqual([[true, true], [true, true]]);
});

test("a compile error is said at the cell's line, naming the shader line", async () => {
  const t = await cell(`<Column>
  <Text>above</Text>
  <Shader>
float4 main(ShaderInput in) {
    return float4(in.uv.z, 0, 0, 1);
}
  </Shader>
</Column>`);
  expect(t.status).toBe("error");
  expect(t.result).toContain("<Shader> did not compile");
  expect(t.result).toContain("shader line 2:");
  expect(t.result).toContain("'z' is not a member of 'vector<float,2>'");
  expect(t.result).toContain("cell line 5");
});

test("an unknown '@id', a duplicate id and a ref to the wrong kind are errors naming the id", async () => {
  const unknown = await cell(`<Shader uniforms={{ g: '@gian' }} source={"float4 main(ShaderInput in) { return in.u.g; }"} />`);
  expect(unknown.status).toBe("error");
  expect(unknown.result).toContain("gian");
  const twice = await cell(`<Column><Slider id="a" /><Toggle id="a" /></Column>`);
  expect(twice.status).toBe("error");
  expect(twice.result).toContain('has the id "a" a Slider has');
  const kind = await cell(`<Column><Image id="pic" src="${scratch}/pic.png" /><Shader uniforms={{ g: '@pic' }} source={"float4 main(ShaderInput in) { return in.u.g; }"} /></Column>`);
  expect(kind.status).toBe("error");
  expect(kind.result).toContain("pic");
  expect(kind.result).toContain("Image");
  const tex = await cell(`<Column><Slider id="s" /><Shader textures={{ t: '@s' }} source={"float4 main(ShaderInput in) { return texture(t, in.uv); }"} /></Column>`);
  expect(tex.status).toBe("error");
  expect(tex.result).toContain("Slider");
});

test("a Video on the wire: its defaults, and bind:currentTime and bind:paused written back by its events", async () => {
  const plain = await cell(`<Video src="${scratch}/clip.mp4" />`);
  expect(plain.ui).toEqual({ type: "Video", props: { src: `${scratch}/clip.mp4`, autoplay: false, loop: false, muted: true, controls: true, fit: "contain" }, children: [] });
  const t = await cell(`<script>
  let at = $state(1);
  let paused = $state(true);
  let level = $state(3);
</script>
<Column>
  <Video id="v" src="https://example.com/a.mp4" poster="${scratch}/pic.png" aspect="16:9" loop bind:currentTime={at} bind:paused onended={() => level = 0} />
  <Slider bind:value={level} />
  <Text>{at} {paused} {level}</Text>
</Column>`);
  const [video] = one(t.ui, "Video");
  const { onTimeUpdate, onPlay, onPause, onEnded, ...props } = video!.props;
  expect(props).toEqual({ src: "https://example.com/a.mp4", poster: `${scratch}/pic.png`, autoplay: false, loop: true, muted: true, controls: true, aspect: 1.777778, fit: "contain", id: "v", paused: true, currentTime: 1 });
  for (const h of [onTimeUpdate, onPlay, onPause, onEnded]) expect(String(h)).toMatch(/^h\d+$/);
  const text = () => one(t.ui, "Text")[0]!.props.text;
  expect(await dispatch(t.id, String(onTimeUpdate), 4.5)).toBeUndefined();
  expect(await dispatch(t.id, String(onPlay), null)).toBeUndefined();
  await Bun.sleep(20);
  expect(text()).toBe("4.5 false 3");
  expect(one(t.ui, "Video")[0]!.props).toMatchObject({ currentTime: 4.5, paused: false });
  await dispatch(t.id, String(one(t.ui, "Slider")[0]!.props.onChange), 7);
  await dispatch(t.id, String(onPause), null);
  await Bun.sleep(20);
  expect(text()).toBe("4.5 true 7");
  await dispatch(t.id, String(onEnded), null);
  await Bun.sleep(20);
  expect(text()).toBe("4.5 true 0");
});
