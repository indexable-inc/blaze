/**
 * The faces of anonymous readers: one small animal each, the animal in their name ("Quiet Otter" is an otter). Drawn
 * here from a kit of parts (heads, ears, eyes, snouts, markings, extras) and compiled by gen/build.ts into one static
 * sprite, dist/animals.<hash>.svg, a `<symbol id="animal-otter">` per animal. The client (client/lib/presence.ts) gets
 * the names and the sprite's file through `BLAZE_ANIMALS` and draws `<svg><use href="…#animal-otter"/></svg>`; no
 * drawing code ships to the browser.
 *
 * One flat style: a front-facing head on a 32x32 canvas, in tone slots rather than colours. Each shape is filled with
 * `var(--a-bg)` (the round background), `var(--a1)` (the body), `var(--a2)` (darker: inner ears, stripes, masks),
 * `var(--a3)` (lighter: muzzles, bellies, faces) or `var(--a-ink)` (eyes, noses); styles/shell.css sets them on
 * `.face.animal` from the person's hue (`--h`) for the light and dark themes. Presentation attributes rather than
 * classes or `style`, so neither the page's CSP (no inline styles) nor the `<use>` boundary gets in the way.
 *
 * Coordinates are absolute, x = 16 is the middle; a shape marked `pair` is drawn again mirrored, so parts are written
 * for the left side only.
 */

export type Tone = 'bg' | 'a1' | 'a2' | 'a3' | 'ink';

/** One shape: a path, filled, or stroked with round caps when `stroke` is set; `pair` adds its mirror image. */
export type Shape = { tone: Tone; d: string; stroke?: number; pair?: boolean };
export type Part = Shape[];

const r2 = (n: number) => String(Math.round(n * 100) / 100);
const pts = (...ns: number[]) => ns.map(r2).join(' ');

// ---- primitives ----

export const path = (tone: Tone, d: string, pair = false): Shape => ({ tone, d, pair });
export const line = (tone: Tone, w: number, d: string, pair = false): Shape => ({ tone, d, stroke: w, pair });
export const ellipse = (tone: Tone, cx: number, cy: number, rx: number, ry: number, rot = 0, pair = false): Shape => {
	// Two arcs from the ends of the major axis, rotated by `rot` degrees.
	const a = (rot * Math.PI) / 180;
	const ex = Math.cos(a) * rx;
	const ey = Math.sin(a) * rx;
	return {
		tone,
		pair,
		d: `M${pts(cx - ex, cy - ey)}A${pts(rx, ry, rot)} 1 0 ${pts(cx + ex, cy + ey)}A${pts(rx, ry, rot)} 1 0 ${pts(cx - ex, cy - ey)}Z`,
	};
};
export const circle = (tone: Tone, cx: number, cy: number, r: number, pair = false) => ellipse(tone, cx, cy, r, r, 0, pair);
/** A rectangle with corners rounded by `r`. */
export const rrect = (tone: Tone, x: number, y: number, w: number, h: number, r: number, pair = false): Shape =>
	path(tone, `M${pts(x + r, y)}H${r2(x + w - r)}Q${pts(x + w, y, x + w, y + r)}V${r2(y + h - r)}Q${pts(x + w, y + h, x + w - r, y + h)}H${r2(x + r)}Q${pts(x, y + h, x, y + h - r)}V${r2(y + r)}Q${pts(x, y, x + r, y)}Z`, pair);

/** A ring of `n` bumps (`depth` deep) around a circle, from angle `from` to `to` (degrees, 0 = right, clockwise). */
function bumpy(tone: Tone, cx: number, cy: number, r: number, n: number, depth: number, pointed: boolean, from = 0, to = 360): Shape {
	const at = (deg: number, rr: number) => [cx + Math.cos((deg * Math.PI) / 180) * rr, cy + Math.sin((deg * Math.PI) / 180) * rr];
	const step = (to - from) / n;
	const closed = to - from >= 360;
	let d = closed ? `M${pts(...at(from, r))}` : `M${pts(cx, cy)}L${pts(...at(from, r))}`;
	for (let i = 0; i < n; i++) {
		const mid = from + step * (i + 0.5);
		const end = from + step * (i + 1);
		d += pointed ? `L${pts(...at(mid, r + depth))}L${pts(...at(end, r))}` : `Q${pts(...at(mid, r + depth * 2))} ${pts(...at(end, r))}`;
	}
	return { tone, d: `${d}Z` };
}

// ---- heads ----

