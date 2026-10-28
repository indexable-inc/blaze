/**
 * Where the others on this page are (worker/presence.ts passes it on; client/lib/presence.ts owns the socket and
 * hands over the frames, `message`, and who is on the page, `setPeople`): their mouse pointer, an arrow in their
 * colour with their face and name, and their face on a thin rail at the right edge where their pointer is on the page
 * (click the face to go there). Where a reader has scrolled is not sent: only the pointer.
 *
 * On a desk (a mouse or a trackpad, which hovers) the rail is also the page's scrollbar, there whenever the page
 * scrolls, whoever else is here: styles/shell.css hides the browser's own, the reader's viewport is the thumb (drag
 * it, or press the track to jump there and drag on), and it dims while nothing moves, like an overlay scrollbar. It
 * takes no focus, so the keys scroll the page as ever. On a phone the browser's scrollbar stays and the rail shows
 * only the others, while there are any.
 *
 * A pointer is sent as what it is on, so it lands on the same thing for every reader whatever their window and layout
 * (client/lib/anchor.ts): a block of the article and the character in it, or a link, a button, ... in the sidebar,
 * the top line or "On this page". A pointer on one of those regions is drawn over the window
 * (`fixedLayer`), since they stay put while the page scrolls; one in the article on the page (`layer`). A reader who
 * doesn't have that thing on screen (the sidebar shut) doesn't see the pointer until it moves to
 * something they have, and only a pointer in the article has a face on the rail.
 *
 * A tab sends its pointer (a mouse or a pen; phones send none and draw none) at most every MOVE_MS while it moves,
 * nothing while it rests, and null when it leaves the window or the tab is hidden; again when someone comes to the
 * page. Nothing goes while nobody else is here. A
 * pointer that rests for IDLE_MS fades. The who-is-here list has a switch that hides the others' pointers, kept per
 * browser (localStorage).
 *
 * What the reader writes in the chat's input (client/lib/feed.ts, opened with T) goes to the others on the page as it
 * is typed (`say`, sent as `say-live` at most every SAY_MS), shown as a bubble by the writer's pointer, in its layer, or
 * by their face on the rail while the pointer is off this reader's screen or not drawn (a phone). Once sent the
 * bubble stays SAY_LINGER_MS where it was: on the page, or in the window for a pointer on a region.
 */

import { tipOn } from './tip';
import { LEAVE, PAGE } from './page';
import { SHELL, pageTop, placeOf, remeasure, spotAt, spotOf, type Place, type Spot } from './anchor';

type Who = { key: string; login: string | null; avatar: string | null };
/** Someone else on this page; `away` while every tab of theirs here is hidden. */
type Peer = Who & { away: boolean };
type Remote = {
	who: Peer;
	/** Their pointer as sent, which layer it is drawn in, and where it is drawn and where it is going, in that layer's px. */
	c: Spot | null;
	fixed: boolean;
	pos: { x: number; y: number } | null;
	to: { x: number; y: number } | null;
	el: HTMLElement | null;
	/** When their pointer last moved (ms). */
	t: number;
	/** The line they are writing by their pointer (`done` once sent), its bubble, and when it goes. */
	say: { text: string; done: boolean } | null;
	sayEl: HTMLElement | null;
	sayTimer: ReturnType<typeof setTimeout> | undefined;
};

const MOVE_MS = 60;
const IDLE_MS = 5000;
const HIDE_KEY = 'blaze.cursors.hide';
const SVG = 'http://www.w3.org/2000/svg';
const fine = matchMedia('(pointer: fine)');
/** Where the rail is the page's scrollbar; the same query hides the browser's in styles/shell.css. */
const desk = matchMedia('(hover: hover) and (pointer: fine)');
/** The thumb's least length (px), and how long the rail stays bright after the page last moved (ms). */
const THUMB_MIN = 28;
const DIM_MS = 1200;
const still = matchMedia('(prefers-reduced-motion: reduce)');
/** A line by the pointer: as long as a chat message (worker/presence.ts CHAT_MAX), sent at most every SAY_MS while it
 * changes and again every SAY_KEEP_MS while it rests (someone new on the page gets it); a bubble with no word from its
 * writer for SAY_STALE_MS goes (a lost take-back), and a sent one stays SAY_LINGER_MS. */
const SAY_MAX = 500;
const SAY_MS = 100;
const SAY_KEEP_MS = 4000;
const SAY_STALE_MS = 10_000;
const SAY_LINGER_MS = 4000;

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

