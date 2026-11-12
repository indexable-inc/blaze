// The kernel's API on a Unix socket, in harness's own protocol (wire.ts frames of deterministic CBOR, as the daemon
// speaks): the client's hello `{t: "hello", schema}` and one request `{t: "req", ...Call}` per connection, answered
// with the kernel's hello and `{t: "reply", ...Answer}` (or `{t: "error", error}` naming both schemas, a bad frame or a
// bad request). Opened only when HARNESS_REPL_SOCK names the path; the socket is the owner's alone (mode 0600).
// `call` is the client: what a CLI or a test uses to speak it (scripts/cell.ts).
import { chmodSync, existsSync, unlinkSync } from "node:fs";
import { decode, encode } from "./cbor.ts";
import { SCHEMA, parseCall, type Answer, type Call, type Kernel } from "./kernel.ts";
import { T_CBOR, frame, split } from "./wire.ts";

const message = (m: Record<string, unknown>) => frame(T_CBOR, encode(m));

/** Frames out of a byte stream. Throws wire.ts's error for a bad frame. */
class Reader {
  #buf = new Uint8Array(0);
  push(chunk: Uint8Array): Record<string, unknown>[] {
    const next = new Uint8Array(this.#buf.length + chunk.length);
    next.set(this.#buf); next.set(chunk, this.#buf.length); this.#buf = next;
    const out: Record<string, unknown>[] = [];
    for (;;) {
      const f = split(this.#buf);
      if (!f) return out;
      this.#buf = this.#buf.subarray(f.size);
      if (f.t !== T_CBOR) throw new Error(`a type ${f.t} frame, where every message is CBOR (type ${T_CBOR})`);
      out.push(decode(f.payload) as Record<string, unknown>);
    }
  }
}

export function serve(kernel: Kernel, path: string): { stop(): void } {
  if (existsSync(path)) unlinkSync(path);
  const server = Bun.listen<{ reader: Reader; greeted: boolean }>({
    unix: path,
    socket: {
      open(s) { s.data = { reader: new Reader(), greeted: false }; },
      data(s, chunk) {
        const refuse = (error: string) => { s.write(message({ t: "error", error })); s.end(); };
        let messages: Record<string, unknown>[];
        try { messages = s.data.reader.push(chunk); } catch (e) { return refuse(`kernel: ${(e as Error).message}`); }
        for (const m of messages) {
          if (!s.data.greeted) {
            if (m.t !== "hello" || m.schema !== SCHEMA) return refuse(`this kernel speaks schema ${SCHEMA}; the client began with ${m.t === "hello" ? `schema ${m.schema}` : `a \`${m.t}\` message`}`);
            s.data.greeted = true;
            s.write(message({ t: "hello", schema: SCHEMA, pid: process.pid }));
            continue;
          }
          const { t, ...body } = m;
          const call = t === "req" ? parseCall(body) : `a \`${t}\` message where a \`req\` goes`;
          if (typeof call === "string") return refuse(`kernel: ${call}`);
          void kernel.handle(call).then((a) => { s.write(message({ t: "reply", ...a })); s.end(); });
        }
      },
    },
  });
  chmodSync(path, 0o600);
  return { stop() { server.stop(true); if (existsSync(path)) unlinkSync(path); } };
}

/** One request to the kernel listening at `path`: its answer. Rejects with the kernel's error, or naming why it could not be asked. */
export function call(path: string, request: Call): Promise<Answer> {
  return new Promise<Answer>((resolve, reject) => {
    const reader = new Reader();
    let settled = false;
    const done = (s: { end(): void } | null, f: () => void) => { if (settled) return; settled = true; s?.end(); f(); };
    Bun.connect({
      unix: path,
      socket: {
        open(s) { s.write(message({ t: "hello", schema: SCHEMA })); s.write(message({ t: "req", ...request })); },
        data(s, chunk) {
          let messages: Record<string, unknown>[];
          try { messages = reader.push(chunk); } catch (e) { return done(s, () => reject(new Error(`kernel at ${path}: ${(e as Error).message}`))); }
          for (const m of messages) {
            if (m.t === "error") return done(s, () => reject(new Error(String(m.error))));
            if (m.t === "hello") continue;
            if (m.t !== "reply") return done(s, () => reject(new Error(`kernel at ${path} answered with a \`${m.t}\` message`)));
            const { t: _t, ...answer } = m;
            return done(s, () => resolve(answer as Answer));
          }
        },
        close() { done(null, () => reject(new Error(`kernel at ${path} closed the connection without answering`))); },
        error(_s, e) { done(null, () => reject(e)); },
        connectError(_s, e) { done(null, () => reject(new Error(`no kernel is listening at ${path}: ${e.message}`))); },
      },
    }).catch((e) => done(null, () => reject(new Error(`no kernel is listening at ${path}: ${e}`))));
  });
}
