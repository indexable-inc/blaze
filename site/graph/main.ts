/**
 * The code graph page (/graph): blaze's symbols and their relations as a live force-directed graph, drawn with
 * WebGPU by @codegraphy-dev/graph-renderer, laid out by its deterministic wasm Barnes-Hut simulation.
 *
 * Loads `?src=<same-origin url>` or graph/sample.json (schema.ts), or a graph file dropped on the page. Without
 * WebGPU it says so and draws nothing: there is no other renderer.
 */
import {
	createGraphLayoutEngine,
	prepareGraphPhysicsFromBytes,
	WebGpuGraphRenderer,
	type GraphLayoutConfig,
	type GraphLayoutEngine,
	type GraphRendererLink,
	type GraphRendererNode,
	type GraphRendererNodeStyle,
} from '@codegraphy-dev/graph-renderer';
import { buildView, colourOf, legendOf, linkStrength, radiusOf, type ColourBy, type ViewGraph } from './model';
import { GraphFileError, parseGraph } from './schema';

const $ = <T extends HTMLElement>(id: string) => {
	const el = document.getElementById(id);
	if (!el) throw new Error(`graph.html has no #${id}`);
	return el as T;
};
const stage = $<HTMLDivElement>('stage');
const gpuCanvas = $<HTMLCanvasElement>('gpu');
const labelCanvas = $<HTMLCanvasElement>('labels');
const messageEl = $<HTMLDivElement>('message');
const statsEl = $<HTMLSpanElement>('stats');
const searchEl = $<HTMLInputElement>('q');
const resultsEl = $<HTMLUListElement>('results');
const detailsEl = $<HTMLElement>('details');
const legendEl = $<HTMLUListElement>('legend');
const colourByEl = $<HTMLSelectElement>('colour-by');
const labelsOnEl = $<HTMLInputElement>('labels-on');

const BACKGROUND = '#16161e';
const LINK = '#565f89';
const DIM_NODE = 0.12;
const LAYOUT: Partial<GraphLayoutConfig> = { chargeStrength: -300, linkDistance: 40, linkStrength: 0.7, centralGravity: 0.04, collisionPadding: 2 };
/** The force sliders, as in Obsidian's graph settings: input id, layout setting, and the setting per slider step. */
const FORCES = [
	['f-centre', 'centralGravity', 0.01],
	['f-repel', 'chargeStrength', -10],
	['f-link', 'linkStrength', 0.05],
	['f-distance', 'linkDistance', 1],
] as const;

/** Shows a message over the page in place of the graph (WebGPU missing, a bad file, a lost device). */
function showMessage(title: string, detail: string) {
	messageEl.replaceChildren(el('h2', {}, title), el('p', {}, detail));
	messageEl.hidden = false;
}

function el<K extends keyof HTMLElementTagNameMap>(tag: K, props: Partial<HTMLElementTagNameMap[K]>, ...children: (Node | string)[]) {
	const e = document.createElement(tag);
	Object.assign(e, props);
	e.append(...children);
	return e;
}

// ---- state --------------------------------------------------------------------------------------------------------

let renderer: WebGpuGraphRenderer | undefined;
let view: ViewGraph;
let engine: GraphLayoutEngine;
let rnodes: GraphRendererNode[] = [];
let rlinks: GraphRendererLink[] = [];
const nodeIndex = new Map<GraphRendererNode, number>();
const linkIndex = new Map<GraphRendererLink, number>();
let radii = new Float32Array();

const camera = { centerX: 0, centerY: 0, zoom: 1 };
/** Until the reader moves the camera, it keeps the whole graph in view as it settles. */
let autoFit = true;
let flight: { from: typeof camera; to: typeof camera; start: number } | undefined;

let colourBy: ColourBy = 'kind';
let hovered = -1;
let selected = -1;
let matches: Set<number> | undefined;
let positionVersion = 0;
let styleVersion = 0;
let dirty = true;
let raf = 0;

/** The focused node (hovered, else selected) and its neighbours, or nothing. */
let focus = -1;
let focusSet = new Set<number>();

let dragging: { node: number; pointer: number; x: number; y: number; moved: boolean } | undefined;
let panning: { pointer: number; x: number; y: number; moved: boolean } | undefined;

// ---- geometry -----------------------------------------------------------------------------------------------------

