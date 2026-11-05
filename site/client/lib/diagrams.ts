/**
 * The lessons' diagrams (Figure and Tap in content/schema.flecs; render.rs writes `<figure class="diagram"
 * data-figure="...">` and `<div class="diagram" data-figure="..." data-pick>`). Each is a few named parts (crates,
 * functions, a store) on a coarse grid, joined by wires drawn as rows of square cells, the home page's pixel language
 * (client/art.ts, styles/art.css): the page's inks, one accent (the fire orange) for the part that changed.
 *
 * - Explore (a Figure): tapping a part, or a button under the diagram, plays what happens, such as an edit lighting
 *   up everything that is compiled again; a line under the diagram says it in words, for screen readers too.
 * - Pick (a Tap question): the parts are toggle buttons; client/learn.ts reads the picks and marks them right or wrong.
 *
 * Parts are buttons over an SVG of wire cells, so they are reachable by keyboard and their labels stay text. Timed
 * steps run at once under prefers-reduced-motion. Under client/lib/ (not bundled as an entry), imported by learn.ts.
 */

type Pt = [number, number];
/** How a part looks: lit (redone), edit (the change itself, the accent), hit (found, not redone), old (replaced),
 *  hide (not there yet). */
export type State = '' | 'lit' | 'edit' | 'hit' | 'old' | 'hide';
export type Mark = '' | 'right' | 'wrong';

type Part = {
	id: string;
	label: string;
	/** The cell it hangs from: its centre, or with `at` its left ('l') or right ('r') edge. */
	x: number;
	y: number;
	at?: 'l' | 'r';
	/** Not a choice in a question (the program, the key): drawn, never picked. */
	fixed?: boolean;
};

/** From one part's cell to another's, vertical first (or through `via`'s corners). A `quiet` wire is drawn only once
 *  lit (the splice's jump, which isn't there until the save). */
type Wire = { from: string; to: string; via?: Pt[]; quiet?: boolean };

type Piece = {
	cols: number;
	rows: number;
	parts: Part[];
	wires: Wire[];
	/** Labels are code (crate and function names). */
	code?: boolean;
	/** The line under the diagram until it is touched. */
	idle?: string;
	/** Two ways to tap (a body edit, a signature change), as a segmented switch above the diagram. */
	modes?: { id: string; label: string }[];
	tap?: (v: View, id: string) => void;
	actions?: { label: string; run: (v: View) => void }[];
	/** Parts' states before anything happens. */
	start?: Record<string, State>;
};

export interface View {
	readonly mode: string;
	/** A part's state, `ms` from now. */
	set(id: string, state: State, ms?: number): void;
	/** A wire's state (lit or not), `ms` from now. */
	wire(from: string, to: string, lit: boolean, ms?: number): void;
	say(text: string, ms?: number): void;
	/** Back to the start: every part and wire as it began, pending steps dropped. */
	reset(): void;
}

const still = matchMedia('(prefers-reduced-motion: reduce)');

/** `a`, `a and b`, `a, b and c`. */
const list = (xs: string[]) => (xs.length < 2 ? xs.join('') : `${xs.slice(0, -1).join(', ')} and ${xs[xs.length - 1]}`);

/** Every part that depends on `id` through `wires` (from user to used), with how many steps away, nearest first. */
function users(wires: Wire[], id: string, skip: Set<string>): [string, number][] {
	const out: [string, number][] = [];
	const seen = new Set([id]);
	let edge = [id];
	for (let d = 1; edge.length; d++) {
		const next: string[] = [];
		for (const w of wires)
			if (edge.includes(w.to) && !seen.has(w.from) && !skip.has(w.from)) {
				seen.add(w.from);
				next.push(w.from);
				out.push([w.from, d]);
			}
		edge = next;
	}
	return out;
}

const STEP = 380;

// Lesson 1: source to program, compile then link.
const pipeline: Piece = {
	cols: 26,
	rows: 10,
	parts: [
		{ id: 'src', label: 'source', x: 5, y: 1 },
		{ id: 'compile', label: 'compile', x: 20, y: 1 },
		{ id: 'link', label: 'link', x: 20, y: 8 },
		{ id: 'program', label: 'program', x: 5, y: 8 },
	],
	wires: [
		{ from: 'src', to: 'compile' },
		{ from: 'compile', to: 'link' },
		{ from: 'link', to: 'program' },
	],
	idle: 'Tap Build to follow your code from source to program.',
	actions: [{ label: 'Build', run: build }],
	tap: (v) => build(v),
};

