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
 * Protocol (JSON text frames):
 *
 *   server -> client  {t: "you", key, login, avatar, signIn}       once, on connect
 *                     {t: "here", people: [{key, login, avatar, pages: [{path, away}]}]}   on every change
 *                     {t: "comments", doc}                      a comment on that doc was posted or deleted
 *                     {t: "chat", messages: [Msg]}              the chat's last CHAT_SHOWN top-level messages, each
 *                                                                  with its thread's summary, once, on connect
 *                     {t: "msg", m: Msg}                        a chat message said; a reply has `parent` set, and
 *                                                                  clients bump that message's summary themselves
 *                     {t: "unsay", id}                          a message taken back (a top-level one takes its
 *                                                                  replies with it)
 *                     {t: "unsay", id, parent, ...Summary}      a reply taken back, and its thread's new summary
 *                     {t: "thread", id, root, messages: [Msg]}  the replies to message `id`, oldest first (the last
 *                                                                  THREAD_SHOWN), and the message itself (null if gone)
 *                     {t: "typing", key, login, avatar}           someone signed in is writing (at most every TYPING_MS)
 *                     {t: "error", text}                        why the tab's last chat message was refused
 *                     {t: "c", k, c}                            someone else on this tab's page moved their pointer:
 *                                                                  `k` their key, `c` as they sent it, or null
 *                     {t: "v", k, v}                            the same for the part of the page they are reading
 *                     {t: "build", build, assets}               on connect, last, and to every socket when a
 *                                                                  deploy's code first runs (sockets outlive a
 *                                                                  deploy): the live build (gen/build.ts's
 *                                                                  build.json); client/lib/update.ts takes it in
 *   client -> server  {t: "at", path, away}                       the page the tab shows (a site path) and whether
 *                                                                  it is hidden; unknown paths count as no page
 *                     {t: "typing"}                            chat: the reader is writing a message
 *                     {t: "say", text, parent?}                 chat: signed-in readers only; `parent` makes it a
 *                                                                  reply in that top-level message's thread
 *                     {t: "unsay", id}                          chat: for the author or a COMMENT_ADMINS id
 *                     {t: "thread", id}                         chat: asks for that message's thread
 *                     {t: "c", c: [x, block, f] | null}         the tab's pointer, or null when it left the page
 *                     {t: "v", v: [block, f, block, f] | null}  the top and bottom of the tab's viewport
 *
 * `c` and `v` are positions in the page's text (client/lib/cursors.ts): x a fraction of the article's width, a block
 * index and a fraction of that block's height. They go only to the other people's tabs on the same page (pointers
 * not to hidden tabs), rebuilt from the numbers, never stored: nothing is written, not even to the socket's
 * attachment, so they cost a wake-up and nothing else. Each person gets MOVE_RATE a second (MOVE_BURST at once),
 * pointers and viewports together; the rest is dropped.
 *
 * Msg is {id, key, login, avatar, text, at, parent?}; a top-level one in `chat` and `thread` frames also carries its
 * Summary, {replies, last, repliers}: how many replies, when the last came (ISO), and up to three of the latest
 * distinct repliers ({key, login, avatar}). Threads are one level deep, like Slack's: a reply to a reply is refused.
 * The chat is everyone on the site, kept in the object's SQLite (the last CHAT_KEPT messages, replies included; a
 * message pruned takes its replies with it). Anyone can read it, only signed-in readers can talk, at most RATE_SHORT
 * per 15 s and RATE_LONG per hour each, replies counted.
 *
 * A visitor with several tabs is one person with several pages. Broadcasts are coalesced (BROADCAST_MS). The
 * comment routes (worker/comments.ts) call `announce` after each write, so open pages refresh their threads live.
 */
import { DurableObject } from 'cloudflare:workers';
import buildJson from '../.gen/build.json';
import pagesJson from '../.gen/pages.json';
import { currentUser, hasSessionCookie, isAdmin } from './auth';
import type { Env } from './env';
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
const MAX_PEOPLE = 300;
const TYPING_MS = 2000;
const MOVE_RATE = 30;
const MOVE_BURST = 60;
/** Bounds on the numbers in `c` and `v`: blocks are indexes into a page, the rest fractions, a little past 0 to 1. */
const MAX_BLOCK = 100_000;
const MAX_FRAC = 50;
/** A reload or a dropped connection shorter than this keeps someone's time on the site. */
const RETURN_MS = 120_000;

type Who = { key: string; login: string | null; avatar: string | null; admin?: boolean };
type Msg = { id: number; key: string; login: string; avatar: string | null; text: string; at: string; parent?: number };
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
	headers.set('x-presence-ua', (request.headers.get('user-agent') ?? '').slice(0, 512));
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

const avatarOf = (url: string | null): string | null =>
	url && /^https:\/\/avatars\.githubusercontent\.com\/[\x21-\x7e]*$/.test(url) ? url : null;

