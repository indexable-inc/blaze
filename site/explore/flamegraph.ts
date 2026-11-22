/**
 * Interactive YourKit / Speedscope-grade Flame Graph & Icicle renderer (HiDPI Canvas).
 *
 * Features:
 *   - Built directly from real `SpanExit` `(spanId, parentSpanId, durationNs)` trees.
 *   - Toggleable Top-Down (Call Tree / Icicle) vs Bottom-Up (Inverted / Self Hotspots).
 *   - Click any frame to zoom horizontally to 100%; click root or Reset to zoom back out.
 *   - Live search query highlighting with exact matched % of total time.
 *   - Hover tooltip showing Self Time, Total Time, % of Total, Call Count, Avg, and Max.
 */

export interface FlameNode {
	name: string;
	slot: number;
	totalNs: number;
	selfNs: number;
	calls: number;
	maxNs: number;
	children: Map<string, FlameNode>;
}

interface PlacedFrame {
	node: FlameNode;
	x: number;
	y: number;
	w: number;
	h: number;
	depth: number;
	matched: boolean;
}

function fmtNs(ns: number): string {
	if (ns < 1_000) return `${ns.toFixed(0)} ns`;
	if (ns < 1_000_000) return `${(ns / 1_000).toFixed(1)} µs`;
	if (ns < 1_000_000_000) return `${(ns / 1_000_000).toFixed(2)} ms`;
	return `${(ns / 1_000_000_000).toFixed(2)} s`;
}

function hashColor(name: string, selfShare: number): string {
	// Warm fire-orange for hot self-time frames, cool ix.dev slate/teal/blue for low self-time wrappers.
	if (selfShare >= 0.35) return '#d9481c';
	if (selfShare >= 0.18) return '#c96b27';
	let h = 0;
	for (let i = 0; i < name.length; i++) h = (h * 31 + name.charCodeAt(i)) | 0;
	const palette = ['#2a6fb8', '#1b8a64', '#3b79b8', '#237e6d', '#7c5cbf', '#556980'];
	return palette[Math.abs(h) % palette.length];
}

export class FlameGraphView {
	private root: FlameNode | null = null;
	private zoomed: FlameNode | null = null;
	private inverted = false;
	private query = '';
	private placed: PlacedFrame[] = [];
	private hovered: PlacedFrame | null = null;
	private ctx: CanvasRenderingContext2D | null;

	constructor(
		private readonly canvas: HTMLCanvasElement,
		private readonly tooltipEl: HTMLElement,
		private readonly onStatus: (info: { matchedPct: number | null; zoomedName: string | null }) => void,
		private readonly onSelectSlot?: (slot: number) => void,
	) {
		this.ctx = canvas.getContext('2d');
		this.bind();
	}

	setData(root: FlameNode): void {
		this.root = root;
		// Preserve zoom if same name still exists under root, else stay on root.
		if (this.zoomed) {
			const found = this.findByName(root, this.zoomed.name);
			this.zoomed = found ?? null;
		}
		this.render();
	}

	setInverted(inv: boolean): void {
		this.inverted = inv;
		this.zoomed = null;
		this.render();
	}

	setQuery(q: string): void {
		this.query = q.trim().toLowerCase();
		this.render();
	}

	resetZoom(): void {
		this.zoomed = null;
		this.render();
	}

	private findByName(node: FlameNode, name: string): FlameNode | null {
		if (node.name === name) return node;
		for (const ch of node.children.values()) {
			const hit = this.findByName(ch, name);
			if (hit) return hit;
		}
		return null;
	}

	private buildInverted(src: FlameNode): FlameNode {
		const invRoot: FlameNode = {
			name: 'all (bottom-up)',
			slot: -1,
			totalNs: src.totalNs,
			selfNs: 0,
			calls: src.calls,
			maxNs: src.maxNs,
			children: new Map(),
		};
		const walk = (node: FlameNode, ancestors: FlameNode[]) => {
			if (node.selfNs > 0 && node.slot >= 0) {
				let cur = invRoot;
				const chain = [node, ...ancestors];
				for (const item of chain) {
					let next = cur.children.get(item.name);
					if (!next) {
						next = {
							name: item.name,
							slot: item.slot,
							totalNs: 0,
							selfNs: 0,
							calls: 0,
							maxNs: 0,
							children: new Map(),
						};
						cur.children.set(item.name, next);
					}
					next.totalNs += node.selfNs;
					if (item === node) next.selfNs += node.selfNs;
					next.calls += node.calls;
					if (node.maxNs > next.maxNs) next.maxNs = node.maxNs;
					cur = next;
				}
			}
			const nextAnc = node.slot >= 0 ? [node, ...ancestors] : ancestors;
			for (const ch of node.children.values()) walk(ch, nextAnc);
		};
		walk(src, []);
		return invRoot;
	}

