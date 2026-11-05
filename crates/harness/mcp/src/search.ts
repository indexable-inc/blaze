// `rg`, `ls`, `glob` and `du` in a cell: what an agent would otherwise shell out for and then parse, as typed data
// (ported from den's repl/src/search.ts). `rg` runs ripgrep with `--json` and reads its event stream; `glob` and `du`
// walk with `rg --files`; `ls` reads the filesystem here (or walks with ripgrep with `gitignore`). None is an "exec"
// step: each records one step of what it did ("search" or "list", effects.ts), its ripgrep runs inside `quiet`.
//
// Results are plain arrays of records (so they print as NUON tables, rows first), with their extras (`files`, `count`,
// `total`, `truncated`, ...) as non-enumerable properties code reads and printing leaves out.
import type { Stats } from "node:fs";
import { lstat, readdir, stat } from "node:fs/promises";
import { basename, extname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { effect, oneLine, quiet } from "./effects.ts";
import { table } from "./table.ts";

/** Where a match is: UTF-16 offsets into `text`, which is the whole line (several lines for a multiline match). */
export type RgSubmatch = { start: number; end: number; text: string };
export type RgMatch = {
  /** Absolute. */
  path: string;
  /** 1-based; the first line of a multiline match. */
  line: number;
  /** 1-based, in UTF-16 units: where the first submatch starts. */
  column: number;
  /** The matching line without its line ending. */
  text: string;
  /** Context lines before the match, nearest last; only with `context`. */
  before?: string[];
  /** Context lines after the match; only with `context`. */
  after?: string[];
  submatches: RgSubmatch[];
};
export type RgOptions = {
  /** Files or directories to search, relative to the working directory (default: the working directory). */
  path?: string | string[];
  /** ripgrep `--glob`s: `'*.ts'`, `'!dist'`. */
  glob?: string | string[];
  /** ripgrep file types (`'ts'`, `'rust'`; `rg --type-list`). */
  type?: string | string[];
  ignoreCase?: boolean;
  /** Ignore case unless the pattern has an uppercase letter. */
  smartCase?: boolean;
  /** The pattern is literal text, not a regex. */
  fixed?: boolean;
  /** Whole words only. */
  word?: boolean;
  /** The pattern may match across lines. */
  multiline?: boolean;
  /** Context lines around each match: one number for both sides, or `{ before, after }`. */
  context?: number | { before?: number; after?: number };
  /** At most this many matches per file. */
  maxCount?: number;
  /** Search hidden files and directories. */
  hidden?: boolean;
  /** Search what .gitignore and friends exclude. */
  noIgnore?: boolean;
  /** One match per file: for when only `.files` matters. */
  filesOnly?: boolean;
  /** At most this many matches in all (default 10,000); `truncated` then says so. */
  limit?: number;
  /** Aborting kills ripgrep and rejects with the signal's reason. */
  signal?: AbortSignal;
};
/** The matches, ordered by file. */
export type RgResults = RgMatch[] & {
  /** The files with a match, each once. */
  readonly files: string[];
  readonly count: number;
  /** The search stopped at `limit`; there are more. */
  readonly truncated: boolean;
  /** What ripgrep said on stderr (files it could not read) while still finding results. */
  readonly warnings: string[];
};

export type LsEntry = {
  name: string;
  /** Absolute. */
  path: string;
  kind: "file" | "dir" | "symlink";
  /** Bytes. */
  size: number;
  /** Last modification, ms since the epoch. */
  modified: number;
  /** The extension without its dot; '' for directories and files without one. */
  ext: string;
};
export type LsOptions = {
  /** The whole tree below. */
  recursive?: boolean;
  /** Levels to list (1 is the directory's own entries); implies `recursive` above 1. */
  depth?: number;
  /** Include dotfiles. */
  hidden?: boolean;
  /** Keep entries whose path relative to the listed directory matches (Bun.Glob syntax). */
  glob?: string | string[];
  /** Leave out what .gitignore and friends exclude (ripgrep's walk); symlinks and directories with no unignored file are not listed. */
  gitignore?: boolean;
  /** At most this many entries (default 10,000); `truncated` then says so. */
  limit?: number;
  signal?: AbortSignal;
};
export type LsResults = LsEntry[] & { readonly path: string; readonly truncated: boolean };

export type GlobOptions = {
  /** What a relative pattern is relative to (default: the working directory). */
  cwd?: string;
  /** Match dotfiles and dot-directories; `.git` itself only with `gitignore: false`. */
  dot?: boolean;
  /** Files only (the default); `false` matches directories as well. */
  onlyFiles?: boolean;
  /** Directories only (found through the files below them). */
  onlyDirs?: boolean;
  /** Leave out what .gitignore, .ignore and .git/info/exclude do (default true). */
  gitignore?: boolean;
  /** More to leave out, gitignore-style, relative to where the walk starts. */
  ignore?: string | string[];
  /** Match and walk symlinks. */
  followSymlinks?: boolean;
  /** Rows `{ path, size, mtime, isDir, ext }` as a table instead of paths. */
  stat?: boolean;
  /** 'path' (default, ascending), 'size' (largest first) or 'mtime' (newest first); the last two see every match before `limit`. */
  sort?: "path" | "size" | "mtime";
  /** At most this many (default 10,000); `truncated` then says so. */
  limit?: number;
  signal?: AbortSignal;
};
/** A row of `glob(pattern, { stat: true })`. */
export type GlobEntry = {
  /** Absolute. */
  path: string;
  /** Bytes. */
  size: number;
  /** Last modification, ms since the epoch. */
  mtime: number;
  isDir: boolean;
  /** The extension without its dot; '' for directories and files without one. */
  ext: string;
};
/** Absolute paths; `truncated` says `limit` cut them short. */
export type GlobResults = string[] & { readonly truncated: boolean };
export type GlobTable = ReturnType<typeof table<GlobEntry>>;

export type DuOptions = {
  /** Levels below the directory to report (default 1: its own entries, each with the total below it). */
  depth?: number;
  /** Leave out what .gitignore excludes (default false: disk use counts target and node_modules). */
  gitignore?: boolean;
  /** Count dotfiles and dot-directories (default true). */
  dot?: boolean;
  /** At most this many entries, largest first (default 10,000). */
  limit?: number;
  signal?: AbortSignal;
};
export type DuEntry = {
  /** Absolute. */
  path: string;
  /** Bytes: the files' own sizes, summed. */
  size: number;
  /** Files counted in `size`. */
  files: number;
  isDir: boolean;
};
/** Largest first; `total` and `files` count the whole directory, whatever `limit` cut. */
export type DuResults = DuEntry[] & { readonly path: string; readonly total: number; readonly files: number; readonly truncated: boolean };

const DEFAULT_LIMIT = 10_000;
const SORT_CAP = 1_000_000;
const MAX_STDERR = 64 << 10;
const STAT_BATCH = 256;
/** Rows a step's shown value carries (shown.ts bounds it again by bytes). */
const SHOWN_ROWS = 200;

/** The ripgrep binary: HARNESS_RG, else `rg` on PATH; an error naming both when neither is there. */
function rgBinary(): string {
  const bin = process.env.HARNESS_RG ?? Bun.which("rg");
  if (!bin) throw new Error("ripgrep (rg) was not found on PATH: install it, or set HARNESS_RG to its path");
  return bin;
}

const natural = (name: string, value: unknown, min = 0): number | undefined => {
  if (value === undefined) return undefined;
  if (typeof value !== "number" || !Number.isInteger(value) || value < min) throw new RangeError(`${name} takes an integer of at least ${min}, got ${String(value)}`);
  return value;
};
const strings = (name: string, value: unknown): string[] => {
  if (value === undefined) return [];
  if (typeof value === "string") return [value];
  if (Array.isArray(value) && value.every((x) => typeof x === "string")) return value;
  throw new TypeError(`${name} takes a string or an array of strings`);
};

/** Every option each helper has, and what to write for a name it lacks that agents reach for. */
const OPTIONS = {
  rg: {
    known: ["path", "glob", "type", "ignoreCase", "smartCase", "fixed", "word", "multiline", "context", "maxCount", "hidden", "noIgnore", "filesOnly", "limit", "signal"],
    hints: { dot: "hidden", cwd: "path", paths: "path", literal: "fixed", caseInsensitive: "ignoreCase", gitignore: "noIgnore (inverted)", max: "limit" },
  },
  ls: {
    known: ["recursive", "depth", "hidden", "glob", "gitignore", "limit", "signal"],
    hints: { dot: "hidden", deep: "depth", maxDepth: "depth", pattern: "glob", max: "limit" },
  },
  glob: {
    known: ["cwd", "dot", "onlyFiles", "onlyDirs", "gitignore", "ignore", "followSymlinks", "stat", "sort", "limit", "signal"],
    hints: {
      hidden: "dot", dirs: "onlyFiles: false", onlyDirectories: "onlyDirs", followSymbolicLinks: "followSymlinks", exclude: "ignore",
      absolute: "nothing: paths are always absolute", deep: "a pattern such as 'src/*/*.ts'", max: "limit", stats: "stat",
    },
  },
  du: {
    known: ["depth", "gitignore", "dot", "limit", "signal"],
    hints: { maxDepth: "depth", hidden: "dot", max: "limit", human: "nothing: sizes are bytes" },
  },
} satisfies Record<string, { known: string[]; hints: Record<string, string> }>;

/** The options object of `name`, refusing a key it does not have (a wrong name would otherwise do nothing). */
function optionsOf<T extends object>(name: keyof typeof OPTIONS, value: T | undefined): T {
  if (value === undefined) return {} as T;
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new TypeError(`${name} takes an options object`);
  const { known, hints } = OPTIONS[name];
  for (const key of Object.keys(value)) {
    if (known.includes(key)) continue;
    const hint = (hints as Record<string, string>)[key];
    throw new TypeError(`${name} has no option ${JSON.stringify(key)}${hint ? `: use ${hint}` : ""}. Its options: ${known.join(", ")}.`);
  }
  return value;
}

const clip = (text: string, max: number) => (text.length > max ? `${text.slice(0, max - 1)}…` : text);
/** A path as short as the working directory allows. */
const rel = (path: string) => {
  const inside = relative(process.cwd(), path);
  return inside === "" ? "." : !inside.startsWith("..") && !isAbsolute(inside) ? inside : path;
};
const plural = (n: number, noun: string, many = `${noun}s`) => `${n} ${n === 1 ? noun : many}`;
/** Bytes for a step's detail: "812 B", "3.2 KB", "2.1 GB". */
function human(n: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = n;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) { v /= 1024; u++; }
  return u ? `${v.toFixed(1)} ${units[u]}` : `${v} B`;
}

