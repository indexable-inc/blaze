/**
 * Discussable pages (the charter, RFCs), rendered per request by the Rust renderer (worker/renderer.ts, the wasm
 * module and snapshot gen/build.ts wrote), the same renderer that wrote the static pages, with the doc's comments.
 *
 * Anonymous requests are served from the Cache API under
 * `<origin><path>?v=<doc version>&b=<build hash>&w=<worker version>&s=<sign-in on>`. The version (doc_versions, bumped
 * by triggers whenever a comment is written or deleted) is one small D1 read; the worker version (`version_metadata`)
 * changes with every deploy, so new code or new vars never serve an older rendering. An entry is never stale, so
 * nothing has to be purged. Requests from a signed-in reader are rendered for them (their form, their
 * delete buttons) and sent `private, no-store`; they never read or write the shared cache.
 */
import buildJson from '../.gen/build.json';
import pagesJson from '../.gen/pages.json';
import { currentUser, hasSessionCookie, isAdmin } from './auth';
import { outer } from './base';
import type { Env } from './env';
import { csp } from './csp';
import { ASSETS, ASSETS_HEADER, methodNotAllowed, plain } from './http';
import { renderPage, type CommentRow, type Discussion } from './renderer';
import type { Page } from './site-wasm';


const list: Page[] = pagesJson;
const byUrl = new Map(list.filter((p) => p.discussable).map((p) => [p.url, p]));
const BUILD = buildJson.hash;

/** The worker's discussable page at this path, if any. */
export const discussablePage = (path: string): Page | undefined => byUrl.get(path);
/** The URL of a discussable doc (`rfcs/capabilities` -> `/rfcs/capabilities`). */
export const docUrl = (doc: string): string | undefined => list.find((p) => p.doc === doc && byUrl.has(p.url))?.url;

/** Messages for the `c-error` codes the no-JS form routes redirect with (worker/comments.ts). */
export const FORM_ERRORS = {
	signin: 'Sign in to comment.',
	length: 'A comment is 1 to 4000 characters.',
	rate: 'Too many comments. Try again later.',
	gone: 'That comment is already gone.',
	forbidden: 'Not your comment.',
} as const;
export type FormError = keyof typeof FORM_ERRORS;

/** The doc's comments (deleted ones left out), oldest first: the one D1 query a page render makes. */
async function comments(env: Env, doc: string): Promise<CommentRow[]> {
	const { results } = await env.DB.prepare(
		`SELECT c.id, c.block, coalesce(c.quote, '') AS quote, c.body, c.created_at, c.user_id,
			u.login, coalesce(u.name, '') AS name, coalesce(u.avatar_url, '') AS avatar_url
		FROM comments c JOIN users u ON u.id = c.user_id
		WHERE c.doc = ? AND c.deleted_at IS NULL
		ORDER BY c.id`,
	)
		.bind(doc)
		.all<CommentRow>();
	return results;
}

/** The page's whole HTML, or null when the renderer failed (logged here; the caller answers 500, never part of a page). */
function html(page: Page, rows: CommentRow[], discussion: Discussion): string | null {
	try {
		return renderPage(page.url, rows, discussion);
	} catch (e) {
		console.error('render failed', page.url, e instanceof Error ? (e.stack ?? e.message) : String(e));
		return null;
	}
}

const renderFailed = () => plain('Server error.', 500);

// ---- The version, memoised for a moment per isolate (a reader may see a new comment a second late). ----

const VERSION_TTL_MS = 2000;
const versions = new Map<string, { version: number; at: number }>();

async function docVersion(env: Env, doc: string): Promise<number> {
	const memo = versions.get(doc);
	if (memo && Date.now() - memo.at < VERSION_TTL_MS) return memo.version;
	const row = await env.DB.prepare('SELECT version FROM doc_versions WHERE doc = ?').bind(doc).first<{ version: number }>();
	const version = row?.version ?? 0;
	versions.set(doc, { version, at: Date.now() });
	return version;
}

/** After a write in this isolate, so the writer's next anonymous view is not a version behind. */
export const forgetVersion = (doc: string) => void versions.delete(doc);

