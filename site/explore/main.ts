import {
	LiveClient,
	resolveViewerOrigin,
	type HookKindName,
	type RunSummary,
	type ServerMessage,
	type TelemetryEvent,
} from './protocol';

interface SpanAgg {
	slot: number;
	calls: number;
	totalNs: number;
	maxNs: number;
	unwound: number;
}

interface HeapSite {
	stack: string;
	allocs: number;
	allocatedBytes: number;
	retainedBytes: number;
}

interface RunState {
	summary: RunSummary;
	spans: Map<number, SpanAgg>;
	heapSites: Map<string, HeapSite>;
	livePtrs: Map<number, { site: string; bytes: number }>;
	totalAllocBytes: number;
	totalFreeBytes: number;
	liveBytes: number;
	ioLog: TelemetryEvent[];
}

const IO_OP_NAMES: Record<number, string> = {
	1: 'open',
	2: 'close',
	3: 'read',
	4: 'write',
	5: 'connect',
	6: 'getenv',
	7: 'pthread_mutex_lock',
};

const ALLOC_OP_NAMES: Record<number, string> = {
	1: 'malloc',
	2: 'calloc',
	3: 'realloc',
	4: 'free',
	5: 'posix_memalign',
};

const runs = new Map<string, RunState>();
let selectedRun: string | null = null;
let paused = false;
let dirty = false;

const connDot = document.getElementById('conn-dot') as HTMLSpanElement;
const connLabel = document.getElementById('conn-label') as HTMLSpanElement;
const runList = document.getElementById('run-list') as HTMLDivElement;
const summaryBar = document.getElementById('summary-bar') as HTMLElement;
const spansBody = document.getElementById('spans-body') as HTMLTableSectionElement;
const heapCards = document.getElementById('heap-cards') as HTMLDivElement;
const heapBody = document.getElementById('heap-body') as HTMLTableSectionElement;
const ioBody = document.getElementById('io-body') as HTMLTableSectionElement;

function ensureRun(summary: RunSummary): RunState {
	let st = runs.get(summary.id);
	if (!st) {
		st = {
			summary,
			spans: new Map(),
			heapSites: new Map(),
			livePtrs: new Map(),
			totalAllocBytes: 0,
			totalFreeBytes: 0,
			liveBytes: 0,
			ioLog: [],
		};
		runs.set(summary.id, st);
	} else {
		st.summary = summary;
	}
	return st;
}

function fmtNs(ns: number): string {
	if (ns >= 1e9) return `${(ns / 1e9).toFixed(2)} s`;
	if (ns >= 1e6) return `${(ns / 1e6).toFixed(2)} ms`;
	if (ns >= 1e3) return `${(ns / 1e3).toFixed(1)} us`;
	return `${ns} ns`;
}

function fmtBytes(b: number): string {
	if (b >= 1 << 30) return `${(b / (1 << 30)).toFixed(2)} GiB`;
	if (b >= 1 << 20) return `${(b / (1 << 20)).toFixed(2)} MiB`;
	if (b >= 1 << 10) return `${(b / (1 << 10)).toFixed(1)} KiB`;
	return `${b} B`;
}

function fmtStack(ev: TelemetryEvent): string {
	const frames = [ev.c, ev.d, ev.e].filter((x) => x !== 0).map((x) => `0x${x.toString(16)}`);
	const op = ALLOC_OP_NAMES[ev.slot_or_op] ?? 'alloc';
	return frames.length > 0 ? `${op} @ ${frames.join(' <- ')}` : `${op} @ (unknown)`;
}

function ingest(ev: TelemetryEvent): void {
	const st = runs.get(ev.run);
	if (!st) return;

	if (ev.kind === 'span_exit' || ev.kind === 'span_unwound') {
		let agg = st.spans.get(ev.slot_or_op);
		if (!agg) {
			agg = { slot: ev.slot_or_op, calls: 0, totalNs: 0, maxNs: 0, unwound: 0 };
			st.spans.set(ev.slot_or_op, agg);
		}
		agg.calls += 1;
		agg.totalNs += ev.a;
		if (ev.a > agg.maxNs) agg.maxNs = ev.a;
		if (ev.kind === 'span_unwound') agg.unwound += 1;
	} else if (ev.kind === 'hook_hit') {
		let agg = st.spans.get(ev.slot_or_op);
		if (!agg) {
			agg = { slot: ev.slot_or_op, calls: 0, totalNs: 0, maxNs: 0, unwound: 0 };
			st.spans.set(ev.slot_or_op, agg);
		}
		agg.calls += 1;
	} else if (ev.kind === 'alloc') {
		const stack = fmtStack(ev);
		let site = st.heapSites.get(stack);
		if (!site) {
			site = { stack, allocs: 0, allocatedBytes: 0, retainedBytes: 0 };
			st.heapSites.set(stack, site);
		}
		site.allocs += 1;
		site.allocatedBytes += ev.b;
		site.retainedBytes += ev.b;
		st.totalAllocBytes += ev.b;
		st.liveBytes += ev.b;
		if (ev.a !== 0) {
			const prev = st.livePtrs.get(ev.a);
			if (prev) {
				const prevSite = st.heapSites.get(prev.site);
				if (prevSite) prevSite.retainedBytes = Math.max(0, prevSite.retainedBytes - prev.bytes);
				st.liveBytes = Math.max(0, st.liveBytes - prev.bytes);
			}
			st.livePtrs.set(ev.a, { site: stack, bytes: ev.b });
		}
	} else if (ev.kind === 'free') {
		const prev = st.livePtrs.get(ev.a);
		if (prev) {
			st.livePtrs.delete(ev.a);
			st.totalFreeBytes += prev.bytes;
			st.liveBytes = Math.max(0, st.liveBytes - prev.bytes);
			const site = st.heapSites.get(prev.site);
			if (site) site.retainedBytes = Math.max(0, site.retainedBytes - prev.bytes);
		}
	} else if (ev.kind === 'io' || ev.kind === 'sync_wait' || ev.kind === 'thread_birth' || ev.kind === 'thread_end') {
		st.ioLog.push(ev);
		if (st.ioLog.length > 400) st.ioLog.splice(0, st.ioLog.length - 400);
	}
}

