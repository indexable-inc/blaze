/**
 * The narration player (audio/README.md). The renderer adds `<div class="listen" data-audio="/audio/<doc>.<hash>.json">`
 * under the title of a page that has audio, and `data-say="<block>"` on every block with a spoken line (the h1 is
 * `_intro`). This script builds the controls in that div and plays the manifest's lines one after another:
 *
 * - Play / Pause, previous and next block, elapsed and total time, speed (0.75x to 2x, pitch kept, remembered);
 * - keys while the player has focus: Space or k play and pause, Left and Right skip a block;
 * - the block being read is highlighted (`.say-now`) and, with the word timings and the CSS Custom Highlight API,
 *   the word being said (`::highlight(say-word)`), as the voice app's karaoke does. With prefers-reduced-motion the
 *   moving word highlight is off and scrolling is instant. A line written for the ear rather than read from the page
 *   (most authored `Say` lines) has no word highlight either: when under `MIN_MATCHED` of its words are found in the
 *   block, only the block is highlighted;
 * - every block gets a "play from here" button among its hover controls.
 *
 * Nothing plays until the reader presses something. DOM is built with createElement and textContent only.
 *
 * A site update (client/lib/update.ts) waits while the narration plays; a paused place is kept across it, for the same
 * doc and line, without scrolling to it.
 *
 * Set up again on every page (client/lib/page.ts). Moving to another page stops the narration: it reads this page and
 * highlights its blocks, so it has nothing to follow on the next one (as a load would stop it).
 */

import { blockControls } from './lib/blockctl';
import { SAVE, kept, onPage } from './lib/page';
import { tipOn } from './lib/tip';

type Word = [string, number, number];
type ManifestLine = { id: string; block: string; src: string; duration: number; words: Word[] };
type Manifest = { doc: string; duration: number; lines: ManifestLine[]; voice?: { id: string; attribution: string } };
/** Where a paused narration was before a site update. */
type Kept = { doc: string; line: string; time: number };

const SPEEDS = [0.75, 1, 1.25, 1.5, 1.75, 2];
const SPEED_KEY = 'blaze:listen-speed';
const INTRO = '_intro';
/** Hover controls and the like inside a block: never part of its words. */
const NOT_TEXT = '.c-btn, .c-thread, .c-form, .say-btn, sup.cite, button, form, script, style';

/** The share of a line's spoken words that must be found in its block for the word highlight to follow it. */
const MIN_MATCHED = 0.7;

const reducedMotion = () => matchMedia('(prefers-reduced-motion: reduce)').matches;

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls?: string, text?: string): HTMLElementTagNameMap[K] {
	const e = document.createElement(tag);
	if (cls) e.className = cls;
	if (text !== undefined) e.textContent = text;
	return e;
}

function icon(kind: 'play' | 'pause' | 'prev' | 'next'): SVGSVGElement {
	const ns = 'http://www.w3.org/2000/svg';
	const svg = document.createElementNS(ns, 'svg');
	svg.setAttribute('viewBox', '0 0 16 16');
	svg.setAttribute('aria-hidden', 'true');
	svg.setAttribute('class', 'listen-icon');
	const path = document.createElementNS(ns, 'path');
	const d = {
		play: 'M4 2.5v11l9-5.5z',
		pause: 'M4 2.5h3v11H4zM9 2.5h3v11H9z',
		prev: 'M3 3h2v10H3zM13 3v10L6 8z',
		next: 'M11 3h2v10h-2zM3 3v10l7-5z',
	}[kind];
	path.setAttribute('d', d);
	svg.append(path);
	return svg;
}

function clock(s: number): string {
	const t = Math.max(0, Math.round(s));
	const m = Math.floor(t / 60);
	return `${m}:${String(t % 60).padStart(2, '0')}`;
}

function loadSpeed(): number {
	try {
		const v = Number(localStorage.getItem(SPEED_KEY));
		return SPEEDS.includes(v) ? v : 1;
	} catch {
		return 1;
	}
}

function saveSpeed(v: number) {
	try {
		localStorage.setItem(SPEED_KEY, String(v));
	} catch {
		// storage blocked: the speed lasts for this page only
	}
}

/** A word's comparable parts: lowercase letters and digits (`mem::forget,` is `mem`, `forget`). */
const parts = (w: string) => w.toLowerCase().match(/[\p{L}\p{N}]+/gu) ?? [];

