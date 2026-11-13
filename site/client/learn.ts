/**
 * The course (lesson 1 is /start, the other lessons are at /learn/<slug>, the updates at /updates/<slug>, the
 * course map at /learn and What's new at /updates; Lesson, Update, Step, Quiz, Tap and Figure in content/schema.flecs,
 * rendered by the renderer's render.rs). Without JS a lesson or update is one page of steps under each other, each
 * question with its answer in a fold. This script turns it into the lesson player, one step at a time, in the page's
 * column with the sidebar beside it:
 *
 * - at the top, the lesson's name and its steps as segments (a seen one goes back to its step); at the bottom, Back and
 *   the one big button (Check, Continue, Finish), where a question's feedback appears too;
 * - it opens on the first step (or where the reader left off), and ends on a short "lesson complete";
 * - a question has to be answered before Continue: a wrong pick says why in one line and is crossed off, and the
 *   reader picks again. A Tap question (tap the parts of a diagram) gives its hint on a miss and, after two, offers
 *   the answer;
 * - keys: Enter is the big button, the arrows step back and forward, 1 to 9 pick a choice.
 *
 * On the course map it lights the course path (`.course`, render.rs `course`, a graph course.rs lays out) with the
 * reader's progress: each node and its mastery cells (one per question, solid when it was right the first time), the
 * edges from finished nodes inked, the updates new since the reader's last visit marked New, Start or Continue on what
 * comes next, and the big button pointed at it. On What's new it marks each update the same way.
 *
 * Progress stays in this browser (client/lib/progress.ts, localStorage `blaze.learn`): per lesson the step to come back to, the furthest
 * step seen, whether it is finished, and each question's tries. Storage can be missing or refuse writes (private
 * windows, blocked site data), so every access is wrapped and the course works for the visit from memory instead.
 * Diagrams are client/lib/diagrams.ts, and the sounds (a chime when right, a soft low tone when not, a tick to move on;
 * the speaker in the top bar mutes them) client/lib/sound.ts.
 */

import { mount, type Diagram } from './lib/diagrams';
import { icon } from './lib/icon';
import { onPage } from './lib/page';
import { CHANGED, KEY, type Saved, clearFresh, firstTime, fresh as freshOf, load, progress, roll, stateOf, stateWords, store } from './lib/progress';
import { enabled as soundOn, setEnabled as setSound, sound } from './lib/sound';

const still = matchMedia('(prefers-reduced-motion: reduce)');

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls?: string, text?: string): HTMLElementTagNameMap[K] {
	const e = document.createElement(tag);
	if (cls) e.className = cls;
	if (text !== undefined) e.textContent = text;
	return e;
}

/** A check mark in pixel cells on a faint 9 by 7 grid, as the lessons' drawings are made. */
function seal(): string {
	const on = new Set(['1,3', '2,4', '3,5', '4,4', '5,3', '6,2', '7,1']);
	let cells = '';
	for (let y = 0; y < 7; y++)
		for (let x = 0; x < 9; x++) cells += `<rect x="${x + 0.1}" y="${y + 0.1}" width="0.8" height="0.8"${on.has(`${x},${y}`) ? ' class="on"' : ''}/>`;
	return `<svg viewBox="0 0 9 7">${cells}</svg>`;
}

/** Follows a link through the client router (client/nav.ts listens for clicks on links). */
function go(href: string) {
	const a = el('a');
	a.href = href;
	a.hidden = true;
	document.body.append(a);
	a.click();
	a.remove();
}

/** One question on a step: a multiple choice, or a Tap on a diagram. */
type Question = {
	id: string;
	/** Something is picked, so Check can run. */
	ready(): boolean;
	/** Checks the pick: right, or the line that says why not. */
	check(): { right: boolean; say: string };
	/** Shows the question answered: the right answer marked, nothing left to pick. */
	settle(): void;
	/** Picks choice `i` (the 1 to 9 keys), when it is a multiple choice. */
	key?(i: number): void;
	/** After two misses, a Tap question can show its answer. */
	reveal?(): string;
	stop(): void;
};

