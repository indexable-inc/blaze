import { analyzeInsights, type HeapSiteMetrics, type IoRowMetrics, type SpanMetrics } from './advisor';
import { FlameGraphView, type FlameNode } from './flamegraph';
import { fmtSpan, GpuTimeline } from './gpu_timeline';
import {
	LiveClient,
	resolveViewerOrigin,
	type BuildSummary,
	type HookKindName,
	type RunSummary,
	type ServerMessage,
	type TelemetryEvent,
} from './protocol';

interface SpanRecord {
	tsMs: number;
	slot: number;
	spanId: number;
	parentId: number;
	durationNs: number;
	thread: number;
}

interface SpanAgg {
	calls: number;
	totalNs: number;
	childNs: number;
	maxNs: number;
	samples: number[]; // bounded reservoir for P50/P95/P99
	hook: HookKindName;
}

interface HeapSite {
	allocs: number;
	frees: number;
	totalBytes: number;
	retainedBytes: number;
}

interface RunState {
	summary: RunSummary;
	spans: Map<number, SpanAgg>;
	spanHistory: SpanRecord[]; // bounded recent span completions for FlameGraph + range filter
	spanSlotById: Map<number, { slot: number; parentId: number }>;
	livePtrs: Map<number, { site: string; bytes: number }>;
	heapSites: Map<string, HeapSite>;
	heapBaseline: Map<string, number> | null;
	heapBaselineAt: string | null;
	ioRows: TelemetryEvent[];
	totalEvents: number;
}

const MAX_SPAN_HISTORY = 4000;
const MAX_PCT_SAMPLES = 256;

const runs = new Map<string, RunState>();
let builds: BuildSummary[] = [];
const buildSlots = new Map<string, Record<string, string>>();
let selectedRunId: string | null = null;
let selectedSlot: number | null = null;
let activeTab: 'flame' | 'spans' | 'heap' | 'builds' | 'advisor' | 'io' = 'flame';
let paused = false;

const connDot = document.getElementById('conn-dot')!;
const connLabel = document.getElementById('conn-label')!;
const gpuChip = document.getElementById('gpu-chip')!;
const procSelect = document.getElementById('proc-select') as HTMLSelectElement;
const inlineMetricsEl = document.getElementById('inline-metrics')!;
const buildsViewEl = document.getElementById('builds-view')!;
const advisorViewEl = document.getElementById('advisor-view')!;
const badgeAdvisorEl = document.getElementById('badge-advisor')!;
const spansBodyEl = document.getElementById('spans-body')!;
const heapBodyEl = document.getElementById('heap-body')!;
const ioBodyEl = document.getElementById('io-body')!;
const btnZoomIn = document.getElementById('btn-zoom-in') as HTMLButtonElement;
const btnZoomOut = document.getElementById('btn-zoom-out') as HTMLButtonElement;
const btnPause = document.getElementById('btn-pause') as HTMLButtonElement;
const pauseLabel = document.getElementById('pause-label')!;
const btnClear = document.getElementById('btn-clear') as HTMLButtonElement;
const gpuCanvas = document.getElementById('gpu-canvas') as HTMLCanvasElement;
const gpuLabels = document.getElementById('gpu-labels') as HTMLCanvasElement;
const gpuRuler = document.getElementById('gpu-ruler')!;
const gpuTooltip = document.getElementById('gpu-tooltip')!;
const gpuSpanLabel = document.getElementById('gpu-span-label')!;
const btnFollow = document.getElementById('btn-follow') as HTMLButtonElement;
const selPill = document.getElementById('sel-pill')!;
const selLabel = document.getElementById('sel-label')!;
const btnClearSel = document.getElementById('btn-clear-sel') as HTMLButtonElement;
const traceHero = document.getElementById('trace-hero')!;
const splitter = document.getElementById('splitter')!;
const workspace = document.getElementById('workspace')!;

// Flame Graph controls
const flameCanvas = document.getElementById('flame-canvas') as HTMLCanvasElement;
const flameTooltip = document.getElementById('flame-tooltip')!;
const fgTopDown = document.getElementById('fg-topdown') as HTMLButtonElement;
const fgBottomUp = document.getElementById('fg-bottomup') as HTMLButtonElement;
const fgSearch = document.getElementById('fg-search') as HTMLInputElement;
const fgStatus = document.getElementById('fg-status')!;
const fgReset = document.getElementById('fg-reset') as HTMLButtonElement;

