import { createRequire } from 'node:module';
import { parse } from 'acorn';
import { AT, CANVAS_TEXT_PROPS, DRAWING, NOT_DRAWN, OUTRO, PROPS, SVELTE, TEXT_PROP, TRANSITIONS, WORDS_ONLY, article, BIND_ALIAS, bindError, isKnownElement, notDrawnError, unknownPropError } from './element.ts';
import { SVG_SAID, attributeError, isSvgTag, svgSizeError, unknownTagError } from './draw.ts';
import { svelteSpecifier } from './files.ts';

/*
 * A cell's markup, compiled. A cell is TypeScript, as ever, or a Svelte 5 component: a cell that starts with markup (`<script>`,
 * `<Column>`, ...) is one, and `svelte\`...\`` inside a TypeScript cell is one too (a template with no `${}` in it, so its `{...}`
 * and backticks are its own). Both go through Svelte's compiler with `experimental.customRenderer` (svelte.ts), in runes mode
 * only, and what comes out is an expression: the compiled component, built where the cell's own names are in scope (`interval`,
 * what an earlier cell declared), which a module could not see. The compiler's imports become reads of the preloaded `__repl_svelte`
 * and the cell's own imports are carried over on their own, because they cannot live in an expression; they ride on the first line, and
 * the expression is one line, so no line of the cell moves. The compiler's warnings come back as text for the agent: the autofixer in the loop.
 */

type Anything = Record<string, any>;
type Node = { type: string; start: number; end: number; [key: string]: any };

/** The module name the compiler bakes into a component for its renderer; replaced by the runtime's, never loaded. */
const RENDERER = 'harness:renderer';
/** What a component's free names are read through, per mount, and the cell-side getters and setters that back it (svelte.ts, `free`). */
const FREE = '$$free';
const TABLE = '$$cell';

const load = createRequire(import.meta.url);
let compiler: { compile(source: string, options: Anything): Anything; parse(source: string, options: Anything): Anything } | undefined;

/** Svelte's compiler, loaded the first time a cell needs it (it costs 40 to 80 ms). */
function svelteCompiler(): NonNullable<typeof compiler> {
  return compiler ??= load('svelte/compiler');
}

/** What Svelte 4 code trips over in runes mode, and what to write instead. The error already says it; this says it once for all of them. */
const SVELTE_4 = new Set([
  'legacy_export_invalid', 'legacy_reactive_statement_invalid', 'legacy_props_invalid', 'legacy_rest_props_invalid',
  'incompatible_with_custom_renderer', 'slot_element_deprecated', 'event_directive_deprecated', 'runes_mode_invalid_import',
]);
const SVELTE_4_HINT = 'cells are Svelte 5 runes only: props with `let { x } = $props()` (never `export let`), `$state`, `$derived` and `$effect` (never `$:`), events as `onclick={...}` (never `on:click`), {@render children?.()} (never <slot>)';

/** Where `await` goes in a component, said when one does not parse. */
const AWAIT_HINT = 'await works at the top of <script>, in $derived(await ...) and in markup as {await ...}, with <svelte:boundary> around what waits; inside a function, mark the function async';

/** Warnings that say nothing a cell's author can act on (there is no stylesheet, no page and no screen reader here). */
const IGNORED = /^(?:css_|a11y_)/;

export interface Compiled {
  /** The JavaScript expression that is the component, one line. */
  expression: string;
  /** The cell's own import lines, as one-line statements. */
  imports: string[];
  /** What the compiler noticed, in the author's words and lines. */
  warnings: string[];
}

const syntaxError = (message: string) => Object.assign(new SyntaxError(message), { stack: `SyntaxError: ${message}` });

/** ` (line:column)` of a compiler error or warning in the cell; `edits` are what the passes before the compiler inserted into `source`, whose columns they move. */
function position(error: Anything, shift: number, source?: string, edits?: Splice[][]): string {
  const line = error?.start?.line;
  const column = error?.start?.column;
  if (typeof line !== 'number') return '';
  const character = error?.start?.character;
  if (source !== undefined && edits && typeof character === 'number') {
    const at = back(edits, character);
    const before = source.slice(0, at);
    return ` (${before.split('\n').length + shift}:${at - (before.lastIndexOf('\n') + 1) + 1})`;
  }
  return ` (${line + shift}:${(column ?? 0) + 1})`;
}

/** `shift` moves the lines of an error or a warning from the template's to the cell's; `source` is the template. */
function describe(error: unknown, shift: number, source = '', edits?: Splice[][]): Error {
  if (!(error instanceof Error)) return syntaxError(String(error));
  const code = Reflect.get(error, 'code') as string | undefined;
  const first = error.message.split('\n', 1)[0] ?? error.message;
  const frame = Reflect.get(error, 'frame');
  const awaits = /\bawait\b/.test(source) && (code === 'js_parse_error' || /\bawait\b/.test(first));
  const hint = code && SVELTE_4.has(code) ? `\n${SVELTE_4_HINT}` : awaits ? `\n${AWAIT_HINT}` : '';
  return syntaxError(`${first}${position(error as Anything, shift, source, edits)}${typeof frame === 'string' && !shift && !edits?.some(splices => splices.length) ? `\n${frame}` : ''}${hint}`);
}

const children = (node: Node): Node[] => {
  const found: Node[] = [];
  for (const value of Object.values(node)) {
    if (Array.isArray(value)) { for (const item of value) if (item && typeof item === 'object' && typeof item.type === 'string') found.push(item as Node); }
    else if (value && typeof value === 'object' && typeof (value as Node).type === 'string') found.push(value as Node);
  }
  return found;
};

interface Splice { start: number; end: number; text: string }
/** `code` with `splices` made; `log`, when given, keeps them, so a place in the result can be found again in `code` (`back`). */
function apply(code: string, splices: Splice[], log?: Splice[][]): string {
  let result = code;
  const sorted = [...splices].sort((a, b) => b.start - a.start);
  for (const splice of sorted) result = result.slice(0, splice.start) + splice.text + result.slice(splice.end);
  log?.push(sorted.reverse());
  return result;
}

/** The offset in the text before `edits` (each a pass's splices, in the order made) of `offset` in the text after; text a splice put in is at its start. */
function back(edits: Splice[][], offset: number): number {
  let at = offset;
  for (const splices of [...edits].reverse()) {
    let delta = 0;
    for (const splice of splices) {
      const start = splice.start + delta;
      if (at < start) break;
      if (at < start + splice.text.length) { at = splice.start + delta; delta = 0; break; }
      delta += splice.text.length - (splice.end - splice.start);
    }
    at -= delta;
  }
  return at;
}

/** The offset of a line (from 1) and column (from 1) in `text`. */
function offsetOf(text: string, line: number, column: number): number {
  let at = 0;
  for (let n = 1; n < line; n++) { const next = text.indexOf('\n', at); if (next < 0) break; at = next + 1; }
  return at + column - 1;
}

