/**
 * The goals' drawings on the home page (Art in content/schema.flecs; render.rs writes `<div class="art"
 * data-piece="..."><canvas>`). Each is a picture of the goal's mechanism, drawn on a coarse grid of square cells, the
 * fire logo's pixels, in the page's inks (styles/art.css), after ix.dev's abstract art (packages/web src/lib/art):
 * monochrome, slow, no words, at most one accent.
 *
 * - A piece is a pure function of time, so it is deterministic: the same moment always draws the same frame. What
 *   varies (which cell changes, how long a worker takes) comes from `hash`, not Math.random.
 * - It draws only while its goal is open, on screen and the tab visible, at 30 frames a second; opening a goal starts
 *   its story from the beginning. Under prefers-reduced-motion it draws one still frame, a moment that tells it.
 * - Cells sit on whole device pixels (the canvas is sized at the device pixel ratio), so they stay crisp.
 * - Set up again on every page (client/lib/page.ts); leaving the page stops each piece's observers, listeners and
 *   animation frame.
 */

import { onPage } from './lib/page';

type Inks = { ink: string; mid: string; faint: string; fire: string };

type Grid = {
	cols: number;
	rows: number;
	/** One cell; anything off the grid or fully transparent is skipped. */
	cell: (i: number, j: number, colour: string, alpha: number) => void;
};

type Piece = {
	/** Seconds in one loop. */
	period: number;
	/** The moment the still frame shows. */
	still: number;
	/** `t` is the time in this loop, `loop` its number, `time` the whole clock (for what never loops). */
	draw: (g: Grid, k: Inks, t: number, loop: number, time: number) => void;
};

/** CSS px from one cell to the next, and the gap that keeps each cell a pixel. */
const PITCH = 8;
const GAP = 2;
const FPS = 30;

function grid(ctx: CanvasRenderingContext2D, w: number, h: number, ratio: number): Grid {
	const p = Math.max(2, Math.round(PITCH * ratio));
	const size = p - Math.max(1, Math.round(GAP * ratio));
	const cols = Math.floor(w / p);
	const rows = Math.floor(h / p);
	const ox = Math.floor((w - cols * p + (p - size)) / 2);
	const oy = Math.floor((h - rows * p + (p - size)) / 2);
	return {
		cols,
		rows,
		cell(i, j, colour, alpha) {
			i = Math.round(i);
			j = Math.round(j);
			if (alpha < 0.01 || i < 0 || j < 0 || i >= cols || j >= rows) return;
			ctx.globalAlpha = Math.min(1, alpha);
			ctx.fillStyle = colour;
			ctx.fillRect(ox + i * p, oy + j * p, size, size);
		},
	};
}

const clamp01 = (x: number) => Math.min(1, Math.max(0, x));
/** 0 before `a`, 1 after `b`, eased (smoothstep) between. */
const ramp = (t: number, a: number, b: number) => {
	const x = clamp01((t - a) / (b - a));
	return x * x * (3 - 2 * x);
};
const lerp = (a: number, b: number, u: number) => a + (b - a) * u;

/** A stable number in [0, 1) for a few integers. */
function hash(a: number, b = 0, c = 0): number {
	let h = Math.imul(a | 0, 0x27d4eb2d) ^ Math.imul((b | 0) + 0x9e3779b9, 0x85ebca6b) ^ Math.imul((c | 0) + 0x632be5ab, 0xc2b2ae35);
	h ^= h >>> 15;
	h = Math.imul(h, 0x2c1b3c6d);
	h ^= h >>> 12;
	h = Math.imul(h, 0x297a2d39);
	h ^= h >>> 15;
	return (h >>> 0) / 4294967296;
}

type Pt = [number, number];

/** The cells of a path through corners joined by straight runs, first to last. */
function path(corners: Pt[]): Pt[] {
	const out: Pt[] = [corners[0]];
	for (const [tx, ty] of corners.slice(1)) {
		let [x, y] = out[out.length - 1];
		while (x !== tx || y !== ty) {
			if (x !== tx) x += Math.sign(tx - x);
			else y += Math.sign(ty - y);
			out.push([x, y]);
		}
	}
	return out;
}

/** The point `u` (0 to 1) of the way along a path. */
const along = (p: Pt[], u: number): Pt => p[Math.round(clamp01(u) * (p.length - 1))];

