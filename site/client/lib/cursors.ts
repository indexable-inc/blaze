/**
 * Where the others on this page are (worker/presence.ts passes it on; client/lib/presence.ts owns the socket and
 * hands over the frames, `message`, and who is on the page, `setPeople`): their mouse pointer, an arrow in their
 * colour with their face and name, and the part of the page each is reading, a band with their face on a thin rail
 * at the right edge (click the face to go there).
 *
 * On a desk (a mouse or a trackpad, which hovers) the rail is also the page's scrollbar, there whenever the page
 * scrolls, whoever else is here: styles/shell.css hides the browser's own, the reader's viewport is the thumb (drag
 * it, or press the track to jump there and drag on), and it dims while nothing moves, like an overlay scrollbar. It
 * takes no focus, so the keys scroll the page as ever. On a phone the browser's scrollbar stays and the rail shows
 * only the others, while there are any.
 *
 * Positions are in the page's text, not the window, so readers with other window sizes line up: x is a fraction of
 * `main`'s width, y a block (the index of a heading, paragraph, list item, ... in `main`, in document order, comment
 * threads left out) and how far down it, as a fraction of its height. Every reader has the same blocks in the same
 * order, so the same place in the text is the same anchor wherever the lines wrap.
 *
 * A tab sends its pointer (a mouse or a pen; phones send none and draw none) at most every MOVE_MS while it moves,
 * nothing while it rests, and null when it leaves the window or the tab is hidden; its viewport at most every VIEW_MS
 * when it scrolls or resizes, and again when someone comes to the page. Nothing goes while nobody else is here. A
 * pointer that rests for IDLE_MS fades. The who-is-here list has a switch that hides the others' pointers, kept per
 * browser (localStorage).
 */

import { tipOn } from './tip';
import { LEAVE, PAGE } from './page';

type Who = { key: string; login: string | null; avatar: string | null };
/** Someone else on this page; `away` while every tab of theirs here is hidden. */
type Peer = Who & { away: boolean };
type Remote = {
	who: Peer;
	/** Their pointer as sent ([x, block, f]), where it is drawn (document px) and where it is going. */
	c: number[] | null;
	pos: { x: number; y: number } | null;
	to: { x: number; y: number } | null;
	el: HTMLElement | null;
	/** When their pointer last moved (ms). */
	t: number;
	/** Their viewport as sent ([block, f, block, f]). */
	v: number[] | null;
};

const MOVE_MS = 60;
const VIEW_MS = 200;
const IDLE_MS = 5000;
const HIDE_KEY = 'blaze.cursors.hide';
/** What counts as a block: what the text is made of, in every page's `main`. */
const BLOCKS = 'h1, h2, h3, h4, h5, h6, p, li, pre, table, tr, figure, blockquote, dt, dd, img, details, summary';
const SVG = 'http://www.w3.org/2000/svg';
const fine = matchMedia('(pointer: fine)');
/** Where the rail is the page's scrollbar; the same query hides the browser's in styles/shell.css. */
const desk = matchMedia('(hover: hover) and (pointer: fine)');
/** The thumb's least length (px), and how long the rail stays bright after the page last moved (ms). */
const THUMB_MIN = 28;
const DIM_MS = 1200;
const still = matchMedia('(prefers-reduced-motion: reduce)');

let send: (m: object) => void = () => {};
let face: (w: Who, away?: boolean) => HTMLElement = () => document.createElement('span');
let name: (w: Who) => string = (w) => w.key;
let path = '';
const peers = new Map<string, Peer>();
const remotes = new Map<string, Remote>();

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls = '', text = ''): HTMLElementTagNameMap[K] {
	const e = document.createElement(tag);
	if (cls) e.className = cls;
	if (text) e.textContent = text;
	return e;
}

/** A person's colour: an anonymous key's is its animal's hue (client/lib/presence.ts), anyone else's a hash. */
function hueOf(key: string): number {
	if (/^a[0-9a-f]{12}$/.test(key)) return parseInt(key.slice(1, 3), 16) * (360 / 256);
	let h = 2166136261;
	for (let i = 0; i < key.length; i++) h = Math.imul(h ^ key.charCodeAt(i), 16777619);
	return (h >>> 0) % 360;
}

