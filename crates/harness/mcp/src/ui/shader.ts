import { createHash } from 'node:crypto';
import { existsSync, readFileSync } from 'node:fs';
import { extname } from 'node:path';
import { isWebUrl, mediaProblem } from './media.ts';

/*
 * A `Shader`'s source, made into what the app draws: the kernel wraps the agent's Slang in the prelude (the `ShaderInput` it reads,
 * its uniforms and textures at fixed slots, GLSL's names that mean the same, a full-screen triangle and the fragment entry that
 * fills `ShaderInput` and premultiplies), runs our slangc fork to MSL, and sends the MSL. So a shader that does not compile fails
 * the cell at its line, in slangc's words at the shader's own lines, before anything reaches the app. The binding layout here is
 * the contract with the app's Media (harness-native-render src/media): fragment buffer 0 is `ShaderFrame { float4 size; float4
 * pointer; float4 slots[max(1, N)]; }`, sampler 0 linear clamp, texture i the wire's `textures[i]`; never scraped from the MSL.
 * Den's crates/den-native/src/shader_element/mod.rs is where the prelude comes from.
 */

/** The most uniforms and textures a shader takes, the longest name either may have, and the longest source compiled (bytes). */
export const MAX_UNIFORMS = 16;
export const MAX_TEXTURES = 8;
const MAX_NAME = 32;
export const MAX_SOURCE = 64 << 10;
/** How long slangc may take over one shader, and the most of its errors a cell is told. */
const COMPILE_MS = 10_000;
const MAX_ERRORS = 5;
/** What slangc's diagnostics call the agent's source and the prelude around it (the names of their `#line` marks). */
const USER_FILE = 'shader';
const PRELUDE_FILE = 'prelude';
/** The entry points slangc emits in the MSL, by these exact names (the app makes its pipeline from them). */
export const VERTEX = 'den_vs';
export const FRAGMENT = 'den_fs';
/** The flags every compile runs with: both entries, and the warnings explicit registers raise (no Vulkan binding, an "overlap" of b0/s0/t0) off. */
export const SLANGC_ARGS = ['-lang', 'slang', '-target', 'metal', '-entry', VERTEX, '-stage', 'vertex', '-entry', FRAGMENT, '-stage', 'fragment', '-warnings-disable', '39029,39001'];

/** Our Slang fork (github.com/indexable-inc/slang), as harness-native-render's build.rs finds it; never a stock slangc from PATH. */
export const SLANGC_FORK = ['/Volumes/Projects/indexable-inc/slang/build/Release/bin/slangc', '/Volumes/Projects/indexable-inc/slang/build/bin/slangc'];

/** The slangc to run: `$SLANGC`, else the fork's Release build, else its plain build; none is an error naming all three. */
export function slangc(): string {
  const env = process.env.SLANGC;
  if (env) {
    if (!existsSync(env)) throw new Error(`<Shader> cannot compile: SLANGC=${env} is not there`);
    return env;
  }
  const found = SLANGC_FORK.find(path => existsSync(path));
  if (found) return found;
  throw new Error(`<Shader> cannot compile: slangc (our fork) is not at $SLANGC (unset), ${SLANGC_FORK.join(' or ')}; build github.com/indexable-inc/slang: cmake --build build --config Release --target slangc`);
}

/** One uniform as the app reads it: its slot's lanes and value (rest 0), and `from`, the id of the element whose live value it is. */
export interface WireUniform { name: string; lanes: 1 | 2 | 3 | 4; value: [number, number, number, number]; from?: string }
/** One texture as the app reads it: a picture or a video file (or http(s) URL), or `ref`, the id of a Video or a Shader in the cell. */
export interface WireTexture { name: string; kind: 'image' | 'video' | 'ref'; src?: string; ref?: string }

/** The extensions a texture file is read by: a picture decoded once, or a video played hidden, muted and looping. */
const IMAGE_EXTS = new Set(['.png', '.jpg', '.jpeg', '.gif', '.heic', '.webp']);
const VIDEO_EXTS = new Set(['.mov', '.mp4', '.m4v']);

/** The id an `'@id'` names, or undefined when `value` is not one. */
export const refOf = (value: unknown): string | undefined => typeof value === 'string' ? /^@([A-Za-z_][\w-]*)$/.exec(value.trim())?.[1] : undefined;

