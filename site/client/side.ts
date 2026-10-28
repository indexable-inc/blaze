/**
 * The sidebar on every page (renderer core side.rs, styles/shell.css). Everything here is an enhancement: without it
 * sections still fold (`<details>`), the phone drawer still opens (`#menu` and `:target`) and the outline is links.
 *
 * - Folds: a section the reader folded stays folded on the next page (localStorage), except the one holding the page.
 *   A fold slides open and shut as the ix sidebar's (svelte/transition slide, 180 ms). The sidebar's scroll carries
 *   over too (sessionStorage, written on pagehide); keep.ts puts both back before the next page's first paint.
 * - Rows: ix's hover pill (lib/site/glide.ts) slides from row to row under the pointer. A lesson row's mark shows the
 *   reader's progress (client/lib/progress.ts), painted on every page.
 * - Reference folds in the phone drawer until the reader opens it there (blaze.side.opened; keep.ts applies it).
 * - The drawer: Menu opens it, the scrim, the close button and Escape close it, without touching the URL.
 * - Search: the pill (and ⌘K, Ctrl K or /) opens a dialog over `search.json` (gen/build.ts writes it: one entry per
 *   page and per section, with its text); every word typed must appear, title and heading matches rank first.
 * - The outline: the section being read is marked, as the ix docs' "On this page" does.
 *
 * The sidebar outlives the client router's swaps (client/nav.ts), so everything but the outline is set up once; the
 * outline is the page's own and is set up again on every page (client/lib/page.ts), and a swap closes the drawer.
 *
 * Every page has this script, so it also brings in who is here (client/lib/presence.ts): the top line and the faces
 * on the rows.
 */

import { PAGE, onPage } from './lib/page';
import './lib/presence';
import './lib/tip';
import { SIDE, handle } from './lib/resize';
import { KEY as PROGRESS, load, stateOf, stateWords } from './lib/progress';

type Entry = { u: string; t: string; h: string; x: string };

const FOLDED = 'blaze.side.folded';
/** FoldOnPhone sections (Reference) the reader opened in the phone drawer: they stay open there. */
const OPENED = 'blaze.side.opened';
const phone = matchMedia('(max-width: 800px)');

function readFolded(key = FOLDED): Set<string> {
	try {
		const raw = localStorage.getItem(key);
		const v: unknown = raw === null ? [] : JSON.parse(raw);
		return new Set(Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : []);
	} catch {
		return new Set();
	}
}

function writeFolded(s: Set<string>, key = FOLDED) {
	try {
		localStorage.setItem(key, JSON.stringify([...s]));
	} catch {
		// Private mode or storage off: folds just don't carry over.
	}
}

/** svelte/transition's slide as the ix sidebar runs it: 180 ms of cubicOut, nothing when motion is reduced. */
const SLIDE: KeyframeAnimationOptions = { duration: 180, easing: 'cubic-bezier(0.33, 1, 0.68, 1)' };
const still = matchMedia('(prefers-reduced-motion: reduce)');

/**
 * A `<details>` body slides between nothing and its height (and vertical margins), as slide does. Native `<details>`
 * doesn't animate in every browser (::details-content with interpolate-size is Chromium only), so the summary's
 * click is taken over here. While it slides shut the details stays open, marked `.closing` so the caret turns now.
 */
function slide(d: HTMLDetailsElement) {
	const head = d.querySelector<HTMLElement>(':scope > summary');
	const body = d.querySelector<HTMLElement>(':scope > div');
	if (!head || !body) return;
	const shut: Keyframe = { height: '0px', marginTop: '0px', marginBottom: '0px' };
	const size = (): Keyframe => {
		const s = getComputedStyle(body);
		return { height: s.height, marginTop: s.marginTop, marginBottom: s.marginBottom };
	};
	let run: Animation | null = null;
	head.addEventListener('click', (e) => {
		if (still.matches || typeof body.animate !== 'function') return;
		e.preventDefault();
		const closing = d.open && !d.classList.contains('closing');
		// A click mid-slide turns it around from where it is.
		const from = d.open ? size() : shut;
		run?.cancel();
		d.open = true;
		d.classList.toggle('closing', closing);
		const to = closing ? shut : size();
		body.style.overflow = 'hidden';
		run = body.animate([from, to], SLIDE);
		run.onfinish = () => {
			run = null;
			body.style.overflow = '';
			d.classList.remove('closing');
			if (closing) d.open = false;
		};
	});
}

