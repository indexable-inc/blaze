/**
 * Scene 4, a superset of Rust: one code card whose lines move from step to step as the scroll goes. The steps are the
 * section's own code blocks, in order (content/site.flecs): each H3 names a feature, the Dim after it says it in a line,
 * and its Code blocks are its steps (Rust as it is, then blaze's form). A line both steps have moves to its new row; a
 * line only one has fades, and a line blaze added is marked in fire. The last block is a terminal (lang ""), coloured
 * as blaze-screen draws a failure (docs/terminal.md, "Rich text and the palette").
 */
import { type Picture, clamp, lerp, span } from './scene';

type Step = { feature: number; html: string[]; text: string[]; term: boolean; tag: string };
type Line = { el: HTMLElement; hl: HTMLElement | null; rows: Map<number, number>; added: Set<number> };

/** Highlighted HTML cut into lines, each closing the spans open at its end and opening them again on the next. */
function splitLines(html: string): string[] {
	const out: string[] = [];
	const open: string[] = [];
	let cur = '';
	const re = /(<span[^>]*>)|(<\/span>)|(\n)|([^<\n]+|<)/g;
	for (let m: RegExpExecArray | null; (m = re.exec(html)); ) {
		if (m[1]) {
			open.push(m[1]);
			cur += m[1];
		} else if (m[2]) {
			open.pop();
			cur += m[2];
		} else if (m[3]) {
			out.push(cur + '</span>'.repeat(open.length));
			cur = open.join('');
		} else cur += m[4];
	}
	out.push(cur + '</span>'.repeat(open.length));
	return out;
}

const esc = (s: string) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

/** A terminal line as blaze-screen colours it: the error and its code, where, the caret, help, the end line. */
function termLine(t: string): string {
	let h = esc(t);
	h = h.replace(/^(error\[E\d+\]):/, '<span class="e">$1</span>:');
	h = h.replace(/^(\s*)(--&gt;)/, '$1<span class="g">$2</span>');
	h = h.replace(/^(\s*\|\s*)(\^+)$/, '$1<span class="u">$2</span>');
	h = h.replace(/^(help):/, '<span class="h">$1</span>:');
	h = h.replace(/^(✗)/, '<span class="e">$1</span>');
	h = h.replace(/(blaze explain \S+)$/, '<span class="c">$1</span>');
	return h;
}

/** The longest common subsequence of two lists of lines, as pairs of indices. */
function lcs(a: string[], b: string[]): [number, number][] {
	const n = a.length;
	const m = b.length;
	const d: number[][] = Array.from({ length: n + 1 }, () => new Array<number>(m + 1).fill(0));
	for (let i = n - 1; i >= 0; i--) for (let j = m - 1; j >= 0; j--) d[i]![j] = a[i] === b[j] ? d[i + 1]![j + 1]! + 1 : Math.max(d[i + 1]![j]!, d[i]![j + 1]!);
	const pairs: [number, number][] = [];
	for (let i = 0, j = 0; i < n && j < m; ) {
		if (a[i] === b[j]) pairs.push([i++, j++]);
		else if (d[i + 1]![j]! >= d[i]![j + 1]!) i++;
		else j++;
	}
	return pairs;
}