// Heap snapshot controls
const btnHeapSnap = document.getElementById('btn-heap-snap') as HTMLButtonElement;
const btnHeapClearSnap = document.getElementById('btn-heap-clear-snap') as HTMLButtonElement;
const heapSnapLabel = document.getElementById('heap-snap-label')!;

const timeline = new GpuTimeline(
	gpuCanvas,
	gpuLabels,
	gpuRuler,
	gpuTooltip,
	(kind) => {
		gpuChip.textContent = kind === 'webgpu' ? 'WebGPU' : '2D';
		gpuChip.classList.toggle('gpu', kind === 'webgpu');
	},
	({ spanMs, following, selectionMs }) => {
		gpuSpanLabel.textContent = fmtSpan(spanMs);
		btnFollow.hidden = following;
		if (selectionMs) {
			selPill.hidden = false;
			selLabel.textContent = `Range ${fmtSpan(selectionMs[1] - selectionMs[0])}`;
		} else {
			selPill.hidden = true;
		}
		scheduleRender();
	},
	(sample) => {
		// Click a bar in the trace to jump to its flame / hotspot detail
		const st = selectedRunId ? runs.get(selectedRunId) ?? null : null;
		if (!st) return;
		if (sample.trackKey.startsWith('t:')) {
			for (const [slot] of st.spans.entries()) {
				if (slotName(st, slot) === sample.label) {
					selectedSlot = slot;
					render();
					break;
				}
			}
		} else if (sample.trackKey === 'heap') {
			switchTab('heap');
		} else if (sample.trackKey === 'io') {
			switchTab('io');
		}
	},
);

const flameView = new FlameGraphView(
	flameCanvas,
	flameTooltip,
	({ matchedPct, zoomedName }) => {
		const parts: string[] = [];
		if (zoomedName) parts.push(`zoom: ${zoomedName}`);
		if (matchedPct !== null) parts.push(`matched ${matchedPct.toFixed(1)}%`);
		fgStatus.textContent = parts.join(' · ');
		fgReset.hidden = !zoomedName;
	},
	(slot) => {
		selectedSlot = slot;
	},
);

btnFollow.onclick = () => timeline.followLive();
btnZoomIn.onclick = () => timeline.zoomBy(0.55);
btnZoomOut.onclick = () => timeline.zoomBy(1.8);
btnClearSel.onclick = () => timeline.clearSelection();

fgTopDown.onclick = () => {
	fgTopDown.classList.add('active');
	fgBottomUp.classList.remove('active');
	flameView.setInverted(false);
};
fgBottomUp.onclick = () => {
	fgBottomUp.classList.add('active');
	fgTopDown.classList.remove('active');
	flameView.setInverted(true);
};
fgSearch.oninput = () => flameView.setQuery(fgSearch.value);
fgReset.onclick = () => flameView.resetZoom();

btnHeapSnap.onclick = () => {
	const st = selectedRunId ? runs.get(selectedRunId) ?? null : null;
	if (!st) return;
	const snap = new Map<string, number>();
	for (const [k, v] of st.heapSites.entries()) snap.set(k, v.retainedBytes);
	st.heapBaseline = snap;
	st.heapBaselineAt = new Date().toLocaleTimeString();
	render();
};
btnHeapClearSnap.onclick = () => {
	const st = selectedRunId ? runs.get(selectedRunId) ?? null : null;
	if (!st) return;
	st.heapBaseline = null;
	st.heapBaselineAt = null;
	render();
};

// Draggable splitter
let resizing = false;
splitter.addEventListener('pointerdown', (e) => {
	resizing = true;
	splitter.classList.add('active');
	splitter.setPointerCapture(e.pointerId);
});
splitter.addEventListener('pointermove', (e) => {
	if (!resizing) return;
	const rect = workspace.getBoundingClientRect();
	const y = e.clientY - rect.top;
	const pct = Math.max(22, Math.min(82, (y / rect.height) * 100));
	traceHero.style.flexBasis = `${pct.toFixed(1)}%`;
	timeline.render();
	if (activeTab === 'flame') flameView.render();
});
const stopResize = () => {
	if (!resizing) return;
	resizing = false;
	splitter.classList.remove('active');
};
splitter.addEventListener('pointerup', stopResize);
splitter.addEventListener('pointercancel', stopResize);

