// A command that runs a while, shown to the person as it runs and told to the model as a digest (ported from den's
// repl/src/follow.ts, "Live commands"). The person sees one "build" step: its `doing` follows the command's progress
// ("Checking blaze-core · 132/388") and its value is the findings so far as a table { level, at, text }, redrawn at most
// every UPDATE_MS; when it ends, its done words are the closing line ("Checked · 1 error · 3 warnings in 41 s"). The model
// gets `{ ok, summary, counts, findings, more?, failure?, unchecked?, log }`: never the raw output, which goes to the
// file `log` names, for reading on purpose.
//
// `cargo.check/clippy/build/test` are presets over it: cargo's JSON messages, cargo's own progress bar (forced on with
// CARGO_TERM_PROGRESS_WHEN, so the unit total is cargo's), `--keep-going` (test: `--no-fail-fast`) so one run shows every
// error, and, for tests, libtest's stable text. When it fails, `unchecked` says what was left unbuilt or unrun, from what
// the JSON says (units planned by the bar vs compiler-artifact messages; test executables built vs run).
import { closeSync, existsSync, openSync, readFileSync, writeSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, isAbsolute, join, relative, resolve } from "node:path";
import { oneLine, quiet, step as beginStep, type Step as LiveStep } from "./effects.ts";

/**
 * Something the command found: a compiler diagnostic, a failed test. `at` is `file:line` (absolute, or relative to the
 * working directory); the digest and the step show `{ level, at, text }`.
 */
export interface Finding { level: string; at?: string; text: string }
/** Where the command is: `label` says the current thing, `done` of `total` how far along (a total of 0 is not known). */
export interface Progress { label?: string; done?: number; total?: number }
/** What a hook is handed beside the line: which stream it came on, and counters the digest reports. */
export interface FollowContext {
  from: "stdout" | "stderr";
  /** Adds `n` (1) to the counter `name`, which the digest's `counts` and the summary report. */
  tally(name: string, n?: number): void;
  readonly counts: Readonly<Record<string, number>>;
}
/** What a summary, a failure or an `unchecked` is made from. */
export interface FollowState {
  title: string;
  ok: boolean;
  /** null when it was killed (a timeout) or never started. */
  exitCode: number | null;
  timedOut: boolean;
  ms: number;
  counts: Readonly<Record<string, number>>;
  findings: readonly Finding[];
  failure?: string;
}
export interface FollowOptions<M = unknown> {
  /** What runs, as the step and the summary say it ('cargo clippy -p x'); the command by default. */
  title?: string;
  /** stdout lines that are JSON reach the hooks parsed; other lines, and every stderr line, as text. */
  json?: boolean;
  /** How many steps there are, when known before they come (a promise is read when it settles). */
  total?: number | Promise<number>;
  /** The step a line says the command is on: its label (each one counts a step), or {label, done, total}. */
  step?: (line: M | string, context: FollowContext) => string | Progress | null | undefined | false;
  /** What a line found, if anything. */
  find?: (line: M | string, context: FollowContext) => Finding | Finding[] | null | undefined | false;
  /** The closing line ("Checked · 0 errors · 3 warnings in 41 s"). */
  summary?: (state: FollowState) => string;
  /** Why it failed, when it did; by default the last error-looking stderr line, else its last line, else the exit code. */
  failure?: (state: FollowState & { stderr: readonly string[] }) => string | undefined;
  /** What a failed run left unchecked, when the hooks know (the cargo presets do). */
  unchecked?: (state: FollowState) => string | undefined;
  /** Working directory (relative to the REPL's); `at` in the digest is relative to it. */
  cwd?: string;
  /** Added to the environment. */
  env?: Record<string, string>;
  /** Milliseconds before the command and everything it started are killed. */
  timeout?: number;
  /** How many findings the digest lists (20). */
  limit?: number;
}
/** What the model gets back. */
export interface Digest {
  ok: boolean;
  summary: string;
  counts: Record<string, number>;
  findings: Finding[];
  /** Findings past `limit`, not listed. */
  more?: number;
  failure?: string;
  /** What a failed run never reached (crates not built, test binaries not run), as far as the output says. */
  unchecked?: string;
  /** The full raw output (stdout and stderr, as they came), to read on purpose. */
  log: string;
}

const DEFAULT_LIMIT = 20;
const FINDING_TEXT = 300;
/** The step's value: at most this many findings while running (the end shows them all, as far as its budget goes). */
const LIVE_ROWS = 200;
/** The step is redrawn at most this often. */
const UPDATE_MS = 200;
/** After the process exits, a grandchild holding its pipes open delays the digest by at most this. */
const DRAIN_MS = 2_000;
/** A timed-out command gets SIGTERM, then SIGKILL after this. */
const KILL_GRACE_MS = 2_000;