/** A name the compiled component reads or assigns that nothing in it declares. */
interface Reference { node: Node; name: string; shorthand: boolean; write: boolean }

const FUNCTIONS = new Set(['FunctionDeclaration', 'FunctionExpression', 'ArrowFunctionExpression']);
const CLASSES = new Set(['ClassDeclaration', 'ClassExpression']);
/**
 * Names that are values of the language, not variables anyone supplies (and `arguments` and `eval` cannot be assigned through a wrapper),
 * and the preloaded runtime, which a prop's `{...}` is checked through (`checkElements`).
 */
/** What the cell's `$` (Bun's shell) is called inside a component, since Svelte keeps the name `$` for itself (`aliasDollar`). */
const DOLLAR = '__repl_dollar';
const NOT_FREE = new Set(['undefined', 'NaN', 'Infinity', 'arguments', 'eval', SVELTE, DOLLAR]);

function patternNames(pattern: Node | null, into: Set<string>): void {
  if (!pattern) return;
  switch (pattern.type) {
    case 'Identifier': into.add(pattern.name); break;
    case 'ObjectPattern': for (const property of pattern.properties as Node[]) patternNames(property.type === 'RestElement' ? property.argument : property.value, into); break;
    case 'ArrayPattern': for (const element of pattern.elements as Array<Node | null>) patternNames(element, into); break;
    case 'RestElement': patternNames(pattern.argument, into); break;
    case 'AssignmentPattern': patternNames(pattern.left, into); break;
  }
}

/** What a list of statements declares in its own block: let, const, class, function and import. */
function lexicalNames(body: Node[], into: Set<string>): void {
  for (const node of body) {
    if (node.type === 'VariableDeclaration' && node.kind !== 'var') for (const declarator of node.declarations as Node[]) patternNames(declarator.id, into);
    else if ((node.type === 'FunctionDeclaration' || node.type === 'ClassDeclaration') && node.id) into.add(node.id.name);
    else if (node.type === 'ImportDeclaration') for (const specifier of node.specifiers as Node[]) into.add(specifier.local.name);
  }
}

/** The `var`s of a function or program: those in nested blocks too, those in nested functions and classes not. */
function varNames(node: Node, into: Set<string>): void {
  if (FUNCTIONS.has(node.type) || CLASSES.has(node.type)) return;
  if (node.type === 'VariableDeclaration' && node.kind === 'var') for (const declarator of node.declarations as Node[]) patternNames(declarator.id, into);
  for (const child of children(node)) varNames(child, into);
}

/**
 * The names a program reads or assigns without declaring them anywhere that is in scope there: the cell's variables, the preloaded
 * elements and globals, and, once `render(component, props)` is known, the props. A scope is a function, a block, a loop, a `switch`, a
 * `catch` or a class; a name is a use when it is not a property name, a label or a declaration.
 */
function freeReferences(tree: Node): Reference[] {
  interface Scope { names: Set<string>; parent?: Scope }
  const found: Reference[] = [];
  const declared = (scope: Scope | undefined, name: string): boolean => {
    for (let at = scope; at; at = at.parent) if (at.names.has(name)) return true;
    return false;
  };
  const use = (node: Node, scope: Scope, shorthand = false, write = false): void => {
    if (!NOT_FREE.has(node.name) && !declared(scope, node.name)) found.push({ node, name: node.name, shorthand, write });
  };

  /** The expressions inside a pattern (defaults, computed keys), and, when it assigns rather than declares, its names as uses. */
  const pattern = (node: Node | null, scope: Scope, assigning: boolean): void => {
    if (!node) return;
    switch (node.type) {
      case 'Identifier': if (assigning) use(node, scope, false, true); break;
      case 'ObjectPattern':
        for (const property of node.properties as Node[]) {
          if (property.type === 'RestElement') { pattern(property.argument, scope, assigning); continue; }
          if (property.computed) visit(property.key, scope);
          pattern(property.value, scope, assigning);
        }
        break;
      case 'ArrayPattern': for (const element of node.elements as Array<Node | null>) pattern(element, scope, assigning); break;
      case 'RestElement': pattern(node.argument, scope, assigning); break;
      case 'AssignmentPattern': pattern(node.left, scope, assigning); visit(node.right, scope); break;
      default: visit(node, scope);
    }
  };

  const block = (statements: Node[], scope: Scope, extra: Set<string> = new Set()): void => {
    lexicalNames(statements, extra);
    const inner: Scope = { names: extra, parent: scope };
    for (const statement of statements) visit(statement, inner);
  };

  const visit = (node: Node, scope: Scope, parent?: Node, key?: string): void => {
    switch (node.type) {
      case 'Identifier': use(node, scope, parent?.type === 'Property' && parent.shorthand === true && key === 'value'); return;
      case 'MemberExpression': visit(node.object, scope); if (node.computed) visit(node.property, scope); return;
      case 'Property': case 'MethodDefinition': case 'PropertyDefinition':
        if (node.computed) visit(node.key, scope);
        if (node.value) visit(node.value, scope, node, 'value');
        return;
      case 'LabeledStatement': visit(node.body, scope); return;
      case 'BreakStatement': case 'ContinueStatement': case 'MetaProperty': case 'ImportDeclaration': return;
      case 'VariableDeclaration':
        for (const declarator of node.declarations as Node[]) { pattern(declarator.id, scope, false); if (declarator.init) visit(declarator.init, scope); }
        return;
      case 'FunctionDeclaration': case 'FunctionExpression': case 'ArrowFunctionExpression': {
        const names = new Set<string>();
        if (node.type === 'FunctionExpression' && node.id) names.add(node.id.name);
        for (const param of node.params as Node[]) patternNames(param, names);
        varNames(node.body, names);
        if (node.body.type === 'BlockStatement') lexicalNames(node.body.body, names);
        const inner: Scope = { names, parent: scope };
        for (const param of node.params as Node[]) pattern(param, inner, false);
        if (node.body.type === 'BlockStatement') for (const statement of node.body.body as Node[]) visit(statement, inner);
        else visit(node.body, inner);
        return;
      }
      case 'ClassDeclaration': case 'ClassExpression': {
        if (node.superClass) visit(node.superClass, scope);
        const inner: Scope = { names: new Set(node.id ? [node.id.name] : []), parent: scope };
        for (const member of node.body.body as Node[]) visit(member, inner);
        return;
      }
      case 'StaticBlock': {
        const names = new Set<string>();
        for (const statement of node.body as Node[]) varNames(statement, names);
        block(node.body, scope, names);
        return;
      }
      case 'BlockStatement': block(node.body, scope); return;
      case 'ForStatement': {
        const names = new Set<string>();
        if (node.init) lexicalNames([node.init], names);
        const inner: Scope = { names, parent: scope };
        for (const part of [node.init, node.test, node.update, node.body]) if (part) visit(part, inner);
        return;
      }
      case 'ForInStatement': case 'ForOfStatement': {
        const names = new Set<string>();
        lexicalNames([node.left], names);
        const inner: Scope = { names, parent: scope };
        if (node.left.type === 'VariableDeclaration') visit(node.left, inner); else pattern(node.left, inner, true);
        visit(node.right, inner);
        visit(node.body, inner);
        return;
      }
      case 'SwitchStatement': {
        visit(node.discriminant, scope);
        const names = new Set<string>();
        for (const clause of node.cases as Node[]) lexicalNames(clause.consequent, names);
        const inner: Scope = { names, parent: scope };
        for (const clause of node.cases as Node[]) { if (clause.test) visit(clause.test, inner); for (const statement of clause.consequent as Node[]) visit(statement, inner); }
        return;
      }
      case 'CatchClause': {
        const names = new Set<string>();
        patternNames(node.param, names);
        const inner: Scope = { names, parent: scope };
        pattern(node.param, inner, false);
        visit(node.body, inner);
        return;
      }
      case 'AssignmentExpression': pattern(node.left, scope, true); visit(node.right, scope); return;
      case 'UpdateExpression': if (node.argument.type === 'Identifier') use(node.argument, scope, false, true); else visit(node.argument, scope); return;
    }
    for (const [name, value] of Object.entries(node)) {
      if (Array.isArray(value)) { for (const item of value) if (item && typeof item === 'object' && typeof item.type === 'string') visit(item as Node, scope, node, name); }
      else if (value && typeof value === 'object' && typeof (value as Node).type === 'string') visit(value as Node, scope, node, name);
    }
  };

  const names = new Set<string>();
  varNames(tree, names);
  lexicalNames(tree.body as Node[], names);
  const program: Scope = { names };
  for (const statement of tree.body as Node[]) visit(statement, program);
  return found;
}

