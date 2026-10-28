/**
 * The course (the lessons at /learn/<slug> and the path on the home page; Lesson, Step, Quiz, Tap and Figure in content/schema.flecs, rendered by the
 * renderer's render.rs). Without JS a lesson is one page of steps under each other, each question with its answer in
 * a fold. This script turns it into the lesson player, one step at a time:
 *
 * - a bar at the top (back to the course, how far along, step k of n) and one at the bottom, within thumb reach (Back,
 *   and the one big button: Start, Check, Continue, Finish), where a question's feedback appears too;
 * - a cover before the first step, and a short "lesson complete" after the last;
 * - a question has to be answered before Continue: a wrong pick says why in one line and is crossed off, and the
 *   reader picks again. A Tap question (tap the parts of a diagram) gives its hint on a miss and, after two, offers
 *   the answer;
 * - keys: Enter is the big button, the arrows step back and forward, 1 to 9 pick a choice, Escape is the course.
 *
 * On the home page it lights the course path (`.course`, render.rs `course`) with the reader's progress: each lesson's
 * node and its mastery cells (one per question, solid when it was right the first time), the trail between the nodes
 * filled as far as the reader has come, Start or Continue on the next lesson, and the big button pointed at it.
 *
 * Progress stays in this browser (client/lib/progress.ts, localStorage `blaze.learn`): per lesson the step to come back to, the furthest
 * step seen, whether it is finished, and each question's tries. Storage can be missing or refuse writes (private
 * windows, blocked site data), so every access is wrapped and the course works for the visit from memory instead.
 * Diagrams are client/lib/diagrams.ts, and the sounds (a chime when right, a soft low tone when not, a tick to move on;
 * the speaker in the top bar mutes them) client/lib/sound.ts.
 */

import { mount, type Diagram } from './lib/diagrams';
import { onPage } from './lib/page';
import { KEY, firstTime, load, progress, stateOf, stateWords, store } from './lib/progress';
import { enabled as soundOn, setEnabled as setSound, sound } from './lib/sound';

const still = matchMedia('(prefers-reduced-motion: reduce)');

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls?: string, text?: string): HTMLElementTagNameMap[K] {
	const e = document.createElement(tag);
	if (cls) e.className = cls;
	if (text !== undefined) e.textContent = text;
	return e;
}

const ICON = {
	close: '<path d="M6 6l12 12M18 6L6 18"/>',
	back: '<path d="M15 5l-7 7 7 7"/>',
	sound: '<path d="M4 9.5h3.5L12 5.5v13l-4.5-4H4z"/><path d="M15.5 9a4 4 0 0 1 0 6M18 6.5a7.5 7.5 0 0 1 0 11"/>',
	muted: '<path d="M4 9.5h3.5L12 5.5v13l-4.5-4H4z"/><path d="M16 9.5l5 5M21 9.5l-5 5"/>',
};

function icon(name: keyof typeof ICON): string {
	return `<svg class="icon" viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${ICON[name]}</svg>`;
}

/** A check mark in pixel cells on a faint 9 by 7 grid, as the home page's drawings are made. */
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

