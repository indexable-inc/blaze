// A decoded CBOR item checked against blaze_protocol::events::Record (serde's externally
// tagged enums: a unit variant is its name as text, a struct variant a map of one entry).
// What does not fit is refused with the path to it, never patched up.

export class RecordError extends Error {}

const STEPS = new Set([
  "LoadWorkspace", "Fetch", "Resolve", "FrontEnd", "Codegen", "Std", "Link", "Check", "Run", "Test",
  "BuildScripts", "Macros", "Optimize", "Store", "Remote",
]);
const CRATE_STATES = new Set(["Waiting", "Running", "UpToDate", "Built", "Failed", "Resolving", "Resolved", "Checking"]);
const LEVELS = new Set(["Note", "Warning", "Error"]);
const NODE_KINDS = new Set(["Manifest", "Fetch", "Script", "ScriptRun", "Macro", "Std", "Names", "Bodies", "Codegen", "Store", "Link", "Test"]);

function fail(path, what) {
  throw new RecordError(`record ${path}: ${what}`);
}

function describe(v) {
  if (v === null) return "null";
  if (Array.isArray(v)) return "an array";
  if (typeof v === "object") return `a map of ${JSON.stringify(Object.keys(v))}`;
  if (typeof v === "string") return `the text ${JSON.stringify(v.length > 40 ? v.slice(0, 40) + "…" : v)}`;
  return `${typeof v} ${String(v)}`;
}

function struct(v, path, fields) {
  if (v === null || typeof v !== "object" || Array.isArray(v)) fail(path, `is ${describe(v)}, not a map`);
  for (const k of Object.keys(v)) if (!(k in fields)) fail(path, `has the unknown field ${JSON.stringify(k)}`);
  const out = {};
  for (const [k, check] of Object.entries(fields)) {
    if (!(k in v)) fail(path, `has no field ${JSON.stringify(k)}`);
    out[k] = check(v[k], `${path}.${k}`);
  }
  return out;
}

// `variants` maps a struct variant's name to its fields' checks and a unit variant's to null.
function enumOf(v, path, variants) {
  if (typeof v === "string") {
    if (variants[v] === null) return { type: v };
    fail(path, variants[v] ? `is the variant ${v} without its fields` : `is the unknown variant ${JSON.stringify(v)}`);
  }
  if (v === null || typeof v !== "object" || Array.isArray(v)) fail(path, `is ${describe(v)}, not a variant`);
  const keys = Object.keys(v);
  if (keys.length !== 1) fail(path, `is a map of ${keys.length} entries, not a variant`);
  const name = keys[0];
  if (!(name in variants)) fail(path, `is the unknown variant ${JSON.stringify(name)}`);
  if (variants[name] === null) fail(path, `is the unit variant ${name} with fields`);
  return { type: name, ...struct(v[name], `${path}.${name}`, variants[name]) };
}

const u64 = (v, path) => {
  if (typeof v === "bigint" && v >= 0n) return v;
  if (Number.isInteger(v) && v >= 0) return v;
  fail(path, `is ${describe(v)}, not a u64`);
};
const num = (v, path) => {
  const x = u64(v, path);
  if (typeof x === "bigint") fail(path, `is ${x}, more than this page counts exactly`);
  return x;
};
const u32 = (v, path) => {
  const x = num(v, path);
  if (x > 0xffffffff) fail(path, `is ${x}, more than a u32`);
  return x;
};
const text = (v, path) => (typeof v === "string" ? v : fail(path, `is ${describe(v)}, not text`));
const bool = (v, path) => (typeof v === "boolean" ? v : fail(path, `is ${describe(v)}, not a bool`));
const option = (check) => (v, path) => (v === null ? null : check(v, path));
const array = (check) => (v, path) => {
  if (!Array.isArray(v)) fail(path, `is ${describe(v)}, not an array`);
  return v.map((x, i) => check(x, `${path}[${i}]`));
};
const unit = (set, what) => (v, path) => (typeof v === "string" && set.has(v) ? v : fail(path, `is ${describe(v)}, not a ${what}`));
const step = unit(STEPS, "Step");

