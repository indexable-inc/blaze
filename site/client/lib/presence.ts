/**
 * Who is here (worker/presence.ts), on every page: imported by client/side.ts, so it runs once and outlives the
 * client router's swaps (client/nav.ts). One WebSocket per tab tells the site which page the tab shows and whether it
 * is hidden; what comes back is everyone on the site, drawn three places:
 *
 * - the top line of the pane (`.topline`, made here, outside `.grid` so a swap keeps it; on a phone it sits in the
 *   phone bar instead): Chat, which opens the feed's history and input as T does (client/lib/feed.ts; the chat itself
 *   is client/lib/chat.ts, which rides this socket), the faces on this page (click: everyone on the site and where),
 *   and the reader: their GitHub avatar and login with Sign out, or Sign in with GitHub;
 * - the sidebar: a small stack of faces on each page's row that someone else is reading, and when one of them says
 *   something in the chat, a speech bubble out of their face for a few seconds (`speak`);
 * - comments: when someone posts or deletes one, the worker says so here, and client/comments.ts refreshes the
 *   threads of that doc if it is the page shown;
 * - the page itself: the pointers of the others on it, and where on it each is reading (client/lib/cursors.ts, which
 *   rides this socket too), and what they type in the chat's input (T), live by their pointer;
 * - the feed at the bottom left (client/lib/feed.ts): who joined the site and who left it.
 *
 * The socket also says which build of the site is live (a `build` frame on every connect): client/lib/update.ts takes
 * a new one in, a reload or a swap in place.
 *
 * Without JS, or with the socket down, the page is as it was.
 *
 * Signed-in people are their GitHub avatar. Anonymous ones are named and drawn from the key the worker gives them (a
 * hash, new every day): "Quiet Otter", and an otter's face (gen/animals.ts, compiled into one sprite by gen/build.ts)
 * in their hue, the colour of their cursor too (client/lib/cursors.ts). Someone whose tab is hidden is drawn faded
 * ("away").
 *
 * A crawler or other automation (the worker decides: worker/bots.ts) comes with `bot`, its name ("Googlebot"): it is
 * drawn as the sprite's robot and named so, listed under "Crawlers" in the who-is-here list, not counted online, and
 * gets no join or leave line and no pointer (the object refuses it one anyway).
 */

import { tipOn } from './tip';
import * as chat from './chat';
import * as cursors from './cursors';
import * as feed from './feed';
import * as update from './update';
import { COMMENTS, LEAVE, PAGE, onPage } from './page';

type Who = { key: string; login: string | null; avatar: string | null; bot?: string };
type Person = Who & { country: string | null; since?: number; devices?: string[]; pages: { path: string; away: boolean }[] };
type You = Who & { signIn: boolean };

/** The site's base (`/blaze/`), from the stylesheet the renderer linked as `<base>/site.css`. */
/** The close code of a tab pushed out by a newer one (worker/presence.ts). */
const TOO_MANY_TABS = 4001;
let pushedOut = false;
const BASE = (() => {
	const css = document.querySelector<HTMLLinkElement>('link[rel="stylesheet"][href$="site.css"]');
	return css ? new URL('.', css.href).pathname : '/';
})();
const SVG = 'http://www.w3.org/2000/svg';
const ROW_FACES = 3;
/** Only someone whose visit began this recently is new (a reload or a reconnect keeps their start: worker/presence.ts). */
const ARRIVAL_MS = 20_000;
const ARRIVAL_GAP_MS = 4000;
/** Someone is gone once they have been off the list this long: a reload, a move between pages, a reconnect come back sooner. */
const LEAVE_MS = 8000;
const SETTLE_MS = 15_000;
let connectedAt = 0;
const BAR_FACES = 5;

