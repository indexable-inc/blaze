//! `/debug/pprof` over HTTP (feature `http`), a driver: Go's `net/http/pprof` layout plus the
//! live stream (docs/profiling.md, "HTTP endpoints" and "The live stream"). One thread per
//! connection. A profile holds the program's function names, arguments in span fields and
//! timing, so another user or host must not read it: the listener is loopback only, and every
//! request must carry the token, a random 128-bit one made at start and given only to this
//! user (with `IX_PROF`, in `<IX_PROF>/http.endpoint`, mode 0600, in a directory that must be
//! this user's own; without it, on stderr), as `Authorization: Bearer <token>` or, from a
//! browser, `token=<token>` in the query. Loopback alone is not enough: every
//! local account can connect to a loopback port. The endpoint stays HTTP rather than a Unix
//! socket because the explorer at ix.dev, a browser page, reads it.
//!
//! What runs before the token check is bounded, since anyone local can send it: a request
//! must arrive whole, at most 16 KiB, within 5 s of its connection being accepted (one
//! deadline for the whole request, not a timeout per read); at most 32 connections wait for
//! their token check at once, and one more evicts the oldest of them rather than being turned
//! away, so connections held open cannot lock the owner out; a connection that passed the
//! check is never evicted; and an `accept` that fails (out of descriptors) backs off instead
//! of spinning.
//!
//! - `GET /debug/pprof/` the index
//! - `GET /debug/pprof/profile?seconds=30` CPU, gzipped profile.proto, of the next 30 seconds
//! - `GET /debug/pprof/heap`, `/allocs` live and total allocations by stack
//! - `GET /debug/pprof/spans?seconds=`, `/waits?seconds=` span self time and waits, of the
//!   last `seconds` (default: everything retained)
//! - `GET /debug/pprof/threads` each thread's open spans now, as text
//! - `GET /debug/pprof/ixprof?since_ns=&until_ns=` or `?last=<seconds>`, `&format=ixprof|pprof|chrome`
//!   (with `kind=cpu|heap|spans|waits` for pprof): a window snapshot
//! - `GET /debug/pprof/live?cursor=` the live stream, chunked
//!   `HELLO BATCH* (TICK BATCH*)* END`, resumable from a TICK's cursor

use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use blaze_core::lock::Lock;

use crate::{Kind, View, clock, live::Log};

/// The most a request (its line and headers) may be; longer is refused unread.
pub(crate) const MAX_REQUEST: u64 = 16 << 10;
/// How long a client has, from the accept, to send its whole request before it is cut off.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a write may block on a client that stopped reading.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Connections waiting for their token check at once; one more evicts the oldest of them.
pub(crate) const MAX_UNCHECKED: usize = 32;
/// The longest the accept loop pauses after a failed `accept` (EMFILE and the like).
const MAX_BACKOFF_MS: u64 = 1_000;

/// A bound listener, not yet serving: [`crate::start`] binds first, so a bad address fails
/// before anything else starts.
pub struct Bound {
    listener: TcpListener,
    pub addr: SocketAddr,
}

/// Binds `addr`, loopback only.
pub fn bind(addr: &str) -> Result<Bound, String> {
    let listener = TcpListener::bind(addr).map_err(|why| format!("binding {addr}: {why}"))?;
    let bound =
        listener.local_addr().map_err(|why| format!("the address bound for {addr}: {why}"))?;
    if !bound.ip().is_loopback() {
        return Err(format!(
            "{addr} is not a loopback address: the profile would be readable from other hosts; use 127.0.0.1:<port>"
        ));
    }
    Ok(Bound { listener, addr: bound })
}

/// The serving thread. Dropping it (or [`Server::stop`]) stops accepting, joins the thread
/// and closes the listener, so the address can be bound again. Connections already open end
/// on their own: a live stream sends END within its tick once the profiler has stopped.
pub struct Server {
    pub addr: SocketAddr,
    pub token: String,
    /// Written to wake the accept loop's `poll` to stop.
    wake: std::os::unix::net::UnixStream,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Bound {
    /// Serves on a thread, every request checked against a new random token.
    pub fn serve(self) -> Result<Server, String> {
        let token = crate::token::new().map_err(|why| format!("making its token: {why}"))?;
        let (wake, woken) = std::os::unix::net::UnixStream::pair()
            .map_err(|why| format!("its stop socket: {why}"))?;
        self.listener.set_nonblocking(true).map_err(|why| format!("the listener: {why}"))?;
        let want: Arc<str> = Arc::from(token.as_str());
        let listener = self.listener;
        let thread = std::thread::Builder::new()
            .name("ix-prof-http".to_owned())
            .spawn(move || accept_loop(&listener, &woken, &want))
            .map_err(|why| format!("the server thread did not start: {why}"))?;
        Ok(Server { addr: self.addr, token, wake, thread: Some(thread) })
    }
}

impl Server {
    pub fn stop(mut self) {
        self.shut();
    }

