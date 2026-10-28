/**
 * The client router: moving from page to page of the site without a document load, as the ix docs' SvelteKit router
 * does. Every page has the same frame (renderer core render.rs): the sidebar, then `.pane` with the phone bar and
 * `.grid`, which holds everything a page has of its own (main, the pager, the outline). So a navigation fetches the
 * next page's HTML, parses it and swaps only what differs:
 *
 * - `.grid`, whole;
 * - the head's `<title>`, description, canonical and `og:` tags, and `<html>`'s lang and class when they differ;
 * - in the sidebar, only the open row (`aria-current`), its section (`data-here`, opened) and its scroll when the row
 *   is out of view. The sidebar itself is never re-rendered, so its scroll, folds and hover pill stay;
 * - the module scripts the new page lists that this document hasn't loaded yet, which are added to the head.
 *
 * The page scripts set themselves up again from the `blaze:page` event fired after the swap, and undo themselves on
 * `blaze:leave` before it (client/lib/page.ts). The swap is a quick cross-fade (a same-document view transition,
 * styles/shell.css; none under prefers-reduced-motion), then the page scrolls to the top or the URL's hash (or, going
 * back and forward, to where that entry was left), focus moves to the new h1, and the URL is pushed.
 *
 * Links are fetched ahead on hover, touchstart and focus into a small in-memory cache, so most clicks swap at once.
 *
 * A page is swapped in only when it was built with the same scripts and styles as this document (the worker's
 * `x-blaze-assets` header against `BLAZE_ASSETS`, both from gen/build.ts): after a deploy, the next click is a load.
 *
 * A deploy that changed only content (client/lib/update.ts asks with `blaze:refresh`) fetches the page in view again,
 * past the cache, and swaps it in where it is: no history entry, the scroll and focus kept, the sidebar left alone.
 * It answers with `blaze:refreshed` (whether it could); it never falls back to a load itself.
 *
 * Only left clicks with no modifier on a same-origin link under the site's base are taken: not downloads, links with
 * a target, hash links within the page (the browser scrolls), the worker's routes that act (/api, /auth) or serve
 * files (/audio), or files (`install.sh`, `search.json`). Anything that goes wrong (not 200, not HTML, a redirect out of
 * the site, a network error, a page in another frame, a page with a script from another origin, which needs its own
 * CSP: /updates with Turnstile) falls back to the browser's own navigation to the same URL. Without JS every link is
 * a plain link.
 */

import { LEAVE, PAGE, REFRESH, REFRESHED } from './lib/page';

/** The hash of the client scripts and styles this bundle was built with (gen/build.ts, Bun.build `define`). */
declare const BLAZE_ASSETS: string;

type Got = { url: string; html: string; live: boolean };
type How = { mode: 'push' | 'replace' | 'pop' | 'refresh'; scroll?: number };

class Miss extends Error {}

/** The site's base (`/blaze/`), from the stylesheet the renderer linked as `<base>/site.css`. */
const BASE = (() => {
	const css = document.querySelector<HTMLLinkElement>('link[rel="stylesheet"][href$="site.css"]');
	return css ? new URL('.', css.href).pathname : '/';
})();
/** The worker's routes that do something or serve a file rather than a page (worker/index.ts). */
const NOT_PAGES = ['api/', 'auth/', 'audio/'].map((p) => BASE + p);
/** How long a fetched page is kept: a page with comments (rendered per request) briefly, the static ones longer. */
const KEEP_LIVE_MS = 15_000;
const KEEP_MS = 300_000;
/** Hovering this long fetches the link ahead (a pointer passing over the sidebar shouldn't fetch every row). */
const HOVER_MS = 40;
const HEADS = 'meta[name="description"], link[rel="canonical"], meta[property^="og:"]';

const still = matchMedia('(prefers-reduced-motion: reduce)');
const here = () => location.pathname + location.search;

/** The page in view: `here()` once a swap is done; during a back or forward fetch the URL is already the next one. */
let shown = here();

function routable(url: URL): boolean {
	if (url.origin !== location.origin) return false;
	const p = url.pathname;
	if (p !== BASE.slice(0, -1) && !p.startsWith(BASE)) return false;
	if (NOT_PAGES.some((n) => p.startsWith(n))) return false;
	// A file, not a page: anything with an extension but .html.
	const last = p.slice(p.lastIndexOf('/') + 1);
	return !/\.[a-z0-9]+$/i.test(last) || last.endsWith('.html');
}