function choiceQuestion(q: HTMLElement, onChange: () => void): Question {
	const why = new Map<HTMLButtonElement, string>();
	let right: HTMLButtonElement | null = null;
	const buttons: HTMLButtonElement[] = [];
	q.querySelectorAll<HTMLLIElement>('li.choice').forEach((li, i) => {
		const b = el('button', 'pick');
		b.type = 'button';
		b.setAttribute('aria-pressed', 'false');
		b.innerHTML = `<span class="k" aria-hidden="true">${i + 1}</span><span class="text">${li.querySelector('.text')?.innerHTML ?? ''}</span>`;
		why.set(b, li.querySelector('.why')?.innerHTML ?? '');
		if (li.dataset.right !== undefined) right = b;
		li.replaceChildren(b);
		buttons.push(b);
	});
	const picked = () => buttons.find((b) => b.getAttribute('aria-pressed') === 'true') ?? null;
	const choose = (b: HTMLButtonElement) => {
		if (b.disabled) return;
		for (const o of buttons) o.setAttribute('aria-pressed', String(o === b));
		onChange();
	};
	for (const b of buttons) b.addEventListener('click', () => choose(b));
	return {
		id: q.dataset.quiz!,
		ready: () => picked() !== null,
		check() {
			const b = picked()!;
			if (b === right) {
				b.dataset.mark = 'right';
				return { right: true, say: why.get(b)! };
			}
			b.dataset.mark = 'wrong';
			b.disabled = true;
			b.setAttribute('aria-pressed', 'false');
			return { right: false, say: why.get(b)! };
		},
		settle() {
			for (const b of buttons) {
				b.disabled = true;
				b.setAttribute('aria-pressed', String(b === right));
				if (b === right) b.dataset.mark = 'right';
			}
		},
		key(i) {
			const b = buttons[i];
			if (b) {
				choose(b);
				b.focus();
			}
		},
		stop() {},
	};
}

function tapQuestion(q: HTMLElement, onChange: () => void): Question | null {
	const fig = q.querySelector<HTMLElement>('.diagram');
	const want = new Set((q.dataset.right ?? '').split(',').filter(Boolean));
	const hint = q.querySelector('.hint')?.innerHTML ?? '';
	const why = q.querySelector('details.answer p')?.innerHTML ?? '';
	if (!fig) return null;
	const d: Diagram | null = mount(fig, true, onChange);
	if (!d) return null;
	const same = (a: Set<string>) => a.size === want.size && [...a].every((x) => want.has(x));
	return {
		id: q.dataset.quiz!,
		ready: () => d.picked().size > 0,
		check() {
			const got = d.picked();
			d.mark(want);
			if (same(got)) {
				d.lock();
				return { right: true, say: why };
			}
			const extra = [...got].filter((x) => !want.has(x)).length;
			const missing = [...want].filter((x) => !got.has(x)).length;
			const what =
				extra && missing ? 'Some of those are wrong, and some are missing.' : extra ? `${extra === 1 ? 'One of those doesn’t' : 'Some of those don’t'} belong.` : `${missing === 1 ? 'One is' : 'Some are'} missing.`;
			return { right: false, say: `${what} ${hint}` };
		},
		settle() {
			d.pick(want);
			d.mark(want);
			d.lock();
		},
		reveal() {
			this.settle();
			return why;
		},
		stop: () => d.stop(),
	};
}

/**
 * The player, in the page's own column with the sidebar beside it. Screens 0..n-1 are the steps and n the end. At the
 * top: the lesson's eyebrow and a row of segments, one per step (filled once seen, ringed for a question; a seen one
 * goes back to its step). At the bottom: Back and the one big button, with a question's feedback above them.
 */
