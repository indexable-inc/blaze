// The benchmark history (docs/benchmarks/history/*.jsonl, written by harness/benchhistory.py, whose docstring
// describes a line), parsed at build time into what /bench charts. Any key of a line but `suite`, `date` and
// `results` may be missing.

/** One phase (an edit, or the cold build) of one project in one run. */
export type Phase = {
	wall_s?: number;
	cargo_s?: number;
	/** Cold only: until ready in a second checkout, with the store the first one filled. */
	warm_store_s?: number;
	/** The most every process doing the work held at once (harness/memory.py): blaze's, and cargo's. */
	mem_mb?: number;
	cargo_mem_mb?: number;
	/** Who built it: `daemon`, or `cargo` when the daemon handed the edit on. */
	built?: string;
	/** Other cargo setups on the same edit: seconds by setup name. */
	setups?: Record<string, number>;
};
/** One `project/backend` in one run. */
export type Entry = { tag?: string; disk_mb?: number; cargo_disk_mb?: number; phases: Record<string, Phase> };
export type Run = {
	date: string;
	commit?: string;
	runUrl?: string;
	platform: string;
	machine: string;
	cpu?: string;
	cores?: number;
	runs?: number;
};
/** The build runs of one machine, oldest first, each with its entries by `project/backend`. */
export type BuildMachine = { machine: string; platform: string; runs: { run: Run; results: Record<string, Entry> }[] };
export type LiveStat = { p50?: number; p90?: number };
/** The live-edit runs of one machine, oldest first: by program, then mode. */
export type LiveMachine = { machine: string; runs: { run: Run; results: Record<string, Record<string, LiveStat>> }[] };
export type History = { build: BuildMachine[]; live: LiveMachine[] };

/** The charts keep the most recent runs of each machine. */
export const MAX_RUNS = 60;

// The platforms each benchmark runs on, in the README's order (harness/benchplatforms.py PLATFORMS).
export const PLATFORMS = ['macOS arm64', 'Linux aarch64', 'Linux x86-64'];
// The README's order of projects (harness/benchreport.py PROJECTS) and of backends.
const PROJECTS = ['ripgrep', 'fd', 'bat', 'hyperfine', 'just', 'axum', 'nushell', 'helix', 'bevy', 'biome', 'zed', 'cargo', 'bun'];
const BACKENDS = ['cranelift', 'llvm'];

type Json = Record<string, unknown>;

const isObject = (v: unknown): v is Json => typeof v === 'object' && v !== null && !Array.isArray(v);
const num = (v: unknown) => (typeof v === 'number' && Number.isFinite(v) ? v : undefined);
const str = (v: unknown) => (typeof v === 'string' && v !== '' ? v : undefined);

/** Names in `order` first, in its order, then the rest by name. */
export function ranked(order: string[]) {
	const rank = (name: string) => (order.includes(name) ? order.indexOf(name) : order.length);
	return (a: string, b: string) => rank(a) - rank(b) || a.localeCompare(b);
}
export const projectOrder = ranked(PROJECTS);
export const backendOrder = ranked(BACKENDS);
export const platformOrder = ranked(PLATFORMS);

/** A line's platform: its own, or for a line from before lines said it, what its CPU says. */
function platformOf(line: Json) {
	const named = str(line.platform);
	if (named) return named;
	const cpu = str(line.cpu) ?? '';
	if (/Apple/.test(cpu)) return 'macOS arm64';
	if (/Neoverse|Graviton|aarch64|ARM/i.test(cpu)) return 'Linux aarch64';
	return 'Linux x86-64';
}

function run(line: Json): Run {
	return {
		date: str(line.date) ?? '',
		commit: str(line.commit),
		runUrl: str(line.run_url),
		platform: platformOf(line),
		machine: str(line.machine) ?? '',
		cpu: str(line.cpu),
		cores: num(line.cores),
		runs: num(line.runs),
	};
}

function entry(value: unknown): Entry | null {
	if (!isObject(value) || !isObject(value.phases)) return null;
	const phases: Record<string, Phase> = {};
	for (const [label, raw] of Object.entries(value.phases)) {
		if (!isObject(raw)) continue;
		const setups = isObject(raw.setups)
			? Object.fromEntries(Object.entries(raw.setups).flatMap(([k, v]) => (num(v) === undefined ? [] : [[k, num(v)!]])))
			: {};
		phases[label] = {
			wall_s: num(raw.wall_s),
			cargo_s: num(raw.cargo_s),
			warm_store_s: num(raw.warm_store_s),
			mem_mb: num(raw.mem_mb),
			cargo_mem_mb: num(raw.cargo_mem_mb),
			built: str(raw.built),
			setups: Object.keys(setups).length ? setups : undefined,
		};
	}
	return { tag: str(value.tag), disk_mb: num(value.disk_mb), cargo_disk_mb: num(value.cargo_disk_mb), phases };
}

/** The lines of every file, in file order, that are an object with a known suite and results. */
function lines(files: string[]): Json[] {
	const out: Json[] = [];
	for (const text of files) {
		for (const raw of text.split('\n')) {
			if (!raw.trim()) continue;
			try {
				const line = JSON.parse(raw);
				if (isObject(line) && (line.suite === 'build' || line.suite === 'live') && isObject(line.results)) out.push(line);
			} catch {
				// A line that is not JSON (a merge gone wrong, a cut-off write) is left out.
			}
		}
	}
	return out;
}

const time = (line: Json) => {
	const t = Date.parse(str(line.date) ?? '');
	return Number.isNaN(t) ? 0 : t;
};

/** Lines grouped by machine (numbers from two machines never share a series), each oldest first, the last MAX_RUNS. */
function machines(all: Json[]) {
	const groups = new Map<string, Json[]>();
	for (const line of [...all].sort((a, b) => time(a) - time(b))) {
		const name = str(line.machine) ?? '';
		groups.set(name, [...(groups.get(name) ?? []), line]);
	}
	return [...groups].map(([machine, group]) => ({ machine, group: group.slice(-MAX_RUNS) }));
}

export function parseHistory(files: string[]): History {
	const all = lines(files);
	const build: BuildMachine[] = machines(all.filter((l) => l.suite === 'build'))
		.map(({ machine, group }) => ({
			machine,
			platform: platformOf(group.at(-1)!),
			runs: group.map((l) => {
				const results: Record<string, Entry> = {};
				for (const [key, v] of Object.entries(l.results as Json)) {
					const e = entry(v);
					if (e) results[key] = e;
				}
				return { run: run(l), results };
			}),
		}))
		.sort((a, b) => platformOrder(a.platform, b.platform) || time({ date: a.runs.at(-1)!.run.date }) - time({ date: b.runs.at(-1)!.run.date }));
	const live: LiveMachine[] = machines(all.filter((l) => l.suite === 'live')).map(({ machine, group }) => ({
		machine,
		runs: group.map((l) => {
			const results: LiveMachine['runs'][number]['results'] = {};
			for (const [program, modes] of Object.entries(l.results as Json)) {
				if (!isObject(modes)) continue;
				results[program] = {};
				for (const [mode, stat] of Object.entries(modes)) {
					if (isObject(stat)) results[program][mode] = { p50: num(stat.p50_ms), p90: num(stat.p90_ms) };
				}
			}
			return { run: run(l), results };
		}),
	}));
	return { build, live };
}