/** A moving cell: its head at `u` along the path, two fading cells behind it. */
function packet(g: Grid, p: Pt[], u: number, colour: string, alpha = 1) {
	const at = Math.round(clamp01(u) * (p.length - 1));
	[1, 0.45, 0.2].forEach((a, back) => {
		const q = p[at - back];
		if (q) g.cell(q[0], q[1], colour, a * alpha);
	});
}

type Glyph = number[][];

/** A glyph with its top left at (x, y), leaving out the cells `skip` says. */
function stamp(g: Grid, glyph: Glyph, x: number, y: number, colour: string, alpha: number, skip?: (i: number, j: number) => boolean) {
	glyph.forEach((row, v) =>
		row.forEach((on, u) => {
			const i = Math.round(x) + u;
			const j = Math.round(y) + v;
			if (on && !skip?.(i, j)) g.cell(i, j, colour, alpha);
		}),
	);
}

// 1. Cacheable proc macros. A pattern goes into a sealed square (the sandbox), is worked on there and comes out
// expanded; a copy of the result goes onto the shelf above. The same pattern comes again: the shelf answers (the one
// accent), the copy goes round the square to the output, and the square, dimmed, does nothing.
const MACRO_IN: Glyph = [
	[1, 0, 1],
	[0, 1, 1],
	[1, 1, 0],
];
// The expansion: the input mirrored out to five by five, a new pattern made only from what came in.
const MACRO_OUT: Glyph = [0, 1, 2, 1, 0].map((r) => [0, 1, 2, 1, 0].map((c) => MACRO_IN[r][c]));

const cache: Piece = {
	period: 11,
	still: 8.9,
	draw(g, k, t) {
		const { cols: C, rows: R } = g;
		const fade = ramp(t, 0, 0.5) * (1 - ramp(t, 10.2, 10.9));
		const mid = Math.floor(R / 2) + 3; // the lane the patterns travel along
		const bx0 = Math.floor(C / 2) - 4;
		const bx1 = bx0 + 8;
		const by0 = mid - 4;
		const by1 = mid + 4;
		const wall = (i: number, j: number) =>
			((i === bx0 || i === bx1) && j >= by0 && j <= by1) || ((j === by0 || j === by1) && i >= bx0 && i <= bx1);
		const sx = bx0 + 2; // the shelf's slot, five by five, above the square
		const sy = by0 - 7;
		const outX = C - 7;

		// The square: solid, sealed; dimmed while the stored copy answers for it.
		const wallA = (1 - 0.65 * ramp(t, 6.8, 7.4) * (1 - ramp(t, 9.2, 9.8))) * fade;
		for (let i = bx0; i <= bx1; i++) for (let j = by0; j <= by1; j++) if (wall(i, j)) g.cell(i, j, k.mid, wallA);
		// The shelf: a dotted outline, a place to keep things rather than a seal.
		for (let i = sx - 1; i <= sx + 5; i++)
			for (let j = sy - 1; j <= sy + 5; j++) {
				const edge = i === sx - 1 || i === sx + 5 || j === sy - 1 || j === sy + 5;
				if (edge && (i + j) % 2 === 0) g.cell(i, j, k.faint, 0.8 * fade);
			}

		// First time: in, worked on inside the square, out.
		if (t < 2.3) stamp(g, MACRO_IN, lerp(1, bx0 + 3, ramp(t, 0.5, 2.3)), mid - 1, k.ink, ramp(t, 0.3, 0.6), wall);
		if (t >= 2.3 && t < 3.5) {
			// The work: noise that settles into the expansion.
			const p = ramp(t, 2.3, 3.4);
			const step = Math.floor(t * 12);
			for (let u = 0; u < 7; u++)
				for (let v = 0; v < 7; v++) {
					const bit = u > 0 && u < 6 && v > 0 && v < 6 ? MACRO_OUT[v - 1][u - 1] : 0;
					const h = hash(u, v, step);
					if (bit ? h < 0.35 + 0.65 * p : h < 0.45 * (1 - p)) g.cell(bx0 + 1 + u, by0 + 1 + v, k.ink, 1);
				}
		}
		if (t >= 3.5) {
			// The kept copy rises onto the shelf and stays.
			const up = ramp(t, 3.5, 4.5);
			stamp(g, MACRO_OUT, sx, lerp(mid - 2, sy, up), k.mid, fade, wall);
			// And the result goes out to the right.
			const out = ramp(t, 3.5, 5);
			if (t < 6.3) stamp(g, MACRO_OUT, lerp(bx0 + 2, outX, out), mid - 2, k.ink, 1 - ramp(t, 5.8, 6.3), wall);
		}

		// Second time: the same pattern stops at the square; the shelf answers.
		if (t >= 5.6 && t < 7.5) stamp(g, MACRO_IN, lerp(1, bx0 - 4, ramp(t, 5.6, 7.2)), mid - 1, k.ink, 1 - ramp(t, 7.2, 7.5));
		if (t >= 7.2) {
			stamp(g, MACRO_OUT, sx, sy, k.fire, ramp(t, 7.2, 7.3) * (1 - ramp(t, 7.6, 8.8)) * fade);
			// A copy goes round the square, along the shelf's row and down, never through it.
			const route = path([
				[sx, sy],
				[outX, sy],
				[outX, mid - 2],
			]);
			const [x, y] = along(route, ramp(t, 7.4, 8.7));
			stamp(g, MACRO_OUT, x, y, k.ink, fade);
		}
	},
};

