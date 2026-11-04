//! The daemon link: harnessd's unix socket (`~/.harness/harnessd.sock`, frames of `[type u8][len u32 BE][payload]`, see
//! crates/harness/src/proto.rs), spoken from plain threads.
//!
//! - [`ensure_daemon`] connects, or starts `harness daemon` the way the CLI's `ensure_daemon` does (setsid, output appended
//!   to `~/.harness/daemon.log`) and waits for the socket.
//! - A poll thread asks `list` every [`POLL`] (the daemon pushes no list changes) and sends the world [`Msg::List`].
//! - Each attached object has a thread reading its stream (`attach`: a JSON ok, the snapshot, then live bytes and the
//!   exit) into [`Msg::Data`] and [`Msg::Exit`]; its write half takes the keyboard (`T_DATA`) and resizes (`T_RESIZE`).
use harness_native_model::{LinkState, Msg};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const T_JSON: u8 = 1;
pub const T_DATA: u8 = 2;
pub const T_RESIZE: u8 = 3;
pub const T_EXIT: u8 = 4;

const POLL: Duration = Duration::from_millis(400);

pub fn home() -> PathBuf {
    std::env::var_os("HARNESS_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".harness"))
}

pub fn sock() -> PathBuf {
    home().join("harnessd.sock")
}

fn write_frame(w: &mut impl Write, t: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(t);
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(payload);
    w.write_all(&buf)?;
    w.flush()
}

fn read_frame(r: &mut impl Read) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 5];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(head[1..].try_into().unwrap()) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(Some((head[0], buf)))
}

/// One request on a fresh connection, and its JSON answer.
pub fn request(v: &Value) -> std::io::Result<Value> {
    let mut s = UnixStream::connect(sock())?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    write_frame(&mut s, T_JSON, v.to_string().as_bytes())?;
    match read_frame(&mut s)? {
        Some((T_JSON, body)) => serde_json::from_slice(&body).map_err(std::io::Error::other),
        _ => Err(std::io::Error::other("no answer")),
    }
}

/// The `harness` binary to start the daemon with: `$HARNESS_BIN`, else this workspace's `target/release/harness` (or
/// debug), else `harness` on PATH.
fn harness_bin() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("HARNESS_BIN") {
        return Some(p.into());
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    for p in ["target/release/harness", "target/debug/harness"] {
        let p = root.join(p);
        if p.is_file() {
            return Some(p);
        }
    }
    std::env::var_os("PATH").and_then(|path| std::env::split_paths(&path).map(|d| d.join("harness")).find(|p| p.is_file()))
}

/// Connect, or start the daemon and wait for its socket (100 tries 50 ms apart, as the CLI does).
pub fn ensure_daemon() -> Result<(), String> {
    if UnixStream::connect(sock()).is_ok() {
        return Ok(());
    }
    let bin = harness_bin().ok_or("no harness binary: build it (cargo build -p harness --release) or set HARNESS_BIN")?;
    std::fs::create_dir_all(home()).map_err(|e| e.to_string())?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(home().join("daemon.log")).map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("daemon").env("HARNESS_NO_OPEN", "1").stdin(std::process::Stdio::null()).stdout(log.try_clone().map_err(|e| e.to_string())?).stderr(log);
    // SAFETY: setsid in the child before exec only; it touches no state of this process.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().map_err(|e| format!("start {}: {e}", bin.display()))?;
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(50));
        if UnixStream::connect(sock()).is_ok() {
            return Ok(());
        }
    }
    Err(format!("harnessd did not start; see {}", home().join("daemon.log").display()))
}

type Wake = Arc<dyn Fn() + Send + Sync>;

/// The app's side of the daemon: what it can ask from the main thread. Every call returns at once; answers arrive as
/// [`Msg`]s.
#[derive(Clone)]
pub struct Link {
    tx: Sender<Msg>,
    wake: Wake,
    /// The write half of each attach stream, by session id.
    streams: Arc<Mutex<HashMap<u64, (u64, UnixStream)>>>,
    poke: Sender<()>,
}

impl Link {
    pub fn start(tx: Sender<Msg>, wake: Wake) -> Self {
        let (poke, poked) = std::sync::mpsc::channel::<()>();
        let link = Self { tx: tx.clone(), wake: wake.clone(), streams: Arc::default(), poke };
        std::thread::Builder::new()
            .name("harness-list".into())
            .spawn(move || {
                let mut up = None;
                loop {
                    let state = match request(&json!({"op": "list"})) {
                        Ok(v) => {
                            let sessions = v["sessions"].as_array().cloned().unwrap_or_default();
                            let _ = tx.send(Msg::List(sessions));
                            LinkState::Up
                        }
                        Err(_) => {
                            if up != Some(LinkState::Connecting) {
                                let _ = tx.send(Msg::Link(LinkState::Connecting));
                                wake();
                            }
                            match ensure_daemon() {
                                Ok(()) => LinkState::Connecting,
                                Err(e) => {
                                    eprintln!("harness-native: {e}");
                                    LinkState::Down
                                }
                            }
                        }
                    };
                    if up != Some(state) {
                        let _ = tx.send(Msg::Link(state));
                        up = Some(state);
                    }
                    wake();
                    // Woken early by a spawn or a kill, so the tree follows at once.
                    let _ = poked.recv_timeout(POLL);
                    while poked.try_recv().is_ok() {}
                }
            })
            .expect("list thread");
        link
    }