export class Presence extends DurableObject<Env> {
	private pending: ReturnType<typeof setTimeout> | null = null;
	private salt: { day: string; key: CryptoKey } | null = null;
	/** Each talker's recent message times (ms), for the rate limit. Lost on hibernation, which only forgives. */
	private said = new Map<string, number[]>();
	/** When each person last said they were typing (not kept: a lost one only drops an indicator). */
	private typed = new Map<string, number>();
	/** When each person came, and when their last tab went (null while one is open). Lost on hibernation; then a tab's own `since` stands. */
	private arrived = new Map<string, { at: number; gone: number | null }>();
	/** Each person's budget for `c` and `v` frames (a token bucket). Lost on hibernation, which only forgives. */
	private moved = new Map<string, { at: number; n: number }>();
	/** The open tabs on each page, for `c` and `v`; rebuilt from the attachments after any tab moves, comes or goes. */
	private rooms: Map<string, { ws: WebSocket; key: string; away: boolean }[]> | null = null;

	constructor(ctx: DurableObjectState, env: Env) {
		super(ctx, env);
		ctx.storage.sql.exec(`CREATE TABLE IF NOT EXISTS chat (
			id INTEGER PRIMARY KEY AUTOINCREMENT, key TEXT NOT NULL, login TEXT NOT NULL, avatar TEXT,
			text TEXT NOT NULL, at TEXT NOT NULL)`);
		// Threads came later: a database from before them gets the column here (and has it already the next time).
		try {
			ctx.storage.sql.exec('ALTER TABLE chat ADD COLUMN parent INTEGER');
		} catch {
			// The column is there.
		}
		ctx.storage.sql.exec('CREATE INDEX IF NOT EXISTS chat_parent ON chat (parent)');
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
		const who: Who = whoHeader
			? (JSON.parse(whoHeader) as Who)
			: { key: await this.anonKey(request.headers.get('x-presence-ip') ?? '', request.headers.get('x-presence-ua') ?? ''), login: null, avatar: null };
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
		server.send(JSON.stringify({ t: 'you', key: who.key, login: who.login, avatar: who.avatar, admin: !!who.admin, signIn: request.headers.get('x-presence-signin') === '1' }));
		server.send(this.snapshot());
		server.send(JSON.stringify({ t: 'chat', messages: this.history() }));
		server.send(BUILD);
		return new Response(null, { status: 101, webSocket: client });
	}