/** The cell line of the template's line `index` (from 0), and that line's text, as an error names the place: `line 5: <Image src={p} />`. */
function lineAt(lines: string[], index: number, shift: number): { line: number; at: string } {
  const text = (lines[index] ?? '').trim().replace(/\s+/g, ' ');
  return { line: shift + index + 1, at: `line ${shift + index + 1}: ${text.length > 80 ? `${text.slice(0, 79)}…` : text}` };
}

/** The cell line of a free name's first use in the template, and that line's text, for the error of a name that is nothing. */
function whereIs(name: string, context: { source: string; shift: number }): { line: number; at: string } {
  const pattern = new RegExp(`(?<![\\w$.])${name.replace(/[$]/g, '\\$&')}(?![\\w$])`);
  const lines = context.source.split('\n');
  return lineAt(lines, Math.max(0, lines.findIndex(line => pattern.test(line))), context.shift);
}

/**
 * The cell's `$` (Bun's shell: `` await $`df -k /`.text() ``) used in a component's script or markup: Svelte refuses `$` as a name, so
 * each use becomes `__repl_dollar`, which `toExpression` binds to the cell's `$`. A property named `$` (`x.$`, `{ $: 1 }`) stays.
 */
function aliasDollar(source: string, log: Splice[][]): { source: string; aliased: boolean } {
  if (!/(?<![\w$])\$(?![\w$])/.test(source)) return { source, aliased: false };
  let ast: Anything;
  try { ast = svelteCompiler().parse(source, { modern: true }); } catch { return { source, aliased: false }; }
  const splices: Splice[] = [];
  const visit = (node: unknown, parent?: Anything, key?: string): void => {
    if (Array.isArray(node)) { for (const item of node) visit(item, parent, key); return; }
    if (!node || typeof node !== 'object') return;
    const it = node as Anything;
    if (it.type === 'Identifier' && it.name === '$' && typeof it.start === 'number') {
      const property = parent?.type === 'MemberExpression' && key === 'property' && !parent.computed;
      const named = parent?.type === 'Property' && key === 'key' && !parent.computed && !parent.shorthand;
      // `$:` is a label (Svelte 4's reactive statement), which the compiler is to refuse with its own words.
      const label = key === 'label';
      if (!property && !named && !label) splices.push({ start: it.start, end: it.end, text: parent?.type === 'Property' && parent.shorthand ? `$: ${DOLLAR}` : DOLLAR });
    }
    for (const [name, child] of Object.entries(it)) if (name !== 'metadata' && child && typeof child === 'object') visit(child, it, name);
  };
  visit([ast.instance, ast.module, ast.fragment]);
  // A shorthand property is its key and its value at once: one splice for it, not two.
  const unique = splices.filter((splice, index) => splices.findIndex(other => other.start === splice.start) === index);
  return { source: apply(source, unique, log), aliased: unique.length > 0 };
}

/** What a boundary shows while it waits when it does not say: a quiet line. */
export const PENDING_SNIPPET = '{#snippet pending()}<svelte:element this="Text" tone="muted" text="Loading\u2026" />{/snippet}';
/** What a boundary shows once it failed when it does not say: the error's first line in the error tone. */
export const FAILED_SNIPPET = `{#snippet failed(error)}<svelte:element this="Text" tone="error" text={${SVELTE}.said(error)} />{/snippet}`;

/**
 * A `<svelte:boundary>` that does not say what to show while it waits or once it fails gets the app's (`PENDING_SNIPPET`,
 * `FAILED_SNIPPET`). The snippets go on the line of the boundary's first child, so no line moves.
 */
function withBoundaryDefaults(source: string, log: Splice[][]): string {
  if (!source.includes('svelte:boundary')) return source;
  let ast: Anything;
  try { ast = svelteCompiler().parse(source, { modern: true }); } catch { return source; }
  const splices: Splice[] = [];
  const visit = (node: unknown): void => {
    if (Array.isArray(node)) { for (const item of node) visit(item); return; }
    if (!node || typeof node !== 'object') return;
    const it = node as Anything;
    const nodes = it.type === 'SvelteBoundary' ? it.fragment?.nodes as Anything[] | undefined : undefined;
    const first = nodes?.[0];
    if (nodes && first) {
      const says = (name: string) => nodes.some(child => child.type === 'SnippetBlock' && child.expression?.name === name)
        || (it.attributes as Anything[]).some(attribute => attribute.name === name);
      const text = (says('pending') ? '' : PENDING_SNIPPET) + (says('failed') ? '' : FAILED_SNIPPET);
      if (text) splices.push({ start: first.start, end: first.start, text });
    }
    for (const [name, child] of Object.entries(it)) if (name !== 'metadata' && child && typeof child === 'object') visit(child);
  };
  visit(ast.fragment);
  return apply(source, splices, log);
}

/** The place an error names: the cell line and column of `index` in the template, and that line's text, `line 5:3: <Image src={p} />`. */
function placeOf(source: string, lines: string[], index: number, shift: number): { line: number; at: string } {
  const line = lineOf(source, index);
  const column = index - (source.lastIndexOf('\n', index - 1) + 1) + 1;
  const { line: cell, at } = lineAt(lines, line - 1, shift);
  return { line: cell, at: at.replace(/^line (\d+):/, `line $1:${column}:`) };
}

