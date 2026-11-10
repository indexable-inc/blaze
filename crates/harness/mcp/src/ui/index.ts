import { SVELTE, isElement } from './element.ts';
import { UI_FILE, placeOfFrame } from './compile.ts';
import { askComponent, cellRuntime, elementGlobals, runtime } from './svelte.ts';
import { askProps, planAsk, type AskOptions } from './ask.ts';
import { UiTree, interval } from './tree.ts';
import type { UiNode } from './tree.ts';
import { currentTask, inTask, taskCall, taskCount, type Task } from '../tasks.ts';
import { req } from '../daemon.ts';

/*
 * What a cell is handed for UI: the element components (`Column`, `Text`, ...), `render`, `svelte` (a template tag compile.ts expands
 * before the cell runs), `interval()` and `ask()` (a question shown under the cell, answered through the same tree). Runes (`$state`, `$derived`, `$effect`, `$props`) are the compiler's; `onMount`,
 * `onDestroy` (imported for you), `untrack` and `tick` are Svelte's own. A cell that is markup, or whose value is a component, is
 * drawn; so is `render(component, props)`, again in the same cell replacing the tree. The tree rides on the cell's post to harnessd
 * (`ui` in repl.ts `body`), sent again each time it changes; the app draws it under the cell's card (harness-native chat/genui.rs).
 * Copied from den's repl/src/ui/index.ts and host.ts.
 */

export { isElement, type UiNode };

/** Cells whose UI is kept live (their effects and intervals running) at once; mounting one more unmounts the oldest's. */
const MAX_LIVE = 16;
/** The most questions (`ask`) one cell shows, answered or not. */
const MAX_ASKS = 16;

/** One question a cell shows: its tree, and what withdraws it while the cell still waits for it. */
interface Prompt { tree: UiTree; withdraw(why: string): void }
/** What a cell shows: its own UI (`render`, or a cell that is markup) and, below it, the questions it asked. */
interface Shown { main?: UiTree; asks: Prompt[] }
const live = new Map<Task, Shown>();

/** The cell's UI as the app gets it: its own tree's nodes, then each question's, as one node or a list. */
function update(t: Task, shown: Shown): void {
  const parts = [shown.main?.ui, ...shown.asks.map(ask => ask.tree.ui)].flatMap(ui => ui === undefined ? [] : Array.isArray(ui) ? ui : [ui]);
  t.ui = parts.length === 0 ? undefined : parts.length === 1 ? parts[0] : parts;
  t.touch();
}

/** The trees of task `t`: what it shows, made now when it shows nothing yet; the oldest cell's past MAX_LIVE are unmounted. */
function shownOf(t: Task): Shown {
  let shown = live.get(t);
  if (shown) return shown;
  live.set(t, shown = { asks: [] });
  for (const [old, oldest] of live) {
    if (live.size <= MAX_LIVE) break;
    oldest.main?.unmount();
    for (const ask of oldest.asks) { ask.withdraw(`${MAX_LIVE} newer cells drew UI`); ask.tree.unmount(); }
    live.delete(old);
  }
  return shown;
}

/** A tree of task `t`'s: each render goes out with the rest of what the cell shows, and what it runs runs as the cell. */
function treeOf(t: Task, shown: Shown): UiTree {
  return new UiTree({
    emit: () => update(t, shown),
    fail: (what, error) => {
      t.logs.push(`[ui] ${what} failed: ${(uiError(error) ?? (error instanceof Error ? `${error.name}: ${error.message}` : String(error))).replace(/\n\s+at repl-cell\.js:.*/, '')}`);
      t.touch();
    },
    enter: (fn) => inTask(t, fn),
    cell: (task) => cellCall(t, task),
  });
}

/** The tool call that ran `task`, for a CellRef in task `t`'s UI; or why it cannot be shown: only another cell this lane ran can be. */
function cellCall(t: Task, task: string): string | { refused: string } {
  const ran = taskCount() === 0 ? 'this lane has run no cells' : `this lane's cells are t1 to t${taskCount()}`;
  if (task === t.id) return { refused: `is the cell itself: a CellRef shows another cell of this lane (${ran})` };
  const call = taskCall(task);
  if (call === undefined) return { refused: `names a cell this lane never ran: ${ran}` };
  if (call === null) return { refused: `names ${task}, whose tool call the agent's client did not say, so the app cannot find it` };
  return call;
}

/**
 * Run handler `handler` of task `id`'s live UI with `value` (what the person did, from the app through harnessd): its own UI's or a
 * question's. The tree re-renders and posts itself as it does after any change. When it cannot run, the reason goes in that cell's
 * logs (or is returned, when no live tree is that cell's).
 */
