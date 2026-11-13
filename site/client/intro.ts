/**
 * The intro, the site's front door (a `Hero` page, content/site.flecs; render.rs writes `<main class="intro">` with
 * the wordmark's still picture, an empty canvas and the page's blocks under them).
 *
 * - The wordmark is drawn live on the canvas (client/lib/wordmark.ts), big in the window: the five letters drop in one
 *   by one, each landing knocking the word; a beat after the last, the fire catches from the b to the e and burns on.
 *   Once it is alight the rest of the page shows. A key or a press shows it at once.
 * - Every cell is a whole number of device pixels, and the canvas sits on a device pixel, so the pixels stay sharp
 *   at any zoom. The pointer's breath parts the flames and its sweep drags them.
 * - The command (the `Lede`, `cargo build` → `blaze build` as written): once the page lit it reads `cargo build`,
 *   then `cargo` burns off letter by letter and `blaze` catches in its place, then the subcommand cycles through
 *   blaze's own (build, check, test, run), each typed over the last. Same commands, another tool.
 * - The race (a `Race` block: blaze and cargo building the same edit, the two times a measurement from the benchmark
 *   history) runs in real time, over and over: both lanes start together, each fills at the pace it was measured at
 *   and stops on its time, so blaze is done before the eye has moved and cargo is still going.
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
/** The command: when `cargo` starts to burn and each letter's lag, how long a letter burns, when `blaze` catches and
 * each letter's lag, how long a caught letter takes to cool, in seconds after the page lit. */
const CMD_BURN = 0.9;
const CMD_BURN_LAG = 0.09;
const CMD_BURN_FOR = 0.4;
const CMD_CATCH = 1.55;
const CMD_CATCH_LAG = 0.07;
const CMD_COOL = 0.5;
/** Then the subcommands, from this moment, each held this long, typed and untyped this fast a letter (seconds). */
const CMD_CYCLE_AT = 3.2;
const CMD_HOLD = 1.7;
const CMD_KEY = 0.055;
/** blaze's commands, as blaze-thin and blaze-client route them (route.rs, ask.rs). */
const SUBCOMMANDS = ['build', 'check', 'test', 'run'];
/** The fire's ramp, hottest first: a letter burning goes down it; one catching comes up it and cools to the ink. */
const RAMP = ['#fff4c4', '#ffd23f', '#ff9a26', '#e0461a', '#9a2c10'];

/** The race: a beat with both lanes empty, the run, then the result held, in seconds. */
const RACE_READY = 0.4;
const RACE_HOLD = 2.6;

const still = matchMedia('(prefers-reduced-motion: reduce)');
const light = matchMedia('(prefers-color-scheme: light)');

type Lane = { ms: number; cells: number; rects: SVGRectElement[]; took: HTMLElement; lit: number; text: string };

/**
 * The race, run in real time. Each lane fills, cell by cell, at the pace its build was measured at (the lanes share
 * one scale: the slower one's time is the whole lane) and its counter stops on its time. `frame(t)` paints the
 * moment `t` seconds after the page lit; as rendered, and with motion reduced, the lanes stand finished.
 */
function racer(fig: HTMLElement): (t: number) => void {
	const lanes: Lane[] = [...fig.querySelectorAll<HTMLElement>('.lane')].flatMap((el) => {
		const ms = Number(fig.dataset[el.classList.contains('blaze') ? 'blazeMs' : 'cargoMs']);
		const took = el.querySelector<HTMLElement>('.took b');
		const rects = [...el.querySelectorAll<SVGRectElement>('.cells rect')];
		const cells = Number(el.dataset.cells);
		return took && ms > 0 && cells > 0 ? [{ ms, cells, rects, took, lit: cells, text: took.textContent ?? '' }] : [];
	});
	const whole = Math.max(...lanes.map((l) => l.ms));
	const period = RACE_READY + whole / 1000 + RACE_HOLD;
	// A time as render.rs writes it: whole milliseconds under a second, else seconds to two places.
	const show = (ms: number, final: number) => (final < 1000 ? String(Math.round(ms)) : (ms / 1000).toFixed(2));
	return (t) => {
		const ran = (((t % period) + period) % period) - RACE_READY;
		for (const l of lanes) {
			const at = Math.min(Math.max(0, ran * 1000), l.ms);
			const lit = at >= l.ms ? l.cells : Math.min(l.cells, Math.floor((l.cells * at) / l.ms));
			if (lit !== l.lit) {
				for (let i = Math.min(lit, l.lit); i < Math.max(lit, l.lit); i++) l.rects[i]?.classList.toggle('on', i < lit);
				l.lit = lit;
			}
			const text = show(at, l.ms);
			if (text !== l.text) l.took.textContent = l.text = text;
		}
	};
}