    pub fn attached(&self, id: u64) -> bool {
        self.streams.lock().unwrap().contains_key(&id)
    }

    /// Attach to `id` at `cols` x `rows`: its stream's bytes come as `Msg::Data`.
    pub fn attach(&self, id: u64, cols: u16, rows: u16) {
        if self.attached(id) {
            return;
        }
        let Ok(mut s) = UnixStream::connect(sock()) else { return };
        if write_frame(&mut s, T_JSON, json!({"op": "attach", "id": id, "cols": cols, "rows": rows}).to_string().as_bytes()).is_err() {
            return;
        }
        let Ok(w) = s.try_clone() else { return };
        // A generation per attach: a stream detached and attached again is not removed by the old reader as it ends.
        static GEN: AtomicU64 = AtomicU64::new(1);
        let generation = GEN.fetch_add(1, Ordering::Relaxed);
        self.streams.lock().unwrap().insert(id, (generation, w));
        let (tx, wake, streams) = (self.tx.clone(), self.wake.clone(), self.streams.clone());
        std::thread::Builder::new()
            .name(format!("harness-attach-{id}"))
            .spawn(move || {
                // The ok (or an error) first.
                if let Ok(Some((T_JSON, body))) = read_frame(&mut s) {
                    let v: Value = serde_json::from_slice(&body).unwrap_or_default();
                    if v.get("error").is_some() {
                        eprintln!("harness-native: attach {id}: {v}");
                    }
                }
                while let Ok(Some((t, body))) = read_frame(&mut s) {
                    match t {
                        T_DATA => {
                            let _ = tx.send(Msg::Data { id, bytes: body });
                        }
                        T_EXIT => {
                            let code = serde_json::from_slice::<Value>(&body).ok().and_then(|v| v["code"].as_i64()).unwrap_or(0);
                            let _ = tx.send(Msg::Exit { id, code: code as i32 });
                        }
                        _ => {}
                    }
                    wake();
                }
                let mut map = streams.lock().unwrap();
                if map.get(&id).is_some_and(|(g, _)| *g == generation) {
                    map.remove(&id);
                    drop(map);
                    let _ = tx.send(Msg::Detached { id });
                    wake();
                }
            })
            .expect("attach thread");
    }

    /// Detach from `id` (a JSON frame ends the attach; the session keeps running).
    pub fn detach(&self, id: u64) {
        if let Some((_, mut s)) = self.streams.lock().unwrap().remove(&id) {
            let _ = write_frame(&mut s, T_JSON, b"{}");
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }

    /// Keyboard bytes into `id`'s PTY.
    pub fn input(&self, id: u64, bytes: &[u8]) {
        if let Some((_, s)) = self.streams.lock().unwrap().get_mut(&id) {
            let _ = write_frame(s, T_DATA, bytes);
        }
    }

    pub fn resize(&self, id: u64, cols: u16, rows: u16) {
        if let Some((_, s)) = self.streams.lock().unwrap().get_mut(&id) {
            let _ = write_frame(s, T_RESIZE, json!({"cols": cols, "rows": rows}).to_string().as_bytes());
        }
    }

    /// A new object of `kind` in `cwd`; `then` gets its id.
    pub fn spawn(&self, kind: &str, cwd: Option<String>, cols: u16, rows: u16, then: impl FnOnce(u64) + Send + 'static) {
        let (kind, poke) = (kind.to_owned(), self.poke.clone());
        std::thread::spawn(move || match request(&json!({"op": "spawn", "kind": kind, "cwd": cwd, "cols": cols, "rows": rows})) {
            Ok(v) => match v["id"].as_u64() {
                Some(id) => {
                    let _ = poke.send(());
                    then(id);
                }
                None => eprintln!("harness-native: spawn {kind}: {v}"),
            },
            Err(e) => eprintln!("harness-native: spawn {kind}: {e}"),
        });
    }

    pub fn kill(&self, id: u64) {
        let poke = self.poke.clone();
        std::thread::spawn(move || {
            if let Err(e) = request(&json!({"op": "kill", "id": id})) {
                eprintln!("harness-native: kill {id}: {e}");
            }
            std::thread::sleep(Duration::from_millis(150));
            let _ = poke.send(());
        });
    }
}
