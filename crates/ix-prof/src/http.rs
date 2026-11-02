//! `/debug/pprof` over HTTP (feature `http`), a driver: Go's `net/http/pprof` layout plus the
//! live stream (docs/profiling.md, "HTTP endpoints" and "The live stream"). One thread per
//! connection, loopback only: a profile holds the program's function names, arguments in
//! span fields and timing, so another user or host must not read it.
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
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::atomic::Ordering,
    time::Duration,
};

use crate::{Kind, View, clock, live::Log};

/// Binds `addr` (loopback only) and serves on a thread; gives the address bound.
pub fn serve(addr: &str) -> Result<SocketAddr, String> {
    let listener = TcpListener::bind(addr).map_err(|why| format!("binding {addr}: {why}"))?;
    let bound = listener.local_addr().map_err(|why| format!("the address bound for {addr}: {why}"))?;
    if !bound.ip().is_loopback() {
        return Err(format!(
            "{addr} is not a loopback address: the profile would be readable from other hosts; use 127.0.0.1:<port>"
        ));
    }
    std::thread::Builder::new()
        .name("ix-prof-http".to_owned())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                let _ = std::thread::Builder::new().name("ix-prof-http-conn".to_owned()).spawn(move || {
                    let _ = handle(conn);
                });
            }
        })
        .map_err(|why| format!("the server thread did not start: {why}"))?;
    Ok(bound)
}

struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    origin: Option<String>,
    host: Option<String>,
}

impl Request {
    fn get(&self, key: &str) -> Option<&str> {
        self.query.iter().find(|it| it.0 == key).map(|it| it.1.as_str())
    }

    fn num<T: std::str::FromStr>(&self, key: &str) -> Result<Option<T>, String> {
        self.get(key).map(|v| v.parse().map_err(|_| format!("query `{key}={v}` is not a number"))).transpose()
    }
}

fn read_request(conn: &TcpStream) -> Result<Request, String> {
    let mut reader = BufReader::new(conn.try_clone().map_err(|it| it.to_string())?);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|it| it.to_string())?;
    let mut parts = line.split_whitespace();
    let method = parts.next().ok_or("an empty request")?.to_owned();
    let target = parts.next().ok_or("a request with no path")?;
    let (path, q) = target.split_once('?').unwrap_or((target, ""));
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
    for _ in 0..100 {
        let mut h = String::new();
        if reader.read_line(&mut h).map_err(|it| it.to_string())? == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':')
            && k.eq_ignore_ascii_case("origin")
        {
            origin = Some(v.trim().to_owned());
        }
        if let Some((k, v)) = h.split_once(':')
            && k.eq_ignore_ascii_case("host")
        {
            host = Some(v.trim().to_owned());
        }
    }
    Ok(Request { method, path: path.trim_end_matches('/').to_owned(), query, origin, host })
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
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

fn head(conn: &mut TcpStream, req: &Request, status: &str, content_type: &str, len: Option<usize>) -> std::io::Result<()> {
    let mut h = format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nCache-Control: no-store\r\nConnection: close\r\n");
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

fn reply(conn: &mut TcpStream, req: &Request, status: &str, content_type: &str, body: &[u8]) -> std::io::Result<()> {
    head(conn, req, status, content_type, Some(body.len()))?;
    conn.write_all(body)
}

fn handle(mut conn: TcpStream) -> std::io::Result<()> {
    let req = match read_request(&conn) {
        Ok(it) => it,
        Err(why) => {
            let r = Request { method: String::new(), path: String::new(), query: Vec::new(), origin: None, host: None };
            return reply(&mut conn, &r, "400 Bad Request", "text/plain", why.as_bytes());
        }
    };
    if !loopback_host(req.host.as_deref()) {
        let why = format!("Host {:?} is not a loopback name: refused (a page could reach this port by DNS rebinding)", req.host);
        return reply(&mut conn, &req, "403 Forbidden", "text/plain", why.as_bytes());
    }
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
        return reply(&mut conn, &req, "503 Service Unavailable", "text/plain", b"ix-prof has stopped");
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
        Err((status, why)) => reply(&mut conn, &req, status, "text/plain; charset=utf-8", why.as_bytes()),
    }
}

const INDEX: &str = "ix-prof /debug/pprof\n\
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
fn window_last(req: &Request) -> Result<Option<(Option<i64>, Option<i64>)>, (&'static str, String)> {
    let secs: Option<u64> = req.num("seconds").map_err(bad)?;
    Ok(secs.map(|s| (Some(clock::now_ns() - (s as i64).saturating_mul(1_000_000_000)), None)))
}

fn snapshot(window: Option<(Option<i64>, Option<i64>)>) -> Result<crate::Profile, (&'static str, String)> {
    let (since, until) = window.unwrap_or((None, None));
    crate::snapshot(since, until).ok_or(("503 Service Unavailable", "ix-prof has stopped".to_owned()))
}

fn pprof_of(p: &crate::Profile, kind: Kind, default: Option<&str>) -> Answer {
    let mut v = View::of(p, kind);
    if let Some(i) = v.type_index(default) {
        v.default_type = i;
    }
    Ok(("application/octet-stream", v.to_pprof()))
}

fn view(_req: &Request, kind: Kind, window: Option<(Option<i64>, Option<i64>)>, default: Option<&str>) -> Answer {
    pprof_of(&snapshot(window)?, kind, default)
}

fn cpu(req: &Request) -> Answer {
    let secs: u64 = req.num("seconds").map_err(bad)?.unwrap_or(30);
    if secs == 0 || secs > 3600 {
        return Err(bad(format!("seconds={secs}: give 1 to 3600")));
    }
    let from = clock::now_ns();
    #[allow(clippy::disallowed_methods, reason = "this connection's own thread waits out the profile's window, as Go's handler does")]
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
            let kind = Kind::parse(kind).ok_or_else(|| bad(format!("kind={kind}: give cpu, heap, spans or waits")))?;
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
        out.push_str(&format!("  {}{} {:.3}ms", "  ".repeat(usize::from(s.depth)), s.name, s.dur_ns as f64 / 1e6));
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
        #[allow(clippy::disallowed_methods, reason = "this connection's own thread polls the log at the client's tick; a driver's timer")]
        std::thread::sleep(Duration::from_millis(tick_ms));
    }
}
