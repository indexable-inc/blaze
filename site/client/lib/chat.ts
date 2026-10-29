/**
 * The site's chat (worker/presence.ts): one room for everyone on the site, shown in the overlay feed at the bottom left
 * (client/lib/feed.ts), like Minecraft's chat. Every message passes there, with the joins, leaves and the site's own
 * word, and the feed's history (T, Enter, or the top line's Chat) holds every message this tab has: the server's last
 * ones on connect (worker/presence.ts CHAT_SHOWN) and all that came since. It rides client/lib/presence.ts's socket:
 * presence hands over every chat frame (`message`), who the reader is (`setYou`) and who is online (`setPeople`), and
 * gives `setup` the function that sends.
 *
 * The feed's input is made here. What is typed shows live by the reader's pointer to the others on the page
 * (client/lib/cursors.ts `say`); Enter sends it to the chat (signed in) or leaves it as that bubble only (not signed
 * in, not kept; the hint under the input offers sign-in). `@` completes GitHub logins (people online, recent authors),
 * and a message that mentions the reader is marked and shows in the feed even muted. Who is writing shows over the
 * input.
 *
 * The chat is flat, like Minecraft's. The server also knows threads and reactions (from the chat's old side panel): a
 * reply shows here only when it was also sent to the chat or mentions the reader, and reactions don't show.
 *
 * Authors (and admins) edit and take back their messages: ↑ in an empty input, or the pencil on the line in the open
 * history, puts the message in the input (Enter saves, Esc cancels and gives the draft back); the × takes it back.
 *
 * Someone else's message in another language is translated where the computer can do it (client/lib/translate.ts),
 * and its line says so. The model downloads once the reader has chatted or pressed a line's Translate; while it does,
 * a line in the feed says how far, with Stop.
 *
 * Before an update reloads the page, `save` hands over the input's text, whether it was open and an edit in progress,
 * and `restore` puts them back.
 */

import { tipOn } from './tip';
import * as feed from './feed';
import { CROSS, icon } from './feed';
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
	/** Set on a reply in a thread (sent by the old panel): the message it answers. */
	parent?: number;
	/** A reply also sent to the chat. */
	broadcast?: boolean;
	/** When the text last changed (ISO). */
	edited?: string;
};

const MAX = 500;
/** How many messages the tab keeps, as many as the server does (worker/presence.ts CHAT_KEPT). */
const KEEP = 500;
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
const PENCIL = 'M3 13l.6-2.8 6.8-6.8a1.4 1.4 0 0 1 2 0l.2.2a1.4 1.4 0 0 1 0 2l-6.8 6.8L3 13ZM9.4 4.4l2.2 2.2';
const DOWNLOAD = 'M8 2.5v8M4.5 7 8 10.5 11.5 7M3 13.5h10';
const TRANSLATOR = 'translator';

let send: (m: object) => void = () => {};
let you: You | null = null;
let base = '/';
let face: (w: Who) => HTMLElement = () => document.createElement('span');
let speak: (key: string, login: string, text: string | null) => void = () => {};
let live: (text: string | null, done?: boolean) => void = () => {};
let messages: Msg[] = [];
/** Signed-in people online now (client/lib/presence.ts), for completing mentions. */
let people: Who[] = [];
/** What the chat keeps across a reload for a site update (client/lib/update.ts). */
export type Kept = {
	/** The input's text, and whether the feed was open. */
	say?: string;
	sayOpen?: boolean;
	/** A message being edited: its new text so far, and the draft the edit took the place of. */
	edit?: { id: number; text: string; draft: string };
};
/** Waiting to be put back: the edit once the socket brings the reader and the messages. */
let restoring: Kept | null = null;
/** Translations by message id: waiting for the reader to agree, made, or not to be made. */
type Tr = { s: 'ask'; from: string } | { s: 'done'; from: string; text: string } | { s: 'none' };
const translations = new Map<number, Tr>();
/** Who is writing now, by key. */
const typing = new Map<string, { who: Who; timer: ReturnType<typeof setTimeout> }>();
/** The message being edited in the input, and the draft it took the place of (given back when the edit ends). */
let editing: { id: number; draft: string } | null = null;

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls = '', text = ''): HTMLElementTagNameMap[K] {
	const e = document.createElement(tag);
	if (cls) e.className = cls;
	if (text) e.textContent = text;
	return e;
}

