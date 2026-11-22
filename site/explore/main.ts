/**
 * Thin WebGPU + WebSocket + Berkeley Mono Ligature Atlas host shim for `explore_core.wasm`.
 *
 * 99% of Blaze Explore (Flecs-style Relational ECS, shared `telemetry.rs` binary ingestion,
 * layout, hit-testing, Flame Graph, Code Map, CPU Topology, Hot Spots, Heap Diff, Builds,
 * Advisor, Read-Only Post-IDE Inspector, 2-Zone Hover Cards) runs in pure Rust WASM and
 * renders on ONE full-viewport WebGPU canvas with zero DOM UI chrome.
 */

const DEFAULT_VIEWER = 'http://127.0.0.1:41937';
const MAX_RECTS = 32_768;
const MAX_GLYPHS = 49_152;
const ATLAS_COLS = 16;
const ATLAS_ROWS = 8;
const CELL_W = 24;
const CELL_H = 40;

interface WasmExports {
	memory: WebAssembly.Memory;
	init(advanceRatio: number, webgpuActive: number): void;
	io_buf_ptr(): number;
	io_buf_cap(): number;
	ingest_binary(byteLen: number, nowMs: number): number;
	ingest_json(byteLen: number): void;
	ingest_topology(byteLen: number): void;
	ingest_source(byteLen: number): void;
	ingest_resources(byteLen: number): void;
	ingest_def(byteLen: number): void;
	active_pid(): number;
	set_connection(connected: number, detailLen: number): void;
	on_pointer_move(x: number, y: number): void;
	on_pointer_down(x: number, y: number, shift: number): void;
	on_pointer_up(x: number, y: number): void;
	on_dblclick(x: number, y: number): void;
	on_wheel(x: number, y: number, dx: number, dy: number, ctrlOrMeta: number): void;
	on_key_down(code: number): void;
	render_frame(nowMs: number, cssW: number, cssH: number): void;
	rects_ptr(): number;
	rects_count(): number;
	glyphs_ptr(): number;
	glyphs_count(): number;
	ov_rects_ptr(): number;
	ov_rects_count(): number;
	ov_glyphs_ptr(): number;
	ov_glyphs_count(): number;
	cmd_buf_ptr(): number;
	take_commands_len(): number;
}