/** Where markup is: among the app's elements, inside a drawing (a Canvas or an `<svg>`), or where the pass cannot tell (a snippet, a component of the cell's own). */
type Within = 'page' | 'draw' | 'unknown';

/** The value of an attribute written as text (`fill="ink"`, a bare `legend`), or undefined when any of it is an expression. */
function textValue(value: true | Anything | Anything[]): string | true | undefined {
  if (value === true) return true;
  if (Array.isArray(value) && value.every(part => part.type === 'Text')) return value.map(part => part.data as string).join('');
  return undefined;
}

/**
 * Every element of the markup that is the app's (`<Code>`, `<Rect>`) or an SVG tag (draw.ts), checked at the line and column the
 * cell writes it, before anything runs:
 * - a lowercase element that is neither (`<div>`) is an error naming what to use, and one that draws only in a drawing (`<rect>`,
 *   `<Rect>`) outside one, or one that does not draw (`<Row>`) inside one, is an error too;
 * - an attribute the element does not read is an error naming what it reads (Svelte would pass it on to nothing), and so is markup
 *   between the tags of one that takes no children;
 * - a value written as text is checked now (element.ts `checkProp`, draw.ts `svgError`), and a `{...}` value is wrapped in
 *   `__repl_svelte.prop(...)` (svelte.ts), so a value that is not one is the same error at that line when it comes;
 * - each `bind:` is checked as before (element.ts `bindError`);
 * - each element is given its place as the attribute `__at` (element.ts `AT`), for the checks that can only run once the markup has
 *   (a spread, a Chart's data against its kind), which then name the same place.
 * Only text is inserted, on the lines it is about, so no line moves. A component of the cell's own, or `<svelte:element>`, is not checked.
 */
function checkElements(source: string, shift: number, log: Splice[][]): string {
  let ast: Anything;
  // What does not parse is the compiler's to report, with its own words.
  try { ast = svelteCompiler().parse(source, { modern: true }); } catch { return source; }
  const lines = source.split('\n');
  const splices: Splice[] = [];
  const fail = (error: Error) => syntaxError(error.message);

  const element = (node: Anything, within: Within): Within => {
    const name = node.name as string;
    const svg = node.type === 'RegularElement';
    const { at } = placeOf(source, lines, node.start, shift);
    const said = `<${name}>`;
    // Where it is, and so where what it holds is.
    let inner: Within = 'page';
    if (svg && !isSvgTag(name)) throw fail(unknownTagError(name, at));
    if (name === 'svg' || name === 'Canvas') {
      if (within === 'draw') throw fail(new TypeError(`${said} inside a drawing (${at}): a drawing holds no other; group shapes with ${name === 'svg' ? '<g>' : '<Group>'}`));
      inner = 'draw';
    } else if (svg || DRAWING.has(name)) {
      if (within === 'page') throw fail(new TypeError(`${said} draws only inside ${svg ? 'an <svg viewBox="...">' : 'a <Canvas view="...">'} (${at}): put it in one`));
      inner = 'draw';
    } else if (within === 'draw' && name !== 'Text') {
      throw fail(new TypeError(`${said} inside a drawing (${at}): a drawing holds only ${SVG_SAID} (or Group, Path, Rect, Circle, Line, Polyline and Text); put the drawing in a Row or Column beside it`));
    }
    if (!svg && !PROPS[name as keyof typeof PROPS].includes('children')) {
      const kids = (node.fragment?.nodes ?? []) as Anything[];
      if (kids.some(kid => kid.type !== 'Comment' && !(kid.type === 'Text' && !(kid.data as string).trim()))) {
        throw fail(unknownPropError(name as keyof typeof PROPS, 'children', undefined, at));
      }
    }
    // A Button's (a Chip's, a Loading's, a Toast's) children are its words, a Shader's its source: text and {expressions}, never another element.
    if (WORDS_ONLY.has(name)) {
      const inner = ((node.fragment?.nodes ?? []) as Anything[]).find(kid => kid.type === 'RegularElement' || kid.type === 'Component' || kid.type === 'SvelteElement');
      const holds = name === 'Shader' ? 'only its Slang source, <Shader>float4 main(ShaderInput in) { ... }</Shader>' : `only its words, <${name}>Save</${name}> or ${TEXT_PROP[name]}="Save"`;
      if (inner) throw fail(new TypeError(`<${name}> holds <${inner.name ?? 'svelte:element'}> (${at}): ${article(name)} ${name} holds ${holds}`));
    }
    // The text values of its attributes, for the checks that read two together (a Chart's kind with its data).
    const known: Record<string, unknown> = {};
    for (const attribute of node.attributes as Anything[]) {
      if (attribute.type === 'Attribute' && textValue(attribute.value) !== undefined) known[attribute.name] = textValue(attribute.value);
    }
    let spread = false;
    for (const attribute of node.attributes as Anything[]) {
      const { at: here } = placeOf(source, lines, attribute.start, shift);
      if (attribute.type === 'SpreadAttribute') { spread = true; continue; }
      if (attribute.type === 'BindDirective') {
        const error = bindError(name, attribute.name, here);
        if (error) throw fail(error);
        // `bind:checked` on a Toggle is its `bind:value` (element.ts `BIND_ALIAS`); `bind:checked` alone binds `checked`, so it keeps that name as the value.
        const alias = BIND_ALIAS[name]?.[attribute.name];
        if (alias) {
          const from = attribute.start + 'bind:'.length;
          const shorthand = !source.slice(attribute.start, attribute.end).includes('=');
          splices.push({ start: from, end: from + attribute.name.length, text: shorthand ? `${alias}={${attribute.name}}` : alias });
        }
        continue;
      }
      if (attribute.type === 'StyleDirective') throw fail(new TypeError(`style:${attribute.name} on ${said} is not drawn (${here}): no styles; colour comes from tone= on words and fill= and stroke= (an ink) on shapes`));
      if (attribute.type !== 'Attribute') continue;
      const written = attribute.name as string;
      if (written === AT) continue;
      // `checked={x}` on a Toggle is its `value={x}` (element.ts `BIND_ALIAS`); `{checked}` gets its name put in front below.
      const prop = BIND_ALIAS[name]?.[written] ?? written;
      if (prop !== written && source[attribute.start] !== '{') splices.push({ start: attribute.start, end: attribute.start + written.length, text: prop });
      if (name === 'Text' && within === 'page' && CANVAS_TEXT_PROPS.has(prop)) {
        throw fail(new TypeError(`<Text ${prop}=...> outside a drawing (${here}): only a Text in a Canvas or an <svg> takes anchor, size, fill, value and onclick; words elsewhere take tone and bold`));
      }
      if (name === 'Text' && within === 'draw' && prop === 'tone') {
        throw fail(new TypeError(`<Text tone=...> inside a drawing (${here}): words in a drawing take fill= (an ink), not tone`));
      }
      const text = textValue(attribute.value);
      // The name first (a prop it has not), then, written as text, the value.
      const error = attributeError(name, prop, text, here, known);
      if (error) throw fail(error);
      const value = attribute.value as Anything;
      if (text !== undefined || /^on/.test(prop) || Array.isArray(value) || value.type !== 'ExpressionTag') continue;
      // `{tip}` is `tip={tip}`: its name goes in front, or the wrap would leave the prop without one.
      if (source[attribute.start] === '{') splices.push({ start: attribute.start, end: attribute.start, text: `${prop}=` });
      const also = name === 'Chart' && (prop === 'data' || prop === 'series') && Object.keys(known).length > 0 ? `, ${JSON.stringify(known)}` : '';
      splices.push({ start: value.expression.start, end: value.expression.start, text: `${SVELTE}.prop(` });
      splices.push({ start: value.expression.end, end: value.expression.end, text: `, ${JSON.stringify(name)}, ${JSON.stringify(prop)}, ${JSON.stringify(here)}${also})` });
    }
    if (name === 'svg' && !spread) {
      const dynamic = (node.attributes as Anything[]).some(a => a.type === 'Attribute' && ['viewBox', 'width', 'height'].includes(a.name) && textValue(a.value) === undefined);
      const error = dynamic ? undefined : svgSizeError(known, at);
      if (error) throw fail(error);
    }
    // Its place goes first, so it is set before the attributes whose errors name it.
    splices.push({ start: node.start + 1 + name.length, end: node.start + 1 + name.length, text: ` ${AT}={${JSON.stringify(at)}}` });
    return inner;
  };

  const visit = (node: unknown, within: Within): void => {
    if (Array.isArray(node)) { for (const item of node) visit(item, within); return; }
    if (!node || typeof node !== 'object') return;
    const it = node as Anything;
    // Den's elements the app does not draw are named as such at their line, not left to be a name the cell never defined.
    if (it.type === 'Component' && NOT_DRAWN.has(it.name)) throw fail(notDrawnError(it.name, placeOf(source, lines, it.start, shift).at));
    if (it.type === 'RegularElement' || (it.type === 'Component' && isKnownElement(it.name))) {
      visit(it.fragment, element(it, within));
      return;
    }
    // What a snippet or a component of the cell's own holds may be drawn anywhere.
    const next: Within = it.type === 'SnippetBlock' || it.type === 'Component' || it.type === 'SvelteElement' || it.type === 'SvelteComponent' ? 'unknown' : within;
    for (const [key, child] of Object.entries(it)) if (key !== 'metadata' && child && typeof child === 'object') visit(child, next);
  };
  visit(ast.fragment, 'page');
  return apply(source, splices, log);
}

