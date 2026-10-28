/**
 * The overlay feed, like Minecraft's chat: one stack of lines at the bottom left of the page, over the article, that
 * carries everything passing: chat messages (client/lib/chat.ts), who joined and who left (client/lib/presence.ts),
 * mentions of the reader and replies in their threads, a message the chat refused, and the site's own word (a new
 * version: client/lib/update.ts). It lives on `body`, outside what the client router swaps, so it outlives a page.
 *
 * A line shows for LINE_MS and fades; while the pointer is on the stack none fades. A chat line opens its message in
 * the chat panel, a join or a leave goes to the page they were on. Muting (the bell in the chat panel) keeps the
 * social lines (chat, joins, leaves) out, never what is for the reader (a mention, a reply in their thread, an error,
 * the site's word). Everything also goes into the feed's history (the last HISTORY lines), shown when it is open.
 *
 * T, or Enter for readers whose T an extension takes (Vimium), when the key would type nothing and press nothing,
 * opens it: the history, scrollable, over the input at its foot (client/lib/chat.ts makes the input, `setInput`, and
 * decides what a line sent there does). Esc, a press outside it, or sending closes it. A phone has no T: a button at
 * the feed's foot opens it. Reduced motion: lines come and go without moving or fading.
 */

import { tipOn } from './tip';

export type Kind = 'chat' | 'join' | 'leave' | 'notice' | 'error' | 'local';
export type Line = {
	/** Set to change or take back the line later (a chat message is `m<id>`). */
	id?: string;
	kind: Kind;
	/** When it happened (ms), for its place in the history; now when left out. */
	at?: number;
	/** A face (client/lib/presence.ts) or an icon. */
	face?: Element;
	who?: string;
	text: string;
	/** A quieter word after the text. */
	note?: string;
	/** For the reader: a mention of them, or a reply in a thread they are in (shown even muted, marked). */
	tone?: 'mention' | 'yours';
	/** A click goes there, or runs `open`. */
	href?: string;
	open?: () => void;
	/** One button at the end of the line (a notice's "Reload now"). */
	action?: { label: string; run: () => void };
	/** Pops up now; false puts it in the history only (the chat's past, or a chat line while the panel is open). */
	live?: boolean;
	/** Stays up until taken back, rather than fading. */
	sticky?: boolean;
};

const LINE_MS = 10_000;
/** After the pointer leaves the stack, lines due to fade get this much more. */
const GRACE_MS = 2500;
const HISTORY = 150;
/** At most this many lines up at once while the feed is shut; older ones fade early. */
const SHOWN = 6;
const MUTE_KEY = 'blaze.chat.mute';
const still = matchMedia('(prefers-reduced-motion: reduce)');

type Entry = { id?: string; at: number; el: HTMLElement; line: Line; until: number | null };
const entries: Entry[] = [];

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls = '', text = ''): HTMLElementTagNameMap[K] {
	const e = document.createElement(tag);
	if (cls) e.className = cls;
	if (text) e.textContent = text;
	return e;
}

export function icon(d: string): SVGSVGElement {
	const ns = 'http://www.w3.org/2000/svg';
	const svg = document.createElementNS(ns, 'svg');
	svg.setAttribute('viewBox', '0 0 16 16');
	svg.setAttribute('aria-hidden', 'true');
	const p = document.createElementNS(ns, 'path');
	p.setAttribute('d', d);
	svg.append(p);
	return svg;
}
export const REFRESH = 'M13 8a5 5 0 1 1-1.6-3.7M13.5 2v3h-3';
export const CROSS = 'M4 4l8 8M12 4l-8 8';
const BUBBLE = 'M2.5 3.5h11v7.5H8l-3 2.5V11H2.5Z';

const root = el('section', 'feed');
root.setAttribute('aria-label', 'Chat and what is happening on the site');
const log = el('div', 'feed-log');
log.setAttribute('role', 'log');
log.setAttribute('aria-live', 'polite');
const foot = el('div', 'feed-foot');
foot.hidden = true;
/** A phone's way in (no T there): a small button under the lines. */
const opener = el('button', 'feed-opener');
opener.type = 'button';
opener.setAttribute('aria-label', 'Open the chat input');
opener.append(icon(BUBBLE));
tipOn(opener, 'Chat');
root.append(log, foot, opener);

let held = false;
let tick: ReturnType<typeof setInterval> | undefined;
let onOpen: () => void = () => {};
let onClose: () => void = () => {};

export function muted(): boolean {
	try {
		return localStorage.getItem(MUTE_KEY) === '1';
	} catch {
		return false;
	}
}

