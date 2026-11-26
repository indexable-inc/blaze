//! Live Blaze Terminal API server, persistent inode-keyed entity & custom-symbol store,
//! Unix-domain socket side-band protocol (`$BLAZE_TERM_SOCK`), in-band `OSC 7770` dispatcher,
//! and built-in `term` / `blaze-shell api` CLI commands.
//!
//! Architecture:
//! - All protocol parsing, PUA codepoint allocation (`U+E100..=U+EFFF`), validation, and canonical
//!   snapshot encoding live in the pure `#![no_std]` sans-IO crate [`blaze_term_api`].
//! - This module is the thin OS driver: owns the `0600` owner-only Unix socket server, atomic
//!   temp-file + fsync + rename persistence (`~/.blaze/shell/term-store/registry.json`), POSIX
//!   `(dev, ino)` stat lookups, and structured `blaze-sh` command handlers.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use blaze_own::Alloc;
use blaze_sh::{Record, Table, Value};
use blaze_term_api::{
    EntityId, EntityKind, EntityMeta, MAX_MESSAGE_BYTES, PUA_END, PUA_START, ServerMeta,
    SymbolRecord, SymbolSource, TermRegistry, format_hex_color, parse_hex_color,
};

/// Environment variable exported to every spawned PTY & child process pointing to the live socket.
pub const ENV_TERM_SOCK: &str = "BLAZE_TERM_SOCK";
/// Environment variable advertising Blaze Terminal Protocol support (`1`).
pub const ENV_TERM_API: &str = "BLAZE_TERM_API";

/// Owned, lock-free snapshot of a registered custom symbol for rendering in `layout.rs` / `IconAtlas`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolSnapshot {
    pub name: String,
    pub codepoint: u32,
    pub width_cells: u8,
    pub revision: u64,
    pub monochrome: bool,
    pub source: SymbolSourceSnapshot,
}

/// Owned payload of a registered custom symbol's source data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SymbolSourceSnapshot {
    Svg {
        markup: String,
        monochrome: bool,
    },
    FontGlyph {
        font_name: Option<String>,
        font_data: Option<Vec<u8>>,
        codepoint: u32,
        tint: Option<[u8; 4]>,
    },
    Png {
        data: Vec<u8>,
    },
    Text {
        text: String,
    },
}

impl SymbolSnapshot {
    fn from_record(rec: &SymbolRecord<'_>) -> Self {
        let (monochrome, source) = match &rec.source {
            SymbolSource::Svg { markup, monochrome } => (
                *monochrome,
                SymbolSourceSnapshot::Svg {
                    markup: markup.as_str().to_owned(),
                    monochrome: *monochrome,
                },
            ),
            SymbolSource::FontGlyph { font_name, font_data_b64, codepoint, tint } => {
                let font_data = font_data_b64.as_ref().and_then(|b64| decode_b64(b64.as_str()));
                (
                    tint.is_none(),
                    SymbolSourceSnapshot::FontGlyph {
                        font_name: font_name.as_ref().map(|s| s.as_str().to_owned()),
                        font_data,
                        codepoint: *codepoint,
                        tint: *tint,
                    },
                )
            }
            SymbolSource::Png { data_b64 } => (
                false,
                SymbolSourceSnapshot::Png {
                    data: decode_b64(data_b64.as_str()).unwrap_or_default(),
                },
            ),
            SymbolSource::Text { text } => {
                (false, SymbolSourceSnapshot::Text { text: text.as_str().to_owned() })
            }
        };
        Self {
            name: rec.name.as_str().to_owned(),
            codepoint: rec.codepoint,
            width_cells: rec.width_cells,
            revision: rec.revision,
            monochrome,
            source,
        }
    }
}

/// Owned, lock-free snapshot of an entity's bound metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntitySnapshot {
    pub kind: EntityKind,
    pub id: String,
    pub symbol: Option<String>,
    pub display_name: Option<String>,
    pub color: Option<[u8; 4]>,
    pub badge: Option<String>,
    pub hint_path: Option<String>,
    pub revision: u64,
}

impl EntitySnapshot {
    fn from_meta(meta: &EntityMeta<'_>) -> Self {
        Self {
            kind: meta.id.kind,
            id: meta.id.id.as_str().to_owned(),
            symbol: meta.symbol.as_ref().map(|s| s.as_str().to_owned()),
            display_name: meta.display_name.as_ref().map(|s| s.as_str().to_owned()),
            color: meta.color,
            badge: meta.badge.as_ref().map(|s| s.as_str().to_owned()),
            hint_path: meta.hint_path.as_ref().map(|s| s.as_str().to_owned()),
            revision: meta.revision,
        }
    }
}

fn decode_b64(s: &str) -> Option<Vec<u8>> {
    let most = blaze_base64::STANDARD.decoded_len_most(s.len());
    let mut out = vec![0u8; most];
    let n = blaze_base64::STANDARD.decode_into(s.as_bytes(), &mut out).ok()?;
    out.truncate(n);
    Some(out)
}

fn encode_b64(bytes: &[u8]) -> String {
    let Some(len) = blaze_base64::STANDARD.encoded_len(bytes.len()) else {
        return String::new();
    };
    let mut buf = vec![0u8; len];
    let Ok(n) = blaze_base64::STANDARD.encode_into(bytes, &mut buf) else {
        return String::new();
    };
    buf.truncate(n);
    String::from_utf8(buf).unwrap_or_default()
}

/// Stat `path` and return POSIX `(st_dev, st_ino)` — the stable filesystem identity that survives
/// `mv` / renames on the same volume.
pub fn stat_file_dev_ino(path: &Path) -> Option<(u64, u64)> {
    let md = fs::metadata(path).ok()?;
    Some((md.dev(), md.ino()))
}

struct CachedPathStat {
    dev_ino: Option<(u64, u64)>,
    checked_at: Instant,
}