const plural = (n: number, one: string, many = `${one}s`) => `${n.toLocaleString("en-US")} ${n === 1 ? one : many}`;
/** "4.2 s", "41 s", "2 min 5 s". */
export function took(ms: number): string {
  const s = ms / 1000;
  if (s < 10) return `${s.toFixed(1)} s`;
  if (s < 60) return `${Math.round(s)} s`;
  const whole = Math.round(s);
  return `${Math.floor(whole / 60)} min${whole % 60 ? ` ${whole % 60} s` : ""}`;
}

/** Splits text into lines as it comes; a carriage return ends a line too (a progress bar redraws its line with one). */
export class LineSplitter {
  private partial = "";
  push(text: string): string[] {
    let all = this.partial + text;
    // A trailing \r may be the first half of a \r\n split across chunks: held back, so it does not make an empty line.
    const held = all.endsWith("\r") ? "\r" : "";
    if (held) all = all.slice(0, -1);
    const parts = all.split(/\r\n|\r|\n/);
    this.partial = (parts.pop() ?? "") + held;
    return parts;
  }
  end(): string[] { const last = this.partial.replace(/\r$/, ""); this.partial = ""; return last ? [last] : []; }
}

/** Words of a command typed as one string: split on blanks, with '...' and "..." kept whole. */
export function words(text: string): string[] {
  const out: string[] = [];
  const re = /'([^']*)'|"((?:[^"\\]|\\.)*)"|([^\s'"]+)/g;
  let word: string | undefined;
  let last = -1;
  for (let m = re.exec(text); m; m = re.exec(text)) {
    const piece = m[1] ?? m[2]?.replace(/\\(.)/g, "$1") ?? m[3]!;
    // Adjacent pieces with no blank between them are one word (--cfg='a b').
    if (word !== undefined && m.index === last) word += piece;
    else { if (word !== undefined) out.push(word); word = piece; }
    last = re.lastIndex;
  }
  if (word !== undefined) out.push(word);
  return out;
}

