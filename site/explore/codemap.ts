/**
 * Live Heat-Mapped Semantic Code Map (Post-IDE Visual Explorer).
 *
 * Fuses blaze's static module/call graph with live runtime telemetry:
 *   - Nodes = functions grouped by module cluster, colored by live CPU heat / heap pressure / lock contention.
 *   - Directed bezier edges = real call edges (`caller -> callee`), thickness weighted by live calls.
 *   - Pan (drag), pinch-zoom (`ctrlKey` wheel), hover for instant metrics, click any node to open
 *     the read-only IDE Source & Runtime Inspector!
 */

export interface CodeMapNode {
	slot: number;
	name: string;
	module: string;
	file?: string;
	line?: number;
	calls: number;
	selfNs: number;
	totalNs: number;
	avgNs: number;
	p95Ns: number;
	rebuilt: boolean;
	lockContended: boolean;
}

export interface CodeMapEdge {
	fromSlot: number;
	toSlot: number;
	calls: number;
}

interface PlacedNode {
	node: CodeMapNode;
	x: number;
	y: number;
	w: number;
	h: number;
}

function fmtNs(ns: number): string {
	if (ns < 1_000) return `${ns.toFixed(0)} ns`;
	if (ns < 1_000_000) return `${(ns / 1_000).toFixed(1)} µs`;
	if (ns < 1_000_000_000) return `${(ns / 1_000_000).toFixed(1)} ms`;
	return `${(ns / 1_000_000_000).toFixed(2)} s`;
}

export class CodeMapView {
	private nodes: CodeMapNode[] = [];
	private edges: CodeMapEdge[] = [];
	private placed: PlacedNode[] = [];
	private hovered: PlacedNode | null = null;
	private selectedSlot: number | null = null;
	private ctx: CanvasRenderingContext2D | null;
	private panX = 0;
	private panY = 0;
	private scale = 1;
	private dragging = false;
	private dragStartX = 0;
	private dragStartY = 0;
	private dragPanX = 0;
	private dragPanY = 0;

	constructor(
		private readonly canvas: HTMLCanvasElement,
		private readonly onSelectNode: (node: CodeMapNode) => void,
	) {
		this.ctx = canvas.getContext('2d');
		this.bind();
	}

	setData(nodes: CodeMapNode[], edges: CodeMapEdge[], selectedSlot: number | null): void {
		this.nodes = nodes;
		this.edges = edges;
		this.selectedSlot = selectedSlot;
		this.render();
	}