/** Mutes the social lines; those already up go at once. */
export function setMuted(m: boolean): void {
	try {
		if (m) localStorage.setItem(MUTE_KEY, '1');
		else localStorage.removeItem(MUTE_KEY);
	} catch {
		// Storage refused: the choice lasts this page.
	}
	if (m) for (const e of entries) if (social(e.line) && e.until !== null) settle(e, true);
}

const social = (l: Line) => (l.kind === 'chat' || l.kind === 'join' || l.kind === 'leave') && !l.tone;
export const isOpen = () => root.classList.contains('open');

function draw(l: Line): HTMLElement {
	const clickable = !!(l.href || l.open);
	const row: HTMLElement = l.href ? el('a', 'feed-line') : el('div', 'feed-line');
	if (row instanceof HTMLAnchorElement && l.href) row.href = l.href;
	row.classList.add(`feed-${l.kind}`);
	if (l.tone) row.classList.add(l.tone);
	if (l.face) {
		const f = l.face;
		if (f instanceof HTMLElement) delete f.dataset.tip;
		row.append(f);
	}
	const words = el('span', 'feed-words');
	if (l.who) words.append(el('b', '', l.who), ' ');
	words.append(el('span', 'feed-text', l.text));
	if (l.note) words.append(' ', el('span', 'feed-note', l.note));
	row.append(words);
	if (l.action) {
		const { label, run } = l.action;
		const b = el('button', 'feed-act', label);
		b.type = 'button';
		b.addEventListener('click', (e) => {
			e.stopPropagation();
			run();
		});
		row.append(b);
	}
	if (l.open && !l.href) {
		row.tabIndex = 0;
		row.setAttribute('role', 'button');
		const go = () => {
			l.open?.();
			close();
		};
		row.addEventListener('click', (e) => {
			if (!(e.target instanceof Element && e.target.closest('.feed-act'))) go();
		});
		row.addEventListener('keydown', (e) => {
			if (e.key === 'Enter' || e.key === ' ') {
				e.preventDefault();
				go();
			}
		});
	}
	if (clickable) row.classList.add('go');
	return row;
}

/** Shows a line (or changes the one with its id). The returned function takes it back. */
export function push(l: Line): () => void {
	const at = l.at ?? Date.now();
	const old = l.id ? entries.find((e) => e.id === l.id) : undefined;
	const live = l.live !== false && !(social(l) && muted());
	if (old) {
		const row = draw(l);
		row.className = `${row.className} ${old.el.classList.contains('fresh') ? 'fresh' : ''}`.trim();
		old.el.replaceWith(row);
		old.el = row;
		old.line = l;
		if (live && old.until === null) show(old);
		return () => take(old);
	}
	const e: Entry = { id: l.id, at, el: draw(l), line: l, until: null };
	// In time order: the chat's past comes with the times it was said.
	let i = entries.length;
	while (i > 0 && entries[i - 1].at > at) i--;
	const stick = near();
	entries.splice(i, 0, e);
	log.insertBefore(e.el, entries[i + 1]?.el ?? null);
	while (entries.length > HISTORY) take(entries[0]);
	if (live) show(e);
	if (stick || !isOpen()) log.scrollTop = log.scrollHeight;
	return () => take(e);
}

/** Up now, for LINE_MS (or until taken back); the oldest of too many goes early. */
function show(e: Entry): void {
	e.until = e.line.sticky ? Infinity : Date.now() + LINE_MS;
	e.el.classList.remove('fading');
	e.el.classList.add('fresh');
	const up = entries.filter((x) => x.until !== null && !x.line.sticky);
	for (const x of up.slice(0, Math.max(0, up.length - SHOWN))) settle(x, true);
	tick ??= setInterval(step, 500);
}

/** Its time is up: it fades out of the stack (it stays in the history). */
function settle(e: Entry, now = false): void {
	e.until = null;
	if (still.matches || now || isOpen()) return void e.el.classList.remove('fresh', 'fading');
	e.el.classList.add('fading');
	setTimeout(() => {
		if (e.until === null) e.el.classList.remove('fresh', 'fading');
	}, 600);
}

function step(): void {
	if (held) return;
	const now = Date.now();
	for (const e of entries) if (e.until !== null && e.until <= now) settle(e);
	if (!entries.some((e) => e.until !== null)) {
		clearInterval(tick);
		tick = undefined;
	}
}

function take(e: Entry): void {
	const i = entries.indexOf(e);
	if (i < 0) return;
	entries.splice(i, 1);
	e.el.remove();
}