/** `rows` with `props` as non-enumerable fields: code reads them, NUON and JSON print only the rows. */
function annotate<T extends object, P extends Record<string, unknown>>(rows: T, props: P): T & Readonly<P> {
  const d: PropertyDescriptorMap = {};
  for (const [k, v] of Object.entries(props)) d[k] = { value: v };
  return Object.defineProperties(rows, d) as T & Readonly<P>;
}

type Run = { code: number; stderr: string; stopped: boolean };
/**
 * Run ripgrep with `args`, `onLine` hearing each stdout line (returning true stops it). Spawned inside `quiet`, so it is
 * not an "exec" step of its own; an aborted `signal` kills it and rejects with the signal's reason.
 */
async function runRg(args: string[], signal: AbortSignal | undefined, onLine: (line: string) => boolean | void): Promise<Run> {
  signal?.throwIfAborted();
  const bin = rgBinary();
  let child: Bun.Subprocess<"ignore", "pipe", "pipe">;
  try {
    child = quiet(() => Bun.spawn([bin, ...args], { cwd: process.cwd(), stdin: "ignore", stdout: "pipe", stderr: "pipe" }));
  } catch (cause) {
    throw new Error(`ripgrep at ${bin} could not start: ${cause instanceof Error ? cause.message : String(cause)}; install it, or set HARNESS_RG to its path`, { cause });
  }
  const kill = () => { try { child.kill(); } catch {} };
  signal?.addEventListener("abort", kill, { once: true });
  let stderr = "";
  const errDone = (async () => {
    const dec = new TextDecoder();
    for await (const chunk of child.stderr) if (stderr.length < MAX_STDERR) stderr += dec.decode(chunk, { stream: true });
  })();
  let stopped = false;
  let failure: unknown;
  try {
    const dec = new TextDecoder();
    let tail = "";
    const reader = child.stdout.getReader();
    const feed = (text: string): boolean => {
      const lines = (tail + text).split("\n");
      tail = lines.pop() ?? "";
      for (const line of lines) if (line && onLine(line)) return true;
      return false;
    };
    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) { if (feed(dec.decode() + "\n")) stopped = true; break; }
        if (feed(dec.decode(value, { stream: true }))) { stopped = true; break; }
      }
    } catch (e) {
      failure = e;
    }
    if (stopped || failure !== undefined) { kill(); await reader.cancel().catch(() => {}); }
    const code = await child.exited;
    await errDone.catch(() => {});
    signal?.throwIfAborted();
    if (failure !== undefined) throw failure;
    return { code, stderr, stopped };
  } finally {
    signal?.removeEventListener("abort", kill);
    kill();
  }
}

