/**
 * The intro, the site's front door (a `Hero` page, content/site.flecs; render.rs writes `<main class="intro">` with
 * the wordmark's still picture, an empty canvas and the page's blocks under them).
 *
 * - The wordmark is drawn live on the canvas (client/lib/wordmark.ts), big in the middle of the window: the five
 *   letters drop in one by one, each landing knocking the word; a beat after the last, the fire catches from the b to
 *   the e and burns on. Once it is alight the line and the Start button under it show. A key or a press shows them
 *   at once.
 * - Every cell is a whole number of device pixels, and the canvas sits on a device pixel, so the pixels stay sharp
 *   at any zoom. The pointer's breath parts the flames and its sweep drags them.
 * - Under prefers-reduced-motion nothing drops and nothing moves: the word is in place, burning, held still.
 * - Sound: none until the reader asks (browsers allow none, and nobody wants a page that starts talking). The
 *   speaker in the corner plays the intro again from the start with its soundtrack (client/lib/intro-sound.ts), the
 *   picture started on the moment the sound is, then a small fire burns on; pressing it again, hiding the tab or
 *   leaving the page stops it. The Start button knocks when the site's sound is on (`blaze.sound`, on unless the
 *   reader turned it off), and the page waits a tenth of a second for the knock before it goes.
 * - A reader who has finished the lesson the Start button leads to is sent on to the course map instead.
 *
 * Without JS the page is the still picture, the line and the button.
 */

import { icon } from './lib/icon';
import { hidden, leave, play, sounding, stop, tap } from './lib/intro-sound';
import { FPS, IGNITE_AT, LANDED, LIT } from './lib/intro-timeline';
import { onPage } from './lib/page';
import { load } from './lib/progress';
import { Fire, LEAD, SHOWN, THEMES, W, draw, knock } from './lib/wordmark';

/** The word is as big as the window lets it be: this share of its width and of its height, up to this many CSS px a cell. */
const WIDE = 0.86;
const TALL = 0.46;
const CELL_MAX = 22;
/** The fire steps the still frame has burnt (reduced motion): the flames at their full height. */
const WARM = 48;
/** How long the Start button's knock is heard before the page goes, in ms: its body, not its ring. */
const KNOCK_MS = 110;

const still = matchMedia('(prefers-reduced-motion: reduce)');
const light = matchMedia('(prefers-color-scheme: light)');

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

	let lit = false;
	const reveal = () => {
		if (lit) return;
		lit = true;
		main.dataset.lit = '';
	};

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
	addEventListener(
		'pagehide',
		() => {
			++asked;
			leave();
			paint('off');
		},
		{ signal },
	);

	// The Start button.
	const go = main.querySelector<HTMLAnchorElement>('.go a');
	// Lesson 1 lived here before the intro, so an old `/#<step>` link names one of its steps: send it on, step kept.
	// The intro has no anchors of its own.
	if (go && location.hash.length > 1) {
		location.replace(go.getAttribute('href') + location.hash);
		return;
	}
	if (go) {
		const { lesson, course } = go.dataset;
		if (lesson && course && load().lessons[lesson]?.done) {
			go.textContent = 'Continue the course';
			go.href = course;
		}
		// A mouse's press is a gesture already, so the knock lands with the press, not the release.
		let knocked = -Infinity;
		go.addEventListener(
			'pointerdown',
			(e) => {
				if (e.button === 0 && e.pointerType === 'mouse' && tap()) knocked = performance.now();
			},
			{ signal },
		);
		go.addEventListener(
			'click',
			(e) => {
				if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
				if (performance.now() - knocked > 600) {
					if (!tap()) return;
					knocked = performance.now();
				}
				const wait = KNOCK_MS - (performance.now() - knocked);
				if (wait <= 0) return;
				e.preventDefault();
				setTimeout(() => location.assign(go.href), wait);
			},
			{ signal },
		);
	}

	return () => {
		cancelAnimationFrame(frame);
		if (sounding()) leave();
		snd.remove();
		delete main.dataset.on;
		delete main.dataset.lit;
	};
});