function player(main: HTMLElement, signal: AbortSignal): () => void {
	const lesson = main.dataset.lesson!;
	const course = main.dataset.course!;
	const steps = [...main.querySelectorAll<HTMLElement>(':scope > section.step')];
	if (!steps.length) return () => {};
	const quizIds = (main.dataset.quizzes ?? '').split(' ').filter(Boolean);
	const title = main.querySelector('.head h1')?.textContent ?? document.title;
	let saved = load();
	let me = progress(saved, lesson);
	const n = steps.length;
	// An update (render.rs gives it data-log, What's new) says so where a lesson says lesson.
	const log = main.dataset.log;
	const kind = log ? 'Update' : 'Lesson';

	const end = el('section', 'step lp-end');
	end.innerHTML = `<div class="lp-seal" aria-hidden="true">${seal()}</div><h2>${kind} complete</h2><p class="lp-score"></p><p class="lp-next"></p>`;
	const screens = [...steps, end];
	steps[n - 1]!.after(end);

	// The top: which lesson, and the steps as segments.
	const top = el('div', 'lp-top');
	const minutes = Math.max(1, Math.ceil((n * 20) / 60));
	top.innerHTML = `<div class="lp-head"><p class="lp-eyebrow"><span class="lp-k"></span><span class="lp-t"></span></p><span class="lp-count"></span><button type="button" class="lp-snd"></button></div><ol class="lp-segs" aria-label="Steps"></ol>`;
	top.querySelector('.lp-k')!.textContent = main.dataset.eyebrow ?? kind;
	top.querySelector('.lp-t')!.textContent = title;
	top.querySelector('.lp-eyebrow')!.setAttribute('title', `${n} steps · ${minutes} min${quizIds.length ? ` · ${quizIds.length} questions` : ''}`);
	const segs = top.querySelector('.lp-segs')!;
	const segBtns = steps.map((s, i) => {
		const li = el('li');
		const b = el('button');
		b.type = 'button';
		const h = s.querySelector('h2')?.textContent ?? '';
		b.setAttribute('aria-label', `Step ${i + 1}: ${h}`);
		b.title = h;
		if (s.querySelector('.quiz')) b.dataset.q = '';
		b.addEventListener('click', () => {
			if (i < me.seen || me.done) show(i, i < at ? -1 : 1);
		});
		li.append(b);
		segs.append(li);
		return b;
	});
	const snd = top.querySelector<HTMLButtonElement>('.lp-snd')!;
	const paintSound = () => {
		snd.innerHTML = icon(soundOn() ? 'sound' : 'muted');
		snd.setAttribute('aria-label', soundOn() ? 'Sound on' : 'Sound off');
		snd.setAttribute('aria-pressed', String(soundOn()));
	};
	paintSound();
	snd.addEventListener('click', () => {
		setSound(!soundOn());
		paintSound();
		sound.pick();
	});
	const count = top.querySelector<HTMLElement>('.lp-count')!;
	const foot = el('div', 'lp-foot');
	foot.innerHTML = `<div class="lp-fb" aria-live="polite"><p class="lp-fb-t"></p><p class="lp-fb-w"></p></div><div class="lp-row"><button type="button" class="lp-back" aria-label="Back">${icon('back')}</button><button type="button" class="lp-go"></button></div>`;
	const fb = foot.querySelector<HTMLElement>('.lp-fb')!;
	const fbT = foot.querySelector<HTMLElement>('.lp-fb-t')!;
	const fbW = foot.querySelector<HTMLElement>('.lp-fb-w')!;
	const backBtn = foot.querySelector<HTMLButtonElement>('.lp-back')!;
	const goBtn = foot.querySelector<HTMLButtonElement>('.lp-go')!;
	steps[0]!.before(top);
	main.append(foot);

	// The questions, set up once: picks made on a step survive going back and forth.
	const questions = new Map<HTMLElement, Question>();
	for (const s of steps) {
		const q = s.querySelector<HTMLElement>('.quiz');
		if (!q) continue;
		const picked = () => {
			sound.pick();
			sync(true);
		};
		const made = q.classList.contains('tap') ? tapQuestion(q, picked) : choiceQuestion(q, picked);
		if (!made) continue;
		questions.set(s, made);
		if (me.pass[made.id]?.right) made.settle();
	}
	// Each step's own figures (not the questions'): explorable diagrams.
	const figures: Diagram[] = [];
	main.querySelectorAll<HTMLElement>('figure.diagram').forEach((f) => {
		const d = mount(f, false);
		if (d) figures.push(d);
	});

	let at = -1;
	let misses = 0;

	const answered = (s: HTMLElement | undefined) => {
		const q = s && questions.get(s);
		return !q || me.pass[q.id]?.right === true;
	};

	function feedback(tone: '' | 'ok' | 'no', t = '', w = '') {
		fb.dataset.tone = tone;
		fbT.textContent = t;
		fbW.innerHTML = w;
		fb.hidden = !tone;
	}

	/** The buttons, the segments and the count for where the reader is. `picking`: a pick just changed. */
	function sync(picking = false) {
		const s = screens[at];
		const q = s ? questions.get(s) : undefined;
		if (picking && q && !answered(s)) {
			feedback('');
			for (const b of s!.querySelectorAll<HTMLElement>('[data-mark="wrong"]:not([disabled])')) b.dataset.mark = '';
		}
		backBtn.disabled = at === 0;
		goBtn.disabled = false;
		goBtn.dataset.kind = '';
		if (at === n) goBtn.textContent = main.dataset.next ? 'Next lesson' : 'Back to the course';
		else if (q && !answered(s)) {
			goBtn.textContent = 'Check';
			goBtn.dataset.kind = 'check';
			goBtn.disabled = !q.ready();
		} else goBtn.textContent = at === n - 1 ? 'Finish' : 'Continue';
		count.textContent = at === n ? 'Done' : `${at + 1} / ${n}`;
		segBtns.forEach((b, i) => {
			b.dataset.state = i === at ? 'cur' : me.done || i < me.seen ? 'seen' : '';
			b.disabled = !(me.done || i < me.seen);
		});
	}

	function show(i: number, dir: 1 | -1 | 0) {
		if (i === at || i < 0 || i > n) return;
		const was = screens[at];
		at = i;
		misses = 0;
		if (was) {
			was.removeAttribute('data-cur');
			was.classList.remove('in-fwd', 'in-back');
		}
		const s = screens[i]!;
		s.dataset.cur = '';
		if (dir && !still.matches) {
			s.classList.add(dir > 0 ? 'in-fwd' : 'in-back');
			s.addEventListener('animationend', () => s.classList.remove('in-fwd', 'in-back'), { once: true });
		}
		if (i < n) {
			me.step = i + 1;
			me.seen = Math.max(me.seen, i + 1);
		}
		// The end: a rising phrase, or the full chord when every question was right the first time.
		if (dir > 0) (i === n ? (quizIds.length && firstTime(me, quizIds) === quizIds.length ? sound.milestone : sound.done) : sound.tick)();
		if (i === n) {
			me.done = true;
			me.step = 0;
			me.seen = n;
			finish();
		}
		store(saved);
		// The URL names the screen in view (`#<step>`, `#done` at the end), so a reload or a copied link opens it.
		const anchor = i < n ? steps[i]!.id : 'done';
		if (decodeURIComponent(location.hash.slice(1)) !== anchor)
			history.replaceState(history.state, '', `${location.pathname}${location.search}#${encodeURIComponent(anchor)}`);
		const q = questions.get(s);
		if (q && answered(s)) feedback('ok', 'Answered', s.querySelector('details.answer p')?.innerHTML ?? '');
		else feedback('');
		sync();
		if (dir) {
			const y = main.getBoundingClientRect().top + scrollY - 8;
			if (scrollY > y) scrollTo({ top: Math.max(0, y), behavior: 'instant' });
			const h = s.querySelector<HTMLElement>('h2');
			if (h) {
				h.tabIndex = -1;
				h.focus({ preventScroll: true });
			}
		}
	}

	function finish() {
		const first = quizIds.filter((id) => me.pass[id]?.right && me.pass[id]!.tries === 1).length;
		end.querySelector('.lp-score')!.textContent = quizIds.length
			? first === quizIds.length
				? `All ${quizIds.length} questions right the first time.`
				: `${first} of ${quizIds.length} questions right the first time. The others count once you got them, so nothing is lost.`
			: 'You went through every step.';
		end.querySelector('.lp-seal')!.classList.toggle('all', quizIds.length > 0 && first === quizIds.length);
		const next = end.querySelector('.lp-next')!;
		next.replaceChildren();
		if (main.dataset.next) {
			next.append('Up next: ');
			const a = el('a', '', main.dataset.nextTitle ?? 'the next lesson');
			a.href = main.dataset.next;
			next.append(a, '. Or see the whole ');
		} else if (log) next.append('Back to the ');
		else next.append('That was the last lesson. Back to the ');
		const c = el('a', '', 'course map');
		c.href = course;
		next.append(c);
		if (log) {
			const w = el('a', '', 'What’s new');
			w.href = log;
			next.append(', or to ', w);
		}
		next.append('.');
	}

	function primary() {
		if (goBtn.disabled) return;
		const s = screens[at];
		if (at === n) return go(main.dataset.next ?? course);
		const q = s ? questions.get(s) : undefined;
		if (q && !answered(s)) {
			const p = (me.pass[q.id] ??= { tries: 0, right: false });
			p.tries++;
			const r = q.check();
			if (r.right) {
				p.right = true;
				q.settle();
				sound.right();
				feedback('ok', p.tries === 1 ? 'Right.' : 'That’s it.', r.say);
			} else {
				misses++;
				sound.wrong();
				feedback('no', 'Not quite.', r.say);
				if (q.reveal && misses >= 2) {
					const b = el('button', 'lp-reveal', 'Show the answer');
					b.type = 'button';
					b.addEventListener('click', () => {
						const w = q.reveal!();
						p.right = true;
						store(saved);
						feedback('ok', 'Here it is.', w);
						sync();
						goBtn.focus();
					});
					fbW.append(' ', b);
				}
			}
			store(saved);
			sync();
			if (r.right) goBtn.focus();
			return;
		}
		show(at + 1, 1);
	}

	goBtn.addEventListener('click', primary);
	backBtn.addEventListener('click', () => show(at - 1, -1));

	document.addEventListener(
		'keydown',
		(e) => {
			if (e.defaultPrevented || e.metaKey || e.ctrlKey || e.altKey) return;
			const t = e.target instanceof HTMLElement ? e.target : null;
			if (t && (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName))) return;
			if (document.querySelector('dialog[open]')) return;
			const onControl = t && t !== goBtn && t.closest('a, button, summary');
			if (e.key === 'Enter') {
				// On another control Enter is that control's (a choice or a part only picks; then Enter checks).
				if (onControl && !(t.closest('.pick, .dg-part') && t.getAttribute('aria-pressed') === 'true')) return;
				e.preventDefault();
				primary();
			} else if (e.key === 'ArrowRight') {
				if (at < n && answered(screens[at])) {
					e.preventDefault();
					show(at + 1, 1);
				}
			} else if (e.key === 'ArrowLeft') {
				if (at > 0) {
					e.preventDefault();
					show(at - 1, -1);
				}
			} else if (/^[1-9]$/.test(e.key)) {
				const q = questions.get(screens[at]!);
				if (q?.key && !answered(screens[at])) {
					e.preventDefault();
					q.key(Number(e.key) - 1);
				}
			}
		},
		{ signal },
	);
	// Another tab finished or reset this lesson.
	addEventListener(
		'storage',
		(e) => {
			if (e.key !== KEY) return;
			saved = load();
			me = progress(saved, lesson);
			sync();
		},
		{ signal },
	);

	// Where to open: a link to one step (search results link to `#<step>`), else where the reader left off, else the
	// first step.
	const hash = decodeURIComponent(location.hash.slice(1));
	const linked = hash === 'done' && me.done ? n : steps.findIndex((s) => s.id === hash);
	main.dataset.on = '';
	show(linked >= 0 ? linked : !me.done && me.step >= 1 && me.step <= n ? me.step - 1 : 0, 0);
	// A link to another step of this page (a search result, a pasted URL) moves the player there.
	addEventListener(
		'hashchange',
		() => {
			const h = decodeURIComponent(location.hash.slice(1));
			const i = h === 'done' && me.done ? n : steps.findIndex((s) => s.id === h);
			if (i >= 0 && i !== at) show(i, i > at ? 1 : -1);
		},
		{ signal },
	);

	return () => {
		for (const q of questions.values()) q.stop();
		for (const f of figures) f.stop();
	};
}

