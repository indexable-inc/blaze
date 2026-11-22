/**
 * Full-bleed Tracy-style WebGPU trace renderer with paired HiDPI label overlay (`#gpu-canvas` + `#gpu-labels`).
 *
 * Ergonomics:
 *   - Trackpad pinch/squeeze (`wheel` + `ctrlKey`) or vertical wheel: cursor-anchored zoom (`0.2 ms` .. `60 s`).
 *   - Two-finger horizontal pan (`wheel` `deltaX`) or pointer drag: smooth pan, auto-detaches live follow.
 *   - Direct text inside every bar wide enough to read (`>= 32px` shows name, `>= 115px` shows `name · 4.2ms`).
 *   - Hover: crisp vertical crosshair, brightened bar, floating tooltip.
 *   - Double-click: reset zoom & snap back to live tail.
 */

export interface TimelineSample {
	startMs: number;
	endMs: number;
	trackKey: string; // 't:1', 't:2', 'heap', 'io'
	depth: number;
	color: [number, number, number, number];
	label: string;
	durationText: string;
	detail: string;
}

export interface TimelineViewportInfo {
	spanMs: number;
	following: boolean;
	selectionMs: [number, number] | null;
}

const MAX_INSTANCES = 16384;
const FLOATS_PER_INST = 8; // [x0, y0, x1, y1, r, g, b, a]
const MIN_SPAN_MS = 0.2;
const MAX_SPAN_MS = 60_000;
const DEFAULT_SPAN_MS = 3_500;
const GUTTER_W = 88;

const WGSL_SHADER = /* wgsl */ `
struct Inst {
  rect: vec4<f32>,
  color: vec4<f32>,
};
struct VsOut {
  @builtin(position) pos: vec4<f32>,
  @location(0) color: vec4<f32>,
};
@group(0) @binding(0) var<storage, read> insts: array<Inst>;

@vertex
fn vs_main(@builtin(vertex_index) vid: u32, @builtin(instance_index) iid: u32) -> VsOut {
  let inst = insts[iid];
  var corners = array<vec2<f32>, 6>(
    vec2<f32>(inst.rect.x, inst.rect.y),
    vec2<f32>(inst.rect.z, inst.rect.y),
    vec2<f32>(inst.rect.z, inst.rect.w),
    vec2<f32>(inst.rect.x, inst.rect.y),
    vec2<f32>(inst.rect.z, inst.rect.w),
    vec2<f32>(inst.rect.x, inst.rect.w),
  );
  var out: VsOut;
  out.pos = vec4<f32>(corners[vid], 0.0, 1.0);
  out.color = inst.color;
  return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
  return in.color;
}
`;

interface GpuLike {
	requestAdapter(): Promise<GpuAdapterLike | null>;
	getPreferredCanvasFormat(): string;
}
interface GpuAdapterLike {
	requestDevice(): Promise<GpuDeviceLike>;
}
interface GpuDeviceLike {
	createShaderModule(desc: { code: string }): unknown;
	createRenderPipeline(desc: unknown): unknown;
	createBuffer(desc: { size: number; usage: number }): GpuBufferLike;
	createBindGroup(desc: unknown): unknown;
	createCommandEncoder(): GpuCommandEncoderLike;
	queue: { writeBuffer(buf: GpuBufferLike, off: number, data: Float32Array, dataOff: number, size: number): void; submit(cmds: unknown[]): void };
}
interface GpuBufferLike {
	destroy(): void;
}
interface GpuCommandEncoderLike {
	beginRenderPass(desc: unknown): GpuPassLike;
	finish(): unknown;
}
interface GpuPassLike {
	setPipeline(p: unknown): void;
	setBindGroup(i: number, g: unknown): void;
	draw(verts: number, instances: number): void;
	end(): void;
}
interface GpuCanvasCtxLike {
	configure(desc: { device: GpuDeviceLike; format: string; alphaMode: string }): void;
	getCurrentTexture(): { createView(): unknown };
}

interface TrackLayout {
	key: string;
	title: string;
	accent: string;
	yTop: number;
	height: number;
	barH: number;
	maxDepth: number;
}

