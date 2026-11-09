// The one TypeScript mirror of harness's schema (crates/harness/harness-proto: msg.rs, event.rs, frame.rs, log.rs):
// the socket's frames and messages, a chat's events and its log, all deterministic CBOR (cbor.ts). wire.test.ts
// round-trips the crate's golden fixtures through this byte for byte.
import { decode } from "./cbor.ts";

/** The schema's version: in every connection's hello and every chat log's MAGIC. */
export const SCHEMA = 1;

/** Raw terminal bytes. */
export const T_DATA = 2;
/** One message: Up from a client, Down from the daemon. */
export const T_CBOR = 6;
/** One chat Event: also every record of a chat log. */
export const T_EVENT = 7;
/** A frame's header: `[type u8][len u32 BE]`. */
export const HEAD = 5;
/** The largest payload either side accepts. */
export const MAX_FRAME = 64 << 20;
/** The first bytes of every chat log. */
export const MAGIC = new TextEncoder().encode(`harness-chat/${SCHEMA}\n`);

/** A frame's bytes. */
export function frame(t: number, payload: Uint8Array): Uint8Array {
  if (payload.length > MAX_FRAME) throw new Error(`a ${payload.length}-byte frame payload is past the ${MAX_FRAME}-byte limit`);
  const b = new Uint8Array(HEAD + payload.length);
  b[0] = t;
  new DataView(b.buffer).setUint32(1, payload.length);
  b.set(payload, HEAD);
  return b;
}

/** A header's type and length, or why it is refused (a type of the JSON protocol before schema 1, a length past the limit). */
export function check(h: Uint8Array): { t: number; len: number } {
  const t = h[0]!, len = new DataView(h.buffer, h.byteOffset, HEAD).getUint32(1);
  if (t === 1 || t === 3 || t === 4)
    throw new Error(`frame type ${t} is the JSON protocol harness spoke before schema ${SCHEMA}; this side speaks only deterministic CBOR (types ${T_CBOR} and ${T_EVENT}, and ${T_DATA} for terminal bytes): update the other side`);
  if (t !== T_DATA && t !== T_CBOR && t !== T_EVENT) throw new Error(`frame type ${t} is not in harness schema ${SCHEMA}`);
  if (len > MAX_FRAME) throw new Error(`a frame of ${len} bytes is past the ${MAX_FRAME}-byte limit`);
  return { t, len };
}

/** The first whole frame in `buf` (its type, payload and size), or null while `buf` holds only part of one. */
export function split(buf: Uint8Array): { t: number; payload: Uint8Array; size: number } | null {
  if (buf.length < HEAD) return null;
  const { t, len } = check(buf);
  return buf.length < HEAD + len ? null : { t, payload: buf.subarray(HEAD, HEAD + len), size: HEAD + len };
}

// ---- messages (msg.rs)

/** A request: the `op` and its fields (msg.rs `Request`; repl_task and spawn_object carry open records). */
export type Request =
  | { op: "list"; all?: boolean; chats?: boolean }
  | { op: "info" | "last" | "watch" | "shutdown" | "browser" }
  | { op: "spawn"; kind: string; args?: string[]; parent?: number | null; cwd?: string | null; prompt?: string | null; name?: string | null; cols?: number | null; rows?: number | null; mode?: string | null; model?: string; effort?: string }
  | { op: "attach"; id: number; cols?: number | null; rows?: number | null; events?: boolean }
  | { op: "send"; id: number; text: string; raw?: boolean; submit?: boolean | null }
  | { op: "set_model"; id: number; model?: string | null; effort?: string | null }
  /** Name a session (its `label`); an empty title takes the name away. */
  | { op: "rename"; id: number; title: string }
  /** Ask chat `id` aside: `text` as one turn on a discarded fork of its conversation, which never has it. */
  | { op: "aside"; id: number; text: string }
  | { op: "screen" | "kill" | "forget"; id: number }
  | { op: "resize"; id: number; cols: number; rows: number }
  | ({ op: "repl_task" } & Record<string, unknown>)
  | ({ op: "spawn_object" } & Record<string, unknown>)
  | { op: "object"; id: string }
  | { op: "rpc"; id: string; method: string; args?: unknown }
  | { op: "ui_event"; cell: string; handler: string; value?: unknown }
  | { op: "ui_listen"; repl: string }
  | { op: "run"; cmd: string; cwd?: string | null; env?: Record<string, string>; clear_env?: boolean; session?: number | null; parent?: string | null; stdin?: string | null; repl?: string | null }
  | { op: "run_status" | "run_kill"; id: string }
  | { op: "runs"; session?: number | null; limit?: number | null; before?: number | null; q?: string | null }
  | { op: "run_read"; id: string; stage?: number | null; stream?: string; offset?: number; max_bytes?: number | null; wait_ms?: number }
  | { op: "run_wait"; id: string; timeout_ms?: number | null }
  | { op: "run_stdin"; id: string; data?: string | null; close?: boolean };