const SVG = 'http://www.w3.org/2000/svg';

function svgEl<K extends keyof SVGElementTagNameMap>(tag: K, attrs: Record<string, string> = {}): SVGElementTagNameMap[K] {
	const e = document.createElementNS(SVG, tag);
	for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, v);
	return e;
}

/** A node on the course path or What's new: its key, questions and state. */
function nodeState(li: HTMLElement, saved: Saved) {
	const p = saved.lessons[li.dataset.lesson!];
	const qs = (li.dataset.quizzes ?? '').split(' ').filter(Boolean);
	return { p, qs, state: stateOf(p, qs), right: firstTime(p, qs) };
}

/** The "New" mark on an update's node, shown while it is new to the reader. */
function markNew(li: HTMLElement, on: boolean) {
	li.toggleAttribute('data-new', on);
	const b = li.querySelector<HTMLElement>('.new');
	if (b) b.hidden = !on;
}

/**
 * The course path (on the course map; render.rs `course`, laid out by course.rs): each node lit with the reader's
 * progress (its state, the ring for how far in, its mastery cells), the updates new since their last visit marked
 * New, the next node marked with its Start or Continue, the edges stroked between the pins (render.rs gives them in
 * `data-edges`; this only measures where the pins landed), the counts and Reset progress under the big button, which
 * points at the next node: a new update whose lessons are done first, then the next lesson, then the next update.
 */
