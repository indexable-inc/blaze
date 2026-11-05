import { DRAWING, TEXT_PROP, ViewNode, wholeChartError, wireValue } from './element.ts';
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

/** At most one render per this long. */
export const RENDER_MS = 16;

export interface TreeOptions {
  /** Each render that differs from the last, as one node or an array of them. */
  emit(ui: UiNode | UiNode[]): void;
  /** Something a handler, an effect or a later update threw: reported, never raised into the kernel. */
  fail(what: string, error: unknown): void;
  /** Runs `fn` as the cell that started the tree did (its task, for its logs). */
  enter?<T>(fn: () => T): T;
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
    const nodes = this.children(this.container);
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
    const props: Record<string, unknown> = {};
    for (const [name, value] of Object.entries(node.props)) {
      if (node.ids.has(name)) props[name] = value;
      else {
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
    // A drawing and what it holds go as the app reads them (draw.ts), the paint a group says passed on to its shapes.
    if (node.type === 'Canvas' || DRAWING.has(node.type) || (node.type === 'Text' && paint)) {
      let drawn: ReturnType<typeof drawWire>;
      try { drawn = drawWire(node.type, node.from, props, paint ?? {}, node.at); }
      catch (error) { throw error instanceof Error ? placed(error, node.at) : error; }
      return { type: node.type, props: drawn.props, children: leaf ? [] : this.children(node, drawn.paint) };
    }
    for (const [name, value] of Object.entries(props)) props[name] = wireValue(node.type, name, value);
    return { type: node.type, props, children: leaf ? [] : this.children(node) };
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