export async function dispatch(id: string, handler: string, value: unknown): Promise<string | undefined> {
  for (const [t, shown] of live) {
    if (t.id !== id) continue;
    for (const tree of [shown.main, ...shown.asks.map(ask => ask.tree)]) {
      if (tree && await tree.dispatch(handler, value)) return undefined;
    }
    t.logs.push(`[ui] ${handler} did nothing: no element of this cell's UI has that handler now (it re-rendered without it)`);
    t.touch();
    return undefined;
  }
  return `ui event ${handler} for cell ${id}: its UI is not live (the cell was replaced, or ${MAX_LIVE} newer UIs unmounted it)`;
}

/** Mount `component` with `props` as task `t`'s UI, replacing any it had (its questions stay); what the first render throws is thrown here. */
export function show(t: Task, component: unknown, props: Record<string, unknown> = {}): UiTree {
  const shown = shownOf(t);
  shown.main?.unmount();
  const tree = treeOf(t, shown);
  shown.main = tree;
  try { tree.mount(component, props); }
  catch (error) {
    if (shown.main === tree) shown.main = undefined;
    update(t, shown);
    throw error;
  }
  return tree;
}

/**
 * Say `text` to this kernel's own agent as a message in its chat (the daemon's `prompt`, as agents.send types one): how an
 * answer given after the agent stopped waiting reaches it, and wakes it. With no session (a kernel run by hand) or no daemon
 * it is only in the cell's logs.
 */
function tellAgent(t: Task, text: string) {
  t.logs.push(`[ask] ${text}`);
  t.touch();
  const sid = process.env.HARNESS_SESSION;
  if (!sid) return;
  req({ op: 'rpc', id: `s${sid}`, method: 'prompt', args: { text, submit: true } }).catch((error) => {
    t.logs.push(`[ask] that answer could not be sent to the agent: ${error instanceof Error ? error.message : String(error)}`);
    t.touch();
  });
}

/**
 * `ask(text, {options, default, detail, allowText, block})` (ask.ts): the question goes below what the calling cell shows, drawn
 * with the cell's elements, and the person's answer comes back through `dispatch` as any control's does. Resolves with the label
 * picked or the words typed; with a default and `block: false`, at once with the default. An answer always reaches the agent:
 * as the cell's result while its tool call still waits, else (the call replied "backgrounded", or the cell went ahead with a
 * default) as a message in the agent's chat ([`tellAgent`]), which wakes it.
 */
export function ask(text: string, options?: AskOptions): Promise<string> {
  const t = currentTask();
  if (!t) return Promise.reject(new Error('ask() shows its question under the cell that calls it; call it in a cell (or a handler of its UI)'));
  let plan: ReturnType<typeof planAsk>;
  try { plan = planAsk(text, options); } catch (error) { return Promise.reject(error); }
  const shown = shownOf(t);
  if (shown.asks.length >= MAX_ASKS) return Promise.reject(new RangeError(`ask: cell ${t.id} already shows ${MAX_ASKS} questions; ask in a new cell, or ask one question with options`));
  const preset = plan.default === undefined ? undefined : plan.options[plan.default]!.label;
  const { promise, resolve, reject } = Promise.withResolvers<string>();
  let waiting = plan.block;
  const tree = treeOf(t, shown);
  const prompt: Prompt = {
    tree,
    withdraw(why) {
      if (!waiting) return;
      waiting = false;
      reject(new Error(`ask: ${JSON.stringify(text)} was withdrawn unanswered: ${why}`));
    },
  };
  const answer = (said: string) => {
    if (waiting) {
      waiting = false;
      resolve(said);
      // The tool call gave up waiting on the cell: nothing reads its result, so the answer goes to the agent's chat.
      if (t.returned) tellAgent(t, `Answer to ${JSON.stringify(text)}: ${said}`);
      return;
    }
    // It did not wait: the cell went ahead with the default; an answer that differs is the person changing it.
    if (said !== preset) tellAgent(t, `Answer to ${JSON.stringify(text)}: ${said} (you went ahead with ${JSON.stringify(preset)}; change it)`);
  };
  shown.asks.push(prompt);
  try { tree.mount(askComponent, { ...elementGlobals(), ...askProps(plan), answer }); }
  catch (error) { shown.asks.splice(shown.asks.indexOf(prompt), 1); update(t, shown); return Promise.reject(error); }
  if (!plan.block) return Promise.resolve(preset!);
  // A cell that stops with an error (its timeout) while it waits withdraws the question: nothing would hear the answer.
  void t.done.then(() => {
    if (!waiting || t.status !== 'error') return;
    prompt.withdraw('the cell stopped');
    tree.unmount();
    const at = shown.asks.indexOf(prompt);
    if (at >= 0) shown.asks.splice(at, 1);
    update(t, shown);
  });
  return promise;
}

