// The notice path end to end against the daemon's protocol: a daemon on a private socket that answers in the bytes
// harness-proto's own tests record (fixtures/down-hello, down-sessions, down-ok: session 7, busy), and keeps every
// request the kernel sends. What the daemon does with `rpc prompt` is harness/src/object.rs (`"prompt" | "send"` reads
// args.text and args.submit) and daemon.rs (`Req::Send`: a chat session's adapter queues it, a TUI's is pasted).
import { afterAll, beforeAll, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { decode, encode } from "./cbor.ts";
import { newTask } from "./tasks.ts";
import { notices, pump, tellAgent } from "./tell.ts";
import { T_CBOR, frame, split } from "./wire.ts";

const fixture = (n: string) => new Uint8Array(readFileSync(join(import.meta.dir, "..", "..", "harness-proto", "fixtures", `${n}.cbor`)));
const sessions = (busy: boolean) => {
  const recorded = decode(fixture("down-sessions")) as { sessions: { id: number; busy: boolean }[] };
  return encode({ ...recorded, sessions: recorded.sessions.map((s) => ({ ...s, busy })) });
};

type Seen = { op: string; id?: string; method?: string; args?: { text: string; submit: boolean } };
const seen: Seen[] = [];
let busy = true;
let refuse = false;
let server: ReturnType<typeof Bun.listen>;

beforeAll(() => {
  process.env.HARNESS_SOCK = join(process.env.HARNESS_HOME!, "fake-harnessd.sock");
  process.env.HARNESS_SESSION = "7";
  server = Bun.listen<{ buf: Uint8Array; greeted: boolean }>({
    unix: process.env.HARNESS_SOCK,
    socket: {
      open(s) { s.data = { buf: new Uint8Array(0), greeted: false }; },
      data(s, chunk) {
        const next = new Uint8Array(s.data.buf.length + chunk.length);
        next.set(s.data.buf); next.set(chunk, s.data.buf.length); s.data.buf = next;
        for (let f = split(s.data.buf); f; f = split(s.data.buf)) {
          s.data.buf = s.data.buf.subarray(f.size);
          const m = decode(f.payload) as Seen & { t: string };
          if (!s.data.greeted) { s.data.greeted = true; s.write(frame(T_CBOR, fixture("down-hello"))); continue; }
          seen.push(m);
          // Busy is the fixture's own bytes; idle is the same record with the flag turned.
          if (m.op === "list") s.write(frame(T_CBOR, busy ? fixture("down-sessions") : sessions(false)));
          else if (m.op === "rpc" && refuse) s.write(frame(T_CBOR, encode({ t: "error", error: "session 7 has ended" })));
          else s.write(frame(T_CBOR, fixture("down-ok")));
          s.end();
        }
      },
    },
  });
});
afterAll(() => { server.stop(true); delete process.env.HARNESS_SOCK; delete process.env.HARNESS_SESSION; });

const prompts = () => seen.filter((m) => m.op === "rpc");
const settle = () => Bun.sleep(60);

test("while the agent is busy in a turn a notice is held, and typed into its chat when the turn has ended", async () => {
  const t = newTask("await build()");
  tellAgent(t, "[t1 done 41.0s]\nbuilt", { tag: "time" });
  await settle();
  expect(seen.some((m) => m.op === "list")).toBe(true);
  expect(prompts()).toEqual([]);
  expect(t.logs.at(-1)).toBe("[time] [t1 done 41.0s]\nbuilt");
  busy = false;
  await pump();
  expect(prompts().map(({ t: _t, ...m }: Seen & { t?: string }) => m)).toEqual([{ op: "rpc", id: "s7", method: "prompt", args: { text: "[t1 done 41.0s]\nbuilt", submit: true } }]);
  expect(notices.pending).toBe(0);
});

test("an urgent notice (the person answered) goes at once even to a busy agent, with whatever else waited", async () => {
  seen.length = 0;
  busy = true;
  const t = newTask("await ask('Deploy?')");
  tellAgent(t, "[t9 still running 2m00s]", { tag: "time" });
  tellAgent(t, 'Answer to "Deploy?": Yes', { urgent: true });
  await settle();
  expect(prompts()).toHaveLength(1);
  expect(prompts()[0]!.args).toEqual({ text: '[t9 still running 2m00s]\n\nAnswer to "Deploy?": Yes', submit: true });
});

test("a notice the daemon would not take is not lost: it rides on the agent's next reply", async () => {
  seen.length = 0;
  busy = false;
  refuse = true;
  const t = newTask("1");
  tellAgent(t, "[t3 error 1.00s]\nboom", { tag: "time" });
  await settle();
  expect(prompts()).toHaveLength(1);
  expect(notices.pending).toBe(1);
  notices.callBegan();
  expect(notices.callEnds().map((n) => n.text)).toEqual(["[t3 error 1.00s]\nboom"]);
  refuse = false;
});
