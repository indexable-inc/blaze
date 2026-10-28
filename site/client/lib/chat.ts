/**
 * The site's chat (worker/presence.ts): one room for everyone on the site, a panel at the right that stays open from
 * page to page (it lives on `body`, outside what the client router swaps). It rides client/lib/presence.ts's socket:
 * presence hands over every chat frame (`message`), who the reader is (`setYou`) and who is online (`setPeople`), and
 * gives `setup` the function that sends.
 *
 * Anyone can read; signed-in readers talk (Enter sends, Shift+Enter is a new line) and take back their own messages,
 * admins anyone's. While the panel is shut the top line's Chat button counts what came in. It starts open where the
 * page makes room for it (a wide screen) until the reader closes it; that choice is kept per browser (localStorage),
 * wrapped like every storage access here because storage can refuse.
 *
 * Someone else's message in another language gets translated where the computer can do it (client/lib/translate.ts),
 * marked as translated with a way back to the original. Who is writing shows under the list, with their faces.
 *
 * Threads, like Slack's: any top-level message can be replied to in its thread, one level deep. The list shows the
 * top-level messages, each with a line for its replies (faces, count, when the last came); the thread opens over the
 * list, or beside it in full screen. Replies stay quiet (no bubbles, toasts or badge) unless they mention the reader.
 *
 * `@login` mentions: both composers complete GitHub logins (people online, recent authors), and a message that
 * mentions the reader is marked, and pops up and counts while the chat is shut, even when muted.
 *
 * The pop-up stack also carries the site's own word (`notice`, for client/lib/update.ts: a new version is in or ready),
 * which shows whether the chat is open or muted. Before an update reloads the page, `save` hands over what the chat
 * has that the reader would miss (drafts, the open thread, full screen, the list's scroll) and `restore` puts it back.
 */

import { tipOn } from './tip';
import { CHAT, handle } from './resize';
import * as tr from './translate';

type Who = { key: string; login: string | null; avatar: string | null };
type You = Who & { signIn: boolean; admin?: boolean };
type Msg = {
	id: number;
	key: string;
	login: string;
	avatar: string | null;
	text: string;
	at: string;
	/** Set on a reply: the top-level message whose thread it is in. */
	parent?: number;
	/** A top-level message's thread: how many replies, when the last came, up to three of the latest repliers. */
	replies?: number;
	last?: string | null;
	repliers?: Who[];
};
type Summary = Required<Pick<Msg, 'replies' | 'last' | 'repliers'>>;