/**
 * Svelte's `transition:`, `in:` and `out:` on one of the app's elements, and `animate:flip`, as the props the app springs them by (a
 * component cannot take a directive): `in:fly={{ y: 8 }}` is `enter={__repl_svelte.trans('fly', { y: 8 }, ...)}`, `out:` the same as
 * `__outro`, which the renderer plays when the element goes (svelte.ts `removeNode`), `transition:` both, and `animate:flip` is
 * `flip={true}`. A name the app does not play is an error at its line, and so is a directive on an SVG tag (draw one of the Canvas
 * elements instead). `before` is the template before `checkElements` (whose splices are the last of `log`), for the places errors
 * name. Only text on the directive's own lines changes, so no line moves. Copied from den's repl/src/ui/compile.ts.
 */
function mapTransitions(source: string, before: string, shift: number, log: Splice[][]): string {
  if (!/\b(?:transition|in|out|animate):/.test(source)) return source;
  let ast: Anything;
  try { ast = svelteCompiler().parse(source, { modern: true }); } catch { return source; }
  const lines = before.split('\n');
  const splices: Splice[] = [];
  const directive = (attribute: Anything) => attribute.type === 'AnimateDirective' ? 'animate' : attribute.intro && attribute.outro ? 'transition' : attribute.intro ? 'in' : 'out';
  const visit = (node: unknown): void => {
    if (Array.isArray(node)) { for (const item of node) visit(item); return; }
    if (!node || typeof node !== 'object') return;
    const element = node as Anything;
    const ours = element.type === 'Component' && isKnownElement(element.name);
    if (ours || element.type === 'RegularElement') {
      for (const attribute of element.attributes as Anything[]) {
        if (attribute.type !== 'AnimateDirective' && attribute.type !== 'TransitionDirective') continue;
        const { line, at } = placeOf(before, lines, back(log, attribute.start), shift);
        const written = `${directive(attribute)}:${attribute.name}`;
        if (!ours) throw syntaxError(`${written} on <${element.name}> is not played (${at}): a drawing's shapes come and go as the Canvas elements, <Rect in:fade ...> inside a <Canvas>`);
        if (attribute.type === 'AnimateDirective') {
          if (attribute.name !== 'flip') throw syntaxError(`${written} on <${element.name}> is not one the app plays (${at}): only animate:flip, which eases an element to where it moved`);
          splices.push({ start: attribute.start, end: attribute.end, text: 'flip={true}' });
          continue;
        }
        if (!(TRANSITIONS as readonly string[]).includes(attribute.name)) throw syntaxError(`${written} on <${element.name}> is not a transition the app plays (${at}): ${TRANSITIONS.join(', ')}`);
        const params = attribute.expression ? source.slice(attribute.expression.start, attribute.expression.end) : 'undefined';
        const call = (text: string) => `${SVELTE}.trans(${JSON.stringify(attribute.name)}, ${text}, ${line}, ${JSON.stringify(at)})`;
        const props: string[] = [];
        if (attribute.intro) props.push(`enter={${call(params)}}`);
        // The second copy is on the first's line, so a param that spans lines is joined there.
        if (attribute.outro) props.push(`${OUTRO}={${call(attribute.intro ? params.replace(/\s*\n\s*/g, ' ') : params)}}`);
        splices.push({ start: attribute.start, end: attribute.end, text: props.join(' ') });
      }
    }
    for (const [key, child] of Object.entries(element)) if (key !== 'metadata' && child && typeof child === 'object') visit(child);
  };
  visit(ast.fragment);
  return apply(source, splices, log);
}

/** The file name a compiled component's frames carry (`repl-ui-3.js:12:40`), its number the one `remember` gave it. */
export const UI_FILE = 'repl-ui-';

/** What finds a place in a compiled component again in the cell: see `placeOfFrame`. */
interface Origin {
  /** The text evaluated, and how long its head is (on its first line) before the body: the compiler's code as `toExpression` changed it. */
  head: number;
  text: string;
  /** What `toExpression` changed, the compiler's code, and its source map (VLQ mappings) into `input`, what the compiler read. */
  moved: Splice[][];
  code: string;
  map: string;
  input: string;
  /** What the passes before the compiler inserted into `source`, the template as the cell writes it; `shift` its first line's place. */
  edits: Splice[][];
  source: string;
  shift: number;
}

