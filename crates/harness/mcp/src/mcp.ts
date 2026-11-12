#!/usr/bin/env bun
// MCP server (stdio) exposing one tool, `ts`: a persistent TypeScript REPL with every harness function in scope.
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import { functions, docs } from "./functions/index.ts";
import { Repl } from "./repl.ts";
import { setConfirm } from "./host.ts";
import { gate, limit, told } from "./patience.ts";
import { sessionMs } from "./tasks.ts";
import { watch } from "./watch.ts";

// stdout is the MCP channel; anything logged outside a cell must go to stderr.
// A promise nobody awaited that rejects (a timer's, a dropped connection's) fails nothing but itself: it is said on stderr
// and the kernel keeps serving. Bun's default ends the process, and with it the REPL's state and the agent's turn.
process.on("unhandledRejection", (e) => { console.error("[kernel] an unawaited promise rejected:", e); });
console.log = console.error;

// cursor runs in an empty private workspace (so it loads no repo rules) and passes the real one here.
if (process.env.HARNESS_CWD) process.chdir(process.env.HARNESS_CWD);

const repl = new Repl(functions);
// Agents with no system-prompt flag (cursor) get the harness note as MCP server instructions.
const instructions = process.env.HARNESS_MCP_INSTRUCTIONS
  ? `${process.env.HARNESS_MCP_INSTRUCTIONS} The working directory is ${process.cwd()} (ts cells run there).`
  : undefined;
const server = new McpServer({ name: "harness", version: "0.1.0" }, { instructions });

server.registerTool(
  "ts",
  {
    description: [
      "Run TypeScript in a persistent Bun REPL. Top-level const/let/function/import persist across calls; top-level await works;",
      "the value of the last expression is returned; console.log output is captured. All Bun/Node APIs and fetch are available.",
      "Every cell is a task (t1, t2, ...) and cells run concurrently. The reply starts with a header saying its time against your estimate:",
      "'[t3 done 0.12s]', '[t3 done 1m12s · expected 20.0s · 3.6× over]', '[t3 error ...]'. You do not feel a wait, so time is told to you: read it.",
      "Say expect_ms for anything that is not instant. A cell expected to take over 5 minutes is refused unless `why` says what it tells you that nothing faster can:",
      "something that slow usually means the approach is wrong (a smaller input, one test, a simulation, a cached copy, an existing profile).",
      "With expect_ms the cell is stopped at 4× it (or timeout_ms); a result well over its estimate comes with a line saying to find out why before repeating it.",
      "A cell still running after yield_ms (default 30000) is backgrounded: the call returns '[t3 running 30.0s · expected 2m00s, backgrounded]' with logs so far, the cell",
      "keeps going, and you are told in a message when it ends, and before that each time it passes 2×, 4×, 8× and 16× its estimate. So do not poll or sleep:",
      "go on with other work; tasks(), task('t3') and `await wait('t3')` read it when you need to (logs and result are kept).",
      "Use the typed calls, which return data and show the person what they did: Bun.file(p).text() to read, rg to search, glob/ls/du to list,",
      "cargo.check/build/test/clippy or follow(cmd) for builds and tests (a digest, not the log), gitLog() for history, github.* for GitHub, fetch for HTTP.",
      "npm packages import by name, optionally versioned (`import { z } from 'zod'`, `await import('lodash@4.17.21')`): installed once into a shared cache and pinned, so a re-run gets the same version.",
      "The shell ($`cmd`, sh) is only for running programs no typed call covers; a command that reads files or the tree, `gh api`/`gh pr`/`gh issue` reads, and curl/wget",
      "are refused with the call to use. Long-lived processes: sh(...), read with out(p, { new: true }).",
      "Results print as NUON (compact JSON: bare strings/keys; lists of records as [[cols]; [row], ...]), capped at 100 rows with a '# … N more rows' note. Strings print raw.",
      "Functions in scope:",
      ...Object.values(docs).map((d) => `- ${d}`),
    ].join("\n"),
    inputSchema: {
      code: z.string().describe("TypeScript to run"),
      yield_ms: z.number().optional().describe("wait this long before backgrounding; default 30000"),
      timeout_ms: z.number().optional().describe("hard limit for the cell, rejecting it; default 4x expect_ms, or none when expect_ms is not given"),
      expect_ms: z.number().optional().describe("how long you expect the cell to take, in ms; the reply and later messages are measured against it. Over 300000 needs `why`"),
      why: z.string().optional().describe("for a cell expected to take over 5 minutes: one sentence saying what it tells you that nothing faster can"),
    },
  },
  // 30 s: a cell that finishes in that time answers in the same call. Backgrounding one sooner costs the agent a
  // whole turn to come back for it (about 6 s of its own, measured 2026-11-10), which is longer than most waits it saved.
  async ({ code, yield_ms = 30_000, timeout_ms, expect_ms, why }, extra) => {
    // The moment to doubt a slow run is before it starts (patience.ts).
    const refused = gate(expect_ms, why);
    if (refused !== null) return { content: [{ type: "text", text: refused }], isError: true };
    // The agent names the tool call in the request's _meta (Claude as `claudecode/toolUseId`, Codex as `callId`, the
    // same id its app-server gives the mcpToolCall item); the cell carries it so the app draws the cell as that call.
    const meta = extra._meta ?? {};
    const call = [meta["claudecode/toolUseId"], meta["callId"]].find((c) => typeof c === "string" && c !== "");
    const t = repl.run(code, limit(expect_ms, timeout_ms), typeof call === "string" ? call : null, { expect_ms, why });
    let timer: Timer | undefined;
    await Promise.race([t.done, new Promise((r) => (timer = setTimeout(r, yield_ms)))]);
    clearTimeout(timer);
    const running = t.status === "running";
    const { time, advice } = told({ elapsed_ms: t.elapsed_ms, expect_ms, running, session_ms: sessionMs() });
    const logs = t.logs.length ? t.logs.join("\n") + "\n" : "";
    const note = advice ? `${advice}\n` : "";
    if (running) {
      t.returned = true;
      // From here the cell's time reaches the agent as messages: when it ends, and each time it passes its estimate again.
      watch(t);
      const text = `[${t.id} running ${time}, backgrounded: you will be told when it ends or runs over; go on with other work]\n${note}${logs}`.trimEnd();
      return { content: [{ type: "text", text }] };
    }
    return { content: [{ type: "text", text: `[${t.id} ${t.status} ${time}]\n${note}${logs}${t.result}` }], isError: t.status === "error" };
  },
);

// Functions that need the person (e.g. gmail.send asking before it sends) ask through MCP elicitation.
// Clients without it get confirm() === false, and the function's error text reaches the model instead.
setConfirm(async (message) => {
  if (!server.server.getClientCapabilities()?.elicitation) return false;
  const r = await server.server.elicitInput({ message, requestedSchema: { type: "object", properties: {} } }).catch((e) => {
    console.error("elicitation failed:", e);
    return undefined;
  });
  return r?.action === "accept";
});

await server.connect(new StdioServerTransport());
