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
