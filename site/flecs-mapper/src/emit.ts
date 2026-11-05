/**
 * The TypeScript tsc checks in place of a .flecs file, built from what Flecs produced for it and
 * mapped back to the spans scan.ts located. Nothing here decides what a name means: every path,
 * component and value comes from Flecs's output, and anything the scanner located that Flecs does
 * not confirm is a 91003 diagnostic, never a guess.
 *
 * The virtual file is a script (no import or export), so what it declares is global and every
 * .flecs file in the program sees the others:
 *
 *   - each named entity `a.b.c` the file declares is `declare namespace __e.a.b.c { var __: 0 }`, with
 *     `var __has_X: 0` for each tag or component the file gives it and `var __isa: ..` for its bases;
 *   - `struct P(text: string)` adds `"P"` to `__Structs` (named form) and `__Tuples` (positional),
 *     typed from Flecs's member types;
 *   - `prefab Doc : Page` adds `"Doc"` to `__Prefabs`;
 *   - each name the file uses (kind, tag, base, pair, entity value, member type, [[ref]]) is the
 *     expression `__e.<path Flecs resolved it to>`, so it stays checked when the file that defines
 *     it changes: an entity that goes away is TS2339 at the name;
 *   - each component value is `(<value as written>) satisfies __Named<"P">`, so a number where Flecs
 *     silently converted it to a string is a type error at the field;
 *   - a pair whose relationship has a rule (Cites: Ref, DependsOn: Page) is
 *     `__target<"Ref">(__e.<target>)`.
 */
import type { FlecsEntity, FlecsValue } from './run.ts';
import { CODE, type Diag, type Item, type Scan, type Tok, type Value } from './scan.ts';

/** [virtualStart, virtualLength, originalStart, originalLength, kind, features?] */
export type Mapping = [number, number, number, number, number, number?];

export type Options = {
	/** Relationship to the tag, component or prefab its target must have or inherit. */
	relationships: Record<string, string>;
	/** The entity `[[name]]` and `[label][name]` resolve under, or null to not check them. */
	refs: string | null;
};

const RESERVED = new Set(
	('break case catch class const continue debugger default delete do else enum export extends false finally for ' +
		'function if import in instanceof new null return super switch this throw true try typeof var void while with ' +
		'implements interface let package private protected public static yield await').split(' '),
);
/** A path TS can name as a namespace. Entities whose path is not one are not declared. */
const okPath = (p: string) => p.split('.').every((s) => /^[A-Za-z_][A-Za-z0-9_]*$/.test(s) && !RESERVED.has(s));
const marker = (p: string) => p.replace(/\./g, '$');
const NUMBER = new Set(['byte', 'u8', 'u16', 'u32', 'u64', 'uptr', 'i8', 'i16', 'i32', 'i64', 'iptr', 'f32', 'f64']);

class Out {
	text = '';
	maps: Mapping[] = [];
	src: string;
	constructor(src: string) {
		this.src = src;
	}
	raw(t: string) {
		this.text += t;
	}
	verb(s: number, e: number) {
		if (e > s) this.maps.push([this.text.length, e - s, s, e - s, 0]);
		this.text += this.src.slice(s, e);
	}
	/** Synthesized text standing for original [s, e). No language features: it is not what was written. */
	atom(t: string, s: number, e: number) {
		if (t.length) this.maps.push([this.text.length, t.length, s, e - s, 1, 0]);
		this.text += t;
	}
}

const asList = (v: string | string[] | undefined) => (v === undefined ? [] : Array.isArray(v) ? v : [v]);
/** The Flecs path among `paths` that the name `t` as written refers to. */
const match = (paths: Iterable<string>, t: string) => {
	for (const p of paths) if (p === t || p.endsWith('.' + t)) return p;
	return null;
};

