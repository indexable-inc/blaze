/**
 * A page script's life across the client router's swaps (client/nav.ts). The router replaces the page column without
 * a load, so a script that sets up the page must run again for each page and undo itself before the next:
 *
 * - `init` runs once the script has loaded (a module runs after the document is parsed, or after the router injected
 *   it into an already swapped page), and again after every swap (`blaze:page`, fired on window);
 * - before a swap (`blaze:leave`) its signal is aborted and the teardown it returned is called. Pass the signal to
 *   every addEventListener on something that outlives the page (window, document, a MediaQueryList); stop observers,
 *   animation frames, timers and audio in the teardown.
 *
 * `init` finds what it works on with querySelector and returns early when the page has none, so a script left loaded
 * from an earlier page does nothing on a page that doesn't use it.
 *
 * Under client/lib/ so the build doesn't bundle it as an entry (gen/build.ts bundles client/*.ts); each script that
 * imports it gets its own copy, which is all it needs.
 */

export type Teardown = () => void;

/** Fired on window by the router: just before it swaps the page, and just after the new one is in place. */
export const LEAVE = 'blaze:leave';
export const PAGE = 'blaze:page';
/** Fired on window by client/lib/presence.ts when a doc's comments changed elsewhere (a CustomEvent, detail: the doc). */
export const COMMENTS = 'blaze:comments';
/** Fired on window by client/comments.ts after it swapped threads in, so the comment count can be redrawn. */
export const THREADS = 'blaze:threads';

export function onPage(init: (signal: AbortSignal) => Teardown | void): void {
	let stop: Teardown | null = null;
	const run = () => {
		stop?.();
		const ac = new AbortController();
		let teardown: Teardown | void = undefined;
		try {
			teardown = init(ac.signal);
		} catch (e) {
			console.error(e);
		}
		stop = () => {
			stop = null;
			ac.abort();
			try {
				teardown?.();
			} catch (e) {
				console.error(e);
			}
		};
	};
	addEventListener(LEAVE, () => stop?.());
	addEventListener(PAGE, run);
	if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', run, { once: true });
	else run();
}