/** A client's message. */
export type Up =
  | { t: "hello"; schema: number }
  | ({ t: "req" } & Request)
  | { t: "resize"; cols: number; rows: number }
  | { t: "detach" };

/** One session as list, screen and attach describe it; `label`: what the person renamed it to, shown in place of `name`. */
export type SessionInfo = {
  id: number; object: string; kind: string; name: string | null; label?: string; parent: number | null; title: string; cwd: string | null;
  argv: string[]; created_ms: number | null; ended_ms: number | null; exit: number | null; status: string | null; attached: number;
  cols: number | null; rows: number | null; busy: boolean; mode: string; model: string | null; past: boolean;
};

/** A request's answer, tagged by `reply`; a run's or an object's answer is an open record. */
export type Reply =
  | { reply: "ok" | "watching" | "listening" }
  | { reply: "spawned"; id: number }
  | { reply: "sessions"; sessions: SessionInfo[]; models?: Record<string, ModelChoice[]> }
  | { reply: "daemon"; pid: number; sock: string }
  | { reply: "last"; id: number | null }
  | { reply: "screen"; text: string; info: SessionInfo }
  | { reply: "attached"; info: SessionInfo }
  /** `browser`: the harness browser host's pid, its socket for the kernel (cdp.ts) and the Mach service the app looks up. */
  | { reply: "browser"; pid: number; sock: string; service: string }
  /** `aside`: the fork's answer, its last words (markdown). */
  | { reply: "aside"; text: string }
  | ({ reply: "record" } & Record<string, unknown>);

/** The daemon's message. */
export type Down =
  | { t: "hello"; schema: number; pid: number }
  | ({ t: "reply" } & Reply)
  | { t: "error"; error: string }
  | { t: "exit"; code: number }
  | { t: "changed"; gen: number }
  | { t: "ui"; cell: string; handler: string; value?: unknown };

// ---- events (event.rs)

