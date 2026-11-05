import { parseSync } from 'oxc-parser';
import type { Plan } from './shape.ts';

/*
 * Ported from den's repl/src/cell-types.ts. Every cell is type-checked before it runs by the pinned TypeScript (7, the Go
 * compiler, through its API; one process for the whole server, type-service.ts) against the cell's real globals, the
 * names earlier cells of the lane bound and the packages of its working directory, and a cell with a type error is
 * refused with the first few. Its types are then stripped (typescript.ts) and it runs.
 *
 * A cell that turns checking off (`as any`, `<any>`, `as never`, which passes anywhere since `never` is assignable to every
 * type, an `any` type, `@ts-ignore`, `@ts-expect-error`, `@ts-nocheck`) is written by an agent guessing at a type it cannot
 * see, and the cast hides the mistake: `glob(p, {dot: true} as any)` ran with an option glob did not have. So such a cell
 * is refused too (`denyAny`, on by default), and the refusal shows the error the escape hid: the cell is checked once as
 * written and once without the escapes, and what only the second check reports is what was hidden.
 *
 * `x as unknown as T` is a double assertion, which TypeScript asks for exactly when `x` is not a `T`. It is allowed when a
 * comment on its line or the line above says why (a value from a source TypeScript cannot see); without one it is refused
 * like `as any`, and the check shows what the single `x as T` would have said.
 *
 * The escapes are found in oxc's TypeScript AST (oxc-parser, den's parser), never by matching text.
 */

export type EscapeKind = 'as any' | '<any>' | 'as never' | '<never>' | ': any' | 'any' | '@ts-ignore' | '@ts-expect-error' | '@ts-nocheck' | 'as unknown as';
export interface Escape {
  kind: EscapeKind;
  /** 1-based. */
  line: number;
  /** UTF-16 offsets of what the checked copy changes. */
  start: number;
  end: number;
  /** What replaces `start..end` in the copy checked without escapes; undefined blanks it. */
  replacement?: string;
  /** An `as unknown as T` that a comment explains: allowed. */
  explained?: boolean;
}

type Node = { type: string; start: number; end: number; [key: string]: unknown };
type Comment = { type: 'Line' | 'Block'; value: string; start: number; end: number };

const DIRECTIVE = /^[\s*]*@ts-(ignore|expect-error|nocheck)\b/;

/** 1-based line of each offset, by binary search over where lines start. */
export function lines(source: string): (offset: number) => number {
  const starts = [0];
  for (const match of source.matchAll(/\r\n?|[\n\u2028\u2029]/g)) starts.push(match.index + match[0].length);
  return offset => {
    let low = 0;
    let high = starts.length - 1;
    while (low < high) {
      const mid = (low + high + 1) >> 1;
      if (starts[mid]! <= offset) low = mid; else high = mid - 1;
    }
    return low + 1;
  };
}

const isNode = (value: unknown): value is Node => !!value && typeof value === 'object' && typeof (value as Node).type === 'string';
const child = (node: Node, key: string): Node | undefined => isNode(node[key]) ? node[key] as Node : undefined;
const children = (node: Node): Node[] => Object.values(node).flatMap(value => Array.isArray(value) ? value.filter(isNode) : isNode(value) ? [value] : []);
/** Whether a type mentions `any` anywhere: `any[]` and `Record<string, any>` let everything through as `any` does. */
const mentionsAny = (node: Node): boolean => node.type === 'TSAnyKeyword' || children(node).some(mentionsAny);

