// The app's flecs world from ts: `world.*` asks the harness app (its socket beside the daemon's, `app.sock`;
// harness-native/src/world_api.rs) to query and edit the world it is drawn from: entities and their components as
// flecs serializes them, the scene's tokens (the theme), flecs script. One request a connection in harness's frames:
// `{schema, call}` in CBOR, answered `{ok}` or `{error}`.
import { dirname, join } from "node:path";
import { decode, encode } from "./cbor.ts";
import { SOCK } from "./daemon.ts";
import { effect } from "./effects.ts";
import { table, type Table } from "./table.ts";
import { T_CBOR, frame, split } from "./wire.ts";

/** The schema of the app's world socket (world_api.rs `SCHEMA`). */
export const WORLD_SCHEMA = 1;
/** How long one call waits for the app. */
const WAIT_MS = 15_000;

export const APP_SOCK = () => join(dirname(SOCK()), "app.sock");

/** One call to the app: its `ok` value. Rejects with the app's error, or naming why it could not be asked. */
export function ask<T = unknown>(call: Record<string, unknown>): Promise<T> {
  const out = frame(T_CBOR, encode({ schema: WORLD_SCHEMA, call }));
  return new Promise<T>((resolve, reject) => {
    let buf = new Uint8Array(0);
    let settled = false;
    const done = (s: { end(): void } | null, f: () => void) => { if (settled) return; settled = true; clearTimeout(timer); s?.end(); f(); };
    const timer = setTimeout(() => done(null, () => reject(new Error(`world: the harness app did not answer within ${WAIT_MS / 1000}s`))), WAIT_MS);
    let sent = 0;
    const flush = (s: { write(b: Uint8Array): number }) => { if (sent < out.length) sent += Math.max(0, s.write(out.subarray(sent))); };
    Bun.connect({
      unix: APP_SOCK(),
      socket: {
        open(s) { flush(s); },
        drain(s) { flush(s); },
        data(s, chunk) {
          const next = new Uint8Array(buf.length + chunk.length);
          next.set(buf); next.set(chunk, buf.length); buf = next;
          let f;
          try { f = split(buf); } catch (e) { return done(s, () => reject(new Error(`world: a bad frame from the app: ${(e as Error).message}`))); }
          if (!f) return;
          if (f.t !== T_CBOR) return done(s, () => reject(new Error(`world: the app sent a type ${f.t} frame`)));
          let m: { ok?: unknown; error?: string };
          try { m = decode(f.payload) as typeof m; } catch (e) { return done(s, () => reject(new Error(`world: the app's answer is not CBOR: ${(e as Error).message}`))); }
          if (typeof m.error === "string") return done(s, () => reject(new Error(`world: ${m.error}`)));
          done(s, () => resolve(m.ok as T));
        },
        close() { done(null, () => reject(new Error("world: the app closed the connection without answering"))); },
        error(_s, e) { done(null, () => reject(e)); },
        connectError(_s, e) { done(null, () => reject(new Error(`world: the harness app is not running, or is older than this API (no ${APP_SOCK()}): ${e.message}`))); },
      },
    }).catch((e) => done(null, () => reject(new Error(`world: the harness app is not reachable at ${APP_SOCK()}: ${e}`))));
  });
}

/** One entity a query matched: its path, and the value of each term that has one, in the query's order. */
export type WorldRow = { entity: string; values: unknown[] };
type IterJson = { results?: { parent?: string; name?: string; fields?: { values?: unknown[] } }[] };

const rows = (j: IterJson): WorldRow[] =>
  (j.results ?? []).map((r) => ({ entity: [r.parent, r.name].filter(Boolean).join("."), values: r.fields?.values ?? [] }));

export const world = {
  /** The entities flecs query `expr` matches (`Fill, (ChildOf, hn.Theme), Prefab`), each with its terms' values. */
  query: (expr: string): Promise<Table<WorldRow>> =>
    effect("search", "Querying the app's world", "Queried the app's world", expr, async () => table(rows(await ask<IterJson>({ op: "query", expr }))), (t) => ({ detail: `${t.length} entities` })),
  /** Entity `path` whole: `{parent, name, tags, pairs, components: {Name: value}}`, what it inherits from a prefab too. */
  get: (path: string): Promise<Record<string, unknown>> =>
    effect("read", "Reading the app's world", "Read from the app's world", path, () => ask({ op: "get", path })),
  /** Write `value` (the members it names; the rest stay) into `component` of entity `path`; the component's value after. Kept across launches unless `save: false`. */
  set: (path: string, component: string, value: unknown, opts: { save?: boolean } = {}): Promise<unknown> =>
    effect("write", "Editing the app's world", "Edited the app's world", `${path} ${component}`, () => ask({ op: "set", path, component, value, save: opts.save ?? true })),
  /** A scene token (`tone_accent`, scene/00-tokens.flecs): read it, or with `value` write the members it names (an `export mut` only), everything made from it following. Its value after. */
  token: (name: string, value?: unknown, opts: { save?: boolean } = {}): Promise<unknown> =>
    value === undefined
      ? effect("read", "Reading a scene token", "Read a scene token", name, () => ask({ op: "token", name }))
      : effect("write", "Changing a scene token", "Changed a scene token", name, () => ask({ op: "token", name, value, save: opts.save ?? true })),
  /** Run flecs script `code` in the app's world (new entities, prefabs, components on what exists). Kept across launches only with `save: true`. */
  script: (code: string, opts: { save?: boolean } = {}): Promise<void> =>
    effect("write", "Running flecs script in the app", "Ran flecs script in the app", code.split("\n")[0]!.slice(0, 60), async () => { await ask({ op: "script", code, save: opts.save ?? false }); }),
  /** Every component's type as flecs reflects it, or `component`'s. */
  schema: (component?: string): Promise<unknown> => ask({ op: "schema", component }),
  /** The edits kept across launches, in order (each `{op, ...}` as it replays). */
  saved: async (): Promise<Table<Record<string, unknown>>> => table(await ask<Record<string, unknown>[]>({ op: "saved" })),
  /** Drop saved edit `index` (as `saved()` lists them), or every one. What it did stays until the app is launched again. */
  forget: (index?: number): Promise<{ left: number }> => ask({ op: "forget", index }),
};

export const worldDocs = {
  world:
    "world.* — the harness app's own flecs world, live. When he asks to change how this app looks (\"make the theme like my Ghostty\", \"warmer\", \"bigger text\", a colour, a corner), do it HERE, at once, with no build: never by editing .flecs scene or token files in a repository. Its look and layout are entities, components and scene tokens. token(name) reads a scene token and token(name, value) writes it, everything made from it following (`await world.token('tone_accent', { dark: { r: 0.9, g: 0.5, b: 0.2 } })`); query(flecsQuery): Table<{ entity; values }>; get(path) is one entity whole; set(path, component, value) writes the members named; script(code) runs flecs script; schema(component?) is the types; saved() and forget(i?) are the edits kept across launches (token and set are kept unless { save: false }). await skill('world') first.",
};
