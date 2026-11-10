import { DRAWING, PROPS, article, TEXT_PROP, WIRED, WORDS_ONLY, ViewNode, commonWire, elementWire, holdsError, placeError, taskOf, wholeChartError, wireValue, written } from './element.ts';
import type { ElementName } from './element.ts';
import { mapOf, mapWire } from './map.ts';
import { resolveFile, resolveMedia } from './media.ts';
import { resolveRefs } from './shader.ts';
import type { Attachment, Handler } from './element.ts';
import { drawWire } from './draw.ts';
import type { Paint } from './draw.ts';
import { placed, rooted, runtime } from './svelte.ts';

/*
 * A mounted UI: `UiTree` mounts a cell's Svelte component with the custom renderer (svelte.ts) and turns the `ViewNode`s it builds into
 * the `{type, props, children}` tree the app draws. Nothing re-runs a whole tree: a `$state` that changes updates the one prop or
 * text that read it, the node reports the change, and the tree sends the new JSON, at most once per RENDER_MS and only when it differs
 * from the last sent. A listener (`onclick`) never travels: it is a handler id (`h1`, `h2`, ...) that stays the same for as long as
 * that element keeps the listener, and `dispatch` calls the latest function behind it. Copied from den's repl/src/ui/tree.ts.
 */

/** One drawn element: what travels to the app. */
export interface UiNode { type: string; props: Record<string, unknown>; children: UiNode[] }

/**
 * The props of an element that name a file, sent as its absolute path: `drawn` ones the app says it cannot open when it draws them,
 * `there` ones must be there now (a Video's, a poster, a Shader's `.slang`), `web` ones may also be an http(s) URL (a Video's src).
 */
const FILE_PROPS: Partial<Record<string, Readonly<Record<string, 'drawn' | 'there' | 'web'>>>> = {
  Image: { src: 'drawn' }, FileRef: { path: 'drawn' }, File: { path: 'drawn' }, Video: { src: 'web', poster: 'there' }, Shader: { src: 'there' },
};

/** A file prop's value as its absolute path (or URL), by how FILE_PROPS takes it; `what` names it for the error. */
function fileValue(how: 'drawn' | 'there' | 'web', value: unknown, what: string): string {
  return how === 'drawn' ? resolveFile(value, what) : resolveMedia(value, what, how === 'web');
}

/** A Shader's textures with their files as absolute paths ('@id's and http(s) URLs as written), before JSON would make a Bun.file `{}`. */
function textureFiles(value: unknown): unknown {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return value;
  return Object.fromEntries(Object.entries(value).map(([name, item]) => [name, typeof item === 'string' && item.startsWith('@') ? item : resolveMedia(item, `<Shader textures.${name}>`, true)]));
}
/** What a File given a path cannot also say: it is then the FileRef chip of that file, which shows none of them. */
const NOT_ON_A_CHIP = ['size', 'modified', 'value'];

/** At most one render per this long. */
export const RENDER_MS = 16;

export interface TreeOptions {
  /** Each render that differs from the last, as one node or an array of them. */
  emit(ui: UiNode | UiNode[]): void;
  /** Something a handler, an effect or a later update threw: reported, never raised into the kernel. */
  fail(what: string, error: unknown): void;
  /** Runs `fn` as the cell that started the tree did (its task, for its logs). */
  enter?<T>(fn: () => T): T;
  /**
   * The tool call id of the cell `task` (`t3`) of this lane, for a CellRef to show it by; or why it cannot be shown (this lane never
   * ran it, it is the cell itself). A tree without it shows no other cell.
   */
  cell?(task: string): string | { refused: string };
}

