// The journal's file (journal.ts has the layout and the rules): read it when a kernel starts, append as tasks change.
import { createHash } from "node:crypto";
import { appendFileSync, existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { Appender, compact, replay } from "./journal.ts";
import { HOME, REPL_ID, session } from "./self.ts";
import { restoreTasks, watchTasks } from "./tasks.ts";

/** Where this kernel's journal is: by agent session, or by working directory for a kernel run by hand. */
export const journalPath = (): string => {
  const sid = session();
  const key = sid !== null ? `s${sid}` : `cwd-${createHash("sha256").update(process.cwd()).digest("hex").slice(0, 16)}`;
  return join(HOME(), "repl", "journal", `${key}.log`);
};

const alive = (pid: number): boolean => { try { process.kill(pid, 0); return true; } catch (e) { return (e as NodeJS.ErrnoException).code === "EPERM"; } };

/**
 * Open the journal at `path`: put back the tasks an earlier kernel left (tasks.ts), rewrite the file to just them, and
 * append every change from here on. Returns what the agent should be told about it (a restore, a refusal), if anything.
 * When another live kernel is appending to `path` (two kernels by hand in one directory), this one keeps its own file.
 */
export function openJournal(path: string): string | null {
  let note: string | null = null;
  mkdirSync(dirname(path), { recursive: true });
  if (existsSync(path)) {
    const r = replay(new Uint8Array(readFileSync(path)));
    if (r.refused !== null) {
      const aside = `${path}.unread-${Date.now().toString(36)}`;
      renameSync(path, aside);
      note = `[kernel] the task journal was not read (${r.refused}): earlier cells are not listed. It is kept at ${aside}.`;
    } else if (r.kernel && r.kernel.pid !== process.pid && alive(r.kernel.pid)) {
      path = `${path}.${process.pid}`;
      note = `[kernel] another kernel (pid ${r.kernel.pid}) is running for this ${session() !== null ? "session" : "directory"}: this one starts with no earlier cells and keeps its own journal.`;
    } else if (r.tasks.length) {
      const lost = r.tasks.filter((t) => t.status === "running").map((t) => t.id);
      for (const t of r.tasks) {
        if (t.status !== "running") continue;
        t.status = "lost";
        t.ended_ms ??= t.started_ms;
        t.logs.push("[kernel] the kernel ended while this cell ran; what it had started may still be running (objects({ live: true }))");
      }
      restoreTasks(r.tasks);
      note = `[kernel] this kernel is new: an earlier one ended. Its ${r.tasks.length} cells are still listed (tasks(), task('${r.tasks.at(-1)!.id}')) with their logs and results${lost.length ? `; ${lost.join(", ")} ${lost.length === 1 ? "was" : "were"} running when it ended` : ""}`
        + `${r.torn ? `; ${r.torn} bytes at the journal's end were a cut write and are dropped` : ""}. The names those cells bound are gone: rerun('tN') runs one again.`;
    }
    if (!note || !note.includes("another kernel")) {
      const tmp = `${path}.tmp-${process.pid}`;
      writeFileSync(tmp, compact(r.refused === null ? r.tasks : [], { pid: process.pid, repl: REPL_ID, at_ms: Date.now() }));
      renameSync(tmp, path);
    }
  }
  if (!existsSync(path)) writeFileSync(path, compact([], { pid: process.pid, repl: REPL_ID, at_ms: Date.now() }));
  const appender = new Appender();
  const append = (bytes: Uint8Array | null) => { if (bytes) appendFileSync(path, bytes); };
  const seen = new WeakMap<object, number>();
  watchTasks((t, what) => {
    if (t.restored) return;
    if (what === "start") { append(appender.start(t.record())); seen.set(t, 0); return; }
    // Every line since the last one journaled (a log change says a line was added, not which).
    for (let i = seen.get(t) ?? 0; i < t.logs.length; i++) append(appender.log(t.id, t.logs[i]!));
    seen.set(t, t.logs.length);
    if (what === "end") append(appender.end(t.record()));
  });
  return note;
}