const colour = (key: string) => `hsl(${hueOf(key).toFixed(0)} 70% 50%)`;

function hidden(): boolean {
	try {
		return localStorage.getItem(HIDE_KEY) === '1';
	} catch {
		return false;
	}
}

// ---- What this tab sends ----------------------------------------------------------------------------------------

/** The pointer in window px, while it is over the page. */
let pointer: { x: number; y: number } | null = null;
let sentC = '';
let cTimer: ReturnType<typeof setTimeout> | undefined;

function sendCursor(): void {
	cTimer = undefined;
	const c = pointer && !document.hidden ? spotAt(pointer.x, pointer.y, mineOnly) : null;
	const s = JSON.stringify(c);
	if (s === sentC || (!sentC && !c)) return;
	sentC = s;
	send({ t: 'c', c });
}

function queueCursor(): void {
	if (peers.size && fine.matches) cTimer ??= setTimeout(sendCursor, MOVE_MS);
}

/** Drops what was sent, so the next change goes out whatever it is (someone new is here, or the socket is new). */
function resend(): void {
	sentC = '';
	sentSay = '';
	if (pointer) queueCursor();
	if (sayText) queueSay();
}

// ---- What the others are doing ----------------------------------------------------------------------------------

/** Over the page, scrolling with it: an absolute box as tall as the body, clipped so a pointer never widens it. The
 * pointers in the article are drawn there; those on the regions beside it in `fixedLayer`, over the window. */
const layer = el('div', 'cursors');
layer.setAttribute('aria-hidden', 'true');
const fixedLayer = el('div', 'cursors cursors-fixed');
fixedLayer.setAttribute('aria-hidden', 'true');
const rail = el('div', 'reading');
/** The others' bands, under the track; the track (the scrollbar, on a desk) holds the reader's own, the thumb. */
const others = el('div', 'reading-others');
const track = el('div', 'reading-track');
const mine = el('span', 'reading-me');
track.append(mine);
rail.append(others, track);

/** This tab's own overlays, which a pointer is never on as far as the others go: what is under them is. */
const mineOnly: Element[] = [];

/** Where a layer's px start in the window's: the fixed layer is the window, the page's layer is off by the scroll. */
const origin = (fixed: boolean) => (fixed ? { x: 0, y: 0 } : { x: -scrollX, y: -pageTop() });
/** A place in its layer's px. */
function onLayer(p: Place): { x: number; y: number } {
	const o = origin(p.fixed);
	return { x: p.x - o.x, y: p.y - o.y };
}

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
	e.style.setProperty('--c', colour(r.who.key));
	const chip = el('span', 'cursor-chip');
	chip.append(face(r.who), el('span', '', name(r.who)));
	e.append(arrow(), chip);
	layer.append(e);
	return (r.el = e);
}

/**
 * Points `r`'s pointer where it now is on this reader's screen, or fades it while they don't have that. Moving between
 * the article and a region, it changes layers and keeps its place on screen, so it still glides; `snap` puts it there
 * at once (a region's pointer while the page or the region scrolls, which must not trail).
 */
