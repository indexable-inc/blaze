/**
 * The course's glyphs: one small line drawing per lesson and per update, the picture on its node on the course path
 * (the home page), on What's new, and above its title on its page. A lesson or update names its glyph
 * (`Lesson: {1, "...", "pipeline"}` in content/learn/*.flecs, `Update: {..., "fetch"}` in content/updates/*.flecs);
 * gen/build.ts bakes the drawing onto the page as its `Baked {html}`, and the renderer (render.rs `glyph`) inlines it.
 * An unknown name fails the build.
 *
 * Drawn as the site's icons are (Phosphor light, renderer/core/src/icons.rs): thin round-capped strokes in
 * `currentColor`, here on a 32x32 canvas. Each glyph also has one filled pixel cell, `.g-cell`: the one part that
 * matters (the function that changed, the kept output), as the home page's drawings mark it. styles/learn.css colours
 * it: the ink of its node, and the fire orange once the lesson is mastered. Inline rather than a `<use>` sprite, so
 * the page's CSS reaches inside, and no fetch.
 */

const r2 = (n: number) => String(Math.round(n * 100) / 100);

/** One shape: a stroked outline, or the filled cell. */
type Shape = string;
type Glyph = Shape[];

// ---- primitives (all stroked unless they say otherwise) ----

const line = (...pts: number[]): Shape => {
	let d = `M${r2(pts[0])} ${r2(pts[1])}`;
	for (let i = 2; i < pts.length; i += 2) d += `L${r2(pts[i])} ${r2(pts[i + 1])}`;
	return `<path d="${d}"/>`;
};
const closed = (...pts: number[]): Shape => line(...pts).replace('"/>', 'Z"/>');
const box = (x: number, y: number, w: number, h: number, r = 1.5): Shape =>
	`<rect x="${r2(x)}" y="${r2(y)}" width="${r2(w)}" height="${r2(h)}" rx="${r2(r)}"/>`;
const curve = (x0: number, y0: number, x1: number, y1: number): Shape => {
	const mx = (x0 + x1) / 2;
	return `<path d="M${r2(x0)} ${r2(y0)}C${r2(mx)} ${r2(y0)} ${r2(mx)} ${r2(y1)} ${r2(x1)} ${r2(y1)}"/>`;
};
const arc = (d: string): Shape => `<path d="${d}"/>`;
/** The filled cell: a square of side `s` centred on (cx, cy). */
const cell = (cx: number, cy: number, s = 3.2): Shape =>
	`<rect class="g-cell" x="${r2(cx - s / 2)}" y="${r2(cy - s / 2)}" width="${r2(s)}" height="${r2(s)}" fill="currentColor" stroke="none"/>`;

/** `s` with a class, the part of its glyph styles/learn.css moves on hover. */
const as = (c: string, ...shapes: Shape[]): Shape[] => shapes.map((sh) => sh.replace(/^<(\w+) /, `<$1 class="${c}" `));

// ---- the glyphs ----

/** A source file: a page with a folded corner and two lines of text. */
const page = (x: number, y: number, w = 9, h = 11): Glyph => [
	closed(x, y, x + w - 3, y, x + w, y + 3, x + w, y + h, x, y + h),
	line(x + 2.2, y + 4.5, x + w - 2.2, y + 4.5),
	line(x + 2.2, y + 7.3, x + w - 3.6, y + 7.3),
];

