// Discovery on demand (pure: docs in, text out). `help()` lists the areas with one line each, `help('files')` an area's
// calls, `help('cargo')` or `help('cargo.test')` a call's signature, example and errors, `help.search('words')` finds a
// call by what it does; an error about a name nobody has says the nearest one and points here (`hint`).
import { AREAS, type Area, type Doc, type Docs } from "./docs.ts";
import { oneLine } from "./disclose.ts";

const HOW = "help('area') lists an area's calls; help('name') gives a call's signature, an example and what it throws; help.search('words') finds a call by what it does.";

/** Edit distance, for the nearest name. */
export function distance(a: string, b: string): number {
  let row = Array.from({ length: b.length + 1 }, (_, i) => i);
  for (let i = 1; i <= a.length; i++) {
    const next = [i];
    for (let j = 1; j <= b.length; j++) next[j] = Math.min(row[j]! + 1, next[j - 1]! + 1, row[j - 1]! + (a[i - 1] === b[j - 1] ? 0 : 1));
    row = next;
  }
  return row[b.length]!;
}

/** The names closest to `word`, nearest first: within a third of its length in edits (case aside), or holding it whole. At most `n`. */
export function nearest(word: string, names: Iterable<string>, n = 3): string[] {
  const w = word.toLowerCase();
  const found: [number, string][] = [];
  for (const name of new Set(names)) {
    const l = name.toLowerCase();
    if (l === w && name !== word) { found.push([0, name]); continue; }
    const d = distance(w, l);
    if (d <= Math.max(1, Math.floor(w.length / 3))) found.push([d, name]);
    else if (w.length >= 3 && (l.includes(w) || (l.length >= 3 && w.includes(l)))) found.push([w.length, name]);
  }
  return found.sort((a, b) => a[0] - b[0] || a[1].localeCompare(b[1])).slice(0, n).map(([, name]) => name);
}

