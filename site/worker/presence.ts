/**
 * Who is on the site right now, and on which page: one Durable Object (`Presence`, named "site") holding a WebSocket
 * per open tab, with the hibernation API, so an idle room costs nothing between messages.
 *
 * GET /api/presence (a WebSocket upgrade, same Origin only) is checked here in the worker: the session cookie gives
 * the GitHub user, if any. The object gets the request with `x-presence-who` (the user, or nothing) and
 * `x-presence-ip`, and names an anonymous visitor by an HMAC of their IP and user agent under a random key it keeps
 * per UTC day: the same browser is the same animal all day, the next day it is another, and neither the IP nor the
 * key leaves the object. Signed-in readers are their GitHub login.
 *
 * A crawler or other automation (worker/bots.ts decides, from Cloudflare's verified-bot signals and the user agent) is
 * welcome but shows as what it is: `bot` set to its name ("Googlebot") on its entry in `here`, left out of `total` (and
 * counted in `bots` instead), and refused everything but `at`: no pointer, no live line, no typing, no chat.
 *
 * Protocol (JSON text frames):
 *
 *   server -> client  {t: "you", key, login, avatar, signIn, bot?}  once, on connect
 *                     {t: "here", people: [{key, login, avatar, bot?, pages: [{path, away}]}], total, bots}
 *                                                                  on every change: `total` the people, `bots` the bots
 *                     {t: "comments", doc}                      a comment on that doc was posted or deleted
 *                     {t: "chat", messages: [Msg], mine?}       the chat's last CHAT_SHOWN messages (top-level ones,
 *                                                                  each with its thread's summary, and replies also
 *                                                                  sent to the chat), once, on connect; for a
 *                                                                  signed-in reader `mine` too (below)
 *                     {t: "msg", m: Msg}                        a chat message said; a reply has `parent` set, and
 *                                                                  clients bump that message's summary themselves
 *                     {t: "edit", id, text, edited}             a message's new text, and when it changed (ISO)
 *                     {t: "react", id, reactions: [Reaction]}   message `id`'s reactions, all of them, after a change
 *                     {t: "unsay", id}                          a message taken back (a top-level one takes its
 *                                                                  replies with it)
 *                     {t: "unsay", id, parent, ...Summary}      a reply taken back, and its thread's new summary
 *                     {t: "thread", id, root, messages: [Msg]}  the replies to message `id`, oldest first (the last
 *                                                                  THREAD_SHOWN), and the message itself (null if gone)
 *                     {t: "typing", key, login, avatar}           someone signed in is writing (at most every TYPING_MS)
 *                     {t: "error", text}                        why the tab's last chat message was refused
 *                     {t: "c", k, c}                            someone else on this tab's page moved their pointer:
 *                                                                  `k` their key, `c` as they sent it, or null
 *                     {t: "say-live", k, text, done}            someone else on this tab's page is writing a line by
 *                                                                  their pointer (client/lib/cursors.ts): `text` as it
 *                                                                  stands, null when they gave up; `done` once sent
 *                     {t: "build", build, assets}               on connect, last, and to every socket when a
 *                                                                  deploy's code first runs (sockets outlive a
 *                                                                  deploy): the live build (gen/build.ts's
 *                                                                  build.json); client/lib/update.ts takes it in
 *   client -> server  {t: "at", path, away}                       the page the tab shows (a site path) and whether
 *                                                                  it is hidden; unknown paths count as no page
 *                     {t: "typing"}                            chat: the reader is writing a message
 *                     {t: "say", text, parent?, broadcast?}     chat: signed-in readers only; `parent` makes it a
 *                                                                  reply in that top-level message's thread, and
 *                                                                  `broadcast: true` shows the reply in the chat too
 *                     {t: "edit", id, text}                     chat: new text, for the author or a COMMENT_ADMINS
 *                                                                  id; the rules and the rate limit of `say`
 *                     {t: "react", id, kind, on}                chat: signed-in readers add (`on: true`) or take
 *                                                                  back their reaction `kind` (REACTIONS) on a message
 *                     {t: "unsay", id}                          chat: for the author or a COMMENT_ADMINS id
 *                     {t: "thread", id}                         chat: asks for that message's thread
 *                     {t: "c", c: [region, key, x, y] | null}  the tab's pointer, or null when it left the page or is
 *                                                                  on nothing the others have
 *                     {t: "say-live", text: string | null, done?}  the line the reader is writing by their pointer (any
 *                                                                  reader, signed in or not), at most CHAT_MAX; null to
 *                                                                  take it back; `done` when sent (a signed-in reader
 *                                                                  sends it to the chat too, as a `say`)
 *
 * `c` is what the pointer is on (client/lib/anchor.ts): `region` one of REGIONS ("m" the article), `key` the thing in
 * it (in the article `b<block>` or `b<block>.<character>`; elsewhere a short string of KEY's characters), and x and y
 * fractions of that thing's box. Anything else (a pointer from a tab older than this shape) is dropped. It goes only to
 * the other people's tabs on the same page that are not hidden, rebuilt from the checked values, never stored: nothing
 * is written, not even to the socket's attachment, so it costs a wake-up and nothing else. Each person gets MOVE_RATE a
 * second (MOVE_BURST at once); the rest is dropped. `say-live` goes the same way, on the same budget: passed on to the
 * page, never stored, gone when the tab is.
 *
 * Msg is {id, key, login, avatar, text, at, parent?, broadcast?, to?, edited?, reactions?}; a top-level one in `chat`
 * and `thread` frames also carries its Summary, {replies, last, repliers}: how many replies, when the last came (ISO),
 * and up to three of the latest distinct repliers ({key, login, avatar}). Threads are one level deep, like Slack's: a
 * reply to a reply is refused. A reply with `broadcast: true` was also sent to the chat, like Slack's "Also send to
 * channel": it stays a reply (counted in its thread, gone with it), and shows in the chat's list as well, with `to`,
 * its thread's message ({login, text}, the text cut to SNIPPET) for a "replied to a thread" line. `edited` is when the
 * text last changed (ISO). Reaction is {kind, who: [{key, login}]}, kinds in the order they first came, people too;
 * `reactions` is left out when there are none.
 *
 * `mine` is {[id]: [reply id]}: the threads the reader started or replied in, each with the ids of the replies others
 * wrote after the reader's own last one (at most MINE_IDS, oldest first). The client keeps which it has seen.
 *
 * The chat is everyone on the site, kept in the object's SQLite (the last CHAT_KEPT messages, replies included; a
 * message pruned takes its replies with it, and every message its reactions). Anyone can read it, only signed-in
 * readers can talk, at most RATE_SHORT per 15 s and RATE_LONG per hour each, replies and edits counted; reactions
 * have their own limit, REACT_SHORT and REACT_LONG.
 *
 * A visitor with several tabs is one person with several pages. Broadcasts are coalesced (BROADCAST_MS). The
 * comment routes (worker/comments.ts) call `announce` after each write, so open pages refresh their threads live.
 */
