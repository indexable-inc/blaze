/**
 * Writes graph/sample.json: a hand-made code graph of about 300 symbols from blaze's real crates, so the viewer works
 * before `blaze index` exists. The crates, modules and items are written out below; the relations between them
 * (calls, references, impls) come from a seeded generator, so the file is the same on every run.
 *
 *   bun graph/sample.ts
 */
import { writeFileSync } from 'node:fs';
import path from 'node:path';
import { GRAPH_FORMAT, GRAPH_VERSION, parseGraph, type GraphEdge, type GraphFact, type GraphNode, type NodeKind } from './schema';

/** Item spec: `S:Name` struct (methods after `=`, comma separated; the first two are kept), `E:Name` enum, `T:Name` trait, `F:name` fn, `C:NAME` const. */
type Crate = { name: string; deps: string[]; modules: Record<string, string[]> };

const CRATES: Crate[] = [
	{
		name: 'blaze-hash',
		deps: [],
		modules: {
			key: ['S:KeyHasher=new,finish,write', 'S:LazyKey=get', 'F:hash_one'],
			table: ['S:Map=new_in,get,insert,remove', 'S:Set=new_in,contains,insert', 'S:FixedState=build_hasher'],
			blake3: ['T:Blake3', 'S:Blake3Hasher=update,finalize', 'F:random'],
		},
	},
	{
		name: 'blaze-ids',
		deps: ['blaze-hash'],
		modules: {
			lib: ['S:Interner=intern,lookup,len', 'S:Id=index', 'T:Idx', 'C:MAX_ID'],
		},
	},
	{
		name: 'blaze-vfs',
		deps: ['blaze-hash', 'blaze-ids'],
		modules: {
			journal: ['S:Journal=open,append,replay', 'E:Op', 'E:OpError', 'F:collect_if_due'],
			image: ['S:Image=load,section,bytes', 'E:ImageError', 'E:ImageUse'],
			disk: ['S:Disk=read,write,changed', 'E:Watched', 'F:apply_files'],
			paths: ['S:VPath=join,parent,as_str', 'F:normalize'],
			lib: ['S:Vfs=apply,commit,collect', 'S:Change', 'E:ChangeKind', 'S:Commit', 'E:CommitError'],
		},
	},
	{
		name: 'blaze-cas',
		deps: ['blaze-hash', 'blaze-vfs'],
		modules: {
			model: ['S:Entry=key,size', 'E:Heat', 'E:Origin', 'S:Config=from_env'],
			chunk: ['S:Chunk=split,join', 'F:chunks'],
			disk: ['S:Store=open,add,content_of,commit_steps', 'S:Corrupt'],
			peers: ['S:Client=ask,fetch', 'E:Answer', 'E:Fetched', 'E:Asked'],
			index: ['S:Index=lookup,insert,counts'],
		},
	},
	{
		name: 'blaze-query',
		deps: ['blaze-hash', 'blaze-ids', 'blaze-cas'],
		modules: {
			db: ['S:Db=new,input,query,revision,cancel', 'E:DbError', 'E:Cancelled', 'T:Database'],
			memo: ['S:Memo=claim,complete,abandon,verify', 'E:Filled', 'E:Outcome'],
			table: ['S:Table=get,insert,slot', 'E:SlotError'],
			sync: ['S:Fiber=park,wake', 'F:capture_reads', 'F:carry_with_fiber_jobs'],
			segment: ['S:Segment=attach,collect', 'E:CycleKind', 'T:Query'],
		},
	},
	{
		name: 'blaze-source',
		deps: ['blaze-vfs', 'blaze-query'],
		modules: {
			lib: ['S:SourceFile=text,line_index', 'F:source_text', 'F:parse_file'],
			span: ['S:Span=join,lo,hi', 'S:LineIndex=line_col'],
		},
	},
	{
		name: 'blaze-items',
		deps: ['blaze-query', 'blaze-source', 'blaze-ids'],
		modules: {
			facts: ['E:Fact', 'F:facts_of', 'F:auto_traits'],
			index: ['S:ItemIndex=lookup,scope', 'E:ScopeEntry'],
			key: ['E:ItemId', 'E:ItemKind', 'F:anon_const_key'],
			lowering: ['S:Lowerer=lower_item,lower_impl', 'E:ImplEntry'],
			goals: ['F:blanket_impls', 'S:Goal=solve'],
		},
	},
	{
		name: 'blaze-resolve',
		deps: ['blaze-hash', 'blaze-vfs'],
		modules: {
			resolve: ['S:Resolve=activated_features,deps,sort', 'E:ConflictReason', 'F:build_feature_map'],
			lockfile: ['F:are_equal_lockfiles', 'S:Lockfile=load,write'],
			registry: ['S:Registry=query,block_on', 'E:IndexSummary', 'E:QueryKind'],
			package_id: ['S:PackageId=name,version', 'E:DepKind', 'E:FeatureValue'],
		},
	},
	{
		name: 'blaze-diagnostics',
		deps: ['blaze-source', 'blaze-query'],
		modules: {
			emit: ['S:Emitter=emit,flush', 'F:render'],
			lints: ['S:LintStore=register,level_of', 'E:Level'],
			cx: ['S:Cx=span,error,warn', 'E:Phase'],
			lib: ['S:Diagnostic=with_note,with_label', 'E:Code', 'T:IntoDiagnostic'],
		},
	},
	{
		name: 'blaze-codegen',
		deps: ['blaze-items', 'blaze-query', 'blaze-cas', 'blaze-diagnostics'],
		modules: {
			backend: ['E:Backend', 'E:Arch', 'F:arch_of', 'T:CodegenBackend'],
			driver: ['S:Driver=run,codegen_unit,emit_object', 'E:Decision'],
			dedupe: ['S:Dedupe=insert,hit', 'E:DedupeError'],
			flight: ['S:Flight=ask,asked,land'],
			exec: ['S:Exec=spawn,wait', 'E:ExecError'],
		},
	},
	{
		name: 'blaze-link',
		deps: ['blaze-cas', 'blaze-codegen', 'blaze-hash'],
		modules: {
			elf: ['S:ElfWriter=section,symbol,finish', 'E:Format'],
			layout: ['S:Layout=place,align,addr_of', 'E:Prot'],
			commit: ['F:commit_output', 'E:Refusal'],
			intern: ['S:Strtab=add,offset'],
			lib: ['S:Bind', 'E:BindKind', 'E:OutputKind', 'F:link'],
		},
	},
	{
		name: 'blaze-live',
		deps: ['blaze-codegen', 'blaze-link', 'blaze-query'],
		modules: {
			patch: ['S:Patch=apply,encode,decode', 'S:PatchRef'],
			slots: ['S:Slots=reserve,swap,generation'],
			ask: ['S:Live=ask,apply_as_they_come', 'E:Message', 'E:Outcome'],
		},
	},
	{
		name: 'blaze-wasi',
		deps: ['blaze-vfs', 'blaze-hash'],
		modules: {
			calls: ['S:Fibers=spawn,join', 'S:Filestat', 'S:Errno', 'F:path_open', 'F:fd_read'],
			seed: ['T:Embed', 'T:EmbedSeed', 'F:seed'],
		},
	},
	{
		name: 'blaze-macro-eval',
		deps: ['blaze-wasi', 'blaze-items', 'blaze-query'],
		modules: {
			lib: ['S:MacroHost=expand,instantiate', 'E:MacroKind', 'F:expand_derive'],
			sandbox: ['S:Sandbox=limits,run'],
		},
	},
	{
		name: 'blaze-engine',
		deps: ['blaze-query', 'blaze-codegen', 'blaze-link', 'blaze-live', 'blaze-resolve', 'blaze-macro-eval', 'blaze-diagnostics'],
		modules: {
			main: ['F:main'],
			program: ['S:Program=build,check,test', 'E:Command'],
			pin: ['F:pin_toolchain'],
		},
	},
];

