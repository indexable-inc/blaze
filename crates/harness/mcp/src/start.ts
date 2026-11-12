// Start the kernel in this process: its globals, its type checker, its journal, and what the process does about errors
// nobody awaited. Every adapter (mcp.ts, serve.ts) begins here, so they serve one kernel made one way.
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { docs, functions } from "./functions/index.ts";
import { journalPath, openJournal } from "./journal-file.ts";
import { Kernel } from "./kernel.ts";
import { serve } from "./serve.ts";
import { allTasks } from "./tasks.ts";
import { TypeChecker } from "./typecheck.ts";
import { UI_TYPED } from "./ui/index.ts";

export type StartOptions = {
  /** The task journal's file; null for a kernel that keeps none. Default: by session or working directory (journal-file.ts). */
  journal?: string | null;
  /** Type-check cells (typecheck.ts). Default true; HARNESS_REPL_TYPECHECK=0 turns it off for the whole kernel. */
  typecheck?: boolean;
};

export function start(o: StartOptions = {}): Kernel {
  // stdout may be an adapter's channel (MCP): anything logged outside a cell goes to stderr.
  console.log = console.error;
  // cursor runs in an empty private workspace (so it loads no repo rules) and passes the real one here.
  if (process.env.HARNESS_CWD) process.chdir(process.env.HARNESS_CWD);

  const typed = [...Object.keys(functions), ...UI_TYPED];
  const check = o.typecheck ?? process.env.HARNESS_REPL_TYPECHECK !== "0";
  const kernel = new Kernel(functions, { known: Object.keys(docs) });
  if (check) {
    const here = dirname(fileURLToPath(import.meta.url));
    kernel.repl.checker = new TypeChecker({ module: join(here, "functions", "index.ts"), typed, untyped: Object.keys(kernel.repl.scope.globals), known: Object.keys(docs) });
  }

  const journal = o.journal === undefined ? journalPath() : o.journal;
  if (journal !== null) {
    try {
      const note = openJournal(journal);
      if (note) kernel.notes.push(note);
    } catch (e) {
      kernel.notes.push(`[kernel] the task journal at ${journal} cannot be used (${e instanceof Error ? e.message : String(e)}): this kernel's cells will not outlive it.`);
    }
  }

  // An error nobody awaited fails nothing but itself, and is the cell's that caused it (Kernel.unawaited): Bun's default
  // ends the process, and with it every cell's names and the agent's turn.
  process.on("unhandledRejection", (e) => kernel.unawaited(e, "an unawaited promise rejected"));
  process.on("uncaughtException", (e) => kernel.unawaited(e, "a callback threw"));
  // The kernel going away takes what its running cells own with it (not what they detached).
  process.on("exit", () => { for (const t of allTasks()) if (t.status === "running") for (const o of t.owned) o.kill(true); });

  if (process.env.HARNESS_REPL_SOCK) serve(kernel, process.env.HARNESS_REPL_SOCK);
  return kernel;
}