let you: You | null = null;
let people: Person[] = [];
/** People on the site, bots left out, and the bots. */
let total = 0;
let bots = 0;
const human = (p: Person) => !p.bot;

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
/** Words for anonymous readers' names: the same key is always the same "Quiet Otter", here and on every tab. */
const ADJECTIVES = [
	'Amber', 'Brave', 'Bright', 'Calm', 'Clever', 'Cosmic', 'Curious', 'Dapper', 'Eager', 'Fuzzy', 'Gentle', 'Golden',
	'Happy', 'Jolly', 'Kind', 'Lucky', 'Merry', 'Mighty', 'Nimble', 'Quiet', 'Rapid', 'Rusty', 'Shiny', 'Sleepy',
	'Snappy', 'Sunny', 'Swift', 'Tidy', 'Witty', 'Zesty', 'Plucky', 'Cozy',
];
/** The animals (gen/animals.ts, the one table of names and faces) and the sprite they are drawn in, from gen/build.ts. */
declare const BLAZE_ANIMALS: { sprite: string; names: string[]; robot: string };
const ANIMALS = BLAZE_ANIMALS.names;
const anonAnimal = (key: string) => ANIMALS[Math.floor((parseInt(key.slice(1, 7), 16) || 0) / ADJECTIVES.length) % ANIMALS.length];
const anonName = (key: string) => `${ADJECTIVES[(parseInt(key.slice(1, 7), 16) || 0) % ADJECTIVES.length]} ${anonAnimal(key)}`;
const name = (w: Who) => (w.login ? `@${w.login}` : w.bot ? w.bot : anonName(w.key));
const awayOn = (p: Person, path: string) => p.pages.filter((x) => x.path === path).every((x) => x.away);

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls = '', text = ''): HTMLElementTagNameMap[K] {
	const e = document.createElement(tag);
	if (cls) e.className = cls;
	if (text) e.textContent = text;
	return e;
}

/** An anonymous reader's animal, from the sprite (its symbols are `animal-<name>`, gen/animals.ts `symbolId`); a bot's robot. */
function animalFace(w: Who): SVGSVGElement {
	const svg = document.createElementNS(SVG, 'svg');
	svg.setAttribute('viewBox', '0 0 32 32');
	svg.setAttribute('aria-hidden', 'true');
	const use = document.createElementNS(SVG, 'use');
	use.setAttribute('href', `${BASE}${BLAZE_ANIMALS.sprite}#${w.bot ? BLAZE_ANIMALS.robot : `animal-${anonAnimal(w.key).toLowerCase()}`}`);
	svg.append(use);
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
	} else if (w.bot) {
		// Steel, whoever it is (styles/shell.css): a bot has no hue of its own, as it has no pointer.
		f.classList.add('animal', 'bot');
		f.append(animalFace(w));
	} else {
		// The hue cursors.ts's `hueOf` gives the same key, so their face and their pointer match.
		f.classList.add('animal');
		f.style.setProperty('--h', ((parseInt(w.key.slice(1, 3), 16) || 0) * (360 / 256)).toFixed(0));
		f.append(animalFace(w));
	}
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

/** Who was here at the last list, null before the first: a key new since then, who came just now, gets a line. */
let known: Map<string, Person> | null = null;
let lastArrival = 0;
let lastLeave = 0;
/** Who went missing from the list, and when they are taken as gone (LEAVE_MS later). */
const missing = new Map<string, { p: Person; timer: ReturnType<typeof setTimeout> }>();

/** "Merry Tiger joined on Charter", "@ana left (was on Install)", in the feed (not when muted: client/lib/feed.ts). */
function line(kind: 'join' | 'leave', p: Person, more: number): void {
	const page = p.pages[0]?.path;
	feed.push({
		kind,
		face: face(p),
		who: `${name(p)}${more ? ` and ${more} more` : ''}`,
		text: kind === 'join' ? `joined ${page ? `on ${titleOf(page)}` : 'the site'}` : `left${page ? ` (was on ${titleOf(page)})` : ''}`,
		href: page ? href(page) : undefined,
	});
}

