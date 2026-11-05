/**
 * Runs the site's Flecs scripts under flecs-doc.wasm (Flecs compiled to wasm32-wasi, flecs/) and turns
 * the entities it prints back into a tree. See content/schema.flecs for what the entities mean.
 */
import { WASI } from 'node:wasi';
import { closeSync, mkdtempSync, openSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';

export type Value = Record<string, string | number | boolean>;

/** One entity, as the build reads it to bake HTML into the snapshot (highlighted code) and find what to generate. */
export type Node = {
	/** Full path, dotted: `site.rfcs.capabilities.motivation.why`. */
	path: string;
	name: string;
	parent: Node | null;
	children: Node[];
	tags: Set<string>;
	/** Relationship to targets: `Status` to `["Draft"]`, `Cites` to `["refs.zig_io", ...]`. */
	pairs: Map<string, string[]>;
	/** Component values, by type: `P` to `{text: "..."}`. Inherited ones included. */
	values: Map<string, Value>;
	/** Every prefab this entity is an instance of, nearest first: `["Rfc", "Doc", "Page"]`. */
	isa: string[];
};

/** A multiline string as written: drop the indent the script added and the blank edges (renderer core `text.rs`). */
export function dedent(text: string): string {
	const lines = text.replace(/\r/g, '').split('\n');
	const rest = lines.slice(1).filter((l) => l.trim());
	const indent = Math.min(...rest.map((l) => l.match(/^ */)![0].length));
	const body = [lines[0], ...lines.slice(1).map((l) => l.slice(Number.isFinite(indent) ? indent : 0))];
	while (body.length && !body[0].trim()) body.shift();
	while (body.length && !body.at(-1)!.trim()) body.pop();
	return body.join('\n');
}

/** Prose: one line, whatever the script's line breaks. */
export const prose = (text: string) => text.replace(/\s*\n\s*/g, ' ').trim();

type Raw = {
	parent?: string;
	name: string;
	tags?: string[];
	pairs?: Record<string, string | string[]>;
	components?: Record<string, Value | null>;
	inherited?: Record<string, { tags?: string[]; pairs?: Record<string, string | string[]>; components?: Record<string, Value | null> }>;
};

const BUILTIN = 'flecs.core.';
const short = (id: string) => (id.startsWith(BUILTIN) ? id.slice(BUILTIN.length) : id);
const list = (v: string | string[]) => (Array.isArray(v) ? v : [v]);

/** Run `files` (absolute paths under `dir`) in one world and return the trees under `roots`. */
export async function load(dir: string, files: string[], roots: string[]): Promise<Map<string, Node>> {
	const wasmPath = path.join(import.meta.dirname, '../flecs/flecs-doc.wasm');
	const out = mkdtempSync(path.join(tmpdir(), 'flecs-doc-'));
	const outFile = path.join(out, 'tree.json');
	const errFile = path.join(out, 'err.txt');
	const stderr = openSync(errFile, 'w');
	let code: number | undefined;
	try {
		const wasi = new WASI({
			version: 'preview1',
			args: ['flecs-doc', '/o/tree.json', roots.join(','), ...files.map((f) => '/c/' + path.relative(dir, f))],
			preopens: { '/c': dir, '/o': out },
			stderr,
			returnOnExit: true,
		});
		const module = await WebAssembly.compile(readFileSync(wasmPath));
		const instance = await WebAssembly.instantiate(module, { wasi_snapshot_preview1: wasi.wasiImport });
		code = wasi.start(instance);
	} finally {
		closeSync(stderr);
	}
	const err = readFileSync(errFile, 'utf8');
	const text = readFileSync(outFile, 'utf8');
	rmSync(out, { recursive: true });
	if (code) throw new Error(`flecs-doc exited ${code}\n${err.replace(/\x1b\[[0-9;]*m/g, '')}`);
	if (err.trim()) console.warn(err);
	return build(JSON.parse(text) as Raw[]);
}

function build(raws: Raw[]): Map<string, Node> {
	const byPath = new Map<string, Node>();
	for (const raw of raws) {
		const parent = raw.parent ? byPath.get(raw.parent) ?? null : null;
		const node: Node = {
			path: raw.parent ? `${raw.parent}.${raw.name}` : raw.name,
			name: raw.name,
			parent,
			children: [],
			tags: new Set(),
			pairs: new Map(),
			values: new Map(),
			isa: [],
		};
		// Prefabs come farthest first (Page, Doc, Rfc), then the entity: the nearest value wins.
		const layers = Object.values(raw.inherited ?? {});
		for (const layer of [...layers, raw]) {
			for (const t of layer.tags ?? []) node.tags.add(short(t));
			for (const [k, v] of Object.entries(layer.components ?? {})) if (v) node.values.set(k, v);
			for (const [rel, tgt] of Object.entries(layer.pairs ?? {})) {
				const r = short(rel);
				if (r === 'IsA') continue;
				// An Exclusive relationship set on the instance replaces the prefab's.
				node.pairs.set(r, layer === raw ? list(tgt) : [...(node.pairs.get(r) ?? []), ...list(tgt)]);
			}
		}
		node.isa = Object.keys(raw.inherited ?? {}).reverse();
		if (parent) parent.children.push(node);
		byPath.set(node.path, node);
	}
	return byPath;
}
