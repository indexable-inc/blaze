// A value as the app shows it: the typed shape the chat's tool card draws (a table as a grid, a record as key/value
// rows, text as text), next to the NUON text the model reads, which stays as it is. Bounded: at most MAX_ROWS rows,
// MAX_CELL chars a cell and about `budget` bytes in all; `total` always says how many rows there were.
import { nuon } from "./nuon.ts";

/** One cell: a scalar, or a nested value written as NUON (its column then says `nested`). */
export type Cell = string | number | boolean | null;
export type ColType = "string" | "number" | "bool" | "url" | "date" | "nested" | "null" | "mixed";
export type Col = { name: string; type: ColType };
/** A grid: a list of records (`table`, header shown), a record (`record`, key/value rows), a list of scalars (`list`). */
export type Grid = { t: "table" | "record" | "list"; cols: Col[]; rows: Cell[][]; total: number };
export type Shown =
  | Grid
  | { t: "scalar"; type: ColType; v: Cell }
  | { t: "text"; v: string; total: number }
  | { t: "error"; message: string; stack: string };

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
  return [clip(nuon(v), MAX_CELL), "nested"];
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

/** How the app shows `v`, in about `budget` bytes. */
export function shown(v: unknown, budget = 64 << 10): Shown {
  if (v instanceof Error) return { t: "error", message: v.message, stack: clip(v.stack ?? "", 8 << 10) };
  if (typeof v === "string") return { t: "text", v: clip(v, budget), total: v.length };
  if (v instanceof Map) v = Object.fromEntries(v);
  if (v instanceof Set) v = [...v];
  if (Array.isArray(v)) {
    if (v.length > 0 && v.every(isRecord)) {
      const names: string[] = [];
      const known = new Set<string>();
      for (const r of v.slice(0, MAX_ROWS)) for (const k of Object.keys(r)) if (!known.has(k)) { known.add(k); names.push(k); }
      return grid("table", names, v.map((r) => names.map((k) => (r as Record<string, unknown>)[k])), budget);
    }
    return grid("list", ["value"], v.map((x) => [x]), budget);
  }
  if (isRecord(v)) return grid("record", ["key", "value"], Object.entries(v), budget);
  const [c, type] = cell(v);
  return { t: "scalar", type, v: c };
}