/** A prop value as JSON: no functions, undefined or non-finite numbers; dates and bigints as text, typed arrays as arrays. */
function plain(value: unknown, depth = 0): unknown {
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return value;
  if (typeof value === 'number') return Number.isFinite(value) ? value : null;
  if (typeof value === 'bigint') return value.toString();
  if (value instanceof Date) return value.toISOString();
  if (typeof value !== 'object') return undefined;
  if (depth > 8) throw new RangeError('a prop is nested more than 8 levels deep (a cycle?)');
  if (Array.isArray(value)) return value.map(item => plain(item, depth + 1) ?? null);
  if (ArrayBuffer.isView(value) && !(value instanceof DataView)) return Array.from(value as unknown as ArrayLike<number>, item => plain(item, depth + 1));
  const object: Record<string, unknown> = {};
  for (const [name, item] of Object.entries(value)) {
    const kept = plain(item, depth + 1);
    if (kept !== undefined) object[name] = kept;
  }
  return object;
}

/** The tree the code now running (a component, an effect, a handler, an interval) belongs to; set around what the tree runs for the cell. */
let running: UiTree | undefined;
/** The tree most recently mounted, for what runs from Svelte's own scheduler (an effect that runs again after a state change), which no tree wraps. */
let latest: UiTree | undefined;

/** The tree a call to `interval()` belongs to. */
function currentTree(name: string): UiTree {
  const tree = running ?? (latest && !latest.closed ? latest : undefined);
  if (!tree) throw new Error(`${name}() belongs to a tree: call it in a component, an effect or a handler of a cell's markup`);
  return tree;
}

export class UiTree implements Attachment {
  private readonly container = ViewNode.element('#root');
  private readonly handlers = new Map<string, Handler>();
  private readonly timers = new Set<ReturnType<typeof setInterval>>();
  private instance: unknown;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private dirty = false;
  private lastPass = -Infinity;
  private sent: string | undefined;
  /** Where the cell writes each node of the last pass, for the errors of the checks over the whole tree (`resolveRefs`). */
  private places = new WeakMap<UiNode, string | undefined>();
  private shown: UiNode | UiNode[] | undefined;
  private disposed = false;
  private mounting = false;

  constructor(private readonly options: TreeOptions) {}

  get closed(): boolean { return this.disposed; }
  /** The latest tree sent. */
  get ui(): UiNode | UiNode[] | undefined { return this.shown; }

  bind(id: string, handler: Handler): void { this.handlers.set(id, handler); }
  unbind(id: string): void { this.handlers.delete(id); }
  touch(): void { this.schedule(); }

  /**
   * Mounts `component` with `props` and keeps it mounted; what a component throws on the first run is thrown here, and what a
   * later update throws is reported as a failure of the tree.
   */
  mount(component: unknown, props: Record<string, unknown> = {}): void {
    if (this.disposed) throw new Error('this tree was unmounted');
    if (typeof component !== 'function') throw new TypeError('render takes a component: render(svelte`<Text>hi</Text>`, props), or a cell that is markup');
    if (typeof props !== 'object' || props === null || Array.isArray(props)) throw new TypeError('render(component, props) takes an object of props');
    const { api, renderer } = runtime();
    this.container.tree = this;
    this.mounting = true;
    latest = this;
    let thrown: { error: unknown } | undefined;
    const onerror = (error: unknown) => {
      if (this.mounting) thrown ??= { error };
      else this.options.fail('render', error);
    };
    try {
      this.within(() => {
        const anchor = renderer.createComment('');
        renderer.insert(this.container, anchor, null);
        // Always inside a boundary: what a later update throws is reported (`fail`) and shown in place, never thrown into Svelte's
        // scheduler, where nothing would catch it.
        const mounted = rooted(component, props, onerror, () => undefined);
        this.instance = api.mount(mounted.component, { renderer, target: this.container, anchor, props: mounted.props });
      });
      if (thrown) throw thrown.error;
    } catch (error) {
      this.mounting = false;
      this.unmount();
      throw error;
    }
    this.mounting = false;
    try { this.pass(); } catch (error) { this.unmount(); throw error; }
  }