/** The components compiled most recently, by number; the oldest is forgotten past this many (its frames then say only its file). */
const ORIGINS_KEPT = 512;
const origins = new Map<number, Origin>();
let originSeq = 0;
function remember(origin: Origin): number {
  origins.set(++originSeq, origin);
  for (const id of origins.keys()) { if (origins.size <= ORIGINS_KEPT) break; origins.delete(id); }
  return originSeq;
}

const BASE64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
/** The segments of one line of a source map (`[generated column, source, source line, source column]`, absolute), from its VLQ text and the source line and column running across lines. */
function segments(line: string, state: { line: number; column: number }): number[][] {
  const out: number[][] = [];
  let column = 0;
  for (const segment of line.split(',')) {
    if (!segment) continue;
    const values: number[] = [];
    let value = 0;
    let bits = 0;
    for (const char of segment) {
      const digit = BASE64.indexOf(char);
      value += (digit & 31) << bits;
      if (digit & 32) { bits += 5; continue; }
      values.push(value & 1 ? -(value >>> 1) : value >>> 1);
      value = 0;
      bits = 0;
    }
    column += values[0] ?? 0;
    if (values.length >= 4) {
      state.line += values[2]!;
      state.column += values[3]!;
      out.push([column, 0, state.line, state.column]);
    }
  }
  return out;
}

/**
 * The cell line and column, and the place as errors name it (`line 5:9: <Text>{rows.length}</Text>`), of a frame inside a compiled
 * component: `id` from its file name, `line` and `column` (from 1) within it. Undefined when the component is forgotten or the
 * place is code the compiler made up (its runtime calls, not the cell's markup or script).
 */
export function placeOfFrame(id: number, line: number, column: number): { line: number; column: number; at: string } | undefined {
  const origin = origins.get(id);
  if (!origin) return undefined;
  // Back from the evaluated text to the body (the head is on the first line only), then to the compiler's code.
  const inBody = offsetOf(origin.text, line, column) - origin.head;
  if (inBody < 0) return undefined;
  const inCode = back(origin.moved, inBody);
  const before = origin.code.slice(0, inCode);
  const genLine = before.split('\n').length - 1;
  const genColumn = inCode - (before.lastIndexOf('\n') + 1);
  // The compiler's source map: the last segment on that line at or before the column.
  const state = { line: 0, column: 0 };
  const lines = origin.map.split(';');
  let found: number[] | undefined;
  for (let n = 0; n <= genLine && n < lines.length; n++) {
    const segs = segments(lines[n]!, state);
    if (n === genLine) for (const seg of segs) if (seg[0]! <= genColumn) found = seg;
  }
  if (!found) return undefined;
  // From what the compiler read back to the template the cell wrote.
  const inSource = back(origin.edits, offsetOf(origin.input, found[2]! + 1, found[3]! + 1));
  const lead = origin.source.slice(0, inSource);
  const sourceLine = lead.split('\n').length;
  const sourceColumn = inSource - (lead.lastIndexOf('\n') + 1) + 1;
  const { line: cellLine, at } = lineAt(origin.source.split('\n'), sourceLine - 1, origin.shift);
  return { line: cellLine, column: sourceColumn, at: at.replace(/^line (\d+):/, `line $1:${sourceColumn}:`) };
}

/**
 * Compiled output as a cell expression: imports of the compiler's own become reads of `__repl_svelte`, the cell's own are lifted out,
 * and `export default function Name` is the value of an immediately called function. The names the component uses and does not
 * declare are read through `__repl_svelte.free(props, table)` (svelte.ts): the props it is mounted with first, the cell's own
 * variable (the table's getter and setter, closures in the cell's scope) second.
 */
function toExpression(code: string, context: { source: string; shift: number; dollar?: boolean; map?: string; input?: string; edits?: Splice[][] }): { expression: string; imports: string[] } {
  const tree = parse(code, { ecmaVersion: 'latest', sourceType: 'module' }) as unknown as Node;
  const header: string[] = [];
  const imports: string[] = [];
  const splices: Splice[] = [];
  let name: string | undefined;
  let component: Node | undefined;
  for (const node of tree.body as Node[]) {
    if (node.type === 'ImportDeclaration') {
      const source = node.source.value as string;
      const local = node.specifiers[0]?.local?.name as string | undefined;
      splices.push({ start: node.start, end: node.end, text: '' });
      if (source === RENDERER) header.push(`const ${local} = ${SVELTE}.renderer;`);
      else if (source === 'svelte/internal/client') header.push(`const ${local} = ${SVELTE}.client;`);
      // The flags are on for the process (svelte.ts); a side-effect import has nothing to bind.
      else if (/^svelte\/internal\/(?:flags\/|disclose-version$)/.test(source)) continue;
      else {
        const text = code.slice(node.start, node.end);
        const file = svelteSpecifier(source);
        imports.push((file ? text.replace(node.source.raw, JSON.stringify(file)) : text).replace(/\s+/g, ' ').trim().replace(/;?$/, ';'));
      }
    } else if (node.type === 'ExportDefaultDeclaration') {
      name = node.declaration.id?.name;
      component = node.declaration;
      splices.push({ start: node.start, end: node.declaration.start, text: '' });
    } else if (node.type === 'ExportNamedDeclaration' || node.type === 'ExportAllDeclaration') {
      throw syntaxError('a cell component cannot export: declare it with let or const, or put it in <script> without export');
    }
  }
  if (!name || !component) throw new Error('the Svelte compiler produced no component');
  // Only the component's own function reads props; what a `<script module>` runs stays where the cell's names are.
  const props = component.params[1] as Node | undefined;
  const used = component.params.length > 0 && (!props || props.type === 'Identifier')
    ? freeReferences(tree).filter(reference => reference.node.start >= component.body.start && reference.node.end <= component.body.end)
    : [];
  if (used.length > 0) {
    const propsName = props?.name ?? '$$props';
    if (!props) splices.push({ start: component.params[0].end, end: component.params[0].end, text: `, ${propsName}` });
    splices.push({ start: component.body.start + 1, end: component.body.start + 1, text: ` const ${FREE} = ${SVELTE}.free(${propsName}, ${TABLE});` });
    const table: string[] = [];
    for (const reference of used) {
      const { name: free, node, shorthand } = reference;
      splices.push({ start: node.start, end: node.end, text: shorthand ? `${free}: ${FREE}.${free}` : `${FREE}.${free}` });
      if (table.some(entry => entry.startsWith(`${JSON.stringify(free)}:`))) continue;
      const { line, at } = whereIs(free, context);
      const setter = used.some(other => other.write && other.name === free) ? ` set: $$value => { ${free} = $$value; },` : '';
      table.push(`${JSON.stringify(free)}: { get: () => ${free},${setter} line: ${line}, at: ${JSON.stringify(at)} }`);
    }
    header.push(`const ${TABLE} = { ${table.join(', ')} };`);
  }
  const moved: Splice[][] = [];
  const body = apply(code, splices, moved);
  // Markup's `{await ...}` compiles to thunks with no `await` left in them, so the template is what says it.
  const awaits = /\bawait\b/.test(context.source);
  const head = `(() => { ${header.join(' ')} `;
  const id = remember({ head: head.length, text: head + body, moved, code, map: context.map ?? '', input: context.input ?? '', edits: context.edits ?? [], source: context.source, shift: context.shift });
  // Evaluated where it stands (a direct eval sees the cell's names), as its own file: a frame inside it says `repl-ui-<id>.js` and
  // where, which `placeOfFrame` finds again in the cell. The string is one line, so no line of the cell moves.
  const evaluated = `${head}${body} return ${SVELTE}.component(${name}, ${awaits}); })()\n//# sourceURL=${UI_FILE}${id}.js`;
  const expression = `eval(${JSON.stringify(evaluated)})`;
  // The cell's `$` is read where the cell's names are, outside the function whose header names Svelte's runtime `$`.
  return { expression: context.dollar ? `((${DOLLAR}) => ${expression})(typeof $ === 'undefined' ? undefined : $)` : expression, imports };
}