const round = (x: number) => Math.round(x * 1000) / 1000;
const clampFrac = (x: number) => Math.max(-50, Math.min(50, x));

function hidden(): boolean {
	try {
		return localStorage.getItem(HIDE_KEY) === '1';
	} catch {
		return false;
	}
}

// ---- Where things are -------------------------------------------------------------------------------------------

/** Each block's top (document px) and height, -1 for one not shown (in a shut `<details>`). Dropped when it moves. */
let blocks: { top: number; h: number }[] | null = null;

function layout(): { top: number; h: number }[] {
	if (blocks) return blocks;
	const main = document.querySelector('main');
	const sy = scrollY;
	blocks = main
		? [...main.querySelectorAll<HTMLElement>(BLOCKS)]
				.filter((e) => !e.closest('.c-thread'))
				.map((e) => {
					const r = e.getBoundingClientRect();
					return { top: r.top + sy, h: e.getClientRects().length ? r.height : -1 };
				})
		: [];
	return blocks;
}

/** The anchor for document y: the shown block that starts last at or above it (the innermost, on a tie). */
function anchor(y: number): [number, number] | null {
	const bs = layout();
	let best = -1;
	for (let i = 0; i < bs.length; i++) if (bs[i].h > 0 && bs[i].top <= y && (best < 0 || bs[i].top >= bs[best].top)) best = i;
	if (best < 0) best = bs.findIndex((b) => b.h > 0);
	if (best < 0) return null;
	return [best, round(clampFrac((y - bs[best].top) / bs[best].h))];
}

/** Document y for an anchor, or null when this reader doesn't have that block shown. */
function place(b: number, f: number): number | null {
	const x = layout()[b];
	return x && x.h > 0 ? x.top + f * x.h : null;
}

// ---- What this tab sends ----------------------------------------------------------------------------------------

/** The pointer in window px, while it is over the page. */
let pointer: { x: number; y: number } | null = null;
let sentC = '';
let sentV = '';
let cTimer: ReturnType<typeof setTimeout> | undefined;
let vTimer: ReturnType<typeof setTimeout> | undefined;

function sendCursor(): void {
	cTimer = undefined;
	let c: number[] | null = null;
	const main = document.querySelector('main');
	const a = pointer && !document.hidden && main ? anchor(pointer.y + scrollY) : null;
	if (pointer && main && a) {
		const r = main.getBoundingClientRect();
		c = [round(clampFrac((pointer.x - r.left) / (r.width || 1))), ...a];
	}
	const s = JSON.stringify(c);
	if (s === sentC || (!sentC && !c)) return;
	sentC = s;
	send({ t: 'c', c });
}

function queueCursor(): void {
	if (peers.size && fine.matches) cTimer ??= setTimeout(sendCursor, MOVE_MS);
}

function sendView(): void {
	vTimer = undefined;
	const top = anchor(scrollY);
	const bottom = anchor(scrollY + innerHeight);
	const v = top && bottom ? [...top, ...bottom] : null;
	const s = JSON.stringify(v);
	if (s === sentV) return;
	sentV = s;
	send({ t: 'v', v });
}

function queueView(): void {
	if (peers.size) vTimer ??= setTimeout(sendView, VIEW_MS);
}

/** Drops what was sent, so the next change goes out whatever it is (someone new is here, or the socket is new). */
function resend(): void {
	sentC = '';
	sentV = '';
	if (pointer) queueCursor();
	queueView();
}

// ---- What the others are doing ----------------------------------------------------------------------------------

/** Over the page, scrolling with it: an absolute box as tall as the body, clipped so a pointer never widens it. */
const layer = el('div', 'cursors');
layer.setAttribute('aria-hidden', 'true');
const rail = el('div', 'reading');
/** The others' bands, under the track; the track (the scrollbar, on a desk) holds the reader's own, the thumb. */
const others = el('div', 'reading-others');
const track = el('div', 'reading-track');
const mine = el('span', 'reading-me');
track.append(mine);
rail.append(others, track);

function arrow(): SVGSVGElement {
	const svg = document.createElementNS(SVG, 'svg');
	svg.setAttribute('viewBox', '0 0 16 20');
	const p = document.createElementNS(SVG, 'path');
	p.setAttribute('d', 'M1.5 1.5v15l4-3.8 2.6 6 2.6-1.1-2.6-5.9h5.6Z');
	svg.append(p);
	return svg;
}

