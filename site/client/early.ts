/**
 * The page's frame as the reader left it, set before anything paints: a classic, synchronous script in <head>
 * (render.rs), so the browser can't draw the sidebar open and then shut it, or at its default width and then the
 * dragged one. It marks <html> (body doesn't exist yet): `side-shut` if the reader shut the sidebar (side.ts writes
 * it), and `--side-w` from the width they dragged it to (client/lib/resize.ts). The sidebar's folds and scroll need
 * its markup and stay in keep.ts. A block, not a module: a module would wait for the parse to end.
 */
{
	try {
		const w = Number(localStorage.getItem('blaze.side.w'));
		if (w > 0) document.documentElement.style.setProperty('--side-w', `${w}px`);
		if (localStorage.getItem('blaze.side.shut') === '1') document.documentElement.classList.add('side-shut');
	} catch {
		// Storage off: the sidebar opens as the page renders it.
	}
}