/** The link an event is on, if it is one the router may take. */
function linkOf(e: Event): HTMLAnchorElement | null {
	const a = e.target instanceof Element ? e.target.closest('a') : null;
	if (!(a instanceof HTMLAnchorElement) || !a.href) return null;
	const raw = a.getAttribute('href') ?? '';
	if (raw.startsWith('#') || a.hasAttribute('download') || (a.target && a.target !== '_self')) return null;
	if (a.relList.contains('external')) return null;
	const url = new URL(a.href);
	return routable(url) ? a : null;
}

// Pages fetched, or being fetched, by URL without the hash.
const cache = new Map<string, Promise<Got>>();
const key = (u: URL) => u.origin + u.pathname + u.search;

async function fetchPage(url: URL, fresh = false): Promise<Got> {
	// Fresh: revalidated with the server, so a page the browser kept from before a deploy isn't swapped back in.
	const res = await fetch(key(url), { headers: { Accept: 'text/html' }, credentials: 'same-origin', cache: fresh ? 'no-cache' : 'default' });
	if (!res.ok) throw new Miss(`HTTP ${res.status}`);
	if (!/^text\/html\b/.test(res.headers.get('content-type') ?? '')) throw new Miss('not HTML');
	if (res.headers.get('x-blaze-assets') !== BLAZE_ASSETS) throw new Miss('built with other scripts or styles');
	const final = new URL(res.url);
	if (!routable(final)) throw new Miss(`redirected to ${final.href}`);
	const html = await res.text();
	// Live: rendered for this request (comments, the reader, /updates' referrer), so used once and kept briefly.
	const live = /<main\b[^>]*\sdata-doc="/.test(html) || /no-store|private/.test(res.headers.get('cache-control') ?? '');
	return { url: final.pathname + final.search, html, live };
}

function load(url: URL): Promise<Got> {
	const k = key(url);
	const hit = cache.get(k);
	if (hit) return hit;
	const p = fetchPage(url);
	cache.set(k, p);
	const drop = () => cache.get(k) === p && cache.delete(k);
	p.then(
		(g) => setTimeout(drop, g.live ? KEEP_LIVE_MS : KEEP_MS),
		() => drop(),
	);
	return p;
}

function ahead(a: HTMLAnchorElement) {
	const url = new URL(a.href);
	if (key(url) === location.origin + here()) return;
	load(url).catch(() => {
		// Fetched again on the click, which falls back to a load if it fails again.
	});
}

/** The new page fits this frame: the same sidebar rows, the same stylesheets, no script from another origin. */
function fits(doc: Document): boolean {
	if (!doc.querySelector('.pane > .grid') || !document.querySelector('.pane > .grid')) return false;
	const rows = (d: Document) => [...d.querySelectorAll('.navlist a.row')].map((a) => a.getAttribute('href')).join('\n');
	if (rows(doc) !== rows(document)) return false;
	const sheets = (d: Document) => [...d.querySelectorAll('link[rel="stylesheet"]')].map((l) => l.getAttribute('href')).join('\n');
	if (sheets(doc) !== sheets(document)) return false;
	// A script from elsewhere that this page doesn't have would need a load to run. One both have, like the analytics
	// beacon Cloudflare adds to every page, is not a different frame.
	const have = new Set([...document.querySelectorAll('script[src]')].map((s) => s.getAttribute('src')));
	return [...doc.querySelectorAll('script[src]')].every((s) => have.has(s.getAttribute('src')) || new URL(s.getAttribute('src')!, location.href).origin === location.origin);
}

function saveScroll() {
	// Only into the entry of the page in view: during a back or forward fetch, the entry is already the next page's.
	if (here() !== shown) return;
	try {
		history.replaceState({ ...(history.state ?? {}), blaze: 1, scroll: scrollY }, '');
	} catch {
		// replaceState throttled: this entry keeps its last saved position.
	}
}

function swapHead(doc: Document) {
	document.title = doc.title;
	for (const m of document.head.querySelectorAll(HEADS)) m.remove();
	const before = document.head.querySelector('link[rel="stylesheet"]');
	for (const m of doc.head.querySelectorAll(HEADS)) document.head.insertBefore(document.importNode(m, true), before);
	const [from, to] = [document.documentElement, doc.documentElement];
	if (from.lang !== to.lang) from.lang = to.lang;
	if (from.className !== to.className) from.className = to.className;
}

