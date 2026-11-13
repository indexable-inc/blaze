/**
 * The blaze wordmark, live: "blaze" in pixel letters, set on fire. A port of ix's BlazeLogo.svelte
 * (indexable-inc/ix, packages/web/src/lib/site), itself a port of anim's src/blaze.rs (indexable-inc/anim), itself
 * this repo's docs/brand/fire/gen.py: the same letters, palettes, automaton and timeline. The letters drop in one by
 * one, each landing knocking the word; a beat after the last lands, the fire catches from the b to the e and burns
 * on. The pointer's breath parts the flames and its sweep drags them.
 *
 * Everything sits on a grid of square cells. Each step a cell takes the heat of a cell below it (shifted a cell left
 * or right now and then) minus a little; the cells just above the letters' top edges are the heat source.
 *
 * This file is the model and the pen: the grid, one fire step, and one frame drawn at a whole number of device
 * pixels per cell. The clock, the canvas's size and the pointer are client/intro.ts's.
 *
 * Under client/lib/ (not bundled as an entry).
 */

import { DROP_DUR, DROP_START, FPS, IGNITE } from './intro-timeline';

const GLYPHS = [
	// b
	['XX.....', 'XX.....', 'XX.....', 'XX.....', 'XXXXXX.', 'XXXXXXX', 'XX...XX', 'XX...XX', 'XX...XX', 'XX...XX', 'XXXXXXX', 'XXXXXX.'],
	// l
	['XX', 'XX', 'XX', 'XX', 'XX', 'XX', 'XX', 'XX', 'XX', 'XX', 'XX', 'XX'],
	// a
	['.......', '.......', '.......', '.......', '.XXXXX.', 'XXXXXXX', '.....XX', '.XXXXXX', 'XXXXXXX', 'XX...XX', 'XXXXXXX', '.XXXXXX'],
	// z
	['.......', '.......', '.......', '.......', 'XXXXXXX', 'XXXXXXX', '....XXX', '...XXX.', '..XXX..', '.XXX...', 'XXXXXXX', 'XXXXXXX'],
	// e
	['.......', '.......', '.......', '.......', '.XXXXX.', 'XXXXXXX', 'XX...XX', 'XXXXXXX', 'XXXXXXX', 'XX.....', 'XXXXXXX', '.XXXXX.'],
];
const LETTER_GAP = 2;
/** Room for the flames above the letters, beside them, and the shadow's row under them. */
const ABOVE = 13;
const SIDE = 2;
const BELOW = 1;
/** Rows at the very top the flames never reach (they are cooled hard there). */
const CROP = 3;
/** Rows above the frame the letters fall through. */
export const LEAD = 12;
const ROWS = GLYPHS[0]!.length;

const HEAT = 12;
const COOL_MIN = 0.4;
const COOL_MAX = 2.3;
/** Heat level (1..=HEAT) to palette index. */
const LEVELS = [0, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4];
/** Each landing kicks its letter up a little, decaying over this long. */
const HOP = 0.22;
/** How far the pointer's breath reaches, and its sweep, in cells. */
const BREATH = 5;
const GUST_REACH = 10;

export type Theme = { ink: string; shade: string; fire: readonly string[] };
/** The brand's two palettes: the letters' ink, their shadow, and the fire from its darkest to its brightest. */
export const THEMES: Record<'dark' | 'light', Theme> = {
	dark: { ink: '#f0f6fc', shade: '#6e2a14', fire: ['#a8240f', '#e0461a', '#ff8f1f', '#ffd43b', '#fff6c2'] },
	light: { ink: '#1f2328', shade: '#e8c3a0', fire: ['#9a1f0c', '#d63c12', '#f57a10', '#ffae1a', '#ffd84a'] },
};

