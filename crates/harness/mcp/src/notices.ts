// When something said to the agent after its tool call replied (a backgrounded cell ended or ran over, the person
// answered a cell's question) reaches it. A pure state machine on an injected clock: tell.ts does the sending.
//
// A notice is a prompt typed into the agent's chat, which is the only way to wake an agent that has stopped. Typed
// while the agent is in the middle of a turn it lands in the middle of other work, so:
//   - while one of the agent's tool calls to this kernel is in flight, every notice rides on that call's reply, which
//     the agent reads at once and which interrupts nothing;
//   - else an urgent notice (the person answered and is waiting) is sent now;
//   - else, while the agent is busy in a turn, notices wait for the turn to end, at most HOLD_MS from the oldest;
//   - else (the agent is idle, or nothing can say whether it is) they are sent now, together as one message.

/** The longest a notice waits for a busy agent's turn to end. */
export const HOLD_MS = 60_000;
/** How often a busy agent is asked again whether its turn has ended. */
export const POLL_MS = 2_000;

export type Notice = { task: string; text: string; urgent: boolean };
export type Due =
  /** Send these now, as one message. */
  | { send: Notice[] }
  /** Nothing to send yet: ask again at this time (the agent is busy). */
  | { again_at: number }
  /** Nothing queued, or a call in flight will carry them. */
  | null;

export class Notices {
  #queue: (Notice & { at: number })[] = [];
  #calls = 0;

  get pending(): number { return this.#queue.length; }
  add(n: Notice, now: number): void { this.#queue.push({ ...n, at: now }); }
  /** One of the agent's tool calls to this kernel began. */
  callBegan(): void { this.#calls++; }
  /** That call is replying: its reply carries every queued notice, which leave the queue. */
  callEnds(): Notice[] {
    this.#calls = Math.max(0, this.#calls - 1);
    return this.#take();
  }
  #take(): Notice[] {
    const out = this.#queue.map(({ task, text, urgent }) => ({ task, text, urgent }));
    this.#queue = [];
    return out;
  }
  /** What to do at `now`. `busy`: the agent is in a turn; undefined when nothing can say. */
  due(now: number, busy: boolean | undefined): Due {
    if (!this.#queue.length || this.#calls > 0) return null;
    const oldest = this.#queue[0]!.at;
    if (busy === true && !this.#queue.some((n) => n.urgent) && now - oldest < HOLD_MS) return { again_at: Math.min(now + POLL_MS, oldest + HOLD_MS) };
    return { send: this.#take() };
  }
}

/** Several notices as the one message the agent reads. */
export const noticeText = (ns: readonly Notice[]): string => ns.map((n) => n.text).join("\n\n");
