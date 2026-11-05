/**
 * The worker's renderer: .gen/site.wasm (the Rust renderer, site/renderer) with .gen/site.cbor (the snapshot of the
 * site gen/build.ts wrote) loaded, behind worker/site-wasm.ts.
 *
 * One instance per isolate, made on the first render, with the snapshot loaded once. A trap (a Rust panic, which is
 * `panic = "abort"`) can leave the instance's memory half-updated, so the instance is dropped and the next render
 * makes a new one. Either way the caller gets an exception, never partial HTML.
 */
import wasm from '../.gen/site.wasm';
import snapshot from '../.gen/site.cbor';
import { RenderError, Site, type CommentRow, type Discussion } from './site-wasm';

export type { CommentRow, Discussion } from './site-wasm';

let site: Site | null = null;

/** The page at `url` with its comments, for `discussion`'s viewer: the whole HTML, or an exception. */
export function renderPage(url: string, comments: CommentRow[], discussion: Discussion): string {
	try {
		site ??= Site.load(wasm, new Uint8Array(snapshot));
		return site.render(url, comments, discussion);
	} catch (e) {
		// A trap: start over with a fresh instance next time. A RenderError from site_render leaves the instance sound
		// (the renderer rolls its world back), so it is kept; one from site_load never set it.
		if (!(e instanceof RenderError)) site = null;
		throw e;
	}
}