/** What `id` must be: words a `'@id'` can name. */
export const ID_RULE = (v: unknown): string | undefined => typeof v === 'string' && /^[A-Za-z_][\w-]{0,63}$/.test(v) ? undefined : `an id: a letter or _, then letters, digits, _ or -, at most 64 ('glow'), which '@glow' names`;

/** Slang's words a name cannot be, and the prelude's names a texture (a global) would collide with. */
const KEYWORDS = new Set([
  'in', 'out', 'inout', 'return', 'if', 'else', 'for', 'while', 'do', 'switch', 'case', 'default', 'break', 'continue', 'discard', 'struct',
  'class', 'interface', 'extension', 'enum', 'namespace', 'import', 'module', 'this', 'This', 'true', 'false', 'void', 'bool', 'int', 'uint',
  'half', 'float', 'double', 'const', 'static', 'uniform', 'let', 'var', 'typedef', 'typealias', 'register', 'groupshared', 'sizeof',
]);
const PRELUDE_NAMES = new Set([
  'ShaderFrame', 'Uniforms', 'ShaderInput', 'DenV', 'main', 'mainImage', 'texture', 'textureSize', 'mix', 'fract', 'mod', 'atan', 'inversesqrt',
  'vec2', 'vec3', 'vec4', 'ivec2', 'ivec3', 'ivec4', 'iTime', 'iResolution', 'iMouse', 'u',
]);

/** Why `name` cannot name a uniform (or, `global`, a texture, which is a global of the shader), or undefined. */
function nameProblem(name: string, global: boolean): string | undefined {
  if (!/^[A-Za-z_]\w*$/.test(name) || name.length > MAX_NAME) return `is not a name: a letter or _, then letters, digits or _, at most ${MAX_NAME}`;
  if (name.startsWith('den_')) return 'starts with den_, which the prelude keeps for itself';
  if (KEYWORDS.has(name)) return `is a word of Slang's`;
  if (global && PRELUDE_NAMES.has(name)) return `is the prelude's own (${[...PRELUDE_NAMES].join(', ')})`;
  return undefined;
}

const said = (v: unknown): string => {
  const text = typeof v === 'string' ? JSON.stringify(v) : v instanceof Blob ? 'Bun.file(...)' : JSON.stringify(v) ?? String(v);
  return text.length > 60 ? `${text.slice(0, 59)}…` : text;
};

/** A `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa` colour as 0..1 channels, sRGB-encoded as written (the UI's colours are), alpha 1 when not said. */
export function colour(text: string): [number, number, number, number] | undefined {
  const hex = /^#([0-9a-f]{3,4}|[0-9a-f]{6}|[0-9a-f]{8})$/i.exec(text.trim())?.[1];
  if (!hex) return undefined;
  const wide = hex.length > 4;
  const out: [number, number, number, number] = [1, 1, 1, 1];
  for (let i = 0; i < hex.length / (wide ? 2 : 1); i++) {
    const v = parseInt(wide ? hex.slice(i * 2, i * 2 + 2) : hex[i]!.repeat(2), 16);
    out[i] = Math.round(v / 255 * 1e6) / 1e6;
  }
  return out;
}

const UNIFORM_TAKES = `a number, true or false, 2 to 4 numbers, a colour ('#ff7a1a', '#rgba') or '@id' of a Slider, NumberField, Toggle, Checkbox or Video`;

/** Uniform `name` as the app reads it, or why `value` is not one. A `'@id'` is one lane whose value the kernel fills in (`resolveRefs`). */
export function uniformOf(name: string, value: unknown): WireUniform | string {
  const bad = nameProblem(name, false);
  if (bad) return `uniforms.${name} ${bad}`;
  const ref = refOf(value);
  if (ref) return { name, lanes: 1, value: [0, 0, 0, 0], from: ref };
  if (typeof value === 'number' && Number.isFinite(value)) return { name, lanes: 1, value: [value, 0, 0, 0] };
  if (typeof value === 'boolean') return { name, lanes: 1, value: [value ? 1 : 0, 0, 0, 0] };
  if (Array.isArray(value) && value.length >= 2 && value.length <= 4 && value.every(n => typeof n === 'number' && Number.isFinite(n))) {
    const lanes = [0, 0, 0, 0] as [number, number, number, number];
    value.forEach((n, i) => { lanes[i] = n as number; });
    return { name, lanes: value.length as 2 | 3 | 4, value: lanes };
  }
  if (typeof value === 'string') {
    const c = colour(value);
    if (c) return { name, lanes: 4, value: c };
    if (value.startsWith('@')) return `uniforms.${name} is ${said(value)}, which names no id: '@' then the id of an element of the cell, '@gain'`;
  }
  return `uniforms.${name} is ${said(value)}: ${UNIFORM_TAKES}`;
}