const WF_SERIES = ['var(--series-1)', 'var(--series-2)', 'var(--series-3)', 'var(--series-4)'];
const IO_OP_NAMES: Record<number, string> = {
	1: 'open',
	2: 'close',
	3: 'read',
	4: 'write',
	5: 'connect',
	6: 'getenv',
	7: 'mutex_lock',
};

function fmtNs(ns: number): string {
	if (ns < 1_000) return `${ns.toFixed(0)} ns`;
	if (ns < 1_000_000) return `${(ns / 1_000).toFixed(1)} µs`;
	if (ns < 1_000_000_000) return `${(ns / 1_000_000).toFixed(2)} ms`;
	return `${(ns / 1_000_000_000).toFixed(2)} s`;
}

function fmtBytes(n: number): string {
	const sign = n < 0 ? '-' : '';
	const abs = Math.abs(n);
	if (abs < 1024) return `${sign}${abs} B`;
	if (abs < 1024 * 1024) return `${sign}${(abs / 1024).toFixed(1)} KiB`;
	return `${sign}${(abs / (1024 * 1024)).toFixed(2)} MiB`;
}

function fmtHex(n: number): string {
	return n === 0 ? '—' : `0x${n.toString(16)}`;
}

function percentile(samples: number[], p: number): number {
	if (samples.length === 0) return 0;
	const sorted = [...samples].sort((a, b) => a - b);
	const idx = Math.min(sorted.length - 1, Math.floor(p * (sorted.length - 1)));
	return sorted[idx];
}

