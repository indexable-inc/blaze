// The shell is for running programs (blaze, git, just, bun, builds, servers). Reading files or the tree (cat, head,
// grep, find, ls, du, ...) is what the typed calls do as data, shown to the person as what was done, so a command line
// that plainly only does that is refused with the call to use; so are GitHub's reads through `gh` (github.*), HTTP
// through curl or wget (fetch) and `open <url>` (browser.open). A reason lets any of them run: `# why: ...` in the
// command, `.why(reason)` on `$`, `{ why }` on sh.
//
// This replaces a 720-line guard that parsed bash with tree-sitter to find a read wherever it sat (behind xargs, in
// `$(...)`, under `sh -c`) and rebuilt the typed call from the command's flags. That precision bought little once
// `help()` makes the calls easy to find: the refusal's job is to name the call and where its help is at the moment of
// the mistake, and a read hidden inside a larger command is rare and harmless. So the rule is the plain case only, by
// words, with no parser to load: every pipeline of the line starts with a reader and is piped only through filters,
// with no redirection and no substitution. Anything it cannot read plainly runs. Pure and synchronous.

/** Programs that only read files or the tree, with the typed call for each (`a` is the first plain argument, `b` the second). */
const READERS: Record<string, (a?: string, b?: string) => string> = {
  cat: (a) => `await Bun.file(${q(a ?? "path")}).text()`,
  bat: (a) => `await Bun.file(${q(a ?? "path")}).text()`,
  less: (a) => `await Bun.file(${q(a ?? "path")}).text()`,
  more: (a) => `await Bun.file(${q(a ?? "path")}).text()`,
  head: (a) => `(await Bun.file(${q(a ?? "path")}).text()).split('\\n').slice(0, 10)`,
  tail: (a) => `(await Bun.file(${q(a ?? "path")}).text()).split('\\n').slice(-10)`,
  wc: (a) => `(await Bun.file(${q(a ?? "path")}).text()).split('\\n').length - 1`,
  sed: (_a, b) => `(await Bun.file(${q(b ?? "path")}).text()).split('\\n').slice(from - 1, to)`,
  awk: (_a, b) => `(await Bun.file(${q(b ?? "path")}).text()).split('\\n').map((line) => ...)`,
  grep: (a, b) => `await rg(${q(a ?? "pattern")}${b ? `, { path: ${q(b)} }` : ""})`,
  egrep: (a, b) => `await rg(${q(a ?? "pattern")}${b ? `, { path: ${q(b)} }` : ""})`,
  rg: (a, b) => `await rg(${q(a ?? "pattern")}${b ? `, { path: ${q(b)} }` : ""})`,
  ag: (a, b) => `await rg(${q(a ?? "pattern")}${b ? `, { path: ${q(b)} }` : ""})`,
  find: (a) => `await glob('**/*', { cwd: ${q(a ?? ".")}, stat: true })`,
  fd: (a) => `await glob(${q(a ? `**/*${a}*` : "**/*")})`,
  ls: (a) => `await ls(${q(a ?? ".")})`,
  tree: (a) => `await ls(${q(a ?? ".")}, { recursive: true, gitignore: true })`,
  stat: (a) => `await glob(${q(a ?? "path")}, { stat: true })`,
  du: (a) => `await du(${q(a ?? ".")})`,
  which: (a) => `Bun.which(${q(a ?? "name")})`,
};
/** Programs a reader's output may pass through and still be only a read. */
const FILTERS = new Set([...Object.keys(READERS), "sort", "uniq", "cut", "tr", "column", "jq", "nl", "rev", "xargs"]);
/** Words in front of a program that are not it. */
const WRAPPERS = new Set(["time", "nice", "nohup", "command", "env", "exec"]);

const q = (s: string) => `'${s.replaceAll("\\", "\\\\").replaceAll("'", "\\'")}'`;

type Stage = { program: string; args: string[] };
type Parsed = {
  /** Each pipeline's stages. */
  pipelines: Stage[][];
  /** It writes or computes: a redirection, a substitution, a heredoc, a group. Not plainly a read. */
  complex: boolean;
  /** The reason of a `# why: ...` comment. */
  why?: string;
};