import { DurableObject } from 'cloudflare:workers';
import buildJson from '../.gen/build.json';
import pagesJson from '../.gen/pages.json';
import { currentUser, hasSessionCookie, isAdmin } from './auth';
import type { Env } from './env';
import { botName } from './bots';
import { json, plain, sameOrigin } from './http';

const PATHS = new Set<string>(pagesJson.map((p) => p.url));
/** This deploy's build, sent on every connect: the object runs the same bundle as the worker that serves the pages. */
const BUILD = JSON.stringify({ t: 'build', build: buildJson.hash, assets: buildJson.assets });
const BROADCAST_MS = 300;
const MAX_SOCKETS = 4000;
const MAX_TABS = 16;
/** The close code of a tab pushed out by a newer one of the same person's (MAX_TABS). */
const TOO_MANY_TABS = 4001;
const MAX_FRAME = 4096;
const CHAT_MAX = 500;
const CHAT_SHOWN = 80;
const CHAT_KEPT = 500;
const THREAD_SHOWN = 200;
const RATE_SHORT = 5;
const RATE_LONG = 120;
/** The reactions there are, by name: our own icons (client/lib/chat.ts), not emoji. */
const REACTIONS = new Set(['up', 'heart', 'laugh', 'eyes', 'check', 'fire']);
const REACT_SHORT = 20;
const REACT_LONG = 600;
/** How much of a thread's message a reply also sent to the chat carries (`to`). */
const SNIPPET = 160;
const MINE_IDS = 99;
const MAX_PEOPLE = 300;
const TYPING_MS = 2000;
const MOVE_RATE = 30;
const MOVE_BURST = 60;
/** What a pointer (`c`) may be on (client/lib/anchor.ts, which keeps the same): the regions, the article's keys, any
 * other region's, and the bound on the fractions, a little past 0 to 1. */