/** Every escape from type checking in `source` (TypeScript), in source order. A source oxc cannot parse has none: the stripper has the last word on syntax. */
export function findEscapes(source: string): Escape[] {
  let parsed: ReturnType<typeof parseSync>;
  try { parsed = parseSync('cell.ts', source, { lang: 'ts', sourceType: 'module' }); } catch { return []; }
  // A top-level `return` is legal in a cell (typescript.ts); anything else oxc rejects is left to the stripper.
  if (parsed.errors.some(error => !/'return' statement/.test(error.message))) return [];
  const lineOf = lines(source);
  const comments = parsed.comments as Comment[];
  const escapes: Escape[] = [];
  const explainedAt = (line: number) => comments.some(comment => !DIRECTIVE.test(comment.value) && comment.value.trim().length > 0 && (lineOf(comment.end) === line || lineOf(comment.end) === line - 1));
  const add = (kind: EscapeKind, at: number, start: number, end: number, replacement?: string) => escapes.push({ kind, line: lineOf(at), start, end, ...(replacement === undefined ? {} : { replacement }) });

  const visit = (node: Node): void => {
    const type = child(node, 'typeAnnotation');
    const expression = child(node, 'expression');
    // A cast to a type with `any` in it is taken out whole, so the check sees the value's own type.
    if (node.type === 'TSAsExpression' && type && expression) {
      if (mentionsAny(type)) { add('as any', expression.end, expression.end, node.end); visit(expression); return; }
      // `never` is assignable to every type, so `x as never` passes anywhere just as `as any` does.
      if (type.type === 'TSNeverKeyword') { add('as never', expression.end, expression.end, node.end); visit(expression); return; }
      const inner = child(expression, 'typeAnnotation');
      const value = child(expression, 'expression');
      if (expression.type === 'TSAsExpression' && inner?.type === 'TSUnknownKeyword' && value) {
        const line = lineOf(value.end);
        escapes.push({ kind: 'as unknown as', line, start: value.end, end: expression.end, ...(explainedAt(line) || explainedAt(lineOf(node.start)) ? { explained: true } : {}) });
        visit(value);
        visit(type);
        return;
      }
    }
    if (node.type === 'TSTypeAssertion' && type && expression) {
      if (mentionsAny(type)) { add('<any>', node.start, node.start, expression.start); visit(expression); return; }
      if (type.type === 'TSNeverKeyword') { add('<never>', node.start, node.start, expression.start); visit(expression); return; }
      const inner = child(expression, 'typeAnnotation');
      const value = child(expression, 'expression');
      if (expression.type === 'TSTypeAssertion' && inner?.type === 'TSUnknownKeyword' && value) {
        const line = lineOf(expression.start);
        escapes.push({ kind: 'as unknown as', line, start: expression.start, end: value.start, ...(explainedAt(line) ? { explained: true } : {}) });
        visit(value);
        visit(type);
        return;
      }
    }
    if (node.type === 'VariableDeclarator') {
      // `let x: any = value`: without the annotation the type is the value's, which is what the check should see.
      const id = child(node, 'id');
      const annotation = id && child(id, 'typeAnnotation');
      if (id && annotation && child(annotation, 'typeAnnotation')?.type === 'TSAnyKeyword' && isNode(node.init)) {
        add(': any', annotation.start, annotation.start, annotation.end);
        visit(node.init);
        return;
      }
    }
    if (node.type === 'TSTypeAnnotation' && type?.type === 'TSAnyKeyword') { add(': any', node.start, type.start, type.end, 'unknown'); return; }
    if (node.type === 'TSAnyKeyword') { add('any', node.start, node.start, node.end, 'unknown'); return; }
    for (const next of children(node)) visit(next);
  };
  visit(parsed.program as unknown as Node);
  for (const comment of comments) {
    const directive = DIRECTIVE.exec(comment.value);
    if (directive) add(`@ts-${directive[1]}` as EscapeKind, comment.start, comment.start, comment.end);
  }
  return escapes.sort((a, b) => a.start - b.start);
}

/** The escapes a cell is refused for: all but an explained `as unknown as`. */
export const denied = (escapes: Escape[]) => escapes.filter(escape => !escape.explained);