interface LabelDraw {
	x: number;
	y: number;
	w: number;
	h: number;
	text: string;
	bright: boolean;
}

function niceStepMs(spanMs: number): number {
	const raw = spanMs / 7;
	const pow = Math.pow(10, Math.floor(Math.log10(Math.max(raw, 0.0001))));
	const norm = raw / pow;
	if (norm <= 1) return pow;
	if (norm <= 2) return 2 * pow;
	if (norm <= 5) return 5 * pow;
	return 10 * pow;
}

export function fmtSpan(ms: number): string {
	if (ms < 1) return `${(ms * 1000).toFixed(0)} µs`;
	if (ms < 1000) return `${ms < 10 ? ms.toFixed(1) : ms.toFixed(0)} ms`;
	return `${(ms / 1000).toFixed(ms < 10_000 ? 1 : 0)} s`;
}

export class GpuTimeline {
	private samples: TimelineSample[] = [];
	private threadIds = new Set<string>();
	private backend: 'webgpu' | '2d' = '2d';
	private device: GpuDeviceLike | null = null;
	private gpuCtx: GpuCanvasCtxLike | null = null;
	private pipeline: unknown = null;
	private bindGroup: unknown = null;
	private instBuf: GpuBufferLike | null = null;
	private ctx2d: CanvasRenderingContext2D | null = null;
	private labelCtx: CanvasRenderingContext2D | null = null;
	private staging = new Float32Array(MAX_INSTANCES * FLOATS_PER_INST);

	private viewSpanMs = DEFAULT_SPAN_MS;
	private viewEndMs = Date.now();
	private following = true;
	private latestTsMs = Date.now();

	private hoverX: number | null = null;
	private hoverY: number | null = null;
	private dragging = false;
	private selecting = false;
	private selAnchorMs = 0;
	private selectionMs: [number, number] | null = null;
	private dragStartX = 0;
	private dragStartEndMs = 0;
	private rafPending = false;
	private lastHoveredSample: TimelineSample | null = null;

	constructor(
		private readonly canvas: HTMLCanvasElement,
		private readonly labelCanvas: HTMLCanvasElement,
		private readonly rulerEl: HTMLElement,
		private readonly tooltipEl: HTMLElement,
		private readonly onBackend: (kind: 'webgpu' | '2d') => void,
		private readonly onViewport: (info: TimelineViewportInfo) => void,
		private readonly onSelectSample?: (sample: TimelineSample) => void,
	) {
		this.labelCtx = this.labelCanvas.getContext('2d');
		this.bindEvents();
		void this.init();
	}

	private emitViewport(): void {
		this.onViewport({ spanMs: this.viewSpanMs, following: this.following, selectionMs: this.selectionMs });
	}

	clearSelection(): void {
		this.selectionMs = null;
		this.emitViewport();
		this.schedule();
	}

	getSelection(): [number, number] | null {
		return this.selectionMs;
	}

	private schedule(): void {
		if (this.rafPending) return;
		this.rafPending = true;
		requestAnimationFrame(() => {
			this.rafPending = false;
			this.render();
		});
	}

	followLive(): void {
		this.following = true;
		this.viewEndMs = Math.max(Date.now(), this.latestTsMs);
		this.emitViewport();
		this.schedule();
	}

	zoomBy(factor: number, anchorNorm = 0.5): void {
		const liveEdge = Math.max(Date.now(), this.latestTsMs);
		const curEnd = this.following ? liveEdge : this.viewEndMs;
		const prevSpan = this.viewSpanMs;
		const nextSpan = Math.max(MIN_SPAN_MS, Math.min(MAX_SPAN_MS, prevSpan * factor));
		const anchorTs = curEnd - (1 - anchorNorm) * prevSpan;
		this.viewSpanMs = nextSpan;
		this.viewEndMs = Math.min(liveEdge, anchorTs + (1 - anchorNorm) * nextSpan);
		this.following = liveEdge - this.viewEndMs < nextSpan * 0.015;
		this.emitViewport();
		this.schedule();
	}