/// Wrapper making a `TermRegistry<'static>` backed exclusively by `Alloc::global()` `Send` so it
/// can be safely shared across threads under a `Mutex`.
struct GlobalRegistry(TermRegistry<'static>);

// SAFETY: Every allocation inside `TermRegistry<'static>` comes solely from `Alloc::global()`
// (thread-safe global allocator), and exclusive access is serialized by the enclosing `Mutex`.
unsafe impl Send for GlobalRegistry {}

/// Live terminal API server owning the registry, persistent file store, and Unix-domain socket.
pub struct TermServer {
    registry: Mutex<GlobalRegistry>,
    revision: AtomicU64,
    save_seq: AtomicU64,
    pid: u32,
    store_path: PathBuf,
    socket_path: PathBuf,
    socket_path_str: String,
    stop: Arc<AtomicBool>,
    stat_cache: Mutex<HashMap<PathBuf, CachedPathStat>>,
}

static GLOBAL_SERVER: OnceLock<Arc<TermServer>> = OnceLock::new();

/// Start (or return) the process-wide `TermServer` and export `$BLAZE_TERM_SOCK` / `$BLAZE_TERM_API`.
pub fn start_global(home: &str) -> Arc<TermServer> {
    Arc::clone(GLOBAL_SERVER.get_or_init(|| {
        let store_path = std::env::var("BLAZE_TERM_STORE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(home)
                    .join(".blaze")
                    .join("shell")
                    .join("term-store")
                    .join("registry.json")
            });
        let pid = std::process::id();
        let uid = current_uid();
        let sock_dir = std::env::temp_dir();
        let socket_path = sock_dir.join(format!("blaze-shell-{uid}-{pid}.sock"));
        let server = TermServer::start(store_path, socket_path);
        // SAFETY: called once at process boot before spawning PTYs or worker threads.
        unsafe {
            std::env::set_var(ENV_TERM_SOCK, &server.socket_path_str);
            std::env::set_var(ENV_TERM_API, "1");
        }
        server
    }))
}

/// Access the running process-wide `TermServer`, if initialized.
pub fn global() -> Option<Arc<TermServer>> {
    GLOBAL_SERVER.get().cloned()
}

/// Fast lock-free revision number of the global server (0 when not started).
#[inline]
pub fn global_revision() -> u64 {
    GLOBAL_SERVER.get().map_or(0, |s| s.revision())
}

/// Look up a registered custom symbol by name in the global server.
pub fn lookup_global_symbol(name: &str) -> Option<SymbolSnapshot> {
    GLOBAL_SERVER.get()?.symbol_by_name(name)
}

/// Look up a registered custom symbol by PUA codepoint (`U+E100..=U+EFFF`) in the global server.
pub fn lookup_global_codepoint(cp: u32) -> Option<SymbolSnapshot> {
    if !(PUA_START..=PUA_END).contains(&cp) {
        return None;
    }
    GLOBAL_SERVER.get()?.symbol_by_codepoint(cp)
}

/// Look up an exact file/folder entity binding by POSIX `(dev, ino)` via cached `stat(path)`.
pub fn lookup_global_path_entity(path: &Path) -> Option<EntitySnapshot> {
    GLOBAL_SERVER.get()?.lookup_path_entity(path)
}

/// Look up a directory's entity binding by `(dev, ino)`, walking ancestors up to root so
/// subdirectories inherit their enclosing project folder's custom symbol & label even after `mv`.
pub fn lookup_global_dir_entity_with_ancestors(cwd: &Path) -> Option<EntitySnapshot> {
    GLOBAL_SERVER.get()?.lookup_dir_entity_with_ancestors(cwd)
}

/// Look up a host entity binding by hostname (case-insensitive exact match).
pub fn lookup_global_host_entity(host: &str) -> Option<EntitySnapshot> {
    GLOBAL_SERVER.get()?.lookup_host_entity(host)
}

fn current_uid() -> u32 {
    fs::metadata("/dev/null")
        .ok()
        .and_then(|_| std::env::var("UID").ok().and_then(|s| s.parse::<u32>().ok()))
        .unwrap_or_else(|| {
            // Stable per-user fallback via home directory owner UID (zero FFI needed).
            std::env::var("HOME")
                .ok()
                .and_then(|h| fs::metadata(h).ok().map(|m| m.uid()))
                .unwrap_or(0)
        })
}