type HeadOpts = { tone?: Tone; cx?: number; cy?: number; rx?: number; ry?: number };
export const head = {
	/** A round head (bear, otter, tiger). */
	round: ({ tone = 'a1', cx = 16, cy = 18, rx = 10, ry = 9.5 }: HeadOpts = {}): Part => [ellipse(tone, cx, cy, rx, ry)],
	/** Wide cheeks narrowing to a chin at `chin` (fox, badger). */
	pointed: ({ tone = 'a1', top = 9, chin = 28, w = 12 }: { tone?: Tone; top?: number; chin?: number; w?: number } = {}): Part => [
		path(tone, `M${pts(16 - w, top + 5)}Q${pts(16 - w, top, 16, top)}Q${pts(16 + w, top, 16 + w, top + 5)}Q${pts(16 + w - 1, top + 12, 16, chin)}Q${pts(16 - w + 1, top + 12, 16 - w, top + 5)}Z`),
	],
	/** A long face, narrow brow to a wide muzzle (moose, yak, llama, zebra). */
	long: ({ tone = 'a1', top = 9.5, w = 6, jaw = 7, bottom = 30.5 }: { tone?: Tone; top?: number; w?: number; jaw?: number; bottom?: number } = {}): Part => [
		path(tone, `M${pts(16 - w, top + 2)}Q${pts(16, top - 1, 16 + w, top + 2)}Q${pts(16 + w + 1, top + 8, 16 + w - 0.5, top + 11)}Q${pts(16 + jaw, bottom - 7, 16 + jaw - 0.5, bottom - 3.5)}Q${pts(16 + jaw - 2, bottom, 16, bottom)}Q${pts(16 - jaw + 2, bottom, 16 - jaw + 0.5, bottom - 3.5)}Q${pts(16 - jaw, bottom - 7, 16 - w + 0.5, top + 11)}Q${pts(16 - w - 1, top + 8, 16 - w, top + 2)}Z`),
	],
	/** A ruff of points at the cheeks (lynx, lion's face). */
	ruff: ({ tone = 'a1', cy = 18, rx = 10.5, ry = 9.5 }: HeadOpts = {}): Part => [
		ellipse(tone, 16, cy, rx, ry),
		path(tone, `M${pts(16 - rx + 1, cy - 1)}L${pts(16 - rx - 2.4, cy + 2.2)}L${pts(16 - rx + 0.6, cy + 2.6)}L${pts(16 - rx - 1.4, cy + 5.6)}L${pts(16 - rx + 3, cy + 6)}Z`, true),
	],
};

// ---- ears ----

type EarOpts = { x: number; y: number; tone?: Tone; inner?: Tone | null };
export const ears = {
	/** Triangles with a rounded tip; `lean` moves the tip outward. */
	pointy: ({ x, y, w = 6, h = 8, lean = 1.5, tone = 'a1', inner = 'a2' }: EarOpts & { w?: number; h?: number; lean?: number }): Part => {
		const ear = (s: number, t: Tone): Shape => {
			const cx = x;
			const by = y;
			const tx = cx - lean * s;
			const ty = by - h * s;
			const hw = (w / 2) * s;
			return path(t, `M${pts(cx - hw, by)}Q${pts(cx - hw - lean * 0.2 * s, by - h * 0.55 * s, tx - 0.5 * s, ty + 0.6 * s)}Q${pts(tx, ty - 0.4 * s, tx + 0.7 * s, ty + 0.4 * s)}Q${pts(cx + hw * 0.6, by - h * 0.5 * s, cx + hw, by)}Z`, true);
		};
		return inner ? [ear(1, tone), ear(0.55, inner)] : [ear(1, tone)];
	},
	/** Round ears; `ri` sizes the inside. */
	round: ({ x, y, r = 3, ri = r * 0.55, tone = 'a1', inner = 'a3' }: EarOpts & { r?: number; ri?: number }): Part =>
		inner ? [circle(tone, x, y, r, true), circle(inner, x + r * 0.08, y + r * 0.1, ri, true)] : [circle(tone, x, y, r, true)],
	/** Long upright ears (rabbit, llama). */
	long: ({ x, y, w = 2.6, h = 7, tilt = -10, tone = 'a1', inner = 'a3' }: EarOpts & { w?: number; h?: number; tilt?: number }): Part => {
		const out = [ellipse(tone, x, y, w, h, tilt, true)];
		if (inner) out.push(ellipse(inner, x + 0.1, y + h * 0.12, w * 0.45, h * 0.72, tilt, true));
		return out;
	},
	/** Leaf-shaped ears out to the sides (cow-like: yak, moose, sheep). */
	side: ({ x, y, w = 3.2, h = 1.5, tilt = 20, tone = 'a1', inner = null }: EarOpts & { w?: number; h?: number; tilt?: number }): Part => {
		const out = [ellipse(tone, x, y, w, h, tilt, true)];
		if (inner) out.push(ellipse(inner, x + 0.4, y + 0.1, w * 0.6, h * 0.5, tilt, true));
		return out;
	},
};

// ---- eyes ----

export const eyes = {
	/** Plain dots. */
	dot: ({ dx = 4.2, y = 16.5, r = 1.4, tone = 'ink' }: { dx?: number; y?: number; r?: number; tone?: Tone } = {}): Part => [circle(tone, 16 - dx, y, r, true)],
	/** A light disc and a pupil (owl, frog, raccoon in its mask). */
	big: ({ dx = 4.4, y = 17, r = 4, pupil = r * 0.5, tone = 'a3' }: { dx?: number; y?: number; r?: number; pupil?: number; tone?: Tone } = {}): Part => [
		circle(tone, 16 - dx, y, r, true),
		circle('ink', 16 - dx + r * 0.1, y + r * 0.05, pupil, true),
	],
	/** Dark patches tilted outward with a light eye in each (panda). */
	patch: ({ dx = 4.6, y = 17.6, rx = 2.6, ry = 3.4, tilt = 35, tone = 'a2', eye = 1 }: { dx?: number; y?: number; rx?: number; ry?: number; tilt?: number; tone?: Tone; eye?: number } = {}): Part => [
		ellipse(tone, 16 - dx, y, rx, ry, tilt, true),
		circle('a3', 16 - dx + 0.4, y - 0.4, eye, true),
		circle('ink', 16 - dx + 0.5, y - 0.3, eye * 0.55, true),
	],
};

// ---- snouts, beaks, muzzles ----

/** A soft rounded-triangle nose centred at (16, y). */
const nose = (y: number, w = 1.8, h = 1.3, tone: Tone = 'ink'): Shape =>
	path(tone, `M${pts(16 - w, y - h * 0.5)}Q${pts(16 - w, y - h, 16, y - h)}Q${pts(16 + w, y - h, 16 + w, y - h * 0.5)}Q${pts(16 + w * 0.6, y + h * 0.6, 16, y + h * 0.8)}Q${pts(16 - w * 0.6, y + h * 0.6, 16 - w, y - h * 0.5)}Z`);

