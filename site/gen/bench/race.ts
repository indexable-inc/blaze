// A Race's two times (content/schema.flecs: the intro's blaze-against-cargo lanes), from the benchmark history: the
// same files /bench charts, read the same way (data.ts), so the front page can only say what a run measured. The page
// names the history, the platform, the project and the edit; this finds the latest run that timed that edit under
// both, and writes it as the Race's `Raced`, which gen/build.ts runs after the content. Nothing is picked here: a
// Race the history cannot answer stops the build.
import { readdirSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { parseHistory } from './data';
import { EDITS, projectName } from './format';
import { literal, script } from './flecs';

export type RaceAsk = { dir: string; platform: string; project: string; edit: string };

/** The history in `dir` answering `ask`, as a Flecs script that sets `Raced` on the entity at `at` (a Race). */
export function raceScript(dir: string, at: string, ask: RaceAsk): string {
	const files = readdirSync(dir).filter((f) => f.endsWith('.jsonl')).sort().map((f) => readFileSync(path.join(dir, f), 'utf8'));
	// Every run on the platform that timed the edit under both, whatever machine it ran on: the newest wins.
	const timed = parseHistory(files)
		.build.filter((m) => m.platform === ask.platform)
		.flatMap((m) => m.runs)
		.flatMap(({ run, results }) => {
			const entry = results[ask.project];
			const phase = entry?.phases[ask.edit];
			return phase?.wall_s !== undefined && phase.cargo_s !== undefined ? [{ run, entry: entry!, blaze: phase.wall_s, cargo: phase.cargo_s, built: phase.built }] : [];
		})
		.sort((a, b) => Date.parse(a.run.date) - Date.parse(b.run.date));
	const last = timed.at(-1);
	if (!last) throw new Error(`${at}: no run in ${dir} on ${ask.platform} timed ${ask.edit} of ${ask.project} under blaze and cargo`);
	// A run where the daemon handed the edit to cargo measured cargo twice: not a race.
	if (last.built && last.built !== 'daemon') throw new Error(`${at}: in the latest run (${last.run.date}) ${ask.edit} of ${ask.project} was built by ${last.built}, not the daemon`);
	const what = EDITS[ask.edit];
	if (!what) throw new Error(`${at}: ${ask.edit} is not an edit the history names (${Object.keys(EDITS).join(', ')})`);
	const fields = {
		blaze_s: last.blaze,
		cargo_s: last.cargo,
		edit: what,
		project: projectName(ask.project.split('/')[0]!, last.entry.tag),
		date: last.run.date.slice(0, 10),
		commit: last.run.commit ?? '',
		machine: last.run.machine,
		runs: last.run.runs ?? 0,
	};
	return script(at, [`Raced: {${Object.entries(fields).map(([k, v]) => `${k}: ${literal(v)}`).join(', ')}}`]);
}
