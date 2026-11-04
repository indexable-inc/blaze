// sql`...`: DuckDB in this process over the harness object store, returning a Table<Row>.
//
// Views (see crates/harness/src/db.rs for the index the daemon writes):
//   objects   every object: PTY sessions (s7: claude|codex|cursor|shell), runs (r12: proc), stages (r12.0: stage), REPL cells (repl)
//   sessions, runs, stages   objects filtered by kind
//   streams   (object, stream, path, bytes, compacted_ms, archive_path): one raw append-only file per object stream
//   lines     (object, kind, stream, line_no, ts_ms, text): every stream line; also named `events`
//   messages  (object, role, kind, tool, text, ts_ms): agent conversations parsed from their transcripts
//
// `lines` = archive/objects/*.parquet for compacted objects, plus the raw files of everything not yet
// compacted (running runs and sessions), tailed incrementally into memory before each query. Both use
// one line rule (split on \n, lossy UTF-8; same as archive.rs), so a query answers the same before
// and after the daemon compacts or restarts.
import { DuckDBInstance, type DuckDBConnection, type DuckDBValue } from "@duckdb/node-api";
import { existsSync, statSync, openSync, readSync, closeSync } from "node:fs";
import { table, type Table } from "./table.ts";

const HOME = () => process.env.HARNESS_HOME ?? `${process.env.HOME}/.harness`;
const lit = (s: string) => `'${s.replaceAll("'", "''")}'`;

type Tail = { object: string; stream: string; offset: number; lines: number };
const tails = new Map<string, Tail>(); // by path

let conn: Promise<DuckDBConnection> | undefined;
let chain: Promise<unknown> = Promise.resolve();

async function connect(): Promise<DuckDBConnection> {
  const dbPath = `${HOME()}/harness.db`;
  if (!existsSync(dbPath)) throw new Error(`no ${dbPath} yet: start the daemon (./harness) so it can index`);
  const c = await (await DuckDBInstance.create(":memory:")).connect();
  await c.run("INSTALL sqlite; LOAD sqlite;");
  await c.run(`ATTACH ${lit(dbPath)} AS h (TYPE sqlite, READ_ONLY)`);
  await c.run(`
    create table live_lines(object varchar, stream varchar, line_no bigint, text varchar);
    create table live_tail(object varchar, stream varchar, line_no bigint, text varchar);
    create table live_objs(object varchar);
    create view objects as select id, kind, name, parent, cwd, args::json as args, created_ms, ended_ms, status, exit_code, signal, meta::json as meta from h.objects;
    create view sessions as select * from objects where kind in ('claude', 'codex', 'cursor', 'shell');
    create view runs as select *, args->>0 as cmd from objects where kind = 'proc';
    create view stages as select *, args->>0 as text, (meta->>'idx')::int as idx, (meta->>'pipeline')::int as pipeline from objects where kind = 'stage';
    create view streams as select * from h.streams;
  `);
  return c;
}

