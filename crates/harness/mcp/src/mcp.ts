#!/usr/bin/env bun
// MCP server (stdio) exposing one tool, `ts`: a persistent, type-checked TypeScript REPL with every harness function in
// scope. The cells run in lanes (lanes.ts), one evaluator process each; this process checks them, and says how they ended.
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import { docs } from "./functions/index.ts";
import { LANE_NAME, Lanes, seconds, stateDirFor, type Cell } from "./lanes.ts";

// stdout is the MCP channel; anything logged outside a cell must go to stderr.
console.log = console.error;

// cursor runs in an empty private workspace (so it loads no repo rules) and passes the real one here.
if (process.env.HARNESS_CWD) process.chdir(process.env.HARNESS_CWD);

// Agents with no system-prompt flag (cursor) get the harness note as MCP server instructions.
const instructions = process.env.HARNESS_MCP_INSTRUCTIONS
  ? `${process.env.HARNESS_MCP_INSTRUCTIONS} The working directory is ${process.cwd()} (ts cells run there).`
  : undefined;
const server = new McpServer({ name: "harness", version: "0.2.0" }, { instructions });

// Functions that need the person (e.g. "start Dia with remote debugging") ask through MCP elicitation.
// Clients without it get confirm() === false, and the function's error text reaches the model instead.
const lanes = new Lanes({
  stateDir: stateDirFor(),
  async confirm(message) {
    if (!server.server.getClientCapabilities()?.elicitation) return false;
    const r = await server.server.elicitInput({ message, requestedSchema: { type: "object", properties: {} } }).catch((e) => {
      console.error("elicitation failed:", e);
      return undefined;
    });
    return r?.action === "accept";
  },
});
// The main lane starts now: its saved bindings (a resumed chat) are back, and the checker warm, before the first cell.
void lanes.start(lanes.lane("main")).catch((e) => console.error("[harness] starting lane main:", e));

export const DESCRIPTION = [
  "Run TypeScript in a persistent Bun REPL. Top-level const/let/function/import persist across cells; top-level await works;",
  "the last expression is the result (strings raw, other values as NUON capped at 100 rows); console output is captured.",
  "Each cell is type-checked first against the globals below and every name earlier cells bound: a type error refuses the cell,",
  "naming the line, and so does `as any`, `as never`, `: any` or `@ts-ignore` (fix the type; `as unknown as T` needs a comment saying why).",
  "Data from outside (JSON.parse, res.json()) is unknown: `parse<{ id: number }>(value)` checks it and throws naming every field that does not fit.",
  "A reply opens 't3 completed in 1.24 s' or 't3 failed after ...'; `$_` is the last result, `repl.result('t3')` a cell's value. Output past 20,000 characters keeps its start and end and names a file holding all of it.",
  "A cell still running after yield_ms (default 10000) keeps running: its result comes with your next ts call; call with no code to wait for it.",
  "Cells in one lane share bindings and may overlap; lanes (`lane`, default 'main') run in parallel with their own bindings.",
  "`cancel: true` stops the lane's running cells (or `task`'s), `timeout_ms` does so for this cell, `reset: true` empties the lane: each clears that lane's bindings.",
  "Bindings that can be copied (data, not functions, class instances or handles) survive a restart of the chat; the first reply after says which did not.",
  "Use the typed calls, which return data and show the person what they did: Bun.file(p).text() to read, rg to search, glob/ls/du to list,",
  "cargo.check/build/test/clippy or follow(cmd) for builds and tests (a digest, not the log), gitLog() for history. The shell ($`cmd`, sh) is for",
  "running programs; a command that only reads files or the tree is refused with the call to use. Long-lived processes: sh(...), read with out(p, { new: true }).",
  "Functions in scope:",
  ...Object.values(docs).map((d) => `- ${d}`),
].join("\n");

const inputSchema = {
  code: z.string().optional().describe("TypeScript to run; omit to wait for running cells"),
  lane: z.string().regex(LANE_NAME).optional().describe("evaluator to run in, with its own bindings; default main"),
  task: z.string().regex(/^t\d+$/).optional().describe("the cell (t3) to wait for or, with cancel, to stop"),
  cancel: z.boolean().optional().describe("stop the lane's running cells (or task's lane); clears that lane's bindings"),
  reset: z.boolean().optional().describe("stop the lane and clear its bindings before running code"),
  yield_ms: z.number().int().min(0).max(600_000).optional().describe("how long to wait before replying that the cell is still running; default 10000"),
  timeout_ms: z.number().int().min(100).optional().describe("stop the cell (and its lane, clearing its bindings) after this long; default none"),
};

/** How a cell ended, or how far it is: its header, then its output. */
async function describe(c: Cell, earlier = false): Promise<string> {
  const where = c.lane.name === "main" ? "" : ` (lane ${c.lane.name})`;
  const when = earlier ? " (in the background, earlier)" : "";
  if (c.ended === undefined) {
    const so = await lanes.peek(c);
    return [
      `${c.id}${where} is still running after ${seconds(Date.now() - c.started)}. Its result comes back with your next ts call; call ts with no code to wait for it, or cancel: true to stop it.`,
      so && `Output so far:\n${so}`,
    ].filter(Boolean).join("\n");
  }
  c.delivered = true;
  const head = `${c.id}${where}${when} ${c.ok ? "completed in" : "failed after"} ${seconds(c.ended - c.started)}`;
  return c.text ? `${head}\n${c.text}` : head;
}