/** `source` without `escapes`, every line where it was: a blanked range keeps its line breaks. */
export function withoutEscapes(source: string, escapes: Escape[]): string {
  let text = source;
  for (const escape of [...escapes].sort((a, b) => b.start - a.start)) {
    const blank = text.slice(escape.start, escape.end).replace(/[^\r\n\u2028\u2029]/g, ' ');
    text = text.slice(0, escape.start) + (escape.replacement ?? blank) + text.slice(escape.end);
  }
  return text;
}

export interface CellDiagnostic { line: number; code: number; text: string }
/** A `parse<T>(value)` in a cell: where its plan goes (just after its last argument, an offset in the cell) and the plan. */
export interface ParseSite { at: number; plan: Plan }
/** A check, and the plans compiled from the cell's `parse<T>(…)` calls (cell-inspect.ts). */
export interface Inspection { diagnostics: CellDiagnostic[]; parses: ParseSite[] }

/** Names a check reports about what it cannot see (an earlier cell's binding, a package of the cell's project), and the cell's legal top-level `return`. */
const UNSEEN = new Set([1108, 2304, 2307, 2552, 2580, 2591, 2592, 7016]);

/** What the copy without escapes reports that the cell as written does not: the errors the escapes hid. */
export function hiddenErrors(asWritten: CellDiagnostic[], without: CellDiagnostic[]): CellDiagnostic[] {
  const seen = new Map<string, number>();
  const key = (diagnostic: CellDiagnostic) => `${diagnostic.code}:${diagnostic.line}:${diagnostic.text}`;
  for (const diagnostic of asWritten) seen.set(key(diagnostic), (seen.get(key(diagnostic)) ?? 0) + 1);
  return without.filter(diagnostic => {
    if (UNSEEN.has(diagnostic.code)) return false;
    const count = seen.get(key(diagnostic)) ?? 0;
    if (count) { seen.set(key(diagnostic), count - 1); return false; }
    return true;
  });
}

const quote = (kind: EscapeKind) => kind === 'as unknown as' ? '`as unknown as T` without a comment saying why' : `\`${kind}\``;
const UNKNOWN_HINT = '`JSON.parse`, `.json()` and a `catch` variable give `unknown`: check it, `const data = parse<{ id: number }>(value)`, which throws saying what does not fit; for an error, `error instanceof Error ? error.message : String(error)`.';
const CAST_HINT = '`parse<T>(value)` checks the value against `T` when the cell runs and throws with every field that does not fit (`items[3].id: expected number, got string`); a `catch` variable is checked with `error instanceof Error`.';
const PARSE_HINT = '`parse<T>` checks data: strings, numbers, booleans, null, literals, arrays, tuples, objects, records, unions and `Date` (from an ISO string).';

/** den's own diagnostics (cell-inspect.ts) are numbered from here, and shown without a `TS` code. */
const DEN_CODES = 90000;
const PARSE_UNSUPPORTED = 90001;
const PARSE_UNTYPED = 90002;
const UNKNOWN_CAST = 90003;

/** Each diagnostic in the cell's own lines, with the line it is on; at most `shown`, then how many more. */
function listed(source: string, diagnostics: CellDiagnostic[], shown: number): string[] {
  const text = source.split(/\r\n?|\n|\u2028|\u2029/);
  const out: string[] = [];
  for (const diagnostic of diagnostics.slice(0, shown)) {
    out.push(`  line ${diagnostic.line}: ${diagnostic.text}${diagnostic.code < DEN_CODES ? ` (TS${diagnostic.code})` : ''}`);
    const at = text[diagnostic.line - 1]?.trim();
    if (at) out.push(`    ${at.length > 160 ? `${at.slice(0, 159)}…` : at}`);
  }
  if (diagnostics.length > shown) out.push(`  and ${diagnostics.length - shown} more`);
  return out;
}