function folds() {
	for (const d of document.querySelectorAll<HTMLDetailsElement>('details.sec, details.fold')) slide(d);
	for (const sec of document.querySelectorAll<HTMLDetailsElement>('details.sec[data-sec]')) {
		const key = sec.dataset.sec!;
		sec.addEventListener('toggle', () => {
			if (phone.matches && 'phoneFold' in sec.dataset) {
				const opened = readFolded(OPENED);
				if (sec.open) opened.add(key);
				else opened.delete(key);
				writeFolded(opened, OPENED);
				return;
			}
			const now = readFolded();
			if (sec.open) now.delete(key);
			else now.add(key);
			writeFolded(now);
		});
	}
	// keep.ts already closed the folded sections and put the scroll back, before the first paint. The scroll is saved
	// as the page goes; the open page's row is centred only when nothing was kept or the kept scroll hides it.
	const row = document.querySelector<HTMLElement>('.navlist [aria-current="page"]');
	const box = document.querySelector<HTMLElement>('.side .scrollbox');
	if (!box) return;
	addEventListener('pagehide', () => {
		try {
			sessionStorage.setItem('blaze.side.scroll', String(box.scrollTop));
		} catch {
			// Storage off: the next page centres its row instead.
		}
	});
	const shown = (r: HTMLElement) => {
		const a = r.getBoundingClientRect();
		const b = box.getBoundingClientRect();
		return a.top >= b.top && a.bottom <= b.bottom;
	};
	if (row && !('kept' in box.dataset && shown(row))) box.scrollTop += row.getBoundingClientRect().top - box.getBoundingClientRect().top - box.clientHeight / 2 + row.offsetHeight / 2;
}

/**
 * ix's hover pill (lib/site/glide.ts): one tint under the rows, placed at once on the first row the pointer touches,
 * then sliding from row to row (shell.css times it); it fades when the pointer leaves the list. `data-glide` drops
 * the rows' own :hover tint, the stand-in without JS. The open row keeps its own tint: ix's resting pill looks for
 * `aria-selected`, which its rows never set, and a page load here would only ever place it, not move it.
 */
function glide() {
	const list = document.querySelector<HTMLElement>('.navlist');
	if (!list) return;
	const pill = document.createElement('span');
	pill.className = 'glide-hov';
	pill.setAttribute('aria-hidden', 'true');
	pill.style.opacity = '0';
	list.append(pill);
	list.dataset.glide = '';
	const put = (row: HTMLElement) => {
		const instant = pill.style.opacity === '0';
		const box = list.getBoundingClientRect();
		const r = row.getBoundingClientRect();
		if (instant) pill.style.transition = 'none';
		pill.style.transform = `translate(${r.left - box.left}px, ${r.top - box.top}px)`;
		pill.style.width = `${r.width}px`;
		pill.style.height = `${r.height}px`;
		pill.style.borderRadius = getComputedStyle(row).borderRadius;
		pill.style.opacity = '1';
		if (instant) {
			// Commit the jump before the transition comes back, so only later moves slide.
			void pill.offsetWidth;
			pill.style.transition = '';
		}
	};
	list.addEventListener('pointerover', (e) => {
		const row = e.target instanceof Element ? e.target.closest<HTMLElement>('.row') : null;
		if (row && list.contains(row)) put(row);
	});
	list.addEventListener('pointerleave', () => (pill.style.opacity = '0'));
}