// 2. Hermetic builds. A sealed box. Inside, three inputs send their cells along fixed wires to one output, which is
// built the same, cell for cell, on every loop. Outside, specks drift and bounce off the wall; none gets in.
const HERMETIC_OUT: Glyph = [
	[1, 1, 0, 1],
	[0, 1, 1, 1],
	[1, 1, 1, 0],
	[1, 0, 1, 1],
];

const hermetic: Piece = {
	period: 6,
	still: 2.8,
	draw(g, k, t, _loop, time) {
		const { cols: C, rows: R } = g;
		const bw = Math.min(C - 14, 30);
		const bx0 = Math.floor((C - bw) / 2);
		const bx1 = bx0 + bw - 1;
		const by0 = 2;
		const by1 = R - 3;
		for (let i = bx0; i <= bx1; i++)
			for (let j = by0; j <= by1; j++)
				if (i === bx0 || i === bx1 || j === by0 || j === by1) g.cell(i, j, k.mid, 1);

		// The noise outside: each speck bounces inside its own region, which stops one cell short of the wall.
		const regions: [number, number, number, number, number][] = [
			// x0, x1, y0, y1, how many
			[0, bx0 - 2, 0, R - 1, Math.max(3, Math.floor(((bx0 - 1) * R) / 18))],
			[bx1 + 2, C - 1, 0, R - 1, Math.max(3, Math.floor(((C - bx1 - 2) * R) / 18))],
			[bx0, bx1, 0, by0 - 2, 3],
			[bx0, bx1, by1 + 2, R - 1, 3],
		];
		const tri = (u: number) => 1 - Math.abs(2 * (u - Math.floor(u)) - 1);
		regions.forEach(([x0, x1, y0, y1, n], z) => {
			const w = x1 - x0;
			const h = y1 - y0;
			if (w < 0 || h < 0) return;
			for (let m = 0; m < n; m++) {
				const r = (q: number) => hash(z, m, q);
				const x = x0 + tri(r(1) + (time * (0.4 + r(2))) / (2 * Math.max(1, w))) * w;
				const y = y0 + tri(r(3) + (time * (0.3 + r(4))) / (2 * Math.max(1, h))) * h;
				g.cell(x, y, k.faint, 0.5 + 0.5 * r(5));
			}
		});

		// Inside: the inputs, their wires, and the output.
		const c = Math.floor((by0 + by1) / 2);
		const ox = bx1 - 6;
		const mx = bx0 + Math.floor(bw / 2);
		const ys = [c - 6, c, c + 5];
		const outA = ramp(t, 1.7, 2.4) * (1 - ramp(t, 4.8, 5.4));
		ys.forEach((y, m) => {
			const wire = path([
				[bx0 + 5, y],
				[mx, y],
				[mx, c],
				[ox - 1, c],
			]);
			for (const [i, j] of wire) g.cell(i, j, k.faint, 0.55);
			const send = 1 - ramp(t, 1.6, 2.2);
			for (let u = 0; u < 2; u++) for (let v = 0; v < 2; v++) g.cell(bx0 + 3 + u, y + v, k.ink, 0.75 + 0.25 * send);
			if (t > 0.5 + m * 0.1 && t < 1.8) packet(g, wire, ramp(t, 0.5 + m * 0.1, 1.7), k.ink);
		});
		// The output builds cell by cell in the same order every loop.
		HERMETIC_OUT.forEach((row, v) =>
			row.forEach((on, u) => {
				const n = v * 4 + u;
				if (on) g.cell(ox + u, c - 1 + v, k.ink, outA * ramp(t, 1.7 + n * 0.035, 1.8 + n * 0.035));
			}),
		);
	},
};