/** The uniforms of a `uniforms` prop in the order written (the slot order), or why they are not. */
export function uniformsOf(value: unknown): WireUniform[] | string {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return `an object of uniforms by name, uniforms={{ gain: 1.5, tint: '#ff7a1a', level: '@level' }}`;
  const entries = Object.entries(value);
  if (entries.length > MAX_UNIFORMS) return `at most ${MAX_UNIFORMS} uniforms (it has ${entries.length}): pack them in float4s, [a, b, c, d]`;
  const out: WireUniform[] = [];
  for (const [name, item] of entries) {
    const one = uniformOf(name, item);
    if (typeof one === 'string') return one;
    out.push(one);
  }
  return out;
}

/** A texture's kind by what it names: an http(s) URL plays as a video; a file by its extension. */
function textureKind(name: string, src: string): 'image' | 'video' | string {
  if (isWebUrl(src)) return 'video';
  const ext = extname(src).toLowerCase();
  if (IMAGE_EXTS.has(ext)) return 'image';
  if (VIDEO_EXTS.has(ext)) return 'video';
  return `textures.${name} is ${said(src)}: a texture is a picture (${[...IMAGE_EXTS].join(' ')}), a video (${[...VIDEO_EXTS].join(' ')} or an http(s) URL) or '@id' of a Video, Image or Shader`;
}

/** Why a `textures` prop is not one (as it is set, before its files are resolved), or undefined. */
export function texturesProblem(value: unknown): string | undefined {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return `an object of textures by name, textures={{ photo: '~/Pictures/a.png', clip: '@clip' }}`;
  const entries = Object.entries(value);
  if (entries.length > MAX_TEXTURES) return `at most ${MAX_TEXTURES} textures (it has ${entries.length})`;
  for (const [name, item] of entries) {
    const bad = nameProblem(name, true);
    if (bad) return `textures.${name} ${bad}`;
    if (refOf(item)) continue;
    if (typeof item === 'string' && item.startsWith('@')) return `textures.${name} is ${said(item)}, which names no id: '@' then the id of a Video, Image or Shader of the cell, '@clip'`;
    const problem = mediaProblem(item);
    if (problem) return `textures.${name} is not ${problem}`;
    const path = typeof item === 'string' ? item : item instanceof Blob ? (item as Blob & { name?: string }).name : (item as { path?: unknown })?.path;
    if (typeof path === 'string') {
      const kind = textureKind(name, path);
      if (kind !== 'image' && kind !== 'video') return kind;
    }
  }
  return undefined;
}

/** The textures of a `textures` prop whose files tree.ts resolved (absolute paths, URLs and '@id's), in slot order. Throws the problem. */
export function texturesOf(value: Record<string, unknown>): WireTexture[] | string {
  const problem = texturesProblem(value);
  if (problem) return problem;
  return Object.entries(value).map(([name, item]): WireTexture => {
    const ref = refOf(item);
    if (ref) return { name, kind: 'ref', ref };
    return { name, kind: textureKind(name, String(item)) as 'image' | 'video', src: String(item) };
  });
}

