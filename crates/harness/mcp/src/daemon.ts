// The harnessd socket: one JSON request/response per connection, [type u8][len u32 BE][payload] frames.
import { existsSync } from "node:fs";

const T_JSON = 1;
export const SOCK = () => process.env.HARNESS_SOCK ?? `${process.env.HOME}/.harness/harnessd.sock`;

/** Whether the daemon socket exists (a stale file still fails at connect time; callers fall back then). */
export const daemonPresent = () => existsSync(SOCK());

export class DaemonUnreachable extends Error {}

/** One request/response over the daemon socket. Rejects with the daemon's `error` field if it sent one. */
export async function req<T = Record<string, unknown>>(body: object): Promise<T> {
  const payload = new TextEncoder().encode(JSON.stringify(body));
  const frame = new Uint8Array(5 + payload.length);
  frame[0] = T_JSON;
  new DataView(frame.buffer).setUint32(1, payload.length);
  frame.set(payload, 5);
  return new Promise<T>((resolve, reject) => {
    let buf = new Uint8Array(0);
    let settled = false;
    // socket.write may take only part of a large frame; the rest goes out on drain.
    let sent = 0;
    const flush = (s: { write(b: Uint8Array): number }) => { if (sent < frame.length) sent += Math.max(0, s.write(frame.subarray(sent))); };
    Bun.connect({
      unix: SOCK(),
      socket: {
        open(s) { flush(s); },
        drain(s) { flush(s); },
        data(s, chunk) {
          const next = new Uint8Array(buf.length + chunk.length);
          next.set(buf); next.set(chunk, buf.length); buf = next;
          if (buf.length < 5) return;
          const len = new DataView(buf.buffer, buf.byteOffset).getUint32(1);
          if (buf.length < 5 + len) return;
          settled = true;
          s.end();
          const v = JSON.parse(new TextDecoder().decode(buf.subarray(5, 5 + len)));
          v.error ? reject(new Error(`harnessd: ${v.error}`)) : resolve(v as T);
        },
        close() { if (!settled) reject(new Error("harnessd closed the connection without replying")); },
        error(_s, e) { settled = true; reject(e); },
        connectError(_s, e) { settled = true; reject(new DaemonUnreachable(`harnessd not reachable at ${SOCK()} (start it with ./harness): ${e.message}`)); },
      },
    }).catch((e) => { if (!settled) { settled = true; reject(new DaemonUnreachable(`harnessd not reachable at ${SOCK()}: ${e}`)); } });
  });
}