const rgFailed = (run: Run) => new Error(`rg failed (exit ${run.code}): ${run.stderr.trim() || "no message"}`);

type Field = { text?: string; bytes?: string };
type RgData = { path?: Field; lines?: Field; line_number?: number; submatches?: { start: number; end: number }[] };
/** A ripgrep JSON string field: UTF-8 text, or base64 bytes when it is not valid UTF-8. */
const bytesOf = (f: Field | undefined): Buffer => (f?.text !== undefined ? Buffer.from(f.text) : Buffer.from(f?.bytes ?? "", "base64"));
/** The UTF-16 offset of the UTF-8 byte offset `at` in `bytes`. */
const unitsAt = (bytes: Buffer, at: number) => bytes.subarray(0, at).toString("utf8").length;

/**
 * Reads `rg --json` events into matches. A context line right after a match belongs to it (up to `after` in a row);
 * every other context line is before the next match, which keeps the last `before`.
 */
class RgParser {
  readonly matches: RgMatch[] = [];
  /** A `summary` event arrived: ripgrep got as far as searching (a bad pattern ends it before). */
  summarized = false;
  private readonly cwd = process.cwd();
  private path = "";
  private before: string[] = [];
  private tail: { match: RgMatch; next: number } | undefined;
  constructor(private readonly context: { before: number; after: number }) {}

