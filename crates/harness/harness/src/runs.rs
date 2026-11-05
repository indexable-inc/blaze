//! Shell runs: every command an agent runs goes through here and every pipeline stage's full
//! stdout and stderr is kept, so any agent can read any stage later (`r12` stage 0).
//!
//! The command is split at top-level `;` `&&` `||` newlines and `|` (quotes, escapes, `$(..)`,
//! backticks, parens and braces respected; anything we can't split safely is one stage). It then
//! runs as ONE bash script so shell state (`cd`, `export`, functions) carries across `&&`/`;` as
//! usual. Each stage is wrapped in a brace group that records its pid and exit code; its stderr goes
//! through `harness tee` (stored in sN.err and passed on to the caller's stderr), its stdout either
//! into the next stage through `harness tee` (stored in sN.out; when the reader quits early, tee
//! keeps draining upstream into the file instead of breaking the pipe) or, for the last stage of a
//! pipeline, straight into sN.out. Pipeline exit status and `&&`/`||` semantics are bash's own.
//!
//! Store: paths::home()/runs/<id>/ meta.json, sN.out, sN.err, sN.idx, sN.pid, sN.rc, final.err.
//! sN.idx is the stage's chunk index: one fixed 24-byte little-endian record per write `harness tee` made,
//! `ts_ms u64, offset u64, len u32, stream u8 (0 out, 1 err), 3 pad`, so stdout and stderr replay in the order they
//! were written (`run_read` with stream "ordered"). Runs from before the index have no .idx and read as before.
//! The caller-visible stdout ("final") is the last stage of each executed pipeline, in order;
//! the caller-visible stderr is final.err (every stage's stderr, as it arrived).
use crate::{archive, db, paths};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use parking_lot::Mutex;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;
use tokio::sync::watch;

pub use harness_proto::msg::RunReq;

struct Live {
    pid: u32,
    done: watch::Receiver<bool>,
    /// Unique among running objects (get-or-create spawn finds the run by it).
    name: Option<String>,
    /// The write end of a live stdin pipe, kept open until `close` or exit.
    stdin: Mutex<Option<std::io::PipeWriter>>,
}

struct Store {
    next: AtomicU64,
    live: Mutex<HashMap<String, Arc<Live>>>,
}

fn dir() -> PathBuf { paths::home().join("runs") }

/// Whether any run is still going (the daemon's sweep then tells `watch` clients its stages may have moved).
pub fn any_live() -> bool { !store().live.lock().is_empty() }

fn store() -> &'static Store {
    static S: OnceLock<Store> = OnceLock::new();
    S.get_or_init(|| {
        // Ids stay unique across daemon restarts: continue after the highest one on disk.
        let max = std::fs::read_dir(dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.strip_prefix('r')?.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        Store { next: AtomicU64::new(max + 1), live: Mutex::new(HashMap::new()) }
    })
}

pub fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64 }

fn run_dir(id: &str) -> Result<PathBuf> {
    if !id.starts_with('r') || id.len() < 2 || !id[1..].bytes().all(|b| b.is_ascii_digit()) {
        bail!("bad run id {id:?} (want r<N>)");
    }
    let d = dir().join(id);
    if !d.join("meta.json").exists() {
        bail!("no run {id}");
    }
    Ok(d)
}

// ---------------------------------------------------------------- parsing

#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    Seq,
    And,
    Or,
}

impl Op {
    fn text(self) -> &'static str {
        match self {
            Op::Seq => ";",
            Op::And => "&&",
            Op::Or => "||",
        }
    }
}