/** One step a cell took outside the REPL (effects.ts shapes it). */
export type Effect = {
  n?: number; kind: string; doing: string; done: string; target: string; status: string;
  started_ms?: number; ms?: number; detail: string; value: unknown;
};
/** A REPL cell's state, as a tool_detail event carries it. */
export type Detail = {
  block: string; cell: string; status: string; ms?: number; code: string; effects: Effect[]; effects_dropped: number;
  value: unknown; ui: unknown; logs: string;
};
export type Kind = "text" | "thinking";
/** One model an agent offers (a models event). */
export type ModelChoice = { id: string; model: string; name: string; about: string; efforts: string[]; effort?: string };
/** The call that filled the context, as the agent's usage reports it (a context event's `last`). */
export type ContextCall = { input: number; cache_read: number; cache_write: number; output: number; thinking?: number };
/** One part of what the context holds; `of` names the part it splits, `estimate` how harness estimated it ("": the agent's count). */
export type ContextPart = { id: string; name: string; tokens: number; of: string; estimate: string };
/** One window of the person's plan (a plan event): `used` is 0 to 1, `resets_at` unix seconds. */
export type PlanWindow = { name: string; used: number; resets_at?: number; warn: boolean; limited: boolean };
/** What happened. */
export type Body =
  | { ev: "session"; agent: string; model: string; session_id?: string; effort?: string }
  | { ev: "models"; models: ModelChoice[] }
  | { ev: "choice"; model?: string; effort?: string }
  | { ev: "context"; used: number; window: number; last?: ContextCall; parts?: ContextPart[] }
  | { ev: "plan"; windows: PlanWindow[] }
  | { ev: "user"; text: string }
  | { ev: "busy"; busy: boolean }
  | { ev: "block_start"; block: string; kind: Kind }
  | { ev: "delta"; block: string; text: string }
  | { ev: "block"; block: string; kind: Kind; text: string }
  | { ev: "tool_start"; block: string; name: string }
  | { ev: "tool_delta"; block: string; json: string }
  | { ev: "tool_call"; block: string; name: string; input: unknown }
  | { ev: "tool_result"; block: string; text: string; error: boolean }
  | ({ ev: "tool_detail" } & Detail)
  | { ev: "note"; text: string }
  | { ev: "error"; text: string }
  | { ev: "turn_end"; ok: boolean; stop: string; duration_ms?: number; cost_usd?: number; usage: unknown; text: string };
/** One event: when (ms since the epoch), and what. */
export type Event = { ts: number } & Body;

// A field's type as serde reads it: required (no default), defaulted when missing, or optional (missing or null is none).
type F = "s" | "b" | "u" | "f" | "k" | "v" | "effects" | "models" | "parts" | "windows" | "call";
type Spec = Record<string, [F, "req" | "def" | "opt"]>;
const DETAIL: Spec = {
  block: ["s", "req"], cell: ["s", "req"], status: ["s", "req"], ms: ["u", "opt"], code: ["s", "def"], effects: ["effects", "def"],
  effects_dropped: ["u", "def"], value: ["v", "def"], ui: ["v", "def"], logs: ["s", "def"],
};
const EFFECT: Spec = {
  n: ["u", "opt"], kind: ["s", "def"], doing: ["s", "def"], done: ["s", "def"], target: ["s", "def"], status: ["s", "def"],
  started_ms: ["u", "opt"], ms: ["u", "opt"], detail: ["s", "def"], value: ["v", "def"],
};
const BODIES: Record<Body["ev"], Spec> = {
  session: { agent: ["s", "req"], model: ["s", "req"], session_id: ["s", "opt"], effort: ["s", "opt"] },
  models: { models: ["models", "req"] },
  choice: { model: ["s", "opt"], effort: ["s", "opt"] },
  context: { used: ["u", "req"], window: ["u", "req"], last: ["call", "opt"], parts: ["parts", "opt"] },
  plan: { windows: ["windows", "req"] },
  user: { text: ["s", "req"] },
  busy: { busy: ["b", "req"] },
  block_start: { block: ["s", "req"], kind: ["k", "req"] },
  delta: { block: ["s", "req"], text: ["s", "req"] },
  block: { block: ["s", "req"], kind: ["k", "req"], text: ["s", "req"] },
  tool_start: { block: ["s", "req"], name: ["s", "req"] },
  tool_delta: { block: ["s", "req"], json: ["s", "req"] },
  tool_call: { block: ["s", "req"], name: ["s", "req"], input: ["v", "req"] },
  tool_result: { block: ["s", "req"], text: ["s", "req"], error: ["b", "req"] },
  tool_detail: DETAIL,
  note: { text: ["s", "req"] },
  error: { text: ["s", "req"] },
  turn_end: { ok: ["b", "req"], stop: ["s", "req"], duration_ms: ["u", "opt"], cost_usd: ["f", "opt"], usage: ["v", "def"], text: ["s", "def"] },
};
const MODEL: Spec = {
  id: ["s", "req"], model: ["s", "def"], name: ["s", "def"], about: ["s", "def"], efforts: ["v", "def"], effort: ["s", "opt"],
};
const CALL: Spec = { input: ["u", "req"], cache_read: ["u", "req"], cache_write: ["u", "req"], output: ["u", "req"], thinking: ["u", "opt"] };
const PART: Spec = { id: ["s", "req"], name: ["s", "req"], tokens: ["u", "req"], of: ["s", "def"], estimate: ["s", "def"] };
const WINDOW: Spec = { name: ["s", "req"], used: ["f", "req"], resets_at: ["u", "opt"], warn: ["b", "def"], limited: ["b", "def"] };
// The lists of maps, each read by its spec.
const LISTS: Partial<Record<F, Spec>> = { effects: EFFECT, models: MODEL, parts: PART, windows: WINDOW };
const EMPTY: Record<F, () => unknown> = {
  s: () => "", b: () => false, u: () => 0, f: () => 0, k: () => "text", v: () => null, effects: () => [], models: () => [], parts: () => [], windows: () => [],
  call: () => null,
};

