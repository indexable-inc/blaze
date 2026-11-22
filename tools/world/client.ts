// The world interface's client (docs/v2/plan/design/world.md): a world is a value, the digest of a
// tree; each call is one request and one response on its own connection (a Unix socket) or HTTP
// POST, with the same bytes over either. Nothing is kept between calls but the ids the caller
// holds. Bun APIs only: `Bun.connect` for the socket, `fetch` for HTTP.
//
//   const client = connect({ socket: "/run/user/blaze/world.sock" });
//   const base = await client.open("/home/me/reth");
//   const next = await transaction(base, (tx) => tx.replaceItem(key, "fn f() -> u8 { 1 }"));
//   const answer = await next.check({ lints: true });
//   await next.commit({ onto: "/home/me/reth", expect: base.id });

import { WireError, decodeResponse, encodeRequest, itemEnd } from "./cbor.ts";
import {
  DEFAULT_LIMITS,
  VERSION,
  type Answer,
  type Call,
  type Change,
  type Checkout,
  type Edit,
  type EditWhy,
  type Entry,
  type ItemKey,
  type Limits,
  type Path,
  type Refusal,
  type Request,
  type Response,
  type Subject,
  type Target,
  type What,
  type WorldId,
} from "./schema.ts";

export { SchemaError, WireError } from "./cbor.ts";

/** The server answered with a refusal: `refusal` says which and of what. */
export class RefusedError extends Error {
  constructor(
    readonly call: Call,
    readonly refusal: Refusal,
  ) {
    super(`world ${call}: refused: ${JSON.stringify(refusal)}`);
    this.name = "RefusedError";
  }
}

/** A patch refused: edit `index` (from 0) could not apply, for `why`; no world was made. */
export class EditRefusedError extends RefusedError {
  readonly index: number;
  readonly why: EditWhy;
  constructor(refusal: Extract<Refusal, { kind: "edit" }>) {
    super("patch", refusal);
    this.name = "EditRefusedError";
    this.index = refusal.index;
    this.why = refusal.why;
  }
}

/** A commit found the disk at another world than it expected; nothing was written. */
export class MovedError extends Error {
  constructor(
    readonly onto: Checkout,
    readonly expected: WorldId,
    readonly now: WorldId,
  ) {
    super(`world commit onto ${onto}: the checkout is at ${now}, not ${expected}; nothing was written`);
    this.name = "MovedError";
  }
}

/** The server answered a call with a reply that call never gets. */
export class UnexpectedReplyError extends Error {
  constructor(
    readonly call: Call,
    readonly reply: Response["reply"],
  ) {
    super(`world ${call}: the server replied \`${reply}\`, which ${call} never gets`);
    this.name = "UnexpectedReplyError";
  }
}

/** The connection failed or closed before a whole response came. */
export class TransportError extends Error {
  constructor(
    readonly call: Call,
    readonly where: string,
    readonly why: string,
  ) {
    super(`world ${call} over ${where}: ${why}`);
    this.name = "TransportError";
  }
}

/** No whole response within the deadline; the connection was closed. */
export class TimeoutError extends Error {
  constructor(
    readonly call: Call,
    readonly where: string,
    readonly ms: number,
  ) {
    super(`world ${call} over ${where}: no response within ${ms} ms`);
    this.name = "TimeoutError";
  }
}

export type Endpoint = { socket: string } | { url: string };

export type ConnectOptions = {
  /** Every call's deadline (rule 18). */
  timeoutMs: number;
  /** The bounds a response is read and decoded to. */
  limits: Limits;
};

export const DEFAULT_CONNECT: ConnectOptions = { timeoutMs: 60_000, limits: DEFAULT_LIMITS };

type Exchange = (call: Call, request: Uint8Array) => Promise<Uint8Array>;

