// NUON-style printing (Nushell Object Notation): like JSON but quieter. Bare strings where safe,
// unquoted keys, and lists of records as tables with the header written once:
//   [[title, url]; ["Bun docs", "https://bun.com"], ["Wiki", "https://en.wikipedia.org"]]
import type { Docs } from "./docs.ts";

/** `maxRows`: rows of the top-level list. `maxItems`: items of a list inside it. `maxString`: characters of a string inside
 *  it (a top-level string is never cut here). `maxDepth`: levels of nesting. What a limit cut is counted in `cut` when given. */
export type NuonOptions = { maxRows?: number; maxItems?: number; maxString?: number; maxDepth?: number; cut?: NuonCut };
export type NuonCut = { rows: number; total: number; items: number; strings: number; depth: number };
export const noCut = (): NuonCut => ({ rows: 0, total: 0, items: 0, strings: 0, depth: 0 });

const BARE = /^[A-Za-z_][A-Za-z0-9_\-.]*$/;
const KEYWORDS = new Set(["true", "false", "null", "NaN", "Infinity"]);

const str = (s: string) => (BARE.test(s) && !KEYWORDS.has(s) ? s : JSON.stringify(s));
const isRecord = (v: unknown): v is Record<string, unknown> =>
  typeof v === "object" && v !== null && !Array.isArray(v) && !(v instanceof Date) && !(v instanceof Map) && !(v instanceof Set) && !(v instanceof Error);

export function nuon(value: unknown, opts: NuonOptions = {}): string {
  const seen = new WeakSet<object>();
  const { maxRows = Infinity, maxItems = Infinity, maxString = Infinity, maxDepth = Infinity, cut = noCut() } = opts;
  let depth = 0;

  const go = (v: unknown, top: boolean): string => {
    switch (typeof v) {
      case "undefined": return "null";
      case "string":
        if (top || v.length <= maxString) return str(v);
        cut.strings++;
        return JSON.stringify(`${v.slice(0, maxString)}… +${v.length - maxString} chars`);
      case "number": return String(v);
      case "bigint": return v.toString();
      case "boolean": return String(v);
      case "symbol": return str(v.toString());
      case "function": return `<function ${v.name || "anonymous"}>`;
    }
    if (v === null) return "null";
    if (v instanceof Date) return Number.isNaN(v.getTime()) ? "null" : v.toISOString();
    if (v instanceof Error) return `<${v.name}: ${v.message}>`;
    const o = v as object;
    if (seen.has(o)) return "<cycle>";
    if (depth >= maxDepth) { cut.depth++; return Array.isArray(v) ? `<list of ${v.length}>` : `<${o.constructor?.name ?? "record"}>`; }
    seen.add(o);
    depth++;
    try {
      if (v instanceof Map) return go(Object.fromEntries(v), top);
      if (v instanceof Set) return go([...v], top);
      if (ArrayBuffer.isView(v)) return `<${v.constructor.name} ${v.byteLength} bytes>`;
      if (Array.isArray(v)) return list(v, top);
      const entries = Object.entries(v as object);
      return `{${entries.map(([k, x]) => `${str(k)}: ${go(x, false)}`).join(", ")}}`;
    } finally {
      depth--;
      seen.delete(o);
    }
  };

  const list = (arr: unknown[], top: boolean): string => {
    const shown = arr.slice(0, top ? maxRows : maxItems);
    const more = arr.length - shown.length;
    if (more > 0 && top) { cut.rows = more; cut.total = arr.length; }
    if (more > 0 && !top) cut.items++;
    const tail = more <= 0 ? "" : top ? `\n# … ${more} more rows (${arr.length} total)` : "";
    const rest = more > 0 && !top ? `, … +${more}` : "";
    if (shown.length > 0 && shown.every(isRecord)) {
      const cols: string[] = [];
      const known = new Set<string>();
      for (const r of shown) for (const k of Object.keys(r)) if (!known.has(k)) { known.add(k); cols.push(k); }
      const rows = shown.map((r) => `[${cols.map((c) => go(r[c], false)).join(", ")}]`);
      const sep = top ? ",\n " : ", ";
      return `[[${cols.map(str).join(", ")}];${top ? "\n " : " "}${rows.join(sep)}${rest}]${tail}`;
    }
    return `[${shown.map((x) => go(x, false)).join(", ")}${rest}]${tail}`;
  };

  return go(value, true);
}

/** What help() says about this module's calls (docs.ts). */
export const docs: Docs = {
  nuon: {
    area: "cells",
    sig: "nuon(value, { maxRows?, maxItems?, maxString?, maxDepth? }): string",
    about: "format any value the way results print: NUON, i.e. JSON with bare strings and keys, and lists of records as [[col, ...]; [v, ...], ...].",
  },
};