	private bindEvents(): void {
		const el = this.canvas;

		el.addEventListener(
			'wheel',
			(e) => {
				e.preventDefault();
				const rect = el.getBoundingClientRect();
				const plotW = Math.max(1, rect.width - GUTTER_W);
				const plotX = Math.max(0, Math.min(plotW, e.clientX - rect.left - GUTTER_W));
				const normX = plotX / plotW;

				const isPinch = e.ctrlKey || e.metaKey;
				const isHorizPan = !isPinch && Math.abs(e.deltaX) > Math.abs(e.deltaY) * 0.55;

				if (isHorizPan) {
					const shiftMs = (e.deltaX / plotW) * this.viewSpanMs;
					const liveEdge = Math.max(Date.now(), this.latestTsMs);
					const curEnd = this.following ? liveEdge : this.viewEndMs;
					this.viewEndMs = Math.min(liveEdge, curEnd + shiftMs);
					this.following = liveEdge - this.viewEndMs < this.viewSpanMs * 0.015;
					this.emitViewport();
					this.schedule();
					return;
				}

				const sens = isPinch ? 0.014 : 0.003;
				this.zoomBy(Math.exp(e.deltaY * sens), normX);
			},
			{ passive: false },
		);

		el.addEventListener('pointerdown', (e) => {
			if (e.button !== 0) return;
			const rect = el.getBoundingClientRect();
			const plotW = Math.max(1, rect.width - GUTTER_W);
			const liveEdge = Math.max(Date.now(), this.latestTsMs);
			const curEnd = this.following ? liveEdge : this.viewEndMs;
			const viewStart = curEnd - this.viewSpanMs;

			// Shift+drag selects a YourKit/Perfetto time range [t0..t1] to filter Flame Graph & Hotspots!
			if (e.shiftKey) {
				this.selecting = true;
				const norm = Math.max(0, Math.min(1, (e.clientX - rect.left - GUTTER_W) / plotW));
				this.selAnchorMs = viewStart + norm * this.viewSpanMs;
				this.selectionMs = [this.selAnchorMs, this.selAnchorMs];
				this.following = false;
				this.viewEndMs = curEnd;
				el.setPointerCapture(e.pointerId);
				el.style.cursor = 'col-resize';
				this.emitViewport();
				this.schedule();
				return;
			}

			this.dragging = true;
			this.dragStartX = e.clientX;
			this.dragStartEndMs = curEnd;
			el.setPointerCapture(e.pointerId);
			el.style.cursor = 'grabbing';
		});

		el.addEventListener('pointermove', (e) => {
			const rect = el.getBoundingClientRect();
			this.hoverX = e.clientX - rect.left;
			this.hoverY = e.clientY - rect.top;
			const plotW = Math.max(1, rect.width - GUTTER_W);
			if (this.selecting) {
				const viewStart = this.viewEndMs - this.viewSpanMs;
				const norm = Math.max(0, Math.min(1, (this.hoverX - GUTTER_W) / plotW));
				const curTs = viewStart + norm * this.viewSpanMs;
				this.selectionMs = curTs >= this.selAnchorMs ? [this.selAnchorMs, curTs] : [curTs, this.selAnchorMs];
				this.emitViewport();
			} else if (this.dragging) {
				const dx = e.clientX - this.dragStartX;
				const liveEdge = Math.max(Date.now(), this.latestTsMs);
				this.viewEndMs = Math.min(liveEdge, this.dragStartEndMs - (dx / plotW) * this.viewSpanMs);
				this.following = liveEdge - this.viewEndMs < this.viewSpanMs * 0.015;
				this.emitViewport();
			}
			this.schedule();
		});

		const endDrag = (e: PointerEvent) => {
			if (this.selecting) {
				this.selecting = false;
				el.style.cursor = 'grab';
				if (this.selectionMs && this.selectionMs[1] - this.selectionMs[0] < this.viewSpanMs * 0.005) {
					this.selectionMs = null;
				}
				this.emitViewport();
				this.schedule();
				return;
			}
			if (!this.dragging) return;
			const moved = Math.abs(e.clientX - this.dragStartX);
			this.dragging = false;
			el.style.cursor = 'grab';
			if (moved < 4 && this.lastHoveredSample) {
				this.onSelectSample?.(this.lastHoveredSample);
			}
		};
		el.addEventListener('pointerup', endDrag);
		el.addEventListener('pointercancel', endDrag);
		el.addEventListener('pointerleave', () => {
			this.hoverX = null;
			this.hoverY = null;
			this.tooltipEl.hidden = true;
			this.schedule();
		});
		el.addEventListener('dblclick', () => {
			this.viewSpanMs = DEFAULT_SPAN_MS;
			this.followLive();
		});
	}