const SDF_WGSL = /* wgsl */ `
struct Viewport { size: vec2<f32> };
struct RectInst {
  rect: vec4<f32>,
  color: vec4<f32>,
  style: vec4<f32>,
};
struct VsOut {
  @builtin(position) pos: vec4<f32>,
  @location(0) px: vec2<f32>,
  @location(1) rect: vec4<f32>,
  @location(2) color: vec4<f32>,
  @location(3) style: vec4<f32>,
};
@group(0) @binding(0) var<uniform> vp: Viewport;
@group(0) @binding(1) var<storage, read> insts: array<RectInst>;

@vertex
fn vs_main(@builtin(vertex_index) vid: u32, @builtin(instance_index) iid: u32) -> VsOut {
  let inst = insts[iid];
  var pmin = inst.rect.xy;
  var pmax = inst.rect.zw;
  if (inst.style.w > 0.5 && inst.style.w < 1.5) {
    let pad = inst.style.x + 2.0;
    pmin = min(inst.rect.xy, inst.rect.zw) - vec2<f32>(pad, pad);
    pmax = max(inst.rect.xy, inst.rect.zw) + vec2<f32>(pad, pad);
  } else if (inst.style.w > 2.5) {
    let pad = inst.style.y + 1.5;
    pmin -= vec2<f32>(pad, pad);
    pmax += vec2<f32>(pad, pad);
  } else {
    pmin -= vec2<f32>(1.0, 1.0);
    pmax += vec2<f32>(1.0, 1.0);
  }
  var uv = array<vec2<f32>, 6>(
    vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
  );
  let p = mix(pmin, pmax, uv[vid]);
  let ndc = vec2<f32>((p.x / vp.size.x) * 2.0 - 1.0, 1.0 - (p.y / vp.size.y) * 2.0);
  var out: VsOut;
  out.pos = vec4<f32>(ndc, 0.0, 1.0);
  out.px = p;
  out.rect = inst.rect;
  out.color = inst.color;
  out.style = inst.style;
  return out;
}

fn sd_round_box(p: vec2<f32>, center: vec2<f32>, half_ext: vec2<f32>, r: f32) -> f32 {
  let q = abs(p - center) - half_ext + vec2<f32>(r, r);
  return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
}

fn sd_segment(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>) -> f32 {
  let pa = p - a;
  let ba = b - a;
  let h = clamp(dot(pa, ba) / max(dot(ba, ba), 0.0001), 0.0, 1.0);
  return length(pa - ba * h);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
  let kind = in.style.w;
  var alpha = 1.0;
  if (kind < 0.5) {
    let center = (in.rect.xy + in.rect.zw) * 0.5;
    let half_ext = (in.rect.zw - in.rect.xy) * 0.5;
    let d = sd_round_box(in.px, center, half_ext, in.style.x);
    alpha = clamp(0.5 - d, 0.0, 1.0);
  } else if (kind < 1.5) {
    let d = sd_segment(in.px, in.rect.xy, in.rect.zw) - in.style.x * 0.5;
    alpha = clamp(0.5 - d, 0.0, 1.0);
  } else if (kind < 2.5) {
    let center = (in.rect.xy + in.rect.zw) * 0.5;
    let d = length(in.px - center) - in.style.x;
    alpha = clamp(0.5 - d, 0.0, 1.0);
  } else {
    let center = (in.rect.xy + in.rect.zw) * 0.5;
    let half_ext = (in.rect.zw - in.rect.xy) * 0.5;
    let d = sd_round_box(in.px, center, half_ext, in.style.x);
    let bw = max(in.style.y, 1.0);
    let bd = abs(d + bw * 0.5) - bw * 0.5;
    alpha = clamp(0.5 - bd, 0.0, 1.0);
  }
  if (alpha <= 0.002) { discard; }
  return vec4<f32>(in.color.rgb, in.color.a * alpha);
}
`;

const GLYPH_WGSL = /* wgsl */ `
struct Viewport { size: vec2<f32> };
struct GlyphInst {
  rect: vec4<f32>,
  uv: vec4<f32>,
  color: vec4<f32>,
};
struct VsOut {
  @builtin(position) pos: vec4<f32>,
  @location(0) uv: vec2<f32>,
  @location(1) color: vec4<f32>,
};
@group(0) @binding(0) var<uniform> vp: Viewport;
@group(0) @binding(1) var<storage, read> glyphs: array<GlyphInst>;
@group(0) @binding(2) var atlas_samp: sampler;
@group(0) @binding(3) var atlas_tex: texture_2d<f32>;

@vertex
fn vs_main(@builtin(vertex_index) vid: u32, @builtin(instance_index) iid: u32) -> VsOut {
  let g = glyphs[iid];
  var c = array<vec2<f32>, 6>(
    vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
  );
  let k = c[vid];
  let p = mix(g.rect.xy, g.rect.zw, k);
  let ndc = vec2<f32>((p.x / vp.size.x) * 2.0 - 1.0, 1.0 - (p.y / vp.size.y) * 2.0);
  var out: VsOut;
  out.pos = vec4<f32>(ndc, 0.0, 1.0);
  out.uv = mix(g.uv.xy, g.uv.zw, k);
  out.color = g.color;
  return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
  let a = textureSample(atlas_tex, atlas_samp, in.uv).a;
  if (a <= 0.01) { discard; }
  return vec4<f32>(in.color.rgb, in.color.a * a);
}
`;

function resolveOrigin(): string {
	const raw = new URLSearchParams(window.location.search).get('viewer');
	if (!raw) return DEFAULT_VIEWER;
	try {
		const u = new URL(raw);
		if ((u.protocol === 'http:' || u.protocol === 'https:') && (u.hostname === '127.0.0.1' || u.hostname === 'localhost')) {
			return u.origin;
		}
	} catch {}
	return DEFAULT_VIEWER;
}

