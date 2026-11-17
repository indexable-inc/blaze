// One way to say something to the agent after its tool call has replied: a line in the cell's logs, and a notice
// (notices.ts decides when it goes): on the agent's next reply from this kernel, or as a message typed into its chat by
// the daemon (`rpc prompt`, which wakes an agent that has stopped). Used for the person's answer to a cell's question
// (ui/index.ts), a backgrounded cell's time (watch.ts) and an unawaited rejection (kernel.ts).
import { req } from "./daemon.ts";
import { Notices, noticeText, type Notice } from "./notices.ts";
import { session } from "./self.ts";
import type { Task } from "./tasks.ts";
import type { SessionInfo } from "./wire.ts";

export const notices = new Notices();
let timer: Timer | undefined;

/** Whether the agent is in a turn, from the daemon; undefined when it cannot say (no session, no daemon). */
async function busy(sid: number): Promise<boolean | undefined> {
  try {
    const { sessions } = await req<{ sessions: SessionInfo[] }>({ op: "list" });
    return sessions.find((s) => s.id === sid)?.busy;
  } catch { return undefined; }
}

/** Send what is due now; when the agent is busy, look again when notices.ts says. */
export async function pump(): Promise<void> {
  clearTimeout(timer);
  const sid = session();
  // A kernel run by hand has no chat to type into: its notices ride on the next reply.
  if (sid === null || !notices.pending) return;
  const due = notices.due(Date.now(), await busy(sid));
  if (!due) return;
  if ("again_at" in due) { timer = setTimeout(() => void pump(), Math.max(0, due.again_at - Date.now())); return; }
  try { await req({ op: "rpc", id: `s${sid}`, method: "prompt", args: { text: noticeText(due.send), submit: true } }); }
  catch (error) {
    // Not lost: back in the queue for the next reply, and said in each cell's logs.
    for (const n of due.send) notices.add(n, Date.now());
    console.error(`[kernel] ${due.send.length} notice(s) could not be sent to the agent (they go with its next reply): ${error instanceof Error ? error.message : String(error)}`);
  }
}

export function tellAgent(t: Task, text: string, o: { tag?: string; urgent?: boolean; end?: boolean } = {}) {
  t.log(`[${o.tag ?? "ask"}] ${text}`);
  const n: Notice = { task: t.id, text, urgent: o.urgent ?? false, ...(o.end ? { end: true } : {}) };
  notices.add(n, Date.now());
  void pump();
}
