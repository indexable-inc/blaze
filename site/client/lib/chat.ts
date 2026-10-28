/**
 * The site's chat (worker/presence.ts): one room for everyone on the site, a panel at the right that stays open from
 * page to page (it lives on `body`, outside what the client router swaps). It rides client/lib/presence.ts's socket:
 * presence hands over every chat frame (`message`) and who the reader is (`setYou`), and gives `setup` the function
 * that sends.
 *
 * Anyone can read; signed-in readers talk (Enter sends, Shift+Enter is a new line) and take back their own messages,
 * admins anyone's. While the panel is shut the top line's Chat button counts what came in. It starts open where the
 * page makes room for it (a wide screen) until the reader closes it; that choice is kept per browser (localStorage),
 * wrapped like every storage access here because storage can refuse.
 *
 * Someone else's message in another language gets translated where the computer can do it (client/lib/translate.ts),
 * marked as translated with a way back to the original. Who is writing shows under the list, with their faces.
 */

import { CHAT, handle } from './resize';
import * as tr from './translate';

type Who = { key: string; login: string | null; avatar: string | null };
type You = Who & { signIn: boolean; admin?: boolean };
type Msg = { id: number; key: string; login: string; avatar: string | null; text: string; at: string };

const OPEN_KEY = 'blaze.chat.open';
const GROUP_MS = 5 * 60_000;
const MAX = 500;
/** How many of the history's latest messages get translated when the chat loads. */
const TRANSLATE_HISTORY = 30;
/** How long a typing indicator lasts without another word from its writer (they repeat every 2 s). */
const TYPING_MS = 4500;
const TYPING_SEND_MS = 2000;

let send: (m: object) => void = () => {};
let you: You | null = null;
let base = '/';
let face: (w: Who) => HTMLElement = () => document.createElement('span');
let speak: (key: string, login: string, text: string | null) => void = () => {};
let messages: Msg[] = [];
let unread = 0;
/** Translations by message id: being made, made, or not to be made. */
type Tr = { s: 'ask'; from: string } | { s: 'pending'; from: string } | { s: 'done'; from: string; text: string; original: boolean } | { s: 'none' };
const translations = new Map<number, Tr>();
/** Who is writing now, by key, until when. */
const typing = new Map<string, { who: Who; until: number; timer: ReturnType<typeof setTimeout> }>();

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls = '', text = ''): HTMLElementTagNameMap[K] {
	const e = document.createElement(tag);
	if (cls) e.className = cls;
	if (text) e.textContent = text;
	return e;
}

const panel = el('aside', 'chat');
panel.setAttribute('aria-label', 'Chat');
panel.hidden = true;
const head = el('div', 'chat-head');
const title = el('span', 'chat-title', 'Chat');
const online = el('span', 'chat-online');
const close = el('button', 'chat-x');
close.type = 'button';
close.setAttribute('aria-label', 'Close the chat');
close.textContent = '×';
/** Line icons for the head's buttons (16px, stroked in currentColor). */
function icon(d: string): SVGSVGElement {
	const ns = 'http://www.w3.org/2000/svg';
	const svg = document.createElementNS(ns, 'svg');
	svg.setAttribute('viewBox', '0 0 16 16');
	svg.setAttribute('aria-hidden', 'true');
	const p = document.createElementNS(ns, 'path');
	p.setAttribute('d', d);
	svg.append(p);
	return svg;
}
const BELL = 'M4 11V7a4 4 0 0 1 8 0v4l1 1.5H3L4 11ZM6.5 14h3';
const BELL_OFF = 'M4 11V7a4 4 0 0 1 6.6-3M12 7v4l1 1.5H5M6.5 14h3M2.5 2.5l11 11';
const GROW = 'M9.5 2.5h4v4M6.5 13.5h-4v-4M13.5 2.5 9 7M2.5 13.5 7 9';
const SHRINK = 'M13.5 6.5h-4v-4M2.5 9.5h4v4M9.5 6.5 14 2M6.5 9.5 2 14';
const mute = el('button', 'chat-x chat-mute');
mute.type = 'button';
const full = el('button', 'chat-x chat-full');
full.type = 'button';
head.append(title, online, mute, full, close);
const list = el('div', 'chat-list');
list.setAttribute('role', 'log');
list.setAttribute('aria-live', 'polite');
const jump = el('button', 'chat-jump', 'New messages');
jump.type = 'button';
jump.hidden = true;
const typingRow = el('div', 'chat-typing');
typingRow.setAttribute('aria-live', 'polite');
const foot = el('div', 'chat-foot');
panel.append(handle(CHAT), head, list, jump, typingRow, foot);

