#!/usr/bin/env bun
// A lane's evaluator: one process per lane, started by kernel.ts as the leader of its own process group, so stopping a lane
// (cancel, timeout, reset) kills it and everything its cells started. It holds the lane's bindings (repl.ts), posts its cells
// to harnessd as they run, and saves and restores what survives a copy (hibernate.ts). It is told what to do over IPC
// (protocol.ts); stdout is not the MCP channel here, but nothing should go there either.
import { closeSync, existsSync, fsyncSync, openSync, readFileSync, renameSync, writeSync } from "node:fs";
import { dirname } from "node:path";
import { deserialize, serialize } from "node:v8";
import { functions } from "./functions/index.ts";
import { REPL_ID, Repl } from "./repl.ts";
import { setConfirm } from "./host.ts";
import { capped } from "./output.ts";
import { persistable } from "./check/cell-types.ts";
import type { FromEvaluator, SavedLane, ToEvaluator } from "./protocol.ts";

console.log = console.error;
const send = (m: FromEvaluator) => process.send?.(m);
const lane = process.env.HARNESS_LANE ?? "main";
const repl = new Repl(functions, lane);

// A function that needs the person (functions/ via host.ts `confirm`) asks the MCP server, which asks the client by elicitation.
let confirms = 0;
const asking = new Map<number, (ok: boolean) => void>();
setConfirm((message) => new Promise<boolean>((resolve) => {
  const req = ++confirms;
  asking.set(req, resolve);
  send({ type: "confirm", req, message });
}));

/** Names a restore could not bring back: still named as dropped in every save until a cell binds them again. */
let lost: string[] = [];
const running = new Map<string, ReturnType<Repl["run"]>>();
const describe = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** `bytes` at `path` all or nothing: a temporary file, synced, renamed over it, and the directory synced (a crash keeps the old or the new). */
function writeAtomically(path: string, bytes: Uint8Array) {
  const temp = `${path}.${process.pid}.tmp`;
  const fd = openSync(temp, "w", 0o600);
  try { for (let at = 0; at < bytes.length; ) at += writeSync(fd, bytes, at); fsyncSync(fd); } finally { closeSync(fd); }
  renameSync(temp, path);
  const dir = openSync(dirname(path), "r");
  try { fsyncSync(dir); } finally { closeSync(dir); }
}

/** A hibernate file read back; throws naming what is wrong with it. */
export function readSaved(path: string): SavedLane {
  const value: unknown = deserialize(readFileSync(path));
  const saved = value as Partial<SavedLane> | null; // checked field by field below
  if (!saved || typeof saved !== "object" || saved.version !== 1 || !(saved.values instanceof Map) || !Array.isArray(saved.dropped) || typeof saved.cwd !== "string" || !saved.checker) {
    throw new Error(`${path} is not a version 1 lane snapshot`);
  }
  return saved as SavedLane;
}

process.on("message", async (raw: unknown) => {
  const m = raw as ToEvaluator; // the server is this file's only peer (kernel.ts), speaking protocol.ts
  switch (m.type) {
    case "run": {
      const t = repl.run(m.code, m.call, m.id);
      running.set(m.id, t);
      await t.done;
      running.delete(m.id);
      const text = capped([t.logs.join("\n"), t.result].filter((s) => s !== "").join("\n"), m.spill);
      send({ type: "done", id: m.id, ok: t.status === "done", text, bound: t.bound, cwd: process.cwd() });
      return;
    }
    case "peek": {
      const logs = running.get(m.id)?.logs.join("\n") ?? "";
      send({ type: "peeked", id: m.id, text: logs.length > 4000 ? `…${logs.slice(-4000)}` : logs });
      return;
    }
    case "save": {
      try {
        const snap = repl.snapshot();
        const { saved } = snap;
        const dropped = [...snap.dropped, ...lost.filter((n) => !repl.bound.has(n))];
        const state: SavedLane = { version: 1, cwd: process.cwd(), values: saved, dropped, checker: persistable(m.checker, [...saved.keys()]) };
        const bytes = serialize(state);
        writeAtomically(m.path, bytes);
        send({ type: "saved", req: m.req, saved: [...saved.keys()], dropped, bytes: bytes.length });
      } catch (e) {
        send({ type: "failed", req: m.req, error: describe(e) });
      }
      return;
    }
    case "restore": {
      try {
        const state = readSaved(m.path);
        repl.restore(state.values);
        lost = state.dropped;
        if (existsSync(state.cwd)) process.chdir(state.cwd);
        send({ type: "restored", req: m.req, restored: [...state.values.keys()], dropped: state.dropped, checker: state.checker, cwd: process.cwd() });
      } catch (e) {
        send({ type: "failed", req: m.req, error: describe(e) });
      }
      return;
    }
    case "confirmed": {
      asking.get(m.req)?.(m.ok);
      asking.delete(m.req);
      return;
    }
  }
});

// The server is gone (its stdin closed, it crashed): so is this lane.
process.on("disconnect", () => process.exit(0));
send({ type: "ready", repl: REPL_ID, names: repl.globalNames });
