/**
 * The reader's progress through the course, kept in localStorage under `blaze.learn` (wrapped: storage can be missing,
 * full or blocked, and then progress lasts for the visit). client/learn.ts writes it as the reader goes; the course
 * map (learn.ts) and the sidebar's lesson marks (side.ts) read it.
 *
 * `lessons` is keyed by each node's key on the course (render: course.rs): a lesson's name (`builds`), an update's
 * `updates/<slug>`. `seen` and `fresh` are what's new: `seen` the updates the site had when the reader was last here,
 * `fresh` those that appeared since and are still new to them. Both are optional, so a store written before them
 * reads the same.
 *
 * Under client/lib/ (not bundled as an entry).
 */

export type Pass = { tries: number; right: boolean };
export type Progress = { step: number; seen: number; done: boolean; pass: Record<string, Pass> };
export type Saved = { v: 1; lessons: Record<string, Progress>; seen?: string[]; fresh?: string[] };
/** todo: not opened; doing: started; done: finished; mastered: finished, every question right the first time. */
export type State = 'todo' | 'doing' | 'done' | 'mastered';

export const KEY = 'blaze.learn';
let memory: Saved = { v: 1, lessons: {} };

export function load(): Saved {
	try {
		const raw = localStorage.getItem(KEY);
		if (raw) {
			const s: unknown = JSON.parse(raw);
			if (s && typeof s === 'object' && 'v' in s && s.v === 1 && 'lessons' in s && s.lessons && typeof s.lessons === 'object')
				memory = s as Saved;
		}
	} catch {
		// No storage, or not ours: keep what this visit has.
	}
	return memory;
}

/** Said on `window` after every store, so this page's other readers repaint (other tabs hear `storage`). */
export const CHANGED = 'blaze:progress';

export function store(s: Saved) {
	memory = s;
	try {
		localStorage.setItem(KEY, JSON.stringify(s));
	} catch {
		// Full or blocked: the progress lasts for this visit.
	}
	dispatchEvent(new Event(CHANGED));
}

/** A lesson's progress, made empty if it has none yet. */
export function progress(s: Saved, lesson: string): Progress {
	const p = s.lessons[lesson];
	if (p && typeof p.step === 'number') {
		p.pass ??= {};
		p.seen ??= p.step;
		return p;
	}
	return (s.lessons[lesson] = { step: 0, seen: 0, done: false, pass: {} });
}

/** How many of `quizzes` were right the first time. */
export function firstTime(p: Progress | undefined, quizzes: string[]): number {
	return quizzes.filter((id) => p?.pass?.[id]?.right && p.pass[id].tries === 1).length;
}

export function stateOf(p: Progress | undefined, quizzes: string[]): State {
	if (p?.done) return firstTime(p, quizzes) === quizzes.length ? 'mastered' : 'done';
	return p && p.seen > 0 ? 'doing' : 'todo';
}

export const stateWords: Record<State, string> = { todo: 'not started', doing: 'in progress', done: 'finished', mastered: 'mastered' };

/**
 * Brings what's new up to date with the updates the site has now (`current`, its keys), and stores it if it changed:
 * anything not seen before is fresh, and stays fresh until the reader studies it or clears the list. On a first visit
 * nothing is fresh; for a reader whose progress predates the list, every update is. Idempotent: every page runs it.
 */
export function roll(s: Saved, current: string[]): Saved {
	const had = s.seen;
	let seen: string[];
	let fresh = (s.fresh ?? []).filter((k) => current.includes(k));
	if (had) {
		seen = had;
		for (const k of current) if (!had.includes(k) && !fresh.includes(k)) fresh.push(k);
	} else {
		seen = [];
		if (Object.keys(s.lessons).length) fresh = [...current];
	}
	seen = [...new Set([...seen.filter((k) => current.includes(k)), ...current])];
	const same = had && had.length === seen.length && had.every((k, i) => k === seen[i]) && (s.fresh ?? []).join() === fresh.join();
	if (same) return s;
	const next: Saved = { ...s, seen, fresh };
	store(next);
	return next;
}

/** The updates that are new to the reader: fresh and not yet finished. */
export function fresh(s: Saved): string[] {
	return (s.fresh ?? []).filter((k) => !s.lessons[k]?.done);
}

/** Clears what's new: every update counts as seen. */
export function clearFresh(s: Saved): Saved {
	const next: Saved = { ...s, fresh: [] };
	store(next);
	return next;
}
