//! The object index: SQLite (WAL) at paths::home()/harness.db, written only by the daemon.
//!
//! Everything is an object (like a Durable Object): a PTY agent session (`s7`, kind claude|codex|shell),
//! a shell run (`r12`, kind proc), each pipeline stage of a run (`r12.0`, kind stage, child of the run),
//! a REPL cell (`<repl>.t3`, kind repl). Each object has append-only raw streams (pty, stdout, stderr,
//! stdin, final_err, messages, chat), one file per (object, stream); the `streams` table says where each
//! file is (new objects: objects/<id>/<stream>; runs keep their runs/rN/ layout), how big, and whether
//! it has been compacted into archive/objects/<root>.parquet (a top-level object and its children). Two streams are never
//! compacted: `messages`, and `chat` (a chat session's event log, which stays one file of CBOR frames for good; chat/mod.rs).
//! The raw files stay the source of the bytes; meta.json stays authoritative for a live run.
//! Readers (the REPL's `sql`) attach this file read-only. Write failures are logged, never fatal.
use crate::paths;
use harness_proto::msg::SessionInfo;
use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use parking_lot::{Mutex, MutexGuard};
use std::sync::OnceLock;

pub fn path() -> PathBuf { paths::home().join("harness.db") }
pub fn object_dir(id: &str) -> PathBuf { paths::home().join("objects").join(id) }
pub fn archive_path(root: &str) -> PathBuf { paths::home().join("archive/objects").join(format!("{root}.parquet")) }

const SCHEMA: &str = "
create table if not exists objects(
  id text primary key,
  kind text not null,            -- agent kinds (claude | codex | shell | ...) | proc | stage | repl
  name text,                     -- unique among running objects
  parent text,                   -- parent object id
  cwd text, args text,           -- args: JSON array
  created_ms integer, ended_ms integer,
  status text,                   -- running | exited | killed | lost | done | skipped | error ...
  exit_code integer, signal text,
  meta text                      -- JSON: kind-specific (stage: pipeline/idx; proc: session; claude: transcript ...)
);
create unique index if not exists objects_live_name on objects(name) where name is not null and status = 'running';
create index if not exists objects_parent on objects(parent);
create table if not exists streams(
  object text not null, stream text not null,  -- pty | stdout | stderr | stdin | final_err | messages
  path text not null, bytes integer,
  compacted_ms integer,          -- null: rows come from the raw file; else from archive_path
  archive_path text,
  primary key(object, stream)
);
-- A chat's log (objects/<id>/chat) is never compacted (chat/mod.rs); undo what an older daemon compacted of one.
update streams set compacted_ms = null, archive_path = null where stream = 'chat' and compacted_ms is not null;
";

fn conn() -> MutexGuard<'static, Connection> {
    static DB: OnceLock<Mutex<Connection>> = OnceLock::new();
    DB.get_or_init(|| {
        std::fs::create_dir_all(paths::home()).ok();
        let c = Connection::open(path()).expect("open harness.db");
        c.pragma_update(None, "journal_mode", "wal").expect("wal");
        c.pragma_update(None, "synchronous", "normal").ok();
        c.busy_timeout(std::time::Duration::from_secs(5)).ok();
        c.execute_batch(SCHEMA).expect("harness.db schema");
        Mutex::new(c)
    })
    .lock()
}

fn log<T>(what: &str, r: rusqlite::Result<T>) -> Option<T> { r.map_err(|e| eprintln!("harness.db {what}: {e}")).ok() }

fn size(p: &Path) -> Option<i64> { std::fs::metadata(p).ok().map(|m| m.len() as i64) }

#[derive(Default)]
pub struct Obj<'a> {
    pub id: &'a str,
    pub kind: &'a str,
    pub name: Option<&'a str>,
    pub parent: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub args: Value,
    pub created_ms: Option<i64>,
    pub ended_ms: Option<i64>,
    pub status: Option<&'a str>,
    pub exit_code: Option<i64>,
    pub signal: Option<&'a str>,
    pub meta: Value,
}

