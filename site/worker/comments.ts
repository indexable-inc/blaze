/**
 * Comments on discussable pages, per block.
 *
 * GET    /api/comments?doc=<doc>        {comments, me, signIn, admin}
 * POST   /api/comments                  JSON {doc, block, body}; 201 {comment}
 *                                       or a form (doc, block, body; no JS): 303 to the page#c-<id>
 * DELETE /api/comments/<id>             author or a COMMENT_ADMINS user id; 204. The row stays (ids and threads stay
 *                                       stable) but its body and quote are erased (D1 Time Travel still holds
 *                                       earlier database states for up to 30 days).
 * POST   /api/comments/<id>/delete      the same from a form (no JS); 303 to the page#block-<block>
 *
 * The form routes run the same checks as the JSON ones (session, Origin, size, validation, the atomic rate limit).
 * A failure redirects to the page with `?c-error=<code>&c-block=<block>#block-<block>`, which shows a fixed message
 * (worker/pages.ts FORM_ERRORS) in that block's form; a cross-site post gets a plain 403, and a delete without a
 * session a plain 401 (checked before the comment is looked up, so it tells nothing about it). Writes bump the doc's
 * version (triggers, migrations/0003), which moves the cached page to a new key.
 *
 * Which docs and blocks take comments comes from .gen/discussable.json, written by `bun gen/build.ts`.
 * The quote stored with a comment is cut from that file, never taken from the client.
 */
import discussableJson from '../.gen/discussable.json';
import { currentUser, isAdmin, type User } from './auth';
import { outer } from './base';
import type { Env } from './env';
import { announce } from './presence';
import { boundedBody, json, mediaType, methodNotAllowed, plain, redirect, safeNext, sameOrigin } from './http';
import { docUrl, forgetVersion, type FormError } from './pages';
import type { Discussable } from './site-wasm';

/** .gen/discussable.json (gen/build.ts, from the renderer): doc -> title and each block's text. */
const discussable: Discussable = discussableJson;

const BODY_MAX = 4000;
const QUOTE_MAX = 140;
/** A request body larger than this (bytes) is refused unread (a 4000-char body is at most ~16 KB of UTF-8, JSON-escaped ~24 KB). */
const REQUEST_MAX = 64 * 1024;
const PER_10_MIN = 10;
const PER_DAY = 100;

export type CommentOut = {
	id: number;
	block: string;
	quote: string | null;
	body: string;
	created_at: string;
	user: { login: string; name: string | null; avatar_url: string | null };
	mine: boolean;
};

type Row = {
	id: number;
	block: string;
	quote: string | null;
	body: string;
	created_at: string;
	user_id: number;
	login: string;
	name: string | null;
	avatar_url: string | null;
};

const out = (r: Row, me: User | null): CommentOut => ({
	id: r.id,
	block: r.block,
	quote: r.quote,
	body: r.body,
	created_at: r.created_at,
	user: { login: r.login, name: r.name, avatar_url: r.avatar_url },
	mine: me !== null && me.id === r.user_id,
});

/** Own keys only, so "__proto__" or "constructor" are not docs. */
function doc(name: unknown): Discussable[string] | null {
	return typeof name === 'string' && Object.hasOwn(discussable, name) ? discussable[name] : null;
}

const FORM_TYPES = new Set(['application/x-www-form-urlencoded', 'multipart/form-data']);

export async function handleComments(request: Request, env: Env, url: URL): Promise<Response> {
	if (url.pathname === '/api/comments') {
		if (request.method === 'GET') return list(request, env, url);
		if (request.method === 'POST') return FORM_TYPES.has(mediaType(request)) ? createForm(request, env) : create(request, env);
		return methodNotAllowed('GET, POST');
	}
	const m = /^\/api\/comments\/([1-9][0-9]{0,14})(\/delete)?$/.exec(url.pathname);
	if (m && m[2]) return request.method === 'POST' ? removeForm(request, env, Number(m[1])) : methodNotAllowed('POST');
	if (m) return request.method === 'DELETE' ? remove(request, env, Number(m[1])) : methodNotAllowed('DELETE');
	return json({ error: 'Not found.' }, 404);
}