/** The top line's Chat button (client/lib/presence.ts places it). */
export const button = el('button', 'tl-btn tl-chat');
button.type = 'button';
button.setAttribute('aria-controls', 'chat');
panel.id = 'chat';
const badge = el('span', 'tl-badge');
badge.hidden = true;
button.append(el('span', 'tl-chat-icon'), el('span', 'tl-label', 'Chat'), badge);

const MUTE_KEY = 'blaze.chat.mute';
function muted(): boolean {
	try {
		return localStorage.getItem(MUTE_KEY) === '1';
	} catch {
		return false;
	}
}
function drawMute(): void {
	const m = muted();
	mute.replaceChildren(icon(m ? BELL_OFF : BELL));
	mute.classList.toggle('on', m);
	mute.title = m ? 'Muted: no pop-ups while the chat is shut' : 'Mute pop-ups while the chat is shut';
	mute.setAttribute('aria-label', mute.title);
	mute.setAttribute('aria-pressed', String(m));
}
function setMuted(m: boolean): void {
	try {
		if (m) localStorage.setItem(MUTE_KEY, '1');
		else localStorage.removeItem(MUTE_KEY);
	} catch {
		// Storage refused: the choice lasts this page.
	}
	drawMute();
	if (m) for (const t of toasts.children) t.remove();
}

/** Full screen: the chat over the whole content pane, the sidebar still beside it. Esc or the button again leaves. */
function setFull(on: boolean): void {
	document.body.classList.toggle('chat-full', on);
	full.replaceChildren(icon(on ? SHRINK : GROW));
	full.title = on ? 'Leave full screen' : 'Full screen';
	full.setAttribute('aria-label', full.title);
	full.setAttribute('aria-pressed', String(on));
	if (on) toBottom();
}

/** Pop-ups for messages that come while the chat is shut (not muted): the newest three, each a few seconds. */
const toasts = el('div', 'chat-toasts');
toasts.setAttribute('aria-live', 'polite');
const TOAST_MS = 5000;
function toast(m: Msg): void {
	if (muted() || !panel.hidden) return;
	const t = el('button', 'chat-toast');
	t.type = 'button';
	const words = el('span', 'chat-toast-words');
	words.append(el('b', '', m.login), el('span', '', m.text));
	t.append(face(m), words);
	t.title = 'Open the chat';
	t.addEventListener('click', () => {
		setOpen(true);
		t.remove();
	});
	toasts.prepend(t);
	while (toasts.children.length > 3) toasts.lastElementChild?.remove();
	setTimeout(() => {
		t.classList.add('out');
		setTimeout(() => t.remove(), 250);
	}, TOAST_MS);
}

/** Open on load: as the reader last left it, else open where the page makes room (styles/shell.css, 1280px). */
function stored(): boolean {
	let v: string | null = null;
	try {
		v = localStorage.getItem(OPEN_KEY);
	} catch {
		// Storage refused: the default below.
	}
	return v === null ? matchMedia('(min-width: 1280px)').matches : v === '1';
}

function setOpen(open: boolean): void {
	panel.hidden = !open;
	document.body.classList.toggle('chat-open', open);
	button.setAttribute('aria-expanded', String(open));
	try {
		localStorage.setItem(OPEN_KEY, open ? '1' : '0');
	} catch {
		// Storage refused: the panel just won't reopen on the next load.
	}
	if (!open) setFull(false);
	if (open) {
		for (const t of toasts.children) t.remove();
		unread = 0;
		drawBadge();
		toBottom();
		foot.querySelector('textarea')?.focus({ preventScroll: true });
	}
}

function drawBadge(): void {
	badge.hidden = unread === 0;
	badge.textContent = unread > 99 ? '99+' : String(unread);
}

const nearBottom = () => list.scrollHeight - list.scrollTop - list.clientHeight < 80;
function toBottom(): void {
	list.scrollTop = list.scrollHeight;
	jump.hidden = true;
}

