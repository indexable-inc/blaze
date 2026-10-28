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
 * list, or beside it in full screen. Replies stay quiet (no bubbles, toasts or badge) unless they mention the reader
 * or come in a thread the reader is in (started or replied in): those pop up even when muted, and the thread's line
 * counts the replies the reader hasn't seen (kept per browser, `SEEN_KEY`) until the thread is opened. A reply can
 * also go to the chat ("Also send to chat"): it shows in the list too, under a line naming its thread.
 *
 * Reactions are our own line icons (`REACTS`), not emoji: a row of them in a message's actions, and a chip under the
 * message for each one it has, with how many, highlighted when one is the reader's (a click takes it back). Authors
 * (and admins) edit a message in place (Enter saves, Esc cancels; ↑ in an empty composer edits the reader's last
 * message there); an edited one says so.
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
	/** A reply also sent to the chat, and (in the list's history) its thread's message, cut short. */
	broadcast?: boolean;
	to?: { login: string; text: string };
	/** When the text last changed (ISO). */
	edited?: string;
	/** Kinds in the order they first came (worker/presence.ts), and who; left out when none. */
	reactions?: Reaction[];
};
type Reaction = { kind: string; who: { key: string; login: string }[] };
/** Where a message is drawn: the chat's list, or the open thread. */
type Pane = 'list' | 'thread';
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
export type Kept = {
	main?: string;
	reply?: string;
	thread?: number;
	full?: boolean;
	list?: number;
	bottom?: boolean;
	/** The reply composer's "Also send to chat", and a message being edited. */
	also?: boolean;
	edit?: { id: number; pane: Pane; text: string };
};
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
const PENCIL = 'M3 13l.6-2.8 6.8-6.8a1.4 1.4 0 0 1 2 0l.2.2a1.4 1.4 0 0 1 0 2l-6.8 6.8L3 13ZM9.4 4.4l2.2 2.2';
/** The reactions (worker/presence.ts REACTIONS), drawn like the icons above: one stroke, currentColor, never emoji. */
const REACTS: Record<string, { name: string; d: string }> = {
	up: { name: 'Thumbs up', d: 'M2.5 7.2H5v6.3H2.5ZM5 7.5 7.4 3a1.2 1.2 0 0 1 2.2.8L9.2 6.5h3.2a1.3 1.3 0 0 1 1.3 1.5l-.8 4.3a1.5 1.5 0 0 1-1.5 1.2H5' },
	heart: { name: 'Heart', d: 'M8 13.2S2.5 10 2.5 6.1A2.8 2.8 0 0 1 8 4.9a2.8 2.8 0 0 1 5.5 1.2C13.5 10 8 13.2 8 13.2Z' },
	laugh: { name: 'Laughing', d: 'M13.5 8A5.5 5.5 0 1 1 2.5 8a5.5 5.5 0 0 1 11 0ZM5.3 6.7c.4-.6 1.1-.6 1.5 0M9.2 6.7c.4-.6 1.1-.6 1.5 0M5.3 9.2h5.4a2.7 2.7 0 0 1-5.4 0Z' },
	eyes: { name: 'Looking', d: 'M1.8 8s2.3-4.3 6.2-4.3S14.2 8 14.2 8s-2.3 4.3-6.2 4.3S1.8 8 1.8 8ZM9.8 8a1.8 1.8 0 1 1-3.6 0 1.8 1.8 0 0 1 3.6 0Z' },
	check: { name: 'Done', d: 'M13.5 8A5.5 5.5 0 1 1 2.5 8a5.5 5.5 0 0 1 11 0ZM5.4 8.2l1.8 1.8 3.5-3.7' },
	fire: {
		name: 'Fire',
		d: 'M8 14c-2.6 0-4.5-1.8-4.5-4.2 0-2.9 2.6-4 2.9-7.3 1.8 1 3 2.7 3 4.4.7-.4 1.2-1.3 1.3-2.3 1.4 1.2 2.3 3 2.3 5.2 0 2.4-1.9 4.2-4.5 4.2ZM8 14c-1.1 0-1.9-.8-1.9-1.8 0-1.3 1.1-1.9 1.5-3 1 .7 2.3 1.6 2.3 3 0 1-.8 1.8-1.9 1.8Z',
	},
};
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
	tipOn(mute, m ? 'Muted: no pop-ups while the chat is shut, except mentions of you and replies in your threads' : 'Mute pop-ups while the chat is shut');
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
	if (m) for (const t of [...toasts.children]) if (!t.matches('.mention, .yours, .chat-notice')) t.remove();
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