/** Back to the page after a form post: to `hash`, or to the block's form with the error shown. */
function back(doc: string, block: string, outcome: { hash: string } | { error: FormError }): Response {
	const path = docUrl(doc) ?? '/';
	if ('hash' in outcome) return redirect(outer(safeNext(`${path}${outcome.hash}`)));
	const q = new URLSearchParams({ 'c-error': outcome.error, 'c-block': block });
	return redirect(outer(safeNext(`${path}?${q}#block-${encodeURIComponent(block)}`)));
}

async function list(request: Request, env: Env, url: URL): Promise<Response> {
	const name = url.searchParams.get('doc');
	if (!doc(name)) return json({ error: 'No such page.' }, 404);
	const me = await currentUser(request, env);
	const { results } = await env.DB.prepare(
		`SELECT c.id, c.block, c.quote, c.body, c.created_at, c.user_id, u.login, u.name, u.avatar_url
		FROM comments c JOIN users u ON u.id = c.user_id
		WHERE c.doc = ? AND c.deleted_at IS NULL
		ORDER BY c.id`,
	)
		.bind(name)
		.all<Row>();
	return json({
		comments: results.map((r) => out(r, me)),
		me: me && { login: me.login, name: me.name, avatar_url: me.avatar_url },
		signIn: !!env.GITHUB_CLIENT_ID,
		admin: isAdmin(env, me),
	});
}

type Created = { comment: CommentOut } | { status: number; error: string; code: FormError | null };

async function create(request: Request, env: Env): Promise<Response> {
	const me = await currentUser(request, env);
	if (!me) return json({ error: 'Sign in to comment.' }, 401);
	if (!sameOrigin(request)) return json({ error: 'Cross-site request refused.' }, 403);
	if (mediaType(request) !== 'application/json') return json({ error: 'Send JSON.' }, 415);

	const limited = await boundedBody(request, REQUEST_MAX);
	if (!limited) return json({ error: 'Comment is too long.' }, 413);
	const raw = await limited.text();
	let input: { doc?: unknown; block?: unknown; body?: unknown };
	try {
		const parsed: unknown = JSON.parse(raw);
		if (typeof parsed !== 'object' || parsed === null || Array.isArray(parsed)) throw new Error('not an object');
		input = parsed;
	} catch {
		return json({ error: 'Send a JSON object.' }, 400);
	}
	const out = await insert(env, me, input.doc, input.block, input.body);
	return 'comment' in out ? json(out, 201) : json({ error: out.error }, out.status);
}

/** The no-JS post: a form with doc, block and body. Same checks as `create`; answers with a redirect. */
async function createForm(request: Request, env: Env): Promise<Response> {
	// Cross-site: refused before anything else, with no redirect into the site.
	if (!sameOrigin(request)) return plain('Cross-site request refused.', 403);
	// Too large to read means no doc to go back to: a plain, readable error.
	const limited = await boundedBody(request, REQUEST_MAX);
	if (!limited) return plain('Comment is too long.', 413);
	let form: FormData;
	try {
		form = await limited.formData();
	} catch {
		return plain('Could not read the form.', 400);
	}
	const field = (k: string) => {
		const v = form.get(k);
		return typeof v === 'string' ? v : '';
	};
	const name = field('doc');
	const page = doc(name);
	const block = field('block');
	if (!page || !Object.hasOwn(page.blocks, block)) return plain('No such page or block.', 400);
	const me = await currentUser(request, env);
	if (!me) return back(name, block, { error: 'signin' });
	const out = await insert(env, me, name, block, field('body'));
	if ('comment' in out) return back(name, block, { hash: `#c-${out.comment.id}` });
	return out.code ? back(name, block, { error: out.code }) : plain(out.error, out.status);
}