  /** Sends what changed, at most once per RENDER_MS however many changes arrive. */
  schedule(): void {
    if (this.disposed) return;
    this.dirty = true;
    if (this.timer || this.mounting) return;
    this.timer = setTimeout(() => this.flush(), Math.max(0, RENDER_MS - (Date.now() - this.lastPass)));
  }

  /** Sends now if something changed since the last send. */
  flush(): void {
    clearTimeout(this.timer);
    this.timer = undefined;
    if (!this.dirty || this.disposed) return;
    try { this.pass(); } catch (error) { this.options.fail('render', error); }
  }

  /** Calls the handler behind `id` with `value`. False when no live element has that id. */
  async dispatch(id: string, value: unknown): Promise<boolean> {
    const handler = this.handlers.get(id);
    if (!handler || this.disposed) return false;
    latest = this;
    await this.guard(`handler ${id}`, () => handler(value));
    return true;
  }

  /** Runs user code as the cell that started the tree did, followed by a flush; its throw or rejection is reported, not raised. */
  async guard(what: string, run: () => unknown): Promise<void> {
    if (this.disposed) return;
    try { await (this.options.enter ? this.options.enter(() => this.within(run)) : this.within(run)); }
    catch (error) { this.options.fail(what, error); }
  }

  /** `interval(fn, ms)`: `fn` runs every `ms` as this tree's code until the returned function is called or the tree unmounts. */
  every(fn: () => unknown, ms: number): () => void {
    const timer = setInterval(() => { void this.guard('interval', fn); }, ms);
    timer.unref?.();
    this.timers.add(timer);
    return () => { clearInterval(timer); this.timers.delete(timer); };
  }

  private within<T>(run: () => T): T {
    const before = running;
    running = this;
    try {
      const result = run();
      runtime().api.flushSync();
      return result;
    } finally { running = before; }
  }

  /** Unmounts everything (`onDestroy` and effect cleanups run) and stops sending. */
  unmount(): void {
    if (this.disposed) return;
    this.disposed = true;
    clearTimeout(this.timer);
    this.timer = undefined;
    for (const timer of this.timers) clearInterval(timer);
    this.timers.clear();
    const { instance } = this;
    this.instance = undefined;
    if (instance !== undefined) {
      const stop = () => { void runtime().api.unmount(instance); };
      try { if (this.options.enter) this.options.enter(stop); else stop(); }
      catch (error) { this.options.fail('cleanup', error); }
    }
    this.container.detach();
    this.handlers.clear();
    if (latest === this) latest = undefined;
  }

  private pass(): void {
    clearTimeout(this.timer);
    this.timer = undefined;
    this.lastPass = Date.now();
    this.dirty = false;
    this.places = new WeakMap();
    const nodes = this.children(this.container);
    // A Shader's '@id's are checked over the whole tree, so an id may come after the Shader that names it.
    const bad = resolveRefs(nodes, node => this.places.get(node as UiNode));
    if (bad) throw placed(bad.error, this.places.get(bad.node as UiNode));
    const ui = nodes.length === 1 ? nodes[0]! : nodes;
    const json = JSON.stringify(ui);
    if (json === this.sent) return;
    this.sent = json;
    this.shown = ui;
    this.options.emit(ui);
  }

  /**
   * The children of `parent` as drawn nodes: runs of text are one string, and a string among elements is a `Text` (trimmed: the
   * spaces Svelte's templates leave around it are layout noise). The anchors Svelte keeps (comments) are not drawn. `paint` is what a
   * drawing passes down to what it holds, and says the children are in one.
   */
  private children(parent: ViewNode, paint?: Paint): UiNode[] {
    const parts: Array<string | UiNode> = [];
    let run: string | undefined;
    for (const child of parent.children) {
      if (child.isText) { run = (run ?? '') + child.text; continue; }
      if (!child.isElement) continue;
      if (run !== undefined) parts.push(run);
      run = undefined;
      parts.push(this.node(child, paint));
    }
    if (run !== undefined) parts.push(run);
    const among = parts.some(part => typeof part !== 'string');
    return parts.flatMap((part): UiNode[] => {
      if (typeof part !== 'string') return [part];
      const text = among ? part.trim() : part;
      return text ? [{ type: 'Text', props: paint ? drawWire('Text', undefined, { text }, paint).props : { text }, children: [] }] : [];
    });
  }

