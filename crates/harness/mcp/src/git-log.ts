// gitLog(): a repository's history as a LazyTable, one row per commit with the files it changed as a list column. Ported
// from den's repl (den/repl/src/git-log.ts). It reads `git log --numstat` and is recorded as one "search" step
// ("Reading git log", then "Read git log · 42 commits"), not as the git process it runs.
import { resolve } from "node:path";
import { effect, quiet } from "./effects.ts";
import { scanTable, type LazyTable } from "./table.ts";

/** One commit. */
export interface CommitRow {
  hash: string;
  author: string;
  email: string;
  /** Author date, ms since the epoch. */
  date: number;
  subject: string;
  /** The paths it changed, relative to the repository root (a rename as git writes it, `a => b`). */
  files: string[];
  /** Lines added and deleted in all, binary files counting none. */
  added: number;
  deleted: number;
}
export interface GitLogOptions {
  /** The repository (default: the working directory). */
  cwd?: string;
  /** What to walk from: a branch, a tag, a range (`main..HEAD`). Default HEAD. */
  ref?: string;
  /** Only commits that touched these paths (git pathspecs). */
  path?: string | string[];
  /** git's dates: `'2 weeks ago'`, `'2026-10-01'`. */
  since?: string;
  until?: string;
  /** Commits whose author matches (git's regex). */
  author?: string;
  /** Merge commits too. Default false. */
  merges?: boolean;
  /** At most this many commits (git's --max-count); a `.head(n)` right after does the same. */
  limit?: number;
}
const KNOWN = ["cwd", "ref", "path", "since", "until", "author", "merges", "limit"];
const HINTS: Record<string, string> = { n: "limit", maxCount: "limit", paths: "path", branch: "ref", dir: "cwd" };

const RECORD = "\u001e";
const FIELD = "\u001f";

/** The rows of `git log --format=<RECORD>%H<FIELD>%an<FIELD>%ae<FIELD>%at<FIELD>%s --numstat` output. */
export function parseGitLog(output: string): CommitRow[] {
  const rows: CommitRow[] = [];
  for (const record of output.split(RECORD)) {
    if (!record.trim()) continue;
    const [header = "", ...stats] = record.split("\n");
    const [hash = "", author = "", email = "", seconds = "0", subject = ""] = header.split(FIELD);
    const row: CommitRow = { hash, author, email, date: Number(seconds) * 1000, subject, files: [], added: 0, deleted: 0 };
    for (const line of stats) {
      const match = /^(\d+|-)\t(\d+|-)\t(.+)$/.exec(line);
      if (!match) continue;
      row.added += match[1] === "-" ? 0 : Number(match[1]);
      row.deleted += match[2] === "-" ? 0 : Number(match[2]);
      row.files.push(match[3]!);
    }
    rows.push(row);
  }
  return rows;
}

function checkOptions(options: unknown): asserts options is GitLogOptions {
  if (typeof options !== "object" || options === null || Array.isArray(options)) throw new TypeError("gitLog(options) takes an options object");
  for (const name of Object.keys(options)) {
    if (!KNOWN.includes(name)) throw new TypeError(`gitLog has no option ${JSON.stringify(name)}${HINTS[name] ? `: use ${HINTS[name]}` : ""}. Its options: ${KNOWN.join(", ")}.`);
  }
  const { limit } = options as GitLogOptions;
  if (limit !== undefined && (!Number.isInteger(limit) || limit < 0)) throw new RangeError(`gitLog's limit is a whole number at least 0, not ${JSON.stringify(limit)}`);
}

/** The commits of a repository, newest first, as a LazyTable: verbs chain now, git runs on await. */
export function gitLog(options: GitLogOptions = {}): LazyTable<CommitRow> {
  checkOptions(options);
  const opts = { ...options };
  return scanTable<CommitRow>({
    describe: `gitLog(${Object.keys(opts).length ? JSON.stringify(opts) : ""})`,
    columns: ["hash", "author", "email", "date", "subject", "files", "added", "deleted"],
    pushdownLimit: true,
    scan: (hints) => {
      const cwd = resolve(opts.cwd ?? ".");
      const limits = [opts.limit, hints.limit].filter((n): n is number => n !== undefined);
      const paths = opts.path === undefined ? [] : Array.isArray(opts.path) ? opts.path : [opts.path];
      const args = [
        "git", "-C", cwd, "--no-pager", "log", "--no-color", "--no-ext-diff", `--format=${RECORD}%H${FIELD}%an${FIELD}%ae${FIELD}%at${FIELD}%s`, "--numstat",
        ...(opts.merges ? [] : ["--no-merges"]),
        ...(limits.length ? [`--max-count=${Math.min(...limits)}`] : []),
        ...(opts.since ? [`--since=${opts.since}`] : []),
        ...(opts.until ? [`--until=${opts.until}`] : []),
        ...(opts.author ? [`--author=${opts.author}`] : []),
        ...(opts.ref ? [opts.ref] : []),
        "--", ...paths,
      ];
      return effect("search", "Reading git log", "Read git log", cwd, async () => {
        const proc = quiet(() => Bun.spawn(args, { stdin: "ignore", stdout: "pipe", stderr: "pipe" }));
        const abort = () => proc.kill();
        hints.signal?.addEventListener("abort", abort, { once: true });
        try {
          const [out, err, code] = await Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text(), proc.exited]);
          if (code !== 0) throw new Error(`gitLog: git log in ${cwd} failed (exit ${code}): ${err.trim() || "no message"}`);
          return parseGitLog(out);
        } finally {
          hints.signal?.removeEventListener("abort", abort);
        }
      }, (rows) => ({ detail: `${rows.length} commit${rows.length === 1 ? "" : "s"}` }));
    },
  });
}
