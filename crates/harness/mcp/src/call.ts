// A phone call placed for the person, from ts: `call.place(to, { goal, onBehalfOf })` has Twilio dial the number and
// bridges the call's audio to an OpenAI Realtime voice session that speaks for him toward `goal`, then returns what
// was said. Nothing is dialled until he presses Call on the question it shows: a call reaches a real person and
// costs money.
//
// The pieces: Twilio's REST API starts the call with TwiML that streams its audio (G.711 mu-law, 8 kHz) over a
// websocket (Media Streams); Twilio must reach that websocket from the internet, so a local server is put behind a
// Cloudflare quick tunnel (`cloudflared`) for the length of the call; the server relays audio both ways to the
// Realtime API's websocket, which takes and gives the same mu-law, so nothing is transcoded. The relay itself is
// `Bridge`, a state machine with no IO (its test drives it with Twilio's and OpenAI's messages).
//
// Keys, never printed: `$HARNESS_HOME/keys/twilio.json` (`{"accountSid", "authToken", "from": "+1..."}`) and
// `$HARNESS_HOME/keys/openai` (the key alone), or TWILIO_ACCOUNT_SID, TWILIO_AUTH_TOKEN, TWILIO_FROM, OPENAI_API_KEY.
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { effect } from "./effects.ts";
import { table, type Table } from "./table.ts";
import { ask } from "./ui/index.ts";

const HOME = () => process.env.HARNESS_HOME ?? `${process.env.HOME}/.harness`;
/** The Realtime model that speaks. */
export const CALL_MODEL = "gpt-realtime";
/** The longest a call may run, minutes, whatever the caller asks. */
export const CALL_MAX_MINUTES = 30;

export class CallError extends Error {}

export type CallOptions = {
  /** What the call is for, in plain words: what to ask or say, what answer to come back with, when it is done. */
  goal: string;
  /** Who it calls for, as the voice says it ("Andrew"): it says at the start that it is an AI assistant calling for them. */
  onBehalfOf: string;
  /** The voice (OpenAI's: marin, cedar, alloy, ...). */
  voice?: string;
  /** Speak first with this line; without it the voice waits for the person who answers to say hello. */
  greeting?: string;
  /** Hang up after this many minutes whatever happens (default 10, at most CALL_MAX_MINUTES). */
  maxMinutes?: number;
};

export type CallLine = { who: "them" | "assistant"; text: string };
export type CallResult = {
  to: string; sid: string;
  /** How the call ended: `completed` (they or the voice hung up), `no-answer`, `busy`, `failed`, `canceled`, `timeout`. */
  status: string;
  seconds: number;
  /** What the voice reported when it hung up (the answer it got), when it did. */
  outcome: string | null;
  transcript: Table<CallLine>;
};

type Keys = { accountSid: string; authToken: string; from: string; openai: string };

/** The keys, or an error saying which is missing and where it goes. */
export async function keys(): Promise<Keys> {
  const dir = join(HOME(), "keys");
  const file = async (name: string) => { try { return (await readFile(join(dir, name), "utf8")).trim(); } catch { return undefined; } };
  let tw: Partial<Keys> = {};
  const raw = await file("twilio.json");
  if (raw) {
    try { tw = JSON.parse(raw) as Partial<Keys>; } catch (e) { throw new CallError(`call: ${join(dir, "twilio.json")} is not JSON: ${(e as Error).message}`); }
  }
  const got = {
    accountSid: tw.accountSid ?? process.env.TWILIO_ACCOUNT_SID,
    authToken: tw.authToken ?? process.env.TWILIO_AUTH_TOKEN,
    from: tw.from ?? process.env.TWILIO_FROM,
    openai: (await file("openai")) ?? process.env.OPENAI_API_KEY,
  };
  const missing = Object.entries(got).filter(([, v]) => !v).map(([k]) => k);
  if (missing.length) {
    throw new CallError(`call: no ${missing.join(", ")}. Put Twilio's in ${join(dir, "twilio.json")} as {"accountSid", "authToken", "from": "+1..."} (a number the account owns) and the OpenAI key in ${join(dir, "openai")}`);
  }
  return got as Keys;
}

