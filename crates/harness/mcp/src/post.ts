// Each cell is posted to harnessd as it runs (`repl_task`): indexed as a `repl` object, and, when the agent said which
// tool call ran it (`call`), drawn by the app as that call's card. A post is the cell's whole state: its code, status,
// logs, steps (effects.ts) and, once done, its value as the app shows it (shown.ts). Its first post goes at once, later
// ones while it runs are coalesced to one per POST_MS; all go out in order, and its end goes at once. A cell that draws UI
// (Svelte 5 markup, ui/) carries its tree as `ui`; each change to it after the end is posted again, also one per POST_MS.
// Best effort: the REPL works without the daemon.
import { req, daemonPresent, listen } from "./daemon.ts";
import { clip } from "./disclose.ts";
import { rawsOf } from "./effects.ts";
import { REPL_ID, session } from "./self.ts";
import { brief, shown } from "./shown.ts";
import type { Change, Task } from "./tasks.ts";
import { dispatch } from "./ui/index.ts";

const POST_MS = 100;
const MAX = 64 << 10;
const posting = new WeakMap<Task, { chain: Promise<unknown>; timer?: Timer }>();

/** The cell's state as harnessd gets it. The app knows three states: a stopped or lost cell is an error there, its message saying which. */
export function body(t: Task) {
  const finished = t.status !== "running";
  const failed = finished && t.status !== "done";
  // A value that is one of its steps' results (a cell ending in `await googleSearch(...)`) is shown once, under the step.
  const value = !finished || t.value === undefined || t.ui !== undefined || rawsOf(t).includes(t.value) ? null
    // An error: one plain sentence for the person (`message`), its stack as the model got it.
    : failed ? { t: "error" as const, message: brief(t.value), stack: t.result }
    : shown(t.value);
  return {
    op: "repl_task" as const, repl: REPL_ID, id: t.id, call: t.call, session: session(),
    cwd: process.cwd(), code: clip(t.code, MAX), status: failed ? "error" : t.status, started_ms: t.started.getTime(), ended_ms: t.ended?.getTime() ?? null,
    logs: clip(t.logs.join("\n"), MAX), result: clip(t.result, MAX), effects: t.effects, effects_dropped: t.effectsDropped, value, ui: t.ui ?? null,
  };
}

/** This kernel's UI feed (daemon.ts `listen`), opened when a cell first draws UI: what the person does to it comes back here. */
let feed: { stop(): void } | undefined;
function listenOnce() {
  if (feed) return;
  feed = listen(REPL_ID, (ev) => {
    const id = ev.cell.slice(ev.cell.lastIndexOf(".") + 1);
    if (ev.cell !== `${REPL_ID}.${id}`) return console.error(`[harness] ui event for cell ${ev.cell}: not this kernel's (${REPL_ID})`);
    dispatch(id, ev.handler, ev.value).then((why) => { if (why) console.error(`[harness] ${why}`); });
  });
}

/** Post a cell's state after a change (tasks.ts `watchTasks`): at once when it starts or ends, else within POST_MS. */
export function post(t: Task, what: Change) {
  if (t.restored || !daemonPresent()) return;
  if (t.ui !== undefined) listenOnce();
  const first = !posting.has(t);
  const p = posting.get(t) ?? posting.set(t, { chain: Promise.resolve() }).get(t)!;
  const send = () => {
    p.timer = undefined;
    // In order: each post waits for the one before it, and reads the task when it goes.
    p.chain = p.chain.then(() => req(body(t))).catch(() => {});
  };
  if (first || what === "start" || what === "end") {
    clearTimeout(p.timer);
    send();
  } else if (!p.timer) {
    p.timer = setTimeout(send, POST_MS);
  }
}
