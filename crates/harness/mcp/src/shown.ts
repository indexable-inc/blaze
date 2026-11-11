// A value as the app shows it: the typed shape the chat's tool card draws (a table as a grid, a record as key/value
// rows, text as text), next to the NUON text the model reads, which stays as it is. Bounded: at most MAX_ROWS rows,
// MAX_CELL chars a cell and about `budget` bytes in all; `total` always says how many rows there were.
import { nuon } from "./nuon.ts";

/** One cell: a scalar, or a nested value written as NUON (its column then says `nested`). */
export type Cell = string | number | boolean | null;
export type ColType = "string" | "number" | "bool" | "url" | "date" | "nested" | "null" | "mixed";
export type Col = { name: string; type: ColType };
/** A grid: a list of records (`table`, header shown), a record (`record`, key/value rows), a list of scalars (`list`).
 * `kind` says what the rows are when the app has a shape of its own for them (`search`: results with a title, url, site
 * and snippet, drawn as a result list). */
export type Grid = { t: "table" | "record" | "list"; cols: Col[]; rows: Cell[][]; total: number; kind?: ShownKind };
/** What a value is, for the app's own shapes (see Grid). */
export type ShownKind = "search";
export type Shown =
  | Grid
  | { t: "scalar"; type: ColType; v: Cell }
  | { t: "text"; v: string; total: number }
  | { t: "error"; message: string; stack: string }
  /** A page of the harness browser (cdp.ts `Page`). The transcript draws nothing for it; with `pane` (a page opened for
   *  the person, browser.open) its step is a line whose link shows view `view` in the browser pane when pressed. */
  | { t: "browser"; view: string; url: string; title: string; pane?: true };

/** A value that says how it is shown: `[SHOWN]()` gives its Shown (a browser Page gives its card). */
export const SHOWN: unique symbol = Symbol.for("harness.shown");
type ShowsItself = { [SHOWN](): Shown };
const showsItself = (v: unknown): v is ShowsItself =>
  typeof v === "object" && v !== null && typeof (v as Partial<ShowsItself>)[SHOWN] === "function";

export const MAX_ROWS = 500;
const MAX_CELL = 400;

const isRecord = (v: unknown): v is Record<string, unknown> =>
  typeof v === "object" && v !== null && !Array.isArray(v) && !(v instanceof Date) && !(v instanceof Map) && !(v instanceof Set) && !(v instanceof Error) && !ArrayBuffer.isView(v);
const clip = (s: string, n: number) => (s.length > n ? s.slice(0, n - 1) + "…" : s);

function cell(v: unknown): [Cell, ColType] {
  if (v === null || v === undefined) return [null, "null"];
  switch (typeof v) {
    case "number": return [Number.isFinite(v) ? v : String(v), "number"];
    case "bigint": return [v.toString(), "number"];
    case "boolean": return [v, "bool"];
    case "string": return [clip(v, MAX_CELL), /^https?:\/\/\S+$/.test(v) ? "url" : "string"];
  }
  if (v instanceof Date) return [Number.isNaN(v.getTime()) ? null : v.toISOString(), "date"];
  // An error in a cell says what went wrong in a sentence, not its class and stack.
  if (v instanceof Error) return [clip(brief(v), MAX_CELL), "string"];
  // One line: a cell is drawn on one line, and a nested table prints its rows on lines of their own.
  return [clip(nuon(v).replace(/\s*\n\s*/g, " "), MAX_CELL), "nested"];
}

/** The column type of cells of these types: the one they share (nulls aside), else `mixed`. */
function unify(types: ColType[]): ColType {
  const seen = new Set(types.filter((t) => t !== "null"));
  if (seen.size === 0) return "null";
  if (seen.size === 1) return [...seen][0]!;
  if ([...seen].every((t) => t === "url" || t === "string")) return "string";
  return "mixed";
}

/** Rows into a grid, stopping at MAX_ROWS or once the cells pass `budget` bytes. */
function grid(t: Grid["t"], names: string[], items: unknown[][], budget: number): Grid {
  const rows: Cell[][] = [];
  const types: ColType[][] = names.map(() => []);
  let bytes = 0;
  for (const item of items.slice(0, MAX_ROWS)) {
    const row = item.map((v, i) => { const [c, ty] = cell(v); types[i]!.push(ty); return c; });
    bytes += JSON.stringify(row).length;
    if (bytes > budget && rows.length > 0) break;
    rows.push(row);
  }
  return { t, cols: names.map((name, i) => ({ name, type: unify(types[i]!) })), rows, total: items.length };
}

/** An error as one plain sentence for a person ("No browser is connected"): its own `brief` when it has one, else the
 *  first sentence of its message's first line, without the error's class name. The model still reads the whole stack. */