/** `to` as E.164 (`+15551234567`), or an error: Twilio dials nothing else. */
export function e164(to: string): string {
  const digits = to.replace(/[\s().-]/g, "");
  if (!/^\+[1-9]\d{6,14}$/.test(digits)) throw new CallError(`call: ${JSON.stringify(to)} is not a phone number in international form (+15551234567)`);
  return digits;
}

const xml = (s: string) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

/** The TwiML that streams the call's audio both ways over the websocket at `wss`. */
export const twiml = (wss: string): string => `<Response><Connect><Stream url="${xml(wss)}"/></Connect></Response>`;

/** What the voice is told. */
export function instructions(o: CallOptions): string {
  return [
    `You are an AI voice assistant making a phone call on behalf of ${o.onBehalfOf}. You are speaking to whoever answers.`,
    `At the start, say that you are an AI assistant calling for ${o.onBehalfOf}; never claim to be a person, and say so plainly if asked.`,
    `The call's purpose: ${o.goal}`,
    "Speak naturally and briefly, one thing at a time, and listen. Do not invent facts about him; if you are asked something you were not told, say you do not know and offer to pass the question on.",
    "Never agree to pay, share card or account numbers, or accept terms on his behalf.",
    "When the purpose is met, or it clearly cannot be, or you reach voicemail (leave one short message), say goodbye and call hang_up with the outcome: the answer you got, in one or two sentences.",
  ].join("\n");
}

/** The Realtime session for a call. */
export function session(o: CallOptions) {
  return {
    type: "session.update",
    session: {
      type: "realtime", model: CALL_MODEL, output_modalities: ["audio"], instructions: instructions(o),
      audio: {
        input: { format: { type: "audio/pcmu" }, turn_detection: { type: "server_vad" }, transcription: { model: "gpt-4o-mini-transcribe" } },
        output: { format: { type: "audio/pcmu" }, voice: o.voice ?? "marin" },
      },
      tools: [{
        type: "function", name: "hang_up", description: "End the call. Call it after saying goodbye.",
        parameters: { type: "object", properties: { outcome: { type: "string", description: "What came of the call: the answer you got, in one or two sentences." } }, required: ["outcome"] },
      }],
    },
  };
}

/** What the bridge asks its driver to do. */
export type Act =
  | { to: "openai"; send: Record<string, unknown> }
  | { to: "twilio"; send: Record<string, unknown> }
  | { to: "end"; after_ms: number };

/**
 * The relay between one call's Twilio media stream and its Realtime session, with no IO: each message from either side
 * gives the messages to send to the other. It keeps what was said and the outcome the voice reported.
 */
export class Bridge {
  streamSid: string | null = null;
  readonly lines: CallLine[] = [];
  outcome: string | null = null;
  #ready = false;
  #speaking = false;
  constructor(readonly opts: CallOptions) {}

  /** The Realtime socket opened: configure the session. */
  opened(): Act[] { return [{ to: "openai", send: session(this.opts) }]; }

  /** One message from Twilio's media stream. */
  fromTwilio(m: { event?: string; start?: { streamSid?: string }; media?: { payload?: string } }): Act[] {
    switch (m.event) {
      case "start": {
        this.streamSid = m.start?.streamSid ?? null;
        return this.#greet();
      }
      case "media":
        return m.media?.payload ? [{ to: "openai", send: { type: "input_audio_buffer.append", audio: m.media.payload } }] : [];
      case "stop":
        return [{ to: "end", after_ms: 0 }];
      default:
        return [];
    }
  }