/** The command line as words, by bash's quoting: enough to see what plainly only reads. */
export function parse(script: string): Parsed {
  const out: Parsed = { pipelines: [], complex: false };
  let words: string[] = [];
  let stages: Stage[] = [];
  let word: string | undefined;
  const endWord = () => { if (word !== undefined) words.push(word); word = undefined; };
  const endStage = () => {
    endWord();
    let at = 0;
    while (at < words.length && (WRAPPERS.has(words[at]!) || /^[A-Za-z_]\w*=/.test(words[at]!))) at++;
    if (at < words.length) stages.push({ program: words[at]!.split("/").at(-1)!, args: words.slice(at + 1) });
    words = [];
  };
  const endPipeline = () => { endStage(); if (stages.length) out.pipelines.push(stages); stages = []; };
  for (let i = 0; i < script.length; i++) {
    const c = script[i]!;
    const two = script.slice(i, i + 2);
    if (c === "\\") { word = (word ?? "") + (script[++i] ?? ""); continue; }
    if (c === "'") {
      const end = script.indexOf("'", i + 1);
      if (end < 0) { out.complex = true; break; }
      word = (word ?? "") + script.slice(i + 1, end);
      i = end;
      continue;
    }
    if (c === '"') {
      let j = i + 1, text = "";
      for (; j < script.length && script[j] !== '"'; j++) {
        if (script[j] === "\\") text += script[++j] ?? "";
        else { if (script[j] === "$" || script[j] === "`") out.complex = true; text += script[j]; }
      }
      if (j >= script.length) { out.complex = true; break; }
      word = (word ?? "") + text;
      i = j;
      continue;
    }
    if (c === "#" && word === undefined) {
      const end = script.indexOf("\n", i);
      const comment = script.slice(i, end < 0 ? script.length : end);
      const m = /^#\s*why:\s*(\S.*)$/i.exec(comment);
      if (m) out.why = m[1]!.trim();
      i = end < 0 ? script.length : end - 1;
      continue;
    }
    if (c === "$" || c === "`" || c === "(" || c === ")" || c === "{" || c === "}" || c === "<" || c === ">") {
      // `2>&1` only joins the streams of what already runs; anything else redirects, substitutes or groups.
      if (c === ">" && script[i + 1] === "&" && /\d/.test(script[i + 2] ?? "")) { if (word !== undefined && /^\d$/.test(word)) word = undefined; i += 2; continue; }
      out.complex = true;
      continue;
    }
    if (two === "&&" || two === "||") { endPipeline(); i++; continue; }
    if (c === ";" || c === "\n" || c === "&") { endPipeline(); continue; }
    if (c === "|") { endStage(); continue; }
    if (c === " " || c === "\t") { endWord(); continue; }
    word = (word ?? "") + c;
  }
  endPipeline();
  return out;
}

const plain = (args: string[]) => args.filter((a) => !a.startsWith("-"));

