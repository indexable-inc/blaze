/**
 * A drag handle that sets a width: the sidebar's right edge (client/side.ts, `--side-w`) and the chat's left edge
 * (client/lib/chat.ts, `--chat-w`). The width is a custom property on `<html>`, kept per browser (localStorage), and
 * client/keep.ts puts it back before the first paint. The handle is a focusable separator: the arrows move it 16px,
 * Home and End go to the ends, and a double click goes back to the default.
 */

export type Resize = {
	/** The custom property set on `<html>`, and the localStorage key it is kept under. */
	prop: string;
	key: string;
	min: number;
	max: number;
	initial: number;
	/** Which way a drag to the right moves the width: +1 for an edge on the right of its box, -1 on the left. */
	dir: 1 | -1;
	label: string;
};

export const SIDE: Resize = { prop: '--side-w', key: 'blaze.side.w', min: 200, max: 440, initial: 268, dir: 1, label: 'Resize the sidebar' };
export const CHAT: Resize = { prop: '--chat-w', key: 'blaze.chat.w', min: 280, max: 640, initial: 340, dir: -1, label: 'Resize the chat' };

function current(r: Resize): number {
	const v = parseFloat(getComputedStyle(document.documentElement).getPropertyValue(r.prop));
	return Number.isFinite(v) ? v : r.initial;
}

function set(r: Resize, w: number, keep: boolean): number {
	const v = Math.round(Math.min(r.max, Math.max(r.min, w)));
	document.documentElement.style.setProperty(r.prop, `${v}px`);
	if (keep) {
		try {
			if (v === r.initial) localStorage.removeItem(r.key);
			else localStorage.setItem(r.key, String(v));
		} catch {
			// Storage refused: the width lasts this page.
		}
	}
	return v;
}

/** A handle for `r`, to be placed on the edge it moves (styles/shell.css `.resizer`). */
export function handle(r: Resize): HTMLElement {
	const h = document.createElement('div');
	h.className = 'resizer';
	h.tabIndex = 0;
	h.setAttribute('role', 'separator');
	h.setAttribute('aria-orientation', 'vertical');
	h.setAttribute('aria-label', r.label);
	h.setAttribute('aria-valuemin', String(r.min));
	h.setAttribute('aria-valuemax', String(r.max));
	const show = (v: number) => h.setAttribute('aria-valuenow', String(v));
	show(current(r));

	h.addEventListener('pointerdown', (e) => {
		if (e.button !== 0) return;
		e.preventDefault();
		h.setPointerCapture(e.pointerId);
		const x0 = e.clientX;
		const w0 = current(r);
		document.documentElement.classList.add('resizing');
		const move = (m: PointerEvent) => show(set(r, w0 + r.dir * (m.clientX - x0), false));
		const up = (u: PointerEvent) => {
			h.removeEventListener('pointermove', move);
			h.removeEventListener('pointerup', up);
			h.removeEventListener('pointercancel', up);
			document.documentElement.classList.remove('resizing');
			show(set(r, w0 + r.dir * (u.clientX - x0), true));
		};
		h.addEventListener('pointermove', move);
		h.addEventListener('pointerup', up);
		h.addEventListener('pointercancel', up);
	});
	h.addEventListener('dblclick', () => show(set(r, r.initial, true)));
	h.addEventListener('keydown', (e) => {
		const w = current(r);
		const step = r.dir * 16;
		const to = e.key === 'ArrowRight' ? w + step : e.key === 'ArrowLeft' ? w - step : e.key === 'Home' ? r.min : e.key === 'End' ? r.max : null;
		if (to === null) return;
		e.preventDefault();
		show(set(r, to, true));
	});
	return h;
}
