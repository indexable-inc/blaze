/**
 * Who is on the site right now, and on which page: one Durable Object (`Presence`, named "site") holding a WebSocket
 * per open tab, with the hibernation API, so an idle room costs nothing between messages.
 *
 * GET /api/presence (a WebSocket upgrade, same Origin only) is checked here in the worker: the session cookie gives
 * the GitHub user, if any. The object gets the request with `x-presence-who` (the user, or nothing) and
 * `x-presence-ip`, and names an anonymous visitor by an HMAC of their IP and user agent under a random key it keeps
 * per UTC day: the same browser is the same identicon all day, the next day it is another, and neither the IP nor the
 * key leaves the object. Signed-in readers are their GitHub login.
 *
 * Protocol (JSON text frames):
 *
 *   server -> client  {t: "you", key, login, avatar, signIn}       once, on connect
 *                     {t: "here", people: [{key, login, avatar, pages: [{path, away}]}]}   on every change
 *                     {t: "comments", doc}                      a comment on that doc was posted or deleted
 *   client -> server  {t: "at", path, away}                       the page the tab shows (a site path) and whether
 *                                                                  it is hidden; unknown paths count as no page
 *
 * A visitor with several tabs is one person with several pages. Broadcasts are coalesced (BROADCAST_MS). The
 * comment routes (worker/comments.ts) call `announce` after each write, so open pages refresh their threads live.
 */
import { DurableObject } from 'cloudflare:workers';
import pagesJson from '../.gen/pages.json';
import { currentUser, hasSessionCookie } from './auth';
import type { Env } from './env';
import { json, plain, sameOrigin } from './http';

const PATHS = new Set<string>(pagesJson.map((p) => p.url));
const BROADCAST_MS = 300;
const MAX_SOCKETS = 4000;
const MAX_TABS = 16;
const MAX_FRAME = 1024;
const MAX_PEOPLE = 300;

type Who = { key: string; login: string | null; avatar: string | null };
/** Kept on each socket (`serializeAttachment`), so it survives hibernation. */
type Tab = Who & { path: string | null; away: boolean };
type Person = Who & { pages: { path: string; away: boolean }[] };

/** The worker side of GET /api/presence. */
export async function handlePresence(request: Request, env: Env): Promise<Response> {
	if (request.method !== 'GET') return plain('Method not allowed.', 405, { allow: 'GET' });
	if (request.headers.get('upgrade')?.toLowerCase() !== 'websocket') return plain('Expected a WebSocket.', 426);
	if (!sameOrigin(request)) return plain('Cross-site request refused.', 403);
	const user = hasSessionCookie(request) ? await currentUser(request, env) : null;
	const headers = new Headers({ upgrade: 'websocket' });
	if (user) headers.set('x-presence-who', JSON.stringify({ key: `u${user.id}`, login: user.login, avatar: avatarOf(user.avatar_url) }));
	headers.set('x-presence-ip', request.headers.get('cf-connecting-ip') ?? '');
	headers.set('x-presence-ua', (request.headers.get('user-agent') ?? '').slice(0, 512));
	headers.set('x-presence-signin', env.GITHUB_CLIENT_ID ? '1' : '');
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

	override async fetch(request: Request): Promise<Response> {
		if (request.method === 'POST' && new URL(request.url).pathname === '/comments') {
			const { doc } = await request.json<{ doc: string }>();
			const out = JSON.stringify({ t: 'comments', doc });
			for (const ws of this.open()) {
				try {
					ws.send(out);
				} catch {
					// Closing.
				}
			}
			return new Response(null, { status: 204 });
		}
		if (this.ctx.getWebSockets().length >= MAX_SOCKETS) return json({ error: 'Too many visitors right now.' }, 503);
		const whoHeader = request.headers.get('x-presence-who');
		const who: Who = whoHeader
			? (JSON.parse(whoHeader) as Who)
			: { key: await this.anonKey(request.headers.get('x-presence-ip') ?? '', request.headers.get('x-presence-ua') ?? ''), login: null, avatar: null };
		if (this.ctx.getWebSockets(who.key).length >= MAX_TABS) return json({ error: 'Too many tabs.' }, 429);

		const { 0: client, 1: server } = new WebSocketPair();
		this.ctx.acceptWebSocket(server, [who.key]);
		const tab: Tab = { ...who, path: null, away: false };
		server.serializeAttachment(tab);
		server.send(JSON.stringify({ t: 'you', key: who.key, login: who.login, avatar: who.avatar, signIn: request.headers.get('x-presence-signin') === '1' }));
		server.send(this.snapshot());
		return new Response(null, { status: 101, webSocket: client });
	}

	override webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): void {
		if (typeof message !== 'string' || message.length > MAX_FRAME) return ws.close(1009, 'Too big.');
		let m: { t?: unknown; path?: unknown; away?: unknown };
		try {
			m = JSON.parse(message) as typeof m;
		} catch {
			return ws.close(1003, 'Not JSON.');
		}
		if (m.t !== 'at') return;
		const tab = ws.deserializeAttachment() as Tab;
		const path = typeof m.path === 'string' && PATHS.has(m.path) ? m.path : null;
		const away = m.away === true;
		if (tab.path === path && tab.away === away) return;
		ws.serializeAttachment({ ...tab, path, away } satisfies Tab);
		this.schedule();
	}

	override webSocketClose(ws: WebSocket, code: number): void {
		try {
			ws.close(code === 1005 || code === 1006 ? 1000 : code, 'Bye.');
		} catch {
			// Already closed.
		}
		this.schedule();
	}

	override webSocketError(): void {
		this.schedule();
	}

	private schedule(): void {
		this.pending ??= setTimeout(() => {
			this.pending = null;
			const out = this.snapshot();
			for (const ws of this.open()) {
				try {
					ws.send(out);
				} catch {
					// Closing: its close handler broadcasts again.
				}
			}
		}, BROADCAST_MS);
	}

	private open(): WebSocket[] {
		return this.ctx.getWebSockets().filter((ws) => ws.readyState === WebSocket.OPEN);
	}

	/** Everyone, one entry per person (their tabs' pages), signed-in readers first, then by key. */
	private snapshot(): string {
		const people = new Map<string, Person>();
		for (const ws of this.open()) {
			const tab = ws.deserializeAttachment() as Tab | null;
			if (!tab) continue;
			let p = people.get(tab.key);
			if (!p) {
				p = { key: tab.key, login: tab.login, avatar: tab.avatar, pages: [] };
				people.set(tab.key, p);
			}
			if (tab.path && !p.pages.some((x) => x.path === tab.path && x.away === tab.away)) p.pages.push({ path: tab.path, away: tab.away });
		}
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
