/**
 * What the front door's scenes share (client/intro.ts). A scene is a Section of the page (content/site.flecs, the
 * `index` page) that intro.ts pins while the scroll passes through it; its picture is a pure function of the scene's
 * progress `p`, 0 as it pins and 1 as it lets go, so scrolling back plays it backwards and the same scroll position
 * always draws the same frame. The picture is decoration (aria-hidden): the heading and sentence say it.
 *
 * Under client/lib/ (not bundled as an entry).
 */

export type Picture = {
	/** Scroll length of the scene, in window heights (the pin holds for this minus one). */
	len: number;
	/** The progress whose frame is the still form, under reduced motion. */
	still: number;
	/** Draw the frame at progress p. Called only when p changed, or after a resize. */
	draw(p: number): void;
	/** The stage changed size (and on start). */
	resize?(w: number, h: number, dpr: number): void;
};

export const clamp = (x: number, lo = 0, hi = 1) => Math.min(hi, Math.max(lo, x));
/** 0 before a, 1 after b, smooth between. */
export const span = (p: number, a: number, b: number) => {
	const t = clamp((p - a) / (b - a));
	return t * t * (3 - 2 * t);
};
export const lerp = (a: number, b: number, t: number) => a + (b - a) * t;

/** The page's landing colours (styles/intro.css, body.front), read once per theme. */
export type Inks = { bg: string; ink: string; ink2: string; ink3: string; line: string; card: string; fire: string; fire2: string };
export function inks(): Inks {
	const s = getComputedStyle(document.body);
	const v = (n: string) => s.getPropertyValue(n).trim();
	return { bg: v('--l-bg'), ink: v('--l-ink'), ink2: v('--l-ink-2'), ink3: v('--l-ink-3'), line: v('--l-line'), card: v('--l-card'), fire: v('--l-fire'), fire2: v('--l-fire-2') };
}

/** A canvas filling its stage at the device pixel ratio, with its 2D context in CSS pixels. */
export function canvas(stage: HTMLElement): { el: HTMLCanvasElement; ctx: CanvasRenderingContext2D; size(w: number, h: number, dpr: number): void } {
	const el = document.createElement('canvas');
	el.setAttribute('aria-hidden', 'true');
	stage.append(el);
	const ctx = el.getContext('2d');
	if (!ctx) throw new Error('intro: no 2D canvas');
	return {
		el,
		ctx,
		size(w, h, dpr) {
			el.width = Math.max(1, Math.round(w * dpr));
			el.height = Math.max(1, Math.round(h * dpr));
			ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
		},
	};
}

/** `#rrggbb` with an alpha, for canvas fills. */
export function rgba(hex: string, a: number): string {
	const n = parseInt(hex.replace('#', ''), 16);
	return `rgba(${(n >> 16) & 255},${(n >> 8) & 255},${n & 255},${clamp(a)})`;
}

export const MONO = "'Berkeley Mono', ui-monospace, SFMono-Regular, Menlo, monospace";
export const SANS = "'Inter', -apple-system, BlinkMacSystemFont, system-ui, sans-serif";