/** The refusal: what was found and where, then the errors it hid (or that there were none), in the cell's own lines. */
export function refusal(source: string, escapes: Escape[], hidden: CellDiagnostic[] | { failed: string }): string {
  const kinds = [...new Set(escapes.map(escape => escape.kind))];
  const where = kinds.map(kind => `${quote(kind)} (line ${[...new Set(escapes.filter(escape => escape.kind === kind).map(escape => escape.line))].join(', ')})`);
  const out = [`Not run: this cell turns type checking off with ${where.join(' and ')}, which hides a type error instead of fixing it.`];
  const it = escapes.length > 1 ? 'them' : 'it';
  if ('failed' in hidden) out.push(`The cell could not be checked without ${it} (${hidden.failed}); write the real types and run it again.`);
  else if (!hidden.length) out.push(`Checked without ${it}, the cell has no type error: take ${it} out and run the cell again.`);
  else {
    out.push(`Checked without ${it}, TypeScript says:`, ...listed(source, hidden, 6));
    out.push('Fix the code or the types it uses: the preloaded globals are typed, and a wrong option name is one of these errors.');
  }
  out.push('`as unknown as T` is allowed with a comment on its line or the line above saying why.');
  return out.join('\n');
}

/** What the check reports that is not the cell's to fix: its legal top-level `return`, and a package the checker cannot resolve (it is `any` there, as at run time it is whatever loads). */
export const NOT_ERRORS = new Set([1108, 2307, 2792, 7016]);
/** The module a missing-types diagnostic names ("Cannot find module 'x'", "Could not find a declaration file for module 'x'"). */
const MISSING_MODULE = /module '([^']+)'/;

/** A plain line for the errors agents hit most, each once, in the order they first appear. */
function hints(errors: CellDiagnostic[]): string[] {
  const out = new Set<string>();
  for (const { code, text } of errors) {
    if (code === UNKNOWN_CAST) out.add(CAST_HINT);
    else if (code === PARSE_UNSUPPORTED || code === PARSE_UNTYPED) out.add(PARSE_HINT);
    else if (code === 18046 || code === 2571 || /\btype 'unknown'/.test(text)) out.add(UNKNOWN_HINT);
    else if (code === 2304 || code === 2552) out.add('A name from an earlier cell is known once that cell has run and bound it; this one never was.');
    else if (code === 2588 || code === 2632) out.add('An earlier cell declared it with const (or as a function, class or import); declare it again with `let name = ...`.');
    else if (code === 7006 || code === 7031 || code === 7034 || code === 7005) out.add('Give it a type where it is declared: `(row: { id: number }) => ...`, `let rows: string[] = []`.');
    else if (code === 2322 && /^Type '.*' is not assignable to type '.*'\.$/.test(text)) out.add('To give an earlier cell\'s name a new type, declare it again: `let name = ...`.');
  }
  return [...out];
}

/** The refusal of a cell with type errors: the first three, in the cell's own lines, and how to fix the common ones. */
export function typeRefusal(source: string, errors: CellDiagnostic[]): string {
  return [
    `Not run: this cell has ${errors.length === 1 ? 'a type error' : `${errors.length} type errors`}.`,
    ...listed(source, errors, 3),
    ...hints(errors.slice(0, 3)),
    'Fix the type, don\'t cast: the preloaded globals and every name earlier cells bound are typed. `as unknown as T` is allowed with a comment on its line or the line above saying why.',
  ].join('\n');
}

/** What a cell is checked against: the module whose `CellGlobals` type has the globals, and which of its names to declare. */
export interface Prelude { module: string; names: string[] }

/** Declarations of each global as the type the worker gives it, so the checker sees what the cell gets; data from outside is `unknown` until a cell gives it a shape. */
export function preludeText(prelude: Prelude): string {
  return [
    `import type { CellGlobals } from ${JSON.stringify(prelude.module)};`,
    'declare global {',
    ...prelude.names.map(name => `  const ${name}: CellGlobals[${JSON.stringify(name)}];`),
    '  interface JSON { parse(text: string, reviver?: (this: unknown, key: string, value: unknown) => unknown): unknown }',
    '  interface Body { json(): Promise<unknown> }',
    '  interface Blob { json(): Promise<unknown> }',
    '}',
    'export {};',
    '',
  ].join('\n');
}

