/**
 * The blaze site worker, at https://ix.dev/blaze/ (worker/base.ts). Every request comes here first
 * (wrangler.jsonc `run_worker_first: true`): an old host or the bare base is redirected, the base is taken off the
 * path, and then, by site path:
 *
 *   /          the home page (lesson 1), or install.sh for a client that doesn't ask for HTML (curl, wget)
 *   /usage     a 301 to /: its content moved into the home lesson
 *   /latest.json   blaze's newest releases and its yanked ones, from the RELEASES bucket (docs/release.md), which
 *              every blaze daemon reads every few minutes with If-None-Match
 *
 * Every page is static. Anything else is dist/ (built by `bun gen/build.ts`) through the ASSETS binding, with the headers `assetHeaders`
 * adds (Workers does not apply a `_headers` file to what a worker returns).
 */
import { enter, outer } from './base';
import type { Env } from './env';
import { assetHeaders, plain } from './http';

async function route(request: Request, env: Env): Promise<Response> {
	const url = new URL(request.url);
	const p = url.pathname;
	if (p === '/usage' || p === '/usage/') return new Response(null, { status: 301, headers: { location: `${outer('/')}${url.search}`, 'cache-control': 'public, max-age=3600' } });
	if (p === '/' && !acceptsHtml(request)) return installer(request, env, url);
	if (p === '/latest.json') return latest(request, env);
	const res = assetHeaders(await env.ASSETS.fetch(request), p);
	if (p === '/') res.headers.append('vary', 'Accept');
	return res;
}

/** A browser loading a page always asks for HTML; curl and wget ask for anything. */
const acceptsHtml = (request: Request) => (request.headers.get('accept') ?? '').includes('text/html');

/** The home page for a shell: `curl -fsSL https://ix.dev/blaze | sh` gets install.sh, as browsers get the page. */
async function installer(request: Request, env: Env, url: URL): Promise<Response> {
	const res = assetHeaders(await env.ASSETS.fetch(new Request(new URL('/install.sh', url), request)));
	res.headers.append('vary', 'Accept');
	return res;
}

/**
 * `latest.json` from the RELEASES bucket, with its ETag: a request whose If-None-Match names it gets a 304 and no
 * body, so a daemon's check of an unchanged file costs a few bytes. Cached for a minute at most, as a release can be
 * yanked.
 */
async function latest(request: Request, env: Env): Promise<Response> {
	if (request.method !== 'GET' && request.method !== 'HEAD') return plain('latest.json is read with GET.', 405, { allow: 'GET, HEAD' });
	const object = await env.RELEASES.get('latest.json', { onlyIf: request.headers });
	if (object === null) return plain('No latest.json yet: release.yaml writes it when it publishes a release.', 404);
	const headers = new Headers({ 'content-type': 'application/json; charset=utf-8', 'cache-control': 'public, max-age=60', etag: object.httpEtag });
	// R2 returns the object without its body when a condition of onlyIf (here If-None-Match) does not hold.
	if (!('body' in object) || request.method === 'HEAD') return new Response(null, { status: 'body' in object ? 200 : 304, headers });
	return new Response(object.body, { headers });
}

export default {
	async fetch(request, env) {
		try {
			const inner = enter(request);
			return inner instanceof Response ? inner : await route(inner, env);
		} catch (e) {
			console.error('unhandled', request.method, new URL(request.url).pathname, e instanceof Error ? e.stack : String(e));
			return plain('Server error.', 500);
		}
	},
} satisfies ExportedHandler<Env>;
