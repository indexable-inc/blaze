// Talk to harnessd from the REPL: list the agent tree, spawn child agents (they appear in the
// harness web UI and TUI under this agent), type into sessions, and read their screens.
import { table, type Table } from "./table.ts";

const T_JSON = 1;
const SOCK = () => process.env.HARNESS_SOCK ?? `${process.env.HOME}/.harness/harnessd.sock`;

export type Kind = "claude" | "codex" | "shell";
export type AgentInfo = {
  id: number; kind: Kind; parent: number | null; title: string; cwd: string;
  exit: number | null; attached: number; cols: number; rows: number; created_ms: number;
};

/** One request/response over the daemon socket ([type u8][len u32 BE][payload] frames). */
async function req<T = Record<string, unknown>>(body: object): Promise<T> {
  const payload = new TextEncoder().encode(JSON.stringify(body));
  const frame = new Uint8Array(5 + payload.length);
  frame[0] = T_JSON;
  new DataView(frame.buffer).setUint32(1, payload.length);
  frame.set(payload, 5);
  return new Promise<T>((resolve, reject) => {
    let buf = new Uint8Array(0);
    Bun.connect({
      unix: SOCK(),
      socket: {
        open(s) { s.write(frame); },
        data(s, chunk) {
          const next = new Uint8Array(buf.length + chunk.length);
          next.set(buf); next.set(chunk, buf.length); buf = next;
          if (buf.length < 5) return;
          const len = new DataView(buf.buffer, buf.byteOffset).getUint32(1);
          if (buf.length < 5 + len) return;
          s.end();
          const v = JSON.parse(new TextDecoder().decode(buf.subarray(5, 5 + len)));
          v.error ? reject(new Error(`harnessd: ${v.error}`)) : resolve(v as T);
        },
        error(_s, e) { reject(e); },
        connectError(_s, e) { reject(new Error(`harnessd not reachable at ${SOCK()} (start it with ./harness): ${e.message}`)); },
      },
    });
  });
}

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
  async spawn(opts: { kind?: Kind; prompt?: string; args?: string[]; cwd?: string; cols?: number; rows?: number } = {}): Promise<number> {
    const { kind = "claude", prompt, args = [], cwd = process.cwd(), cols = 140, rows = 45 } = opts;
    const r = await req<{ id: number }>({ op: "spawn", kind, prompt, args, cwd, cols, rows, parent: selfId() });
    return r.id;
  },

  /** Type a message into a session and press Enter (bracketed paste, so multi-line stays one message). */
  async send(id: number, text: string, opts: { submit?: boolean; raw?: boolean } = {}): Promise<void> {
    await req({ op: "send", id, text, submit: opts.submit ?? true, raw: opts.raw ?? false });
  },

  /** Message the agent that spawned this one. */
  async notifyParent(text: string): Promise<void> {
    const p = agents.parent();
    if (p === null) throw new Error("this agent has no parent session");
    await agents.send(p, text);
  },

  /** The session's visible screen as plain text. */
  async screen(id: number): Promise<string> {
    return (await req<{ text: string }>({ op: "screen", id })).text;
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

  async kill(id: number): Promise<void> { await req({ op: "kill", id }); },

  /** Web UI address, deep-linked to a session if given. */
  async url(id?: number): Promise<string> {
    const { web } = await req<{ web: string }>({ op: "info" });
    return id === undefined ? web : `${web}/#${id}`;
  },
};