/** Rasterizes Berkeley Mono with OpenType ligatures (`liga` + `calt`) into a 16x8 HiDPI GPU atlas. */
async function buildBerkeleyMonoAtlas(): Promise<{ canvas: HTMLCanvasElement; advanceRatio: number }> {
	try {
		await document.fonts.load('500 28px "Berkeley Mono"');
		await document.fonts.ready;
	} catch {}

	const c = document.createElement('canvas');
	c.width = ATLAS_COLS * CELL_W;
	c.height = ATLAS_ROWS * CELL_H;
	c.style.cssText =
		'position:fixed;left:-9999px;top:-9999px;font-family:"Berkeley Mono",ui-monospace,monospace;font-feature-settings:"liga" 1,"calt" 1;font-variant-ligatures:contextual common-ligatures;';
	document.body.appendChild(c);

	const ctx = c.getContext('2d')!;
	const fontPx = 28;
	ctx.font = `500 ${fontPx}px "Berkeley Mono", ui-monospace, SFMono-Regular, Menlo, monospace`;
	ctx.fillStyle = '#ffffff';
	ctx.textAlign = 'center';
	ctx.textBaseline = 'alphabetic';

	const advanceRatio = ctx.measureText('M').width / fontPx;
	const baseline = 29;

	// Rows 0..5: ASCII 32..=126
	for (let i = 0; i <= 94; i++) {
		const col = i % ATLAS_COLS;
		const row = Math.floor(i / ATLAS_COLS);
		ctx.fillText(String.fromCharCode(32 + i), col * CELL_W + CELL_W * 0.5, row * CELL_H + baseline);
	}
	// Row 6: Crisp symbols (`·`, `×`, `µ`, `→`, `Δ`, `…`, `—`, `↵`, `↗`, `←`)
	const syms = ['·', '×', 'µ', '→', 'Δ', '…', '—', '↵', '↗', '←'];
	for (let i = 0; i < syms.length; i++) {
		ctx.fillText(syms[i], i * CELL_W + CELL_W * 0.5, 6 * CELL_H + baseline);
	}
	// Row 7: Double-width Berkeley Mono OpenType ligatures (`->`, `=>`, `::`, `!=`, `==`, `<=`, `>=`, `..`)
	const ligs = ['->', '=>', '::', '!=', '==', '<=', '>=', '..'];
	for (let i = 0; i < ligs.length; i++) {
		ctx.fillText(ligs[i], i * 2 * CELL_W + CELL_W, 7 * CELL_H + baseline);
	}

	c.remove();
	return { canvas: c, advanceRatio };
}