function cursorOf(r: Remote): HTMLElement {
	if (r.el) return r.el;
	const e = el('div', 'cursor');
	e.style.setProperty('--c', `hsl(${hueOf(r.who.key).toFixed(0)} 70% 50%)`);
	const chip = el('span', 'cursor-chip');
	chip.append(face(r.who), el('span', '', name(r.who)));
	e.append(arrow(), chip);
	layer.append(e);
	return (r.el = e);
}

/** Where `r`'s pointer goes on this reader's page, or null. */
function target(r: Remote): { x: number; y: number } | null {
	const main = document.querySelector('main');
	if (!r.c || !main) return null;
	const y = place(r.c[1], r.c[2]);
	if (y === null) return null;
	const m = main.getBoundingClientRect();
	return { x: m.left + scrollX + r.c[0] * m.width, y };
}

function aim(r: Remote): void {
	r.to = hidden() || !fine.matches ? null : target(r);
	if (!r.to) {
		r.el?.classList.add('idle');
		return;
	}
	const e = cursorOf(r);
	r.pos ??= { ...r.to };
	e.classList.toggle('idle', Date.now() - r.t > IDLE_MS);
	animate();
}

let raf = 0;
let then = 0;

/** Each pointer eases toward where it is going (about 70 ms behind), and the loop stops when all are there. */
function tick(now: number): void {
	raf = 0;
	const k = still.matches ? 1 : 1 - Math.exp(-Math.min(now - (then || now - 16), 100) / 70);
	then = now;
	let moving = false;
	for (const r of remotes.values()) {
		if (!r.el || !r.to || !r.pos) continue;
		r.pos.x += (r.to.x - r.pos.x) * k;
		r.pos.y += (r.to.y - r.pos.y) * k;
		if (Math.abs(r.to.x - r.pos.x) + Math.abs(r.to.y - r.pos.y) < 0.5) r.pos = { ...r.to };
		else moving = true;
		r.el.style.transform = `translate(${r.pos.x.toFixed(1)}px, ${r.pos.y.toFixed(1)}px)`;
	}
	if (moving) raf = requestAnimationFrame(tick);
	else then = 0;
}

function animate(): void {
	raf ||= requestAnimationFrame(tick);
}

/** The rail: a band and a face for each viewport sent, and the reader's own (the thumb, on a desk). */
function drawRail(): void {
	const H = document.documentElement.scrollHeight || 1;
	const bands: HTMLElement[] = [];
	for (const r of remotes.values()) {
		if (!r.v) continue;
		const y0 = place(r.v[0], r.v[1]);
		const y1 = place(r.v[2], r.v[3]);
		if (y0 === null || y1 === null) continue;
		const band = el('span', 'reading-band');
		band.style.setProperty('--c', `hsl(${hueOf(r.who.key).toFixed(0)} 70% 50%)`);
		band.style.top = `${((Math.max(0, y0) / H) * 100).toFixed(2)}%`;
		band.style.height = `${((Math.max(0, y1 - y0) / H) * 100).toFixed(2)}%`;
		const go = el('button', 'reading-face');
		go.type = 'button';
		const f = face(r.who, r.who.away);
		// The button's tooltip, not the face's.
		delete f.dataset.tip;
		go.append(f);
		tipOn(go, `${name(r.who)} is reading here`);
		go.addEventListener('click', () => scrollTo({ top: Math.max(0, y0 - 56), behavior: still.matches ? 'auto' : 'smooth' }));
		band.append(go);
		bands.push(band);
	}
	others.replaceChildren(...bands);
	const bar = desk.matches;
	if (bar) {
		track.setAttribute('role', 'scrollbar');
		track.setAttribute('aria-orientation', 'vertical');
		track.setAttribute('aria-valuemin', '0');
		track.setAttribute('aria-valuemax', '100');
		track.setAttribute('aria-label', 'Page');
		const pane = document.querySelector<HTMLElement>('.pane');
		if (pane) track.setAttribute('aria-controls', (pane.id ||= 'pane'));
	} else for (const a of ['role', 'aria-orientation', 'aria-valuemin', 'aria-valuemax', 'aria-valuenow', 'aria-label', 'aria-controls']) track.removeAttribute(a);
	rail.classList.toggle('bar', bar);
	rail.hidden = bar ? H - innerHeight < 1 : !bands.length;
	drawMine();
}

