/**
 * Compact Runtime + Build Recommendations engine for `blaze / explore`.
 * Crisp single-line findings grounded directly in live build + runtime streams.
 */

import type { BuildSummary, RunSummary } from './protocol';

export type InsightSeverity = 'critical' | 'warn' | 'opt' | 'info';
export type InsightCategory = 'HOT' | 'HEAP' | 'LOCK' | 'IO' | 'BUILD' | 'DIAG';

export interface Insight {
	id: string;
	severity: InsightSeverity;
	category: InsightCategory;
	title: string;
	detail: string;
	metric: string;
	targetTab: 'builds' | 'spans' | 'heap' | 'io';
}

export interface SpanMetrics {
	slot: number;
	name: string;
	calls: number;
	totalNs: number;
	maxNs: number;
}

export interface HeapSiteMetrics {
	stack: string;
	allocs: number;
	frees: number;
	totalBytes: number;
	retainedBytes: number;
}

export interface IoRowMetrics {
	kind: string;
	op: number;
	thread: number;
	durationNs: number;
}

function fmtBytes(n: number): string {
	if (n < 1024) return `${n} B`;
	if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
	return `${(n / (1024 * 1024)).toFixed(2)} MiB`;
}

function fmtNs(ns: number): string {
	if (ns < 1_000) return `${ns.toFixed(0)} ns`;
	if (ns < 1_000_000) return `${(ns / 1_000).toFixed(1)} µs`;
	if (ns < 1_000_000_000) return `${(ns / 1_000_000).toFixed(2)} ms`;
	return `${(ns / 1_000_000_000).toFixed(2)} s`;
}

