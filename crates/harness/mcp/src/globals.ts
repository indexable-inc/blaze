// What a cell is handed, as the type checker sees it (check/type-service.ts declares each global as `CellGlobals[name]`):
// the harness functions, the UI elements and helpers, `parse`, `repl` and `$_`. The evaluator's scope (repl.ts) is built from
// the same values, and check.test.ts holds every name in it to a key here, so what is checked is what the cell gets.
import type { functions } from "./functions/index.ts";
import type { UiGlobals } from "./ui/index.ts";
import type { parse } from "./check/shape.ts";
import type { ReplApi } from "./repl.ts";

export type CellGlobals = typeof functions & UiGlobals & {
  /** `parse<T>(value)`: `value` checked against `T` when the cell runs; throws a ParseError naming every field that does not fit. */
  parse: typeof parse;
  repl: ReplApi;
  /** The latest result of this lane that was not undefined. */
  $_: unknown;
};