/** One request over a fresh Unix-socket connection; the bytes of the one response. */
function overSocket(path: string, options: ConnectOptions): Exchange {
  return (call, request) =>
    new Promise<Uint8Array>((resolve, reject) => {
      const where = `unix:${path}`;
      let got = new Uint8Array(0);
      let pending = request;
      let done = false;
      let socket: { end(): void } | null = null;
      const finish = (outcome: () => void): void => {
        if (done) return;
        done = true;
        clearTimeout(timer);
        socket?.end();
        outcome();
      };
      const timer = setTimeout(() => finish(() => reject(new TimeoutError(call, where, options.timeoutMs))), options.timeoutMs);
      const flush = (s: { write(data: Uint8Array): number }): void => {
        if (pending.length === 0) return;
        const wrote = s.write(pending);
        pending = pending.subarray(Math.max(wrote, 0));
      };
      Bun.connect({
        unix: path,
        socket: {
          open(s) {
            socket = s;
            if (done) {
              s.end();
              return;
            }
            flush(s);
          },
          drain(s) {
            flush(s);
          },
          data(_s, chunk) {
            const next = new Uint8Array(got.length + chunk.length);
            next.set(got);
            next.set(chunk, got.length);
            got = next;
            try {
              const end = itemEnd(got, options.limits);
              if (end === null) return;
              if (end !== got.length) throw new WireError(end, "trailing", "message");
              finish(() => resolve(got));
            } catch (error) {
              finish(() => reject(error));
            }
          },
          close() {
            finish(() => reject(new TransportError(call, where, `closed after ${got.length} bytes, before a whole response`)));
          },
          error(_s, error) {
            finish(() => reject(new TransportError(call, where, error.message)));
          },
        },
      }).catch((error: Error) => finish(() => reject(new TransportError(call, where, error.message))));
    });
}

/** One request as one HTTP POST; the bytes of the response body, read no further than the limit. */
function overHttp(url: string, options: ConnectOptions): Exchange {
  return async (call, request) => {
    const where = url;
    let response: globalThis.Response;
    try {
      response = await fetch(url, {
        method: "POST",
        headers: { "content-type": "application/cbor" },
        body: request,
        signal: AbortSignal.timeout(options.timeoutMs),
      });
    } catch (error) {
      if (error instanceof DOMException && error.name === "TimeoutError") throw new TimeoutError(call, where, options.timeoutMs);
      throw new TransportError(call, where, error instanceof Error ? error.message : "fetch failed");
    }
    if (response.status !== 200) throw new TransportError(call, where, `HTTP ${response.status}`);
    if (response.body === null) throw new TransportError(call, where, "no body");
    const parts: Uint8Array[] = [];
    let total = 0;
    const reader = response.body.getReader();
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      total += value.length;
      if (total > options.limits.mostBytes) {
        await reader.cancel();
        throw new WireError(0, "too_large", "message", options.limits.mostBytes);
      }
      parts.push(value);
    }
    const body = new Uint8Array(total);
    let at = 0;
    for (const part of parts) {
      body.set(part, at);
      at += part.length;
    }
    return body;
  };
}

/** A client of the world interface at `endpoint`. Nothing is opened until a call is made. */
export function connect(endpoint: Endpoint, options: ConnectOptions = DEFAULT_CONNECT): Client {
  const exchange = "socket" in endpoint ? overSocket(endpoint.socket, options) : overHttp(endpoint.url, options);
  return new Client(exchange, options.limits);
}

type ReplyOf<R extends Response["reply"]> = Extract<Response, { reply: R }>;

export class Client {
  constructor(
    private readonly exchange: Exchange,
    private readonly limits: Limits,
  ) {}

  /** One call: the reply, if it is one of `expect`; a refusal or another reply is thrown. */
  async call<R extends Response["reply"]>(request: Request, expect: readonly R[]): Promise<ReplyOf<R>> {
    const bytes = await this.exchange(request.call, encodeRequest(request));
    const response = decodeResponse(bytes, this.limits);
    if (response.reply === "refused") {
      const refusal = response.refusal;
      throw refusal.kind === "edit" ? new EditRefusedError(refusal) : new RefusedError(request.call, refusal);
    }
    if (!isOneOf(response, expect)) throw new UnexpectedReplyError(request.call, response.reply);
    return response;
  }

  /** The world a checkout's files are now. */
  async open(checkout: Checkout): Promise<World> {
    const r = await this.call({ v: VERSION, call: "open", checkout }, ["world"]);
    return new World(this, r.world);
  }

  /** The world `id`, as a value; no call is made, and a later call names it if the server has no such world. */
  world(id: WorldId): World {
    return new World(this, id);
  }
}

function isOneOf<R extends Response["reply"]>(response: Response, expect: readonly R[]): response is ReplyOf<R> {
  return (expect as readonly string[]).includes(response.reply);
}

