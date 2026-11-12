// What the REPL tells an agent about its calls, as data: each module says, next to its functions, what they take, what
// they do, an example and what they throw (`export const docs: Docs`). Nothing here is in the tool's description, which
// every conversation pays for: `help()` (help.ts) discloses it when asked. functions/docs.test.ts holds the docs to the
// real exports: a call with no doc, or a doc with no call, fails.

/** The areas `help()` lists, each with its one line. A doc belongs to one. */
export const AREAS = {
  files: "read, write, list and measure files: Bun.file, Bun.write, glob, ls, du, vfs, image",
  search: "find text and history: rg, gitLog",
  shell: "run programs and read what they print: $, sh, out, err, lines, kill, procs, ptree, runs, stage, runInfo",
  builds: "builds, lints and tests as a digest, live for the person: cargo.check/clippy/build/test, follow",
  cells: "this REPL's own cells: tasks, task, wait, cancel, more, rerun, signal, progress, pmap, skill, help",
  tables: "rows as data: table, col, lit, sql, nuon",
  web: "HTTP, pages and places: fetch, browser.read/open, withHiddenPage, googleSearch, location, places",
  services: "accounts the person is signed in to: github.*, gmail.*",
  agents: "other agents and every stored object: spawn, obj, objects, tree",
  ui: "what the person sees: render, ask, Svelte markup cells (skill('ui')), the app's own look (world.*)",
  device: "a connected phone and phone calls: phone.*, call.place",
} as const;
export type Area = keyof typeof AREAS;

export type Doc = {
  area: Area;
  /** What someone wants when this is the call to use, in their words: what help.search matches first. */
  for?: string;
  /** The call as it is written, with its options and what it returns. */
  sig: string;
  /** What it does and what to know before using it. */
  about: string;
  /** One cell line that uses it. */
  example?: string;
  /** What it throws, and when. */
  throws?: string;
};
/** By the name a cell writes: `glob`, `cargo.test`, `Bun.file`. */
export type Docs = Record<string, Doc>;