export const snout = {
	nose: ({ y = 21, w = 1.8, h = 1.3, tone = 'ink' }: { y?: number; w?: number; h?: number; tone?: Tone } = {}): Part => [nose(y, w, h, tone)],
	/** Two cheek lobes under a nose (cats, otters, bears with `round`). */
	muzzle: ({ y = 22.3, r = 3.3, gap = 2.6, tone = 'a3', noseY = y - 1.8, noseW = 1.8, round = false, mouth = false }: { y?: number; r?: number; gap?: number; tone?: Tone; noseY?: number; noseW?: number; round?: boolean; mouth?: boolean } = {}): Part => {
		const out: Part = round ? [ellipse(tone, 16, y, r + gap * 0.7, r * 0.95)] : [circle(tone, 16 - gap, y, r, true)];
		out.push(nose(noseY, noseW, noseW * 0.72));
		if (mouth) out.push(line('a2', 0.9, `M${pts(16, noseY + 1)}L${pts(16, noseY + 2.2)}`));
		return out;
	},
	/** A pig's disc with nostrils. */
	disc: ({ y = 21.6, rx = 4.4, ry = 3.2, tone = 'a3' as Tone, holes = 'a2' as Tone } = {}): Part => [
		ellipse(tone, 16, y, rx, ry),
		ellipse(holes, 16 - rx * 0.38, y, rx * 0.17, ry * 0.36, 0, true),
	],
	/** A hooked triangle beak pointing down. */
	beak: ({ y = 20.2, w = 1.8, h = 3.6, tone = 'a2' as Tone } = {}): Part => [
		path(tone, `M${pts(16 - w, y)}Q${pts(16, y - w * 0.6, 16 + w, y)}Q${pts(16 + w * 0.4, y + h * 0.6, 16, y + h)}Q${pts(16 - w * 0.4, y + h * 0.6, 16 - w, y)}Z`),
	],
	/** A wide flat bill (duck). */
	bill: ({ y = 22.4, w = 6.2, h = 3, tone = 'a3' as Tone } = {}): Part => [
		path(tone, `M${pts(16 - w, y)}Q${pts(16 - w, y - h, 16, y - h)}Q${pts(16 + w, y - h, 16 + w, y)}Q${pts(16 + w, y + h * 0.9, 16, y + h * 0.75)}Q${pts(16 - w, y + h * 0.9, 16 - w, y)}Z`),
		circle('a2', 14.6, y - h * 0.4, 0.5, true),
	],
	/** A trunk curling up to the right at the tip. */
	trunk: ({ top = 18.5, tone = 'a1' as Tone } = {}): Part => [
		path(tone, `M13.4 ${r2(top)}Q13.2 ${r2(top + 6)} 14.4 ${r2(top + 9)}Q15.8 ${r2(top + 12.4)} 19.2 ${r2(top + 11.6)}Q21 ${r2(top + 11)} 20.8 ${r2(top + 9.4)}Q20.6 ${r2(top + 8.4)} 19 ${r2(top + 8.8)}Q17.8 ${r2(top + 9.2)} 17.8 ${r2(top + 7)}Q18.4 ${r2(top + 4)} 18.6 ${r2(top)}Z`),
		line('a2', 0.7, `M14.4 ${r2(top + 4)}h3M14.6 ${r2(top + 6.4)}h2.8`),
	],
};

// ---- markings ----

export const mark = {
	/** A dark band across the eyes (raccoon). */
	mask: ({ y = 17, tone = 'a2' as Tone } = {}): Part => [
		path(tone, `M4.6 ${r2(y)}Q6 ${r2(y - 3.4)} 11 ${r2(y - 2.6)}Q14 ${r2(y - 2)} 16 ${r2(y - 0.2)}Q18 ${r2(y - 2)} 21 ${r2(y - 2.6)}Q26 ${r2(y - 3.4)} 27.4 ${r2(y)}Q26 ${r2(y + 3.4)} 21.5 ${r2(y + 2.6)}Q18 ${r2(y + 2.2)} 16 ${r2(y + 3.4)}Q14 ${r2(y + 2.2)} 10.5 ${r2(y + 2.6)}Q6 ${r2(y + 3.4)} 4.6 ${r2(y)}Z`),
	],
	/** Light cheeks running down to the chin (fox). */
	cheeks: ({ top = 15, chin = 28, tone = 'a3' as Tone } = {}): Part => [
		path(tone, `M4.2 ${r2(top)}Q9.5 ${r2(top + 0.6)} 16 ${r2(top + 6)}L16 ${r2(chin)}Q6.5 ${r2(chin - 6)} 4.2 ${r2(top)}Z`, true),
	],
	/** Short wedge stripes on the brow and cheeks (tiger). */
	stripes: ({ tone = 'a2' as Tone } = {}): Part => [
		path(tone, 'M14.6 8.8L17.4 8.8L16 12.6Z'),
		path(tone, 'M10.2 9.8Q12.4 10.8 13 13Q11.2 12 9.8 10.6Z', true),
		path(tone, 'M5.8 16.4L10 17.8L5.9 19Z', true),
		path(tone, 'M6.4 20.6L9.8 21.2L7 22.8Z', true),
	],
	/** Dark stripes from the ears down past the eyes to the nose (badger). */
	blaze: ({ tone = 'a2' as Tone } = {}): Part => [path(tone, 'M8.2 9.6Q11 9.6 12.6 15Q14 20 15.2 24.6Q14 25 13.4 24.4Q10.2 20.4 8.6 16.4Q7 12.6 6.4 11Z', true)],
	/** Spots, as [x, y, r] on the left side, mirrored. */
	spots: (at: [number, number, number][], tone: Tone = 'a2'): Part => at.map(([x, y, r]) => circle(tone, x, y, r, true)),
	/** A light patch at the bottom of the head (narwhal's belly, penguin's face). */
	belly: ({ cy = 26, rx = 8, ry = 4.5, tone = 'a3' as Tone } = {}): Part => [ellipse(tone, 16, cy, rx, ry)],
	/** A round blush on each cheek. */
	blush: ({ dx = 7, y = 21, r = 1.6, tone = 'a2' as Tone } = {}): Part => [ellipse(tone, 16 - dx, y, r, r * 0.65, 0, true)],
};

