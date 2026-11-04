/** Where Hugging Face serves model files from: the hub, which redirects to its CDN and Xet storage hosts. */
const HUGGING_FACE = 'https://huggingface.co https://*.huggingface.co https://*.hf.co';

/**
 * The Content-Security-Policy of the HTML the worker serves: scripts, styles and fetches from this origin only, avatars
 * from GitHub, forms posting back here.
 * The chat translator's worker script (client/translate-worker.ts) alone also compiles WebAssembly and fetches the model
 * from Hugging Face (`translator`; a dedicated worker runs under its own script's policy). The code graph page
 * (site/graph/) also compiles WebAssembly, its layout simulation (`wasm`).
 * Static files get the same policy through `assetHeaders` (worker/http.ts; a `_headers` file does not apply once every
 * request runs the worker first). Its own module, without the worker's generated imports, so graph/serve.ts sends the
 * same policy.
 */
export function csp({ translator = false, wasm = false } = {}): string {
	return [
		"default-src 'self'",
		translator || wasm ? "script-src 'self' 'wasm-unsafe-eval'" : "script-src 'self'",
		"style-src 'self'",
		"img-src 'self' https://avatars.githubusercontent.com https://github.com",
		translator ? `connect-src 'self' ${HUGGING_FACE}` : "connect-src 'self'",
		"frame-src 'none'",
		"form-action 'self'",
		"frame-ancestors 'none'",
		"base-uri 'none'",
		"object-src 'none'",
	].join('; ');
}
