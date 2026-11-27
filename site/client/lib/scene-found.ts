/**
 * Scene 6, built once, found after: results kept by what they were made from (docs/gc.md, docs/cargo.md). The first
 * project asks for its standard library: its key is worked out from what std is made from, std is built (a ring
 * fills) and put away under that key among the other records. The second project asks for the same: the same key, a
 * look along the shelf, and the record comes out at once. Nothing is built twice.
 */
import { type Inks, type Picture, MONO, canvas, clamp, inks, lerp, rgba, span } from './scene';

const COLS = 9;
const ROWS = 5;
const SLOT = 22; // the shelf slot std goes in

export function found(_section: HTMLElement, stage: HTMLElement): Picture {
	const c = canvas(stage);
	let w = 1;
	let h = 1;
	let k: Inks = inks();
	return {
		len: 3,
		still: 1,
		resize(cw, ch, dpr) {
			w = cw;
			h = ch;
			c.size(cw, ch, dpr);
			k = inks();
		},
		draw(p) {
			const ctx = c.ctx;
			ctx.clearRect(0, 0, w, h);
			const narrow = w < 640;
			const fs = narrow ? 11 : 13;
			// The shelf: a grid of kept records, on the right (below on a phone).
			const cell = Math.min(narrow ? (w - 40) / COLS : (w * 0.42) / COLS, 30);
			const gap = cell * 0.22;
			const gw = COLS * cell;
			const gh = ROWS * cell;
			const gx = narrow ? (w - gw) / 2 : w * 0.54;
			const gy = narrow ? h * 0.56 : (h - gh) / 2;
			const slot = (i: number) => [gx + (i % COLS) * cell, gy + Math.floor(i / COLS) * cell] as const;
			const built = span(p, 0.22, 0.38);
			const stored = span(p, 0.38, 0.46);
			const scan = span(p, 0.62, 0.74);
			const out = span(p, 0.76, 0.88);
			for (let i = 0; i < COLS * ROWS; i++) {
				const [x, y] = slot(i);
				const isStd = i === SLOT;
				const scanAt = scan * (SLOT + 1);
				const passing = scan > 0 && scan < 1 && Math.abs(i - scanAt) < 1.5 && i <= SLOT;
				ctx.fillStyle = isStd && stored > 0 ? rgba(k.fire, 0.25 + 0.75 * stored) : rgba(k.ink, passing ? 0.4 : 0.1 + ((i * 7) % 5) * 0.025);
				ctx.beginPath();
				ctx.roundRect(x + gap / 2, y + gap / 2, cell - gap, cell - gap, 4);
				ctx.fill();
			}
			ctx.font = `500 ${fs - 1}px ${MONO}`;
			ctx.fillStyle = k.ink3;
			ctx.textAlign = 'left';
			ctx.fillText('kept records', gx + gap / 2, gy - fs * 0.9);

			// The two projects, on the left (above on a phone), one under the other.
			const pa = narrow ? w * 0.26 : w * 0.24;
			const pb = narrow ? w * 0.74 : w * 0.24;
			const ya = narrow ? h * 0.08 : h * 0.3;
			const yb = narrow ? h * 0.08 : h * 0.7;
			const [sx, sy] = slot(SLOT);
			const scx = sx + cell / 2;
			const scy = sy + cell / 2;
			ask(ctx, pa, ya, 'project A · std', k, fs, span(p, 0, 0.08));
			ask(ctx, pb, yb, 'project B · std', k, fs, span(p, 0.48, 0.56));

			// A: the key, the build (a ring filling), the record going onto the shelf.
			const keyA = span(p, 0.08, 0.2);
			if (keyA > 0) key(ctx, pa, ya + fs * 2.6, keyA, k, fs, narrow);
			if (built > 0 && stored < 1) {
				const x = lerp(pa, scx, stored);
				const y = lerp(ya + fs * 5.6, scy, stored);
				ctx.strokeStyle = rgba(k.ink, 0.15);
				ctx.lineWidth = 3;
				ctx.beginPath();
				ctx.arc(x, y, 14, 0, Math.PI * 2);
				ctx.stroke();
				ctx.strokeStyle = k.fire;
				ctx.beginPath();
				ctx.arc(x, y, 14, -Math.PI / 2, -Math.PI / 2 + built * Math.PI * 2);
				ctx.stroke();
			}
			if (built > 0) tag(ctx, pa, ya + fs * 5.6 + 28, 'built', k.ink2, fs, built);

			// B: the same key, at once; the look along the shelf; the record handed over.
			const keyB = span(p, 0.56, 0.6);
			if (keyB > 0) key(ctx, pb, yb + fs * 2.6, keyB, k, fs, narrow);
			if (out > 0) {
				const x = lerp(scx, pb, out);
				const y = lerp(scy, yb + fs * 5.6, out);
				ctx.fillStyle = k.fire;
				ctx.beginPath();
				ctx.roundRect(x - cell / 2 + gap / 2, y - cell / 2 + gap / 2, cell - gap, cell - gap, 4);
				ctx.fill();
				tag(ctx, pb, yb + fs * 5.6 + 28, 'found · not built', k.fire, fs, clamp((out - 0.6) / 0.4));
			}
		},
	};
}

function ask(ctx: CanvasRenderingContext2D, x: number, y: number, text: string, k: Inks, fs: number, a: number) {
	if (a <= 0) return;
	ctx.globalAlpha = a;
	ctx.font = `600 ${fs + 2}px ${MONO}`;
	ctx.fillStyle = k.ink;
	ctx.textAlign = 'center';
	ctx.fillText(text, x, y);
	ctx.globalAlpha = 1;
}

function key(ctx: CanvasRenderingContext2D, x: number, y: number, a: number, k: Inks, fs: number, short: boolean) {
	const text = short ? 'key = hash(inputs)' : 'key = hash(what it is made from)';
	ctx.font = `400 ${fs}px ${MONO}`;
	ctx.fillStyle = k.ink3;
	ctx.textAlign = 'center';
	ctx.fillText(text.slice(0, Math.round(text.length * a)), x, y);
}

function tag(ctx: CanvasRenderingContext2D, x: number, y: number, text: string, ink: string, fs: number, a: number) {
	ctx.globalAlpha = clamp(a);
	ctx.font = `600 ${fs}px ${MONO}`;
	ctx.fillStyle = ink;
	ctx.textAlign = 'center';
	ctx.fillText(text, x, y);
	ctx.globalAlpha = 1;
}