// ---- extras ----

export const extra = {
	/** Horns curving out and up. */
	horns: ({ tone = 'a3' as Tone } = {}): Part => [path(tone, 'M11.6 11.8Q5 12.4 3.4 5.4Q4.5 4.4 5.4 5.6Q7.2 9.4 11.8 9Z', true)],
	/** Broad palmate antlers. */
	antlers: ({ tone = 'a2' as Tone } = {}): Part => [
		path(tone, 'M11.2 10.6Q7.6 10.2 5.4 10.8Q2.4 10.4 1.2 6.8Q2.8 7.4 3.3 6Q4.4 7.8 5.2 5.4Q6.4 7.6 7.8 5.4Q8.4 7.6 9.8 6.4Q10.4 8.4 11.8 8.6Z', true),
	],
	/** Tusks hanging from (x, y) down `len`. */
	tusks: ({ x = 13.4, y = 23, len = 8, w = 1.8, curve = 0.8, tone = 'a3' as Tone } = {}): Part => [
		path(tone, `M${pts(x - w / 2, y)}L${pts(x + w / 2, y)}Q${pts(x + w / 2 - curve * 0.3, y + len * 0.6, x - curve, y + len)}Q${pts(x - w / 2 - curve * 0.2, y + len * 0.6, x - w / 2, y)}Z`, true),
	],
	/** Whisker lines out from the muzzle. */
	whiskers: ({ x = 12.4, y = 22, len = 6, tone = 'a2' as Tone, w = 0.7 } = {}): Part => [
		line(tone, w, `M${pts(x, y - 0.6)}L${pts(x - len, y - 1.6)}M${pts(x, y + 0.6)}L${pts(x - len, y + 1.4)}`, true),
	],
	/** A scalloped mane or wool ring. */
	mane: ({ cx = 16, cy = 18, r = 12, n = 14, depth = 1.4, tone = 'a2' as Tone } = {}): Part => [bumpy(tone, cx, cy, r, n, depth, false)],
	/** Spikes along an arc (hedgehog). */
	spikes: ({ cx = 16, cy = 19, r = 11, n = 11, depth = 3.2, from = 180, to = 360, tone = 'a2' as Tone } = {}): Part => [bumpy(tone, cx, cy, r, n, depth, true, from, to)],
	/** Small fangs under the mouth. */
	fangs: ({ y = 23.4, x = 14.6, tone = 'a3' as Tone } = {}): Part => [path(tone, `M${pts(x - 0.8, y)}L${pts(x + 0.8, y)}L${pts(x, y + 2)}Z`, true)],
	/** A curl of tentacles under a dome (octopus). */
	tentacles: ({ tone = 'a1' as Tone } = {}): Part => [
		path(tone, 'M8.4 17Q7 23 3.6 24.4Q2.6 26.6 5.4 26.4Q9.6 25.2 11.6 20Z', true),
		path(tone, 'M12 19.6Q11.6 25.6 9.4 28.2Q9.8 30.4 12.2 29Q14.8 26 15.6 21Z', true),
	],
	/** A smile under the nose. */
	smile: ({ y = 23, w = 3, tone = 'a2' as Tone, sw = 0.9 } = {}): Part => [line(tone, sw, `M${pts(16 - w, y)}Q${pts(16, y + w * 0.7, 16 + w, y)}`)],
};

// ---- the animals ----

/** An animal: its parts back to front. */
export function animal(...parts: Part[]): Part {
	return parts.flat();
}

/**
 * Every animal an anonymous reader can be: the name (the second word of "Quiet Otter") and its face. Exactly 32, and
 * the order matters, client/lib/presence.ts picks by index.
 */
