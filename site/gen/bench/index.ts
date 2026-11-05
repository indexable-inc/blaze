// /bench's charts, from the benchmark history. The page names where they go and where the history is
// (`BenchHistory: {dir}` in content/bench.flecs); this writes them as a Flecs script of Sections, Pickers and charts
// (RowChart, LineChart: content/schema.flecs) under that entity, which gen/build.ts runs after the content. The
// renderer draws them like any other block. The subjects charts compare (subjects.blaze, ...) are defined on the page.
import { readdirSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { backendOrder, MAX_RUNS, parseHistory, platformOrder, projectOrder, ranked, type BuildMachine, type Entry, type LiveMachine, type Run } from './data';
import { BACKEND_NAMES, commitUrl, EDIT_ORDER, EDITS, MODE_ORDER, MODES, PROGRAM_ORDER, PROGRAMS, projectName, SETUPS, short, when } from './format';
import { ent, script, slug, type Ent } from './flecs';

/** The platform /bench opens on. */
const FIRST_PLATFORM = 'Linux x86-64';

const plots = (subject: string) => `(Plots, subjects.${subject})`;
const unique = <T>(xs: T[]) => [...new Set(xs)];
const project = (key: string) => key.split('/')[0];
const backend = (key: string) => key.split('/')[1] ?? '';
const machineName = (m: string) => m || 'Unnamed machine';

function runLine(run: Run) {
	const cpu = [run.cpu?.replace(/( \d+-Core)? Processor$/, ''), run.cores ? `${run.cores} cores` : undefined].filter(Boolean).join(', ');
	const commit = run.commit ? `[\`${short(run.commit)}\`](${commitUrl(run.commit)})` : '(commit unknown)';
	return (
		`${machineName(run.machine)}${cpu ? ` (${cpu})` : ''}, blaze ${commit} on ${when(run.date)}` +
		`${run.runs ? `, median of ${run.runs} edits` : ''}.${run.runUrl ? ` [The CI run](${run.runUrl}).` : ''}`
	);
}

/** A Picker over `items`, one Pick each; `picked` names the one it starts on. */
function picker(name: string, label: string, items: { name: string; label: string; body: Ent[] }[], picked?: string): Ent {
	return ent('Picker', name, { label }, items.map((it) => ent('Pick', slug(it.name), { label: it.label }, [...(it.name === picked ? ['Picked'] : []), ...it.body])));
}

/** The fastest other cargo setup on a phase (not `check`, which builds no program), as [seconds, which]. */
function fastestSetup(setups: Record<string, number> | undefined): [number, string] | undefined {
	const all = Object.entries(setups ?? {}).filter(([s]) => s !== 'check' && s in SETUPS);
	if (!all.length) return undefined;
	const [name, v] = all.reduce((a, b) => (b[1] < a[1] ? b : a));
	return [v, SETUPS[name]];
}

const value = (subject: string, v: number, note = '') => ent('Value', subject, { value: v, note }, [plots(subject)]);

/** The edits that build (not the test runs), in the README's order: what a project's mean over its edits is over. */
const BUILD_EDITS = EDIT_ORDER.filter((p) => p !== 'cold' && p !== 'test' && p !== 'test-edit');

/** The geometric mean of `field` over `phases` of `e`, or undefined when there are none. */
function mean(e: Entry | undefined, phases: string[], field: 'wall_s' | 'cargo_s') {
	if (!e || !phases.length) return undefined;
	return Math.exp(phases.reduce((a, p) => a + Math.log(e.phases[p][field]!), 0) / phases.length);
}

/** The build edits of `e` with both blaze's and cargo's time. */
const bothTimed = (e: Entry) => BUILD_EDITS.filter((p) => e.phases[p]?.wall_s !== undefined && e.phases[p]?.cargo_s !== undefined);

/** One project's edits in one run: blaze and cargo per edit, the speedup on each row. */
function editsChart(key: string, e: Entry): Ent | null {
	const rows = Object.keys(e.phases)
		.filter((p) => p !== 'cold')
		.sort(ranked(EDIT_ORDER))
		.flatMap((p) => {
			const ph = e.phases[p];
			const vals: Ent[] = [];
			if (ph.wall_s !== undefined) vals.push(value('blaze', ph.wall_s, ph.built && ph.built !== 'daemon' ? "the daemon handed this edit to cargo, so it took about cargo's time" : ''));
			if (ph.cargo_s !== undefined) vals.push(value('cargo', ph.cargo_s));
			const setup = fastestSetup(ph.setups);
			if (setup) vals.push(value('setup', setup[0], `the fastest other setup here is ${setup[1]}`));
			return vals.length ? [ent('Row', slug(p), { label: EDITS[p] ?? p }, vals)] : [];
		});
	if (!rows.length) return null;
	return ent('RowChart', 'edits', { title: `${projectName(project(key), e.tag)}, edit by edit`, unit: 's', scale: 'log', note: '' }, ['(Baseline, subjects.cargo)', ...rows]);
}

/** Every project of one run on one chart: a row per project. */
function acrossChart(name: string, title: string, unit: string, scale: 'linear' | 'log', note: string, entries: [string, Entry][], values: (e: Entry) => Ent[], baseline = false): Ent | null {
	const rows = entries.flatMap(([key, e]) => {
		const v = values(e);
		return v.length ? [ent('Row', slug(project(key)), { label: projectName(project(key), e.tag) }, v)] : [];
	});
	return rows.length ? ent('RowChart', name, { title, unit, scale, note }, [...(baseline ? ['(Baseline, subjects.cargo)'] : []), ...rows]) : null;
}

const peak = (e: Entry, field: 'mem_mb' | 'cargo_mem_mb') => {
	const all = Object.entries(e.phases).filter(([p]) => p !== 'cold').flatMap(([, ph]) => (ph[field] === undefined ? [] : [ph[field]!]));
	return all.length ? Math.max(...all) : undefined;
};

/** A run's charts for one backend: every project's edits, then one project's, cold builds, memory and disk. */
function latestCharts(entries: [string, Entry][], tries: number | undefined): Ent[] {
	const medians = tries ? `the median of ${tries} tries each` : 'the median of its tries each';
	const summary = acrossChart('edits', 'Edit, then build: every project', 's', 'log', `Each project's edits in one number: the geometric mean of the edits it measured (${medians}), for blaze and for cargo. Pick a project below for its edits one by one.`, entries, (e) => {
		const both = bothTimed(e);
		const [ours, theirs] = [mean(e, both, 'wall_s'), mean(e, both, 'cargo_s')];
		return ours === undefined || theirs === undefined ? [] : [value('blaze', ours), value('cargo', theirs)];
	}, true);
	const projects = entries.flatMap(([k, e]) => {
		const c = editsChart(k, e);
		return c ? [{ name: project(k), label: project(k), body: [c] }] : [];
	});
	const cold = acrossChart('cold', 'Cold build, until ready', 's', 'linear', 'From nothing built to ready: cargo builds everything; blaze builds and starts its daemon. A second checkout of the same commit starts from the store the first one filled.', entries, (e) => {
		const c = e.phases.cold;
		if (!c) return [];
		return [
			...(c.wall_s !== undefined ? [value('blaze', c.wall_s)] : []),
			...(c.warm_store_s !== undefined ? [value('warm', c.warm_store_s)] : []),
			...(c.cargo_s !== undefined ? [value('cargo', c.cargo_s)] : []),
		];
	}, true);
	const memory = acrossChart('memory', 'Memory while editing', 'MB', 'linear', 'The most every process doing the work held at once over the edits: for blaze the daemon, its workers and the command; for cargo, cargo, rustc and the linker ([how](https://github.com/indexable-inc/blaze/blob/main/docs/benchmarks.md#memory-and-disk)).', entries, (e) => {
		const [ours, theirs] = [peak(e, 'mem_mb'), peak(e, 'cargo_mem_mb')];
		return [...(ours !== undefined ? [value('blaze', ours)] : []), ...(theirs !== undefined ? [value('cargo', theirs)] : [])];
	});
	const disk = acrossChart('disk', 'On disk', 'MB', 'linear', 'blaze: its target directory, store and home. cargo: its target directory.', entries, (e) => [
		...(e.disk_mb !== undefined ? [value('blaze', e.disk_mb)] : []),
		...(e.cargo_disk_mb !== undefined ? [value('cargo', e.cargo_disk_mb)] : []),
	]);
	const pair = [memory, disk].filter((c): c is Ent => c !== null);
	return [
		...(summary ? [summary] : []),
		...(projects.length ? [picker('project', 'Project', projects, projects[0].name)] : []),
		...(cold ? [cold] : []),
		...(pair.length ? [ent('Charts', 'resources', undefined, pair)] : []),
	];
}

/** The newest run of each platform: a Picker of platforms, then of backends. */
function latest(build: BuildMachine[]): Ent[] {
	const newest = new Map<string, BuildMachine['runs'][number]>();
	for (const m of build) {
		const last = m.runs.at(-1)!;
		const had = newest.get(m.platform);
		if (!had || Date.parse(had.run.date) < Date.parse(last.run.date)) newest.set(m.platform, last);
	}
	const platforms = [...newest.keys()].sort(platformOrder);
	if (!platforms.length) return [ent('Dim', 'none', { text: 'No benchmark runs recorded yet.' })];
	const items = platforms.map((platform) => {
		const { run, results } = newest.get(platform)!;
		const keys = Object.keys(results).sort((a, b) => projectOrder(project(a), project(b)) || backendOrder(backend(a), backend(b)));
		const backends = unique(keys.map(backend)).sort(backendOrder).map((b) => ({
			name: b,
			label: BACKEND_NAMES[b] ?? b,
			body: latestCharts(keys.filter((k) => backend(k) === b).map((k) => [k, results[k]] as [string, Entry]), run.runs),
		}));
		return {
			name: platform,
			label: platform,
			body: [ent('Dim', 'run', { text: runLine(run) }), picker('backend', 'Backend', backends, backends[0]?.name)],
		};
	});
	return [
		ent('P', 'how', { text: "Times are on a log scale, so 25 ms and 1.3 s both read: each step of the axis is ten times the last. Beside each row are its numbers and how much faster blaze was than plain cargo; open a chart's numbers for the table." }),
		picker('platform', 'Platform', items, platforms.includes(FIRST_PLATFORM) ? FIRST_PLATFORM : platforms.at(-1)),
	];
}

/** A series' values as Series writes them: one per run, `-` where there is none. */
const values = (xs: (number | undefined)[]) => xs.map((v) => (v === undefined || !Number.isFinite(v) ? '-' : String(v))).join(' ');
const runs = (rs: { run: Run }[]) => `Runs: {dates: "${rs.map((r) => r.run.date).join(' ')}", commits: "${rs.map((r) => r.run.commit ?? '-').join(' ')}"}`;
const series = (subject: string, xs: (number | undefined)[]) => ent('Series', subject, { values: values(xs), lo: '', hi: '' }, [plots(subject)]);

type Line = [subject: string, values: (number | undefined)[]];

/**
 * A LineChart over the runs where any of `lines` has a value (a mode a run left out is no gap in the chart, and a
 * chart with one such run shows its values): null when no run has one. A line whose values are all missing there
 * is left out.
 */
function lineChart(name: string, fields: { title: string; unit: string; scale: 'linear' | 'log'; note: string }, all: { run: Run }[], lines: Line[]): Ent | null {
	const kept = all.map((_, i) => i).filter((i) => lines.some(([, xs]) => xs[i] !== undefined));
	if (!kept.length) return null;
	const drawn = lines.filter(([, xs]) => kept.some((i) => xs[i] !== undefined));
	return ent('LineChart', name, fields, [runs(kept.map((i) => all[i])), ...drawn.map(([s, xs]) => series(s, kept.map((i) => xs[i])))]);
}

/** Per run, the geometric mean of the build edits that every run in `entries` timed for both blaze and cargo. */
function editMeans(entries: (Entry | undefined)[]) {
	const sets = entries.filter((e): e is Entry => !!e).map(bothTimed);
	const common = sets.length ? sets.reduce((a, b) => a.filter((p) => b.includes(p))) : [];
	return (field: 'wall_s' | 'cargo_s') => entries.map((e) => mean(e, common, field));
}

/** A machine's name without what follows it: "AWS c7a.48xlarge (dev-aws-8), pinned ..." is "c7a.48xlarge". */
const shortMachine = (m: string) => machineName(m).replace(/\s*\([^)]*\)/g, '').split(',')[0].replace(/^AWS /, '').trim();