/** The typed call for a stage that is covered by one whatever follows it (GitHub's reads, HTTP, a URL to open), and which kind. */
function covered(s: Stage): { call: string; said: string } | undefined {
  const [a, b] = plain(s.args);
  if (s.program === "gh") {
    const github = "GitHub's reads are github.* in ts (rows as Tables, gh's token, one step per call; await skill('github')), not `gh` in a shell.";
    if (a === "api") return { call: /^graphql$/.test(b ?? "") ? "await github.graphql<{ ... }>(`query { ... }`, { ...vars })" : `await github.rest(${q((b ?? "path").replace(/^\//, ""))})`, said: github };
    if (a === "pr" && ["list", "status"].includes(b ?? "")) return { call: "await github.prs({ repo: 'owner/name' })", said: github };
    if (a === "pr" && ["view", "checks", "diff"].includes(b ?? "")) return { call: "await github.pr({ repo: 'owner/name', n })", said: github };
    if (a === "issue" && ["list", "view"].includes(b ?? "")) return { call: "await github.issues({ repo: 'owner/name' })", said: github };
    return undefined;
  }
  const url = s.args.find((x) => /^https?:\/\//.test(x));
  if (s.program === "curl" || s.program === "wget") return { call: `await (await fetch(${q(url ?? "url")})).text()`, said: "HTTP is fetch in ts (one step per request), not curl or wget." };
  if (s.program === "open" && url) return { call: `await browser.open(${q(url)})`, said: "A web page opens in the harness browser with browser.open in ts (the person can press it to see it; you can read and drive it), not `open` in the Mac's default browser." };
  return undefined;
}

/** Whether a stage only reads: a reader that is not following a file, editing in place or running something on what it finds. */
const reads = (s: Stage): boolean =>
  s.program in READERS
  && !(s.program === "tail" && s.args.some((a) => /^-[a-zA-Z]*[fF]/.test(a) || a === "--follow"))
  && !(s.program === "sed" && s.args.some((a) => /^-[a-zA-Z]*i/.test(a) || a === "--in-place"))
  // grep with only a pattern reads its input, not a file.
  && !(/^e?grep$/.test(s.program) && plain(s.args).length < 2 && !s.args.some((a) => /^-[a-zA-Z]*[rR]/.test(a) || a === "--recursive"))
  && !(s.program === "find" && s.args.some((a) => ["-exec", "-execdir", "-ok", "-delete"].includes(a)));

export type ShellCheck = { ok: true; equivalent?: string } | { ok: false; message: string };

/** Whether `script` may run. `{ ok: false, message }` is the refusal naming the typed call; `equivalent` is that call for a line that runs because it said why. */
export function checkShell(script: string, reason?: string): ShellCheck {
  const p = parse(script);
  const found: { program: string; call: string; said: string }[] = [];
  for (const pipeline of p.pipelines) {
    const first = pipeline[0]!;
    const c = covered(first);
    if (c) { found.push({ program: first.program, ...c }); continue; }
    // A reader fed by a program is a filter on what the program printed; a reader with no file reads its input.
    if (p.complex || !reads(first) || !pipeline.every((s) => FILTERS.has(s.program) && (s === first || s.program !== "xargs" || plain(s.args).every((a) => a in READERS)))) continue;
    if (!plain(first.args).length && !["ls", "find", "fd", "tree", "du"].includes(first.program)) continue;
    // A count (`head -n 20 f`) is not the file.
    const [a, b] = plain(first.args).filter((x) => !/^\d+$/.test(x));
    found.push({ program: first.program, call: READERS[first.program]!(a, b), said: "" });
  }
  // A line that also runs a program is a program's line: only a line that is nothing but reads is refused.
  const readsOnly = found.filter((f) => !f.said).length === p.pipelines.length;
  const refused = found.filter((f) => f.said || readsOnly);
  if (!refused.length) return { ok: true };
  const calls = [...new Set(refused.map((f) => f.call))];
  if (reason?.trim() || p.why) return { ok: true, equivalent: calls.join("; ") };
  const readers = [...new Set(refused.filter((f) => !f.said).map((f) => `\`${f.program}\``))];
  const said = [
    readers.length ? `${readers.join(", ")} only ${readers.length === 1 ? "reads" : "read"} files or the tree, which the typed calls return as data (help('files'), help('search')).` : "",
    ...new Set(refused.map((f) => f.said).filter(Boolean)),
  ].filter(Boolean).join(" ");
  return {
    ok: false,
    message: [
      calls.length === 1 ? `Not run: use ${calls[0]}` : `Not run: use the typed calls:\n${calls.map((c) => `- ${c}`).join("\n")}`,
      `${said} The shell is for running programs no typed call covers (blaze, git, just, bun, builds, servers). If this really needs the shell, say why and it runs: a comment \`# why: <reason>\`, .why(reason) on $, or { why } on sh.`,
    ].join("\n"),
  };
}

/** The error a refused command throws: its message alone, with no stack (the frames would be the harness's, not the cell's). */
export function refusal(message: string): Error {
  const e = new Error(message);
  e.stack = `Error: ${message}`;
  return e;
}