export const ANIMALS: { name: string; draw: Part }[] = [
	{
		name: 'Otter',
		draw: animal(
			ears.round({ x: 6.6, y: 14, r: 2.3, inner: 'a2' }),
			head.round({ rx: 10.8, ry: 8.8, cy: 18.6 }),
			eyes.dot({ dx: 4.6, y: 16.4, r: 1.35 }),
			snout.muzzle({ y: 22.2, r: 3.5, gap: 2.4, noseY: 20.4, noseW: 2 }),
			extra.whiskers({ x: 11.6, y: 22.4, len: 5 }),
		),
	},
	{
		name: 'Fox',
		draw: animal(
			ears.pointy({ x: 9, y: 11.5, w: 7, h: 9, lean: 2 }),
			head.pointed({ top: 9.5, chin: 28.5 }),
			mark.cheeks({ top: 15.6, chin: 28.5 }),
			eyes.dot({ dx: 4.6, y: 16.8, r: 1.3 }),
			snout.nose({ y: 26.4, w: 1.6, h: 1.2 }),
		),
	},
	{
		name: 'Panda',
		draw: animal(
			ears.round({ x: 7.8, y: 10.2, r: 3.4, tone: 'a2', inner: null }),
			head.round({ tone: 'a3', rx: 10.4, ry: 9.4 }),
			eyes.patch({ dx: 4.5, y: 17.4 }),
			snout.nose({ y: 21.6, w: 1.7, h: 1.2 }),
			extra.smile({ y: 23, w: 1.6, sw: 0.8 }),
		),
	},
	{
		name: 'Koala',
		draw: animal(
			ears.round({ x: 6.2, y: 12, r: 5.2, ri: 3, inner: 'a3' }),
			head.round({ rx: 9.6, ry: 9.4, cy: 18.6 }),
			eyes.dot({ dx: 4.8, y: 16.8, r: 1.2 }),
			[ellipse('ink', 16, 20.4, 2.6, 3.6)],
		),
	},
	{
		name: 'Owl',
		draw: animal(
			[path('a1', 'M6 5.6Q8.6 9 11 10.4Q16 9.2 21 10.4Q23.4 9 26 5.6Q28.2 13 27 19.4Q25.6 28.6 16 29.2Q6.4 28.6 5 19.4Q3.8 13 6 5.6Z')],
			eyes.big({ dx: 4.6, y: 17, r: 4.4, pupil: 2.1 }),
			snout.beak({ y: 20.6, w: 1.8, h: 3.6 }),
		),
	},
	{
		name: 'Badger',
		draw: animal(
			ears.round({ x: 7.6, y: 11.6, r: 2.4, tone: 'a2', inner: 'a3', ri: 1.1 }),
			head.pointed({ top: 9, chin: 28, w: 11.6 }),
			head.pointed({ tone: 'a3', top: 10.4, chin: 26.8, w: 9.8 }),
			mark.blaze(),
			eyes.dot({ dx: 4.3, y: 17.2, r: 1.2, tone: 'a3' }),
			snout.nose({ y: 25.6, w: 1.8, h: 1.3 }),
		),
	},
	{
		name: 'Lynx',
		draw: animal(
			[line('ink', 0.9, 'M7.8 2.6L7.4 0.4', true)],
			ears.pointy({ x: 9.4, y: 11.5, w: 6, h: 8.6, lean: 1.6 }),
			head.ruff({ cy: 18, rx: 10, ry: 9.2 }),
			[line('a2', 0.9, 'M14.2 10.4L14.6 13M16 10L16 13M17.8 10.4L17.4 13')],
			eyes.dot({ dx: 4.4, y: 16.6, r: 1.3 }),
			snout.muzzle({ y: 22.4, r: 3, gap: 2.4, noseY: 20.6, noseW: 1.5 }),
		),
	},
	{
		name: 'Moose',
		draw: animal(
			extra.antlers(),
			ears.side({ x: 6.8, y: 13.4, w: 3, h: 1.4, tilt: -20 }),
			head.long({ top: 9.5, w: 5.6, jaw: 7.4 }),
			[ellipse('a2', 16, 26.6, 6.4, 3.6)],
			[ellipse('ink', 13.8, 27, 0.9, 1.3, 0, true)],
			eyes.dot({ dx: 4.3, y: 15.6, r: 1.2 }),
		),
	},
	{
		name: 'Penguin',
		draw: animal(
			head.round({ tone: 'a2', rx: 10.4, ry: 10, cy: 18 }),
			[circle('a3', 12.2, 17.6, 3.8, true), ellipse('a3', 16, 24.4, 8.4, 6.6)],
			eyes.dot({ dx: 3.8, y: 17.4, r: 1.3 }),
			[path('a1', 'M13.8 18.8Q16 17.8 18.2 18.8Q17.2 22 16 22.6Q14.8 22 13.8 18.8Z')],
		),
	},
	{
		name: 'Rabbit',
		draw: animal(
			ears.long({ x: 12.2, y: 8, w: 2.6, h: 7.2, tilt: -12 }),
			head.round({ rx: 9, ry: 8.4, cy: 20.4 }),
			eyes.dot({ dx: 4, y: 19, r: 1.3 }),
			[circle('a3', 14.3, 23.6, 2.3, true)],
			snout.nose({ y: 22.1, w: 1.2, h: 0.9, tone: 'a2' }),
		),
	},
	{
		name: 'Tiger',
		draw: animal(
			ears.round({ x: 7.8, y: 10.6, r: 3, inner: 'a3' }),
			head.round({ rx: 10.6, ry: 9.4 }),
			mark.stripes(),
			eyes.dot({ dx: 4.4, y: 16.4, r: 1.3 }),
			snout.muzzle({ y: 22.4, r: 3.4, gap: 2.4, noseY: 20.6 }),
		),
	},
	{
		name: 'Walrus',
		draw: animal(
			head.round({ rx: 10.4, ry: 9.6, cy: 17 }),
			eyes.dot({ dx: 4, y: 13.8, r: 1.1 }),
			extra.tusks({ x: 13.6, y: 22, len: 9.4, w: 2 }),
			[circle('a3', 13, 20.6, 3.8, true)],
			mark.spots([[11.6, 20, 0.45], [13.4, 21.6, 0.45], [11.4, 22.2, 0.45]]),
			snout.nose({ y: 17.6, w: 1.6, h: 1.1 }),
		),
	},
	{
		name: 'Yak',
		draw: animal(
			extra.horns(),
			ears.side({ x: 6.4, y: 15, w: 3, h: 1.5, tilt: 18 }),
			head.long({ top: 9.5, w: 6.4, jaw: 7 }),
			[path('a2', 'M9.4 11Q16 7 22.6 11L22.4 15.6Q21 14 19.6 16Q18 14 16 16.4Q14 14 12.4 16Q11 14 9.6 15.6Z')],
			eyes.dot({ dx: 4.3, y: 18, r: 1.2 }),
			[ellipse('a3', 16, 26.4, 5.6, 3.6)],
			[ellipse('a2', 14, 26.4, 0.8, 1.1, 0, true)],
		),
	},
	{
		name: 'Zebra',
		draw: animal(
			ears.pointy({ x: 11, y: 11.4, w: 3.8, h: 7.4, lean: 1.4, tone: 'a3', inner: 'a2' }),
			[path('a2', 'M13.4 11Q13.6 6 16 5Q18.4 6 18.6 11Z')],
			head.long({ tone: 'a3', top: 9.5, w: 5.6, jaw: 6.6 }),
			[
				path('a2', 'M10.6 13Q13.4 13.6 16 15.6Q13.4 15.4 10.4 14.8Z', true),
				path('a2', 'M10 18.6Q12.6 19 14.2 20.6Q12.2 20.6 9.8 20.2Z', true),
			],
			[ellipse('a2', 16, 26.4, 6, 3.8)],
			[ellipse('ink', 14, 26.4, 0.8, 1.1, 0, true)],
			eyes.dot({ dx: 4.4, y: 16.8, r: 1.2 }),
		),
	},
	{
		name: 'Crab',
		draw: animal(
			[line('a1', 1.5, 'M8 15L6.4 12', true), line('a2', 1.3, 'M6.6 22.4L3 24.6M7.6 25L4.6 27.8', true)],
			[path('a1', 'M6.4 13Q1.8 12.4 2.2 7.6Q2.8 3.8 6.6 3.6L6 7.4L8.8 6.6Q10.6 10.8 6.4 13Z', true)],
			[line('a1', 1.3, 'M13.4 15L12.8 10.4', true)],
			[ellipse('a1', 16, 21, 9.6, 6.8)],
			[circle('ink', 12.8, 9.4, 1.6, true)],
			extra.smile({ y: 22, w: 2.4 }),
		),
	},
	{
		name: 'Hedgehog',
		draw: animal(
			extra.spikes({ cx: 16, cy: 19, r: 11, n: 11, depth: 3.4, from: 160, to: 380 }),
			[circle('a2', 16, 19.6, 11)],
			ears.round({ x: 9.4, y: 12.4, r: 2, inner: 'a2', ri: 1 }),
			[path('a3', 'M7.2 19Q7.2 12.6 16 12.6Q24.8 12.6 24.8 19Q24 24.4 16 28.4Q8 24.4 7.2 19Z')],
			eyes.dot({ dx: 3.8, y: 18.6, r: 1.25 }),
			snout.nose({ y: 26.6, w: 1.5, h: 1.2 }),
		),
	},
	{
		name: 'Llama',
		draw: animal(
			ears.long({ x: 10.8, y: 7, w: 1.8, h: 5, tilt: -14, inner: 'a2' }),
			head.long({ top: 10, w: 5.4, jaw: 5.6, bottom: 30.4 }),
			[circle('a3', 12.8, 10.2, 2.6), circle('a3', 16, 9.2, 2.9), circle('a3', 19.2, 10.2, 2.6)],
			eyes.dot({ dx: 3.8, y: 16.8, r: 1.2 }),
			[ellipse('a3', 16, 26.4, 4.4, 3.4)],
			[line('ink', 0.8, 'M14.8 25.2Q16 26.4 17.2 25.2M16 26.2V27.6')],
		),
	},
	{
		name: 'Narwhal',
		draw: animal(
			[path('a3', 'M14.2 13L22 1Q22.6 0.8 22.6 1.6L18.2 13.4Z'), line('a2', 0.7, 'M16 10L19.2 10.6M17.8 7L20.4 7.5M19.6 4.2L21.4 4.5')],
			[ellipse('a1', 4.8, 25.6, 3.4, 1.6, -30, true)],
			head.round({ rx: 10.4, ry: 9.2, cy: 21.4 }),
			mark.belly({ cy: 27.4, rx: 8, ry: 4.2 }),
			mark.spots([[9.4, 14.6, 0.9], [11.6, 12.4, 0.6], [7.6, 17.4, 0.6]]),
			eyes.dot({ dx: 5, y: 20, r: 1.3 }),
			extra.smile({ y: 22.2, w: 1.6 }),
		),
	},
	{
		name: 'Octopus',
		draw: animal(
			extra.tentacles(),
			[ellipse('a1', 16, 13.6, 9.2, 9.2)],
			mark.spots([[10.4, 9.4, 0.9], [12.6, 7, 0.6]]),
			eyes.dot({ dx: 3.8, y: 15, r: 1.5 }),
			extra.smile({ y: 18.4, w: 1.6 }),
		),
	},
	{
		name: 'Puffin',
		draw: animal(
			head.round({ tone: 'a2', rx: 10.4, ry: 10, cy: 18 }),
			[circle('a3', 12, 18.4, 4.8, true)],
			[path('a2', 'M9.6 14L13.4 15.2L9.6 16.4Z', true)],
			eyes.dot({ dx: 4.4, y: 15.2, r: 1.15 }),
			[path('a1', 'M12.4 18.6Q16 16.6 19.6 18.6Q18.8 23.8 16 27.6Q13.2 23.8 12.4 18.6Z')],
			[line('a2', 1.1, 'M12.8 18.6Q16 17 19.2 18.6'), line('a3', 0.9, 'M13.4 21.8Q16 20.6 18.6 21.8')],
		),
	},
	{
		name: 'Mouse',
		draw: animal(
			ears.round({ x: 7.2, y: 11.2, r: 5, ri: 3, inner: 'a3' }),
			head.round({ rx: 8.4, ry: 8, cy: 19.6 }),
			eyes.dot({ dx: 3.4, y: 18.6, r: 1.2 }),
			extra.whiskers({ x: 13.4, y: 23, len: 5.6 }),
			snout.nose({ y: 22.8, w: 1.3, h: 1 }),
		),
	},
	{
		name: 'Pig',
		draw: animal(
			ears.pointy({ x: 8.8, y: 11.4, w: 6, h: 5.4, lean: 2.2, inner: 'a2' }),
			head.round({ rx: 10.6, ry: 9.4 }),
			eyes.dot({ dx: 4.6, y: 16, r: 1.25 }),
			snout.disc({ y: 21.4, rx: 4.4, ry: 3.2 }),
			mark.blush({ dx: 7.6, y: 21, r: 1.4 }),
		),
	},
	{
		name: 'Frog',
		draw: animal(
			[circle('a1', 10.6, 12, 4.4, true)],
			[ellipse('a1', 16, 19.6, 12, 8.4)],
			eyes.big({ dx: 5.4, y: 12, r: 2.8, pupil: 1.5 }),
			[circle('ink', 14.6, 17, 0.5, true)],
			extra.smile({ y: 20.6, w: 6.4, sw: 1.1 }),
			mark.blush({ dx: 8.6, y: 21.6, r: 1.5 }),
		),
	},
	{
		name: 'Monkey',
		draw: animal(
			ears.round({ x: 5.4, y: 17.4, r: 3.4, tone: 'a2', inner: 'a3', ri: 1.9 }),
			head.round({ tone: 'a2', rx: 9.4, ry: 10, cy: 17.6 }),
			[circle('a3', 13.2, 16.4, 3.6, true), ellipse('a3', 16, 21.8, 6.4, 4.8)],
			eyes.dot({ dx: 2.9, y: 16.6, r: 1.2 }),
			[ellipse('ink', 15, 20.6, 0.5, 0.7, 0, true)],
			extra.smile({ y: 23, w: 2, sw: 0.8 }),
		),
	},
	{
		name: 'Lion',
		draw: animal(
			extra.mane({ cx: 16, cy: 17.4, r: 12, n: 14, depth: 1.3 }),
			ears.round({ x: 8.8, y: 10.2, r: 2.6, inner: 'a3', ri: 1.3 }),
			head.round({ rx: 8.8, ry: 8.6, cy: 18 }),
			eyes.dot({ dx: 3.8, y: 16.4, r: 1.2 }),
			snout.muzzle({ y: 21.8, r: 2.8, gap: 2.1, noseY: 20, noseW: 1.5 }),
		),
	},
	{
		name: 'Elephant',
		draw: animal(
			[ellipse('a1', 5.6, 16.4, 5.2, 7, -8, true), ellipse('a2', 5.8, 16.6, 3.4, 5.2, -8, true)],
			head.round({ rx: 8.4, ry: 8.8, cy: 15.4 }),
			extra.tusks({ x: 12.4, y: 20.4, len: 4.6, w: 1.6, curve: 1.6 }),
			snout.trunk({ top: 18 }),
			eyes.dot({ dx: 3.8, y: 15, r: 1.2 }),
		),
	},
	{
		name: 'Sheep',
		draw: animal(
			extra.mane({ cx: 16, cy: 16.4, r: 10.8, n: 12, depth: 1.5, tone: 'a3' }),
			ears.side({ x: 6, y: 17.4, w: 3.2, h: 1.5, tilt: 22, tone: 'a2' }),
			[ellipse('a2', 16, 20, 5.6, 7.4)],
			[circle('a3', 13.4, 12.2, 2.6), circle('a3', 16, 11.4, 2.8), circle('a3', 18.6, 12.2, 2.6)],
			eyes.dot({ dx: 2.6, y: 18.4, r: 1.1, tone: 'a3' }),
			[ellipse('a3', 16, 24.4, 1.3, 0.8)],
		),
	},
	{
		name: 'Raccoon',
		draw: animal(
			ears.pointy({ x: 8.6, y: 11.6, w: 6.4, h: 6.4, lean: 1.4, inner: 'a3' }),
			head.pointed({ top: 9.4, chin: 27.4, w: 11.6 }),
			[ellipse('a3', 11.4, 13.4, 3.2, 1.3, 8, true)],
			mark.mask({ y: 17.4 }),
			eyes.big({ dx: 4.6, y: 17.2, r: 1.25, pupil: 0.75 }),
			[ellipse('a3', 16, 23.6, 4.2, 3.6)],
			snout.nose({ y: 22.6, w: 1.5, h: 1.1 }),
		),
	},
	{
		name: 'Duck',
		draw: animal(
			[line('a1', 1.5, 'M15.4 9Q14.8 6 16.8 4.8M16.8 9Q17.6 6.8 19.6 6.4')],
			head.round({ rx: 9.8, ry: 9.6, cy: 17.6 }),
			eyes.dot({ dx: 4.2, y: 15.6, r: 1.35 }),
			snout.bill({ y: 21.4, w: 6.6, h: 3 }),
		),
	},
	{
		name: 'Bat',
		draw: animal(
			[path('a2', 'M8 18Q3 15 0 17L0 27Q2 25.6 3.4 27.4Q4.8 25.6 7 27.4L9 24Z', true)],
			ears.pointy({ x: 9.4, y: 13, w: 6.6, h: 10.4, lean: 2.8, inner: 'a2' }),
			head.round({ rx: 8.4, ry: 7.8, cy: 19.4 }),
			eyes.dot({ dx: 3.6, y: 18.4, r: 1.35 }),
			snout.nose({ y: 21.4, w: 1.2, h: 0.9, tone: 'a2' }),
			extra.fangs({ y: 23.2, x: 14.8 }),
		),
	},
	{
		name: 'Bear',
		draw: animal(
			ears.round({ x: 8, y: 10.6, r: 3.2, inner: 'a3' }),
			head.round({ rx: 10.2, ry: 9.6, cy: 18.4 }),
			eyes.dot({ dx: 4.2, y: 16.4, r: 1.3 }),
			snout.muzzle({ y: 22.4, r: 3.6, gap: 0, round: true, noseY: 20.8, noseW: 1.9, mouth: true }),
		),
	},
	{
		name: 'Hippo',
		draw: animal(
			ears.round({ x: 9.4, y: 8.6, r: 1.9, inner: 'a2', ri: 1 }),
			[ellipse('a1', 16, 14.4, 8.6, 7)],
			[circle('a1', 11.8, 10.4, 2.6, true)],
			[ellipse('a3', 16, 22.4, 11, 6.8)],
			eyes.dot({ dx: 4.2, y: 10.6, r: 1.2 }),
			[ellipse('ink', 12.6, 19.8, 1, 0.75, -20, true)],
			extra.smile({ y: 24.4, w: 4.6, sw: 0.8 }),
		),
	},
];