	private bind(): void {
		const el = this.canvas;
		el.addEventListener(
			'wheel',
			(e) => {
				e.preventDefault();
				if (e.ctrlKey || e.metaKey) {
					const factor = Math.exp(-e.deltaY * 0.01);
					this.scale = Math.max(0.45, Math.min(2.4, this.scale * factor));
				} else {
					this.panX -= e.deltaX;
					this.panY -= e.deltaY;
				}
				this.render();
			},
			{ passive: false },
		);

		el.addEventListener('pointerdown', (e) => {
			if (e.button !== 0) return;
			this.dragging = true;
			this.dragStartX = e.clientX;
			this.dragStartY = e.clientY;
			this.dragPanX = this.panX;
			this.dragPanY = this.panY;
			el.setPointerCapture(e.pointerId);
		});

		el.addEventListener('pointermove', (e) => {
			const rect = el.getBoundingClientRect();
			const mx = (e.clientX - rect.left - this.panX) / this.scale;
			const my = (e.clientY - rect.top - this.panY) / this.scale;
			if (this.dragging) {
				this.panX = this.dragPanX + (e.clientX - this.dragStartX);
				this.panY = this.dragPanY + (e.clientY - this.dragStartY);
				this.render();
				return;
			}
			const hit = this.placed.find((p) => mx >= p.x && mx <= p.x + p.w && my >= p.y && my <= p.y + p.h) ?? null;
			if (hit !== this.hovered) {
				this.hovered = hit;
				el.style.cursor = hit ? 'pointer' : 'grab';
				this.render();
			}
		});

		el.addEventListener('pointerup', (e) => {
			if (!this.dragging) return;
			const moved = Math.hypot(e.clientX - this.dragStartX, e.clientY - this.dragStartY);
			this.dragging = false;
			if (moved < 5 && this.hovered) {
				this.selectedSlot = this.hovered.node.slot;
				this.onSelectNode(this.hovered.node);
				this.render();
			}
		});

		el.addEventListener('dblclick', () => {
			this.panX = 0;
			this.panY = 0;
			this.scale = 1;
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
		ctx.fillStyle = '#121214';
		ctx.fillRect(0, 0, cssW, cssH);

		// Subtle dot-grid background
		ctx.fillStyle = 'rgba(255,255,255,0.05)';
		const step = 24;
		for (let x = ((this.panX % step) + step) % step; x < cssW; x += step) {
			for (let y = ((this.panY % step) + step) % step; y < cssH; y += step) {
				ctx.fillRect(x, y, 1, 1);
			}
		}

		if (this.nodes.length === 0) {
			ctx.fillStyle = '#7d7c7a';
			ctx.font = '11.5px "Berkeley Mono", ui-monospace, monospace';
			ctx.fillText('Waiting for live semantic call graph…', 16, 26);
			return;
		}

		// Compute topological depth (root callers on left -> callees on right)
		const incoming = new Map<number, number>();
		for (const n of this.nodes) incoming.set(n.slot, 0);
		for (const e of this.edges) incoming.set(e.toSlot, (incoming.get(e.toSlot) ?? 0) + 1);

		const col0 = this.nodes.filter((n) => (incoming.get(n.slot) ?? 0) === 0).sort((a, b) => b.totalNs - a.totalNs);
		const col1 = this.nodes.filter((n) => (incoming.get(n.slot) ?? 0) > 0).sort((a, b) => b.totalNs - a.totalNs);
		const cols = col1.length > 0 ? [col0, col1] : [col0];

		const cardW = 236;
		const cardH = 54;
		const colGap = 110;
		const rowGap = 16;
		const startX = Math.max(28, (cssW - (cols.length * cardW + (cols.length - 1) * colGap)) * 0.5);

		this.placed = [];
		const bySlot = new Map<number, PlacedNode>();
		for (let c = 0; c < cols.length; c++) {
			const list = cols[c];
			const totalH = list.length * cardH + Math.max(0, list.length - 1) * rowGap;
			const startY = Math.max(20, (cssH - totalH) * 0.5);
			for (let r = 0; r < list.length; r++) {
				const p: PlacedNode = {
					node: list[r],
					x: startX + c * (cardW + colGap),
					y: startY + r * (cardH + rowGap),
					w: cardW,
					h: cardH,
				};
				this.placed.push(p);
				bySlot.set(list[r].slot, p);
			}
		}

		const totalAllNs = Math.max(1, this.nodes.reduce((s, n) => s + n.selfNs, 0));

		ctx.save();
		ctx.translate(this.panX, this.panY);
		ctx.scale(this.scale, this.scale);

		// Draw directed cubic call edges
		const maxCalls = Math.max(1, this.edges.reduce((m, e) => Math.max(m, e.calls), 1));
		for (const e of this.edges) {
			const a = bySlot.get(e.fromSlot);
			const b = bySlot.get(e.toSlot);
			if (!a || !b) continue;
			const x0 = a.x + a.w;
			const y0 = a.y + a.h * 0.5;
			const x1 = b.x;
			const y1 = b.y + b.h * 0.5;
			const mx = (x0 + x1) * 0.5;
			const weight = 1.2 + (e.calls / maxCalls) * 2.4;
			const active = this.selectedSlot === e.fromSlot || this.selectedSlot === e.toSlot;

			ctx.beginPath();
			ctx.moveTo(x0, y0);
			ctx.bezierCurveTo(mx, y0, mx, y1, x1, y1);
			ctx.strokeStyle = active ? '#e0461a' : 'rgba(180, 179, 175, 0.35)';
			ctx.lineWidth = weight;
			ctx.stroke();

			// Arrowhead
			ctx.fillStyle = active ? '#e0461a' : 'rgba(180, 179, 175, 0.5)';
			ctx.beginPath();
			ctx.moveTo(x1, y1);
			ctx.lineTo(x1 - 7, y1 - 4);
			ctx.lineTo(x1 - 7, y1 + 4);
			ctx.closePath();
			ctx.fill();

			// Call count badge mid-edge
			ctx.font = '500 9.5px "Berkeley Mono", ui-monospace, monospace';
			ctx.fillStyle = '#878684';
			ctx.textAlign = 'center';
			ctx.fillText(`${e.calls}×`, mx, (y0 + y1) * 0.5 - 6);
		}
		ctx.textAlign = 'left';

		// Draw heat-mapped semantic function cards
		for (const p of this.placed) {
			const n = p.node;
			const share = n.selfNs / totalAllNs;
			const hot = share >= 0.25;
			const isSel = n.slot === this.selectedSlot;
			const isHov = this.hovered?.node.slot === n.slot;

			// Card surface
			ctx.fillStyle = isSel ? '#211c1a' : isHov ? '#1b1b1e' : '#161618';
			ctx.beginPath();
			ctx.roundRect(p.x, p.y, p.w, p.h, 7);
			ctx.fill();

			// Border: fire orange if hot/selected, red if lock contended, subtle hairline otherwise
			ctx.strokeStyle = isSel
				? '#e0461a'
				: n.lockContended
					? '#ff7168'
					: hot
						? 'rgba(224, 70, 26, 0.65)'
						: isHov
							? '#45454a'
							: '#2a2a2e';
			ctx.lineWidth = isSel ? 1.8 : 1.1;
			ctx.stroke();

			// Left heat strip
			ctx.fillStyle = n.lockContended ? '#ff7168' : hot ? '#e0461a' : '#199e70';
			ctx.fillRect(p.x + 1, p.y + 8, 3, p.h - 16);

			// Function name
			ctx.font = '600 11.5px "Berkeley Mono", ui-monospace, monospace';
			ctx.fillStyle = '#ebeae6';
			ctx.textBaseline = 'top';
			ctx.fillText(n.name.slice(0, 28), p.x + 12, p.y + 9);

			// Rebuilt pill if modified in last build
			if (n.rebuilt) {
				ctx.font = '600 9px "Berkeley Mono", ui-monospace, monospace';
				ctx.fillStyle = '#3987e5';
				ctx.textAlign = 'right';
				ctx.fillText('REBUILT', p.x + p.w - 9, p.y + 10);
				ctx.textAlign = 'left';
			}

			// Bottom live telemetry line: avg · calls · % self
			ctx.font = '400 10.5px "Berkeley Mono", ui-monospace, monospace';
			ctx.fillStyle = hot ? '#f08a66' : '#8e8d8a';
			ctx.fillText(
				`avg ${fmtNs(n.avgNs)} · ${n.calls}× · ${(share * 100).toFixed(0)}% self`,
				p.x + 12,
				p.y + 30,
			);
		}

		ctx.restore();
	}
}