function when(iso: string): string {
	const d = new Date(iso);
	const s = (Date.now() - d.getTime()) / 1000;
	if (!Number.isFinite(s)) return '';
	if (s < 60) return 'now';
	if (s < 3600) return `${Math.floor(s / 60)}m`;
	if (s < 86400) return d.toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit' });
	return d.toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
}

/** The text, with http(s) links made links. Built as nodes: nothing from a message is parsed as HTML. */
function body(text: string): HTMLElement {
	const p = el('p', 'chat-text');
	const re = /https?:\/\/[^\s<>"']+[^\s<>"'.,;:!?)\]]/g;
	let at = 0;
	for (const m of text.matchAll(re)) {
		p.append(text.slice(at, m.index));
		const a = el('a', '', m[0]);
		a.href = m[0];
		a.rel = 'nofollow noopener ugc';
		a.target = '_blank';
		p.append(a);
		at = m.index + m[0].length;
	}
	p.append(text.slice(at));
	return p;
}

/** A message's text, translated when it has been (unless flipped back), and the line that says so. */
function text(m: Msg): HTMLElement[] {
	const t = translations.get(m.id);
	if (!t || t.s === 'none') return [body(m.text)];
	const note = el('p', 'chat-tr');
	if (t.s === 'ask') {
		const go = el('button', '', 'Translate');
		go.type = 'button';
		go.title = 'Downloads the translator once (about 1.2 GB), then translates on this computer';
		go.addEventListener('click', () => tr.allow());
		note.append(`${tr.languageName(t.from)} · `, go, el('span', 'chat-tr-size', ' (1.2 GB, once)'));
		return [body(m.text), note];
	}
	if (t.s === 'pending') {
		note.classList.add('pending');
		note.append(pendingText(t.from));
		return [body(m.text), note];
	}
	const flip = el('button', '', t.original ? 'Show translation' : 'Show original');
	flip.type = 'button';
	flip.addEventListener('click', () => {
		translations.set(m.id, { ...t, original: !t.original });
		redraw(m);
	});
	const label = el('span', '', `Translated from ${tr.languageName(t.from)}`);
	label.title = 'Translated on this computer by HY-MT1.5 (Tencent). It can be wrong.';
	note.append(label, ' · ', flip);
	return [body(t.original ? m.text : t.text), note];
}

let trState: tr.State = { s: 'idle' };
function pendingText(from: string): Node {
	const f = document.createDocumentFragment();
	const lang = tr.languageName(from);
	if (trState.s === 'loading') {
		const pct = trState.total ? ` ${Math.floor((trState.loaded / trState.total) * 100)}% of ${(trState.total / 1e9).toFixed(1)} GB` : '';
		f.append(`Getting the translator, once:${pct} · `);
		const no = el('button', '', 'Stop');
		no.type = 'button';
		no.title = 'Stop the download and do not translate on this computer';
		no.addEventListener('click', () => tr.stop());
		f.append(no);
	} else f.append(`Translating from ${lang}…`);
	return f;
}

/** Swaps a drawn message's text for what it is now. */
function redraw(m: Msg): void {
	const row = list.querySelector<HTMLElement>(`.chat-msg[data-id="${m.id}"]`);
	if (!row) return;
	for (const old of row.querySelectorAll('.chat-text, .chat-tr')) old.remove();
	const del = row.querySelector('.chat-del');
	const stick = nearBottom();
	for (const n of text(m)) row.insertBefore(n, del);
	if (stick) toBottom();
}

/** Translates `m` when it is someone else's, in another language, and this computer can. */
async function translateOne(m: Msg): Promise<void> {
	if (m.key === you?.key || translations.has(m.id)) return;
	translations.set(m.id, { s: 'none' });
	const from = await tr.needs(m.text);
	if (!from) return;
	if (await tr.mustAsk()) {
		translations.set(m.id, { s: 'ask', from });
		redraw(m);
		return;
	}
	translations.set(m.id, { s: 'pending', from });
	redraw(m);
	const out = await tr.translate(m.text);
	translations.set(m.id, out && out !== m.text ? { s: 'done', from, text: out, original: false } : { s: 'none' });
	redraw(m);
}

function drawTyping(): void {
	const now = Date.now();
	const who = [...typing.values()].filter((t) => t.until > now).map((t) => t.who);
	typingRow.replaceChildren();
	typingRow.hidden = who.length === 0;
	if (!who.length) return;
	const faces = el('span', 'chat-typing-faces');
	for (const w of who.slice(0, 3)) faces.append(face(w));
	const names = who.map((w) => w.login ?? 'someone');
	const line = names.length === 1 ? `${names[0]} is typing` : names.length === 2 ? `${names[0]} and ${names[1]} are typing` : `${names.length} people are typing`;
	const dots = el('span', 'dots');
	dots.append(el('i'), el('i'), el('i'));
	typingRow.append(faces, el('span', '', line), dots);
}

function typed(w: Who): void {
	if (w.key === you?.key) return;
	const old = typing.get(w.key);
	if (old) clearTimeout(old.timer);
	const timer = setTimeout(() => {
		typing.delete(w.key);
		drawTyping();
	}, TYPING_MS);
	typing.set(w.key, { who: w, until: Date.now() + TYPING_MS, timer });
	drawTyping();
	if (w.login) speak(w.key, w.login, null);
}

function stoppedTyping(key: string): void {
	const t = typing.get(key);
	if (!t) return;
	clearTimeout(t.timer);
	typing.delete(key);
	drawTyping();
}

function item(m: Msg, prev: Msg | undefined): HTMLElement {
	const grouped = prev && prev.key === m.key && Date.parse(m.at) - Date.parse(prev.at) < GROUP_MS;
	const row = el('div', grouped ? 'chat-msg grouped' : 'chat-msg');
	row.dataset.id = String(m.id);
	if (!grouped) {
		const who = el('div', 'chat-who');
		const name = el('a', 'chat-name', m.login);
		name.href = `https://github.com/${encodeURIComponent(m.login)}`;
		name.rel = 'nofollow noopener';
		const t = el('time', 'chat-when', when(m.at));
		t.dateTime = m.at;
		t.title = new Date(m.at).toLocaleString();
		who.append(face(m), name, t);
		row.append(who);
	}
	row.append(...text(m));
	if (you && (you.key === m.key || you.admin)) {
		const del = el('button', 'chat-del', '×');
		del.type = 'button';
		del.title = 'Delete';
		del.setAttribute('aria-label', 'Delete message');
		del.addEventListener('click', () => send({ t: 'unsay', id: m.id }));
		row.append(del);
	}
	return row;
}

function drawList(): void {
	const stick = nearBottom();
	list.replaceChildren();
	if (messages.length === 0) list.append(el('p', 'chat-empty', 'No messages yet. Say hi.'));
	messages.forEach((m, i) => list.append(item(m, messages[i - 1])));
	if (stick || panel.hidden) toBottom();
}

function drawFoot(): void {
	foot.replaceChildren();
	if (you?.login) {
		const form = el('form', 'chat-form');
		const ta = el('textarea');
		ta.rows = 1;
		ta.maxLength = MAX;
		ta.placeholder = 'Message everyone here…';
		ta.setAttribute('aria-label', 'Message');
		const err = el('p', 'chat-err');
		err.hidden = true;
		err.id = 'chat-err';
		const go = el('button', 'chat-send');
		go.type = 'submit';
		go.setAttribute('aria-label', 'Send');
		go.title = 'Send (Enter)';
		go.disabled = true;
		go.append(arrow());
		const grow = () => {
			ta.style.height = 'auto';
			ta.style.height = `${Math.min(ta.scrollHeight, 140)}px`;
			go.disabled = !ta.value.trim();
		};
		let sentTyping = 0;
		ta.addEventListener('input', () => {
			grow();
			if (ta.value.trim() && Date.now() - sentTyping > TYPING_SEND_MS) {
				sentTyping = Date.now();
				send({ t: 'typing' });
			}
		});
		ta.addEventListener('keydown', (e) => {
			if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
				e.preventDefault();
				form.requestSubmit();
			}
		});
		form.addEventListener('submit', (e) => {
			e.preventDefault();
			const text = ta.value.trim();
			if (!text) return;
			send({ t: 'say', text });
			tr.allow();
			sentTyping = 0;
			ta.value = '';
			grow();
			err.hidden = true;
		});
		form.append(ta, go);
		foot.append(err, form);
	} else if (you?.signIn) {
		const a = el('a', 'chat-signin', 'Sign in with GitHub to chat');
		a.href = `${base}auth/github?next=${encodeURIComponent(location.pathname.slice(base.length - 1) || '/')}`;
		foot.append(a);
	} else {
		foot.append(el('p', 'chat-empty', 'Chat opens once sign-in is set up.'));
	}
}