impl TermServer {
    /// Start a live `TermServer` backed by `store_path` and serving requests on `socket_path`.
    pub fn start(store_path: PathBuf, socket_path: PathBuf) -> Arc<Self> {
        let alloc = Alloc::global();
        let registry = match fs::read(&store_path) {
            Ok(bytes) if !bytes.is_empty() => match TermRegistry::decode_snapshot(&bytes, alloc) {
                Ok(reg) => reg,
                Err(e) => {
                    eprintln!(
                        "blaze-shell: term-store {} failed to load ({e}); starting empty",
                        store_path.display()
                    );
                    TermRegistry::new_in(alloc)
                }
            },
            _ => TermRegistry::new_in(alloc),
        };
        let init_rev = registry.revision();
        let socket_path_str = socket_path.to_string_lossy().into_owned();
        let stop = Arc::new(AtomicBool::new(false));
        let server = Arc::new(Self {
            registry: Mutex::new(GlobalRegistry(registry)),
            revision: AtomicU64::new(init_rev),
            save_seq: AtomicU64::new(0),
            pid: std::process::id(),
            store_path,
            socket_path: socket_path.clone(),
            socket_path_str,
            stop: Arc::clone(&stop),
            stat_cache: Mutex::new(HashMap::new()),
        });

        let _ = fs::remove_file(&socket_path);
        if let Some(parent) = socket_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match UnixListener::bind(&socket_path) {
            Ok(listener) => {
                // Trust rule: restrict socket strictly to the owning user (`0600`).
                let _ = fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600));
                let _ = listener.set_nonblocking(true);
                let srv = Arc::clone(&server);
                let _ = thread::Builder::new()
                    .name("blaze-term-api-server".to_owned())
                    .spawn(move || accept_loop(listener, srv, stop));
            }
            Err(e) => {
                eprintln!(
                    "blaze-shell: failed to bind terminal API socket {}: {e}",
                    socket_path.display()
                );
            }
        }
        server
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn store_path(&self) -> &Path {
        &self.store_path
    }

    #[inline]
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }

    fn server_meta(&self) -> ServerMeta<'_> {
        ServerMeta { pid: self.pid, socket_path: &self.socket_path_str }
    }

    /// Execute one JSON-RPC request against the live registry, persisting atomically on mutation.
    pub fn handle_json(&self, raw: &[u8]) -> String {
        let (resp_str, mutated, snap_bytes) = {
            let Ok(mut guard) = self.registry.lock() else {
                return "{\"ok\":false,\"code\":\"internal\",\"error\":\"registry lock poisoned\"}"
                    .to_owned();
            };
            let reg = &mut guard.0;
            let (resp, mutated) = reg.apply_json(raw, self.server_meta(), Alloc::global());
            let snap = if mutated {
                self.revision.store(reg.revision(), Ordering::Release);
                Some(reg.encode_snapshot(Alloc::global()).as_str().as_bytes().to_vec())
            } else {
                None
            };
            (resp.as_str().to_owned(), mutated, snap)
        };
        if mutated && let Some(bytes) = snap_bytes {
            self.persist_atomic(&bytes);
        }
        resp_str
    }

    /// Handle an in-band `OSC 7770;<payload>` sequence emitted by a running PTY program.
    /// Returns `(mutated, optional_osc_reply_bytes)`.
    pub fn handle_osc_7770(&self, payload: &[u8]) -> (bool, Option<String>) {
        let (mutated, reply, snap_bytes) = {
            let Ok(mut guard) = self.registry.lock() else {
                return (false, None);
            };
            let reg = &mut guard.0;
            let outcome = reg.apply_osc_7770(payload, self.server_meta(), Alloc::global());
            let snap = if outcome.mutated {
                self.revision.store(reg.revision(), Ordering::Release);
                Some(reg.encode_snapshot(Alloc::global()).as_str().as_bytes().to_vec())
            } else {
                None
            };
            let reply = outcome.reply_osc.map(|s| s.as_str().to_owned());
            (outcome.mutated, reply, snap)
        };
        if mutated && let Some(bytes) = snap_bytes {
            self.persist_atomic(&bytes);
        }
        (mutated, reply)
    }

    fn persist_atomic(&self, bytes: &[u8]) {
        if let Some(parent) = self.store_path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            eprintln!("blaze-shell: failed to create term-store dir {}: {e}", parent.display());
            return;
        }
        let seq = self.save_seq.fetch_add(1, Ordering::Relaxed);
        let tmp = self.store_path.with_extension(format!("json.tmp.{}.{seq}", self.pid));
        match fs::OpenOptions::new().write(true).create(true).truncate(true).open(&tmp) {
            Ok(mut file) => {
                if let Err(e) = file.write_all(bytes).and_then(|()| file.sync_all()) {
                    eprintln!("blaze-shell: failed writing term-store {}: {e}", tmp.display());
                    let _ = fs::remove_file(&tmp);
                    return;
                }
                let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
                if let Err(e) = fs::rename(&tmp, &self.store_path) {
                    eprintln!(
                        "blaze-shell: failed renaming term-store {} -> {}: {e}",
                        tmp.display(),
                        self.store_path.display()
                    );
                    let _ = fs::remove_file(&tmp);
                }
            }
            Err(e) => {
                eprintln!("blaze-shell: failed opening term-store temp {}: {e}", tmp.display());
            }
        }
    }

    pub fn symbol_by_name(&self, name: &str) -> Option<SymbolSnapshot> {
        let guard = self.registry.lock().ok()?;
        guard.0.symbol_by_name(name).map(SymbolSnapshot::from_record)
    }

    pub fn symbol_by_codepoint(&self, cp: u32) -> Option<SymbolSnapshot> {
        let guard = self.registry.lock().ok()?;
        guard.0.symbol_by_codepoint(cp).map(SymbolSnapshot::from_record)
    }

    pub fn list_symbols(&self) -> Vec<SymbolSnapshot> {
        let Ok(guard) = self.registry.lock() else { return Vec::new() };
        guard.0.symbols().iter().map(SymbolSnapshot::from_record).collect()
    }

    pub fn list_entities(&self, kind_filter: Option<EntityKind>) -> Vec<EntitySnapshot> {
        let Ok(guard) = self.registry.lock() else { return Vec::new() };
        guard
            .0
            .entities()
            .iter()
            .filter(|e| kind_filter.is_none_or(|k| e.id.kind == k))
            .map(EntitySnapshot::from_meta)
            .collect()
    }

    pub fn lookup_entity(&self, kind: EntityKind, id: &str) -> Option<EntitySnapshot> {
        let guard = self.registry.lock().ok()?;
        guard.0.entity(kind, id).map(EntitySnapshot::from_meta)
    }

    pub fn lookup_file_entity(&self, dev: u64, ino: u64) -> Option<EntitySnapshot> {
        let guard = self.registry.lock().ok()?;
        guard.0.entity_file(dev, ino).map(EntitySnapshot::from_meta)
    }

    pub fn lookup_host_entity(&self, host: &str) -> Option<EntitySnapshot> {
        let guard = self.registry.lock().ok()?;
        let reg = &guard.0;
        if let Some(hit) = reg.entity(EntityKind::Host, host) {
            return Some(EntitySnapshot::from_meta(hit));
        }
        reg.entities()
            .iter()
            .find(|e| e.id.kind == EntityKind::Host && e.id.id.as_str().eq_ignore_ascii_case(host))
            .map(EntitySnapshot::from_meta)
    }

    fn cached_stat(&self, path: &Path) -> Option<(u64, u64)> {
        let now = Instant::now();
        if let Ok(cache) = self.stat_cache.lock()
            && let Some(entry) = cache.get(path)
            && now.duration_since(entry.checked_at) < Duration::from_millis(1200)
        {
            return entry.dev_ino;
        }
        let dev_ino = stat_file_dev_ino(path);
        if let Ok(mut cache) = self.stat_cache.lock() {
            if cache.len() >= 1024 {
                cache.clear();
            }
            cache.insert(path.to_path_buf(), CachedPathStat { dev_ino, checked_at: now });
        }
        dev_ino
    }

    /// Look up an exact file/folder entity binding by POSIX `(dev, ino)`.
    pub fn lookup_path_entity(&self, path: &Path) -> Option<EntitySnapshot> {
        // Fast path: if there are zero entities registered, skip stat completely.
        {
            let guard = self.registry.lock().ok()?;
            if guard.0.entities().is_empty() {
                return None;
            }
        }
        let (dev, ino) = self.cached_stat(path)?;
        self.lookup_file_entity(dev, ino)
    }

    /// Look up a directory entity binding by `(dev, ino)`, walking up ancestors (closest wins).
    pub fn lookup_dir_entity_with_ancestors(&self, cwd: &Path) -> Option<EntitySnapshot> {
        {
            let guard = self.registry.lock().ok()?;
            if guard.0.entities().is_empty() {
                return None;
            }
        }
        let mut cur: Option<&Path> = Some(cwd);
        let mut depth = 0usize;
        while let Some(dir) = cur {
            if depth >= 32 {
                break;
            }
            depth += 1;
            if let Some((dev, ino)) = self.cached_stat(dir)
                && let Some(hit) = self.lookup_file_entity(dev, ino)
            {
                return Some(hit);
            }
            cur = dir.parent();
        }
        None
    }

    /// Register a symbol directly from Rust (used by built-in `term` command & tests).
    pub fn register_symbol(
        &self,
        name: &str,
        requested_cp: Option<u32>,
        width_cells: u8,
        source: SymbolSource<'static>,
    ) -> Result<SymbolSnapshot, String> {
        let (snap, snap_bytes) = {
            let mut guard =
                self.registry.lock().map_err(|_| "registry lock poisoned".to_owned())?;
            let reg = &mut guard.0;
            let rec = reg
                .register_symbol(name, requested_cp, width_cells, source)
                .map_err(|e| e.to_string())?;
            let snap = SymbolSnapshot::from_record(rec);
            self.revision.store(reg.revision(), Ordering::Release);
            let bytes = reg.encode_snapshot(Alloc::global()).as_str().as_bytes().to_vec();
            (snap, bytes)
        };
        self.persist_atomic(&snap_bytes);
        Ok(snap)
    }

    pub fn unregister_symbol(&self, name: &str) -> Result<bool, String> {
        let (removed, snap_bytes) = {
            let mut guard =
                self.registry.lock().map_err(|_| "registry lock poisoned".to_owned())?;
            let reg = &mut guard.0;
            let removed = reg.unregister_symbol(name);
            let bytes = if removed {
                self.revision.store(reg.revision(), Ordering::Release);
                Some(reg.encode_snapshot(Alloc::global()).as_str().as_bytes().to_vec())
            } else {
                None
            };
            (removed, bytes)
        };
        if let Some(bytes) = snap_bytes {
            self.persist_atomic(&bytes);
        }
        Ok(removed)
    }

    pub fn bind_entity(&self, meta: EntityMeta<'static>) -> Result<EntitySnapshot, String> {
        let (snap, snap_bytes) = {
            let mut guard =
                self.registry.lock().map_err(|_| "registry lock poisoned".to_owned())?;
            let reg = &mut guard.0;
            let bound = reg.bind_entity(meta).map_err(|e| e.to_string())?;
            let snap = EntitySnapshot::from_meta(bound);
            self.revision.store(reg.revision(), Ordering::Release);
            let bytes = reg.encode_snapshot(Alloc::global()).as_str().as_bytes().to_vec();
            (snap, bytes)
        };
        self.persist_atomic(&snap_bytes);
        Ok(snap)
    }

    pub fn unbind_entity(&self, kind: EntityKind, id: &str) -> Result<bool, String> {
        let (removed, snap_bytes) = {
            let mut guard =
                self.registry.lock().map_err(|_| "registry lock poisoned".to_owned())?;
            let reg = &mut guard.0;
            let removed = reg.unbind_entity(kind, id);
            let bytes = if removed {
                self.revision.store(reg.revision(), Ordering::Release);
                Some(reg.encode_snapshot(Alloc::global()).as_str().as_bytes().to_vec())
            } else {
                None
            };
            (removed, bytes)
        };
        if let Some(bytes) = snap_bytes {
            self.persist_atomic(&bytes);
        }
        Ok(removed)
    }

    /// Stop the Unix socket accept loop and remove the socket file.
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = fs::remove_file(&self.socket_path);
    }
}