	private async init(): Promise<void> {
		const nav = navigator as unknown as { gpu?: GpuLike };
		if (nav.gpu) {
			try {
				const adapter = await nav.gpu.requestAdapter();
				if (adapter) {
					const device = await adapter.requestDevice();
					const ctx = this.canvas.getContext('webgpu') as unknown as GpuCanvasCtxLike | null;
					if (ctx) {
						const format = nav.gpu.getPreferredCanvasFormat();
						ctx.configure({ device, format, alphaMode: 'premultiplied' });
						const module = device.createShaderModule({ code: WGSL_SHADER });
						const pipeline = device.createRenderPipeline({
							layout: 'auto',
							vertex: { module, entryPoint: 'vs_main' },
							fragment: {
								module,
								entryPoint: 'fs_main',
								targets: [
									{
										format,
										blend: {
											color: { srcFactor: 'src-alpha', dstFactor: 'one-minus-src-alpha', operation: 'add' },
											alpha: { srcFactor: 'one', dstFactor: 'one-minus-src-alpha', operation: 'add' },
										},
									},
								],
							},
							primitive: { topology: 'triangle-list' },
						});
						const instBuf = device.createBuffer({
							size: MAX_INSTANCES * FLOATS_PER_INST * 4,
							usage: 0x0080 | 0x0008,
						});
						const bindGroup = device.createBindGroup({
							layout: (pipeline as { getBindGroupLayout(i: number): unknown }).getBindGroupLayout(0),
							entries: [{ binding: 0, resource: { buffer: instBuf } }],
						});
						this.device = device;
						this.gpuCtx = ctx;
						this.pipeline = pipeline;
						this.instBuf = instBuf;
						this.bindGroup = bindGroup;
						this.backend = 'webgpu';
						this.onBackend('webgpu');
						this.emitViewport();
						this.render();
						return;
					}
				}
			} catch {
				// Fall back cleanly to HiDPI Canvas2D.
			}
		}
		this.ctx2d = this.canvas.getContext('2d');
		this.backend = '2d';
		this.onBackend('2d');
		this.emitViewport();
		this.render();
	}

	push(sample: TimelineSample): void {
		if (sample.endMs > this.latestTsMs) this.latestTsMs = sample.endMs;
		if (sample.trackKey.startsWith('t:')) this.threadIds.add(sample.trackKey);
		this.samples.push(sample);
		if (this.samples.length > MAX_INSTANCES - 128) {
			this.samples.splice(0, this.samples.length - (MAX_INSTANCES - 128));
		}
	}

	clear(): void {
		this.samples.length = 0;
		this.threadIds.clear();
		this.followLive();
	}

	private buildTracks(cssH: number): TrackLayout[] {
		const threads = [...this.threadIds].sort();
		if (threads.length === 0) threads.push('t:1');

		const heapH = 38;
		const ioH = 34;
		const avail = Math.max(72, cssH - heapH - ioH);
		const perThread = Math.max(46, Math.floor(avail / threads.length));

		const tracks: TrackLayout[] = [];
		let y = 0;
		for (const key of threads) {
			const tid = key.slice(2);
			const maxDepth = perThread >= 74 ? 3 : 2;
			const barH = Math.min(22, Math.max(17, Math.floor((perThread - 10) / maxDepth) - 2));
			tracks.push({
				key,
				title: `Thread #${tid}`,
				accent: '#e0461a',
				yTop: y,
				height: perThread,
				barH,
				maxDepth,
			});
			y += perThread;
		}
		tracks.push({ key: 'heap', title: 'Heap', accent: '#3987e5', yTop: y, height: heapH, barH: 22, maxDepth: 1 });
		y += heapH;
		tracks.push({
			key: 'io',
			title: 'Locks / I/O',
			accent: '#ff7168',
			yTop: y,
			height: Math.max(ioH, cssH - y),
			barH: 20,
			maxDepth: 1,
		});
		return tracks;
	}