/** The sidebar's open row and section, from the new page's. */
function swapSide(doc: Document, scroll = true) {
	const href = doc.querySelector('.navlist [aria-current="page"]')?.getAttribute('href') ?? null;
	for (const r of document.querySelectorAll('.navlist [aria-current]')) r.removeAttribute('aria-current');
	const open = new Set([...doc.querySelectorAll<HTMLElement>('details.sec[data-here]')].map((d) => d.dataset.sec));
	for (const sec of document.querySelectorAll<HTMLDetailsElement>('details.sec[data-sec]')) {
		if (open.has(sec.dataset.sec)) {
			sec.dataset.here = '';
			sec.open = true;
		} else delete sec.dataset.here;
	}
	if (href === null) return;
	const row = [...document.querySelectorAll<HTMLAnchorElement>('.navlist a.row')].find((a) => a.getAttribute('href') === href);
	if (!row) return;
	row.setAttribute('aria-current', 'page');
	for (let d = row.parentElement?.closest('details'); d; d = d.parentElement?.closest('details')) d.open = true;
	// Into view if it is out of it, as a load would centre it; otherwise the sidebar doesn't move.
	if (!scroll) return;
	const box = document.querySelector<HTMLElement>('.side .scrollbox');
	if (!box || box.clientHeight === 0) return;
	const a = row.getBoundingClientRect();
	const b = box.getBoundingClientRect();
	if (a.top < b.top || a.bottom > b.bottom) box.scrollTop += a.top - b.top - box.clientHeight / 2 + row.offsetHeight / 2;
}

/**
 * The module scripts the new page lists that this document hasn't run, loaded before the swap so the new page is
 * whole when it shows. Each one's `onPage` runs once now, on the old page, where it finds nothing of its own, then
 * takes the new page from `blaze:page` like the rest. `import()` shares the module map with `<script type=module>`,
 * so a script runs once whichever way it came.
 */
const imported = new Set<string>();
async function loadScripts(doc: Document) {
	const have = new Set([...document.scripts].map((s) => s.src).filter(Boolean));
	const missing = [...doc.querySelectorAll<HTMLScriptElement>('script[type="module"][src]')]
		.map((s) => new URL(s.getAttribute('src')!, location.href).href)
		.filter((src) => !have.has(src) && !imported.has(src));
	await Promise.all(missing.map((src) => import(src)));
	for (const src of missing) imported.add(src);
}

function target(hash: string): Element | null {
	if (hash.length < 2) return null;
	try {
		return document.getElementById(decodeURIComponent(hash.slice(1)));
	} catch {
		return null;
	}
}

function place(how: How) {
	const t = target(location.hash);
	if (how.scroll !== undefined) scrollTo(0, how.scroll);
	else if (t) t.scrollIntoView();
	else scrollTo(0, 0);
}

function focusTitle() {
	const h1 = document.querySelector<HTMLElement>('.grid main h1');
	if (!h1) return;
	if (!h1.hasAttribute('tabindex')) h1.tabIndex = -1;
	h1.focus({ preventScroll: true });
}

let seq = 0;

function fallback(url: URL, how: How) {
	if (how.mode === 'refresh') dispatchEvent(new CustomEvent(REFRESHED, { detail: false }));
	else if (how.mode === 'pop') location.reload();
	else location.assign(url.href);
}

async function go(url: URL, how: How) {
	const n = ++seq;
	if (how.mode === 'push' || how.mode === 'replace') saveScroll();
	// A slow fetch shows the progress cursor, as a load would show its bar.
	const wait = setTimeout(() => document.documentElement.classList.add('nav-wait'), 150);
	let got: Got;
	let doc: Document;
	try {
		got = how.mode === 'refresh' ? await fetchPage(url, true) : await load(url);
		if (n !== seq) return;
		doc = new DOMParser().parseFromString(got.html, 'text/html');
		if (!fits(doc)) throw new Miss('a different frame');
		await loadScripts(doc);
		if (n !== seq) return;
	} catch (e) {
		if (n !== seq) return;
		console.info('nav: a full load:', e instanceof Error ? e.message : e);
		fallback(url, how);
		return;
	} finally {
		clearTimeout(wait);
		document.documentElement.classList.remove('nav-wait');
	}
	if (got.live) cache.delete(key(url));
	const next = new URL(got.url + url.hash, location.href);

	const apply = () => {
		// A later navigation (a click, back or forward) started while this one waited for the transition: it wins.
		if (n !== seq) return;
		dispatchEvent(new Event(LEAVE));
		const refresh = how.mode === 'refresh';
		if (how.mode === 'push') history.pushState({ blaze: 1, scroll: 0 }, '', next.href);
		else if (how.mode === 'replace') history.replaceState({ blaze: 1, scroll: 0 }, '', next.href);
		shown = here();
		swapHead(doc);
		document.querySelector('.pane > .grid')!.replaceWith(document.adoptNode(doc.querySelector('.pane > .grid')!));
		swapSide(doc, !refresh);
		place(how);
		// Before the page scripts, so one that focuses something of its own (a comment form for #block-x) keeps it.
		// A refresh leaves the focus where the reader has it (the chat, the search), unless it was in the old page.
		if (!refresh || !document.activeElement || document.activeElement === document.body) focusTitle();
		dispatchEvent(new Event(PAGE));
		if (refresh) dispatchEvent(new CustomEvent(REFRESHED, { detail: true }));
	};
	try {
		// A refresh is the same page: no cross-fade.
		if (still.matches || how.mode === 'refresh' || typeof document.startViewTransition !== 'function') apply();
		else await document.startViewTransition(apply).updateCallbackDone;
	} catch (e) {
		// Half swapped: the browser loads the URL now in the address bar, which is the new page's.
		console.error('nav:', e);
		location.reload();
	}
}