const size = () => ({ w: stage.clientWidth, h: stage.clientHeight });
const toScreen = (x: number, y: number) => {
	const { w, h } = size();
	return { x: w / 2 + (x - camera.centerX) * camera.zoom, y: h / 2 + (y - camera.centerY) * camera.zoom };
};
const toWorld = (sx: number, sy: number) => {
	const { w, h } = size();
	return { x: camera.centerX + (sx - w / 2) / camera.zoom, y: camera.centerY + (sy - h / 2) / camera.zoom };
};
/** The renderer draws a node of world radius r at r * sqrt(zoom) px (never under half a pixel). */
const screenRadius = (i: number) => Math.max(0.5, radii[i] * Math.sqrt(camera.zoom)) * (i === hovered ? 1.3 : 1);

function pick(sx: number, sy: number): number {
	let best = -1;
	let bestD = Infinity;
	for (let i = 0; i < rnodes.length; i++) {
		const p = toScreen(engine.x[i], engine.y[i]);
		const d = Math.hypot(p.x - sx, p.y - sy);
		const r = Math.max(screenRadius(i), 4) + 2;
		if (d <= r && d < bestD) {
			best = i;
			bestD = d;
		}
	}
	return best;
}

/** The camera that shows `nodes` (every node when not given), zoomed no closer than `maxZoom`. */
function fitTarget(nodes?: Iterable<number>, maxZoom = 4): typeof camera {
	const { w, h } = size();
	let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
	for (const i of nodes ?? rnodes.keys()) {
		x0 = Math.min(x0, engine.x[i] - radii[i]);
		x1 = Math.max(x1, engine.x[i] + radii[i]);
		y0 = Math.min(y0, engine.y[i] - radii[i]);
		y1 = Math.max(y1, engine.y[i] + radii[i]);
	}
	if (!Number.isFinite(x0)) return { ...camera };
	const zoom = Math.min(maxZoom, Math.max(0.02, Math.min(w / (x1 - x0 + 120), h / (y1 - y0 + 120))));
	return { centerX: (x0 + x1) / 2, centerY: (y0 + y1) / 2, zoom };
}

/** Flies to node i and its neighbours. */
function flyToNode(i: number) {
	const around = [i];
	for (let k = view.adjStart[i]; k < view.adjStart[i + 1]; k++) around.push(view.adj[k]);
	const t = fitTarget(around, 2.5);
	flyTo({ ...t, zoom: t.zoom * 0.8 });
}

function flyTo(to: typeof camera) {
	flight = { from: { ...camera }, to, start: performance.now() };
	autoFit = false;
	schedule();
}

// ---- styles (stable accessors: the renderer re-reads them when styleVersion moves) ----------------------------------

function restyle() {
	focus = hovered >= 0 ? hovered : selected;
	focusSet = new Set<number>();
	if (focus >= 0) {
		focusSet.add(focus);
		for (let k = view.adjStart[focus]; k < view.adjStart[focus + 1]; k++) focusSet.add(view.adj[k]);
	}
	styleVersion++;
	dirty = true;
	schedule();
}

const getNodeStyle = (n: GraphRendererNode): GraphRendererNodeStyle => {
	const i = nodeIndex.get(n)!;
	const fact = view.nodes[i].type === 'fact';
	let opacity = 1;
	if (focus >= 0) opacity = focusSet.has(i) ? 1 : DIM_NODE;
	else if (matches) opacity = matches.has(i) ? 1 : DIM_NODE;
	const d = radii[i] * 2;
	return {
		borderColor: '#ffffff',
		borderWidth: i === selected ? 2 : 0,
		cornerRadius: 0,
		fillColor: colourOf(view, i, colourBy),
		fillOpacity: 1,
		height: d,
		width: d,
		opacity,
		shape: fact ? 'diamond' : 'circle',
	};
};
const touchesFocus = (li: number) => view.links[li].a === focus || view.links[li].b === focus;
const getLinkColor = (l: GraphRendererLink) => (focus >= 0 && touchesFocus(linkIndex.get(l)!) ? colourOf(view, focus, colourBy) : LINK);
const getLinkOpacity = (l: GraphRendererLink) => {
	const li = linkIndex.get(l)!;
	if (focus >= 0) return touchesFocus(li) ? 0.9 : 0.04;
	if (matches) return matches.has(view.links[li].a) && matches.has(view.links[li].b) ? 0.6 : 0.05;
	return 0.55 * linkStrength(view, li);
};
const getLinkWidth = (l: GraphRendererLink) => (focus >= 0 && touchesFocus(linkIndex.get(l)!) ? 1.6 : 1);
const getArrowColor = () => LINK;