// ---- Headers ----

function headers(cacheControl: string, how: string): Headers {
	return new Headers({
		'content-type': 'text/html; charset=utf-8',
		'cache-control': cacheControl,
		'content-security-policy': csp(),
		'x-content-type-options': 'nosniff',
		'referrer-policy': 'strict-origin-when-cross-origin',
		// Which path served it: cache hit, cache miss (rendered and stored), anonymous but not cacheable, or per user.
		'x-render': how,
		[ASSETS_HEADER]: ASSETS,
	});
}

const respond = (request: Request, body: string, h: Headers, status = 200) =>
	new Response(request.method === 'HEAD' ? null : body, { status, headers: h });

/**
 * A discussable page, or null when `url` is not one (the caller hands it to the assets). Signed in: rendered for that
 * reader, private. Anonymous: from the shared cache, unless a form error is to be shown.
 */
export async function handlePage(request: Request, env: Env, ctx: ExecutionContext, url: URL): Promise<Response | null> {
	const page = discussablePage(url.pathname);
	if (!page) return null;
	if (request.method !== 'GET' && request.method !== 'HEAD') return methodNotAllowed('GET, HEAD');

	const signIn = !!env.GITHUB_CLIENT_ID;
	const code = url.searchParams.get('c-error');
	const errBlock = url.searchParams.get('c-block') ?? '';
	const error = code !== null && Object.hasOwn(FORM_ERRORS, code) ? { block: errBlock, message: FORM_ERRORS[code as FormError] } : undefined;

	const me = hasSessionCookie(request) ? await currentUser(request, env) : null;
	if (me) {
		const viewer = { id: me.id, login: me.login, admin: isAdmin(env, me) };
		const body = html(page, await comments(env, page.doc), { viewer, signIn, error });
		if (body === null) return renderFailed();
		return respond(request, body, headers('private, no-store', 'private'));
	}

	// Anonymous from here on: the HTML is rendered with viewer null, so it holds nothing about whoever asked.
	if (error) {
		const body = html(page, await comments(env, page.doc), { viewer: null, signIn, error });
		return body === null ? renderFailed() : respond(request, body, headers('no-store', 'anonymous'));
	}
	return anonymous(request, env, ctx, url, page, signIn);
}

/**
 * The only path that touches caches.default. It takes no user: the HTML it stores is rendered with `viewer: null`
 * right here, so per-user markup cannot reach the cache. The version is read before the comments, so an entry holds
 * at least what its version says (a comment landing in between makes it newer than its key, never older).
 */
async function anonymous(request: Request, env: Env, ctx: ExecutionContext, url: URL, page: Page, signIn: boolean): Promise<Response> {
	const version = await docVersion(env, page.doc);
	// `w`: the deploy (code and vars). `wrangler dev` gives a random id per session; "dev" should the binding ever be
	// missing, where the build hash still moves the key. `s`: sign-in changes the markup (the thread's sign-in link).
	const worker = env.CF_VERSION?.id || 'dev';
	const q = new URLSearchParams({ v: String(version), b: BUILD, w: worker, s: signIn ? '1' : '0' });
	// Keyed by the page's real URL (under the base), so no other ix.dev worker's cache entry can share it.
	const key = new Request(`${url.origin}${outer(page.url)}?${q}`, { method: 'GET' });
	const cache = caches.default;
	const hit = await cache.match(key);
	// The browser revalidates every time (no-cache, varying on the cookie): once the reader signs in, they must get
	// their own rendering, not this anonymous copy.
	const toBrowser = (how: string) => {
		const h = headers('no-cache', how);
		h.set('vary', 'cookie');
		return h;
	};
	if (hit) return respond(request, await hit.text(), toBrowser('hit'));

	const body = html(page, await comments(env, page.doc), { viewer: null, signIn });
	if (body === null) return renderFailed();
	ctx.waitUntil(cache.put(key, new Response(body, { headers: headers('public, max-age=86400', 'stored') })));
	return respond(request, body, toBrowser('miss'));
}