    fn shut(&mut self) {
        let Some(thread) = self.thread.take() else { return };
        // One byte ends the accept loop's poll; a failed write (the loop already gone, its
        // end closed) leaves nothing to wake.
        let _ = (&self.wake).write_all(&[1]);
        let _wait = tracing::info_span!(target: "blaze::ix_prof", "join", wait = "thread", on = "ix-prof-http").entered();
        let _ = thread.join();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shut();
    }
}

/// Accepts until a byte arrives on `woken` (or it closes): a wait on both, no timer, except
/// after a failed `accept`, when it waits on `woken` alone for a while (10 ms, doubling to 1 s)
/// rather than spin on a listener that stays readable. Each pause is one deadline on `clock`,
/// made as it begins, so an interrupted wait goes on for what is left of it, never anew.
fn accept_loop(listener: &TcpListener, woken: &std::os::unix::net::UnixStream, token: &Arc<str>) {
    use std::os::fd::AsRawFd;

    use blaze_core::wait::{self, Clock as _, Deadline, Monotonic, Waited};
    let clock = Monotonic::new();
    let gate = Arc::new(Gate::default());
    let mut fds = [
        libc::pollfd { fd: woken.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 },
    ];
    // Set while `accept` keeps failing: the pause before the listener is waited on again.
    let mut backoff_ms: Option<u64> = None;
    loop {
        fds[0].revents = 0;
        fds[1].revents = 0;
        // Backing off, only `woken` is waited on (the first fd), until the pause ends.
        let (nfds, deadline) = match backoff_ms {
            Some(ms) => (1, Some(Deadline::after(clock.now(), Duration::from_millis(ms)))),
            None => (2, None),
        };
        let waited = {
            let _wait = backoff_ms
                .map(|ms| tracing::info_span!(target: "blaze::ix_prof", "accept_backoff", wait = "timer", on = "ix-prof-http", ms).entered());
            wait::readable(
                "the ix-prof endpoint's listener or its stop byte",
                &mut fds[..nfds],
                deadline,
                &clock,
            )
        };
        match waited {
            // A pause that ran out leaves both `revents` at 0: the listener is tried again.
            Ok(Waited::Ready(_) | Waited::Deadline) => {}
            Err(why) => {
                eprintln!("ix-prof: the /debug/pprof endpoint stopped: {why}");
                return;
            }
        }
        if fds[0].revents != 0 {
            return;
        }
        // Nonblocking: a client that went between the poll and here is no wait.
        loop {
            match listener.accept() {
                Ok((conn, _)) => {
                    backoff_ms = None;
                    admit(conn, &gate, token);
                }
                Err(why) if why.kind() == std::io::ErrorKind::WouldBlock => {
                    // Drained, so `accept` works again: back to polling the listener.
                    backoff_ms = None;
                    break;
                }
                Err(why)
                    if matches!(
                        why.kind(),
                        std::io::ErrorKind::Interrupted | std::io::ErrorKind::ConnectionAborted
                    ) => {}
                Err(why) => {
                    backoff_ms = Some(match backoff_ms {
                        None => {
                            eprintln!(
                                "ix-prof: the /debug/pprof endpoint cannot accept ({why}); pausing accepts, up to {MAX_BACKOFF_MS} ms at a time, until it can"
                            );
                            10
                        }
                        Some(ms) => (ms * 2).min(MAX_BACKOFF_MS),
                    });
                    break;
                }
            }
        }
    }
}

/// One accepted connection, served on its own thread. Its request must be whole by
/// [`READ_TIMEOUT`] after now. It waits in the [`Gate`] until it passes the token check; when
/// that puts more than [`MAX_UNCHECKED`] there, the oldest waiting is shut down.
fn admit(conn: TcpStream, gate: &Arc<Gate>, token: &Arc<str>) {
    let deadline = Instant::now() + READ_TIMEOUT;
    // An accepted socket inherits nonblocking on macOS; the handler reads blocking, bounded by
    // the deadline. The clone is the gate's handle to shut it down; without one (out of
    // descriptors) the connection is closed here.
    let Ok(handle_of) = conn.try_clone() else { return };
    if conn.set_nonblocking(false).is_err() || conn.set_write_timeout(Some(WRITE_TIMEOUT)).is_err()
    {
        return;
    }
    let (waiting, evicted) = gate.enter(handle_of);
    if let Some(oldest) = evicted {
        // Its thread's read returns at once, and it ends.
        let _ = oldest.shutdown(Shutdown::Both);
    }
    let token = Arc::clone(token);
    // A thread that fails to start drops the closure, and with it the connection's place.
    let _ = std::thread::Builder::new().name("ix-prof-http-conn".to_owned()).spawn(move || {
        let _ = handle(conn, deadline, waiting, &token);
    });
}

/// The connections that have not passed the token check, oldest first, with a handle to shut
/// each down. Anyone local can open these, so there are at most [`MAX_UNCHECKED`]: a new one
/// evicts the oldest rather than being refused, or whoever holds the slots would lock the
/// owner out. A connection leaves when it passes the check (it is then never evicted) or ends.
struct Gate(Lock<GateState>);

impl Default for Gate {
    fn default() -> Gate {
        Gate(Lock::new("ixprof.http.gate", GateState::default()))
    }
}

#[derive(Default)]
struct GateState {
    next: u64,
    waiting: VecDeque<(u64, TcpStream)>,
}

impl Gate {
    /// Enters `conn`, giving its place and the connection it evicts, if any.
    fn enter(self: &Arc<Gate>, conn: TcpStream) -> (Waiting, Option<TcpStream>) {
        let mut g = self.0.lock();
        let id = g.next;
        g.next += 1;
        let evicted = (g.waiting.len() >= MAX_UNCHECKED)
            .then(|| g.waiting.pop_front())
            .flatten()
            .map(|it| it.1);
        g.waiting.push_back((id, conn));
        drop(g);
        (Waiting { gate: Arc::clone(self), id, left: false }, evicted)
    }
}

/// A connection's place in the [`Gate`], given back when it passes the token check or ends.
struct Waiting {
    gate: Arc<Gate>,
    id: u64,
    left: bool,
}

impl Waiting {
    fn leave(&mut self) {
        if std::mem::replace(&mut self.left, true) {
            return;
        }
        let gone = {
            let mut g = self.gate.0.lock();
            g.waiting.iter().position(|it| it.0 == self.id).and_then(|i| g.waiting.remove(i))
        };
        // Closed outside the lock.
        drop(gone);
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        self.leave();
    }
}

/// Whether `given` is the token, in time that does not depend on where they differ.
pub fn token_matches(given: Option<&str>, want: &str) -> bool {
    crate::token::matches(given, want)
}

struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    origin: Option<String>,
    host: Option<String>,
    /// The token of an `Authorization: Bearer` header.
    bearer: Option<String>,
}