impl Drop for TermServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn accept_loop(listener: UnixListener, server: Arc<TermServer>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                let srv = Arc::clone(&server);
                let _ = thread::Builder::new()
                    .name("blaze-term-api-conn".to_owned())
                    .spawn(move || handle_client(stream, &srv));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(12));
            }
            Err(_) => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

fn handle_client(stream: UnixStream, server: &TermServer) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let Ok(mut writer) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    // Cap requests per connection so a runaway client cannot hold a thread forever.
    for _ in 0..256 {
        line.clear();
        let mut total = 0usize;
        let mut too_large = false;
        loop {
            let Ok(avail) = reader.fill_buf() else { return };
            if avail.is_empty() {
                if total == 0 {
                    return;
                }
                break;
            }
            if let Some(nl) = avail.iter().position(|&b| b == b'\n') {
                let take = nl;
                if total + take <= MAX_MESSAGE_BYTES {
                    line.extend_from_slice(&avail[..take]);
                } else {
                    too_large = true;
                }
                reader.consume(nl + 1);
                break;
            }
            let n = avail.len();
            if total + n <= MAX_MESSAGE_BYTES {
                line.extend_from_slice(avail);
            } else {
                too_large = true;
            }
            total += n;
            reader.consume(n);
            if too_large && total > MAX_MESSAGE_BYTES + 65536 {
                let _ = writer.write_all(
                    b"{\"ok\":false,\"code\":\"too_large\",\"error\":\"request exceeds 4 MiB limit\"}\n",
                );
                let _ = writer.flush();
                return;
            }
        }
        while line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }
        let reply = if too_large {
            "{\"ok\":false,\"code\":\"too_large\",\"error\":\"request exceeds 4 MiB limit\"}"
                .to_owned()
        } else {
            server.handle_json(&line)
        };
        if writer.write_all(reply.as_bytes()).is_err()
            || writer.write_all(b"\n").is_err()
            || writer.flush().is_err()
        {
            return;
        }
    }
}

/// Client helper: send one JSON-RPC request over a `blaze-shell` Unix-domain socket and return
/// the JSON response line.
pub fn send_socket_request(sock_path: &Path, request_json: &str) -> Result<String, String> {
    let mut stream = UnixStream::connect(sock_path)
        .map_err(|e| format!("failed to connect to {}: {e}", sock_path.display()))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
    stream
        .write_all(request_json.as_bytes())
        .and_then(|()| stream.write_all(b"\n"))
        .and_then(|()| stream.flush())
        .map_err(|e| format!("failed writing request to {}: {e}", sock_path.display()))?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    let n = reader
        .read_line(&mut resp)
        .map_err(|e| format!("failed reading response from {}: {e}", sock_path.display()))?;
    if n == 0 {
        return Err(format!("empty response from {}", sock_path.display()));
    }
    Ok(resp.trim_end_matches(['\r', '\n']).to_owned())
}

// ---------------------------------------------------------------------------
// Built-in `term` shell command & external `blaze-shell api` CLI
// ---------------------------------------------------------------------------

fn parse_codepoint_arg(s: &str) -> Result<u32, String> {
    let t = s.trim();
    if let Some(hex) = t.strip_prefix("U+").or_else(|| t.strip_prefix("u+")).or_else(|| t.strip_prefix("0x")).or_else(|| t.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).map_err(|_| format!("invalid hex codepoint `{s}`"))
    } else if let Ok(v) = t.parse::<u32>() {
        Ok(v)
    } else if t.chars().count() == 1 {
        Ok(t.chars().next().unwrap() as u32)
    } else {
        Err(format!("invalid codepoint `{s}` (use 0xE100, U+E100, decimal, or single char)"))
    }
}

