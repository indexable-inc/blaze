#!/usr/bin/env bun
// MCP server (stdio) exposing one tool, `ts`: a persistent TypeScript REPL with every harness function in scope.
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import { functions, docs } from "./functions/index.ts";
import { Repl } from "./repl.ts";
import { setConfirm } from "./host.ts";

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
      "Every cell is a task (t1, t2, ...) and cells run concurrently. The reply starts with a header '[t3 done 0.12s]' / '[t3 error ...]';",
      "if the cell is still running after yield_ms (default 30000) the call returns '[t3 running 30.0s, backgrounded]' with logs so far and the cell keeps going:",
      "check with tasks(), task('t3'), or `await wait('t3')` (logs and result are kept).",
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
      timeout_ms: z.number().optional().describe("hard limit for the cell, rejecting it; default none"),
    },
  },
  // 30 s: a cell that finishes in that time answers in the same call. Backgrounding one sooner costs the agent a
  // whole turn to come back for it (about 6 s of its own, measured 2026-11-10), which is longer than most waits it saved.
  async ({ code, yield_ms = 30_000, timeout_ms }, extra) => {
    // The agent names the tool call in the request's _meta (Claude as `claudecode/toolUseId`, Codex as `callId`, the
    // same id its app-server gives the mcpToolCall item); the cell carries it so the app draws the cell as that call.
    const meta = extra._meta ?? {};
    const call = [meta["claudecode/toolUseId"], meta["callId"]].find((c) => typeof c === "string" && c !== "");
    const t = repl.run(code, timeout_ms, typeof call === "string" ? call : null);
    let timer: Timer | undefined;
    await Promise.race([t.done, new Promise((r) => (timer = setTimeout(r, yield_ms)))]);
    clearTimeout(timer);
    const secs = (t.elapsed_ms / 1000).toFixed(2) + "s";
    const logs = t.logs.length ? t.logs.join("\n") + "\n" : "";
    if (t.status === "running") {
      t.returned = true;
      const text = `[${t.id} running ${secs}, backgrounded; await wait('${t.id}') or tasks()]\n${logs}`.trimEnd();
      return { content: [{ type: "text", text }] };
    }
    return { content: [{ type: "text", text: `[${t.id} ${t.status} ${secs}]\n${logs}${t.result}` }], isError: t.status === "error" };
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