fn put_obj(c: &Connection, o: &Obj) -> rusqlite::Result<usize> {
    c.execute(
        "insert into objects(id, kind, name, parent, cwd, args, created_ms, ended_ms, status, exit_code, signal, meta)
         values(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         on conflict(id) do update set name = coalesce(excluded.name, name), created_ms = coalesce(excluded.created_ms, created_ms),
           ended_ms = excluded.ended_ms,
           status = excluded.status, exit_code = excluded.exit_code, signal = excluded.signal, meta = excluded.meta",
        params![
            o.id, o.kind, o.name, o.parent, o.cwd, o.args.to_string(), o.created_ms, o.ended_ms, o.status, o.exit_code,
            o.signal, o.meta.to_string()
        ],
    )
}

/// Index a raw stream file (bytes refreshed on every call; compaction state kept).
fn put_stream(c: &Connection, object: &str, stream: &str, path: &Path) -> rusqlite::Result<usize> {
    c.execute(
        "insert into streams(object, stream, path, bytes) values(?1, ?2, ?3, ?4)
         on conflict(object, stream) do update set path = excluded.path, bytes = excluded.bytes",
        params![object, stream, path.display().to_string(), size(path)],
    )
}

/// Upsert a run (`rN`, kind proc) and its stages (`rN.K`, kind stage) from runs::status output.
pub fn upsert_run(m: &Value, dir: &Path) {
    let mut c = conn();
    let id = m["id"].as_str().unwrap_or_default().to_string();
    let r = (|| {
        let tx = c.transaction()?;
        let session = m["session"].as_u64();
        let parent = m["parent"].as_str().map(str::to_string).or(session.map(|s| format!("s{s}")));
        put_obj(&tx, &Obj {
            id: &id, kind: "proc", name: m["name"].as_str(), parent: parent.as_deref(), cwd: m["cwd"].as_str(), args: json!([m["cmd"]]),
            created_ms: m["start_ms"].as_i64(), ended_ms: m["end_ms"].as_i64(), status: m["status"].as_str(),
            exit_code: m["exit_code"].as_i64(), signal: m["signal"].as_str(),
            meta: json!({ "session": session, "parent_run": m["parent"], "pid": m["pid"], "split": m["split"],
                "pipelines": m["pipelines"], "final_out_bytes": m["final_out_bytes"], "dir": dir }),
            ..Default::default()
        })?;
        for (stream, file) in [("final_err", "final.err"), ("stdin", "stdin")] {
            if dir.join(file).exists() {
                put_stream(&tx, &id, stream, &dir.join(file))?;
            }
        }
        for (k, s) in m["stages"].as_array().into_iter().flatten().enumerate() {
            let sid = format!("{id}.{k}");
            put_obj(&tx, &Obj {
                id: &sid, kind: "stage", parent: Some(&id), cwd: m["cwd"].as_str(), args: json!([s["text"]]),
                created_ms: s["start_ms"].as_i64(), ended_ms: s["end_ms"].as_i64(), status: s["status"].as_str(),
                exit_code: s["exit_code"].as_i64(), signal: s["signal"].as_str(),
                meta: json!({ "idx": k, "pipeline": s["pipeline"], "pid": s["pid"] }),
                ..Default::default()
            })?;
            put_stream(&tx, &sid, "stdout", &dir.join(format!("s{k}.out")))?;
            put_stream(&tx, &sid, "stderr", &dir.join(format!("s{k}.err")))?;
        }
        tx.commit()
    })();
    log(&format!("upsert run {id}"), r);
}

/// Streams of `root` and its children, for compaction: (object, stream, path).
pub fn tree_streams(root: &str) -> Vec<(String, String, PathBuf)> {
    let c = conn();
    let r = (|| {
        let mut st = c.prepare(
            "select s.object, s.stream, s.path from streams s join objects o on o.id = s.object
             where (o.id = ?1 or (o.parent = ?1 and o.kind = 'stage')) and s.stream not in ('messages', 'chat') order by o.created_ms, s.object, s.stream",
        )?;
        let rows = st.query_map([root], |r| Ok((r.get(0)?, r.get(1)?, PathBuf::from(r.get::<_, String>(2)?))))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    })();
    log("tree_streams", r).unwrap_or_default()
}

pub fn set_compacted(root: &str, archive: &Path, ms: u64) {
    let r = conn().execute(
        "update streams set compacted_ms = ?3, archive_path = ?2, bytes = coalesce(bytes, 0)
         where stream not in ('messages', 'chat') and object in (select id from objects where id = ?1 or (parent = ?1 and kind = 'stage'))",
        params![root, archive.display().to_string(), ms as i64],
    );
    log("set_compacted", r);
}

/// Top-level objects that are finished but whose streams aren't compacted yet.
pub fn uncompacted_roots() -> Vec<String> {
    let c = conn();
    let r = (|| {
        let mut st = c.prepare(
            "select distinct coalesce(case when o.kind = 'stage' then o.parent end, o.id) from streams s
             join objects o on o.id = s.object
             where s.compacted_ms is null and s.stream not in ('messages', 'chat')
               and (select status from objects r where r.id = coalesce(case when o.kind = 'stage' then o.parent end, o.id)) != 'running'",
        )?;
        let rows = st.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<String>>>()
    })();
    log("uncompacted_roots", r).unwrap_or_default()
}

pub fn is_compacted(root: &str) -> bool {
    let r = conn()
        .query_row(
            "select count(*) > 0 and count(*) = count(s.compacted_ms) from streams s join objects o on o.id = s.object
             where (o.id = ?1 or (o.parent = ?1 and o.kind = 'stage')) and s.stream not in ('messages', 'chat')",
            [root],
            |r| r.get::<_, bool>(0),
        )
        .optional();
    log("is_compacted", r).flatten().unwrap_or(false)
}

// ---------------------------------------------------------------- PTY sessions

/// Highest PTY session number ever handed out (`s<N>`), so ids stay unique across restarts.
pub fn max_session_id() -> u64 {
    let r = conn().query_row(
        "select coalesce(max(cast(substr(id, 2) as integer)), 0) from objects where id like 's%' and kind not in ('proc', 'stage', 'repl')",
        [],
        |r| r.get::<_, i64>(0),
    );
    log("max_session_id", r).unwrap_or(0) as u64
}

pub struct SessionRow<'a> {
    pub id: u64,
    pub kind: &'a str,
    pub name: Option<&'a str>,
    pub parent: Option<u64>,
    pub cwd: &'a str,
    pub argv: &'a [String],
    pub created_ms: u64,
    pub meta: Value,
    /// Its output's stream: `pty` (a terminal session), or `chat` (a chat session's events, chat/mod.rs).
    pub stream: &'a str,
}