/** Whether a source reads the clock (`in.time`, Shadertoy's `iTime`), and the pointer: what decides `animate` and `pointer` when not said. */
export const readsTime = (source: string): boolean => /\.\s*time\b|\biTime\b/.test(source);
export const readsPointer = (source: string): boolean => /\.\s*(?:pointer|hovered|down)\b|\biMouse\b/.test(source);
/** A Shadertoy shader: it defines mainImage and no main. */
const isShadertoy = (source: string): boolean => /\bmainImage\s*\(/.test(source) && !/\bmain\s*\(/.test(source);

const TYPE = ['float', 'float2', 'float3', 'float4'];
const SWIZZLE = ['.x', '.xy', '.xyz', ''];

/** GLSL's names Slang lacks, each with the same meaning (generic, so a shader's own overload wins); `mat2`.. are not (see HINTS). */
const GLSL = `typealias vec2 = float2; typealias vec3 = float3; typealias vec4 = float4;
typealias ivec2 = int2; typealias ivec3 = int3; typealias ivec4 = int4;
__generic<T : __BuiltinFloatingPointType> T mix(T x, T y, T a) { return lerp(x, y, a); }
__generic<T : __BuiltinFloatingPointType, let N : int> vector<T, N> mix(vector<T, N> x, vector<T, N> y, vector<T, N> a) { return lerp(x, y, a); }
__generic<T : __BuiltinFloatingPointType, let N : int> vector<T, N> mix(vector<T, N> x, vector<T, N> y, T a) { return lerp(x, y, vector<T, N>(a)); }
__generic<T : __BuiltinFloatingPointType> T fract(T x) { return frac(x); }
__generic<T : __BuiltinFloatingPointType, let N : int> vector<T, N> fract(vector<T, N> x) { return frac(x); }
__generic<T : __BuiltinFloatingPointType> T mod(T x, T y) { return x - y * floor(x / y); }
__generic<T : __BuiltinFloatingPointType, let N : int> vector<T, N> mod(vector<T, N> x, vector<T, N> y) { return x - y * floor(x / y); }
__generic<T : __BuiltinFloatingPointType, let N : int> vector<T, N> mod(vector<T, N> x, T y) { return x - y * floor(x / y); }
__generic<T : __BuiltinFloatingPointType> T atan(T y, T x) { return atan2(y, x); }
__generic<T : __BuiltinFloatingPointType, let N : int> vector<T, N> atan(vector<T, N> y, vector<T, N> x) { return atan2(y, x); }
__generic<T : __BuiltinFloatingPointType> T inversesqrt(T x) { return rsqrt(x); }
__generic<T : __BuiltinFloatingPointType, let N : int> vector<T, N> inversesqrt(vector<T, N> x) { return rsqrt(x); }
float4 texture(Texture2D<float4> t, float2 uv) { return t.Sample(den_linear, uv); }
float2 textureSize(Texture2D<float4> t) { uint w, h; t.GetDimensions(w, h); return float2(float(w), float(h)); }
int2 textureSize(Texture2D<float4> t, int lod) { uint w, h, levels; t.GetDimensions(uint(lod), w, h, levels); return int2(int(w), int(h)); }
`;

/** `source` in the prelude, for uniforms of `lanes` (by name, in slot order) and textures named `textures` (in slot order). */
export function wrap(source: string, uniforms: ReadonlyArray<{ name: string; lanes: number }>, textures: readonly string[]): string {
  const toy = isShadertoy(source);
  const fields = uniforms.length ? uniforms.map(u => ` ${TYPE[u.lanes - 1]} ${u.name};`).join('') : ' float den_none;';
  const lines = [
    `#line 1 "${PRELUDE_FILE}"`,
    `struct ShaderFrame { float4 size; float4 pointer; float4 slots[${Math.max(1, uniforms.length)}]; };`,
    `struct Uniforms {${fields} };`,
    'struct ShaderInput { float2 uv; float2 pixel; float2 resolution; float time; float scale; float2 pointer; float hovered; float down; Uniforms u; };',
    'ConstantBuffer<ShaderFrame> den_frame : register(b0);',
    'SamplerState den_linear : register(s0);',
    ...textures.map((name, i) => `Texture2D<float4> ${name} : register(t${i});`),
    GLSL,
    // Shadertoy's globals, set before mainImage runs; `u` is the uniforms there, as `in.u` is in main.
    ...(toy ? ['static float iTime; static float3 iResolution; static float4 iMouse; static Uniforms u;'] : []),
    `#line 1 "${USER_FILE}"`,
    source.endsWith('\n') ? source.slice(0, -1) : source,
    `#line 1 "${PRELUDE_FILE}"`,
    'struct DenV { float4 pos : SV_Position; };',
    // A full-screen triangle from the vertex id alone: no buffers.
    `DenV ${VERTEX}(uint vid : SV_VertexID) { DenV o; o.pos = float4(float((vid << 1) & 2) * 2.0 - 1.0, float(vid & 2) * 2.0 - 1.0, 0.0, 1.0); return o; }`,
    `float4 ${FRAGMENT}(DenV v) : SV_Target {`,
    '  ShaderInput i;',
    '  i.pixel = v.pos.xy; i.resolution = den_frame.size.xy; i.uv = v.pos.xy / den_frame.size.xy;',
    '  i.time = den_frame.size.w; i.scale = den_frame.size.z;',
    '  i.pointer = den_frame.pointer.xy; i.hovered = den_frame.pointer.z; i.down = den_frame.pointer.w;',
    ...(uniforms.length ? uniforms.map((u, slot) => `  i.u.${u.name} = den_frame.slots[${slot}]${SWIZZLE[u.lanes - 1]};`) : ['  i.u.den_none = 0.0;']),
    ...(toy ? [
      // Shadertoy's coordinates are from the bottom-left; iMouse is the pointer while over it (zw negative unless pressed); its alpha is not shown.
      '  iTime = i.time; iResolution = float3(i.resolution, 1.0); u = i.u;',
      '  float2 m = float2(i.pointer.x, i.resolution.y - i.pointer.y);',
      '  iMouse = i.hovered > 0.5 ? float4(m, i.down > 0.5 ? m : -m) : float4(0.0);',
      '  float4 c = float4(0.0); mainImage(c, float2(i.pixel.x, i.resolution.y - i.pixel.y)); c.a = 1.0;',
    ] : ['  float4 c = main(i);']),
    '  c = select(isnan(c), float4(0.0), c); c.a = saturate(c.a); c.rgb = clamp(c.rgb, 0.0, 60000.0);',
    '  return float4(c.rgb * c.a, c.a);',
    '}',
  ];
  return `${lines.join('\n')}\n`;
}

/** One thing slangc said: where (the shader's line and column, or the prelude's), whether it is an error, and what. */
export interface Diagnostic { file?: string; line?: number; column?: number; error: boolean; message: string }

/**
 * slangc's diagnostics, in either of its formats: `error[E30027]: header` with ` --> file:line:col` and a caret line whose words say
 * what (kept over the header), or `file(line): error 30027: what`.
 */
export function diagnostics(text: string): Diagnostic[] {
  const out: Diagnostic[] = [];
  let last: Diagnostic | undefined;
  let labelled = false;
  for (const raw of text.split('\n')) {
    const line = raw.trimEnd();
    const head = /^(error|warning|note)(?:\[\w+\])?: (.*)$/.exec(line);
    if (head) {
      last = { error: head[1] === 'error', message: head[2]! };
      labelled = false;
      // A note belongs to the diagnostic before it.
      if (head[1] !== 'note') out.push(last);
      continue;
    }
    const place = /^\s*--> (.*):(\d+):(\d+)$/.exec(line);
    if (place && last && last.line === undefined) { last.file = place[1]; last.line = Number(place[2]); last.column = Number(place[3]); continue; }
    const caret = /^\s*\|\s*[\^~-]+\s+(.*)$/.exec(line);
    if (caret && last && !labelled && caret[1]!.trim()) { last.message = caret[1]!.trim(); labelled = true; continue; }
    const classic = /^(.*)\((\d+)(?:,\s*(\d+))?\): (error|warning)\s*\w*: (.*)$/.exec(line);
    if (classic) out.push(last = { file: classic[1], line: Number(classic[2]), column: classic[3] ? Number(classic[3]) : undefined, error: classic[4] === 'error', message: classic[5]! });
  }
  return out;
}

/** What GLSL and Shadertoy carry that Slang names otherwise, found in a source that did not compile: one hint line names each. */
const HINTS: Array<[RegExp, string]> = [
  [/\bmat[234](?:x[234])?\b/, 'mat2/mat3/mat4 are float2x2/float3x3/float4x4 (filled by rows: GLSL m * v is mul(v, m) with the same numbers)'],
  [/\bgl_FragCoord\b/, 'gl_FragCoord is in.pixel (from the top-left), or fragCoord in mainImage (from the bottom-left)'],
  [/\bgl_FragColor\b/, 'gl_FragColor is what main returns (fragColor in mainImage)'],
  [/\biChannel\d\b/, "iChannel0.. are textures the Shader names: textures={{ iChannel0: 'photo.png' }}"],
  [/\bsampler2D\b/, "sampler2D is a texture named in textures={{ name: 'photo.png' }}, read with texture(name, uv)"],
  [/^\s*uniform\s/m, "uniforms come from the uniforms prop, read as in.u.name (u.name in mainImage): drop the uniform lines"],
  [/^\s*(?:#version|precision\s)/m, 'drop the #version and precision lines: Slang has neither'],
];

/** A compiled shader, or what slangc said against it. */
type Outcome = { msl: string; key: string } | { diagnostics: Diagnostic[]; raw: string };
/** Compiles by the hash of the slangc and the wrapped text: a re-render that changes only uniform values never runs slangc again. */
const cache = new Map<string, Outcome>();
const CACHE_KEPT = 256;

/** Runs slangc on `text` (the wrapped source), synchronously and at most COMPILE_MS. */
function run(text: string): Outcome {
  const bin = slangc();
  const id = createHash('sha256').update(bin).update('\0').update(SLANGC_ARGS.join(' ')).update('\0').update(text).digest('hex');
  const hit = cache.get(id);
  if (hit) return hit;
  const done = Bun.spawnSync({ cmd: [bin, ...SLANGC_ARGS, '-o', '-', '--', '-'], stdin: Buffer.from(text), stdout: 'pipe', stderr: 'pipe', timeout: COMPILE_MS });
  if (done.exitedDueToTimeout) throw new Error(`<Shader> did not compile: slangc (${bin}) ran for more than ${COMPILE_MS / 1000} s`);
  const msl = done.stdout.toString();
  const said = done.stderr.toString();
  let outcome: Outcome;
  if (done.exitCode === 0 && msl.includes(` ${FRAGMENT}(`) && msl.includes(` ${VERTEX}(`)) {
    outcome = { msl, key: createHash('sha256').update(msl).digest('hex').slice(0, 16) };
  } else if (done.exitCode === 0) {
    throw new Error(`<Shader> did not compile: slangc (${bin}) exited 0 but its MSL has no ${VERTEX} and ${FRAGMENT}: ${said.trim().slice(-400) || msl.slice(0, 400)}`);
  } else {
    outcome = { diagnostics: diagnostics(said), raw: said || `slangc exited ${done.exitCode ?? `on ${done.signalCode}`} and said nothing` };
  }
  cache.set(id, outcome);
  for (const key of cache.keys()) { if (cache.size <= CACHE_KEPT) break; cache.delete(key); }
  return outcome;
}

/** The message a cell is told for a source that did not compile: the errors at the shader's lines (and the cell's, `sourceLine`), and a hint. */
function compileError(found: { diagnostics: Diagnostic[]; raw: string }, source: string, toy: boolean, where?: string, sourceLine?: number): Error {
  const errors = found.diagnostics.filter(d => d.error);
  const lines = errors.slice(0, MAX_ERRORS).map(d => {
    if (d.file === USER_FILE && d.line !== undefined) {
      const cell = sourceLine === undefined ? '' : ` (cell line ${sourceLine + d.line - 1})`;
      return `shader line ${d.line}${d.column ? `:${d.column}` : ''}${cell}: ${d.message}`;
    }
    const main = /\bmain(Image)?\b/.test(d.message) || (d.file === PRELUDE_FILE && /undefined identifier|no overload/.test(d.message) && !/\bmain(?:Image)?\s*\(/.test(source));
    const need = toy ? 'void mainImage(out float4 fragColor, in float2 fragCoord)' : 'float4 main(ShaderInput in)';
    return `in the prelude: ${d.message}${main ? ` (the shader defines ${need})` : ' (a name of the shader\'s may collide with a texture\'s or the prelude\'s)'}`;
  });
  if (errors.length > MAX_ERRORS) lines.push(`and ${errors.length - MAX_ERRORS} more`);
  if (lines.length === 0) lines.push(found.raw.split('\n').map(l => l.trim()).filter(Boolean).slice(0, MAX_ERRORS).join('\n'));
  const hints = HINTS.filter(([seen]) => seen.test(source)).map(([, hint]) => hint);
  if (hints.length) lines.push(`hint: ${hints.join('; ')}`);
  // Each error names its cell line when the source sits between the tags; otherwise the element's own line (never `where`'s quoted
  // text, which is the kernel's rewrite of the element, not what the cell says).
  const at = sourceLine === undefined ? /^line \d+(?::\d+)?/.exec(where ?? '')?.[0] : undefined;
  return new SyntaxError(`<Shader> did not compile${at ? ` (${at})` : ''}: ${lines.join('\n')}`);
}

/** `source` wrapped and compiled to MSL for these uniforms and textures; throws the cell's error, at `where`, when it does not compile. */
export function compileShader(source: string, uniforms: ReadonlyArray<{ name: string; lanes: number }>, textures: readonly string[], where?: string, sourceLine?: number): { msl: string; key: string } {
  const found = run(wrap(source, uniforms, textures));
  if ('msl' in found) return found;
  throw compileError(found, source, isShadertoy(source), where, sourceLine);
}

/**
 * A Shader's props as the app reads them (the spec's wire: msl, key, uniforms, textures, width, height, aspect, animate, pointer, id),
 * from props whose `src` and texture files tree.ts already resolved to absolute paths. `'@id'` uniforms carry `from` and a value of 0
 * until `resolveRefs` fills it in; `'@id'` textures are kind `ref`. Throws the error naming what is wrong at `where`.
 */
export function shaderWire(props: Record<string, unknown>, where?: string): Record<string, unknown> {
  const at = where ? ` (${where})` : '';
  const given = (name: string) => props[name] !== undefined && props[name] !== null && props[name] !== '';
  const fail = (problem: string, hint?: string) => new TypeError(`<Shader> ${problem}${at}${hint ? `: ${hint}` : ''}`);
  if (given('source') && given('src')) throw fail('has both source and src', 'the source is written between its tags (or source={...}), or read from src="effect.slang", not both');
  let source: string;
  if (given('src')) source = readFileSync(String(props.src), 'utf8');
  else if (typeof props.source === 'string' && props.source.trim()) source = props.source;
  else throw fail('has no source', 'write float4 main(ShaderInput in) { return float4(in.uv, 0.5, 1.0); } between its tags, or src="effect.slang"');
  if (source.length > MAX_SOURCE) throw fail(`has a source of ${source.length} characters; a Shader's is at most ${MAX_SOURCE}`);
  const uniforms = given('uniforms') ? uniformsOf(props.uniforms) : [];
  if (typeof uniforms === 'string') throw fail(`has ${uniforms}`);
  const textures = given('textures') ? texturesOf(props.textures as Record<string, unknown>) : [];
  if (typeof textures === 'string') throw fail(`has ${textures}`);
  const sourceLine = given('src') || typeof props.sourceLine !== 'number' ? undefined : props.sourceLine;
  const { msl, key } = compileShader(source, uniforms, textures.map(t => t.name), where, sourceLine);
  const out: Record<string, unknown> = { msl, key, uniforms, textures };
  for (const name of ['width', 'height'] as const) if (given(name)) out[name] = Number(props[name]);
  if (given('aspect')) out.aspect = aspectOf(props.aspect);
  out.animate = given('animate') ? props.animate === true || props.animate === 'true' : readsTime(source);
  out.pointer = readsPointer(source);
  if (given('id')) out.id = props.id;
  return out;
}

/** A width over a height: a number, or `"16:9"` / `"16/9"`; undefined when it is not a positive one. */
export function aspectOf(value: unknown): number | undefined {
  let a: number;
  if (typeof value === 'number') a = value;
  else if (typeof value === 'string') {
    const parts = /^\s*(\d+(?:\.\d+)?)\s*[:/]\s*(\d+(?:\.\d+)?)\s*$/.exec(value);
    a = parts ? Number(parts[1]) / Number(parts[2]) : /^\s*\d+(?:\.\d+)?\s*$/.test(value) ? Number(value) : NaN;
  } else return undefined;
  return Number.isFinite(a) && a >= 0.01 && a <= 100 ? Math.round(a * 1e6) / 1e6 : undefined;
}

/** The elements whose live value a `'@id'` uniform reads, and the elements a `'@id'` texture shows. */
export const UNIFORM_SOURCES: ReadonlySet<string> = new Set(['Slider', 'NumberField', 'Toggle', 'Checkbox', 'Video']);
export const TEXTURE_SOURCES: ReadonlySet<string> = new Set(['Video', 'Image', 'Shader']);

/** A node of the sent tree, as `resolveRefs` reads it (tree.ts `UiNode`). */
interface RefNode { type: string; props: Record<string, unknown>; children: RefNode[] }

/** The value the app starts a `'@id'` uniform at: the element's own value as the tree has it (the app reads it live after). */
function liveValue(node: RefNode): number {
  const p = node.props;
  switch (node.type) {
    case 'Toggle': return p.value === true ? 1 : 0;
    case 'Checkbox': return p.checked === true ? 1 : 0;
    case 'Video': return typeof p.currentTime === 'number' ? p.currentTime : 0;
    default: return typeof p.value === 'number' ? p.value : 0;
  }
}

/**
 * Every `'@id'` of a cell's tree checked and resolved, over the whole tree (order does not matter): an id said twice, a ref to no id,
 * a uniform's to what has no live number, a texture's to what is no picture are errors (the first is returned with the node it is
 * about, for its line; `where` says a node's place); an Image texture becomes kind `image` with the Image's src, and a uniform's value
 * is the element's now.
 */
export function resolveRefs(roots: readonly RefNode[], where: (node: RefNode) => string | undefined = () => undefined): { error: TypeError; node: RefNode } | undefined {
  const at = (node: RefNode) => { const w = where(node); return w ? ` (${w})` : ''; };
  const ids = new Map<string, RefNode>();
  const shaders: RefNode[] = [];
  const walk = (node: RefNode): { error: TypeError; node: RefNode } | undefined => {
    const id = node.props.id;
    if (typeof id === 'string') {
      const before = ids.get(id);
      if (before) return { node, error: new TypeError(`<${node.type} id=${JSON.stringify(id)}>${at(node)} has the id ${JSON.stringify(id)} ${before.type === node.type ? 'another' : `${/^[AEIOU]/.test(before.type) ? 'an' : 'a'}`} ${before.type} has${at(before)}: each id names one element (in an {#each}, make it from the item, id={'clip-' + i})`) };
      ids.set(id, node);
    }
    if (node.type === 'Shader') shaders.push(node);
    for (const child of node.children) { const found = walk(child); if (found) return found; }
    return undefined;
  };
  for (const root of roots) { const found = walk(root); if (found) return found; }
  const known = () => ids.size ? `the ids of this cell are ${[...ids.entries()].map(([id, n]) => `${id} (${n.type})`).join(', ')}` : 'no element of this cell has an id; give the one it means id="..."';
  for (const shader of shaders) {
    for (const u of shader.props.uniforms as WireUniform[]) {
      if (u.from === undefined) continue;
      const target = ids.get(u.from);
      if (!target) return { node: shader, error: new TypeError(`<Shader> uniforms.${u.name} is '@${u.from}'${at(shader)}, and no element has id="${u.from}": ${known()}`) };
      if (!UNIFORM_SOURCES.has(target.type)) return { node: shader, error: new TypeError(`<Shader> uniforms.${u.name} is '@${u.from}'${at(shader)}, which is ${/^[AEIOU]/.test(target.type) ? 'an' : 'a'} ${target.type}: a uniform reads the live value of a ${[...UNIFORM_SOURCES].join(', ')}`) };
      u.value = [liveValue(target), 0, 0, 0];
    }
    const textures = shader.props.textures as WireTexture[];
    for (const [i, t] of textures.entries()) {
      if (t.kind !== 'ref') continue;
      const target = ids.get(t.ref!);
      if (!target) return { node: shader, error: new TypeError(`<Shader> textures.${t.name} is '@${t.ref}'${at(shader)}, and no element has id="${t.ref}": ${known()}`) };
      if (!TEXTURE_SOURCES.has(target.type)) return { node: shader, error: new TypeError(`<Shader> textures.${t.name} is '@${t.ref}'${at(shader)}, which is ${/^[AEIOU]/.test(target.type) ? 'an' : 'a'} ${target.type}: a texture shows a Video, an Image or a Shader (its own id: its last frame)`) };
      // The app samples an Image's file as a texture of its own; only a Video or a Shader is a live surface it refers to.
      if (target.type === 'Image') textures[i] = { name: t.name, kind: 'image', src: String(target.props.src) };
    }
  }
  return undefined;
}