/// `[(op joining it to the previous pipeline, stages)]`, or None when the command can't be split safely.
fn split(cmd: &str) -> Option<Vec<(Op, Vec<String>)>> {
    #[derive(PartialEq)]
    enum Ctx {
        Paren,
        Brace,
        Dq,
        Bt,
    }
    let b = cmd.as_bytes();
    let mut stack: Vec<Ctx> = vec![];
    let mut out: Vec<(Op, Vec<String>)> = vec![];
    let mut stages: Vec<String> = vec![];
    let mut op = Op::Seq;
    let mut start = 0;
    let mut i = 0;
    let push_stage = |stages: &mut Vec<String>, s: &str| -> Option<()> {
        let t = s.trim();
        if t.is_empty() {
            return None;
        }
        stages.push(t.to_string());
        Some(())
    };
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        match stack.last() {
            Some(Ctx::Dq) => {
                match (c, next) {
                    (b'\\', _) => i += 1,
                    (b'"', _) => {
                        stack.pop();
                    }
                    (b'$', Some(b'(')) => {
                        stack.push(Ctx::Paren);
                        i += 1;
                    }
                    (b'$', Some(b'{')) => {
                        stack.push(Ctx::Brace);
                        i += 1;
                    }
                    (b'`', _) => stack.push(Ctx::Bt),
                    _ => {}
                }
                i += 1;
                continue;
            }
            Some(Ctx::Bt) => {
                match c {
                    b'\\' => i += 1,
                    b'`' => {
                        stack.pop();
                    }
                    _ => {}
                }
                i += 1;
                continue;
            }
            _ => {}
        }
        let word_start = i == 0 || matches!(b[i - 1], b' ' | b'\t' | b'\n' | b';' | b'&' | b'|' | b'(' | b')');
        match c {
            b'\\' => {
                i += 2;
                continue;
            }
            b'\'' => {
                i = i + 1 + cmd[i + 1..].find('\'')?;
            }
            b'"' => stack.push(Ctx::Dq),
            b'`' => stack.push(Ctx::Bt),
            b'(' => stack.push(Ctx::Paren),
            b'{' => stack.push(Ctx::Brace),
            b')' => {
                if stack.pop()? != Ctx::Paren {
                    return None;
                }
            }
            b'}' => {
                if stack.pop()? != Ctx::Brace {
                    return None;
                }
            }
            b'#' if word_start && stack.is_empty() => return None,
            b'<' if stack.is_empty() && next == Some(b'<') && b.get(i + 2) != Some(&b'<') => return None, // heredoc
            b'<' if stack.is_empty() && next == Some(b'<') => i += 2,                                       // here-string
            b'|' | b'&' | b';' | b'\n' if stack.is_empty() => {
                let (kind, len) = match (c, next) {
                    (b'|', Some(b'|')) => (Some(Op::Or), 2),
                    (b'|', Some(b'&')) => return None,
                    (b'|', _) => (None, 1),
                    (b'&', Some(b'&')) => (Some(Op::And), 2),
                    (b'&', Some(b'>')) => {
                        i += 1;
                        continue;
                    }
                    (b'&', _) if i > 0 && matches!(b[i - 1], b'>' | b'<') => {
                        i += 1;
                        continue;
                    }
                    (b'&', _) => return None, // background job
                    (b';', Some(b';' | b'&')) => return None, // case
                    _ => (Some(Op::Seq), 1),
                };
                let text = &cmd[start..i];
                if c == b'\n' && text.trim().is_empty() {
                    // newline right after `|`, `&&`, `||` or another separator continues the command
                    i += 1;
                    start = i;
                    continue;
                }
                match kind {
                    None => push_stage(&mut stages, text)?,
                    Some(k) => {
                        if text.trim().is_empty() && stages.is_empty() {
                            // blank line or leading separator: only allowed for `;`/newline
                            if k != Op::Seq {
                                return None;
                            }
                        } else {
                            push_stage(&mut stages, text)?;
                            out.push((op, std::mem::take(&mut stages)));
                            op = k;
                        }
                    }
                }
                i += len;
                start = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    if !stack.is_empty() {
        return None;
    }
    let rest = &cmd[start..];
    if rest.trim().is_empty() {
        if !stages.is_empty() || op != Op::Seq {
            return None; // dangling `|`, `&&` or `||`
        }
    } else {
        push_stage(&mut stages, rest)?;
        out.push((op, stages));
    }
    if out.is_empty() {
        return None;
    }
    // Compound commands span separators; leave those to bash whole.
    const RESERVED: &[&str] = &[
        "if", "then", "elif", "else", "fi", "for", "while", "until", "do", "done", "case", "esac", "select", "function",
        "time", "coproc", "!", "[[", "]]",
    ];
    for (_, st) in &out {
        for s in st {
            let first = s.split_whitespace().next().unwrap_or("");
            if RESERVED.contains(&first) {
                return None;
            }
        }
    }
    Some(out)
}

fn shq(s: &str) -> String { format!("'{}'", s.replace('\'', r"'\''")) }

/// The bash script for a split command; stage indices are global across pipelines.
fn script(parts: &[(Op, Vec<String>)], dir: &Path, exe: &Path) -> String {
    let mut s = format!(
        "__HD={}; __H={}\n__hrs(){{ echo \"${{BASHPID:-$$}}\" >\"$__HD/s$1.pid\"; }}\n__hrc(){{ echo \"$1\" >\"$__HD/s$2.rc\"; return \"$1\"; }}\n",
        shq(&dir.display().to_string()),
        shq(&exe.display().to_string())
    );
    let mut k = 0;
    for (pi, (op, stages)) in parts.iter().enumerate() {
        if pi > 0 {
            s.push(' ');
            s.push_str(op.text());
            s.push('\n');
        }
        for (si, text) in stages.iter().enumerate() {
            s.push_str(&format!("{{ __hrs {k}\n{text}\n__hrc $? {k}\n}} 2> >(\"$__H\" tee \"$__HD/s{k}.err\" >&2)"));
            if si + 1 < stages.len() {
                s.push_str(&format!(" | \"$__H\" tee \"$__HD/s{k}.out\" | "));
            } else {
                // Through tee too (for the chunk index); a process substitution keeps the stage's exit status.
                s.push_str(&format!(" > >(\"$__H\" tee \"$__HD/s{k}.out\" >/dev/null)"));
            }
            k += 1;
        }
    }
    s.push('\n');
    s
}

// ---------------------------------------------------------------- running

/// Start a run as an object (object.rs): optional unique `name`, and with `live_stdin` a pipe the
/// caller writes through `run_stdin` (else `stdin` text, or /dev/null). Call inside the runtime.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    cmd: String, cwd: Option<String>, env: BTreeMap<String, String>, clear_env: bool, session: Option<u64>,
    parent: Option<String>, stdin: Option<String>, name: Option<String>, live_stdin: bool,
) -> Result<Value> {
    let _g = tokio::runtime::Handle::current().enter();
    start(cmd, cwd, env, clear_env, session, parent, stdin, name, live_stdin)
}

/// The live run holding `name`, if any.
pub fn live_named(name: &str) -> Option<String> {
    store().live.lock().iter().find(|(_, l)| l.name.as_deref() == Some(name)).map(|(id, _)| id.clone())
}

/// Write `data` to a live run's stdin (also appended to its `stdin` stream), then close it if asked.
fn write_stdin(id: &str, data: Option<&str>, close: bool) -> Result<Value> {
    use std::io::Write;
    let d = run_dir(id)?;
    let l = store().live.lock().get(id).cloned().ok_or_else(|| anyhow!("run {id} is not running"))?;
    let mut w = l.stdin.lock();
    if let Some(data) = data.filter(|s| !s.is_empty()) {
        let pipe = w.as_mut().ok_or_else(|| anyhow!("run {id} has no open stdin (spawn it with live stdin; closed already?)"))?;
        pipe.write_all(data.as_bytes()).with_context(|| format!("write stdin of {id}"))?;
        pipe.flush()?;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(d.join("stdin"))?;
        f.write_all(data.as_bytes())?;
    }
    if close {
        *w = None;
    }
    Ok(json!({ "ok": true, "open": w.is_some() }))
}

/// Send a signal to a live run's process group.
pub fn signal(id: &str, sig: i32) -> Result<Value> {
    run_dir(id)?;
    let Some(l) = store().live.lock().get(id).cloned() else { return Ok(json!({ "ok": true, "running": false })) };
    unsafe { libc::kill(-(l.pid as i32), sig) };
    Ok(json!({ "ok": true, "running": true }))
}

#[allow(clippy::too_many_arguments)]
fn start(
    cmd: String, cwd: Option<String>, env: BTreeMap<String, String>, clear_env: bool, session: Option<u64>,
    parent: Option<String>, stdin: Option<String>, name: Option<String>, live_stdin: bool,
) -> Result<Value> {
    let st = store();
    let n = st.next.fetch_add(1, Ordering::Relaxed);
    let id = format!("r{n}");
    let d = dir().join(&id);
    std::fs::create_dir_all(&d)?;
    let cwd = cwd.unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| "/".into()));
    let parts = split(&cmd).unwrap_or_else(|| vec![(Op::Seq, vec![cmd.clone()])]);
    let mut stages = vec![];
    let mut pipelines = vec![];
    for (pi, (op, st)) in parts.iter().enumerate() {
        let first = stages.len();
        for t in st {
            stages.push(json!({ "text": t, "pipeline": pi }));
        }
        pipelines.push(json!({ "op": if pi == 0 { Value::Null } else { json!(op.text()) }, "stages": (first..stages.len()).collect::<Vec<_>>() }));
    }
    let exe = std::env::current_exe()?;
    let body = script(&parts, &d, &exe);
    std::fs::write(d.join("script.sh"), &body)?;
    let mut stdin_pipe = None;
    let stdin_file = match &stdin {
        _ if live_stdin => {
            std::fs::write(d.join("stdin"), stdin.as_deref().unwrap_or(""))?;
            let (r, w) = std::io::pipe()?;
            stdin_pipe = Some(w);
            std::process::Stdio::from(r)
        }
        Some(s) => {
            std::fs::write(d.join("stdin"), s)?;
            std::process::Stdio::from(std::fs::File::open(d.join("stdin"))?)
        }
        None => std::process::Stdio::null(),
    };
    let bash = ["/run/current-system/sw/bin/bash", "/opt/homebrew/bin/bash", "/usr/local/bin/bash", "/usr/bin/bash", "/bin/bash"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .unwrap_or("bash");
    let mut c = tokio::process::Command::new(bash);
    c.arg(d.join("script.sh")).current_dir(&cwd).stdin(stdin_file).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
    if clear_env {
        c.env_clear();
    }
    c.envs(&env).env("HARNESS_RUN", &id);
    if let Some(s) = session {
        c.env("HARNESS_SESSION", s.to_string());
    }
    c.process_group(0).kill_on_drop(false);
    let mut child = c.spawn().with_context(|| format!("spawn {bash}"))?;
    let pid = child.id().unwrap_or(0);
    if let (Some(w), Some(text)) = (stdin_pipe.as_mut(), stdin.as_deref().filter(|_| live_stdin)) {
        // After spawn, so text past the pipe buffer drains into the child instead of blocking.
        let _ = std::io::Write::write_all(w, text.as_bytes());
    }
    let meta = json!({
        "id": id, "cmd": cmd, "name": name, "cwd": cwd, "session": session, "parent": parent, "repl": env.get("HARNESS_REPL"), "pid": pid, "start_ms": now_ms(),
        "end_ms": null, "exit_code": null, "signal": null, "split": parts.len() > 1 || parts[0].1.len() > 1,
        "pipelines": pipelines, "stages": stages,
    });
    write_meta(&d, &meta)?;
    let (tx, rx) = watch::channel(false);
    st.live.lock().insert(id.clone(), Arc::new(Live { pid, done: rx, name, stdin: Mutex::new(stdin_pipe) }));
    index(&id);
    crate::daemon::changed();
    let mut err = child.stderr.take().ok_or_else(|| anyhow!("no stderr pipe"))?;
    let id2 = id.clone();
    tokio::spawn(async move {
        let mut f = match tokio::fs::File::create(d.join("final.err")).await {
            Ok(f) => f,
            Err(e) => {
                eprintln!("run {id2}: final.err: {e}");
                return;
            }
        };
        let (eof_tx, mut eof_rx) = watch::channel(false);
        tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match err.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tokio::io::AsyncWriteExt::write_all(&mut f, &buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = eof_tx.send(true);
        });
        let status = child.wait().await;
        // Stage tees finish when their stage does; a stray background process holding stderr
        // shouldn't keep the run open forever.
        let _ = tokio::time::timeout(Duration::from_secs(2), eof_rx.wait_for(|e| *e)).await;
        use std::os::unix::process::ExitStatusExt;
        let (code, sig) = match status {
            Ok(s) => (s.code(), s.signal()),
            Err(_) => (None, None),
        };
        if let Ok(mut m) = read_meta(&d) {
            m["end_ms"] = json!(now_ms());
            m["exit_code"] = json!(code);
            m["signal"] = json!(sig.map(sig_name));
            overlay(&d, &mut m, false);
            let _ = write_meta(&d, &m);
        }
        store().live.lock().remove(&id2);
        let id3 = id2.clone();
        let _ = tokio::task::spawn_blocking(move || {
            index(&id3);
            archive::compact_bg(id3);
        })
        .await;
        crate::daemon::changed();
        let _ = tx.send(true);
    });
    Ok(json!({ "id": id, "pid": pid }))
}