impl Request {
    fn get(&self, key: &str) -> Option<&str> {
        self.query.iter().find(|it| it.0 == key).map(|it| it.1.as_str())
    }

    /// The token the client sent: its `Authorization: Bearer` header, else (a browser) the
    /// query's `token=`.
    fn token(&self) -> Option<&str> {
        self.bearer.as_deref().or_else(|| self.get("token"))
    }

    fn num<T: std::str::FromStr>(&self, key: &str) -> Result<Option<T>, String> {
        self.get(key)
            .map(|v| v.parse().map_err(|_| format!("query `{key}={v}` is not a number")))
            .transpose()
    }
}

/// Reads one request through a [`MAX_REQUEST`] cap: a line or header that has not ended by
/// then is refused, never read further; nor is one not whole by `deadline`.
fn read_request(conn: &TcpStream, deadline: Instant) -> Result<Request, String> {
    parse_request(Before { conn, deadline })
}

/// Reads from a connection until a deadline: each read waits at most for what is left of it,
/// so a client that trickles a byte at a time is cut off at the deadline all the same.
struct Before<'a> {
    conn: &'a TcpStream,
    deadline: Instant,
}

impl Read for Before<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let late = || {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("the request was not whole within {READ_TIMEOUT:?} of the connection"),
            )
        };
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(late());
        }
        self.conn.set_read_timeout(Some(left))?;
        let mut conn = self.conn;
        conn.read(buf).map_err(|why| match why.kind() {
            // The socket's timeout, set to what was left: the deadline passed.
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => late(),
            _ => why,
        })
    }
}

fn parse_request(from: impl Read) -> Result<Request, String> {
    let mut reader = BufReader::new(from.take(MAX_REQUEST));
    let mut line = Vec::new();
    if !next(&mut reader, &mut line)? {
        return Err("an empty request".to_owned());
    }
    let first =
        std::str::from_utf8(&line).map_err(|_| "the request line is not UTF-8".to_owned())?;
    let mut parts = first.split_whitespace();
    let method = parts.next().ok_or("an empty request")?.to_owned();
    let target = parts.next().ok_or("a request with no path")?;
    let (path, q) = target.split_once('?').unwrap_or((target, ""));
    let path = path.trim_end_matches('/').to_owned();
    let query = q
        .split('&')
        .filter(|it| !it.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (unescape(k), unescape(v))
        })
        .collect();
    let mut origin = None;
    let mut host = None;
    let mut bearer = None;
    // Bounded by the cap too: at most MAX_REQUEST lines.
    while next(&mut reader, &mut line)? {
        let h = String::from_utf8_lossy(&line);
        if h.trim().is_empty() {
            break;
        }
        let Some((k, v)) = h.split_once(':') else { continue };
        if k.eq_ignore_ascii_case("origin") {
            origin = Some(v.trim().to_owned());
        } else if k.eq_ignore_ascii_case("host") {
            host = Some(v.trim().to_owned());
        } else if k.eq_ignore_ascii_case("authorization") {
            bearer = crate::token::bearer(v).map(str::to_owned);
        }
    }
    Ok(Request { method, path, query, origin, host, bearer })
}

/// The next line into `line`, its `\n` included; false at the end. A line the cap or the end
/// of the stream cuts short is an error.
fn next<R: Read>(
    reader: &mut BufReader<std::io::Take<R>>,
    line: &mut Vec<u8>,
) -> Result<bool, String> {
    line.clear();
    let n = reader.read_until(b'\n', line).map_err(|why| format!("reading the request: {why}"))?;
    if (n > 0 && line.last() != Some(&b'\n')) || (n == 0 && reader.get_ref().limit() == 0) {
        return Err(if reader.get_ref().limit() == 0 {
            format!("the request is over {MAX_REQUEST} bytes: refused")
        } else {
            "the request ended in the middle of a line".to_owned()
        });
    }
    Ok(n > 0)
}