/** Read new complete lines of every uncompacted stream into live_lines; partial last lines into live_tail. */
async function syncLive(c: DuckDBConnection) {
  const r = await c.runAndReadAll("select object, stream, path from h.streams where compacted_ms is null and stream != 'messages'");
  const want = new Map(r.getRowObjectsJS().map((x) => [String(x.path), { object: String(x.object), stream: String(x.stream) }]));
  for (const [path, t] of tails) {
    if (want.has(path)) continue;
    await c.run("delete from live_lines where object = $1 and stream = $2", [t.object, t.stream]);
    tails.delete(path);
  }
  const app = await c.createAppender("live_lines");
  const tail: { object: string; stream: string; line_no: number; text: string }[] = [];
  const dec = new TextDecoder(); // lossy, like Rust's from_utf8_lossy
  for (const [path, w] of want) {
    let t = tails.get(path);
    if (!t) tails.set(path, (t = { ...w, offset: 0, lines: 0 }));
    const size = existsSync(path) ? statSync(path).size : 0;
    if (size < t.offset) { // rewritten: start over
      await c.run("delete from live_lines where object = $1 and stream = $2", [t.object, t.stream]);
      Object.assign(t, { offset: 0, lines: 0 });
    }
    if (size === t.offset) continue;
    const buf = Buffer.alloc(size - t.offset);
    const fd = openSync(path, "r");
    try { readSync(fd, buf, 0, buf.length, t.offset); } finally { closeSync(fd); }
    const cut = buf.lastIndexOf(10);
    if (cut >= 0) {
      const ls = dec.decode(buf.subarray(0, cut)).split("\n");
      for (const text of ls) {
        app.appendVarchar(t.object); app.appendVarchar(t.stream); app.appendBigInt(BigInt(++t.lines)); app.appendVarchar(text); app.endRow();
      }
      t.offset += cut + 1;
    }
    if (cut + 1 < buf.length) tail.push({ object: t.object, stream: t.stream, line_no: t.lines + 1, text: dec.decode(buf.subarray(cut + 1)) });
  }
  app.flushSync();
  app.closeSync();
  await c.run("delete from live_tail; delete from live_objs;");
  const ta = await c.createAppender("live_tail");
  for (const x of tail) { ta.appendVarchar(x.object); ta.appendVarchar(x.stream); ta.appendBigInt(BigInt(x.line_no)); ta.appendVarchar(x.text); ta.endRow(); }
  ta.flushSync(); ta.closeSync();
  const oa = await c.createAppender("live_objs");
  for (const o of new Set([...want.values()].map((w) => w.object))) { oa.appendVarchar(o); oa.endRow(); }
  oa.flushSync(); oa.closeSync();

  const archive = `${HOME()}/archive/objects`;
  const any = existsSync(archive) && !new Bun.Glob("*.parquet").scanSync(archive).next().done;
  const parquet = any
    ? `select object, stream, line_no, ts_ms, text from read_parquet(${lit(archive + "/*.parquet")}) where object not in (select object from live_objs)`
    : "select null::varchar object, null::varchar stream, null::bigint line_no, null::bigint ts_ms, null::varchar text where false";
  await c.run(`create or replace view lines as
    select l.object, o.kind, l.stream, l.line_no, l.ts_ms, l.text from (
      ${parquet}
      union all select object, stream, line_no, null::bigint, text from live_lines
      union all select object, stream, line_no, null::bigint, text from live_tail
    ) l left join h.objects o on o.id = l.object`);
  await c.run("create or replace view events as select * from lines");
  const msgs = [...new Bun.Glob("*/messages").scanSync(`${HOME()}/objects`)];
  await c.run(`create or replace view messages as ${msgs.length
    ? `select * from read_json([${msgs.map((m) => lit(`${HOME()}/objects/${m}`)).join(", ")}], format = 'newline_delimited', ignore_errors = true,
        columns = {object: 'VARCHAR', role: 'VARCHAR', kind: 'VARCHAR', tool: 'VARCHAR', text: 'VARCHAR', ts_ms: 'BIGINT'})`
    : "select null::varchar object, null::varchar role, null::varchar kind, null::varchar tool, null::varchar text, null::bigint ts_ms where false"}`);
}

const plain = (v: unknown): unknown =>
  typeof v === "bigint" ? (v >= Number.MIN_SAFE_INTEGER && v <= Number.MAX_SAFE_INTEGER ? Number(v) : v)
  : Array.isArray(v) ? v.map(plain)
  : v && typeof v === "object" && v.constructor === Object ? Object.fromEntries(Object.entries(v).map(([k, x]) => [k, plain(x)]))
  : v;

/**
 * Query the harness object store with DuckDB: sql`select * from lines where object = ${id}`.
 * Interpolated values are bound parameters ($1, $2, ...), never spliced into the text. sql(text) runs raw text.
 */
export function sql<Row extends object = Record<string, unknown>>(strings: TemplateStringsArray | string, ...values: unknown[]): Promise<Table<Row>> {
  const text = typeof strings === "string" ? strings : strings.reduce((s, part, i) => s + `$${i}` + part);
  const run = async () => {
    const c = await (conn ??= connect().catch((e) => { conn = undefined; throw e; }));
    await syncLive(c);
    const r = await c.runAndReadAll(text, values as DuckDBValue[]);
    return table(r.getRowObjectsJS().map((row) => plain(row) as Row));
  };
  const p = chain.then(run, run);
  chain = p.catch(() => {});
  return p;
}
