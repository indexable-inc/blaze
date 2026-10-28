/**
 * Who is here (worker/presence.ts), on every page: imported by client/side.ts, so it runs once and outlives the
 * client router's swaps (client/nav.ts). One WebSocket per tab tells the site which page the tab shows and whether it
 * is hidden; what comes back is everyone on the site, drawn three places:
 *
 * - the top line of the pane (`.topline`, made here, outside `.grid` so a swap keeps it; on a phone it sits in the
 *   phone bar instead): the page's discussion
 *   (its comment count, a jump to it) on a page that has one, the faces on this page (click: everyone on the site
 *   and where), the chat (client/lib/chat.ts, which rides this socket), and the reader: their GitHub avatar and login
 *   with Sign out, or Sign in with GitHub;
 * - the sidebar: a small stack of faces on each page's row that someone else is reading, and when one of them says
 *   something in the chat, a speech bubble out of their face for a few seconds (`speak`);
 * - comments: when someone posts or deletes one, the worker says so here, and client/comments.ts refreshes the
 *   threads of that doc if it is the page shown.
 *
 * Without JS, or with the socket down, the page is as it was.
 *
 * Signed-in people are their GitHub avatar; anonymous ones a GitHub-style identicon of the key the worker gives them
 * (a hash, new every day). Someone whose tab is hidden is drawn faded ("away").
 */

import { tipOn } from './tip';
import * as chat from './chat';
import { COMMENTS, LEAVE, PAGE, THREADS, onPage } from './page';

type Who = { key: string; login: string | null; avatar: string | null };
type Person = Who & { country: string | null; since?: number; pages: { path: string; away: boolean }[] };
type You = Who & { signIn: boolean };

/** The site's base (`/blaze/`), from the stylesheet the renderer linked as `<base>/site.css`. */
const BASE = (() => {
	const css = document.querySelector<HTMLLinkElement>('link[rel="stylesheet"][href$="site.css"]');
	return css ? new URL('.', css.href).pathname : '/';
})();
const SVG = 'http://www.w3.org/2000/svg';
const ROW_FACES = 3;
const BAR_FACES = 5;

let you: You | null = null;
let people: Person[] = [];
let total = 0;

/** The page this tab shows, as a site path (`/rfcs/capabilities`; the home page is `/`). */
function sitePath(): string {
	let p = location.pathname;
	if (p.startsWith(BASE)) p = `/${p.slice(BASE.length)}`;
	else if (`${p}/` === BASE) p = '/';
	if (p.endsWith('.html')) p = p.slice(0, -5);
	if (p.length > 1 && p.endsWith('/')) p = p.slice(0, -1);
	return p === '/index' ? '/' : p;
}

const href = (path: string) => BASE + path.slice(1);
const name = (w: Who) => (w.login ? `@${w.login}` : `anon ${w.key.slice(1, 5)}`);
const awayOn = (p: Person, path: string) => p.pages.filter((x) => x.path === path).every((x) => x.away);

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls = '', text = ''): HTMLElementTagNameMap[K] {
	const e = document.createElement(tag);
	if (cls) e.className = cls;
	if (text) e.textContent = text;
	return e;
}

/** GitHub's identicon: a 5x5 grid mirrored left to right, cells and hue from the key. */
function identicon(key: string): SVGSVGElement {
	const hex = key.slice(1).padEnd(12, '0');
	const nib = [...hex].map((c) => parseInt(c, 16) || 0);
	const svg = document.createElementNS(SVG, 'svg');
	svg.setAttribute('viewBox', '-0.5 -0.5 6 6');
	svg.setAttribute('aria-hidden', 'true');
	const hue = ((nib[0] << 4) | nib[1]) * (360 / 256);
	svg.style.setProperty('--id-ink', `hsl(${hue.toFixed(0)} 55% 58%)`);
	for (let y = 0; y < 5; y++)
		for (let x = 0; x < 3; x++) {
			if ((nib[(y * 3 + x) % 12] + y) % 2 === 0) continue;
			for (const cx of x === 2 ? [2] : [x, 4 - x]) {
				const r = document.createElementNS(SVG, 'rect');
				r.setAttribute('x', String(cx));
				r.setAttribute('y', String(y));
				r.setAttribute('width', '1');
				r.setAttribute('height', '1');
				svg.append(r);
			}
		}
	return svg;
}

function face(w: Who, away = false): HTMLElement {
	const f = el('span', `face${away ? ' away' : ''}`);
	f.dataset.key = w.key;
	tipOn(f, away ? `${name(w)} (away)` : name(w));
	if (w.avatar && w.avatar.startsWith('https://avatars.githubusercontent.com/')) {
		const img = el('img');
		img.src = `${w.avatar}${w.avatar.includes('?') ? '&' : '?'}s=48`;
		img.alt = '';
		img.referrerPolicy = 'no-referrer';
		img.loading = 'lazy';
		f.append(img);
	} else f.append(identicon(w.key));
	return f;
}

