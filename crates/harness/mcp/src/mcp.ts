#!/usr/bin/env bun
// MCP server (stdio) exposing one tool, `ts`: a persistent TypeScript REPL with every harness function in scope.
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import { functions, docs } from "./functions/index.ts";
import { Repl } from "./repl.ts";
import { setConfirm } from "./host.ts";

// stdout is the MCP channel; anything logged outside a cell must go to stderr.
console.log = console.error;

const repl = new Repl(functions);
const server = new McpServer({ name: "harness", version: "0.1.0" });

server.registerTool(
  "ts",
  {
    description: [
      "Run TypeScript in a persistent Bun REPL. Top-level const/let/function/import persist across calls; top-level await works;",
      "the value of the last expression is returned; console.log output is captured. All Bun/Node APIs and fetch are available.",
      "Every cell is a task (t1, t2, ...) and cells run concurrently. The reply starts with a header '[t3 done 0.12s]' / '[t3 error ...]';",
      "if the cell is still running after yield_ms (default 10000) the call returns '[t3 running 10.0s, backgrounded]' with logs so far and the cell keeps going:",
      "check with tasks(), task('t3'), or `await wait('t3')` (logs and result are kept). Long commands: start with sh(...), read with out(p, { new: true }).",
      "Results print as NUON (compact JSON: bare strings/keys; lists of records as [[cols]; [row], ...]), capped at 100 rows with a '# … N more rows' note. Strings print raw.",
      "Functions in scope:",
      ...Object.values(docs).map((d) => `- ${d}`),
    ].join("\n"),
    inputSchema: {
      code: z.string().describe("TypeScript to run"),
      yield_ms: z.number().optional().describe("wait this long before backgrounding; default 10000"),
      timeout_ms: z.number().optional().describe("hard limit for the cell, rejecting it; default none"),
    },
  },
  async ({ code, yield_ms = 10_000, timeout_ms }) => {
    const t = repl.run(code, timeout_ms);
    let timer: Timer | undefined;
    await Promise.race([t.done, new Promise((r) => (timer = setTimeout(r, yield_ms)))]);
    clearTimeout(timer);
    const secs = (t.elapsed_ms / 1000).toFixed(2) + "s";
    const logs = t.logs.length ? t.logs.join("\n") + "\n" : "";
    if (t.status === "running") {
      const text = `[${t.id} running ${secs}, backgrounded; await wait('${t.id}') or tasks()]\n${logs}`.trimEnd();
      return { content: [{ type: "text", text }] };
    }
    return { content: [{ type: "text", text: `[${t.id} ${t.status} ${secs}]\n${logs}${t.result}` }], isError: t.status === "error" };
  },
);

// Functions that need the person (e.g. "start Dia with remote debugging") ask through MCP elicitation.
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