/** Cells that ended since the last reply without being said (they outlived their call). */
function backlog(except?: Cell): Cell[] {
  return [...lanes.cells.values()].filter((c) => c !== except && c.ended !== undefined && !c.delivered);
}

async function reply(parts: string[], current: Cell | undefined, isError: boolean) {
  const out: string[] = [];
  for (const lane of lanes.lanes.values()) out.push(...lane.notes.splice(0));
  for (const c of backlog(current)) out.push(await describe(c, true));
  if (current) {
    // A cell that ended is said now and only now; one still running is said again when it ends.
    if (current.ended !== undefined) current.delivered = true;
    out.push(await describe(current));
  }
  out.push(...parts);
  const others = lanes.running().filter((c) => c !== current);
  if (others.length) out.push(`Still unfinished: ${others.map((c) => `${c.id} (${c.lane.name}, ${seconds(Date.now() - c.started)})`).join(", ")}.`);
  // Cells done in the background are kept until said; once said, only the running and the last few are worth holding.
  for (const [id, c] of lanes.cells) if (c.delivered && lanes.cells.size > 200) lanes.cells.delete(id);
  const text = out.filter((s) => s !== "").join("\n\n") || "Nothing is running.";
  return { content: [{ type: "text" as const, text }], isError: isError || (current?.ended !== undefined && current.ok === false) };
}

const waitFor = (done: Promise<unknown>, ms: number) => {
  let timer: Timer | undefined;
  return Promise.race([done, new Promise((r) => (timer = setTimeout(r, ms)))]).finally(() => clearTimeout(timer));
};

server.registerTool("ts", { description: DESCRIPTION, inputSchema }, async (input, extra) => {
  const { code, task, cancel, reset, yield_ms = 10_000, timeout_ms } = input;
  const named = task ? lanes.cells.get(task) : undefined;
  if (task && !named) return reply([`No cell ${task} in this server (task ids start over when the chat restarts).`], undefined, true);
  const lane = lanes.lane(input.lane ?? named?.lane.name ?? "main");
  const parts: string[] = [];

  if (cancel) {
    if (!lane.running.size) parts.push(`Nothing is running in lane ${lane.name}; its bindings are kept.`);
    else {
      const stopped = await lanes.kill(lane, `cancelled; lane ${lane.name}'s bindings are gone`);
      parts.push(`Cancelled ${stopped.map((c) => c.id).join(", ")}: lane ${lane.name} stopped and its bindings are gone (its next cell starts empty).`);
      for (const c of stopped) c.delivered = true;
    }
    if (code === undefined) return reply(parts, undefined, false);
  }
  if (reset) {
    const stopped = await lanes.kill(lane, `reset; lane ${lane.name}'s bindings are gone`);
    for (const c of stopped) c.delivered = true;
    parts.push(`Lane ${lane.name} reset: its bindings are gone${stopped.length ? ` (stopped ${stopped.map((c) => c.id).join(", ")})` : ""}.`);
    if (code === undefined) return reply(parts, undefined, false);
  }

  if (code === undefined) {
    // Wait for the named cell, else the oldest running one in the lane asked for, else in any lane.
    const target = named ?? (input.lane ? [...lane.running.values()][0] : lanes.running()[0]);
    if (target && target.ended === undefined) await waitFor(target.done, yield_ms);
    return reply(parts, target && !target.delivered ? target : undefined, false);
  }

  // The agent names the tool call in the request's _meta (Claude as `claudecode/toolUseId`, Codex as `callId`, the
  // same id its app-server gives the mcpToolCall item); the cell carries it so the app draws the cell as that call.
  const meta = extra._meta ?? {};
  const call = [meta["claudecode/toolUseId"], meta["callId"]].find((c) => typeof c === "string" && c !== "");
  let started: Awaited<ReturnType<Lanes["run"]>>;
  try {
    started = await lanes.run(lane, code, typeof call === "string" ? call : null, parts);
  } catch (e) {
    return reply([...parts, `Not run: lane ${lane.name} could not start: ${e instanceof Error ? e.message : String(e)}`], undefined, true);
  }
  if ("refusal" in started) return reply([...parts, started.refusal], undefined, true);
  const c = started;
  if (timeout_ms !== undefined) {
    const timer = setTimeout(() => {
      if (c.ended === undefined) void lanes.kill(c.lane, `timed out after ${seconds(timeout_ms)}; lane ${c.lane.name} was stopped and its bindings are gone`);
    }, timeout_ms);
    void c.done.then(() => clearTimeout(timer));
  }
  await waitFor(c.done, yield_ms);
  return reply(parts, c, false);
});

// Save every lane when the server is told to stop (its client went away), so a resumed chat gets its bindings back.
let closing = false;
async function shutdown() {
  if (closing) return;
  closing = true;
  await lanes.close();
  process.exit(0);
}
process.on("SIGTERM", shutdown);
process.on("SIGINT", shutdown);
process.stdin.on("end", shutdown);
process.stdin.on("close", shutdown);

await server.connect(new StdioServerTransport());