if (ANIMALS.length !== 32) throw new Error(`gen/animals.ts: ${ANIMALS.length} animals, want 32`);

/**
 * The face of a crawler or other automation (worker/bots.ts): a robot, drawn in the same kit and tone slots, but not one
 * of ANIMALS, so no person is ever drawn as one. styles/shell.css gives `.face.bot` its own steel tones.
 */
export const ROBOT: { name: string; draw: Part } = {
	name: 'Robot',
	draw: animal(
		[line('a2', 1.3, 'M16 4.8V8.6'), circle('a3', 16, 4.2, 1.7)],
		[rrect('a2', 3.2, 14.4, 3.4, 7.2, 1.2, true)],
		[rrect('a1', 5.6, 8.2, 20.8, 19.6, 4.6)],
		[rrect('a2', 8.4, 11.6, 15.2, 9.6, 3.2)],
		eyes.dot({ dx: 3.9, y: 16.4, r: 2.1, tone: 'a3' }),
		[rrect('a3', 11.4, 22.6, 9.2, 2.9, 1.1)],
		[line('a2', 0.8, 'M14.1 22.9V25.2M16 22.9V25.2M17.9 22.9V25.2')],
	),
};

/** The symbol id of an animal: `animal-otter`. */
export const symbolId = (name: string) => `animal-${name.toLowerCase()}`;

