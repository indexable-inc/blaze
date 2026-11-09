//! The object API, Durable-Object style: every agent PTY (`s7`, kind claude|codex|cursor|shell), run
//! (`r12`, kind proc) and run stage (`r12.0`) is an object with an id, an optional name (unique among
//! running objects), a parent, append-only streams (indexed in db.rs) and RPC methods.
//!
//! Socket ops (daemon.rs routes them here):
//!   spawn_object {kind, name?, model?, effort?, prompt?, cmd?, cwd?, env?, args?, parent?: "s5"|"r9"|5, session?, stdin?, live_stdin?, cols?, rows?, mode?: "chat"|"tui"}
//!       -> {id, kind, name, existing}. Get-or-create: a live object holding `name` is returned instead
//!       (error if it is another kind). Atomic: one lock covers the lookup and the start.
//!       Procs get a live stdin pipe unless live_stdin: false.
//!   object {id}  (id or a name) -> {id, kind, name, parent, status, exit_code, signal, cwd, ...}
//!   rpc {id, method, args} -> per kind:
//!     sessions: prompt {text, submit?=true} (the daemon's per-kind injection: `send`), input {data},
//!               interrupt {key?} (Esc for agents, Ctrl-C for shell), resize {cols, rows}, kill,
//!               signal {sig}, screen, info, sync (bring a terminal agent's messages stream up to date now), forget
//!     runs:     stdin {data?, close?}, close_stdin, kill (TERM then KILL), signal {sig}, interrupt (INT),
//!               info, wait {timeout_ms?}, read {stage?, stream?, offset?, max_bytes?, wait_ms?}
//!     stages:   info, read {stream?, offset?, ...}
//! The old ops (send, screen, resize, kill, run_kill, run_read, ...) stay as aliases.
use crate::daemon::Daemon;
use harness_proto::msg::{Reply, Request as Req, RunReq};
use crate::{db, messages, runs};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use parking_lot::Mutex;
use std::sync::Arc;

static SPAWN: Mutex<()> = Mutex::new(());

pub enum Target {
    Session(u64),
    Run(String),
    Stage(String, usize),
}

pub fn parse(id: &str) -> Option<Target> {
    if let Some(n) = id.strip_prefix('s').and_then(|n| n.parse().ok()) {
        return Some(Target::Session(n));
    }
    let rest = id.strip_prefix('r')?;
    match rest.split_once('.') {
        Some((r, k)) if r.bytes().all(|b| b.is_ascii_digit()) => Some(Target::Stage(format!("r{r}"), k.parse().ok()?)),
        None if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) => Some(Target::Run(id.to_string())),
        _ => None,
    }
}

/// The live object holding `name`.
fn live_named(d: &Daemon, name: &str) -> Option<String> {
    let s = d.sessions.lock().values().find(|s| s.name.as_deref() == Some(name) && s.exit.lock().is_none()).map(|s| s.id);
    s.map(|n| format!("s{n}")).or_else(|| runs::live_named(name))
}

/// An object id from an id or a name (live holder first, then the most recent object with that name).
pub fn resolve(d: &Daemon, id_or_name: &str) -> Result<String> {
    if parse(id_or_name).is_some() {
        return Ok(id_or_name.to_string());
    }
    if let Some(id) = live_named(d, id_or_name) {
        return Ok(id);
    }
    let past = db::object_by_name(id_or_name);
    past.ok_or_else(|| anyhow!("no object {id_or_name:?} (want s7, r12, r12.0 or a name)"))
}

fn kind_of(d: &Daemon, id: &str) -> String {
    match parse(id) {
        Some(Target::Session(n)) => d.get(n).map(|s| s.kind.clone()).ok().or_else(|| db::object_kind_meta(id).map(|k| k.0)).unwrap_or_default(),
        Some(Target::Run(_)) => "proc".into(),
        Some(Target::Stage(..)) => "stage".into(),
        None => String::new(),
    }
}

fn session_num(v: &Value) -> Result<Option<u64>> {
    Ok(match v {
        Value::Null => None,
        Value::Number(n) => n.as_u64(),
        Value::String(s) => match parse(s) {
            Some(Target::Session(n)) => Some(n),
            _ => bail!("parent {s:?} of an agent must be a session (s7)"),
        },
        other => bail!("bad parent {other}"),
    })
}

