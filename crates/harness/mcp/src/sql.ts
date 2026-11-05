// sql`...`: DuckDB in this process over the harness object store, returning a Table<Row>.
//
// Views (see crates/harness/harness/src/db.rs for the index the daemon writes):
//   objects   every object: PTY sessions (s7: claude|codex|cursor|shell), runs (r12: proc), stages (r12.0: stage), REPL cells (repl)
//   sessions, runs, stages   objects filtered by kind
//   streams   (object, stream, path, bytes, compacted_ms, archive_path): one raw append-only file per object stream
//   lines     (object, kind, stream, line_no, ts_ms, text): every line stream's line; also named `events`
//   messages  (object, role, kind, tool, text, ts_ms): agent conversations: a TUI session's parsed from its transcript
//             (objects/<id>/messages), a chat session's projected from its log here (wire.ts messageRows)
//   chat      one row per event of every chat session's log (harness/src/chat/mod.rs has the event model)
//   chats     one row per chat session
//
// `lines` = archive/objects/*.parquet for compacted objects, plus the raw files of everything not yet
// compacted (running runs and sessions), tailed incrementally into memory before each query. Both use
// one line rule (split on \n, lossy UTF-8; same as archive.rs), so a query answers the same before
// and after the daemon compacts or restarts. A chat's log is not a line stream: it is never compacted,
// stays one file (objects/<id>/chat) for good, live or long finished alike: wire.ts's MAGIC, then one
// deterministic-CBOR event frame per record. The kernel decodes them here, incrementally before each
// query (from the end of the last whole frame read; a file that shrank or changed inode, as the
// daemon's one-time NDJSON conversion renames over it, is read again), into chat_lines as JSON text,
// numbered by record from 1 as the daemon reads them (log.rs). A torn tail (a crash mid-append) is one
// row with line NULL, read again next query; so is an undecodable frame, and a file that is not a log.
import { DuckDBInstance, type DuckDBConnection, type DuckDBValue } from "@duckdb/node-api";
import { existsSync, statSync, openSync, readSync, closeSync } from "node:fs";
import { table, type Table } from "./table.ts";
import { effect } from "./effects.ts";
import { MAGIC, Projection, json, readFrames, readLog, type Log } from "./wire.ts";

const HOME = () => process.env.HARNESS_HOME ?? `${process.env.HOME}/.harness`;
const lit = (s: string) => `'${s.replaceAll("'", "''")}'`;

type Tail = { object: string; stream: string; offset: number; lines: number };
const tails = new Map<string, Tail>(); // by path
/** A chat log read so far: its inode, the end of its last whole frame consumed, its records, its messages projection. */
type ChatLog = { session: string; ino: number; offset: number; n: number; proj: Projection };
const chatLogs = new Map<string, ChatLog>(); // by path

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
    create table chat_lines(session varchar, n bigint, line varchar);
    create table chat_tail(session varchar, n bigint, line varchar);
    create table chat_msgs(object varchar, role varchar, kind varchar, tool varchar, text varchar, ts_ms bigint);
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
  const r = await c.runAndReadAll("select object, stream, path from h.streams where compacted_ms is null and stream not in ('messages', 'chat')");
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
    ? `select object, stream, line_no, ts_ms, text from read_parquet(${lit(archive + "/*.parquet")}) where object not in (select object from live_objs) and stream != 'chat'`
    : "select null::varchar object, null::varchar stream, null::bigint line_no, null::bigint ts_ms, null::varchar text where false";
  await c.run(`create or replace view lines as
    select l.object, o.kind, l.stream, l.line_no, l.ts_ms, l.text from (
      ${parquet}
      union all select object, stream, line_no, null::bigint, text from live_lines
      union all select object, stream, line_no, null::bigint, text from live_tail
    ) l left join h.objects o on o.id = l.object`);
  await c.run("create or replace view events as select * from lines");
  await syncChats(c);
  // A TUI session's transcript rows; a chat session's come from its log (an old `messages` file beside it is left out).
  const objects = `${HOME()}/objects`;
  const msgs = existsSync(objects) ? [...new Bun.Glob("*/messages").scanSync(objects)].filter((m) => !existsSync(`${objects}/${m.split("/")[0]}/chat`)) : [];
  await c.run(`create or replace view messages as ${msgs.length
    ? `select * from read_json([${msgs.map((m) => lit(`${objects}/${m}`)).join(", ")}], format = 'newline_delimited', ignore_errors = true,
        columns = {object: 'VARCHAR', role: 'VARCHAR', kind: 'VARCHAR', tool: 'VARCHAR', text: 'VARCHAR', ts_ms: 'BIGINT'})`
    : "select null::varchar object, null::varchar role, null::varchar kind, null::varchar tool, null::varchar text, null::bigint ts_ms where false"}
    union all select object, role, kind, tool, text, ts_ms from chat_msgs`);
  await chatViews(c);
}