/// `%XX` and `+` decoded, on bytes: a `%` not followed by two hex digits stays as it is.
fn unescape(s: &str) -> String {
    fn hexval(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match (hexval(b[i + 1]), hexval(b[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push((hi << 4) | lo);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `Host` is `127.0.0.1:<port>`, `localhost:<port>` or `[::1]:<port>`: what a browser sends
/// for a loopback URL, and never for a name an attacker's DNS points here.
pub fn loopback_host(host: Option<&str>) -> bool {
    let Some(h) = host else { return false };
    let name = match h.rsplit_once(':') {
        Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => h,
    };
    matches!(name, "127.0.0.1" | "localhost" | "[::1]")
}

/// The page may be read by the explorer at ix.dev and by pages on loopback; nothing else.
fn allowed_origin(origin: Option<&str>) -> Option<&str> {
    let o = origin?;
    let loopback = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|p| o == *p || o.strip_prefix(p).is_some_and(|rest| rest.starts_with(':')));
    (o == "https://ix.dev" || loopback).then_some(o)
}

fn head(
    conn: &mut TcpStream,
    req: &Request,
    status: &str,
    content_type: &str,
    len: Option<usize>,
) -> std::io::Result<()> {
    let mut h = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nCache-Control: no-store\r\nConnection: close\r\n"
    );
    if let Some(o) = allowed_origin(req.origin.as_deref()) {
        h.push_str(&format!("Access-Control-Allow-Origin: {o}\r\nVary: Origin\r\n"));
    }
    match len {
        Some(n) => h.push_str(&format!("Content-Length: {n}\r\n")),
        None => h.push_str("Transfer-Encoding: chunked\r\n"),
    }
    h.push_str("\r\n");
    conn.write_all(h.as_bytes())
}

fn reply(
    conn: &mut TcpStream,
    req: &Request,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    head(conn, req, status, content_type, Some(body.len()))?;
    conn.write_all(body)
}

fn handle(
    mut conn: TcpStream,
    deadline: Instant,
    mut waiting: Waiting,
    token: &str,
) -> std::io::Result<()> {
    let req = match read_request(&conn, deadline) {
        Ok(it) => it,
        Err(why) => {
            let r = Request {
                method: String::new(),
                path: String::new(),
                query: Vec::new(),
                origin: None,
                host: None,
                bearer: None,
            };
            return reply(&mut conn, &r, "400 Bad Request", "text/plain", why.as_bytes());
        }
    };
    if !loopback_host(req.host.as_deref()) {
        let why = format!(
            "Host {:?} is not a loopback name: refused (a page could reach this port by DNS rebinding)",
            req.host
        );
        return reply(&mut conn, &req, "403 Forbidden", "text/plain", why.as_bytes());
    }
    if !token_matches(req.token(), token) {
        let why = "no or a wrong token: send `Authorization: Bearer <token>` with the token in <IX_PROF>/http.endpoint \
                   (`blaze pprof --dir <IX_PROF> ...` does), or, from a browser, `token=` in the query";
        return reply(&mut conn, &req, "403 Forbidden", "text/plain", why.as_bytes());
    }
    // The owner's: out of the gate, never evicted.
    waiting.leave();
    if req.method == "OPTIONS" {
        let mut h = "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Methods: GET\r\nAccess-Control-Allow-Headers: *\r\nAccess-Control-Allow-Private-Network: true\r\nContent-Length: 0\r\nConnection: close\r\n".to_owned();
        if let Some(o) = allowed_origin(req.origin.as_deref()) {
            h.push_str(&format!("Access-Control-Allow-Origin: {o}\r\nVary: Origin\r\n"));
        }
        h.push_str("\r\n");
        return conn.write_all(h.as_bytes());
    }
    if req.method != "GET" {
        return reply(&mut conn, &req, "405 Method Not Allowed", "text/plain", b"only GET");
    }
    let Some(session) = crate::session() else {
        return reply(
            &mut conn,
            &req,
            "503 Service Unavailable",
            "text/plain",
            b"ix-prof has stopped",
        );
    };
    let route = req.path.strip_prefix("/debug/pprof").unwrap_or("?");
    let result: Result<(&str, Vec<u8>), (&str, String)> = match route {
        "" => Ok(("text/plain; charset=utf-8", INDEX.as_bytes().to_vec())),
        "/profile" => cpu(&req),
        "/heap" => view(&req, Kind::Heap, None, Some("inuse_space")),
        "/allocs" => view(&req, Kind::Heap, None, Some("alloc_space")),
        "/spans" => window_last(&req).and_then(|w| view(&req, Kind::Spans, w, None)),
        "/waits" => window_last(&req).and_then(|w| view(&req, Kind::Waits, w, None)),
        "/threads" => Ok(("text/plain; charset=utf-8", threads().into_bytes())),
        "/ixprof" => ixprof(&req),
        "/live" => return live(&mut conn, &req, &session),
        _ => Err(("404 Not Found", format!("no endpoint {}; see /debug/pprof/", req.path))),
    };
    match result {
        Ok((ty, body)) => reply(&mut conn, &req, "200 OK", ty, &body),
        Err((status, why)) => {
            reply(&mut conn, &req, status, "text/plain; charset=utf-8", why.as_bytes())
        }
    }
}

const INDEX: &str = "ix-prof /debug/pprof (every request: `Authorization: Bearer <token>`, or a browser's &token=<token>)\n\
profile?seconds=30   CPU profile of the next N seconds (pprof)\n\
heap                 live allocations by stack (pprof)\n\
allocs               all allocations by stack (pprof)\n\
spans?seconds=N      span self time (pprof)\n\
waits?seconds=N      time waiting, by what is waited on (pprof)\n\
threads              each thread's open spans now\n\
ixprof?last=N        a window snapshot (.ixprof; format=pprof|chrome)\n\
live?cursor=C        the live stream\n";

type Answer = Result<(&'static str, Vec<u8>), (&'static str, String)>;

fn bad(why: String) -> (&'static str, String) {
    ("400 Bad Request", why)
}

/// `?seconds=N` as the last N seconds, or everything retained.
fn window_last(
    req: &Request,
) -> Result<Option<(Option<i64>, Option<i64>)>, (&'static str, String)> {
    let secs: Option<u64> = req.num("seconds").map_err(bad)?;
    Ok(secs.map(|s| (Some(clock::now_ns() - (s as i64).saturating_mul(1_000_000_000)), None)))
}

fn snapshot(
    window: Option<(Option<i64>, Option<i64>)>,
) -> Result<crate::Profile, (&'static str, String)> {
    let (since, until) = window.unwrap_or((None, None));
    crate::snapshot(since, until)
        .ok_or(("503 Service Unavailable", "ix-prof has stopped".to_owned()))
}

fn pprof_of(p: &crate::Profile, kind: Kind, default: Option<&str>) -> Answer {
    let mut v = View::of(p, kind);
    if let Some(i) = v.type_index(default) {
        v.default_type = i;
    }
    Ok(("application/octet-stream", v.to_pprof()))
}

fn view(
    _req: &Request,
    kind: Kind,
    window: Option<(Option<i64>, Option<i64>)>,
    default: Option<&str>,
) -> Answer {
    pprof_of(&snapshot(window)?, kind, default)
}

fn cpu(req: &Request) -> Answer {
    let secs: u64 = req.num("seconds").map_err(bad)?.unwrap_or(30);
    if secs == 0 || secs > 3600 {
        return Err(bad(format!("seconds={secs}: give 1 to 3600")));
    }
    let from = clock::now_ns();
    #[allow(
        clippy::disallowed_methods,
        reason = "this connection's own thread waits out the profile's window, as Go's handler does"
    )]
    std::thread::sleep(Duration::from_secs(secs));
    pprof_of(&snapshot(Some((Some(from), None)))?, Kind::Cpu, None)
}

fn ixprof(req: &Request) -> Answer {
    let since: Option<i64> = req.num("since_ns").map_err(bad)?;
    let until: Option<i64> = req.num("until_ns").map_err(bad)?;
    let last: Option<f64> = req.num("last").map_err(bad)?;
    let since = match (since, last) {
        (Some(_), Some(_)) => return Err(bad("give since_ns or last, not both".to_owned())),
        (None, Some(l)) => Some(clock::now_ns() - (l * 1e9) as i64),
        (s, None) => s,
    };
    let p = snapshot(Some((since, until)))?;
    match req.get("format").unwrap_or("ixprof") {
        "ixprof" => p
            .to_ixprof()
            .map(|b| ("application/vnd.ix.prof", b))
            .map_err(|why| ("500 Internal Server Error", format!("encoding .ixprof: {why}"))),
        "chrome" => Ok(("application/json", crate::chrome::to_chrome(&p).into_bytes())),
        "pprof" => {
            let kind = req.get("kind").unwrap_or("cpu");
            let kind = Kind::parse(kind)
                .ok_or_else(|| bad(format!("kind={kind}: give cpu, heap, spans or waits")))?;
            pprof_of(&p, kind, None)
        }
        other => Err(bad(format!("format={other}: give ixprof, pprof or chrome"))),
    }
}

fn threads() -> String {
    let mut out = String::new();
    let mut last = None;
    for s in crate::open_spans() {
        if last != Some(s.thread) {
            out.push_str(&format!("thread {}\n", s.thread));
            last = Some(s.thread);
        }
        out.push_str(&format!(
            "  {}{} {:.3}ms",
            "  ".repeat(usize::from(s.depth)),
            s.name,
            s.dur_ns as f64 / 1e6
        ));
        if let Some(w) = &s.wait {
            out.push_str(&format!(" wait {w}"));
            if let Some(on) = &s.wait_on {
                out.push_str(&format!(" on {on}"));
            }
            if let Some(owner) = &s.wait_owner {
                out.push_str(&format!(" owner {owner}"));
            }
        }
        out.push('\n');
    }
    out
}

fn chunk(conn: &mut TcpStream, bytes: &[u8]) -> std::io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    conn.write_all(format!("{:x}\r\n", bytes.len()).as_bytes())?;
    conn.write_all(bytes)?;
    conn.write_all(b"\r\n")
}