/**
 * Pop-ups for messages that come while the chat is shut (not muted, or mentioning the reader, or a reply in a thread
 * they are in): the newest three.
 */
const toasts = el('div', 'chat-toasts');
toasts.setAttribute('aria-live', 'polite');
const TOAST_MS = 5000;
function toast(m: Msg, mention: boolean, yours = false): void {
	if ((muted() && !mention && !yours) || !panel.hidden) return;
	const t = el('button', mention ? 'chat-toast mention' : yours ? 'chat-toast yours' : 'chat-toast');
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
export function notice(title: string, text: string, opts: { ms?: number; icon?: string; action?: { label: string; run: () => void } } = {}): () => void {
	const t = el('div', 'chat-toast chat-notice');
	t.setAttribute('role', 'status');
	const words = el('span', 'chat-toast-words');
	words.append(el('b', '', title), el('span', '', text));
	t.append(icon(opts.icon ?? REFRESH), words);
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
		if (thread) markRead(thread.id);
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
function bodyOf(text: string): HTMLElement {
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
	const body = (s: string) => edited(bodyOf(s), m);
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
		if (row.querySelector('.chat-edit')) continue;
		const box = row.closest<HTMLElement>('.chat-list');
		const stick = box ? near(box) : false;
		for (const old of row.querySelectorAll('.chat-text, .chat-tr')) old.remove();
		const after = row.querySelector('.chat-reacts, .chat-also, .chat-replies, .chat-acts');
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

/**
 * The line under a message with replies: the latest repliers' faces, how many, when the last came, and on hover or
 * focus "View thread" in the time's place. In a thread the reader is in, the replies they haven't seen count instead,
 * in bold with a dot. Opens the thread.
 */
function summary(m: Msg): HTMLElement {
	const b = el('button', 'chat-replies');
	b.type = 'button';
	const faces = el('span', 'chat-replies-faces');
	for (const w of (m.repliers ?? []).slice(0, 3)) faces.append(face(w));
	const n = m.replies ?? 0;
	const fresh = unseen(m.id);
	const count = el('b', 'chat-replies-n', fresh ? `${fresh} new ${fresh === 1 ? 'reply' : 'replies'}` : `${n} ${n === 1 ? 'reply' : 'replies'}`);
	if (fresh) {
		b.classList.add('unread');
		const dot = el('i', 'chat-replies-dot');
		dot.setAttribute('aria-hidden', 'true');
		count.prepend(dot);
	}
	b.append(faces, count);
	const w = m.last ? when(m.last) : '';
	if (w) b.append(el('span', 'chat-replies-when', `last reply ${w === 'now' ? 'just now' : w}`));
	b.append(el('span', 'chat-replies-view', 'View thread ›'));
	b.addEventListener('click', () => openThread(m.id));
	return b;
}

/** Whether the reader may change `m`: theirs, or they are an admin. */
const canEdit = (m: Msg) => !!you?.login && (you.key === m.key || !!you.admin);

/** A message's row. `where`: in the list (with its thread's line and a reply button), atop a thread, or a reply. */
function item(m: Msg, prev: Msg | undefined, where: 'list' | 'root' | 'reply' = 'list'): HTMLElement {
	const pane: Pane = where === 'list' ? 'list' : 'thread';
	// A reply also sent to the chat, in the list: always its own header, under the line naming its thread.
	const shared = where === 'list' && m.parent !== undefined;
	const grouped = !shared && prev && prev.key === m.key && Date.parse(m.at) - Date.parse(prev.at) < GROUP_MS;
	const row = el('div', grouped ? 'chat-msg grouped' : 'chat-msg');
	row.dataset.id = String(m.id);
	if (mentionsYou(m)) row.classList.add('mention');
	if (shared) row.append(context(m));
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
	const editNow = editing?.id === m.id && editing.pane === pane && canEdit(m);
	if (editNow) {
		row.classList.add('editing');
		row.append(editor(m));
	} else row.append(...text(m));
	const rs = reactions(m, pane);
	if (rs) row.append(rs);
	if (where === 'reply' && m.broadcast) row.append(el('p', 'chat-also', 'Also sent to the chat'));
	if (where === 'list' && m.replies) row.append(summary(m));
	if (editNow) return row;
	const acts = el('div', 'chat-acts');
	if (you?.login) acts.append(quick(m, pane));
	if (where === 'list') {
		const r = el('button', 'chat-act');
		r.type = 'button';
		tipOn(r, 'Reply in thread');
		r.append(icon(BUBBLE));
		r.addEventListener('click', () => openThread(m.parent ?? m.id));
		acts.append(r);
	}
	if (canEdit(m)) {
		const ed = el('button', 'chat-act');
		ed.type = 'button';
		tipOn(ed, 'Edit');
		ed.append(icon(PENCIL));
		ed.addEventListener('click', () => startEdit(m, pane));
		acts.append(ed);
	}
	if (you && (you.key === m.key || you.admin)) {
		const del = el('button', 'chat-act chat-del', '×');
		del.type = 'button';
		tipOn(del, m.parent !== undefined || !m.replies ? 'Delete' : 'Delete, with its thread');
		del.setAttribute('aria-label', 'Delete message');
		del.addEventListener('click', () => send({ t: 'unsay', id: m.id }));
		acts.append(del);
	}
	if (acts.childElementCount) row.append(acts);
	return row;
}

/** The line over a reply also sent to the chat, in the list: "replied to a thread:" and its message. Opens the thread. */
function context(m: Msg): HTMLElement {
	const b = el('button', 'chat-ctx');
	b.type = 'button';
	b.append(el('span', '', 'replied to a thread:'), el('span', 'chat-ctx-text', m.to?.text ?? ''));
	tipOn(b, m.to ? `${m.to.login}: ${m.to.text}` : 'Open the thread');
	b.addEventListener('click', () => openThread(m.parent!));
	return b;
}

/** The "(edited)" after a message's text, with when in its tooltip. */
function edited(p: HTMLElement, m: Msg): HTMLElement {
	if (!m.edited) return p;
	const e = el('span', 'chat-edited', ' (edited)');
	tipOn(e, `Edited ${new Date(m.edited).toLocaleString()}`);
	p.append(e);
	return p;
}

// Reactions.

/** Every drawn copy of message `id` (the list's, the open thread's: a reply also sent to the chat is in both). */
function copies(id: number): Msg[] {
	const all = [...messages, ...(thread?.root ? [thread.root] : []), ...(thread?.replies ?? [])];
	return [...new Set(all.filter((x) => x.id === id))];
}

/** Whether the reader has reacted to `m` with `kind`. */
const reacted = (m: Msg, kind: string) => (m.reactions ?? []).some((r) => r.kind === kind && r.who.some((w) => w.key === you?.key));

/** Adds the reader's reaction, or takes it back if they have it: the server tells everyone (a `react` frame). */
function react(m: Msg, kind: string, pane: Pane): void {
	if (!you?.login) return;
	send({ t: 'react', id: m.id, kind, on: !reacted(m, kind) });
	errAt = (pane === 'thread' ? threadFoot : foot).querySelector('.chat-err');
}

/** "you, ana and bob" for a reaction's tooltip: the reader first, at most a dozen names. */
function names(who: Reaction['who']): string {
	const l = [...who.filter((w) => w.key === you?.key).map(() => 'you'), ...who.filter((w) => w.key !== you?.key).map((w) => w.login)];
	const shown = l.length > 12 ? [...l.slice(0, 11), `${l.length - 11} others`] : l;
	return shown.length < 2 ? shown.join('') : `${shown.slice(0, -1).join(', ')} and ${shown[shown.length - 1]}`;
}

/** The chips under a message, one per reaction it has: its icon and how many; the tooltip says who. */
function reactions(m: Msg, pane: Pane): HTMLElement | null {
	const rs = (m.reactions ?? []).filter((r) => REACTS[r.kind] && r.who.length);
	if (!rs.length) return null;
	const box = el('div', 'chat-reacts');
	for (const r of rs) {
		const mine = r.who.some((w) => w.key === you?.key);
		const b = el('button', mine ? 'chat-react mine' : 'chat-react');
		b.type = 'button';
		b.append(icon(REACTS[r.kind].d), el('span', '', String(r.who.length)));
		const who = names(r.who);
		tipOn(b, `${who.startsWith('you') ? `Y${who.slice(1)}` : who} reacted with ${REACTS[r.kind].name.toLowerCase()}`);
		b.setAttribute('aria-label', `${REACTS[r.kind].name}, ${r.who.length}: ${who}`);
		b.setAttribute('aria-pressed', String(mine));
		if (you?.login) b.addEventListener('click', () => react(m, r.kind, pane));
		else b.setAttribute('aria-disabled', 'true');
		box.append(b);
	}
	return box;
}

/** The quick-react row in a message's actions: every reaction, pressed when the reader has it. */
function quick(m: Msg, pane: Pane): HTMLElement {
	const row = el('span', 'chat-quick');
	for (const [kind, r] of Object.entries(REACTS)) {
		const b = el('button', 'chat-act');
		b.type = 'button';
		tipOn(b, r.name);
		b.setAttribute('aria-pressed', String(reacted(m, kind)));
		b.append(icon(r.d));
		b.addEventListener('click', () => react(m, kind, pane));
		row.append(b);
	}
	return row;
}

/** Draws message `id` again wherever it shows (an edit, a reaction, an edit box opened or shut). */
function again(id: number): void {
	const m = messages.find((x) => x.id === id);
	if (m) drawRow(m);
	const t = thread;
	const row = t && threadList.querySelector(`.chat-msg[data-id="${id}"]`);
	if (!t || !row) return;
	const stick = near(threadList);
	const i = t.replies.findIndex((x) => x.id === id);
	if (t.root?.id === id) row.replaceWith(item(t.root, undefined, 'root'));
	else if (i >= 0) row.replaceWith(item(t.replies[i], t.replies[i - 1], 'reply'));
	if (stick) threadList.scrollTop = threadList.scrollHeight;
}

// Editing in place.

/** The message being edited, where, and the text so far; `focus` while its box has the focus (it survives redraws). */
let editing: { id: number; pane: Pane; draft: string; focus: boolean } | null = null;

function startEdit(m: Msg, pane: Pane): void {
	const was = editing;
	editing = { id: m.id, pane, draft: m.text, focus: true };
	if (was && was.id !== m.id) again(was.id);
	again(m.id);
}

function stopEdit(): void {
	const e = editing;
	if (!e) return;
	editing = null;
	again(e.id);
	(e.pane === 'thread' ? threadFoot : foot).querySelector('textarea')?.focus({ preventScroll: true });
}

/** Sends the edit, if the text changed and isn't empty; the box shuts and the server's `edit` frame redraws. */
function saveEdit(): void {
	const e = editing;
	if (!e) return;
	const text = e.draft.trim();
	if (!text) return;
	const m = copies(e.id)[0];
	if (m && text !== m.text) {
		send({ t: 'edit', id: e.id, text });
		errAt = (e.pane === 'thread' ? threadFoot : foot).querySelector('.chat-err');
	}
	stopEdit();
}

/** The edit box in a message's place: Enter saves, Shift+Enter is a new line, Esc cancels (and nothing else). */
function editor(m: Msg): HTMLElement {
	const e = editing!;
	const box = el('div', 'chat-edit');
	const form = el('div', 'chat-form');
	const ta = el('textarea');
	ta.rows = 1;
	ta.maxLength = MAX;
	ta.value = e.draft;
	ta.setAttribute('aria-label', `Edit message: ${m.text.slice(0, 60)}`);
	const grow = () => {
		ta.style.height = 'auto';
		ta.style.height = `${Math.min(ta.scrollHeight, 200)}px`;
	};
	ta.addEventListener('input', () => {
		e.draft = ta.value;
		grow();
	});
	ta.addEventListener('focus', () => (e.focus = true));
	// A redraw takes the box away with the focus in it: the next box takes it back. Only a real move away lets go.
	ta.addEventListener('blur', () => queueMicrotask(() => ta.isConnected && (e.focus = false)));
	ta.addEventListener('keydown', (ev) => {
		if (ev.isComposing) return;
		if (ev.key === 'Escape') {
			ev.preventDefault();
			ev.stopPropagation();
			stopEdit();
		} else if (ev.key === 'Enter' && !ev.shiftKey) {
			ev.preventDefault();
			saveEdit();
		}
	});
	const hint = el('p', 'chat-edit-hint');
	const no = el('button', '', 'cancel');
	no.type = 'button';
	no.addEventListener('click', stopEdit);
	const yes = el('button', '', 'save');
	yes.type = 'button';
	yes.addEventListener('click', saveEdit);
	hint.append('Esc to ', no, ' · Enter to ', yes);
	form.append(ta);
	box.append(form, hint);
	requestAnimationFrame(() => {
		if (!ta.isConnected) return;
		grow();
		// Not under the open thread (it covers the list outside full screen): the thread's composer keeps the focus.
		const covered = e.pane === 'list' && panel.classList.contains('threaded') && !document.body.classList.contains('chat-full');
		if (!e.focus || covered) return;
		ta.focus({ preventScroll: true });
		ta.setSelectionRange(ta.value.length, ta.value.length);
		box.scrollIntoView({ block: 'nearest' });
	});
	return box;
}

/** The reader's last message in a composer's place (the list, or the open thread), for ↑ to edit. */
function lastOwn(pane: Pane): Msg | undefined {
	const l = pane === 'list' ? messages : [...(thread?.root ? [thread.root] : []), ...(thread?.replies ?? [])];
	return l.findLast((x) => x.key === you?.key);
}

// Unread replies in the reader's threads.

/**
 * The threads the reader is in (started or replied in), by id, each with the ids of the replies others wrote after the
 * reader's own last one: worker/presence.ts `mine`, then kept up to date from the socket. Signed-in readers only.
 */
const yours = new Map<number, number[]>();
/** The last reply seen in each of those threads (localStorage, by thread id). */
const SEEN_KEY = 'blaze.chat.seen';
let seen: Record<string, number> = {};

/** Reads what was seen from storage; false when nothing ever was kept there. */
function loadSeen(): boolean {
	try {
		const v = localStorage.getItem(SEEN_KEY);
		if (v === null) return false;
		const o: unknown = JSON.parse(v);
		if (o && typeof o === 'object') seen = o as Record<string, number>;
	} catch {
		// Storage refused, or a bad value: as on a first run, so old replies don't all light up on every connect.
		return false;
	}
	return true;
}

function keepSeen(): void {
	try {
		localStorage.setItem(SEEN_KEY, JSON.stringify(seen));
	} catch {
		// Storage refused: it lasts this page.
	}
}

/** How many of thread `id`'s replies the reader hasn't seen (0 for a thread they're not in). */
const unseen = (id: number) => (you?.login ? (yours.get(id) ?? []).filter((x) => x > (seen[id] ?? 0)).length : 0);
const inYours = (r: Msg) => r.parent !== undefined && r.key !== you?.key && yours.has(r.parent);

/** The reader has seen thread `id` up to its last reply. */
function markRead(id: number): void {
	const ids = [...(yours.get(id) ?? []), ...(thread?.id === id ? thread.replies.map((r) => r.id) : [])];
	if (!yours.has(id) || !ids.length) return;
	const top = Math.max(...ids);
	if (top <= (seen[id] ?? 0)) return;
	seen[id] = top;
	keepSeen();
	const m = messages.find((x) => x.id === id);
	if (m) drawRow(m);
}

/** The `chat` frame's `mine`. The first time, all so far counts as seen (no flood of old replies); gone threads are dropped. */
function takeMine(raw: unknown): void {
	yours.clear();
	if (!you?.login || !raw || typeof raw !== 'object') return;
	for (const [k, v] of Object.entries(raw)) if (Array.isArray(v)) yours.set(Number(k), v.filter((x): x is number => typeof x === 'number'));
	const first = !loadSeen();
	const next: Record<string, number> = {};
	for (const [id, ids] of yours) {
		const s = first ? Math.max(0, ...ids) : (seen[id] ?? 0);
		if (s) next[id] = s;
	}
	seen = next;
	keepSeen();
}

/** A reply came: who is in its thread now, and what they haven't seen. */
function track(r: Msg): void {
	if (!you?.login || r.parent === undefined) return;
	if (r.key === you.key) yours.set(r.parent, []);
	else yours.get(r.parent)?.push(r.id);
}

/** The reply composer's "Also send to chat": the reply shows in the chat's list too. */
function alsoBox(on: boolean): { box: HTMLElement; input: HTMLInputElement } {
	const box = el('label', 'chat-also-to');
	const input = el('input');
	input.type = 'checkbox';
	input.checked = on;
	box.append(input, el('span', '', 'Also send to chat'));
	return { box, input };
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
	if (!panel.hidden) markRead(id);
}

function closeThread(): void {
	if (!thread) return;
	thread = null;
	if (editing?.pane === 'thread') editing = null;
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
	const also = restoring?.thread === thread.id && !!restoring.also;
	if (you?.login && restoring) delete restoring.reply;
	threadFoot.replaceChildren(you?.login ? composer('Reply…', thread.id, draft, also) : signIn());
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
function composer(placeholder: string, parent: number | null, draft = '', alsoOn = false): HTMLElement {
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
		// ↑ in an empty composer edits the reader's last message here, as in Slack.
		if (e.key === 'ArrowUp' && !ta.value && !e.shiftKey && !e.altKey && !e.metaKey && !e.ctrlKey && !e.isComposing) {
			const pane: Pane = parent === null ? 'list' : 'thread';
			const last = lastOwn(pane);
			if (last) {
				e.preventDefault();
				return startEdit(last, pane);
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
		send(parent === null ? { t: 'say', text } : { t: 'say', text, parent, ...(also?.input.checked && { broadcast: true }) });
		if (also) also.input.checked = false;
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
	const also = parent === null ? null : alsoBox(alsoOn);
	if (also) box.append(also.box);
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

/**
 * Posts a top-level message as the reader, from outside the panel (client/lib/cursors.ts: a line said by the pointer
 * with T); false when the reader can't talk. A refusal shows under the panel's composer, and in a pop-up while the
 * panel is shut.
 */
export function post(text: string): boolean {
	if (!you?.login) return false;
	send({ t: 'say', text });
	errAt = null;
	return true;
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
	// A thread read in another tab is read here too.
	addEventListener('storage', (e) => {
		if (e.key !== SEEN_KEY) return;
		loadSeen();
		drawList();
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

/**
 * A reply came: its thread's line, the open thread, and, when it names the reader or comes in a thread they are in,
 * the one signal a reply gets (a reply also sent to the chat gets the list's signals instead).
 */
function reply(r: Msg): void {
	track(r);
	bump(r);
	const t = thread;
	if (t && t.id === r.parent && !t.replies.some((x) => x.id === r.id)) {
		const stick = near(threadList) || r.key === you?.key;
		t.replies.push(r);
		drawThread(stick);
		void translateOne(r);
		if (!panel.hidden && !document.hidden) markRead(t.id);
	}
	const mention = mentionsYou(r);
	if (r.broadcast || !(mention || inYours(r))) return;
	speak(r.key, r.login, r.text);
	if (panel.hidden) {
		unread++;
		toast(r, mention, !mention);
		drawBadge();
	}
}

/** A chat frame from the socket; false when it is not one. */
export function message(m: { t?: unknown }): boolean {
	if (m.t === 'chat') {
		const ms = (m as { messages?: unknown }).messages;
		messages = Array.isArray(ms) ? (ms as Msg[]) : [];
		takeMine((m as { mine?: unknown }).mine);
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
			// A reply also sent to the chat goes on into the list.
			if (!msg.broadcast) return true;
		}
		const stick = nearBottom();
		const mine = msg.key === you?.key;
		if (mine && msg.parent === undefined && you?.login) yours.set(msg.id, []);
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
				toast(msg, mentionsYou(msg), inYours(msg));
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
		if (!panel.hidden) markRead(t.id);
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
			const ids = yours.get(u.parent);
			if (ids) yours.set(u.parent, ids.filter((x) => x !== u.id));
			// A reply also sent to the chat leaves the list too.
			if (messages.some((x) => x.id === u.id)) {
				messages = messages.filter((x) => x.id !== u.id);
				drawList();
			}
		} else {
			// With its replies also sent to the chat.
			messages = messages.filter((x) => x.id !== u.id && x.parent !== u.id);
			yours.delete(u.id);
			drawList();
			if (thread?.id === u.id) closeThread();
		}
		if (editing?.id === u.id) editing = null;
	} else if (m.t === 'edit') {
		const f = m as { id?: unknown; text?: unknown; edited?: unknown };
		if (typeof f.id !== 'number' || typeof f.text !== 'string') return true;
		const { id, text } = f;
		for (const x of copies(id)) {
			x.text = text;
			x.edited = typeof f.edited === 'string' ? f.edited : x.edited;
		}
		// The replies also sent to the chat quote their thread's message.
		for (const x of messages) {
			if (x.parent !== id || !x.to) continue;
			x.to = { login: x.to.login, text: text.slice(0, 160) };
			again(x.id);
		}
		translations.delete(id);
		again(id);
		const now = copies(id)[0];
		if (now) void translateOne(now);
	} else if (m.t === 'react') {
		const f = m as { id?: unknown; reactions?: unknown };
		if (typeof f.id !== 'number') return true;
		const rs = Array.isArray(f.reactions) ? (f.reactions as Reaction[]) : [];
		for (const x of copies(f.id)) {
			if (rs.length) x.reactions = rs;
			else delete x.reactions;
		}
		again(f.id);
	} else if (m.t === 'error') {
		const err = errAt?.isConnected ? errAt : foot.querySelector<HTMLElement>('.chat-err');
		const why = String((m as { text?: unknown }).text ?? 'Not sent.');
		if (err) {
			err.textContent = why;
			err.hidden = false;
		}
		// Shut, so it came from outside the panel (`post`): the reader would never see the line under the composer.
		if (panel.hidden) notice('Not sent to the chat', why, { ms: TOAST_MS, icon: CROSS });
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
		also: threadFoot.querySelector<HTMLInputElement>('.chat-also-to input')?.checked || undefined,
		edit: editing ? { id: editing.id, pane: editing.pane, text: editing.draft } : undefined,
	};
}

/**
 * Called before setup: full screen goes back with the panel, drafts, the thread and an edit in progress once the socket
 * says who the reader is.
 */
export function restore(k: Kept): void {
	restoring = { ...k };
	// The edit box comes back when its message is drawn (with the reader known: only theirs, or an admin's, can be).
	if (k.edit) editing = { id: k.edit.id, pane: k.edit.pane, draft: k.edit.text, focus: true };
}