/** The name an engine or TypeScript message says does not exist, if it says one. */
export function missingName(message: string): string | undefined {
  return (/^(?:Can't find variable: )?([\w$.]+) is not defined/.exec(message)
    ?? /^Can't find variable: ([\w$]+)/.exec(message)
    ?? /^([\w$.]+) is not a function/.exec(message)
    ?? /Cannot find name '([\w$]+)'/.exec(message)
    ?? /Property '([\w$]+)' does not exist/.exec(message))?.[1];
}

/** For an error about a name that is not there: the nearest names there are, and where to look. Null when the message names none. */
export function hint(message: string, names: Iterable<string>): string | null {
  const missing = missingName(message);
  if (!missing) return null;
  const all = [...names];
  const last = missing.split(".").at(-1)!;
  const near = [...new Set([...nearest(missing, all), ...nearest(last, all.map((n) => n.split(".").at(-1)!).filter((n) => n !== last))])]
    .map((n) => all.find((a) => a === n) ?? all.find((a) => a.endsWith(`.${n}`)) ?? n).slice(0, 3);
  return near.length
    ? `No \`${missing}\` here. Nearest: ${near.map((n) => `${n} (help('${n}'))`).join(", ")}.`
    : `No \`${missing}\` here. help.search('what it should do') finds a call by intent; help() lists the areas.`;
}

const entry = (name: string, d: Doc): string =>
  [d.sig, d.about, d.example ? `Example: ${d.example}` : "", d.throws ? `Throws: ${d.throws}` : "", `Area: ${d.area} (help('${d.area}'))`].filter(Boolean).join("\n");

const firstSentence = (s: string) => oneLine(/^.*?[.;:](?=\s|$)/.exec(s)?.[0] ?? s, 110);

export type Help = ((topic?: string) => string) & { search(words: string): string };

/** `help` over `docs`. */
export function makeHelp(docs: Docs): Help {
  const names = Object.keys(docs);
  const help = (topic?: string): string => {
    if (topic === undefined) return [HOW, ...Object.entries(AREAS).map(([a, line]) => `${a.padEnd(9)}${line}`)].join("\n");
    if (typeof topic !== "string") throw new TypeError(`help takes a name or an area as a string, got ${typeof topic}: help('glob'), help('files')`);
    const t = topic.trim().replace(/\(\)$/, "");
    if (t in AREAS) {
      const mine = names.filter((n) => docs[n]!.area === t);
      return [`${t}: ${AREAS[t as Area]}`, ...mine.map((n) => `${n.padEnd(14)} ${firstSentence(docs[n]!.about)}`), `help('name') gives one in full.`].join("\n");
    }
    const own = docs[t];
    const under = names.filter((n) => n.startsWith(`${t}.`));
    if (own || under.length) return [...(own ? [entry(t, own)] : []), ...under.map((n) => entry(n, docs[n]!))].join("\n\n");
    // `cargo.test` when only `cargo` is documented: the namespace's entry covers its members.
    const parent = t.includes(".") ? docs[t.slice(0, t.indexOf("."))] : undefined;
    if (parent && new RegExp(`\\b${t.split(".").at(-1)}\\b`).test(`${parent.sig} ${parent.about}`)) return entry(t, parent);
    const near = nearest(t, [...names, ...Object.keys(AREAS)]);
    throw new Error(`help: nothing is called ${JSON.stringify(topic)}.${near.length ? ` Nearest: ${near.join(", ")}.` : ""} help() lists the areas; help.search('words') finds a call by what it does.`);
  };
  const words = (s: string) => s.toLowerCase().split(/[^a-z0-9$]+/).filter((w) => w.length > 1).map((w) => w.replace(/(?:ies|es|s|ing|ed)$/, ""));
  const search = (query: string): string => {
    if (typeof query !== "string" || !query.trim()) throw new TypeError("help.search takes the words to look for: help.search('largest files')");
    const q = words(query);
    const scored = names.map((n) => {
      const d = docs[n]!;
      const inName = words(n), inSig = words(d.sig), inAbout = new Set(words(`${d.about} ${d.example ?? ""} ${AREAS[d.area]}`));
      const score = q.reduce((sum, w) => sum + (inName.some((x) => x.includes(w)) ? 4 : 0) + (inSig.includes(w) ? 2 : 0) + (inAbout.has(w) ? 1 : 0), 0);
      return [score, n] as const;
    }).filter(([s]) => s > 0).sort((a, b) => b[0] - a[0] || a[1].localeCompare(b[1])).slice(0, 8);
    if (!scored.length) return `Nothing matches ${JSON.stringify(query)}. help() lists the areas.`;
    return [...scored.map(([, n]) => `${n.padEnd(14)} ${firstSentence(docs[n]!.about)}`), `help('name') gives one in full.`].join("\n");
  };
  return Object.assign(help, { search });
}

/** The tool's description: what the REPL is, what an agent must know before its first cell, and how to find the rest. */
export function description(): string {
  return [
    "Run TypeScript in a persistent Bun REPL (Bun and Node APIs, fetch, npm packages by `import`). Top-level names persist across cells; the last expression is the value; top-level await works.",
    "1. Find calls with help: help() lists the areas (files, search, shell, builds, web, tables, agents, ui, ...), help('glob') a call's signature, example and errors, help.search('largest files') by intent. Use the typed calls (Bun.file(p).text(), rg, glob, ls, du, cargo.test, follow, github.*, fetch) before the shell ($`cmd`, sh): they return data and show the person what was done; a shell command that only reads files is refused with the call to use.",
    "2. Cells are type-checked before they run, with earlier cells' names typed: fix the type instead of casting to any.",
    "3. Time is told to you, since you do not feel a wait: every reply starts '[t3 done 1m12s · expected 20.0s · 3.6× over]'. Give expect_ms for anything not instant; over 5 minutes needs `why`. A cell is stopped at 4× expect_ms (or timeout_ms).",
    "4. A cell still running after yield_ms is backgrounded: the reply says so with its logs so far, and you are told when it ends or runs far over. Do not poll or sleep: go on; task('t3'), await wait('t3'), cancel('t3') (or the `cancel` input) reach it. Stopping a cell kills the processes it started and says what was left.",
    "5. Big values are summarized, not dumped: the reply says how to get more (more('t3', { rows: [100, 200] }), { path: 'a.b[0]' }, { fields: [...] }).",
  ].join("\n");
}
