/**
 * Where each block's controls go (the comment button, client/comments.ts, and "Play from here", client/listen.ts).
 * When the page has a left margin wide enough for them, `main` gets `gutter` and they sit in it; without one they
 * float at the block's top-right corner. Either way they show only for the block under the pointer, or on a touch
 * screen for the block last tapped (`ctl-open`). styles/site.css places them; this only measures and toggles.
 */

/** The margin, in px, the controls need left of the text (a list item's sit furthest out). */
const GUTTER = 110;

export function blockControls(main: HTMLElement, signal: AbortSignal): void {
	// comments.js and listen.js each bundle this module, so the mark that it has run is on `main` itself.
	if ('ctl' in main.dataset) return;
	main.dataset.ctl = '';
	signal.addEventListener('abort', () => delete main.dataset.ctl);

	const box = main.parentElement ?? document.body;
	const fit = () => main.classList.toggle('gutter', main.getBoundingClientRect().left - box.getBoundingClientRect().left >= GUTTER);
	const ro = new ResizeObserver(fit);
	ro.observe(box);
	ro.observe(main);
	signal.addEventListener('abort', () => ro.disconnect());

	// A tap shows the tapped block's controls and hides the last one's; a tap on a link, a control or while
	// selecting text leaves them be.
	if (!matchMedia('(hover: none)').matches) return;
	main.addEventListener('click', (e) => {
		const t = e.target instanceof Element ? e.target : null;
		if (!t || t.closest('a, button, input, textarea, .c-thread') || !document.getSelection()?.isCollapsed) return;
		const b = t.closest('[data-block], [data-say]');
		const was = main.querySelector('.ctl-open');
		if (was && was !== b) was.classList.remove('ctl-open');
		b?.classList.toggle('ctl-open');
	}, { signal });
}
