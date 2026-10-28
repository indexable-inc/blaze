/**
 * The sidebar as the last page left it. A classic script placed right after the sidebar's markup (render.rs), so it
 * runs while the page is still parsing, before the first paint: the folded sections close (and, on a phone, Reference
 * unless the reader opened it there) and the sidebar's scroller
 * goes back to where it was, and moving to another page doesn't make the sidebar jump. side.ts writes both (the
 * folds on toggle, the scroll on pagehide) and marks the scroller `data-kept` so it doesn't re-centre the open row.
 * The widths the reader dragged the sidebar and the chat to (client/lib/resize.ts) are set here too.
 * A block, not a module: it must not wait for the parse to end.
 */
{
	try {
		for (const [key, prop] of [['blaze.side.w', '--side-w'], ['blaze.chat.w', '--chat-w']]) {
			const w = Number(localStorage.getItem(key));
			if (w > 0) document.documentElement.style.setProperty(prop, `${w}px`);
		}
		const raw = localStorage.getItem('blaze.side.folded');
		const folded: unknown = raw === null ? [] : JSON.parse(raw);
		// In the phone drawer a FoldOnPhone section (Reference) starts folded unless the reader opened it there
		// (side.ts keeps that in blaze.side.opened); elsewhere a section is folded if the reader folded it.
		const phone = matchMedia('(max-width: 800px)').matches;
		const rawOpened = localStorage.getItem('blaze.side.opened');
		const opened: unknown = rawOpened === null ? [] : JSON.parse(rawOpened);
		for (const sec of document.querySelectorAll<HTMLDetailsElement>('details.sec[data-sec]')) {
			if ('here' in sec.dataset) continue;
			if (phone && 'phoneFold' in sec.dataset) sec.open = Array.isArray(opened) && opened.includes(sec.dataset.sec);
			else if (Array.isArray(folded) && folded.includes(sec.dataset.sec)) sec.open = false;
		}
		const box = document.querySelector<HTMLElement>('.side .scrollbox');
		const top = sessionStorage.getItem('blaze.side.scroll');
		if (box && top !== null) {
			box.scrollTop = Number(top);
			box.dataset.kept = '';
		}
	} catch {
		// Storage off: the sidebar opens as the page renders it.
	}
}