// ---- The input ----------------------------------------------------------------------------------------------------

const box = el('div', 'chat-compose');
const typingRow = el('div', 'chat-typing');
typingRow.setAttribute('aria-live', 'polite');
typingRow.hidden = true;
const form = el('form', 'chat-form');
const ta = el('textarea');
ta.rows = 1;
ta.maxLength = MAX;
ta.enterKeyHint = 'send';
ta.setAttribute('aria-label', 'Chat');
const go = el('button', 'chat-send');
go.type = 'submit';
go.setAttribute('aria-label', 'Send');
tipOn(go, 'Send (Enter)');
go.disabled = true;
go.append(icon('M8 13V3M3.5 7.5 8 3l4.5 4.5'));
/** The mention pop-up: the logins that fit the `@word` the caret ends. */
const pop = el('ul', 'chat-pop');
pop.setAttribute('role', 'listbox');
pop.hidden = true;
const hint = el('p', 'feed-hint');
form.append(ta, go);
box.append(typingRow, pop, form, hint);

function grow(): void {
	ta.style.height = 'auto';
	ta.style.height = `${Math.min(ta.scrollHeight, 140)}px`;
	go.disabled = !ta.value.trim();
}

let picks: Who[] = [];
let pick = 0;
function query(): { start: number; q: string } | null {
	if (ta.selectionStart !== ta.selectionEnd) return null;
	const upto = ta.value.slice(0, ta.selectionStart);
	const m = /(?:^|[^\w])@([A-Za-z0-9-]{0,39})$/.exec(upto);
	return m ? { start: upto.length - m[1].length - 1, q: m[1] } : null;
}
function shut(): void {
	picks = [];
	pop.hidden = true;
}
function insert(login: string): void {
	const at = query();
	if (!at) return shut();
	ta.setRangeText(`@${login} `, at.start, ta.selectionStart, 'end');
	shut();
	grow();
}
function drawPop(): void {
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
}

/** Logins to offer after `@`: recent authors, latest first, then the rest online; not the reader. */
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
	for (let i = messages.length - 1; i >= 0; i--) add(messages[i]);
	for (const w of people) add(w);
	const lq = q.toLowerCase();
	const starts = all.filter((w) => w.login!.toLowerCase().startsWith(lq));
	const inside = all.filter((w) => !w.login!.toLowerCase().startsWith(lq) && w.login!.toLowerCase().includes(lq));
	return [...starts, ...inside].slice(0, PICKS);
}

