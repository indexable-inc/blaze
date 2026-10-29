/**
 * A page script's life across the client router's swaps (client/nav.ts). The router replaces the page column without
 * a load, so a script that sets up the page must run again for each page and undo itself before the next:
 *
 * - `init` runs once the script has loaded (a module runs after the document is parsed, or after the router injected
 *   it into an already swapped page), and again after every swap (`blaze:page`, fired on window);
 * - before a swap (`blaze:leave`) its signal is aborted and the teardown it returned is called. Pass the signal to
 *   every addEventListener on something that outlives the page (window, document, a MediaQueryList); stop observers,
 *   animation frames and timers in the teardown.
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

/**
 * A new version of the site is going in (client/lib/update.ts): fired on window just before a reload or an in-place
 * refresh, as a CustomEvent whose detail is a plain object. Each script that owns state worth keeping (a draft, a
 * position) adds it under its own name, JSON-safe; the object waits in sessionStorage under KEPT, and the script
 * takes its part back with `kept` when it sets up again.
 */
export const SAVE = 'blaze:save';
export const KEPT = 'blaze.update.v1';
/** Fired on window by client/lib/update.ts: fetch this page again, bypassing the prefetch cache, and swap it in. */
export const REFRESH = 'blaze:refresh';
/** Fired on window by client/nav.ts after a REFRESH (a CustomEvent, detail: whether the page was swapped in). */
export const REFRESHED = 'blaze:refreshed';
/** How long a saved state is good for: a reload that takes longer than this, or failed, starts clean. */
const KEPT_MS = 120_000;

/** The part `name` of the state saved before the last update, if one is waiting (client/lib/update.ts clears it). */
export function kept<T>(name: string): T | undefined {
	try {
		const raw = sessionStorage.getItem(KEPT);
		const v: unknown = raw === null ? null : JSON.parse(raw);
		if (!v || typeof v !== 'object' || !('at' in v) || typeof v.at !== 'number' || Date.now() - v.at > KEPT_MS) return undefined;
		return (v as Record<string, unknown>)[name] as T | undefined;
	} catch {
		return undefined;
	}
}

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