function arrow(): SVGSVGElement {
	const ns = 'http://www.w3.org/2000/svg';
	const svg = document.createElementNS(ns, 'svg');
	svg.setAttribute('viewBox', '0 0 16 16');
	svg.setAttribute('aria-hidden', 'true');
	const p = document.createElementNS(ns, 'path');
	p.setAttribute('d', 'M8 13V3M3.5 7.5 8 3l4.5 4.5');
	svg.append(p);
	return svg;
}

/** Wires the chat to the presence socket and the page. Called once. */
export function setup(opts: {
	send: (m: object) => void;
	base: string;
	face: (w: Who) => HTMLElement;
	speak: (key: string, login: string, text: string | null) => void;
}): void {
	send = opts.send;
	speak = opts.speak;
	base = opts.base;
	face = opts.face;
	document.body.append(panel, toasts);
	drawMute();
	setFull(false);
	mute.addEventListener('click', () => setMuted(!muted()));
	full.addEventListener('click', () => setFull(!document.body.classList.contains('chat-full')));
	typingRow.hidden = true;
	tr.setup(base);
	// Agreed (a Translate button, or the reader's first message): translate every message that was waiting on it.
	tr.onAllow(() => {
		for (const m of messages) {
			if (translations.get(m.id)?.s !== 'ask') continue;
			translations.delete(m.id);
			void translateOne(m);
		}
	});
	tr.watch((s) => {
		trState = s;
		if (s.s === 'failed' || (s.s === 'idle' && tr.off())) for (const [id, t] of translations) if (t.s === 'pending') translations.set(id, { s: 'none' });
		for (const m of messages) if (translations.get(m.id)?.s !== 'done') redraw(m);
	});
	button.addEventListener('click', () => setOpen(!document.body.classList.contains('chat-open')));
	close.addEventListener('click', () => setOpen(false));
	jump.addEventListener('click', toBottom);
	list.addEventListener('scroll', () => {
		if (nearBottom()) jump.hidden = true;
	});
	document.addEventListener('keydown', (e) => {
		if (e.key !== 'Escape' || panel.hidden) return;
		if (document.body.classList.contains('chat-full')) setFull(false);
		else if (panel.contains(document.activeElement)) setOpen(false);
	});
	drawList();
	drawFoot();
	if (stored()) setOpen(true);
}

