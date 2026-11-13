/**
 * The Content-Security-Policy of everything the worker serves: scripts, styles, images and fetches from this origin
 * only, no frames or plugins. The code graph page (site/graph/) also compiles WebAssembly, its layout
 * simulation (`wasm`).
 * Files get it through `assetHeaders` (worker/http.ts; a `_headers` file does not apply once every request runs the
 * worker first). Its own module, without the worker's generated imports, so graph/serve.ts sends the same policy.
 */
export function csp({ wasm = false } = {}): string {
	return [
		"default-src 'self'",
		wasm ? "script-src 'self' 'wasm-unsafe-eval'" : "script-src 'self'",
		"style-src 'self'",
		"img-src 'self'",
		"connect-src 'self'",
		"frame-src 'none'",
		"form-action 'self'",
		"frame-ancestors 'none'",
		"base-uri 'none'",
		"object-src 'none'",
	].join('; ');
}
