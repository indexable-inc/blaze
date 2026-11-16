/**
 * Scene 2, the edit loop: a line of Rust is edited, then cargo and blaze build it side by side on one time scale, the
 * scroll being the clock. The two times are the sentence's own (`<strong>6.84 s</strong>` and `<strong>82 ms</strong>`
 * in the section, content/site.flecs, from docs/benchmarks.md), so the picture can never say another number; the bars
 * are to scale, so blaze's is a sliver. Transform and opacity only, plus the two clocks' text.
 */
import { type Picture, clamp, span } from './scene';

/** "6.84 s" or "82 ms" to milliseconds. */
function ms(text: string): number {
	const m = /([\d.]+)\s*(ms|s)\b/.exec(text);
	if (!m) throw new Error(`intro: no time in "${text}"`);
	return Number(m[1]) * (m[2] === 's' ? 1000 : 1);
}

const said = (t: number) => (t < 1000 ? `${Math.round(t)} ms` : `${(t / 1000).toFixed(2)} s`);

// The edit: `.trim()` is typed into the second line, before its closing parenthesis.
const BEFORE = '    format!("{}", self.name)';
const TYPED = '    format!("{}", self.name.trim())';

export function loop(section: HTMLElement, stage: HTMLElement): Picture {
	const times = [...section.querySelectorAll('p strong')].map((s) => ms(s.textContent ?? ''));
	if (times.length !== 2) throw new Error('intro: the edit scene names two times, cargo then blaze');
	const [cargo, blaze] = times as [number, number];

	const root = document.createElement('div');
	root.className = 'loop';
	root.setAttribute('aria-hidden', 'true');
	root.innerHTML = `<div class="ed"><span class="ln">fn title(&amp;self) -&gt; String {</span>
<span class="ln line2"></span><span class="caret"></span>
<span class="ln">}</span></div>
<div class="lanes">
<div class="lane c"><span class="who">cargo</span><span class="track"><span class="fill"></span></span><span class="t">0 ms</span></div>
<div class="lane b"><span class="who">blaze</span><span class="track"><span class="fill"></span></span><span class="t">0 ms</span></div>
</div>
<div class="x">${Math.round(cargo / blaze)}×<small>faster, this edit</small></div>`;
	stage.append(root);
	const line2 = root.querySelector<HTMLElement>('.line2')!;
	const caret = root.querySelector<HTMLElement>('.caret')!;
	const [lc, lb] = [...root.querySelectorAll<HTMLElement>('.lane')] as [HTMLElement, HTMLElement];
	const fill = (l: HTMLElement) => l.querySelector<HTMLElement>('.fill')!;
	const clock = (l: HTMLElement) => l.querySelector<HTMLElement>('.t')!;
	const x = root.querySelector<HTMLElement>('.x')!;
	let shown = ['', '', ''];

	return {
		len: 3.2,
		still: 1,
		draw(p) {
			// 0 to 0.2: the edit is typed. 0.22 to 0.86: the clock runs from 0 to cargo's time. Then the ratio.
			const typed = clamp(p / 0.2);
			const common = BEFORE.length - 1; // up to the closing parenthesis, which moves along
			const n = Math.round(typed * (TYPED.length - BEFORE.length));
			const text = typed === 0 ? BEFORE : TYPED.slice(0, common + n) + ')';
			const tMs = clamp((p - 0.22) / 0.64) * cargo;
			const cText = said(tMs);
			const bText = said(Math.min(tMs, blaze));
			if (shown[0] !== text) {
				shown[0] = text;
				// Before the edit, the line as it was; while and after, what was typed in fire.
				line2.innerHTML = '';
				const head = document.createElement('span');
				head.textContent = typed === 0 ? text : text.slice(0, common);
				line2.append(head);
				if (typed > 0) {
					const added = document.createElement('span');
					added.className = 'ins';
					added.textContent = text.slice(common, -1);
					line2.append(added, ')');
				}
			}
			caret.style.opacity = p < 0.21 ? '1' : '0';
			if (shown[1] !== cText) clock(lc).textContent = shown[1] = cText;
			if (shown[2] !== bText) clock(lb).textContent = shown[2] = bText;
			fill(lc).style.transform = `scaleX(${tMs / cargo})`;
			fill(lb).style.transform = `scaleX(${Math.min(tMs, blaze) / cargo})`;
			lb.classList.toggle('done', tMs >= blaze);
			lc.classList.toggle('done', tMs >= cargo);
			const r = span(p, 0.86, 0.95);
			x.style.opacity = String(r);
			x.style.transform = `translateY(${(1 - r) * 16}px) scale(${0.96 + r * 0.04})`;
		},
	};
}