/**
 * `onMount` and `onDestroy` used without an import get theirs: the compiler gives a component its own lifecycle context only when
 * it sees them imported, and without one their effects would hang on the parent and outlive the component. The import goes on the
 * `<script>` line, so no line moves.
 */
function withLifecycleImports(source: string, log: Splice[][]): string {
  const script = /<script(?![^>]*\bmodule\b)[^>]*>/.exec(source);
  if (!script) return source;
  const missing = ['onMount', 'onDestroy'].filter(name => new RegExp(`\\b${name}\\s*\\(`).test(source) && !new RegExp(`import\\s*\\{[^}]*\\b${name}\\b`).test(source));
  if (missing.length === 0) return source;
  const at = script.index + script[0].length;
  return apply(source, [{ start: at, end: at, text: ` import { ${missing.join(', ')} } from 'svelte';` }], log);
}

/** The elements whose children, written between their tags, are their code as written, and the prop that code becomes. */
const RAW_TEXT: Partial<Record<string, string>> = { Code: 'text', Shader: 'source' };

/** The index just past the `{...}` that opens at `at`, its strings, template literals and comments skipped; the end of `code` if it never closes. */
function skipBraces(code: string, at: number): number {
  let depth = 0;
  for (let i = at; i < code.length; i++) {
    const c = code[i];
    if (c === '{') depth++;
    else if (c === '}') { if (--depth === 0) return i + 1; }
    else if (c === '"' || c === "'") { i = skipQuoted(code, i); }
    else if (c === '`') {
      for (i++; i < code.length && code[i] !== '`'; i++) {
        if (code[i] === '\\') i++;
        else if (code[i] === '$' && code[i + 1] === '{') i = skipBraces(code, i + 1) - 1;
      }
    } else if (c === '/' && code[i + 1] === '/') { const nl = code.indexOf('\n', i); i = nl === -1 ? code.length : nl; }
    else if (c === '/' && code[i + 1] === '*') { const close = code.indexOf('*/', i + 2); i = close === -1 ? code.length : close + 1; }
  }
  return code.length;
}

/** The index of the quote that closes the string opening at `at`. */
function skipQuoted(code: string, at: number): number {
  for (let i = at + 1; i < code.length; i++) {
    if (code[i] === '\\') i++;
    else if (code[i] === code[at]) return i;
  }
  return code.length;
}

/** The index of the `>` that ends the start tag whose attributes begin at `at` (quoted values and `{...}` skipped), or -1. */
function tagEnd(code: string, at: number): number {
  for (let i = at; i < code.length; i++) {
    const c = code[i];
    if (c === '>') return i;
    if (c === '"' || c === "'") i = skipQuoted(code, i);
    else if (c === '{') i = skipBraces(code, i) - 1;
  }
  return -1;
}

