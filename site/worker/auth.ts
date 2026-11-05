/**
 * Sign-in with a GitHub OAuth App (no scopes: only the public profile), and the session cookie.
 *
 * GET  /auth/github?next=/path   redirects to GitHub, with a random state kept in the `__Host-gh_oauth` cookie
 * GET  /auth/github/callback     checks the state, reads the GitHub user, starts a session, 303 to next
 * POST /auth/logout              ends the session (form field `next`), 303 to next
 *
 * The GitHub access token is used once to read /user and then revoked: never stored or logged. The
 * session cookie holds a random token; D1 keeps only its SHA-256. Both cookies are `__Secure-`, host-only and scoped to
 * the site's base path (worker/http.ts setCookie), so they are not sent to the rest of ix.dev.
 *
 * Paths here are site paths (worker/base.ts): `next` has no base, and every redirect goes to `outer(next)`.
 */
import { outer } from './base';
import type { Env } from './env';
import {
	boundedBody,
	clearCookie,
	cookie,
	methodNotAllowed,
	plain,
	randomToken,
	redirect,
	safeNext,
	sameOrigin,
	setCookie,
	sha256hex,
	timingSafeEqual,
} from './http';

const OAUTH_COOKIE = '__Secure-blaze_gh_oauth';
const SESSION_COOKIE = '__Secure-blaze_session';
const OAUTH_MAX_AGE = 600;
const SESSION_MAX_AGE = 30 * 86400;
const USER_AGENT = 'blaze-site';
/** The logout form is one `next` field. */
const LOGOUT_MAX = 4096;

export type User = { id: number; login: string; name: string | null; avatar_url: string | null };

/** The request carries a session cookie (maybe expired): only then is the user looked up. */
export const hasSessionCookie = (request: Request): boolean => cookie(request, SESSION_COOKIE) !== null;

/** COMMENT_ADMINS holds GitHub user ids, matched against the session's user id (a login can be renamed and re-registered). */
export function isAdmin(env: Env, me: User | null): boolean {
	if (!me) return false;
	return (env.COMMENT_ADMINS ?? '')
		.split(',')
		.map((s) => s.trim())
		.some((s) => /^[1-9][0-9]{0,14}$/.test(s) && Number(s) === me.id);
}

/** The signed-in user, or null. */
export async function currentUser(request: Request, env: Env): Promise<User | null> {
	const token = cookie(request, SESSION_COOKIE);
	if (!token || token.length > 128) return null;
	return env.DB.prepare(
		`SELECT u.id, u.login, u.name, u.avatar_url FROM sessions s JOIN users u ON u.id = s.user_id
		WHERE s.token_hash = ? AND s.expires_at > ?`,
	)
		.bind(await sha256hex(token), Math.floor(Date.now() / 1000))
		.first<User>();
}

export async function handleAuth(request: Request, env: Env, ctx: ExecutionContext, url: URL): Promise<Response> {
	switch (url.pathname) {
		case '/auth/github':
			return request.method === 'GET' ? start(env, url) : methodNotAllowed('GET');
		case '/auth/github/callback':
			return request.method === 'GET' ? callback(request, env, ctx, url) : methodNotAllowed('GET');
		case '/auth/logout':
			return request.method === 'POST' ? logout(request, env) : methodNotAllowed('POST');
		default:
			return plain('Not found.', 404);
	}
}

function start(env: Env, url: URL): Response {
	if (!env.GITHUB_CLIENT_ID) return plain('Sign-in with GitHub is not set up on this server.', 503);
	const next = safeNext(url.searchParams.get('next'));
	const state = randomToken();
	const gh = new URL('https://github.com/login/oauth/authorize');
	gh.searchParams.set('client_id', env.GITHUB_CLIENT_ID);
	gh.searchParams.set('redirect_uri', `${url.origin}${outer('/auth/github/callback')}`);
	gh.searchParams.set('state', state);
	gh.searchParams.set('allow_signup', 'true');
	// The state is base64url, so the first "." splits it from next.
	return redirect(gh.toString(), [setCookie(OAUTH_COOKIE, `${state}.${encodeURIComponent(next)}`, OAUTH_MAX_AGE)]);
}