	render(): void {
		const dpr = Math.min(window.devicePixelRatio || 1, 2);
		const wrap = this.canvas.parentElement ?? this.canvas;
		const rect = wrap.getBoundingClientRect();
		const cssW = Math.max(1, Math.floor(rect.width));
		const cssH = Math.max(1, Math.floor(rect.height));
		const pxW = Math.round(cssW * dpr);
		const pxH = Math.round(cssH * dpr);
		if (this.canvas.width !== pxW || this.canvas.height !== pxH) {
			this.canvas.width = pxW;
			this.canvas.height = pxH;
		}
		if (this.labelCanvas.width !== pxW || this.labelCanvas.height !== pxH) {
			this.labelCanvas.width = pxW;
			this.labelCanvas.height = pxH;
		}

		const plotW = Math.max(1, cssW - GUTTER_W);
		const liveEdge = Math.max(Date.now(), this.latestTsMs);
		if (this.following) this.viewEndMs = liveEdge;
		const viewStartMs = this.viewEndMs - this.viewSpanMs;

		const tracks = this.buildTracks(cssH);
		const trackByKey = new Map<string, TrackLayout>();
		for (const tr of tracks) trackByKey.set(tr.key, tr);

		let count = 0;
		const toNdcX = (px: number) => -1 + (px / cssW) * 2;
		const toNdcY = (py: number) => 1 - (py / cssH) * 2;
		const pushPxRect = (x: number, y: number, w: number, h: number, r: number, g: number, b: number, a: number) => {
			if (count >= MAX_INSTANCES || w <= 0 || h <= 0) return;
			const o = count * FLOATS_PER_INST;
			this.staging[o] = toNdcX(x);
			this.staging[o + 1] = toNdcY(y + h);
			this.staging[o + 2] = toNdcX(x + w);
			this.staging[o + 3] = toNdcY(y);
			this.staging[o + 4] = r;
			this.staging[o + 5] = g;
			this.staging[o + 6] = b;
			this.staging[o + 7] = a;
			count += 1;
		};

		// Track lanes + dividers
		for (let i = 0; i < tracks.length; i++) {
			const tr = tracks[i];
			if (i % 2 === 1) pushPxRect(0, tr.yTop, cssW, tr.height, 0.092, 0.092, 0.098, 1.0);
			if (i > 0) pushPxRect(0, tr.yTop, cssW, 1, 0.16, 0.16, 0.175, 1.0);
		}

		// Left gutter
		pushPxRect(0, 0, GUTTER_W, cssH, 0.068, 0.068, 0.072, 1.0);
		pushPxRect(GUTTER_W - 1, 0, 1, cssH, 0.17, 0.17, 0.185, 1.0);

		// Adaptive time ruler ticks (aligned to plotX inside `.gpu-ruler`, which starts at GUTTER_W!)
		const stepMs = niceStepMs(this.viewSpanMs);
		const rulerSpans: string[] = [];
		for (let offset = 0; offset <= this.viewSpanMs; offset += stepMs) {
			const ts = this.viewEndMs - offset;
			const norm = (ts - viewStartMs) / this.viewSpanMs;
			if (norm < 0.015 || norm > 0.99) continue;
			const plotX = norm * plotW;
			pushPxRect(GUTTER_W + plotX, 0, 1, cssH, 0.15, 0.15, 0.165, 0.65);
			const relNow = liveEdge - ts;
			const lbl = relNow < 40 ? 'now' : `-${fmtSpan(relNow)}`;
			rulerSpans.push(`<span class="ruler-tick" style="left:${plotX.toFixed(1)}px">${lbl}</span>`);
		}
		this.rulerEl.innerHTML = rulerSpans.join('');

		// Span / Heap / Lock bars + direct inside-bar text labels
		const labels: LabelDraw[] = [];
		let hoveredSample: TimelineSample | null = null;
		let hoveredArea = Infinity;

		for (const s of this.samples) {
			if (s.endMs < viewStartMs || s.startMs > this.viewEndMs) continue;
			const tr = trackByKey.get(s.trackKey);
			if (!tr) continue;

			const rawX0 = GUTTER_W + ((s.startMs - viewStartMs) / this.viewSpanMs) * plotW;
			const rawX1 = GUTTER_W + ((s.endMs - viewStartMs) / this.viewSpanMs) * plotW;
			const x0 = Math.max(GUTTER_W + 1, rawX0);
			const x1 = Math.min(cssW, Math.max(x0 + 3, rawX1));
			const w = x1 - x0;
			if (w <= 0) continue;

			const depth = s.depth % tr.maxDepth;
			const y = tr.yTop + 6 + depth * (tr.barH + 3);
			const h = tr.barH;

			let isHit = false;
			if (
				this.hoverX !== null &&
				this.hoverY !== null &&
				this.hoverX >= x0 - 3 &&
				this.hoverX <= x1 + 3 &&
				this.hoverY >= y &&
				this.hoverY <= y + h
			) {
				if (w < hoveredArea) {
					hoveredArea = w;
					hoveredSample = s;
					isHit = true;
				}
			}

			const boost = isHit ? 1.25 : 1.0;
			const r = Math.min(1, s.color[0] * boost);
			const g = Math.min(1, s.color[1] * boost);
			const b = Math.min(1, s.color[2] * boost);
			const a = isHit ? 1.0 : s.color[3];

			pushPxRect(x0, y, w, h, r, g, b, a);
			if (w >= 4) {
				pushPxRect(x0, y, w, 1, Math.min(1, r * 1.35), Math.min(1, g * 1.35), Math.min(1, b * 1.35), 0.72);
			}
			if (w >= 5) {
				pushPxRect(x1 - 1, y, 1, h, 0.06, 0.06, 0.07, 0.7);
			}

			if (w >= 32) {
				const full = w >= 115 && s.durationText ? `${s.label} · ${s.durationText}` : s.label;
				labels.push({ x: x0, y, w, h, text: full, bright: isHit });
			}
		}

		// Shift+drag range selection highlight
		if (this.selectionMs) {
			const [s0, s1] = this.selectionMs;
			if (s1 >= viewStartMs && s0 <= this.viewEndMs) {
				const sx0 = Math.max(GUTTER_W, GUTTER_W + ((s0 - viewStartMs) / this.viewSpanMs) * plotW);
				const sx1 = Math.min(cssW, GUTTER_W + ((s1 - viewStartMs) / this.viewSpanMs) * plotW);
				pushPxRect(sx0, 0, Math.max(1, sx1 - sx0), cssH, 0.88, 0.27, 0.1, 0.14);
				pushPxRect(sx0, 0, 1, cssH, 0.88, 0.27, 0.1, 0.85);
				pushPxRect(sx1, 0, 1, cssH, 0.88, 0.27, 0.1, 0.85);
			}
		}

		// Hover crosshair
		this.lastHoveredSample = hoveredSample;
		if (this.hoverX !== null && this.hoverX >= GUTTER_W) {
			pushPxRect(this.hoverX, 0, 1, cssH, 0.92, 0.91, 0.89, 0.34);
		}

		// Live right-edge fire cursor
		if (liveEdge >= viewStartMs && liveEdge <= this.viewEndMs + 1) {
			const lx = GUTTER_W + Math.min(1, (liveEdge - viewStartMs) / this.viewSpanMs) * plotW;
			pushPxRect(lx - 1.5, 0, 1.5, cssH, 0.88, 0.27, 0.1, 0.95);
		}

		// Submit GPU draw call
		if (this.backend === 'webgpu' && this.device && this.gpuCtx && this.instBuf) {
			this.device.queue.writeBuffer(this.instBuf, 0, this.staging, 0, count * FLOATS_PER_INST);
			const encoder = this.device.createCommandEncoder();
			const pass = encoder.beginRenderPass({
				colorAttachments: [
					{
						view: this.gpuCtx.getCurrentTexture().createView(),
						clearValue: { r: 0.075, g: 0.075, b: 0.079, a: 1.0 },
						loadOp: 'clear',
						storeOp: 'store',
					},
				],
			});
			pass.setPipeline(this.pipeline);
			pass.setBindGroup(0, this.bindGroup);
			if (count > 0) pass.draw(6, count);
			pass.end();
			this.device.queue.submit([encoder.finish()]);
		} else if (this.ctx2d) {
			const ctx = this.ctx2d;
			ctx.fillStyle = '#131314';
			ctx.fillRect(0, 0, pxW, pxH);
			for (let i = 0; i < count; i++) {
				const o = i * FLOATS_PER_INST;
				const x0 = ((this.staging[o] + 1) * 0.5) * pxW;
				const y0 = ((1 - this.staging[o + 3]) * 0.5) * pxH;
				const w = Math.max(1, ((this.staging[o + 2] - this.staging[o]) * 0.5) * pxW);
				const h = Math.max(1, ((this.staging[o + 3] - this.staging[o + 1]) * 0.5) * pxH);
				const r = Math.round(this.staging[o + 4] * 255);
				const g = Math.round(this.staging[o + 5] * 255);
				const b = Math.round(this.staging[o + 6] * 255);
				ctx.fillStyle = `rgba(${r},${g},${b},${this.staging[o + 7]})`;
				ctx.fillRect(x0, y0, w, h);
			}
		}

		// Render crisp HiDPI text overlay directly inside every visible bar + left gutter
		const lctx = this.labelCtx;
		if (lctx) {
			lctx.setTransform(dpr, 0, 0, dpr, 0, 0);
			lctx.clearRect(0, 0, cssW, cssH);

			// Left gutter track headers
			lctx.font = '500 10.5px "Berkeley Mono", ui-monospace, monospace';
			lctx.textBaseline = 'middle';
			for (const tr of tracks) {
				const cy = tr.yTop + Math.min(18, tr.height * 0.5);
				lctx.fillStyle = tr.accent;
				lctx.fillRect(10, cy - 2.5, 5, 5);
				lctx.fillStyle = '#a4a3a0';
				lctx.fillText(tr.title, 20, cy);
			}

			// Text directly inside every bar wide enough to read
			lctx.font = '500 10.5px "Berkeley Mono", ui-monospace, monospace';
			lctx.textBaseline = 'middle';
			const charW = 6.4;
			for (const lb of labels) {
				const pad = 5;
				const avail = lb.w - pad * 2;
				if (avail < 18) continue;
				const maxChars = Math.floor(avail / charW);
				let text = lb.text;
				if (text.length > maxChars) {
					text = maxChars <= 2 ? text.slice(0, maxChars) : `${text.slice(0, maxChars - 1)}…`;
				}
				lctx.fillStyle = lb.bright ? '#ffffff' : 'rgba(255, 255, 255, 0.94)';
				lctx.fillText(text, lb.x + pad, lb.y + lb.h * 0.5 + 0.5);
			}
		}

		// Hover tooltip
		if (hoveredSample && this.hoverX !== null && this.hoverY !== null) {
			this.tooltipEl.hidden = false;
			this.tooltipEl.innerHTML = `<strong>${hoveredSample.label}</strong> <span>${hoveredSample.detail}</span>`;
			const left = Math.min(cssW - 250, Math.max(GUTTER_W + 6, this.hoverX + 12));
			const top = Math.min(cssH - 28, Math.max(6, this.hoverY - 26));
			this.tooltipEl.style.left = `${left}px`;
			this.tooltipEl.style.top = `${top}px`;
		} else {
			this.tooltipEl.hidden = true;
		}
	}
}
