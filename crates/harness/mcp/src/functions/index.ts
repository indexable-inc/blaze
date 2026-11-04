// Every function the harness exposes. The MCP `ts` tool puts each one in scope by name.
import { googleSearch } from "./google.ts";
import { withHiddenPage } from "../cdp.ts";
import { table, Table } from "../table.ts";
import { nuon } from "../nuon.ts";
import { tasks, task, taskValue, wait } from "../tasks.ts";
import { sh, procs, out, err, lines, kill, ptree } from "../proc.ts";

export const functions = { googleSearch, withHiddenPage, table, Table, nuon, tasks, task, taskValue, wait, sh, procs, out, err, lines, kill, ptree };

/** Shown in the MCP tool description so the model knows what it can call. */
export const docs: Record<keyof typeof functions, string> = {
  googleSearch:
    "googleSearch(query: string, opts?: { limit?: number = 10; hl?: string = 'en' }): Promise<Table<{ title; url; resolved: boolean; site; snippet }>> — Google search in a hidden tab of Dia (CDP :9222). url is the real link when resolved, else Google's /goto link (Google throttles resolving after ~2 per page); site is the displayed breadcrumb.",
  withHiddenPage:
    "withHiddenPage<T>(fn: (page) => Promise<T>): Promise<T> — open a hidden Chrome tab; page has goto(url), evaluate(fn, arg), waitForSelector(sel), url(), send(cdpMethod, params). Closed afterwards.",
  table:
    "table(rows): Table<Row> — wrap any array of objects. A Table is a real array (r[0], .map, .length work) plus verbs that return new Tables: select(...cols), drop(...cols), where(fn), derive({ col: r => v }), sortBy(col | cols, { desc }), head(n), tail(n), unique(...cols), column(col), groupBy(...cols).agg(a => ({ n: a.count(), s: a.sum(c), m: a.mean(c), lo: a.min(c), hi: a.max(c), f: a.first(c), l: a.list(c), d: a.distinct(c), sub: a.rows() })), joinOn(other, on, 'inner' | 'left'), explode(listCol), unnest(recordCol), rows(). Cells may hold nested records, lists and Tables.",
  Table: "Table — the class behind table(); use for instanceof checks.",
  nuon: "nuon(value, { maxRows? }): string — format any value the way results print: NUON, i.e. JSON with bare strings and keys, and lists of records as [[col, ...]; [v, ...], ...].",
  tasks: "tasks(): Table<{ id; status: running|done|error; code; started; elapsed_ms; logs: count; result: preview }> — every cell run so far.",
  task: "task(id: 't3' | 3): { id; status; code; started; elapsed_ms; logs: string[]; result: string } — snapshot of one cell; logs and result stay after it finishes.",
  taskValue: "taskValue(id): unknown — raw value of a finished cell's last expression (or its thrown error).",
  wait: "wait(id, timeoutMs?): Promise<task snapshot> — wait for a backgrounded cell; on timeout returns it still running.",
  sh: "sh(cmd: string | string[], { cwd?, env?, stdin?: string, maxBytes? = 8MB per stream (oldest lines dropped, line numbers kept), timeoutMs? }): Proc — spawn now (string runs via /bin/sh -c), return a handle at once; `await sh(...)` (or a cell ending in sh(...)) waits and gives { id, pid, exit_code, signal, stdout, stderr }. Handle: id 'p1', pid, status running|exited|killed, kill(sig?).",
  procs: "procs(): Table<{ id; pid; cmd; status; exit_code; started; elapsed_ms; stdout_bytes; stderr_bytes; truncated }> — all sh() processes, finished included.",
  out: "out(p: 'p1' | pid | Proc, { from?, to?, tail?, new?: true, cursor? = 'default' }): string — stdout lines; from/to are 1-based inclusive line numbers, tail last N; new: only output since the previous read with that cursor (every read moves it to the end).",
  err: "err(p, opts?): string — stderr, same options as out().",
  lines: "lines(p, 'stdout' | 'stderr' = 'stdout', opts?): Table<{ n; text }> — same options as out(), as numbered rows.",
  kill: "kill(p, signal? = 'SIGTERM'): proc record.",
  ptree: "ptree(): Promise<Table<{ pid; ppid; depth; proc: 'p1' | null; stat; etime; command (indented by depth) }>> — live process tree of everything this REPL spawned, grandchildren included (from ps).",
};