/**
 * How a later cell sees a name: `let` (a `let` or `var`, which a later cell may assign: declared with the type it was
 * inferred to have), `import` (a `const`, function, class or import binding: imported, which keeps a class's or
 * namespace's type along with its value), or `type` (an interface or type alias).
 */
export type ScopeKind = 'let' | 'import' | 'type';

/** The names a cell declares at its top level (and `var`s in its blocks), as the evaluator binds them; undefined when oxc cannot parse it. */
export function declarations(source: string): Map<string, ScopeKind> | undefined {
  let parsed: ReturnType<typeof parseSync>;
  try { parsed = parseSync('cell.ts', source, { lang: 'ts', sourceType: 'module' }); } catch { return undefined; }
  if (parsed.errors.some(error => !/'return' statement/.test(error.message))) return undefined;
  const names = new Map<string, ScopeKind>();
  const pattern = (node: Node | null | undefined, kind: ScopeKind): void => {
    if (!node) return;
    if (node.type === 'Identifier') names.set(node.name as string, kind);
    else if (node.type === 'RestElement') pattern(child(node, 'argument'), kind);
    else if (node.type === 'AssignmentPattern') pattern(child(node, 'left'), kind);
    else if (node.type === 'ArrayPattern') for (const element of node.elements as (Node | null)[]) pattern(element, kind);
    else if (node.type === 'ObjectPattern') for (const property of node.properties as Node[]) pattern(property.type === 'RestElement' ? property : child(property, 'value'), kind);
  };
  const variables = (node: Node) => {
    if (node.declare) return;
    for (const declarator of node.declarations as Node[]) pattern(child(declarator, 'id'), node.kind === 'let' || node.kind === 'var' ? 'let' : 'import');
  };
  // A `var` in a block is the cell's too; a function's or class's own scope is not.
  const nested = (node: Node): void => {
    if (/^(?:Function|ArrowFunction|Class)(?:Declaration|Expression)$|^TSDeclareFunction$/.test(node.type)) return;
    if (node.type === 'VariableDeclaration' && node.kind === 'var') variables(node);
    for (const next of children(node)) nested(next);
  };
  for (const statement of parsed.program.body as unknown as Node[]) {
    const id = child(statement, 'id');
    if (statement.type === 'VariableDeclaration') variables(statement);
    else if ((statement.type === 'FunctionDeclaration' || statement.type === 'ClassDeclaration') && id && !statement.declare) pattern(id, 'import');
    else if ((statement.type === 'TSInterfaceDeclaration' || statement.type === 'TSTypeAliasDeclaration') && id) names.set(id.name as string, 'type');
    else if (statement.type === 'ImportDeclaration') {
      for (const specifier of statement.specifiers as Node[]) {
        const local = child(specifier, 'local');
        if (local) names.set(local.name as string, statement.importKind === 'type' || specifier.importKind === 'type' ? 'type' : 'import');
      }
    } else nested(statement);
  }
  return names;
}

/**
 * The names a cell asks about with `typeof name` (an expression, not a type query), each as `name@line`: JavaScript
 * answers `'undefined'` for a name nothing bound, so a cell may ask whether an earlier one did.
 */
export function probes(source: string): Set<string> {
  const found = new Set<string>();
  let parsed: ReturnType<typeof parseSync>;
  try { parsed = parseSync('cell.ts', source, { lang: 'ts', sourceType: 'module' }); } catch { return found; }
  const lineOf = lines(source);
  const visit = (node: Node): void => {
    const argument = child(node, 'argument');
    if (node.type === 'UnaryExpression' && node.operator === 'typeof' && argument?.type === 'Identifier') found.add(`${argument.name as string}@${lineOf(argument.start)}`);
    for (const next of children(node)) visit(next);
  };
  for (const statement of parsed.program.body as unknown as Node[]) visit(statement);
  return found;
}