// The grid: letter cells carry their letter's index, shadows theirs too; -1 is air.
const letters: [number, number, number][] = [];
let x0 = 0;
GLYPHS.forEach((rows, n) => {
	rows.forEach((row, y) => [...row].forEach((c, dx) => c === 'X' && letters.push([x0 + dx, y, n])));
	x0 += rows[0]!.length + (n + 1 < GLYPHS.length ? LETTER_GAP : 0);
});
/** The frame, in cells: its width, and the rows shown (the grid without its top CROP rows). */
export const W = x0 + 2 * SIDE + 1;
const H = ABOVE + ROWS + BELOW;
export const SHOWN = H - CROP;
const letterOf = new Int8Array(W * H).fill(-1);
const shadowOf = new Int8Array(W * H).fill(-1);
/** Per column, the row just above its topmost letter cell: where the fire starts. -1 for a column with no letter. */
const source = new Int16Array(W).fill(-1);
for (const [x, y, n] of letters) {
	const gx = x + SIDE;
	const gy = y + ABOVE;
	letterOf[gy * W + gx] = n;
	source[gx] = source[gx] === -1 ? gy - 1 : Math.min(source[gx]!, gy - 1);
}
for (const [x, y, n] of letters) {
	const i = (y + ABOVE + 1) * W + x + SIDE + 1;
	if (letterOf[i] === -1) shadowOf[i] = n;
}

function hash(seed: number, i: number, j: number): number {
	let h = Math.imul(seed + 0x9e3779b9, 0x85ebca6b) ^ Math.imul(i + 0x7f4a7c15, 0xc2b2ae35) ^ Math.imul(j + 0x165667b1, 0x27d4eb2f);
	h = Math.imul(h ^ (h >>> 15), 0x85ebca6b);
	h = Math.imul(h ^ (h >>> 13), 0xc2b2ae35);
	return ((h ^ (h >>> 16)) >>> 8) / 16777216;
}

/** Smooth value noise over (x, k), `sx` and `sk` cells to a lattice step. */
function noise(seed: number, x: number, k: number, sx: number, sk: number): number {
	const fx = x / sx;
	const fk = k / sk;
	const xi = Math.floor(fx);
	const ki = Math.floor(fk);
	const s = (v: number): number => v * v * (3 - 2 * v);
	const tx = s(fx - xi);
	const tk = s(fk - ki);
	const top = hash(seed, xi, ki) * (1 - tx) + hash(seed, xi + 1, ki) * tx;
	const bottom = hash(seed, xi, ki + 1) * (1 - tx) + hash(seed, xi + 1, ki + 1) * tx;
	return top * (1 - tk) + bottom * tk;
}

/** The heat fed in above column x at step t: three slow waves, so the flames lean and swell rather than flicker. */
function sourceHeat(x: number, t: number): number {
	const p = (2 * Math.PI * (t % 32)) / 32;
	const w =
		0.55 * Math.sin((2 * Math.PI * x) / 6 + 0.7) * Math.cos(p) +
		0.35 * Math.sin((2 * Math.PI * x) / 3.7 + 1.9) * Math.cos(2 * p + 0.5) +
		0.25 * Math.sin((2 * Math.PI * x) / 9 + 0.3) * Math.sin(3 * p);
	return Math.min(HEAT, Math.max(0, Math.round(HEAT * (0.72 + 0.4 * w))));
}

/** How far letter n sits above its place at t, in cells, or null before it starts to fall. */
function dropOffset(n: number, t: number): number | null {
	const start = DROP_START[n]!;
	if (t < start) return null;
	const p = Math.min(1, (t - start) / DROP_DUR);
	let dy = -(LEAD + ABOVE - CROP) * (1 - p * p);
	const since = t - start - DROP_DUR;
	if (since >= 0 && since < HOP) {
		const q = since / HOP;
		dy -= 0.8 * Math.sin(Math.PI * q) * (1 - q);
	}
	return dy;
}

/** How much the word shakes at t, in cells, from the letters landing (the last one hardest). */
export function knock(t: number): [number, number] {
	let kx = 0;
	let ky = 0;
	DROP_START.forEach((start, i) => {
		const since = t - start - DROP_DUR;
		if (since >= 0 && since < 0.2) {
			const amp = (i === DROP_START.length - 1 ? 0.6 : 0.35) * (1 - since / 0.2) ** 2;
			kx += amp * 0.4 * Math.sin(2 * Math.PI * 31 * since);
			ky += amp * Math.sin(2 * Math.PI * 24 * since + 0.7);
		}
	});
	return [kx, ky];
}

/** The pointer, in cells of the grid, or null when it is away. */
export type Pointer = { x: number; y: number } | null;

/**
 * The fire: its heat per cell and how many steps it has burnt. `step` is one tick of the automaton; the wind in it is
 * the pointer: its breath pushes the flames away to either side and parts them where it is, and `gust` (its sweep,
 * decaying) drags them along.
 */
export class Fire {
	heat = new Float32Array(W * H);
	private next = new Float32Array(W * H);
	steps = 0;
	pointer: Pointer = null;
	gust = 0;