/**
 * Who came and who went, against the last list. Right after this tab (re)connects everyone else is new to it, and after
 * a deploy everyone reconnects at once: none of either then. A crowd at once is one line, not a stack.
 */
function announce(): void {
	const now = Date.now();
	const settling = known === null || now - connectedAt < SETTLE_MS;
	// Bots come and go without a line.
	const humans = people.filter(human);
	const here = new Map(humans.map((p) => [p.key, p]));
	const fresh = settling ? [] : humans.filter((p) => !known!.has(p.key) && !missing.has(p.key) && p.key !== you?.key && now - (p.since ?? 0) < ARRIVAL_MS);
	// Back within LEAVE_MS (a reload, a reconnect): never gone.
	for (const [k, m] of missing) {
		if (!here.has(k)) continue;
		clearTimeout(m.timer);
		missing.delete(k);
	}
	// Gone from a list cut short (more online than it carries) may only be out of it.
	if (!settling && known && humans.length >= total) {
		for (const [k, p] of known) {
			if (here.has(k) || k === you?.key || missing.has(k)) continue;
			missing.set(k, { p, timer: setTimeout(() => left(k), LEAVE_MS) });
		}
	}
	known = here;
	if (!fresh.length || now - lastArrival < ARRIVAL_GAP_MS) return;
	lastArrival = now;
	line('join', fresh[0], fresh.length - 1);
}

/** Still off the list LEAVE_MS after they went: gone, said with the others who went with them. */
function left(key: string): void {
	const m = missing.get(key);
	missing.delete(key);
	if (!m || people.some((p) => p.key === key && human(p)) || Date.now() - connectedAt < SETTLE_MS) return;
	const now = Date.now();
	if (now - lastLeave < ARRIVAL_GAP_MS) return;
	lastLeave = now;
	// Whoever else is gone by now goes in the same line.
	const with_ = [...missing.entries()].filter(([k]) => !people.some((p) => p.key === k && human(p)));
	for (const [k, x] of with_) {
		clearTimeout(x.timer);
		missing.delete(k);
	}
	line('leave', m.p, with_.length);
}

// ---- The top line -------------------------------------------------------------------------------------------------

const bar = el('div', 'topline');
bar.setAttribute('aria-label', 'Chat and who is here');
const hereSlot = el('span', 'tl-here');
const meSlot = el('span', 'tl-me');
// Chat: the feed's history and input (client/lib/feed.ts), as T opens them.
const chatBtn = el('button', 'tl-btn tl-chat');
chatBtn.type = 'button';
chatBtn.append(el('span', 'tl-bubble'), el('span', 'tl-label', 'Chat'));
tipOn(chatBtn, feed.hint() ? 'Chat with everyone on the site (T)' : 'Chat with everyone on the site');
chatBtn.addEventListener('click', () => feed.open());
// Shut the sidebar for more room to read (a wide screen's; a phone's is a drawer already). Remembered per browser.
const SIDE_SHUT = 'blaze.side.shut';
const sideBtn = el('button', 'tl-btn tl-side');
sideBtn.type = 'button';
sideBtn.innerHTML = '<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="2" y="3" width="12" height="10" rx="2"/><path d="M6.5 3v10"/></svg>';
function shutSide(shut: boolean, keep = true): void {
	document.body.classList.toggle('side-shut', shut);
	sideBtn.setAttribute('aria-pressed', String(shut));
	tipOn(sideBtn, shut ? 'Show the sidebar' : 'Hide the sidebar');
	if (!keep) return;
	try {
		if (shut) localStorage.setItem(SIDE_SHUT, '1');
		else localStorage.removeItem(SIDE_SHUT);
	} catch {
		// Storage refused: shut for this page only.
	}
}
let wasShut = false;
try {
	wasShut = localStorage.getItem(SIDE_SHUT) === '1';
} catch {
	// Storage refused: open.
}
shutSide(wasShut, false);
sideBtn.addEventListener('click', () => shutSide(!document.body.classList.contains('side-shut')));
bar.append(sideBtn, chatBtn, el('span', 'tl-gap'), hereSlot, meSlot);
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

