import { afterAll, expect, test } from "bun:test";

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
const { Repl } = await import("../repl.ts");
const { dispatch } = await import("./index.ts");
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
