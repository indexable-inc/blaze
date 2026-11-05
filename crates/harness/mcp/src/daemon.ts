// The harnessd socket, one request per connection in harness's schema (wire.ts): the client's hello and its request go
// out back to back as T_CBOR frames; the daemon answers with its hello (or an error naming both versions), then the reply.
import { existsSync } from "node:fs";
import { decode, encode } from "./cbor.ts";
import { SCHEMA, T_CBOR, frame, split, type Down, type Request } from "./wire.ts";

export const SOCK = () => process.env.HARNESS_SOCK ?? `${process.env.HOME}/.harness/harnessd.sock`;

/** Whether the daemon socket exists (a stale file still fails at connect time; callers fall back then). */
export const daemonPresent = () => existsSync(SOCK());

export class DaemonUnreachable extends Error {}

/** One request/reply over the daemon socket: the reply's fields (without `t` and `reply`). Rejects with the daemon's error. */
export async function req<T = Record<string, unknown>>(body: Request): Promise<T> {
  const hello = frame(T_CBOR, encode({ t: "hello", schema: SCHEMA }));
  const request = frame(T_CBOR, encode({ ...body, t: "req" }));
  const out = new Uint8Array(hello.length + request.length);
  out.set(hello); out.set(request, hello.length);
  return new Promise<T>((resolve, reject) => {
    let buf = new Uint8Array(0);
    let settled = false;
    let greeted = false;
    const fail = (s: { end(): void }, e: Error) => { settled = true; s.end(); reject(e); };
    // socket.write may take only part of a large frame; the rest goes out on drain.
    let sent = 0;
    const flush = (s: { write(b: Uint8Array): number }) => { if (sent < out.length) sent += Math.max(0, s.write(out.subarray(sent))); };
    Bun.connect({
      unix: SOCK(),
      socket: {
        open(s) { flush(s); },
        drain(s) { flush(s); },
        data(s, chunk) {
          if (settled) return;
          const next = new Uint8Array(buf.length + chunk.length);
          next.set(buf); next.set(chunk, buf.length); buf = next;
          for (;;) {
            let f;
            try { f = split(buf); } catch (e) { return fail(s, new Error(`harnessd: ${(e as Error).message}`)); }
            if (!f) return;
            buf = buf.subarray(f.size);
            if (f.t !== T_CBOR) return fail(s, new Error(`harnessd sent a type ${f.t} frame before its reply`));
            let m: Down;
            try { m = decode(f.payload) as Down; } catch (e) { return fail(s, new Error(`harnessd: its message is not CBOR: ${(e as Error).message}`)); }
            if (m.t === "error") return fail(s, new Error(`harnessd: ${m.error}`));
            if (!greeted) {
              if (m.t !== "hello") return fail(s, new Error(`harnessd answered the hello with a \`${m.t}\` message`));
              if (m.schema !== SCHEMA) return fail(s, new Error(`harnessd speaks harness schema ${m.schema}, this kernel speaks schema ${SCHEMA}: restart one of them on the same build`));
              greeted = true;
              continue;
            }
            if (m.t !== "reply") return fail(s, new Error(`harnessd answered the request with a \`${m.t}\` message`));
            settled = true;
            s.end();
            const { t: _t, reply: _reply, ...fields } = m;
            return resolve(fields as T);
          }
        },
        close() { if (!settled) reject(new Error("harnessd closed the connection without replying")); },
        error(_s, e) { settled = true; reject(e); },
        connectError(_s, e) { settled = true; reject(new DaemonUnreachable(`harnessd not reachable at ${SOCK()} (start it with ./harness): ${e.message}`)); },
      },
    }).catch((e) => { if (!settled) { settled = true; reject(new DaemonUnreachable(`harnessd not reachable at ${SOCK()}: ${e}`)); } });
  });
}

/** What the person did to a cell's UI, as the daemon hands it on (msg.rs `Down::Ui`). */
export type UiEvent = { cell: string; handler: string; value: unknown };

/** How long to wait before reconnecting a lost UI feed, doubling from the first to the last. */
const FEED_RETRY_MS = [250, 5000] as const;
/** Reconnects in a row that fail before the feed gives up (about five minutes at the longest wait). */
const FEED_TRIES = 64;

/**
 * Hold a `ui_listen` connection as REPL `repl`'s UI feed: `on` gets each event the app sends for one of its cells. When the
 * connection drops (the daemon restarted, or a newer listener replaced it) it reconnects, waiting longer each time, and
 * gives up after FEED_TRIES failures in a row, saying so on stderr. `stop()` ends it.
 */
export function listen(repl: string, on: (ev: UiEvent) => void): { stop(): void } {
  let stopped = false;
  let failures = 0;
  let current: { end(): void } | undefined;
  let timer: Timer | undefined;
  const retry = (why: string) => {
    current = undefined;
    if (stopped) return;
    failures++;
    if (failures >= FEED_TRIES) {
      console.error(`[harness] ui feed for ${repl}: gave up after ${failures} tries (${why}); controls in this kernel's cells do nothing until it restarts`);
      return;
    }
    const wait = Math.min(FEED_RETRY_MS[1], FEED_RETRY_MS[0] * 2 ** Math.min(failures - 1, 8));
    timer = setTimeout(open, wait);
  };
  const open = () => {
    if (stopped) return;
    const hello = frame(T_CBOR, encode({ t: "hello", schema: SCHEMA }));
    const request = frame(T_CBOR, encode({ t: "req", op: "ui_listen", repl }));
    let buf = new Uint8Array(0);
    let greeted = false;
    let ended = false;
    const end = (s: { end(): void }, why: string) => { if (ended) return; ended = true; s.end(); retry(why); };
    Bun.connect({
      unix: SOCK(),
      socket: {
        open(s) { current = s; s.write(hello); s.write(request); },
        data(s, chunk) {
          const next = new Uint8Array(buf.length + chunk.length);
          next.set(buf); next.set(chunk, buf.length); buf = next;
          for (;;) {
            let f;
            try { f = split(buf); } catch (e) { return end(s, `a bad frame: ${(e as Error).message}`); }
            if (!f) return;
            buf = buf.subarray(f.size);
            if (f.t !== T_CBOR) return end(s, `a type ${f.t} frame on the feed`);
            let m: Down;
            try { m = decode(f.payload) as Down; } catch (e) { return end(s, `a message that is not CBOR: ${(e as Error).message}`); }
            if (m.t === "error") return end(s, `harnessd: ${m.error}`);
            if (!greeted) {
              if (m.t !== "hello") return end(s, `harnessd answered the hello with a \`${m.t}\` message`);
              if (m.schema !== SCHEMA) return end(s, `harnessd speaks harness schema ${m.schema}, this kernel ${SCHEMA}`);
              greeted = true;
              continue;
            }
            if (m.t === "reply") { failures = 0; continue; }
            if (m.t !== "ui") return end(s, `a \`${m.t}\` message on the ui feed`);
            on({ cell: m.cell, handler: m.handler, value: m.value ?? null });
          }
        },
        close(s) { end(s, "the connection closed"); },
        error(s, e) { end(s, e.message); },
        connectError(s, e) { end(s, `not reachable at ${SOCK()}: ${e.message}`); },
      },
    }).catch((e) => retry(`connect: ${e}`));
  };
  open();
  return { stop() { stopped = true; clearTimeout(timer); current?.end(); } };
}