fn resolve_cli_path(cwd: &str, raw: &str) -> PathBuf {
    let expanded = if let Some(rest) = raw.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        PathBuf::from(home).join(rest)
    } else if raw == "~"
        && let Ok(home) = std::env::var("HOME")
    {
        PathBuf::from(home)
    } else {
        let p = Path::new(raw);
        if p.is_absolute() { p.to_path_buf() } else { Path::new(cwd).join(p) }
    };
    fs::canonicalize(&expanded).unwrap_or(expanded)
}

fn str_val(s: &str) -> Value<'static> {
    Value::string(s, Alloc::global())
}

fn make_record(fields: &[(&str, Value<'static>)]) -> Value<'static> {
    let alloc = Alloc::global();
    let mut rec = Record::new(alloc);
    for (k, v) in fields {
        rec.insert_str(k, v.clone());
    }
    Value::Record(rec)
}

/// Tokenize a shell command line respecting `'...'` and `"..."` quotes (with `\"` / `\\` escapes).
pub fn split_shell_tokens(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut started = false;
    for ch in input.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            started = true;
            continue;
        }
        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                cur.push(ch);
            }
            continue;
        }
        if in_double {
            if ch == '"' {
                in_double = false;
            } else if ch == '\\' {
                escaped = true;
            } else {
                cur.push(ch);
            }
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '\'' => {
                in_single = true;
                started = true;
            }
            '"' => {
                in_double = true;
                started = true;
            }
            c if c.is_ascii_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// Execute the built-in `term ...` command directly against the live `TermServer`, returning a
/// structured `blaze-sh` [`Value`] (`Table` / `Record` / `String`).
pub fn exec_term_command(
    server: &TermServer,
    cwd: &str,
    args: &[String],
) -> Result<Value<'static>, String> {
    let sub = args.first().map(String::as_str).unwrap_or("info");
    match sub {
        "info" | "status" => {
            let syms = server.list_symbols();
            let ents = server.list_entities(None);
            Ok(make_record(&[
                ("protocol", Value::Int(i64::from(blaze_term_api::PROTOCOL_VERSION))),
                ("pid", Value::Int(i64::from(server.pid()))),
                ("socket", str_val(&server.socket_path_str)),
                ("store", str_val(&server.store_path.to_string_lossy())),
                ("revision", Value::Int(server.revision() as i64)),
                ("symbols", Value::Int(syms.len() as i64)),
                ("entities", Value::Int(ents.len() as i64)),
                ("pua_range", str_val("U+E100..=U+EFFF")),
                ("osc", str_val("OSC 7770")),
            ]))
        }
        "help" | "-h" | "--help" => Ok(str_val(
            "Blaze Terminal API (`$BLAZE_TERM_SOCK` + `OSC 7770`)\n\
             \n\
             term info\n\
             term symbol add <name> (--svg '<svg...>' | --svg-file <path> | --font <ttf/otf> --glyph <cp> | --font-name <name> --glyph <cp> | --png <path> | --text <str>) [--codepoint 0xE100] [--width 1|2] [--mono] [--color #rrggbb]\n\
             term symbol list\n\
             term symbol rm <name>\n\
             term bind <path> [--symbol <name>] [--name <label>] [--badge <tag>] [--color #rrggbb]\n\
             term bind --kind <file|host|repo|branch|process|custom> --id <id> [--symbol <name>] [--name <label>] [--badge <tag>] [--color #rrggbb]\n\
             term unbind <path> | term unbind --kind <kind> --id <id>\n\
             term list [file|host|repo|branch|process|custom]\n\
             term emit <symbol_name>",
        )),
        "symbol" | "sym" => exec_symbol_subcommand(server, cwd, args.get(1..).unwrap_or(&[])),
        "bind" | "tag" => exec_bind_subcommand(server, cwd, args.get(1..).unwrap_or(&[])),
        "unbind" | "untag" => exec_unbind_subcommand(server, cwd, args.get(1..).unwrap_or(&[])),
        "list" | "entities" => {
            let filter = match args.get(1).map(String::as_str) {
                Some(k) => Some(
                    EntityKind::from_str(k)
                        .ok_or_else(|| format!("unknown entity kind `{k}` (expected file|host|repo|branch|process|custom)"))?,
                ),
                None => None,
            };
            let ents = server.list_entities(filter);
            let alloc = Alloc::global();
            let mut tbl = Table::from_col_names(
                &["kind", "id", "symbol", "display_name", "badge", "color", "hint_path"],
                alloc,
            );
            for e in ents {
                let col_s = e
                    .color
                    .map(|c| format_hex_color(c, alloc).as_str().to_owned())
                    .unwrap_or_default();
                tbl.push_row_values(&[
                    str_val(e.kind.as_str()),
                    str_val(&e.id),
                    str_val(e.symbol.as_deref().unwrap_or("")),
                    str_val(e.display_name.as_deref().unwrap_or("")),
                    str_val(e.badge.as_deref().unwrap_or("")),
                    str_val(&col_s),
                    str_val(e.hint_path.as_deref().unwrap_or("")),
                ]);
            }
            Ok(Value::Table(tbl))
        }
        "emit" => {
            let name = args.get(1).ok_or_else(|| "usage: term emit <symbol_name>".to_owned())?;
            let sym = server
                .symbol_by_name(name)
                .ok_or_else(|| format!("unknown symbol `{name}`"))?;
            let ch = char::from_u32(sym.codepoint).unwrap_or('?');
            Ok(str_val(&format!("{ch}  ({} · U+{:04X})", sym.name, sym.codepoint)))
        }
        "raw" => {
            let json = args
                .get(1..)
                .filter(|s| !s.is_empty())
                .map(|s| s.join(" "))
                .ok_or_else(|| "usage: term raw '{\"op\":\"info\"}'".to_owned())?;
            let resp = server.handle_json(json.as_bytes());
            Ok(str_val(&resp))
        }
        other => Err(format!("unknown `term` subcommand `{other}` (run `term help`)")),
    }
}

fn exec_symbol_subcommand(
    server: &TermServer,
    cwd: &str,
    args: &[String],
) -> Result<Value<'static>, String> {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    match sub {
        "list" | "ls" => {
            let syms = server.list_symbols();
            let alloc = Alloc::global();
            let mut tbl = Table::from_col_names(
                &["preview", "name", "codepoint", "kind", "width", "rev"],
                alloc,
            );
            for s in syms {
                let ch = char::from_u32(s.codepoint).unwrap_or('?');
                let mut preview = String::new();
                preview.push(ch);
                let kind = match &s.source {
                    SymbolSourceSnapshot::Svg { monochrome: true, .. } => "svg (mono)",
                    SymbolSourceSnapshot::Svg { monochrome: false, .. } => "svg",
                    SymbolSourceSnapshot::FontGlyph { font_data: Some(_), .. } => "font_bytes",
                    SymbolSourceSnapshot::FontGlyph { .. } => "font_name",
                    SymbolSourceSnapshot::Png { .. } => "png",
                    SymbolSourceSnapshot::Text { .. } => "text",
                };
                tbl.push_row_values(&[
                    str_val(&preview),
                    str_val(&s.name),
                    str_val(&format!("U+{:04X}", s.codepoint)),
                    str_val(kind),
                    Value::Int(i64::from(s.width_cells)),
                    Value::Int(s.revision as i64),
                ]);
            }
            Ok(Value::Table(tbl))
        }
        "rm" | "remove" | "delete" => {
            let name = args.get(1).ok_or_else(|| "usage: term symbol rm <name>".to_owned())?;
            let removed = server.unregister_symbol(name)?;
            Ok(make_record(&[("name", str_val(name)), ("removed", Value::Bool(removed))]))
        }
        "add" | "register" | "set" => {
            let name = args
                .get(1)
                .ok_or_else(|| "usage: term symbol add <name> (--svg ... | --svg-file ... | --font ... --glyph ... | --font-name ... --glyph ... | --png ... | --text ...)".to_owned())?;
            let mut svg_markup: Option<String> = None;
            let mut font_file: Option<String> = None;
            let mut font_name: Option<String> = None;
            let mut glyph_cp: Option<u32> = None;
            let mut png_file: Option<String> = None;
            let mut text: Option<String> = None;
            let mut requested_cp: Option<u32> = None;
            let mut width_cells: u8 = 1;
            let mut monochrome = false;
            let mut tint: Option<[u8; 4]> = None;

            let mut i = 2usize;
            while i < args.len() {
                match args[i].as_str() {
                    "--svg" => {
                        i += 1;
                        svg_markup = Some(args.get(i).ok_or("--svg requires SVG markup")?.clone());
                    }
                    "--svg-file" => {
                        i += 1;
                        let raw = args.get(i).ok_or("--svg-file requires a path")?;
                        let p = resolve_cli_path(cwd, raw);
                        svg_markup = Some(
                            fs::read_to_string(&p)
                                .map_err(|e| format!("failed reading {}: {e}", p.display()))?,
                        );
                    }
                    "--font" | "--font-file" => {
                        i += 1;
                        font_file = Some(args.get(i).ok_or("--font requires a font path")?.clone());
                    }
                    "--font-name" => {
                        i += 1;
                        font_name = Some(args.get(i).ok_or("--font-name requires a name")?.clone());
                    }
                    "--glyph" | "--glyph-codepoint" => {
                        i += 1;
                        let raw = args.get(i).ok_or("--glyph requires a codepoint or char")?;
                        glyph_cp = Some(parse_codepoint_arg(raw)?);
                    }
                    "--png" | "--png-file" => {
                        i += 1;
                        png_file = Some(args.get(i).ok_or("--png requires a path")?.clone());
                    }
                    "--text" | "--emoji" => {
                        i += 1;
                        text = Some(args.get(i).ok_or("--text requires a string")?.clone());
                    }
                    "--codepoint" | "--cp" => {
                        i += 1;
                        let raw = args.get(i).ok_or("--codepoint requires U+E100..U+EFFF")?;
                        requested_cp = Some(parse_codepoint_arg(raw)?);
                    }
                    "--width" => {
                        i += 1;
                        let raw = args.get(i).ok_or("--width requires 1 or 2")?;
                        width_cells = raw.parse::<u8>().map_err(|_| format!("invalid --width `{raw}`"))?;
                    }
                    "--mono" | "--monochrome" => {
                        monochrome = true;
                    }
                    "--color" | "--tint" => {
                        i += 1;
                        let raw = args.get(i).ok_or("--color requires #rrggbb")?;
                        tint = Some(
                            parse_hex_color(raw)
                                .ok_or_else(|| format!("invalid hex color `{raw}`"))?,
                        );
                    }
                    other => return Err(format!("unknown flag `{other}` for `term symbol add`")),
                }
                i += 1;
            }

            let alloc = Alloc::global();
            let source = if let Some(markup) = svg_markup {
                SymbolSource::Svg {
                    markup: blaze_own::String::from_str_in(&markup, alloc),
                    monochrome,
                }
            } else if font_file.is_some() || font_name.is_some() {
                let cp = glyph_cp
                    .ok_or("`--font` / `--font-name` requires `--glyph <codepoint|char>`")?;
                let font_data_b64 = if let Some(ff) = font_file {
                    let p = resolve_cli_path(cwd, &ff);
                    let bytes =
                        fs::read(&p).map_err(|e| format!("failed reading {}: {e}", p.display()))?;
                    Some(blaze_own::String::from_str_in(&encode_b64(&bytes), alloc))
                } else {
                    None
                };
                SymbolSource::FontGlyph {
                    font_name: font_name.map(|n| blaze_own::String::from_str_in(&n, alloc)),
                    font_data_b64,
                    codepoint: cp,
                    tint,
                }
            } else if let Some(pf) = png_file {
                let p = resolve_cli_path(cwd, &pf);
                let bytes =
                    fs::read(&p).map_err(|e| format!("failed reading {}: {e}", p.display()))?;
                SymbolSource::Png {
                    data_b64: blaze_own::String::from_str_in(&encode_b64(&bytes), alloc),
                }
            } else if let Some(t) = text {
                SymbolSource::Text { text: blaze_own::String::from_str_in(&t, alloc) }
            } else {
                return Err(
                    "specify one of --svg, --svg-file, --font, --font-name, --png, or --text"
                        .to_owned(),
                );
            };

            let snap = server.register_symbol(name, requested_cp, width_cells, source)?;
            let ch = char::from_u32(snap.codepoint).unwrap_or('?');
            let mut utf8 = String::new();
            utf8.push(ch);
            Ok(make_record(&[
                ("name", str_val(&snap.name)),
                ("codepoint", str_val(&format!("U+{:04X}", snap.codepoint))),
                ("preview", str_val(&utf8)),
                ("width", Value::Int(i64::from(snap.width_cells))),
                ("revision", Value::Int(snap.revision as i64)),
            ]))
        }
        other => Err(format!("unknown `term symbol` subcommand `{other}` (use add|list|rm)")),
    }
}

