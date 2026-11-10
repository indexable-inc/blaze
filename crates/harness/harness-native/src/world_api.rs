//! The app's world, served to agents: a Unix socket (`$HARNESS_HOME/app.sock`, the person's own) where a kernel's
//! `world.*` (mcp/src/world.ts) queries and edits the flecs world the app is drawn from: entities and their components
//! as flecs serializes them, the scene's tokens (`export mut` variables: the theme), and flecs script. One schema
//! ([`SCHEMA`]), deterministic CBOR in harness's frames: a request is `{schema, call}`, its answer `{ok: value}` or
//! `{error}`, naming what was refused and why.
//!
//! The socket's threads own no world: each request goes to the main thread ([`Pending`], drained each frame by
//! `App::step_world`), which answers it between frames; a request it does not answer within [`ANSWER_WAIT`] is told so.
//!
//! An edit asked with `save` is kept in the journal (`$HARNESS_HOME/world.json`: the edits in order, each as it was
//! asked but with the whole value it left, so a later launch reaches the same world) and replayed after the scene loads.
//! A later edit of the same thing replaces the earlier one. `saved` lists it; `forget` drops one or all.
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, sync_channel};
use std::time::Duration;

use harness_proto::frame::{self, T_CBOR};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The schema of the socket's messages: a kernel on another one is refused, naming both.
pub const SCHEMA: u32 = 1;
/// How long a request waits for the main thread's answer.
const ANSWER_WAIT: Duration = Duration::from_secs(10);
/// How long a connection may sit between requests.
const IDLE: Duration = Duration::from_secs(30);
/// Connections served at once; one more is refused.
const CONNS: usize = 16;

/// One thing asked of the world.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Call {
    /// The entities flecs query `expr` matches, with the values of its terms.
    Query { expr: String },
    /// Entity `path` whole: its tags, pairs and components with their values.
    Get { path: String },
    /// Write `value` (the members it names) into `component` of entity `path`.
    Set {
        path: String,
        component: String,
        value: Value,
        #[serde(default)]
        save: bool,
    },
    /// A scene variable (a token): read it, or with `value` write it (an `export mut` only; the members it names).
    Token {
        name: String,
        #[serde(default)]
        value: Option<Value>,
        #[serde(default)]
        save: bool,
    },
    /// Run flecs script `code`.
    Script {
        code: String,
        #[serde(default)]
        save: bool,
    },
    /// Every component's type, or `component`'s.
    Schema {
        #[serde(default)]
        component: Option<String>,
    },
    /// The journal: the edits kept, in order.
    Saved,
    /// Drop edit `index` of the journal, or all of it. What it did stays until the app is launched again.
    Forget {
        #[serde(default)]
        index: Option<usize>,
    },
}

/// A request as it arrives.
#[derive(Deserialize)]
struct Envelope {
    schema: u32,
    call: Value,
}

/// A call on its way to the main thread, and where its answer goes.
pub struct Pending {
    pub call: Call,
    pub answer: SyncSender<Result<Value, String>>,
}

/// The socket's path.
pub fn sock() -> PathBuf {
    crate::link::home().join("app.sock")
}

/// Serve the socket on threads of its own: each call comes out of the receiver, `wake` called so the main thread
/// draws a frame and answers it.
///
/// # Errors
/// Another app already serves the socket, or it cannot be bound (named with its path).
pub fn serve(wake: crate::app::Wake) -> Result<Receiver<Pending>, String> {
    let path = sock();
    if UnixStream::connect(&path).is_ok() {
        return Err(format!("another harness app already serves {}", path.display()));
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).map_err(|e| format!("binding {}: {e}", path.display()))?;
    // The person's own: no other user may ask.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(|e| format!("{}: {e}", path.display()))?;
    let (tx, rx) = std::sync::mpsc::channel::<Pending>();
    let live = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name("world-api".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut s) = conn else { continue };
                if live.fetch_add(1, Ordering::AcqRel) >= CONNS {
                    live.fetch_sub(1, Ordering::AcqRel);
                    let _ = answer(&mut s, &Err(format!("the app already serves {CONNS} world connections: close one")));
                    continue;
                }
                let (tx, wake, live) = (tx.clone(), wake.clone(), live.clone());
                let spawned = std::thread::Builder::new().name("world-conn".into()).spawn(move || {
                    connection(s, &tx, &wake);
                    live.fetch_sub(1, Ordering::AcqRel);
                });
                if let Err(e) = spawned {
                    eprintln!("harness-native: world api: a connection's thread: {e}");
                }
            }
        })
        .map_err(|e| format!("the world api's thread: {e}"))?;
    Ok(rx)
}

/// Write one answer.
fn answer(s: &mut UnixStream, r: &Result<Value, String>) -> std::io::Result<()> {
    let m = match r {
        Ok(v) => json!({"ok": v}),
        Err(e) => json!({"error": e}),
    };
    s.write_all(&frame::frame(T_CBOR, &harness_proto::cbor::encode(&m)))
}

/// One connection: each request answered in turn, until it closes, idles past [`IDLE`] or sends what is not a request.
fn connection(mut s: UnixStream, tx: &Sender<Pending>, wake: &crate::app::Wake) {
    if s.set_read_timeout(Some(IDLE)).is_err() {
        return;
    }
    while let Ok(Some((t, body))) = frame::read(&mut s) {
        let r = if t == T_CBOR { ask(&body, tx, wake) } else { Err(format!("a frame of type {t} where a request belongs")) };
        if answer(&mut s, &r).is_err() {
            return;
        }
    }
}

