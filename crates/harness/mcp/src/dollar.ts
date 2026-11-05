// `$` template tag over sh(): `` await $`git status` `` resolves to { stdout, stderr, exitCode, ok, text(), lines(), json() }.
// Modelled on den's `$` (repl/src/shell.ts). Interpolations are shell-quoted words; `$.raw(text)` splices unquoted.
// A command that only reads files or the tree is refused before it starts (src/shell-check.ts) unless it says why.
import { sh } from "./proc.ts";
import { checkShell, initShellCheck } from "./shell-check.ts";

/** Text spliced into a `$` command as written, not quoted. */
export class Raw { constructor(readonly text: string) {} }

const quote = (word: string) => (/^[\w@%+=:,./-]+$/.test(word) ? word : `'${word.replaceAll("'", `'\\''`)}'`);
const words = (value: unknown): string =>
  value instanceof Raw ? value.text
  : Array.isArray(value) ? value.map((item) => (item instanceof Raw ? item.text : quote(String(item)))).join(" ")
  : quote(String(value));

/** The template's text as bash gets it: as written (backslashes kept), but `` \` `` and `\${` are a backtick and `${`. */
function textOf(strings: TemplateStringsArray, values: unknown[]): string {
  const raw = (at: number) => strings.raw[at]!.replace(/\\(`|\$\{)/g, "$1");
  let text = raw(0);
  for (let at = 0; at < values.length; at++) text += words(values[at]) + raw(at + 1);
  return text;
}

/** Throws the refusal when `cmd` only reads files or the tree and no reason was given. Needs `await initShellCheck()` first. */
export function checked(cmd: string, why?: string): void {
  const verdict = checkShell(cmd, why);
  if (!verdict.ok) throw new Error(verdict.message);
}

export class ShellError extends Error {
  constructor(readonly cmd: string, readonly result: ShellOutput) {
    const how = result.signal ? `killed (${result.signal})` : `exit ${result.exitCode}`;
    const tail = result.stderr.trimEnd().split("\n").slice(-20).join("\n");
    super(`${how}: ${cmd}${tail ? `\n${tail}` : ""}`);
    this.name = "ShellError";
  }
}

export class ShellOutput {
  constructor(readonly stdout: string, readonly stderr: string, readonly exitCode: number | null, readonly signal: string | null) {}
  get ok() { return this.exitCode === 0; }
  text() { return this.stdout; }
  /** stdout's lines, without the empty one after a final newline. */
  lines() {
    const lines = this.stdout.split("\n");
    if (lines.at(-1) === "") lines.pop();
    return lines;
  }
  json(): unknown { return JSON.parse(this.stdout); }
}

export class ShellCommand implements PromiseLike<ShellOutput> {
  #nothrow = false;
  #cwd?: string;
  #env?: Record<string, string>;
  #timeoutMs?: number;
  #stdin?: string;
  #why?: string;
  #started = false;
  #awaited = false;
  readonly #result: Promise<ShellOutput>;

  constructor(readonly cmd: string) {
    // Started in a microtask, so options chained on the same line apply first.
    this.#result = Promise.resolve().then(() => this.#run());
    // A command nobody awaits still runs; its failure is reported rather than lost.
    this.#result.catch((error) => { if (!this.#awaited) console.error(`$\`${this.cmd}\`: ${error instanceof Error ? error.message : String(error)}`); });
  }

  async #run(): Promise<ShellOutput> {
    this.#started = true;
    await initShellCheck();
    // sh() refuses a read-only command (shell-check.ts) unless the reason given with .why() lets it run.
    const r = await sh(this.cmd, { cwd: this.#cwd, env: this.#env, stdin: this.#stdin, timeoutMs: this.#timeoutMs, why: this.#why });
    const output = new ShellOutput(r.stdout, r.stderr, r.exit_code, r.signal);
    if (!output.ok && !this.#nothrow) throw new ShellError(this.cmd, output);
    return output;
  }

  #option(name: string): this {
    if (this.#started) throw new TypeError(`$\`${this.cmd}\`.${name}(): options chain before the command starts`);
    return this;
  }
  /** Resolve on a non-zero exit instead of throwing. */
  nothrow() { this.#option("nothrow").#nothrow = true; return this; }
  cwd(dir: string) { this.#option("cwd").#cwd = dir; return this; }
  env(vars: Record<string, string>) { this.#option("env").#env = { ...this.#env, ...vars }; return this; }
  /** Kill the command after ms. */
  timeout(ms: number) { this.#option("timeout").#timeoutMs = ms; return this; }
  stdin(data: string) { this.#option("stdin").#stdin = data; return this; }
  /** Why a command that reads files or the tree needs the shell; lets it run. */
  why(reason: string) {
    if (!reason.trim()) throw new TypeError(`$\`${this.cmd}\`.why(): give the reason the shell is needed`);
    this.#option("why").#why = reason;
    return this;
  }

  then<A = ShellOutput, B = never>(ok?: ((r: ShellOutput) => A | PromiseLike<A>) | null, fail?: ((e: unknown) => B | PromiseLike<B>) | null): Promise<A | B> {
    this.#awaited = true;
    return this.#result.then(ok, fail);
  }
  catch<B = never>(fail?: ((e: unknown) => B | PromiseLike<B>) | null) { return this.then(undefined, fail); }
  text() { return this.then((r) => r.text()); }
  lines() { return this.then((r) => r.lines()); }
  json() { return this.then((r) => r.json()); }
}

type Dollar = ((strings: TemplateStringsArray, ...values: unknown[]) => ShellCommand) & { raw: (text: string) => Raw };
/** `` $`cmd ${arg}` ``: run cmd through sh(); see ShellCommand. */
export const $: Dollar = Object.assign(
  (strings: TemplateStringsArray, ...values: unknown[]) => new ShellCommand(textOf(strings, values)),
  { raw: (text: string) => new Raw(text) },
);