/** Each machine's runs over time: a Picker of machines, then of backends, then of projects. */
function overTime(build: BuildMachine[]): Ent[] {
	if (!build.length) return [ent('Dim', 'none', { text: 'No benchmark runs recorded yet.' })];
	const newestOf = (platform: string) => build.filter((m) => m.platform === platform).at(-1);
	const first = newestOf(FIRST_PLATFORM) ?? build.at(-1)!;
	const items = build.map((m) => {
		const keys = unique(m.runs.flatMap((r) => Object.keys(r.results))).sort((a, b) => projectOrder(project(a), project(b)));
		const backends = unique(keys.map(backend)).sort(backendOrder).map((b) => {
			const projects = keys.filter((k) => backend(k) === b).flatMap((k) => {
				const entries = m.runs.map((r) => r.results[k]);
				const means = editMeans(entries);
				const [ours, theirs] = [means('wall_s'), means('cargo_s')];
				const cold = entries.map((e) => e?.phases.cold);
				const charts = [
					lineChart('edits', { title: 'Edit, then build', unit: 's', scale: 'log', note: 'The geometric mean of the edits every run measured.' }, m.runs, [
						['blaze', ours],
						['cargo', theirs],
					]),
					lineChart('cold', { title: 'Cold build, until ready', unit: 's', scale: 'linear', note: '' }, m.runs, [
						['blaze', cold.map((c) => c?.wall_s)],
						['warm', cold.map((c) => c?.warm_store_s)],
						['cargo', cold.map((c) => c?.cargo_s)],
					]),
				].filter((c): c is Ent => c !== null);
				return charts.length ? [{ name: project(k), label: project(k), body: [ent('Charts', 'charts', undefined, charts)] }] : [];
			});
			return { name: b, label: BACKEND_NAMES[b] ?? b, body: projects.length ? [picker('project', 'Project', projects, projects[0].name)] : [] };
		});
		const [a, z] = [m.runs[0].run, m.runs.at(-1)!.run];
		const span = m.runs.length === 1 ? `one run, on ${when(a.date)}` : `${m.runs.length} runs, from ${when(a.date)} to ${when(z.date)}`;
		return {
			name: `${m.platform} ${m.machine}`,
			label: `${m.platform}, ${shortMachine(m.machine)}`,
			body: [ent('Dim', 'runs', { text: `${machineName(m.machine)}: ${span}.` }), picker('backend', 'Backend', backends, backends[0]?.name)],
		};
	});
	return [
		ent('P', 'how', { text: "A machine's runs, oldest to newest. Edit, then build is one number per run, the geometric mean of the edits every run measured, so it moves when any edit does (on a log scale, where a step is the same share at any height); each edit's own numbers are in the latest run above. Point at a chart to read a run." }),
		picker('machine', 'Machine', items, `${first.platform} ${first.machine}`),
	];
}

