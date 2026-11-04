// NUON-style printing (Nushell Object Notation): like JSON but quieter. Bare strings where safe,
// unquoted keys, and lists of records as tables with the header written once:
//   [[title, url]; ["Bun docs", "https://bun.com"], ["Wiki", "https://en.wikipedia.org"]]

export type NuonOptions = { maxRows?: number };

const BARE = /^[A-Za-z_][A-Za-z0-9_\-.]*$/;
const KEYWORDS = new Set(["true", "false", "null", "NaN", "Infinity"]);

const str = (s: string) => (BARE.test(s) && !KEYWORDS.has(s) ? s : JSON.stringify(s));
const isRecord = (v: unknown): v is Record<string, unknown> =>
  typeof v === "object" && v !== null && !Array.isArray(v) && !(v instanceof Date) && !(v instanceof Map) && !(v instanceof Set) && !(v instanceof Error);

export function nuon(value: unknown, opts: NuonOptions = {}): string {
  const seen = new WeakSet<object>();
  const { maxRows = Infinity } = opts;

  const go = (v: unknown, top: boolean): string => {
    switch (typeof v) {
      case "undefined": return "null";
      case "string": return str(v);
      case "number": return Number.isFinite(v) ? String(v) : String(v);
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
    seen.add(o);
    try {
      if (v instanceof Map) return go(Object.fromEntries(v), top);
      if (v instanceof Set) return go([...v], top);
      if (ArrayBuffer.isView(v)) return `<${v.constructor.name} ${v.byteLength} bytes>`;
      if (Array.isArray(v)) return list(v, top);
      const entries = Object.entries(v as object);
      return `{${entries.map(([k, x]) => `${str(k)}: ${go(x, false)}`).join(", ")}}`;
    } finally {
      seen.delete(o);
    }
  };

  const list = (arr: unknown[], top: boolean): string => {
    const shown = top ? arr.slice(0, maxRows) : arr;
    const more = arr.length - shown.length;
    const tail = more > 0 ? `\n# … ${more} more rows (${arr.length} total)` : "";
    if (shown.length > 0 && shown.every(isRecord)) {
      const cols: string[] = [];
      const known = new Set<string>();
      for (const r of shown) for (const k of Object.keys(r)) if (!known.has(k)) { known.add(k); cols.push(k); }
      const rows = shown.map((r) => `[${cols.map((c) => go(r[c], false)).join(", ")}]`);
      const sep = top ? ",\n " : ", ";
      return `[[${cols.map(str).join(", ")}];${top ? "\n " : " "}${rows.join(sep)}]${tail}`;
    }
    return `[${shown.map((x) => go(x, false)).join(", ")}]${tail}`;
  };

  return go(value, true);
}
