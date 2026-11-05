/**
 * Locates things in a .flecs file: where each entity is named, where each tag, component, value,
 * base and pair is written. It decides no meaning. What a name resolves to, which entities exist and
 * what values they hold come from running the script under Flecs (run.ts); emit.ts matches each
 * located thing against Flecs's output and fails loudly when the two disagree.
 *
 * Statement shapes follow flecs/src/addons/script/parser.c. Statements it does not model (const,
 * template, if, for, with, fn, ...) are recorded as `skipped` so that anything Flecs creates inside
 * them shows up as a disagreement instead of being silently ignored.
 */

export type Tok = {
	k: 'id' | 'str' | 'num' | 'p' | 'nl' | 'eof';
	/** Source text; for 'str' the decoded value. */
	v: string;
	s: number;
	e: number;
};

export type Diag = { messageText: string; start: number; length: number; code: number };

/** Codes: 91001 syntax, 91002 Flecs error, 91003 mapper and Flecs disagree, 91004 unsupported. */
export const CODE = { syntax: 91001, flecs: 91002, disagree: 91003, unsupported: 91004 } as const;

export function tokenize(src: string, diags: Diag[]): Tok[] {
	const out: Tok[] = [];
	let i = 0;
	const n = src.length;
	while (i < n) {
		const c = src[i];
		if (c === '\n' || c === ';') {
			out.push({ k: 'nl', v: c, s: i, e: i + 1 });
			i++;
		} else if (c === ' ' || c === '\t' || c === '\r') {
			i++;
		} else if (c === '/' && src[i + 1] === '/') {
			while (i < n && src[i] !== '\n') i++;
		} else if (c === '/' && src[i + 1] === '*') {
			const end = src.indexOf('*/', i + 2);
			if (end < 0) {
				diags.push({ messageText: 'Missing */ for a comment.', start: i, length: 2, code: CODE.syntax });
				i = n;
			} else i = end + 2;
		} else if (c === '"' || c === '`' || c === "'") {
			const s = i;
			let v = '';
			let closed = false;
			i++;
			while (i < n) {
				const d = src[i];
				if (d === '\\' && i + 1 < n) {
					const x = src[i + 1];
					v += x === 'n' ? '\n' : x === 't' ? '\t' : x === 'r' ? '\r' : x === '0' ? '\0' : x;
					i += 2;
				} else if (d === c) {
					closed = true;
					i++;
					break;
				} else if (d === '\n' && c !== '`') break;
				else {
					v += d;
					i++;
				}
			}
			if (!closed) diags.push({ messageText: 'Unterminated string.', start: s, length: i - s, code: CODE.syntax });
			out.push({ k: 'str', v, s, e: i });
		} else if (c >= '0' && c <= '9') {
			const m = /^(0[xX][0-9a-fA-F]+|0[bB][01]+|[0-9]+(\.[0-9]+)?([eE][+-]?[0-9]+)?)/.exec(src.slice(i, i + 64))!;
			out.push({ k: 'num', v: m[0], s: i, e: i + m[0].length });
			i += m[0].length;
		} else if (/[A-Za-z_$#]/.test(c)) {
			const s = i;
			const isVar = c === '$';
			i++;
			while (i < n) {
				const d = src[i];
				if (/[A-Za-z0-9_$#]/.test(d)) i++;
				else if (!isVar && d === '.' && src[i + 1] !== '.') i++;
				else if (d === '\\' && src[i + 1] === '.') i += 2;
				else if (d === '*' && src[i - 1] === '.') i++;
				else break;
			}
			out.push({ k: 'id', v: src.slice(s, i), s, e: i });
		} else {
			const two = src.slice(i, i + 2);
			const p = ['..', '==', '!=', '<=', '>=', '&&', '||', '<<', '>>', '->', '+=', '-=', '*=', '/='].includes(two) ? two : c;
			out.push({ k: 'p', v: p, s: i, e: i + p.length });
			i += p.length;
		}
	}
	out.push({ k: 'eof', v: '', s: n, e: n });
	return out;
}

/** A value as written. `expr` is anything with operators, variables or calls: not modelled. */
export type Value =
	| { k: 'str'; tok: Tok }
	| { k: 'num'; s: number; e: number; n: number }
	| { k: 'bool'; tok: Tok }
	| { k: 'name'; tok: Tok }
	| { k: 'expr'; s: number; e: number }
	| { k: 'obj'; open: Tok; close: Tok; fields: { name: Tok; value: Value }[] }
	| { k: 'list'; open: Tok; close: Tok; items: Value[] };

/** An entity as the file names it. `key` is its path, with `#n` for the n-th anonymous child. */
export type Entity = { key: string; parent: string | null; name: Tok | null; anon: number };

export type Item =
	| { k: 'tag'; owner: string; tok: Tok }
	| { k: 'value'; owner: string; comp: Tok; value: Value }
	| { k: 'base'; owner: string; tok: Tok }
	| { k: 'pair'; owner: string; rel: Tok; tgt: Tok }
	| { k: 'struct'; owner: string; members: { name: Tok; type: Tok | null }[] }
	| { k: 'prefab'; owner: string }
	| { k: 'skipped'; s: number; e: number; what: string };

export type Scan = { entities: Map<string, Entity>; items: Item[]; roots: string[]; diags: Diag[] };

const UNSUPPORTED = new Set(['with', 'template', 'prop', 'mut', 'const', 'export', 'if', 'for', 'fn', 'await', 'try', 'while', 'async', 'continue', 'module']);

export function scan(src: string): Scan {
	const diags: Diag[] = [];
	const toks = tokenize(src, diags);
	const entities = new Map<string, Entity>();
	const items: Item[] = [];
	const roots: string[] = [];
	const anonCount = new Map<string, number>();
	let i = 0;

	const peek = (o = 0) => toks[Math.min(i + o, toks.length - 1)];
	const next = () => toks[Math.min(i++, toks.length - 1)];
	const isP = (t: Tok, v: string) => t.k === 'p' && t.v === v;
	const skipNl = () => {
		while (peek().k === 'nl') i++;
	};
	const braceAhead = () => {
		let j = i;
		while (toks[j].k === 'nl') j++;
		return isP(toks[j], '{');
	};
	const err = (t: Tok, msg: string) => diags.push({ messageText: msg, start: t.s, length: Math.max(1, t.e - t.s), code: CODE.syntax });
	const open = (t: Tok) => t.k === 'p' && (t.v === '{' || t.v === '(' || t.v === '[');
	const close = (t: Tok) => t.k === 'p' && (t.v === '}' || t.v === ')' || t.v === ']');

	const skipStmt = (from: Tok, what: string) => {
		let depth = 0;
		for (;;) {
			const t = peek();
			if (t.k === 'eof' || (depth === 0 && (t.k === 'nl' || isP(t, '}')))) break;
			if (open(t)) depth++;
			if (close(t)) depth--;
			i++;
		}
		items.push({ k: 'skipped', s: from.s, e: toks[Math.max(0, i - 1)].e, what });
	};

	/** The entity `name` (null: anonymous) under `parent` ('' is the root). */
	const entity = (name: Tok | null, parent: string): string => {
		if (name && (name.k === 'str' || (name.k === 'id' && name.v !== '_' && name.v !== '$'))) {
			const key = parent ? `${parent}.${name.v}` : name.v;
			if (!parent) roots.push(name.v.split('.')[0]);
			if (!entities.has(key)) entities.set(key, { key, parent: parent || null, name, anon: -1 });
			return key;
		}
		const n = anonCount.get(parent) ?? 0;
		anonCount.set(parent, n + 1);
		const key = parent ? `${parent}.#${n}` : `#${n}`;
		entities.set(key, { key, parent: parent || null, name, anon: n });
		return key;
	};

	const term = (t: Tok, inner: boolean) => t.k === 'eof' || (t.k === 'p' && [',', ')', '}', ']'].includes(t.v)) || (!inner && t.k === 'nl');
	const skipExpr = (inner: boolean) => {
		let depth = 0;
		for (;;) {
			const t = peek();
			if (t.k === 'eof' || (depth === 0 && term(t, inner))) return;
			if (open(t)) depth++;
			if (close(t)) depth--;
			i++;
		}
	};

	const list = (o: Tok, closeCh: string): Value => {
		const fields: { name: Tok; value: Value }[] = [];
		const elems: Value[] = [];
		let expr = false;
		for (;;) {
			skipNl();
			const t = peek();
			if (isP(t, closeCh)) {
				i++;
				if (expr || (fields.length && elems.length)) return { k: 'expr', s: o.s, e: t.e };
				return fields.length ? { k: 'obj', open: o, close: t, fields } : { k: 'list', open: o, close: t, items: elems };
			}
			if (t.k === 'eof') {
				err(o, `Missing "${closeCh}".`);
				return { k: 'expr', s: o.s, e: t.s };
			}
			if (t.k === 'id' && isP(peek(1), ':') && closeCh !== ']') {
				i += 2;
				skipNl();
				fields.push({ name: t, value: value(true) });
			} else if (t.k === 'id' && peek(1).k === 'p' && /^[+\-*/]=$/.test(peek(1).v)) {
				expr = true;
				skipExpr(true);
			} else elems.push(value(true));
			skipNl();
			if (isP(peek(), ',')) i++;
			else if (!isP(peek(), closeCh)) {
				err(peek(), `Expected "," or "${closeCh}".`);
				skipExpr(true);
				if (isP(peek(), ',')) i++;
			}
		}
	};

	const value = (inner: boolean): Value => {
		const t = peek();
		let v: Value;
		if (isP(t, '-') && peek(1).k === 'num' && peek(1).s === t.e) {
			i += 2;
			v = { k: 'num', s: t.s, e: toks[i - 1].e, n: -Number(toks[i - 1].v) };
		} else if (t.k === 'num') {
			i++;
			v = { k: 'num', s: t.s, e: t.e, n: Number(t.v) };
		} else if (t.k === 'str') {
			i++;
			v = { k: 'str', tok: t };
		} else if (t.k === 'id') {
			i++;
			v = t.v === 'true' || t.v === 'false' ? { k: 'bool', tok: t } : t.v.startsWith('$') || t.v === 'match' ? { k: 'expr', s: t.s, e: t.e } : { k: 'name', tok: t };
		} else if (isP(t, '{') || isP(t, '[')) {
			i++;
			v = list(t, t.v === '{' ? '}' : ']');
		} else {
			if (term(t, inner)) {
				err(t, 'Expected a value.');
				return { k: 'expr', s: t.s, e: t.s };
			}
			skipExpr(inner);
			return { k: 'expr', s: t.s, e: toks[i - 1].e };
		}
		if (!term(peek(), inner)) {
			skipExpr(inner);
			return { k: 'expr', s: t.s, e: toks[i - 1].e };
		}
		return v;
	};

	const bases = (owner: string) => {
		for (;;) {
			skipNl();
			const t = next();
			if (t.k !== 'id') {
				err(t, 'Expected a base after ":".');
				i--;
				return;
			}
			items.push({ k: 'base', owner, tok: t });
			if (!isP(peek(), ',')) return;
			i++;
		}
	};

	const scopeAfter = (owner: string) => {
		if (braceAhead()) {
			skipNl();
			i++;
			body(owner, true);
		}
	};

	const members = (o: Tok) => {
		const out: { name: Tok; type: Tok | null }[] = [];
		for (;;) {
			skipNl();
			const t = peek();
			if (isP(t, ')')) {
				i++;
				return out;
			}
			if (t.k === 'eof') {
				err(o, 'Missing ")".');
				return out;
			}
			if (t.k === 'id' && isP(peek(1), ':')) {
				i += 2;
				skipNl();
				const ty = peek();
				if (ty.k === 'id' && term(peek(1), true)) {
					i++;
					out.push({ name: t, type: ty });
				} else {
					out.push({ name: t, type: null });
					skipExpr(true);
				}
			} else skipExpr(true);
			skipNl();
			if (isP(peek(), ',')) i++;
		}
	};

	const kindStmt = (kind: Tok, name: Tok, parent: string) => {
		const owner = entity(name, parent);
		if (isP(peek(), ':')) {
			i++;
			bases(owner);
		}
		if (kind.v === 'struct' && isP(peek(), '(')) {
			items.push({ k: 'struct', owner, members: members(next()) });
		} else if (kind.v === 'prefab') {
			items.push({ k: 'prefab', owner });
		} else if (kind.v === 'enum' || kind.v === 'bitmask') {
			skipStmt(kind, `${kind.v} definitions`);
			return;
		} else {
			items.push({ k: 'tag', owner, tok: kind });
			if (isP(peek(), '(')) {
				const o = next();
				items.push({ k: 'value', owner, comp: kind, value: list(o, ')') });
			}
		}
		scopeAfter(owner);
	};

	const stmt = (parent: string) => {
		const t = next();
		if (t.k === 'id' && t.v === 'using') {
			if (peek().k === 'id') i++;
			return;
		}
		if (t.k === 'id' && t.v === 'include') return skipStmt(t, '"include"');
		if (t.k === 'id' && UNSUPPORTED.has(t.v)) return skipStmt(t, `"${t.v}" statements`);
		if (isP(t, '@')) return skipStmt(t, 'annotations');
		if (isP(t, '{')) return body(entity(null, parent), true);
		if (isP(t, '(')) return pair(parent, t);
		if (t.k !== 'id' && t.k !== 'str') {
			err(t, `Unexpected "${t.v || t.k}".`);
			return skipStmt(t, 'this statement');
		}
		const n1 = peek();
		if (isP(n1, '|') || isP(n1, '[') || isP(n1, '=')) return skipStmt(t, 'this statement form');
		if (isP(n1, ':')) {
			i++;
			const a = peek();
			const b = peek(1);
			const inherit = t.k === 'str' || (a.k === 'id' && !a.v.startsWith('$') && (isP(b, '{') || isP(b, ',')));
			if (!inherit) {
				skipNl();
				items.push({ k: 'value', owner: parent, comp: t, value: value(false) });
				return;
			}
			const owner = entity(t, parent);
			bases(owner);
			return scopeAfter(owner);
		}
		if (isP(n1, '(')) {
			const owner = entity(null, parent);
			items.push({ k: 'tag', owner, tok: t });
			items.push({ k: 'value', owner, comp: t, value: list(next(), ')') });
			return scopeAfter(owner);
		}
		if (n1.k === 'id' || n1.k === 'str') {
			i++;
			return kindStmt(t, n1, parent);
		}
		if (braceAhead()) {
			skipNl();
			i++;
			return body(entity(t, parent), true);
		}
		items.push({ k: 'tag', owner: parent, tok: t });
	};

	const pair = (parent: string, o: Tok) => {
		skipNl();
		const rel = next();
		skipNl();
		const comma = next();
		skipNl();
		const tgt = next();
		skipNl();
		const c = next();
		if (rel.k !== 'id' || !isP(comma, ',') || tgt.k !== 'id' || !isP(c, ')')) {
			err(o, 'Expected "(Relationship, Target)".');
			i--;
			return skipStmt(o, 'this pair');
		}
		if (isP(peek(), ':')) return skipStmt(o, 'pair values');
		if (braceAhead()) return skipStmt(o, 'pair scopes');
		items.push({ k: 'pair', owner: parent, rel, tgt });
	};

	const body = (parent: string, braced: boolean) => {
		for (;;) {
			skipNl();
			const t = peek();
			if (t.k === 'eof') {
				if (braced) err(t, 'Missing "}".');
				return;
			}
			if (isP(t, '}')) {
				i++;
				if (braced) return;
				err(t, 'Unexpected "}".');
				continue;
			}
			const before = i;
			stmt(parent);
			const after = peek();
			if (after.k !== 'nl' && after.k !== 'eof' && !isP(after, '}')) {
				err(after, 'Expected a newline or ";".');
				skipStmt(after, 'this statement');
			}
			if (i === before) i++;
		}
	};

	body('', false);
	return { entities, items, roots: [...new Set(roots)], diags };
}