async function callback(request: Request, env: Env, ctx: ExecutionContext, url: URL): Promise<Response> {
	const clearOauth = clearCookie(OAUTH_COOKIE);
	const fail = (msg: string, status = 400) => plain(msg, status, { 'set-cookie': clearOauth });

	const saved = cookie(request, OAUTH_COOKIE);
	const state = url.searchParams.get('state');
	const dot = saved?.indexOf('.') ?? -1;
	if (!saved || dot < 0 || !state) return fail('Sign-in expired or was started elsewhere. Try again.');
	if (!timingSafeEqual(saved.slice(0, dot), state)) return fail('Sign-in expired or was started elsewhere. Try again.');
	let next: string;
	try {
		next = safeNext(decodeURIComponent(saved.slice(dot + 1)));
	} catch {
		next = '/';
	}

	// The visitor said no on GitHub's page.
	if (url.searchParams.get('error')) return redirect(outer(next), [clearOauth]);

	const code = url.searchParams.get('code');
	if (!code) return fail('GitHub did not send a code.');
	if (!env.GITHUB_CLIENT_ID || !env.GITHUB_CLIENT_SECRET) return fail('Sign-in with GitHub is not set up on this server.', 503);

	const tokenRes = await fetch('https://github.com/login/oauth/access_token', {
		method: 'POST',
		headers: { accept: 'application/json', 'content-type': 'application/x-www-form-urlencoded', 'user-agent': USER_AGENT },
		body: new URLSearchParams({
			client_id: env.GITHUB_CLIENT_ID,
			client_secret: env.GITHUB_CLIENT_SECRET,
			code,
			redirect_uri: `${url.origin}${outer('/auth/github/callback')}`,
		}),
	});
	const tokenOut: { access_token?: unknown; error?: unknown } = tokenRes.ok ? await tokenRes.json<{ access_token?: unknown; error?: unknown }>().catch(() => ({})) : {};
	if (typeof tokenOut.access_token !== 'string' || !tokenOut.access_token) {
		console.warn('github token exchange failed', tokenRes.status, typeof tokenOut.error === 'string' ? tokenOut.error : '');
		return fail('GitHub sign-in failed. Try again.', 502);
	}

	const accessToken = tokenOut.access_token;
	let userRes: Response;
	try {
		userRes = await fetch('https://api.github.com/user', {
			headers: {
				authorization: `Bearer ${accessToken}`,
				accept: 'application/vnd.github+json',
				'x-github-api-version': '2022-11-28',
				'user-agent': USER_AGENT,
			},
		});
	} finally {
		// The token was needed only for /user: revoke it whatever happened, without holding up the response.
		ctx.waitUntil(revoke(env.GITHUB_CLIENT_ID, env.GITHUB_CLIENT_SECRET, accessToken));
	}
	if (!userRes.ok) {
		console.warn('github /user failed', userRes.status);
		return fail('GitHub sign-in failed. Try again.', 502);
	}
	const gh: { id?: unknown; login?: unknown; name?: unknown; avatar_url?: unknown } = await userRes.json<{ id?: unknown; login?: unknown; name?: unknown; avatar_url?: unknown }>().catch(() => ({}));
	if (typeof gh.id !== 'number' || !Number.isSafeInteger(gh.id) || typeof gh.login !== 'string' || !gh.login) {
		return fail('GitHub sent an unexpected profile.', 502);
	}
	const str = (v: unknown, max: number) => (typeof v === 'string' && v ? v.slice(0, max) : null);

	// A new token on every sign-in (no session fixation); a session the browser already had is ended.
	const token = randomToken();
	const now = Math.floor(Date.now() / 1000);
	const old = cookie(request, SESSION_COOKIE);
	await env.DB.batch([
		env.DB.prepare(
			`INSERT INTO users (id, login, name, avatar_url) VALUES (?, ?, ?, ?)
			ON CONFLICT (id) DO UPDATE SET
				login = excluded.login,
				name = excluded.name,
				avatar_url = excluded.avatar_url,
				updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')`,
		).bind(gh.id, gh.login.slice(0, 100), str(gh.name, 256), str(gh.avatar_url, 512)),
		env.DB.prepare('DELETE FROM sessions WHERE token_hash = ? OR expires_at <= ?').bind(old ? await sha256hex(old) : '', now),
		env.DB.prepare('INSERT INTO sessions (token_hash, user_id, expires_at) VALUES (?, ?, ?)').bind(
			await sha256hex(token),
			gh.id,
			now + SESSION_MAX_AGE,
		),
	]);
	return redirect(outer(next), [clearOauth, setCookie(SESSION_COOKIE, token, SESSION_MAX_AGE)]);
}

async function logout(request: Request, env: Env): Promise<Response> {
	if (!sameOrigin(request)) return plain('Cross-site request refused.', 403);
	let next = '/';
	try {
		const body = await boundedBody(request, LOGOUT_MAX);
		const form = body && (await body.formData());
		const v = form ? form.get('next') : null;
		next = safeNext(typeof v === 'string' ? v : null);
	} catch {
		// No form body: go home.
	}
	const token = cookie(request, SESSION_COOKIE);
	if (token) await env.DB.prepare('DELETE FROM sessions WHERE token_hash = ?').bind(await sha256hex(token)).run();
	return redirect(outer(next), [clearCookie(SESSION_COOKIE)]);
}

/** DELETE /applications/{client_id}/token: ends this one token (the user's grant to the app stays). Logs only the status. */
async function revoke(clientId: string, clientSecret: string, accessToken: string): Promise<void> {
	try {
		const res = await fetch(`https://api.github.com/applications/${encodeURIComponent(clientId)}/token`, {
			method: 'DELETE',
			headers: {
				authorization: `Basic ${btoa(`${clientId}:${clientSecret}`)}`,
				accept: 'application/vnd.github+json',
				'content-type': 'application/json',
				'x-github-api-version': '2022-11-28',
				'user-agent': USER_AGENT,
			},
			body: JSON.stringify({ access_token: accessToken }),
		});
		if (res.status !== 204) console.warn('github token revoke failed', res.status);
	} catch (e) {
		console.warn('github token revoke failed', e instanceof Error ? e.name : 'error');
	}
}
