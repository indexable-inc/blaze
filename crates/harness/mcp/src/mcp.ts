#!/usr/bin/env bun
// The kernel as an MCP server on stdio: the adapter agents connect through today. One tool, `ts`, whose every input goes
// to the kernel's one API (kernel.ts) and whose reply is the API's answer; nothing is decided here.
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import { setConfirm } from "./host.ts";
import { toolCall } from "./kernel.ts";
import { start } from "./start.ts";

const kernel = start();
// Agents with no system-prompt flag (cursor) get the harness note as MCP server instructions.
const instructions = process.env.HARNESS_MCP_INSTRUCTIONS
  ? `${process.env.HARNESS_MCP_INSTRUCTIONS} The working directory is ${process.cwd()} (ts cells run there).`
  : undefined;
const server = new McpServer({ name: "harness", version: "0.1.0" }, { instructions });

server.registerTool(
  "ts",
  {
    description: (await kernel.handle({ op: "describe" })).text,
    inputSchema: {
      code: z.string().optional().describe("TypeScript to run as a cell"),
      cancel: z.string().optional().describe("instead of code: the id of a running cell to stop ('t3')"),
      yield_ms: z.number().optional().describe("wait this long for the cell before backgrounding it; default 30000"),
      timeout_ms: z.number().optional().describe("stop the cell after this long; default 4x expect_ms, or never when expect_ms is not given"),
      expect_ms: z.number().optional().describe("how long you expect the cell to take, in ms; over 300000 needs `why`"),
      why: z.string().optional().describe("for a cell expected to take over 5 minutes: one sentence saying what it tells you that nothing faster can"),
    },
  },
  async (input, extra) => {
    // The agent names the tool call in the request's _meta (Claude as `claudecode/toolUseId`, Codex as `callId`, the
    // same id its app-server gives the mcpToolCall item); the cell carries it so the app draws the cell as that call.
    const meta = extra._meta ?? {};
    const call = [meta["claudecode/toolUseId"], meta["callId"]].find((c): c is string => typeof c === "string" && c !== "") ?? null;
    const a = await kernel.handle(toolCall(input, call));
    return { content: [{ type: "text", text: a.text }], isError: a.error };
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
// The client going away ends the kernel (and, through its exit, what its running cells own): the type checker's process
// and the socket would otherwise keep it alive with nobody to serve.
process.stdin.on("close", () => process.exit(0));