  push(line: string): void {
    let event: { type?: string; data?: RgData };
    try { event = JSON.parse(line) as typeof event; } catch { throw new Error(`rg --json printed something that is not JSON: ${clip(line, 200)}`); }
    const data = event.data ?? {};
    if (event.type === "summary") { this.summarized = true; return; }
    if (event.type === "begin") {
      this.path = resolve(this.cwd, bytesOf(data.path).toString("utf8"));
      this.before = [];
      this.tail = undefined;
      return;
    }
    if (event.type !== "match" && event.type !== "context") return;
    const raw = bytesOf(data.lines);
    const text = raw.toString("utf8").replace(/\r?\n$/, "");
    const n = data.line_number ?? 0;
    if (event.type === "context") {
      const { tail } = this;
      if (tail && n === tail.next && (tail.match.after?.length ?? 0) < this.context.after) {
        (tail.match.after ??= []).push(text);
        tail.next++;
      } else {
        this.before.push(text);
        if (this.before.length > this.context.before) this.before.shift();
      }
      return;
    }
    const submatches = (data.submatches ?? []).map(({ start, end }) => ({ start: unitsAt(raw, start), end: unitsAt(raw, end), text: raw.subarray(start, end).toString("utf8") }));
    const match: RgMatch = { path: this.path, line: n, column: (submatches[0]?.start ?? 0) + 1, text, submatches };
    if (this.before.length) match.before = this.before.splice(0);
    this.matches.push(match);
    this.tail = { match, next: n + text.split("\n").length };
  }
}

function contextOf(value: RgOptions["context"]): { before: number; after: number } {
  if (value === undefined) return { before: 0, after: 0 };
  if (typeof value === "number") { const both = natural("context", value)!; return { before: both, after: both }; }
  if (typeof value === "object" && value !== null) return { before: natural("context.before", value.before) ?? 0, after: natural("context.after", value.after) ?? 0 };
  throw new TypeError("context takes a number of lines, or { before, after }");
}

function rgArgs(pattern: string, o: RgOptions, paths: string[]): string[] {
  const { before, after } = o.filesOnly ? { before: 0, after: 0 } : contextOf(o.context);
  const maxCount = o.filesOnly ? 1 : natural("maxCount", o.maxCount, 1);
  return [
    "--json", "--no-config",
    ...(o.ignoreCase ? ["-i"] : o.smartCase ? ["-S"] : []),
    ...(o.fixed ? ["-F"] : []),
    ...(o.word ? ["-w"] : []),
    ...(o.multiline ? ["-U"] : []),
    ...(o.hidden ? ["--hidden"] : []),
    ...(o.noIgnore ? ["--no-ignore"] : []),
    ...(before ? ["-B", String(before)] : []),
    ...(after ? ["-A", String(after)] : []),
    ...(maxCount ? ["-m", String(maxCount)] : []),
    ...strings("type", o.type).flatMap((t) => ["-t", t]),
    ...strings("glob", o.glob).flatMap((g) => ["-g", g]),
    "-e", pattern, "--", ...paths,
  ];
}