// 3. Fast rebuilds and tests. A lattice of functions. One changes (the accent); the change travels along the wires to
// the few functions that depend on it, which light up and settle; the rest of the lattice never moves. Each loop
// another function changes.
const incremental: Piece = {
	period: 5.5,
	still: 2.6,
	draw(g, k, t, loop) {
		const { cols: C, rows: R } = g;
		const NC = Math.floor((C + 1) / 2);
		const NR = Math.floor((R + 1) / 2);
		const ox = Math.floor((C - (2 * NC - 1)) / 2);
		const oy = Math.floor((R - (2 * NR - 1)) / 2);
		const at = (a: number, b: number): Pt => [ox + 2 * a, oy + 2 * b];
		for (let a = 0; a < NC; a++) for (let b = 0; b < NR; b++) g.cell(...at(a, b), k.faint, 0.8);

		const r = (q: number) => hash(loop, q, 77);
		const clampB = (b: number) => Math.min(NR - 1, Math.max(0, b));
		const root: Pt = [2 + Math.floor(r(1) * Math.max(1, NC - 10)), 1 + Math.floor(r(2) * Math.max(1, NR - 2))];
		type Dep = { node: Pt; from: Pt; depth: number };
		const deps: Dep[] = [];
		const seen = new Set([root.join()]);
		const add = (node: Pt, from: Pt, depth: number) => {
			if (seen.has(node.join())) return;
			seen.add(node.join());
			deps.push({ node, from, depth });
		};
		const first = 2 + (r(3) < 0.5 ? 1 : 0);
		for (let m = 0; m < first; m++) add([root[0] + 1 + Math.floor(r(10 + m) * 3), clampB(root[1] + Math.floor(r(20 + m) * 5) - 2)], root, 1);
		for (const [m, d] of deps.filter((d) => d.depth === 1).entries())
			if (r(30 + m) < 0.6) add([d.node[0] + 1 + Math.floor(r(40 + m) * 2), clampB(d.node[1] + Math.floor(r(50 + m) * 3) - 1)], d.node, 2);

		const settle = 1 - ramp(t, 3.4, 4.6);
		for (const { node, from, depth } of deps) {
			const [x0, y0] = at(...from);
			const [x1, y1] = at(...node);
			// Wires run along odd rows and columns, between the functions, so they never touch another one.
			const wire = path([
				[x0, y0 + 1],
				[x1 - 1, y0 + 1],
				[x1 - 1, y1],
			]);
			const start = depth === 1 ? 0.7 : 1.6;
			const drawn = ramp(t, start, start + 0.6) * wire.length;
			const wireA = 0.8 * (1 - ramp(t, 3, 3.8));
			wire.forEach(([i, j], n) => n < drawn && g.cell(i, j, k.mid, wireA));
			g.cell(x1, y1, k.ink, ramp(t, start + 0.6, start + 0.75) * settle);
		}
		g.cell(...at(...root), k.fire, ramp(t, 0.4, 0.55) * (1 - ramp(t, 3.6, 4.6)));
	},
};

// 4. Remote execution. One block hands work out along wires to a column of blocks; each works for its own time and
// sends the result back, and the results stack up beside the first block.
const remote: Piece = {
	period: 8,
	still: 3,
	draw(g, k, t) {
		const { cols: C, rows: R } = g;
		const fade = 1 - ramp(t, 7, 7.7);
		const mid = Math.floor(R / 2);
		const ox = Math.max(3, Math.round(C * 0.12));
		const n = Math.floor((R - 1) / 3);
		const top = Math.floor((R - (3 * n - 1)) / 2);
		const wx = C - Math.max(4, Math.round(C * 0.1)) - 3;
		const bus = Math.round(ox + 3 + (wx - ox - 3) * 0.4);
		for (let u = 0; u < 3; u++) for (let v = -1; v <= 1; v++) g.cell(ox + u, mid + v, k.ink, 0.9);

		const jobs = Array.from({ length: n }, (_, m) => {
			const y = top + 3 * m;
			const wire = path([
				[ox + 3, mid],
				[bus, mid],
				[bus, y],
				[wx - 1, y],
			]);
			const sent = 0.4 + m * 0.12;
			const arrived = sent + 1.1;
			const done = arrived + 0.8 + 1.8 * hash(m, 4);
			return { y, wire, sent, arrived, done, back: done + 1.1 };
		});
		const order = [...jobs].sort((a, b) => a.back - b.back);
		for (const { wire } of jobs) for (const [i, j] of wire) g.cell(i, j, k.faint, 0.6);
		jobs.forEach(({ y, wire, sent, arrived, done, back }, m) => {
			const step = Math.floor(t * 8);
			for (let u = 0; u < 3; u++)
				for (let v = 0; v < 2; v++) {
					const busy = t >= arrived && t < done && hash(m * 8 + u, v, step) < 0.6;
					g.cell(wx + u, y + v, busy ? k.ink : k.mid, busy ? 1 : 0.6);
				}
			if (t >= sent && t < arrived + 0.1) packet(g, wire, ramp(t, sent, arrived), k.ink);
			if (t >= done && t < back + 0.1) packet(g, [...wire].reverse(), ramp(t, done, back), k.ink);
		});
		// The results, one cell each, bottom up in the order they came back.
		const tray = ox - 2;
		const bottom = mid + Math.floor(n / 2);
		order.forEach(({ back }, rank) => {
			const lit = ramp(t, back, back + 0.15) * fade;
			g.cell(tray, bottom - rank, k.faint, 0.45 * (1 - lit));
			g.cell(tray, bottom - rank, k.ink, lit);
		});
	},
};

