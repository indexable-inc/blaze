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
 *
 * Saying something by the pointer, like Figma's cursor chat or Minecraft's T: on a desk T (outside a field, no
 * modifiers) opens a one-line composer at the reader's pointer (at the foot of the window when the pointer is off the
 * page). What is typed goes to the others on the page as it is typed (`say-live`, at most every SAY_MS), shown as a
 * bubble by the pointer, or by the face on the rail while the pointer is off their screen. Enter sends: the bubble
 * stays SAY_LINGER_MS and a signed-in reader's line goes to the chat too, to be kept (client/lib/chat.ts `post`);
 * an anonymous one's is only ever the bubble. Esc, or leaving the field, takes it back.
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
	/** The line they are writing by their pointer (`done` once sent), its bubble, and when it goes. */
	say: { text: string; done: boolean } | null;
	sayEl: HTMLElement | null;
	sayTimer: ReturnType<typeof setTimeout> | undefined;
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
let me: () => Who | null = () => null;
let post: (text: string) => boolean = () => false;
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
	// While the reader writes, their pointer stays where they pressed the key, for them and for the others.
	if (saying) return;
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
	sentSay = '';
	if (pointer) queueCursor();
	queueView();
	if (saying) queueSay();
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
	e.style.setProperty('--c', colour(r.who.key));
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
		return placeSay(r);
	}
	const e = cursorOf(r);
	r.pos ??= { ...r.to };
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
		band.style.setProperty('--c', colour(r.who.key));
		band.dataset.key = r.who.key;
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
	if (!b) {
		r.sayEl = el('div', 'cursor-say');
		r.sayEl.style.setProperty('--c', colour(r.who.key));
	}
	r.sayEl!.textContent = r.say.text;
	r.sayEl!.classList.toggle('live', !r.say.done);
	placeSay(r);
}

/** By their pointer while it is on this reader's screen, else by their face on the rail, else nowhere for now. */
function placeSay(r: Remote): void {
	const b = r.sayEl;
	if (!b) return;
	const to = r.to;
	if (r.el && to && !r.el.classList.contains('idle') && to.y > scrollY && to.y < scrollY + innerHeight - 40) {
		b.classList.remove('on-rail');
		// Near the right edge it opens to the left of the pointer, so the page's clip never cuts it.
		b.classList.toggle('flip', to.x - scrollX > innerWidth - 300);
		if (b.parentElement !== r.el) r.el.append(b);
		return;
	}
	const band = rail.hidden ? null : others.querySelector(`.reading-band[data-key="${CSS.escape(r.who.key)}"]`);
	if (!band) return b.remove();
	b.classList.remove('flip');
	b.classList.add('on-rail');
	if (b.parentElement !== band) band.append(b);
}

/** The reader's own composer: one line, in their colour, by their pointer. */
const sayBox = el('form', 'say-live');
sayBox.hidden = true;
const sayField = el('input');
sayField.type = 'text';
sayField.maxLength = SAY_MAX;
sayField.autocomplete = 'off';
sayField.enterKeyHint = 'send';
sayField.setAttribute('aria-label', 'Say something on this page');
const sayKey = el('kbd', '', '↵');
sayKey.setAttribute('aria-hidden', 'true');
sayBox.append(sayField, sayKey);
/** Whether the composer is open; what the others have of it ('' nothing); the timers that send it. */
let saying = false;
let sentSay = '';
let sayQueue: ReturnType<typeof setTimeout> | undefined;
let sayKeep: ReturnType<typeof setInterval> | undefined;
let sayFade: ReturnType<typeof setTimeout> | undefined;
/** Where on the page (document px) the composer opened, or null at the foot of the window. */
let sayAt: { x: number; y: number } | null = null;

const line = (v: string) => v.replace(/\s+/g, ' ').trim();

function pushSay(again = false): void {
	sayQueue = undefined;
	const text = line(sayField.value);
	if (!saying || !peers.size || (text === sentSay && !again) || (!text && !sentSay)) return;
	sentSay = text;
	send({ t: 'say-live', text: text || null });
}

function queueSay(): void {
	sayQueue ??= setTimeout(pushSay, SAY_MS);
}

/**
 * Where the pointer was when the composer opened, scrolling with the page and kept inside the window; at the foot of
 * the window when it opened with the pointer off the page.
 */
function placeBox(): void {
	if (sayBox.hidden) return;
	sayBox.classList.toggle('at-foot', !sayAt);
	if (!sayAt) return void (sayBox.style.left = sayBox.style.top = '');
	sayBox.style.left = `${Math.round(Math.max(8, Math.min(innerWidth - sayBox.offsetWidth - 8, sayAt.x - scrollX + 14)))}px`;
	sayBox.style.top = `${Math.round(Math.max(8, Math.min(innerHeight - sayBox.offsetHeight - 8, sayAt.y - scrollY + 18)))}px`;
}

function openSay(): void {
	if (saying) return sayField.focus();
	clearTimeout(sayFade);
	const w = me();
	saying = true;
	sentSay = '';
	sayBox.classList.remove('sent', 'out');
	sayBox.style.setProperty('--c', colour(w?.key ?? ''));
	sayField.readOnly = false;
	sayField.value = '';
	sayField.placeholder = w?.login ? 'Say something on this page' : 'Say hi to this page (not kept)';
	tipOn(sayKey, 'Enter sends, Esc cancels');
	sayBox.querySelector(':scope > .face')?.remove();
	if (w) sayBox.prepend(face(w));
	sayAt = pointer ? { x: pointer.x + scrollX, y: pointer.y + scrollY } : null;
	// The others' copy of the pointer parks here too, so the bubble they see stays put.
	clearTimeout(cTimer);
	if (peers.size) sendCursor();
	sayBox.hidden = false;
	placeBox();
	sayField.focus({ preventScroll: true });
	sayKeep = setInterval(() => sentSay && pushSay(true), SAY_KEEP_MS);
}

