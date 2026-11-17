// How much of a value the agent is shown, as one rule: a value is printed whole when it fits its budget, and summarized
// with the call that gives more when it does not. Never dumped: a reply is read by a model that pays for every byte.
// Pure (no clock, no IO): the REPL's results, console lines, notices and `more()` all go through here.
//
// The rule, for a cell's value (`present`):
//   1. a string prints raw; over the budget, its head and tail with `more(id, { chars: [from, to] })`;
//   2. anything else prints as NUON (nuon.ts) with at most ROWS top-level rows, and nested lists, strings and depth
//      bounded; over the budget the bounds are tightened step by step, and last the value is cut at the budget;
//   3. whatever was left out is said in a `#` line naming how to get it: rows, fields, or a path into the value.
import { nuon, noCut, type NuonCut } from "./nuon.ts";

/** Bytes of one value in a reply. */
export const BUDGET = 16 << 10;
/** Top-level rows of a list in a reply. */
export const ROWS = 100;
/** Bytes of a cell's console lines in a reply. */
export const LOG_BUDGET = 8 << 10;

/** `s` cut to `n` characters, the last an ellipsis. */
export const clip = (s: string, n: number): string => (s.length > n ? `${s.slice(0, n - 1)}…` : s);
/** `s` as one line of at most `n` characters. */
export const oneLine = (s: string, n = 80): string => clip(s.replace(/\s+/g, " ").trim(), n);

/** The bounds tried in order: each is tighter than the one before. */
const STEPS = [
  { rows: ROWS, maxItems: 20, maxString: 400, maxDepth: 6 },
  { rows: 30, maxItems: 8, maxString: 120, maxDepth: 4 },
  { rows: 10, maxItems: 3, maxString: 60, maxDepth: 3 },
  { rows: 3, maxItems: 2, maxString: 30, maxDepth: 2 },
] as const;

export type Presented = { text: string; /** Something was left out (the text says what and how to get it). */ cut: boolean };

