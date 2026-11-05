/*
 * The model of a cell's UI: a tree of `ViewNode`s, the plain `{type, props, children}` shape the native app draws once it is
 * sent as JSON (harness-native/src/app/chat/genui.rs). Svelte (svelte.ts) builds and updates the tree through its custom
 * renderer; this file is only the data: the element names, the checks on their props, a node that knows its handlers, and the
 * brand of a component `render` can mount. Nothing here depends on Svelte. Copied from den's repl/src/ui/element.ts and cut to
 * the elements the harness app draws.
 */

/** The element names the app draws (genui.rs `ELEMENTS`, one `hn.Gen<Name>` prefab each in scene/widgets/genui.flecs). */
export const ELEMENT_NAMES = [
  'Column', 'Row', 'Text', 'Markdown', 'Code', 'Table', 'Badge', 'Stat', 'Card', 'Divider', 'Spacer',
] as const;
export type ElementName = typeof ELEMENT_NAMES[number];

const known = new Set<string>(ELEMENT_NAMES);
export const isKnownElement = (name: string): boolean => known.has(name);

export type Tone = 'ink' | 'muted' | 'strong' | 'error' | 'accent' | 'success' | 'warning';
/** Every tone, as a record so the type and this list cannot drift apart. */
const TONE_SET: Record<Tone, true> = { ink: true, muted: true, strong: true, error: true, accent: true, success: true, warning: true };
export const TONES = Object.keys(TONE_SET) as Tone[];
/** The elements that take a `tone`. */
export const TONED: ReadonlySet<string> = new Set(['Text', 'Badge', 'Stat']);

/**
 * The error for a `tone` on `element` that is not one, or undefined when it is one or none (`undefined`, `null`, `''`); `where` is the
 * cell's line, when it is known.
 */
export function toneError(element: string, value: unknown, where?: string): TypeError | undefined {
  if (value === undefined || value === null || value === '' || (typeof value === 'string' && Object.hasOwn(TONE_SET, value))) return undefined;
  const said = typeof value === 'string' ? JSON.stringify(value) : `{${String(value)}}`;
  return new TypeError(`tone=${said} on <${element}> is not a tone${where ? ` (${where})` : ''}: colour comes only from tone, one of ${TONES.join(', ')}`);
}

const said = (v: unknown): string => typeof v === 'string' || (typeof v === 'object' && v !== null) ? JSON.stringify(v) : String(v);

/**
 * The error for a Table's `columns` and `rows` when they are not what it reads, naming what is wrong; undefined when they are fine.
 * `rows` is a list of records (`[{name, cpu}]`) or of lists (`[['a', 1]]`); `columns`, when given, is a list of titles (or
 * `{title, key}`: the record field it shows, its title when not said).
 */
export function tableError(columns: unknown, rows: unknown, where?: string): TypeError | undefined {
  const at = where ? ` (${where})` : '';
  if (rows !== undefined && rows !== null && !Array.isArray(rows)) return new TypeError(`rows=${said(rows)} on <Table> is not a list${at}: rows={[{name: 'a', cpu: 3}]}`);
  if (columns === undefined || columns === null) return undefined;
  if (!Array.isArray(columns)) return new TypeError(`columns=${said(columns)} on <Table> is not a list${at}: columns={['name', 'cpu']}`);
  for (const [i, col] of columns.entries()) {
    if (typeof col === 'string') continue;
    if (typeof col !== 'object' || col === null || typeof (col as { title?: unknown }).title !== 'string') {
      return new TypeError(`<Table> column ${i} is ${said(col)}${at}: a column is a title or {title, key}`);
    }
  }
  return undefined;
}

/** The error for `bind:<name>` on one of the app's elements: none takes a binding yet (there are no inputs). */
export function bindError(element: string, name: string, where?: string): TypeError | undefined {
  if (name === 'this') return undefined;
  return new TypeError(`bind:${name} on <${element}> binds nothing${where ? ` (${where})` : ''}: the harness app's elements show values and take no input yet`);
}

/** The props whose value children text becomes: `<Text>hi</Text>` is `{text: 'hi'}`. */
export const TEXT_PROP: Partial<Record<string, string>> = { Text: 'text', Badge: 'text', Code: 'text', Markdown: 'text' };

/** What each prop must be, on every element; a prop not here is not checked. */
const size = (v: unknown) => typeof v === 'number' && Number.isFinite(v) && v >= 0 ? undefined : 'a number of points, 0 or more';
const COMMON_RULES: Record<string, (v: unknown) => string | undefined> = { gap: size, pad: size, size };

