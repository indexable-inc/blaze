import { SVELTE, isElement } from './element.ts';
import { UI_FILE, placeOfFrame } from './compile.ts';
import { cellRuntime, elementGlobals, runtime } from './svelte.ts';
import { UiTree, interval } from './tree.ts';
import type { UiNode } from './tree.ts';
import { currentTask, inTask, type Task } from '../tasks.ts';

/*
 * What a cell is handed for UI: the element components (`Column`, `Text`, ...), `render`, `svelte` (a template tag compile.ts expands
 * before the cell runs) and `interval()`. Runes (`$state`, `$derived`, `$effect`, `$props`) are the compiler's; `onMount`,
 * `onDestroy` (imported for you), `untrack` and `tick` are Svelte's own. A cell that is markup, or whose value is a component, is
 * drawn; so is `render(component, props)`, again in the same cell replacing the tree. The tree rides on the cell's post to harnessd
 * (`ui` in repl.ts `body`), sent again each time it changes; the app draws it under the cell's card (harness-native chat/genui.rs).
 * Copied from den's repl/src/ui/index.ts and host.ts.
 */

export { isElement, type UiNode };

/** Trees kept live (their effects and intervals running) at once; mounting one more unmounts the oldest. */
const MAX_LIVE = 16;
const live = new Map<Task, UiTree>();

/** Mount `component` with `props` as task `t`'s UI, replacing any it had; what the first render throws is thrown here. */
export function show(t: Task, component: unknown, props: Record<string, unknown> = {}): UiTree {
  live.get(t)?.unmount();
  live.delete(t);
  const tree = new UiTree({
    emit: (ui) => { t.ui = ui; t.touch(); },
    fail: (what, error) => {
      t.logs.push(`[ui] ${what} failed: ${(uiError(error) ?? (error instanceof Error ? `${error.name}: ${error.message}` : String(error))).replace(/\n\s+at repl-cell\.js:.*/, '')}`);
      t.touch();
    },
    enter: (fn) => inTask(t, fn),
  });
  tree.mount(component, props);
  live.set(t, tree);
  for (const [old, oldest] of live) {
    if (live.size <= MAX_LIVE) break;
    oldest.unmount();
    live.delete(old);
  }
  return tree;
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
    render(component: unknown, props?: Record<string, unknown>) {
      const t = currentTask();
      if (!t) throw new Error('render() draws under the cell that calls it; call it in a cell');
      if (typeof component !== 'function') throw new TypeError('render takes a component: render(svelte`<Text>hi</Text>`, props), or write the cell as markup');
      show(t, component, props);
      return { unmount: () => { live.get(t)?.unmount(); live.delete(t); } };
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