fn sig_name(n: i32) -> String {
    match n {
        1 => "SIGHUP".into(),
        2 => "SIGINT".into(),
        9 => "SIGKILL".into(),
        13 => "SIGPIPE".into(),
        15 => "SIGTERM".into(),
        n => format!("SIG{n}"),
    }
}

fn write_meta(d: &Path, m: &Value) -> Result<()> {
    let tmp = d.join("meta.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(m)?)?;
    std::fs::rename(tmp, d.join("meta.json"))?;
    Ok(())
}

fn read_meta(d: &Path) -> Result<Value> { Ok(serde_json::from_slice(&std::fs::read(d.join("meta.json"))?)?) }

fn mtime_ms(p: &Path) -> Option<u64> {
    Some(std::fs::metadata(p).ok()?.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64)
}

fn size(p: &Path) -> u64 { std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) }

/// Fill per-stage pid/exit/bytes/times from the stage files and the run's status.
fn overlay(d: &Path, m: &mut Value, live: bool) {
    let ended = !live;
    m["status"] = json!(if live {
        "running"
    } else if m["end_ms"].is_null() {
        "lost" // the daemon stopped while it ran
    } else if !m["signal"].is_null() {
        "killed"
    } else {
        "exited"
    });
    if let Some(stages) = m["stages"].as_array_mut() {
        for (k, s) in stages.iter_mut().enumerate() {
            let pidf = d.join(format!("s{k}.pid"));
            let rcf = d.join(format!("s{k}.rc"));
            let pid = std::fs::read_to_string(&pidf).ok().and_then(|t| t.trim().parse::<u64>().ok());
            let rc = std::fs::read_to_string(&rcf).ok().and_then(|t| t.trim().parse::<i32>().ok());
            s["pid"] = json!(pid);
            s["exit_code"] = json!(rc);
            // bash reports a signal death as 128+N
            s["signal"] = json!(rc.filter(|c| (129..160).contains(c)).map(|c| sig_name(c - 128)));
            s["start_ms"] = json!(mtime_ms(&pidf));
            s["end_ms"] = json!(mtime_ms(&rcf));
            s["status"] = json!(match (pid.is_some(), rc.is_some(), ended) {
                (_, true, _) => "done",
                (true, false, false) => "running",
                (true, false, true) => "unknown",
                (false, _, false) => "pending",
                (false, _, true) => "skipped",
            });
            s["stdout_bytes"] = json!(size(&d.join(format!("s{k}.out"))));
            s["stderr_bytes"] = json!(size(&d.join(format!("s{k}.err"))));
        }
    }
}

