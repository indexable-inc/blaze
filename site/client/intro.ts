/**
 * The intro, the site's front door (a `Hero` page, content/site.flecs; render.rs writes `<main class="intro">` with
 * the wordmark's still picture, an empty canvas and the page's blocks under them).
 *
 * - The wordmark is drawn live on the canvas (client/lib/wordmark.ts), big in the window: the five letters drop in one
 *   by one, each landing knocking the word; a beat after the last, the fire catches from the b to the e and burns on.
 *   Once it is alight the rest of the page shows. A key or a press shows it at once.
 * - Every cell is a whole number of device pixels, and the canvas sits on a device pixel, so the pixels stay sharp
 *   at any zoom. The pointer's breath parts the flames and its sweep drags them.
 * - The race (a `Race` block: blaze and cargo building the same edit, the two times a measurement from the benchmark
 *   history) is two terminals side by side, run in real time, over and over: both type their command, both start on
 *   the same keystroke, each prints `Finished` at the moment it was measured to, a clock counting on each, so blaze is
 *   done before the eye has moved and cargo is still compiling.
 * - Start: the page burns away (client/lib/burn.ts), from the bottom left up and across in about three quarters of a
 *   second, and then the lesson loads and fades in (the site's page transition, styles/shell.css). A key or a second
 *   press during it goes at once. Back brings the intro back whole, not burnt.
 * - Under prefers-reduced-motion nothing drops and nothing moves: the word is in place, burning, held still, the race
 *   stands finished, and Start goes at once.
 * - Sound: none until the reader asks (browsers allow none, and nobody wants a page that starts talking). The
 *   speaker in the corner plays the intro again from the start with its soundtrack (client/lib/intro-sound.ts), the
 *   picture started on the moment the sound is, then a small fire burns on; pressing it again, hiding the tab or
 *   leaving the page stops it. Start knocks when the site's sound is on (`blaze.sound`, on unless the reader turned
 *   it off), and the fire starts on the knock.
 * - A reader who has finished the lesson the Start button leads to is sent on to the course map instead.
 *
 * Without JS the page is the still picture, the line, the race standing finished, and the links.
 */

import { BURNT, burn } from './lib/burn';
import { icon } from './lib/icon';
import { hidden, leave, play, ready, sounding, stop, tap } from './lib/intro-sound';
import { FPS, IGNITE_AT, LANDED, LIT } from './lib/intro-timeline';
import { onPage } from './lib/page';
import { load } from './lib/progress';
import { Fire, LEAD, SHOWN, THEMES, W, draw, knock } from './lib/wordmark';

/**
 * The word is as big as the window lets it be: this share of its width and of its height, up to this many CSS px a
 * cell. The height's share leaves room under it for the line, the race, the button and the links without a scroll.
 */
const WIDE = 0.86;
const TALL = 0.4;
const CELL_MAX = 22;
/** The fire steps the still frame has burnt (reduced motion): the flames at their full height. */
const WARM = 48;
/** The longest the exit waits for the Start knock to be heard before it starts without it, in ms. */
const KNOCK_WAIT = 120;
/** A page that is still here this long after it burnt (the load was stopped, or failed) is put back, in ms. */
const STRANDED = 5000;
/** The race: an empty prompt, the commands typed (seconds a key), the run, then the result held, in seconds. */
const RACE_READY = 0.5;
const RACE_KEY = 0.045;
const RACE_ENTER = 0.25;
const RACE_HOLD = 2.8;

const still = matchMedia('(prefers-reduced-motion: reduce)');
const light = matchMedia('(prefers-color-scheme: light)');

type Term = { ms: number; typed: HTMLElement; caret: HTMLElement; took: HTMLElement; outs: { el: HTMLElement; at: number }[]; text: string; word: string };

/**
 * The race, run in real time (see the top). `frame(t)` paints the moment `t` seconds after the page lit; as rendered,
 * and with motion reduced, both terminals stand finished.
 */
function racer(fig: HTMLElement): (t: number) => void {
	const terms: Term[] = [...fig.querySelectorAll<HTMLElement>('.term')].flatMap((el) => {
		const ms = Number(fig.dataset[el.classList.contains('blaze') ? 'blazeMs' : 'cargoMs']);
		const typed = el.querySelector<HTMLElement>('.typed');
		const caret = el.querySelector<HTMLElement>('.caret');
		const took = el.querySelector<HTMLElement>('.took b');
		if (!typed || !caret || !took || !(ms > 0)) return [];
		const outs = [...el.querySelectorAll<HTMLElement>('.out')].map((o) => ({ el: o, at: o.dataset.at === 'end' ? ms : Number(o.dataset.at) }));
		return [{ ms, typed, caret, took, outs, text: took.textContent ?? '', word: typed.textContent ?? '' }];
	});
	const longest = Math.max(...terms.map((m) => m.word.length));
	const go = RACE_READY + longest * RACE_KEY + RACE_ENTER;
	const whole = Math.max(...terms.map((m) => m.ms));
	const period = go + whole / 1000 + RACE_HOLD;
	// A time as render.rs writes it: whole milliseconds under a second, else seconds to two places.
	const show = (ms: number, final: number) => (final < 1000 ? String(Math.round(ms)) : (ms / 1000).toFixed(2));
	for (const m of terms) m.typed.textContent = '';
	return (t) => {
		const at = ((t % period) + period) % period;
		const ran = (at - go) * 1000;
		for (const m of terms) {
			const keys = Math.max(0, Math.min(m.word.length, Math.floor((at - RACE_READY) / RACE_KEY)));
			const typed = m.word.slice(0, keys);
			if (m.typed.textContent !== typed) m.typed.textContent = typed;
			const done = ran >= m.ms;
			m.caret.classList.toggle('on', !done);
			for (const o of m.outs) o.el.hidden = ran < o.at;
			const text = show(Math.min(Math.max(0, ran), m.ms), m.ms);
			if (text !== m.text) m.took.textContent = m.text = text;
			m.took.parentElement?.classList.toggle('done', done);
		}
	};
}