export const GLYPHS: Record<string, Glyph> = {
	/** Lesson 1: two source files compiled (the curves), then linked into one program (the box, its cell). */
	pipeline: [...page(3.5, 3.5), ...page(3.5, 17.5), ...as('flow', curve(12.5, 9, 19.5, 13), curve(12.5, 23, 19.5, 19)), ...as('link', box(19.5, 9.5, 9, 13, 2)), cell(24, 16)],
	/** Lesson 2: a grid of functions, one of them changed. */
	functions: [0, 1, 2].flatMap((y) => [0, 1, 2].flatMap((x) => (x === 2 && y === 1 ? [cell(7.5 + x * 8.5, 7.5 + y * 8.5, 5.4)] : as('fn', box(4.75 + x * 8.5, 4.75 + y * 8.5, 5.5, 5.5, 1))))),
	/** Lesson 3: code that writes code, in a box. */
	sandbox: [...as('wall', box(3.5, 5.5, 25, 21, 3)), ...as('code', line(12.5, 11.5, 8.5, 16, 12.5, 20.5), line(19.5, 11.5, 23.5, 16, 19.5, 20.5)), cell(16, 16)],
	/** Lesson 4: a sealed box, the same from every side; the lid (the top face) closes it. */
	sealed: [...as('lid', closed(16, 3.5, 27.5, 9.5, 16, 15.5, 4.5, 9.5)), line(4.5, 9.5, 4.5, 22.5, 16, 28.5, 27.5, 22.5, 27.5, 9.5), line(16, 15.5, 16, 28.5), cell(16, 9.5)],
	/** Lesson 5: one store, and two machines that share it; the pulses are work going in, unseen until hovered. */
	store: [
		arc('M10 19.5a6 2.2 0 1 0 12 0a6 2.2 0 1 0 -12 0'),
		arc('M10 19.5v6.5a6 2.2 0 0 0 12 0v-6.5'),
		box(3.5, 3.5, 7.5, 6, 1.2),
		box(21, 3.5, 7.5, 6, 1.2),
		curve(7.25, 9.5, 12.5, 17),
		curve(24.75, 9.5, 19.5, 17),
		...as('pulse', curve(7.25, 9.5, 12.5, 17), curve(24.75, 9.5, 19.5, 17)),
		cell(16, 24.2, 2.8),
	],

	// Updates (content/updates/*.flecs name these in `Update`), drawn the same way.
	/** Pinned downloads: a download comes down into the build, held where the lock says (the cell). */
	fetch: [...as('flow', line(16, 3.5, 16, 17.5), line(11, 12.5, 16, 17.5, 21, 12.5)), ...as('link', line(4.5, 19.5, 4.5, 27.5, 27.5, 27.5, 27.5, 19.5)), cell(16, 23.2)],
	/** Why it rebuilt: a build's last lines, the rebuilt function marked. */
	why: [...as('code', line(11.5, 8.5, 27.5, 8.5), line(11.5, 16, 23.5, 16), line(11.5, 23.5, 25.5, 23.5)), box(3.5, 5.5, 5, 5, 1), box(3.5, 20.5, 5, 5, 1), cell(6, 16, 5)],
	/** Kept test passes: a passing check, kept in its cell. */
	pass: [box(4.5, 4.5, 23, 23, 3), ...as('flow', line(10, 16.5, 14, 20.5, 22.5, 11)), cell(24.5, 24.5)],
	/** Macros that keep state: two expansions in one session, what the first left behind carried to the next. */
	session: [box(3.5, 9.5, 10, 13, 2), ...as('flow', curve(13.5, 16, 18.5, 16)), ...as('link', box(18.5, 9.5, 10, 13, 2)), cell(8.5, 16), cell(23.5, 16)],
	/** Build scripts, shared: two checkouts finding one kept run. */
	anywhere: [
		box(3.5, 3.5, 8, 7, 1.2),
		box(3.5, 21.5, 8, 7, 1.2),
		curve(11.5, 7, 21, 16),
		curve(11.5, 25, 21, 16),
		...as('pulse', curve(11.5, 7, 21, 16), curve(11.5, 25, 21, 16)),
		arc('M21 16a5 5 0 1 0 10 0a5 5 0 1 0 -10 0'),
		cell(26, 16, 2.8),
	],
	/** Const macros: code that writes code, with no box round it: it runs where it is. */
	inline: [...as('code', line(11.5, 9.5, 5, 16, 11.5, 22.5), line(20.5, 9.5, 27, 16, 20.5, 22.5)), line(18, 7.5, 14, 24.5), cell(16, 27.5, 2.6)],
};

/** A glyph as an inline SVG, sized by CSS (`.glyph svg`). */
export function glyphSvg(name: string): string {
	const g = GLYPHS[name];
	if (!g) throw new Error(`gen/glyphs.ts: no glyph "${name}" (have ${Object.keys(GLYPHS).join(', ')})`);
	// Each stroke measures 1 (`pathLength`), so styles/learn.css can draw parts of it again when its lesson is hovered.
	const shapes = g.map((sh) => (sh.includes('g-cell') ? sh : sh.replace(/^<(\w+) /, '<$1 pathLength="1" ')));
	return `<svg class="glyph g-${name}" viewBox="0 0 32 32" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${shapes.join('')}</svg>`;
}
