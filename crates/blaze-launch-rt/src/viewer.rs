//! Local loopback live telemetry viewer (`blaze-launcher --viewer`, `127.0.0.1:41937`).
//!
//! Serves `https://ix.dev/blaze/explore` (and local `/explore`) over loopback HTTP + RFC 6455
//! WebSocket with zero third-party dependencies:
//! - `GET /api/info` -> `{"version":1,"active_viewers":<n>}` (lets `blaze run` skip opening a
//!   duplicate browser tab whenever an Explore tab is already connected).
//! - `GET /api/runs` -> current run summaries snapshot.
//! - `GET /ws` -> live stream of `runs`, `run_started`, `events`, and `run_ended` messages drained
//!   from every shared-memory `*.telem` ring under `<runs_dir>`.
//! - `GET /explore` -> self-contained local fallback UI when offline.
//!
//! Upheld invariants:
//! - Loopback-only (`127.0.0.1`), CORS + Private Network Access headers restricted to `https://ix.dev`
//!   and loopback origins.
//! - Every loop, frame read, header size, client count, and ring drain per tick has a hard bound.
//! - Idle expiry (rule 70): exits cleanly once both `active_viewers == 0` and `active_runs == 0`
//!   continuously for `idle_expiry`.

use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::telemetry::MappedRing;

/// Default loopback port matching `site/explore/protocol.ts` and `blaze_run::viewer::DEFAULT_VIEWER_PORT`.
pub const DEFAULT_PORT: u16 = 41937;

/// Hosted Explore page opened by default when `active_viewers == 0`.
pub const DEFAULT_EXPLORE_URL: &str = "https://ix.dev/blaze/explore";

/// Default idle expiry: 120 seconds with zero active runs and zero connected browser tabs.
pub const DEFAULT_IDLE_EXPIRY: Duration = Duration::from_secs(120);

const MAX_HTTP_HEADER_BYTES: usize = 8192;
const MAX_CLIENTS: usize = 32;
const MAX_DRAIN_PER_RUN_PER_TICK: usize = 256;
const MAX_RUNS_TRACKED: usize = 64;
const ACTIVE_TICK: Duration = Duration::from_millis(15);
const MAX_IDLE_TICK: Duration = Duration::from_millis(200);
const DIR_SCAN_INTERVAL: Duration = Duration::from_millis(200);
const BIN_EVENT_MAGIC: u8 = b'B';
const BIN_EVENT_VERSION: u8 = 1;
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Typed error for viewer startup and loopback operations.
#[derive(Debug)]
pub enum ViewerError {
    Bind { addr: SocketAddr, io: std::io::Error },
    RunsDir { path: PathBuf, io: std::io::Error },
    Spawn { thread: &'static str, io: std::io::Error },
}

impl core::fmt::Display for ViewerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ViewerError::Bind { addr, io } => write!(f, "blaze viewer: cannot bind {addr}: {io}"),
            ViewerError::RunsDir { path, io } => write!(f, "blaze viewer: runs dir {}: {io}", path.display()),
            ViewerError::Spawn { thread, io } => write!(f, "blaze viewer: spawn {thread}: {io}"),
        }
    }
}

impl core::error::Error for ViewerError {}

/// Configuration for running [`ViewerServer`].
#[derive(Clone, Debug)]
pub struct ViewerConfig {
    pub runs_dir: PathBuf,
    pub port: u16,
    pub idle_expiry: Duration,
}

impl ViewerConfig {
    pub fn for_home(home: &Path) -> Self {
        Self {
            runs_dir: home.join(".blaze/v2/runs"),
            port: DEFAULT_PORT,
            idle_expiry: DEFAULT_IDLE_EXPIRY,
        }
    }
}

/// Metadata written by `blaze run` beside `<run_id>.telem` as `<run_id>.meta`.
#[derive(Clone, Debug)]
struct RunMeta {
    pid: u32,
    package: String,
    started_ms: u64,
    slots: BTreeMap<u32, String>,
}

struct TrackedRun {
    id: String,
    meta: RunMeta,
    ring: MappedRing,
    alive: bool,
    ended_notified: bool,
    dead_since: Option<Instant>,
}

struct SharedState {
    active_viewers: AtomicU32,
    stop: AtomicBool,
    clients: Mutex<Vec<TcpStream>>,
    runs_snapshot: Mutex<String>,
    builds_snapshot: Mutex<String>,
}