/// Reserve `name` among running objects; errors with the holder's id if taken.
pub fn check_name(name: &str) -> Result<()> {
    let holder = conn()
        .query_row("select id from objects where name = ?1 and status = 'running'", [name], |r| r.get::<_, String>(0))
        .optional()?;
    if let Some(h) = holder {
        bail!("name {name:?} is taken by running object {h}");
    }
    Ok(())
}

pub fn session_start(s: &SessionRow) {
    let id = format!("s{}", s.id);
    let mut c = conn();
    let r = (|| {
        let tx = c.transaction()?;
        put_obj(&tx, &Obj {
            id: &id, kind: s.kind, name: s.name, parent: s.parent.map(|p| format!("s{p}")).as_deref(), cwd: Some(s.cwd),
            args: json!(s.argv), created_ms: Some(s.created_ms as i64), status: Some("running"), meta: s.meta.clone(),
            ..Default::default()
        })?;
        put_stream(&tx, &id, s.stream, &object_dir(&id).join(s.stream))?;
        tx.commit()
    })();
    log("session_start", r);
}

/// A PTY session ended: `status` is `exited` or `killed` (by a signal).
pub fn session_end(id: u64, code: i32, status: &str, ended_ms: u64) {
    let oid = format!("s{id}");
    let c = conn();
    log(
        "session_end",
        c.execute(
            "update objects set status = ?4, exit_code = ?2, ended_ms = ?3 where id = ?1",
            params![oid, code, ended_ms as i64, status],
        ),
    );
    for stream in ["pty", "chat"] {
        let path = object_dir(&oid).join(stream);
        if path.exists() {
            log("session_end bytes", c.execute("update streams set bytes = ?3 where object = ?1 and stream = ?2", params![oid, stream, size(&path)]));
        }
    }
}