const shq = (a: string) => (/^[\w@%+=:,./-]+$/.test(a) ? a : `'${a.replaceAll("'", `'\\''`)}'`);

/** An absolute `file:line` under `cwd` said relative to it, the way the model would write it. */
function shortPath(at: string, cwd: string): string {
  if (!isAbsolute(at)) return at;
  const short = relative(cwd, at);
  return short && !short.startsWith("..") ? short : at;
}

/** A finding as the digest and the step list it: `at` relative to `cwd`, `text` clipped. */
const listed = (f: Finding, cwd: string): Finding => ({
  level: f.level,
  ...(f.at ? { at: shortPath(f.at, cwd) } : {}),
  text: f.text.length > FINDING_TEXT ? `${f.text.slice(0, FINDING_TEXT)}…` : f.text,
});

/** Every process below `pid`, deepest last (from one `ps`). */
function descendants(pid: number): number[] {
  const r = quiet(() => Bun.spawnSync(["ps", "-A", "-o", "pid=,ppid="], { stdout: "pipe", stderr: "ignore" }));
  const children = new Map<number, number[]>();
  for (const line of r.stdout.toString().split("\n")) {
    const [c, p] = line.trim().split(/\s+/).map(Number);
    if (!c || p === undefined || Number.isNaN(p)) continue;
    (children.get(p) ?? children.set(p, []).get(p)!).push(c);
  }
  const out: number[] = [];
  const queue = [pid];
  while (queue.length) for (const c of children.get(queue.shift()!) ?? []) { out.push(c); queue.push(c); }
  return out;
}

/** SIGTERM to the process and everything below it, SIGKILL to what is left after KILL_GRACE_MS. */
function killTree(sub: Bun.Subprocess) {
  const signal = (sig: NodeJS.Signals) => {
    for (const pid of [sub.pid, ...descendants(sub.pid)]) { try { process.kill(pid, sig); } catch { /* already gone */ } }
  };
  signal("SIGTERM");
  const late = setTimeout(() => { if (sub.exitCode === null && sub.signalCode === null) signal("SIGKILL"); }, KILL_GRACE_MS);
  void sub.exited.then(() => clearTimeout(late));
}

/**
 * `follow(cmd, options)`: runs `cmd` (a string, run by bash, or an argv) as one live "build" step and resolves to the digest.
 * Each stdout and stderr line goes to `step`, then `find`.
 */
export async function follow<M = unknown>(cmd: string | readonly string[], options: FollowOptions<M> = {}): Promise<Digest> {
  const started = Date.now();
  const text = typeof cmd === "string" ? cmd : cmd.map(shq).join(" ");
  const title = options.title ?? oneLine(text);
  const cwd = options.cwd ? resolve(process.cwd(), options.cwd) : process.cwd();
  const env: Record<string, string | undefined> = { ...process.env, ...options.env };
  const argv = typeof cmd === "string" ? [Bun.which("bash") ?? "/bin/sh", "-c", cmd] : [...cmd];
  if (argv.length === 0) throw new Error("follow: an empty command");
  // Found on the PATH the command will run with (options.env may put a different one first).
  const program = Bun.which(argv[0]!, { PATH: env.PATH ?? "", cwd });
  if (!program) throw new Error(`follow: no program ${argv[0]} on PATH, for ${title}`);
  argv[0] = program;

  const log = join(tmpdir(), `harness-follow-${started}-${Math.random().toString(36).slice(2, 8)}.log`);
  const fd = openSync(log, "w");

  const counts: Record<string, number> = {};
  const findings: Finding[] = [];
  const seen = new Set<string>();
  const stderr: string[] = [];
  const at = { label: title, done: 0, total: 0, totalSet: false };
  const tally = (name: string, n = 1) => { counts[name] = (counts[name] ?? 0) + n; };

  const live: LiveStep | null = beginStep("build", `Running ${title}`, `Ran ${title}`, text);
  let dirty = false;
  let lastDraw = 0;
  let drawTimer: Timer | undefined;
  const doing = () => at.total > 0 && !/\d+\/\d+/.test(at.label) ? `${at.label} · ${at.done}/${at.total}` : at.label;
  const tallyWords = () => Object.entries(counts).filter(([, n]) => n > 0).map(([k, n]) => plural(n, k)).join(" · ");
  const draw = () => {
    clearTimeout(drawTimer);
    drawTimer = undefined;
    if (!live || !dirty) return;
    dirty = false;
    lastDraw = Date.now();
    live.update({ doing: doing(), detail: tallyWords(), ...(findings.length ? { show: findings.slice(0, LIVE_ROWS).map(f => listed(f, cwd)) } : {}) });
  };
  const changed = () => {
    dirty = true;
    if (!live || drawTimer) return;
    const wait = UPDATE_MS - (Date.now() - lastDraw);
    if (wait <= 0) draw();
    else drawTimer = setTimeout(draw, wait);
  };

  const handle = (line: string, from: "stdout" | "stderr") => {
    if (from === "stderr") { stderr.push(line); if (stderr.length > 200) stderr.shift(); }
    let message: M | string = line;
    if (options.json && from === "stdout" && /^\s*[[{]/.test(line)) {
      try { message = JSON.parse(line) as M; } catch { message = line; }
    }
    const context: FollowContext = { from, tally, counts };
    const s = options.step?.(message, context);
    if (typeof s === "string") { at.label = s; at.done++; changed(); }
    else if (s) {
      if (s.label !== undefined) at.label = s.label;
      if (s.done !== undefined) at.done = s.done;
      if (s.total !== undefined) { at.total = s.total; at.totalSet = true; }
      changed();
    }
    const found = options.find?.(message, context);
    for (const finding of !found ? [] : Array.isArray(found) ? found : [found]) {
      const key = `${finding.level}|${finding.at ?? ""}|${finding.text}`;
      if (seen.has(key)) continue;
      seen.add(key);
      findings.push(finding);
      tally(finding.level);
      changed();
    }
  };
  if (options.total !== undefined) {
    void Promise.resolve(options.total).then(n => {
      if (!at.totalSet && Number.isFinite(n) && n > 0) { at.total = n; changed(); }
    }, () => undefined);
  }

  let exitCode: number | null = null;
  let timedOut = false;
  let broke: string | undefined;
  try {
    // quiet: the spawn is this step's, not an exec step of its own.
    const sub = quiet(() => Bun.spawn(argv, { cwd, env, stdin: "ignore", stdout: "pipe", stderr: "pipe" }));
    const timer = options.timeout ? setTimeout(() => { timedOut = true; killTree(sub); }, options.timeout) : undefined;
    const pump = async (from: "stdout" | "stderr", stream: ReadableStream<Uint8Array>) => {
      const split = new LineSplitter();
      const dec = new TextDecoder();
      for await (const chunk of stream) {
        writeSync(fd, chunk);
        for (const line of split.push(dec.decode(chunk, { stream: true }))) handle(line, from);
      }
      for (const line of split.push(dec.decode())) handle(line, from);
      for (const line of split.end()) handle(line, from);
    };
    const pipes = Promise.all([pump("stdout", sub.stdout), pump("stderr", sub.stderr)]);
    await sub.exited;
    clearTimeout(timer);
    let drained: Timer | undefined;
    const late = await Promise.race([pipes.then(() => false), new Promise<boolean>(r => (drained = setTimeout(() => r(true), DRAIN_MS)))]);
    clearTimeout(drained);
    if (late) { void sub.stdout.cancel().catch(() => undefined); void sub.stderr.cancel().catch(() => undefined); }
    exitCode = sub.signalCode !== null ? null : sub.exitCode;
  } catch (error) {
    broke = (error instanceof Error ? error.message : String(error)).split("\n", 1)[0];
  } finally {
    closeSync(fd);
  }
  clearTimeout(drawTimer);

  const ms = Date.now() - started;
  const finished: FollowState = { title, ok: exitCode === 0 && !timedOut && !broke, exitCode, timedOut, ms, counts, findings };
  const failure = finished.ok ? undefined
    : timedOut ? `timed out after ${took(options.timeout!)}`
    : broke ?? options.failure?.({ ...finished, stderr }) ?? defaultFailure(stderr, exitCode, findings.length);
  const done: FollowState = { ...finished, ...(failure ? { failure } : {}) };
  const summary = options.summary?.(done) ?? defaultSummary(done);
  const unchecked = done.ok ? undefined : options.unchecked?.(done);
  const limit = options.limit ?? DEFAULT_LIMIT;
  const digest: Digest = {
    ok: done.ok, summary, counts: { ...counts }, findings: findings.slice(0, limit).map(f => listed(f, cwd)),
    ...(findings.length > limit ? { more: findings.length - limit } : {}),
    ...(failure ? { failure } : {}),
    ...(unchecked ? { unchecked } : {}),
    log,
  };
  if (live) {
    live.e.done = summary;
    const detail = timedOut ? "timed out" : broke ? broke : exitCode === null ? "killed" : `exit ${exitCode}`;
    live.end(done.ok, detail, digest, findings.length ? findings.map(f => listed(f, cwd)) : undefined);
  }
  return digest;
}

function defaultFailure(stderr: readonly string[], exitCode: number | null, found: number): string {
  const lines = stderr.map(line => line.trim()).filter(Boolean);
  // The findings already say what went wrong; a last line that is not an error (a tool's "Saved lockfile") would only mislead.
  const said = lines.findLast(line => /^(error|fatal)\b|\b(panicked|failed)\b/i.test(line)) ?? (found ? undefined : lines.at(-1));
  const code = exitCode === null ? "killed" : `exit ${exitCode}`;
  return said ? `${said.slice(0, 300)} (${code})` : code;
}

function defaultSummary(state: FollowState): string {
  const parts = Object.entries(state.counts).filter(([, n]) => n > 0).map(([name, n]) => plural(n, name));
  const what = parts.length ? parts.join(" · ") : state.ok ? "Done" : "Failed";
  const why = state.ok || !state.failure || (parts.length && /^exit \d+$/.test(state.failure)) ? "" : ` · ${state.failure}`;
  return `${what} in ${took(state.ms)}${why}`;
}

// ---- cargo -------------------------------------------------------------------------------------------------------------------

/** One frame of cargo's progress bar: `Building [=====>   ] 132/388: blaze-core, blaze-query(build)`. */
export function cargoBar(line: string): { verb: string; done: number; total: number; names: string[] } | undefined {
  const m = /^\s*([A-Z][\w-]*)\s+\[[=> -]*\]\s+(\d+)\/(\d+)(?::\s*(.*?))?\s*$/.exec(line);
  if (!m) return undefined;
  const names = (m[4] ?? "").split(/,\s*/).map(name => name.replace(/\(.*\)$/, "").replace(/\.{3}$|…$/, "").trim()).filter(Boolean);
  return { verb: m[1]!, done: Number(m[2]), total: Number(m[3]), names };
}

/** A status line on cargo's stderr: `   Checking blaze-core v0.1.0 (/path)`, `   Downloaded serde v1`, `Blocking waiting for file lock`. */
export function cargoStatus(line: string): { verb: string; rest: string } | undefined {
  const m = /^\s*(Compiling|Checking|Documenting|Running|Doc-tests|Fresh|Finished|Blocking|Updating|Downloading|Downloaded|Locking|Adding|Waiting|Building|Executable)\s+(.*)$/.exec(line);
  return m ? { verb: m[1]!, rest: m[2]! } : undefined;
}

interface CargoSpan { file_name: string; line_start: number; column_start?: number; is_primary?: boolean }
/** The parts of cargo's JSON messages read here. */
export interface CargoMessage {
  reason?: string;
  success?: boolean;
  package_id?: string;
  target?: { name?: string; kind?: string[] };
  profile?: { test?: boolean };
  executable?: string | null;
  message?: { level?: string; message?: string; rendered?: string | null; code?: { code?: string } | null; spans?: CargoSpan[] };
}

/** rustc's closing words, which are not findings of their own. */
const RUSTC_TALLY = /^(?:aborting due to|\d+ warnings? emitted|\d+ errors?(?:;| emitted)|Some errors have detailed explanations|For more information about this error)/;

/** A compiler message as a finding (errors and warnings only), `at` against `root`, the workspace the spans are relative to. */
export function cargoFinding(m: CargoMessage, root: string): Finding | undefined {
  if (m.reason !== "compiler-message" || !m.message) return undefined;
  const { level = "", message = "", code, spans = [] } = m.message;
  if (level !== "error" && level !== "warning" && !/^error/.test(level)) return undefined;
  if (spans.length === 0 && RUSTC_TALLY.test(message)) return undefined;
  const span = spans.find(s => s.is_primary) ?? spans[0];
  const lint = code?.code && !/^E\d+$/.test(code.code) ? ` (${code.code})` : "";
  return {
    level: level.startsWith("error") ? "error" : "warning",
    ...(span ? { at: `${isAbsolute(span.file_name) ? span.file_name : join(root, span.file_name)}:${span.line_start}` } : {}),
    text: `${message}${lint}`,
  };
}

/** The package a cargo package id names: `path+file:///x/p#0.1.0` is p, `registry+https://...#serde@1.0.0` is serde. */
export function packageName(id: string): string {
  const [url = "", frag = ""] = id.split("#");
  if (frag.includes("@")) return frag.slice(0, frag.lastIndexOf("@"));
  if (/^\d/.test(frag) || !frag) return basename(url.replace(/[?#].*$/, ""));
  return frag;
}

/**
 * The workspace root cargo's spans are relative to: the nearest Cargo.toml above `cwd` with a [workspace], else the nearest one.
 * Not the highest: a git worktree inside its repository's checkout has the checkout's workspace above its own.
 */
export function cargoRoot(cwd: string): string {
  let nearest: string | undefined;
  for (let dir = cwd; ; dir = dirname(dir)) {
    const manifest = join(dir, "Cargo.toml");
    if (existsSync(manifest)) {
      nearest ??= dir;
      try { if (/^\s*\[workspace\]/m.test(readFileSync(manifest, "utf8"))) return dir; } catch { /* unreadable: not it */ }
    }
    if (dirname(dir) === dir) return nearest ?? cwd;
  }
}

/** What one line of libtest's stable text (`cargo test`'s test phase) says. */
export type TestEvent =
  | { kind: "running"; count: number }
  | { kind: "test"; name: string; result: "ok" | "failed" | "ignored" }
  | { kind: "result"; passed: number; failed: number; ignored: number }
  | { kind: "failure"; name: string; at?: string; message: string; detail: string };

/**
 * libtest's text, a line at a time: `running 12 tests`, `test a::b ... ok|FAILED|ignored`, the `---- a::b stdout ----` block of
 * each failure (its panic's location and message), and `test result:`. Stable cargo has no JSON for tests (that is `-Z
 * unstable-options --format json`), so this is what there is.
 */
export class TestText {
  private block: { name: string; lines: string[] } | undefined;
  feed(line: string): TestEvent[] {
    const events: TestEvent[] = [];
    const header = /^---- (.+?) std(?:out|err) ----$/.exec(line);
    if (this.block && (header || line === "failures:" || line.startsWith("test result:"))) events.push(this.close());
    if (header) { this.block = { name: header[1]!, lines: [] }; return events; }
    if (this.block) { this.block.lines.push(line); return events; }
    let m = /^running (\d+) tests?$/.exec(line);
    if (m) { events.push({ kind: "running", count: Number(m[1]) }); return events; }
    m = /^test (.+?) \.\.\. (ok|FAILED|ignored)\b/.exec(line);
    if (m) { events.push({ kind: "test", name: m[1]!, result: m[2] === "ok" ? "ok" : m[2] === "FAILED" ? "failed" : "ignored" }); return events; }
    m = /^test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored/.exec(line);
    if (m) events.push({ kind: "result", passed: Number(m[1]), failed: Number(m[2]), ignored: Number(m[3]) });
    return events;
  }
  end(): TestEvent[] { return this.block ? [this.close()] : []; }
  private close(): TestEvent {
    const { name } = this.block!;
    // A backtrace (RUST_BACKTRACE set) is the runtime's frames, not the failure: the block stops where it starts.
    const cut = this.block!.lines.findIndex(line => line === "stack backtrace:");
    const lines = cut >= 0 ? this.block!.lines.slice(0, cut) : this.block!.lines;
    this.block = undefined;
    while (lines.length && !lines.at(-1)!.trim()) lines.pop();
    while (lines.length && !lines[0]!.trim()) lines.shift();
    const detail = lines.filter(line => !/^note: run with `RUST_BACKTRACE=1`/.test(line)).join("\n").trimEnd();
    const at = lines.findIndex(line => /panicked at /.test(line));
    let message = "";
    let where: string | undefined;
    if (at >= 0) {
      const line = lines[at]!;
      // Rust 1.73 and later: `thread 'a' panicked at src/lib.rs:9:9:` and the message on the lines after; before: `panicked at 'msg', src/lib.rs:9:9`.
      const now = /panicked at ([^\s'][^:]*):(\d+):\d+:?$/.exec(line);
      const old = /panicked at '(.*)', ([^:]+):(\d+):\d+$/.exec(line);
      if (now) {
        where = `${now[1]}:${now[2]}`;
        message = lines.slice(at + 1).find(next => next.trim() && !/^note: /.test(next))?.trim() ?? "";
      } else if (old) {
        where = `${old[2]}:${old[3]}`;
        message = old[1]!;
      }
    }
    if (!message) message = lines.find(line => line.trim())?.trim() ?? "failed";
    return { kind: "failure", name, ...(where ? { at: where } : {}), message, detail };
  }
}

/** A cargo command's arguments: a string as you would type them, or a list of words. */
export type CargoArgs = string | readonly string[] | undefined;
export interface CargoOptions { cwd?: string; env?: Record<string, string>; timeout?: number; limit?: number }

/**
 * cargo's argv for `sub args`: `--message-format=json` and `flags` (`--keep-going`, `--no-fail-fast`) put before a `--`
 * (what follows it is the test binary's), each unless already given.
 */
export function cargoArgv(sub: string, args: CargoArgs, flags: readonly string[]): string[] {
  const list = args === undefined ? [] : typeof args === "string" ? words(args) : [...args];
  const split = list.indexOf("--");
  const ours = list.slice(0, split < 0 ? list.length : split);
  const add = [
    ...(ours.some(a => a.startsWith("--message-format")) ? [] : ["--message-format=json"]),
    ...flags.filter(f => !ours.includes(f)),
  ];
  return ["cargo", sub, ...ours, ...add, ...(split < 0 ? [] : list.slice(split))];
}

const CARGO_ENV = { CARGO_TERM_PROGRESS_WHEN: "always", CARGO_TERM_PROGRESS_WIDTH: "200", CARGO_TERM_COLOR: "never" };

/**
 * What a cargo build says as it goes: the step (cargo's bar and status lines), the diagnostics as findings, and the units it
 * planned, finished, failed and started, for `unchecked`.
 */
function cargoBuild(root: string, verb: string) {
  let current = "";
  const units = {
    planned: 0,
    finished: 0,
    /** Packages whose compile failed (`could not compile `p``), once per failed unit. */
    failed: [] as string[],
    /** Packages cargo said it started (`Checking p v0.1.0`), and those with an artifact. */
    started: new Set<string>(),
    built: new Set<string>(),
    /** Test executables built: their file name, and the name to say. */
    executables: new Map<string, string>(),
  };
  const step = (line: CargoMessage | string): Progress | undefined => {
    if (typeof line !== "string") {
      if (line.reason === "compiler-artifact" || line.reason === "build-script-executed") {
        units.finished++;
        if (line.package_id) units.built.add(packageName(line.package_id));
        if (line.reason === "compiler-artifact" && line.executable && line.profile?.test) {
          units.executables.set(basename(line.executable), line.target?.name ?? basename(line.executable));
        }
      }
      return undefined;
    }
    const failed = /could not compile `([^`]+)`/.exec(line);
    if (failed) units.failed.push(failed[1]!);
    const bar = cargoBar(line);
    if (bar) {
      if (bar.verb === "Downloading") return { label: `Downloading crates · ${bar.done}/${bar.total}`, done: bar.done, total: bar.total };
      units.planned = Math.max(units.planned, bar.total);
      // The first name is the unit running longest: the one being waited on.
      current = bar.names[0] ?? current;
      return { label: `${verb}${current ? ` ${current}` : ""} · ${bar.done}/${bar.total}`, done: bar.done, total: bar.total };
    }
    const status = cargoStatus(line);
    if (!status) return undefined;
    if (status.verb === "Compiling" || status.verb === "Checking" || status.verb === "Documenting") {
      const name = status.rest.split(/\s/, 1)[0]!;
      units.started.add(name);
      current = name;
      return { label: `${verb} ${name}` };
    }
    if (status.verb === "Blocking") return { label: "Waiting for the build directory lock" };
    if (status.verb === "Updating") return { label: "Updating the crate index" };
    if (status.verb === "Downloading" || status.verb === "Downloaded") return { label: "Downloading crates" };
    if (status.verb === "Locking" || status.verb === "Adding") return { label: "Resolving dependencies" };
    return undefined;
  };
  const find = (line: CargoMessage | string) => typeof line === "string" ? undefined : cargoFinding(line, root);
  /** What the build never reached: units counted from cargo's bar against its artifacts (cargo's JSON names no unit it skipped). */
  const unchecked = (state: FollowState): string | undefined => {
    const parts: string[] = [];
    const failedSet = [...new Set(units.failed)];
    const missing = units.planned - units.finished - units.failed.length;
    if (missing > 0) {
      const why = state.timedOut || state.exitCode === null ? "stopped before they ran"
        : failedSet.length ? `cargo skips what depends on ${failedSet.map(n => `\`${n}\``).join(", ")}; its output does not name them`
        : "cargo's output does not name them";
      parts.push(`${plural(missing, "unit")} of ${units.planned} never built (${why})`);
    } else if (failedSet.length && units.planned === 0) {
      parts.push(`what depends on ${failedSet.map(n => `\`${n}\``).join(", ")} was not built (cargo drew no progress, so how much is not known)`);
    }
    const unfinished = [...units.started].filter(n => !units.built.has(n) && !failedSet.includes(n));
    if (unfinished.length && (state.timedOut || state.exitCode === null)) parts.push(`started but unfinished: ${unfinished.slice(0, 10).join(", ")}${unfinished.length > 10 ? ", …" : ""}`);
    return parts.length ? parts.join("; ") : undefined;
  };
  return { step, find, unchecked, units };
}

function buildSummary(done: string) {
  return (state: FollowState): string => {
    const errors = state.counts.error ?? 0;
    const warnings = state.counts.warning ?? 0;
    const time = took(state.ms);
    if (errors || warnings) return `${done} · ${plural(errors, "error")} · ${plural(warnings, "warning")} in ${time}${!state.ok && errors === 0 && state.failure ? ` · ${state.failure}` : ""}`;
    if (state.ok) return `No errors or warnings in ${time}`;
    return `Failed in ${time}${state.failure ? ` · ${state.failure}` : ""}`;
  };
}

/** Why a cargo command failed: cargo's own `error:` lines (`could not compile ...`), else the default. */
function cargoFailure(state: FollowState & { stderr: readonly string[] }): string | undefined {
  const said = [...new Set(state.stderr.map(line => line.trim()).filter(line => /^error(?:\[\w+\])?:/.test(line)).map(line => line.replace(/^error:\s*/, "")))];
  if (!said.length) return undefined;
  return said.length <= 3 ? said.join("; ") : `${said.slice(0, 3).join("; ")}; and ${said.length - 3} more`;
}

const titleOf = (argv: readonly string[]) => argv.filter(a => a !== "--message-format=json").map(shq).join(" ");
const cargoEnv = (options: CargoOptions) => ({ ...CARGO_ENV, ...options.env });
const passOn = (options: CargoOptions) => ({
  ...(options.cwd ? { cwd: options.cwd } : {}),
  env: cargoEnv(options),
  ...(options.timeout ? { timeout: options.timeout } : {}),
  ...(options.limit ? { limit: options.limit } : {}),
});
const rootOf = (options: CargoOptions) => cargoRoot(options.cwd ? resolve(process.cwd(), options.cwd) : process.cwd());

function build(sub: string, verb: string, past: string) {
  return (args?: CargoArgs, options: CargoOptions = {}): Promise<Digest> => {
    const hooks = cargoBuild(rootOf(options), verb);
    const argv = cargoArgv(sub, args, ["--keep-going"]);
    return follow<CargoMessage>(argv, {
      title: titleOf(argv), json: true, step: hooks.step, find: hooks.find, unchecked: hooks.unchecked,
      summary: buildSummary(past), failure: cargoFailure, ...passOn(options),
    });
  };
}

function test(args?: CargoArgs, options: CargoOptions = {}): Promise<Digest> {
  const root = rootOf(options);
  const argv = cargoArgv("test", args, ["--no-fail-fast"]);
  const compile = cargoBuild(root, "Compiling");
  const text = new TestText();
  let binary = "";
  let done = 0;
  let of = 0;
  let testing = false;
  const failed: string[] = [];
  const reported = new Set<string>();
  /** Test executables cargo ran (file names), and the one running now with no result line yet. */
  const ran = new Set<string>();
  let open: string | undefined;
  const tests = (line: CargoMessage | string, context: FollowContext): { step?: Progress; found?: Finding[] } => {
    if (typeof line !== "string") return {};
    if (context.from === "stderr") {
      const run = /^\s*Running (?:unittests )?(\S+) \((?:.*[/\\])?(([^/\\]+?)(?:-[0-9a-f]{8,})?(?:\.exe)?)\)$/.exec(line);
      if (run) { ran.add(run[2]!); binary = run[3]!; open = binary; return { step: { label: `Testing ${binary}`, done: 0, total: 0 } }; }
      const doc = /^\s*Doc-tests (\S+)$/.exec(line);
      if (doc) { binary = `${doc[1]} doctests`; open = binary; return { step: { label: `Testing ${binary}`, done: 0, total: 0 } }; }
      return {};
    }
    const found: Finding[] = [];
    let step: Progress | undefined;
    for (const event of text.feed(line)) {
      if (event.kind === "running") { testing = true; done = 0; of = event.count; step = { label: `Testing ${binary || "tests"} · 0/${of}`, done: 0, total: of }; }
      else if (event.kind === "test") {
        done++;
        // A failure is counted by its finding, which its block (or the binary's result line) brings.
        if (event.result !== "failed") context.tally(event.result === "ok" ? "passed" : event.result);
        else failed.push(event.name);
        step = { label: `Testing ${binary || "tests"} · ${done}/${of}`, done, total: of };
      } else if (event.kind === "failure") {
        reported.add(event.name);
        found.push({ level: "failed", ...(event.at ? { at: isAbsolute(event.at) ? event.at : join(root, event.at) } : {}), text: `${event.name}: ${event.message}` });
      } else if (event.kind === "result") {
        open = undefined;
        // A failed test with no block of its own (run with --nocapture) is still a finding.
        for (const name of failed) if (!reported.has(name)) { reported.add(name); found.push({ level: "failed", text: name }); }
      }
    }
    return { ...(step ? { step } : {}), found };
  };
  // `step` sees each line first and `find` right after it: the line's failures wait here for `find`.
  let pending: Finding[] | undefined;
  return follow<CargoMessage>(argv, {
    title: titleOf(argv),
    json: true,
    step: (line, context) => {
      const read = tests(line, context);
      pending = read.found;
      const built = compile.step(line);
      return read.step ?? (testing ? undefined : built);
    },
    find: line => {
      const found = pending;
      pending = undefined;
      return typeof line === "string" ? found : compile.find(line);
    },
    summary: state => {
      const { passed = 0, failed: bad = 0, ignored = 0, error = 0 } = state.counts;
      if (!testing && error > 0) return `${plural(error, "error")} in ${took(state.ms)} · the tests did not run`;
      if (!testing && !state.ok) return `Failed in ${took(state.ms)}${state.failure ? ` · ${state.failure}` : ""}`;
      const parts = ["Tested", `${passed.toLocaleString("en-US")} passed`, ...(bad ? [`${bad} failed`] : []), ...(ignored ? [`${ignored} ignored`] : [])];
      return `${parts.join(" · ")} in ${took(state.ms)}${state.timedOut ? " · timed out" : ""}`;
    },
    failure: state => failed.length && !state.timedOut ? `${plural(failed.length, "test")} failed: ${failed.slice(0, 5).join(", ")}${failed.length > 5 ? ", …" : ""}` : cargoFailure(state),
    unchecked: state => {
      const parts: string[] = [];
      if (!testing && ran.size === 0 && (state.counts.error ?? 0) > 0) parts.push("a compile error stopped the tests: no test binary ran");
      const built = compile.unchecked(state);
      if (built) parts.push(built);
      if (open && (state.timedOut || state.exitCode === null)) parts.push(`stopped inside ${open}`);
      const never = [...compile.units.executables].filter(([file]) => !ran.has(file)).map(([, name]) => name);
      if (never.length && testing) parts.push(`built but never ran: ${never.slice(0, 10).join(", ")}${never.length > 10 ? ", …" : ""}`);
      return parts.length ? parts.join("; ") : undefined;
    },
    ...passOn(options),
  });
}

/** The `cargo` global: each command live for the person as one build step, a digest for the model. */
export const cargo = {
  check: build("check", "Checking", "Checked"),
  clippy: build("clippy", "Checking", "Checked"),
  build: build("build", "Compiling", "Built"),
  test,
};

/** For the tool description, in the style of functions/index.ts. */
export const followDocs = {
  follow:
    "follow(cmd: string | string[], { step?, find?, json?, total?, summary?, failure?, title?, cwd?, env?, timeout?, limit? = 20 }): Promise<{ ok; summary; counts; findings: { level; at?; text }[]; more?; failure?; log }> — run a command live as one build step and get a digest, never its output: each stdout/stderr line (\\r ends one; stdout JSON parsed with json) goes to step(line, { from, tally }) → label | { label, done, total } and find(line, ctx) → finding(s); log is the path of the full raw output.",
  cargo:
    "cargo.check | clippy | build | test(args?: string | string[], { cwd?, env?, timeout?, limit? }): Promise<digest & { unchecked? }> — cargo with --message-format=json and --keep-going (test: --no-fail-fast), live as one build step; findings are rustc's errors and warnings at file:line (relative to cwd) and, for test, each failed test at its panic, with passed/failed/ignored counted; unchecked says what a failure left unbuilt or unrun.",
} as const;