const fnsCount = (v, path) => struct(v, path, { total: num, done: num, kept: num });
const cause = (v, path) => enumOf(v, path, {
  Cold: null, Edited: { file: text }, Body: null, Signature: null, Callee: { name: text },
  Uses: null, Dependency: { name: text }, Flags: { what: text },
});
const how = (v, path) => enumOf(v, path, { Compiled: { ns: num }, Unit: null, Unchanged: null, Stored: null });
const fn = (v, path) => struct(v, path, { id: u32, key: u64, krate: text, name: text, calls: array(u32), how });
const nodeState = (v, path) => enumOf(v, path, {
  Waiting: null, Ready: null, Running: null, Built: null,
  Cached: { remote: bool }, Failed: { why: text }, Skipped: { because: u32 },
});
const cache = (v, path) => struct(v, path, { found: num, fetched: num, compiled: num, cold: option(text) });
const slow = (v, path) => struct(v, path, { krate: text, name: text, ns: num });
const took = (v, path) => struct(v, path, { step, of: option(text), took_ns: num });
const summary = (v, path) => struct(v, path, {
  took_ns: num, crates: num, fns: fnsCount,
  rebuilt: array((x, p) => {
    if (!Array.isArray(x) || x.length !== 2) fail(p, `is ${describe(x)}, not a (name, cause) pair`);
    return [text(x[0], `${p}.0`), cause(x[1], `${p}.1`)];
  }),
  more: num, error: option(text), cache: option(cache), slowest: array(took),
});

const EVENT = {
  Began: { build: num, command: text, root: text },
  Started: { step, of: option(text) },
  Ended: { step, of: option(text) },
  Progress: { step, done: num, total: num, longest: option(text) },
  Crate: { name: text, state: unit(CRATE_STATES, "CrateState"), fns: fnsCount },
  Rebuilt: { name: text, krate: text, cause },
  Unlisted: { count: num },
  Cache: { cache },
  Refused: { name: text, why: text },
  Said: { level: unit(LEVELS, "Level"), text },
  Finished: { ok: bool, summary },
  Node: { id: u32, kind: unit(NODE_KINDS, "NodeKind"), name: text, deps: array(u32) },
  Moved: { id: u32, state: nodeState },
  Critical: { path: array(u32) },
  Cores: { busy: u32, of: u32 },
  Compiling: { functions: array(slow) },
  Slowest: { functions: array(slow) },
  Functions: { object: u32, whole: bool, functions: array(fn) },
};

export function parseRecord(v) {
  const r = struct(v, "", { at_ns: num, event: (e, p) => e });
  return { at_ns: r.at_ns, event: enumOf(r.event, ".event", EVENT) };
}

// Cause::short, as the terminal says it.
export function causeText(c) {
  switch (c.type) {
    case "Cold": return "not kept";
    case "Edited": return `${c.file} edited`;
    case "Body": return "body edited";
    case "Signature": return "signature changed";
    case "Callee": return `calls ${c.name}`;
    case "Uses": return "uses what changed";
    case "Dependency": return `${c.name} changed`;
    case "Flags": return `${c.what} changed`;
  }
  return c.type;
}

// Step::word and Step::whole.
export const STEP_WORD = {
  LoadWorkspace: "Loading", Fetch: "Fetching", Resolve: "Resolving", FrontEnd: "Checking", Codegen: "Compiling",
  Std: "Compiling", Link: "Linking", Check: "Checking", Run: "Running", Test: "Testing",
  BuildScripts: "Building", Macros: "Loading", Optimize: "Optimizing", Store: "Storing", Remote: "Fetching",
};
export const STEP_WHOLE = {
  LoadWorkspace: "workspace", Fetch: "dependencies", Resolve: "names", FrontEnd: "types", Codegen: "functions",
  Std: "the standard library", Link: "program", Check: "crates", Run: "program", Test: "tests",
  BuildScripts: "build scripts and proc macros", Macros: "proc macros", Optimize: "code",
  Store: "the build in the store", Remote: "from the remote store",
};