function build(v: View) {
	v.reset();
	v.set('src', 'lit');
	v.wire('src', 'compile', true, STEP);
	v.set('compile', 'edit', STEP * 2);
	v.say('Compiling: the code becomes machine code.', STEP * 2);
	v.set('compile', 'lit', STEP * 4);
	v.wire('compile', 'link', true, STEP * 4);
	v.set('link', 'edit', STEP * 5);
	v.say('Linking: the pieces are joined into one file.', STEP * 5);
	v.set('link', 'lit', STEP * 7);
	v.wire('link', 'program', true, STEP * 7);
	v.set('program', 'lit', STEP * 8);
	v.say('Done: one program you can run.', STEP * 8);
}

// Lesson 1: crates using crates; cargo reruns rustc on an edited crate and every crate that uses it, then links.
const CRATE_WIRES: Wire[] = [
	{ from: 'app', to: 'net' },
	{ from: 'app', to: 'json' },
	{ from: 'net', to: 'core' },
	{ from: 'json', to: 'core' },
	{ from: 'app', to: 'program' },
];
const crates: Piece = {
	cols: 26,
	rows: 15,
	code: true,
	parts: [
		{ id: 'app', label: 'app', x: 11, y: 1 },
		{ id: 'net', label: 'net', x: 4, y: 7 },
		{ id: 'json', label: 'json', x: 18, y: 7 },
		{ id: 'core', label: 'core', x: 11, y: 13 },
		{ id: 'program', label: 'program', x: 25, y: 1, at: 'r', fixed: true },
	],
	wires: CRATE_WIRES,
	idle: 'Tap a crate to edit it.',
	tap(v, id) {
		if (id === 'program') return;
		v.reset();
		v.set(id, 'edit');
		const up = users(CRATE_WIRES, id, new Set(['program']));
		let far = 0;
		for (const [u, d] of up) {
			v.set(u, 'lit', d * STEP);
			for (const w of CRATE_WIRES) if (w.from === u && (w.to === id || up.some(([x]) => x === w.to))) v.wire(w.from, w.to, true, d * STEP);
			far = Math.max(far, d);
		}
		v.wire('app', 'program', true, (far + 1) * STEP);
		v.set('program', 'lit', (far + 1) * STEP);
		const redo = [id, ...up.map(([u]) => u)];
		v.say(`You edited ${id}: cargo runs rustc again on ${list(redo)}, then the whole program is linked again.`, (far + 1) * STEP);
	},
};

// Lesson 2: functions calling functions; a body edit redoes one, a signature change also its callers.
const FN_WIRES: Wire[] = [
	{ from: 'main', to: 'parse' },
	{ from: 'main', to: 'print' },
	{ from: 'parse', to: 'lex' },
];
const fns: Piece = {
	cols: 26,
	rows: 15,
	code: true,
	parts: [
		{ id: 'main', label: 'main', x: 12, y: 1 },
		{ id: 'parse', label: 'parse', x: 5, y: 7 },
		{ id: 'print', label: 'print', x: 19, y: 7 },
		{ id: 'lex', label: 'lex', x: 5, y: 13 },
	],
	wires: FN_WIRES,
	idle: 'Tap a function to edit it.',
	modes: [
		{ id: 'body', label: 'Body edit' },
		{ id: 'sig', label: 'Signature change' },
	],
	tap(v, id) {
		v.reset();
		v.set(id, 'edit');
		if (v.mode === 'body') {
			v.say(`The body of ${id} changed: blaze compiles ${id}, and nothing else.`);
			return;
		}
		const callers = FN_WIRES.filter((w) => w.to === id).map((w) => w.from);
		for (const c of callers) {
			v.set(c, 'lit', STEP);
			v.wire(c, id, true, STEP);
		}
		v.say(
			callers.length
				? `The signature of ${id} changed: its callers, ${list(callers)}, have to be checked again too.`
				: `The signature of ${id} changed, but nothing calls ${id}, so only ${id} is redone.`,
			STEP,
		);
	},
};

// Lesson 2: the splice. The program file as a column of functions; the new lex goes at the end, the old one jumps to it.
const splice: Piece = {
	cols: 32,
	rows: 19,
	code: true,
	parts: [
		{ id: 'main', label: 'main', x: 4, at: 'l', y: 1 },
		{ id: 'parse', label: 'parse', x: 4, at: 'l', y: 5 },
		{ id: 'lex', label: 'lex', x: 4, at: 'l', y: 9 },
		{ id: 'print', label: 'print', x: 4, at: 'l', y: 13 },
		{ id: 'new', label: 'new lex', x: 4, at: 'l', y: 17 },
	],
	wires: [{ from: 'lex', to: 'new', via: [[18, 9], [18, 17]], quiet: true }],
	start: { new: 'hide' },
	idle: 'The program file, one function after another. Tap Save to edit lex.',
	actions: [{ label: 'Save', run: save }],
	tap: (v) => save(v),
};

