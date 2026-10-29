/**
 * New versions of the site, picked up by every open tab on its own. A deploy restarts the presence object
 * (worker/presence.ts), which drops every socket; client/lib/presence.ts reconnects, and on every connect the object
 * sends `{t: "build", build, assets}` from gen/build.ts, handed to `build` here:
 *
 * - `assets` is not this bundle's `BLAZE_ASSETS`: the scripts or styles changed, so the page must load again;
 * - else `build` is not the one this tab last saw (the first frame after a load only notes it): only content changed,
 *   so client/nav.ts fetches the page in view again and swaps it in where it is (`blaze:refresh`).
 *
 * Either waits for a moment that loses the reader nothing: not while they type in a field with text in it or drag;
 * a hidden tab goes at once. Until then a line in the feed
 * (client/lib/feed.ts) says an update is ready, with a button to take it now. Just before, every script that owns state puts it in sessionStorage (`blaze:save`,
 * client/lib/page.ts): the page's scroll, the feed input's (client/lib/chat.ts `save`), comment drafts (client/comments.ts).
 * After the load each takes its part back once, and this clears the rest.
 *
 * After a reload whose assets still differ (a CDN behind), it doesn't reload again for LOOP_MS.
 */

import * as chat from './chat';
import * as feed from './feed';
import { KEPT, REFRESH, REFRESHED, SAVE, kept } from './page';

/** The hash of the client scripts and styles this bundle was built with (gen/build.ts, Bun.build `define`). */
declare const BLAZE_ASSETS: string;

/** When this tab last reloaded for new assets, and which: sessionStorage, so it outlives the reload. */
const TRIED = 'blaze.update.tried';
const LOOP_MS = 60_000;
const POLL_MS = 3000;
const DONE_MS = 5000;
/** A refresh client/nav.ts doesn't answer in this long (a page without the router) is loaded instead. */
const ANSWER_MS = 15_000;

type Want = { kind: 'reload' | 'refresh'; build: string; assets: string };

/** The build this tab shows, once the socket has said it. */
let seen: string | null = null;
let want: Want | null = null;
let busy = false;
let hideReady: (() => void) | null = null;
let poll: ReturnType<typeof setInterval> | undefined;
let dragging = false;

// What was saved before the reload that loaded this page: the chat takes its part now; the page's scroll and the
// "updated" line wait for the load and the first frame.
const back = kept<{ scroll?: number }>('page');
{
	const c = kept<chat.Kept>('chat');
	if (c) chat.restore(c);
}
let reloaded = back !== undefined;
const settled = () => {
	// Every page script has taken its part by now (they read it when they set up, before `load`).
	setTimeout(() => {
		if (back?.scroll !== undefined && scrollY === 0) scrollTo(0, back.scroll);
		forget();
	}, 0);
};
if (document.readyState === 'complete') settled();
else addEventListener('load', settled, { once: true });

function forget(): void {
	try {
		sessionStorage.removeItem(KEPT);
	} catch {
		// Storage refused: nothing was kept either.
	}
}

function tried(): { assets: string; at: number } | null {
	try {
		const v: unknown = JSON.parse(sessionStorage.getItem(TRIED) ?? 'null');
		return v && typeof v === 'object' && 'assets' in v && 'at' in v && typeof v.assets === 'string' && typeof v.at === 'number' ? { assets: v.assets, at: v.at } : null;
	} catch {
		return null;
	}
}

/** A `build` frame from the presence socket. */
export function build(f: { build?: unknown; assets?: unknown }): void {
	if (typeof f.build !== 'string' || typeof f.assets !== 'string') return;
	const next: Want = { kind: f.assets === BLAZE_ASSETS ? 'refresh' : 'reload', build: f.build, assets: f.assets };
	if (next.kind === 'refresh') {
		if (reloaded) {
			reloaded = false;
			done();
		}
		if (seen === null) seen = next.build;
		if (next.build === seen) {
			// Back to what this tab shows (a deploy rolled back): nothing to wait for any more.
			if (want && !busy) settle();
			return;
		}
	}
	reloaded = false;
	want = next;
	attempt();
}

/** Reloaded for these assets less than LOOP_MS ago and still got the old ones (a CDN behind): not again yet. */
function looping(assets: string): boolean {
	const t = tried();
	return !!t && t.assets === assets && Date.now() - t.at < LOOP_MS;
}

/** Whether going now would lose the reader something: a reload anything on the page, a refresh only the page's part. */
function safe(kind: Want['kind']): boolean {
	if (document.hidden) return true;
	if (dragging) return false;
	const a = document.activeElement;
	const typing = (a instanceof HTMLTextAreaElement || (a instanceof HTMLInputElement && /^(text|search|email|url)$/.test(a.type))) && a.value.trim() !== '';
	if (!typing) return true;
	return kind === 'refresh' && !a.closest('.pane > .grid');
}

function attempt(force = false): void {
	if (!want || busy || (!force && (!safe(want.kind) || (want.kind === 'reload' && looping(want.assets))))) {
		if (want) ready();
		return;
	}
	const w = want;
	save();
	if (w.kind === 'reload') {
		try {
			sessionStorage.setItem(TRIED, JSON.stringify({ assets: w.assets, at: Date.now() }));
		} catch {
			// Storage refused: no guard, but the next frame only comes on the next reconnect anyway.
		}
		location.reload();
		return;
	}
	busy = true;
	const answer = (ok: boolean) => {
		removeEventListener(REFRESHED, heard);
		clearTimeout(silent);
		busy = false;
		forget();
		if (ok) {
			seen = w.build;
			if (want === w) settle();
			done();
		} else if (want === w) {
			// The page didn't fit the frame any more (a new page in the sidebar, say): load it instead.
			want = { ...w, kind: 'reload' };
			attempt();
		}
	};
	const heard = (e: Event) => answer((e as CustomEvent<boolean>).detail === true);
	const silent = setTimeout(() => answer(false), ANSWER_MS);
	addEventListener(REFRESHED, heard);
	dispatchEvent(new Event(REFRESH));
}

/** Nothing waits any more: the feed's line and the poll go. */
function settle(): void {
	want = null;
	hideReady?.();
	hideReady = null;
	clearInterval(poll);
	poll = undefined;
}

/** Everything that owns state adds it (client/lib/page.ts SAVE), then it waits in sessionStorage for the next page. */
function save(): void {
	const bag: Record<string, unknown> = { at: Date.now(), page: { scroll: scrollY }, chat: chat.save() };
	dispatchEvent(new CustomEvent(SAVE, { detail: bag }));
	try {
		sessionStorage.setItem(KEPT, JSON.stringify(bag));
	} catch {
		// Storage refused: the page comes back as a plain load would.
	}
}

/** An update is waiting on the reader: say so, with a way to take it now, and look again every few seconds. */
function ready(): void {
	hideReady ??= feed.notice('A new version of the site is ready', 'It loads when you pause; your drafts and place are kept.', {
		action: { label: want?.kind === 'refresh' ? 'Update now' : 'Reload now', run: () => attempt(true) },
	});
	poll ??= setInterval(() => attempt(), POLL_MS);
}

function done(): void {
	feed.notice('Site updated', 'You are reading the latest version.', { ms: DONE_MS });
}

// Hiding the tab is the moment to go; otherwise the poll looks. Not on blur or pointerup: those come just before a
// click (Send, a link), which a reload would swallow.
document.addEventListener('visibilitychange', () => want && attempt());
addEventListener('pointerdown', () => (dragging = true), { capture: true, passive: true });
for (const t of ['pointerup', 'pointercancel'] as const) addEventListener(t, () => (dragging = false), { capture: true, passive: true });