function stack(list: Person[], max: number, path: string): HTMLElement {
	const s = el('span', 'faces');
	for (const p of list.slice(0, max)) s.append(face(p, awayOn(p, path)));
	if (list.length > max) s.append(el('span', 'face more', `+${list.length - max}`));
	return s;
}

/** The sidebar's label for a site path, else the path. */
function titleOf(path: string): string {
	const a = document.querySelector<HTMLAnchorElement>(`.navlist a.row[href="${CSS.escape(href(path))}"]`);
	return a?.title || a?.dataset.tip || a?.textContent?.trim() || path;
}

// ---- The top line -------------------------------------------------------------------------------------------------

const bar = el('div', 'topline');
bar.setAttribute('aria-label', 'Discussion and who is here');
const discussSlot = el('span', 'tl-discuss');
const hereSlot = el('span', 'tl-here');
const meSlot = el('span', 'tl-me');
const chatSlot = el('span', 'tl-chatslot');
chatSlot.append(chat.button);
bar.append(discussSlot, el('span', 'tl-gap'), hereSlot, chatSlot, meSlot);
const list = el('div', 'tl-list');
list.hidden = true;
list.id = 'who-is-here';
bar.append(list);
const phone = matchMedia('(max-width: 800px)');

/** In the pane above the page, or on a phone in the phone bar, before its search button. */
function mount(): void {
	const mbar = document.querySelector('.mbar');
	const pane = document.querySelector('.pane');
	if (phone.matches && mbar) {
		if (bar.parentElement !== mbar) mbar.insertBefore(bar, mbar.querySelector('.msearch'));
	} else if (pane && bar.parentElement !== pane) pane.insertBefore(bar, pane.querySelector(':scope > .grid'));
}
phone.addEventListener('change', mount);

function closeList(): void {
	if (list.hidden) return;
	list.hidden = true;
	hereSlot.querySelector('button')?.setAttribute('aria-expanded', 'false');
}

/** The page's discussion: its comments, counted from the threads the page was rendered with. */
function drawDiscuss(): void {
	discussSlot.replaceChildren();
	if (!document.querySelector('main #discussion')) return;
	const n = document.querySelectorAll('main .c-item').length;
	const a = el('a', 'tl-btn');
	a.href = '#discussion';
	tipOn(a, n === 0 ? 'Discuss this page' : `${n} comment${n === 1 ? '' : 's'} on this page`);
	a.append(el('span', 'tl-bubble'), el('span', 'tl-label', n === 0 ? 'Discuss' : n === 1 ? 'comment' : 'comments'));
	if (n > 0) a.insertBefore(el('span', 'tl-n', String(n)), a.lastChild);
	discussSlot.append(a);
}

function drawHere(): void {
	const path = sitePath();
	const on = people.filter((p) => p.pages.some((x) => x.path === path));
	hereSlot.replaceChildren();
	if (!you) return;
	const b = el('button', 'tl-btn');
	b.type = 'button';
	b.setAttribute('aria-expanded', String(!list.hidden));
	b.setAttribute('aria-controls', list.id);
	b.append(el('span', 'tl-dot'), stack(on, BAR_FACES, path), el('span', 'tl-count', on.length <= 1 ? `${total} online` : `${on.length} here`));
	tipOn(b, `${on.length} on this page, ${total} on the site`);
	b.addEventListener('click', (e) => {
		e.stopPropagation();
		if (!list.hidden) return closeList();
		list.hidden = false;
		b.setAttribute('aria-expanded', 'true');
		drawList();
	});
	hereSlot.append(b);
}

const regions = (() => {
	try {
		return new Intl.DisplayNames(undefined, { type: 'region' });
	} catch {
		return null;
	}
})();

/** A country's flag: its two letters as regional indicator symbols, named in a tooltip. */
function flag(cc: string): HTMLElement {
	const f = el('span', 'flag', String.fromCodePoint(...[...cc].map((c) => 0x1f1e6 + c.charCodeAt(0) - 65)));
	const n = regions?.of(cc) ?? cc;
	tipOn(f, n);
	f.setAttribute('aria-label', n);
	return f;
}

/** How long since `t`, short: now, 5m, 2h, 3d. */
function ago(t: number): string {
	const m = Math.floor((Date.now() - t) / 60_000);
	return m < 1 ? 'now' : m < 60 ? `${m}m` : m < 1440 ? `${Math.floor(m / 60)}h` : `${Math.floor(m / 1440)}d`;
}
// The times in an open list keep up on their own.
setInterval(() => drawList(), 30_000);