/// One request's answer: read, checked, handed to the main thread and waited for.
fn ask(body: &[u8], tx: &Sender<Pending>, wake: &crate::app::Wake) -> Result<Value, String> {
    let env: Envelope = harness_proto::cbor::decode(body).map_err(|e| format!("a request is {{schema, call}} in CBOR: {e}"))?;
    if env.schema != SCHEMA {
        return Err(format!("this app speaks world schema {SCHEMA}, the request {}: restart the older of the two", env.schema));
    }
    let call: Call = serde_json::from_value(env.call).map_err(|e| format!("not a world call: {e}"))?;
    let (answer, got) = sync_channel(1);
    tx.send(Pending { call, answer }).map_err(|_| "the app is quitting".to_owned())?;
    wake();
    got.recv_timeout(ANSWER_WAIT).map_err(|_| format!("the app did not answer within {}s (its main thread is busy)", ANSWER_WAIT.as_secs()))?
}

/// The journal's path.
fn journal_path() -> PathBuf {
    crate::link::home().join("world.json")
}

/// The journal: the edits kept, in order. None there is an empty one.
///
/// # Errors
/// A journal that cannot be read or is not a list of calls, named with its path: it is never silently taken for empty.
pub fn journal() -> Result<Vec<Call>, String> {
    let path = journal_path();
    match std::fs::read(&path) {
        Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("{}: not a list of world edits: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Write the journal whole: beside it, then renamed over it, so a crash leaves the old one or the new.
fn write_journal(edits: &[Call]) -> Result<(), String> {
    let path = journal_path();
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_vec_pretty(edits).map_err(|e| format!("the journal: {e}"))?;
    std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, &path)).map_err(|e| format!("{}: {e}", path.display()))
}

/// Whether `a` and `b` edit the same thing (one component of one entity, one token), so the later replaces the earlier.
fn same_target(a: &Call, b: &Call) -> bool {
    match (a, b) {
        (Call::Set { path: p, component: c, .. }, Call::Set { path: q, component: d, .. }) => p == q && c == d,
        (Call::Token { name: n, .. }, Call::Token { name: m, .. }) => n == m,
        _ => false,
    }
}

/// `edits` with `edit` kept: in place of an earlier edit of the same thing, at the end.
#[must_use]
pub fn kept(mut edits: Vec<Call>, edit: Call) -> Vec<Call> {
    edits.retain(|e| !same_target(e, &edit));
    edits.push(edit);
    edits
}

/// Keep `edit` in the journal.
///
/// # Errors
/// The journal cannot be read or written (named).
pub fn keep(edit: Call) -> Result<(), String> {
    write_journal(&kept(journal()?, edit))
}

/// Drop edit `index` of the journal, or all of it; how many are left.
///
/// # Errors
/// No such edit, or the journal cannot be read or written (named).
pub fn forget(index: Option<usize>) -> Result<usize, String> {
    let mut edits = journal()?;
    match index {
        Some(i) if i < edits.len() => _ = edits.remove(i),
        Some(i) => return Err(format!("the journal has {} edits: none at {i}", edits.len())),
        None => edits.clear(),
    }
    write_journal(&edits)?;
    Ok(edits.len())
}

/// `into` with `from` laid over it: an object's members one by one, anything else whole. How a value that names only
/// some members becomes the whole value to write.
pub fn merge(into: &mut Value, from: Value) {
    match (into, from) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                merge(a.entry(k).or_insert(Value::Null), v);
            }
        }
        (slot, v) => *slot = v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_later_edit_of_the_same_thing_replaces_the_earlier() {
        let set = |c: &str, v: i64| Call::Set { path: "hn.Theme.card".into(), component: c.into(), value: json!(v), save: true };
        let token = |v: i64| Call::Token { name: "tone_accent".into(), value: Some(json!(v)), save: true };
        let script = Call::Script { code: "a {}".into(), save: true };
        let mut j = Vec::new();
        for e in [set("Fill", 1), token(1), script.clone(), set("Shape", 1), set("Fill", 2), token(2), script.clone()] {
            j = kept(j, e);
        }
        assert_eq!(j, [script.clone(), set("Shape", 1), set("Fill", 2), token(2), script]);
    }

    #[test]
    fn a_partial_value_lays_over_the_whole() {
        let mut v = json!({"dark": {"r": 0.1, "g": 0.2}, "light": {"r": 1.0}});
        merge(&mut v, json!({"dark": {"g": 0.9}}));
        assert_eq!(v, json!({"dark": {"r": 0.1, "g": 0.9}, "light": {"r": 1.0}}));
    }

    #[test]
    fn a_call_reads_as_the_kernel_writes_it() {
        let c: Call = serde_json::from_value(json!({"op": "set", "path": "a.b", "component": "Fill", "value": {"x": 1}})).unwrap();
        assert_eq!(c, Call::Set { path: "a.b".into(), component: "Fill".into(), value: json!({"x": 1}), save: false });
        assert!(serde_json::from_value::<Call>(json!({"op": "nope"})).is_err());
    }
}