/// Running local viewer server handle (for both CLI `--viewer` and tests).
pub struct ViewerHandle {
    addr: SocketAddr,
    shared: Arc<SharedState>,
    accept_thread: Option<std::thread::JoinHandle<()>>,
    pump_thread: Option<std::thread::JoinHandle<()>>,
}

impl ViewerHandle {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn active_viewers(&self) -> u32 {
        self.shared.active_viewers.load(Ordering::Acquire)
    }

    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        // Wake blocking accept() with a quick loopback connect.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(50));
        if let Some(t) = self.accept_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.pump_thread.take() {
            let _ = t.join();
        }
    }

    /// Blocks until the viewer stops (e.g. idle expiry).
    pub fn wait(mut self) {
        if let Some(t) = self.pump_thread.take() {
            let _ = t.join();
        }
        self.shared.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(50));
        if let Some(t) = self.accept_thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for ViewerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts the loopback viewer server on `127.0.0.1:<config.port>`.
pub fn start(config: ViewerConfig) -> Result<ViewerHandle, ViewerError> {
    std::fs::create_dir_all(&config.runs_dir).map_err(|io| ViewerError::RunsDir {
        path: config.runs_dir.clone(),
        io,
    })?;
    let bind_addr = SocketAddr::from(([127, 0, 0, 1], config.port));
    let listener = TcpListener::bind(bind_addr).map_err(|io| ViewerError::Bind { addr: bind_addr, io })?;
    let addr = listener.local_addr().map_err(|io| ViewerError::Bind { addr: bind_addr, io })?;
    let _ = listener.set_nonblocking(false);

    let shared = Arc::new(SharedState {
        active_viewers: AtomicU32::new(0),
        stop: AtomicBool::new(false),
        clients: Mutex::new(Vec::new()),
        runs_snapshot: Mutex::new(String::from(r#"{"type":"runs","runs":[]}"#)),
        builds_snapshot: Mutex::new(String::from(r#"{"type":"builds","builds":[]}"#)),
    });

    let accept_shared = Arc::clone(&shared);
    let accept_thread = std::thread::Builder::new()
        .name(String::from("blaze-viewer-accept"))
        .spawn(move || accept_loop(listener, accept_shared))
        .map_err(|io| ViewerError::Spawn { thread: "blaze-viewer-accept", io })?;

    let pump_shared = Arc::clone(&shared);
    let pump_thread = std::thread::Builder::new()
        .name(String::from("blaze-viewer-pump"))
        .spawn(move || pump_loop(config, pump_shared))
        .map_err(|io| ViewerError::Spawn { thread: "blaze-viewer-pump", io })?;

    Ok(ViewerHandle {
        addr,
        shared,
        accept_thread: Some(accept_thread),
        pump_thread: Some(pump_thread),
    })
}

fn accept_loop(listener: TcpListener, shared: Arc<SharedState>) {
    for incoming in listener.incoming() {
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        let Ok(stream) = incoming else {
            continue;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
        let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
        handle_connection(stream, &shared);
    }
}

fn handle_connection(mut stream: TcpStream, shared: &Arc<SharedState>) {
    let mut buf = [0u8; MAX_HTTP_HEADER_BYTES];
    let mut len = 0usize;
    let mut header_end = None;
    for _ in 0..32 {
        match stream.read(&mut buf[len..]) {
            Ok(0) => return,
            Ok(n) => {
                len += n;
                if let Some(pos) = find_header_end(&buf[..len]) {
                    header_end = Some(pos);
                    break;
                }
                if len == buf.len() {
                    return;
                }
            }
            Err(_) => return,
        }
    }
    let Some(end) = header_end else {
        return;
    };
    let Ok(head) = core::str::from_utf8(&buf[..end]) else {
        return;
    };
    let mut lines = head.split("\r\n");
    let Some(request_line) = lines.next() else {
        return;
    };
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let raw_path = parts.next().unwrap_or("/");
    let path = raw_path.split('?').next().unwrap_or(raw_path);

    let mut origin: Option<&str> = None;
    let mut ws_key: Option<&str> = None;
    let mut upgrade_ws = false;

    for line in lines {
        let Some((name, val)) = line.split_once(':') else {
            continue;
        };
        let val = val.trim();
        if name.eq_ignore_ascii_case("origin") {
            origin = Some(val);
        } else if name.eq_ignore_ascii_case("sec-websocket-key") {
            ws_key = Some(val);
        } else if name.eq_ignore_ascii_case("upgrade") && val.eq_ignore_ascii_case("websocket") {
            upgrade_ws = true;
        }
    }

    let cors_origin = allowed_cors_origin(origin);

    if method == "OPTIONS" {
        let resp = format!(
            "HTTP/1.1 204 No Content\r\n\
             Access-Control-Allow-Origin: {cors_origin}\r\n\
             Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
             Access-Control-Allow-Headers: content-type\r\n\
             Access-Control-Allow-Private-Network: true\r\n\
             Content-Length: 0\r\n\
             Connection: close\r\n\r\n"
        );
        let _ = stream.write_all(resp.as_bytes());
        return;
    }

    if method == "GET" && path == "/ws" && upgrade_ws {
        let Some(key) = ws_key else {
            write_http(&mut stream, "400 Bad Request", "text/plain", cors_origin, "missing Sec-WebSocket-Key");
            return;
        };
        if let Ok(clients) = shared.clients.lock() {
            if clients.len() >= MAX_CLIENTS {
                write_http(&mut stream, "503 Service Unavailable", "text/plain", cors_origin, "viewer client cap reached");
                return;
            }
        }
        let accept = ws_accept_key(key);
        let handshake = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\r\n"
        );
        if stream.write_all(handshake.as_bytes()).is_err() {
            return;
        }
        let _ = stream.set_nonblocking(true);
        // Send initial runs and builds snapshots immediately.
        if let Ok(snap) = shared.runs_snapshot.lock() {
            let frame = ws_text_frame(&snap);
            let _ = stream.write_all(&frame);
        }
        if let Ok(bsnap) = shared.builds_snapshot.lock() {
            let frame = ws_text_frame(&bsnap);
            let _ = stream.write_all(&frame);
        }
        if let Ok(mut clients) = shared.clients.lock() {
            clients.push(stream);
            shared.active_viewers.store(u32::try_from(clients.len()).unwrap_or(u32::MAX), Ordering::Release);
        }
        return;
    }

    match (method, path) {
        ("GET", "/api/info") => {
            let active = shared.active_viewers.load(Ordering::Acquire);
            let body = format!(r#"{{"version":1,"active_viewers":{active}}}"#);
            write_http(&mut stream, "200 OK", "application/json", cors_origin, &body);
        }
        ("GET", "/api/runs") => {
            let body = shared
                .runs_snapshot
                .lock()
                .map(|s| s.clone())
                .unwrap_or_else(|_| String::from(r#"{"type":"runs","runs":[]}"#));
            write_http(&mut stream, "200 OK", "application/json", cors_origin, &body);
        }
        ("GET", "/api/builds") => {
            let body = shared
                .builds_snapshot
                .lock()
                .map(|s| s.clone())
                .unwrap_or_else(|_| String::from(r#"{"type":"builds","builds":[]}"#));
            write_http(&mut stream, "200 OK", "application/json", cors_origin, &body);
        }
        ("POST", "/api/toggle") => {
            write_http(&mut stream, "200 OK", "application/json", cors_origin, r#"{"ok":true}"#);
        }
        ("GET", "/" | "/explore") => {
            write_http(&mut stream, "200 OK", "text/html; charset=utf-8", cors_origin, LOCAL_EXPLORE_HTML);
        }
        _ => {
            write_http(&mut stream, "404 Not Found", "text/plain", cors_origin, "not found");
        }
    }
}

fn allowed_cors_origin(origin: Option<&str>) -> &str {
    match origin {
        Some(o) if o == "https://ix.dev" || o.starts_with("http://127.0.0.1:") || o.starts_with("http://localhost:") => o,
        _ => "https://ix.dev",
    }
}

fn write_http(stream: &mut TcpStream, status: &str, content_type: &str, cors_origin: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Access-Control-Allow-Origin: {cors_origin}\r\n\
         Access-Control-Allow-Private-Network: true\r\n\
         Cache-Control: no-store\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

fn pump_loop(config: ViewerConfig, shared: Arc<SharedState>) {
    let mut tracked: BTreeMap<String, TrackedRun> = BTreeMap::new();
    let mut builds: BTreeMap<String, (SystemTime, String)> = BTreeMap::new();
    let mut idle_since: Option<Instant> = None;
    let mut last_dir_scan = Instant::now() - DIR_SCAN_INTERVAL;
    let mut sleep_dur = ACTIVE_TICK;
    let mut tick: u64 = 0;
    let mut bin_scratch: Vec<u8> = Vec::with_capacity(4096);

    while !shared.stop.load(Ordering::Acquire) {
        tick = tick.wrapping_add(1);
        let now = Instant::now();
        let mut runs_changed = false;
        let mut builds_changed = false;
        let mut any_events = false;

        // 1. Poll existing clients for disconnect / ping / close frames so `active_viewers` stays exact.
        poll_clients(&shared);

        // 2. Discover new `*.telem` rings and `*.build.json` snapshots under `config.runs_dir` (~5 Hz).
        if now.duration_since(last_dir_scan) >= DIR_SCAN_INTERVAL {
            last_dir_scan = now;
            if let Ok(entries) = std::fs::read_dir(&config.runs_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                    if name.ends_with(".build.json") && builds.len() < MAX_RUNS_TRACKED {
                        let mtime = entry
                            .metadata()
                            .ok()
                            .and_then(|m| m.modified().ok())
                            .unwrap_or(UNIX_EPOCH);
                        let changed = match builds.get(name) {
                            Some((prev, _)) => *prev != mtime,
                            None => true,
                        };
                        if changed {
                            if let Ok(raw) = std::fs::read_to_string(&path) {
                                let trimmed = raw.trim();
                                if trimmed.starts_with('{') && trimmed.ends_with('}') {
                                    builds.insert(String::from(name), (mtime, String::from(trimmed)));
                                    builds_changed = true;
                                }
                            }
                        }
                        continue;
                    }
                    if tracked.len() >= MAX_RUNS_TRACKED {
                        continue;
                    }
                    if path.extension().and_then(|s| s.to_str()) != Some("telem") {
                        continue;
                    }
                    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                        continue;
                    };
                    if tracked.contains_key(stem) {
                        continue;
                    }
                    let Ok(cpath) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
                        continue;
                    };
                    let Ok(ring) = MappedRing::attach_file(&cpath) else {
                        continue;
                    };
                    let meta = read_meta(&config.runs_dir.join(format!("{stem}.meta")), stem);
                    let alive = meta.pid == 0 || pid_alive(meta.pid);
                    let run = TrackedRun {
                        id: String::from(stem),
                        meta,
                        ring,
                        alive,
                        ended_notified: false,
                        dead_since: if alive { None } else { Some(now) },
                    };
                    let started_msg = format!(r#"{{"type":"run_started","run":{}}}"#, format_run_json(&run));
                    tracked.insert(String::from(stem), run);
                    broadcast_text(&shared, &started_msg);
                    runs_changed = true;
                }
            }
        }

        // 3. Drain events from all tracked runs as packed 64-byte binary WebSocket frames (zero JSON!).
        let mut to_remove: Vec<String> = Vec::new();

        for (id, run) in tracked.iter_mut() {
            bin_scratch.clear();
            let id_bytes = id.as_bytes();
            let id_len = u16::try_from(id_bytes.len()).unwrap_or(u16::MAX);
            let hdr_len = (4 + usize::from(id_len) + 7) & !7;
            bin_scratch.push(BIN_EVENT_MAGIC);
            bin_scratch.push(BIN_EVENT_VERSION);
            bin_scratch.extend_from_slice(&id_len.to_le_bytes());
            bin_scratch.extend_from_slice(&id_bytes[..usize::from(id_len)]);
            bin_scratch.resize(hdr_len, 0);

            let mut drained = 0usize;
            while drained < MAX_DRAIN_PER_RUN_PER_TICK {
                let Some(ev) = run.ring.ring().pop() else {
                    break;
                };
                // SAFETY: `Event` is `#[repr(C, align(8))]`, 64 bytes, all plain integer fields.
                let raw: [u8; 64] = unsafe { core::mem::transmute(ev) };
                bin_scratch.extend_from_slice(&raw);
                drained += 1;
            }
            if drained > 0 {
                any_events = true;
                broadcast_binary(&shared, &bin_scratch);
            }

            if run.alive && run.meta.pid != 0 && !pid_alive(run.meta.pid) {
                run.alive = false;
                run.dead_since = Some(now);
                runs_changed = true;
            }
            if !run.alive && !run.ended_notified && drained == 0 {
                run.ended_notified = true;
                let ended = format!(r#"{{"type":"run_ended","id":"{}","status":0}}"#, escape_json(id));
                broadcast_text(&shared, &ended);
                runs_changed = true;
            }
            // Garbage-collect exited run files after 15s so the runs directory stays clean.
            if let Some(dead_at) = run.dead_since {
                if now.duration_since(dead_at) > Duration::from_secs(15) {
                    to_remove.push(id.clone());
                }
            }
        }

        for id in to_remove {
            tracked.remove(&id);
            let _ = std::fs::remove_file(config.runs_dir.join(format!("{id}.telem")));
            let _ = std::fs::remove_file(config.runs_dir.join(format!("{id}.meta")));
            runs_changed = true;
        }

        // 4. Refresh runs and builds snapshots periodically or on changes.
        if builds_changed {
            let mut bmsg = String::from(r#"{"type":"builds","builds":["#);
            for (idx, (_, raw)) in builds.values().enumerate() {
                if idx > 0 {
                    bmsg.push(',');
                }
                bmsg.push_str(raw);
            }
            bmsg.push_str("]}");
            if let Ok(mut guard) = shared.builds_snapshot.lock() {
                *guard = bmsg.clone();
            }
            broadcast_text(&shared, &bmsg);
        }
        if runs_changed || tick % 20 == 0 {
            let snap = format_runs_message(&tracked);
            if let Ok(mut guard) = shared.runs_snapshot.lock() {
                *guard = snap.clone();
            }
            if runs_changed || (tick % 40 == 0 && !tracked.is_empty()) {
                broadcast_text(&shared, &snap);
            }
        }

        // 5. Idle expiry check (rule 70: never linger forever with 0 runs and 0 viewers).
        let active_viewers = shared.active_viewers.load(Ordering::Acquire);
        let active_runs = tracked.values().filter(|r| r.alive).count();
        if active_viewers > 0 || active_runs > 0 {
            idle_since = None;
        } else {
            let since = *idle_since.get_or_insert(now);
            if now.duration_since(since) >= config.idle_expiry {
                shared.stop.store(true, Ordering::Release);
                break;
            }
        }

        // Adaptive backoff: 15ms while streaming, smoothly backing off to 200ms when quiet.
        if any_events || runs_changed {
            sleep_dur = ACTIVE_TICK;
        } else {
            sleep_dur = (sleep_dur * 2).min(MAX_IDLE_TICK);
        }
        std::thread::sleep(sleep_dur);
    }
}

fn poll_clients(shared: &Arc<SharedState>) {
    let Ok(mut clients) = shared.clients.lock() else {
        return;
    };
    let mut i = 0;
    let mut scratch = [0u8; 512];
    while i < clients.len() {
        let mut keep = true;
        match clients[i].read(&mut scratch) {
            Ok(0) => keep = false,
            Ok(n) => {
                // RFC 6455 close opcode = 0x8, ping = 0x9.
                let opcode = scratch[0] & 0x0f;
                if opcode == 0x8 {
                    let _ = clients[i].write_all(&[0x88, 0x00]);
                    keep = false;
                } else if opcode == 0x9 && n >= 2 {
                    let _ = clients[i].write_all(&[0x8a, 0x00]);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => keep = false,
        }
        if keep {
            i += 1;
        } else {
            clients.swap_remove(i);
        }
    }
    shared.active_viewers.store(u32::try_from(clients.len()).unwrap_or(u32::MAX), Ordering::Release);
}

fn broadcast_frame(shared: &Arc<SharedState>, frame: &[u8]) {
    let Ok(mut clients) = shared.clients.lock() else {
        return;
    };
    let mut i = 0;
    while i < clients.len() {
        let ok = match clients[i].write_all(frame) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(_) => false,
        };
        if ok {
            i += 1;
        } else {
            clients.swap_remove(i);
        }
    }
    shared.active_viewers.store(u32::try_from(clients.len()).unwrap_or(u32::MAX), Ordering::Release);
}

fn broadcast_text(shared: &Arc<SharedState>, text: &str) {
    let frame = ws_frame(0x81, text.as_bytes());
    broadcast_frame(shared, &frame);
}

fn broadcast_binary(shared: &Arc<SharedState>, payload: &[u8]) {
    let frame = ws_frame(0x82, payload);
    broadcast_frame(shared, &frame);
}

fn read_meta(path: &Path, fallback_id: &str) -> RunMeta {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let mut meta = RunMeta {
        pid: 0,
        package: String::from(fallback_id),
        started_ms: now_ms,
        slots: BTreeMap::new(),
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return meta;
    };
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("pid=") {
            if let Ok(p) = v.trim().parse::<u32>() {
                meta.pid = p;
            }
        } else if let Some(v) = line.strip_prefix("package=") {
            meta.package = String::from(v.trim());
        } else if let Some(v) = line.strip_prefix("started_ms=") {
            if let Ok(t) = v.trim().parse::<u64>() {
                meta.started_ms = t;
            }
        } else if let Some(v) = line.strip_prefix("slot=") {
            if let Some((k, name)) = v.split_once(':') {
                if let Ok(slot_id) = k.trim().parse::<u32>() {
                    meta.slots.insert(slot_id, String::from(name.trim()));
                }
            }
        }
    }
    meta
}

fn pid_alive(pid: u32) -> bool {
    let Ok(p) = i32::try_from(pid) else {
        return false;
    };
    if p <= 0 {
        return false;
    }
    // SAFETY: `kill(pid, 0)` checks existence without delivering a signal.
    let rc = unsafe { libc::kill(p, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn format_runs_message(tracked: &BTreeMap<String, TrackedRun>) -> String {
    let mut out = String::from(r#"{"type":"runs","runs":["#);
    for (idx, run) in tracked.values().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        out.push_str(&format_run_json(run));
    }
    out.push_str("]}");
    out
}

fn format_run_json(run: &TrackedRun) -> String {
    let stats = run.ring.ring().stats();
    let mut slots_json = String::from("{");
    for (idx, (k, v)) in run.meta.slots.iter().enumerate() {
        if idx > 0 {
            slots_json.push(',');
        }
        let _ = write!(slots_json, r#""{}":"{}""#, k, escape_json(v));
    }
    slots_json.push('}');
    format!(
        r#"{{"id":"{}","pid":{},"package":"{}","alive":{},"started_ms":{},"stats":{{"cap":{},"pushed":{},"popped":{},"dropped":{},"shadow_overflows":{},"unwound_spans":{}}},"slots":{slots_json}}}"#,
        escape_json(&run.id),
        run.meta.pid,
        escape_json(&run.meta.package),
        run.alive,
        run.meta.started_ms,
        stats.cap,
        stats.pushed,
        stats.popped,
        stats.dropped,
        stats.shadow_overflows,
        stats.unwound_spans,
    )
}

fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str(r#"\""#),
            '\\' => out.push_str(r"\\"),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\t' => out.push_str(r"\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Probes `http://127.0.0.1:<port>/api/info` with a tight loopback timeout.
/// Returns `Some(active_viewers)` when a live viewer responded.
pub fn probe_active_viewers(port: u16) -> Option<u32> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(120)).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(150)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(150)));
    stream
        .write_all(b"GET /api/info HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut resp = [0u8; 1024];
    let n = stream.read(&mut resp).ok()?;
    let text = core::str::from_utf8(&resp[..n]).ok()?;
    let key = r#""active_viewers":"#;
    let at = text.find(key)? + key.len();
    let digits: String = text[at..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u32>().ok()
}

/// Opens `url` in the user's default browser (`open` on macOS, `xdg-open` on Linux).
pub fn open_browser_url(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(not(target_os = "macos"))]
    let cmd = "xdg-open";
    std::process::Command::new(cmd)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

/// Encodes a server-to-client (unmasked) RFC 6455 frame (`0x81` text, `0x82` binary).
pub fn ws_frame(fin_opcode: u8, bytes: &[u8]) -> Vec<u8> {
    let len = bytes.len();
    let mut out = Vec::with_capacity(len + 10);
    out.push(fin_opcode);
    if len <= 125 {
        out.push(u8::try_from(len).unwrap_or(0));
    } else if len <= 65535 {
        out.push(126);
        let l16 = u16::try_from(len).unwrap_or(u16::MAX);
        out.extend_from_slice(&l16.to_be_bytes());
    } else {
        out.push(127);
        let l64 = u64::try_from(len).unwrap_or(u64::MAX);
        out.extend_from_slice(&l64.to_be_bytes());
    }
    out.extend_from_slice(bytes);
    out
}

/// Encodes a server-to-client (unmasked) RFC 6455 text frame.
pub fn ws_text_frame(text: &str) -> Vec<u8> {
    ws_frame(0x81, text.as_bytes())
}

/// RFC 6455 `Sec-WebSocket-Accept = base64(SHA-1(key || GUID))`.
pub fn ws_accept_key(key: &str) -> String {
    let mut input = Vec::with_capacity(key.len() + WS_GUID.len());
    input.extend_from_slice(key.as_bytes());
    input.extend_from_slice(WS_GUID.as_bytes());
    let digest = sha1(&input);
    base64_encode(&digest)
}

/// Minimal FIPS 180-4 SHA-1 solely for the RFC 6455 WebSocket handshake (not security crypto).
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x6745_2301;
    let mut h1: u32 = 0xEFCD_AB89;
    let mut h2: u32 = 0x98BA_DCFE;
    let mut h3: u32 = 0x1032_5476;
    let mut h4: u32 = 0xC3D2_E1F0;

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(data.len() + 72);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut out = [0u8; 20];
    out[0..4].copy_from_slice(&h0.to_be_bytes());
    out[4..8].copy_from_slice(&h1.to_be_bytes());
    out[8..12].copy_from_slice(&h2.to_be_bytes());
    out[12..16].copy_from_slice(&h3.to_be_bytes());
    out[16..20].copy_from_slice(&h4.to_be_bytes());
    out
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b0 = usize::from(chunk[0]);
        let b1 = if chunk.len() > 1 { usize::from(chunk[1]) } else { 0 };
        let b2 = if chunk.len() > 2 { usize::from(chunk[2]) } else { 0 };
        out.push(char::from(TABLE[b0 >> 2]));
        out.push(char::from(TABLE[((b0 & 0x03) << 4) | (b1 >> 4)]));
        out.push(if chunk.len() > 1 {
            char::from(TABLE[((b1 & 0x0f) << 2) | (b2 >> 6)])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(TABLE[b2 & 0x3f])
        } else {
            '='
        });
    }
    out
}

const LOCAL_EXPLORE_HTML: &str = r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><title>blaze explore (local)</title>
<style>body{background:#0b0c10;color:#e6e8ef;font:13px/1.5 ui-monospace,monospace;margin:0;padding:16px}
.hdr{display:flex;gap:12px;align-items:center;border-bottom:1px solid #232734;padding-bottom:10px;margin-bottom:12px}
.pill{background:#182436;color:#7cc4ff;padding:2px 8px;border-radius:999px;font-size:11px}
pre{background:#12141c;border:1px solid #232734;border-radius:8px;padding:12px;max-height:75vh;overflow:auto}</style></head>
<body><div class="hdr"><strong>blaze explore · local fallback</strong><span id="st" class="pill">connecting…</span>
<a href="https://ix.dev/blaze/explore" style="color:#7cc4ff;margin-left:auto">Open hosted ix.dev/blaze/explore ↗</a></div>
<pre id="log"></pre><script>
const st=document.getElementById('st'),log=document.getElementById('log');
function go(){const ws=new WebSocket(`ws://${location.host}/ws`);
ws.onopen=()=>{st.textContent='live (websocket)';};
ws.onclose=()=>{st.textContent='reconnecting…';setTimeout(go,500);};
ws.onmessage=(e)=>{log.textContent=e.data+'\n'+log.textContent.slice(0,24000);};}go();
</script></body></html>"#;

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_launch::telemetry::{AllocOp, Event, EventKind};

    #[test]
    fn rfc6455_accept_key_matches_spec_vector() {
        // RFC 6455 Section 1.3 official test vector.
        assert_eq!(ws_accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn viewer_tracks_active_viewers_streams_live_ring_events_and_expires_idle() {
        let dir = std::env::temp_dir().join(format!("blaze-viewer-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create runs dir");

        let mut handle = start(ViewerConfig {
            runs_dir: dir.clone(),
            port: 0,
            idle_expiry: Duration::from_secs(30),
        })
        .expect("start viewer");
        let port = handle.addr().port();

        // 1. Before any tab connects: active_viewers == 0 -> blaze run would open browser.
        assert_eq!(probe_active_viewers(port), Some(0));

        // 2. Connect a real RFC 6455 WebSocket client.
        let mut ws = TcpStream::connect(handle.addr()).expect("connect ws");
        ws.set_read_timeout(Some(Duration::from_secs(3))).expect("timeout");
        ws.write_all(
            b"GET /ws HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nOrigin: https://ix.dev\r\n\r\n",
        )
        .expect("send upgrade");

        let mut head = [0u8; 512];
        let n = ws.read(&mut head).expect("read handshake");
        let hdr_end = find_header_end(&head[..n]).expect("complete http response header");
        let text = core::str::from_utf8(&head[..hdr_end]).expect("utf8 handshake");
        assert!(text.starts_with("HTTP/1.1 101 Switching Protocols"), "unexpected response: {text}");
        assert!(text.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="), "bad accept: {text}");

        // 3. While the tab is connected: active_viewers == 1 -> blaze run skips opening duplicate tab!
        assert_eq!(probe_active_viewers(port), Some(1));

        // 4. Create a shared-memory telemetry ring + build snapshot in `runs_dir` and push live Span + Alloc events.
        let telem_path = dir.join("run-live-test.telem");
        let meta_path = dir.join("run-live-test.meta");
        let build_path = dir.join("run-live-test.build.json");
        std::fs::write(&meta_path, format!("pid={}\npackage=demo-app\nstarted_ms=12345\n", std::process::id()))
            .expect("write meta");
        std::fs::write(
            &build_path,
            r#"{"id":"run-live-test","package":"demo-app","command":"blaze run","status":"running","exit_code":null,"errors":0,"warnings":1,"reached":40,"found":36,"made":4,"placed":36,"resolved":4,"frontend_built":"no","recompile_reason":"Certificate changed","phases":[]}"#,
        )
        .expect("write build json");
        let telem_cpath = std::ffi::CString::new(telem_path.as_os_str().as_encoded_bytes()).expect("cpath");
        let ring = MappedRing::create(&telem_cpath, 64).expect("create ring");
        assert!(ring.ring().try_push(Event::span_enter(
            1,
            blaze_launch::Slot(7),
            1000,
            0x4444,
            blaze_launch::telemetry::SpanId(99),
            blaze_launch::telemetry::SpanId(0),
        )));
        assert!(ring.ring().try_push(Event::alloc(
            1,
            AllocOp::Malloc,
            1050,
            0xdeadbeef,
            512,
            [0x1000, 0x2000, 0x3000],
        )));

        // 5. Read WebSocket frames until we see both our `run_started` text frame and the
        //    packed binary event batch frame (`0x82`, magic `'B'`, version `1`, 2 x 64B Events).
        let mut raw_stream: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut saw_binary_events = false;
        let mut saw_builds = false;
        while Instant::now() < deadline {
            match ws.read(&mut buf) {
                Ok(0) => break,
                Ok(k) => {
                    raw_stream.extend_from_slice(&buf[..k]);
                    if !saw_builds && raw_stream.windows(19).any(|w| w == br#"Certificate changed"#) {
                        saw_builds = true;
                    }
                    // Locate binary batch header: [b'B', 1, 13, 0, b"run-live-test"...]
                    if !saw_binary_events {
                        if let Some(pos) = raw_stream.windows(17).position(|w| {
                            w[0] == BIN_EVENT_MAGIC && w[1] == BIN_EVENT_VERSION && &w[4..17] == b"run-live-test"
                        }) {
                            let hdr_len = (4 + 13 + 7) & !7; // 24 bytes
                            if raw_stream.len() >= pos + hdr_len + 128 {
                                let ev_bytes = &raw_stream[pos + hdr_len..pos + hdr_len + 128];
                                assert_eq!(ev_bytes[0], EventKind::SpanEnter as u8, "first binary event must be SpanEnter");
                                assert_eq!(ev_bytes[64], EventKind::Alloc as u8, "second binary event must be Alloc");
                                let alloc_size = u64::from_le_bytes(ev_bytes[64 + 32..64 + 40].try_into().expect("8 bytes"));
                                assert_eq!(alloc_size, 512, "binary Alloc event size must match 512");
                                saw_binary_events = true;
                            }
                        }
                    }
                    if saw_binary_events && saw_builds {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        assert!(saw_binary_events, "did not receive packed binary event batch in ws stream");
        assert!(saw_builds, "did not receive live build.json snapshot in ws stream");

        // 6. Close client -> active_viewers drops back to 0.
        drop(ws);
        let wait_zero = Instant::now() + Duration::from_secs(2);
        while Instant::now() < wait_zero && handle.active_viewers() != 0 {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(handle.active_viewers(), 0);

        handle.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