async function boot(): Promise<void> {
	const canvas = document.getElementById('gpu') as HTMLCanvasElement;
	const [wasmRes, atlas] = await Promise.all([fetch('explore/explore_core.wasm'), buildBerkeleyMonoAtlas()]);
	const { instance } = await WebAssembly.instantiate(await wasmRes.arrayBuffer(), {});
	const wasm = instance.exports as unknown as WasmExports;

	const enc = new TextEncoder();
	const dec = new TextDecoder();
	const writeIo = (bytes: Uint8Array): number => {
		const len = Math.min(bytes.byteLength, wasm.io_buf_cap());
		new Uint8Array(wasm.memory.buffer, wasm.io_buf_ptr(), len).set(bytes.subarray(0, len));
		return len;
	};
	const writeIoStr = (s: string): number => writeIo(enc.encode(s));

	// Boot WebGPU (2 pipelines: SDF geometry + Berkeley Mono ligature glyph atlas)
	const nav = navigator as unknown as { gpu?: any };
	const adapter = await nav.gpu?.requestAdapter();
	const device = await adapter?.requestDevice();
	const gpuCtx = device ? (canvas.getContext('webgpu') as any) : null;
	const webgpuOk = Boolean(device && gpuCtx);
	wasm.init(atlas.advanceRatio, webgpuOk ? 1 : 0);

	let sdfPipe: any, glyphPipe: any, sdfBG: any, glyphBG: any, vpBuf: any, rectBuf: any, glyphBuf: any;
	if (webgpuOk) {
		const fmt = nav.gpu.getPreferredCanvasFormat();
		gpuCtx.configure({ device, format: fmt, alphaMode: 'premultiplied' });

		const blend = {
			color: { srcFactor: 'src-alpha', dstFactor: 'one-minus-src-alpha', operation: 'add' },
			alpha: { srcFactor: 'one', dstFactor: 'one-minus-src-alpha', operation: 'add' },
		};
		const sdfMod = device.createShaderModule({ code: SDF_WGSL });
		const glyphMod = device.createShaderModule({ code: GLYPH_WGSL });

		sdfPipe = device.createRenderPipeline({
			layout: 'auto',
			vertex: { module: sdfMod, entryPoint: 'vs_main' },
			fragment: { module: sdfMod, entryPoint: 'fs_main', targets: [{ format: fmt, blend }] },
			primitive: { topology: 'triangle-list' },
		});
		glyphPipe = device.createRenderPipeline({
			layout: 'auto',
			vertex: { module: glyphMod, entryPoint: 'vs_main' },
			fragment: { module: glyphMod, entryPoint: 'fs_main', targets: [{ format: fmt, blend }] },
			primitive: { topology: 'triangle-list' },
		});

		vpBuf = device.createBuffer({ size: 16, usage: 0x0040 | 0x0008 });
		rectBuf = device.createBuffer({ size: MAX_RECTS * 48, usage: 0x0080 | 0x0008 });
		glyphBuf = device.createBuffer({ size: MAX_GLYPHS * 48, usage: 0x0080 | 0x0008 });

		const tex = device.createTexture({
			size: [atlas.canvas.width, atlas.canvas.height, 1],
			format: 'rgba8unorm',
			usage: 0x04 | 0x02 | 0x10,
		});
		device.queue.copyExternalImageToTexture({ source: atlas.canvas }, { texture: tex }, [
			atlas.canvas.width,
			atlas.canvas.height,
		]);
		const samp = device.createSampler({ magFilter: 'linear', minFilter: 'linear' });

		sdfBG = device.createBindGroup({
			layout: sdfPipe.getBindGroupLayout(0),
			entries: [
				{ binding: 0, resource: { buffer: vpBuf } },
				{ binding: 1, resource: { buffer: rectBuf } },
			],
		});
		glyphBG = device.createBindGroup({
			layout: glyphPipe.getBindGroupLayout(0),
			entries: [
				{ binding: 0, resource: { buffer: vpBuf } },
				{ binding: 1, resource: { buffer: glyphBuf } },
				{ binding: 2, resource: samp },
				{ binding: 3, resource: tex.createView() },
			],
		});
	}

	// Input forwarding (100% handled in Rust WASM!)
	canvas.addEventListener('pointermove', (e) => wasm.on_pointer_move(e.clientX, e.clientY));
	canvas.addEventListener('pointerdown', (e) => {
		if (e.button !== 0) return;
		canvas.setPointerCapture(e.pointerId);
		wasm.on_pointer_down(e.clientX, e.clientY, e.shiftKey ? 1 : 0);
	});
	const up = (e: PointerEvent) => wasm.on_pointer_up(e.clientX, e.clientY);
	canvas.addEventListener('pointerup', up);
	canvas.addEventListener('pointercancel', up);
	canvas.addEventListener('pointerleave', () => wasm.on_pointer_move(-1, -1));
	canvas.addEventListener('dblclick', (e) => wasm.on_dblclick(e.clientX, e.clientY));
	canvas.addEventListener(
		'wheel',
		(e) => {
			e.preventDefault();
			wasm.on_wheel(e.clientX, e.clientY, e.deltaX, e.deltaY, e.ctrlKey || e.metaKey ? 1 : 0);
		},
		{ passive: false },
	);
	window.addEventListener('keydown', (e) => {
		const code = e.key === 'Escape' ? 27 : e.key.length === 1 ? e.key.charCodeAt(0) : 0;
		if (code) wasm.on_key_down(code);
	});

	// Loopback WebSocket + Topology + Source loader (direct into WASM linear memory!)
	const viewerOrigin = resolveOrigin();
	const origins = window.location.origin !== viewerOrigin ? [viewerOrigin, window.location.origin] : [viewerOrigin];
	const fetchApi = async (path: string): Promise<string | null> => {
		for (const base of origins) {
			try {
				const r = await fetch(`${base}${path}`, { cache: 'no-store' });
				if (r.ok) return await r.text();
			} catch {}
		}
		return null;
	};

	void fetchApi('/api/topology').then((txt) => {
		if (txt) wasm.ingest_topology(writeIoStr(txt));
	});
	const pollResources = () => {
		const pid = wasm.active_pid();
		if (pid > 0) {
			void fetchApi(`/api/resources?pid=${pid}`).then((txt) => {
				if (txt) wasm.ingest_resources(writeIoStr(txt));
			});
		}
	};
	setTimeout(pollResources, 400);
	setInterval(pollResources, 2500);

	let ws: WebSocket | null = null;
	const connectWs = () => {
		const url = `${viewerOrigin.replace(/^http/, 'ws')}/ws`;
		ws = new WebSocket(url);
		ws.binaryType = 'arraybuffer';
		ws.onopen = () => wasm.set_connection(1, writeIoStr(`${url} (binary)`));
		ws.onmessage = (ev) => {
			if (ev.data instanceof ArrayBuffer) {
				const dv = new DataView(ev.data);
				if (ev.data.byteLength < 8 || dv.getUint8(0) !== 0x42 || dv.getUint8(1) !== 1) return;
				const hdrLen = (4 + dv.getUint16(2, true) + 7) & ~7;
				const raw = new Uint8Array(ev.data, hdrLen);
				if (raw.byteLength >= 64) wasm.ingest_binary(writeIo(raw), Date.now());
			} else if (typeof ev.data === 'string') {
				wasm.ingest_json(writeIoStr(ev.data));
			}
		};
		ws.onclose = () => {
			wasm.set_connection(0, writeIoStr(`reconnecting ${viewerOrigin}…`));
			setTimeout(connectWs, 800);
		};
		ws.onerror = () => ws?.close();
	};
	connectWs();

	// Single-tab smart focus broadcast
	const ch = 'BroadcastChannel' in window ? new BroadcastChannel('blaze-explore') : null;
	ch?.postMessage({ type: 'explore-tab-open', ts: Date.now() });

	// 60/120 FPS Zero-DOM WebGPU Frame Loop
	const vpData = new Float32Array(2);
	const frame = () => {
		const dpr = Math.min(window.devicePixelRatio || 1, 2);
		const cssW = Math.max(1, window.innerWidth);
		const cssH = Math.max(1, window.innerHeight);
		const pxW = Math.round(cssW * dpr);
		const pxH = Math.round(cssH * dpr);
		if (canvas.width !== pxW || canvas.height !== pxH) {
			canvas.width = pxW;
			canvas.height = pxH;
		}

		wasm.render_frame(Date.now(), cssW, cssH);

		if (webgpuOk) {
			const rc = Math.min(wasm.rects_count(), MAX_RECTS - 512);
			const gc = Math.min(wasm.glyphs_count(), MAX_GLYPHS - 2048);
			const ovRc = Math.min(wasm.ov_rects_count(), MAX_RECTS - rc);
			const ovGc = Math.min(wasm.ov_glyphs_count(), MAX_GLYPHS - gc);
			vpData[0] = cssW;
			vpData[1] = cssH;
			device.queue.writeBuffer(vpBuf, 0, vpData);
			if (rc > 0) device.queue.writeBuffer(rectBuf, 0, new Uint8Array(wasm.memory.buffer, wasm.rects_ptr(), rc * 48));
			if (ovRc > 0) device.queue.writeBuffer(rectBuf, rc * 48, new Uint8Array(wasm.memory.buffer, wasm.ov_rects_ptr(), ovRc * 48));
			if (gc > 0) device.queue.writeBuffer(glyphBuf, 0, new Uint8Array(wasm.memory.buffer, wasm.glyphs_ptr(), gc * 48));
			if (ovGc > 0) device.queue.writeBuffer(glyphBuf, gc * 48, new Uint8Array(wasm.memory.buffer, wasm.ov_glyphs_ptr(), ovGc * 48));

			const encGpu = device.createCommandEncoder();
			const pass = encGpu.beginRenderPass({
				colorAttachments: [
					{
						view: gpuCtx.getCurrentTexture().createView(),
						clearValue: { r: 0.063, g: 0.063, b: 0.067, a: 1.0 },
						loadOp: 'clear',
						storeOp: 'store',
					},
				],
			});
			// Z-Pass 1: Base UI geometry + text
			if (rc > 0) {
				pass.setPipeline(sdfPipe);
				pass.setBindGroup(0, sdfBG);
				pass.draw(6, rc, 0, 0);
			}
			if (gc > 0) {
				pass.setPipeline(glyphPipe);
				pass.setBindGroup(0, glyphBG);
				pass.draw(6, gc, 0, 0);
			}
			// Z-Pass 2: Floating Hover Card geometry + syntax-highlighted Rust source peek (zero bleed-through!)
			if (ovRc > 0) {
				pass.setPipeline(sdfPipe);
				pass.setBindGroup(0, sdfBG);
				pass.draw(6, ovRc, 0, rc);
			}
			if (ovGc > 0) {
				pass.setPipeline(glyphPipe);
				pass.setBindGroup(0, glyphBG);
				pass.draw(6, ovGc, 0, gc);
			}
			pass.end();
			device.queue.submit([encGpu.finish()]);
		}

		// Drain host commands from Rust WASM (`cursor:`, `source:`, `toggle:`, `open:`)
		const cmdLen = wasm.take_commands_len();
		if (cmdLen > 0) {
			const cmds = dec.decode(new Uint8Array(wasm.memory.buffer, wasm.cmd_buf_ptr(), cmdLen)).split('\n');
			for (const cmd of cmds) {
				if (cmd.startsWith('cursor:')) {
					const cur = cmd.slice(7);
					if (canvas.style.cursor !== cur) canvas.style.cursor = cur;
				} else if (cmd.startsWith('source:')) {
					const [file, line] = cmd.slice(7).split('|');
					void fetchApi(`/api/source?file=${encodeURIComponent(file)}&line=${line}`).then((txt) => {
						if (txt) wasm.ingest_source(writeIoStr(txt));
					});
				} else if (cmd.startsWith('def:')) {
					const [key, ident, from, prefix, imp, kind] = cmd.slice(4).split('\t');
					const qs = new URLSearchParams({
						key: key || ident || '',
						ident: ident || '',
						from: from || '',
						prefix: prefix || '',
						import: imp || '',
						kind: kind || 'any',
					});
					void fetchApi(`/api/def?${qs.toString()}`).then((txt) => {
						if (txt) wasm.ingest_def(writeIoStr(txt));
					});
				} else if (cmd.startsWith('toggle:')) {
					const [run, slot, kind] = cmd.slice(7).split('|');
					if (ws && ws.readyState === WebSocket.OPEN) {
						ws.send(JSON.stringify({ type: 'toggle', run, slot: Number(slot), kind }));
					}
				} else if (cmd.startsWith('open:')) {
					window.open(cmd.slice(5), '_blank');
				}
			}
		}

		requestAnimationFrame(frame);
	};
	requestAnimationFrame(frame);
}

void boot();