/** True when `body` is one Svelte expression and nothing else (`{code}`, `{snippets.main}`): what a `<Code>{code}</Code>` passes. */
function oneExpression(body: string): boolean {
  const text = body.trim();
  if (!text.startsWith('{') || /^\{[#:/@]/.test(text) || skipBraces(text, 0) !== text.length) return false;
  try { parse(`(${text.slice(1, -1)})`, { ecmaVersion: 'latest', sourceType: 'module' }); return true; } catch { return false; }
}

/** `text` as the inside of a template literal: its backslashes, backticks and `${` escaped, nothing else changed. */
const asTemplate = (text: string) => text.replace(/\\/g, '\\\\').replace(/`/g, '\\`').replace(/\$\{/g, '\\${');

/** A code block's text without the markup's indent: its blank first and last lines dropped (how many newlines that took is `dropped`) and the indent every line shares taken off. */
function dedent(body: string): { text: string; dropped: number } {
  const lines = body.split('\n');
  let dropped = 0;
  while (lines.length > 1 && !lines[0]!.trim()) { lines.shift(); dropped++; }
  while (lines.length > 1 && !lines[lines.length - 1]!.trim()) { lines.pop(); dropped++; }
  const indent = Math.min(...lines.filter(line => line.trim()).map(line => /^[ \t]*/.exec(line)![0].length));
  return { text: lines.map(line => line.slice(Number.isFinite(indent) ? indent : 0)).join('\n'), dropped };
}

/**
 * What is written between the tags of a `<Code>` or a `<Shader>` is raw text, as a `<script>`'s is: its braces, `#define`s and
 * comments are the code's, never Svelte's. Each such body becomes the element's prop as a template literal
 * (`<Code text={\`...\`}>`), so no line moves. A Shader's is not dedented (its lines are the cell's) and gets `sourceLine`, the cell
 * line its source's first line is on, which its compile errors name (shader.ts); a blank first line is dropped, so the shader's line 1
 * is its first. A body that is one `{expression}` (`<Code>{code}</Code>`) is left to Svelte, as is an element that already has the
 * prop. `shift` is how far the template's first line is below the cell's.
 */
export function rawText(source: string, shift = 0): string {
  if (!/<(?:Code|Shader)[\s>]/.test(source)) return source;
  const splices: Splice[] = [];
  let i = 0;
  while (i < source.length) {
    const c = source[i];
    if (c === '{') { i = skipBraces(source, i); continue; }
    if (c !== '<') { i++; continue; }
    if (source.startsWith('<!--', i)) { const end = source.indexOf('-->', i + 4); i = end === -1 ? source.length : end + 3; continue; }
    const opaque = /^<(script|style)\b/i.exec(source.slice(i, i + 8));
    if (opaque) {
      const close = new RegExp(`</${opaque[1]}\\s*>`, 'ig');
      close.lastIndex = i;
      const found = close.exec(source);
      i = found ? found.index + found[0].length : source.length;
      continue;
    }
    const tag = /^<([A-Z]\w*)(?=[\s/>])/.exec(source.slice(i, i + 64));
    if (!tag) { i++; continue; }
    const name = tag[1]!;
    const end = tagEnd(source, i + 1 + name.length);
    if (end === -1) break;
    const prop = RAW_TEXT[name];
    if (!prop || source[end - 1] === '/') { i = end + 1; continue; }
    const close = new RegExp(`</${name}\\s*>`, 'g');
    close.lastIndex = end + 1;
    const found = close.exec(source);
    if (!found) { i = end + 1; continue; }
    const body = source.slice(end + 1, found.index);
    const start = i;
    i = found.index + found[0].length;
    if (!body.trim() || oneExpression(body) || new RegExp(`\\s${prop}\\s*=`).test(source.slice(start, end))) continue;
    if (name === 'Shader') {
      const blank = /^[ \t]*\n/.exec(body)?.[0] ?? '';
      const line = lineOf(source, end + 1 + blank.length) + shift;
      splices.push({ start: end, end: found.index, text: ` ${prop}={\`${asTemplate(body.slice(blank.length))}\`} sourceLine={${line}}${blank ? '\n' : ''}>` });
      continue;
    }
    const { text, dropped } = dedent(body);
    splices.push({ start: end, end: found.index, text: ` ${prop}={\`${asTemplate(text)}\`}${'\n'.repeat(dropped)}>` });
  }
  return apply(source, splices);
}

/**
 * Compiles `source`, a Svelte 5 component (runes mode, the custom renderer), to an expression. `shift` is how far the template's
 * first line is below the cell's first line, for the lines of errors and warnings.
 */
export function compileComponent(original: string, options: { filename?: string; shift?: number } = {}): Compiled {
  const shift = options.shift ?? 0;
  let result: Anything;
  let dollar = false;
  let source = original;
  // What each pass inserted, so a place in what the compiler read is found again in the cell (`back`).
  const edits: Splice[][] = [];
  let input = '';
  try {
    source = rawText(original, shift);
    const aliased = aliasDollar(mapTransitions(checkElements(source, shift, edits), source, shift, edits), edits);
    dollar = aliased.aliased;
    input = withLifecycleImports(withBoundaryDefaults(aliased.source, edits), edits);
    result = svelteCompiler().compile(input, {
      generate: 'client', runes: true, dev: false, css: 'external', filename: options.filename ?? 'Cell.svelte',
      experimental: { customRenderer: RENDERER, async: true },
    });
  } catch (error) { throw describe(error, shift, source, edits); }
  const warnings = (result.warnings as Anything[])
    .filter(warning => !IGNORED.test(warning.code))
    .map(warning => `${(warning.message as string).split('\n', 1)[0]}${position(warning, shift, source, edits)}${SVELTE_4.has(warning.code) ? ` (${SVELTE_4_HINT})` : ''}`);
  if (result.css?.code) warnings.push('<style> does nothing here: there is no stylesheet, so look comes from props (gap, pad, tone, bold)');
  return { ...toExpression(result.js.code, { source, shift, dollar, map: result.js.map?.mappings, input, edits }), warnings };
}

/** True when a cell is itself a component: its first thing is markup. */
export function isSvelteCell(code: string): boolean {
  return /^\s*(?:<!--[\s\S]*?-->\s*)*<(?:script\b|style\b|svelte:|!--|[A-Za-z][\w.:-]*(?=[\s/>]))/.test(code);
}

const joined = (imports: string[], rest: string) => (imports.length ? `${imports.join(' ')} ` : '') + rest;

/** A cell that is a component: its last expression is the component, and a cell whose value is one is drawn (host.ts). */
export function svelteCell(code: string, warn?: (text: string) => void): string {
  const { expression, imports, warnings } = compileComponent(code.replace(/\r\n/g, '\n'));
  for (const text of warnings) warn?.(text);
  return joined(imports, `(${expression});`);
}

const lineOf = (code: string, index: number) => code.slice(0, index).split('\n').length;

/** `svelte\`...\`` tagged templates in a JavaScript cell, each replaced by its compiled expression and the cell's lines kept. */
export function expandTags(js: string, warn?: (text: string) => void): string {
  if (!/\bsvelte\s*`/.test(js)) return js;
  let tree: Node;
  try { tree = parse(js, { ecmaVersion: 'latest', sourceType: 'module', allowAwaitOutsideFunction: true, allowReturnOutsideFunction: true }) as unknown as Node; }
  catch { return js; }
  const splices: Splice[] = [];
  const lifted: string[] = [];
  const visit = (node: Node): void => {
    if (node.type === 'TaggedTemplateExpression' && node.tag.type === 'Identifier' && node.tag.name === 'svelte') {
      if (node.quasi.expressions.length > 0) {
        throw syntaxError(`svelte\`...\` takes no \${...} (line ${lineOf(js, node.quasi.expressions[0].start)}): the template is compiled before the cell runs, so pass values as props, render(svelte\`<Text>{label}</Text>\`, { label })`);
      }
      const raw = node.quasi.quasis[0].value.raw as string;
      // The template is raw text: a backtick or `${` the markup itself needs is escaped, and only those escapes are undone.
      const source = raw.replace(/\\(`|\$\{)/g, '$1');
      const shift = lineOf(js, node.quasi.quasis[0].start) - 1;
      const { expression, imports, warnings } = compileComponent(source, { filename: 'App.svelte', shift });
      for (const text of warnings) warn?.(text);
      lifted.push(...imports);
      const kept = js.slice(node.start, node.end).split('\n').length - 1;
      splices.push({ start: node.start, end: node.end, text: `${expression}${kept ? `/*${'\n'.repeat(kept)}*/` : ''}` });
      return;
    }
    for (const child of children(node)) visit(child);
  };
  visit(tree);
  return joined(lifted, apply(js, splices));
}

/** A hint for the mistakes agents bring from React (JSX), said when TypeScript cannot read the cell. */
function hint(error: unknown, code: string): unknown {
  if (!/(?:^|[\s(,=:?&|!>])<[A-Z>]/m.test(code) && !/\brender\(\s*</.test(code)) return error;
  const message = `${error instanceof Error ? error.message : String(error)}\nJSX is not what cells draw with: a cell is TypeScript or a Svelte 5 component. Start the cell with the markup (<script>...</script><Column>...</Column>), or compute in TypeScript and render(svelte\`<Table {rows} />\`, { rows })`;
  return syntaxError(message);
}

const transpiler = new Bun.Transpiler({ loader: 'ts', target: 'bun', trimUnusedImports: false, deadCodeElimination: false });

/** A cell as JavaScript: a component cell compiled, or TypeScript with its types erased and its `svelte\`\`` templates compiled. `warn` hears what the compiler noticed. */
export function stripCell(code: string, warn?: (text: string) => void): string {
  if (isSvelteCell(code)) return svelteCell(code, warn);
  let js: string;
  try { js = transpiler.transformSync(code); }
  catch (error) { throw hint(error, code); }
  return expandTags(js, warn);
}
