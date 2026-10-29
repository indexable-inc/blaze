/**
 * Where a pointer is, in terms every reader shares (client/lib/cursors.ts sends and draws the others' pointers with
 * it): what the pointer is on, not where in the window, so it lands on the same thing for each reader whatever their
 * window size and layout (the sidebar shut, "On this page" shown or not).
 *
 * `spotAt` turns a window point into a `Spot`, the anchor sent, and `placeOf` turns a Spot back into a point on this
 * reader's screen. A Spot is
 *
 * - in the article (`m`): a block (the index of a heading, paragraph, list item, ... in `main`, in document order,
 *   comment threads left out) and, while the pointer is on its text, the character there (its offset in the block's
 *   text); between blocks or beside the article, the block level with it. Every reader has the same blocks with the
 *   same text, so the same character is the same anchor wherever the lines wrap.
 * - in a region of the shell beside the article (REGIONS): the thing in it a reader would know (a link by its href,
 *   else the n-th button, list item, ... of the region), or the region itself.
 *
 * and where on that thing, as fractions of its box. The regions are fixed or sticky: they stay put in the window while
 * the page scrolls under them, so a place in one is `fixed` (drawn in the window), and one in the article is drawn on
 * the page. A reader who has no such thing on screen (a block in a shut `<details>`, the sidebar shut, a link
 * scrolled out of its list) gets null: that pointer isn't on their screen.
 *
 * worker/presence.ts checks a Spot the same way (`relay`): keep REGIONS' codes and KEY in step with it.
 */

/** Where a pointer is: a region code (`m`, or one of REGIONS'), what in it (`key`: `b<block>` or `b<block>.<char>` in
 * the article; `a<href>`, `k<kind>.<n>`, or '' for the region itself, in a region), and x and y as
 * fractions of that thing's box. */
export type Spot = [region: string, key: string, x: number, y: number];
/** A Spot on this reader's screen, in window px; `fixed` when it stays put there as the page scrolls. */
export type Place = { x: number; y: number; fixed: boolean };

/** What counts as a block: what the text is made of, in every page's `main`. */
const BLOCKS = 'h1, h2, h3, h4, h5, h6, p, li, pre, table, tr, figure, blockquote, dt, dd, img, details, summary';
/**
 * The regions of the shell beside the article, by the code sent for each, innermost first: the who-is-here list
 * (inside the top line), the top line, "On this page" and the sidebar.
 */
const REGIONS: [string, string][] = [
	['w', '.tl-list'],
	['t', '.topline'],
	['o', '.toc'],
	['s', '.side'],
];
/** What the article and the regions are, for whoever watches them move (client/lib/cursors.ts's ResizeObserver). */
export const SHELL = ['main', ...REGIONS.map(([, sel]) => sel)].join(', ');
/** A thing in a region that is not a link is known by its kind and its place among the region's
 * things of that kind (`k<kind>.<n>`): the same for every reader, since the shell is. */
