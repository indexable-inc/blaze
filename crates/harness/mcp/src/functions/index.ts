// Every function the harness exposes. The MCP `ts` tool puts each one in scope by name.
import { googleSearch } from "./google.ts";
import { gmail } from "./gmail.ts";
import { github } from "./github.ts";
import { browser } from "./browser.ts";
import { withHiddenPage } from "../cdp.ts";
import { table, Table, LazyTable, col, lit, tableDocs } from "../table.ts";
import { gitLog } from "../git-log.ts";
import { nuon } from "../nuon.ts";
import { tasks, task, taskValue, wait as waitTask } from "../tasks.ts";
import { effect } from "../effects.ts";
import { brief } from "../shown.ts";
import { agents } from "../agents.ts";
import { sql } from "../sql.ts";
import { spawn, obj, objects, tree } from "../obj.ts";
import { rg, ls, glob, du, searchDocs } from "../search.ts";
import { $ } from "../dollar.ts";
import { follow, cargo, followDocs } from "../follow.ts";
import { pmap } from "../pmap.ts";
import { image } from "../image.ts";
import { vfs } from "../vfs.ts";
import { sh, procs, out, err, lines, kill, ptree, runs, stage, runInfo } from "../proc.ts";
import { skill, skills, skillDocs } from "../skills.ts";

/** `wait` as a step of the cell that waits (effects.ts kind `wait`): the step shows the awaited cell's own value (or its
 *  error), so the app draws that cell's output, never the task's record (id, status, timestamps); the cell still returns
 *  the record to the model. */
const wait = (id: string | number, timeoutMs?: number) =>
  effect("wait", "Waiting for an earlier cell", "Waited for an earlier cell", String(id), () => waitTask(id, timeoutMs), (v) => ({
    ok: v.status !== "error",
    detail: v.status === "running" ? "still running" : v.status === "error" ? brief(taskValue(id)) : "",
    show: v.status === "running" ? undefined : taskValue(id),
  }));

export const functions = { skill, skills, rg, ls, glob, du, cargo, follow, $, pmap, image, vfs, spawn, obj, objects, tree, sql, googleSearch, gmail, github, browser, withHiddenPage, table, Table, LazyTable, col, lit, gitLog, nuon, tasks, task, taskValue, wait, sh, procs, out, err, lines, kill, ptree, runs, stage, runInfo, agents };

/** Shown in the MCP tool description so the model knows what it can call. A call only some tasks need keeps its long
 *  reference in a skill (mcp/skills/) and a one-line pointer here. */