/** A small seeded PRNG (mulberry32), so the sample is the same on every run. */
function rng(seed: number) {
	let a = seed >>> 0;
	return () => {
		a = (a + 0x6d2b79f5) >>> 0;
		let t = a;
		t = Math.imul(t ^ (t >>> 15), t | 1);
		t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
		return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
	};
}
const rand = rng(0x5eed);
const pick = <T>(xs: readonly T[]): T => xs[Math.floor(rand() * xs.length)];
const between = (lo: number, hi: number) => lo + Math.floor(rand() * (hi - lo + 1));

const nodes: GraphNode[] = [];
const edges = new Map<string, GraphEdge>();
const facts: GraphFact[] = [];

const rustName = (c: string) => c.replaceAll('-', '_');
const symbol = (crate: string, desc: string) => `scip-blaze cargo ${crate} . ${desc}`;

function node(n: GraphNode): string {
	nodes.push(n);
	return n.id;
}
function edge(from: string, to: string, kind: GraphEdge['kind'], count = 1) {
	const k = `${from}\n${to}\n${kind}`;
	const e = edges.get(k);
	if (e) e.count += count;
	else edges.set(k, { from, to, kind, count });
}

/** Per crate: its callables, its types and its traits, for the relations below. */
type Items = { callables: string[]; types: string[]; traits: string[] };
const byCrate = new Map<string, Items>();

const KIND: Record<string, NodeKind> = { S: 'struct', E: 'enum', T: 'trait', F: 'fn', C: 'const' };