// ---- frame loop ---------------------------------------------------------------------------------------------------

function schedule() {
	if (!raf) raf = requestAnimationFrame(frame);
}

function frame(now: number) {
	raf = 0;
	if (!renderer) return;
	let busy = false;
	if (!engine.settled || dragging) {
		const tick = engine.tick();
		if (tick.steps > 0) positionVersion++;
		busy = true;
	}
	if (flight) {
		const t = Math.min(1, (now - flight.start) / 450);
		const e = t < 0.5 ? 2 * t * t : 1 - (-2 * t + 2) ** 2 / 2;
		camera.centerX = flight.from.centerX + (flight.to.centerX - flight.from.centerX) * e;
		camera.centerY = flight.from.centerY + (flight.to.centerY - flight.from.centerY) * e;
		camera.zoom = flight.from.zoom * (flight.to.zoom / flight.from.zoom) ** e;
		if (t >= 1) flight = undefined;
		busy = true;
	} else if (autoFit && !engine.settled) {
		const to = fitTarget();
		camera.centerX += (to.centerX - camera.centerX) * 0.08;
		camera.centerY += (to.centerY - camera.centerY) * 0.08;
		camera.zoom *= (to.zoom / camera.zoom) ** 0.08;
	}
	if (busy || dirty) {
		if (!renderer.canRender()) {
			// The GPU still has frames queued (the renderer bounds its queue): try again next frame.
			schedule();
			return;
		}
		const { w, h } = size();
		renderer.render({
			backgroundColor: BACKGROUND,
			camera: { ...camera },
			cssWidth: w,
			cssHeight: h,
			devicePixelRatio: window.devicePixelRatio,
			directionMode: 'none',
			edgeSources: view.edgeSources,
			edgeTargets: view.edgeTargets,
			getArrowColor,
			getLinkColor,
			getLinkOpacity,
			getLinkWidth,
			getNodeStyle,
			hoveredLink: null,
			hoveredNodeIndex: hovered,
			hoveredNodeScale: 1.3,
			links: rlinks,
			nodes: rnodes,
			nodeX: engine.x,
			nodeY: engine.y,
			positionVersion,
			styleVersion,
		});
		drawLabels();
		dirty = false;
	}
	if (busy) schedule();
}

// ---- labels (2D text over the WebGPU canvas; text only) -----------------------------------------------------------

const labelCtx = labelCanvas.getContext('2d')!;

function drawLabels() {
	const { w, h } = size();
	const dpr = window.devicePixelRatio;
	if (labelCanvas.width !== Math.round(w * dpr) || labelCanvas.height !== Math.round(h * dpr)) {
		labelCanvas.width = Math.round(w * dpr);
		labelCanvas.height = Math.round(h * dpr);
	}
	labelCtx.setTransform(dpr, 0, 0, dpr, 0, 0);
	labelCtx.clearRect(0, 0, w, h);
	labelCtx.font = '12px ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif';
	labelCtx.textAlign = 'center';
	labelCtx.textBaseline = 'top';

	// Most important first: the focus and its neighbours, the selection, matches, then by degree. A label that
	// would overlap one already placed is skipped.
	const rank = (i: number) =>
		(i === focus ? 4e6 : 0) + (focusSet.has(i) ? 2e6 : 0) + (i === selected ? 3e6 : 0) + (matches?.has(i) ? 1e6 : 0) + view.degree[i];
	const order: number[] = [];
	for (let i = 0; i < rnodes.length; i++) {
		if (view.nodes[i].type === 'fact' && !focusSet.has(i)) continue;
		if (focus >= 0 && !focusSet.has(i)) continue;
		order.push(i);
	}
	order.sort((a, b) => rank(b) - rank(a));
	const placed: { x0: number; y0: number; x1: number; y1: number }[] = [];
	let drawn = 0;
	for (const i of order) {
		if (drawn >= 400) break;
		const sr = screenRadius(i);
		const forced = focusSet.has(i) || i === selected || (matches?.has(i) ?? false);
		// Obsidian-style fade in with size on screen: small nodes get labels once zoomed in.
		const alpha = forced ? 1 : labelsOnEl.checked ? Math.min(1, Math.max(0, (sr - 5) / 4)) : 0;
		if (alpha <= 0.02) continue;
		const p = toScreen(engine.x[i], engine.y[i]);
		if (p.x < -100 || p.x > w + 100 || p.y < -40 || p.y > h + 40) continue;
		const text = view.nodes[i].label;
		const tw = labelCtx.measureText(text).width;
		const box = { x0: p.x - tw / 2 - 2, y0: p.y + sr + 2, x1: p.x + tw / 2 + 2, y1: p.y + sr + 17 };
		if (!forced && placed.some((q) => box.x0 < q.x1 && box.x1 > q.x0 && box.y0 < q.y1 && box.y1 > q.y0)) continue;
		placed.push(box);
		labelCtx.globalAlpha = alpha;
		labelCtx.fillStyle = i === focus || i === selected ? '#ffffff' : '#c0caf5';
		labelCtx.fillText(text, p.x, box.y0 + 1);
		drawn++;
	}
	labelCtx.globalAlpha = 1;
}