/** Structured ripgrep: `await rg('TODO', { path: 'src', type: 'ts', context: 2 })`, with `.files`, `.count`, `.truncated`, `.warnings`. */
export async function rg(pattern: string, input?: RgOptions): Promise<RgResults> {
  if (typeof pattern !== "string") throw new TypeError("rg(pattern, options) takes the pattern as a string: a regex, or literal text with { fixed: true }");
  const o = optionsOf("rg", input);
  const limit = natural("limit", o.limit, 1) ?? DEFAULT_LIMIT;
  const roots = strings("path", o.path).map((p) => resolve(p));
  const paths = roots.length ? roots : [process.cwd()];
  const args = rgArgs(pattern, o, paths);
  const what = oneLine(pattern, 60);
  return effect("search", `Searching for ${what}`, `Searched for ${what}`, paths.map(rel).join(", "), async () => {
    const parser = new RgParser(o.filesOnly ? { before: 0, after: 0 } : contextOf(o.context));
    let truncated = false;
    const run = await runRg(args, o.signal, (line) => {
      parser.push(line);
      if (parser.matches.length <= limit) return false;
      parser.matches.pop();
      truncated = true;
      return true;
    });
    // Exit 1 is "no matches"; 2 after a summary only met files it could not read.
    if (!run.stopped && run.code !== 0 && run.code !== 1 && !(run.code === 2 && parser.summarized)) throw rgFailed(run);
    const matches = parser.matches.sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : a.line - b.line));
    const files = [...new Set(matches.map((m) => m.path))];
    return annotate(matches, { files, count: matches.length, truncated, warnings: run.stderr.split("\n").filter(Boolean) });
  }, (r) => ({
    detail: r.length ? `${plural(r.length, "hit")} in ${plural(r.files.length, "file")}${r.truncated ? ", truncated" : ""}` : "no hits",
    show: r.slice(0, SHOWN_ROWS).map((m) => ({ path: rel(m.path), line: m.line, text: clip(m.text, 200) })),
  }));
}

/** Directories before files at each level, names by code unit. */
type Item = { segs: string[]; dir: boolean };
const order = (a: Item, b: Item): number => {
  const shared = Math.min(a.segs.length, b.segs.length);
  for (let level = 0; level < shared; level++) {
    const l = a.segs[level]!;
    const r = b.segs[level]!;
    if (l === r) continue;
    const lDir = level < a.segs.length - 1 || a.dir;
    const rDir = level < b.segs.length - 1 || b.dir;
    if (lDir !== rDir) return lDir ? -1 : 1;
    return l < r ? -1 : 1;
  }
  return a.segs.length - b.segs.length;
};

const entryOf = (path: string, s: Stats): LsEntry => {
  const kind = s.isSymbolicLink() ? "symlink" : s.isDirectory() ? "dir" : "file";
  const name = basename(path);
  return { name, path, kind, size: s.size, modified: Math.round(s.mtimeMs), ext: kind === "file" ? extname(name).slice(1) : "" };
};

/** lstat (or stat, following links) of each path, a bounded number at a time; undefined for one that vanished. */
async function statAll(paths: string[], signal: AbortSignal | undefined, follow = false): Promise<(Stats | undefined)[]> {
  const out: (Stats | undefined)[] = [];
  const read = follow ? stat : lstat;
  for (let at = 0; at < paths.length; at += STAT_BATCH) {
    signal?.throwIfAborted();
    out.push(...(await Promise.all(paths.slice(at, at + STAT_BATCH).map((p) => read(p).catch(() => undefined)))));
  }
  return out;
}

type Found = Item & { entry: LsEntry };
type Walk = { maxDepth: number; hidden: boolean; accept: (relative: string) => boolean; limit: number; signal: AbortSignal | undefined };

async function walk(root: string, { maxDepth, hidden, accept, limit, signal }: Walk): Promise<{ found: Found[]; truncated: boolean }> {
  const found: Found[] = [];
  let truncated = false;
  const visit = async (dir: string, prefix: string[]): Promise<void> => {
    signal?.throwIfAborted();
    // A directory below the root that cannot be read is skipped; the root's own error is the caller's.
    const names = (await readdir(dir).catch((e: unknown): string[] => { if (dir === root) throw e; return []; })).filter((n) => hidden || !n.startsWith("."));
    const stats = await statAll(names.map((n) => join(dir, n)), signal);
    const below: [string, string[]][] = [];
    for (const [i, name] of names.entries()) {
      const info = stats[i];
      if (!info) continue;
      const path = join(dir, name);
      const segs = [...prefix, name];
      const entry = entryOf(path, info);
      if (accept(segs.join("/"))) {
        if (found.length >= limit) { truncated = true; return; }
        found.push({ segs, dir: entry.kind === "dir", entry });
      }
      if (entry.kind === "dir" && segs.length < maxDepth) below.push([path, segs]);
    }
    for (const [path, segs] of below) {
      if (truncated) return;
      await visit(path, segs);
    }
  };
  await visit(root, []);
  return { found, truncated };
}

