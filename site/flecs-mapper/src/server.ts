/**
 * flecs-content-mapper: the TypeScript 7.1 content mapper process for .flecs files. tsc spawns it
 * (package.json `typescript.contentMapper.exec`) and speaks JSON-RPC over stdio with LSP framing
 * (Content-Length headers); see microsoft/typescript-go#4712 for the protocol.
 *
 * For each file, it runs the project's scripts under the site's Flecs in build order (gen/build.ts:
 * schema.flecs, refs.flecs, then the rest by path), up to and including the file being transformed
 * with its current text, then builds the TypeScript from what Flecs produced (emit.ts).
 */
import { createHash } from 'node:crypto';
import { readdirSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { emit, type Options as EmitOptions } from './emit.ts';
import { run } from './run.ts';
import { CODE, scan, type Diag } from './scan.ts';

const PKG = path.join(import.meta.dirname, '..');
const PRELUDE = path.join(PKG, 'prelude.d.ts');
const VERSION = (JSON.parse(readFileSync(path.join(PKG, 'package.json'), 'utf8')) as { version: string }).version;
const MARK = '__flecs_mapper_mark';

type Options = EmitOptions & {
	/** The directories holding the site's scripts, relative to the tsconfig. */
	scripts: string[];
	/** Scripts that run first, in this order, by base name; the rest follow in path order. */
	first: string[];
	/** flecs-doc.wasm, relative to the tsconfig. */
	wasm: string | null;
};

type Project = { options: Options; dir: string; scripts: string[]; wasm: string };
const projects = new Map<string, Project>();

type OptionDiagnostic = { path: (string | number)[]; messageText: string; code: number };

function readOptions(raw: Record<string, unknown> | undefined): { options: Options; diags: OptionDiagnostic[] } {
	const diags: OptionDiagnostic[] = [];
	const o = raw ?? {};
	const str = (k: string, d: string | null): string | null => {
		if (o[k] === undefined) return d;
		if (typeof o[k] === 'string') return o[k] as string;
		diags.push({ path: [k], messageText: `"${k}" must be a string.`, code: 91010 });
		return d;
	};
	let relationships: Record<string, string> = { Cites: 'Ref', DependsOn: 'Page' };
	if (o.relationships !== undefined) {
		const r = o.relationships;
		if (!r || typeof r !== 'object' || Array.isArray(r)) diags.push({ path: ['relationships'], messageText: '"relationships" must be an object.', code: 91010 });
		else {
			relationships = {};
			for (const [k, v] of Object.entries(r)) {
				if (typeof v === 'string' && /^[A-Za-z_]\w*$/.test(v)) relationships[k] = v;
				else diags.push({ path: ['relationships', k], messageText: 'A rule must name a tag, component or prefab at the root, like "Ref".', code: 91010 });
			}
		}
	}
	let first = ['schema.flecs', 'refs.flecs'];
	if (o.first !== undefined) {
		if (Array.isArray(o.first) && o.first.every((x) => typeof x === 'string')) first = o.first as string[];
		else diags.push({ path: ['first'], messageText: '"first" must be an array of file names.', code: 91010 });
	}
	let dirs = ['.'];
	if (typeof o.scripts === 'string') dirs = [o.scripts];
	else if (Array.isArray(o.scripts) && o.scripts.every((x) => typeof x === 'string')) dirs = o.scripts as string[];
	else if (o.scripts !== undefined) diags.push({ path: ['scripts'], messageText: '"scripts" must be a directory or an array of directories.', code: 91010 });
	const refs = o.refs === null ? null : str('refs', 'refs');
	return { options: { relationships, refs, first, scripts: dirs, wasm: str('wasm', null) }, diags };
}

/** The scripts under `dirs`, in the order the site's build runs them. */
function scripts(dirs: string[], first: string[]): string[] {
	const files = [
		...new Set(
			dirs.flatMap((dir) =>
				readdirSync(dir, { recursive: true, encoding: 'utf8' })
					.filter((f) => f.endsWith('.flecs') && !f.split(path.sep).includes('node_modules'))
					.map((f) => path.join(dir, f)),
			),
		),
	];
	const rank = (f: string) => {
		const i = first.indexOf(path.basename(f));
		return i < 0 ? first.length : i;
	};
	return files.sort((a, b) => rank(a) - rank(b) || a.localeCompare(b));
}

function openProject(p: { configFileName: string; projectHandle: string; options?: Record<string, unknown> }) {
	const { options, diags } = readOptions(p.options);
	const dir = p.configFileName ? path.dirname(p.configFileName) : process.cwd();
	const wasm = options.wasm ? path.resolve(dir, options.wasm) : path.join(PKG, '..', 'flecs', 'flecs-doc.wasm');
	const project: Project = { options, dir, scripts: options.scripts.map((d) => path.resolve(dir, d)), wasm };
	projects.set(p.projectHandle, project);
	// Every transform reads the project's other scripts and the wasm, so they are configuration.
	const watched = [wasm, ...scripts(project.scripts, options.first)];
	const h = createHash('sha256').update(VERSION).update(JSON.stringify(options));
	for (const f of watched) {
		h.update(f);
		try {
			h.update(readFileSync(f));
		} catch {
			h.update('missing');
		}
	}
	return { configIdentity: h.digest('hex'), watchedFiles: watched, optionDiagnostics: diags };
}

function lineStart(src: string, line: number) {
	let at = 0;
	for (let l = 1; l < line && at >= 0; l++) at = src.indexOf('\n', at) + 1 || -1;
	return Math.max(0, at);
}

async function transform(p: { fileName: string; content: string; projectHandle: string }) {
	const project = projects.get(p.projectHandle);
	if (!project) throw new Error(`unknown project ${p.projectHandle}`);
	const src = p.content;
	const located = scan(src);
	const fail = (diags: Diag[]) => ({ text: `/// <reference path=${JSON.stringify(PRELUDE)} />\n`, extension: '.ts', mappings: [], diagnostics: diags });
	if (located.diags.length) return fail(located.diags);

	// The build's order, cut after this file, which runs with the editor's text.
	const all = scripts(project.scripts, project.options.first);
	const self = path.resolve(p.fileName);
	const at = all.indexOf(self);
	const before = at < 0 ? all : all.slice(0, at);
	const names = [...before, '(mapper marker)', self];
	const texts = [...before.map((f) => readFileSync(f, 'utf8')), `${MARK} {}\n`, src];
	const result = await run(project.wasm, texts, [...located.roots, MARK]);

	if (!result.ok) {
		const diags: Diag[] = result.errors.map((e) => {
			if (e.file !== names.length - 1) {
				const where = e.file >= 0 ? `${path.relative(project.dir, names[e.file])}:${e.line}` : 'flecs-doc';
				return { messageText: `Flecs failed in ${where}: ${e.message}`, start: 0, length: 0, code: CODE.flecs };
			}
			const ls = lineStart(src, e.line);
			const le = src.indexOf('\n', ls) < 0 ? src.length : src.indexOf('\n', ls);
			let s = ls + (e.column ?? 0);
			let len = 1;
			// Point at the name the message quotes when it is on that line: Flecs's caret marks the statement.
			const quoted = /'([^']+)'/.exec(e.message)?.[1];
			const found = quoted ? src.slice(s, le).indexOf(quoted) : -1;
			if (quoted && found >= 0) {
				s += found;
				len = quoted.length;
			}
			return { messageText: `Flecs: ${e.message}`, start: Math.min(s, src.length), length: len, code: CODE.flecs };
		});
		return fail(diags);
	}
	const mark = result.entities.find((e) => e.name === MARK && !e.parent);
	if (!mark) return fail([{ messageText: 'Flecs did not create the mapper marker entity.', start: 0, length: 0, code: CODE.disagree }]);
	const out = emit(src, located, result.entities, mark.id, project.options, PRELUDE);
	return { text: out.text, extension: '.ts', mappings: out.mappings, diagnostics: out.diags };
}

