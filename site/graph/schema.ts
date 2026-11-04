/**
 * The code graph `blaze index` writes, and its loader: the one place the viewer knows the file's shape.
 *
 * Draft, version 1 (2026-11-03). The scip lane owns the format; when `blaze index` changes it, change this file (the
 * types, the kind lists and `parseGraph`) and bump `GRAPH_VERSION`. Everything else in site/graph/ reads the `Graph`
 * this returns, never the raw JSON.
 *
 *   {
 *     "format": "blaze-graph", "version": 1,
 *     "tree": { "commit": "1d96f77a9f", "dirty": false },          optional: the exact tree it was indexed from
 *     "nodes": [ { "id": "<SCIP symbol>", "kind": "struct", "crate": "blaze-query", "name": "Db",
 *                  "path": "blaze_query::db::Db", "file": "crates/blaze-query/src/db.rs", "line": 41, "doc": "..." } ],
 *     "edges": [ { "from": "<id>", "to": "<id>", "kind": "calls", "count": 3 } ],
 *     "facts": [ { "kind": "call", "roles": { "caller": "<id>", "callee": "<id>" }, "file": "...", "line": 88 },
 *                { "kind": "impl", "roles": { "type": "<id>", "trait": "<id>", "impl": "<id>" } } ]
 *   }
 *
 * `nodes` are symbols. `edges` are binary relations, one per (from, to, kind), `count` how many times it occurs (call
 * sites, references). `facts` are hyperedges, joining more than two symbols (a call site: caller, callee and where; an
 * impl: type, trait and the impl block); the viewer draws each as a small connector node linked to its roles.
 *
 * The loader refuses what it doesn't know (another format or version, an unknown field or kind, a dangling id, a
 * duplicate id), with an error naming the item and the rule, so a schema change shows up as a clear message, not a
 * quietly half-drawn graph.
 */

export const GRAPH_FORMAT = 'blaze-graph';
export const GRAPH_VERSION = 1;

export const NODE_KINDS = [
	'crate',
	'module',
	'struct',
	'enum',
	'union',
	'trait',
	'fn',
	'method',
	'impl',
	'const',
	'static',
	'type',
	'macro',
	'field',
	'variant',
] as const;
export type NodeKind = (typeof NODE_KINDS)[number];

export const EDGE_KINDS = ['contains', 'defines', 'references', 'implements', 'calls'] as const;
export type EdgeKind = (typeof EDGE_KINDS)[number];

/** Each fact kind and its roles: the ones every fact of that kind has, and the ones it may have. */
export const FACT_ROLES = {
	call: { required: ['caller', 'callee'], optional: [] },
	impl: { required: ['type', 'impl'], optional: ['trait'] },
} as const satisfies Record<string, { required: readonly string[]; optional: readonly string[] }>;
export type FactKind = keyof typeof FACT_ROLES;
export const FACT_KINDS = Object.keys(FACT_ROLES) as FactKind[];

export interface GraphNode {
	/** The SCIP symbol: unique in the file. */
	id: string;
	kind: NodeKind;
	crate: string;
	name: string;
	/** The Rust path, `blaze_query::db::Db`. */
	path?: string;
	/** Repository-relative. */
	file?: string;
	/** 1-based. */
	line?: number;
	/** The first paragraph of its doc comment. */
	doc?: string;
}

export interface GraphEdge {
	from: string;
	to: string;
	kind: EdgeKind;
	/** At least 1. */
	count: number;
}

export interface GraphFact {
	kind: FactKind;
	/** Role name to node id. */
	roles: Record<string, string>;
	file?: string;
	line?: number;
}

export interface GraphTree {
	commit?: string;
	dirty?: boolean;
}

export interface Graph {
	tree?: GraphTree;
	nodes: GraphNode[];
	edges: GraphEdge[];
	facts: GraphFact[];
}

/** A refusal: `at` is where in the file (`nodes[12].kind`), `rule` what it broke. */
export class GraphFileError extends Error {
	constructor(
		readonly at: string,
		readonly rule: string,
	) {
		super(`${at}: ${rule}`);
		this.name = 'GraphFileError';
	}
}

type Obj = Record<string, unknown>;

function object(v: unknown, at: string): Obj {
	if (typeof v !== 'object' || v === null || Array.isArray(v)) throw new GraphFileError(at, 'expected an object');
	return v as Obj;
}

function onlyFields(o: Obj, at: string, fields: readonly string[]) {
	for (const k of Object.keys(o)) if (!fields.includes(k)) throw new GraphFileError(`${at}.${k}`, `unknown field (known: ${fields.join(', ')})`);
}

function str(o: Obj, k: string, at: string): string {
	const v = o[k];
	if (typeof v !== 'string' || v === '') throw new GraphFileError(`${at}.${k}`, 'expected a non-empty string');
	return v;
}

