import { createRequire } from 'node:module';
import { AT, ELEMENT_NAMES, COMPONENT, ViewNode, cellError, isKnownElement } from './element.ts';
import type { CellComponent, Handler } from './element.ts';
import { SVG_TAGS, attributeError, isSvgTag, unknownTagError } from './draw.ts';
import { FAILED_SNIPPET, PENDING_SNIPPET, compileComponent } from './compile.ts';
import { sveltePath } from './files.ts';

/*
 * Svelte 5 for a cell's UI: runes (`$state`, `$derived`, `$effect`, `$props`) with fine-grained updates, rendering into `ViewNode`s
 * (element.ts) instead of a page. Svelte's custom renderer is how: a component compiled with `experimental.customRenderer` calls
 * the object `renderer` below (createElement, insert, setAttribute, addEventListener, ...) wherever the browser's would call the DOM.
 * This file loads that Svelte, gives the renderer nodes to work on, and makes every element name a preloaded component. Copied
 * from den's repl/src/ui/svelte.ts (den's docs/ui-runtime.md says why Svelte and which Svelte: the custom renderer is a pull
 * request that has not landed), without den's transitions, bindings and own elements.
 *
 * Which svelte: the package's own `exports` map hands a server build to anything that does not set the `custom-renderer` (or
 * `browser`) condition, and a server build has no reactivity, so the UI would draw once and never move. The client build is
 * loaded by its file path instead. Everything a compiled cell reaches for (the client runtime `$`, the renderer) is handed to it
 * as the preloaded `__repl_svelte` (compile.ts), so a cell imports nothing and there is exactly one copy. Loading is lazy: a
 * kernel that never draws pays for none of it.
 */

type Anything = Record<string, any>;

export interface SvelteRuntime {
  /** `svelte/internal/client`: what compiled components call (`$.state`, `$.get`, `$.set`, ...). */
  client: Anything;
  /** The public client API of `svelte`: `mount`, `unmount`, `flushSync`, `tick`, `onMount`, `untrack`, ... */
  api: Anything;
  renderer: Anything;
}

const load = createRequire(import.meta.url);

let loaded: SvelteRuntime | undefined;
/** Svelte's client runtime and the renderer, loaded on first use. */
export function runtime(): SvelteRuntime {
  if (loaded) return loaded;
  // Turns the custom renderer feature on for every component in this process (the flag `svelte/renderer` sets on import).
  load(sveltePath('internal/flags/custom-renderer.js'));
  const client = load(sveltePath('internal/client/index.js')) as Anything;
  const api = load(sveltePath('index-client.js')) as Anything;
  return loaded = { client, api, renderer };
}

function insertNode(parent: ViewNode, node: ViewNode, anchor: ViewNode | null): void {
  // A fragment is a bag: its children go in, in order, and it is empty after.
  if (node.kind === 'fragment') {
    for (const child of [...node.children]) insertNode(parent, child, anchor);
    return;
  }
  const before = node.parent;
  if (before) {
    const at = before.children.indexOf(node);
    if (at >= 0) before.children.splice(at, 1);
  }
  const at = anchor ? parent.children.indexOf(anchor) : -1;
  if (anchor && at < 0) throw new Error('the node to insert before is not a child of the parent (svelte renderer)');
  if (at < 0) parent.children.push(node);
  else parent.children.splice(at, 0, node);
  node.parent = parent;
  if (node.tree && node.tree !== parent.tree) node.detach();
  if (parent.tree && !node.tree) node.attach(parent.tree);
  parent.tree?.touch();
  if (before !== parent) before?.tree?.touch();
}

/** Takes `node` out of its parent. */
function removeNode(node: ViewNode): void {
  const parent = node.parent;
  if (!parent) return;
  const at = parent.children.indexOf(node);
  if (at >= 0) parent.children.splice(at, 1);
  node.parent = undefined;
  node.detach();
  parent.tree?.touch();
}

const sibling = (node: ViewNode): ViewNode | null => {
  const siblings = node.parent?.children;
  return siblings?.[siblings.indexOf(node) + 1] ?? null;
};

/**
 * The operations Svelte's compiled components call, over `ViewNode`s (`createRenderer` of `svelte/renderer` returns what it is given,
 * so this is the object itself). A prop is an attribute and an event listener is a handler id (element.ts).
 */