/// At startup nothing is running yet: sessions the last daemon left open died with it, last heard from when their pty
/// stream was last written (else now).
pub fn mark_lost_sessions() {
    let c = conn();
    let r = (|| {
        let mut st = c.prepare("select id from objects where status = 'running' and kind not in ('proc', 'stage', 'repl')")?;
        let ids = st.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for id in ids {
            let heard = ["pty", "chat"].iter().find_map(|f| std::fs::metadata(object_dir(&id).join(f)).and_then(|m| m.modified()).ok());
            let ms = heard.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or_else(crate::runs::now_ms, |d| d.as_millis() as u64);
            c.execute("update objects set status = 'lost', ended_ms = coalesce(ended_ms, ?2) where id = ?1", params![id, ms as i64])?;
        }
        Ok(())
    })();
    log("mark_lost", r);
}

/// The columns a session row (`sessions`, `session`) reads.
const SESSION_COLS: &str =
    "select id, kind, name, parent, cwd, args, status, created_ms, ended_ms, exit_code, meta from objects where kind not in ('proc', 'stage', 'repl')";

/// One row of `SESSION_COLS` as a session row (daemon::Session::info's type): its mode (`chat` or `tui`), title, label
/// (a rename) and model from its meta.
fn session_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionInfo> {
    let num = |s: Option<String>| s.and_then(|s| s.get(1..)?.parse::<u64>().ok());
    let object: String = r.get(0)?;
    let args: Option<String> = r.get(5)?;
    let meta: Value = r.get::<_, Option<String>>(10)?.and_then(|m| serde_json::from_str(&m).ok()).unwrap_or(Value::Null);
    let u = |v: Option<i64>| v.and_then(|v| u64::try_from(v).ok());
    Ok(SessionInfo {
        id: num(Some(object.clone())).unwrap_or(0),
        object,
        kind: r.get(1)?,
        name: r.get(2)?,
        parent: num(r.get(3)?),
        cwd: r.get(4)?,
        argv: args.and_then(|a| serde_json::from_str(&a).ok()).unwrap_or_default(),
        status: r.get(6)?,
        created_ms: u(r.get(7)?),
        ended_ms: u(r.get(8)?),
        exit: r.get(9)?,
        title: meta["title"].as_str().unwrap_or("").to_owned(),
        label: meta["label"].as_str().filter(|l| !l.is_empty()).map(str::to_owned),
        mode: meta["mode"].as_str().unwrap_or("tui").to_owned(),
        model: meta["model"].as_str().map(str::to_owned),
        effort: meta["effort"].as_str().map(str::to_owned),
        ..SessionInfo::default()
    })
}