function optStr(o: Obj, k: string, at: string): string | undefined {
	return o[k] === undefined ? undefined : str(o, k, at);
}

function optPositiveInt(o: Obj, k: string, at: string): number | undefined {
	const v = o[k];
	if (v === undefined) return undefined;
	if (typeof v !== 'number' || !Number.isInteger(v) || v < 1) throw new GraphFileError(`${at}.${k}`, 'expected an integer of at least 1');
	return v;
}

function array(o: Obj, k: string, at: string): unknown[] {
	const v = o[k];
	if (!Array.isArray(v)) throw new GraphFileError(`${at}.${k}`, 'expected an array');
	return v;
}

function oneOf<T extends string>(o: Obj, k: string, at: string, allowed: readonly T[]): T {
	const v = str(o, k, at);
	if (!(allowed as readonly string[]).includes(v)) throw new GraphFileError(`${at}.${k}`, `unknown ${k} "${v}" (known: ${allowed.join(', ')})`);
	return v as T;
}

/** Checks a parsed JSON value against version 1 and returns it typed, or throws a `GraphFileError`. */
export function parseGraph(value: unknown): Graph {
	const top = object(value, '$');
	onlyFields(top, '$', ['format', 'version', 'tree', 'nodes', 'edges', 'facts']);
	if (top.format !== GRAPH_FORMAT) throw new GraphFileError('$.format', `expected "${GRAPH_FORMAT}", got ${JSON.stringify(top.format)}`);
	if (top.version !== GRAPH_VERSION)
		throw new GraphFileError('$.version', `this viewer reads version ${GRAPH_VERSION}, the file is ${JSON.stringify(top.version)}`);

	let tree: GraphTree | undefined;
	if (top.tree !== undefined) {
		const t = object(top.tree, '$.tree');
		onlyFields(t, '$.tree', ['commit', 'dirty']);
		if (t.dirty !== undefined && typeof t.dirty !== 'boolean') throw new GraphFileError('$.tree.dirty', 'expected a boolean');
		tree = { commit: optStr(t, 'commit', '$.tree'), dirty: t.dirty as boolean | undefined };
	}

	const ids = new Set<string>();
	const nodes = array(top, 'nodes', '$').map((raw, i): GraphNode => {
		const at = `$.nodes[${i}]`;
		const o = object(raw, at);
		onlyFields(o, at, ['id', 'kind', 'crate', 'name', 'path', 'file', 'line', 'doc']);
		const id = str(o, 'id', at);
		if (ids.has(id)) throw new GraphFileError(`${at}.id`, `duplicate id ${JSON.stringify(id)}`);
		ids.add(id);
		return {
			id,
			kind: oneOf(o, 'kind', at, NODE_KINDS),
			crate: str(o, 'crate', at),
			name: str(o, 'name', at),
			path: optStr(o, 'path', at),
			file: optStr(o, 'file', at),
			line: optPositiveInt(o, 'line', at),
			doc: optStr(o, 'doc', at),
		};
	});

	const known = (id: string, at: string) => {
		if (!ids.has(id)) throw new GraphFileError(at, `no node has id ${JSON.stringify(id)}`);
		return id;
	};

	const edges = array(top, 'edges', '$').map((raw, i): GraphEdge => {
		const at = `$.edges[${i}]`;
		const o = object(raw, at);
		onlyFields(o, at, ['from', 'to', 'kind', 'count']);
		const count = optPositiveInt(o, 'count', at);
		if (count === undefined) throw new GraphFileError(`${at}.count`, 'missing');
		return { from: known(str(o, 'from', at), `${at}.from`), to: known(str(o, 'to', at), `${at}.to`), kind: oneOf(o, 'kind', at, EDGE_KINDS), count };
	});

	const facts = (top.facts === undefined ? [] : array(top, 'facts', '$')).map((raw, i): GraphFact => {
		const at = `$.facts[${i}]`;
		const o = object(raw, at);
		onlyFields(o, at, ['kind', 'roles', 'file', 'line']);
		const kind = oneOf(o, 'kind', at, FACT_KINDS);
		const spec = FACT_ROLES[kind];
		const r = object(o.roles, `${at}.roles`);
		const allowed: readonly string[] = [...spec.required, ...spec.optional];
		onlyFields(r, `${at}.roles`, allowed);
		const roles: Record<string, string> = {};
		for (const role of spec.required) if (r[role] === undefined) throw new GraphFileError(`${at}.roles.${role}`, `a ${kind} fact needs this role`);
		for (const role of allowed) if (r[role] !== undefined) roles[role] = known(str(r, role, `${at}.roles`), `${at}.roles.${role}`);
		return { kind, roles, file: optStr(o, 'file', at), line: optPositiveInt(o, 'line', at) };
	});

	return { tree, nodes, edges, facts };
}
