/**
 * Where the site is served: `Served` on `site` in content/site.flecs, written by gen/build.ts to .gen/site.json.
 * https://ix.dev/blaze/ today: the worker runs on the zone route `ix.dev/blaze*` (wrangler.jsonc).
 *
 * Inside the worker every path is a site path, without the base (`/rfcs/x`, `/learn/builds`), as the renderer lists pages
 * and as dist/ is laid out; `index.ts` strips the base on the way in. Anything going back to the browser as a path
 * (a Location, a cookie's Path, an OAuth redirect_uri) goes through `outer`.
 *
 * Any other host the worker is reached on (the custom domains blaze.ix.dev and blaze.ix.dev, the site's old homes)
 * is answered with a redirect to the same path and query under ORIGIN + BASE. localhost (`wrangler dev`) is served.
 */
import siteJson from '../.gen/site.json';

/** `https://ix.dev`. */
export const ORIGIN: string = siteJson.origin;
/** `/blaze`: no trailing slash; empty for a site at the root. */
export const BASE: string = siteJson.base;
const HOST = ORIGIN ? new URL(ORIGIN).host : '';

/** A site path (`/rfcs/x`, `/`) as the browser sees it (`/blaze/rfcs/x`, `/blaze/`). */
export const outer = (sitePath: string): string => BASE + sitePath;

/** The path the cookies are scoped to: the base, so they never go to the rest of ix.dev. */
export const COOKIE_PATH = BASE || '/';

const isLocal = (host: string) => host === 'localhost' || host === '127.0.0.1' || host === '[::1]';

/**
 * The request with the base taken off its path, or the response to send instead: a redirect from an old host or from
 * the bare base (`/blaze` to `/blaze/`), or a 404 for a path outside the base.
 */
export function enter(request: Request): Request | Response {
	const url = new URL(request.url);
	if (HOST && url.host !== HOST && !isLocal(url.hostname)) {
		// The old homes: same path and query under the base. GET and HEAD get a 301; anything else a 308, which keeps
		// the method and body (a form posted from a page still open on the old host).
		const to = `${ORIGIN}${outer(url.pathname)}${url.search}`;
		return moved(to, request.method === 'GET' || request.method === 'HEAD' ? 301 : 308);
	}
	if (!BASE) return request;
	const p = url.pathname;
	if (p === BASE || (p === '/' && isLocal(url.hostname))) return moved(`${BASE}/${url.search}`, 301);
	if (!p.startsWith(`${BASE}/`)) return new Response('Not found.', { status: 404, headers: { 'content-type': 'text/plain; charset=utf-8' } });
	url.pathname = p.slice(BASE.length);
	return new Request(url, request);
}

function moved(location: string, status: 301 | 308): Response {
	return new Response(null, { status, headers: { location, 'cache-control': 'public, max-age=3600' } });
}