export function morph(section: HTMLElement, stage: HTMLElement): Picture {
	const features: { name: string; line: string }[] = [];
	const steps: Step[] = [];
	for (const el of section.querySelectorAll('h3, h3 ~ p.dim, h3 ~ figure.codeblock')) {
		if (el.tagName === 'H3') features.push({ name: el.textContent ?? '', line: '' });
		else if (el.matches('p.dim') && features.length && !features.at(-1)!.line) features.at(-1)!.line = el.innerHTML;
		else if (el.matches('figure.codeblock') && features.length) {
			const code = el.querySelector('code');
			const term = el.querySelector('pre')?.dataset.lang === '';
			const text = (code?.textContent ?? '').split('\n');
			const html = term ? text.map(termLine) : splitLines(code?.innerHTML ?? '');
			steps.push({ feature: features.length - 1, html, text, term, tag: '' });
		}
	}
	if (steps.length < 2) throw new Error('intro: the superset scene has fewer than two code steps');
	// The card's tag: a feature's first step of two is Rust as it is, its second blaze's form; a lone step is blaze's.
	for (const s of steps) {
		const pair = steps.filter((t) => t.feature === s.feature);
		s.tag = s.term ? 'blaze build' : pair.length > 1 && pair[0] === s ? 'Rust' : 'blaze';
	}

	const root = document.createElement('div');
	root.className = 'morph';
	root.setAttribute('aria-hidden', 'true');
	const cap = document.createElement('div');
	cap.className = 'cap';
	const caps = features.map((f) => {
		const d = document.createElement('div');
		d.innerHTML = `<b></b><span>${f.line}</span>`;
		d.querySelector('b')!.textContent = f.name;
		cap.append(d);
		return d;
	});
	const card = document.createElement('div');
	card.className = 'card code-roles';
	const tags = steps.map((s) => {
		const t = document.createElement('div');
		t.className = 'tag';
		t.textContent = s.tag;
		card.append(t);
		return t;
	});
	root.append(cap, card);
	stage.append(root);

	// One element per line, shared by neighbouring steps where the text is the same.
	const lines: Line[] = [];
	const make = (step: number, row: number): Line => {
		const s = steps[step]!;
		const el = document.createElement('div');
		el.className = s.term ? 'line term' : 'line hljs';
		el.innerHTML = s.html[row] || ' ';
		card.append(el);
		const line: Line = { el, hl: null, rows: new Map([[step, row]]), added: new Set() };
		lines.push(line);
		return line;
	};
	let prev = steps[0]!.text.map((_, r) => make(0, r));
	for (let k = 1; k < steps.length; k++) {
		const a = steps[k - 1]!;
		const b = steps[k]!;
		const pairs = a.feature === b.feature && !a.term && !b.term ? lcs(a.text, b.text) : [];
		const next: Line[] = [];
		for (let r = 0; r < b.text.length; r++) {
			const match = pairs.find(([, j]) => j === r);
			if (match) {
				const l = prev[match[0]]!;
				l.rows.set(k, r);
				next.push(l);
			} else {
				const l = make(k, r);
				// A line blaze's form adds to the Rust before it.
				if (pairs.length && b.text[r]!.trim()) {
					l.added.add(k);
					l.hl = document.createElement('span');
					l.hl.className = 'hl';
					l.el.prepend(l.hl);
				}
				next.push(l);
			}
		}
		prev = next;
	}

	let lh = 24;
	const pad = 20;
	const head = 40; // room for the tag above the first line
	const N = steps.length;
	// How each move from step k to k + 1 splits its stretch between its parts (see draw): a part with nothing to do
	// takes no time, and one that only resizes the card takes half a part.
	const phases = steps.slice(0, -1).map((a, k) => {
		const b = steps[k + 1]!;
		const label = a.tag !== b.tag || a.feature !== b.feature;
		const kept = lines.filter((l) => l.rows.has(k) && l.rows.has(k + 1));
		const wOut = label || lines.some((l) => l.rows.has(k) && !l.rows.has(k + 1)) ? 1 : 0;
		const wMove = kept.some((l) => l.rows.get(k) !== l.rows.get(k + 1)) ? 1 : a.text.length !== b.text.length ? 0.5 : 0;
		const wIn = label || lines.some((l) => l.rows.has(k + 1) && !l.rows.has(k)) ? 1 : 0;
		const all = Math.max(wOut + wMove + wIn, 1e-6);
		return { b1: wOut / all, b2: (wOut + wMove) / all };
	});
	return {
		len: 1 + N * 0.85,
		still: 0,
		resize(w) {
			const chars = Math.max(...steps.flatMap((s) => s.text.map((t) => t.length)));
			const inner = Math.min(w, 780) - 44;
			const fs = clamp(Math.floor((inner / (chars * 0.6)) * 2) / 2, 7, 14.5);
			lh = Math.round(fs * 1.7);
			card.style.setProperty('--fs', `${fs}px`);
			card.style.setProperty('--lh', `${lh}px`);
		},
		draw(p) {
			// Each step holds, then moves to the next over the middle of its stretch, in three parts that never
			// share a frame, so no text is drawn over other text wherever the scroll stops:
			// 1. `out`: the lines the next step drops fade where they stand, and so does a label that changes;
			// 2. `move`: the lines both steps have slide to their new rows and the card takes its new height,
			//    opening the rows the next step adds (shared lines keep their order, so they never cross);
			// 3. `inn`: the added lines and the new label fade in, into the room made for them.
			const s = clamp(p * 1.04 - 0.02) * (N - 1);
			const k = Math.min(N - 2, Math.floor(s));
			const f = span(s - k, 0.3, 0.75);
			const { b1, b2 } = phases[k]!;
			const out = b1 > 0 ? clamp(f / b1) : 1;
			const move = b2 > b1 ? span(f, b1, b2) : Number(f >= b1);
			const inn = b2 < 1 ? clamp((f - b2) / (1 - b2)) : 1;
			const rows = lerp(steps[k]!.text.length, steps[k + 1]!.text.length, move);
			const top = head;
			// The card grows and shrinks with its step: its own layout only (contain: layout paint), one element.
			card.style.height = `${Math.round(head + pad + rows * lh)}px`;
			for (const l of lines) {
				const a = l.rows.get(k);
				const b = l.rows.get(k + 1);
				let y = 0;
				let o = 0;
				let dx = 0;
				if (a !== undefined && b !== undefined) {
					y = lerp(a, b, move);
					o = 1;
				} else if (a !== undefined) {
					y = a;
					o = 1 - out;
					dx = -out * 12;
				} else if (b !== undefined) {
					y = b;
					o = inn;
					dx = (1 - inn) * 12;
				}
				l.el.style.opacity = o.toFixed(3);
				l.el.style.transform = `translate(${dx.toFixed(1)}px, ${(top + y * lh).toFixed(1)}px)`;
				if (l.hl) {
					const lit = l.added.has(k + 1) && b !== undefined ? inn : l.added.has(k) && a !== undefined ? 1 : 0;
					l.hl.style.opacity = lit.toFixed(3);
				}
			}
			// A label both steps share stays as it is; one that changes is gone before the next comes in.
			const same = steps[k]!.tag === steps[k + 1]!.tag;
			for (const [i, t] of tags.entries()) {
				const o = i === k ? (same ? Number(f < 0.5) : 1 - out) : i === k + 1 ? (same ? Number(f >= 0.5) : inn) : 0;
				t.style.opacity = String(o);
			}
			for (const [i, c] of caps.entries()) {
				const was = steps[k]!.feature === i;
				const next = steps[k + 1]!.feature === i;
				const o = was && next ? 1 : was ? 1 - out : next ? inn : 0;
				c.style.opacity = o.toFixed(3);
				c.style.transform = `translateY(${((1 - o) * 8).toFixed(1)}px)`;
			}
		},
	};
}