/** The tree below `root` as ripgrep's file walk sees it, with the directories those files sit in. */
async function walkIgnoring(root: string, { maxDepth, hidden, accept, limit, signal }: Walk): Promise<{ found: Found[]; truncated: boolean }> {
  const files: string[] = [];
  const run = await runRg(["--files", "--no-config", ...(hidden ? ["--hidden", "-g", "!.git"] : []), "--", root], signal, (line) => { files.push(line); });
  if (run.code !== 0 && run.code !== 1 && !(run.code === 2 && files.length)) throw rgFailed(run);
  const items = new Map<string, Item>();
  for (const file of files) {
    const segs = relative(root, file).split(sep);
    for (let level = 1; level < segs.length && level <= maxDepth; level++) {
      const dir = segs.slice(0, level);
      items.set(dir.join("/"), { segs: dir, dir: true });
    }
    if (segs.length <= maxDepth) items.set(segs.join("/"), { segs, dir: false });
  }
  const wanted = [...items].filter(([p]) => accept(p)).map(([, item]) => item).sort(order);
  const truncated = wanted.length > limit;
  const kept = wanted.slice(0, limit);
  const paths = kept.map((item) => join(root, ...item.segs));
  const stats = await statAll(paths, signal);
  const found: Found[] = [];
  for (const [i, item] of kept.entries()) {
    const info = stats[i];
    if (info) found.push({ ...item, entry: entryOf(paths[i]!, info) });
  }
  return { found, truncated };
}

/** A directory as data: `await ls('src', { recursive: true, gitignore: true })`, directories first at each level. */
export async function ls(target?: string, input?: LsOptions): Promise<LsResults> {
  if (target !== undefined && typeof target !== "string") throw new TypeError("ls(path, options) takes the directory as a string");
  const o = optionsOf("ls", input);
  const root = resolve(target ?? ".");
  const depth = natural("depth", o.depth, 1);
  const limit = natural("limit", o.limit, 1) ?? DEFAULT_LIMIT;
  const matchers = strings("glob", o.glob).map((p) => new Bun.Glob(p));
  const name = rel(root);
  return effect("list", `Listing ${name}`, `Listed ${name}`, name, async () => {
    const info = await stat(root);
    if (!info.isDirectory()) return annotate([entryOf(root, await lstat(root))], { path: root, truncated: false });
    const settings: Walk = {
      maxDepth: depth ?? (o.recursive ? Infinity : 1), hidden: o.hidden === true, limit, signal: o.signal,
      accept: (r) => !matchers.length || matchers.some((m) => m.match(r)),
    };
    const { found, truncated } = o.gitignore ? await walkIgnoring(root, settings) : await walk(root, settings);
    return annotate(found.sort(order).map((f) => f.entry), { path: root, truncated });
  }, (r) => ({
    detail: `${plural(r.length, "entry", "entries")}${r.truncated ? ", truncated" : ""}`,
    show: r.slice(0, SHOWN_ROWS).map((e) => ({ path: relative(root, e.path) || e.name, kind: e.kind, size: e.size })),
  }));
}

/** Characters that make a pattern segment a pattern rather than a name (Bun.Glob's syntax). */
const MAGIC = /[*?[\]{}!]/;
/** Where a walk for `pattern` starts (its leading plain segments) and the rest, matched below it. */
function globBase(pattern: string, cwd: string): { base: string; rest: string; depth: number | undefined } {
  const segments = pattern.replace(/\/+$/, "").split("/");
  let at = segments.findIndex((s) => MAGIC.test(s));
  // A pattern that names one path: walk its directory for its last segment.
  if (at < 0) at = segments.length - 1;
  const lead = segments.slice(0, at).join("/");
  const base = lead === "" && isAbsolute(pattern) ? "/" : resolve(cwd, lead);
  const rest = segments.slice(at);
  return { base, rest: rest.join("/"), depth: rest.includes("**") ? undefined : rest.length };
}