/// The live stream: HELLO, then everything after the cursor, then each tick's frames and its
/// TICK, until the profiler stops (END) or the client goes. A client that falls behind the
/// retained bytes gets a GAP naming what it missed, never a silent skip.
fn live(conn: &mut TcpStream, req: &Request, session: &crate::Session) -> std::io::Result<()> {
    session.live_wanted.store(true, Ordering::Relaxed);
    let tick_ms: u64 = match req.num("tick_ms") {
        Ok(it) => it.unwrap_or(250).clamp(50, 5_000),
        Err(why) => return reply(conn, req, "400 Bad Request", "text/plain", why.as_bytes()),
    };
    // From now (the catalog whole) unless `from=start` asks for everything retained.
    let fresh_from_now = req.get("from") != Some("start");
    head(conn, req, "200 OK", "application/vnd.ix.prof-stream", None)?;
    let meta = session.recorder.lock().meta_mut().clone();
    let (hello, mut from) = session.log.lock().hello(&meta, req.get("cursor"));
    chunk(conn, &hello)?;
    let mut first = true;
    loop {
        let (since, tick, ended) = {
            let log = session.log.lock();
            let since = log.since(from, first && fresh_from_now);
            let tick = log.tick_frame(since.next);
            (since, tick, log.ended.clone())
        };
        first = false;
        if let Some(gap) = &since.gap {
            chunk(conn, gap)?;
        }
        for f in &since.frames {
            chunk(conn, f)?;
        }
        if since.next > from || since.gap.is_some() {
            if let Some(t) = tick {
                chunk(conn, &t)?;
            }
        }
        from = since.next;
        if let Some(why) = ended {
            chunk(conn, &Log::end_frame(&why))?;
            return conn.write_all(b"0\r\n\r\n");
        }
        if crate::session().is_none() {
            chunk(conn, &Log::end_frame("profiler stopped"))?;
            return conn.write_all(b"0\r\n\r\n");
        }
        #[allow(
            clippy::disallowed_methods,
            reason = "this connection's own thread polls the log at the client's tick; a driver's timer"
        )]
        std::thread::sleep(Duration::from_millis(tick_ms));
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read as _, Write as _},
        net::TcpStream,
        os::unix::fs::PermissionsExt as _,
        time::{Duration, Instant},
    };

    use super::{MAX_UNCHECKED, READ_TIMEOUT, parse_request, unescape};

    /// An answer read to its end (or the client's timeout).
    fn answer(mut c: TcpStream) -> String {
        let mut s = String::new();
        let _ = c.read_to_string(&mut s);
        s
    }

    fn scratch(what: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "ix-prof-{what}-{}-{}",
            std::process::id(),
            crate::nonce()
        ))
    }

    #[test]
    fn a_percent_before_a_multibyte_char_decodes_without_panicking() {
        assert_eq!(unescape("%0\u{e9}"), "%0\u{e9}");
        assert_eq!(unescape("%\u{e9}"), "%\u{e9}");
        assert_eq!(unescape("a%20b+c%2"), "a b c%2");
        assert_eq!(unescape("%C3%a9"), "\u{e9}");
    }

    /// Endless header lines, each one whole.
    struct Headers;

    impl std::io::Read for Headers {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let line = b"X-Fill: 0123456789abcdef\r\n";
            let n = buf.len().min(line.len());
            buf[..n].copy_from_slice(&line[..n]);
            Ok(n)
        }
    }

    #[test]
    fn a_request_that_never_ends_is_refused_after_16_kib() {
        let line = parse_request(std::io::repeat(b'a')).err();
        assert!(
            line.as_deref().is_some_and(|why| why.contains("over 16384 bytes")),
            "an endless request line: {line:?}"
        );
        let headers =
            parse_request(std::io::Read::chain(&b"GET /debug/pprof HTTP/1.1\r\n"[..], Headers))
                .err();
        assert!(
            headers.as_deref().is_some_and(|why| why.contains("over 16384 bytes")),
            "endless headers: {headers:?}"
        );
        let ok = parse_request(&b"GET /debug/pprof/heap?token=ab HTTP/1.1\r\nHost: 127.0.0.1:1\r\nAuthorization: Bearer cd\r\n\r\n"[..])
            .unwrap_or_else(|why| panic!("a whole request: {why}"));
        assert_eq!(
            (ok.path.as_str(), ok.get("token"), ok.token()),
            ("/debug/pprof/heap", Some("ab"), Some("cd"))
        );
    }

    /// S2: when [`MAX_UNCHECKED`] connections wait for their token check, one more evicts the
    /// oldest of them instead of being turned away, so connections held open by another user
    /// cannot lock the owner out; and the owner's request is answered.
    #[test]
    fn a_connection_over_the_cap_evicts_the_oldest_unchecked_one() {
        let _g = crate::TEST_GLOBALS.lock();
        let dir = scratch("evict");
        let (guard, addr, token) = start_serving(&dir);
        // Each would hold its place until the deadline: they send nothing.
        let idle: Vec<TcpStream> =
            (0..MAX_UNCHECKED).map(|_| TcpStream::connect(&addr).unwrap()).collect();
        // Let the server take all of them in before the owner's comes.
        std::thread::sleep(Duration::from_millis(300));
        let mut owner = TcpStream::connect(&addr).unwrap();
        owner.set_read_timeout(Some(READ_TIMEOUT / 2)).unwrap();
        write!(
            owner,
            "GET /debug/pprof/ HTTP/1.0\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\n\r\n"
        )
        .unwrap();
        let got = answer(owner);
        assert!(
            got.starts_with("HTTP/1.1 200"),
            "connection {} with {MAX_UNCHECKED} waiting: {got:?}",
            MAX_UNCHECKED + 1
        );
        // The oldest was shut down at once, long before its deadline.
        let mut oldest = idle.into_iter().next().unwrap();
        oldest.set_read_timeout(Some(READ_TIMEOUT / 2)).unwrap();
        let t = Instant::now();
        let mut buf = [0u8; 64];
        let n = oldest.read(&mut buf);
        assert!(
            n.as_ref().is_ok_and(|&n| n == 0 || buf[..n].starts_with(b"HTTP/1.1 400")),
            "the oldest: {n:?}"
        );
        assert!(
            t.elapsed() < READ_TIMEOUT / 2,
            "the oldest was not evicted: it lasted {:?} more",
            t.elapsed()
        );
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_client_that_sends_nothing_is_cut_off_after_the_read_timeout() {
        let server = super::bind("127.0.0.1:0").unwrap().serve().unwrap();
        let c = TcpStream::connect(server.addr).unwrap();
        c.set_read_timeout(Some(READ_TIMEOUT * 3)).unwrap();
        let t = Instant::now();
        let got = answer(c);
        let took = t.elapsed();
        assert!(
            got.starts_with("HTTP/1.1 400") && took < READ_TIMEOUT * 2,
            "{got:?} after {took:?}"
        );
    }

    /// S2, from the review's `review_a_trickling_client_holds_its_slot_past_the_read_timeout`,
    /// which showed a client sending a byte every 2 s held its connection 14 s: the deadline is
    /// for the whole request, so a client that trickles is cut off at [`READ_TIMEOUT`] all
    /// the same.
    #[test]
    fn a_trickling_client_is_cut_off_at_the_requests_deadline() {
        let server = super::bind("127.0.0.1:0").unwrap().serve().unwrap();
        let mut c = TcpStream::connect(server.addr).unwrap();
        // Between bytes the client waits for an answer, so it never sends into a closed socket.
        c.set_read_timeout(Some(Duration::from_millis(900))).unwrap();
        let t = Instant::now();
        let mut got = Vec::new();
        for b in b"GET /debug/pprof/ HTTP/1.0\r\n".iter().take(14) {
            if c.write_all(&[*b]).is_err() {
                break;
            }
            let mut buf = [0u8; 512];
            match c.read(&mut buf) {
                Ok(n) => {
                    // The answer, to its end (the server closes after it).
                    got.extend_from_slice(&buf[..n]);
                    let _ = c.read_to_end(&mut got);
                    break;
                }
                Err(why)
                    if matches!(
                        why.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => break,
            }
        }
        let took = t.elapsed();
        let got = String::from_utf8_lossy(&got);
        assert!(
            took < READ_TIMEOUT + Duration::from_secs(2),
            "a byte every 0.9 s held the connection {took:?}: {got:?}"
        );
        assert!(
            got.starts_with("HTTP/1.1 400") && got.contains("not whole within"),
            "after {took:?}: {got:?}"
        );
    }

    fn start_serving(dir: &std::path::Path) -> (crate::Guard, String, String) {
        let guard = crate::start(crate::Config {
            http: Some("127.0.0.1:0".to_owned()),
            dir: Some(dir.to_owned()),
            cpu_hz: None,
            alloc_sample_bytes: None,
            ..crate::Config::default()
        })
        .unwrap();
        let e = crate::endpoint::read(dir).unwrap().expect("http.endpoint after start");
        (guard, e.addr.to_string(), e.token)
    }

    /// The prof-review lane's `review_token_gate`, with the `Authorization: Bearer` cases.
    #[test]
    fn only_a_request_with_the_token_is_answered() {
        let _g = crate::TEST_GLOBALS.lock();
        let dir = scratch("gate");
        let (guard, addr, token) = start_serving(&dir);
        let mode = std::fs::metadata(dir.join(crate::endpoint::FILE)).unwrap().permissions().mode()
            & 0o777;
        assert_eq!(mode, 0o600, "http.endpoint mode");
        let get = |path: &str, auth: Option<&str>| -> String {
            let mut c = TcpStream::connect(&addr).unwrap();
            let auth = auth.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
            write!(c, "GET {path} HTTP/1.0\r\nHost: {addr}\r\n{auth}\r\n").unwrap();
            answer(c)
        };
        assert!(get("/debug/pprof/", None).starts_with("HTTP/1.1 403"), "no token");
        assert!(get("/debug/pprof/?token=00", None).starts_with("HTTP/1.1 403"), "short token");
        let wrong = "0".repeat(32);
        assert!(
            get(&format!("/debug/pprof/?token={wrong}"), None).starts_with("HTTP/1.1 403"),
            "wrong token"
        );
        assert!(
            get("/debug/pprof/", Some(wrong.as_str())).starts_with("HTTP/1.1 403"),
            "wrong bearer token"
        );
        let ok = get(&format!("/debug/pprof/?token={token}"), None);
        assert!(ok.starts_with("HTTP/1.1 200"), "the token in the query: {ok}");
        let ok = get("/debug/pprof/", Some(token.as_str()));
        assert!(ok.starts_with("HTTP/1.1 200"), "the token as a bearer: {ok}");
        drop(guard);
        assert!(TcpStream::connect(&addr).is_err(), "port released after the guard drops");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S5 and S6: the address and the token are one file, written with one rename (no
    /// `http.addr`/`http.token` pair a reader could catch half replaced, and no part file left),
    /// and stopping takes it back.
    #[test]
    fn the_endpoint_is_one_file_and_stopping_takes_it_back() {
        let _g = crate::TEST_GLOBALS.lock();
        let dir = scratch("retract");
        let (guard, addr, token) = start_serving(&dir);
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|it| it.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [crate::endpoint::FILE], "the directory while serving");
        assert_eq!(
            std::fs::read_to_string(dir.join(crate::endpoint::FILE)).unwrap(),
            format!("{addr}\n{token}\n")
        );
        drop(guard);
        assert!(
            !dir.join(crate::endpoint::FILE).exists(),
            "http.endpoint is still there after the stop"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S1, the server's half, from the review's `review_an_existing_world_writable_symlinked_dir_is_used_as_is`:
    /// an `IX_PROF` that is a link (here to a 0777 directory), or a directory open to group or
    /// other, is refused at start, naming the directory and the rule, and nothing is written
    /// into it.
    #[test]
    fn an_ix_prof_dir_that_is_a_link_or_open_to_others_is_refused() {
        let _g = crate::TEST_GLOBALS.lock();
        let root = scratch("shared-dir");
        let shared = root.join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        let link = root.join("prof");
        std::os::unix::fs::symlink(&shared, &link).unwrap();
        let start = |dir: &std::path::Path| {
            crate::start(crate::Config {
                http: Some("127.0.0.1:0".to_owned()),
                dir: Some(dir.to_owned()),
                cpu_hz: None,
                alloc_sample_bytes: None,
                ..crate::Config::default()
            })
            .map(drop)
            .map_err(|why| why.to_string())
        };
        let linked = start(&link);
        let open = start(&shared);
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o750)).unwrap();
        let group = start(&shared);
        let left: Vec<_> =
            std::fs::read_dir(&shared).unwrap().map(|it| it.unwrap().file_name()).collect();
        let _ = std::fs::remove_dir_all(&root);
        let link_s = link.display().to_string();
        let shared_s = shared.display().to_string();
        assert!(
            linked
                .as_ref()
                .is_err_and(|why| why.contains(&link_s) && why.contains("symbolic link")),
            "a link: {linked:?}"
        );
        assert!(
            open.as_ref().is_err_and(|why| why.contains(&shared_s) && why.contains("mode 0777")),
            "mode 0777: {open:?}"
        );
        assert!(
            group.as_ref().is_err_and(|why| why.contains(&shared_s) && why.contains("mode 0750")),
            "mode 0750: {group:?}"
        );
        assert!(left.is_empty(), "written into the refused directory: {left:?}");
        assert!(
            !crate::recording() && crate::session().is_none(),
            "a refused start leaves no session"
        );
    }

    /// S1: at stop, a FIFO planted as `http.endpoint` (or a link to `/dev/zero`) is refused by
    /// its type, not read: the stop neither hangs on it nor reads without end.
    #[test]
    fn a_fifo_planted_as_the_endpoint_does_not_hang_the_stop() {
        let _g = crate::TEST_GLOBALS.lock();
        let dir = scratch("fifo");
        let (guard, _, _) = start_serving(&dir);
        let file = dir.join(crate::endpoint::FILE);
        std::fs::remove_file(&file).unwrap();
        let c = std::ffi::CString::new(file.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let (done, stopped) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(guard);
            let _ = done.send(());
        });
        let ended = stopped.recv_timeout(Duration::from_secs(10)).is_ok();
        let still = std::fs::symlink_metadata(&file).is_ok();
        if ended {
            let _ = std::fs::remove_dir_all(&dir);
        }
        assert!(ended, "the stop hung on a FIFO at {}", file.display());
        assert!(still, "the FIFO, not this session's file, was removed");
    }

    /// S3: with a directory, the line on stderr says where the token is, never the token, since
    /// stderr often lands where other users read it; without one it is the only place.
    #[test]
    fn stderr_carries_the_token_only_when_there_is_no_directory_for_it() {
        let addr = "127.0.0.1:6060".parse().unwrap();
        let token = "0123456789abcdef0123456789abcdef";
        let dir = std::path::Path::new("/home/me/prof");
        let with = crate::announcement(addr, token, Some(dir));
        assert!(!with.contains(token) && with.contains("/home/me/prof/http.endpoint"), "{with}");
        let without = crate::announcement(addr, token, None);
        assert!(without.contains(token), "{without}");
    }
}
