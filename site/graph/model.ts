/**
 * The graph as the viewer draws it: symbols and fact connectors as one indexed node list, edges merged per pair of
 * nodes (a pair related by calls and references is one line), degree, adjacency, colours and sizes.
 */
import type { EdgeKind, FactKind, Graph, GraphFact, GraphNode, NodeKind } from './schema';

export type ViewNode =
	| { i: number; type: 'symbol'; node: GraphNode; label: string; search: string }
	| { i: number; type: 'fact'; fact: GraphFact; label: string; search: string };

/** One line between two nodes: every relation between them, either way. */
export interface ViewLink {
	a: number;
	b: number;
	/** Sum of the counts of every relation on it (1 for each fact role). */
	count: number;
	relations: { from: number; to: number; kind: EdgeKind | `role:${string}`; count: number }[];
}

export interface ViewGraph {
	source: Graph;
	nodes: ViewNode[];
	links: ViewLink[];
	edgeSources: Uint32Array;
	edgeTargets: Uint32Array;
	degree: Uint32Array;
	/** CSR adjacency: the neighbours of node i are `adj[adjStart[i] .. adjStart[i+1]]`, links `adjLink[...]`. */
	adjStart: Uint32Array;
	adj: Uint32Array;
	adjLink: Uint32Array;
	maxCount: number;
	crates: string[];
}

export function buildView(g: Graph): ViewGraph {
	const nodes: ViewNode[] = g.nodes.map((node, i) => ({
		i,
		type: 'symbol',
		node,
		label: node.name,
		search: `${node.name} ${node.path ?? ''} ${node.crate}`.toLowerCase(),
	}));
	const index = new Map<string, number>(g.nodes.map((n, i) => [n.id, i]));
	for (const fact of g.facts) {
		const names = Object.values(fact.roles).map((id) => g.nodes[index.get(id)!].name);
		nodes.push({ i: nodes.length, type: 'fact', fact, label: `${fact.kind}: ${names.join(' · ')}`, search: '' });
	}

	const byPair = new Map<string, ViewLink>();
	const relate = (from: number, to: number, kind: ViewLink['relations'][number]['kind'], count: number) => {
		if (from === to) return;
		const a = Math.min(from, to);
		const b = Math.max(from, to);
		const key = `${a},${b}`;
		let link = byPair.get(key);
		if (!link) byPair.set(key, (link = { a, b, count: 0, relations: [] }));
		link.count += count;
		link.relations.push({ from, to, kind, count });
	};
	for (const e of g.edges) relate(index.get(e.from)!, index.get(e.to)!, e.kind, e.count);
	g.facts.forEach((fact, f) => {
		const at = g.nodes.length + f;
		for (const [role, id] of Object.entries(fact.roles)) relate(at, index.get(id)!, `role:${role}`, 1);
	});
	const links = [...byPair.values()];

	const n = nodes.length;
	const degree = new Uint32Array(n);
	for (const l of links) {
		degree[l.a]++;
		degree[l.b]++;
	}
	const adjStart = new Uint32Array(n + 1);
	for (let i = 0; i < n; i++) adjStart[i + 1] = adjStart[i] + degree[i];
	const fill = adjStart.slice(0, n);
	const adj = new Uint32Array(adjStart[n]);
	const adjLink = new Uint32Array(adjStart[n]);
	links.forEach((l, li) => {
		adj[fill[l.a]] = l.b;
		adjLink[fill[l.a]++] = li;
		adj[fill[l.b]] = l.a;
		adjLink[fill[l.b]++] = li;
	});

	return {
		source: g,
		nodes,
		links,
		edgeSources: Uint32Array.from(links, (l) => l.a),
		edgeTargets: Uint32Array.from(links, (l) => l.b),
		degree,
		adjStart,
		adj,
		adjLink,
		maxCount: links.reduce((m, l) => Math.max(m, l.count), 1),
		crates: [...new Set(g.nodes.map((x) => x.crate))].sort(),
	};
}

/** World-space radius: grows with the square root of degree, as Obsidian's does; facts are small connectors. */
export function radiusOf(v: ViewGraph, i: number): number {
	if (v.nodes[i].type === 'fact') return 2.2;
	return Math.min(24, 2.5 + 2.2 * Math.sqrt(v.degree[i]));
}

/** Line opacity from how often the relation occurs, on a log scale. */
export function linkStrength(v: ViewGraph, li: number): number {
	return 0.15 + 0.85 * (Math.log1p(v.links[li].count) / Math.log1p(v.maxCount));
}

/** Categorical colours, tuned for a dark background. */
const PALETTE = [
	'#7aa2f7', '#f7768e', '#9ece6a', '#e0af68', '#bb9af7', '#2ac3de', '#ff9e64', '#73daca',
	'#c0caf5', '#f4a3c4', '#b4e07a', '#d7b4ff', '#89ddff', '#e6c07b', '#ff7a93', '#a3be8c',
	'#88c0d0', '#ebcb8b', '#d08770', '#b48ead',
];

export const KIND_COLOURS: Record<NodeKind, string> = {
	crate: '#e0af68',
	module: '#9aa5ce',
	struct: '#7aa2f7',
	enum: '#2ac3de',
	union: '#89ddff',
	trait: '#bb9af7',
	fn: '#9ece6a',
	method: '#73daca',
	impl: '#f7768e',
	const: '#ff9e64',
	static: '#ff9e64',
	type: '#c0caf5',
	macro: '#f4a3c4',
	field: '#565f89',
	variant: '#565f89',
};
export const FACT_COLOURS: Record<FactKind, string> = { call: '#a9b1d6', impl: '#ff7a93' };

export type ColourBy = 'kind' | 'crate';

export function colourOf(v: ViewGraph, i: number, by: ColourBy): string {
	const n = v.nodes[i];
	if (n.type === 'fact') return FACT_COLOURS[n.fact.kind];
	if (by === 'kind') return KIND_COLOURS[n.node.kind];
	return PALETTE[v.crates.indexOf(n.node.crate) % PALETTE.length];
}

/** What the legend lists for a colouring: label and colour, only those present in this graph. */
export function legendOf(v: ViewGraph, by: ColourBy): { label: string; colour: string }[] {
	if (by === 'crate') return v.crates.map((c, k) => ({ label: c, colour: PALETTE[k % PALETTE.length] }));
	const kinds = new Set(v.source.nodes.map((n) => n.kind));
	const out: { label: string; colour: string }[] = (Object.keys(KIND_COLOURS) as NodeKind[]).filter((k) => kinds.has(k)).map((k) => ({ label: k, colour: KIND_COLOURS[k] }));
	for (const k of new Set(v.source.facts.map((f) => f.kind))) out.push({ label: `${k} fact`, colour: FACT_COLOURS[k] });
	return out;
}