function drawList(): void {
	if (list.hidden) return;
	list.replaceChildren(el('p', 'tl-head', total === 1 ? 'Only you, right now' : `${total} on the site`));
	const ul = el('ul');
	for (const p of people) {
		const li = el('li');
		const who = p.login ? el('a', 'who', name(p)) : el('span', 'who', name(p));
		if (who instanceof HTMLAnchorElement) {
			who.href = `https://github.com/${encodeURIComponent(p.login!)}`;
			who.rel = 'nofollow noopener';
		}
		// The dot: green while a tab of theirs is in front of them, hollow while every one is in the background.
		const active = p.pages.some((x) => !x.away);
		const av = el('span', 'tl-av');
		const dot = el('span', active ? 'dot on' : 'dot');
		tipOn(dot, active ? 'Here now' : 'Tab in the background');
		av.append(face(p, !active), dot);
		li.append(av, who);
		if (p.country) li.append(flag(p.country));
		if (p.key === you?.key) li.append(el('span', 'tag', 'you'));
		// The page in front of them, the rest of their tabs as +N (named in its tooltip).
		const where = el('span', 'where');
		const [front, ...rest] = p.pages;
		if (front) {
			const a = el('a', front.away ? 'away' : '', titleOf(front.path));
			a.href = href(front.path);
			where.append(a);
		}
		if (rest.length) {
			const more = el('span', 'more', `+${rest.length}`);
			tipOn(more, rest.map((x) => titleOf(x.path)).join(', '));
			where.append(more);
		}
		li.append(where);
		if (p.since) {
			const t = el('span', 'for', ago(p.since));
			tipOn(t, `On the site since ${new Date(p.since).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })}`);
			li.append(t);
		}
		ul.append(li);
	}
	if (people.length < total) ul.append(el('li', 'tl-head', `and ${total - people.length} more`));
	list.append(ul);
}

function drawMe(): void {
	meSlot.replaceChildren();
	if (!you) return;
	if (you.login) {
		const a = el('a', 'tl-btn tl-user');
		a.setAttribute('aria-label', `Signed in as ${you.login}`);
		a.href = `https://github.com/${encodeURIComponent(you.login)}`;
		a.rel = 'noopener';
		tipOn(a, 'Your GitHub profile');
		a.append(face(you), el('span', '', you.login));
		const form = el('form', 'tl-out');
		form.method = 'post';
		form.action = `${BASE}auth/logout`;
		const next = el('input');
		next.type = 'hidden';
		next.name = 'next';
		next.value = sitePath() + location.hash;
		const out = el('button', 'tl-btn tl-quiet', 'Sign out');
		out.type = 'submit';
		form.append(next, out);
		meSlot.append(a, form);
	} else if (you.signIn) {
		const a = el('a', 'tl-btn tl-signin');
		a.append(el('span', 'tl-label', 'Sign in with GitHub'));
		a.setAttribute('aria-label', 'Sign in with GitHub');
		a.href = `${BASE}auth/github?next=${encodeURIComponent(sitePath() + location.hash)}`;
		a.prepend(githubMark());
		meSlot.append(a);
	}
}

function githubMark(): SVGSVGElement {
	const svg = document.createElementNS(SVG, 'svg');
	svg.setAttribute('viewBox', '0 0 16 16');
	svg.setAttribute('aria-hidden', 'true');
	const p = document.createElementNS(SVG, 'path');
	p.setAttribute(
		'd',
		'M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0016 8c0-4.42-3.58-8-8-8z',
	);
	svg.append(p);
	return svg;
}

// ---- The sidebar --------------------------------------------------------------------------------------------------

function drawSide(): void {
	for (const old of document.querySelectorAll('.navlist .row > .faces')) old.remove();
	const byPath = new Map<string, Person[]>();
	for (const p of people) {
		if (p.key === you?.key) continue;
		// One face a person, on the page in front of them (the presence object sends that page first).
		const pg = p.pages[0];
		if (!pg) continue;
		const l = byPath.get(pg.path) ?? [];
		l.push(p);
		byPath.set(pg.path, l);
	}
	for (const [path, list] of byPath) {
		const row = document.querySelector<HTMLAnchorElement>(`.navlist a.row[href="${CSS.escape(href(path))}"]`);
		if (!row) continue;
		const s = stack(list, ROW_FACES, path);
		tipOn(s, `${list.map(name).join(', ')} reading this`);
		row.append(s);
	}
}

const SAY_MS = 6000;
const TYPING_BUBBLE_MS = 3000;
const bubbles = new Map<string, HTMLElement>();