  private node(node: ViewNode, paint?: Paint): UiNode {
    const made = this.made(node, paint);
    this.places.set(made, node.at);
    return made;
  }

  private made(node: ViewNode, paint?: Paint): UiNode {
    const props: Record<string, unknown> = {};
    for (const [name, value] of Object.entries(node.props)) {
      const file = FILE_PROPS[node.type]?.[name];
      if (node.ids.has(name)) props[name] = value;
      // A picture's, a video's or a file chip's file goes as its absolute path (media.ts), resolved before JSON would make a Bun.file `{}`.
      else if (file && value !== undefined && value !== null && value !== '') {
        try { props[name] = fileValue(file, value, `<${node.type} ${name}>`); }
        catch (error) { throw error instanceof Error ? placed(error, node.at) : error; }
      } else if (node.type === 'Shader' && name === 'textures' && value !== undefined && value !== null) {
        try { props[name] = textureFiles(value); }
        catch (error) { throw error instanceof Error ? placed(error, node.at) : error; }
      } else if (node.type === 'File' && name === 'modified' && value instanceof Date) {
        // A time as its milliseconds, which elementWire says as a File shows it (JSON would make it ISO words).
        props[name] = value.getTime();
      } else {
        const kept = plain(value);
        if (kept !== undefined) props[name] = kept;
      }
    }
    // A table's columns travel in the order the rows first name them: the app's JSON objects keep no key order.
    if (node.type === 'Table' && props.columns === undefined && Array.isArray(props.rows) && props.rows.some(row => row && typeof row === 'object' && !Array.isArray(row))) {
      const keys = new Set<string>();
      for (const row of props.rows) if (row && typeof row === 'object' && !Array.isArray(row)) for (const key of Object.keys(row)) keys.add(key);
      props.columns = [...keys];
    }
    // Text children are the prop the element reads: `<Text>hi {n}</Text>` is `{text: 'hi 1'}`, unless the prop was given.
    const into = TEXT_PROP[node.type];
    // (Svelte leaves empty text nodes it fills in later; an element with only those has no children.)
    const kids = node.children.filter(child => child.kind === 'element' || (child.kind === 'text' && child.text !== ''));
    const leaf = into !== undefined && props[into] === undefined && kids.length > 0 && kids.every(child => child.isText);
    if (leaf) props[into] = kids.map(child => child.text).join('');
    if (node.type === 'Chart') {
      const error = wholeChartError(props, node.at);
      if (error) throw placed(error, node.at);
    }
    // A drawing and what it holds go as the app reads them (draw.ts), the paint a group says passed on to its shapes; an SVG tag's
    // handlers go as the Canvas element's, and a Canvas element's common props as the rest's.
    if (node.type === 'Canvas' || DRAWING.has(node.type) || (node.type === 'Text' && paint)) {
      let drawn: ReturnType<typeof drawWire>;
      try {
        drawn = drawWire(node.type, node.from, props, paint ?? {}, node.at);
        for (const [name, id] of node.ids) drawn.props[name] = id;
        if (!node.from) commonWire(node.type, props, drawn.props, node.at);
      } catch (error) { throw error instanceof Error ? placed(error, node.at) : error; }
      return { type: node.type, props: drawn.props, children: leaf ? [] : this.children(node, drawn.paint) };
    }
    for (const [name, value] of Object.entries(props)) props[name] = wireValue(node.type, name, value);
    let type = node.type;
    // A File that names a path is that file's chip, sent as the FileRef it is.
    if (type === 'File' && props.path !== undefined) {
      const also = NOT_ON_A_CHIP.find(name => props[name] !== undefined);
      if (also) throw placed(new TypeError(`<File path=... ${also}=...> is a file chip${node.at ? ` (${node.at})` : ''}: a File given a path is the FileRef chip of that file, which shows no ${also}; say name, kind, size and modified without a path for a row of a listing`), node.at);
      type = 'FileRef';
    }
    // A CellRef names a cell of this lane by its task; the app finds it by the tool call that ran it.
    if (type === 'CellRef') {
      const where = node.at ? ` (${node.at})` : '';
      const task = taskOf(props.task);
      if (task === undefined) throw placed(new TypeError(`<CellRef${props.task === undefined ? '' : ` ${written('task', props.task)}`}> names no cell${where}: say task="t3" or task={3}, the id a cell's reply gives it`), node.at);
      const found = this.options.cell?.(task) ?? { refused: 'is not shown here: this UI shows no other cell' };
      if (typeof found !== 'string') throw placed(new TypeError(`<CellRef task="${task}"> ${found.refused}${where}`), node.at);
      props.task = task;
      props.call = found;
    }
    // A Map goes as its checked places and where it looks (map.ts `mapWire`): the app covers its card with Apple's map.
    if (type === 'Map') {
      const where = node.at ? ` (${node.at})` : '';
      const inner = kids.find(child => child.isElement);
      if (inner) throw placed(new TypeError(`<Map> holds ${inner.type}${where}: a Map holds nothing; its places are markers={[{lat, lon, label}]}`), node.at);
      const spec = mapOf(props);
      if (typeof spec === 'string') throw placed(new TypeError(`<Map> ${spec}${where}`), node.at);
      try { return { type: 'Map', props: commonWire('Map', props, mapWire(spec), node.at), children: [] }; }
      catch (error) { throw error instanceof Error ? placed(error, node.at) : error; }
    }
    // A control, and every element of tier 3, goes as exactly what the app reads of it (element.ts `elementWire`). One that holds only
    // its words (a Button's label) or nothing has no children on the wire; the rest hold what `holdsError` lets them, where `placeError` does.
    // Then the props every element takes (element.ts `commonWire`), which elementWire leaves out.
    if (WIRED.has(type)) {
      const where = node.at ? ` (${node.at})` : '';
      const leaf = WORDS_ONLY.has(type) || !PROPS[type as ElementName].includes('children');
      const inner = kids.find(child => child.isElement);
      const holds = type === 'Shader' ? 'only its Slang source' : `only its words, <${type}>${type === 'Button' ? 'Save' : 'Open'}</${type}>`;
      if (leaf && inner) throw placed(new TypeError(`<${node.type}> holds ${inner.type}${where}: ${WORDS_ONLY.has(type) ? `${article(type)} ${type} holds ${holds}` : `${article(node.type)} ${node.type} holds nothing`}`), node.at);
      try {
        const error = placeError(type, node.parent?.type, node.at);
        if (error) throw error;
        const children = leaf ? [] : this.children(node);
        const holds = holdsError(type, children.map(child => child.type), node.at);
        if (holds) throw holds;
        return { type, props: commonWire(type, props, elementWire(type, props, node.at), node.at), children };
      } catch (error) { throw error instanceof Error && !error.stack?.includes('repl-cell.js') ? placed(error, node.at) : error; }
    }
    try { commonWire(type, props, props, node.at); }
    catch (error) { throw error instanceof Error ? placed(error, node.at) : error; }
    return { type, props, children: leaf ? [] : this.children(node) };
  }
}

/**
 * `interval(fn, ms)`: calls `fn` every `ms` for as long as the tree lives; the `$state` it sets updates the UI. Returns a function
 * that stops it, so `$effect(() => interval(tick, 1000))` stops and starts with the effect.
 */
export function interval(fn: () => unknown, ms: number): () => void {
  if (typeof fn !== 'function' || !Number.isFinite(ms) || ms < 1) throw new RangeError(`interval(fn, ms) takes a function and milliseconds of at least 1, got ${String(ms)}`);
  return currentTree('interval').every(fn, ms);
}