const REGIONS = new Set(['m', 'w', 't', 'c', 'o', 's']);
const BLOCK_KEY = /^b\d{1,6}(?:\.\d{1,7})?$/;
const KEY = /^[\w/.#:?=&%~+@,-]{0,160}$/;
const MAX_FRAC = 50;
/** A reload or a dropped connection shorter than this keeps someone's time on the site. */
const RETURN_MS = 120_000;

/** `bot` is the name of a crawler or other automation (worker/bots.ts), never set for a signed-in reader. */
type Who = { key: string; login: string | null; avatar: string | null; admin?: boolean; bot?: string };
type Reaction = { kind: string; who: { key: string; login: string }[] };
type Msg = {
	id: number;
	key: string;
	login: string;
	avatar: string | null;
	text: string;
	at: string;
	parent?: number;
	broadcast?: true;
	to?: { login: string; text: string };
	edited?: string;
	reactions?: Reaction[];
};
/** A chat row as SQLite has it (COLS); `msgOf` makes it a Msg. */
type Row = { id: number; key: string; login: string; avatar: string | null; text: string; at: string; parent: number | null; broadcast: number | null; edited_at: string | null };
const COLS = 'id, key, login, avatar, text, at, parent, broadcast, edited_at';
const msgOf = (r: Row): Msg => ({
	id: r.id,
	key: r.key,
	login: r.login,
	avatar: r.avatar,
	text: r.text,
	at: r.at,
	...(r.parent !== null && { parent: r.parent }),
	...(r.broadcast ? { broadcast: true as const } : {}),
	...(r.edited_at !== null && { edited: r.edited_at }),
});
const NO_REPLIES: Summary = { replies: 0, last: null, repliers: [] };
type Summary = { replies: number; last: string | null; repliers: Who[] };
/** Kept on each socket (`serializeAttachment`), so it survives hibernation. */
type Tab = Who & { path: string | null; away: boolean; country: string | null; device?: Device; seen?: number; since?: number };
/** What a tab runs on, from its user agent (an iPad asking for the desktop site counts as a computer). */
type Device = 'computer' | 'phone' | 'tablet';
/** `country` is Cloudflare's two-letter guess from the IP (`request.cf.country`), shown as a flag; null when unknown or Tor. */
type Person = Who & { country: string | null; since: number; devices: Device[]; pages: { path: string; away: boolean }[] };

function deviceOf(ua: string): Device {
	if (/iPad|Tablet|PlayBook|Silk|Android(?!.*Mobile)/i.test(ua)) return 'tablet';
	if (/iPhone|iPod|Android|Mobile|Windows Phone/i.test(ua)) return 'phone';
	return 'computer';
}

/** A chat message's text, trimmed, or null when it is empty or longer than CHAT_MAX (`say` and `edit`). */
function chatText(raw: unknown): string | null {
	const text = typeof raw === 'string' ? raw.trim() : '';
	return text.length < 1 || text.length > CHAT_MAX ? null : text;
}

/** The worker side of GET /api/presence. */
export async function handlePresence(request: Request, env: Env): Promise<Response> {
	if (request.method !== 'GET') return plain('Method not allowed.', 405, { allow: 'GET' });
	if (request.headers.get('upgrade')?.toLowerCase() !== 'websocket') return plain('Expected a WebSocket.', 426);
	if (!sameOrigin(request)) return plain('Cross-site request refused.', 403);
	const user = hasSessionCookie(request) ? await currentUser(request, env) : null;
	const headers = new Headers({ upgrade: 'websocket' });
	if (user) headers.set('x-presence-who', JSON.stringify({ key: `u${user.id}`, login: user.login, avatar: avatarOf(user.avatar_url), admin: isAdmin(env, user) }));
	headers.set('x-presence-ip', request.headers.get('cf-connecting-ip') ?? '');
	const country = String(request.cf?.country ?? '');
	headers.set('x-presence-country', /^[A-Z]{2}$/.test(country) && country !== 'XX' ? country : '');
	const ua = request.headers.get('user-agent') ?? '';
	headers.set('x-presence-ua', ua.slice(0, 512));
	// Decided here from what Cloudflare saw, never from the tab. A signed-in reader is a person.
	const bot = user ? null : botName(ua, request.cf);
	if (bot) headers.set('x-presence-bot', bot);
	headers.set('x-presence-signin', env.GITHUB_CLIENT_ID ? '1' : '');
	headers.set('x-presence-built', String(buildJson.at));
	const stub = env.PRESENCE.get(env.PRESENCE.idFromName('site'));
	return stub.fetch(new Request(request.url, { headers }));
}

/** Tells every open tab that `doc`'s comments changed. Best effort: a failure is logged, never the comment's. */
export async function announce(env: Env, doc: string): Promise<void> {
	try {
		const stub = env.PRESENCE.get(env.PRESENCE.idFromName('site'));
		const res = await stub.fetch('https://presence.internal/comments', { method: 'POST', body: JSON.stringify({ doc }) });
		if (!res.ok) console.warn('presence announce failed', res.status);
	} catch (e) {
		console.warn('presence announce failed', e instanceof Error ? e.message : String(e));
	}
}

/** A pointer's place as a tab sent it, checked (`relay`): its parts, rebuilt, or undefined when anything is off. */
function spotOf(raw: unknown): [string, string, number, number] | undefined {
	if (!Array.isArray(raw) || raw.length !== 4) return;
	const [region, key, x, y]: unknown[] = raw;
	if (typeof region !== 'string' || !REGIONS.has(region) || typeof key !== 'string') return;
	if (region === 'm' ? !BLOCK_KEY.test(key) : !KEY.test(key)) return;
	if (typeof x !== 'number' || typeof y !== 'number' || !(Math.abs(x) <= MAX_FRAC) || !(Math.abs(y) <= MAX_FRAC)) return;
	return [region, key, Math.round(x * 1000) / 1000, Math.round(y * 1000) / 1000];
}

const avatarOf = (url: string | null): string | null =>
	url && /^https:\/\/avatars\.githubusercontent\.com\/[\x21-\x7e]*$/.test(url) ? url : null;

export class Presence extends DurableObject<Env> {
	private pending: ReturnType<typeof setTimeout> | null = null;
	private salt: { day: string; key: CryptoKey } | null = null;
	/** Each talker's recent message times (ms), for the rate limit. Lost on hibernation, which only forgives. */
	private said = new Map<string, number[]>();
	/** The same for reactions. */
	private reacted = new Map<string, number[]>();
	/** When each person last said they were typing (not kept: a lost one only drops an indicator). */
	private typed = new Map<string, number>();
	/** When each person came, and when their last tab went (null while one is open). Lost on hibernation; then a tab's own `since` stands. */
	private arrived = new Map<string, { at: number; gone: number | null }>();
	/** Each person's budget for `c` and `say-live` frames (a token bucket). Lost on hibernation, which only forgives. */
	private moved = new Map<string, { at: number; n: number }>();
	/** The open tabs on each page, for `c` and `say-live`; rebuilt from the attachments after any tab moves, comes or goes. */
	private rooms: Map<string, { ws: WebSocket; key: string; away: boolean }[]> | null = null;

	constructor(ctx: DurableObjectState, env: Env) {
		super(ctx, env);
		ctx.storage.sql.exec(`CREATE TABLE IF NOT EXISTS chat (
			id INTEGER PRIMARY KEY AUTOINCREMENT, key TEXT NOT NULL, login TEXT NOT NULL, avatar TEXT,
			text TEXT NOT NULL, at TEXT NOT NULL)`);
		// Columns that came later (threads, then replies also sent to the chat, and edits): a database from before them
		// gets them here. Only the missing ones are added, so a failing ALTER is a real error and stops here.
		const have = new Set(ctx.storage.sql.exec<{ name: string }>('PRAGMA table_info(chat)').toArray().map((c) => c.name));
		for (const [name, type] of [['parent', 'INTEGER'], ['broadcast', 'INTEGER'], ['edited_at', 'TEXT']]) {
			if (!have.has(name)) ctx.storage.sql.exec(`ALTER TABLE chat ADD COLUMN ${name} ${type}`);
		}
		ctx.storage.sql.exec('CREATE INDEX IF NOT EXISTS chat_parent ON chat (parent)');
		// One row per person, reaction and message; `login` for the list of who reacted. A message's reactions go when it
		// does (deleted with it in `say`'s prune and in `unsay`).
		ctx.storage.sql.exec(`CREATE TABLE IF NOT EXISTS reactions (
			message_id INTEGER NOT NULL, key TEXT NOT NULL, kind TEXT NOT NULL, login TEXT NOT NULL,
			PRIMARY KEY (message_id, key, kind))`);
		// A deploy doesn't drop hibernated sockets: the new code wakes on the next message or connect with the old
		// build's tabs still attached. The first time it runs, it tells them.
		ctx.storage.sql.exec('CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL)');
		const last = ctx.storage.sql.exec<{ v: string }>("SELECT v FROM meta WHERE k = 'build'").toArray()[0]?.v;
		if (last !== BUILD) {
			ctx.storage.sql.exec("INSERT OR REPLACE INTO meta (k, v) VALUES ('build', ?)", BUILD);
			for (const ws of ctx.getWebSockets()) {
				try {
					ws.send(BUILD);
				} catch {
					// Closing: it reconnects and gets the build then.
				}
			}
		}
	}

	override async fetch(request: Request): Promise<Response> {
		if (request.method === 'POST' && new URL(request.url).pathname === '/comments') {
			const { doc } = await request.json<{ doc: string }>();
			this.broadcast(JSON.stringify({ t: 'comments', doc }));
			return new Response(null, { status: 204 });
		}
		// Cloudflare doesn't restart a Durable Object with open sockets on a deploy, while the worker in front of it runs
		// the new code at once: an object older than the worker that sent this resets itself, and the tab's retry (and
		// everyone's next message) meets the new code, whose constructor tells the open tabs.
		if (Number(request.headers.get('x-presence-built')) > buildJson.at) this.ctx.abort('A newer deploy.');
		if (this.ctx.getWebSockets().length >= MAX_SOCKETS) return json({ error: 'Too many visitors right now.' }, 503);
		const whoHeader = request.headers.get('x-presence-who');
		const bot = request.headers.get('x-presence-bot')?.slice(0, 64);
		const who: Who = whoHeader
			? (JSON.parse(whoHeader) as Who)
			: {
				key: await this.anonKey(request.headers.get('x-presence-ip') ?? '', request.headers.get('x-presence-ua') ?? ''),
				login: null,
				avatar: null,
				...(bot ? { bot } : {}),
			};
		// One person's tabs past MAX_TABS: the newest stays and the one used longest ago goes (a hidden one first). It
		// closes with TOO_MANY_TABS, and reconnects only when it is shown again (client/lib/presence.ts).
		const mine = this.ctx.getWebSockets(who.key);
		const rank = (ws: WebSocket) => {
			const t = ws.deserializeAttachment() as Tab | null;
			return (t?.away === false ? 1e15 : 0) + (t?.seen ?? t?.since ?? 0);
		};
		for (const ws of mine.sort((a, b) => rank(a) - rank(b)).slice(0, Math.max(0, mine.length - MAX_TABS + 1))) {
			try {
				ws.close(TOO_MANY_TABS, 'Too many tabs.');
			} catch {
				// Already closed.
			}
		}

		const { 0: client, 1: server } = new WebSocketPair();
		this.ctx.acceptWebSocket(server, [who.key]);
		const now = Date.now();
		const before = this.arrived.get(who.key);
		const since = before && (before.gone === null || now - before.gone < RETURN_MS) ? before.at : now;
		this.arrived.set(who.key, { at: since, gone: null });
		const tab: Tab = { ...who, path: null, away: false, since, country: request.headers.get('x-presence-country') || null, device: deviceOf(request.headers.get('x-presence-ua') ?? '') };
		server.serializeAttachment(tab);
		server.send(JSON.stringify({ t: 'you', key: who.key, login: who.login, avatar: who.avatar, admin: !!who.admin, signIn: request.headers.get('x-presence-signin') === '1', ...(who.bot ? { bot: who.bot } : {}) }));
		server.send(this.snapshot());
		server.send(JSON.stringify({ t: 'chat', messages: this.history(), ...(who.login && { mine: this.mine(who.key) }) }));
		server.send(BUILD);
		return new Response(null, { status: 101, webSocket: client });
	}

	override webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): void {
		if (typeof message !== 'string' || message.length > MAX_FRAME) return ws.close(1009, 'Too big.');
		let m: {
			t?: unknown;
			path?: unknown;
			away?: unknown;
			text?: unknown;
			done?: unknown;
			id?: unknown;
			parent?: unknown;
			broadcast?: unknown;
			kind?: unknown;
			on?: unknown;
			c?: unknown;
		};
		try {
			m = JSON.parse(message) as typeof m;
		} catch {
			return ws.close(1003, 'Not JSON.');
		}
		const tab = ws.deserializeAttachment() as Tab;
		// A bot is shown where it is, and that is all: no pointer, no live line, no typing, no chat.
		if (tab.bot && m.t !== 'at') {
			if (m.t === 'say' || m.t === 'edit' || m.t === 'react' || m.t === 'unsay') ws.send(JSON.stringify({ t: 'error', text: 'Bots can read, not chat.' }));
			return;
		}
		if (m.t === 'c' || m.t === 'k') return this.relay(tab, m.c, m.t);
		if (m.t === 'say-live') return this.sayLive(tab, m.text, m.done);
		if (m.t === 'typing') {
			const now = Date.now();
			if (!tab.login || now - (this.typed.get(tab.key) ?? 0) < TYPING_MS) return;
			this.typed.set(tab.key, now);
			return this.broadcast(JSON.stringify({ t: 'typing', key: tab.key, login: tab.login, avatar: tab.avatar }));
		}
		if (m.t === 'say') return this.say(ws, tab, m.text, m.parent, m.broadcast);
		if (m.t === 'edit') return this.edit(ws, tab, m.id, m.text);
		if (m.t === 'react') return this.react(ws, tab, m.id, m.kind, m.on);
		if (m.t === 'unsay') return this.unsay(ws, tab, m.id);
		if (m.t === 'thread') return this.thread(ws, m.id);
		if (m.t !== 'at') return;
		const path = typeof m.path === 'string' && PATHS.has(m.path) ? m.path : null;
		const away = m.away === true;
		if (tab.path === path && tab.away === away) return;
		ws.serializeAttachment({ ...tab, path, away, seen: Date.now() } satisfies Tab);
		this.rooms = null;
		this.schedule();
	}

	override webSocketClose(ws: WebSocket, code: number): void {
		try {
			ws.close(code === 1005 || code === 1006 ? 1000 : code, 'Bye.');
		} catch {
			// Already closed.
		}
		this.rooms = null;
		this.schedule();
	}

	override webSocketError(): void {
		this.rooms = null;
		this.schedule();
	}

	/**
	 * Passes a pointer (`c`) or a press (`k`, where the reader clicked) on to the other people on the tab's page, checked
	 * and rebuilt; a pointer may be null (gone), a press may not. Anything else is dropped.
	 */
	private relay(tab: Tab, raw: unknown, t: 'c' | 'k'): void {
		if (!tab.path) return;
		const c = raw === null && t === 'c' ? null : spotOf(raw);
		if (c === undefined || !this.allow(tab.key)) return;
		const out = JSON.stringify({ t, k: tab.key, c });
		for (const r of this.room(tab.path)) {
			if (r.key === tab.key || r.away) continue;
			try {
				r.ws.send(out);
			} catch {
				// Closing.
			}
		}
	}

	/**
	 * Passes the line a reader is writing by their pointer on to the others on their page, as `relay` does a pointer
	 * (not to hidden tabs: it is gone by the time they look). One line, at most CHAT_MAX; empty is the same as null.
	 */
	private sayLive(tab: Tab, raw: unknown, done: unknown): void {
		if (!tab.path || (raw !== null && typeof raw !== 'string')) return;
		const text = raw === null ? null : raw.replace(/[\r\n\t]+/g, ' ').trim() || null;
		if ((text?.length ?? 0) > CHAT_MAX || !this.allow(tab.key)) return;
		const out = JSON.stringify({ t: 'say-live', k: tab.key, text, done: done === true && text !== null });
		for (const r of this.room(tab.path)) {
			if (r.key === tab.key || r.away) continue;
			try {
				r.ws.send(out);
			} catch {
				// Closing.
			}
		}
	}

	/** Takes one of `key`'s MOVE_BURST tokens, which come back at MOVE_RATE a second; false when none is left. */
	private allow(key: string): boolean {
		const now = Date.now();
		const b = this.moved.get(key) ?? { at: now, n: MOVE_BURST };
		b.n = Math.min(MOVE_BURST, b.n + ((now - b.at) / 1000) * MOVE_RATE);
		b.at = now;
		this.moved.set(key, b);
		if (b.n < 1) return false;
		b.n--;
		return true;
	}

	private room(path: string): { ws: WebSocket; key: string; away: boolean }[] {
		if (!this.rooms) {
			this.rooms = new Map();
			for (const ws of this.open()) {
				const tab = ws.deserializeAttachment() as Tab | null;
				if (!tab?.path) continue;
				const l = this.rooms.get(tab.path) ?? [];
				l.push({ ws, key: tab.key, away: tab.away });
				this.rooms.set(tab.path, l);
			}
		}
		return this.rooms.get(path) ?? [];
	}

	private say(ws: WebSocket, tab: Tab, raw: unknown, rawParent: unknown, rawBroadcast: unknown): void {
		const refuse = (text: string) => ws.send(JSON.stringify({ t: 'error', text }));
		if (!tab.login) return refuse('Sign in with GitHub to chat.');
		const text = chatText(raw);
		if (text === null) return refuse(`A message is 1 to ${CHAT_MAX} characters.`);
		let parent: number | null = null;
		let to: Msg['to'];
		if (rawParent !== undefined && rawParent !== null) {
			if (typeof rawParent !== 'number' || !Number.isSafeInteger(rawParent)) return refuse('No such thread.');
			const p = this.ctx.storage.sql
				.exec<{ parent: number | null; login: string; text: string }>('SELECT parent, login, substr(text, 1, ?) AS text FROM chat WHERE id = ?', SNIPPET, rawParent)
				.toArray()[0];
			if (!p) return refuse('That message is gone.');
			// One level, like Slack: a reply goes in the thread of a top-level message, never under another reply.
			if (p.parent !== null) return refuse('Reply in the thread, not to a reply.');
			parent = rawParent;
			to = { login: p.login, text: p.text };
		}
		// Only a reply can also go to the chat: a top-level message is there already.
		const broadcast = rawBroadcast === true;
		if (broadcast && parent === null) return refuse('Only a reply can also be sent to the chat.');
		if (!this.limit(this.said, tab.key, RATE_SHORT, RATE_LONG)) return refuse('Slow down a little.');
		this.typed.delete(tab.key);
		const at = new Date().toISOString();
		const row = this.ctx.storage.sql
			.exec<{ id: number }>(
				'INSERT INTO chat (key, login, avatar, text, at, parent, broadcast) VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id',
				tab.key,
				tab.login,
				tab.avatar,
				text,
				at,
				parent,
				broadcast ? 1 : null,
			)
			.one();
		// Replies are always newer than their message, so a reply pruned by id never leaves its message behind; the
		// second half takes the replies of messages pruned, wherever they are, so no reply is left without one.
		const cutoff = row.id - CHAT_KEPT;
		this.ctx.storage.sql.exec(
			'DELETE FROM reactions WHERE message_id <= ? OR message_id IN (SELECT id FROM chat WHERE parent <= ?)',
			cutoff,
			cutoff,
		);
		this.ctx.storage.sql.exec('DELETE FROM chat WHERE id <= ? OR parent <= ?', cutoff, cutoff);
		if (parent !== null && parent <= cutoff) return refuse('That thread is too old for replies.');
		const m: Msg = { id: row.id, key: tab.key, login: tab.login, avatar: tab.avatar, text, at, ...(parent !== null && { parent }), ...(broadcast && { broadcast: true as const, to }) };
		this.broadcast(JSON.stringify({ t: 'msg', m }));
	}

	/** Takes one of `key`'s `short` a 15 s and `long` an hour in `times`; false when none is left. */
	private limit(times: Map<string, number[]>, key: string, short: number, long: number): boolean {
		const now = Date.now();
		const mine = (times.get(key) ?? []).filter((t) => now - t < 3_600_000);
		if (mine.filter((t) => now - t < 15_000).length >= short || mine.length >= long) return false;
		mine.push(now);
		times.set(key, mine);
		return true;
	}

	/** New text for a message, by its author or an admin: the rules of `say`, and its rate limit. */
	private edit(ws: WebSocket, tab: Tab, id: unknown, raw: unknown): void {
		const refuse = (text: string) => ws.send(JSON.stringify({ t: 'error', text }));
		if (!tab.login) return refuse('Sign in with GitHub to chat.');
		if (typeof id !== 'number' || !Number.isSafeInteger(id)) return;
		const text = chatText(raw);
		if (text === null) return refuse(`A message is 1 to ${CHAT_MAX} characters.`);
		const row = this.ctx.storage.sql.exec<{ key: string; text: string }>('SELECT key, text FROM chat WHERE id = ?', id).toArray()[0];
		if (!row) return refuse('That message is gone.');
		if (row.key !== tab.key && !tab.admin) return refuse('Not your message.');
		if (row.text === text) return;
		if (!this.limit(this.said, tab.key, RATE_SHORT, RATE_LONG)) return refuse('Slow down a little.');
		const edited = new Date().toISOString();
		this.ctx.storage.sql.exec('UPDATE chat SET text = ?, edited_at = ? WHERE id = ?', text, edited, id);
		this.broadcast(JSON.stringify({ t: 'edit', id, text, edited }));
	}

	/** Adds or takes back the reader's reaction `kind` on message `id`, and tells everyone the message's reactions. */
	private react(ws: WebSocket, tab: Tab, id: unknown, kind: unknown, on: unknown): void {
		const refuse = (text: string) => ws.send(JSON.stringify({ t: 'error', text }));
		if (!tab.login) return refuse('Sign in with GitHub to react.');
		if (typeof id !== 'number' || !Number.isSafeInteger(id) || typeof kind !== 'string' || !REACTIONS.has(kind) || typeof on !== 'boolean') return;
		if (!this.ctx.storage.sql.exec('SELECT 1 FROM chat WHERE id = ?', id).toArray().length) return refuse('That message is gone.');
		if (!this.limit(this.reacted, tab.key, REACT_SHORT, REACT_LONG)) return refuse('Slow down a little.');
		const done = on
			? this.ctx.storage.sql.exec('INSERT OR IGNORE INTO reactions (message_id, key, kind, login) VALUES (?, ?, ?, ?)', id, tab.key, kind, tab.login)
			: this.ctx.storage.sql.exec('DELETE FROM reactions WHERE message_id = ? AND key = ? AND kind = ?', id, tab.key, kind);
		if (!done.rowsWritten) return;
		this.broadcast(JSON.stringify({ t: 'react', id, reactions: this.reactionsOf([id]).get(id) ?? [] }));
	}

	/** The reactions on `ids`, by message: kinds in the order they first came, and who, in the order they came. */
	private reactionsOf(ids: number[]): Map<number, Reaction[]> {
		const out = new Map<number, Reaction[]>();
		if (!ids.length) return out;
		const want = new Set(ids);
		// A range, not `IN (...)`: a query takes at most 100 bound values.
		const rows = this.ctx.storage.sql
			.exec<{ message_id: number; key: string; login: string; kind: string }>(
				'SELECT message_id, key, login, kind FROM reactions WHERE message_id BETWEEN ? AND ? ORDER BY rowid',
				Math.min(...ids),
				Math.max(...ids),
			)
			.toArray();
		for (const r of rows) {
			if (!want.has(r.message_id)) continue;
			const l = out.get(r.message_id) ?? [];
			out.set(r.message_id, l);
			let k = l.find((x) => x.kind === r.kind);
			if (!k) l.push((k = { kind: r.kind, who: [] }));
			k.who.push({ key: r.key, login: r.login });
		}
		return out;
	}

	/** `ms` with their reactions. */
	private withReactions<M extends Msg>(ms: M[]): M[] {
		const r = this.reactionsOf(ms.map((m) => m.id));
		return ms.map((m) => (r.has(m.id) ? { ...m, reactions: r.get(m.id) } : m));
	}

	/** The threads `key` started or replied in, each with the ids of others' replies after `key`'s own last one. */
	private mine(key: string): Record<number, number[]> {
		const out: Record<number, number[]> = {};
		for (const r of this.ctx.storage.sql.exec<{ id: number }>('SELECT id FROM chat WHERE parent IS NULL AND key = ?', key)) out[r.id] = [];
		// Oldest first: the reader's own reply starts the count again; a thread they only joined later counts from there.
		for (const r of this.ctx.storage.sql.exec<{ id: number; parent: number; key: string }>('SELECT id, parent, key FROM chat WHERE parent IS NOT NULL ORDER BY id')) {
			if (r.key === key) out[r.parent] = [];
			else out[r.parent]?.push(r.id);
		}
		for (const id in out) out[id] = out[id].slice(-MINE_IDS);
		return out;
	}

	private unsay(ws: WebSocket, tab: Tab, id: unknown): void {
		if (typeof id !== 'number' || !Number.isSafeInteger(id) || !tab.login) return;
		const row = this.ctx.storage.sql.exec<{ key: string; parent: number | null }>('SELECT key, parent FROM chat WHERE id = ?', id).toArray()[0];
		if (!row) return;
		if (row.key !== tab.key && !tab.admin) return ws.send(JSON.stringify({ t: 'error', text: 'Not your message.' }));
		// A top-level message takes its thread with it.
		this.ctx.storage.sql.exec('DELETE FROM reactions WHERE message_id = ? OR message_id IN (SELECT id FROM chat WHERE parent = ?)', id, id);
		this.ctx.storage.sql.exec('DELETE FROM chat WHERE id = ? OR parent = ?', id, id);
		if (row.parent === null) return this.broadcast(JSON.stringify({ t: 'unsay', id }));
		const s = this.summaries([row.parent]).get(row.parent) ?? { replies: 0, last: null, repliers: [] };
		this.broadcast(JSON.stringify({ t: 'unsay', id, parent: row.parent, ...s }));
	}

	/** Sends one tab the replies to `id`, oldest first, and the message itself. */
	private thread(ws: WebSocket, id: unknown): void {
		if (typeof id !== 'number' || !Number.isSafeInteger(id)) return;
		const root = this.ctx.storage.sql.exec<Row>(`SELECT ${COLS} FROM chat WHERE id = ? AND parent IS NULL`, id).toArray()[0];
		const replies = root
			? this.ctx.storage.sql
					.exec<Row>(`SELECT ${COLS} FROM chat WHERE parent = ? ORDER BY id DESC LIMIT ?`, id, THREAD_SHOWN)
					.toArray()
					.reverse()
					.map(msgOf)
			: [];
		const [top, ...messages] = this.withReactions([...(root ? [msgOf(root)] : []), ...replies]);
		const s = root ? this.summaries([id]).get(id) : undefined;
		ws.send(JSON.stringify({ t: 'thread', id, root: root ? { ...top, ...(s ?? NO_REPLIES) } : null, messages: root ? messages : [] }));
	}

	/**
	 * The last CHAT_SHOWN messages of the chat's list, oldest first: the top-level ones, each with its thread's summary,
	 * and the replies also sent to the chat, each with its thread's message (`to`).
	 */
	private history(): Msg[] {
		const shown = this.withReactions(
			this.ctx.storage.sql
				.exec<Row>(`SELECT ${COLS} FROM chat WHERE parent IS NULL OR broadcast = 1 ORDER BY id DESC LIMIT ?`, CHAT_SHOWN)
				.toArray()
				.reverse()
				.map(msgOf),
		);
		const s = this.summaries(shown.filter((m) => m.parent === undefined).map((m) => m.id));
		const parents = shown.flatMap((m) => (m.parent === undefined ? [] : [m.parent]));
		const to = new Map<number, { login: string; text: string }>();
		if (parents.length) {
			for (const r of this.ctx.storage.sql.exec<{ id: number; login: string; text: string }>(
				'SELECT id, login, substr(text, 1, ?) AS text FROM chat WHERE parent IS NULL AND id BETWEEN ? AND ?',
				SNIPPET,
				Math.min(...parents),
				Math.max(...parents),
			))
				to.set(r.id, { login: r.login, text: r.text });
		}
		return shown.map((m) => (m.parent === undefined ? { ...m, ...(s.get(m.id) ?? NO_REPLIES) } : { ...m, to: to.get(m.parent) }));
	}

	/** The thread summaries of `ids` (top-level messages), only those with replies. The table is at most CHAT_KEPT rows. */
	private summaries(ids: number[]): Map<number, Summary> {
		const out = new Map<number, Summary>();
		if (!ids.length) return out;
		const want = new Set(ids);
		const rows = this.ctx.storage.sql
			.exec<{ parent: number; key: string; login: string; avatar: string | null; at: string }>(
				'SELECT parent, key, login, avatar, at FROM chat WHERE parent >= ? ORDER BY id DESC',
				Math.min(...ids),
			)
			.toArray();
		for (const r of rows) {
			if (!want.has(r.parent)) continue;
			let s = out.get(r.parent);
			// Newest first, so the first row a thread meets is its last reply.
			if (!s) out.set(r.parent, (s = { replies: 0, last: r.at, repliers: [] }));
			s.replies++;
			if (s.repliers.length < 3 && !s.repliers.some((w) => w.key === r.key)) s.repliers.push({ key: r.key, login: r.login, avatar: r.avatar });
		}
		return out;
	}

	private broadcast(out: string): void {
		for (const ws of this.open()) {
			try {
				ws.send(out);
			} catch {
				// Closing.
			}
		}
	}

	private schedule(): void {
		this.pending ??= setTimeout(() => {
			this.pending = null;
			this.broadcast(this.snapshot());
		}, BROADCAST_MS);
	}

	private open(): WebSocket[] {
		return this.ctx.getWebSockets().filter((ws) => ws.readyState === WebSocket.OPEN);
	}

	/** Everyone, one entry per person (their tabs' pages), signed-in readers first, then by key. */
	private snapshot(): string {
		const people = new Map<string, Person>();
		const seen = new Map<Person['pages'][number], number>();
		for (const ws of this.open()) {
			const tab = ws.deserializeAttachment() as Tab | null;
			if (!tab) continue;
			let p = people.get(tab.key);
			if (!p) {
				p = { key: tab.key, login: tab.login, avatar: tab.avatar, ...(tab.bot ? { bot: tab.bot } : {}), country: tab.country ?? null, since: tab.since ?? Date.now(), devices: [], pages: [] };
				people.set(tab.key, p);
			}
			if (tab.since && tab.since < p.since) p.since = tab.since;
			if (tab.device && !p.devices.includes(tab.device)) p.devices.push(tab.device);
			if (!tab.path) continue;
			const same = p.pages.find((x) => x.path === tab.path && x.away === tab.away);
			if (same) seen.set(same, Math.max(seen.get(same) ?? 0, tab.seen ?? 0));
			else {
				const pg = { path: tab.path, away: tab.away };
				p.pages.push(pg);
				seen.set(pg, tab.seen ?? 0);
			}
		}
		const now = Date.now();
		for (const [key, a] of this.arrived) {
			if (people.has(key)) a.gone = null;
			else if (a.gone === null) a.gone = now;
			else if (now - a.gone > RETURN_MS) this.arrived.delete(key);
		}
		for (const key of this.moved.keys()) if (!people.has(key)) this.moved.delete(key);
		// Each person's pages, the one in front of them first: in view before hidden, then the latest moved to.
		for (const p of people.values()) p.pages.sort((a, b) => Number(a.away) - Number(b.away) || (seen.get(b) ?? 0) - (seen.get(a) ?? 0));
		// Signed-in readers, then anonymous ones, then bots, so a crowd of crawlers never pushes a person off the list.
		const rank = (p: Person) => (p.login ? 0 : p.bot ? 2 : 1);
		const list = [...people.values()].sort((a, b) => rank(a) - rank(b) || a.key.localeCompare(b.key)).slice(0, MAX_PEOPLE);
		let bots = 0;
		for (const p of people.values()) if (p.bot) bots++;
		return JSON.stringify({ t: 'here', people: list, total: people.size - bots, bots });
	}

	/** `a` + 12 hex chars of HMAC-SHA256(today's key, ip \0 user agent). The key is random, made once per UTC day. */
	private async anonKey(ip: string, ua: string): Promise<string> {
		const day = new Date().toISOString().slice(0, 10);
		if (this.salt?.day !== day) {
			let raw = await this.ctx.storage.get<ArrayBuffer>(`salt:${day}`);
			if (!raw) {
				raw = crypto.getRandomValues(new Uint8Array(32)).buffer;
				await this.ctx.storage.put(`salt:${day}`, raw);
				// Yesterday's key is no longer needed: dropping it makes yesterday's names unlinkable to the IPs.
				for (const k of (await this.ctx.storage.list({ prefix: 'salt:' })).keys()) if (k !== `salt:${day}`) await this.ctx.storage.delete(k);
			}
			this.salt = { day, key: await crypto.subtle.importKey('raw', raw, { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']) };
		}
		const mac = await crypto.subtle.sign('HMAC', this.salt.key, new TextEncoder().encode(`${ip}\0${ua}`));
		return `a${[...new Uint8Array(mac).slice(0, 6)].map((b) => b.toString(16).padStart(2, '0')).join('')}`;
	}
}