function handleMessage(msg: ServerMessage): void {
	if (msg.type === 'runs') {
		for (const r of msg.runs) ensureRun(r);
		if (!selectedRun && msg.runs.length > 0) selectedRun = msg.runs[0].id;
	} else if (msg.type === 'run_started') {
		ensureRun(msg.run);
		selectedRun = msg.run.id;
	} else if (msg.type === 'run_ended') {
		const st = runs.get(msg.id);
		if (st) st.summary.alive = false;
	} else if (msg.type === 'events' && !paused) {
		for (const ev of msg.events) ingest(ev);
	}
	dirty = true;
}

const origin = resolveViewerOrigin(window.location.search);
const client = new LiveClient(origin, {
	onStatus(transport, detail) {
		connDot.className = transport === 'disconnected' ? 'dot' : 'dot ok';
		connLabel.textContent = detail;
	},
	onMessage: handleMessage,
});

// Coordinate across tabs via BroadcastChannel so multiple tabs stay aware of one another.
if ('BroadcastChannel' in window) {
	const bc = new BroadcastChannel('blaze-explore');
	bc.postMessage({ type: 'hello', origin });
}

function render(): void {
	if (!dirty) return;
	dirty = false;

	runList.textContent = '';
	if (runs.size === 0) {
		const empty = document.createElement('div');
		empty.className = 'empty';
		empty.textContent = 'Waiting for `blaze run`…';
		runList.appendChild(empty);
	} else {
		for (const st of runs.values()) {
			const el = document.createElement('div');
			el.className = st.summary.id === selectedRun ? 'run-item selected' : 'run-item';
			el.onclick = () => {
				selectedRun = st.summary.id;
				dirty = true;
			};
			const title = document.createElement('div');
			title.className = 'run-title';
			const pkg = document.createElement('span');
			pkg.textContent = st.summary.package;
			const badge = document.createElement('span');
			badge.className = st.summary.alive ? 'badge' : 'badge dead';
			badge.textContent = st.summary.alive ? `pid ${st.summary.pid}` : 'exited';
			title.append(pkg, badge);
			const meta = document.createElement('div');
			meta.className = 'run-meta';
			meta.textContent = `events ${st.summary.stats.pushed} · dropped ${st.summary.stats.dropped}`;
			el.append(title, meta);
			runList.appendChild(el);
		}
	}

	const cur = selectedRun ? runs.get(selectedRun) : undefined;
	if (!cur) return;

	summaryBar.textContent = '';
	const s = cur.summary.stats;
	const chips: Array<[string, string]> = [
		['PID', String(cur.summary.pid)],
		['Events', String(s.pushed)],
		['Dropped', String(s.dropped)],
		['Shadow Overflows', String(s.shadow_overflows)],
		['Unwound Spans', String(s.unwound_spans)],
		['Live Heap', fmtBytes(cur.liveBytes)],
	];
	for (const [k, v] of chips) {
		const span = document.createElement('span');
		span.textContent = `${k}: `;
		const strong = document.createElement('strong');
		strong.textContent = v;
		span.appendChild(strong);
		summaryBar.appendChild(span);
	}

	// Spans table
	spansBody.textContent = '';
	const sortedSpans = [...cur.spans.values()].sort((a, b) => b.totalNs - a.totalNs || b.calls - a.calls);
	for (const row of sortedSpans.slice(0, 100)) {
		const tr = document.createElement('tr');
		const name = cur.summary.slots[String(row.slot)] ?? `slot #${row.slot}`;
		const avg = row.calls > 0 ? Math.round(row.totalNs / row.calls) : 0;
		const cells = [name, String(row.calls), fmtNs(row.totalNs), fmtNs(avg), fmtNs(row.maxNs)];
		cells.forEach((text, i) => {
			const td = document.createElement('td');
			if (i > 0) td.className = 'num';
			td.textContent = text;
			tr.appendChild(td);
		});
		const actions = document.createElement('td');
		const wrap = document.createElement('div');
		wrap.className = 'hook-btns';
		for (const kind of ['plain', 'hook', 'span'] as HookKindName[]) {
			const btn = document.createElement('button');
			btn.type = 'button';
			btn.textContent = kind;
			btn.onclick = () => void client.toggleHook(cur.summary.id, row.slot, kind);
			wrap.appendChild(btn);
		}
		actions.appendChild(wrap);
		tr.appendChild(actions);
		spansBody.appendChild(tr);
	}

	// Heap cards & sites
	heapCards.textContent = '';
	for (const [label, value] of [
		['Total Allocated', fmtBytes(cur.totalAllocBytes)],
		['Total Freed', fmtBytes(cur.totalFreeBytes)],
		['Estimated Live Heap', fmtBytes(cur.liveBytes)],
		['Live Pointers Tracked', String(cur.livePtrs.size)],
	]) {
		const card = document.createElement('div');
		card.className = 'card';
		const l = document.createElement('div');
		l.className = 'label';
		l.textContent = label;
		const v = document.createElement('div');
		v.className = 'value';
		v.textContent = value;
		card.append(l, v);
		heapCards.appendChild(card);
	}

	heapBody.textContent = '';
	const sortedHeap = [...cur.heapSites.values()].sort((a, b) => b.allocatedBytes - a.allocatedBytes);
	const maxAlloc = sortedHeap[0]?.allocatedBytes || 1;
	for (const site of sortedHeap.slice(0, 100)) {
		const tr = document.createElement('tr');
		for (const [text, isNum] of [
			[site.stack, false],
			[String(site.allocs), true],
			[fmtBytes(site.allocatedBytes), true],
			[fmtBytes(site.retainedBytes), true],
		] as const) {
			const td = document.createElement('td');
			if (isNum) td.className = 'num';
			td.textContent = text;
			tr.appendChild(td);
		}
		const shareTd = document.createElement('td');
		const bar = document.createElement('div');
		bar.className = 'bar-wrap';
		const fill = document.createElement('div');
		fill.className = 'bar-fill';
		fill.style.width = `${Math.max(2, Math.round((site.allocatedBytes / maxAlloc) * 100))}%`;
		bar.appendChild(fill);
		shareTd.appendChild(bar);
		tr.appendChild(shareTd);
		heapBody.appendChild(tr);
	}

	// I/O, Locks & Threads timeline
	ioBody.textContent = '';
	for (const ev of cur.ioLog.slice(-150).reverse()) {
		const tr = document.createElement('tr');
		const op =
			ev.kind === 'thread_birth'
				? 'thread_birth'
				: ev.kind === 'thread_end'
					? 'thread_end'
					: (IO_OP_NAMES[ev.slot_or_op] ?? `${ev.kind}#${ev.slot_or_op}`);
		const detail =
			ev.kind === 'io' || ev.kind === 'sync_wait'
				? `arg=0x${ev.b.toString(16)} ret=${ev.c} lr=0x${ev.d.toString(16)}`
				: '';
		for (const [text, isNum] of [
			[fmtNs(ev.ts_ns), false],
			[op, false],
			[String(ev.thread), true],
			[ev.kind === 'io' || ev.kind === 'sync_wait' ? fmtNs(ev.a) : '-', true],
			[detail, false],
		] as const) {
			const td = document.createElement('td');
			if (isNum) td.className = 'num';
			td.textContent = text;
			tr.appendChild(td);
		}
		ioBody.appendChild(tr);
	}
}

// Tab switching & controls
for (const btn of document.querySelectorAll<HTMLButtonElement>('.tab')) {
	btn.addEventListener('click', () => {
		const target = btn.dataset.tab;
		for (const b of document.querySelectorAll('.tab')) b.classList.toggle('active', b === btn);
		for (const p of document.querySelectorAll('.panel')) p.classList.toggle('active', p.id === `panel-${target}`);
	});
}

document.getElementById('btn-pause')?.addEventListener('click', (e) => {
	paused = !paused;
	(e.currentTarget as HTMLButtonElement).textContent = paused ? 'Resume' : 'Pause';
});

document.getElementById('btn-clear')?.addEventListener('click', () => {
	if (!selectedRun) return;
	const cur = runs.get(selectedRun);
	if (!cur) return;
	cur.spans.clear();
	cur.heapSites.clear();
	cur.livePtrs.clear();
	cur.totalAllocBytes = 0;
	cur.totalFreeBytes = 0;
	cur.liveBytes = 0;
	cur.ioLog.length = 0;
	dirty = true;
});

function tick(): void {
	render();
	requestAnimationFrame(tick);
}

client.start();
requestAnimationFrame(tick);
