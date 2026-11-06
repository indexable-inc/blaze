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
  /** A page of the harness browser (cdp.ts `Page`): the app draws it as a live card of view `view` (the genui Browser
   *  element, compact), its title and host under it; pressing it opens the view in the browser pane. */
  | { t: "browser"; view: string; url: string; title: string };

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

/** How the app shows `v`, in about `budget` bytes; a table of `kind` (what the step that gave it knows it is) says so. */
export function shown(v: unknown, budget = 64 << 10, kind?: ShownKind): Shown {
  if (showsItself(v)) return v[SHOWN]();
  if (v instanceof Error) return { t: "error", message: v.message, stack: clip(v.stack ?? "", 8 << 10) };
  if (typeof v === "string") return { t: "text", v: clip(v, budget), total: v.length };
  if (v instanceof Map) v = Object.fromEntries(v);
  if (v instanceof Set) v = [...v];
  if (Array.isArray(v)) {
    if (v.length > 0 && v.every(isRecord)) {
      const names: string[] = [];
      const known = new Set<string>();
      for (const r of v.slice(0, MAX_ROWS)) for (const k of Object.keys(r)) if (!known.has(k)) { known.add(k); names.push(k); }
      const g = grid("table", names, v.map((r) => names.map((k) => (r as Record<string, unknown>)[k])), budget);
      return kind ? { ...g, kind } : g;
    }
    return grid("list", ["value"], v.map((x) => [x]), budget);
  }
  if (isRecord(v)) return grid("record", ["key", "value"], Object.entries(v), budget);
  const [c, type] = cell(v);
  return { t: "scalar", type, v: c };
}
