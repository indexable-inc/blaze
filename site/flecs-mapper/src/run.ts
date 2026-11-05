/**
 * Runs scripts under the site's own Flecs (site/flecs/flecs-doc.wasm, the binary the build uses) and
 * returns the entities it prints, or the errors Flecs reported, with line and column.
 */
import { mkdtempSync, readFileSync, rmSync, writeFileSync, openSync, closeSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { WASI } from 'node:wasi';

export type FlecsValue = Record<string, unknown>;

export type FlecsEntity = {
	parent?: string;
	name: string;
	id: number;
	tags?: string[];
	pairs?: Record<string, string | string[]>;
	components?: Record<string, FlecsValue | null>;
};

export type FlecsError = {
	/** Index into the files passed to run(), or -1 when Flecs named no script. */
	file: number;
	line: number;
	/** Column of Flecs's caret, when it printed one. */
	column: number | null;
	message: string;
};

export type RunResult = { ok: true; entities: FlecsEntity[] } | { ok: false; errors: FlecsError[]; stderr: string };

const modules = new Map<string, WebAssembly.Module>();

/** Run `files` in order in one world, then print the entities under `roots`. */
export async function run(wasmPath: string, files: string[], roots: string[]): Promise<RunResult> {
	let module = modules.get(wasmPath);
	if (!module) {
		module = await WebAssembly.compile(readFileSync(wasmPath));
		modules.set(wasmPath, module);
	}
	const dir = mkdtempSync(path.join(tmpdir(), 'flecs-mapper-'));
	try {
		// Flecs names a script by its base name in errors, so each gets a unique one.
		const names = files.map((_, i) => `s${i}.flecs`);
		files.forEach((text, i) => writeFileSync(path.join(dir, names[i]), text));
		// Flecs logs its errors to stdout, which here is the JSON-RPC stream, so both go to a file.
		const errFile = path.join(dir, 'log.txt');
		const log = openSync(errFile, 'w');
		let code: number;
		try {
			const wasi = new WASI({
				version: 'preview1',
				args: ['flecs-doc', '/w/tree.json', roots.join(','), ...names.map((n) => `/w/${n}`)],
				preopens: { '/w': dir },
				stdout: log,
				stderr: log,
				returnOnExit: true,
			});
			const instance = await WebAssembly.instantiate(module, { wasi_snapshot_preview1: wasi.wasiImport });
			code = wasi.start(instance);
		} catch (e) {
			code = -1;
			writeFileSync(errFile, `flecs-doc trapped: ${String(e)}\n`, { flag: 'a' });
		} finally {
			closeSync(log);
		}
		const err = readFileSync(errFile, 'utf8').replace(/\x1b\[[0-9;]*m/g, '');
		if (code !== 0) return { ok: false, errors: parseErrors(err, names), stderr: err };
		const entities = JSON.parse(readFileSync(path.join(dir, 'tree.json'), 'utf8')) as FlecsEntity[];
		return { ok: true, entities };
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
}

/**
 * Flecs prints `error: s2.flecs: 9: message`, then the source line and a caret under the column.
 * A detail without a position (`error: flecs.c: 55604: unknown member 'nope' for type 'P'`) is
 * attached to the positioned error after it.
 */
function parseErrors(stderr: string, names: string[]): FlecsError[] {
	const lines = stderr.split('\n');
	const out: FlecsError[] = [];
	let detail: string[] = [];
	for (let k = 0; k < lines.length; k++) {
		const m = /^(?:error|warning): ([^:]+): (\d+): (.*)$/.exec(lines[k]);
		if (!m) continue;
		const file = names.indexOf(m[1]);
		if (file < 0) {
			detail.push(m[3]);
			continue;
		}
		let column: number | null = null;
		const caret = lines[k + 2];
		if (caret !== undefined && /^\s*\^\s*$/.test(caret)) column = caret.indexOf('^');
		out.push({ file, line: Number(m[2]), column, message: [...detail, m[3]].join(': ') });
		detail = [];
	}
	const failed = /flecs-doc: \/w\/(s\d+\.flecs) failed/.exec(stderr);
	if (!out.length) {
		const tail = stderr.trim().split('\n').slice(-6).join(' | ') || 'flecs-doc failed without a message';
		out.push({ file: failed ? names.indexOf(failed[1]) : -1, line: 0, column: null, message: [...detail, tail].join(': ') });
	}
	return out;
}