function save(v: View) {
	v.reset();
	v.set('new', 'edit', 120);
	v.say('The new lex is written at the end of the file…', 120);
	v.set('lex', 'old', STEP * 3);
	v.wire('lex', 'new', true, STEP * 3);
	v.say('…and the old lex now starts with a jump to it. Nothing is linked again.', STEP * 3);
}

// Lesson 3: what a sandboxed proc macro can read (a Tap question). The choices sit in two columns under the one they
// are about, so the longest label still fits half of a phone's width; no wires, since which ones connect is the
// question.
const sandbox: Piece = {
	cols: 28,
	rows: 18,
	parts: [
		{ id: 'macro', label: 'proc macro', x: 14, y: 1, fixed: true },
		{ id: 'package', label: 'its own package', x: 7, y: 6 },
		{ id: 'home', label: 'your home folder', x: 21, y: 6 },
		{ id: 'vars', label: "cargo's variables", x: 7, y: 11 },
		{ id: 'net', label: 'the network', x: 21, y: 11 },
		{ id: 'other', label: "other crates' files", x: 14, y: 16 },
	],
	wires: [],
};

// Lesson 4: what goes into a compiled function's key (a Tap question), laid out as the sandbox is, without wires.
const inputs: Piece = {
	cols: 28,
	rows: 23,
	parts: [
		{ id: 'key', label: 'key', x: 14, y: 1, fixed: true },
		{ id: 'name', label: 'its name', x: 7, y: 6 },
		{ id: 'source', label: "its crate's source", x: 21, y: 6 },
		{ id: 'opt', label: 'optimisation level', x: 7, y: 11 },
		{ id: 'clock', label: 'the time of day', x: 21, y: 11 },
		{ id: 'target', label: 'the target', x: 7, y: 16 },
		{ id: 'user', label: 'your user name', x: 21, y: 16 },
		{ id: 'blaze', label: "blaze's build", x: 14, y: 21 },
	],
	wires: [],
};

// Lesson 5: a shared store. Your build pushes to it; a teammate's build of the same code pulls instead of compiling.
let stored = false;
const share: Piece = {
	cols: 28,
	rows: 12,
	parts: [
		{ id: 'you', label: 'your laptop', x: 0, y: 1, at: 'l' },
		{ id: 'mate', label: "teammate's laptop", x: 27, y: 1, at: 'r' },
		{ id: 'store', label: 'shared store', x: 14, y: 10 },
	],
	wires: [
		{ from: 'you', to: 'store', via: [[3, 1], [3, 10]] },
		{ from: 'mate', to: 'store', via: [[24, 1], [24, 10]] },
	],
	idle: 'Build on your laptop first, then on your teammate’s.',
	actions: [
		{
			label: 'Build on yours',
			run(v) {
				v.reset();
				v.set('you', 'edit');
				v.say('Your laptop compiles the code…');
				v.set('you', 'lit', STEP * 2);
				v.wire('you', 'store', true, STEP * 2);
				v.set('store', 'lit', STEP * 3);
				v.say('…and pushes what it compiled to the shared store.', STEP * 3);
				stored = true;
			},
		},
		{
			label: 'Build on theirs',
			run(v) {
				if (!stored) {
					v.reset();
					v.set('mate', 'edit');
					v.say('Nothing in the store yet, so their laptop compiles it all itself. Try yours first.');
					return;
				}
				v.reset();
				v.set('store', 'lit');
				v.wire('mate', 'store', true, STEP);
				v.set('mate', 'hit', STEP * 2);
				v.say('Same code, same keys: their build pulls the results instead of compiling them.', STEP * 2);
			},
		},
	],
};

const PIECES: Record<string, Piece> = { pipeline, crates, fns, splice, sandbox, inputs, share };

/** The cells of a path through corners joined by straight runs, first to last. */
function cells(corners: Pt[]): Pt[] {
	const out: Pt[] = [corners[0]!];
	for (const [tx, ty] of corners.slice(1)) {
		let [x, y] = out[out.length - 1]!;
		while (x !== tx || y !== ty) {
			if (y !== ty) y += Math.sign(ty - y);
			else x += Math.sign(tx - x);
			out.push([x, y]);
		}
	}
	return out;
}

