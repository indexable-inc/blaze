/** Small helpers shared by the routes: cookies, origin checks, redirects, tokens. */
import buildJson from '../.gen/build.json';
import { COOKIE_PATH } from './base';

export function json(body: unknown, status = 200, headers: HeadersInit = {}): Response {
	const h = new Headers(headers);
	h.set('content-type', 'application/json; charset=utf-8');
	h.set('cache-control', 'no-store');
	return new Response(JSON.stringify(body), { status, headers: h });
}

export function plain(body: string, status: number, headers: HeadersInit = {}): Response {
	const h = new Headers(headers);
	h.set('content-type', 'text/plain; charset=utf-8');
	h.set('cache-control', 'no-store');
	return new Response(body, { status, headers: h });
}

export function methodNotAllowed(allow: string): Response {
	return plain('Method not allowed.', 405, { allow });
}

const NEXT_MAX = 1024;
const NEXT_BASE = 'https://x.invalid';

/**
 * A site path (no base, worker/base.ts) to send the browser back to, or "/"; redirect to `outer()` of it. The value is resolved against a placeholder origin the way
 * a browser would resolve it, so "//host", "/\host", "/.//host" and the like (another host) fail, and what comes
 * back is the parsed path, query and fragment: percent-encoded ASCII, safe in a Location header. Input or output
 * longer than 1024 chars: "/".
 */
export function safeNext(v: string | null | undefined): string {
	if (typeof v !== 'string' || v.length > NEXT_MAX || !v.startsWith('/')) return '/';
	// URL parsers drop tab and newline, which could turn "/\t/evil" into "//evil"; refuse control characters outright.
	if (/[\u0000-\u001f\u007f]/.test(v)) return '/';
	let u: URL;
	try {
		u = new URL(v, NEXT_BASE);
	} catch {
		return '/';
	}
	if (u.origin !== NEXT_BASE) return '/';
	const out = u.pathname + u.search + u.hash;
	// "/.//host" parses to the path "//host", which a browser would read as another host.
	return out.length <= NEXT_MAX && /^\/[\x21-\x7e]*$/.test(out) && !out.startsWith('//') ? out : '/';
}

/** A 303. A location the Headers API refuses becomes "/", so building the response never throws. */
export function redirect(location: string, cookies: string[] = []): Response {
	const h = new Headers({ 'cache-control': 'no-store' });
	try {
		h.set('location', location);
	} catch {
		h.set('location', '/');
	}
	for (const c of cookies) h.append('set-cookie', c);
	return new Response(null, { status: 303, headers: h });
}

/** True when the request's Origin header is this site's origin. A missing Origin fails. */
export function sameOrigin(request: Request): boolean {
	return request.headers.get('origin') === new URL(request.url).origin;
}

export function cookie(request: Request, name: string): string | null {
	const header = request.headers.get('cookie');
	if (!header) return null;
	for (const part of header.split(';')) {
		const i = part.indexOf('=');
		if (i < 0) continue;
		if (part.slice(0, i).trim() === name) return part.slice(i + 1).trim();
	}
	return null;
}

/**
 * An HttpOnly, Secure, SameSite=Lax, host-only (no Domain) cookie on the site's base path (`/blaze`), so it never goes
 * to the rest of ix.dev. Its name is `__Secure-` prefixed (browsers drop it unless it is set Secure from https); a
 * `__Host-` cookie would have to be Path=/ and so be sent to every ix.dev service. Browsers count http://localhost as
 * secure, so `wrangler dev` still works.
 */
export function setCookie(name: string, value: string, maxAge: number): string {
	return `${name}=${value}; HttpOnly; Secure; SameSite=Lax; Path=${COOKIE_PATH}; Max-Age=${maxAge}`;
}

export const clearCookie = (name: string) => setCookie(name, '', 0);

/** 32 random bytes, base64url (43 chars, no padding). */
export function randomToken(): string {
	const bytes = crypto.getRandomValues(new Uint8Array(32));
	let s = '';
	for (const b of bytes) s += String.fromCharCode(b);
	return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

export async function sha256hex(s: string): Promise<string> {
	const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(s));
	return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, '0')).join('');
}