// ---- interaction --------------------------------------------------------------------------------------------------

function local(e: PointerEvent | WheelEvent | MouseEvent) {
	const r = stage.getBoundingClientRect();
	return { x: e.clientX - r.left, y: e.clientY - r.top };
}

stage.addEventListener('pointerdown', (e) => {
	if (!renderer || e.button !== 0) return;
	const p = local(e);
	const i = pick(p.x, p.y);
	stage.setPointerCapture(e.pointerId);
	if (i >= 0) {
		dragging = { node: i, pointer: e.pointerId, x: p.x, y: p.y, moved: false };
		engine.pin(i);
		engine.setAlphaTarget(0.3);
		engine.reheat(0.3);
		schedule();
	} else {
		panning = { pointer: e.pointerId, x: p.x, y: p.y, moved: false };
		stage.classList.add('panning');
	}
});

stage.addEventListener('pointermove', (e) => {
	if (!renderer) return;
	const p = local(e);
	if (dragging && e.pointerId === dragging.pointer) {
		if (Math.hypot(p.x - dragging.x, p.y - dragging.y) > 3) dragging.moved = true;
		const w = toWorld(p.x, p.y);
		engine.setNodePosition(dragging.node, w.x, w.y);
		positionVersion++;
		schedule();
		return;
	}
	if (panning && e.pointerId === panning.pointer) {
		const dx = p.x - panning.x;
		const dy = p.y - panning.y;
		if (Math.abs(dx) + Math.abs(dy) > 0) {
			if (Math.hypot(dx, dy) > 3) panning.moved = true;
			camera.centerX -= dx / camera.zoom;
			camera.centerY -= dy / camera.zoom;
			panning.x = p.x;
			panning.y = p.y;
			autoFit = false;
			flight = undefined;
			dirty = true;
			schedule();
		}
		return;
	}
	const i = pick(p.x, p.y);
	if (i !== hovered) {
		hovered = i;
		stage.classList.toggle('over-node', i >= 0);
		restyle();
	}
});

function endPointer(e: PointerEvent) {
	if (dragging && e.pointerId === dragging.pointer) {
		const { node, moved } = dragging;
		dragging = undefined;
		engine.release(node);
		engine.setAlphaTarget(0);
		if (!moved) select(node);
		schedule();
	}
	if (panning && e.pointerId === panning.pointer) {
		if (!panning.moved && e.type === 'pointerup') select(-1);
		panning = undefined;
		stage.classList.remove('panning');
	}
}
stage.addEventListener('pointerup', endPointer);
stage.addEventListener('pointercancel', endPointer);
stage.addEventListener('pointerleave', () => {
	if (hovered >= 0 && !dragging) {
		hovered = -1;
		restyle();
	}
});