const KINDS = ['button', 'summary', 'li', 'input, textarea', 'p', 'h2, h3'];
/** What only this reader has on screen (a dialog, the feed: client/lib/feed.ts): a pointer there is on nothing the others have. */
const PRIVATE = 'dialog, .feed';
/** What a key may hold. */
const KEY = /^[\w/.#:?=&%~+@,-]{0,160}$/;
/** A fraction's bounds: a little past the box, as for a pointer beside a block. */
const FRAC = 50;

const round = (x: number) => Math.round(x * 1000) / 1000;
const frac = (v: number, from: number, size: number) => round(Math.max(-FRAC, Math.min(FRAC, (v - from) / (size || 1))));
const shown = (e: Element) => e.checkVisibility({ visibilityProperty: true });

/**
 * How far down the page the window shows (document px), as drawn: during a rubber-band bounce `scrollY` stays put (or
 * goes past the ends, in Safari) while the page moves, so positions measured or placed against it would be off.
 */
export const pageTop = () => -document.documentElement.getBoundingClientRect().top;

// ---- The article's blocks -----------------------------------------------------------------------------------------

/** The blocks with each one's top (document px) and height, -1 for one not shown, and each block's index. */
let blocks: { e: Element; top: number; h: number }[] | null = null;
let indexOf = new Map<Element, number>();

/** The page moved under the anchors (a resize, a fold, a thread opened, another page): measure the blocks again. */
export function remeasure(): void {
	blocks = null;
}

function layout(): { e: Element; top: number; h: number }[] {
	if (blocks) return blocks;
	const main = document.querySelector('main');
	const sy = pageTop();
	blocks = main
		? [...main.querySelectorAll(BLOCKS)]
				.filter((e) => !e.closest('.c-thread'))
				.map((e) => {
					const r = e.getBoundingClientRect();
					return { e, top: r.top + sy, h: e.getClientRects().length ? r.height : -1 };
				})
		: [];
	indexOf = new Map(blocks.map((b, i) => [b.e, i]));
	return blocks;
}

/** The block level with document y: the shown block that starts last at or above it (the innermost, on a tie); -1 for none. */
function levelWith(y: number): number {
	const bs = layout();
	let best = -1;
	for (let i = 0; i < bs.length; i++) if (bs[i].h > 0 && bs[i].top <= y && (best < 0 || bs[i].top >= bs[best].top)) best = i;
	return best < 0 ? bs.findIndex((b) => b.h > 0) : best;
}

/** A block's text nodes in order, comment threads left out: what a character offset counts. */
function texts(b: Element): Text[] {
	const out: Text[] = [];
	const w = document.createTreeWalker(b, NodeFilter.SHOW_TEXT);
	for (let n = w.nextNode(); n; n = w.nextNode()) if (n instanceof Text && !n.parentElement?.closest('.c-thread')) out.push(n);
	return out;
}

function charRange(t: Text, i: number): Range {
	const r = document.createRange();
	r.setStart(t, i);
	r.setEnd(t, i + 1);
	return r;
}

/** The character of block `b` under the window point: its offset in the block's text and its box; null off its text. */
function charAt(b: Element, px: number, py: number): { o: number; box: DOMRect } | null {
	const p = document.caretPositionFromPoint(px, py);
	const node = p?.offsetNode;
	if (!p || !(node instanceof Text) || !b.contains(node)) return null;
	// The caret falls between characters: the one under the pointer is just after it or just before.
	for (const i of [p.offset, p.offset - 1]) {
		if (i < 0 || i >= node.length) continue;
		const box = charRange(node, i).getBoundingClientRect();
		if (box.width <= 0 || px < box.left || px > box.right || py < box.top || py > box.bottom) continue;
		let o = i;
		for (const t of texts(b)) {
			if (t === node) return { o, box };
			o += t.length;
		}
	}
	return null;
}

/** The box of character `o` of block `b` (window px), or null when its text is shorter or the character isn't drawn. A
 * space where this reader's line wraps has no width, but still its place at the line's end. */
function charBox(b: Element, o: number): DOMRect | null {
	for (const t of texts(b)) {
		if (o < t.length) return [...charRange(t, o).getClientRects()].find((r) => r.height > 0) ?? null;
		o -= t.length;
	}
	return null;
}

// ---- A point to a Spot --------------------------------------------------------------------------------------------

/** What is under the window point, `skip` (the reader's own overlays) left out, and its region ('m' in none); null on what only this reader has. */
function hitAt(px: number, py: number, skip: Element[]): { hit: Element; region: string; box: HTMLElement | null } | null {
	const hit = document.elementsFromPoint(px, py).find((e) => !skip.some((s) => s.contains(e)));
	if (!hit || hit.closest(PRIVATE)) return null;
	for (const [region, sel] of REGIONS) {
		const box = hit.closest<HTMLElement>(sel);
		if (box) return { hit, region, box };
	}
	return { hit, region: 'm', box: null };
}

/** What the window point is on, as sent; null when it is on nothing the others have. */
export function spotAt(px: number, py: number, skip: Element[]): Spot | null {
	const at = hitAt(px, py, skip);
	if (!at) return null;
	const { hit, region, box } = at;
	const on = (key: string, r: DOMRect): Spot => [region, key, frac(px, r.left, r.width), frac(py, r.top, r.height)];
	if (box) {
		// The innermost link, else the innermost thing of a kind, else the region.
		let kind = -1;
		let thing: Element | null = null;
		for (let e: Element | null = hit; e && e !== box; e = e.parentElement) {
			const href = e.matches('a[href]') ? e.getAttribute('href') : null;
			if (href !== null && KEY.test(`a${href}`)) return on(`a${href}`, e.getBoundingClientRect());
			if (kind < 0 && (kind = KINDS.findIndex((k) => e!.matches(k))) >= 0) thing = e;
		}
		if (thing) return on(`k${kind}.${[...box.querySelectorAll(KINDS[kind])].indexOf(thing)}`, thing.getBoundingClientRect());
		return on('', box.getBoundingClientRect());
	}
	const bs = layout();
	const under = hit.closest(BLOCKS);
	let i = under ? (indexOf.get(under) ?? -1) : -1;
	const ch = i >= 0 ? charAt(bs[i].e, px, py) : null;
	if (ch) return on(`b${i}.${ch.o}`, ch.box);
	if (i < 0) i = levelWith(py + pageTop());
	return i < 0 ? null : on(`b${i}`, bs[i].e.getBoundingClientRect());
}

// ---- A Spot to a point --------------------------------------------------------------------------------------------

/** A region's thing for `key` that this reader has shown, or null. */
function thingOf(box: HTMLElement, key: string): Element | null {
	if (!key) return box;
	const v = key.slice(1);
	if (key[0] === 'a') return [...box.querySelectorAll('a[href]')].find((a) => a.getAttribute('href') === v && shown(a)) ?? null;
	const k = /^(\d+)\.(\d+)$/.exec(v);
	return key[0] === 'k' && k && KINDS[+k[1]] ? (box.querySelectorAll(KINDS[+k[1]])[+k[2]] ?? null) : null;
}

/** Where `s` is on this reader's screen, or null when they don't have that thing on it. */
export function placeOf(s: Spot): Place | null {
	const [region, key, fx, fy] = s;
	if (region === 'm') {
		const k = /^b(\d+)(?:\.(\d+))?$/.exec(key);
		const b = k ? layout()[+k[1]] : undefined;
		if (!k || !b || b.h <= 0) return null;
		const r = k[2] === undefined ? b.e.getBoundingClientRect() : charBox(b.e, +k[2]);
		if (!r) return null;
		// Beside the article (a fraction past a block's edge) it stays on the pane, where a narrower margin would put it off.
		const pane = document.querySelector('.pane')?.getBoundingClientRect();
		const x = r.left + fx * r.width;
		return { x: pane ? Math.max(pane.left, Math.min(pane.right - 8, x)) : x, y: r.top + fy * r.height, fixed: false };
	}
	const sel = REGIONS.find(([c]) => c === region)?.[1];
	const box = sel ? document.querySelector<HTMLElement>(sel) : null;
	const e = box && thingOf(box, key);
	if (!box || !e || !shown(e)) return null;
	const r = e.getBoundingClientRect();
	const x = r.left + fx * r.width;
	const y = r.top + fy * r.height;
	if (x < 0 || y < 0 || x > innerWidth || y > innerHeight) return null;
	// Inside the region, and inside whatever scrolls in it (the sidebar's list).
	for (let a: Element | null = e.parentElement; a; a = a === box ? null : a.parentElement) {
		const st = getComputedStyle(a);
		if (a !== box && st.overflowX === 'visible' && st.overflowY === 'visible') continue;
		const c = a.getBoundingClientRect();
		if (x < c.left || x > c.right || y < c.top || y > c.bottom) return null;
	}
	return { x, y, fixed: true };
}

/** A Spot as it came over the wire, or null for anything else (worker/presence.ts has checked it already). */
export function spotOf(x: unknown): Spot | null {
	return Array.isArray(x) && x.length === 4 && typeof x[0] === 'string' && typeof x[1] === 'string' && Number.isFinite(x[2]) && Number.isFinite(x[3])
		? [x[0], x[1], x[2], x[3]]
		: null;
}