/** The thumb where the page is: proportional, but never shorter than THUMB_MIN, so the rest of the track maps on. */
function drawMine(): void {
	if (rail.hidden) return;
	const T = track.clientHeight;
	const room = Math.max(1, document.documentElement.scrollHeight - innerHeight);
	const t = Math.min(T, Math.max(THUMB_MIN, (innerHeight / (room + innerHeight)) * T));
	const at = Math.max(0, Math.min(1, scrollY / room));
	mine.style.top = `${(at * (T - t)).toFixed(1)}px`;
	mine.style.height = `${t.toFixed(1)}px`;
	const now = String(Math.round(at * 100));
	if (rail.classList.contains('bar') && track.getAttribute('aria-valuenow') !== now) track.setAttribute('aria-valuenow', now);
}

let mineRaf = 0;
let dimTimer: ReturnType<typeof setTimeout> | undefined;

/** The page moved: the thumb follows at the next frame, and the rail brightens for a while. */
function moved(): void {
	mineRaf ||= requestAnimationFrame(() => {
		mineRaf = 0;
		drawMine();
	});
	rail.classList.add('live');
	clearTimeout(dimTimer);
	dimTimer = setTimeout(() => rail.classList.remove('live'), DIM_MS);
}

/**
 * Pressing the rail (a desk only; the faces are buttons of their own): on the thumb it drags, anywhere else on the
 * track the page jumps so the thumb is centred there and the drag goes on from that point. The rail keeps the pointer
 * (capture) until it is let go, wherever it goes.
 */
function grab(e: PointerEvent): void {
	if (!rail.classList.contains('bar') || e.button !== 0 || (e.target as Element).closest('.reading-face')) return;
	e.preventDefault();
	const T = track.clientHeight;
	const room = Math.max(0, document.documentElement.scrollHeight - innerHeight);
	const t = mine.offsetHeight;
	// Page px per track px.
	const per = room / Math.max(1, T - t);
	let from = scrollY;
	if (e.target !== mine) {
		from = Math.max(0, Math.min(room, (e.clientY - track.getBoundingClientRect().top - t / 2) * per));
		scrollTo(scrollX, from);
	}
	const y0 = e.clientY;
	const move = (m: PointerEvent) => scrollTo(scrollX, from + (m.clientY - y0) * per);
	rail.setPointerCapture(e.pointerId);
	rail.classList.add('drag');
	rail.addEventListener('pointermove', move);
	rail.addEventListener(
		'lostpointercapture',
		() => {
			rail.classList.remove('drag');
			rail.removeEventListener('pointermove', move);
		},
		{ once: true },
	);
}

/** The page moved under the anchors (a resize, a fold, a thread opened): place everything again. */
function relayout(): void {
	blocks = null;
	layer.style.height = `${document.body.offsetHeight}px`;
	for (const r of remotes.values()) {
		if (!r.c) continue;
		r.pos = null;
		aim(r);
	}
	drawRail();
}

function forget(key: string): void {
	const r = remotes.get(key);
	r?.el?.remove();
	remotes.delete(key);
}

// ---- What client/lib/presence.ts calls ----------------------------------------------------------------------------

/** Who is on the page this tab shows (`at`, a site path), this reader left out. */
export function setPeople(at: string, list: Peer[]): void {
	if (at !== path) {
		path = at;
		peers.clear();
		for (const k of [...remotes.keys()]) forget(k);
		blocks = null;
	}
	let fresh = false;
	const now = new Set(list.map((p) => p.key));
	for (const k of [...peers.keys()]) if (!now.has(k)) peers.delete(k);
	for (const k of [...remotes.keys()]) if (!now.has(k)) forget(k);
	for (const p of list) {
		if (!peers.has(p.key)) fresh = true;
		peers.set(p.key, p);
		const r = remotes.get(p.key);
		if (r) r.who = p;
	}
	if (fresh) resend();
	drawRail();
}

