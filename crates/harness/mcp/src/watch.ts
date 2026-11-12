// A cell whose tool call replied "backgrounded" is watched: the agent is told when it ends, and while it runs each
// time it passes the next multiple of its estimate (patience.ts: noticeAt). Nothing is told for a cell the call
// still waits on: its reply carries the time.
import { clip } from "./disclose.ts";
import { noticeAt, told } from "./patience.ts";
import type { Task } from "./tasks.ts";
import { tellAgent } from "./tell.ts";

/** Characters of a cell's result in the notice of its end; the rest is `task(id).result`. */
const RESULT = 2000;

/** Watch `t` from now until it ends. Call once, when its tool call replies while it still runs. `sessionMs` is the session's waited time now. */
export function watch(t: Task, sessionMs: () => number) {
  let sent = 0;
  let timer: Timer | undefined;
  const arm = () => {
    const at = noticeAt(t.expect_ms, sent);
    if (at === undefined || t.status !== "running") return;
    timer = setTimeout(() => {
      if (t.status !== "running") return;
      sent++;
      const { time, advice } = told({ elapsed_ms: t.elapsed_ms, expect_ms: t.expect_ms, running: true, session_ms: sessionMs() });
      tellAgent(t, `[${t.id} still running ${time}] ${advice ?? "It is past the point you asked to hear about; check on it or stop it."} (task('${t.id}') shows its logs so far; cancel('${t.id}') stops it.)`, { tag: "time" });
      arm();
    }, Math.max(0, at - t.elapsed_ms));
  };
  arm();
  void t.done.then(() => {
    clearTimeout(timer);
    const { time, advice } = told({ elapsed_ms: t.elapsed_ms, expect_ms: t.expect_ms, running: false, session_ms: sessionMs() });
    const more = t.result.length > RESULT ? `\n# … ${t.result.length - RESULT} more characters: task('${t.id}').result` : "";
    tellAgent(t, `[${t.id} ${t.status} ${time}]${advice ? `\n${advice}` : ""}\n${clip(t.result, RESULT)}${more}`, { tag: "time" });
  });
}