export const docs: Record<keyof typeof functions, string> = {
  ...skillDocs,
  ...searchDocs,
  ...followDocs,
  $: "$`cmd ${arg}`: thenable of { stdout, stderr, exitCode, ok, text(), lines(), json() } — runs a program through sh(); each ${value} is one quoted word (arrays several), $.raw(s) unquoted. Chain before awaiting: .nothrow(), .cwd(dir), .env(vars), .timeout(ms), .stdin(data), .why(reason). A non-zero exit throws with the stderr tail. `await $`git log --oneline -5`.lines()`.",
  pmap: "pmap(items, (item, i, signal) => ..., { concurrency? = 16 }): Promise<R[]> — map in parallel, results in input order; the first error aborts the rest: `await pmap(await glob('src/**/*.ts'), async p => (await Bun.file(p).text()).split('\\n').length)`.",
  image: "image(path | svgMarkup, { format?: 'png'|'jpeg'|'pdf', width?, height?, out? }): Promise<{ path, bytes, format }> — convert or resize (rsvg-convert, sips); an SVG with no format stays an SVG.",
  vfs: "vfs.write(name, text | bytes | Blob | image()): Promise<path> — a file the person asked for (an exported image, a report) goes here, not in the project; show it with render(svelte`<Image src={f} />`, { f }) or <FileRef path={f} />. Also vfs.text/bytes/read(name), list(dir?), move(a, b), delete(name), path(name?).",
  spawn:
    "spawn(kind: 'claude'|'codex'|'cursor'|'shell'|'proc', { name?, model?, effort?, prompt?, cmd? (proc), cwd?, env?, args?, parent? = this agent, stdin? }): Promise<Obj> — start a child object, or get the running one holding that name. A claude/codex/cursor sub agent must give name, model and (when the model lists efforts) effort, never --model/--effort in args; nothing is defaulted; which to pick: skill('agents'). Obj (every agent, proc, stage is one): fields id ('s7' | 'r12' | 'r12.0'), kind, name, parent, status, exitCode; send(method, args) = RPC; prompt(text); reply(timeoutMs? = 600000): Promise<string> = waits for the turn to finish and returns its last assistant message (claude/codex; the round trip: `const a = await spawn('claude', { name: 'x', model: 'claude-opus-5-5', effort: 'high', prompt: '...' }); await a.reply(); await a.prompt('more'); await a.reply()`); input(bytes); stdin(text), closeStdin() (proc stdin stays open until closed); interrupt() (agents Esc, shell ^C, proc SIGINT); signal(sig); kill(); resize(cols, rows); wait(pred | { status?, match?: string|RegExp, timeoutMs? }) (no arg: until it stops); refresh(). Sync reads: .stdout / .stderr (proc: what its caller sees; a stage: its own) / .pty (escapes stripped) / read(stream, { from?, to?, tail?, new? }) / .messages: Table<{ n, role, kind: text|thinking|tool_call|tool_result|turn_end, tool, text, ts_ms }>. Async: screen(), events({ stream?, since?, tail? }): Table of stored lines, children().",
  obj: "obj(idOrName): Promise<Obj> — handle on any object, live or past: 's7', 'r12', 'r12.0', or a name.",
  objects:
    "objects({ kind?, parent?, status?, name?, live?, all? (stages and REPL cells too), limit? = 200 }): Promise<Table<{ id; kind; name; parent; status; exit_code; cmd; cwd; created; ended_ms }>> — every object, live and past, newest first. tree(root?, filter?): the same as an indented tree (me marks this agent).",
  tree: "tree(root?, filter?) — see objects.",
  sql:
    "sql`select ...`: Promise<Table<Row>> — DuckDB over everything harness has stored, surviving daemon restarts; ${values} become bound parameters. Everything is an object: PTY sessions 's7' (kind claude|codex|cursor|shell), shell runs 'r12' (kind proc, every sh() call), their pipeline stages 'r12.0' (kind stage, parent r12), REPL cells (kind repl). Views: objects(id, kind, name, parent, cwd, args json, created_ms, ended_ms, status, exit_code, signal, meta json); sessions / runs (+cmd) / stages (+text, idx, pipeline) = objects by kind; streams(object, stream: pty|stdout|stderr|stdin|final_err|messages (TUI agent sessions only)|chat, path, bytes, compacted_ms); lines(object, kind, stream, line_no, ts_ms, text) = every line of every terminal and process stream, live and archived (alias events); messages(object, role, kind: text|thinking|tool_call|tool_result|turn_end, tool, text, ts_ms) = agent conversations (a chat session's rows come from its chat log, a TUI agent session's from its transcript); chat(session, n, ev, ts_ms, partial, block, kind, role, text, tool, input json, error, cell, status, ms, effects json, value json, ui json, ok, stop, duration_ms, cost_usd, usage json, agent, model, session_id, busy, valid, json) = one row per event of every chat session ever (its log, live or long finished alike), n its record in the log; ev is user|session|busy|block|tool_call|tool_result|tool_detail|note|error|turn_end, or block_start|delta|tool_start|tool_delta with partial = true (streaming pieces a later block/tool_call replaces: add `where not partial`); tool names the call on its result and detail rows too; valid = false (ev null) for a torn or undecodable frame a crash left; chats(id, agent, name, parent, cwd, status, started_ms, ended_ms, last_ms, model, agent_session, turns, messages, title, first_message, cost_usd, bad_lines) = one row per chat session, title its first user line. Examples: what chats asked about tests: sql`select c.session, c.ts_ms, c.text from chat c where c.ev = 'user' and c.text ilike '%test%' order by c.ts_ms desc limit 20`; the tools a chat used most: sql`select tool, count(*) n from chat where ev = 'tool_call' and session = ${'s12'} group by tool order by n desc`; errors across all runs: sql`select object, line_no, text from lines where kind = 'stage' and text ilike '%error%' order by object desc limit 20`; last clippy output from any agent: sql`select l.line_no, l.text from lines l where l.object = (select id from stages where text like 'cargo clippy%' order by created_ms desc limit 1) order by stream, line_no`; one run's stages: sql`select id, text, status, exit_code from stages where parent = ${'r12'} order by idx`.",
  googleSearch:
    "googleSearch(query: string, opts?: { limit?: number = 10; hl?: string = 'en' }): Promise<Table<{ title; url; resolved: boolean; site; snippet }>> — Google search in a page of the harness browser (embedded Chromium the daemon runs; its card shows in the chat). url is the real link when resolved, else Google's /goto link (Google throttles resolving after ~2 per page); site is the displayed breadcrumb.",
  gmail: "gmail.* — Andrew's Gmail through the harness browser's profile (he signs in once in the browser pane, cmd+shift+B): search, messages, read, labels, attachment, send, reply, forward, draft, archive, trash, label, markRead, via; await skill('gmail') for every call's arguments and rows before using it.",
  browser:
    "browser.* — the harness browser (embedded Chromium the daemon runs; the person sees it in the browser pane, cmd+shift+B). open(url): Promise<Page> opens url in a view that stays open (a tab in the pane), loaded or after 20 s still loading, and shows it as a live card in the chat; draw one yourself with <Browser view={page.view} /> or <Browser src=\"https://…\" />. views(): Table<{ view; target; url; title; loading; can_back; can_forward; owner; size; scale }>. extensions(): Table<{ id; name; version; enabled; popup; source: store|unpacked; path }>. manage(url? = 'chrome://extensions'): opens the manage window (a full Chrome-style window: install from the Chrome Web Store there). When the browser cannot start, every call throws CdpUnavailable naming why (no other browser is tried).",
  github:
    "github.* — GitHub with gh's token, never `gh api` in a shell: graphql<T>(query, vars?): Promise<T> (data, typed by you); prs({ repo: 'owner/name', state?: open|closed|merged|all = open, author?, limit? = 30 }): Table<{ number; title; state; draft; author; ci: success|failure|pending|error|null; mergeable; review: approved|changes_requested|review_required|null; head; base; created; updated; url }>; pr({ repo, n }): that row plus body, sha, files, checks, comments, reviews (Tables); issues({ repo, state?, author?, assignee?, labels?, limit? }): Table<{ number; title; state; author; labels; assignees; comments; created; updated; url }>; checks({ repo, ref }): Table<{ name; workflow; status; conclusion; url; started; completed }>; rest(path, { method?, body? }) for what GraphQL lacks (Actions logs). await skill('github') first.",
  withHiddenPage:
    "withHiddenPage<T>(fn: (page) => Promise<T>): Promise<T> — a page of the harness browser for fn, closed afterwards; page has view, goto(url) (a cell's goto shows the page as a live card), evaluate(fn, arg), waitForSelector(sel), url(), title(), send(cdpMethod, params), on(cdpEvent, fn), waitFor(cdpEvent, ms).",
  table: tableDocs.table,
  col: tableDocs.col,
  lit: "lit(value) — see col.",
  gitLog: tableDocs.gitLog,
  LazyTable: "LazyTable — the class behind table.lazy() and gitLog().",
  Table: "Table — the class behind table(); use for instanceof checks.",
  nuon: "nuon(value, { maxRows? }): string — format any value the way results print: NUON, i.e. JSON with bare strings and keys, and lists of records as [[col, ...]; [v, ...], ...].",
  tasks: "tasks(): Table<{ id; status: running|done|error; code; started; elapsed_ms; logs: count; result: preview }> — every cell run so far.",
  task: "task(id: 't3' | 3): { id; status; code; started; elapsed_ms; logs: string[]; result: string } — snapshot of one cell; logs and result stay after it finishes.",
  taskValue: "taskValue(id): unknown — raw value of a finished cell's last expression (or its thrown error).",
  wait: "wait(id, timeoutMs?): Promise<task snapshot> — wait for a backgrounded cell; on timeout returns it still running.",
  sh: "sh(cmd: string | string[], { cwd?, env?, stdin?: string, why?, maxBytes? = 8MB per stream kept in memory (oldest lines dropped, line numbers kept), timeoutMs?, local?: true to bypass harnessd }): Proc — the awaitable form of spawn('proc'): start now, return a handle at once; `await sh(...)` (or a cell ending in sh(...)) waits and gives { id: 'p1', run: 'r12' | null, pid, exit_code, signal, stdout, stderr }. Runs through harnessd (bash) when it's up: the command is split at top-level ; && || and |, and the FULL stdout and stderr of EVERY pipeline stage is stored as run r12 stage 0, 1, ... readable later by any agent (`cargo clippy 2>&1 | head -n5` shows you 5 lines, clippy still runs to the end and stage(r, 0) has all of it — so pipe through head/tail/grep freely, nothing is lost). stdout here = last stage of each pipeline; stderr = every stage's. Falls back to a local Bun.spawn (run: null) when the daemon is down. Handle: id, run, pid, status running|exited|killed, kill(). For running programs: a command that only reads files or the tree (cat, head, tail, grep, rg, find, ls, stat, wc, sed -n, awk, du, tree, which), even in a pipe, xargs or $(...), and GitHub reads through gh (api, pr list/status/view/checks, issue list/view) and curl/wget, are refused with the typed call to use (Bun.file, rg, glob, ls, du, github.*, fetch) unless { why } says why the shell is needed; filtering a program's output (cargo test 2>&1 | grep FAIL) runs.",
  procs: "procs(): Table<{ id; run; pid; cmd; status; exit_code; started; elapsed_ms; stdout_bytes; stderr_bytes; truncated }> — this REPL's sh() processes, finished included.",
  out: "out(p: 'p1' | 'r12' | pid | Proc, { from?, to?, tail?, new?: true, cursor? = 'default', stage? }): string — stdout lines; from/to are 1-based inclusive line numbers, tail last N; new: only output since the previous read with that cursor (every read moves it to the end). With stage: n | 'final' it returns Promise<string> of that stored stage (same as stage()).",
  err: "err(p, opts?): string — stderr, same options as out() (stage too).",
  lines: "lines(p, 'stdout' | 'stderr' = 'stdout', opts?): Table<{ n; text }> — same options as out(), as numbered rows.",
  kill: "kill(p, signal? = 'SIGTERM'): proc record. harnessd runs: SIGTERM to the run's process group, SIGKILL after 3s.",
  ptree: "ptree(): Promise<Table<{ pid; ppid; depth; proc: 'p1' | null; stat; etime; command (indented by depth) }>> — live process tree of everything this REPL spawned, grandchildren included, plus each live harnessd run's tree (from ps).",
  runs: "runs({ all?: boolean, session?: number, limit? = 50 }): Promise<Table<{ id: 'r12'; session; status: running|exited|killed|lost; exit_code; stages: '0:0 1:running' (index:exit|status); cmd; cwd; started; elapsed_ms; out_bytes; err_bytes }>> — harnessd runs newest first: this agent's by default, every agent's with all: true (or one session's).",
  stage: "stage(run: 'r12' | Proc, i: number | 'final' = 'final', { stream?: 'out' | 'err' = 'out', from?, to?, tail?, new?, cursor? }): Promise<string> — stored output of one pipeline stage of any agent's run (works while it runs: new: true tails it); 'final' = what the caller saw (stdout of each pipeline's last stage; stream 'err' = all stderr).",
  runInfo: "runInfo(run: 'r12' | Proc): Promise<{ id; cmd; cwd; session; parent; status; exit_code; signal; start_ms; end_ms; pipelines: { op: ';'|'&&'|'||'|null; stages: number[] }[]; stages: Table<{ i; text; pipeline; pid; exit_code; signal; status: pending|running|done|skipped|unknown; start_ms; end_ms; stdout_bytes; stderr_bytes }>; procs?: live process tree while running }> — the run graph for one command.",
  agents: "agents.* — deprecated aliases of the above (bare session numbers): self(), parent(), list(), tree(), spawn(), send(), notifyParent(text), screen(), waitFor(), kill(), url(id?) (web UI link).",
};