/** The socket is new: what this tab sent went to the old one, and who is here comes again. */
export function reset(): void {
	peers.clear();
	sentC = '';
	sentV = '';
}

/** Takes a `c` or `v` frame; false for anything else. */
export function message(m: { t?: unknown }): boolean {
	if (m.t !== 'c' && m.t !== 'v') return false;
	const { k, c, v } = m as { k?: unknown; c?: unknown; v?: unknown };
	const who = typeof k === 'string' ? peers.get(k) : undefined;
	if (!who) return true;
	const nums = (x: unknown, n: number) => (Array.isArray(x) && x.length === n && x.every((y) => typeof y === 'number' && Number.isFinite(y)) ? (x as number[]) : null);
	let r = remotes.get(who.key);
	if (!r) remotes.set(who.key, (r = { who, c: null, pos: null, to: null, el: null, t: 0, v: null }));
	if (m.t === 'v') {
		r.v = nums(v, 4);
		drawRail();
	} else {
		r.c = nums(c, 3);
		r.t = Date.now();
		aim(r);
	}
	return true;
}

/** The switch for the who-is-here list: show or hide the others' pointers. Null on a phone, which draws none. */
export function toggle(): HTMLElement | null {
	if (!fine.matches) return null;
	const b = el('button', 'tl-btn tl-quiet cursors-toggle', hidden() ? 'Show cursors' : 'Hide cursors');
	b.type = 'button';
	b.setAttribute('aria-pressed', String(!hidden()));
	tipOn(b, hidden() ? "Show other readers' pointers on this page" : "Hide other readers' pointers on this page");
	b.addEventListener('click', (e) => {
		e.stopPropagation();
		try {
			if (hidden()) localStorage.removeItem(HIDE_KEY);
			else localStorage.setItem(HIDE_KEY, '1');
		} catch {
			// Storage off: nothing to keep it in.
		}
		const h = hidden();
		b.textContent = h ? 'Show cursors' : 'Hide cursors';
		b.setAttribute('aria-pressed', String(!h));
		tipOn(b, h ? "Show other readers' pointers on this page" : "Hide other readers' pointers on this page");
		for (const r of remotes.values()) aim(r);
	});
	return b;
}

export function setup(opts: { send: (m: object) => void; face: (w: Who, away?: boolean) => HTMLElement; name: (w: Who) => string }): void {
	({ send, face, name } = opts);
	rail.hidden = true;
	document.body.append(layer, rail);
	rail.addEventListener('pointerdown', grab);
	desk.addEventListener('change', drawRail);

	document.addEventListener(
		'pointermove',
		(e) => {
			if (e.pointerType === 'touch') return;
			pointer = { x: e.clientX, y: e.clientY };
			queueCursor();
		},
		{ passive: true },
	);
	// Out of the window: gone for the others at once.
	document.addEventListener('mouseout', (e) => {
		if (e.relatedTarget || !pointer) return;
		pointer = null;
		queueCursor();
	});
	addEventListener(
		'scroll',
		() => {
			if (pointer) queueCursor();
			queueView();
			moved();
		},
		{ passive: true },
	);
	addEventListener('resize', () => {
		relayout();
		queueView();
	});
	document.addEventListener('visibilitychange', () => {
		if (!document.hidden) return;
		pointer = null;
		clearTimeout(cTimer);
		sendCursor();
	});
	// Leaving the page: off the others' screens now, rather than at the next who-is-here.
	addEventListener(LEAVE, () => {
		clearTimeout(cTimer);
		clearTimeout(vTimer);
		cTimer = vTimer = undefined;
		if (sentC && sentC !== 'null') send({ t: 'c', c: null });
		if (sentV && sentV !== 'null') send({ t: 'v', v: null });
		sentC = sentV = '';
	});
	const ro = new ResizeObserver(() => relayout());
	const watch = () => {
		ro.disconnect();
		ro.observe(document.body);
		const main = document.querySelector('main');
		if (main) ro.observe(main);
	};
	addEventListener(PAGE, watch);
	watch();
	// Pointers that rest fade; one that moves again comes back (`aim`).
	setInterval(() => {
		const now = Date.now();
		for (const r of remotes.values()) if (r.el && now - r.t > IDLE_MS) r.el.classList.add('idle');
	}, 1000);
}