/** The error for prop `name` set to `value` on `element` when it is not what the element reads; undefined when it is fine or not checked. */
export function propError(element: string, name: string, value: unknown, where?: string): TypeError | undefined {
  if (value === undefined || value === null) return undefined;
  const wants = COMMON_RULES[name]?.(value);
  if (!wants) return undefined;
  return new TypeError(`${name}=${typeof value === 'string' ? JSON.stringify(value) : `{${said(value)}}`} on <${element}> is not ${wants}${where ? ` (${where})` : ''}`);
}

export type Handler = (value: unknown) => unknown;

/** What a node reports to the tree it hangs in. */
export interface Attachment {
  /** Something a viewer could see changed. */
  touch(): void;
  /** `id` now answers with `handler`. */
  bind(id: string, handler: Handler, name: string): void;
  /** `id` answers nothing. */
  unbind(id: string): void;
}

/** The last handler id handed out, across every tree: a tree that replaces another never answers the old one's ids. */
let handlerSeq = 0;

/** The prop the app knows an event by: `onclick={...}` (Svelte's `click` event) is `onClick` on the wire. */
export const wireName = (type: string): string => `on${type.slice(0, 1).toUpperCase()}${type.slice(1)}`;

export type Kind = 'element' | 'text' | 'comment' | 'fragment';

/**
 * One element, a piece of text, or one of the two things Svelte needs that are not drawn: a comment (an anchor it inserts
 * before) and a fragment (a bag of nodes that dissolves into the parent it is inserted in). A function prop is an event
 * listener: it is kept in `fns` and shows in `props` as its handler id (`h1`, `h2`, ...), which stays the same for as long as the
 * node keeps a listener for that event. While a node hangs in a tree its ids answer there, and stop when it leaves.
 */
export class ViewNode {
  parent: ViewNode | undefined;
  readonly children: ViewNode[] = [];
  readonly props: Record<string, unknown> = {};
  /** The handler id of each listener, by the prop it travels as (`onClick`). */
  readonly ids = new Map<string, string>();
  private readonly fns = new Map<string, Handler>();
  tree: Attachment | undefined;

  private constructor(readonly kind: Kind, readonly type: string, public text: string) {}
  static element(type: string): ViewNode { return new ViewNode('element', type, ''); }
  static text(value: string): ViewNode { return new ViewNode('text', '#text', value); }
  static comment(value: string): ViewNode { return new ViewNode('comment', '#comment', value); }
  static fragment(): ViewNode { return new ViewNode('fragment', '#fragment', ''); }

  get isText(): boolean { return this.kind === 'text'; }
  get isElement(): boolean { return this.kind === 'element'; }

  /** Sets a prop; `undefined` removes it. */
  setProp(name: string, value: unknown): void {
    if (value === undefined) delete this.props[name];
    else this.props[name] = value;
    this.tree?.touch();
  }

  /** `type` (`click`) now has `handler`: the prop `onClick` is its handler id, the same one for as long as the node listens. */
  listen(type: string, handler: Handler): void {
    const name = wireName(type);
    let id = this.ids.get(name);
    if (!id) this.ids.set(name, id = `h${++handlerSeq}`);
    this.fns.set(name, handler);
    this.props[name] = id;
    this.tree?.bind(id, handler, name);
    this.tree?.touch();
  }

  /** Stops listening for `type`, when `handler` is still the one that listens. */
  unlisten(type: string, handler: Handler): void {
    const name = wireName(type);
    const id = this.ids.get(name);
    if (!id || this.fns.get(name) !== handler) return;
    this.ids.delete(name);
    this.fns.delete(name);
    delete this.props[name];
    this.tree?.unbind(id);
    this.tree?.touch();
  }

  /** Hangs this node and everything under it in `tree`: its ids answer there from now on. */
  attach(tree: Attachment): void {
    this.tree = tree;
    for (const [name, id] of this.ids) tree.bind(id, this.fns.get(name)!, name);
    for (const child of this.children) child.attach(tree);
  }

  /** Takes this node and everything under it out of its tree: its ids answer nothing. */
  detach(): void {
    const { tree } = this;
    if (!tree) return;
    this.tree = undefined;
    for (const id of this.ids.values()) tree.unbind(id);
    for (const child of this.children) child.detach();
  }
}

/** The preloaded global compiled components use: the Svelte runtime (the client build and the renderer, see svelte.ts). */
export const SVELTE = '__repl_svelte';

/** A function Svelte compiled a cell's markup to: `render` mounts it. */
export type CellComponent = ((anchor: unknown, props: Record<string, unknown>) => unknown) & { readonly [COMPONENT]: true };
/** The brand of a component a cell's markup was compiled to (and of the element components), so a returned one can be told from any function. */
export const COMPONENT: unique symbol = Symbol.for('repl.ui.component');

/** True for what a cell can return to have it shown: a component its own markup compiled to. */
export function isElement(value: unknown): value is CellComponent {
  return typeof value === 'function' && Reflect.get(value, COMPONENT) === true;
}