function drawer() {
	const side = document.getElementById('menu');
	if (!side) return;
	const open = (e: Event) => {
		e.preventDefault();
		side.classList.add('open');
		side.querySelector<HTMLElement>('[aria-current="page"], a')?.focus({ preventScroll: true });
	};
	const close = (e?: Event) => {
		e?.preventDefault();
		side.classList.remove('open');
		// Opened without JS first (`#menu` in the URL): drop the target too.
		if (location.hash === '#menu') history.replaceState(history.state, '', location.pathname + location.search);
	};
	for (const b of document.querySelectorAll('a.menubtn[href="#menu"]')) b.addEventListener('click', open);
	for (const b of document.querySelectorAll('.scrim, .closebtn')) b.addEventListener('click', close);
	document.addEventListener('keydown', (e) => {
		if (e.key === 'Escape' && side.classList.contains('open')) close();
	});
	// A link followed from the drawer: the next page shows with the drawer shut, as after a load.
	addEventListener(PAGE, () => side.classList.remove('open'));
}

/** The site's base (`/blaze/`), from the stylesheet the renderer linked as `<base>/site.css`. */
function base(): string {
	const css = document.querySelector<HTMLLinkElement>('link[rel="stylesheet"][href$="site.css"]');
	return css ? new URL('.', css.href).pathname : '/';
}

const words = (q: string) => q.toLowerCase().split(/\s+/).filter(Boolean);

function rank(entries: Entry[], q: string): Entry[] {
	const ws = words(q);
	if (ws.length === 0) return [];
	const scored: [number, Entry][] = [];
	for (const e of entries) {
		const t = e.t.toLowerCase();
		const h = e.h.toLowerCase();
		const x = e.x.toLowerCase();
		let score = 0;
		let all = true;
		for (const w of ws) {
			if (t.includes(w)) score += h === '' ? 12 : 4;
			else if (h.includes(w)) score += 8;
			else if (x.includes(w)) score += 1;
			else {
				all = false;
				break;
			}
		}
		if (all) scored.push([score, e]);
	}
	return scored.sort((a, b) => b[0] - a[0]).slice(0, 20).map(([, e]) => e);
}

function snippet(x: string, q: string): string {
	const w = words(q)[0] ?? '';
	const at = Math.max(0, x.toLowerCase().indexOf(w) - 40);
	return (at > 0 ? '…' : '') + x.slice(at, at + 160);
}