/** Where a top-level `return` cannot reach: its own function's or class's. */
const OWN_RETURNS = new Set(['FunctionDeclaration', 'FunctionExpression', 'ArrowFunctionExpression', 'ClassDeclaration', 'ClassExpression', 'TSModuleDeclaration']);

/**
 * `source` as the checker reads it: each top-level `return value` as `throw  value`, the same length. TypeScript reports a
 * `return` outside a function and checks nothing after the keyword; a `throw` ends the cell just as surely, so what
 * follows it is narrowed as before, and its value is checked as a trailing expression would be. A bare `return` has
 * nothing to check and stays.
 */
export function checkedReturns(source: string): string {
  if (!/\breturn\b/.test(source)) return source;
  let parsed: ReturnType<typeof parseSync>;
  try { parsed = parseSync('cell.ts', source, { lang: 'ts', sourceType: 'module' }); } catch { return source; }
  const at: number[] = [];
  const visit = (node: Node): void => {
    if (OWN_RETURNS.has(node.type)) return;
    if (node.type === 'ReturnStatement' && child(node, 'argument') && source.startsWith('return', node.start)) at.push(node.start);
    for (const next of children(node)) visit(next);
  };
  for (const statement of parsed.program.body as unknown as Node[]) visit(statement);
  let out = source;
  for (const start of at) out = `${out.slice(0, start)}throw ${out.slice(start + 'return'.length)}`;
  return out;
}

/** Whether `error` is only TypeScript not finding a name the cell probes with `typeof` (`probes`). */
export function probed(error: CellDiagnostic, asked: Set<string>): boolean {
  const name = (error.code === 2304 || error.code === 2552) && /^Cannot find name '([^']+)'/.exec(error.text)?.[1];
  return !!name && asked.has(`${name}@${error.line}`);
}

/** A lane's scope and the cell modules behind it, as a hibernated lane saves it to restore with its values. */
export interface CheckerState { cells: Record<string, string>; scope: [string, { cell: string; kind: ScopeKind }][] }

/**
 * `state` as a hibernate file keeps it: every cell module (a later one may import an earlier one's types), the types in
 * scope, and of the values only `saved` ones, each a `let` as restore binds it. So restoring a file and saving it again
 * writes the same bytes (`LaneChecker.load` keeps exactly this).
 */
export function persistable(state: CheckerState, saved: string[]): CheckerState {
  const kept = new Set(saved);
  return {
    cells: state.cells,
    scope: state.scope.filter(([name, entry]) => entry.kind === 'type' || kept.has(name)).map(([name, entry]) => [name, entry.kind === 'type' ? entry : { cell: entry.cell, kind: 'let' }]),
  };
}

/** One lane's view of the type checker (type-service.ts): in the server itself, or over the worker's link to it. */
export interface LaneChecker {
  /**
   * The diagnostics TypeScript reports for `source` as the lane's next cell, on the cell's own lines.
   */
  diagnostics(source: string): Promise<CellDiagnostic[]>;
  /** `diagnostics`, and the plans of the cell's `parse<T>(…)` calls, which its run needs. */
  inspect(source: string): Promise<Inspection>;
  /** `source` ran and bound `bound` (all it declares when not given): its names join the lane's scope. */
  commit(source: string, bound?: string[]): Promise<void>;
  save(): Promise<CheckerState>;
  /** A saved scope, keeping the values in `restored` (each a `let` now, as the evaluator restores it) and every type. */
  load(state: CheckerState, restored: string[]): Promise<void>;
  /** Load the lane's project and the globals' types in the background, so its first cell does not wait for them. */
  warm(): Promise<void>;
}