	private bind(): void {
		this.canvas.addEventListener('pointermove', (e) => {
			const rect = this.canvas.getBoundingClientRect();
			const mx = e.clientX - rect.left;
			const my = e.clientY - rect.top;
			const hit = this.placed.find((f) => mx >= f.x && mx <= f.x + f.w && my >= f.y && my <= f.y + f.h) ?? null;
			if (hit !== this.hovered) {
				this.hovered = hit;
				this.render();
			}
			if (hit && this.root && this.root.totalNs > 0) {
				const n = hit.node;
				const pct = ((n.totalNs / this.root.totalNs) * 100).toFixed(1);
				const selfPct = ((n.selfNs / this.root.totalNs) * 100).toFixed(1);
				const avg = n.calls > 0 ? n.totalNs / n.calls : 0;
				this.tooltipEl.hidden = false;
				this.tooltipEl.innerHTML = `<strong>${n.name}</strong> <span>total ${fmtNs(n.totalNs)} (${pct}%) · self ${fmtNs(n.selfNs)} (${selfPct}%) · ${n.calls}× · avg ${fmtNs(avg)}</span>`;
				const left = Math.min(rect.width - 340, Math.max(8, mx + 12));
				const top = Math.min(rect.height - 28, Math.max(6, my - 26));
				this.tooltipEl.style.left = `${left}px`;
				this.tooltipEl.style.top = `${top}px`;
				this.canvas.style.cursor = 'pointer';
			} else {
				this.tooltipEl.hidden = true;
				this.canvas.style.cursor = 'default';
			}
		});

		this.canvas.addEventListener('pointerleave', () => {
			this.hovered = null;
			this.tooltipEl.hidden = true;
			this.render();
		});

		this.canvas.addEventListener('click', () => {
			if (!this.hovered) return;
			const node = this.hovered.node;
			if (node.slot === -1) {
				this.zoomed = null;
			} else {
				this.zoomed = node;
				this.onSelectSlot?.(node.slot);
			}
			this.render();
		});
	}

	render(): void {
		const ctx = this.ctx;
		if (!ctx) return;
		const wrap = this.canvas.parentElement ?? this.canvas;
		const rect = wrap.getBoundingClientRect();
		const cssW = Math.max(1, Math.floor(rect.width));
		const cssH = Math.max(1, Math.floor(rect.height));
		const dpr = Math.min(window.devicePixelRatio || 1, 2);
		const pxW = Math.round(cssW * dpr);
		const pxH = Math.round(cssH * dpr);
		if (this.canvas.width !== pxW || this.canvas.height !== pxH) {
			this.canvas.width = pxW;
			this.canvas.height = pxH;
		}

		ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
		ctx.fillStyle = '#141415';
		ctx.fillRect(0, 0, cssW, cssH);

		this.placed = [];
		if (!this.root || this.root.totalNs <= 0) {
			ctx.fillStyle = '#7d7c7a';
			ctx.font = '11.5px "Berkeley Mono", ui-monospace, monospace';
			ctx.fillText('No span call tree yet', 14, 24);
			this.onStatus({ matchedPct: null, zoomedName: null });
			return;
		}

		const displayRoot = this.inverted ? this.buildInverted(this.root) : this.root;
		const focus = this.zoomed ? (this.findByName(displayRoot, this.zoomed.name) ?? displayRoot) : displayRoot;
		const rootTotal = Math.max(1, focus.totalNs);
		const globalTotal = Math.max(1, displayRoot.totalNs);
		const rowH = 22;
		const gap = 2;
		const padTop = 6;
		let matchedNs = 0;

		const layout = (node: FlameNode, depth: number, xStart: number, width: number) => {
			if (width < 1.5) return;
			const y = padTop + depth * (rowH + gap);
			if (y > cssH) return;
			const matched = this.query.length > 0 && node.name.toLowerCase().includes(this.query);
			if (matched) matchedNs += node.selfNs > 0 ? node.selfNs : node.totalNs;
			this.placed.push({ node, x: xStart, y, w: width, h: rowH, depth, matched });

			const kids = [...node.children.values()].sort((a, b) => b.totalNs - a.totalNs);
			let cx = xStart;
			for (const ch of kids) {
				const cw = (ch.totalNs / Math.max(1, node.totalNs)) * width;
				layout(ch, depth + 1, cx, cw);
				cx += cw;
			}
		};

		layout(focus, 0, 6, cssW - 12);

		ctx.font = '500 10.5px "Berkeley Mono", ui-monospace, monospace';
		ctx.textBaseline = 'middle';
		const charW = 6.4;

		for (const f of this.placed) {
			const isHov = this.hovered?.node === f.node && this.hovered?.depth === f.depth;
			const selfShare = f.node.selfNs / globalTotal;
			let fill = f.node.slot === -1 ? '#262629' : hashColor(f.node.name, selfShare);
			if (this.query.length > 0) {
				fill = f.matched ? '#e0461a' : '#232326';
			}

			ctx.globalAlpha = isHov ? 1.0 : this.query.length > 0 && !f.matched ? 0.55 : 0.92;
			ctx.fillStyle = fill;
			ctx.fillRect(f.x, f.y, Math.max(1, f.w - 1), f.h);

			// Top highlight hairline
			ctx.fillStyle = isHov ? 'rgba(255,255,255,0.45)' : 'rgba(255,255,255,0.14)';
			ctx.fillRect(f.x, f.y, Math.max(1, f.w - 1), 1);

			// Direct text label inside frame
			const avail = f.w - 10;
			if (avail >= 22) {
				const pct = ((f.node.totalNs / rootTotal) * 100).toFixed(1);
				const label = avail >= 145 ? `${f.node.name} (${pct}% · ${fmtNs(f.node.totalNs)})` : f.node.name;
				const maxChars = Math.floor(avail / charW);
				const text = label.length > maxChars ? (maxChars <= 2 ? label.slice(0, maxChars) : `${label.slice(0, maxChars - 1)}…`) : label;
				ctx.globalAlpha = 1.0;
				ctx.fillStyle = f.node.slot === -1 ? '#d4d3cf' : '#ffffff';
				ctx.fillText(text, f.x + 5, f.y + f.h * 0.5 + 0.5);
			}
		}
		ctx.globalAlpha = 1.0;

		const matchedPct = this.query.length > 0 ? Math.min(100, (matchedNs / globalTotal) * 100) : null;
		this.onStatus({
			matchedPct,
			zoomedName: focus === displayRoot ? null : focus.name,
		});
	}
}