let sentTyping = 0;
ta.addEventListener('input', () => {
	grow();
	pick = 0;
	drawPop();
	if (editing) return;
	live(ta.value);
	if (you?.login && ta.value.trim() && Date.now() - sentTyping > TYPING_SEND_MS) {
		sentTyping = Date.now();
		send({ t: 'typing' });
	}
});
ta.addEventListener('click', drawPop);
ta.addEventListener('keyup', (e) => {
	if (['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(e.key)) drawPop();
});
ta.addEventListener('blur', shut);
ta.addEventListener('focus', () => feed.open());
ta.addEventListener('keydown', (e) => {
	if (e.isComposing) return;
	if (picks.length) {
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
			// The pop-up only: the feed's own Esc waits for the next press.
			e.preventDefault();
			e.stopPropagation();
			return shut();
		}
	}
	if (e.key === 'Escape' && editing) {
		e.preventDefault();
		e.stopPropagation();
		return stopEdit();
	}
	// ↑ in an empty input edits the reader's last message, as in Slack.
	if (e.key === 'ArrowUp' && !ta.value && !editing && !e.shiftKey && !e.altKey && !e.metaKey && !e.ctrlKey) {
		const last = messages.findLast((x) => x.key === you?.key && canEdit(x));
		if (last) {
			e.preventDefault();
			return startEdit(last);
		}
	}
	if (e.key === 'Enter' && !e.shiftKey) {
		e.preventDefault();
		form.requestSubmit();
	}
});
form.addEventListener('submit', (e) => {
	e.preventDefault();
	const text = ta.value.trim();
	if (!text) return;
	if (editing) return saveEdit(text);
	live(text, true);
	if (you?.login) send({ t: 'say', text });
	else feed.push({ kind: 'local', face: you ? face(you) : undefined, who: 'You', text, note: 'seen by readers on this page, not kept' });
	tr.allow();
	sentTyping = 0;
	ta.value = '';
	shut();
	grow();
	feed.close();
});

/** The words under the input: what the keys do, sign-in for a reader who isn't, and Mute. */
function drawHint(): void {
	hint.replaceChildren();
	ta.placeholder = editing ? 'Edit your message…' : you?.login ? 'Message everyone…' : 'Say hi to this page…';
	if (editing) {
		const no = el('button', '', 'cancel');
		no.type = 'button';
		no.addEventListener('click', stopEdit);
		hint.append('Editing your message · ↵ saves · Esc to ', no);
		return;
	}
	if (you?.login) hint.append('↵ sends to everyone · ↑ edits your last · Esc closes');
	else {
		hint.append('Readers on this page see it by your pointer; it is not kept.');
		if (you?.signIn) {
			const a = el('a', '', 'Sign in with GitHub to chat');
			a.href = `${base}auth/github?next=${encodeURIComponent(location.pathname.slice(base.length - 1) || '/')}`;
			hint.append(' ', a);
		}
	}
	const m = feed.muted();
	const mute = el('button', '', m ? 'Unmute' : 'Mute');
	mute.type = 'button';
	mute.setAttribute('aria-pressed', String(m));
	tipOn(mute, m ? 'Muted: no chat, joins or leaves pass by, except mentions of you' : 'Keep the chat, joins and leaves from passing by (mentions of you still do)');
	mute.addEventListener('click', () => {
		feed.setMuted(!m);
		drawHint();
	});
	hint.append(' · ', mute);
}

function drawTyping(): void {
	const who = [...typing.values()].map((t) => t.who);
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
	const timer = setTimeout(() => stoppedTyping(w.key), TYPING_MS);
	typing.set(w.key, { who: w, timer });
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

// ---- Editing ------------------------------------------------------------------------------------------------------

/** Whether the reader may change `m`: theirs, or they are an admin. */
const canEdit = (m: Msg) => !!you?.login && (you.key === m.key || !!you.admin);

/** Puts message `m` in the input to edit; what was typed there waits until the edit ends. */
function startEdit(m: Msg, text = m.text, draft?: string): void {
	const was = editing;
	editing = { id: m.id, draft: draft ?? was?.draft ?? ta.value };
	if (!was) live(null);
	else refeedId(was.id);
	ta.value = text;
	grow();
	refeedId(m.id);
	drawHint();
	feed.open();
	ta.focus({ preventScroll: true });
	ta.setSelectionRange(ta.value.length, ta.value.length);
}

/** Ends the edit: the draft comes back. */
function stopEdit(): void {
	const e = editing;
	if (!e) return;
	editing = null;
	ta.value = e.draft;
	grow();
	refeedId(e.id);
	drawHint();
	if (feed.isOpen() && ta.value.trim()) live(ta.value);
}

/** Sends the edit if the text changed; the server's `edit` frame redraws the line. */
function saveEdit(text: string): void {
	const m = messages.find((x) => x.id === editing?.id);
	if (m && text !== m.text) send({ t: 'edit', id: m.id, text });
	stopEdit();
}

// ---- Lines in the feed --------------------------------------------------------------------------------------------

/** Whether `m` is someone else's and names the reader with an `@` (links skipped: an `@` in a URL is no mention). */
function mentionsYou(m: Msg): boolean {
	const me = you?.login?.toLowerCase();
	if (!me || m.key === you?.key) return false;
	for (const t of m.text.matchAll(TOKENS)) if (t[1]?.toLowerCase() === me) return true;
	return false;
}

/** The text, with http(s) links made links and `@login`s marked. Built as nodes: nothing from a message is parsed as HTML. */
function words(text: string): Node[] {
	const out: Node[] = [];
	const me = you?.login?.toLowerCase();
	let at = 0;
	for (const m of text.matchAll(TOKENS)) {
		out.push(document.createTextNode(text.slice(at, m.index)));
		const login = m[1];
		const a = el('a', login ? 'chat-at' : '', m[0]);
		if (login) {
			a.href = `https://github.com/${encodeURIComponent(login)}`;
			if (login.toLowerCase() === me) a.classList.add('me');
		} else a.href = m[0];
		a.rel = 'nofollow noopener ugc';
		a.target = '_blank';
		out.push(a);
		at = m.index + m[0].length;
	}
	out.push(document.createTextNode(text.slice(at)));
	return out;
}

/** A message's line in the feed: its writer and text (translated, when it has been), marked when it names the reader. */
function feedLine(m: Msg, live: boolean): feed.Line {
	const t = translations.get(m.id);
	const text = t?.s === 'done' ? t.text : m.text;
	const notes = [m.edited ? 'edited' : '', t?.s === 'done' ? `translated from ${tr.languageName(t.from)}` : ''].filter(Boolean);
	const tools: NonNullable<feed.Line['tools']> = [];
	if (t?.s === 'ask') tools.push({ label: 'Translate', tip: `${tr.languageName(t.from)}: downloads the translator once (about 1.2 GB), then translates on this computer`, run: () => tr.allow() });
	if (canEdit(m)) {
		tools.push({ label: 'Edit', d: PENCIL, run: () => startEdit(m) });
		tools.push({ label: 'Delete', d: CROSS, run: () => send({ t: 'unsay', id: m.id }) });
	}
	return {
		id: `m${m.id}`,
		kind: 'chat',
		at: Date.parse(m.at) || Date.now(),
		face: face(m),
		who: m.login,
		text,
		words: words(text),
		note: notes.length ? `(${notes.join(', ')})` : undefined,
		tone: mentionsYou(m) ? 'mention' : undefined,
		editing: editing?.id === m.id,
		tools,
		live,
	};
}

/** Message `id`'s line drawn again (an edit, a translation, who the reader is), if it has one. */
function refeedId(id: number): void {
	const m = messages.find((x) => x.id === id);
	if (m && feed.has(`m${id}`)) feed.push(feedLine(m, false));
}

/** Translates `m` when it is someone else's, in another language, and this computer can. */
async function translateOne(m: Msg): Promise<void> {
	if (m.key === you?.key || translations.has(m.id)) return;
	translations.set(m.id, { s: 'none' });
	const from = await tr.needs(m.text);
	if (!from) return;
	if (await tr.mustAsk()) {
		translations.set(m.id, { s: 'ask', from });
		return refeedId(m.id);
	}
	const out = await tr.translate(m.text);
	translations.set(m.id, out && out !== m.text ? { s: 'done', from, text: out } : { s: 'none' });
	refeedId(m.id);
}

/** Messages `ids` taken back: out of the list and the feed (an edit of one ends). */
function drop(ids: Set<number>): void {
	messages = messages.filter((x) => !ids.has(x.id));
	for (const id of ids) feed.remove(`m${id}`);
	if (editing && ids.has(editing.id)) stopEdit();
}

// ---- Wiring -------------------------------------------------------------------------------------------------------

/** Wires the chat to the presence socket and the feed. Called once. */
export function setup(opts: {
	send: (m: object) => void;
	base: string;
	face: (w: Who) => HTMLElement;
	speak: (key: string, login: string, text: string | null) => void;
	/** The line typed in the feed's input, live by the reader's pointer (client/lib/cursors.ts `say`). */
	live: (text: string | null, done?: boolean) => void;
}): void {
	send = opts.send;
	speak = opts.speak;
	live = opts.live;
	base = opts.base;
	face = opts.face;
	tr.setup(base);
	// Agreed (a line's Translate, or the reader's first message): translate every message that was waiting on it.
	tr.onAllow(() => {
		for (const m of messages) {
			if (translations.get(m.id)?.s !== 'ask') continue;
			translations.delete(m.id);
			void translateOne(m);
		}
	});
	// The download, said in the feed while it runs, with a way to stop it.
	tr.watch((s) => {
		if (s.s === 'loading') {
			const pct = s.total ? `${Math.floor((s.loaded / s.total) * 100)}% of ${(s.total / 1e9).toFixed(1)} GB, once` : 'about 1.2 GB, once';
			feed.push({ id: TRANSLATOR, kind: 'notice', face: icon(DOWNLOAD), who: 'Getting the translator', text: pct, action: { label: 'Stop', run: tr.stop }, sticky: true, live: !feed.has(TRANSLATOR) });
			return;
		}
		feed.remove(TRANSLATOR);
		if (s.s === 'failed') feed.push({ kind: 'error', face: icon(CROSS), who: 'No translations', text: s.message });
		// Stopped or failed: nothing waits on the reader to agree any more.
		if (s.s !== 'failed' && s.s !== 'idle') return;
		for (const [id, t] of translations) {
			if (t.s !== 'ask') continue;
			translations.set(id, { s: 'none' });
			refeedId(id);
		}
	});
	if (restoring?.say) ta.value = restoring.say;
	grow();
	drawHint();
	feed.setInput(box, {
		open: () => {
			if (ta.value.trim() && !editing) live(ta.value);
		},
		// Shut unsent: the bubble goes, the text stays for next time.
		close: () => live(null),
	});
	if (restoring?.sayOpen) feed.open();
}

export function setYou(w: You): void {
	you = w;
	if (editing && !messages.some((x) => x.id === editing!.id && canEdit(x))) stopEdit();
	drawHint();
	for (const m of messages) refeedId(m.id);
}

/** Everyone online (client/lib/presence.ts); the signed-in ones are offered when completing a mention. */
export function setPeople(list: Who[]): void {
	people = list.filter((w) => w.login);
}

/** Whether a message shows here: the chat is flat, so a reply in a thread only when also sent to the chat, or it names the reader. */
const flat = (m: Msg) => m.parent === undefined || !!m.broadcast || mentionsYou(m);

/** A chat frame from the socket; false when it is not one. */
export function message(m: { t?: unknown }): boolean {
	if (m.t === 'chat') {
		const ms = (m as { messages?: unknown }).messages;
		messages = (Array.isArray(ms) ? (ms as Msg[]) : []).filter(flat);
		feed.sync('m', messages.map((x) => feedLine(x, false)));
		// The last of what the socket sends on connect: an edit in progress before an update reloaded the page comes back.
		const k = restoring?.edit;
		restoring = null;
		const was = k && messages.find((x) => x.id === k.id);
		if (k && was && canEdit(was)) startEdit(was, k.text, k.draft);
		for (const x of messages.slice(-TRANSLATE_HISTORY).reverse()) void translateOne(x);
	} else if (m.t === 'msg') {
		const msg = (m as { m: Msg }).m;
		if (!flat(msg)) return true;
		stoppedTyping(msg.key);
		if (msg.key !== you?.key) speak(msg.key, msg.login, msg.text);
		messages.push(msg);
		if (messages.length > KEEP) drop(new Set(messages.slice(0, messages.length - KEEP).map((x) => x.id)));
		feed.push(feedLine(msg, true));
		void translateOne(msg);
	} else if (m.t === 'typing') {
		const w = m as { key?: unknown; login?: unknown; avatar?: unknown };
		if (typeof w.key === 'string') typed({ key: w.key, login: typeof w.login === 'string' ? w.login : null, avatar: typeof w.avatar === 'string' ? w.avatar : null });
	} else if (m.t === 'unsay') {
		const u = m as { id?: unknown; parent?: unknown };
		if (typeof u.id !== 'number') return true;
		const id = u.id;
		// A message taken back takes its replies with it.
		drop(new Set([id, ...(u.parent === undefined ? messages.filter((x) => x.parent === id).map((x) => x.id) : [])]));
	} else if (m.t === 'edit') {
		const f = m as { id?: unknown; text?: unknown; edited?: unknown };
		const x = messages.find((y) => y.id === f.id);
		if (!x || typeof f.text !== 'string') return true;
		x.text = f.text;
		if (typeof f.edited === 'string') x.edited = f.edited;
		translations.delete(x.id);
		refeedId(x.id);
		void translateOne(x);
	} else if (m.t === 'error') {
		feed.push({ kind: 'error', face: icon(CROSS), who: 'Not sent to the chat', text: String((m as { text?: unknown }).text ?? 'Not sent.') });
	} else return false;
	return true;
}

export function save(): Kept {
	return {
		say: (editing ? editing.draft : ta.value) || undefined,
		sayOpen: feed.isOpen() || undefined,
		edit: editing ? { id: editing.id, text: ta.value, draft: editing.draft } : undefined,
	};
}

/** Called before setup: the input's text and state go back at setup, an edit once the socket brings the messages. */
export function restore(k: Kept): void {
	restoring = { ...k };
}