const PIECES: Record<string, Piece> = { cache, hermetic, incremental, remote };
const still = matchMedia('(prefers-reduced-motion: reduce)');
const dark = matchMedia('(prefers-color-scheme: dark)');

function inks(el: Element): Inks {
	const s = getComputedStyle(el);
	const v = (name: string, fallback: string) => s.getPropertyValue(name).trim() || fallback;
	return { ink: v('--art-ink', '#ebeae6'), mid: v('--art-mid', '#878684'), faint: v('--art-faint', '#57575a'), fire: v('--art-fire', '#e0461a') };
}

function mount(el: HTMLElement, signal: AbortSignal): () => void {
	const piece = PIECES[el.dataset.piece ?? ''];
	const canvas = el.querySelector('canvas');
	const ctx = canvas?.getContext('2d');
	if (!piece || !canvas || !ctx) return () => {};
	const goal = el.closest('details');
	let k = inks(el);
	let clock = 0;
	let last = 0;
	let raf = 0;
	let seen = false;
	const paint = () => {
		const box = el.getBoundingClientRect();
		if (box.width === 0 || box.height === 0) return;
		const ratio = Math.min(devicePixelRatio || 1, 3);
		const w = Math.round(box.width * ratio);
		const h = Math.round(box.height * ratio);
		if (canvas.width !== w || canvas.height !== h) {
			canvas.width = w;
			canvas.height = h;
		}
		ctx.setTransform(1, 0, 0, 1, 0, 0);
		ctx.clearRect(0, 0, w, h);
		const time = still.matches ? piece.still : clock;
		const loop = Math.floor(time / piece.period);
		piece.draw(grid(ctx, w, h, ratio), k, time - loop * piece.period, loop, time);
		ctx.globalAlpha = 1;
	};
	const tick = (now: number) => {
		raf = requestAnimationFrame(tick);
		if (last !== 0 && now - last < 1000 / FPS - 2) return;
		// The clock moves only while drawing, and never more than a tenth of a second at once, so a pause resumes.
		if (last !== 0) clock += Math.min(now - last, 100) / 1000;
		last = now;
		paint();
	};
	const sync = () => {
		const run = !still.matches && (goal?.open ?? true) && seen && !document.hidden;
		if (run && raf === 0) {
			last = 0;
			raf = requestAnimationFrame(tick);
		} else if (!run && raf !== 0) {
			cancelAnimationFrame(raf);
			raf = 0;
		}
	};
	const io = new IntersectionObserver((entries) => {
		seen = entries.some((e) => e.isIntersecting);
		sync();
	});
	io.observe(el);
	const ro = new ResizeObserver(paint);
	ro.observe(el);
	goal?.addEventListener(
		'toggle',
		() => {
			if (goal.open) {
				clock = 0;
				paint();
			}
			sync();
		},
		{ signal },
	);
	document.addEventListener('visibilitychange', sync, { signal });
	dark.addEventListener(
		'change',
		() => {
			k = inks(el);
			paint();
		},
		{ signal },
	);
	still.addEventListener(
		'change',
		() => {
			paint();
			sync();
		},
		{ signal },
	);
	return () => {
		io.disconnect();
		ro.disconnect();
		cancelAnimationFrame(raf);
		raf = 0;
	};
}

onPage((signal) => {
	const stops = [...document.querySelectorAll<HTMLElement>('.art[data-piece]')].map((el) => mount(el, signal));
	return () => stops.forEach((stop) => stop());
});