/** The block's words as ranges in the page, each split into comparable parts. */
function pageWords(block: Element): { part: string; range: Range }[] {
	const out: { part: string; range: Range }[] = [];
	const walker = document.createTreeWalker(block, NodeFilter.SHOW_TEXT, {
		acceptNode: (n) => (n.parentElement?.closest(NOT_TEXT) ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT),
	});
	for (let n = walker.nextNode(); n; n = walker.nextNode()) {
		const text = n.textContent ?? '';
		for (const m of text.matchAll(/\S+/g)) {
			const range = document.createRange();
			range.setStart(n, m.index!);
			range.setEnd(n, m.index! + m[0].length);
			for (const part of parts(m[0])) out.push({ part, range });
		}
	}
	return out;
}

/**
 * For each spoken word, the page word it is (or null): matched in order, looking a few words ahead, so a line written
 * differently from the text (a `Say`) simply highlights fewer words.
 */
function match(spoken: Word[], page: { part: string; range: Range }[]): (Range | null)[] {
	let at = 0;
	return spoken.map(([w]) => {
		const ps = parts(w);
		if (!ps.length) return null;
		for (let j = at; j < Math.min(page.length, at + 8); j++) {
			if (page[j]!.part === ps[0]) {
				at = j + ps.length;
				return page[j]!.range;
			}
		}
		return null;
	});
}

/** Whether the line is the block's text: at least `MIN_MATCHED` of its spoken words were found in the block. */
function follows(spoken: Word[], found: (Range | null)[]): boolean {
	let said = 0;
	let hit = 0;
	spoken.forEach(([w], i) => {
		if (!parts(w).length) return;
		said++;
		if (found[i]) hit++;
	});
	return said > 0 && hit / said >= MIN_MATCHED;
}