/// Write the run's current state into the object index.
fn index(id: &str) {
    match (run_dir(id), status(id, false)) {
        (Ok(d), Ok(m)) => db::upsert_run(&m, &d),
        (_, Err(e)) => eprintln!("index {id}: {e:#}"),
        _ => {}
    }
}

/// Startup: index every run on disk that isn't compacted yet (older runs, runs the last daemon
/// lost mid-flight), so the index covers runs from before it existed.
pub fn backfill() {
    let ids: Vec<String> = std::fs::read_dir(dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().filter(|n| n.starts_with('r')).map(str::to_string))
        .collect();
    for id in ids {
        if run_dir(&id).is_ok() && !db::is_compacted(&id) && !store().live.lock().contains_key(&id) {
            index(&id);
        }
    }
}

fn status(id: &str, with_procs: bool) -> Result<Value> {
    let d = run_dir(id)?;
    let mut m = read_meta(&d)?;
    let live = store().live.lock().get(id).cloned();
    overlay(&d, &mut m, live.is_some());
    m["final_out_bytes"] = json!(terminal_files(&m, &d).iter().map(|p| size(p)).sum::<u64>());
    m["final_err_bytes"] = json!(size(&d.join("final.err")));
    if let (Some(l), true) = (live, with_procs) {
        m["procs"] = json!(descendants(l.pid));
    }
    Ok(m)
}