function drawHere(): void {
	const path = sitePath();
	const all = people.filter((p) => p.pages.some((x) => x.path === path));
	// The people first; the bots' faces after them, not counted.
	const on = all.filter(human);
	const onBots = all.filter((p) => !human(p));
	hereSlot.replaceChildren();
	if (!you) return;
	const b = el('button', 'tl-btn');
	b.type = 'button';
	b.setAttribute('aria-expanded', String(!list.hidden));
	b.setAttribute('aria-controls', list.id);
	b.append(el('span', 'tl-dot'), stack([...on, ...onBots], BAR_FACES, path), el('span', 'tl-count', on.length <= 1 ? `${total} online` : `${on.length} here`));
	const botsSaid = bots ? `, and ${bots === 1 ? 'a crawler' : `${bots} crawlers`}` : '';
	tipOn(b, `${on.length} on this page, ${total} on the site${botsSaid}${feed.hint() ? '. Press T or Enter to chat' : ''}`);
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

/** What they read on, as a small outline: each kind of device they have a tab on. */
const DEVICES: Record<string, [string, string]> = {
	computer: ['On a computer', '<rect x="3" y="3.5" width="10" height="7" rx="1"/><path d="M1.5 12.5h13"/>'],
	phone: ['On a phone', '<rect x="5" y="1.5" width="6" height="13" rx="1.5"/><path d="M7.3 12.5h1.4"/>'],
	tablet: ['On a tablet', '<rect x="3" y="1.5" width="10" height="13" rx="1.5"/><path d="M7.3 12.5h1.4"/>'],
};
function devices(p: Person): HTMLElement | null {
	const kinds = (p.devices ?? []).filter((d) => DEVICES[d]);
	if (!kinds.length) return null;
	const s = el('span', 'devices');
	for (const d of kinds) {
		const [label, paths] = DEVICES[d];
		const i = el('span', 'device');
		i.innerHTML = `<svg viewBox="0 0 16 16" aria-hidden="true">${paths}</svg>`;
		tipOn(i, label);
		i.setAttribute('aria-label', label);
		s.append(i);
	}
	return s;
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
	const head = el('p', 'tl-head', total === 1 ? 'Only you, right now' : `${total} on the site`);
	const flip = cursors.toggle();
	if (flip) head.append(flip);
	list.replaceChildren(head);
	// Here now first, then those with every tab in the background; you lead your group, the rest by who came first.
	const active = (p: Person) => p.pages.some((x) => !x.away);
	const order = (p: Person) => (active(p) ? 0 : 2) + (p.key === you?.key ? 0 : 1);
	const sorted = people.filter(human).sort((a, b) => order(a) - order(b) || (a.since ?? 0) - (b.since ?? 0));
	const ul = el('ul');
	let awayHead = false;
	for (const p of sorted) {
		const on = active(p);
		if (!on && !awayHead && sorted.some(active)) {
			ul.append(el('li', 'tl-sub', 'In the background'));
			awayHead = true;
		}
		ul.append(row(p, on));
	}
	// The bots apart, under their own heading, by who came first.
	const crawlers = people.filter((p) => !human(p)).sort((a, b) => (a.since ?? 0) - (b.since ?? 0));
	if (crawlers.length) {
		const sub = el('li', 'tl-sub', 'Crawlers');
		tipOn(sub, 'Search engines, link previews and other automation: welcome to read, not counted as people');
		ul.append(sub);
		for (const p of crawlers) ul.append(row(p, active(p)));
	}
	if (people.length < total + bots) ul.append(el('li', 'tl-sub', `and ${total + bots - people.length} more`));
	list.append(ul);
	const hint = feed.hint();
	if (hint) list.append(hint);
}

/** One person (or bot) in the list: their face, name, and where they are and since when. */
function row(p: Person, on: boolean): HTMLLIElement {
	const li = el('li', `${on ? '' : 'away'}${p.bot ? ' bot' : ''}`.trim());
	const av = el('span', 'tl-av');
	const dot = el('span', on ? 'dot on' : 'dot');
	tipOn(dot, on ? 'Here now' : 'Every tab in the background');
	av.append(face(p, !on), dot);
	// The name (a GitHub link for the signed in, without the @: the list is of people, not handles), then where they
	// are and how long they have been here, on a quieter line.
	const top = el('span', 'tl-name');
	const who = p.login ? el('a', 'who', p.login) : el('span', 'who', name(p));
	if (who instanceof HTMLAnchorElement) {
		who.href = `https://github.com/${encodeURIComponent(p.login!)}`;
		who.rel = 'nofollow noopener';
	}
	top.append(who);
	if (p.key === you?.key) top.append(el('span', 'tag', 'you'));
	if (p.bot) top.append(el('span', 'tag', 'bot'));
	if (p.country) top.append(flag(p.country));
	const meta = el('span', 'tl-meta');
	const [front, ...rest] = p.pages;
	if (front) {
		const a = el('a', 'where', titleOf(front.path));
		a.href = href(front.path);
		meta.append(a);
	}
	if (rest.length) {
		const more = el('span', 'more', `+${rest.length}`);
		tipOn(more, rest.map((x) => titleOf(x.path)).join(', '));
		meta.append(more);
	}
	const dev = p.bot ? null : devices(p);
	if (dev) meta.append(dev);
	if (p.since) {
		const t = el('span', 'for', ago(p.since));
		tipOn(t, `On the site since ${new Date(p.since).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })}`);
		meta.append(t);
	}
	const body = el('span', 'tl-body');
	body.append(top, meta);
	li.append(av, body);
	return li;
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
		// One face a person, on the page they have in front of them (the presence object sends that page first), and
		// only while they are there: someone whose every tab is in the background shows on no page.
		const pg = p.pages.find((x) => !x.away);
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
	const path = sitePath();
	cursors.setPeople(
		path,
		people.filter((p) => human(p) && p.key !== you?.key && p.pages.some((x) => x.path === path)).map((p) => ({ key: p.key, login: p.login, avatar: p.avatar, away: awayOn(p, path) })),
	);
	mount();
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
		connectedAt = Date.now();
		retry = 1000;
		cursors.reset();
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
		if (cursors.message(msg)) return;
		if (chat.message(msg)) return;
		if (msg.t === 'build') return update.build(m as { build?: unknown; assets?: unknown });
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
			const h = m as { people?: Person[]; total?: number; bots?: number };
			people = Array.isArray(h.people) ? h.people : [];
			bots = typeof h.bots === 'number' ? h.bots : people.filter((p) => !human(p)).length;
			total = typeof h.total === 'number' ? h.total : people.length - bots;
			announce();
			chat.setPeople(people);
		} else return;
		draw();
	});
	s.addEventListener('close', (e) => {
		if (ws !== s) return;
		ws = null;
		// A newer tab of the same person's took this one's place (worker/presence.ts MAX_TABS): back when shown again.
		if (e.code === TOO_MANY_TABS) return void (pushedOut = true);
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
document.addEventListener('visibilitychange', () => {
	if (!pushedOut || document.hidden) return send();
	pushedOut = false;
	connect();
});
addEventListener(PAGE, send);
document.addEventListener('keydown', (e) => {
	if (e.key === 'Escape') closeList();
});
document.addEventListener('click', (e) => {
	if (!(e.target instanceof Node && list.contains(e.target))) closeList();
});

const out = (m: object) => ws?.readyState === WebSocket.OPEN && ws.send(JSON.stringify(m));
chat.setup({ send: out, base: BASE, face, speak, live: cursors.say });
cursors.setup({ send: out, face, name });
onPage(() => {
	closeList();
	draw();
});
connect();