/** Mirror a path of absolute commands left to right about x = 16. */
function mirror(d: string): string {
	const tokens = d.match(/[A-Za-z]|-?\d*\.?\d+(?:e-?\d+)?/g) ?? [];
	let out = '';
	let cmd = '';
	let i = 0;
	const args: Record<string, number> = { M: 2, L: 2, T: 2, Q: 4, S: 4, C: 6, A: 7, H: 1, V: 1, Z: 0 };
	while (i < tokens.length) {
		const t = tokens[i];
		if (/[A-Za-z]/.test(t)) {
			cmd = t;
			if (cmd !== cmd.toUpperCase()) {
				// Relative commands mirror by negating dx (h and the x of each pair); arcs flip their sweep.
				if (!'mlhqcstav'.includes(cmd) && cmd !== 'z') throw new Error(`mirror: ${cmd}`);
			}
			out += cmd;
			i++;
			if (cmd === 'Z' || cmd === 'z') continue;
		}
		const up = cmd.toUpperCase();
		const n = args[up];
		const vals = tokens.slice(i, i + n).map(Number);
		i += n;
		const flip = (x: number) => (cmd === up ? 32 - x : -x);
		let v: number[];
		if (up === 'A') v = [vals[0], vals[1], -vals[2], vals[3], 1 - vals[4], flip(vals[5]), vals[6]];
		else if (up === 'H') v = [flip(vals[0])];
		else if (up === 'V') v = vals;
		else v = vals.map((x, k) => (k % 2 === 0 ? flip(x) : x));
		out += `${v.map(r2).join(' ')} `;
	}
	return out.replace(/ ([A-Za-z])/g, '$1').trim();
}