	override webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): void {
		if (typeof message !== 'string' || message.length > MAX_FRAME) return ws.close(1009, 'Too big.');
		let m: { t?: unknown; path?: unknown; away?: unknown; text?: unknown; id?: unknown; parent?: unknown; c?: unknown; v?: unknown };
		try {
			m = JSON.parse(message) as typeof m;
		} catch {
			return ws.close(1003, 'Not JSON.');
		}
		const tab = ws.deserializeAttachment() as Tab;
		if (m.t === 'c') return this.relay(tab, 'c', m.c, 3);
		if (m.t === 'v') return this.relay(tab, 'v', m.v, 4);
		if (m.t === 'typing') {
			const now = Date.now();
			if (!tab.login || now - (this.typed.get(tab.key) ?? 0) < TYPING_MS) return;
			this.typed.set(tab.key, now);
			return this.broadcast(JSON.stringify({ t: 'typing', key: tab.key, login: tab.login, avatar: tab.avatar }));
		}
		if (m.t === 'say') return this.say(ws, tab, m.text, m.parent);
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
	 * Passes a pointer (`c`) or viewport (`v`) on to the other people on the tab's page: `n` numbers, every other one
	 * a block index (for `c` the second; for `v` the first and third), or null. Anything else is dropped.
	 */
	private relay(tab: Tab, t: 'c' | 'v', raw: unknown, n: number): void {
		if (!tab.path) return;
		let at: number[] | null = null;
		if (raw !== null) {
			if (!Array.isArray(raw) || raw.length !== n) return;
			at = [];
			for (let i = 0; i < n; i++) {
				const x: unknown = raw[i];
				if (typeof x !== 'number' || !Number.isFinite(x)) return;
				const block = t === 'c' ? i === 1 : i % 2 === 0;
				if (block ? !Number.isInteger(x) || x < 0 || x > MAX_BLOCK : Math.abs(x) > MAX_FRAC) return;
				at.push(block ? x : Math.round(x * 1000) / 1000);
			}
		}
		if (!this.allow(tab.key)) return;
		const out = JSON.stringify({ t, k: tab.key, [t]: at });
		for (const r of this.room(tab.path)) {
			if (r.key === tab.key || (t === 'c' && r.away)) continue;
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

	private say(ws: WebSocket, tab: Tab, raw: unknown, rawParent: unknown): void {
		const refuse = (text: string) => ws.send(JSON.stringify({ t: 'error', text }));
		if (!tab.login) return refuse('Sign in with GitHub to chat.');
		const text = typeof raw === 'string' ? raw.trim() : '';
		if (text.length < 1 || text.length > CHAT_MAX) return refuse(`A message is 1 to ${CHAT_MAX} characters.`);
		let parent: number | null = null;
		if (rawParent !== undefined && rawParent !== null) {
			if (typeof rawParent !== 'number' || !Number.isSafeInteger(rawParent)) return refuse('No such thread.');
			const p = this.ctx.storage.sql.exec<{ parent: number | null }>('SELECT parent FROM chat WHERE id = ?', rawParent).toArray()[0];
			if (!p) return refuse('That message is gone.');
			// One level, like Slack: a reply goes in the thread of a top-level message, never under another reply.
			if (p.parent !== null) return refuse('Reply in the thread, not to a reply.');
			parent = rawParent;
		}
		const now = Date.now();
		const times = (this.said.get(tab.key) ?? []).filter((t) => now - t < 3_600_000);
		if (times.filter((t) => now - t < 15_000).length >= RATE_SHORT || times.length >= RATE_LONG) return refuse('Slow down a little.');
		times.push(now);
		this.said.set(tab.key, times);
		this.typed.delete(tab.key);
		const at = new Date(now).toISOString();
		const row = this.ctx.storage.sql
			.exec<{ id: number }>(
				'INSERT INTO chat (key, login, avatar, text, at, parent) VALUES (?, ?, ?, ?, ?, ?) RETURNING id',
				tab.key,
				tab.login,
				tab.avatar,
				text,
				at,
				parent,
			)
			.one();
		// Replies are always newer than their message, so a reply pruned by id never leaves its message behind; the
		// second half takes the replies of messages pruned, wherever they are, so no reply is left without one.
		const cutoff = row.id - CHAT_KEPT;
		this.ctx.storage.sql.exec('DELETE FROM chat WHERE id <= ? OR parent <= ?', cutoff, cutoff);
		if (parent !== null && parent <= cutoff) return refuse('That thread is too old for replies.');
		const m: Msg = { id: row.id, key: tab.key, login: tab.login, avatar: tab.avatar, text, at, ...(parent !== null && { parent }) };
		this.broadcast(JSON.stringify({ t: 'msg', m }));
	}

	private unsay(ws: WebSocket, tab: Tab, id: unknown): void {
		if (typeof id !== 'number' || !Number.isSafeInteger(id) || !tab.login) return;
		const row = this.ctx.storage.sql.exec<{ key: string; parent: number | null }>('SELECT key, parent FROM chat WHERE id = ?', id).toArray()[0];
		if (!row) return;
		if (row.key !== tab.key && !tab.admin) return ws.send(JSON.stringify({ t: 'error', text: 'Not your message.' }));
		// A top-level message takes its thread with it.
		this.ctx.storage.sql.exec('DELETE FROM chat WHERE id = ? OR parent = ?', id, id);
		if (row.parent === null) return this.broadcast(JSON.stringify({ t: 'unsay', id }));
		const s = this.summaries([row.parent]).get(row.parent) ?? { replies: 0, last: null, repliers: [] };
		this.broadcast(JSON.stringify({ t: 'unsay', id, parent: row.parent, ...s }));
	}

	/** Sends one tab the replies to `id`, oldest first, and the message itself. */
	private thread(ws: WebSocket, id: unknown): void {
		if (typeof id !== 'number' || !Number.isSafeInteger(id)) return;
		const root = this.ctx.storage.sql
			.exec<Msg>('SELECT id, key, login, avatar, text, at FROM chat WHERE id = ? AND parent IS NULL', id)
			.toArray()[0];
		const messages = root
			? this.ctx.storage.sql
					.exec<Msg>('SELECT id, key, login, avatar, text, at, parent FROM chat WHERE parent = ? ORDER BY id DESC LIMIT ?', id, THREAD_SHOWN)
					.toArray()
					.reverse()
			: [];
		const s = root ? this.summaries([id]).get(id) : undefined;
		ws.send(JSON.stringify({ t: 'thread', id, root: root ? { ...root, ...s } : null, messages }));
	}

	/** The last CHAT_SHOWN top-level messages, oldest first, each with its thread's summary. */
	private history(): (Msg & Summary)[] {
		const top = this.ctx.storage.sql
			.exec<Msg>('SELECT id, key, login, avatar, text, at FROM chat WHERE parent IS NULL ORDER BY id DESC LIMIT ?', CHAT_SHOWN)
			.toArray()
			.reverse();
		const s = this.summaries(top.map((m) => m.id));
		return top.map((m) => ({ ...m, ...(s.get(m.id) ?? { replies: 0, last: null, repliers: [] }) }));
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
				p = { key: tab.key, login: tab.login, avatar: tab.avatar, country: tab.country ?? null, since: tab.since ?? Date.now(), devices: [], pages: [] };
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
		const list = [...people.values()]
			.sort((a, b) => Number(!a.login) - Number(!b.login) || a.key.localeCompare(b.key))
			.slice(0, MAX_PEOPLE);
		return JSON.stringify({ t: 'here', people: list, total: people.size });
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