/** `glob`'s walk: ripgrep's file list under the pattern's base, matched against the rest; directories found through their files. */
async function walkGlob(pattern: string, o: GlobOptions, cap: number): Promise<{ found: { path: string; isDir: boolean }[]; truncated: boolean }> {
  const wantFiles = o.onlyDirs !== true;
  const wantDirs = o.onlyDirs === true || o.onlyFiles === false;
  const gitignore = o.gitignore !== false;
  const { base, rest, depth } = globBase(pattern, o.cwd === undefined ? process.cwd() : resolve(o.cwd));
  const matcher = new Bun.Glob(rest);
  const files: string[] = [];
  const dirs = new Set<string>();
  let truncated = false;
  if (await stat(base).then((i) => i.isDirectory(), () => false)) {
    const args = [
      "--files", "--no-config",
      ...(gitignore ? [] : ["--no-ignore"]),
      ...(o.dot ? ["--hidden", ...(gitignore ? ["-g", "!.git"] : [])] : []),
      ...(o.followSymlinks ? ["--follow"] : []),
      // Directories are found through the files below them, so only a files-only walk may stop at the pattern's depth.
      ...(depth !== undefined && !wantDirs ? ["--max-depth", String(depth)] : []),
      ...strings("ignore", o.ignore).flatMap((i) => ["-g", `!${i}`]),
      "--", base,
    ];
    const run = await runRg(args, o.signal, (line) => {
      const inside = relative(base, line).split(sep);
      if (wantDirs) {
        for (let level = 1; level < inside.length; level++) {
          const dir = inside.slice(0, level).join("/");
          if (!dirs.has(dir) && matcher.match(dir)) dirs.add(dir);
        }
      }
      if (wantFiles && matcher.match(inside.join("/"))) files.push(line);
      if (files.length + dirs.size <= cap) return false;
      truncated = true;
      return true;
    });
    if (!run.stopped && run.code !== 0 && run.code !== 1 && !(run.code === 2 && (files.length || dirs.size))) throw rgFailed(run);
  }
  const found = [...files.map((path) => ({ path, isDir: false })), ...[...dirs].map((d) => ({ path: join(base, d), isDir: true }))].slice(0, cap);
  return { found, truncated };
}

const byPath = (a: { path: string }, b: { path: string }) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0);

async function globRun(pattern: string, input: GlobOptions | undefined): Promise<GlobResults | GlobTable> {
  if (typeof pattern !== "string") throw new TypeError("glob(pattern, options) takes the pattern as a string");
  const o = optionsOf("glob", input);
  const limit = natural("limit", o.limit, 1);
  const sort = o.sort ?? "path";
  if (!["path", "size", "mtime"].includes(sort)) throw new TypeError(`glob's sort is 'path', 'size' or 'mtime', not ${JSON.stringify(sort)}`);
  if (o.onlyDirs && o.onlyFiles) throw new TypeError("glob takes onlyFiles or onlyDirs, not both");
  const noun = o.onlyDirs ? ["dir", "dirs"] : o.onlyFiles === false ? ["path", "paths"] : ["file", "files"];
  const what = oneLine(pattern, 60);
  // What the step shows, kept here: with `stat` the result is a table, whose shape table.ts owns.
  let found: string[] = [];
  let cut = false;
  return effect("list", `Listing ${what}`, `Listed ${what}`, pattern, async (): Promise<GlobResults | GlobTable> => {
    const kept = limit ?? DEFAULT_LIMIT;
    const walked = await walkGlob(pattern, o, sort === "path" ? kept : SORT_CAP);
    let truncated = walked.truncated;
    const need = o.stat || sort !== "path";
    const infos = need ? await statAll(walked.found.map((f) => f.path), o.signal, o.followSymlinks) : [];
    let rows: GlobEntry[] = walked.found.map((f, i) => {
      const info = infos[i];
      return { path: f.path, size: info?.size ?? 0, mtime: info ? Math.round(info.mtimeMs) : 0, isDir: f.isDir, ext: f.isDir ? "" : extname(basename(f.path)).slice(1) };
    });
    rows.sort(sort === "size" ? (a, b) => b.size - a.size || byPath(a, b) : sort === "mtime" ? (a, b) => b.mtime - a.mtime || byPath(a, b) : byPath);
    if (rows.length > kept) { rows = rows.slice(0, kept); truncated = true; }
    found = rows.map((r) => r.path);
    cut = truncated;
    if (o.stat) return annotate(table(rows), { truncated });
    return annotate(found.slice(), { truncated });
  }, () => ({
    detail: `${plural(found.length, noun[0]!, noun[1])}${cut ? ", truncated" : ""}`,
    show: found.slice(0, SHOWN_ROWS).map(rel),
  }));
}

