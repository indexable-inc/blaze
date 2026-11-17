// The invariant: every notice reaches the agent exactly once; none is typed into its chat while one of its calls to this
// kernel is in flight (the reply carries it) or, unless urgent, while it is busy in a turn and the notice is younger
// than HOLD_MS; and none waits past HOLD_MS for a busy agent. Simulated on a virtual clock over random schedules, with
// a planted bug the oracle must catch.
import { expect, test } from "bun:test";
import { HOLD_MS, Notices, type Due, type Notice } from "./notices.ts";

const rng = (seed: number) => () => (seed = (seed * 48271) % 2147483647) / 2147483647;

/** A queue that types a notice into a busy agent's chat at once: the bug the rules are there to prevent. */
class Impatient extends Notices {
  override due(now: number, _busy: boolean | undefined): Due { return super.due(now, false); }
}

/** Run one random schedule; the first violation, or null. */
function simulate(seed: number, make: () => Notices): string | null {
  const r = rng(seed);
  const q = make();
  let now = 0, calls = 0, busy = false, made = 0;
  /** What the daemon last said of the agent: busy, idle, or nothing (it could not be asked). */
  let said: boolean | undefined = false;
  const added = new Map<string, { at: number; urgent: boolean }>();
  const got = new Map<string, number>();
  const take = (ns: Notice[], how: "reply" | "typed"): string | null => {
    for (const n of ns) {
      if (got.has(n.text)) return `seed ${seed}: ${n.text} delivered twice`;
      got.set(n.text, now);
      const a = added.get(n.text)!;
      if (how === "typed" && calls > 0) return `seed ${seed}: ${n.text} typed while a call was in flight`;
      if (how === "typed" && said === true && !ns.some((x) => x.urgent) && now - Math.min(...ns.map((x) => added.get(x.text)!.at)) < HOLD_MS) return `seed ${seed}: ${n.text} typed into a busy agent ${now - a.at} ms after it was queued`;
    }
    return null;
  };
  let again: number | undefined;
  for (let step = 0; step < 200; step++) {
    now += Math.floor(r() * 5000);
    const pick = r();
    let bad: string | null = null;
    if (pick < 0.3) { const text = `n${made++}`; const urgent = r() < 0.15; added.set(text, { at: now, urgent }); q.add({ task: "t1", text, urgent }, now); }
    else if (pick < 0.45) { q.callBegan(); calls++; }
    else if (pick < 0.6 && calls > 0) { bad = take(q.callEnds(), "reply"); calls--; }
    else if (pick < 0.75) busy = !busy;
    if (bad) return bad;
    // The driver: asks when something was added, when a call ended, and when it was told to ask again.
    said = r() < 0.1 ? undefined : busy;
    const due = q.due(now, said);
    if (due && "send" in due) { bad = take(due.send, "typed"); if (bad) return bad; again = undefined; }
    else if (due) { if (due.again_at <= now) return `seed ${seed}: asked to look again at ${due.again_at}, which is not after ${now}`; again = due.again_at; }
    else again = undefined;
    for (const [text, a] of added) {
      if (!got.has(text) && calls === 0 && now - a.at > HOLD_MS + 5000) return `seed ${seed}: ${text} still waits ${now - a.at} ms after it was queued, with no call in flight`;
    }
  }
  // Quiesce: calls end, the agent goes idle, the driver asks once more.
  while (calls > 0) { const bad = take(q.callEnds(), "reply"); if (bad) return bad; calls--; }
  said = busy = false;
  const last = q.due(now + HOLD_MS, false);
  if (last && "send" in last) { const bad = take(last.send, "typed"); if (bad) return bad; }
  if (got.size !== added.size || q.pending) return `seed ${seed}: ${added.size - got.size} notices were never delivered`;
  void again;
  return null;
}

test("every notice arrives once, at a sensible moment, over 500 random schedules", () => {
  for (let seed = 1; seed <= 500; seed++) expect(simulate(seed, () => new Notices())).toBeNull();
});

test("the oracle catches a queue that types into a busy agent's chat", () => {
  const caught = Array.from({ length: 500 }, (_, i) => simulate(i + 1, () => new Impatient())).filter(Boolean);
  expect(caught.length).toBeGreaterThan(50);
  expect(caught[0]).toContain("typed into a busy agent");
});

test("the plain cases", () => {
  const q = new Notices();
  expect(q.due(0, false)).toBeNull();
  q.add({ task: "t1", text: "ended", urgent: false }, 0);
  expect(q.due(10, true)).toEqual({ again_at: 2010 });
  expect(q.due(HOLD_MS - 500, true)).toEqual({ again_at: HOLD_MS });
  expect(q.due(HOLD_MS, true)).toEqual({ send: [{ task: "t1", text: "ended", urgent: false }] });
  q.add({ task: "t2", text: "answer", urgent: true }, 0);
  q.callBegan();
  expect(q.due(1, false)).toBeNull();
  expect(q.callEnds()).toHaveLength(1);
  expect(q.pending).toBe(0);
});