const OPEN_KEY = 'blaze.chat.open';
const GROUP_MS = 5 * 60_000;
const MAX = 500;
/** How many of the history's latest messages get translated when the chat loads. */
const TRANSLATE_HISTORY = 30;
/** How long a typing indicator lasts without another word from its writer (they repeat every 2 s). */
const TYPING_MS = 4500;
const TYPING_SEND_MS = 2000;
/** A GitHub login after `@`: alphanumerics and single hyphens, at most 39, not inside a word (an email address). */
const MENTION = /(?<!\w)@([A-Za-z0-9](?:[A-Za-z0-9]|-(?=[A-Za-z0-9])){0,38})(?!\w)/;
const LINK = /https?:\/\/[^\s<>"']+[^\s<>"'.,;:!?)\]]/;
/** Links and mentions in one pass, so an `@` inside a link stays part of the link. Group 1 is a mention's login. */
const TOKENS = new RegExp(`${LINK.source}|${MENTION.source}`, 'g');
/** How many logins the mention pop-up offers. */
const PICKS = 6;

let send: (m: object) => void = () => {};
let you: You | null = null;
let base = '/';
let face: (w: Who) => HTMLElement = () => document.createElement('span');
let speak: (key: string, login: string, text: string | null) => void = () => {};
let messages: Msg[] = [];
let unread = 0;
/** Signed-in people online now (client/lib/presence.ts), for completing mentions. */
let people: Who[] = [];
/** The open thread: its message (null until known) and its replies, once the server sent them. */
let thread: { id: number; root: Msg | null; replies: Msg[]; loaded: boolean } | null = null;
/** The error line of the composer that sent last, where a refusal shows. */
let errAt: HTMLElement | null = null;
/** What the chat keeps across a reload for a site update (client/lib/update.ts). */
export type Kept = { main?: string; reply?: string; thread?: number; full?: boolean; list?: number; bottom?: boolean };
/** Waiting to be put back, piece by piece as the socket brings the reader and the messages. */
let restoring: Kept | null = null;
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
const BUBBLE = 'M2.5 3.5h11v7.5H8l-3 2.5V11H2.5Z';
const BACK = 'M10 3 5 8l5 5';
const CROSS = 'M4 4l8 8M12 4l-8 8';
const REFRESH = 'M13 8a5 5 0 1 1-1.6-3.7M13.5 2v3h-3';
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

/** The open thread: over the list and composer in the panel, a column at its right in full screen (styles/shell.css). */
const threadPane = el('section', 'chat-thread');
threadPane.setAttribute('aria-label', 'Thread');
threadPane.hidden = true;
const threadHead = el('div', 'chat-head');
const back = el('button', 'chat-x chat-back');
back.type = 'button';
threadHead.append(back, el('span', 'chat-title', 'Thread'));
const threadList = el('div', 'chat-list');
threadList.setAttribute('role', 'log');
threadList.setAttribute('aria-live', 'polite');
const threadFoot = el('div', 'chat-foot');
threadPane.append(threadHead, threadList, threadFoot);
panel.append(handle(CHAT), head, list, jump, typingRow, foot, threadPane);

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
	tipOn(mute, m ? 'Muted: no pop-ups while the chat is shut, except mentions of you' : 'Mute pop-ups while the chat is shut');
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
	if (m) for (const t of [...toasts.children]) if (!t.classList.contains('mention') && !t.classList.contains('chat-notice')) t.remove();
}

/** Full screen: the chat over the whole content pane, the sidebar still beside it. Esc or the button again leaves. */
function setFull(on: boolean): void {
	document.body.classList.toggle('chat-full', on);
	full.replaceChildren(icon(on ? SHRINK : GROW));
	tipOn(full, on ? 'Leave full screen' : 'Full screen');
	full.setAttribute('aria-pressed', String(on));
	// The thread is a column beside the list there, so it closes rather than goes back.
	back.replaceChildren(icon(on ? CROSS : BACK));
	tipOn(back, on ? 'Close the thread' : 'Back to the chat');
	if (on) toBottom();
}

/** Pop-ups for messages that come while the chat is shut (not muted, or mentioning the reader): the newest three. */
const toasts = el('div', 'chat-toasts');
toasts.setAttribute('aria-live', 'polite');
const TOAST_MS = 5000;
function toast(m: Msg, mention: boolean): void {
	if ((muted() && !mention) || !panel.hidden) return;
	const t = el('button', mention ? 'chat-toast mention' : 'chat-toast');
	t.type = 'button';
	const words = el('span', 'chat-toast-words');
	words.append(el('b', '', m.parent === undefined ? m.login : `${m.login} in a thread`), el('span', '', m.text));
	t.append(face(m), words);
	tipOn(t, 'Open the chat');
	t.addEventListener('click', () => {
		setOpen(true);
		if (m.parent !== undefined) openThread(m.parent);
		t.remove();
	});
	toasts.prepend(t);
	// The oldest messages go; the site's notices stay until they are done.
	const msgs = [...toasts.children].filter((x) => !x.classList.contains('chat-notice'));
	for (const x of msgs.slice(3)) x.remove();
	fade(t, TOAST_MS);
}

/** A pop-up that someone arrived on the site (client/lib/presence.ts): not when muted; a click goes where they are. */
export function arrived(f: HTMLElement, who: string, where: string, href: string | null): void {
	if (muted()) return;
	const t = el(href ? 'a' : 'div', 'chat-toast chat-arrived');
	if (t instanceof HTMLAnchorElement && href) t.href = href;
	const words = el('span', 'chat-toast-words');
	words.append(el('b', '', `${who} joined`), el('span', '', where));
	t.append(f, words);
	toasts.prepend(t);
	const msgs = [...toasts.children].filter((x) => !x.classList.contains('chat-notice'));
	for (const x of msgs.slice(3)) x.remove();
	fade(t, TOAST_MS);
}