/// Live processes under `root` (inclusive), from ps.
fn descendants(root: u32) -> Vec<Value> {
    let Ok(o) = std::process::Command::new("ps").args(["-axo", "pid=,ppid=,stat=,etime=,command="]).output() else { return vec![] };
    let text = String::from_utf8_lossy(&o.stdout);
    let mut rows = vec![];
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(stat), Some(etime)) = (it.next(), it.next(), it.next(), it.next()) else { continue };
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else { continue };
        let command: Vec<&str> = it.collect();
        rows.push((pid, ppid, stat.to_string(), etime.to_string(), command.join(" ")));
    }
    let mut out = vec![];
    let mut walk = vec![(root, 0usize)];
    while let Some((p, depth)) = walk.pop() {
        if let Some(r) = rows.iter().find(|r| r.0 == p) {
            out.push(json!({ "pid": r.0, "ppid": r.1, "depth": depth, "stat": r.2, "etime": r.3, "command": r.4 }));
        }
        for r in rows.iter().filter(|r| r.1 == p) {
            walk.push((r.0, depth + 1));
        }
    }
    out
}

/// Each run's meta as `status` gives it; a REPL's run (no session) also gets `owner`, the session its REPL's cells
/// were posted under, when the index knows one.
fn list(session: Option<u64>, limit: usize, before: Option<u64>, q: Option<&str>) -> Result<Value> {
    let q = q.map(str::trim).filter(|q| !q.is_empty()).map(str::to_lowercase);
    let mut ids: Vec<u64> = std::fs::read_dir(dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_prefix('r')?.parse().ok())
        .filter(|&n: &u64| before.is_none_or(|b| n < b))
        .collect();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    let mut out = vec![];
    let mut owners: BTreeMap<String, Option<u64>> = BTreeMap::new();
    for n in ids {
        if out.len() >= limit {
            break;
        }
        let id = format!("r{n}");
        let Ok(m) = status(&id, false) else { continue };
        if session.is_some() && m["session"].as_u64() != session {
            continue;
        }
        if let Some(q) = &q
            && id != *q
            && !m["cmd"].as_str().is_some_and(|c| c.to_lowercase().contains(q.as_str()))
        {
            continue;
        }
        let mut m = m;
        if m["session"].is_null()
            && let Some(repl) = m["repl"].as_str().map(str::to_owned)
        {
            let owner = *owners.entry(repl.clone()).or_insert_with(|| crate::db::repl_owner(&repl));
            m["owner"] = json!(owner);
        }
        out.push(m);
    }
    Ok(json!({ "runs": out }))
}

