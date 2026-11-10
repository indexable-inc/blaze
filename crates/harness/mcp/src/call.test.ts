import { expect, test } from "bun:test";
import { Bridge, e164, instructions, session, tunnelUrl, twiml } from "./call.ts";

const opts = { goal: "Ask whether they open on Sunday.", onBehalfOf: "Sam" };

test("a number is international or refused", () => {
  expect(e164("+1 (555) 123-4567")).toBe("+15551234567");
  for (const bad of ["5551234567", "+0123", "call mom", ""]) expect(() => e164(bad)).toThrow();
});

test("the TwiML streams to the websocket, escaped", () => {
  expect(twiml("wss://a.trycloudflare.com/media/x?y=1&z=2")).toBe('<Response><Connect><Stream url="wss://a.trycloudflare.com/media/x?y=1&amp;z=2"/></Connect></Response>');
});

test("the tunnel's address is read from cloudflared's log", () => {
  expect(tunnelUrl("INF |  https://calm-tea-leaf.trycloudflare.com  |")).toBe("https://calm-tea-leaf.trycloudflare.com");
  expect(tunnelUrl("INF starting")).toBeNull();
});

test("the voice is told who it calls for, that it is an AI, and the goal; the session is mu-law both ways", () => {
  const words = instructions(opts);
  for (const part of ["Sam", "AI assistant", "open on Sunday", "hang_up"]) expect(words).toContain(part);
  const s = session(opts).session;
  expect([s.audio.input.format.type, s.audio.output.format.type]).toEqual(["audio/pcmu", "audio/pcmu"]);
  expect(s.tools[0]!.name).toBe("hang_up");
});

// The invariant: audio from either side goes to the other and nowhere else; talking over the voice clears what is
// queued; what was said and the outcome are kept; hang_up ends the call after the goodbye has played.
test("the bridge relays a call", () => {
  const b = new Bridge({ ...opts, greeting: "Hello, this is an assistant calling for Sam." });
  expect(b.opened()[0]).toMatchObject({ to: "openai", send: { type: "session.update" } });
  // Nothing is said before both sides are up.
  expect(b.fromOpenAI({ type: "session.updated" })).toEqual([]);
  const greet = b.fromTwilio({ event: "start", start: { streamSid: "MZ1" } });
  expect(greet.map((a) => (a.to === "openai" ? a.send.type : a.to))).toEqual(["conversation.item.create", "response.create"]);
  expect(b.fromTwilio({ event: "start", start: { streamSid: "MZ1" } })).toEqual([]);
  expect(b.fromTwilio({ event: "media", media: { payload: "AAAA" } })).toEqual([{ to: "openai", send: { type: "input_audio_buffer.append", audio: "AAAA" } }]);
  expect(b.fromOpenAI({ type: "response.output_audio.delta", delta: "BBBB" })).toEqual([{ to: "twilio", send: { event: "media", streamSid: "MZ1", media: { payload: "BBBB" } } }]);
  // They talk over it.
  expect(b.fromOpenAI({ type: "input_audio_buffer.speech_started" }).map((a) => (a.to === "twilio" ? a.send.event : a.to === "openai" ? a.send.type : a.to))).toEqual(["clear", "response.cancel"]);
  // Talking while the voice is silent interrupts nothing.
  expect(b.fromOpenAI({ type: "input_audio_buffer.speech_started" })).toEqual([]);
  b.fromOpenAI({ type: "conversation.item.input_audio_transcription.completed", transcript: " Yes, ten to four. " });
  b.fromOpenAI({ type: "response.output_audio_transcript.done", transcript: "Thank you, goodbye." });
  expect(b.lines).toEqual([{ who: "them", text: "Yes, ten to four." }, { who: "assistant", text: "Thank you, goodbye." }]);
  expect(b.fromOpenAI({ type: "response.function_call_arguments.done", name: "hang_up", arguments: '{"outcome":"Open Sunday 10 to 4."}' })).toEqual([{ to: "end", after_ms: 2500 }]);
  expect(b.outcome).toBe("Open Sunday 10 to 4.");
  expect(b.fromTwilio({ event: "stop" })).toEqual([{ to: "end", after_ms: 0 }]);
});