export const has = (id: string) => entries.some((e) => e.id === id);

/** The input's field, once client/lib/chat.ts has made it. */
export const input = () => foot.querySelector('textarea');

/** Takes back the line with this id. */
export function remove(id: string): void {
	const e = entries.find((x) => x.id === id);
	if (e) take(e);
}

/** The lines whose ids start with `prefix` are now these (the chat's past, on every connect): the rest go. */
export function sync(prefix: string, lines: Line[]): void {
	const ids = new Set(lines.map((l) => l.id));
	for (const e of [...entries]) if (e.id?.startsWith(prefix) && !ids.has(e.id)) take(e);
	for (const l of lines) push({ ...l, live: false });
}

/** A word from the site itself: shown even muted, fading after `ms` or staying until the returned function is called. */
export function notice(title: string, text: string, opts: { ms?: number; icon?: string; action?: { label: string; run: () => void } } = {}): () => void {
	return push({ kind: 'notice', face: icon(opts.icon ?? REFRESH), who: title, text, action: opts.action, sticky: !opts.ms });
}

const near = () => log.scrollHeight - log.scrollTop - log.clientHeight < 40;

/** Opens the history and the input (T, Enter, the phone's button), or puts the focus back in it. */
export function open(): void {
	if (foot.hidden) return;
	if (!isOpen()) {
		root.classList.add('open');
		for (const e of entries) if (e.until !== null) e.el.classList.remove('fading');
		log.scrollTop = log.scrollHeight;
		onOpen();
	}
	foot.querySelector('textarea')?.focus({ preventScroll: true });
}

/** Back to the passing lines; what was typed stays in the input. */
export function close(): void {
	if (!isOpen()) return;
	root.classList.remove('open');
	if (root.contains(document.activeElement) && document.activeElement instanceof HTMLElement) document.activeElement.blur();
	for (const e of entries) if (e.until !== null && e.until <= Date.now()) settle(e, true);
	log.scrollTop = log.scrollHeight;
	onClose();
}

/** The input at the foot (client/lib/chat.ts), made again when who the reader is changes. */
export function setInput(box: HTMLElement, hooks: { open: () => void; close: () => void }): void {
	const had = foot.contains(document.activeElement);
	foot.replaceChildren(box);
	foot.hidden = false;
	({ open: onOpen, close: onClose } = hooks);
	if (had || isOpen()) foot.querySelector('textarea')?.focus({ preventScroll: true });
}

/** A line for the who-is-here list: how to chat from the keyboard. Null on a phone, which has no T. */
export function hint(): HTMLElement | null {
	if (!matchMedia('(pointer: fine)').matches) return null;
	const p = el('p', 'tl-hint');
	p.append('Press ', el('kbd', '', 'T'), ' or ', el('kbd', '', '↵'), ' to chat');
	tipOn(p, 'If Vimium or another extension takes T, use Enter, or exclude the key t for ix.dev in its options.');
	return p;
}

/**
 * T, when the key would type nothing anywhere else; Enter too, for readers whose T an extension takes first (Vimium
 * opens a tab with it, before the page hears the key), when it would not press a link or a button.
 */
function keyDown(e: KeyboardEvent): void {
	if (e.defaultPrevented || e.repeat || e.isComposing || e.metaKey || e.ctrlKey || e.altKey || e.shiftKey) return;
	if (e.key !== 't' && e.key !== 'T' && e.key !== 'Enter') return;
	const t = e.target instanceof HTMLElement ? e.target : null;
	if (t && (t.isContentEditable || t.closest('input, textarea, select, [contenteditable]'))) return;
	if (e.key === 'Enter' && t?.closest('a[href], button, summary, [role="button"], [tabindex]')) return;
	if (document.querySelector('dialog[open]') || document.body.classList.contains('chat-full') || foot.hidden) return;
	e.preventDefault();
	open();
}

document.body.append(root);
document.addEventListener('keydown', keyDown);
root.addEventListener('keydown', (e) => {
	if (e.key !== 'Escape' || !isOpen()) return;
	e.preventDefault();
	e.stopPropagation();
	close();
});
root.addEventListener('pointerenter', () => (held = true));
root.addEventListener('pointerleave', () => {
	held = false;
	const now = Date.now();
	for (const e of entries) if (e.until !== null && e.until < now + GRACE_MS) e.until = now + GRACE_MS;
});
opener.addEventListener('click', open);
// A press anywhere else closes it (the draft stays).
document.addEventListener('pointerdown', (e) => {
	if (isOpen() && !(e.target instanceof Node && root.contains(e.target))) close();
}, { capture: true });