export function setYou(w: You): void {
	you = w;
	drawFoot();
	drawList();
}

export function setOnline(n: number): void {
	online.textContent = `${n} online`;
}

/** A chat frame from the socket; false when it is not one. */
export function message(m: { t?: unknown }): boolean {
	if (m.t === 'chat') {
		const ms = (m as { messages?: unknown }).messages;
		messages = Array.isArray(ms) ? (ms as Msg[]) : [];
		drawList();
		for (const x of messages.slice(-TRANSLATE_HISTORY).reverse()) void translateOne(x);
	} else if (m.t === 'msg') {
		const msg = (m as { m: Msg }).m;
		const stick = nearBottom();
		stoppedTyping(msg.key);
		if (msg.key !== you?.key) speak(msg.key, msg.login, msg.text);
		messages.push(msg);
		if (messages.length > 200) messages.shift();
		list.querySelector('.chat-empty')?.remove();
		list.append(item(msg, messages[messages.length - 2]));
		void translateOne(msg);
		if (panel.hidden) {
			if (msg.key !== you?.key) {
				unread++;
				toast(msg);
			}
			drawBadge();
			toBottom();
		} else if (stick || msg.key === you?.key) toBottom();
		else jump.hidden = false;
	} else if (m.t === 'typing') {
		const w = m as { key?: unknown; login?: unknown; avatar?: unknown };
		if (typeof w.key === 'string') typed({ key: w.key, login: typeof w.login === 'string' ? w.login : null, avatar: typeof w.avatar === 'string' ? w.avatar : null });
	} else if (m.t === 'unsay') {
		const id = (m as { id: number }).id;
		messages = messages.filter((x) => x.id !== id);
		drawList();
	} else if (m.t === 'error') {
		const err = foot.querySelector<HTMLElement>('.chat-err');
		if (err) {
			err.textContent = String((m as { text?: unknown }).text ?? 'Not sent.');
			err.hidden = false;
		}
	} else return false;
	return true;
}