function coursePath(course: HTMLElement, signal: AbortSignal): () => void {
	const nodes = [...course.querySelectorAll<HTMLLIElement>('ol.path > li.node')];
	const end = course.querySelector<HTMLElement>('.path-end');
	const go = course.querySelector<HTMLAnchorElement>('.course-go');
	const updates = nodes.filter((li) => li.classList.contains('update')).map((li) => li.dataset.lesson!);
	const lessons = nodes.filter((li) => !li.classList.contains('update'));
	const edges = (course.dataset.edges ?? '')
		.split(' ')
		.filter(Boolean)
		.map((e) => {
			const [ft, t] = e.split(':');
			const [f, to] = ft!.split('-').map(Number);
			return { from: f!, to: to!, track: t === undefined ? -1 : Number(t) };
		})
		.filter((e) => nodes[e.from] && nodes[e.to]);

	const stats = el('p', 'course-stats');
	stats.innerHTML = '<span class="c-done"></span><span class="c-upd"></span><span class="c-first"></span>';
	const reset = el('button', 'c-reset', 'Reset progress');
	reset.type = 'button';
	stats.append(reset);
	(go?.parentElement ?? course).append(stats);
	let armed: ReturnType<typeof setTimeout> | null = null;
	let cleared: ReturnType<typeof setTimeout> | null = null;
	reset.addEventListener('click', () => {
		if (!armed) {
			reset.textContent = 'Tap again to clear all progress';
			reset.dataset.armed = '';
			armed = setTimeout(disarm, 4000);
			return;
		}
		disarm();
		// What the reader has seen stays seen: clearing progress is not a first visit.
		store({ v: 1, lessons: {}, seen: load().seen, fresh: [] });
		reset.textContent = 'Progress cleared';
		reset.hidden = false;
		cleared = setTimeout(() => {
			reset.textContent = 'Reset progress';
			paint();
		}, 2000);
	});
	function disarm() {
		if (armed) clearTimeout(armed);
		armed = null;
		reset.textContent = 'Reset progress';
		delete reset.dataset.armed;
	}

	// Each node's mastery cells, under its meta line; the way on, for the next one.
	const cells = nodes.map((li) => {
		const qs = el('span', 'qs');
		qs.setAttribute('role', 'img');
		li.querySelector('.body')!.append(qs);
		return qs;
	});
	const goPill = el('span', 'go');

	// The edges: a halo in the page's background under each line, so where two cross the one on top reads as passing
	// over. Those that cross a divider first, under the ones within a band.
	const trail = svgEl('svg', { class: 'trail', 'aria-hidden': 'true' });
	course.prepend(trail);
	course.dataset.drawn = '';
	const pins = nodes.map((li) => li.querySelector<HTMLElement>('.pin')!);
	const order = [...edges.filter((e) => e.track >= 0), ...edges.filter((e) => e.track < 0)];
	const strokes = order.map((e) => {
		const dashed = nodes[e.to]!.closest('.theme')?.classList.contains('dashed') ?? false;
		const g = svgEl('g', { class: `edge${e.track >= 0 ? ' cross' : ''}${dashed ? ' dash' : ''}` });
		const halo = svgEl('path', { class: 'halo' });
		const line = svgEl('path', { class: 'line' });
		g.append(halo, line);
		trail.append(g);
		return { e, g, halo, line };
	});

	function draw() {
		const box = course.getBoundingClientRect();
		trail.setAttribute('width', String(box.width));
		trail.setAttribute('height', String(box.height));
		const at = pins.map((p) => {
			const r = p.getBoundingClientRect();
			return [r.left - box.left + r.width / 2, r.top - box.top + r.height / 2] as const;
		});
		// The gutter's tracks, from the inside out: the first list's left edge plus its padding is where column 0
		// starts (styles/learn.css gives the padding as --gut,
		// and the spacing of the tracks as --trk).
		const path = course.querySelector<HTMLElement>('ol.path');
		const inner = path ? path.getBoundingClientRect().left - box.left + parseFloat(getComputedStyle(path).paddingLeft) : 0;
		const trk = parseFloat(getComputedStyle(course).getPropertyValue('--trk'));
		const trackX = (t: number) => inner - 6 - t * trk;
		for (const { e, halo, line } of strokes) {
			const [x1, y1] = at[e.from]!;
			const [x2, y2] = at[e.to]!;
			let d: string;
			if (e.track >= 0) {
				const tx = trackX(e.track);
				const r = Math.min(8, (y2 - y1) / 2);
				d = `M${x1} ${y1}H${tx + r}Q${tx} ${y1} ${tx} ${y1 + r}V${y2 - r}Q${tx} ${y2} ${tx + r} ${y2}H${x2}`;
			} else if (Math.abs(x1 - x2) < 1) {
				d = `M${x1} ${y1}V${y2}`;
			} else {
				// Bend into the target's column halfway to the next row, then straight down it.
				const yn = at[e.from + 1]?.[1] ?? y2;
				const bend = (y1 + yn) / 2;
				const k = Math.min(16, (bend - y1) / 2);
				d = `M${x1} ${y1}V${bend - k}C${x1} ${bend} ${x2} ${bend} ${x2} ${bend + k}V${y2}`;
			}
			halo.setAttribute('d', d);
			line.setAttribute('d', d);
		}
	}

	function paint() {
		const saved = roll(load(), updates);
		const fresh = new Set(freshOf(saved));
		const done = (key: string) => saved.lessons[key]?.done === true;
		let first = 0;
		let total = 0;
		let lessonsDone = 0;
		let updatesDone = 0;
		for (const [i, li] of nodes.entries()) {
			const { p, qs, state, right } = nodeState(li, saved);
			const steps = Number(li.dataset.steps ?? 1);
			first += right;
			total += qs.length;
			if (p?.done) li.classList.contains('update') ? updatesDone++ : lessonsDone++;
			li.dataset.state = state;
			li.style.setProperty('--p', String(p?.done ? 1 : Math.min(1, (p?.seen ?? 0) / steps)));
			const isNew = fresh.has(li.dataset.lesson!);
			markNew(li, isNew);
			cells[i]!.replaceChildren(
				...qs.map((id) => {
					const pass = p?.pass?.[id];
					const c = el('i');
					c.dataset.state = pass?.right ? (pass.tries === 1 ? 'first' : 'later') : '';
					return c;
				}),
			);
			cells[i]!.hidden = qs.length === 0;
			cells[i]!.setAttribute('aria-label', `${right} of ${qs.length} questions right the first time`);
			const n = li.querySelector('.body .n')?.firstChild?.textContent ?? '';
			const title = li.querySelector('.body .t')?.textContent ?? '';
			const doing = state === 'doing' ? `, step ${Math.min(steps, p?.seen ?? 0)} of ${steps}` : '';
			li.querySelector('a')?.setAttribute('aria-label', `${n}: ${title}, ${isNew ? 'new, ' : ''}${stateWords[state]}${doing}`);
		}
		for (const s of strokes) s.g.classList.toggle('lit', done(nodes[s.e.from]!.dataset.lesson!));

		// What comes next: a new update the reader can follow, the next lesson, the next update.
		const ready = (li: HTMLElement) => (li.dataset.requires ?? '').split(' ').filter(Boolean).every(done);
		const open = (li: HTMLElement) => !done(li.dataset.lesson!) && ready(li);
		const up =
			nodes.find((li) => li.classList.contains('update') && fresh.has(li.dataset.lesson!) && open(li)) ??
			lessons.find(open) ??
			nodes.find(open) ??
			null;
		for (const li of nodes) li.toggleAttribute('data-next', li === up);
		goPill.textContent = up?.dataset.state === 'doing' ? 'Continue' : 'Start';
		if (up) up.querySelector('.body')!.append(goPill);
		else goPill.remove();

		const all = nodes.every((li) => done(li.dataset.lesson!));
		if (all) course.dataset.all = first === total ? 'mastered' : 'done';
		else delete course.dataset.all;
		if (end) {
			end.querySelector('.t')!.textContent = all ? 'You’re caught up' : 'The end of the course';
			end.querySelector('.l')!.textContent = all
				? first === total
					? 'Every lesson and update done, every question right the first time. New updates show up here as blaze changes.'
					: `Every lesson and update done; ${first} of ${total} questions right the first time. Review one to fill in the rest.`
				: `After lesson ${lessons.length}, you know how blaze builds. New updates show up in their section as blaze changes.`;
		}

		// The big button: what comes next, or a way back in once all is done.
		const target = up ?? nodes[0];
		const a = target?.querySelector('a');
		if (go && a && target) {
			go.href = a.href;
			const eyebrow = target.querySelector('.body .n')?.firstChild?.textContent ?? '';
			const title = target.querySelector('.body .t')?.textContent ?? '';
			const update = target.classList.contains('update');
			const doing = up?.dataset.state === 'doing';
			go.querySelector('.k')!.textContent = !up
				? 'Review the course'
				: update
					? fresh.has(target.dataset.lesson!)
						? `${doing ? 'Continue' : 'New'}: ${title}`
						: `${doing ? 'Continue' : 'Study'} the update: ${title}`
					: `${doing ? 'Continue' : 'Start'} ${eyebrow.toLowerCase()}`;
			go.querySelector('.t')!.textContent = !up ? `From lesson 1: ${title}` : update ? eyebrow : title;
		}
		stats.querySelector('.c-done')!.innerHTML = `<b>${lessonsDone}/${lessons.length}</b> lessons done`;
		stats.querySelector('.c-upd')!.innerHTML = updates.length ? `<b>${updatesDone}/${updates.length}</b> updates${fresh.size ? `, <b>${fresh.size}</b> new` : ''}` : '';
		stats.querySelector('.c-first')!.innerHTML = `<b>${first}/${total}</b> questions right the first time`;
		reset.hidden = Object.keys(saved.lessons).length === 0 && reset.textContent === 'Reset progress';
	}

	paint();
	draw();
	const ro = new ResizeObserver(draw);
	ro.observe(course);
	addEventListener('storage', (e) => e.key === KEY && paint(), { signal });
	addEventListener(CHANGED, paint, { signal });
	return () => {
		ro.disconnect();
		if (armed) clearTimeout(armed);
		if (cleared) clearTimeout(cleared);
		stats.remove();
		trail.remove();
		goPill.remove();
		for (const c of cells) c.remove();
		delete course.dataset.drawn;
		delete course.dataset.all;
	};
}

