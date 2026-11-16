//! The driver's promise, in bstd's simulated world (`Io::sim`: virtual clock, simulated
//! sockets): a ranged GET gives exactly the bytes asked, however the server cuts its reply into
//! pieces, and every way a server can fail or lie is refused by name, never taken:
//!
//! - it stalls: the ask's deadline (`cx.until`) ends it, `Stopped`, naming the URL;
//! - it closes early: `Truncated`;
//! - it sends another range, or a length that is not the range's: `RangeMismatch`;
//! - it sends more than it promised: `AfterEnd`;
//! - it redirects: `RedirectsOver` (an origin follows none);
//! - it has no such object: `RangeStatus { status: 404 }`.
//!
//! Wrong bytes in the right range are not HTTP's to catch: the read path's digest chain refuses
//! them (blaze-cli store.rs tests). Plant: the `Content-Range` check removed (`plant.rs`).

use std::{alloc::Global, format, string::String, vec::Vec as StdVec};

use blaze_h1::{Limits, Range, Redirects};
use blaze_own::Vec;
use blaze_sim::{Broke, Config, Tape, check, control};
use bstd::{
    Io,
    cx::Cx,
    mem::TASK_FRAME,
    net::{Addr, Conn, HostPort},
    url::Url,
};
use core::time::Duration;

use crate::{Ask, Client, NoTls, plant};

/// The object the server holds, and the range asked of it.
const OBJECT_LEN: usize = 64;
const ASKED: Range = Range { first: 8, last: Some(23) };

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Server {
    Honest,
    Stalls,
    ClosesEarly,
    WrongRange,
    WrongLength,
    Longer,
    Redirects,
    Missing,
}

const SERVERS: [Server; 8] = [
    Server::Honest,
    Server::Stalls,
    Server::ClosesEarly,
    Server::WrongRange,
    Server::WrongLength,
    Server::Longer,
    Server::Redirects,
    Server::Missing,
];

fn object() -> StdVec<u8> {
    (0..OBJECT_LEN).map(|i| (i * 7 + 3) as u8).collect()
}

/// What `server` sends, and whether it then waits for the client to hang up (a stall).
fn reply(server: Server) -> (StdVec<u8>, bool) {
    let o = object();
    let head = |status: &str, range: &str, length: usize| {
        format!("HTTP/1.1 {status}\r\nContent-Range: bytes {range}/64\r\nContent-Length: {length}\r\n\r\n").into_bytes()
    };
    match server {
        Server::Honest => ([head("206 Partial Content", "8-23", 16), o[8..24].to_vec()].concat(), false),
        Server::Stalls => (head("206 Partial Content", "8-23", 16)[..20].to_vec(), true),
        Server::ClosesEarly => ([head("206 Partial Content", "8-23", 16), o[8..14].to_vec()].concat(), false),
        Server::WrongRange => ([head("206 Partial Content", "9-24", 16), o[9..25].to_vec()].concat(), false),
        Server::WrongLength => ([head("206 Partial Content", "8-23", 17), o[8..25].to_vec()].concat(), false),
        Server::Longer => ([head("206 Partial Content", "8-23", 16), o[8..36].to_vec()].concat(), false),
        Server::Redirects => (b"HTTP/1.1 302 Found\r\nLocation: /elsewhere\r\nContent-Length: 0\r\n\r\n".to_vec(), false),
        Server::Missing => (b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(), false),
    }
}

/// The name the refusal of `server` must carry; `None` for the honest one.
fn refusal(server: Server) -> Option<&'static str> {
    match server {
        Server::Honest => None,
        Server::Stalls => Some("Stopped"),
        Server::ClosesEarly => Some("Truncated"),
        Server::WrongRange | Server::WrongLength => Some("RangeMismatch"),
        Server::Longer => Some("AfterEnd"),
        Server::Redirects => Some("RedirectsOver"),
        Server::Missing => Some("RangeStatus { status: 404 }"),
    }
}

/// Reads the request's head on `conn`, then sends `bytes` in the pieces `cuts` makes; a
/// staller then waits for the client to hang up.
fn serve(cx: &Cx, conn: &Conn, bytes: &[u8], cuts: &[usize], stall: bool) -> Result<(), String> {
    let mut got = cx.bytes();
    // Bounded: a GET's head is far under eight receives.
    let mut whole = false;
    for _ in 0..8 {
        if got.as_slice().windows(4).any(|it| it == b"\r\n\r\n") {
            whole = true;
            break;
        }
        conn.recv(cx, 4096, &mut got).map_err(|e| format!("harness: the server's receive: {e}"))?;
    }
    if !whole {
        return Err(String::from("harness: no whole request after eight receives"));
    }
    let mut from = 0;
    for &cut in cuts.iter().chain(core::iter::once(&bytes.len())) {
        let cut = cut.clamp(from, bytes.len());
        if cut > from {
            conn.send_all(cx, &bytes[from..cut]).map_err(|e| format!("harness: the server's send: {e}"))?;
        }
        from = cut;
    }
    if stall {
        // The client hangs up at its deadline: a receive of 0 (or its reset) ends the stall.
        let mut rest = cx.bytes();
        let _ = conn.recv(cx, 4096, &mut rest);
    }
    Ok(())
}