/** `value` as the agent reads it. `id` is the task whose value it is (`t3`), named in the line saying how to get more. */
export function present(value: unknown, o: { budget?: number; rows?: number; id?: string } = {}): Presented {
  const budget = o.budget ?? BUDGET;
  const from = o.id ? `more('${o.id}', ` : "more(id, ";
  if (value === undefined) return { text: "undefined", cut: false };
  if (typeof value === "string") {
    if (value.length <= budget) return { text: value, cut: false };
    const head = Math.floor(budget * 0.75), tail = budget - head;
    return {
      text: `${value.slice(0, head)}\n# … ${value.length - budget} characters left out (${value.length} in all): ${from}{ chars: [${head}, ${head + budget}] })\n${value.slice(-tail)}`,
      cut: true,
    };
  }
  let text = "";
  let cut: NuonCut = noCut();
  for (const step of STEPS) {
    cut = noCut();
    text = nuon(value, { maxRows: Math.min(step.rows, o.rows ?? ROWS), maxItems: step.maxItems, maxString: step.maxString, maxDepth: step.maxDepth, cut });
    if (text.length <= budget) break;
  }
  const notes: string[] = [];
  if (text.length > budget) {
    notes.push(`# cut at ${budget} of ${text.length} characters: ask for a part with ${from}{ path: 'a.b[0]' }) or ${from}{ fields: [...] })`);
    text = text.slice(0, budget);
  }
  // nuon says how many rows it left out; this adds the call that gives them.
  if (cut.rows > 0) {
    const shown = cut.total - cut.rows;
    text = text.replace(/\n# … \d+ more rows \(\d+ total\)$/, "");
    notes.push(`# … ${cut.rows} more rows (${cut.total} total): ${from}{ rows: [${shown}, ${Math.min(cut.total, shown * 2)}] })`);
  }
  if (cut.items + cut.strings + cut.depth > 0) {
    const what = [cut.items && "long lists", cut.strings && "long strings", cut.depth && "deep values"].filter(Boolean).join(", ");
    notes.push(`# ${what} inside it are shortened: ${from}{ path: 'field[0].name' }) gives one whole`);
  }
  return { text: notes.length ? `${text}\n${notes.join("\n")}` : text, cut: notes.length > 0 };
}

/** A cell's console lines for a reply: all of them within the budget, else the first few and the last that fit. */
export function presentLogs(logs: readonly string[], o: { budget?: number; id?: string } = {}): string {
  const budget = o.budget ?? LOG_BUDGET;
  let size = 0;
  for (const l of logs) size += l.length + 1;
  if (size <= budget) return logs.join("\n");
  const head: string[] = [], tail: string[] = [];
  let used = 0;
  for (const l of logs.slice(0, 5)) { const c = clip(l, 400); head.push(c); used += c.length + 1; }
  for (let i = logs.length - 1; i >= head.length; i--) {
    const c = clip(logs[i]!, 2000);
    if (used + c.length + 1 > budget) break;
    tail.unshift(c);
    used += c.length + 1;
  }
  const left = logs.length - head.length - tail.length;
  const where = o.id ? `task('${o.id}').logs.slice(${head.length}, ${head.length + left})` : "task(id).logs";
  return [...head, `# … ${left} log lines left out (${logs.length} in all): ${where}`, ...tail].join("\n");
}

/** Which part of a value `more()` gives: a path into it, then a row range (or character range of a string), then fields of each row. */
export type Part = { path?: string; rows?: [number, number]; chars?: [number, number]; fields?: string[] };

const STEP = /^(?:\.?([A-Za-z_$][\w$]*)|\[(\d+)\]|\[(?:"([^"]*)"|'([^']*)')\])/;

/** The part of `value` that `part` names. Throws naming the step of the path that leads nowhere and what is there instead. */
export function select(value: unknown, part: Part): unknown {
  const known = ["path", "rows", "chars", "fields"];
  const unknown = Object.keys(part).filter((k) => !known.includes(k));
  if (unknown.length) throw new TypeError(`more: unknown option ${unknown.join(", ")}; it takes { path: 'a.b[0]', rows: [from, to], chars: [from, to], fields: ['name'] }`);
  let v = value;
  let rest = part.path ?? "";
  let walked = "";
  while (rest) {
    const m = STEP.exec(rest);
    if (!m) throw new TypeError(`more: cannot read the path at ${JSON.stringify(rest)}: write it as a.b[0]['key with spaces']`);
    const key = m[1] ?? m[3] ?? m[4] ?? Number(m[2]);
    if (v === null || typeof v !== "object") throw new TypeError(`more: ${walked || "the value"} is ${v === null ? "null" : `a ${typeof v}`}, so it has no ${JSON.stringify(key)}`);
    const next = v instanceof Map ? v.get(key) : (v as Record<string | number, unknown>)[key];
    if (next === undefined) {
      const has = Array.isArray(v) ? `a list of ${v.length}` : `{${oneLine((v instanceof Map ? [...v.keys()] : Object.keys(v)).join(", "), 200)}}`;
      throw new TypeError(`more: ${walked || "the value"} has no ${JSON.stringify(key)}: it is ${has}`);
    }
    v = next;
    walked += m[0];
    rest = rest.slice(m[0].length);
  }
  if (part.chars) {
    if (typeof v !== "string") throw new TypeError(`more: chars is for a string; ${walked || "the value"} is ${Array.isArray(v) ? "a list (use rows)" : `a ${typeof v}`}`);
    v = v.slice(part.chars[0], part.chars[1]);
  }
  if (part.rows) {
    if (!Array.isArray(v)) throw new TypeError(`more: rows is for a list; ${walked || "the value"} is ${typeof v === "string" ? "a string (use chars)" : `a ${typeof v}`}`);
    v = v.slice(part.rows[0], part.rows[1]);
  }
  if (part.fields) {
    const pick = (r: unknown) => (r !== null && typeof r === "object" ? Object.fromEntries(part.fields!.map((f) => [f, (r as Record<string, unknown>)[f]])) : r);
    v = Array.isArray(v) ? v.map(pick) : pick(v);
  }
  return v;
}