function fade(t: Element, ms: number): void {
	setTimeout(() => {
		t.classList.add('out');
		setTimeout(() => t.remove(), 250);
	}, ms);
}

/**
 * A word from the site itself (not a chat message) in the pop-up stack: shown whether the chat is open or muted, with
 * an optional button. It fades after `ms`, or stays until the returned function is called.
 */
export function notice(title: string, text: string, opts: { ms?: number; action?: { label: string; run: () => void } } = {}): () => void {
	const t = el('div', 'chat-toast chat-notice');
	t.setAttribute('role', 'status');
	const words = el('span', 'chat-toast-words');
	words.append(el('b', '', title), el('span', '', text));
	t.append(icon(REFRESH), words);
	if (opts.action) {
		const b = el('button', 'chat-notice-act', opts.action.label);
		b.type = 'button';
		b.addEventListener('click', opts.action.run);
		t.append(b);
	}
	toasts.prepend(t);
	if (opts.ms) fade(t, opts.ms);
	return () => t.remove();
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
		for (const t of [...toasts.children]) if (!t.classList.contains('chat-notice')) t.remove();
		unread = 0;
		drawBadge();
		toBottom();
		(thread ? threadFoot : foot).querySelector('textarea')?.focus({ preventScroll: true });
	}
}

function drawBadge(): void {
	badge.hidden = unread === 0;
	badge.textContent = unread > 99 ? '99+' : String(unread);
}

const near = (box: HTMLElement) => box.scrollHeight - box.scrollTop - box.clientHeight < 80;
const nearBottom = () => near(list);
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

/** Whether `m` is someone else's and names the reader with an `@` (links skipped: an `@` in a URL is no mention). */
function mentionsYou(m: Msg): boolean {
	const me = you?.login?.toLowerCase();
	if (!me || m.key === you?.key) return false;
	for (const t of m.text.matchAll(TOKENS)) if (t[1]?.toLowerCase() === me) return true;
	return false;
}

/** The text, with http(s) links made links and `@login`s marked. Built as nodes: nothing from a message is parsed as HTML. */
function body(text: string): HTMLElement {
	const p = el('p', 'chat-text');
	const me = you?.login?.toLowerCase();
	let at = 0;
	for (const m of text.matchAll(TOKENS)) {
		p.append(text.slice(at, m.index));
		const login = m[1];
		const a = el('a', login ? 'chat-at' : '', m[0]);
		if (login) {
			a.href = `https://github.com/${encodeURIComponent(login)}`;
			if (login.toLowerCase() === me) a.classList.add('me');
		} else a.href = m[0];
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
		tipOn(go, 'Downloads the translator once (about 1.2 GB), then translates on this computer');
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
	tipOn(label, 'Translated on this computer by HY-MT1.5 (Tencent). It can be wrong.');
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
		tipOn(no, 'Stop the download and do not translate on this computer');
		no.addEventListener('click', () => tr.stop());
		f.append(no);
	} else f.append(`Translating from ${lang}…`);
	return f;
}

