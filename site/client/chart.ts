/**
 * Hover for the line charts the renderer draws (renderer core chart.rs): the chart is whole without this, its numbers
 * in the table under it; this only shows them where the pointer is. The run nearest the pointer gets a guide line and
 * a tooltip: the table's row for it, each value beside its subject's key from the legend, so it says exactly what
 * the table says. Row charts need none: each row's values are written beside it.
 */
import { onPage } from './lib/page';

/** The chart table's body rows, newest run first. */
function rows(fig: HTMLElement): HTMLTableRowElement[] {
	return [...fig.querySelectorAll<HTMLTableRowElement>('.numbers tbody tr')];
}

function fill(tip: HTMLElement, fig: HTMLElement, row: HTMLTableRowElement | undefined): boolean {
	if (!row) return false;
	const keys = [...fig.querySelectorAll<SVGSVGElement>('.legend .key')];
	const names = [...fig.querySelectorAll<HTMLElement>('.legend li')].map((li) => li.textContent ?? '');
	const head = [...fig.querySelectorAll<HTMLElement>('.numbers thead th')].map((th) => th.textContent ?? '');
	const cells = [...row.cells];
	tip.replaceChildren();
	const title = document.createElement('b');
	title.textContent = cells[0]?.textContent ?? '';
	tip.append(title);
	cells.slice(1).forEach((c, i) => {
		const line = document.createElement('span');
		const key = keys[i];
		if (key) line.append(key.cloneNode(true), `${names[i]}: `);
		else line.append(`${head[i + 1] ?? ''}: `);
		line.append(c.textContent ?? '');
		tip.append(line);
	});
	return true;
}

function place(tip: HTMLElement, x: number, y: number): void {
	tip.hidden = false;
	const r = tip.getBoundingClientRect();
	const left = Math.min(Math.max(8, x + 14), innerWidth - r.width - 8);
	const top = y - r.height - 12 < 8 ? y + 18 : y - r.height - 12;
	tip.style.left = `${left}px`;
	tip.style.top = `${top}px`;
}

onPage((signal) => {
	const figs = [...document.querySelectorAll<HTMLElement>('figure.chart.lines')];
	if (!figs.length) return;
	const tip = document.createElement('div');
	tip.className = 'chart-tip';
	tip.setAttribute('role', 'tooltip');
	tip.hidden = true;
	document.body.append(tip);
	const hide = () => {
		tip.hidden = true;
	};

	for (const fig of figs) {
		// A chart of one run has no plot, only its values.
		const plot = fig.querySelector<SVGSVGElement>('svg.plot');
		const guide = plot?.querySelector<SVGLineElement>('.guide');
		const n = Number(plot?.dataset.runs);
		const edge = Number(plot?.dataset.edge);
		if (!plot || !guide || !(n > 1)) continue;
		plot.addEventListener(
			'pointermove',
			(ev) => {
				const box = plot.getBoundingClientRect();
				const f = (ev.clientX - box.left) / box.width;
				const run = Math.min(n - 1, Math.max(0, Math.round(((f - edge) / (1 - 2 * edge)) * (n - 1))));
				const at = edge + (run / (n - 1)) * (1 - 2 * edge);
				guide.setAttribute('x1', `${at * 100}%`);
				guide.setAttribute('x2', `${at * 100}%`);
				plot.classList.add('on');
				// The table lists runs newest first.
				if (fill(tip, fig, rows(fig)[n - 1 - run])) place(tip, box.left + at * box.width, ev.clientY);
			},
			{ signal },
		);
		plot.addEventListener(
			'pointerleave',
			() => {
				plot.classList.remove('on');
				hide();
			},
			{ signal },
		);
	}
	addEventListener('scroll', hide, { signal, passive: true });
	return () => tip.remove();
});