function player(main: HTMLElement, signal: AbortSignal): () => void {
	const lesson = main.dataset.lesson!;
	const course = main.dataset.course!;
	const steps = [...main.querySelectorAll<HTMLElement>(':scope > section.step')];
	if (!steps.length) return () => {};
	const quizIds = (main.dataset.quizzes ?? '').split(' ').filter(Boolean);
	const quizCount = quizIds.length;
	const title = main.querySelector('.head h1')?.textContent ?? document.title;
	let saved = load();
	let me = progress(saved, lesson);
	const n = steps.length;

	// Cover and end, as screens like the steps: 0 is the cover, 1..n the steps, n + 1 the end.
	const cover = el('section', 'step lp-cover');
	cover.innerHTML = `<p class="lp-eyebrow">Lesson ${main.dataset.n ?? ''}</p><h2></h2><p class="lp-line"></p><p class="lp-meta"></p><ol class="lp-map" aria-label="Steps"></ol>`;
	cover.querySelector('h2')!.textContent = title;
	// The lesson's glyph (render.rs puts it above the page's title), as on its node on the course path.
	const glyph = main.querySelector('.head .lesson-glyph');
	if (glyph) cover.prepend(glyph.cloneNode(true));
	cover.querySelector('.lp-line')!.textContent = main.dataset.line ?? '';
	const minutes = Math.max(1, Math.ceil((n * 20) / 60));
	cover.querySelector('.lp-meta')!.textContent = `${n} steps · ${minutes} min${quizCount ? ` · ${quizCount} questions` : ''}`;
	const map = cover.querySelector('.lp-map')!;
	steps.forEach((s, i) => {
		const li = el('li');
		const b = el('button');
		b.type = 'button';
		b.setAttribute('aria-label', `Step ${i + 1}: ${s.querySelector('h2')?.textContent ?? ''}`);
		b.title = s.querySelector('h2')?.textContent ?? '';
		if (s.querySelector('.quiz')) b.dataset.q = '';
		b.addEventListener('click', () => {
			if (i + 1 <= me.seen || me.done) show(i + 1, 1);
		});
		li.append(b);
		map.append(li);
	});
	const end = el('section', 'step lp-end');
	end.innerHTML = `<div class="lp-seal" aria-hidden="true">${seal()}</div><h2>Lesson complete</h2><p class="lp-score"></p><p class="lp-next"></p>`;
	const screens = [cover, ...steps, end];
	steps[0]!.before(cover);
	steps[n - 1]!.after(end);

	// The bars.
	const top = el('div', 'lp-top');
	top.innerHTML = `<a class="lp-x" aria-label="Back to the course">${icon('close')}</a><div class="lp-bar" role="progressbar" aria-label="Lesson progress" aria-valuemin="0" aria-valuemax="${n}"><i></i></div><span class="lp-count"></span><button type="button" class="lp-snd"></button>`;
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
	const close = top.querySelector<HTMLAnchorElement>('.lp-x')!;
	close.href = course;
	const bar = top.querySelector<HTMLElement>('.lp-bar')!;
	const count = top.querySelector<HTMLElement>('.lp-count')!;
	const foot = el('div', 'lp-foot');
	foot.innerHTML = `<div class="lp-fb" aria-live="polite"><p class="lp-fb-t"></p><p class="lp-fb-w"></p></div><div class="lp-row"><button type="button" class="lp-back" aria-label="Back">${icon('back')}</button><button type="button" class="lp-go"></button></div>`;
	const fb = foot.querySelector<HTMLElement>('.lp-fb')!;
	const fbT = foot.querySelector<HTMLElement>('.lp-fb-t')!;
	const fbW = foot.querySelector<HTMLElement>('.lp-fb-w')!;
	const backBtn = foot.querySelector<HTMLButtonElement>('.lp-back')!;
	const goBtn = foot.querySelector<HTMLButtonElement>('.lp-go')!;
	cover.before(top);
	main.append(foot);
	// The step's last line stays clear of the bottom bar, however tall its feedback grows.
	const ro = new ResizeObserver(() => main.style.setProperty('--foot-h', `${foot.offsetHeight}px`));
	ro.observe(foot);

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
		const p = me.pass[made.id];
		if (p?.right) made.settle();
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

	/** The buttons, the bar and the count for where the reader is. `picking`: a pick just changed. */
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
		if (at === 0) goBtn.textContent = me.done ? 'Review' : me.seen > 0 ? 'Continue' : 'Start';
		else if (at === n + 1) goBtn.textContent = main.dataset.next ? 'Next lesson' : 'Back to the course';
		else if (q && !answered(s)) {
			goBtn.textContent = 'Check';
			goBtn.dataset.kind = 'check';
			goBtn.disabled = !q.ready();
		} else goBtn.textContent = at === n ? 'Finish' : 'Continue';
		const k = Math.min(at, n);
		bar.style.setProperty('--p', String(at === n + 1 ? 1 : Math.max(0, k - 1) / n));
		bar.setAttribute('aria-valuenow', String(at === n + 1 ? n : Math.max(0, k - 1)));
		count.textContent = at === 0 ? '' : at === n + 1 ? 'Done' : `${k} / ${n}`;
		top.dataset.cover = String(at === 0);
		cover.querySelectorAll<HTMLButtonElement>('.lp-map button').forEach((b, i) => {
			const step = i + 1;
			b.dataset.state = me.done || step <= me.seen ? 'seen' : '';
			b.disabled = !(me.done || step <= me.seen);
		});
	}

	function show(i: number, dir: 1 | -1 | 0) {
		if (i === at || i < 0 || i > n + 1) return;
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
		if (i >= 1 && i <= n) {
			me.step = i;
			me.seen = Math.max(me.seen, i);
		}
		// The end: a rising phrase, or the full chord when every question was right the first time.
		if (dir > 0) (i === n + 1 ? (quizIds.length && firstTime(me, quizIds) === quizIds.length ? sound.milestone : sound.done) : sound.tick)();
		if (i === n + 1) {
			me.done = true;
			me.step = 0;
			me.seen = n;
			finish();
		}
		store(saved);
		const q = questions.get(s);
		if (q && answered(s)) {
			const w = s.querySelector('details.answer p')?.innerHTML ?? '';
			feedback('ok', 'Answered', w);
		} else feedback('');
		sync();
		scrollTo({ top: 0, behavior: 'instant' });
		const h = s.querySelector<HTMLElement>('h2');
		if (h && dir) {
			h.tabIndex = -1;
			h.focus({ preventScroll: true });
		}
	}

	function finish() {
		const ids = (main.dataset.quizzes ?? '').split(' ').filter(Boolean);
		const first = ids.filter((id) => me.pass[id]?.right && me.pass[id]!.tries === 1).length;
		end.querySelector('.lp-score')!.textContent = ids.length
			? first === ids.length
				? `All ${ids.length} questions right the first time.`
				: `${first} of ${ids.length} questions right the first time. The others count once you got them, so nothing is lost.`
			: 'You went through every step.';
		end.querySelector('.lp-seal')!.classList.toggle('all', ids.length > 0 && first === ids.length);
		const next = end.querySelector('.lp-next')!;
		next.replaceChildren();
		if (main.dataset.next) {
			next.append('Up next: ');
			const a = el('a', '', main.dataset.nextTitle ?? 'the next lesson');
			a.href = main.dataset.next;
			next.append(a, '. Or go back to the ');
		} else next.append('That was the last lesson. Back to the ');
		const c = el('a', '', 'course');
		c.href = course;
		next.append(c, '.');
	}

	function primary() {
		if (goBtn.disabled) return;
		const s = screens[at];
		if (at === 0) return show(me.done || me.step < 1 ? 1 : me.step, 1);
		if (at === n + 1) return go(main.dataset.next ?? course);
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
				const s = screens[at];
				if (at <= n && answered(s)) {
					e.preventDefault();
					if (at === 0) primary();
					else show(at + 1, 1);
				}
			} else if (e.key === 'ArrowLeft') {
				if (at > 0) {
					e.preventDefault();
					show(at - 1, -1);
				}
			} else if (e.key === 'Escape') {
				close.click();
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
	// cover. A finished lesson opens on its cover, to review.
	const hash = decodeURIComponent(location.hash.slice(1));
	const linked = steps.findIndex((s) => s.id === hash);
	main.dataset.on = '';
	show(linked >= 0 ? linked + 1 : !me.done && me.step >= 1 && me.step <= n ? me.step : 0, 0);

	return () => {
		ro.disconnect();
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

/**
 * The course path (on the home page): each lesson's node lit with the reader's progress (its state, the ring for how
 * far in, its mastery cells), the next one marked with its Start or Continue, the trail drawn between the nodes and
 * filled as far as the reader has come, the counts and Reset progress under the big button, which points at the next
 * lesson.
 */
function coursePath(course: HTMLElement, signal: AbortSignal): () => void {
	const list = course.querySelector<HTMLOListElement>('ol.path')!;
	const nodes = [...list.querySelectorAll<HTMLLIElement>(':scope > li.node')];
	const end = course.querySelector<HTMLElement>('.path-end');
	const go = course.querySelector<HTMLAnchorElement>('.course-go');

	const stats = el('p', 'course-stats');
	stats.innerHTML = '<span class="c-done"></span><span class="c-first"></span>';
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
		store({ v: 1, lessons: {} });
		paint();
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

	// Each lesson's mastery cells, under its meta line; the way on, for the next one.
	const cells = nodes.map((li) => {
		const qs = el('span', 'qs');
		qs.setAttribute('role', 'img');
		li.querySelector('.body')!.append(qs);
		return qs;
	});
	const goPill = el('span', 'go');

	// The trail: one curve from each disc to the next (the last to the end), a dotted track and its fill, the fill
	// revealed by a mask as far as the lesson it leaves from has come.
	const trail = svgEl('svg', { class: 'trail', 'aria-hidden': 'true' });
	course.prepend(trail);
	course.dataset.drawn = '';
	const pins = [...nodes.map((li) => li.querySelector<HTMLElement>('.pin')!), ...(end ? [end.querySelector<HTMLElement>('.pin')!] : [])];
	const segments = pins.slice(1).map((_, i) => {
		const reveal = svgEl('path', { pathLength: '1' });
		const mask = svgEl('mask', { id: `trail-${i}`, maskUnits: 'userSpaceOnUse' });
		mask.append(reveal);
		const track = svgEl('path', { class: 'track' });
		const fill = svgEl('path', { class: 'fill', mask: `url(#trail-${i})` });
		trail.append(mask, track, fill);
		return { reveal, track, fill };
	});
	let fills = segments.map(() => 0);

	function draw() {
		const box = course.getBoundingClientRect();
		trail.setAttribute('width', String(box.width));
		trail.setAttribute('height', String(box.height));
		const at = pins.map((p) => {
			const r = p.getBoundingClientRect();
			return [r.left - box.left + r.width / 2, r.top - box.top + r.height / 2] as const;
		});
		segments.forEach((s, i) => {
			const [x0, y0] = at[i]!;
			const [x1, y1] = at[i + 1]!;
			const k = (y1 - y0) / 2;
			const d = `M${x0} ${y0}C${x0} ${y0 + k} ${x1} ${y1 - k} ${x1} ${y1}`;
			for (const p of [s.reveal, s.track, s.fill]) p.setAttribute('d', d);
		});
	}

	function fillTrail() {
		segments.forEach((s, i) => s.reveal.style.setProperty('stroke-dashoffset', String(1 - fills[i]!)));
	}

	function paint() {
		const saved = load();
		let first = 0;
		let total = 0;
		let done = 0;
		let next: HTMLLIElement | null = null;
		const at: number[] = [];
		for (const [i, li] of nodes.entries()) {
			const p = saved.lessons[li.dataset.lesson!];
			const steps = Number(li.dataset.steps ?? 1);
			const qs = (li.dataset.quizzes ?? '').split(' ').filter(Boolean);
			const state = stateOf(p, qs);
			const right = firstTime(p, qs);
			first += right;
			total += qs.length;
			if (p?.done) done++;
			const far = p?.done ? 1 : Math.min(1, (p?.seen ?? 0) / steps);
			at.push(far);
			li.dataset.state = state;
			li.style.setProperty('--p', String(far));
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
			if (!next && state !== 'done' && state !== 'mastered') next = li;
			const title = li.querySelector('.body .t')?.textContent ?? '';
			const doing = state === 'doing' ? `, step ${Math.min(steps, p?.seen ?? 0)} of ${steps}` : '';
			li.querySelector('a')?.setAttribute('aria-label', `Lesson ${i + 1}: ${title}, ${stateWords[state]}${doing}`);
		}
		for (const li of nodes) li.toggleAttribute('data-next', li === next);
		const up: HTMLLIElement | null = next;
		goPill.textContent = up?.dataset.state === 'doing' ? 'Continue' : 'Start';
		if (up) up.querySelector('.body')!.append(goPill);
		else goPill.remove();
		// The trail from a node fills as far as its lesson has come; from the last, to the end, once it is done.
		fills = segments.map((_, i) => at[i] ?? 0);
		fillTrail();

		const all = done === nodes.length;
		if (all) course.dataset.all = first === total ? 'mastered' : 'done';
		else delete course.dataset.all;
		if (end) {
			end.querySelector('.t')!.textContent = all ? 'Course complete' : 'The end of the course';
			end.querySelector('.l')!.textContent = all
				? first === total
					? `Every question right the first time. You know how blaze builds.`
					: `${first} of ${total} questions right the first time. Review a lesson to fill in the rest.`
				: `After lesson ${nodes.length}, you know how blaze builds.`;
		}

		// The big button: the next lesson to start or finish, or a way back in once all are done.
		const target = up ?? nodes[0];
		const a = target?.querySelector('a');
		if (go && a && target) {
			const n = nodes.indexOf(target) + 1;
			go.href = a.href;
			go.querySelector('.k')!.textContent = !up ? 'Review the course' : up.dataset.state === 'doing' ? `Continue lesson ${n}` : `Start lesson ${n}`;
			const title = target.querySelector('.body .t')?.textContent ?? '';
			go.querySelector('.t')!.textContent = up ? title : `From lesson 1: ${title}`;
		}
		stats.querySelector('.c-done')!.innerHTML = `<b>${done}/${nodes.length}</b> lessons done`;
		stats.querySelector('.c-first')!.innerHTML = `<b>${first}/${total}</b> questions right the first time`;
		reset.hidden = Object.keys(saved.lessons).length === 0 && reset.textContent === 'Reset progress';
	}

	paint();
	draw();
	const ro = new ResizeObserver(draw);
	ro.observe(course);
	addEventListener('storage', (e) => e.key === KEY && paint(), { signal });
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

onPage((signal) => {
	const main = document.querySelector<HTMLElement>('main.lesson[data-lesson]');
	if (main && !main.hasAttribute('data-on')) return player(main, signal);
	const course = document.querySelector<HTMLElement>('.course:has(> ol.path)');
	if (course) return coursePath(course, signal);
	// A figure outside a lesson (none yet), drawn all the same.
	const figs = [...document.querySelectorAll<HTMLElement>('figure.diagram:not([data-drawn])')].map((f) => mount(f, false));
	return () => figs.forEach((d) => d?.stop());
});