for (const c of CRATES) {
	const items: Items = { callables: [], types: [], traits: [] };
	byCrate.set(c.name, items);
	const r = rustName(c.name);
	const crateId = node({ id: symbol(c.name, '/'), kind: 'crate', crate: c.name, name: c.name, path: r, file: `crates/${c.name}/src/lib.rs`, line: 1 });
	for (const [mod, specs] of Object.entries(c.modules)) {
		const file = `crates/${c.name}/src/${mod}.rs`;
		const modPath = mod === 'lib' ? r : `${r}::${mod}`;
		const modId = mod === 'lib' ? crateId : node({ id: symbol(c.name, `${mod}/`), kind: 'module', crate: c.name, name: mod, path: modPath, file, line: 1 });
		if (modId !== crateId) edge(crateId, modId, 'contains');
		let line = 10;
		for (const spec of specs) {
			const [head, methodList] = spec.split('=');
			const [tag, name] = head.split(':');
			const kind = KIND[tag];
			const desc = mod === 'lib' ? '' : `${mod}/`;
			const id = node({
				id: symbol(c.name, `${desc}${name}${kind === 'fn' ? '().' : kind === 'const' ? '.' : '#'}`),
				kind,
				crate: c.name,
				name,
				path: `${modPath}::${name}`,
				file,
				line: (line += between(4, 40)),
			});
			edge(modId, id, 'contains');
			if (kind === 'fn') items.callables.push(id);
			if (kind === 'struct' || kind === 'enum') items.types.push(id);
			if (kind === 'trait') items.traits.push(id);
			if (methodList) {
				// Inherent methods hang off their type (`defines`); only trait impls are drawn as impl blocks.
				for (const m of methodList.split(',').slice(0, 2)) {
					const mid = node({ id: symbol(c.name, `${desc}${name}#${m}().`), kind: 'method', crate: c.name, name: m, path: `${modPath}::${name}::${m}`, file, line: (line += between(5, 25)) });
					edge(id, mid, 'defines');
					items.callables.push(mid);
				}
			}
		}
	}
}

const reach = (c: Crate) => [c.name, ...c.deps];
const fileOf = (id: string) => nodes.find((n) => n.id === id)!;

// Calls and references: mostly within a crate, some into its dependencies. Cross-crate call sites are also facts.
for (const c of CRATES) {
	const own = byCrate.get(c.name)!;
	for (const caller of own.callables) {
		for (let k = between(1, 2); k > 0; k--) {
			const into = rand() < 0.8 || !c.deps.length ? c.name : pick(c.deps);
			const callee = pick(byCrate.get(into)!.callables);
			if (callee === caller) continue;
			const count = between(1, rand() < 0.15 ? 24 : 4);
			edge(caller, callee, 'calls', count);
			if (into !== c.name && rand() < 0.6) {
				const site = fileOf(caller);
				facts.push({ kind: 'call', roles: { caller, callee }, file: site.file, line: (site.line ?? 1) + between(1, 30) });
			}
		}
		if (rand() < 0.6) {
			const into = rand() < 0.7 ? c.name : pick(reach(c));
			const types = byCrate.get(into)!.types;
			if (types.length) edge(caller, pick(types), 'references', between(1, 9));
		}
	}
}

// Trait impls: a type in a crate implements a trait from its own crate or a dependency, as an impl block and a fact.
let implNo = 0;
for (const c of CRATES) {
	const own = byCrate.get(c.name)!;
	const traits = reach(c).flatMap((d) => byCrate.get(d)!.traits);
	if (!traits.length) continue;
	for (const type of own.types) {
		if (rand() > 0.3) continue;
		const trait = pick(traits);
		const t = fileOf(type);
		const tr = fileOf(trait);
		const impl = node({
			id: symbol(c.name, `impl#${t.name}#${tr.name}#${implNo++}`),
			kind: 'impl',
			crate: c.name,
			name: `impl ${tr.name} for ${t.name}`,
			path: t.path,
			file: t.file,
			line: (t.line ?? 1) + between(20, 80),
		});
		edge(impl, trait, 'implements');
		facts.push({ kind: 'impl', roles: { type, trait, impl }, file: t.file, line: fileOf(impl).line });
	}
}

const out = {
	format: GRAPH_FORMAT,
	version: GRAPH_VERSION,
	tree: { commit: 'sample', dirty: false },
	nodes,
	edges: [...edges.values()],
	facts,
};
parseGraph(JSON.parse(JSON.stringify(out)));
const file = path.join(import.meta.dirname, 'sample.json');
writeFileSync(file, `${JSON.stringify(out)}\n`);
console.log(`${file}: ${nodes.length} nodes, ${out.edges.length} edges, ${facts.length} facts`);
