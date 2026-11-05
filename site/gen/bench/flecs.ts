// Flecs script, written by the build: entities as the content files write them (content/schema.flecs), so that
// what the build generates runs in the same world and is checked the same way.

/** A Flecs string literal: `\`, `"`, and `{` and `$`, which would start an expression, escaped. */
export const str = (s: string) => `"${s.replace(/[\\"{$]/g, (c) => `\\${c}`).replace(/\s+/g, ' ')}"`;

const KEYWORDS = new Set([
	'try', 'for', 'if', 'in', 'with', 'using', 'module', 'const', 'prop', 'match', 'new', 'export', 'template', 'prefab',
	'struct', 'enum', 'else', 'elif', 'and', 'or', 'not', 'true', 'false', 'var',
]);

/** An entity name from any text: lower-case letters, digits and `_`, never a keyword or starting with a digit. */
export function slug(s: string) {
	const n = s.toLowerCase().replace(/[^a-z0-9]+/g, '_').replace(/^_+|_+$/g, '') || 'x';
	return KEYWORDS.has(n) || /^[0-9]/.test(n) ? `x_${n}` : n;
}

type Field = string | number;
/** One entity: its kind (a component with fields, or a tag), its name, and what goes in its scope. */
export type Ent = { kind: string; name: string; fields?: Record<string, Field>; body: (Ent | string)[] };

/** An entity of `kind`, named `name` (made unique among `siblings` when given). */
export function ent(kind: string, name: string, fields?: Record<string, Field>, body: (Ent | string)[] = []): Ent {
	return { kind, name, fields, body };
}

/** Gives every child entity of `body` a name no sibling has: `x`, `x_2`, ... */
function unique(body: (Ent | string)[]) {
	const seen = new Map<string, number>();
	for (const b of body) {
		if (typeof b === 'string') continue;
		const n = (seen.get(b.name) ?? 0) + 1;
		seen.set(b.name, n);
		if (n > 1) b.name = `${b.name}_${n}`;
		unique(b.body);
	}
}

const value = (v: Field) => (typeof v === 'number' ? (Number.isFinite(v) ? String(v) : '0') : str(v));

function print(e: Ent, indent: string): string {
	const fields = e.fields ? `(${Object.entries(e.fields).map(([k, v]) => `${k}: ${value(v)}`).join(', ')})` : '';
	const head = `${indent}${e.kind} ${e.name}${fields}`;
	if (!e.body.length) return e.fields ? head : `${head} {}`;
	const inner = e.body.map((b) => (typeof b === 'string' ? `${indent}  ${b}` : print(b, `${indent}  `))).join('\n');
	return `${head} {\n${inner}\n${indent}}`;
}

/** A script that opens the entity at `path` (dotted, made already) and adds `body` to its scope, in order. */
export function script(path: string, body: Ent[]): string {
	unique(body);
	const parts = path.split('.');
	const open = parts.map((p, i) => `${'  '.repeat(i)}${p} {`).join('\n');
	const close = parts.map((_, i) => `${'  '.repeat(parts.length - 1 - i)}}`).join('\n');
	return `${open}\n${body.map((b) => print(b, '  '.repeat(parts.length))).join('\n')}\n${close}\n`;
}