fn exec_bind_subcommand(
    server: &TermServer,
    cwd: &str,
    args: &[String],
) -> Result<Value<'static>, String> {
    if args.is_empty() {
        return Err(
            "usage: term bind <path> [--symbol <name>] [--name <label>] [--badge <tag>] [--color #rrggbb]"
                .to_owned(),
        );
    }
    let mut kind: Option<EntityKind> = None;
    let mut explicit_id: Option<String> = None;
    let mut path_arg: Option<String> = None;
    let mut symbol: Option<String> = None;
    let mut display_name: Option<String> = None;
    let mut badge: Option<String> = None;
    let mut color: Option<[u8; 4]> = None;
    let mut hint_path: Option<String> = None;

    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--kind" => {
                i += 1;
                let raw = args.get(i).ok_or("--kind requires a kind")?;
                kind = Some(
                    EntityKind::from_str(raw)
                        .ok_or_else(|| format!("unknown entity kind `{raw}`"))?,
                );
            }
            "--id" => {
                i += 1;
                explicit_id = Some(args.get(i).ok_or("--id requires a value")?.clone());
            }
            "--symbol" | "--sym" | "-s" => {
                i += 1;
                symbol = Some(args.get(i).ok_or("--symbol requires a symbol name or emoji")?.clone());
            }
            "--name" | "--label" | "-n" => {
                i += 1;
                display_name = Some(args.get(i).ok_or("--name requires a display label")?.clone());
            }
            "--badge" | "-b" => {
                i += 1;
                badge = Some(args.get(i).ok_or("--badge requires text")?.clone());
            }
            "--color" | "-c" => {
                i += 1;
                let raw = args.get(i).ok_or("--color requires #rrggbb")?;
                color = Some(
                    parse_hex_color(raw).ok_or_else(|| format!("invalid hex color `{raw}`"))?,
                );
            }
            "--hint" => {
                i += 1;
                hint_path = Some(args.get(i).ok_or("--hint requires a path")?.clone());
            }
            arg if !arg.starts_with("--") && path_arg.is_none() => {
                path_arg = Some(arg.to_owned());
            }
            other => return Err(format!("unknown argument `{other}` for `term bind`")),
        }
        i += 1;
    }

    let alloc = Alloc::global();
    let entity_id = if let (Some(k), Some(id)) = (kind, explicit_id) {
        EntityId::new(k, &id, alloc)
    } else if let Some(raw_p) = path_arg {
        let p = resolve_cli_path(cwd, &raw_p);
        let (dev, ino) = stat_file_dev_ino(&p)
            .ok_or_else(|| format!("cannot stat `{}` to read (dev, ino)", p.display()))?;
        if hint_path.is_none() {
            hint_path = Some(p.to_string_lossy().into_owned());
        }
        EntityId::file(dev, ino, alloc)
    } else {
        return Err("provide a filesystem <path> or both --kind and --id".to_owned());
    };

    let mut meta = EntityMeta::new(entity_id, alloc);
    meta.symbol = symbol.map(|s| blaze_own::String::from_str_in(&s, alloc));
    meta.display_name = display_name.map(|s| blaze_own::String::from_str_in(&s, alloc));
    meta.badge = badge.map(|s| blaze_own::String::from_str_in(&s, alloc));
    meta.color = color;
    meta.hint_path = hint_path.map(|s| blaze_own::String::from_str_in(&s, alloc));

    let bound = server.bind_entity(meta)?;
    let col_s = bound
        .color
        .map(|c| format_hex_color(c, alloc).as_str().to_owned())
        .unwrap_or_default();
    Ok(make_record(&[
        ("kind", str_val(bound.kind.as_str())),
        ("id", str_val(&bound.id)),
        ("symbol", str_val(bound.symbol.as_deref().unwrap_or(""))),
        ("display_name", str_val(bound.display_name.as_deref().unwrap_or(""))),
        ("badge", str_val(bound.badge.as_deref().unwrap_or(""))),
        ("color", str_val(&col_s)),
        ("hint_path", str_val(bound.hint_path.as_deref().unwrap_or(""))),
        ("revision", Value::Int(bound.revision as i64)),
    ]))
}