document.addEventListener('click', (e) => {
	if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
	const raw = e.target instanceof Element ? e.target.closest('a')?.getAttribute('href') : null;
	if (raw?.startsWith('#')) {
		// The browser adds an entry for the hash: this entry keeps where the reader was.
		clearTimeout(scrolled);
		saveScroll();
		return;
	}
	const a = linkOf(e);
	if (!a) return;
	const url = new URL(a.href);
	const same = key(url) === location.origin + here();
	// A hash within this page: the browser scrolls to it (and the entry's scroll is saved first).
	if (same && url.hash) {
		clearTimeout(scrolled);
		saveScroll();
		return;
	}
	e.preventDefault();
	void go(url, { mode: same ? 'replace' : 'push' });
});

let hover: { a: HTMLAnchorElement; timer: ReturnType<typeof setTimeout> } | null = null;
document.addEventListener(
	'pointerover',
	(e) => {
		const a = linkOf(e);
		if (a === hover?.a) return;
		if (hover) clearTimeout(hover.timer);
		hover = a ? { a, timer: setTimeout(() => ahead(a), HOVER_MS) } : null;
	},
	{ passive: true },
);
document.addEventListener(
	'touchstart',
	(e) => {
		const a = linkOf(e);
		if (a) ahead(a);
	},
	{ passive: true },
);
document.addEventListener('focusin', (e) => {
	const a = linkOf(e);
	if (a) ahead(a);
});

// Back and forward. The entries this document made restore their scroll here (manual restoration); a move between
// two entries of the same page (a hash) only scrolls.
addEventListener('popstate', (e) => {
	clearTimeout(scrolled);
	const state: unknown = e.state;
	const scroll = state && typeof state === 'object' && 'scroll' in state && typeof state.scroll === 'number' ? state.scroll : undefined;
	if (here() === shown) {
		// Back on the page in view (forward again during a back fetch, or a hash): drop any navigation in flight.
		++seq;
		document.documentElement.classList.remove('nav-wait');
		const t = target(location.hash);
		if (scroll !== undefined) scrollTo(0, scroll);
		else if (t) t.scrollIntoView();
		return;
	}
	void go(new URL(location.href), { mode: 'pop', scroll });
});

// The browser's own scroll restoration would scroll the old page as the URL changes, before the new one is in, so this
// document restores every entry itself, including on a reload or a traversal that missed the back/forward cache (the
// position was saved as the reader scrolled and as the page went). Once more at `load`, in case the page was still
// too short to reach it.
let scrolled: ReturnType<typeof setTimeout> | undefined;
{
	const nav = performance.getEntriesByType('navigation')[0] as PerformanceNavigationTiming | undefined;
	const s: unknown = history.state;
	const want = s && typeof s === 'object' && 'scroll' in s && typeof s.scroll === 'number' ? s.scroll : undefined;
	history.scrollRestoration = 'manual';
	if (want !== undefined && (nav?.type === 'reload' || nav?.type === 'back_forward') && !target(location.hash)) {
		scrollTo(0, want);
		if (document.readyState !== 'complete') addEventListener('load', () => scrollY === 0 && scrollTo(0, want), { once: true });
	}
	saveScroll();
	addEventListener('pagehide', saveScroll);
	// This entry's scroll, kept as the reader scrolls, for coming back to it.
	addEventListener(
		'scroll',
		() => {
			clearTimeout(scrolled);
			scrolled = setTimeout(saveScroll, 150);
		},
		{ passive: true },
	);
}

// A deploy changed the content (client/lib/update.ts): every page kept ahead may be the old one, and this one is.
addEventListener(REFRESH, () => {
	cache.clear();
	const n = seq + 1;
	void go(new URL(location.href), { mode: 'refresh', scroll: scrollY }).then(() => {
		// A click, back or forward took over: that page came past the cleared cache, so it is the new one too.
		if (seq !== n) dispatchEvent(new CustomEvent(REFRESHED, { detail: true }));
	});
});
