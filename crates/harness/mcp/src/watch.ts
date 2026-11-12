// A cell whose tool call replied "backgrounded" is watched: the agent is told when it ends, and while it runs each
// time it passes the next multiple of its estimate (patience.ts: noticeAt). Nothing is told for a cell the call
// still waits on: its reply carries the time.
import { noticeAt, span, told } from "./patience.ts";
import { sessionMs, type Task } from "./tasks.ts";
import { tellAgent } from "./tell.ts";

const head = (s: string, n: number) => (s.length > n ? `${s.slice(0, n)}… (${s.length - n} more characters: task('ID').result)` : s);

/** Watch `t` from now until it ends. Call once, when its tool call replies while it still runs. */
export function watch(t: Task) {
  let sent = 0;
  let timer: Timer | undefined;
  const arm = () => {
    const at = noticeAt(t.expect_ms, sent);
    if (at === undefined || t.status !== "running") return;
    timer = setTimeout(() => {
      if (t.status !== "running") return;
      sent++;
      const { time, advice } = told({ elapsed_ms: t.elapsed_ms, expect_ms: t.expect_ms, running: true, session_ms: sessionMs() });
      tellAgent(t, `[${t.id} still running ${time}] ${advice ?? "It is past the point you asked to hear about; check on it or stop it."} (task('${t.id}') shows its logs so far.)`, "time");
      arm();
    }, Math.max(0, at - t.elapsed_ms));
  };
  arm();
  void t.done.then(() => {
    clearTimeout(timer);
    const { time, advice } = told({ elapsed_ms: t.elapsed_ms, expect_ms: t.expect_ms, running: false, session_ms: sessionMs() });
    const result = head(t.result, 2000).replace("task('ID')", `task('${t.id}')`);
    tellAgent(t, `[${t.id} ${t.status} ${time}]${advice ? `\n${advice}` : ""}\n${result}`, "time");
  });
}
