// What a cell can do about cells: wait for one, stop one, read more of one's value, run one again.
import { abortable, cancel, signal } from "./cancel.ts";
import { select, type Part } from "./disclose.ts";
import type { Docs } from "./docs.ts";
import { effect } from "./effects.ts";
import { brief } from "./shown.ts";
import { getTask, taskValue, type Task } from "./tasks.ts";

/** Wait for a task; on timeout its snapshot still says running. A step of the cell that waits: the step shows the awaited
 *  cell's own value (or its error), so the app draws that cell's output; the cell gets the record. */
export const wait = (id: string | number, timeoutMs?: number) =>
  effect("wait", "Waiting for an earlier cell", "Waited for an earlier cell", String(id), async () => {
    const t = getTask(id);
    let timer: Timer | undefined;
    await abortable(timeoutMs === undefined ? t.done : Promise.race([t.done, new Promise((r) => (timer = setTimeout(r, timeoutMs)))])).finally(() => clearTimeout(timer));
    return t.view();
  }, (v) => {
    const t = getTask(id);
    return {
      ok: v.status === "done" || v.status === "running",
      detail: v.status === "running" ? "still running" : v.status === "done" ? "" : t.restored ? v.status : brief(t.value),
      show: v.status === "running" || t.restored ? undefined : t.value,
    };
  });

/** A part of an earlier cell's value: what its reply summarized. */
export const more = (id: string | number, part: Part = {}): unknown => select(taskValue(id), part);

let runner: ((code: string) => Task) | undefined;
/** The kernel says how a cell is started (kernel.ts). */
export function setRunner(fn: (code: string) => Task) { runner = fn; }

/** Run an earlier cell's code again, as a new cell: how a name is made again after the kernel restarted. Its value, or its error thrown. */
export async function rerun(id: string | number): Promise<unknown> {
  const old = getTask(id);
  if (!runner) throw new Error("rerun: no kernel is running cells here");
  const t = await abortable(runner(old.code).done);
  if (t.status !== "done") throw new Error(`rerun('${old.id}') ran as ${t.id} and ended ${t.status}:\n${t.result}`);
  return t.value;
}

export { cancel, signal };

export const docs: Docs = {
  wait: {
    area: "cells",
    for: "wait for a backgrounded cell to finish",
    sig: "wait(id: 't3' | 3, timeoutMs?): Promise<{ id; status; code; started; elapsed_ms; logs; result }>",
    about: "Wait for a backgrounded cell; on timeout it returns the snapshot still running. Stopping the waiting cell ends the wait, not the awaited cell.",
    example: "(await wait('t3')).result",
    throws: "Error naming the range of cells when there is no such task.",
  },
  cancel: {
    area: "cells",
    for: "stop, kill or abort a running cell",
    sig: "cancel(id: 't3' | 3, why?): Promise<string>",
    about: "Stop a running cell: its signal aborts (every typed call it waits in rejects), every process it started is killed with its process group (SIGTERM, SIGKILL after 2 s) and so is every harnessd run; what it started with sh(cmd, { detach: true }) is left. Resolves with what was killed, what would not die and what was left running. Code that never checks the signal keeps running and is said to; the tool's `cancel` input does the same without running a cell.",
    example: "await cancel('t3')",
    throws: "Error when the cell is not running (it says what it is), or is the cell calling.",
  },
  signal: {
    area: "cells",
    sig: "signal(): AbortSignal",
    about: "The running cell's AbortSignal, aborted when the cell is stopped. The typed calls honour it by themselves; pass it to your own waits (a WebSocket, a loop: signal().throwIfAborted()).",
    example: "for (const f of files) { signal().throwIfAborted(); await work(f) }",
  },
  more: {
    area: "cells",
    for: "see the rest of a big result",
    sig: "more(id: 't3' | 3, { path?: 'a.b[0]', rows?: [from, to], chars?: [from, to], fields?: string[] }): unknown",
    about: "A part of an earlier cell's value, which its reply summarized: follow path into it, then take a row range of a list (or a character range of a string), then only some fields of each row. Returns the data itself, printed under the same budget.",
    example: "more('t3', { path: 'items', rows: [100, 200], fields: ['name', 'size'] })",
    throws: "TypeError naming the step of the path that leads nowhere and what is there instead; Error for a cell an earlier kernel ran.",
  },
  rerun: {
    area: "cells",
    for: "run an earlier cell again; restore names after a restart",
    sig: "rerun(id: 't3' | 3): Promise<unknown>",
    about: "Run an earlier cell's code again as a new cell and give its value. After a kernel restart the task list, logs and results are still there (the journal) but the names cells bound are not: rerun the cells that made them.",
    example: "await rerun('t2')",
    throws: "Error with the new cell's id and result when it does not end done.",
  },
  help: {
    area: "cells",
    for: "what can this REPL do; find a function",
    sig: "help(topic?: string): string; help.search(words): string",
    about: "help() lists the areas with one line each; help('files') an area's calls; help('cargo') or help('cargo.test') a call's signature, an example and what it throws; help.search('largest files') finds calls by what they do.",
    example: "help.search('wait for a process')",
    throws: "Error naming the nearest names when nothing is called that.",
  },
  "Bun.file": {
    area: "files",
    for: "read a file; part of a file; count its lines; cat, head, tail",
    sig: "Bun.file(path): BunFile — .text(), .json(), .bytes(), .exists(), .size",
    about: "Read a file: await Bun.file(p).text(); a part of it: (await Bun.file(p).text()).split('\\n').slice(99, 140). Each read is shown to the person as a step.",
    example: "(await Bun.file('Cargo.toml').text()).split('\\n').length",
    throws: "ENOENT naming the path when there is no such file.",
  },
  "Bun.write": {
    area: "files",
    for: "write, create or overwrite a file",
    sig: "Bun.write(path, text | bytes | Blob | Response): Promise<number>",
    about: "Write a file, creating its folders. rename, rm, mkdir, cp come from 'node:fs/promises'. A file the person asked for (an export, a report) goes in vfs, not the project.",
    example: "await Bun.write('notes/todo.md', text)",
  },
  fetch: {
    area: "web",
    for: "HTTP request; call an API; download",
    sig: "fetch(url, init?): Promise<Response>",
    about: "HTTP, shown to the person as a step with its status. A stopped cell's request is aborted. Not curl or wget in a shell. For the words of a web page use browser.read.",
    example: "await (await fetch('https://api.github.com/zen')).text()",
    throws: "The engine's error for a failed connection; the stop's reason when the cell is stopped.",
  },
};