/// One ask of `server`'s reply cut as the tape says; the outcome against the oracle.
fn one(tape: &Tape, server: Server) -> Result<(), Broke> {
    let seed = tape.int_in(0u64..=1 << 20, "seed");
    let (bytes, stall) = reply(server);
    let mut cuts = tape.vec(0..=6, "cuts", |t| t.index(bytes.len() + 1, "cut"));
    cuts.sort_unstable();
    // More than promised in one piece: what the client reads with the end of the body.
    if server == Server::Longer {
        cuts.clear();
    }
    let io = Io::sim(seed);
    let cx = io.cx(Duration::from_secs(3600), TASK_FRAME).map_err(|e| Broke::Oracle(format!("harness: {e}")))?;
    let net = io.need_net().map_err(|e| Broke::Oracle(format!("harness: {e}")))?;
    let clock = io.need_clock().map_err(|e| Broke::Oracle(format!("harness: {e}")))?;
    let at = HostPort::new("127.0.0.1", 8001).map_err(|e| Broke::Oracle(format!("harness: {e}")))?;
    let addr = Addr::Tcp(at);
    let url = Url::parse("http://127.0.0.1:8001/prefix/seg-00ff").map_err(|e| Broke::Oracle(format!("harness: {e:?}")))?;
    let limits = Limits { most_head: 16 << 10, most_headers: 64, most_body: 16 };
    let redirects = Redirects { most: 0, cross_scheme: false, cross_origin: false };
    let client = Client { net: &net.connect_only(&addr), tls: &NoTls, limits, redirects, agent: "blaze-test", alloc: Global };
    let ask = Ask { method: "GET", url: &url, headers: &[], body: &[], range: Some(ASKED) };
    let outcome = cx.scope_with(|s| {
        // The task itself never fails (a failed task fails the scope and hides the client's
        // outcome); the server's own result is its value.
        let server_task = s.may_overlap("server", TASK_FRAME, |cx| -> Result<Result<(), String>, core::convert::Infallible> {
            // Listened on in the task that accepts: a listener's completions are its task's.
            let mut listener = match net.listen(cx, &addr) {
                Ok(listener) => listener,
                Err(e) => return Ok(Err(format!("harness: listen: {e}"))),
            };
            let waiting = cx.until(clock.after(Duration::from_secs(30)));
            let served = match listener.accept(&waiting) {
                Ok(conn) => {
                    let served = serve(cx, &conn, &bytes, &cuts, stall);
                    let _ = conn.finish(cx);
                    served
                }
                Err(e) => Err(format!("harness: accept: {e}")),
            };
            let _ = listener.close(cx);
            Ok(served)
        });
        // The server listens before the client connects: a second of the world's clock.
        let _ = clock.sleep(s.cx(), Duration::from_secs(1));
        let asking = s.cx().until(clock.after(Duration::from_secs(5)));
        let mut out = Vec::new_in(Global);
        let sent = client.send(&asking, &ask, &mut out).map(|got| (got, out.as_slice().to_vec()));
        let served = server_task.join(s.cx()).map_err(|e| format!("{e}")).and_then(|it| it);
        (sent, served)
    });
    let (sent, served) = outcome.map_err(|e| Broke::Oracle(format!("harness: the scope: {e}")))?;
    served.map_err(|e| Broke::Oracle(format!("harness: the client: {}; the server task: {e}", match &sent { Err(error) => format!("{error}"), Ok(_) => String::from("ok") })))?;
    match (refusal(server), sent) {
        (None, Ok((got, body))) if got.status == 206 && body == object()[8..24] => Ok(()),
        (Some(name), Err(error)) if format!("{:?}", error.why).contains(name) => {
            std::eprintln!("{server:?}: refused: {error}");
            Ok(())
        }
        (want, sent) => Err(Broke::Oracle(format!("{server:?} taken wrongly: wanted {want:?}, got {sent:?}"))),
    }
}

fn every_server(tape: &Tape) -> Result<(), Broke> {
    SERVERS.iter().try_for_each(|&server| one(tape, server))
}

#[test]
fn a_ranged_get_is_taken_whole_or_refused_by_name() {
    check(Config::cases(24), every_server);
}

#[test]
fn the_oracle_catches_the_content_range_check_removed() {
    control(Config::cases(24), false, true, "WrongRange taken wrongly", |tape, planted| {
        plant::with(planted, || one(tape, Server::WrongRange))
    });
}