	/** Out, as before the word catches. */
	reset() {
		this.heat.fill(0);
		this.steps = 0;
		this.gust = 0;
	}

	/** The pointer moved to (x, y), in cells of the frame as shown. */
	blow(x: number, y: number) {
		const gy = y + CROP;
		if (this.pointer) this.gust = Math.max(-1.5, Math.min(1.5, this.gust + 0.15 * (x - this.pointer.x)));
		this.pointer = { x, y: gy };
	}

	step() {
		const { heat, next, steps, pointer, gust } = this;
		const since = steps / FPS;
		next.fill(0);
		for (let y = 0; y < H - 1; y++) {
			for (let x = 0; x < W; x++) {
				const i = y * W + x;
				if (letterOf[i] !== -1) continue;
				if (source[x] === y) {
					// The fire catches left to right, each column fading up.
					const lit = Math.min(1, Math.max(0, ((since / IGNITE) * W - x) / 4));
					next[i] = Math.round(sourceHeat(x, steps) * lit);
					continue;
				}
				// A parcel of heat rises a cell a step, so y + t is constant along its path: tongues, not sparks.
				const k = y + steps;
				let w = 0;
				let parted = 0;
				if (pointer) {
					const dx = x + 0.5 - pointer.x;
					const dy = y + 0.5 - pointer.y;
					const d2 = dx * dx + dy * dy;
					const breath = ((2.3 * dx) / BREATH) * Math.exp(-d2 / (BREATH * BREATH));
					w = Math.max(-1, Math.min(1, breath + gust * Math.exp(-d2 / (GUST_REACH * GUST_REACH))));
					parted = 3 * Math.exp(-d2 / 4);
				}
				const right = 0.16 + 0.7 * Math.max(0, w) - 0.16 * Math.max(0, -w);
				const left = 0.16 + 0.7 * Math.max(0, -w) - 0.16 * Math.max(0, w);
				const sway = noise(1, x, k, 5, 4);
				const dx = sway < left ? -1 : sway > 1 - right ? 1 : 0;
				const sx = Math.min(W - 1, Math.max(0, x - dx));
				let cool = COOL_MIN + (COOL_MAX - COOL_MIN) * noise(2, x, k, 2.2, 4) ** 1.5 + parted;
				cool += 2.5 * Math.max(0, 4 - y); // near the top edge, so no flame is cut off flat
				const v = heat[(y + 1) * W + sx]! - cool;
				next[i] = v > 0 ? v : 0;
			}
		}
		this.heat = next;
		this.next = heat;
		this.steps = steps + 1;
		this.gust = gust * 0.85;
	}
}

/**
 * Draws frame t (seconds on the intro's timeline; before 0 the frame is empty) into a canvas of W by SHOWN + LEAD
 * cells at `px` device pixels a cell: the fire first, then the shadows, then the letters, so falling letters cover the
 * rest. Every cell is a whole number of device pixels, so the pixels stay sharp.
 */
export function draw(ctx: CanvasRenderingContext2D, fire: Fire, t: number, px: number, theme: Theme) {
	ctx.clearRect(0, 0, W * px, (SHOWN + LEAD) * px);
	const rect = (x: number, y: number, color: string) => {
		// A falling letter sits between rows: its edges round to device pixels, so it never blurs.
		const top = Math.round((y + LEAD) * px);
		ctx.fillStyle = color;
		ctx.fillRect(x * px, top, px, Math.round((y + 1 + LEAD) * px) - top);
	};
	const heat = fire.heat;
	for (let y = 0; y < SHOWN; y++) {
		for (let x = 0; x < W; x++) {
			const i = (y + CROP) * W + x;
			const h = heat[i]! | 0;
			if (letterOf[i] === -1 && h >= 1) rect(x, y, theme.fire[LEVELS[Math.min(h, LEVELS.length) - 1]!]!);
		}
	}
	const offsets = GLYPHS.map((_, n) => dropOffset(n, t));
	for (const pass of [shadowOf, letterOf]) {
		const color = pass === letterOf ? theme.ink : theme.shade;
		for (let y = 0; y < SHOWN; y++) {
			for (let x = 0; x < W; x++) {
				const n = pass[(y + CROP) * W + x]!;
				const dy = n === -1 ? null : offsets[n];
				if (dy == null) continue;
				rect(x, y + dy, color);
			}
		}
	}
}