export interface VetOptions {
  /** Refuse the escapes from checking, with what they hid (config.ts `denyAny`). Default true. */
  denyAny?: boolean;
  /** Refuse a cell with a type error (config.ts `typecheck`). Default true. */
  typecheck?: boolean;
  /** Told when the checker failed and the cell runs unchecked. */
  warn?: (text: string) => void;
  /** Told the packages the cell imports that have no types where it runs: each is `any` there, which is not refused. */
  missing?: (modules: string[]) => void;
}

/** What vetting decided: the refusal, or the cell to run, with each `parse<T>(…)` given the plan its check compiled. */
export interface Vetted { refusal?: string; run?: string }

/** `source` with each parse site's plan passed as `parse`'s last argument, on the same line, so every line keeps its number. */
export function withPlans(source: string, parses: ParseSite[]): string {
  let out = source;
  for (const site of [...parses].sort((a, b) => b.at - a.at)) out = `${out.slice(0, site.at)}, ${JSON.stringify(site.plan)}${out.slice(site.at)}`;
  return out;
}

/** Whether `source` may hold a `parse<T>(…)` call, which only runs with the plan a check compiles for it. */
const PARSES = /\bparse\s*</;

/**
 * Whether `source` (TypeScript, after `$ ` lines became calls) may run: undefined when it may, else the refusal. A cell
 * with an escape is refused for it, with the errors it hid (and, when the checker fails, still for the escape); any other
 * is refused for its type errors. When the checker fails on a cell without escapes, the cell runs and `warn` says so.
 */
export async function vetCell(source: string, checker: Pick<LaneChecker, 'diagnostics'> & Partial<Pick<LaneChecker, 'inspect'>>, options: VetOptions = {}): Promise<string | undefined> {
  return (await vetTyped(source, checker, options)).refusal;
}

/** `vetCell`, and the cell to run: `source` with the plans its `parse<T>(…)` calls need (undefined when it has none). */
export async function vetTyped(source: string, checker: Pick<LaneChecker, 'diagnostics'> & Partial<Pick<LaneChecker, 'inspect'>>, options: VetOptions = {}): Promise<Vetted> {
  const { denyAny = true, typecheck = true, warn, missing } = options;
  const inspect = async () => checker.inspect ? checker.inspect(source) : { diagnostics: await checker.diagnostics(source), parses: [] };
  const escapes = denyAny ? denied(findEscapes(source)) : [];
  if (escapes.length) {
    // An explained `as unknown as` stays in both copies, so neither check reports what it allows.
    let hidden: CellDiagnostic[] | { failed: string };
    try {
      const asWritten = await checker.diagnostics(source);
      const without = await checker.diagnostics(withoutEscapes(source, escapes));
      hidden = hiddenErrors(asWritten, without);
    } catch (error) {
      hidden = { failed: error instanceof Error ? error.message : String(error) };
    }
    return { refusal: refusal(source, escapes, hidden) };
  }
  if (!typecheck) {
    // Type errors are not refused, but a `parse<T>` still needs its plan to run.
    if (!PARSES.test(source)) return {};
    try { const { parses } = await inspect(); return parses.length ? { run: withPlans(source, parses) } : {}; } catch { return {}; }
  }
  let found: CellDiagnostic[];
  let parses: ParseSite[];
  try { ({ diagnostics: found, parses } = await inspect()); } catch (error) {
    warn?.(`this cell ran without its type check: ${error instanceof Error ? error.message : String(error)}`);
    return {};
  }
  const untyped = found.filter(diagnostic => diagnostic.code === 2307 || diagnostic.code === 2792 || diagnostic.code === 7016).map(diagnostic => MISSING_MODULE.exec(diagnostic.text)?.[1]);
  if (untyped.length && missing) missing([...new Set(untyped.filter(name => name !== undefined))]);
  const asked = probes(source);
  const errors = found.filter(diagnostic => !NOT_ERRORS.has(diagnostic.code) && !probed(diagnostic, asked));
  if (errors.length) return { refusal: typeRefusal(source, errors) };
  return parses.length ? { run: withPlans(source, parses) } : {};
}