/** Live edits over time: a Picker of machines, then of programs, then a chart per mode. */
function live(machines: LiveMachine[]): Ent[] {
	if (!machines.length) return [ent('Dim', 'none', { text: 'No live-edit runs recorded yet.' })];
	const newest = [...machines].sort((a, b) => Date.parse(a.runs.at(-1)!.run.date) - Date.parse(b.runs.at(-1)!.run.date)).at(-1)!;
	const items = machines.map((m) => {
		const programs = unique(m.runs.flatMap((r) => Object.keys(r.results))).sort(ranked(PROGRAM_ORDER)).flatMap((program) => {
			const modes = unique(m.runs.flatMap((r) => Object.keys(r.results[program] ?? {}))).sort(ranked(MODE_ORDER));
			const charts = modes.flatMap((mode) => {
				const stat = m.runs.map((r) => r.results[program]?.[mode]);
				const chart = lineChart(slug(mode), { title: MODES[mode] ?? mode, unit: 'ms', scale: 'log', note: '' }, m.runs, [
					['median', stat.map((s) => s?.p50)],
					['p90', stat.map((s) => s?.p90)],
				]);
				return chart ? [chart] : [];
			});
			return charts.length ? [{ name: program, label: PROGRAMS[program] ?? program, body: [ent('Charts', 'charts', undefined, charts)] }] : [];
		});
		const span = m.runs.length === 1 ? `One run, on ${when(m.runs[0].run.date)}.` : `${m.runs.length} runs, from ${when(m.runs[0].run.date)} to ${when(m.runs.at(-1)!.run.date)}.`;
		return {
			name: m.machine || 'unnamed',
			label: machineName(m.machine),
			body: [ent('Dim', 'runs', { text: span }), ...(programs.length ? [picker('program', 'Program', programs, programs[0].name)] : [])],
		};
	});
	return [
		ent('P', 'how', { text: 'Edit to running: from the edit leaving the editor to the new code running in the program, on a log scale. Solid is the median edit, dashed the 90th percentile. A chart counts only the runs that measured it.' }),
		picker('machine', 'Machine', items, newest.machine || 'unnamed'),
	];
}

/** The history in `dir` as Flecs script under the entity at `at` (a BenchHistory). */
export function benchScript(dir: string, at: string): string {
	const files = readdirSync(dir).filter((f) => f.endsWith('.jsonl')).sort().map((f) => readFileSync(path.join(dir, f), 'utf8'));
	const h = parseHistory(files);
	const capped = [...h.build, ...h.live].some((m) => m.runs.length >= MAX_RUNS);
	return script(at, [
		ent('Section', 'latest', { title: 'Latest run' }, latest(h.build)),
		ent('Section', 'time', { title: 'Over time' }, [
			...overTime(h.build),
			...(capped ? [ent('Dim', 'capped', { text: `The charts show the last ${MAX_RUNS} runs of each machine.` })] : []),
		]),
		ent('Section', 'live', { title: 'Live edits' }, live(h.live)),
	]);
}