/**
 * The command, run in real time (see the top). It takes over the `Lede`'s first `code` and paints the moment `t`
 * seconds after the page lit. `null` when the lede is not the command it expects, which leaves it as written.
 */
function typer(lede: HTMLElement): ((t: number) => void) | null {
	const codes = lede.querySelectorAll('code');
	if (codes.length !== 2 || codes[0].textContent !== 'cargo build') return null;
	const code = document.createElement('code');
	code.className = 'cmd';
	const old = [...'cargo'].map((c) => Object.assign(document.createElement('span'), { textContent: c }));
	const now = [...'blaze'].map((c) => Object.assign(document.createElement('span'), { textContent: c, className: 'cmd-new' }));
	const tool = document.createElement('span');
	tool.className = 'tool';
	tool.append(...old, ...now);
	const sub = document.createElement('span');
	sub.className = 'sub';
	const caret = document.createElement('span');
	caret.className = 'caret';
	code.append(tool, ' ', sub, caret);
	lede.replaceChildren(code);
	const ramp = (u: number) => RAMP[Math.min(RAMP.length - 1, Math.max(0, Math.floor(u * RAMP.length)))];
	const cycle = SUBCOMMANDS.length * CMD_HOLD;
	let shown = '';
	return (t) => {
		old.forEach((el, i) => {
			const u = (t - CMD_BURN - i * CMD_BURN_LAG) / CMD_BURN_FOR;
			el.style.display = u >= 1 ? 'none' : '';
			el.style.color = u <= 0 ? '' : ramp(u);
			el.style.transform = u <= 0 ? '' : `translateY(${-u * 0.35}em)`;
			el.style.opacity = u <= 0 ? '' : String(1 - u * u);
		});
		now.forEach((el, i) => {
			const u = (t - CMD_CATCH - i * CMD_CATCH_LAG) / CMD_COOL;
			el.style.display = u < 0 ? 'none' : '';
			el.style.color = u >= 1 ? '' : ramp(1 - u);
		});
		// The subcommand: `build` until the cycle starts, then each typed over the last.
		let word = 'build';
		let typing = false;
		if (t >= CMD_CYCLE_AT) {
			const at = (t - CMD_CYCLE_AT) % cycle;
			const k = Math.floor(at / CMD_HOLD);
			const into = at - k * CMD_HOLD;
			const from = SUBCOMMANDS[k];
			const to = SUBCOMMANDS[(k + 1) % SUBCOMMANDS.length];
			const out = from.length * CMD_KEY;
			const back = CMD_HOLD - out - to.length * CMD_KEY;
			if (into < back) word = from;
			else if (into < back + out) ((word = from.slice(0, from.length - Math.ceil((into - back) / CMD_KEY))), (typing = true));
			else ((word = to.slice(0, Math.ceil((into - back - out) / CMD_KEY))), (typing = true));
		}
		if (word !== shown) sub.textContent = shown = word;
		caret.classList.toggle('on', typing || (t >= CMD_BURN && t < CMD_CYCLE_AT));
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
	const race = fig && racer(fig);
	const lede = main.querySelector<HTMLElement>('.lede');
	const command = lede && !still.matches ? typer(lede) : null;

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
			command?.((now - lit) / 1000);
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