  /** One event from the Realtime session. */
  fromOpenAI(ev: { type?: string; delta?: string; transcript?: string; name?: string; arguments?: string; call_id?: string }): Act[] {
    switch (ev.type) {
      case "session.updated":
        this.#ready = true;
        return this.#greet();
      case "response.output_audio.delta":
        this.#speaking = true;
        return this.streamSid && ev.delta ? [{ to: "twilio", send: { event: "media", streamSid: this.streamSid, media: { payload: ev.delta } } }] : [];
      case "response.output_audio.done":
        this.#speaking = false;
        return [];
      // They started talking over the voice: drop what Twilio has not played yet and stop the reply.
      case "input_audio_buffer.speech_started":
        if (!this.#speaking || !this.streamSid) return [];
        this.#speaking = false;
        return [{ to: "twilio", send: { event: "clear", streamSid: this.streamSid } }, { to: "openai", send: { type: "response.cancel" } }];
      case "response.output_audio_transcript.done":
        if (ev.transcript?.trim()) this.lines.push({ who: "assistant", text: ev.transcript.trim() });
        return [];
      case "conversation.item.input_audio_transcription.completed":
        if (ev.transcript?.trim()) this.lines.push({ who: "them", text: ev.transcript.trim() });
        return [];
      case "response.function_call_arguments.done": {
        if (ev.name !== "hang_up") return [];
        try { this.outcome = String((JSON.parse(ev.arguments ?? "{}") as { outcome?: unknown }).outcome ?? ""); } catch { this.outcome = ""; }
        // After the goodbye still in Twilio's buffer has played.
        return [{ to: "end", after_ms: 2500 }];
      }
      default:
        return [];
    }
  }

  #greeted = false;
  /** Once both sides are up, with a greeting to speak first: say it. */
  #greet(): Act[] {
    if (this.#greeted || !this.#ready || !this.streamSid || !this.opts.greeting) return [];
    this.#greeted = true;
    return [
      { to: "openai", send: { type: "conversation.item.create", item: { type: "message", role: "user", content: [{ type: "input_text", text: `The call was answered. Say: ${this.opts.greeting}` }] } } },
      { to: "openai", send: { type: "response.create" } },
    ];
  }
}

/** The `https://….trycloudflare.com` address in cloudflared's output so far. */
export const tunnelUrl = (log: string): string | null => /https:\/\/[a-z0-9-]+\.trycloudflare\.com/.exec(log)?.[0] ?? null;

async function twilio(k: Keys, path: string, form?: Record<string, string>): Promise<Record<string, unknown>> {
  const r = await fetch(`https://api.twilio.com/2010-04-01/Accounts/${k.accountSid}/${path}`, {
    method: form ? "POST" : "GET",
    headers: { Authorization: `Basic ${btoa(`${k.accountSid}:${k.authToken}`)}`, ...(form ? { "Content-Type": "application/x-www-form-urlencoded" } : {}) },
    body: form ? new URLSearchParams(form).toString() : undefined,
  });
  const body = (await r.json().catch(() => ({}))) as Record<string, unknown>;
  if (!r.ok) throw new CallError(`call: Twilio refused ${path.split("/")[0]}: ${String(body.message ?? r.statusText)} (${r.status})`);
  return body;
}

async function place(to: string, opts: CallOptions): Promise<CallResult> {
  if (!opts?.goal?.trim() || !opts?.onBehalfOf?.trim()) throw new CallError("call.place(to, { goal, onBehalfOf }) needs both: what the call is for, and who it is for");
  const number = e164(to);
  const minutes = Math.min(Math.max(opts.maxMinutes ?? 10, 1), CALL_MAX_MINUTES);
  const k = await keys();
  if (!Bun.which("cloudflared")) throw new CallError("call: cloudflared is not installed (Twilio must reach this machine for the call's audio): brew install cloudflared");
  // He decides each call: it reaches a real person and is billed.
  const said = await ask(`Call ${number}?`, { detail: opts.goal.slice(0, 300), options: ["Call", "Don't call"] });
  if (said !== "Call") throw new CallError(`call: not placed: he answered ${JSON.stringify(said)}`);

  const token = crypto.randomUUID();
  const bridge = new Bridge(opts);
  const { promise: over, resolve: finish } = Promise.withResolvers<string>();
  let ai: WebSocket | undefined;
  let line: { send(s: string): unknown } | undefined;
  const run = (acts: Act[]) => {
    for (const a of acts) {
      if (a.to === "openai") { if (ai?.readyState === WebSocket.OPEN) ai.send(JSON.stringify(a.send)); }
      else if (a.to === "twilio") line?.send(JSON.stringify(a.send));
      else setTimeout(() => finish("completed"), a.after_ms);
    }
  };
  const server = Bun.serve({
    hostname: "127.0.0.1", port: 0,
    fetch(req, srv) {
      const url = new URL(req.url);
      if (url.pathname === `/media/${token}`) return srv.upgrade(req) ? undefined : new Response("expected a websocket", { status: 400 });
      if (url.pathname === `/status/${token}` && req.method === "POST") {
        void req.formData().then((f) => { const s = String(f.get("CallStatus") ?? ""); if (["completed", "busy", "no-answer", "failed", "canceled"].includes(s)) finish(s); });
        return new Response("");
      }
      return new Response("not found", { status: 404 });
    },
    websocket: {
      open(ws) {
        line = ws;
        ai = new WebSocket(`wss://api.openai.com/v1/realtime?model=${CALL_MODEL}`, { headers: { Authorization: `Bearer ${k.openai}` } } as unknown as string[]);
        ai.onopen = () => run(bridge.opened());
        ai.onmessage = (m) => { try { run(bridge.fromOpenAI(JSON.parse(String(m.data)))); } catch { /* a frame that is not JSON: nothing to relay */ } };
        ai.onclose = () => finish("completed");
        ai.onerror = () => finish("failed");
      },
      message(_ws, data) { try { run(bridge.fromTwilio(JSON.parse(String(data)))); } catch { /* as above */ } },
      close() { finish("completed"); },
    },
  });
  const tunnel = Bun.spawn(["cloudflared", "tunnel", "--no-autoupdate", "--url", `http://127.0.0.1:${server.port}`], { stdout: "ignore", stderr: "pipe" });
  const started = Date.now();
  let sid = "";
  try {
    // The tunnel's address is in its log; then wait until it answers from outside.
    let log = "";
    const reader = tunnel.stderr.getReader();
    const dec = new TextDecoder();
    let host: string | null = null;
    while (!host && Date.now() - started < 30_000) {
      const { value, done } = await reader.read();
      if (done) break;
      log += dec.decode(value);
      host = tunnelUrl(log);
    }
    reader.releaseLock();
    if (!host) throw new CallError(`call: cloudflared gave no tunnel address within 30s: ${log.trim().split("\n").slice(-2).join(" ")}`);
    for (let i = 0; i < 20; i++) {
      const up = await fetch(`${host}/ping`, { signal: AbortSignal.timeout(3000) }).then((r) => r.status === 404, () => false);
      if (up) break;
      if (i === 19) throw new CallError(`call: the tunnel at ${host} never answered`);
      await Bun.sleep(1000);
    }
    const made = await twilio(k, "Calls.json", {
      To: number, From: k.from, Twiml: twiml(`${host.replace(/^https/, "wss")}/media/${token}`),
      StatusCallback: `${host}/status/${token}`, StatusCallbackEvent: "completed", TimeLimit: String(minutes * 60),
    });
    sid = String(made.sid ?? "");
    const status = await Promise.race([over, Bun.sleep(minutes * 60_000 + 30_000).then(() => "timeout")]);
    // Whatever ended it here, the call itself ends too.
    await twilio(k, `Calls/${sid}.json`, { Status: "completed" }).catch(() => {});
    const final = await twilio(k, `Calls/${sid}.json`).catch(() => ({}) as Record<string, unknown>);
    return {
      to: number, sid, status: status === "completed" ? String(final.status ?? status) : status,
      seconds: Number(final.duration ?? Math.round((Date.now() - started) / 1000)), outcome: bridge.outcome, transcript: table(bridge.lines),
    };
  } finally {
    ai?.close();
    void server.stop(true);
    tunnel.kill();
  }
}

export const call = {
  /** Call `to` for the person: asks him first, dials, speaks toward `goal`, and returns the status, outcome and transcript. */
  place: (to: string, opts: CallOptions): Promise<CallResult> =>
    effect("net", "On a phone call", "Made a phone call", to, () => place(to, opts), (r) => ({ ok: r.status === "completed", detail: `${r.status} · ${r.seconds}s` })),
};

export const callDocs = {
  call:
    "call.place(to: '+15551234567', { goal, onBehalfOf, voice?, greeting?, maxMinutes? = 10 }): Promise<{ to; sid; status: completed|no-answer|busy|failed|canceled|timeout; seconds; outcome; transcript: Table<{ who: them|assistant; text }> }> — a real phone call: Twilio dials, an OpenAI Realtime voice speaks for him toward goal (it says it is an AI assistant calling for onBehalfOf) and hangs up with the outcome. It shows him a Call / Don't call question first and dials only on Call; it is billed and reaches a real person, so write the goal as you would brief a careful assistant, and never call a number he did not give or approve. The cell waits for the whole call: pass yield_ms 0 to ts, or expect it backgrounded. Needs keys in ~/.harness/keys (the error says which).",
};
