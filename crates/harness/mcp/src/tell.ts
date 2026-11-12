// One way to say something to the agent after its tool call has replied: a line in the cell's logs, and a message in
// the agent's chat, which wakes it. Used for the person's answer to a cell's question (ui/index.ts) and for a
// backgrounded cell's time (watch.ts).
import { req } from "./daemon.ts";
import type { Task } from "./tasks.ts";

export function tellAgent(t: Task, text: string, tag = "ask") {
  t.logs.push(`[${tag}] ${text}`);
  t.touch();
  const sid = process.env.HARNESS_SESSION;
  if (!sid) return;
  req({ op: "rpc", id: `s${sid}`, method: "prompt", args: { text, submit: true } }).catch((error) => {
    t.logs.push(`[${tag}] that could not be sent to the agent: ${error instanceof Error ? error.message : String(error)}`);
    t.touch();
  });
}
