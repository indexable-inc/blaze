#!/usr/bin/env bun
// MCP server (stdio) exposing one tool, `ts`: a persistent TypeScript REPL with every harness function in scope.
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import { functions, docs } from "./functions/index.ts";
import { Repl } from "./repl.ts";

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
      "Functions in scope:",
      ...Object.values(docs).map((d) => `- ${d}`),
    ].join("\n"),
    inputSchema: { code: z.string().describe("TypeScript to run"), timeout_ms: z.number().optional().describe("default 60000") },
  },
  async ({ code, timeout_ms }) => {
    const { ok, result, logs } = await repl.run(code, timeout_ms);
    const text = (logs.length ? logs.join("\n") + "\n" : "") + result;
    return { content: [{ type: "text", text }], isError: !ok };
  },
);

await server.connect(new StdioServerTransport());