function escapeHtml(s: string): string {
	return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

function slotName(st: RunState, slot: number): string {
	const key = String(slot);
	return st.summary.slots[key] ?? buildSlots.get(st.summary.package)?.[key] ?? `slot #${slot}`;
}

function switchTab(tab: 'flame' | 'spans' | 'heap' | 'builds' | 'advisor' | 'io'): void {
	activeTab = tab;
	document.querySelectorAll<HTMLButtonElement>('.tab').forEach((b) => {
		b.classList.toggle('active', b.dataset.tab === tab);
	});
	document.querySelectorAll('.panel').forEach((p) => p.classList.remove('active'));
	document.getElementById(`panel-${tab}`)?.classList.add('active');
	render();
}

function ensureRun(summary: RunSummary): RunState {
	let st = runs.get(summary.id);
	if (!st) {
		st = {
			summary,
			spans: new Map(),
			spanHistory: [],
			spanSlotById: new Map(),
			livePtrs: new Map(),
			heapSites: new Map(),
			heapBaseline: null,
			heapBaselineAt: null,
			ioRows: [],
			totalEvents: 0,
		};
		runs.set(summary.id, st);
		if (!selectedRunId) selectedRunId = summary.id;
	} else {
		st.summary = { ...st.summary, ...summary, slots: { ...st.summary.slots, ...summary.slots } };
	}
	return st;
}

function ingestEvent(ev: TelemetryEvent): void {
	let st = runs.get(ev.run);
	if (!st) {
		st = ensureRun({
			id: ev.run,
			pid: 0,
			package: ev.run,
			alive: true,
			started_ms: Date.now(),
			stats: { cap: 0, pushed: 0, popped: 0, dropped: 0, shadow_overflows: 0, unwound_spans: 0 },
			slots: {},
		});
	}
	st.totalEvents += 1;
	const nowMs = Date.now();

	if (ev.kind === 'span_exit' || ev.kind === 'span_unwound') {
		let agg = st.spans.get(ev.slot_or_op);
		if (!agg) {
			agg = { calls: 0, totalNs: 0, childNs: 0, maxNs: 0, samples: [], hook: 'span' };
			st.spans.set(ev.slot_or_op, agg);
		}
		agg.calls += 1;
		agg.totalNs += ev.a;
		if (ev.a > agg.maxNs) agg.maxNs = ev.a;
		if (agg.samples.length < MAX_PCT_SAMPLES) {
			agg.samples.push(ev.a);
		} else {
			agg.samples[agg.calls % MAX_PCT_SAMPLES] = ev.a;
		}

		// Track parent->child hierarchy for Flame Graph & exact Self Time
		const spanId = ev.b;
		const parentId = ev.c;
		if (spanId !== 0) {
			st.spanSlotById.set(spanId, { slot: ev.slot_or_op, parentId });
			if (st.spanSlotById.size > 6000) {
				const first = st.spanSlotById.keys().next().value;
				if (first !== undefined) st.spanSlotById.delete(first);
			}
		}
		if (parentId !== 0) {
			const parentInfo = st.spanSlotById.get(parentId);
			if (parentInfo) {
				const pAgg = st.spans.get(parentInfo.slot);
				if (pAgg) pAgg.childNs += ev.a;
			}
		}

		st.spanHistory.push({
			tsMs: nowMs,
			slot: ev.slot_or_op,
			spanId,
			parentId,
			durationNs: ev.a,
			thread: ev.thread,
		});
		if (st.spanHistory.length > MAX_SPAN_HISTORY) st.spanHistory.shift();

		const name = slotName(st, ev.slot_or_op);
		const durMs = Math.max(0.25, ev.a / 1_000_000);
		const hot = ev.a >= 100_000_000 || (parentId === 0 && ev.a >= 80_000_000);
		const depth = parentId !== 0 ? 1 : 0;
		timeline.push({
			startMs: nowMs - durMs,
			endMs: nowMs,
			trackKey: `t:${ev.thread}`,
			depth,
			color: hot ? [0.85, 0.28, 0.11, 0.92] : depth > 0 ? [0.16, 0.49, 0.82, 0.88] : [0.11, 0.56, 0.41, 0.88],
			label: name,
			durationText: fmtNs(ev.a),
			detail: `${fmtNs(ev.a)} · t#${ev.thread}`,
		});
	} else if (ev.kind === 'hook_hit') {
		let agg = st.spans.get(ev.slot_or_op);
		if (!agg) {
			agg = { calls: 0, totalNs: 0, childNs: 0, maxNs: 0, samples: [], hook: 'hook' };
			st.spans.set(ev.slot_or_op, agg);
		}
		agg.calls += 1;
	} else if (ev.kind === 'alloc') {
		const siteKey = `${fmtHex(ev.c)} → ${fmtHex(ev.d)} → ${fmtHex(ev.e)}`;
		let site = st.heapSites.get(siteKey);
		if (!site) {
			site = { allocs: 0, frees: 0, totalBytes: 0, retainedBytes: 0 };
			st.heapSites.set(siteKey, site);
		}
		site.allocs += 1;
		site.totalBytes += ev.b;
		site.retainedBytes += ev.b;
		if (ev.a !== 0) st.livePtrs.set(ev.a, { site: siteKey, bytes: ev.b });
		timeline.push({
			startMs: nowMs - 14,
			endMs: nowMs,
			trackKey: 'heap',
			depth: 0,
			color: [0.22, 0.52, 0.88, 0.88],
			label: fmtBytes(ev.b),
			durationText: '',
			detail: `alloc ${fmtBytes(ev.b)} · ${fmtHex(ev.c)} · t#${ev.thread}`,
		});
	} else if (ev.kind === 'free') {
		const live = st.livePtrs.get(ev.a);
		if (live) {
			const site = st.heapSites.get(live.site);
			if (site) {
				site.frees += 1;
				site.retainedBytes = Math.max(0, site.retainedBytes - (ev.b > 0 ? ev.b : live.bytes));
			}
			st.livePtrs.delete(ev.a);
		}
	} else if (
		ev.kind === 'io' ||
		ev.kind === 'sync_wait' ||
		ev.kind === 'thread_birth' ||
		ev.kind === 'thread_end'
	) {
		st.ioRows.push(ev);
		if (st.ioRows.length > 300) st.ioRows.shift();
		const durMs = Math.max(10, ev.a / 1_000_000);
		const opLabel = IO_OP_NAMES[ev.slot_or_op] ?? ev.kind;
		timeline.push({
			startMs: nowMs - durMs,
			endMs: nowMs,
			trackKey: 'io',
			depth: 0,
			color: ev.kind === 'sync_wait' ? [0.95, 0.38, 0.35, 0.92] : [0.85, 0.58, 0.19, 0.88],
			label: opLabel,
			durationText: fmtNs(ev.a),
			detail: `${fmtNs(ev.a)} · t#${ev.thread}`,
		});
	}
}

function buildFlameTree(st: RunState, sel: [number, number] | null): FlameNode {
	const root: FlameNode = {
		name: `all (${st.summary.package})`,
		slot: -1,
		totalNs: 0,
		selfNs: 0,
		calls: 0,
		maxNs: 0,
		children: new Map(),
	};
	const records = sel
		? st.spanHistory.filter((r) => r.tsMs >= sel[0] && r.tsMs <= sel[1])
		: st.spanHistory;

	const childSumBySpan = new Map<number, number>();
	for (const r of records) {
		if (r.parentId !== 0) {
			childSumBySpan.set(r.parentId, (childSumBySpan.get(r.parentId) ?? 0) + r.durationNs);
		}
	}

	for (const r of records) {
		// Reconstruct stack path root -> ... -> r (bounded to depth 12)
		const chain: Array<{ slot: number; name: string }> = [{ slot: r.slot, name: slotName(st, r.slot) }];
		let pid = r.parentId;
		for (let d = 0; d < 12 && pid !== 0; d++) {
			const info = st.spanSlotById.get(pid);
			if (!info) break;
			chain.push({ slot: info.slot, name: slotName(st, info.slot) });
			pid = info.parentId;
		}
		chain.reverse();

		const selfNs = Math.max(0, r.durationNs - (childSumBySpan.get(r.spanId) ?? 0));
		if (chain.length === 1) {
			root.totalNs += r.durationNs;
			root.calls += 1;
		}

		let cur = root;
		for (let i = 0; i < chain.length; i++) {
			const seg = chain[i];
			let next = cur.children.get(seg.name);
			if (!next) {
				next = {
					name: seg.name,
					slot: seg.slot,
					totalNs: 0,
					selfNs: 0,
					calls: 0,
					maxNs: 0,
					children: new Map(),
				};
				cur.children.set(seg.name, next);
			}
			if (i === chain.length - 1) {
				next.totalNs += r.durationNs;
				next.selfNs += selfNs;
				next.calls += 1;
				if (r.durationNs > next.maxNs) next.maxNs = r.durationNs;
			}
			cur = next;
		}
	}
	// Ensure root total covers children even if a parent wasn't seen in a sliced selection window.
	let childTotal = 0;
	for (const ch of root.children.values()) childTotal += ch.totalNs;
	if (childTotal > root.totalNs) root.totalNs = childTotal;
	return root;
}

let renderScheduled = false;
function scheduleRender(): void {
	if (renderScheduled) return;
	renderScheduled = true;
	requestAnimationFrame(() => {
		renderScheduled = false;
		render();
	});
}

function collectCurrentMetrics(st: RunState | null): {
	spans: SpanMetrics[];
	heapSites: HeapSiteMetrics[];
	ioRows: IoRowMetrics[];
} {
	if (!st) return { spans: [], heapSites: [], ioRows: [] };
	const spans: SpanMetrics[] = [];
	for (const [slot, agg] of st.spans.entries()) {
		spans.push({
			slot,
			name: slotName(st, slot),
			calls: agg.calls,
			totalNs: agg.totalNs,
			maxNs: agg.maxNs,
		});
	}
	const heapSites: HeapSiteMetrics[] = [];
	for (const [stack, site] of st.heapSites.entries()) {
		heapSites.push({ stack, ...site });
	}
	const ioRows: IoRowMetrics[] = st.ioRows.map((ev) => ({
		kind: ev.kind,
		op: ev.slot_or_op,
		thread: ev.thread,
		durationNs: ev.a,
	}));
	return { spans, heapSites, ioRows };
}

function syncProcessPicker(): void {
	const entries = [...runs.values()];
	if (entries.length === 0) {
		if (procSelect.options.length !== 1 || procSelect.options[0].value !== '') {
			procSelect.innerHTML = '<option value="">no active run</option>';
		}
		return;
	}
	const sig = entries.map((r) => `${r.summary.id}:${r.summary.alive ? 1 : 0}`).join('|');
	if (procSelect.dataset.sig !== sig) {
		procSelect.dataset.sig = sig;
		procSelect.innerHTML = entries
			.map(
				(r) =>
					`<option value="${escapeHtml(r.summary.id)}">${escapeHtml(r.summary.package)} (pid ${r.summary.pid || '—'}${r.summary.alive ? '' : ' · exit'})</option>`,
			)
			.join('');
	}
	if (selectedRunId) procSelect.value = selectedRunId;
}

procSelect.onchange = () => {
	selectedRunId = procSelect.value || null;
	render();
};

function render(): void {
	timeline.render();
	syncProcessPicker();

	const st = selectedRunId ? runs.get(selectedRunId) ?? null : null;
	const { spans, heapSites, ioRows } = collectCurrentMetrics(st);
	const insights = analyzeInsights(builds, st?.summary ?? null, spans, heapSites, ioRows);

	const totalErrors = builds.reduce((s, b) => s + b.errors, 0);
	const totalWarnings = builds.reduce((s, b) => s + b.warnings, 0);
	const latestBuild = builds[0] ?? null;
	const cacheHitPct =
		latestBuild && latestBuild.reached > 0
			? Math.round((latestBuild.found / latestBuild.reached) * 100)
			: null;
	let totalRetained = 0;
	for (const s of heapSites) totalRetained += s.retainedBytes;

	inlineMetricsEl.innerHTML = [
		cacheHitPct !== null
			? `<span class="im">cache <strong class="${cacheHitPct >= 80 ? 'ok' : 'fire'}">${cacheHitPct}%</strong></span>`
			: '',
		st ? `<span class="im">heap <strong>${fmtBytes(totalRetained)}</strong></span>` : '',
		`<span class="im"><strong class="${totalErrors > 0 ? 'err' : 'ok'}">${totalErrors}</strong> err · <strong class="${totalWarnings > 0 ? 'fire' : ''}">${totalWarnings}</strong> warn</span>`,
		st ? `<span class="im"><strong>${st.totalEvents.toLocaleString()}</strong> ev</span>` : '',
	].join('');

	badgeAdvisorEl.textContent = String(insights.length);
	badgeAdvisorEl.classList.toggle('hot', insights.length > 0);

	// 1. Flame Graph tab
	if (activeTab === 'flame') {
		if (st) {
			flameView.setData(buildFlameTree(st, timeline.getSelection()));
		} else {
			flameView.render();
		}
	}

	// 2. Hot Spots (Self / Total / P50 / P95 / P99 / Max + live Hook toggles)
	if (activeTab === 'spans') {
		spansBodyEl.innerHTML = '';
		if (!st || st.spans.size === 0) {
			spansBodyEl.innerHTML = '<tr><td colspan="10" class="quiet">—</td></tr>';
		} else {
			const totalSpanNs = spans.reduce((acc, s) => acc + s.totalNs, 0);
			const sorted = [...st.spans.entries()].sort((a, b) => b[1].totalNs - a[1].totalNs);
			for (const [slot, agg] of sorted) {
				const name = slotName(st, slot);
				const selfNs = Math.max(0, agg.totalNs - agg.childNs);
				const pct = totalSpanNs > 0 ? (agg.totalNs / totalSpanNs) * 100 : 0;
				const p50 = percentile(agg.samples, 0.5);
				const p95 = percentile(agg.samples, 0.95);
				const p99 = percentile(agg.samples, 0.99);
				const tr = document.createElement('tr');
				if (selectedSlot === slot) tr.className = 'row-sel';
				tr.innerHTML = `
					<td class="primary">${escapeHtml(name)}</td>
					<td>${agg.calls.toLocaleString()}</td>
					<td class="primary">${fmtNs(selfNs)}</td>
					<td>${fmtNs(agg.totalNs)}</td>
					<td class="bar-cell">
						<div class="bar-track" title="${pct.toFixed(1)}%"><div class="bar-fill${pct >= 25 ? ' hot' : ''}" style="width:${Math.min(100, pct).toFixed(1)}%"></div></div>
					</td>
					<td>${fmtNs(p50)}</td>
					<td>${fmtNs(p95)}</td>
					<td>${fmtNs(p99)}</td>
					<td>${fmtNs(agg.maxNs)}</td>
					<td><div class="seg"></div></td>
				`;
				const seg = tr.querySelector('.seg')!;
				for (const kind of ['plain', 'count', 'hook', 'span'] as HookKindName[]) {
					const b = document.createElement('button');
					b.textContent = kind;
					if (agg.hook === kind) b.classList.add('active');
					b.onclick = () => {
						agg.hook = kind;
						void client.toggleHook(st.summary.id, slot, kind);
						render();
					};
					seg.appendChild(b);
				}
				spansBodyEl.appendChild(tr);
			}
		}
	}

	// 3. Heap & Snapshot Diff tab
	if (activeTab === 'heap') {
		const hasBaseline = Boolean(st?.heapBaseline);
		btnHeapClearSnap.hidden = !hasBaseline;
		heapSnapLabel.textContent = hasBaseline
			? `Diff vs snapshot (${st?.heapBaselineAt})`
			: st
				? `${fmtBytes(totalRetained)} retained · ${st.livePtrs.size} live`
				: '—';
		heapBodyEl.innerHTML = '';
		if (!st || heapSites.length === 0) {
			heapBodyEl.innerHTML = '<tr><td colspan="7" class="quiet">—</td></tr>';
		} else {
			const maxRetained = heapSites.reduce((m, s) => Math.max(m, s.retainedBytes), 1);
			const sortedHeap = [...heapSites].sort((a, b) => b.retainedBytes - a.retainedBytes);
			for (const site of sortedHeap) {
				const pct = (site.retainedBytes / maxRetained) * 100;
				let deltaHtml = '—';
				let deltaClass = '';
				if (st.heapBaseline) {
					const prev = st.heapBaseline.get(site.stack) ?? 0;
					const diff = site.retainedBytes - prev;
					deltaHtml = `${diff >= 0 ? '+' : ''}${fmtBytes(diff)}`;
					deltaClass = diff > 0 ? 'delta-pos' : diff < 0 ? 'delta-neg' : '';
				}
				const tr = document.createElement('tr');
				tr.innerHTML = `
					<td class="primary">${escapeHtml(site.stack)}</td>
					<td>${site.allocs.toLocaleString()}</td>
					<td>${site.frees.toLocaleString()}</td>
					<td>${fmtBytes(site.totalBytes)}</td>
					<td class="primary">${fmtBytes(site.retainedBytes)}</td>
					<td class="${deltaClass}">${deltaHtml}</td>
					<td class="bar-cell"><div class="bar-track"><div class="bar-fill heap" style="width:${Math.min(100, pct).toFixed(1)}%"></div></div></td>
				`;
				heapBodyEl.appendChild(tr);
			}
		}
	}

	// 4. Builds tab
	if (activeTab === 'builds') {
		if (builds.length === 0) {
			buildsViewEl.innerHTML = '<div class="quiet">No builds recorded</div>';
		} else {
			buildsViewEl.innerHTML = builds
				.map((b) => {
					const hitPct = b.reached > 0 ? Math.round((b.found / b.reached) * 100) : 100;
					const statusTag =
						b.status === 'failed'
							? '<span class="eyebrow err">failed</span>'
							: b.status === 'running' || b.status === 'building'
								? '<span class="eyebrow fire">live</span>'
								: '<span class="eyebrow ok">ok</span>';
					const totalPhaseNs = b.phases.reduce((acc, p) => acc + p.ns, 0);
					const waterfallHtml =
						totalPhaseNs > 0
							? `<div class="wf-bar">${b.phases
									.map((p, i) => {
										const pct = Math.max(3, (p.ns / totalPhaseNs) * 100);
										const color = WF_SERIES[i % WF_SERIES.length];
										return `<div class="wf-seg" style="width:${pct.toFixed(1)}%;background:${color}" title="${escapeHtml(p.name)}: ${fmtNs(p.ns)}">${pct >= 12 ? escapeHtml(p.name) : ''}</div>`;
									})
									.join('')}</div>
								<div class="wf-legend">${b.phases
									.map((p, i) => {
										const color = WF_SERIES[i % WF_SERIES.length];
										return `<span><span class="wf-dot" style="background:${color}"></span>${escapeHtml(p.name)} <strong>${fmtNs(p.ns)}</strong></span>`;
									})
									.join('')}</div>`
							: '';
					return `<article class="card">
						<div class="card-top">
							<div class="card-title">
								<span class="eyebrow">${escapeHtml(b.command)}</span>
								<span>${escapeHtml(b.package)}</span>
								${statusTag}
							</div>
							<div class="card-meta">
								${b.errors > 0 ? `<span class="eyebrow err">${b.errors} err</span>` : ''}
								${b.warnings > 0 ? `<span class="eyebrow fire">${b.warnings} warn</span>` : ''}
								<span>${b.made} built · ${b.found}/${b.reached} cached${totalPhaseNs > 0 ? ` · ${fmtNs(totalPhaseNs)}` : ''}</span>
							</div>
						</div>
						<div class="reason-row">
							<span>${escapeHtml(b.recompile_reason)}</span>
							<span class="meter-inline">
								<span class="meter-track"><span class="meter-fill${hitPct < 70 ? ' warn' : ''}" style="width:${hitPct}%"></span></span>
								<span class="mono" style="font-size:10.5px;color:var(--ink-3)">${hitPct}%</span>
							</span>
						</div>
						${waterfallHtml}
					</article>`;
				})
				.join('');
		}
	}

	// 5. Advisor tab
	if (activeTab === 'advisor') {
		if (insights.length === 0) {
			advisorViewEl.innerHTML = '<div class="quiet">0 issues</div>';
		} else {
			advisorViewEl.innerHTML = '<div class="advisor-list" id="advisor-list-inner"></div>';
			const list = document.getElementById('advisor-list-inner')!;
			for (const ins of insights) {
				const tagClass =
					ins.severity === 'critical'
						? 'err'
						: ins.category === 'HOT' || ins.severity === 'warn'
							? 'fire'
							: ins.category === 'BUILD'
								? 'blue'
								: 'ok';
				const row = document.createElement('div');
				row.className = 'advisor-row';
				row.innerHTML = `
					<div class="advisor-main">
						<span class="eyebrow ${tagClass}">${ins.category}</span>
						<span class="advisor-title">${escapeHtml(ins.title)}</span>
						<span class="advisor-sep">·</span>
						<span class="advisor-desc">${escapeHtml(ins.detail)}</span>
					</div>
					<div class="advisor-right">
						<span class="metric-chip">${escapeHtml(ins.metric)}</span>
						<button class="btn" type="button" data-jump="${ins.targetTab}">→</button>
					</div>
				`;
				row.querySelector<HTMLButtonElement>('button[data-jump]')!.onclick = () => {
					switchTab(ins.targetTab);
				};
				list.appendChild(row);
			}
		}
	}

	// 6. I/O & Locks tab
	if (activeTab === 'io') {
		ioBodyEl.innerHTML = '';
		if (!st || st.ioRows.length === 0) {
			ioBodyEl.innerHTML = '<tr><td colspan="5" class="quiet">—</td></tr>';
		} else {
			for (const ev of [...st.ioRows].reverse().slice(0, 120)) {
				const opLabel = IO_OP_NAMES[ev.slot_or_op] ?? `#${ev.slot_or_op}`;
				const tr = document.createElement('tr');
				tr.innerHTML = `
					<td class="primary">${escapeHtml(ev.kind)}</td>
					<td>t#${ev.thread}</td>
					<td>${escapeHtml(opLabel)}</td>
					<td>${fmtNs(ev.a)}</td>
					<td>arg=${ev.b} · res=${ev.c}</td>
				`;
				ioBodyEl.appendChild(tr);
			}
		}
	}
}

document.querySelectorAll<HTMLButtonElement>('.tab').forEach((btn) => {
	btn.onclick = () => {
		const tab = btn.dataset.tab as 'flame' | 'spans' | 'heap' | 'builds' | 'advisor' | 'io';
		switchTab(tab);
	};
});

btnPause.onclick = () => {
	paused = !paused;
	pauseLabel.textContent = paused ? 'Resume' : 'Pause';
	btnPause.classList.toggle('active', paused);
};

btnClear.onclick = () => {
	timeline.clear();
	if (!selectedRunId) return;
	const st = runs.get(selectedRunId);
	if (!st) return;
	st.spans.clear();
	st.spanHistory.length = 0;
	st.spanSlotById.clear();
	st.heapSites.clear();
	st.livePtrs.clear();
	st.ioRows.length = 0;
	st.totalEvents = 0;
	render();
};

window.addEventListener('resize', () => {
	timeline.render();
	if (activeTab === 'flame') flameView.render();
});

const channel = 'BroadcastChannel' in window ? new BroadcastChannel('blaze-explore') : null;
channel?.postMessage({ type: 'explore-tab-open', ts: Date.now() });

const origin = resolveViewerOrigin(window.location.search);
const client = new LiveClient(origin, {
	onStatus(transport, detail) {
		connDot.className = `dot ${transport === 'disconnected' ? 'off' : 'live'}`;
		connLabel.textContent = detail.replace(/^Live · /, '');
	},
	onMessage(msg: ServerMessage) {
		if (msg.type === 'runs') {
			for (const r of msg.runs) ensureRun(r);
		} else if (msg.type === 'builds') {
			builds = [...msg.builds].reverse();
			for (const b of builds) {
				if (b.slots) buildSlots.set(b.package, b.slots);
			}
		} else if (msg.type === 'run_started') {
			ensureRun(msg.run);
			selectedRunId = msg.run.id;
		} else if (msg.type === 'run_ended') {
			const st = runs.get(msg.id);
			if (st) st.summary.alive = false;
		} else if (msg.type === 'events' && !paused) {
			for (const ev of msg.events) ingestEvent(ev);
		}
		scheduleRender();
	},
});

client.start();
render();