const renderer = {
  createFragment: () => ViewNode.fragment(),
  createElement(name: string): ViewNode {
    // An SVG tag is the Canvas element it is sent as, keeping its tag so its attributes are read as SVG's (draw.ts).
    if (isSvgTag(name)) {
      const node = ViewNode.element(SVG_TAGS[name]!);
      node.from = name;
      return node;
    }
    if (!isKnownElement(name)) throw cellError(unknownTagError(name));
    return ViewNode.element(name);
  },
  createTextNode: (data: string) => ViewNode.text(String(data)),
  createComment: (data: string) => ViewNode.comment(String(data)),
  nodeType: (node: ViewNode) => node.kind,
  getNodeValue: (node: ViewNode) => node.kind === 'text' || node.kind === 'comment' ? node.text : null,
  getAttribute: (node: ViewNode, name: string) => node.props[name] ?? null,
  setAttribute(node: ViewNode, name: string, value: unknown) {
    // Where the cell writes the element (compile.ts gives each its place first), kept off the wire for the errors below.
    if (name === AT) { node.at = typeof value === 'string' ? value : undefined; return; }
    // What compile.ts could not check at its line (a spread, a value made of text and expressions) is checked here, at the element's.
    const error = attributeError(node.from ?? node.type, name, value, node.at, node.props);
    if (error) throw placed(error, node.at);
    node.setProp(name, value);
  },
  removeAttribute(node: ViewNode, name: string) { node.setProp(name, undefined); },
  hasAttribute: (node: ViewNode, name: string) => name in node.props,
  setText(node: ViewNode, text: string) {
    if (node.kind === 'text' || node.kind === 'comment') { node.text = text; node.tree?.touch(); return; }
    for (const child of [...node.children]) removeNode(child);
    if (text) insertNode(node, ViewNode.text(text), null);
  },
  getFirstChild: (node: ViewNode) => node.children[0] ?? null,
  getLastChild: (node: ViewNode) => node.children.at(-1) ?? null,
  getNextSibling: sibling,
  insert: insertNode,
  remove: removeNode,
  getParent: (node: ViewNode) => node.parent ?? null,
  addEventListener(node: ViewNode, type: string, handler: unknown) {
    // An event the element has not (`onclick` on a Text, any on a shape) would never be called: refused, as a prop it has not is.
    const error = attributeError(node.from ?? node.type, `on${type}`, handler, node.at);
    if (error) throw placed(error, node.at);
    node.listen(type, handler as Handler);
  },
  removeEventListener(node: ViewNode, type: string, handler: unknown) { node.unlisten(type, handler as Handler); },
};

/** `error` as the cell's, at the line and column `at` (`line 5:3: ...`) names. */
export function placed<E extends Error>(error: E, at: string | undefined): E {
  const found = at ? /^line (\d+)(?::(\d+))?/.exec(at) : null;
  return cellError(error, found ? Number(found[1]) : undefined, found?.[2] ? Number(found[2]) : 1);
}

const components = new Map<string, CellComponent>();

/** Svelte source for one element as a component: its props are the element's, reactive ones updating just that attribute. */
export function hostSource(name: string): string {
  return `<script>let { children, ...rest } = $props();</script><svelte:element this={${JSON.stringify(name)}} {...rest}>{@render children?.()}</svelte:element>`;
}

/** The component of a source Svelte compiled, as the preloaded runtime would evaluate it. */
export function evaluate(expression: string): CellComponent {
  const make = new Function(SVELTE_PARAM, `return ${expression}`) as (rt: unknown) => CellComponent;
  return make(cellRuntime());
}
const SVELTE_PARAM = '__repl_svelte';

/** Compiled the first time it is used, from markup that names only the app's elements and the `__repl_svelte` it is given. */
function lazy(name: string, source: string): CellComponent {
  let inner: CellComponent | undefined;
  const made = ((anchor: unknown, props: Record<string, unknown>) => {
    inner ??= evaluate(compileComponent(source, { filename: `${name}.svelte` }).expression);
    return inner(anchor, props);
  }) as CellComponent;
  // Named for the element, so a stack and `console.log(Column)` say what it is.
  Object.defineProperty(made, 'name', { value: name });
  return Object.defineProperty(made, COMPONENT, { value: true });
}

/** The component that draws the element `name`. */
export function hostComponent(name: string): CellComponent {
  let component = components.get(name);
  if (!component) components.set(name, component = lazy(name, hostSource(name)));
  return component;
}

/** The element components (`Column`, `Row`, ...), as a cell is handed them. */
export function elementGlobals(): Record<string, unknown> {
  return Object.fromEntries(ELEMENT_NAMES.map(name => [name, hostComponent(name)]));
}

/** How the cell reaches a name a template uses without declaring it (compile.ts writes one of these per name). */
interface CellName {
  get(): unknown;
  /** Present when the template assigns the name. */
  set?(value: unknown): void;
  /** The cell line of the template's first use of it, and that line's text, for the error. */
  line: number;
  at: string;
}

