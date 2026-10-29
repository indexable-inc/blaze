/**
 * The site's own tooltips, in place of the browser's (slow, and drawn in the system's style). Hovering anything with
 * a `title` moves it to `data-tip`, so the native one never shows, and shows it after TIP_MS above the element (below
 * when there is no room). An element with no other name keeps the text as its `aria-label`. Keyboard focus shows it
 * too. Loaded on every page by client/side.ts; what it draws itself sets `tipOn` instead of a title, since an element
 * redrawn under a resting pointer gets no hover event and would show the native tooltip.
 */

const TIP_MS = 80;

/** Gives `e` a tooltip, and the same text as its name when it has no text of its own (a face, an icon button). */
export function tipOn(e: HTMLElement, text: string): void {
	e.removeAttribute('title');
	e.dataset.tip = text;
	if (!e.textContent?.trim()) e.setAttribute('aria-label', text);
}
const tip = document.createElement('div');
tip.className = 'tip';
tip.setAttribute('role', 'tooltip');
tip.hidden = true;
let cur: HTMLElement | null = null;
let timer: ReturnType<typeof setTimeout> | undefined;

function target(n: EventTarget | null): HTMLElement | null {
	const t = n instanceof Element ? n.closest<HTMLElement>('[title], [data-tip]') : null;
	if (!t) return null;
	const v = t.getAttribute('title');
	if (v !== null) {
		t.removeAttribute('title');
		if (v) {
			t.dataset.tip = v;
			if (!t.hasAttribute('aria-label') && !t.textContent?.trim()) t.setAttribute('aria-label', v);
		} else delete t.dataset.tip;
	}
	return t.dataset.tip ? t : null;
}

let watch: ReturnType<typeof setInterval> | undefined;

/** A tip that only repeats what is in full view on the element (a sidebar row's own name) says nothing: not shown. */
function needless(t: HTMLElement, text: string): boolean {
	const squash = (v: string) => v.replace(/\s+/g, ' ').trim().toLowerCase();
	if (!squash(t.innerText).includes(squash(text))) return false;
	for (const e of [t, ...t.querySelectorAll<HTMLElement>('*')]) if (e.scrollWidth > e.clientWidth + 1) return false;
	return true;
}

function show(t: HTMLElement): void {
	if (!t.isConnected || !t.dataset.tip || needless(t, t.dataset.tip)) return;
	// Redrawn away (the presence lists redraw on every change): go with it.
	clearInterval(watch);
	watch = setInterval(() => {
		if (!t.isConnected) hide();
	}, 200);
	if (!tip.isConnected) document.body.append(tip);
	tip.textContent = t.dataset.tip;
	tip.hidden = false;
	const r = t.getBoundingClientRect();
	const w = tip.offsetWidth;
	const h = tip.offsetHeight;
	const below = r.top - h - 8 < 4;
	tip.classList.toggle('below', below);
	tip.style.left = `${Math.round(Math.min(innerWidth - w - 6, Math.max(6, r.left + r.width / 2 - w / 2)))}px`;
	tip.style.top = `${Math.round(below ? r.bottom + 6 : r.top - h - 6)}px`;
}

function hide(): void {
	clearTimeout(timer);
	clearInterval(watch);
	cur = null;
	tip.hidden = true;
}

function enter(t: HTMLElement | null): void {
	if (t === cur) return;
	hide();
	if (!t) return;
	cur = t;
	timer = setTimeout(() => show(t), TIP_MS);
}

document.addEventListener('pointerover', (e) => {
	if (e.pointerType !== 'touch') enter(target(e.target));
});
document.addEventListener('pointerout', (e) => {
	if (cur && !(e.relatedTarget instanceof Node && cur.contains(e.relatedTarget))) hide();
});
document.addEventListener('focusin', (e) => {
	if (e.target instanceof Element && e.target.matches(':focus-visible')) enter(target(e.target));
});
document.addEventListener('focusout', hide);
for (const t of ['pointerdown', 'scroll', 'keydown'] as const) addEventListener(t, hide, { capture: true, passive: true });