/** What React users reach for, and the Svelte way to say it: calling one throws this instead of a ReferenceError. */
const REACT_HOOKS: Record<string, string> = {
  useState: 'let v = $state(initial) in a <script>, read v and assign v = next; the markup updates',
  useEffect: '$effect(() => ...) re-runs when the $state it reads changes (return a cleanup function); onMount(fn) runs once; there is no dependency array',
  useMemo: 'const m = $derived(expression), read m',
  useInterval: 'interval(fn, ms), which stops when the tree unmounts',
};

/** `svelte\`...\`` is expanded when the cell is compiled (compile.ts); a call that reaches the runtime was written in a way that cannot be. */
function svelte(): never {
  throw new TypeError('svelte`...` is expanded when the cell is compiled, so write it as a tagged template with no ${} and as an expression of the cell: render(svelte`<Text>{label}</Text>`, { label })');
}

/** Install the UI globals on a REPL's scope (a getter for the runtime, so Svelte loads when a component first runs). */
export function installUi(scope: Record<string, unknown>): void {
  Object.assign(scope, elementGlobals());
  for (const [name, instead] of Object.entries(REACT_HOOKS)) {
    scope[name] = () => { throw new TypeError(`${name} is React; this REPL's UI is Svelte 5: ${instead}`); };
  }
  for (const name of ['untrack', 'tick']) {
    scope[name] = (...args: unknown[]) => (runtime().api[name] as (...a: unknown[]) => unknown)(...args);
  }
  Object.assign(scope, {
    interval,
    svelte,
    ask,
    render(component: unknown, props?: Record<string, unknown>) {
      const t = currentTask();
      if (!t) throw new Error('render() draws under the cell that calls it; call it in a cell');
      if (typeof component !== 'function') throw new TypeError('render takes a component: render(svelte`<Text>hi</Text>`, props), or write the cell as markup');
      const tree = show(t, component, props);
      return {
        unmount: () => {
          const shown = live.get(t);
          if (!shown || shown.main !== tree) return;
          tree.unmount();
          shown.main = undefined;
          update(t, shown);
        },
      };
    },
  });
  Object.defineProperty(scope, SVELTE, { get: () => cellRuntime(), enumerable: false });
}

/** How many elements a sent tree has (what the agent is told it drew). */
export function count(ui: unknown): number {
  if (Array.isArray(ui)) return ui.reduce((n: number, node) => n + count(node), 0);
  const node = ui as UiNode | undefined;
  return node && typeof node === 'object' ? 1 + count(node.children ?? []) : 0;
}

/** Hints for what a cell's markup most often throws at run time, by its words. */
const RUNTIME_HINTS: Array<[RegExp, string]> = [
  [/is not an object|Cannot read propert|null is not|undefined is not/, 'a value the markup reads is null or undefined: guard it with {#if value} or read value?.field'],
  [/is not a function/, 'only a function can be called; a prop or a cell variable may be something else'],
];

const FRAME = new RegExp(`${UI_FILE}(\\d+)\\.js:(\\d+):(\\d+)`);
const THROUGH_UI = new RegExp(`${UI_FILE}\\d+\\.js|/node_modules/svelte/|/src/ui/`);

/**
 * What the cell is told for `error` when it came through its UI (its compiled markup, Svelte, this kernel's ui/): one message, the
 * cell line and column the frame inside its markup or script maps to (compile.ts `placeOfFrame`) with that line's text, a hint, and no
 * frames of Svelte or the kernel. Undefined for an error that did not (the REPL reports it as ever), and for one already said this
 * way (a check's, whose stack is its own words).
 */
export function uiError(error: unknown): string | undefined {
  if (!(error instanceof Error) || !error.stack || !THROUGH_UI.test(error.stack)) return undefined;
  let place: ReturnType<typeof placeOfFrame>;
  for (const line of error.stack.split('\n')) {
    const found = FRAME.exec(line);
    if (found && (place = placeOfFrame(Number(found[1]), Number(found[2]), Number(found[3])))) break;
  }
  const hint = RUNTIME_HINTS.find(([words]) => words.test(error.message))?.[1];
  // The engine's words quote the compiled code: a name read through the props (`$$free.rows`) or a state (`$.get(o)`) is the cell's own.
  const message = error.message.replace(/\$\$free\./g, '').replace(/\$\.get\(([\w$]+)\)/g, '$1');
  return `${error.name}: ${message}${place ? ` (${place.at})` : ''}${hint ? `: ${hint}` : ''}${place ? `\n    at repl-cell.js:${place.line}:${place.column}` : ''}`;
}