/** A bubble with `text` out of `key`'s face on the sidebar row of the page they are on, if it is in view. */
function speak(key: string, login: string, text: string | null): void {
	// Typing dots never cover something said that is still showing.
	if (text === null && bubbles.get(key)?.classList.contains('said')) return;
	bubbles.get(key)?.remove();
	const p = people.find((x) => x.key === key);
	// The page they are looking at first, then any other tab of theirs.
	let at: Element | null = null;
	for (const pg of p?.pages ?? []) {
		const row = `.navlist a.row[href="${CSS.escape(href(pg.path))}"] > .faces`;
		at = document.querySelector(`${row} > .face[data-key="${CSS.escape(key)}"]`) ?? document.querySelector(row);
		if (at) break;
	}
	at ??= document.querySelector(`.navlist .row > .faces > .face[data-key="${CSS.escape(key)}"]`);
	const r = at?.getBoundingClientRect();
	if (!at || !r || r.width === 0 || r.bottom < 0 || r.top > innerHeight) return;
	const b = el('div', text === null ? 'say-bubble typing' : 'say-bubble said');
	b.setAttribute('aria-hidden', 'true');
	if (text === null) {
		const dots = el('span', 'dots');
		dots.append(el('i'), el('i'), el('i'));
		b.append(dots);
	} else b.append(el('b', '', login), el('span', '', text));
	b.style.left = `${Math.round(r.right + 10)}px`;
	b.style.top = `${Math.round(r.top + r.height / 2 - 14)}px`;
	document.body.append(b);
	bubbles.set(key, b);
	at.classList.add('talking');
	setTimeout(() => {
		b.classList.add('out');
		at.classList.remove('talking');
		setTimeout(() => {
			b.remove();
			if (bubbles.get(key) === b) bubbles.delete(key);
		}, 300);
	}, text === null ? TYPING_BUBBLE_MS : SAY_MS);
}
// The sidebar scrolling or the page swapping moves the faces from under a bubble: let it go.
for (const t of ['scroll', LEAVE] as const) addEventListener(t, () => {
	for (const b of bubbles.values()) b.remove();
	bubbles.clear();
}, { capture: true, passive: true });

function draw(): void {
	mount();
	drawDiscuss();
	drawHere();
	drawMe();
	drawList();
	drawSide();
}

// ---- The socket ---------------------------------------------------------------------------------------------------

let ws: WebSocket | null = null;
let retry = 1000;

function send(): void {
	if (ws?.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ t: 'at', path: sitePath(), away: document.hidden }));
}

function connect(): void {
	const url = new URL(`${BASE}api/presence`, location.href);
	url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
	const s = new WebSocket(url);
	ws = s;
	s.addEventListener('open', () => {
		retry = 1000;
		send();
	});
	s.addEventListener('message', (e) => {
		let m: unknown;
		try {
			m = JSON.parse(String(e.data));
		} catch {
			return;
		}
		if (typeof m !== 'object' || m === null) return;
		const msg = m as { t?: unknown };
		if (chat.message(msg)) return;
		if (msg.t === 'comments') {
			const doc = (m as { doc?: unknown }).doc;
			if (typeof doc === 'string') dispatchEvent(new CustomEvent(COMMENTS, { detail: doc }));
			return;
		}
		if (msg.t === 'you') {
			you = m as You;
			chat.setYou(you);
		}
		else if (msg.t === 'here') {
			const h = m as { people?: Person[]; total?: number };
			people = Array.isArray(h.people) ? h.people : [];
			total = typeof h.total === 'number' ? h.total : people.length;
			chat.setOnline(total);
			chat.setPeople(people);
		} else return;
		draw();
	});
	s.addEventListener('close', () => {
		if (ws !== s) return;
		ws = null;
		// Back off up to a minute; a page that came back from the bfcache reconnects on pageshow.
		setTimeout(connect, retry + Math.random() * 500);
		retry = Math.min(retry * 2, 60_000);
	});
}

addEventListener('pagehide', () => {
	const s = ws;
	ws = null;
	s?.close(1000);
});
addEventListener('pageshow', (e) => {
	if (e.persisted && !ws) connect();
});
document.addEventListener('visibilitychange', send);
addEventListener(PAGE, send);
addEventListener(THREADS, drawDiscuss);
document.addEventListener('keydown', (e) => {
	if (e.key === 'Escape') closeList();
});
document.addEventListener('click', (e) => {
	if (!(e.target instanceof Node && list.contains(e.target))) closeList();
});

chat.setup({ send: (m) => ws?.readyState === WebSocket.OPEN && ws.send(JSON.stringify(m)), base: BASE, face, speak });
onPage(() => {
	closeList();
	draw();
});
connect();