fn exec_unbind_subcommand(
    server: &TermServer,
    cwd: &str,
    args: &[String],
) -> Result<Value<'static>, String> {
    if args.is_empty() {
        return Err("usage: term unbind <path> | term unbind --kind <kind> --id <id>".to_owned());
    }
    let mut kind: Option<EntityKind> = None;
    let mut explicit_id: Option<String> = None;
    let mut path_arg: Option<String> = None;
    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--kind" => {
                i += 1;
                let raw = args.get(i).ok_or("--kind requires a kind")?;
                kind = Some(
                    EntityKind::from_str(raw)
                        .ok_or_else(|| format!("unknown entity kind `{raw}`"))?,
                );
            }
            "--id" => {
                i += 1;
                explicit_id = Some(args.get(i).ok_or("--id requires a value")?.clone());
            }
            arg if !arg.starts_with("--") && path_arg.is_none() => {
                path_arg = Some(arg.to_owned());
            }
            other => return Err(format!("unknown argument `{other}` for `term unbind`")),
        }
        i += 1;
    }

    let (k, id) = if let (Some(k), Some(id)) = (kind, explicit_id) {
        (k, id)
    } else if let Some(raw_p) = path_arg {
        let p = resolve_cli_path(cwd, &raw_p);
        let (dev, ino) = stat_file_dev_ino(&p)
            .ok_or_else(|| format!("cannot stat `{}` to read (dev, ino)", p.display()))?;
        (EntityKind::File, format!("{dev}:{ino}"))
    } else {
        return Err("provide a filesystem <path> or both --kind and --id".to_owned());
    };
    let removed = server.unbind_entity(k, &id)?;
    Ok(make_record(&[
        ("kind", str_val(k.as_str())),
        ("id", str_val(&id)),
        ("removed", Value::Bool(removed)),
    ]))
}