stage.addEventListener(
	'wheel',
	(e) => {
		e.preventDefault();
		if (!renderer) return;
		const p = local(e);
		const before = toWorld(p.x, p.y);
		const step = e.deltaMode === 1 ? e.deltaY * 16 : e.deltaY;
		camera.zoom = Math.min(12, Math.max(0.02, camera.zoom * Math.exp(-step * (e.ctrlKey ? 0.01 : 0.0015))));
		const { w, h } = size();
		camera.centerX = before.x - (p.x - w / 2) / camera.zoom;
		camera.centerY = before.y - (p.y - h / 2) / camera.zoom;
		autoFit = false;
		flight = undefined;
		dirty = true;
		schedule();
	},
	{ passive: false },
);

stage.addEventListener('dblclick', (e) => {
	const p = local(e);
	const i = pick(p.x, p.y);
	if (i >= 0) flyToNode(i);
});

new ResizeObserver(() => {
	dirty = true;
	schedule();
}).observe(stage);

document.addEventListener('keydown', (e) => {
	const typing = e.target instanceof HTMLInputElement || e.target instanceof HTMLSelectElement;
	if (e.key === '/' && !typing) {
		e.preventDefault();
		searchEl.focus();
		searchEl.select();
	} else if (e.key === 'Escape') {
		if (typing) searchEl.blur();
		setQuery('');
		select(-1);
	} else if ((e.key === 'f' || e.key === 'F') && !typing && renderer) {
		flyTo(fitTarget());
	}
});
$<HTMLButtonElement>('fit').addEventListener('click', () => renderer && flyTo(fitTarget()));

colourByEl.addEventListener('change', () => {
	colourBy = colourByEl.value === 'crate' ? 'crate' : 'kind';
	drawLegend();
	if (selected >= 0) showDetails(selected);
	restyle();
});
labelsOnEl.addEventListener('change', () => {
	dirty = true;
	schedule();
});

for (const [id, key, scale] of FORCES) {
	const input = $<HTMLInputElement>(id);
	input.value = String(Math.round((LAYOUT[key] ?? 0) / scale));
	input.addEventListener('input', () => {
		if (!engine) return;
		engine.setConfig({ [key]: Number(input.value) * scale });
		engine.reheat(0.5);
		schedule();
	});
}

// ---- selection and details ----------------------------------------------------------------------------------------

function select(i: number) {
	selected = i;
	if (i >= 0) showDetails(i);
	else detailsEl.hidden = true;
	restyle();
}

function nodeButton(j: number, extra = '') {
	const b = el('button', { className: 'node-link', type: 'button' }, swatch(colourOf(view, j, colourBy)), view.nodes[j].label);
	if (extra) b.append(el('span', { className: 'count' }, extra));
	b.addEventListener('click', () => {
		select(j);
		flyToNode(j);
	});
	b.addEventListener('pointerenter', () => {
		hovered = j;
		restyle();
	});
	b.addEventListener('pointerleave', () => {
		hovered = -1;
		restyle();
	});
	return b;
}

function swatch(colour: string) {
	const s = el('span', { className: 'swatch' });
	s.style.background = colour;
	return s;
}

function showDetails(i: number) {
	const n = view.nodes[i];
	const body: Node[] = [];
	const row = (k: string, v: string, mono = false) => el('div', { className: 'row' }, el('span', { className: 'k' }, k), el('span', { className: mono ? 'v mono' : 'v' }, v));
	if (n.type === 'symbol') {
		const s = n.node;
		body.push(el('h2', {}, swatch(colourOf(view, i, colourBy)), s.name), el('div', { className: 'kind' }, `${s.kind} in ${s.crate}`));
		if (s.path) body.push(row('path', s.path, true));
		if (s.file) body.push(row('file', s.line ? `${s.file}:${s.line}` : s.file, true));
		if (s.doc) body.push(el('p', { className: 'doc' }, s.doc));
		body.push(row('symbol', s.id, true));
	} else {
		const f = n.fact;
		body.push(el('h2', {}, swatch(colourOf(view, i, colourBy)), `${f.kind} fact`));
		if (f.file) body.push(row('at', f.line ? `${f.file}:${f.line}` : f.file, true));
	}
	body.push(row('links', String(view.degree[i])));

	// Relations grouped by kind and direction, most frequent first.
	const groups = new Map<string, { j: number; count: number }[]>();
	for (let k = view.adjStart[i]; k < view.adjStart[i + 1]; k++) {
		const j = view.adj[k];
		for (const r of view.links[view.adjLink[k]].relations) {
			const out = r.from === i;
			const kind = r.kind.startsWith('role:') ? r.kind.slice(5) : r.kind;
			const title = r.kind.startsWith('role:') ? (out ? kind : `${kind} in`) : out ? `${kind} →` : `← ${kind}`;
			let g = groups.get(title);
			if (!g) groups.set(title, (g = []));
			g.push({ j, count: r.count });
		}
	}
	for (const [title, list] of [...groups].sort((a, b) => b[1].length - a[1].length)) {
		list.sort((a, b) => b.count - a.count || view.nodes[a.j].label.localeCompare(view.nodes[b.j].label));
		const ul = el('ul', { className: 'rel' });
		for (const { j, count } of list.slice(0, 60)) ul.append(el('li', {}, nodeButton(j, count > 1 ? `×${count}` : '')));
		if (list.length > 60) ul.append(el('li', { className: 'more' }, `and ${list.length - 60} more`));
		body.push(el('h3', {}, `${title} (${list.length})`), ul);
	}
	const close = el('button', { className: 'close', type: 'button', title: 'Close (Esc)' }, '×');
	close.addEventListener('click', () => select(-1));
	detailsEl.replaceChildren(close, ...body);
	detailsEl.hidden = false;
	detailsEl.scrollTop = 0;
}

