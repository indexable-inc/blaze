/**
 * Inline comments on a discussable page (`main[data-doc]`). The worker renders everything (the Rust renderer, renderer/core/src/comments.rs):
 * each block's count button and thread, the forms, the `_page` discussion. Without JS that page works by itself
 * (threads open on :target, forms post and redirect). This script only enhances it:
 *
 * - buttons toggle their thread in place instead of jumping to it;
 * - posting and deleting go through the JSON API, then the affected thread is swapped for the worker's fresh
 *   rendering of it (one GET of the page), so the markup always comes from the one renderer;
 * - times show relative ("3 minutes ago");
 * - `#c-<id>` opens that comment's thread and highlights it; `#block-<id>` (where sign-in returns to) opens that
 *   block's thread and focuses its form.
 *
 * Nothing here builds HTML from strings: text goes in with textContent, markup comes parsed from the worker's page.
 *
 * Before a site update reloads or swaps the page (client/lib/update.ts), the comments being written and the threads
 * open are saved, by doc and block, and put back when the page sets up again.
 *
 * Set up again on every page (client/lib/page.ts). The page the client router swapped in was fetched with the reader's
 * cookie, so its threads, forms and sign-in state are the reader's, as on a load.
 */

import { COMMENTS, SAVE, THREADS, kept, onPage } from './lib/page';

const PAGE_THREAD = 'block-_page';

/** What a site update keeps (client/lib/update.ts): drafts by block, and the threads open, for one doc. */
type Kept = { doc: string; drafts: Record<string, string>; open: string[] };

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls?: string, text?: string): HTMLElementTagNameMap[K] {
	const e = document.createElement(tag);
	if (cls) e.className = cls;
	if (text !== undefined) e.textContent = text;
	return e;
}

async function errorOf(res: Response): Promise<string> {
	try {
		const j: unknown = await res.json();
		if (j && typeof j === 'object' && 'error' in j && typeof j.error === 'string') return j.error;
	} catch {
		// not JSON
	}
	return res.status === 401 ? 'Sign in to comment.' : res.status === 429 ? 'Too many comments; try again in a minute.' : `Request failed (${res.status}).`;
}

const RTF = new Intl.RelativeTimeFormat('en', { numeric: 'auto' });
function ago(iso: string): string {
	const t = Date.parse(iso);
	if (Number.isNaN(t)) return '';
	const s = (t - Date.now()) / 1000;
	const steps: [number, Intl.RelativeTimeFormatUnit][] = [
		[60, 'second'],
		[3600, 'minute'],
		[86400, 'hour'],
		[86400 * 7, 'day'],
		[86400 * 30, 'week'],
		[86400 * 365, 'month'],
	];
	const sizes: Record<string, number> = { second: 1, minute: 60, hour: 3600, day: 86400, week: 86400 * 7, month: 86400 * 30, year: 86400 * 365 };
	for (const [limit, unit] of steps) if (Math.abs(s) < limit) return RTF.format(Math.round(s / sizes[unit]!), unit);
	return RTF.format(Math.round(s / sizes.year!), 'year');
}

function relativeTimes(root: ParentNode) {
	for (const t of root.querySelectorAll<HTMLTimeElement>('.c-item time[datetime]')) {
		const rel = ago(t.dateTime);
		if (!rel) continue;
		t.title = t.textContent ?? '';
		t.textContent = rel;
	}
}