async function handle(method: string, params: unknown): Promise<unknown> {
	switch (method) {
		case 'initialize':
			return { positionEncoding: 'utf-16', diagnosticSource: 'flecs' };
		case 'openProject':
			return openProject(params as Parameters<typeof openProject>[0]);
		case 'closeProject':
			projects.delete((params as { projectHandle: string }).projectHandle);
			return null;
		case 'transform':
			return transform(params as Parameters<typeof transform>[0]);
		default:
			throw Object.assign(new Error(`unknown method ${method}`), { code: -32601 });
	}
}

// JSON-RPC with Content-Length framing. Requests are answered in order.
let buf = Buffer.alloc(0);
let queue = Promise.resolve();
const send = (msg: unknown) => {
	const body = Buffer.from(JSON.stringify(msg), 'utf8');
	process.stdout.write(Buffer.concat([Buffer.from(`Content-Length: ${body.length}\r\n\r\n`), body]));
};
process.stdin.on('data', (chunk: Buffer) => {
	buf = Buffer.concat([buf, chunk]);
	for (;;) {
		const end = buf.indexOf('\r\n\r\n');
		if (end < 0) return;
		const len = /Content-Length:\s*(\d+)/i.exec(buf.subarray(0, end).toString('ascii'));
		if (!len) throw new Error('flecs-content-mapper: message without Content-Length');
		const total = end + 4 + Number(len[1]);
		if (buf.length < total) return;
		const msg = JSON.parse(buf.subarray(end + 4, total).toString('utf8')) as { id?: string | number; method: string; params?: unknown };
		buf = buf.subarray(total);
		queue = queue.then(async () => {
			try {
				const result = await handle(msg.method, msg.params);
				if (msg.id !== undefined) send({ jsonrpc: '2.0', id: msg.id, result: result ?? null });
			} catch (e) {
				const err = e as Error & { code?: number };
				process.stderr.write(`flecs-content-mapper: ${msg.method} failed: ${err.stack ?? err}\n`);
				if (msg.id !== undefined) send({ jsonrpc: '2.0', id: msg.id, error: { code: err.code ?? -32603, message: String(err.message ?? err) } });
			}
		});
	}
});
process.stdin.on('end', () => process.exit(0));