/**
 * What's new (render.rs `update_log`): each update's state, as on the path, and the ones new since the reader's last
 * visit marked New, with a way to mark them all as seen.
 */
function updateLog(log: HTMLElement, signal: AbortSignal): () => void {
	const items = [...log.querySelectorAll<HTMLLIElement>('li.node')];
	const keys = (log.dataset.updates ?? '').split(' ').filter(Boolean);
	const bar = el('p', 'ulog-bar');
	bar.innerHTML = '<span class="ulog-count"></span>';
	const clear = el('button', 'c-reset', 'Mark all as seen');
	clear.type = 'button';
	clear.addEventListener('click', () => clearFresh(load()));
	bar.append(clear);
	log.before(bar);
	function paint() {
		const saved = roll(load(), keys);
		const fresh = new Set(freshOf(saved));
		for (const li of items) {
			const { state } = nodeState(li, saved);
			li.dataset.state = state;
			markNew(li, fresh.has(li.dataset.lesson!));
			const title = li.querySelector('.body .t')?.textContent ?? '';
			li.querySelector('a')?.setAttribute('aria-label', `${title}, ${fresh.has(li.dataset.lesson!) ? 'new, ' : ''}${stateWords[state]}`);
		}
		const done = items.filter((li) => saved.lessons[li.dataset.lesson!]?.done).length;
		bar.querySelector('.ulog-count')!.textContent = fresh.size
			? `${fresh.size} new since your last visit · ${done} of ${items.length} studied`
			: `Nothing new since your last visit · ${done} of ${items.length} studied`;
		clear.hidden = fresh.size === 0;
	}
	paint();
	addEventListener('storage', (e) => e.key === KEY && paint(), { signal });
	addEventListener(CHANGED, paint, { signal });
	return () => bar.remove();
}

onPage((signal) => {
	const main = document.querySelector<HTMLElement>('main.lesson[data-lesson]');
	if (main && !main.hasAttribute('data-on')) return player(main, signal);
	const course = document.querySelector<HTMLElement>('.course[data-edges]');
	if (course) return coursePath(course, signal);
	const log = document.querySelector<HTMLElement>('.ulog[data-updates]');
	if (log) return updateLog(log, signal);
	// A figure outside a lesson (none yet), drawn all the same.
	const figs = [...document.querySelectorAll<HTMLElement>('figure.diagram:not([data-drawn])')].map((f) => mount(f, false));
	return () => figs.forEach((d) => d?.stop());
});
