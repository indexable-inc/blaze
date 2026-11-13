/**
 * The intro's exit: the page burns away. On Start a fire sweeps across and up through the window, from the bottom
 * left corner to the top right, on the wordmark's own grid of cells: each cell the front reaches glows white hot,
 * turns to flame, darkens and is gone, and what was under it (the line, the button, the links, the word) with it. A
 * few cells crumble into embers that drift up and go out. After anim's exit (src/blaze.rs in indexable-inc/anim:
 * the burn front that climbs into the letters), on the page's cells instead of the terminal's.
 *
 * A pure function of time: `draw(t)` paints the frame `t` seconds after the fire starts, so the same moment always
 * draws the same frame. The clock, the skip and the navigation are client/intro.ts's.
 *
 * Under client/lib/ (not bundled as an entry).
 */

import { type Theme, hash, noise } from './wordmark';

/** Seconds for the front to cross the page, and how far its tongues run ahead of it or lag. */
const SWEEP = 0.44;
const RAGGED = 0.12;
/** How much of the sweep goes to climbing (the rest to crossing): mostly up, a little across. */
const UP = 0.78;
/** A burning cell's life, in seconds after the front reaches it: the ages at which it steps down the palette, brightest first, and is gone. */
const FLAME = [0.03, 0.065, 0.1, 0.14, 0.18];
const GONE = FLAME[4]!;
/** One cell in this many leaves an ember, which rises this many cells a second for this long. */
const EMBERS = 0.03;
const RISE = 26;
const EMBER_LIFE = 0.4;
/** Every cell is gone: the page may go. Within the 0.6 to 0.9 s an exit can take without feeling like a wait. */
export const BURNT = SWEEP + RAGGED + GONE;

export type Burn = {
	canvas: HTMLCanvasElement;
	/**
	 * Paints the frame `t` seconds after the fire starts; before 0 nothing has caught. A burnt cell is the page's
	 * background (`ash`), or with `reveal` nothing, so what is under the canvas shows through it.
	 */
	draw(t: number, reveal?: boolean): void;
	/** The cells not yet burnt at `t`, as a `clip-path` for a layer over the whole window (CSS px). */
	standing(t: number): string;
	/** Every cell is gone and every ember out. */
	readonly out: number;
};

/**
 * A canvas over the whole window, its cells `px` device pixels wide and lined up with the wordmark's (`ox`, `oy`:
 * a cell corner of the wordmark, in device pixels from the window's top left). `ash` is what a burnt cell leaves:
 * the page's own background.
 */
export function burn(px: number, ox: number, oy: number, theme: Theme, ash: string): Burn {
	const dpr = devicePixelRatio || 1;
	const canvas = document.createElement('canvas');
	canvas.className = 'intro-burn';
	canvas.width = Math.ceil(innerWidth * dpr);
	canvas.height = Math.ceil(innerHeight * dpr);
	canvas.style.width = `${canvas.width / dpr}px`;
	canvas.style.height = `${canvas.height / dpr}px`;
	const ctx = canvas.getContext('2d')!;
	// The first cell starts at or before the window's corner.
	const x0 = (((ox % px) + px) % px) - px;
	const y0 = (((oy % px) + px) % px) - px;
	const cols = Math.ceil((canvas.width - x0) / px);
	const rows = Math.ceil((canvas.height - y0) / px);
	// When the front reaches each cell: up from the bottom and across from the left, with tongues that run ahead.
	const reach = new Float32Array(cols * rows);
	for (let y = 0; y < rows; y++) {
		for (let x = 0; x < cols; x++) {
			const climb = 1 - (y + 0.5) / rows;
			const cross = (x + 0.5) / cols;
			reach[y * cols + x] = SWEEP * (UP * climb + (1 - UP) * cross) + RAGGED * (0.75 * noise(7, x, y, 5, 9) + 0.25 * hash(8, x, y));
		}
	}
	const cell = (x: number, y: number, color: string) => {
		ctx.fillStyle = color;
		ctx.fillRect(x0 + x * px, y0 + y * px, px, px);
	};
	return {
		canvas,
		out: BURNT + EMBER_LIFE,
		standing(t) {
			// One rectangle per run of standing cells in a row; nonzero winding unions them.
			let d = '';
			const at = (v: number) => +(v / dpr).toFixed(2);
			for (let y = 0; y < rows; y++) {
				let x = 0;
				while (x < cols) {
					while (x < cols && t - reach[y * cols + x]! >= GONE) x++;
					const from = x;
					while (x < cols && t - reach[y * cols + x]! < GONE) x++;
					if (x > from) d += `M${at(x0 + from * px)} ${at(y0 + y * px)}H${at(x0 + x * px)}V${at(y0 + (y + 1) * px)}H${at(x0 + from * px)}Z`;
				}
			}
			return `path('${d || 'M0 0Z'}')`;
		},
		draw(t, reveal = false) {
			ctx.clearRect(0, 0, canvas.width, canvas.height);
			if (t >= BURNT + EMBER_LIFE) {
				if (reveal) return;
				ctx.fillStyle = ash;
				ctx.fillRect(0, 0, canvas.width, canvas.height);
				return;
			}
			for (let y = 0; y < rows; y++) {
				for (let x = 0; x < cols; x++) {
					const age = t - reach[y * cols + x]!;
					if (age < 0) continue;
					if (age >= GONE) {
						if (!reveal) cell(x, y, ash);
					}
					else cell(x, y, theme.fire[4 - FLAME.findIndex((a) => age < a)]!);
				}
			}
			// The embers, over the ash: each rises from its cell, cooling as it goes.
			for (let y = 0; y < rows; y++) {
				for (let x = 0; x < cols; x++) {
					if (hash(11, x, y) >= EMBERS) continue;
					const life = (t - reach[y * cols + x]! - GONE) / EMBER_LIFE;
					if (life < 0 || life >= 1) continue;
					const sway = Math.round(1.5 * Math.sin(9 * life + 6 * hash(12, x, y)));
					cell(x + sway, y - Math.round(RISE * EMBER_LIFE * life), theme.fire[life < 0.35 ? 2 : life < 0.7 ? 1 : 0]!);
				}
			}
		},
	};
}
