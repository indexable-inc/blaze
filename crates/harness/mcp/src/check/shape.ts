/*
 * Ported from den's repl/src/shape.ts (without its JSON Schema reader). Shapes: what a value has to look like, as data, and the one validator that checks a value against one.
 *
 * A cell's `parse<T>(value)` gets its shape from `T` at check time (type-service.ts reads the type from TypeScript's
 * checker, `shapeOf`), and an MCP tool's result gets its shape from the tool's `outputSchema` (`fromJsonSchema`), so
 * the type a cell is checked against and the check its value passes at run time come from one description, never
 * from a second schema written beside the type. Nothing here imports anything: the worker, the MCP client and the
 * type service all use it.
 */

export type Shape =
  | { t: 'string'; pattern?: string; minLength?: number; maxLength?: number }
  | { t: 'number'; integer?: boolean; minimum?: number; maximum?: number }
  | { t: 'boolean' } | { t: 'null' } | { t: 'undefined' } | { t: 'unknown' }
  | { t: 'literal'; value: string | number | boolean }
  /** A `Date`: a Date, or an ISO 8601 string turned into one. Only where the type says `Date`. */
  | { t: 'date' }
  | { t: 'array'; of: Shape }
  /** Fixed elements, the last `optional` of them optional, then any number of `rest`. */
  | { t: 'tuple'; of: Shape[]; optional: number; rest?: Shape }
  /** Named fields (those in `optional` may be missing), any other key checked against `rest`; `closed` refuses other keys. */
  | { t: 'object'; fields: Record<string, Shape>; optional: string[]; rest?: Shape; closed?: boolean }
  /** One of `of`; with `tag`, the field whose literal picks the member (a discriminated union). */
  | { t: 'union'; of: Shape[]; tag?: string }
  /** `defs[id]`: a named or recursive part. */
  | { t: 'ref'; id: string };

/** A whole shape: the type's name (for messages), its root and the parts it refers to. */
export interface Plan { name: string; root: Shape; defs: Record<string, Shape> }

export interface Mismatch { path: string; expected: string; got: string }

const IDENTIFIER = /^[A-Za-z_$][\w$]*$/;
const field = (path: string, key: string) => IDENTIFIER.test(key) ? `${path ? `${path}.` : ''}${key}` : `${path}[${JSON.stringify(key)}]`;
const ISO_DATE = /^\d{4}-\d{2}-\d{2}(?:[T ]\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?(?:Z|[+-]\d{2}:?\d{2})?)?$/;

const isRecord = (value: unknown): value is Record<string, unknown> => typeof value === 'object' && value !== null && !Array.isArray(value);
const kindOf = (value: unknown): string => {
  if (value === undefined) return 'nothing';
  if (value === null) return 'null';
  if (Array.isArray(value)) return 'an array';
  if (value instanceof Date) return 'a Date';
  return typeof value === 'object' ? 'an object' : typeof value;
};
const literals = (shape: Shape, defs: Record<string, Shape>): Shape[] => {
  const resolved = shape.t === 'ref' ? defs[shape.id] ?? shape : shape;
  return resolved.t === 'union' ? resolved.of.flatMap(member => literals(member, defs)) : [resolved];
};