function start(main: HTMLElement, signal: AbortSignal) {
	const threadOf = (id: string) => document.getElementById(id) as HTMLElement | null;
	const buttonOf = (threadId: string) => main.querySelector<HTMLAnchorElement>(`.c-btn[aria-controls="${CSS.escape(threadId)}"]`);

	function setOpen(thread: HTMLElement, open: boolean) {
		thread.hidden = !open;
		buttonOf(thread.id)?.setAttribute('aria-expanded', String(open));
	}

	function open(thread: HTMLElement, focus: boolean) {
		setOpen(thread, true);
		if (focus) thread.querySelector('textarea')?.focus({ preventScroll: false });
	}

	/** Swap a thread (and its button) for the worker's current rendering of it. */
	async function reload(threadId: string) {
		const res = await fetch(location.pathname, { headers: { Accept: 'text/html' }, cache: 'no-store', credentials: 'same-origin' });
		if (!res.ok) throw new Error(`Could not reload comments (${res.status}).`);
		const fresh = new DOMParser().parseFromString(await res.text(), 'text/html');
		const next = fresh.getElementById(threadId);
		const old = threadOf(threadId);
		if (!next || !old) throw new Error('Could not reload comments.');
		const nextButton = fresh.querySelector(`.c-btn[aria-controls="${CSS.escape(threadId)}"]`);
		const oldButton = buttonOf(threadId);
		const thread = document.adoptNode(next);
		relativeTimes(thread);
		old.replaceWith(thread);
		if (nextButton && oldButton) {
			const b = document.adoptNode(nextButton);
			b.setAttribute('role', 'button');
			oldButton.replaceWith(b);
		}
		setOpen(thread, true);
		dispatchEvent(new Event(THREADS));
	}

	/**
	 * Someone posted or deleted a comment on this page (client/lib/presence.ts relays the worker's word): every
	 * thread's list and count button are swapped for the worker's current rendering. The forms stay, so a comment
	 * being written is kept, and so does what is open. A comment that is new here is marked for a moment.
	 */
	async function refresh() {
		const res = await fetch(location.pathname, { headers: { Accept: 'text/html' }, cache: 'no-store', credentials: 'same-origin' });
		if (!res.ok || signal.aborted) return;
		const fresh = new DOMParser().parseFromString(await res.text(), 'text/html');
		if (signal.aborted) return;
		const seen = new Set([...main.querySelectorAll('.c-item')].map((i) => i.id));
		for (const next of fresh.querySelectorAll<HTMLElement>('.c-thread')) {
			const old = threadOf(next.id);
			const list = next.querySelector('.c-list');
			const oldList = old?.querySelector('.c-list');
			if (!old || !list || !oldList) continue;
			const adopted = document.adoptNode(list);
			relativeTimes(adopted);
			for (const item of adopted.querySelectorAll('.c-item')) if (!seen.has(item.id)) item.classList.add('c-new');
			oldList.replaceWith(adopted);
			const nextButton = fresh.querySelector(`.c-btn[aria-controls="${CSS.escape(next.id)}"]`);
			const oldButton = buttonOf(next.id);
			if (nextButton && oldButton) {
				const b = document.adoptNode(nextButton);
				b.setAttribute('role', 'button');
				b.setAttribute('aria-expanded', String(!old.hidden));
				oldButton.replaceWith(b);
			}
		}
		dispatchEvent(new Event(THREADS));
	}

	function mark(item: HTMLElement) {
		for (const old of main.querySelectorAll('.c-item.c-target')) old.classList.remove('c-target');
		item.classList.add('c-target');
		item.scrollIntoView({ block: 'center' });
	}

	async function post(form: HTMLFormElement) {
		const ta = form.querySelector('textarea');
		const button = form.querySelector<HTMLButtonElement>('button[type=submit]:not([form])');
		const err = form.querySelector<HTMLElement>('p.error');
		const thread = form.closest<HTMLElement>('.c-thread');
		const data = new FormData(form);
		const body = String(data.get('body') ?? '').trim();
		if (!ta || !button || !thread || !body || button.disabled) return;
		button.disabled = true;
		button.textContent = 'Posting…';
		if (err) err.hidden = true;
		try {
			// The form's action is the API's URL under the site's base (the renderer writes it).
			const res = await fetch(form.action, {
				method: 'POST',
				headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
				body: JSON.stringify({ doc: data.get('doc'), block: data.get('block'), body }),
			});
			if (!res.ok) throw new Error(await errorOf(res));
			const j = (await res.json()) as { comment: { id: number } };
			await reload(thread.id);
			const item = document.getElementById(`c-${j.comment.id}`);
			if (item) mark(item);
		} catch (e) {
			if (err) {
				err.textContent = e instanceof Error ? e.message : String(e);
				err.hidden = false;
			}
			button.disabled = false;
			button.textContent = 'Post';
		}
	}

	async function remove(form: HTMLFormElement) {
		const m = /\/api\/comments\/([0-9]+)\/delete$/.exec(form.action);
		const thread = form.closest<HTMLElement>('.c-thread');
		const at = form.closest('.c-main');
		const del = form.querySelector('button');
		if (!m || !thread || !at || !del || del.disabled) return;
		if (!confirm('Delete this comment?')) return;
		del.disabled = true;
		at.querySelector(':scope > .error')?.remove();
		try {
			// The form posts to <base>/api/comments/<id>/delete; the JSON API is the same URL without /delete.
			const res = await fetch(form.action.slice(0, -'/delete'.length), { method: 'DELETE' });
			if (!res.ok) throw new Error(await errorOf(res));
			await reload(thread.id);
		} catch (e) {
			del.disabled = false;
			at.appendChild(el('p', 'error', e instanceof Error ? e.message : String(e)));
		}
	}

	function followHash() {
		let hash: string;
		try {
			hash = decodeURIComponent(location.hash.slice(1));
		} catch {
			return; // a malformed escape such as "#%E0": not one of ours
		}
		if (hash.startsWith('c-')) {
			const item = document.getElementById(hash);
			const thread = item?.closest<HTMLElement>('.c-thread');
			if (!item || !thread) return;
			open(thread, false);
			mark(item);
		} else if (hash.startsWith('block-')) {
			const thread = threadOf(hash);
			if (!thread?.classList.contains('c-thread')) return;
			open(thread, false);
			thread.scrollIntoView({ block: 'center' });
			thread.querySelector('textarea')?.focus({ preventScroll: true });
		}
	}

	// Take over: from here the threads are shown by `hidden`, not by :target (styles/site.css).
	main.classList.add('c-live');
	for (const b of main.querySelectorAll<HTMLAnchorElement>('.c-btn')) b.setAttribute('role', 'button');
	for (const t of main.querySelectorAll<HTMLElement>('.c-thread')) setOpen(t, t.id === PAGE_THREAD);
	relativeTimes(main);

	main.addEventListener('click', (e) => {
		const b = e.target instanceof Element ? e.target.closest<HTMLAnchorElement>('.c-btn') : null;
		const id = b?.getAttribute('aria-controls');
		const thread = id ? threadOf(id) : null;
		if (!thread) return;
		e.preventDefault();
		if (thread.hidden) open(thread, true);
		else setOpen(thread, false);
	}, { signal });
	main.addEventListener('submit', (e) => {
		const f = e.target;
		if (!(f instanceof HTMLFormElement)) return;
		if (f.classList.contains('c-compose')) {
			e.preventDefault();
			void post(f);
		} else if (f.classList.contains('c-delform')) {
			e.preventDefault();
			void remove(f);
		}
	}, { signal });
	main.addEventListener('keydown', (e) => {
		const ta = e.target;
		if (!(ta instanceof HTMLTextAreaElement) || !(e.key === 'Enter' && (e.metaKey || e.ctrlKey))) return;
		const f = ta.closest<HTMLFormElement>('form.c-compose');
		if (!f) return;
		e.preventDefault();
		void post(f);
	}, { signal });
	window.addEventListener('hashchange', followHash, { signal });
	const blockOf = (f: HTMLFormElement) => (f.elements.namedItem('block') as HTMLInputElement | null)?.value ?? '';
	window.addEventListener(SAVE, (e) => {
		const drafts: Record<string, string> = {};
		for (const f of main.querySelectorAll<HTMLFormElement>('form.c-compose')) {
			const v = f.querySelector('textarea')?.value;
			if (v?.trim()) drafts[blockOf(f)] = v;
		}
		const open = [...main.querySelectorAll<HTMLElement>('.c-thread')].filter((t) => !t.hidden).map((t) => t.id);
		(e as CustomEvent<Record<string, unknown>>).detail.comments = { doc: main.dataset.doc ?? '', drafts, open } satisfies Kept;
	}, { signal });
	const back = kept<Kept>('comments');
	if (back && back.doc === main.dataset.doc) {
		for (const id of back.open ?? []) {
			const t = threadOf(id);
			if (t?.classList.contains('c-thread')) setOpen(t, true);
		}
		for (const f of main.querySelectorAll<HTMLFormElement>('form.c-compose')) {
			const v = back.drafts?.[blockOf(f)];
			const ta = f.querySelector('textarea');
			const t = f.closest<HTMLElement>('.c-thread');
			if (!v || !ta) continue;
			ta.value = v;
			if (t) setOpen(t, true);
		}
	}
	// The worker's page cache keys on a doc version each isolate memoises for 2 s (worker/pages.ts): wait that out.
	let pending: ReturnType<typeof setTimeout> | undefined;
	window.addEventListener(COMMENTS, (e) => {
		if ((e as CustomEvent<string>).detail !== main.dataset.doc) return;
		clearTimeout(pending);
		pending = setTimeout(() => void refresh().catch(() => {}), 2200);
	}, { signal });
	signal.addEventListener('abort', () => clearTimeout(pending));
	followHash();
}

onPage((signal) => {
	const main = document.querySelector<HTMLElement>('main[data-doc]');
	if (main) start(main, signal);
});