export function brief(e: unknown): string {
  const own = (e as { brief?: unknown } | null)?.brief;
  if (typeof own === "string" && own.trim()) return own.trim();
  const m = e instanceof Error ? e.message : String(e);
  const line = m.split("\n").map((l) => l.trim()).find((l) => l) ?? "";
  const plain = line.replace(/^[A-Z]\w*(Error|Exception|Unavailable)?:\s+/, "");
  const end = plain.search(/\.\s/);
  return clip((end > 0 ? plain.slice(0, end) : plain).replace(/\.$/, ""), 160);
}

/** A search result as any search gives it: a record with a string title and an http(s) url. */
const isHit = (r: unknown): boolean =>
  isRecord(r) && typeof r.title === "string" && typeof r.url === "string" && /^https?:\/\//.test(r.url);
const isHits = (v: unknown): v is Record<string, unknown>[] => Array.isArray(v) && v.length > 0 && v.every(isHit);
const hitsOrNone = (x: unknown) => isHits(x) || (Array.isArray(x) && x.length === 0);

/** Search results grouped under what found them, flattened to one result table with a `query` column, or null:
 *  records that each hold one list of results (`[{q, results}]`, the query their first string field), or a record of
 *  result lists (`{web: [...], mail: [...]}`, the query its key). */
function grouped(v: unknown): Record<string, unknown>[] | null {
  let out: Record<string, unknown>[] = [];
  if (Array.isArray(v) && v.length > 0 && v.every(isRecord)) {
    for (const r of v as Record<string, unknown>[]) {
      const lists = Object.entries(r).filter(([, x]) => hitsOrNone(x));
      if (lists.length !== 1) return null;
      const [key, hits] = lists[0]!;
      const label = Object.entries(r).find(([k, x]) => k !== key && typeof x === "string")?.[1] ?? "";
      for (const h of hits as Record<string, unknown>[]) out.push({ ...h, query: label });
    }
  } else if (isRecord(v)) {
    const entries = Object.entries(v);
    if (entries.length === 0 || !entries.every(([, x]) => hitsOrNone(x))) return null;
    out = entries.flatMap(([k, x]) => (x as Record<string, unknown>[]).map((h) => ({ ...h, query: k })));
  }
  return out.length > 0 ? out : null;
}

/** Promise.allSettled's results as what they settled to: each value, or its reason. */
function settled(v: unknown[]): unknown[] | null {
  const ok = v.length > 0 && v.every((r) => isRecord(r) && ((r.status === "fulfilled" && "value" in r) || (r.status === "rejected" && "reason" in r)));
  return ok ? v.map((r) => { const s = r as { status: string; value?: unknown; reason?: unknown }; return s.status === "fulfilled" ? s.value : s.reason; }) : null;
}

/** How the app shows `v`, in about `budget` bytes; a table of `kind` (what the step that gave it knows it is) says so.
 *  Search results are known by their shape wherever they come from (a list of {title, url}, or such lists grouped under
 *  their queries), so the app draws them as results, never as a table with a JSON array in a cell; Promise.allSettled's
 *  results show as what they settled to. */
export function shown(v: unknown, budget = 64 << 10, kind?: ShownKind): Shown {
  if (showsItself(v)) return v[SHOWN]();
  if (v instanceof Error) return { t: "error", message: brief(v), stack: clip(v.stack ?? "", 8 << 10) };
  if (typeof v === "string") return { t: "text", v: clip(v, budget), total: v.length };
  if (v instanceof Map) v = Object.fromEntries(v);
  if (v instanceof Set) v = [...v];
  if (Array.isArray(v)) {
    const s = settled(v);
    if (s) return shown(s.length === 1 ? s[0] : s, budget, kind);
    const g = kind ? null : grouped(v);
    if (g) return shown(g, budget, "search");
    if (!kind && isHits(v)) kind = "search";
    if (v.length > 0 && v.every(isRecord)) {
      const names: string[] = [];
      const known = new Set<string>();
      for (const r of v.slice(0, MAX_ROWS)) for (const k of Object.keys(r)) if (!known.has(k)) { known.add(k); names.push(k); }
      const g = grid("table", names, v.map((r) => names.map((k) => (r as Record<string, unknown>)[k])), budget);
      return kind ? { ...g, kind } : g;
    }
    return grid("list", ["value"], v.map((x) => [x]), budget);
  }
  if (isRecord(v)) {
    const g = kind ? null : grouped(v);
    if (g) return shown(g, budget, "search");
    return grid("record", ["key", "value"], Object.entries(v), budget);
  }
  const [c, type] = cell(v);
  return { t: "scalar", type, v: c };
}
