// Deprecated: the first agent-tree API, kept as thin aliases over the object API in obj.ts
// (spawn/obj/objects/tree, Obj.prompt/screen/kill/reply). Ids here are bare session numbers.
import { table, type Table } from "./table.ts";
import { req } from "./daemon.ts";

export type Kind = "claude" | "codex" | "cursor" | "shell";
export type AgentInfo = {
  id: number; kind: Kind; parent: number | null; title: string; cwd: string;
  exit: number | null; attached: number; cols: number; rows: number; created_ms: number;
};

const selfId = (): number | null => (process.env.HARNESS_SESSION ? Number(process.env.HARNESS_SESSION) : null);

const pick = (s: AgentInfo): AgentInfo => ({
  id: s.id, kind: s.kind, parent: s.parent, title: s.title, cwd: s.cwd, exit: s.exit,
  attached: s.attached, cols: s.cols, rows: s.rows, created_ms: s.created_ms,
});

export const agents = {
  /** This agent's session id (null when the REPL isn't running under harness). */
  self: selfId,
  /** The agent that spawned this one. */
  parent: (): number | null => (process.env.HARNESS_PARENT ? Number(process.env.HARNESS_PARENT) : null),

  async list(): Promise<Table<AgentInfo>> {
    return table((await req<{ sessions: AgentInfo[] }>({ op: "list" })).sessions.map(pick));
  },

  /** Sessions in tree order with depth; `me` marks this agent. */
  async tree(): Promise<Table<{ depth: number; id: number; kind: Kind; title: string; status: string; me: boolean }>> {
    const all = (await req<{ sessions: AgentInfo[] }>({ op: "list" })).sessions;
    const ids = new Set(all.map((s) => s.id));
    const out: { depth: number; id: number; kind: Kind; title: string; status: string; me: boolean }[] = [];
    const walk = (p: number | null, depth: number) => {
      for (const s of all.filter((s) => (s.parent !== null && ids.has(s.parent) ? s.parent : null) === p)) {
        out.push({ depth, id: s.id, kind: s.kind, title: s.title, status: s.exit === null ? "running" : `exit ${s.exit}`, me: s.id === selfId() });
        walk(s.id, depth + 1);
      }
    };
    walk(null, 0);
    return table(out);
  },

  /** Start an agent as a child of this one (it shows up under us in the harness UI). */
  async spawn(opts: { kind?: Kind; name: string; model: string; effort?: string; prompt?: string; args?: string[]; cwd?: string; cols?: number; rows?: number }): Promise<number> {
    const { kind = "claude", name, model, effort, prompt, args = [], cwd = process.cwd(), cols = 140, rows = 45 } = opts;
    const missing = [!name && "name", (kind !== "shell" && !model) && "model"].filter(Boolean);
    if (missing.length) throw new Error(`agents.spawn needs an explicit ${missing.join(", ")}: agents.spawn({ kind, name, model, effort, prompt })`);
    const r = await req<{ id: string }>({ op: "spawn_object", kind, name, model, effort: effort ?? null, prompt, args, cwd, cols, rows, parent: selfId() });
    return Number(r.id.slice(1));
  },

  /** Type a message into a session and press Enter (bracketed paste, so multi-line stays one message). */
  async send(id: number, text: string, opts: { submit?: boolean; raw?: boolean } = {}): Promise<void> {
    await req({ op: "rpc", id: `s${id}`, method: opts.raw ? "input" : "prompt", args: opts.raw ? { data: text } : { text, submit: opts.submit ?? true } });
  },

  /** Message the agent that spawned this one. */
  async notifyParent(text: string): Promise<void> {
    const p = agents.parent();
    if (p === null) throw new Error("this agent has no parent session");
    await agents.send(p, text);
  },

  /** The session's visible screen as plain text. */
  async screen(id: number): Promise<string> {
    return (await req<{ text: string }>({ op: "rpc", id: `s${id}`, method: "screen" })).text;
  },

  /** Poll a session's screen until `match` appears (or timeout). Returns the screen. */
  async waitFor(id: number, match: string | RegExp, timeoutMs = 60_000): Promise<string> {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const text = await agents.screen(id);
      if (typeof match === "string" ? text.includes(match) : match.test(text)) return text;
      if (Date.now() > deadline) throw new Error(`timed out after ${timeoutMs}ms waiting for ${match} in session ${id}`);
      await Bun.sleep(500);
    }
  },

  async kill(id: number): Promise<void> { await req({ op: "rpc", id: `s${id}`, method: "kill" }); },

  /** Web UI address, deep-linked to a session if given. */
  async url(id?: number): Promise<string> {
    const { web } = await req<{ web: string }>({ op: "info" });
    return id === undefined ? web : `${web}/#${id}`;
  },
};