/// Files making up the caller-visible stdout: the last stage of each pipeline, in order.
fn terminal_files(m: &Value, d: &Path) -> Vec<PathBuf> {
    m["pipelines"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["stages"].as_array()?.last()?.as_u64())
        .map(|k| d.join(format!("s{k}.out")))
        .collect()
}

/// Read up to `max` bytes at `offset` of the concatenation of `files`.
fn read_concat(files: &[PathBuf], mut offset: u64, max: usize) -> Result<(Vec<u8>, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut data = vec![];
    let mut total = 0;
    for f in files {
        let len = size(f);
        total += len;
        if data.len() >= max {
            continue;
        }
        if offset >= len {
            offset -= len;
            continue;
        }
        let mut fh = std::fs::File::open(f)?;
        fh.seek(SeekFrom::Start(offset))?;
        let want = (max - data.len()).min((len - offset) as usize);
        let mut buf = vec![0u8; want];
        let n = fh.read(&mut buf)?;
        data.extend_from_slice(&buf[..n]);
        offset = 0;
    }
    Ok((data, total))
}

/// One write of a stage's tee: when, where in which stream file, how long.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Chunk {
    pub ts_ms: u64,
    pub offset: u64,
    pub len: u32,
    pub err: bool,
}

const IDX_REC: usize = 24;

/// The chunk index file of a stage stream file (`sN.out` / `sN.err` -> `sN.idx`) and the stream's tag there.
fn idx_of(path: &Path) -> Option<(PathBuf, u8)> {
    let stream = match path.extension()?.to_str()? {
        "out" => 0,
        "err" => 1,
        _ => return None,
    };
    let stem = path.file_stem()?.to_str()?;
    stem.strip_prefix('s')?.parse::<usize>().ok()?;
    Some((path.with_extension("idx"), stream))
}

/// A stage's chunk index, in write order; None when it has none (a run from before the index).
pub fn chunks(idx: &Path) -> Option<Vec<Chunk>> {
    let b = std::fs::read(idx).ok()?;
    Some(
        b.chunks_exact(IDX_REC)
            .map(|r| Chunk {
                ts_ms: u64::from_le_bytes(r[0..8].try_into().unwrap()),
                offset: u64::from_le_bytes(r[8..16].try_into().unwrap()),
                len: u32::from_le_bytes(r[16..20].try_into().unwrap()),
                err: r[20] == 1,
            })
            .collect(),
    )
}

/// For a stage stream file: (offset, ts_ms) of each of its chunks, by offset (the archive's line timestamps).
pub fn chunk_times(path: &Path) -> Option<Vec<(u64, u64)>> {
    let (idx, stream) = idx_of(path)?;
    let mut v: Vec<(u64, u64)> = chunks(&idx)?.into_iter().filter(|c| u8::from(c.err) == stream).map(|c| (c.offset, c.ts_ms)).collect();
    v.sort_unstable();
    Some(v)
}