pub fn spawn(d: &Arc<Daemon>, v: &Value) -> Result<Value> {
    let kind = v["kind"].as_str().context("spawn_object needs kind")?;
    let kind = if kind == "run" { "proc" } else { kind };
    let name = v["name"].as_str().map(str::to_string);
    let _g = SPAWN.lock();
    if let Some(n) = &name
        && let Some(id) = live_named(d, n)
    {
        let k = kind_of(d, &id);
        if k != kind {
            bail!("name {n:?} is held by running {k} object {id}, not a {kind}");
        }
        return Ok(json!({ "id": id, "kind": k, "name": n, "existing": true }));
    }
    // A parent may be given by name.
    let mut v = v.clone();
    if let Some(p) = v["parent"].as_str() {
        v["parent"] = json!(resolve(d, p)?);
    }
    let v = &v;
    let cwd = v["cwd"].as_str().map(str::to_string);
    let env: BTreeMap<String, String> = serde_json::from_value(v["env"].clone()).unwrap_or_default();
    let id = if kind == "proc" {
        let cmd = v["cmd"].as_str().context("spawn_object proc needs cmd")?.to_string();
        let (mut session, mut parent) = (v["session"].as_u64(), None);
        match &v["parent"] {
            Value::Null => {}
            Value::Number(n) => session = n.as_u64(),
            Value::String(p) => match parse(p) {
                Some(Target::Session(n)) => session = Some(n),
                Some(Target::Run(r)) => parent = Some(r),
                _ => bail!("parent {p:?} must be a session (s7) or a run (r12)"),
            },
            other => bail!("bad parent {other}"),
        }
        let r = runs::spawn(
            cmd, cwd, env, v["clear_env"].as_bool().unwrap_or(false), session, parent, v["stdin"].as_str().map(str::to_string),
            name.clone(), v["live_stdin"].as_bool().unwrap_or(true),
        )?;
        r["id"].as_str().unwrap_or_default().to_string()
    } else {
        let mut args: Vec<String> = serde_json::from_value(v["args"].clone()).unwrap_or_default();
        let parent = session_num(&v["parent"])?;
        let model = v["model"].as_str().map(str::to_string);
        let effort = v["effort"].as_str().map(str::to_string);
        if parent.is_some() {
            explicit_sub_agent(kind, name.as_deref(), model.as_deref(), effort.as_deref(), &args)?;
        }
        // Claude and Codex are chat sessions unless asked for their terminal UI (`mode: "tui"`, or `--tui` in args).
        let choice = crate::daemon::launch_choice(kind, &mut args, model, effort, v["mode"] == "tui")?;
        let cols = v["cols"].as_u64().map_or(140, |c| c as u16);
        let rows = v["rows"].as_u64().map_or(45, |r| r as u16);
        let n = d.spawn_full(kind, &args, parent, cwd, v["prompt"].as_str(), name.clone(), &env, cols, rows, choice)?;
        format!("s{n}")
    };
    Ok(json!({ "id": id, "kind": kind, "name": name, "existing": false }))
}