const SVG = 'http://www.w3.org/2000/svg';

export type Diagram = {
	/** The part buttons that can be picked, by id (pick mode). */
	picked(): Set<string>;
	/** Marks each picked part right or wrong (pick mode); `null` clears the marks. */
	mark(right: Set<string> | null): void;
	/** Picks exactly these parts (to show a past answer). */
	pick(ids: Iterable<string>): void;
	/** Parts can no longer be picked (the question is answered). */
	lock(): void;
	stop(): void;
};

/**
 * Draws the piece named by `el.dataset.figure` into `el`. In pick mode `onPick` is called after each toggle. Returns
 * null for an unknown piece (the element keeps its caption).
 */
export function mount(el: HTMLElement, pickMode: boolean, onPick?: () => void): Diagram | null {
	const piece = PIECES[el.dataset.figure ?? ''];
	if (!piece) return null;
	if (piece === share) stored = false;
	const caption = el.querySelector('figcaption');
	caption?.classList.add('sr');
	const box = document.createElement('div');
	box.className = 'dg-box';
	box.style.aspectRatio = `${piece.cols} / ${piece.rows}`;
	const svg = document.createElementNS(SVG, 'svg');
	svg.setAttribute('viewBox', `0 0 ${piece.cols} ${piece.rows}`);
	svg.setAttribute('aria-hidden', 'true');
	svg.classList.add('dg-wires');
	box.append(svg);

	const at = new Map(piece.parts.map((p) => [p.id, p]));
	const wires = new Map<string, SVGGElement>();
	// Every wire cell, with its column and row: the ones a part covers are hidden (clip below), so a wire stops at the
	// part's edge instead of running under it and showing past its far side.
	const drawn: [SVGRectElement, number, number][] = [];
	for (const w of piece.wires) {
		const a = at.get(w.from)!;
		const b = at.get(w.to)!;
		const mid = Math.round((a.y + b.y) / 2);
		const corners: Pt[] = w.via ? [[a.x, a.y], ...w.via, [b.x, b.y]] : [[a.x, a.y], [a.x, mid], [b.x, mid], [b.x, b.y]];
		const g = document.createElementNS(SVG, 'g');
		g.classList.add('dg-wire');
		if (w.quiet) g.classList.add('quiet');
		for (const [i, j] of cells(corners)) {
			const r = rect(i, j);
			g.append(r);
			drawn.push([r, i, j]);
		}
		svg.append(g);
		wires.set(`${w.from}>${w.to}`, g);
	}

	const buttons = new Map<string, HTMLButtonElement>();
	for (const p of piece.parts) {
		const b = document.createElement('button');
		b.type = 'button';
		b.className = `dg-part${piece.code ? ' code' : ''}`;
		b.dataset.id = p.id;
		if (p.at) b.dataset.at = p.at;
		b.style.left = `${((p.x + (p.at === 'l' ? 0 : p.at === 'r' ? 1 : 0.5)) / piece.cols) * 100}%`;
		b.style.top = `${((p.y + 0.5) / piece.rows) * 100}%`;
		const led = document.createElement('i');
		led.className = 'led';
		const label = document.createElement('span');
		label.textContent = p.label;
		b.append(led, label);
		if (p.fixed || (!pickMode && !piece.tap)) {
			b.disabled = true;
			b.classList.add('fixed');
		} else if (pickMode) b.setAttribute('aria-pressed', 'false');
		box.append(b);
		buttons.set(p.id, b);
	}

	const say = document.createElement('p');
	say.className = 'dg-say';
	say.setAttribute('aria-live', 'polite');
	say.textContent = pickMode ? '' : (piece.idle ?? '');

	const timers = new Set<ReturnType<typeof setTimeout>>();
	const later = (ms: number, f: () => void) => {
		if (ms <= 0 || still.matches) return f();
		const t = setTimeout(() => {
			timers.delete(t);
			f();
		}, ms);
		timers.add(t);
	};
	let mode = piece.modes?.[0]?.id ?? '';
	const view: View = {
		get mode() {
			return mode;
		},
		set: (id, state, ms = 0) => later(ms, () => setState(buttons.get(id), state)),
		wire: (from, to, lit, ms = 0) =>
			later(ms, () => {
				const g = wires.get(`${from}>${to}`);
				if (!g) return;
				g.classList.toggle('lit', lit);
				// Wires can share cells (main to parse and main to print leave main the same way): a lit one is drawn
				// last, so its whole length shows.
				if (lit) svg.append(g);
			}),
		say: (text, ms = 0) => later(ms, () => (say.textContent = text)),
		reset() {
			for (const t of timers) clearTimeout(t);
			timers.clear();
			for (const [id, b] of buttons) setState(b, piece.start?.[id] ?? '');
			for (const g of wires.values()) g.classList.remove('lit');
		},
	};
	view.reset();

	const tools = document.createElement('div');
	tools.className = 'dg-tools';
	if (!pickMode && piece.modes) {
		const seg = document.createElement('div');
		seg.className = 'dg-modes';
		seg.setAttribute('role', 'group');
		for (const m of piece.modes) {
			const b = document.createElement('button');
			b.type = 'button';
			b.textContent = m.label;
			b.setAttribute('aria-pressed', String(m.id === mode));
			b.addEventListener('click', () => {
				mode = m.id;
				for (const o of seg.children) o.setAttribute('aria-pressed', String(o === b));
				view.reset();
				view.say(piece.idle ?? '');
			});
			seg.append(b);
		}
		tools.append(seg);
	}
	if (!pickMode)
		for (const a of piece.actions ?? []) {
			const b = document.createElement('button');
			b.type = 'button';
			b.className = 'dg-act';
			b.textContent = a.label;
			b.addEventListener('click', () => a.run(view));
			tools.append(b);
		}

	box.addEventListener('click', (e) => {
		const b = e.target instanceof Element ? e.target.closest<HTMLButtonElement>('.dg-part') : null;
		if (!b || b.disabled) return;
		const id = b.dataset.id!;
		if (pickMode) {
			b.setAttribute('aria-pressed', String(b.getAttribute('aria-pressed') !== 'true'));
			for (const o of buttons.values()) o.dataset.mark = '';
			onPick?.();
		} else piece.tap?.(view, id);
	});

	// A switch between ways to tap goes above the drawing, buttons that play something below it.
	const parts: HTMLElement[] = [box];
	if (tools.childElementCount) piece.modes ? parts.unshift(tools) : parts.push(tools);
	if (!pickMode) parts.push(say);
	if (caption) parts.push(caption);
	el.replaceChildren(...parts);
	el.dataset.drawn = '';

	/** Hides the wire cells under a part (with a few pixels to spare), from where the parts are on screen. */
	const clip = () => {
		const b = box.getBoundingClientRect();
		if (!b.width) return;
		const cell = b.width / piece.cols;
		const rects = [...buttons.values()].map((p) => p.getBoundingClientRect());
		for (const [r, i, j] of drawn) {
			const x0 = b.left + (i + 0.25) * cell, x1 = b.left + (i + 0.75) * cell;
			const y0 = b.top + (j + 0.25) * cell, y1 = b.top + (j + 0.75) * cell;
			const under = rects.some((p) => x1 > p.left - 1 && x0 < p.right + 1 && y1 > p.top - 1 && y0 < p.bottom + 1);
			r.style.display = under ? 'none' : '';
		}
	};
	const ro = new ResizeObserver(clip);
	ro.observe(box);
	void document.fonts?.ready.then(clip);

	const pickable = () => [...buttons.values()].filter((b) => !b.classList.contains('fixed'));
	return {
		picked: () => new Set(pickable().filter((b) => b.getAttribute('aria-pressed') === 'true').map((b) => b.dataset.id!)),
		mark(right) {
			for (const b of pickable()) {
				const on = b.getAttribute('aria-pressed') === 'true';
				const m: Mark = right === null || !on ? '' : right.has(b.dataset.id!) ? 'right' : 'wrong';
				b.dataset.mark = m;
			}
		},
		pick(ids) {
			const want = new Set(ids);
			for (const b of pickable()) b.setAttribute('aria-pressed', String(want.has(b.dataset.id!)));
		},
		lock() {
			for (const b of pickable()) {
				b.disabled = true;
				b.dataset.locked = '';
			}
		},
		stop() {
			ro.disconnect();
			for (const t of timers) clearTimeout(t);
			timers.clear();
		},
	};
}

function setState(b: HTMLButtonElement | undefined, s: State) {
	if (b) b.dataset.state = s;
}

function rect(i: number, j: number, cls?: string): SVGRectElement {
	const r = document.createElementNS(SVG, 'rect');
	r.setAttribute('x', String(i + 0.25));
	r.setAttribute('y', String(j + 0.25));
	r.setAttribute('width', '0.5');
	r.setAttribute('height', '0.5');
	if (cls) r.classList.add(cls);
	return r;
}