/** What `shape` accepts, in a few words: `number`, `"a" | "b"`, `an object`, `string[]`. */
export function describe(shape: Shape, defs: Record<string, Shape> = {}, depth = 0): string {
  switch (shape.t) {
    case 'string': return shape.pattern ? `a string matching /${shape.pattern}/` : 'string';
    case 'number': return shape.integer ? 'an integer' : 'number';
    case 'boolean': case 'null': case 'undefined': case 'unknown': return shape.t;
    case 'literal': return JSON.stringify(shape.value);
    case 'date': return 'a date (an ISO 8601 string)';
    case 'array': { const of = describe(shape.of, defs, depth + 1); return /^[\w"]+$/.test(of) ? `${of}[]` : 'an array'; }
    case 'tuple': return `a tuple of ${shape.of.length}${shape.rest ? ' or more' : ''}`;
    case 'object': return 'an object';
    case 'union': return shape.of.length ? [...new Set(shape.of.map(member => describe(member, defs, depth + 1)))].join(' | ') : 'nothing (no value is allowed)';
    case 'ref': return depth > 4 || !defs[shape.id] ? 'an object' : describe(defs[shape.id]!, defs, depth + 1);
  }
}

/** The union member a tagged union's value picks, by its tag's literal; undefined when no member has it. */
function tagged(shape: Shape & { t: 'union' }, value: Record<string, unknown>, defs: Record<string, Shape>): Shape[] {
  return shape.of.filter(member => {
    const resolved = member.t === 'ref' ? defs[member.id] : member;
    const tag = resolved?.t === 'object' ? resolved.fields[shape.tag!] : undefined;
    return !!tag && literals(tag, defs).some(literal => literal.t === 'literal' && literal.value === value[shape.tag!]);
  });
}

/** Checks `value` against `shape`, adding each mismatch to `out`; returns the value, with dates made Dates. */
function check(shape: Shape, value: unknown, path: string, defs: Record<string, Shape>, out: Mismatch[]): unknown {
  const fail = (expected: string, got = kindOf(value)) => { out.push({ path: path || 'value', expected, got }); return value; };
  switch (shape.t) {
    case 'unknown': return value;
    case 'string':
      if (typeof value !== 'string') return fail('string');
      if (shape.minLength !== undefined && value.length < shape.minLength) return fail(`a string of at least ${shape.minLength}`, `${value.length} characters`);
      if (shape.maxLength !== undefined && value.length > shape.maxLength) return fail(`a string of at most ${shape.maxLength}`, `${value.length} characters`);
      if (shape.pattern !== undefined && !new RegExp(shape.pattern, 'u').test(value)) return fail(describe(shape), JSON.stringify(value));
      return value;
    case 'number':
      if (typeof value !== 'number' || Number.isNaN(value)) return fail(describe(shape));
      if (shape.integer && !Number.isInteger(value)) return fail('an integer', String(value));
      if (shape.minimum !== undefined && value < shape.minimum) return fail(`a number of at least ${shape.minimum}`, String(value));
      if (shape.maximum !== undefined && value > shape.maximum) return fail(`a number of at most ${shape.maximum}`, String(value));
      return value;
    case 'boolean': return typeof value === 'boolean' ? value : fail('boolean');
    case 'null': return value === null ? value : fail('null');
    case 'undefined': return value === undefined ? value : fail('nothing');
    case 'literal': return value === shape.value ? value : fail(JSON.stringify(shape.value), typeof value === 'string' || typeof value === 'number' || typeof value === 'boolean' ? JSON.stringify(value) : kindOf(value));
    case 'date': {
      if (value instanceof Date) return Number.isNaN(value.getTime()) ? fail('a valid date', 'an invalid Date') : value;
      if (typeof value !== 'string') return fail('a date (an ISO 8601 string)');
      const date = new Date(value);
      return ISO_DATE.test(value) && !Number.isNaN(date.getTime()) ? date : fail('a date (an ISO 8601 string)', JSON.stringify(value));
    }
    case 'array': {
      if (!Array.isArray(value)) return fail(describe(shape, defs));
      let copy: unknown[] | undefined;
      value.forEach((item, index) => {
        const next = check(shape.of, item, `${path}[${index}]`, defs, out);
        if (next !== item) (copy ??= [...value])[index] = next;
      });
      return copy ?? value;
    }
    case 'tuple': {
      if (!Array.isArray(value)) return fail(describe(shape));
      const required = shape.of.length - shape.optional;
      if (value.length < required || (!shape.rest && value.length > shape.of.length)) {
        return fail(shape.optional || shape.rest ? `${required}${shape.rest ? ' or more' : `-${shape.of.length}`} elements` : `${shape.of.length} elements`, `${value.length}`);
      }
      let copy: unknown[] | undefined;
      value.forEach((item, index) => {
        const element = index < shape.of.length ? shape.of[index]! : shape.rest!;
        const next = check(element, item, `${path}[${index}]`, defs, out);
        if (next !== item) (copy ??= [...value])[index] = next;
      });
      return copy ?? value;
    }
    case 'object': {
      if (!isRecord(value) || value instanceof Date) return fail('an object');
      let copy: Record<string, unknown> | undefined;
      const set = (key: string, next: unknown) => { if (next !== value[key]) (copy ??= { ...value })[key] = next; };
      for (const [key, member] of Object.entries(shape.fields)) {
        if (!(key in value) || value[key] === undefined) {
          if (!shape.optional.includes(key) && !literals(member, defs).some(option => option.t === 'undefined' || option.t === 'unknown')) {
            out.push({ path: field(path, key), expected: describe(member, defs), got: 'nothing' });
          }
          continue;
        }
        set(key, check(member, value[key], field(path, key), defs, out));
      }
      for (const key of Object.keys(value)) {
        if (key in shape.fields) continue;
        if (shape.rest) set(key, check(shape.rest, value[key], field(path, key), defs, out));
        else if (shape.closed) out.push({ path: field(path, key), expected: 'no such field', got: kindOf(value[key]) });
      }
      return copy ?? value;
    }
    case 'union': {
      let members = shape.of;
      if (shape.tag !== undefined && isRecord(value)) {
        members = tagged(shape, value, defs);
        if (!members.length) {
          const options = shape.of.flatMap(member => { const resolved = member.t === 'ref' ? defs[member.id] : member; const tag = resolved?.t === 'object' ? resolved.fields[shape.tag!] : undefined; return tag ? [describe(tag, defs)] : []; });
          const got = value[shape.tag];
          out.push({ path: field(path, shape.tag), expected: [...new Set(options)].join(' | '), got: typeof got === 'string' || typeof got === 'number' || typeof got === 'boolean' ? JSON.stringify(got) : kindOf(got) });
          return value;
        }
      }
      const tries = members.map(member => { const errors: Mismatch[] = []; return { errors, next: check(member, value, path, defs, errors) }; });
      const passed = tries.find(attempt => !attempt.errors.length);
      if (passed) return passed.next;
      // A member of the value's own kind (its mismatches are all inside it) explains it best; the fewest mismatches wins.
      const inside = tries.filter(attempt => attempt.errors.every(error => error.path !== (path || 'value'))).sort((a, b) => a.errors.length - b.errors.length);
      if (inside.length) { out.push(...inside[0]!.errors); return value; }
      const primitive = typeof value === 'string' || typeof value === 'number' || typeof value === 'boolean';
      return fail(describe(shape, defs), primitive && shape.of.every(member => member.t === 'literal') ? JSON.stringify(value) : kindOf(value));
    }
    case 'ref': {
      const target = defs[shape.id];
      return target ? check(target, value, path, defs, out) : value;
    }
  }
}

/** `value` checked against `plan`: the value (dates made Dates), or every mismatch by its path. */
export function validate(plan: Plan, value: unknown): { ok: true; value: unknown } | { ok: false; mismatches: Mismatch[] } {
  const mismatches: Mismatch[] = [];
  const checked = check(plan.root, value, '', plan.defs, mismatches);
  return mismatches.length ? { ok: false, mismatches } : { ok: true, value: checked };
}

const SHOWN = 50;
/** The mismatches, one a line: `items[3].id: expected number, got string`. */
export function mismatchLines(mismatches: Mismatch[]): string[] {
  const lines = mismatches.slice(0, SHOWN).map(m => `  ${m.path}: expected ${m.expected}, got ${m.got}`);
  if (mismatches.length > SHOWN) lines.push(`  and ${mismatches.length - SHOWN} more`);
  return lines;
}

/** What `parse<T>` throws: the type, then every mismatch by its path. */
export class ParseError extends TypeError {
  override name = 'ParseError';
  constructor(readonly type: string, readonly mismatches: Mismatch[]) {
    super([`the value does not fit ${type}: ${mismatches.length === 1 ? '1 mismatch' : `${mismatches.length} mismatches`}`, ...mismatchLines(mismatches)].join('\n'));
  }
}

const isPlan = (value: unknown): value is Plan => isRecord(value) && typeof value.name === 'string' && isRecord(value.root) && isRecord(value.defs);

/**
 * The cell global: `value` checked against `T`, returned as one (a `Date` field made a Date), or a `ParseError` listing
 * every mismatch by its path. The check comes from `T` itself: the cell's type check compiles each `parse<T>(…)` to a
 * plan and passes it as a last argument the cell never writes (cell-inspect.ts, `withPlans`).
 */
export function parse<T>(value: unknown): T;
export function parse(value: unknown, plan?: unknown): unknown {
  if (!isPlan(plan)) {
    throw new Error('parse<T>(value) checks against the plan its cell\'s type check compiles from T, and this call has none: call it as `parse<T>(value)` in the cell itself (not through another name), with the cell\'s check able to run.');
  }
  const checked = validate(plan, value);
  if (!checked.ok) throw new ParseError(plan.name, checked.mismatches);
  return checked.value;
}

/** The tag of a discriminated union: a field every member has, required, as literals no two members share. */
export function tagOf(members: Shape[], defs: Record<string, Shape> = {}): string | undefined {
  const objects = members.map(member => member.t === 'ref' ? defs[member.id] : member);
  if (objects.length < 2 || !objects.every(member => member?.t === 'object')) return undefined;
  const [first, ...others] = objects as (Shape & { t: 'object' })[];
  for (const key of Object.keys(first!.fields)) {
    const seen = new Set<unknown>();
    const distinct = [first!, ...others].every(member => {
      const tag = member.fields[key];
      if (!tag || member.optional.includes(key)) return false;
      const values = literals(tag, defs);
      if (!values.length || !values.every(value => value.t === 'literal')) return false;
      return values.every(value => { const v = (value as { value: unknown }).value; if (seen.has(v)) return false; seen.add(v); return true; });
    });
    if (distinct) return key;
  }
  return undefined;
}