/// One object's current record, live state first, else the index.
pub fn info(d: &Daemon, id: &str) -> Result<Value> {
    let id = resolve(d, id)?;
    Ok(match parse(&id) {
        Some(Target::Session(n)) => match d.get(n) {
            Ok(s) => {
                let i = s.info();
                let exit = *s.exit.lock();
                json!({
                    "id": id, "kind": s.kind, "name": s.name, "parent": s.parent.map(|p| format!("s{p}")), "cwd": s.cwd,
                    "status": i.status, "exit_code": exit, "signal": null,
                    "created_ms": s.created_ms, "title": i.title, "pid": s.pid(), "cols": i.cols, "rows": i.rows,
                })
            }
            Err(_) => known(&id)?,
        },
        Some(Target::Run(r)) => {
            let m = runs::handle(RunReq::RunStatus { id: r.clone() })?;
            let parent = m["parent"].as_str().map(str::to_string).or(m["session"].as_u64().map(|s| format!("s{s}")));
            json!({
                "id": r, "kind": "proc", "name": m["name"], "parent": parent, "cwd": m["cwd"], "cmd": m["cmd"],
                "status": m["status"], "exit_code": m["exit_code"], "signal": m["signal"], "created_ms": m["start_ms"],
                "ended_ms": m["end_ms"], "pid": m["pid"],
                "stages": m["stages"].as_array().map_or(0, Vec::len),
            })
        }
        Some(Target::Stage(r, k)) => {
            let m = runs::handle(RunReq::RunStatus { id: r.clone() })?;
            let s = m["stages"].get(k).cloned().ok_or_else(|| anyhow!("run {r} has no stage {k}"))?;
            json!({
                "id": id, "kind": "stage", "name": null, "parent": r, "cwd": m["cwd"], "cmd": s["text"], "status": s["status"],
                "exit_code": s["exit_code"], "signal": s["signal"], "created_ms": s["start_ms"], "ended_ms": s["end_ms"], "pid": s["pid"],
            })
        }
        None => bail!("bad object id {id:?}"),
    })
}

fn known(id: &str) -> Result<Value> {
    let v = db::object(id);
    if v.is_null() {
        bail!("no object {id}");
    }
    Ok(v)
}

fn sig_num(v: &Value) -> Result<i32> {
    if let Some(n) = v.as_i64() {
        return Ok(n as i32);
    }
    let s = v.as_str().unwrap_or("TERM").to_ascii_uppercase();
    Ok(match s.trim_start_matches("SIG") {
        "HUP" => libc::SIGHUP,
        "INT" => libc::SIGINT,
        "QUIT" => libc::SIGQUIT,
        "KILL" => libc::SIGKILL,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        "TERM" => libc::SIGTERM,
        "STOP" => libc::SIGSTOP,
        "CONT" => libc::SIGCONT,
        "WINCH" => libc::SIGWINCH,
        other => other.parse().map_err(|_| anyhow!("unknown signal {s:?}"))?,
    })
}