function aim(r: Remote, snap = false): void {
	const at = hidden() || !fine.matches || !r.c ? null : placeOf(r.c);
	if (!at) {
		r.to = null;
		r.el?.classList.add('idle');
		return placeSay(r);
	}
	const to = (r.to = onLayer(at));
	const e = cursorOf(r);
	if (r.fixed !== at.fixed) {
		(at.fixed ? fixedLayer : layer).append(e);
		const [a, b] = [origin(r.fixed), origin(at.fixed)];
		if (r.pos) r.pos = { x: r.pos.x + a.x - b.x, y: r.pos.y + a.y - b.y };
		r.fixed = at.fixed;
	}
	if (snap || !r.pos) {
		r.pos = { ...to };
		e.style.transform = `translate(${to.x.toFixed(1)}px, ${to.y.toFixed(1)}px)`;
	}
	e.classList.toggle('idle', Date.now() - r.t > IDLE_MS);
	placeSay(r);
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

let railQueued = 0;
/** drawRail at the next frame, once however many pointers moved. */
function queueRail(): void {
	railQueued ||= requestAnimationFrame(() => {
		railQueued = 0;
		drawRail();
	});
}

/** The rail: a mark and a face where each other reader's pointer is, and the reader's own place (the thumb, on a desk). */
function drawRail(): void {
	const H = document.documentElement.scrollHeight || 1;
	const bands: HTMLElement[] = [];
	for (const r of remotes.values()) {
		// Only a pointer in the article has a place on the page.
		const at = r.c && placeOf(r.c);
		if (!at || at.fixed) continue;
		const y0 = onLayer(at).y;
		const band = el('span', 'reading-band');
		band.style.setProperty('--c', colour(r.who.key));
		band.dataset.key = r.who.key;
		band.style.top = `${((Math.max(0, y0) / H) * 100).toFixed(2)}%`;
		const go = el('button', 'reading-face');
		go.type = 'button';
		const f = face(r.who, r.who.away);
		// The button's tooltip, not the face's.
		delete f.dataset.tip;
		go.append(f);
		tipOn(go, `${name(r.who)}'s pointer is here`);
		go.addEventListener('click', () => scrollTo({ top: Math.max(0, y0 - innerHeight / 3), behavior: still.matches ? 'auto' : 'smooth' }));
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
	for (const r of remotes.values()) placeSay(r);
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
	remeasure();
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
	r?.sayEl?.remove();
	clearTimeout(r?.sayTimer);
	remotes.delete(key);
}

// ---- Lines said by the pointer ----------------------------------------------------------------------------------

/** Someone's line came (null: taken back). A pointer that is writing stays awake. */
function hearSay(r: Remote, text: string | null, done: boolean): void {
	clearTimeout(r.sayTimer);
	r.say = text === null ? null : { text, done };
	if (r.say) {
		r.t = Date.now();
		if (r.c) aim(r);
		r.sayTimer = setTimeout(() => {
			r.say = null;
			drawSay(r);
		}, done ? SAY_LINGER_MS : SAY_STALE_MS);
	}
	drawSay(r);
}

function drawSay(r: Remote): void {
	const b = r.sayEl;
	if (!r.say) {
		if (!b) return;
		r.sayEl = null;
		if (still.matches || !b.isConnected) return b.remove();
		b.classList.add('out');
		setTimeout(() => b.remove(), 300);
		return;
	}
	if (b?.classList.contains('pinned') && !r.say.done) {
		// A new line after a sent one: the sent one stays its while, and the new one follows the pointer.
		setTimeout(() => {
			b.classList.add('out');
			setTimeout(() => b.remove(), 300);
		}, SAY_LINGER_MS);
		r.sayEl = null;
	}
	if (!r.sayEl) {
		r.sayEl = el('div', 'cursor-say');
		r.sayEl.style.setProperty('--c', colour(r.who.key));
	}
	r.sayEl!.textContent = r.say.text;
	r.sayEl!.classList.toggle('live', !r.say.done);
	placeSay(r);
}

/**
 * By their pointer while it is on this reader's screen, else by their face on the rail, else nowhere for now. Once sent,
 * a bubble by the pointer stays on the page where it was, and the pointer moves on.
 */
function placeSay(r: Remote): void {
	const b = r.sayEl;
	if (!b || b.classList.contains('pinned')) return;
	if (r.say?.done && b.parentElement === r.el && r.el) {
		// Into the pointer's layer: on the page for one in the article, in the window for one on a region, which stays put.
		const into = r.fixed ? fixedLayer : layer;
		const at = b.getBoundingClientRect();
		const to = into.getBoundingClientRect();
		b.classList.remove('flip');
		b.classList.add('pinned');
		b.style.left = `${Math.round(at.left - to.left)}px`;
		b.style.top = `${Math.round(at.top - to.top)}px`;
		into.append(b);
		return;
	}
	// In the window.
	const o = origin(r.fixed);
	const to = r.to && { x: r.to.x + o.x, y: r.to.y + o.y };
	if (r.el && to && !r.el.classList.contains('idle') && to.y > 0 && to.y < innerHeight - 40) {
		b.classList.remove('on-rail');
		// Near the right edge it opens to the left of the pointer, so the page's clip never cuts it.
		b.classList.toggle('flip', to.x > innerWidth - 300);
		if (b.parentElement !== r.el) r.el.append(b);
		return;
	}
	const band = rail.hidden ? null : others.querySelector(`.reading-band[data-key="${CSS.escape(r.who.key)}"]`);
	if (!band) return b.remove();
	b.classList.remove('flip');
	b.classList.add('on-rail');
	if (b.parentElement !== band) band.append(b);
}

/**
 * The line the reader is writing (client/lib/feed.ts's input, client/lib/chat.ts): what the others have of it ('' none),
 * what it is now, and the timers that send it.
 */
let sayText = '';
let sentSay = '';
let sayQueue: ReturnType<typeof setTimeout> | undefined;
let sayKeep: ReturnType<typeof setInterval> | undefined;

function pushSay(again = false): void {
	sayQueue = undefined;
	if (!peers.size || (sayText === sentSay && !again) || (!sayText && !sentSay)) return;
	sentSay = sayText;
	send({ t: 'say-live', text: sayText || null });
}

function queueSay(): void {
	sayQueue ??= setTimeout(pushSay, SAY_MS);
}

/**
 * The line the reader is writing, as it stands: the others on the page see it by the reader's pointer as it is typed.
 * Null (or empty) takes it back; `done` sends it, and their bubble stays SAY_LINGER_MS where the pointer was.
 */
export function say(text: string | null, done = false): void {
	const t = (text ?? '').replace(/\s+/g, ' ').trim().slice(0, SAY_MAX);
	if (done) {
		clearTimeout(sayQueue);
		clearInterval(sayKeep);
		sayQueue = sayKeep = undefined;
		if (t && peers.size) send({ t: 'say-live', text: t, done: true });
		sayText = sentSay = '';
		return;
	}
	sayText = t;
	if (!t) {
		clearInterval(sayKeep);
		sayKeep = undefined;
	} else sayKeep ??= setInterval(() => sentSay && pushSay(true), SAY_KEEP_MS);
	queueSay();
}

// ---- What client/lib/presence.ts calls ----------------------------------------------------------------------------

/** Who is on the page this tab shows (`at`, a site path), this reader left out. */
export function setPeople(at: string, list: Peer[]): void {
	if (at !== path) {
		path = at;
		peers.clear();
		for (const k of [...remotes.keys()]) forget(k);
		remeasure();
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
	sentSay = '';
}

/** Takes a `c` or `say-live` frame; false for anything else. */
export function message(m: { t?: unknown }): boolean {
	if (m.t !== 'c' && m.t !== 'say-live') return false;
	const { k, c, text, done } = m as { k?: unknown; c?: unknown; text?: unknown; done?: unknown };
	const who = typeof k === 'string' ? peers.get(k) : undefined;
	if (!who) return true;
	let r = remotes.get(who.key);
	if (!r) remotes.set(who.key, (r = { who, c: null, fixed: false, pos: null, to: null, el: null, t: 0, say: null, sayEl: null, sayTimer: undefined }));
	if (m.t === 'say-live') hearSay(r, typeof text === 'string' && text ? text.slice(0, SAY_MAX) : null, done === true);
	else {
		r.c = spotOf(c);
		r.t = Date.now();
		aim(r);
		queueRail();
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

export function setup(opts: {
	send: (m: object) => void;
	face: (w: Who, away?: boolean) => HTMLElement;
	name: (w: Who) => string;
}): void {
	({ send, face, name } = opts);
	rail.hidden = true;
	document.body.append(layer, fixedLayer, rail);
	mineOnly.push(rail);
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
	// The page or anything in it scrolled (captured, since a list's scroll doesn't bubble): what the pointer is on may be
	// another thing, and the others' pointers on a region go with what they are on.
	document.addEventListener(
		'scroll',
		(e) => {
			if (pointer) queueCursor();
			if (e.target === document) moved();
			for (const r of remotes.values()) {
				if (r.c && (r.fixed || !r.to)) aim(r, true);
				else placeSay(r);
			}
		},
		{ capture: true, passive: true },
	);
	addEventListener('resize', () => relayout());
	document.addEventListener('visibilitychange', () => {
		if (!document.hidden) return;
		pointer = null;
		clearTimeout(cTimer);
		sendCursor();
	});
	// Leaving the page: off the others' screens now, rather than at the next who-is-here.
	addEventListener(LEAVE, () => {
		// The line being written stays in the feed's input; the next page's readers get it (`resend`).
		if (sentSay) send({ t: 'say-live', text: null });
		sentSay = '';
		clearTimeout(cTimer);
		cTimer = undefined;
		if (sentC && sentC !== 'null') send({ t: 'c', c: null });
		sentC = '';
	});
	const ro = new ResizeObserver(() => relayout());
	const watch = () => {
		ro.disconnect();
		ro.observe(document.body);
		// The article, and the regions beside it, which open, shut and move with no change to the body.
		for (const e of document.querySelectorAll(SHELL)) ro.observe(e);
	};
	addEventListener(PAGE, watch);
	watch();
	// Pointers that rest fade; one that moves again comes back (`aim`).
	setInterval(() => {
		const now = Date.now();
		for (const r of remotes.values()) if (r.el && now - r.t > IDLE_MS) r.el.classList.add('idle');
	}, 1000);
}