onPage((signal) => {
	const main = document.querySelector<HTMLElement>('main.intro');
	const box = main?.querySelector<HTMLElement>('.wordmark');
	const canvas = box?.querySelector('canvas');
	const ctx = canvas?.getContext('2d');
	if (!main || !box || !canvas || !ctx) return;

	const fire = new Fire();
	const theme = () => (light.matches ? THEMES.light : THEMES.dark);
	/** Device pixels a cell, and the same in CSS px. */
	let px = 1;
	let cell = 1;
	/** The fire step last drawn; -1 forces a draw. */
	let drawn = -1;
	/** The timeline's zero on the page's clock (the first frame drawn, so a tab opened in the background plays it when
	 * it is first looked at), and how long the fire has burnt. */
	let begin = Number.NaN;
	let burnt = 0;
	let last = 0;
	let frame = 0;

	const fit = () => {
		const dpr = devicePixelRatio || 1;
		px = Math.max(1, Math.floor(Math.min((WIDE * innerWidth) / W, (TALL * innerHeight) / SHOWN, CELL_MAX) * dpr));
		cell = px / dpr;
		box.style.width = `${W * cell}px`;
		box.style.height = `${SHOWN * cell}px`;
		canvas.width = W * px;
		canvas.height = (SHOWN + LEAD) * px;
		canvas.style.width = `${W * cell}px`;
		canvas.style.height = `${(SHOWN + LEAD) * cell}px`;
		// The box is centred, which can leave it between device pixels: the canvas moves onto the nearest one.
		const r = box.getBoundingClientRect();
		const snap = (v: number) => Math.round(v * dpr) / dpr - v;
		canvas.style.left = `${snap(r.left)}px`;
		canvas.style.top = `${snap(r.top + scrollY) - LEAD * cell}px`;
		drawn = -1;
		if (still.matches) hold();
	};

	/** The still frame: the letters in place and the fire burning, held. */
	const hold = () => draw(ctx, fire, LANDED + 1, px, theme());

	/** The moment the rest of the page showed, on the page's clock: the race's zero. */
	let lit = Number.NaN;
	const reveal = () => {
		if (!Number.isNaN(lit)) return;
		lit = performance.now();
		main.dataset.lit = '';
	};
	const fig = main.querySelector<HTMLElement>('.race');
	const race = fig && !still.matches ? racer(fig) : null;

	const tick = (now: number) => {
		frame = requestAnimationFrame(tick);
		if (Number.isNaN(begin)) begin = last = now;
		const t = (now - begin) / 1000;
		const dt = Math.min(0.1, Math.max(0, (now - last) / 1000));
		last = now;
		if (t >= IGNITE_AT) {
			burnt += dt;
			while (fire.steps < Math.floor(burnt * FPS)) fire.step();
		}
		if (t >= LIT) reveal();
		if (!Number.isNaN(lit)) {
			race?.((now - lit) / 1000);
		}
		// Every frame while a letter is falling or the word still shakes; after that only when the fire has stepped.
		const moving = t < LANDED + 0.3;
		if (!moving && fire.steps === drawn) return;
		draw(ctx, fire, t, px, theme());
		drawn = fire.steps;
		const [kx, ky] = knock(t);
		canvas.style.transform = kx === 0 && ky === 0 ? '' : `translate(${kx * cell}px, ${ky * cell}px)`;
	};

	/** The intro again from its start, its timeline's zero at `at` on the page's clock. */
	const restart = (at: number) => {
		fire.reset();
		begin = at;
		burnt = 0;
		drawn = -1;
	};

	main.dataset.on = '';
	if (still.matches) {
		for (let i = 0; i < WARM; i++) fire.step();
		reveal();
	}
	fit();
	if (!still.matches) frame = requestAnimationFrame(tick);
	addEventListener('resize', fit, { signal });
	// The text under the word settles once its font is in, which can move the word by part of a pixel.
	addEventListener('load', fit, { signal });
	void document.fonts?.ready.then(() => signal.aborted || fit());
	light.addEventListener(
		'change',
		() => {
			drawn = -1;
			if (still.matches) hold();
		},
		{ signal },
	);
	// Anyone who starts using the page gets it whole at once.
	addEventListener('keydown', reveal, { signal });
	addEventListener('pointerdown', reveal, { signal });

	box.addEventListener(
		'pointermove',
		(e) => {
			const r = box.getBoundingClientRect();
			fire.blow((e.clientX - r.left) / cell, (e.clientY - r.top) / cell);
		},
		{ signal },
	);
	box.addEventListener('pointerleave', () => (fire.pointer = null), { signal });

	// The speaker: the intro again, with its soundtrack.
	const snd = document.createElement('button');
	snd.type = 'button';
	snd.className = 'intro-snd';
	let asked = 0;
	const paint = (state: 'off' | 'wait' | 'on') => {
		snd.dataset.state = state;
		snd.innerHTML = icon(state === 'off' ? 'muted' : 'sound');
		snd.setAttribute('aria-pressed', String(state !== 'off'));
		snd.setAttribute('aria-label', state === 'off' ? 'Play the intro with sound' : 'Sound on');
	};
	paint('off');
	snd.addEventListener(
		'click',
		async () => {
			const n = ++asked;
			if (snd.dataset.state !== 'off') {
				stop();
				paint('off');
				return;
			}
			paint('wait');
			const at = await play();
			if (n !== asked) return;
			paint(at === null ? 'off' : 'on');
			if (at !== null && !still.matches) restart(at);
		},
		{ signal },
	);
	main.append(snd);
	document.addEventListener('visibilitychange', () => hidden(document.hidden), { signal });

	/**
	 * The exit: the page burns away, then `href` loads. The fire starts on the knock when there is one (never waiting
	 * more than KNOCK_WAIT for it). A key or a press while it burns goes at once. `undo` puts the page back whole,
	 * for a reader who comes back to it out of the back/forward cache.
	 */
	let undo: (() => void) | null = null;
	const exit = (href: string) => {
		if (undo) return;
		const pressed = performance.now();
		const dpr = devicePixelRatio || 1;
		const r = canvas.getBoundingClientRect();
		const fireOver = burn(px, Math.round(r.left * dpr), Math.round(r.top * dpr), theme(), getComputedStyle(document.documentElement).backgroundColor);
		document.body.append(fireOver.canvas);
		const gone = new AbortController();
		let burning = 0;
		let went = false;
		const go = () => {
			if (went) return;
			went = true;
			location.assign(href);
			setTimeout(() => went && undo?.(), STRANDED);
		};
		const step = (now: number) => {
			const t = (now - at) / 1000;
			fireOver.draw(t);
			if (t >= BURNT) go();
			// On after the page is asked for: the embers go out while the next one loads.
			burning = requestAnimationFrame(step);
		};
		// With no knock the fire starts on the press. With one it starts when the knock is heard: now, or the moment
		// it is due, or KNOCK_WAIT after the press if the sound is slower than that. It only ever moves earlier, and
		// never once it is burning.
		const heard = tap();
		let at = heard ? pressed + KNOCK_WAIT : pressed;
		const sooner = (when: number) => (at = Math.min(at, Math.max(when, performance.now())));
		heard?.then(sooner, () => sooner(0));
		burning = requestAnimationFrame(step);
		addEventListener('keydown', go, { signal: gone.signal });
		addEventListener('pointerdown', go, { signal: gone.signal });
		undo = () => {
			undo = null;
			went = false;
			gone.abort();
			cancelAnimationFrame(burning);
			fireOver.canvas.remove();
		};
	};
	addEventListener(
		'pagehide',
		() => {
			++asked;
			leave();
			paint('off');
		},
		{ signal },
	);
	// Back, out of the back/forward cache: the page as it was before it burnt.
	addEventListener('pageshow', (e) => e.persisted && undo?.(), { signal });

	// The Start button.
	const go = main.querySelector<HTMLAnchorElement>('.go a');
	if (go) {
		const { lesson, course } = go.dataset;
		if (lesson && course && load().lessons[lesson]?.done) {
			go.textContent = 'Continue the course';
			go.href = course;
		}
		// A mouse's press comes a moment before its click: the sound is made ready then, so the knock is on time.
		go.addEventListener('pointerdown', (e) => e.button === 0 && ready(), { signal });
		go.addEventListener(
			'click',
			(e) => {
				// A new tab, a download, a window: the browser's, and this page stays as it is.
				if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
				if (still.matches) {
					// Nothing burns and nothing waits; the knock rings for as long as the page takes to go.
					tap()?.catch(() => {});
					return;
				}
				e.preventDefault();
				exit(go.href);
			},
			{ signal },
		);
	}

	return () => {
		cancelAnimationFrame(frame);
		undo?.();
		if (sounding()) leave();
		snd.remove();
		delete main.dataset.on;
		delete main.dataset.lit;
	};
});