/** Read what is new in every chat log into chat_lines and chat_msgs; its torn tail, or why it is not a log, into chat_tail. */
async function syncChats(c: DuckDBConnection) {
  const objects = `${HOME()}/objects`;
  const paths = new Set(existsSync(objects) ? [...new Bun.Glob("*/chat").scanSync(objects)].map((p) => `${objects}/${p}`) : []);
  const forget = async (path: string, l: ChatLog) => {
    await c.run("delete from chat_lines where session = $1", [l.session]);
    await c.run("delete from chat_msgs where object = $1", [l.session]);
    chatLogs.delete(path);
  };
  for (const [path, l] of chatLogs) if (!paths.has(path)) await forget(path, l);
  const lines = await c.createAppender("chat_lines");
  const msgs = await c.createAppender("chat_msgs");
  const tail: { session: string; n: number }[] = [];
  const str = (s: string | null) => (s === null ? msgs.appendNull() : msgs.appendVarchar(s));
  for (const path of paths) {
    const session = path.slice(objects.length + 1, -"/chat".length);
    let st;
    try { st = statSync(path); } catch { continue; } // gone since the glob: forgotten next query
    let l = chatLogs.get(path);
    if (l && (st.ino !== l.ino || st.size < l.offset)) { await forget(path, l); l = undefined; } // replaced or cut: read again
    if (!l) chatLogs.set(path, (l = { session, ino: st.ino, offset: 0, n: 0, proj: new Projection(session) }));
    if (st.size === l.offset) continue;
    const buf = Buffer.alloc(st.size - l.offset);
    const fd = openSync(path, "r");
    let got: number;
    try { got = readSync(fd, buf, 0, buf.length, l.offset); } finally { closeSync(fd); }
    const bytes = new Uint8Array(buf.buffer, buf.byteOffset, got);
    let log: Log;
    if (l.offset === 0) {
      const r = readLog(bytes);
      if ("notALog" in r) { tail.push({ session, n: 1 }); continue; } // NDJSON the daemon has yet to convert, or foreign
      if (bytes.length < MAGIC.length) continue; // empty, or cut inside the magic: nothing yet
      log = r;
    } else log = readFrames(bytes, l.offset, l.n);
    for (const r of log.records) {
      lines.appendVarchar(session); lines.appendBigInt(BigInt(r.n));
      if (r.event) lines.appendVarchar(json(r.event)); else lines.appendNull();
      lines.endRow();
      if (!r.event) continue;
      for (const m of l.proj.rows(r.event)) {
        msgs.appendVarchar(m.object); msgs.appendVarchar(m.role); msgs.appendVarchar(m.kind); str(m.tool); msgs.appendVarchar(m.text);
        if (m.ts_ms === null) msgs.appendNull(); else msgs.appendBigInt(BigInt(m.ts_ms));
        msgs.endRow();
      }
    }
    l.n += log.records.length;
    l.offset = log.end;
    if (log.torn) tail.push({ session, n: l.n + 1 }); // not consumed: read again next query
  }
  lines.flushSync(); lines.closeSync();
  msgs.flushSync(); msgs.closeSync();
  await c.run("delete from chat_tail");
  const ta = await c.createAppender("chat_tail");
  for (const t of tail) { ta.appendVarchar(t.session); ta.appendBigInt(BigInt(t.n)); ta.appendNull(); ta.endRow(); }
  ta.flushSync(); ta.closeSync();
}

/** The `chat` and `chats` views over every chat log (see the header). */
async function chatViews(c: DuckDBConnection) {
  await c.run(`create or replace view chat as
    with l as (select session, n, line, line is not null valid from (select session, n, line from chat_lines union all select session, n, line from chat_tail)),
    e as (select session, n, valid, case when valid then line::json end j from l)
    select session, n, j->>'ev' ev, (j->>'ts')::bigint ts_ms,
      j->>'ev' in ('block_start', 'delta', 'tool_start', 'tool_delta') partial,
      j->>'block' block,
      case when j->>'ev' in ('block', 'block_start') then j->>'kind' end kind,
      case when j->>'ev' = 'user' then 'user'
           when j->>'ev' in ('block', 'block_start', 'delta', 'tool_start', 'tool_delta', 'tool_call') then 'assistant'
           when j->>'ev' in ('tool_result', 'tool_detail') then 'tool'
           else 'system' end role,
      coalesce(j->>'text', j->>'json') text,
      coalesce(j->>'name', max(case when j->>'ev' in ('tool_start', 'tool_call') then j->>'name' end) over (partition by session, j->>'block')) tool,
      j->'input' input, (j->>'error')::boolean error,
      j->>'cell' cell, j->>'status' status, (j->>'ms')::bigint ms, j->'effects' effects, j->'value' value, j->'ui' ui,
      (j->>'ok')::boolean ok, j->>'stop' stop, (j->>'duration_ms')::bigint duration_ms, (j->>'cost_usd')::double cost_usd, j->'usage' usage,
      j->>'agent' agent, j->>'model' model, j->>'session_id' session_id, (j->>'busy')::boolean busy,
      valid, j json
    from e`);
  await c.run(`create or replace view chats as
    select o.id, o.kind agent, o.name, o.parent, o.cwd, o.status, o.created_ms started_ms, o.ended_ms,
      max(c.ts_ms) last_ms,
      arg_max(c.model, c.n) filter (where c.ev = 'session') model,
      arg_max(c.session_id, c.n) filter (where c.ev = 'session' and c.session_id is not null) agent_session,
      count(*) filter (where c.ev = 'turn_end') turns,
      count(*) filter (where c.ev = 'user') messages,
      list_filter(string_split(arg_min(c.text, c.n) filter (where c.ev = 'user'), chr(10)), lambda x: trim(x) != '')[1] title,
      arg_min(c.text, c.n) filter (where c.ev = 'user') first_message,
      sum(c.cost_usd) cost_usd,
      count(*) filter (where not c.valid) bad_lines
    from objects o left join chat c on c.session = o.id
    where o.meta->>'mode' = 'chat'
    group by all`);
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
  return effect("sql", "Querying the harness store", "Queried the harness store", text, () => p, (r) => ({ detail: `${r.length} row${r.length === 1 ? "" : "s"}`, show: r }));
}