export type ReadResult =
  | { kind: "text"; path: Path; start: number; text: string }
  | { kind: "listing"; entries: Entry[] };

export type CheckOptions = {
  /** What to check; [] is the whole world. */
  select: Target[];
  /** Whether lint findings are wanted beside errors. */
  lints: boolean;
};

/** One world: an id and the client that names it. Every method is one call. */
export class World {
  constructor(
    readonly client: Client,
    readonly id: WorldId,
  ) {}

  /** The world these edits make of this one, applied in order, all or none. */
  async patch(edits: Edit[]): Promise<World> {
    const r = await this.client.call({ v: VERSION, call: "patch", base: this.id, edits }, ["world"]);
    return new World(this.client, r.world);
  }

  /** A file's text, an item's text, or a directory's entries (`{kind: "dir", path: ""}` is the root). */
  async read(what: What): Promise<ReadResult> {
    const r = await this.client.call({ v: VERSION, call: "read", world: this.id, what }, ["text", "listing"]);
    return r.reply === "text"
      ? { kind: "text", path: r.path, start: r.start, text: r.text }
      : { kind: "listing", entries: r.entries };
  }

  /** The check's answer: errors always, lint findings when asked. */
  async check(options: CheckOptions = { select: [], lints: false }): Promise<Answer> {
    const r = await this.client.call(
      { v: VERSION, call: "check", world: this.id, select: options.select, lints: options.lints },
      ["checked"],
    );
    return { world: r.world, findings: r.findings, certified: r.certified, entered: r.entered, ms: r.ms };
  }

  /** What changed from this world to `to`, files and items. */
  async diff(to: World | WorldId): Promise<Change[]> {
    const r = await this.client.call({ v: VERSION, call: "diff", from: this.id, to: idOf(to) }, ["changes"]);
    return r.changes;
  }

  /**
   * Writes this world's files onto the checkout, only if the checkout is still `expect`; how many
   * files were written. When it is not, throws `MovedError` with the world it is now.
   */
  async commit(at: { onto: Checkout; expect: World | WorldId }): Promise<number> {
    const expect = idOf(at.expect);
    const r = await this.client.call(
      { v: VERSION, call: "commit", world: this.id, onto: at.onto, expect },
      ["committed", "moved"],
    );
    if (r.reply === "moved") throw new MovedError(at.onto, expect, r.now);
    return r.files_written;
  }

  /** Why a finding was given or an item was rebuilt, and what it read. */
  async explain(subject: Subject): Promise<{ why: string; reads: string[] }> {
    const r = await this.client.call({ v: VERSION, call: "explain", world: this.id, subject }, ["explained"]);
    return { why: r.why, reads: r.reads };
  }
}

function idOf(world: World | WorldId): WorldId {
  return typeof world === "string" ? world : world.id;
}

/** Edits gathered for one patch. */
export class Transaction {
  readonly edits: Edit[] = [];

  writeFile(path: Path, text: string): this {
    this.edits.push({ kind: "write_file", path, text });
    return this;
  }

  deleteFile(path: Path): this {
    this.edits.push({ kind: "delete_file", path });
    return this;
  }

  renameFile(from: Path, to: Path): this {
    this.edits.push({ kind: "rename_file", from, to });
    return this;
  }

  /** Bytes `start..end` of `path`, which must be `expect` now, become `text`. */
  replaceRange(path: Path, start: number, end: number, text: string, expect: string): this {
    this.edits.push({ kind: "replace_range", path, start, end, text, expect });
    return this;
  }

  replaceItem(item: ItemKey, text: string): this {
    this.edits.push({ kind: "replace_item", item, text });
    return this;
  }

  /** A finding's fix, as it came. */
  apply(fix: readonly Edit[]): this {
    this.edits.push(...fix);
    return this;
  }
}

/**
 * Runs `fn` to gather edits, then makes them ONE patch of `base`: the new world, or a thrown
 * `EditRefusedError` naming the edit that could not apply. If `fn` throws, nothing is sent.
 */
export async function transaction(base: World, fn: (tx: Transaction) => void | Promise<void>): Promise<World> {
  const tx = new Transaction();
  await fn(tx);
  return base.patch(tx.edits);
}