function search() {
	const buttons = [...document.querySelectorAll<HTMLButtonElement>('[data-search]')];
	if (buttons.length === 0 || typeof HTMLDialogElement === 'undefined') return;
	const mac = /Mac|iPhone|iPad/.test(navigator.platform);
	for (const b of buttons) {
		b.hidden = false;
		const kbd = b.querySelector('kbd');
		if (kbd && !mac) kbd.textContent = 'Ctrl K';
	}

	const dialog = document.createElement('dialog');
	dialog.className = 'search';
	dialog.setAttribute('aria-label', 'Search the site');
	const icon = buttons[0]!.querySelector('svg')?.outerHTML ?? '';
	dialog.innerHTML = `<form method="dialog" role="search">${icon}<input type="search" placeholder="Search blaze" aria-label="Search" autocomplete="off" spellcheck="false" /></form><ul role="listbox"></ul>`;
	document.body.append(dialog);
	const input = dialog.querySelector('input')!;
	const list = dialog.querySelector('ul')!;
	let entries: Entry[] | null = null;
	let shown: HTMLAnchorElement[] = [];
	let sel = 0;

	const select = (i: number) => {
		shown.forEach((a, j) => a.setAttribute('aria-selected', String(j === i)));
		sel = i;
		shown[i]?.scrollIntoView({ block: 'nearest' });
	};
	const draw = () => {
		const q = input.value.trim();
		list.replaceChildren();
		shown = [];
		if (entries === null || q === '') return;
		const hits = rank(entries, q);
		if (hits.length === 0) {
			const li = document.createElement('li');
			li.className = 'none';
			li.textContent = 'Nothing matches.';
			list.append(li);
			return;
		}
		for (const e of hits) {
			const li = document.createElement('li');
			const a = document.createElement('a');
			a.href = e.u;
			a.setAttribute('role', 'option');
			const t = document.createElement('span');
			t.className = 't';
			t.textContent = e.h ? `${e.t} › ${e.h}` : e.t;
			const p = document.createElement('span');
			p.className = 'p';
			p.textContent = snippet(e.x, q);
			a.append(t, p);
			a.addEventListener('click', () => dialog.close());
			li.append(a);
			list.append(li);
			shown.push(a);
		}
		select(0);
	};
	const load = async () => {
		if (entries !== null) return;
		try {
			const res = await fetch(`${base()}search.json`);
			const v: unknown = await res.json();
			entries = Array.isArray(v) ? (v as Entry[]) : [];
		} catch {
			entries = [];
		}
		draw();
	};
	const open = () => {
		document.getElementById('menu')?.classList.remove('open');
		dialog.showModal();
		input.select();
		void load();
	};

	input.addEventListener('input', draw);
	input.addEventListener('keydown', (e) => {
		if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
			e.preventDefault();
			if (shown.length) select((sel + (e.key === 'ArrowDown' ? 1 : shown.length - 1)) % shown.length);
		} else if (e.key === 'Enter') {
			e.preventDefault();
			const a = shown[sel];
			// A click, so the router takes it (the link's own click listener closes the dialog).
			a?.click();
		}
	});
	dialog.addEventListener('click', (e) => {
		if (e.target === dialog) dialog.close();
	});
	for (const b of buttons) b.addEventListener('click', open);
	document.addEventListener('keydown', (e) => {
		const typing = e.target instanceof HTMLElement && e.target.closest('input, textarea, [contenteditable]');
		if ((e.key === 'k' && (e.metaKey || e.ctrlKey)) || (e.key === '/' && !typing)) {
			e.preventDefault();
			if (dialog.open) dialog.close();
			else open();
		}
	});
}

function spy(signal: AbortSignal) {
	const links = [...document.querySelectorAll<HTMLAnchorElement>('.toc a[href^="#"]')];
	if (links.length === 0) return;
	const heads = links.map((a) => document.getElementById(decodeURIComponent(a.hash.slice(1))));
	let frame = 0;
	const mark = () => {
		frame = 0;
		let cur = -1;
		heads.forEach((h, i) => {
			if (h && h.getBoundingClientRect().top <= 96) cur = i;
		});
		links.forEach((a, i) => a.classList.toggle('cur', i === cur));
	};
	addEventListener('scroll', () => (frame ||= requestAnimationFrame(mark)), { passive: true, signal });
	signal.addEventListener('abort', () => cancelAnimationFrame(frame));
	mark();
}

folds();
glide();
drawer();
search();
// The sidebar's right edge drags its width (client/lib/resize.ts); not in the phone drawer (styles/shell.css).
document.querySelector('.side')?.append(handle(SIDE));

onPage(spy);

/** Each lesson row's mark: not started, started, finished, or every question right the first time. */
function marks(signal: AbortSignal) {
	const paint = () => {
		const saved = load();
		for (const row of document.querySelectorAll<HTMLAnchorElement>('.navlist .row[data-lesson]')) {
			const state = stateOf(saved.lessons[row.dataset.lesson!], (row.dataset.quizzes ?? '').split(' ').filter(Boolean));
			row.dataset.state = state;
			row.setAttribute('aria-label', `${row.title}, ${stateWords[state]}`);
		}
	};
	paint();
	addEventListener('storage', (e) => e.key === PROGRESS && paint(), { signal });
}
onPage(marks);
