// Messages between the MCP server (mcp.ts, lanes.ts, kernel.ts) and a lane's evaluator (evaluator.ts), one process per lane,
// over Node IPC (JSON). The server owns the type checker and the replies; the evaluator owns the cells' values.
import type { CheckerState } from "./check/cell-types.ts";

/** What a hibernate file holds (one v8 serialization, hibernate.ts): the lane's restorable bindings and their types. */
export interface SavedLane {
  version: 1;
  cwd: string;
  values: Map<string, unknown>;
  /** Names that were bound but could not be saved (functions, class instances, handles, over 32 MB). */
  dropped: string[];
  checker: CheckerState;
}

export type ToEvaluator =
  | { type: "run"; id: string; code: string; call: string | null; spill: string }
  | { type: "peek"; id: string }
  | { type: "save"; req: number; path: string; checker: CheckerState }
  | { type: "restore"; req: number; path: string }
  | { type: "confirmed"; req: number; ok: boolean };

export type FromEvaluator =
  /** `repl`: this evaluator's id in harnessd (the parent of its cells, and of the runs its cells start); `names`: its globals. */
  | { type: "ready"; repl: string; names: string[] }
  | { type: "done"; id: string; ok: boolean; text: string; bound: string[]; cwd: string }
  | { type: "peeked"; id: string; text: string }
  | { type: "saved"; req: number; saved: string[]; dropped: string[]; bytes: number }
  | { type: "restored"; req: number; restored: string[]; dropped: string[]; checker: CheckerState; cwd: string }
  | { type: "failed"; req: number; error: string }
  | { type: "confirm"; req: number; message: string };