/** Paths by pattern, sorted: `await glob('src/**\/*.ts')`; with `{ stat: true }` a table of `{ path, size, mtime, isDir, ext }`. */
export function glob(pattern: string, options: GlobOptions & { stat: true }): Promise<GlobTable & { readonly truncated: boolean }>;
export function glob(pattern: string, options?: GlobOptions & { stat?: false }): Promise<GlobResults>;
export function glob(pattern: string, options?: GlobOptions): Promise<GlobResults | (GlobTable & { readonly truncated: boolean })>;
export function glob(pattern: string, options?: GlobOptions): Promise<GlobResults | (GlobTable & { readonly truncated: boolean })> {
  return globRun(pattern, options) as Promise<GlobResults | (GlobTable & { readonly truncated: boolean })>;
}

/** Disk use, largest first: `await du('target', { depth: 2 })`, with `.total` and `.files`. */
export async function du(target?: string, input?: DuOptions): Promise<DuResults> {
  if (target !== undefined && typeof target !== "string") throw new TypeError("du(path, options) takes the directory as a string");
  const o = optionsOf("du", input);
  const depth = natural("depth", o.depth, 1) ?? 1;
  const limit = natural("limit", o.limit, 1) ?? DEFAULT_LIMIT;
  const root = resolve(target ?? ".");
  const name = rel(root);
  return effect("list", `Measuring ${name}`, `Measured ${name}`, name, async () => {
    if (!(await stat(root)).isDirectory()) throw new TypeError(`du takes a directory; for one file's size use (await Bun.file(${JSON.stringify(target)}).stat()).size`);
    const files: string[] = [];
    const args = ["--files", "--no-config", ...(o.gitignore ? [] : ["--no-ignore"]), ...(o.dot === false ? [] : ["--hidden"]), "--", root];
    const run = await runRg(args, o.signal, (line) => { files.push(line); });
    if (run.code !== 0 && run.code !== 1 && !(run.code === 2 && files.length)) throw rgFailed(run);
    const stats = await statAll(files, o.signal);
    const buckets = new Map<string, DuEntry>();
    let total = 0;
    let counted = 0;
    for (const [i, file] of files.entries()) {
      const size = stats[i]?.size;
      if (size === undefined) continue;
      total += size;
      counted++;
      const segs = relative(root, file).split(sep);
      const path = join(root, ...segs.slice(0, depth));
      const entry = buckets.get(path) ?? { path, size: 0, files: 0, isDir: segs.length > depth };
      entry.size += size;
      entry.files++;
      buckets.set(path, entry);
    }
    const sorted = [...buckets.values()].sort((a, b) => b.size - a.size || byPath(a, b));
    return annotate(sorted.slice(0, limit), { path: root, total, files: counted, truncated: sorted.length > limit });
  }, (r) => ({
    detail: human(r.total),
    show: r.slice(0, SHOWN_ROWS).map((e) => ({ path: rel(e.path) + (e.isDir ? "/" : ""), size: human(e.size), files: e.files })),
  }));
}

/** One line each, for the MCP tool description (functions/index.ts). */
export const searchDocs = {
  rg: "rg(pattern, { path?, glob?, type?, ignoreCase?, smartCase?, fixed?, word?, multiline?, context?: n | { before, after }, maxCount?, hidden?, noIgnore?, filesOnly?, limit? = 10000, signal? }): Promise<{ path; line; column; text; before?; after?; submatches }[] & { files; count; truncated; warnings }> — ripgrep as data, sorted by path, recorded as a search step (not exec): `(await rg('fn main', { path: 'crates', type: 'rust', context: 2 })).files`.",
  ls: "ls(path = '.', { recursive?, depth?, hidden?, glob?, gitignore?, limit? = 10000, signal? }): Promise<{ name; path; kind: file|dir|symlink; size; modified; ext }[] & { path; truncated }> — a directory as rows, directories first: `await ls('src', { recursive: true, gitignore: true })`.",
  glob: "glob(pattern, { cwd?, dot?, onlyFiles? = true, onlyDirs?, gitignore? = true, ignore?, followSymlinks?, stat?, sort?: 'path'|'size'|'mtime', limit? = 10000, signal? }): Promise<string[] & { truncated }> — sorted absolute paths via ripgrep's walk; stat: true gives table({ path, size, mtime, isDir, ext }); the five largest: `await glob('**/*', { stat: true, sort: 'size', limit: 5 })`.",
  du: "du(path = '.', { depth? = 1, gitignore? = false, dot? = true, limit?, signal? }): Promise<{ path; size; files; isDir }[] & { path; total; files; truncated }> — disk use per entry, largest first, bytes: `(await du('target', { depth: 2 })).total`.",
};