/// stdout and stderr together in the order they were written: the stage's (or, for None, every terminal stage's
/// stdout and every stage's stderr), as `chunks` of `{err, data}`; `offset` counts bytes of that merged sequence.
fn read_ordered(id: &str, stage: Option<usize>, offset: u64, max: usize, wait_ms: u64) -> Result<Value> {
    let d = run_dir(id)?;
    let m = read_meta(&d)?;
    let n = m["stages"].as_array().map_or(0, Vec::len);
    let terminal: Vec<usize> = match stage {
        Some(k) if k >= n => bail!("run {id} has {n} stages (0..{})", n.saturating_sub(1)),
        Some(k) => vec![k],
        None => m["pipelines"].as_array().into_iter().flatten().filter_map(|p| p["stages"].as_array()?.last()?.as_u64()).map(|k| k as usize).collect(),
    };
    let stages: Vec<usize> = stage.map_or_else(|| (0..n).collect(), |k| vec![k]);
    if !stages.iter().any(|k| d.join(format!("s{k}.idx")).exists()) && m["end_ms"].is_u64() {
        bail!("run {id} has no chunk index (it ran before the index existed): read out and err");
    }
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    loop {
        let done = !store().live.lock().contains_key(id);
        // (ts, stage, seq) orders the segments; within one stage the index is already in write order.
        let mut segs: Vec<(u64, usize, usize, Chunk)> = vec![];
        for &k in &stages {
            let Some(cs) = chunks(&d.join(format!("s{k}.idx"))) else { continue };
            for (i, c) in cs.into_iter().enumerate() {
                if c.err || terminal.contains(&k) {
                    segs.push((c.ts_ms, k, i, c));
                }
            }
        }
        segs.sort_unstable_by_key(|s| (s.0, s.1, s.2));
        let total: u64 = segs.iter().map(|s| u64::from(s.3.len)).sum();
        let mut out: Vec<(bool, Vec<u8>)> = vec![];
        let (mut pos, mut taken) = (0u64, 0usize);
        for (_, k, _, c) in &segs {
            let len = u64::from(c.len);
            if pos + len <= offset {
                pos += len;
                continue;
            }
            if taken >= max {
                break;
            }
            let skip = offset.saturating_sub(pos);
            let want = ((len - skip) as usize).min(max - taken);
            let path = d.join(format!("s{k}.{}", if c.err { "err" } else { "out" }));
            let (bytes, _) = read_concat(&[path], c.offset + skip, want)?;
            let short = (bytes.len() as u64) < len - skip;
            taken += bytes.len();
            pos += skip + bytes.len() as u64;
            match out.last_mut() {
                Some((e, b)) if *e == c.err => b.extend_from_slice(&bytes),
                _ => out.push((c.err, bytes)),
            }
            if short {
                break; // the file is shorter than its index says (still being written): read the rest next time
            }
        }
        let next = offset + taken as u64;
        let eof = next >= total;
        if taken > 0 || done || Instant::now() >= deadline {
            let chunks: Vec<Value> = out.iter().map(|(e, b)| json!({ "err": e, "data": String::from_utf8_lossy(b) })).collect();
            return Ok(json!({ "chunks": chunks, "offset": offset, "next_offset": next, "size": total, "eof": eof, "done": done }));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn read(id: &str, stage: Option<usize>, stream: &str, offset: u64, max: usize, wait_ms: u64) -> Result<Value> {
    if stream == "ordered" {
        return read_ordered(id, stage, offset, max, wait_ms);
    }
    let d = run_dir(id)?;
    let m = read_meta(&d)?;
    let files = match (stage, stream) {
        (Some(k), "out" | "stdout") => vec![d.join(format!("s{k}.out"))],
        (Some(k), "err" | "stderr") => vec![d.join(format!("s{k}.err"))],
        (None, "out" | "stdout" | "final") => terminal_files(&m, &d),
        (None, "err" | "stderr") => vec![d.join("final.err")],
        (_, s) => bail!("bad stream {s:?} (out | err | final)"),
    };
    if let Some(k) = stage {
        let n = m["stages"].as_array().map_or(0, Vec::len);
        if k >= n {
            bail!("run {id} has {n} stages (0..{})", n.saturating_sub(1));
        }
    }
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    loop {
        // Check liveness before reading, so `done` with `eof` means nothing more can arrive.
        let done = !store().live.lock().contains_key(id);
        let (mut data, total) = read_concat(&files, offset, max)?;
        // Don't split a UTF-8 character across reads.
        let keep = match std::str::from_utf8(&data) {
            Ok(_) => data.len(),
            Err(e) if e.error_len().is_none() && !(done && offset + data.len() as u64 >= total) => e.valid_up_to(),
            Err(_) => data.len(),
        };
        data.truncate(keep);
        let next = offset + data.len() as u64;
        let eof = next >= total;
        if !data.is_empty() || done || Instant::now() >= deadline {
            return Ok(json!({
                "data": String::from_utf8_lossy(&data), "offset": offset, "next_offset": next, "size": total,
                "eof": eof, "done": done,
            }));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn kill(id: &str) -> Result<Value> {
    run_dir(id)?;
    let Some(l) = store().live.lock().get(id).cloned() else { return Ok(json!({ "ok": true, "running": false })) };
    let pg = l.pid as i32;
    unsafe { libc::kill(-pg, libc::SIGTERM) };
    let mut done = l.done.clone();
    tokio::spawn(async move {
        if tokio::time::timeout(Duration::from_secs(3), done.wait_for(|d| *d)).await.is_err() {
            unsafe { libc::kill(-pg, libc::SIGKILL) };
        }
    });
    Ok(json!({ "ok": true, "running": true }))
}

async fn wait(id: &str, timeout_ms: Option<u64>) -> Result<Value> {
    run_dir(id)?;
    let live = store().live.lock().get(id).cloned();
    if let Some(l) = live {
        let mut done = l.done.clone();
        let fut = done.wait_for(|d| *d);
        match timeout_ms {
            Some(t) => {
                let _ = tokio::time::timeout(Duration::from_millis(t), fut).await;
            }
            None => {
                let _ = fut.await;
            }
        }
    }
    status(id, false)
}

/// Called from the daemon's blocking request handler (inside the tokio runtime's blocking pool).
pub fn handle(req: RunReq) -> Result<Value> {
    let rt = tokio::runtime::Handle::current();
    match req {
        RunReq::Run { cmd, cwd, mut env, clear_env, session, parent, stdin, repl } => {
            if let Some(r) = repl {
                env.insert("HARNESS_REPL".into(), r);
            }
            let _g = rt.enter();
            start(cmd, cwd, env, clear_env, session, parent, stdin, None, false)
        }
        RunReq::RunStdin { id, data, close } => write_stdin(&id, data.as_deref(), close),
        RunReq::RunStatus { id } => status(&id, true),
        RunReq::Runs { session, limit, before, q } => list(session, limit.unwrap_or(50), before, q.as_deref()),
        RunReq::RunRead { id, stage, stream, offset, max_bytes, wait_ms } => {
            read(&id, stage, &stream, offset, max_bytes.unwrap_or(1 << 20).min(16 << 20), wait_ms.min(60_000))
        }
        RunReq::RunKill { id } => {
            let _g = rt.enter();
            kill(&id)
        }
        RunReq::RunWait { id, timeout_ms } => rt.block_on(wait(&id, timeout_ms)),
    }
}

/// `harness tee FILE`: copy stdin to FILE and stdout. When stdout's reader goes away (EPIPE),
/// keep copying into FILE until stdin ends, so the upstream stage runs to completion.
pub fn tee(path: &str) -> Result<()> {
    use std::io::{Read, Write};
    let mut f = std::fs::File::create(path).with_context(|| format!("create {path}"))?;
    // A stage stream file gets its chunk index records (shared by the stage's out and err tees; O_APPEND keeps each
    // 24-byte record whole).
    let mut idx = idx_of(Path::new(path)).and_then(|(p, s)| Some((std::fs::OpenOptions::new().create(true).append(true).open(p).ok()?, s)));
    let mut at = 0u64;
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut pass = true;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        f.write_all(&buf[..n])?;
        if let Some((ix, stream)) = idx.as_mut() {
            let mut rec = [0u8; IDX_REC];
            rec[0..8].copy_from_slice(&now_ms().to_le_bytes());
            rec[8..16].copy_from_slice(&at.to_le_bytes());
            rec[16..20].copy_from_slice(&(n as u32).to_le_bytes());
            rec[20] = *stream;
            ix.write_all(&rec)?;
        }
        at += n as u64;
        if pass && (stdout.write_all(&buf[..n]).is_err() || stdout.flush().is_err()) {
            pass = false;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sp(s: &str) -> Option<Vec<(String, Vec<String>)>> {
        split(s).map(|v| v.into_iter().map(|(o, st)| (o.text().to_string(), st)).collect())
    }

    #[test]
    fn splits() {
        assert_eq!(sp("seq 1 9 | head -n3"), Some(vec![(";".into(), vec!["seq 1 9".into(), "head -n3".into()])]));
        assert_eq!(sp("false && echo no || echo yes").unwrap().len(), 3);
        assert_eq!(sp("echo 'a|b' \"$(x | y)\" | wc 2>&1").unwrap()[0].1.len(), 2);
        assert_eq!(sp("cd x && { a; b; } | c").unwrap()[1].1, vec!["{ a; b; }".to_string(), "c".into()]);
        assert!(sp("sleep 1 &").is_none());
        assert!(sp("for i in 1 2; do echo $i; done").is_none());
        assert!(sp("cat <<EOF\nx\nEOF").is_none());
        assert!(sp("a |").is_none());
        assert_eq!(sp("a &> f; b").unwrap().len(), 2);
    }
}