/// External CLI client entry point: `blaze-shell api [--sock <path>] <json | ping | info | ...>`.
/// Connects to the running `blaze-shell` Unix-domain socket server (`$BLAZE_TERM_SOCK`) and prints
/// the JSON response to stdout.
pub fn run_cli_api(args: &[String]) -> i32 {
    let mut sock_path: Option<PathBuf> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        if args[i] == "--sock" {
            if let Some(p) = args.get(i + 1) {
                sock_path = Some(PathBuf::from(p));
                i += 2;
                continue;
            }
            eprintln!("blaze-shell api: --sock requires a socket path");
            return 2;
        }
        rest.push(args[i].clone());
        i += 1;
    }
    let sock = sock_path.or_else(|| {
        std::env::var(ENV_TERM_SOCK).ok().filter(|s| !s.is_empty()).map(PathBuf::from)
    });
    let Some(sock) = sock else {
        eprintln!(
            "blaze-shell api: {ENV_TERM_SOCK} is not set (pass --sock <path> or run inside blaze-shell)"
        );
        return 1;
    };

    let payload = match rest.first().map(String::as_str) {
        None | Some("info") => "{\"op\":\"info\"}".to_owned(),
        Some("ping") => "{\"op\":\"ping\"}".to_owned(),
        Some("symbols") => "{\"op\":\"symbol.list\"}".to_owned(),
        Some("entities") => "{\"op\":\"entity.list\"}".to_owned(),
        Some(s) if s.trim_start().starts_with('{') => rest.join(" "),
        Some("bind") if rest.len() >= 2 => {
            // Ergonomic external client shorthand: `blaze-shell api bind <path> <symbol> [display_name]`
            let cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "/".to_owned());
            let p = resolve_cli_path(&cwd, &rest[1]);
            let Some((dev, ino)) = stat_file_dev_ino(&p) else {
                eprintln!("blaze-shell api: cannot stat {}", p.display());
                return 1;
            };
            let sym = rest.get(2).map(String::as_str).unwrap_or("");
            let name = rest.get(3).map(String::as_str);
            let mut obj = format!(
                "{{\"op\":\"entity.bind\",\"kind\":\"file\",\"dev\":{dev},\"ino\":{ino},\"hint_path\":{}",
                json_quote(&p.to_string_lossy())
            );
            if !sym.is_empty() {
                obj.push_str(&format!(",\"symbol\":{}", json_quote(sym)));
            }
            if let Some(n) = name {
                obj.push_str(&format!(",\"display_name\":{}", json_quote(n)));
            }
            obj.push('}');
            obj
        }
        Some(other) => {
            eprintln!(
                "blaze-shell api: unknown command `{other}` (pass JSON `{{\"op\":...}}`, `info`, `ping`, `symbols`, `entities`, or `bind <path> <symbol> [name]`)"
            );
            return 2;
        }
    };

    match send_socket_request(&sock, &payload) {
        Ok(resp) => {
            println!("{resp}");
            0
        }
        Err(e) => {
            eprintln!("blaze-shell api: {e}");
            1
        }
    }
}

fn json_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("blaze-term-srv-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn unix_socket_server_persists_symbols_and_inode_survives_rename() {
        let dir = temp_root("e2e");
        let store_path = dir.join("store").join("registry.json");
        let sock_path = dir.join("term.sock");
        let folder_a = dir.join("project-alpha");
        let folder_b = dir.join("project-beta-renamed");
        fs::create_dir_all(&folder_a).unwrap();

        let srv = TermServer::start(store_path.clone(), sock_path.clone());

        // 1. Socket permissions must be strictly owner-only (0600).
        let mode = fs::metadata(&sock_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket must be owner-only 0600");

        // 2. Register a custom SVG symbol over the live Unix domain socket.
        let reg_req = r#"{"op":"symbol.register","name":"rocket","kind":"svg","svg":"<svg viewBox='0 0 16 16'><circle cx='8' cy='8' r='6' fill='#ff6b35'/></svg>"}"#;
        let reg_resp = send_socket_request(&sock_path, reg_req).unwrap();
        assert!(reg_resp.contains("\"ok\":true"), "register response: {reg_resp}");
        assert!(reg_resp.contains("\"name\":\"rocket\""), "register response: {reg_resp}");

        // 3. Bind the folder by its POSIX (dev, ino) via built-in `term bind`.
        let bind_args: Vec<String> = vec![
            "bind".into(),
            folder_a.to_string_lossy().into_owned(),
            "--symbol".into(),
            "rocket".into(),
            "--name".into(),
            "Alpha Launchpad".into(),
            "--color".into(),
            "#ff6b35".into(),
        ];
        let _ = exec_term_command(&srv, &dir.to_string_lossy(), &bind_args).unwrap();

        // 4. Rename the folder on disk! Its (dev, ino) stays identical so lookup MUST follow it!
        fs::rename(&folder_a, &folder_b).unwrap();
        let nested = folder_b.join("sub").join("deep");
        fs::create_dir_all(&nested).unwrap();

        let direct = srv.lookup_path_entity(&folder_b).expect("entity must survive mv/rename");
        assert_eq!(direct.symbol.as_deref(), Some("rocket"));
        assert_eq!(direct.display_name.as_deref(), Some("Alpha Launchpad"));

        let inherited = srv
            .lookup_dir_entity_with_ancestors(&nested)
            .expect("nested subdir must inherit renamed ancestor's inode binding");
        assert_eq!(inherited.symbol.as_deref(), Some("rocket"));

        // 5. Register a second symbol via in-band OSC 7770 with reply=true.
        let (mutated, reply) = srv.handle_osc_7770(
            r#"{"op":"symbol.register","name":"spark","kind":"text","text":"⚡","reply":true}"#
                .as_bytes(),
        );
        assert!(mutated);
        let reply = reply.expect("reply=true must emit OSC 7770 reply");
        assert!(reply.starts_with("\x1b]7770;"));
        assert!(reply.ends_with("\x1b\\"));

        // 6. Restart a fresh TermServer from the persisted store file and verify everything survived!
        srv.shutdown();
        drop(srv);
        let sock_path2 = dir.join("term2.sock");
        let srv2 = TermServer::start(store_path, sock_path2);
        assert!(srv2.symbol_by_name("rocket").is_some());
        assert!(srv2.symbol_by_name("spark").is_some());
        let after_restart = srv2
            .lookup_path_entity(&folder_b)
            .expect("inode binding must survive restart");
        assert_eq!(after_restart.symbol.as_deref(), Some("rocket"));
        assert_eq!(after_restart.display_name.as_deref(), Some("Alpha Launchpad"));

        srv2.shutdown();
        let _ = fs::remove_dir_all(&dir);
    }
}