/** Shuts the composer: with `text` it was sent (a bubble a while, and the chat), without it taken back. */
function closeSay(text: string | null): void {
	if (!saying) return;
	saying = false;
	clearInterval(sayKeep);
	clearTimeout(sayQueue);
	sayQueue = undefined;
	if (text) {
		if (peers.size) send({ t: 'say-live', text, done: true });
		post(text);
		sayField.value = text;
		sayField.readOnly = true;
		sayBox.classList.add('sent');
		sayField.blur();
		sayFade = setTimeout(() => {
			if (still.matches) return void (sayBox.hidden = true);
			sayBox.classList.add('out');
			sayFade = setTimeout(() => (sayBox.hidden = true), 300);
		}, SAY_LINGER_MS);
	} else {
		if (sentSay) send({ t: 'say-live', text: null });
		sayBox.hidden = true;
	}
	sentSay = '';
	queueCursor();
}

sayField.addEventListener('input', () => {
	queueSay();
	placeBox();
});
sayField.addEventListener('keydown', (e) => {
	if (e.isComposing) return;
	if (e.key === 'Enter') {
		e.preventDefault();
		closeSay(line(sayField.value) || null);
	} else if (e.key === 'Escape') {
		// Only the composer: nothing else's Esc (the chat, the who-is-here list) on the same press.
		e.preventDefault();
		e.stopPropagation();
		closeSay(null);
	}
});
sayField.addEventListener('blur', () => closeSay(null));
sayBox.addEventListener('submit', (e) => e.preventDefault());
// A press on the box (its padding, the key) keeps the field's focus.
sayBox.addEventListener('mousedown', (e) => {
	if (e.target !== sayField) e.preventDefault();
});

/**
 * T, on a desk, when the key would type nothing anywhere else; Enter too, for readers whose T an extension takes first
 * (Vimium opens a tab with it, before the page hears the key), when it would not press a link or a button.
 */
function sayKeyDown(e: KeyboardEvent): void {
	if (e.defaultPrevented || e.repeat || e.isComposing || e.metaKey || e.ctrlKey || e.altKey || e.shiftKey) return;
	if ((e.key !== 't' && e.key !== 'T' && e.key !== 'Enter') || !fine.matches) return;
	const t = e.target instanceof HTMLElement ? e.target : null;
	if (t && (t.isContentEditable || t.closest('input, textarea, select, [contenteditable]'))) return;
	if (e.key === 'Enter' && t?.closest('a[href], button, summary, [role="button"], [tabindex]')) return;
	if (document.querySelector('dialog[open]') || document.body.classList.contains('chat-full')) return;
	e.preventDefault();
	openSay();
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
	sentSay = '';
}

/** Takes a `c`, `v` or `say-live` frame; false for anything else. */
export function message(m: { t?: unknown }): boolean {
	if (m.t !== 'c' && m.t !== 'v' && m.t !== 'say-live') return false;
	const { k, c, v, text, done } = m as { k?: unknown; c?: unknown; v?: unknown; text?: unknown; done?: unknown };
	const who = typeof k === 'string' ? peers.get(k) : undefined;
	if (!who) return true;
	const nums = (x: unknown, n: number) => (Array.isArray(x) && x.length === n && x.every((y) => typeof y === 'number' && Number.isFinite(y)) ? (x as number[]) : null);
	let r = remotes.get(who.key);
	if (!r) remotes.set(who.key, (r = { who, c: null, pos: null, to: null, el: null, t: 0, v: null, say: null, sayEl: null, sayTimer: undefined }));
	if (m.t === 'say-live') hearSay(r, typeof text === 'string' && text ? text.slice(0, SAY_MAX) : null, done === true);
	else if (m.t === 'v') {
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

/** A line for the who-is-here list: how to say something by the pointer. Null on a phone, which has no T. */
export function hint(): HTMLElement | null {
	if (!fine.matches) return null;
	const p = el('p', 'tl-hint');
	p.append('Press ', el('kbd', '', 'T'), ' or ', el('kbd', '', '↵'), ' to say something on this page');
	tipOn(p, 'If Vimium or another extension takes T, use Enter, or exclude the key t for ix.dev in its options.');
	return p;
}

export function setup(opts: {
	send: (m: object) => void;
	face: (w: Who, away?: boolean) => HTMLElement;
	name: (w: Who) => string;
	/** The reader, once the socket says (for their colour and whether they can post). */
	me: () => Who | null;
	/** Posts a line to the chat; false when the reader can't (not signed in). */
	post: (text: string) => boolean;
}): void {
	({ send, face, name, me, post } = opts);
	rail.hidden = true;
	document.body.append(layer, rail, sayBox);
	document.addEventListener('keydown', sayKeyDown);
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
			placeBox();
			for (const r of remotes.values()) placeSay(r);
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
		closeSay(null);
		clearTimeout(sayFade);
		sayBox.hidden = true;
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