const isMap = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v) && !(v instanceof Uint8Array);

/** `m` read by `spec` into a fresh object (unknown fields dropped, defaults filled), or why not, naming the field. */
function fields(m: Record<string, unknown>, spec: Spec, what: string): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [k, [f, how]] of Object.entries(spec)) {
    const x = m[k];
    if (x === undefined || (x === null && how === "opt")) {
      if (how === "req") throw new Error(`${what}: no \`${k}\``);
      if (how === "def") out[k] = EMPTY[f]();
      continue;
    }
    const ok =
      f === "s" ? typeof x === "string"
      : f === "b" ? typeof x === "boolean"
      : f === "u" ? (typeof x === "number" && Number.isInteger(x) && x >= 0) || (typeof x === "bigint" && x >= 0n)
      : f === "f" ? typeof x === "number" || typeof x === "bigint"
      : f === "k" ? x === "text" || x === "thinking"
      : f === "call" ? isMap(x)
      : LISTS[f] ? Array.isArray(x)
      : true;
    if (!ok) throw new Error(`${what}: \`${k}\` is not ${{ s: "a string", b: "a bool", u: "a u64", f: "a number", k: "text or thinking", v: "", effects: "a list", models: "a list", parts: "a list", windows: "a list", call: "a map" }[f]}`);
    const list = LISTS[f];
    out[k] = list ? (x as unknown[]).map((e, i) => {
      if (!isMap(e)) throw new Error(`${what}: ${k}[${i}] is not a map`);
      return fields(e, list, `${what} ${k}[${i}]`);
    }) : f === "call" ? fields(x as Record<string, unknown>, CALL, `${what} ${k}`)
    : f === "f" && typeof x === "bigint" ? Number(x) : x;
  }
  return out;
}

/** A decoded value as an Event, as event.rs's serde reads it, or why it is not one. */
export function toEvent(v: unknown): Event {
  if (!isMap(v)) throw new Error("an event is a map");
  const ev = v.ev;
  if (typeof ev !== "string" || !Object.hasOwn(BODIES, ev)) throw new Error(`no event \`${String(ev)}\` in harness schema ${SCHEMA}`);
  const { ts } = fields(v, { ts: ["u", "def"] }, ev);
  return { ts, ev, ...fields(v, BODIES[ev as Body["ev"]], ev) } as Event;
}

// ---- a chat's log (log.rs)

export type LogRecord = { n: number; event: Event | null; error?: string };
export type Log = { records: LogRecord[]; torn: { at: number; bytes: number } | null; end: number };

/**
 * The frames of a log from a frame boundary: `rest` starts at byte `at` of the log, after `n` records. A tail that is not
 * a whole event frame is the torn write a crash mid-append left (reported, not consumed); a whole frame whose payload is
 * not an event is a record with event null and why.
 */