// ---- search -------------------------------------------------------------------------------------------------------

let results: number[] = [];
let active = 0;

function setQuery(q: string) {
	searchEl.value = q;
	runSearch();
}

function runSearch() {
	const q = searchEl.value.trim().toLowerCase();
	if (!q) {
		matches = undefined;
		results = [];
		resultsEl.hidden = true;
		restyle();
		return;
	}
	const scored: { i: number; s: number }[] = [];
	for (const n of view.nodes) {
		if (n.type !== 'symbol') continue;
		const name = n.node.name.toLowerCase();
		const s = name === q ? 4 : name.startsWith(q) ? 3 : name.includes(q) ? 2 : n.search.includes(q) ? 1 : 0;
		if (s) scored.push({ i: n.i, s });
	}
	scored.sort((a, b) => b.s - a.s || view.degree[b.i] - view.degree[a.i]);
	matches = new Set(scored.map((x) => x.i));
	results = scored.slice(0, 12).map((x) => x.i);
	active = 0;
	drawResults(scored.length);
	restyle();
}

function drawResults(total: number) {
	resultsEl.replaceChildren(
		...results.map((i, k) => {
			const n = view.nodes[i];
			const li = el(
				'li',
				{ className: k === active ? 'active' : '' },
				swatch(colourOf(view, i, colourBy)),
				el('span', { className: 'name' }, n.label),
				el('span', { className: 'meta' }, n.type === 'symbol' ? `${n.node.kind} · ${n.node.crate}` : ''),
			);
			li.addEventListener('pointerdown', (e) => {
				e.preventDefault();
				choose(i);
			});
			return li;
		}),
	);
	if (!results.length) resultsEl.append(el('li', { className: 'none' }, 'No symbol matches.'));
	else if (total > results.length) resultsEl.append(el('li', { className: 'none' }, `${total} match; all are lit in the graph.`));
	resultsEl.hidden = false;
}

function choose(i: number) {
	resultsEl.hidden = true;
	searchEl.blur();
	select(i);
	flyToNode(i);
}

searchEl.addEventListener('input', runSearch);
searchEl.addEventListener('focus', () => searchEl.value.trim() && runSearch());
searchEl.addEventListener('blur', () => (resultsEl.hidden = true));
searchEl.addEventListener('keydown', (e) => {
	if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
		e.preventDefault();
		if (!results.length) return;
		active = (active + (e.key === 'ArrowDown' ? 1 : results.length - 1)) % results.length;
		drawResults(matches?.size ?? 0);
	} else if (e.key === 'Enter' && results.length) {
		e.preventDefault();
		choose(results[active]);
	}
});

// ---- loading ------------------------------------------------------------------------------------------------------

function drawLegend() {
	legendEl.replaceChildren(...legendOf(view, colourBy).map(({ label, colour }) => el('li', {}, swatch(colour), label)));
}