export function analyzeInsights(
	builds: BuildSummary[],
	run: RunSummary | null,
	spans: SpanMetrics[],
	heapSites: HeapSiteMetrics[],
	ioRows: IoRowMetrics[],
): Insight[] {
	const out: Insight[] = [];

	const totalErrors = builds.reduce((s, b) => s + b.errors, 0);
	const totalWarnings = builds.reduce((s, b) => s + b.warnings, 0);
	if (totalErrors > 0) {
		out.push({
			id: 'diag-errors',
			severity: 'critical',
			category: 'DIAG',
			title: `${totalErrors} build error${totalErrors === 1 ? '' : 's'}`,
			detail: 'Failed target in Builds',
			metric: `${totalErrors} err`,
			targetTab: 'builds',
		});
	}
	if (totalWarnings > 0) {
		out.push({
			id: 'diag-warnings',
			severity: 'warn',
			category: 'DIAG',
			title: `${totalWarnings} warning${totalWarnings === 1 ? '' : 's'}`,
			detail: 'Active compiler diagnostics',
			metric: `${totalWarnings} warn`,
			targetTab: 'builds',
		});
	}

	const latestBuild = builds[0] ?? null;
	if (latestBuild) {
		if (latestBuild.made > 0 && latestBuild.reached > 0) {
			const hitPct = Math.round((latestBuild.found / latestBuild.reached) * 100);
			out.push({
				id: `build-recompile-${latestBuild.id}`,
				severity: hitPct < 50 ? 'warn' : 'info',
				category: 'BUILD',
				title: `Recompiled ${latestBuild.made}/${latestBuild.reached} fns (${hitPct}% cached)`,
				detail: latestBuild.recompile_reason,
				metric: `${latestBuild.made} rebuilt`,
				targetTab: 'builds',
			});
		}
		if (latestBuild.phases.length > 0) {
			const totalPhaseNs = latestBuild.phases.reduce((s, p) => s + p.ns, 0);
			const slowest = latestBuild.phases.reduce((best, p) => (p.ns > best.ns ? p : best), latestBuild.phases[0]);
			const share = totalPhaseNs > 0 ? Math.round((slowest.ns / totalPhaseNs) * 100) : 0;
			if (share >= 35 && slowest.ns >= 5_000_000) {
				out.push({
					id: `build-phase-${slowest.name}`,
					severity: share >= 60 ? 'warn' : 'opt',
					category: 'BUILD',
					title: `Slowest phase: ${slowest.name} (${share}%)`,
					detail: `${fmtNs(slowest.ns)} of ${fmtNs(totalPhaseNs)} wall`,
					metric: fmtNs(slowest.ns),
					targetTab: 'builds',
				});
			}
		}
	}

	const totalSpanNs = spans.reduce((s, x) => s + x.totalNs, 0);
	if (totalSpanNs > 0) {
		const sorted = [...spans].sort((a, b) => b.totalNs - a.totalNs);
		for (const sp of sorted.slice(0, 3)) {
			const share = (sp.totalNs / totalSpanNs) * 100;
			const avgNs = sp.calls > 0 ? sp.totalNs / sp.calls : 0;
			if (share >= 25 || avgNs >= 5_000_000) {
				out.push({
					id: `hot-span-${sp.slot}`,
					severity: share >= 50 ? 'warn' : 'opt',
					category: 'HOT',
					title: `Hot: ${sp.name} (${share.toFixed(1)}%)`,
					detail: `${sp.calls} calls · avg ${fmtNs(avgNs)} · tier-2 / cache candidate`,
					metric: `${share.toFixed(0)}% CPU`,
					targetTab: 'spans',
				});
			}
		}
	}

	const sortedHeap = [...heapSites].sort((a, b) => b.retainedBytes - a.retainedBytes);
	for (const site of sortedHeap.slice(0, 2)) {
		if (site.retainedBytes >= 256 * 1024) {
			out.push({
				id: `heap-retain-${site.stack}`,
				severity: site.retainedBytes >= 4 * 1024 * 1024 ? 'warn' : 'opt',
				category: 'HEAP',
				title: `Retained ${fmtBytes(site.retainedBytes)} at ${site.stack}`,
				detail: `${site.allocs} alloc / ${site.frees} free · check growth or reuse buffer`,
				metric: fmtBytes(site.retainedBytes),
				targetTab: 'heap',
			});
		} else if (site.allocs >= 250) {
			out.push({
				id: `heap-churn-${site.stack}`,
				severity: 'opt',
				category: 'HEAP',
				title: `Alloc churn (${site.allocs}×) at ${site.stack}`,
				detail: `Pre-size with Vec::with_capacity or reuse scratch`,
				metric: `${site.allocs}×`,
				targetTab: 'heap',
			});
		}
	}

	let worstLockNs = 0;
	let lockStalls = 0;
	let worstIoNs = 0;
	let slowIoCount = 0;
	for (const row of ioRows) {
		if (row.kind === 'sync_wait' && row.durationNs >= 1_000_000) {
			lockStalls += 1;
			if (row.durationNs > worstLockNs) worstLockNs = row.durationNs;
		} else if (row.kind === 'io' && row.durationNs >= 10_000_000) {
			slowIoCount += 1;
			if (row.durationNs > worstIoNs) worstIoNs = row.durationNs;
		}
	}
	if (lockStalls > 0) {
		out.push({
			id: 'rt-lock-contention',
			severity: worstLockNs >= 10_000_000 ? 'warn' : 'opt',
			category: 'LOCK',
			title: `Mutex contention (${lockStalls} stalls ≥ 1 ms)`,
			detail: `Peak wait ${fmtNs(worstLockNs)} · shrink critical section`,
			metric: fmtNs(worstLockNs),
			targetTab: 'io',
		});
	}
	if (slowIoCount > 0) {
		out.push({
			id: 'rt-slow-io',
			severity: 'opt',
			category: 'IO',
			title: `Slow blocking I/O (${slowIoCount} calls ≥ 10 ms)`,
			detail: `Peak ${fmtNs(worstIoNs)} · batch or offload`,
			metric: fmtNs(worstIoNs),
			targetTab: 'io',
		});
	}

	if (run?.stats.unwound_spans) {
		out.push({
			id: `rt-unwound-${run.id}`,
			severity: 'warn',
			category: 'HOT',
			title: `${run.stats.unwound_spans} panicking unwind${run.stats.unwound_spans === 1 ? '' : 's'}`,
			detail: 'Prefer Result over panic on hot path',
			metric: `${run.stats.unwound_spans} unwind`,
			targetTab: 'spans',
		});
	}

	const rank: Record<InsightSeverity, number> = { critical: 0, warn: 1, opt: 2, info: 3 };
	out.sort((a, b) => rank[a.severity] - rank[b.severity]);
	return out;
}