/** The names a template was given, as a reader of the error would list them. */
function listed(props: Anything): string {
  let names: string[];
  try { names = Object.keys(props).filter(name => !name.startsWith('$$')); }
  catch { names = []; }
  if (names.length === 0) return 'no props given';
  return `props given: ${names.slice(0, 12).join(', ')}${names.length > 12 ? ', ...' : ''}`;
}

/** The error for a template name that is nothing: the name, where the template says it, and both places it could have come from. */
function notDefined(name: string, entry: CellName, props: Anything): ReferenceError {
  const element = /^[A-Z]/.test(name) ? `; the elements are ${ELEMENT_NAMES.join(', ')}` : '';
  const error = new ReferenceError(`${name} is not defined (${entry.at}): it is neither a prop passed to render() nor a variable of the cell (${listed(props)})${element}`);
  error.stack = `ReferenceError: ${error.message}\n    at repl-cell.js:${entry.line}:1`;
  return error;
}

/**
 * What a template's free names (the ones its markup and script use and do not declare) mean, for one mount: a prop of the object
 * `render(component, props)` passed, else a variable of the cell, else the error above. Read each time the markup runs, so a prop
 * that changes is seen; an assignment goes where the read came from.
 */
function free(given: Anything | undefined, table: Record<string, CellName>): Anything {
  const props = given ?? {};
  // `in` finds what every object has (`toString`, `constructor`); a template naming one means the global, not the object.
  const has = (name: string) => name in props && (Object.hasOwn(props, name) || !(name in Object.prototype));
  const cell = <T>(name: string, use: (entry: CellName) => T): T => {
    const entry = table[name]!;
    try { return use(entry); }
    catch (error) {
      if (error instanceof ReferenceError && error.message.includes(name) && !/initiali[sz]ation|uninitiali[sz]ed/i.test(error.message)) throw notDefined(name, entry, props);
      throw error;
    }
  };
  return new Proxy({}, {
    get: (_target, name) => typeof name === 'string' && !has(name) ? cell(name, entry => entry.get()) : Reflect.get(props, name),
    set(_target, name, value) {
      if (typeof name !== 'string') return false;
      if (has(name)) props[name] = value;
      else cell(name, entry => { entry.set!(value); });
      return true;
    },
  });
}

/**
 * A prop's `{...}` value on an element of the app's (or an SVG tag) at cell line `line` (compile.ts `checkElements`): the value when it
 * is one the element reads, else the error at that line and column. `known` holds the element's other props whose values the markup
 * writes as text (a Chart's `kind`), for the checks that read two together.
 */
function prop(value: unknown, element: string, name: string, at: string, known?: Record<string, unknown>): unknown {
  const error = attributeError(element, name, value, at, known);
  if (!error) return value;
  throw placed(error, at);
}

/** Marks a component that awaits (compile.ts): mounted on its own, it is drawn inside `root()`, which shows a pending line while it loads. */
export const AWAITS = Symbol.for('repl.ui.awaits');

export const OWN_SOURCES = {
  /** Calls `done` once it is shown: inside a boundary, that is when everything the boundary waited for has come. */
  Settle: `<script>let { done } = $props(); $effect(() => { done(); });</script>`,
  /** Where a component that awaits is mounted: a boundary around it, so what it waits for shows a pending line and a failure its error. */
  Root: `<script>let { body: Body, given, onerror, onloaded, settle: Settle } = $props();</script>
<svelte:boundary {onerror}><Body {...given} /><Settle done={onloaded} />${PENDING_SNIPPET}${FAILED_SNIPPET}</svelte:boundary>`,
};

const settle = lazy('Settle', OWN_SOURCES.Settle);
const root = lazy('Root', OWN_SOURCES.Root);

/** The component and props that mount `body` with `given` inside a boundary (see `root`). */
export function rooted(body: unknown, given: Record<string, unknown>, onerror: (error: unknown) => void, onloaded: () => void): { component: CellComponent; props: Record<string, unknown> } {
  return { component: root, props: { body, given, onerror, onloaded, settle } };
}

/** The words a failed boundary shows for `error`: its message's first line. */
const said = (error: unknown): string => String((error as Error | undefined)?.message ?? error).split('\n', 1)[0]!;

/** What compiled components are given as `__repl_svelte` (compile.ts): Svelte's client runtime, the renderer, the brand of a component, the names it reads, its prop check and a failed boundary's words. */
export function cellRuntime(): { client: Anything; renderer: Anything; component: (fn: unknown, awaits?: boolean) => CellComponent; free: typeof free; prop: typeof prop; said: typeof said } {
  const { client } = runtime();
  return {
    client, renderer, free, prop, said,
    component: (fn: unknown, awaits = false) => Object.defineProperties(fn as CellComponent, {
      [COMPONENT]: { value: true }, [AWAITS]: { value: awaits },
    }),
  };
}