/** Swaps a drawn message's text for what it is now, wherever it shows (the list, the thread, or both). */
function redraw(m: Msg): void {
	for (const row of panel.querySelectorAll<HTMLElement>(`.chat-msg[data-id="${m.id}"]`)) {
		const box = row.closest<HTMLElement>('.chat-list');
		const stick = box ? near(box) : false;
		for (const old of row.querySelectorAll('.chat-text, .chat-tr')) old.remove();
		const after = row.querySelector('.chat-replies, .chat-acts');
		for (const n of text(m)) row.insertBefore(n, after);
		if (box && stick) box.scrollTop = box.scrollHeight;
	}
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

/** The line under a message with replies: the latest repliers' faces, how many, when the last came. Opens the thread. */
function summary(m: Msg): HTMLElement {
	const b = el('button', 'chat-replies');
	b.type = 'button';
	tipOn(b, 'Open the thread');
	const faces = el('span', 'chat-replies-faces');
	for (const w of (m.repliers ?? []).slice(0, 3)) faces.append(face(w));
	const n = m.replies ?? 0;
	b.append(faces, el('b', '', `${n} ${n === 1 ? 'reply' : 'replies'}`));
	if (m.last) {
		const w = when(m.last);
		b.append(el('span', 'chat-replies-when', `last reply ${w === 'now' ? 'just now' : w}`));
	}
	b.addEventListener('click', () => openThread(m.id));
	return b;
}

/** A message's row. `where`: in the list (with its thread's line and a reply button), atop a thread, or a reply. */
function item(m: Msg, prev: Msg | undefined, where: 'list' | 'root' | 'reply' = 'list'): HTMLElement {
	const grouped = prev && prev.key === m.key && Date.parse(m.at) - Date.parse(prev.at) < GROUP_MS;
	const row = el('div', grouped ? 'chat-msg grouped' : 'chat-msg');
	row.dataset.id = String(m.id);
	if (mentionsYou(m)) row.classList.add('mention');
	if (!grouped) {
		const who = el('div', 'chat-who');
		const name = el('a', 'chat-name', m.login);
		name.href = `https://github.com/${encodeURIComponent(m.login)}`;
		name.rel = 'nofollow noopener';
		const t = el('time', 'chat-when', when(m.at));
		t.dateTime = m.at;
		tipOn(t, new Date(m.at).toLocaleString());
		who.append(face(m), name, t);
		row.append(who);
	}
	row.append(...text(m));
	if (where === 'list' && m.replies) row.append(summary(m));
	const acts = el('div', 'chat-acts');
	if (where === 'list') {
		const r = el('button', 'chat-act');
		r.type = 'button';
		tipOn(r, 'Reply in thread');
		r.append(icon(BUBBLE));
		r.addEventListener('click', () => openThread(m.id));
		acts.append(r);
	}
	if (you && (you.key === m.key || you.admin)) {
		const del = el('button', 'chat-act chat-del', '×');
		del.type = 'button';
		tipOn(del, where === 'reply' || !m.replies ? 'Delete' : 'Delete, with its thread');
		del.setAttribute('aria-label', 'Delete message');
		del.addEventListener('click', () => send({ t: 'unsay', id: m.id }));
		acts.append(del);
	}
	if (acts.childElementCount) row.append(acts);
	return row;
}

function drawList(): void {
	const stick = nearBottom();
	list.replaceChildren();
	if (messages.length === 0) list.append(el('p', 'chat-empty', 'No messages yet. Say hi.'));
	messages.forEach((m, i) => list.append(item(m, messages[i - 1])));
	if (stick || panel.hidden) toBottom();
}

/** Draws one message of the list again (its thread's line changed). */
function drawRow(m: Msg): void {
	const row = list.querySelector(`.chat-msg[data-id="${m.id}"]`);
	const i = messages.indexOf(m);
	if (!row || i < 0) return;
	const stick = nearBottom();
	row.replaceWith(item(m, messages[i - 1]));
	if (stick) toBottom();
}

/** A reply came: its message's line counts it, with its writer first among the faces. */
function bump(r: Msg): void {
	const p = messages.find((x) => x.id === r.parent) ?? (thread && thread.id === r.parent ? thread.root : null);
	if (!p) return;
	p.replies = (p.replies ?? 0) + 1;
	p.last = r.at;
	p.repliers = [{ key: r.key, login: r.login, avatar: r.avatar }, ...(p.repliers ?? []).filter((w) => w.key !== r.key)].slice(0, 3);
	drawRow(p);
}

/** Sets a message's thread summary to what the server says (after a reply was taken back, or the thread loaded). */
function resum(id: number, s: Summary): void {
	const p = messages.find((x) => x.id === id);
	if (p) {
		Object.assign(p, s);
		drawRow(p);
	}
	if (thread?.id === id && thread.root && thread.root !== p) Object.assign(thread.root, s);
}

function openThread(id: number): void {
	if (thread?.id !== id) thread = { id, root: messages.find((x) => x.id === id) ?? null, replies: [], loaded: false };
	send({ t: 'thread', id });
	panel.classList.add('threaded');
	threadPane.hidden = false;
	drawThread();
	drawThreadFoot();
	threadFoot.querySelector('textarea')?.focus({ preventScroll: true });
}

function closeThread(): void {
	if (!thread) return;
	thread = null;
	panel.classList.remove('threaded');
	threadPane.hidden = true;
	threadList.replaceChildren();
	threadFoot.replaceChildren();
	if (!panel.hidden) foot.querySelector('textarea')?.focus({ preventScroll: true });
}

/** The thread's message, a line counting its replies, the replies; scrolled to the end unless `stick` is false. */
function drawThread(stick = true): void {
	const t = thread;
	const top = threadList.scrollTop;
	threadList.replaceChildren();
	if (!t) return;
	if (t.root) threadList.append(item(t.root, undefined, 'root'));
	const n = Math.max(t.replies.length, t.root?.replies ?? 0);
	if (!t.loaded) threadList.append(el('p', 'chat-empty', 'Loading replies…'));
	else threadList.append(el('div', 'chat-thread-n', n === 0 ? 'No replies yet' : `${n} ${n === 1 ? 'reply' : 'replies'}`));
	t.replies.forEach((m, i) => threadList.append(item(m, t.replies[i - 1], 'reply')));
	threadList.scrollTop = stick ? threadList.scrollHeight : top;
}

function drawThreadFoot(): void {
	if (!thread) return;
	// A reply being written before an update reloaded the page, in the thread it was for.
	const draft = restoring?.thread === thread.id ? restoring.reply : undefined;
	if (you?.login && restoring) delete restoring.reply;
	threadFoot.replaceChildren(you?.login ? composer('Reply…', thread.id, draft) : signIn());
}

/** Logins to offer after `@`: in the open thread, then the list, latest first, then the rest online; not the reader. */
function candidates(q: string): Who[] {
	const me = you?.login?.toLowerCase();
	const seen = new Set<string>();
	const all: Who[] = [];
	const add = (w: Who) => {
		const l = w.login?.toLowerCase();
		if (!l || l === me || seen.has(l)) return;
		seen.add(l);
		all.push(w);
	};
	for (let i = (thread?.replies.length ?? 0) - 1; i >= 0; i--) add(thread!.replies[i]);
	if (thread?.root) add(thread.root);
	for (let i = messages.length - 1; i >= 0; i--) add(messages[i]);
	for (const w of people) add(w);
	const lq = q.toLowerCase();
	const starts = all.filter((w) => w.login!.toLowerCase().startsWith(lq));
	const inside = all.filter((w) => !w.login!.toLowerCase().startsWith(lq) && w.login!.toLowerCase().includes(lq));
	return [...starts, ...inside].slice(0, PICKS);
}

/** A composer: Enter sends (a reply when `parent` is set), Shift+Enter is a new line, `@` offers logins to mention. */
function composer(placeholder: string, parent: number | null, draft = ''): HTMLElement {
	const box = el('div', 'chat-compose');
	const form = el('form', 'chat-form');
	const ta = el('textarea');
	ta.rows = 1;
	ta.maxLength = MAX;
	ta.placeholder = placeholder;
	ta.setAttribute('aria-label', parent === null ? 'Message' : 'Reply');
	const err = el('p', 'chat-err');
	err.hidden = true;
	const go = el('button', 'chat-send');
	go.type = 'submit';
	go.setAttribute('aria-label', 'Send');
	tipOn(go, 'Send (Enter)');
	go.disabled = true;
	go.append(arrow());
	const grow = () => {
		ta.style.height = 'auto';
		ta.style.height = `${Math.min(ta.scrollHeight, 140)}px`;
		go.disabled = !ta.value.trim();
	};

	// The mention pop-up: the `@word` the caret ends, if any, and the logins that fit it.
	const pop = el('ul', 'chat-pop');
	pop.setAttribute('role', 'listbox');
	pop.hidden = true;
	let picks: Who[] = [];
	let pick = 0;
	const query = (): { start: number; q: string } | null => {
		if (ta.selectionStart !== ta.selectionEnd) return null;
		const upto = ta.value.slice(0, ta.selectionStart);
		const m = /(?:^|[^\w])@([A-Za-z0-9-]{0,39})$/.exec(upto);
		return m ? { start: upto.length - m[1].length - 1, q: m[1] } : null;
	};
	const shut = () => {
		picks = [];
		pop.hidden = true;
	};
	const insert = (login: string) => {
		const at = query();
		if (!at) return shut();
		ta.setRangeText(`@${login} `, at.start, ta.selectionStart, 'end');
		shut();
		grow();
	};
	const drawPop = () => {
		const at = query();
		picks = at ? candidates(at.q) : [];
		pop.hidden = picks.length === 0;
		pick = Math.min(pick, Math.max(picks.length - 1, 0));
		pop.replaceChildren(
			...picks.map((w, i) => {
				const li = el('li', i === pick ? 'on' : '');
				li.setAttribute('role', 'option');
				li.setAttribute('aria-selected', String(i === pick));
				li.append(face(w), el('span', '', w.login!));
				// Mousedown, not click: the textarea keeps the focus (and its caret) through the pick.
				li.addEventListener('mousedown', (e) => {
					e.preventDefault();
					insert(w.login!);
				});
				li.addEventListener('mousemove', () => {
					if (pick === i) return;
					pick = i;
					drawPop();
				});
				return li;
			}),
		);
	};

	let sentTyping = 0;
	ta.addEventListener('input', () => {
		grow();
		pick = 0;
		drawPop();
		// Typing shows in the main room only: a reply's writer would show there as writing to everyone.
		if (parent === null && ta.value.trim() && Date.now() - sentTyping > TYPING_SEND_MS) {
			sentTyping = Date.now();
			send({ t: 'typing' });
		}
	});
	ta.addEventListener('click', drawPop);
	ta.addEventListener('keyup', (e) => {
		if (['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(e.key)) drawPop();
	});
	ta.addEventListener('blur', shut);
	ta.addEventListener('keydown', (e) => {
		if (picks.length && !e.isComposing) {
			if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
				e.preventDefault();
				pick = (pick + (e.key === 'ArrowDown' ? 1 : -1) + picks.length) % picks.length;
				return drawPop();
			}
			if ((e.key === 'Enter' && !e.shiftKey) || e.key === 'Tab') {
				e.preventDefault();
				return insert(picks[pick].login!);
			}
			if (e.key === 'Escape') {
				// The pop-up only: the chat's own Esc (close the thread, the chat) waits for the next press.
				e.preventDefault();
				e.stopPropagation();
				return shut();
			}
		}
		if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
			e.preventDefault();
			form.requestSubmit();
		}
	});
	form.addEventListener('submit', (e) => {
		e.preventDefault();
		const text = ta.value.trim();
		if (!text) return;
		send(parent === null ? { t: 'say', text } : { t: 'say', text, parent });
		tr.allow();
		errAt = err;
		sentTyping = 0;
		ta.value = '';
		shut();
		grow();
		err.hidden = true;
	});
	form.append(ta, go);
	box.append(err, pop, form);
	if (draft) {
		ta.value = draft;
		grow();
	}
	return box;
}

function signIn(): HTMLElement {
	if (!you?.signIn) return el('p', 'chat-empty', 'Chat opens once sign-in is set up.');
	const a = el('a', 'chat-signin', 'Sign in with GitHub to chat');
	a.href = `${base}auth/github?next=${encodeURIComponent(location.pathname.slice(base.length - 1) || '/')}`;
	return a;
}

function drawFoot(): void {
	const draft = you?.login ? restoring?.main : undefined;
	if (draft !== undefined) delete restoring!.main;
	foot.replaceChildren(you?.login ? composer('Message everyone here…', null, draft) : signIn());
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
	// A touch screen has no hover: a tap on a message (not on its links or buttons) shows its actions, one at a time.
	if (matchMedia('(hover: none)').matches) {
		panel.addEventListener('click', (e) => {
			const t = e.target instanceof Element ? e.target : null;
			if (!t || t.closest('a, button, textarea')) return;
			const row = t.closest('.chat-msg');
			for (const open of panel.querySelectorAll('.chat-msg.acts-open')) if (open !== row) open.classList.remove('acts-open');
			row?.classList.toggle('acts-open');
		});
	}
	send = opts.send;
	speak = opts.speak;
	base = opts.base;
	face = opts.face;
	document.body.append(panel, toasts);
	drawMute();
	setFull(false);
	mute.addEventListener('click', () => setMuted(!muted()));
	full.addEventListener('click', () => setFull(!document.body.classList.contains('chat-full')));
	back.addEventListener('click', closeThread);
	typingRow.hidden = true;
	tr.setup(base);
	// Agreed (a Translate button, or the reader's first message): translate every message that was waiting on it.
	tr.onAllow(() => {
		for (const m of [...messages, ...(thread?.replies ?? [])]) {
			if (translations.get(m.id)?.s !== 'ask') continue;
			translations.delete(m.id);
			void translateOne(m);
		}
	});
	tr.watch((s) => {
		trState = s;
		if (s.s === 'failed' || (s.s === 'idle' && tr.off())) for (const [id, t] of translations) if (t.s === 'pending') translations.set(id, { s: 'none' });
		for (const m of [...messages, ...(thread?.replies ?? [])]) if (translations.get(m.id)?.s !== 'done') redraw(m);
	});
	button.addEventListener('click', () => setOpen(!document.body.classList.contains('chat-open')));
	close.addEventListener('click', () => setOpen(false));
	jump.addEventListener('click', toBottom);
	list.addEventListener('scroll', () => {
		if (nearBottom()) jump.hidden = true;
	});
	// Esc steps back one thing at a time: the thread, then full screen, then the chat.
	document.addEventListener('keydown', (e) => {
		if (e.key !== 'Escape' || panel.hidden) return;
		const isFull = document.body.classList.contains('chat-full');
		if (thread && (isFull || panel.contains(document.activeElement))) closeThread();
		else if (isFull) setFull(false);
		else if (panel.contains(document.activeElement)) setOpen(false);
	});
	drawList();
	drawFoot();
	if (stored()) setOpen(true);
	if (restoring?.full && !panel.hidden) setFull(true);
}

export function setYou(w: You): void {
	you = w;
	drawFoot();
	drawList();
	drawThread(false);
	drawThreadFoot();
	// The thread the reader had open before an update reloaded the page (its composer takes the reply draft).
	if (restoring?.thread !== undefined && !thread) openThread(restoring.thread);
}

export function setOnline(n: number): void {
	online.textContent = `${n} online`;
}

/** Everyone online (client/lib/presence.ts); the signed-in ones are offered when completing a mention. */
export function setPeople(list: Who[]): void {
	people = list.filter((w) => w.login);
}

/** A reply came: its thread's line, the open thread, and, when it names the reader, the one signal a reply gets. */
function reply(r: Msg): void {
	bump(r);
	const t = thread;
	if (t && t.id === r.parent && !t.replies.some((x) => x.id === r.id)) {
		const stick = near(threadList) || r.key === you?.key;
		t.replies.push(r);
		drawThread(stick);
		void translateOne(r);
	}
	if (!mentionsYou(r)) return;
	speak(r.key, r.login, r.text);
	if (panel.hidden) {
		unread++;
		toast(r, true);
		drawBadge();
	}
}

/** A chat frame from the socket; false when it is not one. */
export function message(m: { t?: unknown }): boolean {
	if (m.t === 'chat') {
		const ms = (m as { messages?: unknown }).messages;
		messages = Array.isArray(ms) ? (ms as Msg[]) : [];
		drawList();
		// The last of what the socket sends on connect: the list goes back where the reader left it, then nothing is left to restore.
		if (restoring) {
			if (restoring.list !== undefined && !restoring.bottom) list.scrollTop = restoring.list;
			restoring = null;
		}
		for (const x of messages.slice(-TRANSLATE_HISTORY).reverse()) void translateOne(x);
		// A reconnect: the open thread may have missed replies.
		if (thread) {
			thread.root = messages.find((x) => x.id === thread!.id) ?? thread.root;
			send({ t: 'thread', id: thread.id });
		}
	} else if (m.t === 'msg') {
		const msg = (m as { m: Msg }).m;
		if (typeof msg.parent === 'number') {
			reply(msg);
			return true;
		}
		const stick = nearBottom();
		const mine = msg.key === you?.key;
		stoppedTyping(msg.key);
		if (!mine) speak(msg.key, msg.login, msg.text);
		messages.push(msg);
		if (messages.length > 200) messages.shift();
		list.querySelector('.chat-empty')?.remove();
		list.append(item(msg, messages[messages.length - 2]));
		void translateOne(msg);
		if (panel.hidden) {
			if (!mine) {
				unread++;
				toast(msg, mentionsYou(msg));
			}
			drawBadge();
			toBottom();
		} else if (stick || mine) toBottom();
		else jump.hidden = false;
	} else if (m.t === 'thread') {
		const f = m as { id?: unknown; root?: Msg | null; messages?: unknown };
		const t = thread;
		if (!t || f.id !== t.id) return true;
		// The message is gone (taken back, or pruned): nothing to show.
		if (!f.root) {
			closeThread();
			return true;
		}
		resum(t.id, { replies: f.root.replies ?? 0, last: f.root.last ?? null, repliers: f.root.repliers ?? [] });
		t.root = messages.find((x) => x.id === t.id) ?? t.root ?? f.root;
		t.replies = Array.isArray(f.messages) ? (f.messages as Msg[]) : [];
		t.loaded = true;
		drawThread();
		for (const x of t.replies.slice(-TRANSLATE_HISTORY).reverse()) void translateOne(x);
	} else if (m.t === 'typing') {
		const w = m as { key?: unknown; login?: unknown; avatar?: unknown };
		if (typeof w.key === 'string') typed({ key: w.key, login: typeof w.login === 'string' ? w.login : null, avatar: typeof w.avatar === 'string' ? w.avatar : null });
	} else if (m.t === 'unsay') {
		const u = m as { id: number; parent?: number; replies?: number; last?: string | null; repliers?: Who[] };
		if (typeof u.parent === 'number') {
			resum(u.parent, { replies: u.replies ?? 0, last: u.last ?? null, repliers: u.repliers ?? [] });
			if (thread?.id === u.parent) {
				thread.replies = thread.replies.filter((x) => x.id !== u.id);
				drawThread(false);
			}
		} else {
			messages = messages.filter((x) => x.id !== u.id);
			drawList();
			if (thread?.id === u.id) closeThread();
		}
	} else if (m.t === 'error') {
		const err = errAt?.isConnected ? errAt : foot.querySelector<HTMLElement>('.chat-err');
		if (err) {
			err.textContent = String((m as { text?: unknown }).text ?? 'Not sent.');
			err.hidden = false;
		}
	} else return false;
	return true;
}

export function save(): Kept {
	const draft = (box: HTMLElement) => box.querySelector('textarea')?.value || undefined;
	return {
		main: draft(foot),
		reply: thread ? draft(threadFoot) : undefined,
		thread: thread?.id,
		full: document.body.classList.contains('chat-full') || undefined,
		list: list.scrollTop,
		bottom: nearBottom(),
	};
}

/** Called before setup: full screen goes back with the panel, drafts and the thread once the socket says who the reader is. */
export function restore(k: Kept): void {
	restoring = { ...k };
}