export function readFrames(rest: Uint8Array, at = 0, n = 0): Log {
  const records: LogRecord[] = [];
  let torn: Log["torn"] = null;
  let i = 0;
  while (i < rest.length) {
    const left = rest.length - i;
    if (left < HEAD) { torn = { at: at + i, bytes: left }; break; }
    const len = new DataView(rest.buffer, rest.byteOffset + i, HEAD).getUint32(1);
    // Not a frame boundary: only a torn header looks like this (a crash never writes bytes that are not a frame's).
    if (rest[i] !== T_EVENT || len > MAX_FRAME || left < HEAD + len) { torn = { at: at + i, bytes: left }; break; }
    let event: Event | null = null, error: string | undefined;
    try { event = toEvent(decode(rest.subarray(i + HEAD, i + HEAD + len))); } catch (e) { error = e instanceof Error ? e.message : String(e); }
    records.push(error === undefined ? { n: n + records.length + 1, event } : { n: n + records.length + 1, event, error });
    i += HEAD + len;
  }
  return { records, torn, end: at + i };
}

/** A whole log's records; an empty file or a prefix of MAGIC is an empty log. Not a log: what it is instead. */
export function readLog(bytes: Uint8Array): Log | { notALog: string } {
  if (bytes.length < MAGIC.length && MAGIC.subarray(0, bytes.length).every((b, i) => bytes[i] === b))
    return { records: [], torn: null, end: bytes.length };
  if (!MAGIC.every((b, i) => bytes[i] === b)) {
    const first = bytes.find((b) => !(b === 0x20 || (b >= 0x09 && b <= 0x0d)));
    return {
      notALog: first === 0x7b
        ? "an NDJSON chat log from before harness schema 1 (the daemon converts these when it starts)"
        : `not a harness chat log (it starts ${JSON.stringify(new TextDecoder().decode(bytes.subarray(0, 16)))}, not ${JSON.stringify(new TextDecoder().decode(MAGIC))})`,
    };
  }
  return readFrames(bytes.subarray(MAGIC.length), MAGIC.length);
}

// ---- the messages projection

/** One messages row (what Obj.messages and the sql `messages` view give). */
export type Msg = { n: number; role: string; kind: string; tool: string | null; text: string; ts_ms: number | null };
export type MsgRow = Msg & { object: string };

/** JSON text of a decoded value: a bigint as its digits (JSON.stringify refuses bigints), bytes as a list of numbers. */
export function json(v: unknown): string {
  if (typeof v === "bigint") return v.toString();
  if (v instanceof Uint8Array) return `[${v.join(",")}]`;
  if (Array.isArray(v)) return `[${v.map((x) => (x === undefined ? "null" : json(x))).join(",")}]`;
  if (isMap(v)) return `{${Object.entries(v).filter(([, x]) => x !== undefined).map(([k, x]) => `${JSON.stringify(k)}:${json(x)}`).join(",")}}`;
  return JSON.stringify(v) ?? "null";
}

/**
 * A chat's messages rows, one event at a time: user and assistant words, tool calls and results, turn ends (the rows
 * the daemon wrote to a chat's `messages` stream before schema 1). The one home of this projection.
 */
export class Projection {
  #n = 0;
  #tools = new Map<string, string>(); // a tool call's name by block, from its tool_start or tool_call
  constructor(readonly session: string) {}
  rows(e: Event): MsgRow[] {
    if (e.ev === "tool_start" || e.ev === "tool_call") this.#tools.set(e.block, e.name);
    const row = (role: string, kind: string, tool: string | null, text: string): MsgRow[] =>
      [{ object: this.session, n: ++this.#n, role, kind, tool, text, ts_ms: e.ts }];
    switch (e.ev) {
      case "user": return row("user", "text", null, e.text);
      case "block": return row("assistant", e.kind === "thinking" ? "thinking" : "text", null, e.text);
      case "tool_call": return row("assistant", "tool_call", e.name, json(e.input));
      case "tool_result": return row("user", "tool_result", this.#tools.get(e.block) ?? null, e.text);
      case "turn_end": return row("system", "turn_end", null, e.text);
      default: return [];
    }
  }
}

/** A chat's messages rows from its events, numbered from 1. */
export function messageRows(session: string, events: Event[]): MsgRow[] {
  const p = new Projection(session);
  return events.flatMap((e) => p.rows(e));
}
