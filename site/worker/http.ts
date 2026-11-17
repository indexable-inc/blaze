/** Small helpers shared by the routes: plain responses and the headers every asset gets. */
import buildJson from '../.gen/build.json';
import { csp } from './csp';

export function plain(body: string, status: number, headers: HeadersInit = {}): Response {
	const h = new Headers(headers);
	h.set('content-type', 'text/plain; charset=utf-8');
	h.set('cache-control', 'no-store');
	return new Response(body, { status, headers: h });
}

/** The code graph page (site/graph/), under the base: it compiles its layout simulation's wasm. */
const GRAPH = '/graph';

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
	out.headers.set('content-security-policy', csp({ wasm: path === GRAPH }));
	out.headers.set('x-content-type-options', 'nosniff');
	out.headers.set('referrer-policy', 'strict-origin-when-cross-origin');
	return out;
}