export function emit(src: string, scan: Scan, tree: FlecsEntity[], markId: number, options: Options, prelude: string) {
	const diags: Diag[] = [];
	const loud = (s: number, e: number, msg: string, code: number = CODE.disagree) =>
		diags.push({ messageText: msg, start: s, length: Math.max(0, e - s), code });

	const byPath = new Map<string, FlecsEntity>();
	const children = new Map<string, FlecsEntity[]>();
	for (const e of tree) {
		const p = e.parent ? `${e.parent}.${e.name}` : e.name;
		byPath.set(p, e);
		if (e.parent) children.set(e.parent, [...(children.get(e.parent) ?? []), e]);
	}

	// Each located entity, as Flecs's path. Anonymous ones are the n-th anonymous child this file
	// created under the same parent, in creation (id) order.
	const real = new Map<string, string | null>();
	for (const ent of scan.entities.values()) {
		const at = ent.name ?? { s: 0, e: 0 };
		const parent = ent.parent === null ? '' : real.get(ent.parent);
		if (parent === undefined || parent === null) {
			real.set(ent.key, null);
			continue;
		}
		let p: string | null;
		if (ent.anon >= 0) {
			if (!parent) {
				loud(at.s, at.e, 'flecs-content-mapper does not support anonymous entities at the root.', CODE.unsupported);
				p = null;
			} else {
				const anon = (children.get(parent) ?? []).filter((c) => c.name.startsWith('#') && c.id > markId).sort((a, b) => a.id - b.id);
				p = anon[ent.anon] ? `${parent}.${anon[ent.anon].name}` : null;
				if (!p) loud(at.s, at.e, `Flecs created no anonymous entity #${ent.anon + 1} under ${parent} here.`);
			}
		} else {
			p = parent ? `${parent}.${ent.name!.v}` : ent.name!.v;
			if (!byPath.has(p)) {
				loud(at.s, at.e, `Flecs has no entity ${p}, but this file names it.`);
				p = null;
			}
		}
		real.set(ent.key, p);
	}

	// Everything Flecs created while running this file must have been located.
	const located = new Set([...real.values()].filter((p) => p !== null));
	const structs = new Set(tree.filter((e) => e.components?.['flecs.meta.struct']).map((e) => (e.parent ? `${e.parent}.${e.name}` : e.name)));
	const skipped = scan.items.filter((it) => it.k === 'skipped');
	for (const e of tree) {
		const p = e.parent ? `${e.parent}.${e.name}` : e.name;
		if (e.id <= markId || located.has(p) || p === '__flecs_mapper_mark') continue;
		if (e.parent && structs.has(e.parent)) continue; // struct members
		const where = skipped[0] ?? { s: 0, e: 0, what: '' };
		loud(where.s, where.e, `Flecs created ${p} in this file, but flecs-content-mapper did not find where${where.what ? ` (it does not model ${where.what})` : ''}; its values are not checked.`);
	}

	const out = new Out(src);
	out.raw(`/// <reference path=${JSON.stringify(prelude)} />\n`);
	const body = new Out(src);

	/** `__e.<path>` for the name `t`, the written part copied verbatim. False when TS cannot name it. */
	const ref = (o: Out, path: string, t: Tok): boolean => {
		if (path.startsWith('flecs.') || !okPath(path)) return false;
		const first = t.v.indexOf('.');
		o.atom(`__e.${path.slice(0, path.length - t.v.length)}`, t.s, first < 0 ? t.e : t.s + first);
		o.verb(t.s, t.e);
		return true;
	};

	const has = new Map<string, Set<string>>();
	const isa = new Map<string, Set<string>>();
	const add = (m: Map<string, Set<string>>, k: string, v: string) => m.set(k, (m.get(k) ?? new Set()).add(v));

	/** Compare a value as written with Flecs's, and write it as TS. */
	const value = (v: Value, flecs: unknown, at: string): void => {
		const mismatch = (s: number, e: number, what: string) =>
			loud(s, e, `Flecs stored ${JSON.stringify(flecs)} for ${at}, not ${what}.`);
		switch (v.k) {
			case 'str': {
				if (flecs !== v.tok.v && String(flecs) !== v.tok.v) mismatch(v.tok.s, v.tok.e, JSON.stringify(v.tok.v));
				const lit = JSON.stringify(v.tok.v);
				if (lit === src.slice(v.tok.s, v.tok.e)) body.verb(v.tok.s, v.tok.e);
				else body.atom(lit, v.tok.s, v.tok.e);
				if (typeof flecs === 'string') strings.push({ tok: v.tok, text: flecs });
				return;
			}
			case 'num': {
				const text = src.slice(v.s, v.e);
				const same = typeof flecs === 'number' ? Math.abs(flecs - v.n) <= 1e-6 * Math.max(1, Math.abs(v.n)) : String(flecs) === text;
				if (!same) mismatch(v.s, v.e, text);
				if (String(v.n) === text || /^-?[0-9.eE+-]+$/.test(text)) body.verb(v.s, v.e);
				else body.atom(String(v.n), v.s, v.e);
				return;
			}
			case 'bool':
				if (String(flecs) !== v.tok.v) mismatch(v.tok.s, v.tok.e, v.tok.v);
				body.verb(v.tok.s, v.tok.e);
				return;
			case 'name': {
				const p = typeof flecs === 'string' ? match([flecs], v.tok.v) : null;
				if (!p) {
					mismatch(v.tok.s, v.tok.e, v.tok.v);
					body.atom('(0 as any)', v.tok.s, v.tok.e);
				} else if (!ref(body, p, v.tok)) body.atom('(0 as any)', v.tok.s, v.tok.e);
				return;
			}
			case 'expr':
				body.atom('(0 as any)', v.s, v.e);
				return;
			case 'obj': {
				const rec = flecs && typeof flecs === 'object' && !Array.isArray(flecs) ? (flecs as FlecsValue) : null;
				if (!rec) mismatch(v.open.s, v.close.e, 'a composite');
				body.atom('{', v.open.s, v.open.e);
				v.fields.forEach((f, n) => {
					if (n) body.raw(', ');
					if (rec && !(f.name.v in rec)) loud(f.name.s, f.name.e, `Flecs has no member ${f.name.v} in ${at}.`);
					body.verb(f.name.s, f.name.e);
					body.raw(': ');
					value(f.value, rec?.[f.name.v], `${at}.${f.name.v}`);
				});
				body.atom('}', v.close.s, v.close.e);
				return;
			}
			case 'list': {
				// Positional: element n is the n-th member (a struct) or element (a collection).
				const vals = Array.isArray(flecs) ? flecs : flecs && typeof flecs === 'object' ? Object.values(flecs) : null;
				const keys = Array.isArray(flecs) ? null : flecs && typeof flecs === 'object' ? Object.keys(flecs) : null;
				if (!vals) mismatch(v.open.s, v.close.e, 'a composite');
				body.atom('[', v.open.s, v.open.e);
				v.items.forEach((item, n) => {
					if (n) body.raw(', ');
					value(item, vals?.[n], keys ? `${at}.${keys[n]}` : `${at}[${n}]`);
				});
				body.atom(']', v.close.s, v.close.e);
				return;
			}
		}
	};
	const strings: { tok: Tok; text: string }[] = [];

	const item = (it: Item) => {
		if (it.k === 'skipped') return;
		const owner = real.get(it.owner);
		if (it.owner === '') {
			const t = 'tok' in it ? it.tok : 'comp' in it ? it.comp : it.k === 'pair' ? it.rel : null;
			if (t) loud(t.s, t.e, 'flecs-content-mapper does not support tags, components or pairs on the root.', CODE.unsupported);
			return;
		}
		if (!owner) return; // already reported
		const e = byPath.get(owner)!;
		const comps = Object.keys(e.components ?? {});
		switch (it.k) {
			case 'tag': {
				const p = match([...(e.tags ?? []), ...comps], it.tok.v) ?? match(asList(e.pairs?.['flecs.core.IsA']), it.tok.v);
				if (!p) return loud(it.tok.s, it.tok.e, `Flecs did not add ${it.tok.v} to ${owner}.`);
				add(has, it.owner, p);
				if (ref(body, p, it.tok)) body.raw(';\n');
				return;
			}
			case 'value': {
				const p = match(comps, it.comp.v);
				if (!p) return loud(it.comp.s, it.comp.e, `Flecs set no ${it.comp.v} on ${owner}.`);
				add(has, it.owner, p);
				const v = it.value;
				const flecs = e.components![p];
				if (v.k !== 'obj' && v.k !== 'list') {
					// A scalar component (`Mass: 5`): compared with Flecs, nothing to type.
					const before = body.text.length;
					body.raw('(');
					value(v, flecs, `${owner}: ${p}`);
					body.raw(');\n');
					void before;
					return;
				}
				body.raw('(');
				value(v, flecs, `${p} of ${owner}`);
				body.raw(') satisfies ');
				if (p.startsWith('flecs.')) body.raw('unknown;\n');
				else {
					body.raw(v.k === 'obj' ? '__Named<' : '__Positional<');
					body.atom(JSON.stringify(p), it.comp.s, it.comp.e);
					body.raw('>;\n');
				}
				return;
			}
			case 'base': {
				const p = match(asList(e.pairs?.['flecs.core.IsA']), it.tok.v);
				if (!p) return loud(it.tok.s, it.tok.e, `Flecs did not make ${owner} an instance of ${it.tok.v}.`);
				add(isa, it.owner, p);
				if (ref(body, p, it.tok)) body.raw(';\n');
				return;
			}
			case 'pair': {
				const rel = match(Object.keys(e.pairs ?? {}), it.rel.v);
				const tgt = rel && match(asList(e.pairs![rel]), it.tgt.v);
				if (!rel || !tgt) return loud(it.rel.s, it.tgt.e, `Flecs did not add (${it.rel.v}, ${it.tgt.v}) to ${owner}.`);
				if (rel === 'flecs.core.IsA') add(isa, it.owner, tgt);
				if (ref(body, rel, it.rel)) body.raw(';\n');
				const rule = options.relationships[rel];
				// A call, so TypeScript reports a broken rule on the target as written, not in virtual code.
				if (!okPath(tgt) || tgt.startsWith('flecs.')) {
					if (rule) loud(it.tgt.s, it.tgt.e, `${tgt} cannot be checked against the ${rel} rule.`, CODE.unsupported);
				} else {
					body.raw(rule ? `__target<${JSON.stringify(rule)}>(` : '');
					ref(body, tgt, it.tgt);
					body.raw(rule ? ');\n' : ';\n');
				}
				return;
			}
			case 'struct': {
				const def = e.components?.['flecs.meta.struct'] as { members?: { name: string; type: string }[] } | undefined;
				const members = def?.members ?? [];
				if (!def) return loud(0, 0, `Flecs did not define ${owner} as a struct.`);
				if (members.length !== it.members.length || members.some((m, n) => m.name !== it.members[n].name.v)) {
					const at = it.members[0]?.name ?? { s: 0, e: 0 };
					return loud(at.s, at.e, `Flecs's members of ${owner} (${members.map((m) => m.name).join(', ')}) are not the ones written.`);
				}
				const ty = (t: string) => {
					if (t === 'flecs.meta.string' || t === 'flecs.meta.char') return 'string';
					if (t === 'flecs.meta.bool') return 'boolean';
					if (t === 'flecs.meta.entity') return '__Entity';
					if (t.startsWith('flecs.meta.') && NUMBER.has(t.slice(11))) return 'number';
					if (t.startsWith('flecs.')) return 'unknown';
					return `__Val<${JSON.stringify(t)}>`;
				};
				const key = JSON.stringify(owner);
				out.raw(`interface __Structs { ${key}: { `);
				members.forEach((m, n) => {
					out.verb(it.members[n].name.s, it.members[n].name.e);
					out.raw(`?: ${ty(m.type)}; `);
				});
				out.raw(`} }\ninterface __Tuples { ${key}: [${members.map((m) => `${m.name}?: ${ty(m.type)}`).join(', ')}] }\n`);
				members.forEach((m, n) => {
					const t = it.members[n].type;
					if (t && ref(body, m.type, t)) body.raw(';\n');
				});
				return;
			}
			case 'prefab': {
				if (!(e.tags ?? []).includes('flecs.core.Prefab')) return loud(0, 0, `Flecs did not make ${owner} a prefab.`);
				return;
			}
		}
	};
	for (const it of scan.items) item(it);

	// Prefabs, after the bases are known.
	for (const it of scan.items) {
		if (it.k !== 'prefab') continue;
		const p = real.get(it.owner);
		if (!p || !okPath(p)) continue;
		out.raw(`interface __Prefabs { ${JSON.stringify(p)}: `);
		for (const b of isa.get(it.owner) ?? []) out.raw(`__PrefabOf<${JSON.stringify(b)}> & `);
		out.raw(`{ readonly ${JSON.stringify('__isa_' + marker(p))}: 0 } }\n`);
	}

	// Entities this file names, as namespaces.
	for (const ent of scan.entities.values()) {
		const p = real.get(ent.key);
		if (!p || ent.anon >= 0 || !okPath(p) || !ent.name) continue;
		const dot = p.lastIndexOf('.');
		out.raw(`declare namespace __e.${p.slice(0, dot + 1)}`);
		const last = ent.name.v.lastIndexOf('.');
		out.verb(ent.name.s + last + 1, ent.name.e);
		out.raw(' { var __: 0;');
		for (const h of has.get(ent.key) ?? []) out.raw(` var __has_${marker(h)}: 0;`);
		const bases = [...(isa.get(ent.key) ?? [])];
		if (bases.length) out.raw(` var __isa: ${bases.map((b) => `__PrefabOf<${JSON.stringify(b)}>`).join(' & ')};`);
		out.raw(' }\n');
	}

	// [[name]] and [label][name] in strings, as Flecs stored them, at the place they are written.
	if (options.refs !== null) {
		for (const { tok, text } of strings) {
			const raw = src.slice(tok.s, tok.e);
			for (const m of text.matchAll(/\[\[([\w.-]+)\]\]|\[[^\]]+\]\[([\w.-]+)\]/g)) {
				const name = m[1] ?? m[2];
				const at = raw.indexOf(m[0]);
				if (at < 0) {
					loud(tok.s, tok.e, `The reference ${m[0]} is not written as is in this string; it is not checked.`, CODE.unsupported);
					continue;
				}
				const s = tok.s + at + m[0].lastIndexOf(name);
				if (!okPath(name)) {
					loud(s, s + name.length, `${name} is not a name flecs-content-mapper can check.`, CODE.unsupported);
					continue;
				}
				body.atom(`__e.${options.refs}.`, s, s + name.length);
				body.verb(s, s + name.length);
				body.raw(';\n');
			}
		}
	}

	// Concatenate: declarations, then the checks.
	const shift = out.text.length;
	const mappings = [...out.maps, ...body.maps.map(([vs, vl, os, ol, k, f]): Mapping => (f === undefined ? [vs + shift, vl, os, ol, k] : [vs + shift, vl, os, ol, k, f]))];
	return { text: out.text + body.text, mappings, diags };
}