/// Sessions as `list` rows (for `list` with all: true).
pub fn sessions() -> Vec<SessionInfo> {
    let c = conn();
    let r = (|| {
        let mut st = c.prepare(&format!("{SESSION_COLS} order by created_ms"))?;
        let rows = st.query_map([], session_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    })();
    log("sessions", r).unwrap_or_default()
}

/// Session `sN` as the index knows it (one that ended before this daemon started), in the shape of [`sessions`]' rows.
pub fn session(id: u64) -> Option<SessionInfo> {
    let r = conn().query_row(&format!("{SESSION_COLS} and id = ?1"), [format!("s{id}")], session_row).optional();
    log("session", r).flatten()
}

/// Chat sessions whose log the index has not marked as harness schema `schema`'s (chat::migrate_logs).
pub fn chats_unmarked(schema: u32) -> Vec<String> {
    let c = conn();
    let r = (|| {
        let mut st = c.prepare(
            "select id from objects where kind not in ('proc', 'stage', 'repl') and json_extract(meta, '$.mode') = 'chat'
               and coalesce(json_extract(meta, '$.chat_log'), 0) <> ?1",
        )?;
        let rows = st.query_map([schema], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<String>>>()
    })();
    log("chats_unmarked", r).unwrap_or_default()
}

/// Forget one stream of an object (its file is the caller's).
pub fn drop_stream(object: &str, stream: &str) {
    log("drop_stream", conn().execute("delete from streams where object = ?1 and stream = ?2", params![object, stream]));
}

/// Chat sessions whose index row has no title yet (chat::backfill_titles).
pub fn chats_without_title() -> Vec<String> {
    let c = conn();
    let r = (|| {
        let mut st = c.prepare(
            "select id from objects where kind not in ('proc', 'stage', 'repl') and json_extract(meta, '$.mode') = 'chat'
               and json_extract(meta, '$.title') is null",
        )?;
        let rows = st.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<String>>>()
    })();
    log("chats_without_title", r).unwrap_or_default()
}

/// Agent sessions with a transcript to mirror into their `messages` stream: (object id, kind, meta).
pub fn agent_sessions(only_running: bool) -> Vec<(String, String, Value)> {
    let c = conn();
    let r = (|| {
        let mut st = c.prepare(
            "select id, kind, meta from objects where kind in ('claude', 'codex') and (?1 = 0 or status = 'running' or ended_ms > ?2)",
        )?;
        let rows = st.query_map(params![only_running as i64, crate::runs::now_ms() as i64 - 10_000], |r| {
            let meta: Option<String> = r.get(2)?;
            Ok((r.get(0)?, r.get(1)?, meta.and_then(|m| serde_json::from_str(&m).ok()).unwrap_or(Value::Null)))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    })();
    log("agent_sessions", r).unwrap_or_default()
}

pub fn set_meta(id: &str, key: &str, v: &Value) {
    log("set_meta", set_meta_in(&conn(), id, key, v));
}

fn set_meta_in(c: &Connection, id: &str, key: &str, v: &Value) -> rusqlite::Result<usize> {
    c.execute(
        "update objects set meta = json_set(coalesce(meta, '{}'), '$.' || ?2, json(?3)) where id = ?1",
        params![id, key, v.to_string()],
    )
}

/// (kind, meta) of one object.
pub fn object_kind_meta(id: &str) -> Option<(String, Value)> {
    let r = conn()
        .query_row("select kind, meta from objects where id = ?1", [id], |r| {
            let meta: Option<String> = r.get(1)?;
            Ok((r.get::<_, String>(0)?, meta.and_then(|m| serde_json::from_str(&m).ok()).unwrap_or(Value::Null)))
        })
        .optional();
    log("object_kind_meta", r).flatten()
}

/// The most recent object ever named `name`.
pub fn object_by_name(name: &str) -> Option<String> {
    let r = conn()
        .query_row("select id from objects where name = ?1 order by created_ms desc limit 1", [name], |r| r.get::<_, String>(0))
        .optional();
    log("object_by_name", r).flatten()
}

/// The session a REPL's cells were posted under (its newest cell's parent `s<n>`), if any.
pub fn repl_owner(repl: &str) -> Option<u64> {
    let r = conn()
        .query_row(
            "select parent from objects where kind = 'repl' and id like ?1 escape '\\' and parent is not null order by created_ms desc limit 1",
            [format!("{}.%", repl.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"))],
            |r| r.get::<_, String>(0),
        )
        .optional();
    log("repl_owner", r).flatten().and_then(|p| p.strip_prefix('s')?.parse().ok())
}

/// One object's index row as JSON (null when unknown).
pub fn object(id: &str) -> Value {
    let r = conn()
        .query_row(
            "select id, kind, name, parent, cwd, status, exit_code, signal, created_ms, ended_ms from objects where id = ?1",
            [id],
            |r| {
                Ok(json!({
                    "id": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "name": r.get::<_, Option<String>>(2)?,
                    "parent": r.get::<_, Option<String>>(3)?, "cwd": r.get::<_, Option<String>>(4)?,
                    "status": r.get::<_, Option<String>>(5)?, "exit_code": r.get::<_, Option<i64>>(6)?,
                    "signal": r.get::<_, Option<String>>(7)?, "created_ms": r.get::<_, Option<i64>>(8)?,
                    "ended_ms": r.get::<_, Option<i64>>(9)?,
                }))
            },
        )
        .optional();
    log("object", r).flatten().unwrap_or(Value::Null)
}

pub fn index_stream(object: &str, stream: &str, path: &Path) {
    log("index_stream", put_stream(&conn(), object, stream, path));
}

// ---------------------------------------------------------------- REPL cells

/// A REPL cell (`<repl>.tN`, kind repl), posted by the MCP process when it starts and finishes.
/// Code, logs and result are small; they live in meta rather than in streams.
pub fn repl_task(v: &Value) -> Result<()> {
    let id = format!("{}.{}", v["repl"].as_str().unwrap_or("repl"), v["id"].as_str().unwrap_or("t?"));
    let parent = v["session"].as_u64().map(|s| format!("s{s}"));
    put_obj(&conn(), &Obj {
        id: &id, kind: "repl", parent: parent.as_deref(), cwd: v["cwd"].as_str(), args: json!([v["code"]]),
        created_ms: v["started_ms"].as_i64(), ended_ms: v["ended_ms"].as_i64(), status: v["status"].as_str(),
        meta: json!({ "repl": v["repl"], "task": v["id"], "code": v["code"], "logs": v["logs"], "result": v["result"] }),
        ..Default::default()
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session's label (a rename) round-trips through its index row: set, read back as the row a restart restores the
    /// session from, kept when a resume writes the row again with the meta it read, and gone when taken away.
    #[test]
    fn a_label_round_trips_through_the_index() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        let row = |c: &Connection| c.query_row(&format!("{SESSION_COLS} and id = ?1"), ["s7"], session_row).unwrap();
        let start = |c: &Connection, meta: Value| {
            put_obj(c, &Obj { id: "s7", kind: "claude", status: Some("running"), meta, ..Default::default() }).unwrap();
        };
        start(&c, json!({"mode": "chat", "title": "count the files"}));
        assert_eq!(row(&c).label, None, "a session nobody renamed has no label");
        set_meta_in(&c, "s7", "label", &json!("tidy the build")).unwrap();
        let back = row(&c);
        assert_eq!(back.label.as_deref(), Some("tidy the build"));
        assert_eq!(back.title, "count the files", "the automatic title stays beside it");
        // A resume starts the row again with the meta it read (daemon.rs `resume`).
        let meta: String = c.query_row("select meta from objects where id = 's7'", [], |r| r.get(0)).unwrap();
        start(&c, serde_json::from_str(&meta).unwrap());
        assert_eq!(row(&c).label.as_deref(), Some("tidy the build"), "a resume keeps the label");
        set_meta_in(&c, "s7", "label", &Value::Null).unwrap();
        assert_eq!(row(&c).label, None, "taken away, the session has its automatic name again");
    }
}