function start(root: HTMLElement, m: Manifest, base: URL, from?: Kept): () => void {
	const main = root.closest('main') ?? document.body;
	const lines = m.lines;
	const total = lines.reduce((s, l) => s + l.duration, 0);
	const offsets = lines.map((_, i) => lines.slice(0, i).reduce((s, l) => s + l.duration, 0));
	const blockOf = (l: ManifestLine) => (l.block === INTRO ? main.querySelector('h1') : main.querySelector(`[data-say="${CSS.escape(l.block)}"]`));

	// Two elements: one plays, the other loads the next line.
	const players = [new Audio(), new Audio()];
	for (const a of players) {
		a.preload = 'auto';
		a.preservesPitch = true;
	}
	let cur = 0; // the line playing or paused
	let which = 0; // players[which] holds line `cur`
	let playing = false;
	let speed = loadSpeed();
	let words: (Range | null)[] = [];
	let raf = 0;
	const highlights = 'highlights' in CSS ? CSS.highlights : null;

	// Controls.
	const bar = el('div', 'listen-bar');
	bar.setAttribute('role', 'group');
	bar.setAttribute('aria-label', 'Listen to this page');
	const play = el('button', 'listen-play');
	play.type = 'button';
	const playLabel = el('span', 'listen-label', 'Listen');
	play.append(icon('play'), playLabel);
	const prev = el('button', 'listen-skip');
	prev.type = 'button';
	prev.setAttribute('aria-label', 'Previous block');
	prev.title = 'Previous block (Left)';
	prev.append(icon('prev'));
	const next = el('button', 'listen-skip');
	next.type = 'button';
	next.setAttribute('aria-label', 'Next block');
	next.title = 'Next block (Right)';
	next.append(icon('next'));
	const time = el('span', 'listen-time');
	time.setAttribute('aria-label', 'Elapsed and total time');
	// Speed is a row of buttons, not a <select>: one click, and no native popup over the page.
	const speedGroup = el('span', 'listen-speed');
	speedGroup.setAttribute('role', 'group');
	speedGroup.setAttribute('aria-label', 'Playback speed');
	const speedButtons = SPEEDS.map((s) => {
		const b = el('button', 'listen-rate', `${s}×`);
		b.type = 'button';
		b.dataset.rate = String(s);
		b.setAttribute('aria-label', `${s} times speed`);
		speedGroup.append(b);
		return b;
	});
	const status = el('span', 'listen-status');
	status.setAttribute('role', 'status');
	// The voice is synthetic, and its reference recording asks for credit (CC BY): say both.
	const credit = el('span', 'listen-credit', 'Synthetic voice');
	if (m.voice?.attribution) credit.title = `Voice: ${m.voice.attribution}`;
	bar.append(play, prev, next, time, speedGroup, credit, status);
	root.append(bar);
	root.classList.add('ready');

	const elapsed = () => offsets[cur]! + (players[which]!.currentTime || 0);
	const showTime = () => {
		time.textContent = `${clock(elapsed())} / ${clock(total)}`;
	};
	const showPlaying = () => {
		play.replaceChildren(icon(playing ? 'pause' : 'play'), playLabel);
		playLabel.textContent = playing ? 'Pause' : elapsed() > 0 ? 'Resume' : 'Listen';
		play.setAttribute('aria-label', playing ? 'Pause narration' : 'Play narration');
		root.classList.toggle('playing', playing);
	};

	function setSpeed(v: number) {
		speed = v;
		for (const a of players) {
			a.playbackRate = v;
			a.defaultPlaybackRate = v;
			a.preservesPitch = true;
		}
		for (const b of speedButtons) b.setAttribute('aria-pressed', String(Number(b.dataset.rate) === v));
		saveSpeed(v);
	}
	setSpeed(speed);

	let shown: Element | null = null;
	function mark(i: number) {
		const b = blockOf(lines[i]!);
		if (b !== shown) {
			shown?.classList.remove('say-now');
			b?.classList.add('say-now');
			shown = b;
			if (b) {
				const r = b.getBoundingClientRect();
				if (r.top < 60 || r.bottom > innerHeight) b.scrollIntoView({ block: 'center', behavior: reducedMotion() ? 'auto' : 'smooth' });
			}
		}
		const found = b && !reducedMotion() ? match(lines[i]!.words, pageWords(b)) : [];
		// A line that is not the block's text (a podcast-style `Say`) would light up stray words: block highlight only.
		words = follows(lines[i]!.words, found) ? found : [];
		highlights?.delete('say-word');
	}

	function word() {
		if (!highlights) return;
		const t = players[which]!.currentTime;
		const ws = lines[cur]!.words;
		let k = -1;
		for (let j = 0; j < ws.length && ws[j]![1] <= t; j++) k = j;
		const r = k >= 0 && t <= ws[k]![2] + 0.35 ? words[k] : null;
		if (r) highlights.set('say-word', new Highlight(r));
		else highlights.delete('say-word');
	}

	function tick() {
		showTime();
		word();
		if (playing) raf = requestAnimationFrame(tick);
	}

	function load(i: number, into: number) {
		const a = players[into]!;
		const src = new URL(lines[i]!.src, base).href;
		if (a.src !== src) a.src = src;
		a.playbackRate = speed;
		a.preservesPitch = true;
	}

	/** Make line `i` current, from its start; keep playing if we were. */
	function go(i: number, autoplay = playing) {
		if (i < 0 || i >= lines.length) return;
		for (const a of players) a.pause();
		const ready = players[1 - which]!;
		if (i === cur + 1 && ready.src === new URL(lines[i]!.src, base).href) which = 1 - which;
		else load(i, which);
		cur = i;
		players[which]!.currentTime = 0;
		if (i + 1 < lines.length) load(i + 1, 1 - which);
		mark(i);
		showTime();
		if (autoplay) resume();
		else showPlaying();
	}

	function resume() {
		status.textContent = '';
		const a = players[which]!;
		if (!a.src) load(cur, which);
		if (shown === null) mark(cur);
		a.playbackRate = speed;
		a.play().then(
			() => {
				playing = true;
				showPlaying();
				cancelAnimationFrame(raf);
				raf = requestAnimationFrame(tick);
			},
			(e: unknown) => {
				playing = false;
				showPlaying();
				status.textContent = `Could not play: ${e instanceof Error ? e.message : String(e)}`;
			},
		);
	}

	function pause() {
		for (const a of players) a.pause();
		playing = false;
		cancelAnimationFrame(raf);
		highlights?.delete('say-word');
		showPlaying();
		showTime();
	}

	const toggle = () => (playing ? pause() : resume());

	for (const a of players) {
		a.addEventListener('ended', () => {
			if (a !== players[which]) return;
			if (cur + 1 < lines.length) go(cur + 1, true);
			else {
				pause();
				shown?.classList.remove('say-now');
				shown = null;
				cur = 0;
				which = 0;
				load(0, 0);
				players[0]!.currentTime = 0;
				playLabel.textContent = 'Listen';
				showTime();
			}
		});
		a.addEventListener('error', () => {
			if (a !== players[which]) return;
			pause();
			status.textContent = 'The audio could not be loaded.';
		});
	}

	play.addEventListener('click', toggle);
	prev.addEventListener('click', () => {
		// Within the first two seconds of a line, go back a line; later, back to its start (as players do).
		if ((players[which]!.currentTime || 0) > 2 && playing) go(cur);
		else go(cur - 1);
	});
	next.addEventListener('click', () => go(cur + 1));
	speedGroup.addEventListener('click', (e) => {
		const b = (e.target as HTMLElement).closest<HTMLButtonElement>('.listen-rate');
		if (b) setSpeed(Number(b.dataset.rate));
	});
	bar.addEventListener('keydown', (e) => {
		const t = e.target as HTMLElement;
		// Space plays and pauses from any of the player's buttons (on a speed button it picks that speed).
		if (e.key === 'k' || (e.key === ' ' && !t.classList.contains('listen-rate'))) {
			e.preventDefault();
			toggle();
		} else if (e.key === 'ArrowLeft' || e.key === 'ArrowRight') {
			e.preventDefault();
			go(cur + (e.key === 'ArrowRight' ? 1 : -1));
		}
	});

	// A button activates on Space's keyup: keep that from also pressing it.
	bar.addEventListener('keyup', (e) => {
		if (e.key === ' ' && !(e.target as HTMLElement).classList.contains('listen-rate')) e.preventDefault();
	});

	// "Play from here" among each block's hover controls.
	lines.forEach((l, i) => {
		if (l.block === INTRO) return;
		const b = main.querySelector(`[data-say="${CSS.escape(l.block)}"]`);
		if (!b || b instanceof HTMLImageElement) return;
		const btn = el('button', 'say-btn');
		btn.type = 'button';
		tipOn(btn, 'Play from here');
		btn.append(icon('play'));
		btn.addEventListener('click', (e) => {
			e.preventDefault();
			go(i, true);
		});
		const c = b.querySelector(':scope > .c-btn');
		if (c) c.before(btn);
		else b.append(btn);
	});

	if ('mediaSession' in navigator) {
		navigator.mediaSession.metadata = new MediaMetadata({ title: document.querySelector('h1')?.textContent ?? document.title, artist: 'blaze' });
		navigator.mediaSession.setActionHandler('play', resume);
		navigator.mediaSession.setActionHandler('pause', pause);
		navigator.mediaSession.setActionHandler('previoustrack', () => go(cur - 1));
		navigator.mediaSession.setActionHandler('nexttrack', () => go(cur + 1));
	}

	load(0, 0);
	if (lines.length > 1) load(1, 1);
	showTime();
	showPlaying();

	// Back where it was paused before a site update: the line and the time, but no highlight or scroll until Play.
	const at = from && from.doc === m.doc ? lines.findIndex((l) => l.id === from.line) : -1;
	if (at >= 0) {
		cur = at;
		load(at, 0);
		if (at + 1 < lines.length) load(at + 1, 1);
		const a = players[0]!;
		const seek = () => {
			a.currentTime = Math.min(from!.time, lines[at]!.duration);
			showTime();
			showPlaying();
		};
		if (a.readyState >= 1) seek();
		else a.addEventListener('loadedmetadata', seek, { once: true });
	}
	const keep = (e: Event) => {
		if (elapsed() > 0) (e as CustomEvent<Record<string, unknown>>).detail.listen = { doc: m.doc, line: lines[cur]!.id, time: players[which]!.currentTime } satisfies Kept;
	};
	addEventListener(SAVE, keep);

	return () => {
		removeEventListener(SAVE, keep);
		playing = false;
		cancelAnimationFrame(raf);
		for (const a of players) {
			a.pause();
			a.removeAttribute('src');
			a.load(); // drops what it was loading
		}
		highlights?.delete('say-word');
		shown?.classList.remove('say-now');
		if ('mediaSession' in navigator) {
			for (const action of ['play', 'pause', 'previoustrack', 'nexttrack'] as const) navigator.mediaSession.setActionHandler(action, null);
			navigator.mediaSession.metadata = null;
		}
	};
}

onPage((signal) => {
	const root = document.querySelector<HTMLElement>('.listen[data-audio]');
	if (!root) return;
	const url = new URL(root.dataset.audio!, location.href);
	// Taken now: the saved state is cleared once the page has loaded, before the manifest may arrive.
	const from = kept<Kept>('listen');
	let stop: (() => void) | undefined;
	void (async () => {
		try {
			const res = await fetch(url, { credentials: 'same-origin', signal });
			if (!res.ok) throw new Error(`HTTP ${res.status}`);
			const m = (await res.json()) as Manifest;
			if (signal.aborted || !m.lines?.length) return;
			stop = start(root, m, url, from);
			const main = root.closest('main');
			if (main) blockControls(main, signal);
		} catch (e) {
			if (signal.aborted) return;
			root.textContent = '';
			root.append(el('p', 'dim', 'The narration could not be loaded.'));
			console.warn('listen:', e);
		}
	})();
	return () => stop?.();
});