function load(g: ViewGraph, name: string) {
	view = g;
	rnodes = view.nodes.map((n) => ({ id: n.type === 'symbol' ? n.node.id : `fact:${n.i}` }));
	rlinks = view.links.map((l) => ({ source: rnodes[l.a], target: rnodes[l.b] }));
	nodeIndex.clear();
	linkIndex.clear();
	rnodes.forEach((n, i) => nodeIndex.set(n, i));
	rlinks.forEach((l, i) => linkIndex.set(l, i));
	radii = Float32Array.from(view.nodes, (_n, i) => radiusOf(view, i));
	engine = createGraphLayoutEngine(
		{ nodeIds: rnodes.map((n) => n.id), radii, edgeSources: view.edgeSources, edgeTargets: view.edgeTargets },
		LAYOUT,
	);
	for (const [id, key, scale] of FORCES) engine.setConfig({ [key]: Number($<HTMLInputElement>(id).value) * scale });
	hovered = selected = -1;
	matches = undefined;
	searchEl.value = '';
	detailsEl.hidden = true;
	autoFit = true;
	flight = undefined;
	camera.centerX = camera.centerY = 0;
	camera.zoom = 1;
	const src = view.source;
	const tree = src.tree?.commit ? ` · tree ${src.tree.commit}${src.tree.dirty ? ' (dirty)' : ''}` : '';
	statsEl.textContent = `${name}: ${src.nodes.length} symbols · ${src.facts.length} facts · ${view.links.length} links${tree}`;
	drawLegend();
	positionVersion++;
	restyle();
}

/** Parses a graph file's text, or shows why it can't. */
function parse(text: string, name: string): ViewGraph | undefined {
	try {
		return buildView(parseGraph(JSON.parse(text)));
	} catch (e) {
		const why = e instanceof GraphFileError ? `${e.at}: ${e.rule}` : e instanceof Error ? e.message : String(e);
		showMessage(`Can't read ${name}`, `${why}. The format is site/graph/schema.ts (blaze-graph, version 1).`);
		return undefined;
	}
}

async function fetchText(url: string): Promise<string> {
	const res = await fetch(url);
	if (!res.ok) throw new Error(`${url}: HTTP ${res.status}`);
	return res.text();
}

window.addEventListener('dragover', (e) => {
	e.preventDefault();
	document.body.classList.add('dropping');
});
window.addEventListener('dragleave', () => document.body.classList.remove('dropping'));
window.addEventListener('drop', async (e) => {
	e.preventDefault();
	document.body.classList.remove('dropping');
	const file = e.dataTransfer?.files[0];
	if (!file || !renderer) return;
	const g = parse(await file.text(), file.name);
	if (g) {
		messageEl.hidden = true;
		load(g, file.name);
	}
});

async function main() {
	if (!('gpu' in navigator)) {
		showMessage(
			'This page needs WebGPU',
			"It draws the graph with WebGPU, and this browser doesn't offer it (navigator.gpu is missing). Open it in a browser with WebGPU on: Chrome or Edge 113+, Safari 26+, or Firefox 141+ on Windows. There is no other renderer.",
		);
		return;
	}
	const src = new URLSearchParams(location.search).get('src') ?? 'graph/sample.json';
	let text: string;
	let wasm: ArrayBuffer;
	try {
		[text, wasm] = await Promise.all([fetchText(src), fetch('graph/physics.wasm').then((r) => (r.ok ? r.arrayBuffer() : Promise.reject(new Error(`graph/physics.wasm: HTTP ${r.status}`))))]);
		await prepareGraphPhysicsFromBytes(wasm);
	} catch (e) {
		showMessage("Can't load the graph", e instanceof Error ? e.message : String(e));
		return;
	}
	renderer = await WebGpuGraphRenderer.create(gpuCanvas, {
		onDeviceLost: (m) => stop('The GPU device was lost', `${m}. Reload the page to draw again.`),
		onRendererError: (m) => stop('WebGPU failed', m),
		onFrameComplete: () => {},
	});
	if (!renderer) {
		showMessage(
			'This page needs WebGPU',
			'This browser has WebGPU but gave no GPU adapter or canvas context (it may be turned off for this GPU, or by policy). There is no other renderer.',
		);
		return;
	}
	const g = parse(text, src);
	if (g) load(g, src.split('/').pop() ?? src);
}

function stop(title: string, detail: string) {
	renderer?.dispose();
	renderer = undefined;
	showMessage(title, detail);
}

void main();
