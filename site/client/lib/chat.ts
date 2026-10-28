/**
 * The site's chat (worker/presence.ts): one room for everyone on the site, a panel at the right that stays open from
 * page to page (it lives on `body`, outside what the client router swaps). It rides client/lib/presence.ts's socket:
 * presence hands over every chat frame (`message`) and who the reader is (`setYou`), and gives `setup` the function
 * that sends.
 *
 * Anyone can read; signed-in readers talk (Enter sends, Shift+Enter is a new line) and take back their own messages,
 * admins anyone's. While the panel is shut the top line's Chat button counts what came in. Whether it is open is kept
 * per browser (localStorage), wrapped like every storage access here because storage can refuse.
 */

type Who = { key: string; login: string | null; avatar: string | null };
type You = Who & { signIn: boolean; admin?: boolean };
type Msg = { id: number; key: string; login: string; avatar: string | null; text: string; at: string };

const OPEN_KEY = 'blaze.chat.open';
const GROUP_MS = 5 * 60_000;
const MAX = 500;

let send: (m: object) => void = () => {};
let you: You | null = null;
let base = '/';
let face: (w: Who) => HTMLElement = () => document.createElement('span');
let messages: Msg[] = [];
let unread = 0;

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
head.append(title, online, close);
const list = el('div', 'chat-list');
list.setAttribute('role', 'log');
list.setAttribute('aria-live', 'polite');
const jump = el('button', 'chat-jump', 'New messages');
jump.type = 'button';
jump.hidden = true;
const foot = el('div', 'chat-foot');
panel.append(head, list, jump, foot);

/** The top line's Chat button (client/lib/presence.ts places it). */
export const button = el('button', 'tl-btn tl-chat');
button.type = 'button';
button.setAttribute('aria-controls', 'chat');
panel.id = 'chat';
const badge = el('span', 'tl-badge');
badge.hidden = true;
button.append(el('span', 'tl-chat-icon'), el('span', 'tl-label', 'Chat'), badge);

function stored(): boolean {
	try {
		return localStorage.getItem(OPEN_KEY) === '1';
	} catch {
		return false;
	}
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
	if (open) {
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
	row.append(body(m.text));
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
		ta.placeholder = `Message as ${you.login}`;
		ta.setAttribute('aria-label', 'Message');
		const err = el('p', 'chat-err');
		err.hidden = true;
		err.id = 'chat-err';
		const go = el('button', 'chat-send', 'Send');
		go.type = 'submit';
		const grow = () => {
			ta.style.height = 'auto';
			ta.style.height = `${Math.min(ta.scrollHeight, 140)}px`;
		};
		ta.addEventListener('input', grow);
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

/** Wires the chat to the presence socket and the page. Called once. */
export function setup(opts: { send: (m: object) => void; base: string; face: (w: Who) => HTMLElement }): void {
	send = opts.send;
	base = opts.base;
	face = opts.face;
	document.body.append(panel);
	button.addEventListener('click', () => setOpen(panel.hidden));
	close.addEventListener('click', () => setOpen(false));
	jump.addEventListener('click', toBottom);
	list.addEventListener('scroll', () => {
		if (nearBottom()) jump.hidden = true;
	});
	document.addEventListener('keydown', (e) => {
		if (e.key === 'Escape' && !panel.hidden && panel.contains(document.activeElement)) setOpen(false);
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
	} else if (m.t === 'msg') {
		const msg = (m as { m: Msg }).m;
		const stick = nearBottom();
		messages.push(msg);
		if (messages.length > 200) messages.shift();
		list.querySelector('.chat-empty')?.remove();
		list.append(item(msg, messages[messages.length - 2]));
		if (panel.hidden) {
			if (msg.key !== you?.key) unread++;
			drawBadge();
			toBottom();
		} else if (stick || msg.key === you?.key) toBottom();
		else jump.hidden = false;
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