/// Dispatch one RPC. `handle` runs an old-style socket request (so `prompt` goes through the same
/// per-kind `send` path as every other client).
pub fn rpc(d: &Daemon, id: &str, method: &str, args: &Value, handle: impl Fn(Req) -> Result<Reply>) -> Result<Reply> {
    let id = resolve(d, id)?;
    let a = |k: &str| args[k].as_str().map(str::to_string);
    match parse(&id) {
        Some(Target::Session(n)) => match method {
            "prompt" | "send" => handle(Req::Send {
                id: n,
                text: a("text").context("prompt needs args.text")?,
                raw: false,
                submit: args["submit"].as_bool(),
                images: args["images"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect(),
            }),
            "input" => handle(Req::Send { id: n, text: a("data").context("input needs args.data")?, raw: true, submit: None, images: Vec::new() }),
            "interrupt" => {
                let s = d.get(n)?;
                if s.chat.is_some() && a("key").is_none() {
                    s.chat_interrupt()?;
                    return Ok(Reply::Ok);
                }
                let key = a("key").unwrap_or_else(|| if s.kind == "shell" { "\x03".into() } else { "\x1b".into() });
                s.write(key.as_bytes())?;
                Ok(Reply::Ok)
            }
            "resize" => handle(Req::Resize {
                id: n, cols: args["cols"].as_u64().context("resize needs cols")? as u16, rows: args["rows"].as_u64().context("resize needs rows")? as u16,
            }),
            "kill" => handle(Req::Kill { id: n }),
            "forget" => handle(Req::Forget { id: n }),
            "rename" => handle(Req::Rename { id: n, title: a("title").context("rename needs args.title (\"\": its name taken away)")? }),
            "signal" => {
                let pid = d.get(n)?.pid().ok_or_else(|| anyhow!("{id} has no pid"))?;
                unsafe { libc::kill(pid as i32, sig_num(&args["sig"])?) };
                Ok(Reply::Ok)
            }
            "screen" => handle(Req::Screen { id: n }),
            "info" | "status" => info(d, &id).map(Reply::Record),
            "sync" => {
                messages::sync_object(&id)?;
                Ok(Reply::Ok)
            }
            m => bail!("{id} ({}) has no method {m:?} (prompt input interrupt resize kill signal screen info sync forget rename)", kind_of(d, &id)),
        },
        Some(Target::Run(r)) => match method {
            "stdin" => runs::handle(RunReq::RunStdin { id: r, data: a("data"), close: args["close"].as_bool().unwrap_or(false) }).map(Reply::Record),
            "close_stdin" => runs::handle(RunReq::RunStdin { id: r, data: None, close: true }).map(Reply::Record),
            "kill" => runs::handle(RunReq::RunKill { id: r }).map(Reply::Record),
            "interrupt" => runs::signal(&r, libc::SIGINT).map(Reply::Record),
            "signal" => {
                let sig = sig_num(&args["sig"])?;
                if sig == libc::SIGTERM && args["sig"].is_null() { runs::handle(RunReq::RunKill { id: r }).map(Reply::Record) } else { runs::signal(&r, sig).map(Reply::Record) }
            }
            "info" | "status" => info(d, &id).map(Reply::Record),
            "wait" => {
                runs::handle(RunReq::RunWait { id: r.clone(), timeout_ms: args["timeout_ms"].as_u64() })?;
                info(d, &r).map(Reply::Record)
            }
            "read" => read(r, args["stage"].as_u64().map(|k| k as usize), args).map(Reply::Record),
            m => bail!("{id} (proc) has no method {m:?} (stdin close_stdin kill signal interrupt info wait read)"),
        },
        Some(Target::Stage(r, k)) => match method {
            "info" | "status" => info(d, &id).map(Reply::Record),
            "read" => read(r, Some(k), args).map(Reply::Record),
            m => bail!("{id} (stage) has no method {m:?} (info read)"),
        },
        None => bail!("bad object id {id:?}"),
    }
}

fn read(id: String, stage: Option<usize>, args: &Value) -> Result<Value> {
    runs::handle(RunReq::RunRead {
        id, stage, stream: args["stream"].as_str().unwrap_or("out").to_string(), offset: args["offset"].as_u64().unwrap_or(0),
        max_bytes: args["max_bytes"].as_u64().map(|n| n as usize), wait_ms: args["wait_ms"].as_u64().unwrap_or(0),
    })
}

/// An agent another agent starts says what it is: a name (its row in the tree, and how it is found again), the model
/// and, when the model's listed entry takes one, the reasoning effort. A default picked silently is how a lane ran at the wrong
/// effort (2026-11-09), so none is picked here. `--model`/`--effort` in `args` are refused for the same reason: the
/// fields are the one place they are said.
///
/// # Errors
/// Naming each missing or doubled field.
fn explicit_sub_agent(kind: &str, name: Option<&str>, model: Option<&str>, effort: Option<&str>, args: &[String]) -> Result<()> {
    if !matches!(kind, "claude" | "codex" | "cursor") {
        return Ok(());
    }
    let mut missing = Vec::new();
    if name.is_none_or(str::is_empty) {
        missing.push("name");
    }
    if model.is_none_or(str::is_empty) {
        missing.push("model");
    }
    // The effort is required exactly when the model's listed entry takes efforts: a kind that has listed none yet
    // (codex before its first start) or a model without efforts (cursor) cannot be given one (models::check).
    let takes_effort = model.is_some_and(|m| {
        crate::chat::models::snapshot().get(kind).and_then(|l| l.iter().find(|c| c.is(m))).is_some_and(|c| !c.efforts.is_empty())
    });
    if takes_effort && effort.is_none_or(str::is_empty) {
        missing.push("effort");
    }
    if !missing.is_empty() {
        bail!("a {kind} sub agent needs an explicit {}: spawn('{kind}', {{ name, model, effort, prompt }})", missing.join(", "));
    }
    if let Some(flag) = args.iter().find(|a| ["--model", "--effort"].iter().any(|f| a == f || a.starts_with(&format!("{f}=")))) {
        bail!("{flag} in args: say the model and effort as spawn's own model and effort fields");
    }
    Ok(())
}