/** Constant-time string comparison (the length is not secret). */
export function timingSafeEqual(a: string, b: string): boolean {
	const x = new TextEncoder().encode(a);
	const y = new TextEncoder().encode(b);
	if (x.byteLength !== y.byteLength) return false;
	return crypto.subtle.timingSafeEqual(x, y);
}

/** The media type of a Content-Type header, lowercased, without parameters. */
export function mediaType(request: Request): string {
	return (request.headers.get('content-type') ?? '').split(';')[0].trim().toLowerCase();
}

/**
 * The request body, at most `max` bytes, as a Response (for .text() / .formData() with the request's Content-Type),
 * or null when it is longer. A Content-Length over `max` is refused before anything is read; without one (chunked),
 * reading stops as soon as the bytes pass `max`.
 */
export async function boundedBody(request: Request, max: number): Promise<Response | null> {
	const len = request.headers.get('content-length');
	if (len !== null && !(/^[0-9]{1,15}$/.test(len.trim()) && Number(len.trim()) <= max)) return null;
	const headers = { 'content-type': request.headers.get('content-type') ?? '' };
	if (!request.body) return new Response('', { headers });
	const reader = request.body.getReader();
	const chunks: Uint8Array[] = [];
	let size = 0;
	for (;;) {
		const { done, value } = await reader.read();
		if (done) break;
		size += value.byteLength;
		if (size > max) {
			await reader.cancel();
			return null;
		}
		chunks.push(value);
	}
	const bytes = new Uint8Array(size);
	let at = 0;
	for (const c of chunks) {
		bytes.set(c, at);
		at += c.byteLength;
	}
	return new Response(bytes, { headers });
}

/** Where Hugging Face serves model files from: the hub, which redirects to its CDN and Xet storage hosts. */
const HUGGING_FACE = 'https://huggingface.co https://*.huggingface.co https://*.hf.co';
/** The chat translator's worker script, under the base. */
const TRANSLATOR = '/js/translate-worker.js';

/**
 * The Content-Security-Policy of the HTML the worker serves: scripts, styles and fetches from this origin only, avatars
 * from GitHub, narration audio (media-src) from this origin, forms posting back here.
 * The chat translator's worker script (client/translate-worker.ts) alone also compiles WebAssembly and fetches the model
 * from Hugging Face (`translator`; a dedicated worker runs under its own script's policy).
 * Static files get the same policy through `assetHeaders` (a `_headers` file does not apply once every request runs the
 * worker first).
 */
export function csp({ translator = false } = {}): string {
	return [
		"default-src 'self'",
		translator ? "script-src 'self' 'wasm-unsafe-eval'" : "script-src 'self'",
		"style-src 'self'",
		"img-src 'self' https://avatars.githubusercontent.com https://github.com",
		translator ? `connect-src 'self' ${HUGGING_FACE}` : "connect-src 'self'",
		"media-src 'self'",
		"frame-src 'none'",
		"form-action 'self'",
		"frame-ancestors 'none'",
		"base-uri 'none'",
		"object-src 'none'",
	].join('; ');
}

/**
 * The build of the client scripts and styles the site's pages run with (gen/build.ts). Sent with every page as
 * `x-blaze-assets`: the client router (client/nav.ts) swaps a page in only when it matches its own, so after a deploy
 * an open tab's next click is a load.
 */
export const ASSETS_HEADER = 'x-blaze-assets';
export const ASSETS = buildJson.assets;

/** A static file (the ASSETS binding) with the headers every page gets: the CSP above, nosniff, the referrer policy. */
export function assetHeaders(res: Response, path = ''): Response {
	const out = new Response(res.body, res);
	out.headers.set(ASSETS_HEADER, ASSETS);
	out.headers.set('content-security-policy', csp({ translator: path === TRANSLATOR }));
	out.headers.set('x-content-type-options', 'nosniff');
	out.headers.set('referrer-policy', 'strict-origin-when-cross-origin');
	return out;
}
