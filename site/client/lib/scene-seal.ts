/**
 * Scene 5, sealed by default: a build script inside the sandbox's wall (docs/cargo.md, "Build scripts and proc macros
 * run in a sandbox"). Its reaches for the network and for files outside go out to the wall and stop there, each
 * marked refused; its write to its own OUT_DIR, inside, lands. A proc macro sits beside it, as WebAssembly.
 */
import { type Inks, type Picture, MONO, canvas, clamp, inks, lerp, rgba, span } from './scene';

type Reach = { label: string; to: [number, number]; start: number; ok: boolean };

// Targets in units of the box (0..1 inside; outside beyond). The start of each, in scroll progress.
const REACHES: Reach[] = [
	{ label: 'network', to: [1.32, 0.12], start: 0.12, ok: false },
	{ label: '~/.ssh', to: [1.3, 0.86], start: 0.24, ok: false },
	{ label: '/usr/lib', to: [-0.3, 0.9], start: 0.36, ok: false },
	{ label: 'OUT_DIR', to: [0.78, 0.68], start: 0.54, ok: true },
];
const FROM: [number, number] = [0.3, 0.42];

export function seal(_section: HTMLElement, stage: HTMLElement): Picture {
	const c = canvas(stage);
	let w = 1;
	let h = 1;
	let k: Inks = inks();
	return {
		len: 2.8,
		still: 0.95,
		resize(cw, ch, dpr) {
			w = cw;
			h = ch;
			c.size(cw, ch, dpr);
			k = inks();
		},
		draw(p) {
			const ctx = c.ctx;
			ctx.clearRect(0, 0, w, h);
			const bw = Math.min(w * 0.62, 560);
			const bh = Math.min(h * 0.62, 300);
			const bx = (w - bw) / 2;
			const by = (h - bh) / 2;
			const P = (u: number, v: number) => [bx + u * bw, by + v * bh] as const;
			const small = w < 520;
			const fs = small ? 11 : 13;

			// The wall. Where a reach hits it, it lights for a moment.
			const appear = span(p, 0, 0.1);
			ctx.globalAlpha = appear;
			ctx.fillStyle = rgba(k.fire, 0.04);
			ctx.strokeStyle = rgba(k.ink, 0.5);
			ctx.lineWidth = 1.5;
			round(ctx, bx, by, bw, bh, 22);
			ctx.fill();
			ctx.stroke();
			ctx.font = `500 ${fs - 1}px ${MONO}`;
			ctx.fillStyle = k.ink3;
			ctx.textAlign = 'left';
			ctx.fillText('sandbox', bx + 16, by + 24);

			for (const r of REACHES) {
				const t = span(p, r.start, r.start + 0.16);
				if (t <= 0) {
					if (!r.ok) label(ctx, ...P(...r.to), r.label, k.ink3, fs, 0.6 * appear);
					continue;
				}
				const [x0, y0] = P(...FROM);
				const [x1, y1] = P(...r.to);
				// Where the line crosses the wall (for those outside): the box edge along the way.
				const stop = r.ok ? 1 : crossing(FROM, r.to);
				const head = Math.min(t / 0.8, 1) * stop;
				const x = lerp(x0, x1, head);
				const y = lerp(y0, y1, head);
				ctx.strokeStyle = rgba(r.ok ? k.fire2 : k.ink, r.ok ? 0.85 : 0.45);
				ctx.setLineDash([3, 4]);
				ctx.lineWidth = 1.2;
				ctx.beginPath();
				ctx.moveTo(x0, y0);
				ctx.lineTo(x, y);
				ctx.stroke();
				ctx.setLineDash([]);
				const arrived = clamp((t - 0.8) / 0.2);
				if (r.ok) {
					chip(ctx, x1, y1, 'OUT_DIR', k, fs, arrived > 0 ? k.fire2 : k.ink3);
					if (arrived > 0) label(ctx, x1, y1 + fs * 2.2, '✓ written', k.fire2, fs, arrived);
				} else {
					label(ctx, x1, y1, r.label, k.ink3, fs, 0.6);
					if (arrived > 0) {
						// The refusal: a flash on the wall and a mark.
						const g = ctx.createRadialGradient(x, y, 0, x, y, 40);
						g.addColorStop(0, rgba(k.fire, 0.5 * (1 - arrived * 0.6)));
						g.addColorStop(1, rgba(k.fire, 0));
						ctx.fillStyle = g;
						ctx.fillRect(x - 40, y - 40, 80, 80);
						label(ctx, x, y - fs * 1.4, '✗', k.fire, fs + 3, arrived);
					}
				}
			}
			// The script and the macro, over the lines that leave them.
			chip(ctx, ...P(...FROM), 'build.rs', k, fs, k.ink);
			chip(ctx, ...P(0.3, 0.74), 'proc macro · wasm', k, fs, k.ink2);
			ctx.globalAlpha = 1;
		},
	};
}

/** The fraction of the way from `a` to `b` (box units) where the segment leaves the unit box. */
function crossing(a: [number, number], b: [number, number]): number {
	let t = 1;
	for (const axis of [0, 1] as const) {
		const d = b[axis] - a[axis];
		if (d > 0 && b[axis] > 1) t = Math.min(t, (1 - a[axis]) / d);
		if (d < 0 && b[axis] < 0) t = Math.min(t, (0 - a[axis]) / d);
	}
	return t;
}

function round(ctx: CanvasRenderingContext2D, x: number, y: number, w: number, h: number, r: number) {
	ctx.beginPath();
	ctx.roundRect(x, y, w, h, r);
}

function chip(ctx: CanvasRenderingContext2D, x: number, y: number, text: string, k: Inks, fs: number, ink: string) {
	ctx.font = `500 ${fs}px ${MONO}`;
	const tw = ctx.measureText(text).width;
	const pw = tw + fs * 1.6;
	const ph = fs * 2.2;
	ctx.fillStyle = k.card;
	ctx.strokeStyle = rgba(k.ink, 0.25);
	ctx.lineWidth = 1;
	round(ctx, x - pw / 2, y - ph / 2, pw, ph, ph / 2);
	ctx.fill();
	ctx.stroke();
	ctx.fillStyle = ink;
	ctx.textAlign = 'center';
	ctx.textBaseline = 'middle';
	ctx.fillText(text, x, y + 0.5);
	ctx.textBaseline = 'alphabetic';
}

function label(ctx: CanvasRenderingContext2D, x: number, y: number, text: string, ink: string, fs: number, a: number) {
	const keep = ctx.globalAlpha;
	ctx.globalAlpha = keep * clamp(a);
	ctx.font = `500 ${fs}px ${MONO}`;
	ctx.fillStyle = ink;
	ctx.textAlign = 'center';
	ctx.textBaseline = 'middle';
	// Kept inside the canvas: on a phone the targets outside the wall are near its edge.
	const half = ctx.measureText(text).width / 2 + 4;
	ctx.fillText(text, clamp(x, half, ctx.canvas.width / ctx.getTransform().a - half), y);
	ctx.textBaseline = 'alphabetic';
	ctx.globalAlpha = keep;
}