/** Validates and stores one comment, under the rate limit. */
async function insert(env: Env, me: User, docName: unknown, block: unknown, rawBody: unknown): Promise<Created> {
	// Sign-in off (no client id) means commenting off too, including for sessions started before it was turned off.
	if (!env.GITHUB_CLIENT_ID) return { status: 403, error: 'Comments are off.', code: null };
	const name = typeof docName === 'string' ? docName : '';
	const page = doc(name);
	if (!page) return { status: 400, error: 'No such page.', code: null };
	if (typeof block !== 'string' || !Object.hasOwn(page.blocks, block)) return { status: 400, error: 'No such block.', code: null };
	const body = typeof rawBody === 'string' ? rawBody.trim() : '';
	if (body.length < 1 || body.length > BODY_MAX) return { status: 400, error: `A comment is 1 to ${BODY_MAX} characters.`, code: 'length' };

	// The page as a whole has no text to quote.
	const quote = block === '_page' ? null : page.blocks[block].slice(0, QUOTE_MAX) || null;
	// The limit is checked in the INSERT itself, so parallel posts cannot all pass a count read before any of them
	// is written. Counted from comments (deleted ones too, so deleting does not give quota back).
	const row = await env.DB.prepare(
		`INSERT INTO comments (doc, block, quote, body, user_id)
		SELECT ?1, ?2, ?3, ?4, ?5
		WHERE (SELECT count(*) FROM comments WHERE user_id = ?5 AND created_at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-10 minutes')) < ?6
			AND (SELECT count(*) FROM comments WHERE user_id = ?5 AND created_at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-1 day')) < ?7
		RETURNING id, created_at`,
	)
		.bind(name, block, quote, body, me.id, PER_10_MIN, PER_DAY)
		.first<{ id: number; created_at: string }>();
	if (!row) return { status: 429, error: 'Too many comments. Try again later.', code: 'rate' };
	forgetVersion(name);
	await announce(env, name);
	const comment: CommentOut = {
		id: row.id,
		block,
		quote,
		body,
		created_at: row.created_at,
		user: { login: me.login, name: me.name, avatar_url: me.avatar_url },
		mine: true,
	};
	return { comment };
}

async function remove(request: Request, env: Env, id: number): Promise<Response> {
	const me = await currentUser(request, env);
	if (!me) return json({ error: 'Sign in first.' }, 401);
	if (!sameOrigin(request)) return json({ error: 'Cross-site request refused.' }, 403);
	const out = await erase(env, me, id);
	if (out === 'gone') return json({ error: 'No such comment.' }, 404);
	if (out === 'forbidden') return json({ error: 'Not your comment.' }, 403);
	return new Response(null, { status: 204, headers: { 'cache-control': 'no-store' } });
}

/** The no-JS delete (HTML forms cannot send DELETE). Same checks as `remove`; answers with a redirect. */
async function removeForm(request: Request, env: Env, id: number): Promise<Response> {
	if (!sameOrigin(request)) return plain('Cross-site request refused.', 403);
	// The session first: without one, nothing about the comment (its doc, its block, whether it exists) is told.
	const me = await currentUser(request, env);
	if (!me) return plain('Sign in to delete a comment.', 401);
	const row = await env.DB.prepare('SELECT doc, block FROM comments WHERE id = ?').bind(id).first<{ doc: string; block: string }>();
	if (!row || !doc(row.doc)) return plain('No such comment.', 404);
	const block = Object.hasOwn(doc(row.doc)!.blocks, row.block) ? row.block : '_page';
	const out = await erase(env, me, id);
	if (out === 'ok') return back(row.doc, block, { hash: `#block-${encodeURIComponent(block)}` });
	return back(row.doc, block, { error: out });
}

/** Erases one comment if `me` wrote it or is an admin. */
async function erase(env: Env, me: User, id: number): Promise<'ok' | 'gone' | 'forbidden'> {
	const row = await env.DB.prepare('SELECT user_id, doc FROM comments WHERE id = ? AND deleted_at IS NULL')
		.bind(id)
		.first<{ user_id: number; doc: string }>();
	if (!row) return 'gone';
	if (row.user_id !== me.id && !isAdmin(env, me)) return 'forbidden';
	// Deleting erases the text: the row stays (so ids and threads stay stable), its body and quote do not.
	await env.DB.prepare(
		`UPDATE comments SET deleted_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), body = '', quote = NULL
		WHERE id = ? AND deleted_at IS NULL`,
	)
		.bind(id)
		.run();
	forgetVersion(row.doc);
	await announce(env, row.doc);
	return 'ok';
}