const FILL: Record<Tone, string> = { bg: 'var(--a-bg)', a1: 'var(--a1)', a2: 'var(--a2)', a3: 'var(--a3)', ink: 'var(--a-ink)' };

function shapeSvg(s: Shape, d: string): string {
	return s.stroke
		? `<path d="${d}" fill="none" stroke="${FILL[s.tone]}" stroke-width="${r2(s.stroke)}" stroke-linecap="round"/>`
		: `<path d="${d}" fill="${FILL[s.tone]}"/>`;
}

/** One animal as the inside of a 32x32 SVG: the background, then its shapes back to front. */
export function animalSvg(draw: Part): string {
	let out = `<rect width="32" height="32" fill="${FILL.bg}"/>`;
	for (const s of draw) {
		out += shapeSvg(s, s.d);
		if (s.pair) out += shapeSvg(s, mirror(s.d));
	}
	return out;
}

/** The sprite: every animal, and the robot, as a `<symbol>`. */
export function sprite(): string {
	const symbols = [...ANIMALS, ROBOT].map((a) => `<symbol id="${symbolId(a.name)}" viewBox="0 0 32 32">${animalSvg(a.draw)}</symbol>`);
	return `<svg xmlns="http://www.w3.org/2000/svg">${symbols.join('')}</svg>\n`;
}
