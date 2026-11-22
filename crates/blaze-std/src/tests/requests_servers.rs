//! [`Http::over`] against the servers a shared cache's origin can meet: one honest, and seven
//! that stall, close early, send another range, another length, more than they promised, a
//! redirect or a 404. A ranged GET of an object must come back as exactly the bytes asked or as
//! an error naming what was wrong, its bytes not added to the caller's.
//!
//! The server is the test's, on the simulation's sockets: it reads the request, takes the range
//! from its `Range` header, and sends its answer in tape-drawn pieces. The exchange is begun,
//! served and finished by hand (the begin/finish split).
//!
//! Plant: `requests::plant`, the `Content-Range` check skipped (the stand-alone driver's
//! `plant.rs`, moved here with the driver).

use blaze_h1::{ContentRange, Range, Why};
use blaze_sim::{Broke, Config, Tape};

use super::{check, requests::WIRE, root, world};
use crate::{
    Cx, Plant,
    net::{Addr, Conn, HostPort, Listener},
    requests::{Finished, Http, HttpError, HttpKind, Request, Response, Wire, plant},
    roots::Roots,
    url::Url,
};

const AT: &str = "127.0.0.1:8003";

/// The object's length.
const OBJECT: u64 = 4096;

/// How the test's client talks: a short wait, so a stall ends soon on the simulation's clock.
const SHORT: Wire = Wire { wait_most: core::time::Duration::from_secs(2), ..WIRE };

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Server {
    Honest,
    /// Sends half a head, then nothing, and does not close.
    Stalls,
    /// Promises the range, sends half its body and closes.
    ClosesEarly,
    /// Answers the range one byte further on.
    WrongRange,
    /// Answers the range asked with one byte fewer, and says so in its `Content-Length`.
    WrongLength,
    /// Answers the range asked, then sends more bytes than it promised.
    Longer,
    /// A 302 to another path of itself.
    Redirects,
    /// A 404.
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

#[derive(Debug)]
struct Plan {
    seed: u64,
    server: Server,
    first: u64,
    last: u64,
    /// Where the answer's bytes are cut into pieces, as fractions of 1000.
    cuts: Vec<u64>,
}

fn plan(tape: &Tape) -> Plan {
    let seed = tape.next_u64("sim seed");
    let server = *tape.one_of(&SERVERS, "server");
    let first = tape.int_in(0..=OBJECT - 3, "first");
    // Two bytes at least, so the short server's answer is one; one byte before the end, so the
    // shifted server's is inside the object.
    let last = tape.int_in(first + 1..=OBJECT - 2, "last");
    let cuts = (0..tape.int_in(0..=4u64, "cuts")).map(|_| tape.int_in(0..=1000u64, "cut")).collect();
    Plan { seed, server, first, last, cuts }
}

/// The object's byte at `at`.
fn byte(at: u64) -> u8 {
    (at * 7 % 251) as u8
}

fn bytes(first: u64, last: u64) -> Vec<u8> {
    (first..=last).map(byte).collect()
}

/// The range the request on `conn` asked for, from its `Range: bytes=first-last` header.
fn asked(cx: &Cx, conn: &Conn) -> Result<(u64, u64), String> {
    let mut into = cx.bytes();
    // Bounded: a request here is far under eight receives.
    for _ in 0..8 {
        let bytes = into.as_slice();
        if let Some(end) = bytes.windows(4).position(|it| it == b"\r\n\r\n") {
            let head = std::str::from_utf8(&bytes[..end]).map_err(|e| format!("harness: a request's head: {e}"))?;
            let value = head
                .lines()
                .find_map(|line| line.split_once(':').filter(|(name, _)| name.eq_ignore_ascii_case("range")))
                .map(|(_, value)| value.trim())
                .ok_or("harness: the request asked no range")?;
            let (first, last) = value
                .strip_prefix("bytes=")
                .and_then(|it| it.split_once('-'))
                .ok_or_else(|| format!("harness: a range {value:?}"))?;
            let first = first.parse().map_err(|e| format!("harness: a range's first {first:?}: {e}"))?;
            let last = last.parse().map_err(|e| format!("harness: a range's last {last:?}: {e}"))?;
            return Ok((first, last));
        }
        conn.recv(cx, 64 << 10, &mut into).map_err(|e| format!("harness: the server's receive: {e}"))?;
    }
    Err("harness: a request was not whole after eight receives".to_owned())
}

/// The answer `server` gives to a request of `first..=last`.
fn answer(server: Server, first: u64, last: u64) -> Vec<u8> {
    let ranged = |status: &str, from: u64, to: u64, length: u64, body: &[u8]| {
        let mut wire = format!(
            "HTTP/1.1 {status}\r\ncontent-range: bytes {from}-{to}/{OBJECT}\r\ncontent-length: {length}\r\n\r\n"
        )
        .into_bytes();
        wire.extend_from_slice(body);
        wire
    };
    let len = last - first + 1;
    match server {
        Server::Honest => ranged("206 Partial", first, last, len, &bytes(first, last)),
        Server::Stalls => b"HTTP/1.1 206 Partial\r\ncontent-ra".to_vec(),
        Server::ClosesEarly => {
            let mut wire = ranged("206 Partial", first, last, len, &bytes(first, last));
            wire.truncate(wire.len() - (len as usize).div_ceil(2));
            wire
        }
        Server::WrongRange => ranged("206 Partial", first + 1, last + 1, len, &bytes(first + 1, last + 1)),
        Server::WrongLength => ranged("206 Partial", first, last, len - 1, &bytes(first, last - 1)),
        Server::Longer => {
            let mut wire = ranged("206 Partial", first, last, len, &bytes(first, last));
            wire.extend_from_slice(b"and more");
            wire
        }
        Server::Redirects => format!("HTTP/1.1 302 Found\r\nlocation: http://{AT}/elsewhere\r\ncontent-length: 0\r\n\r\n").into_bytes(),
        Server::Missing => b"HTTP/1.1 404 Not Found\r\ncontent-length: 7\r\n\r\nmissing".to_vec(),
    }
}

/// The request on `conn` answered as `plan.server` does, in the plan's pieces.
fn serve(cx: &Cx, conn: &Conn, plan: &Plan) -> Result<(), String> {
    let (first, last) = asked(cx, conn)?;
    if (first, last) != (plan.first, plan.last) {
        return Err(format!("harness: the client asked {first}-{last}, not the plan's {}-{}", plan.first, plan.last));
    }
    let wire = answer(plan.server, first, last);
    let mut cuts: Vec<usize> = plan.cuts.iter().map(|&cut| (cut as usize * wire.len()) / 1000).collect();
    cuts.sort_unstable();
    let mut from = 0;
    for to in cuts.into_iter().chain([wire.len()]) {
        if to > from {
            conn.send_all(cx, &wire[from..to]).map_err(|e| format!("harness: the server's send: {e}"))?;
            from = to;
        }
    }
    Ok(())
}

/// The oracle: what the client must have said to `plan.server`.
fn judge(plan: &Plan, answered: &Result<Response, HttpError>, got: &[u8]) -> Result<(), String> {
    let refused = |want: fn(&HttpKind) -> bool| match answered {
        Err(error) if want(&error.kind) && got.is_empty() && error.to_string().contains(AT) => Ok(()),
        _ => Err(format!("the {:?} server: the client said {answered:?} with {} bytes", plan.server, got.len())),
    };
    fn why(kind: &HttpKind) -> Option<Why> {
        match kind {
            HttpKind::Protocol(error) => Some(error.why),
            _ => None,
        }
    }
    match plan.server {
        Server::Honest => {
            let range = ContentRange { first: plan.first, last: plan.last, complete: Some(OBJECT) };
            let want = Response { status: 206, body: (plan.last - plan.first + 1) as usize, range: Some(range), hops: 0 };
            match answered {
                Ok(response) if *response == want && got == bytes(plan.first, plan.last) => Ok(()),
                _ => Err(format!("the honest server: the client said {answered:?} with {} bytes", got.len())),
            }
        }
        Server::Stalls => refused(|kind| matches!(kind, HttpKind::Silent { .. })),
        Server::ClosesEarly => refused(|kind| matches!(why(kind), Some(Why::Truncated { .. }))),
        Server::WrongRange | Server::WrongLength => refused(|kind| matches!(why(kind), Some(Why::RangeMismatch { .. }))),
        Server::Longer => refused(|kind| matches!(why(kind), Some(Why::AfterEnd))),
        Server::Redirects => refused(|kind| matches!(kind, HttpKind::Redirect(Why::RedirectsOver { most: 0 }))),
        Server::Missing => refused(|kind| matches!(why(kind), Some(Why::RangeStatus { status: 404 }))),
    }
}

/// The ranged GET begun, served by the plan's server, finished and judged.
fn exchange(plan: &Plan, cx: &Cx, http: &Http, listener: &mut Listener) -> Result<(), String> {
    let url = Url::parse(&format!("http://{AT}/object")).map_err(|e| format!("harness: {e}"))?;
    let request = Request { range: Some(Range { first: plan.first, last: Some(plan.last) }), ..Request::get(&url) };
    let mut into = cx.bytes();
    let exchange = http.begin(cx, &request).map_err(|e| format!("the begin: {e}"))?;
    let conn = match listener.accept(cx) {
        Ok(conn) => conn,
        Err(error) => {
            let _ = exchange.finish(cx, &mut into);
            return Err(format!("harness: accept: {error}"));
        }
    };
    let served = serve(cx, &conn, plan);
    // A stalling server holds its connection open while the client waits; the others close.
    let mut open = Some(conn);
    let closed = match plan.server {
        Server::Stalls => Ok(()),
        _ => open.take().map_or(Ok(()), |conn| conn.finish(cx)),
    };
    let finished = exchange.finish(cx, &mut into);
    let held = open.map_or(Ok(()), |conn| conn.finish(cx));
    served?;
    closed.and(held).map_err(|e| format!("harness: the server's finish: {e}"))?;
    let answered = match finished {
        Ok(Finished::Answered(response)) => Ok(response),
        Ok(Finished::Redirected(next)) => {
            let _ = next.finish(cx, &mut into);
            return Err("a redirect was followed under NO_REDIRECTS".to_owned());
        }
        Err(error) => Err(error),
    };
    judge(plan, &answered, into.as_slice())
}

fn run(plan: &Plan) -> Result<(), String> {
    let io = world(plan.seed, Plant::None);
    let cx = root(&io);
    let net = io.need_net().map_err(|e| format!("harness: {e}"))?;
    let clock = io.need_clock().map_err(|e| format!("harness: {e}"))?;
    let http = Http::over(&net, &clock, &Roots::none(), SHORT);
    let mut listener = match net.listen(&cx, &Addr::Tcp(HostPort::parse(AT).expect("a test's address"))) {
        Ok(listener) => listener,
        Err(error) => return Err(format!("harness: {error}")),
    };
    let judged = exchange(plan, &cx, &http, &mut listener);
    match (judged, listener.close(&cx)) {
        (Err(why), _) => Err(why),
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) => Err(format!("harness: close: {error}")),
    }
}

/// The invariant: a ranged HTTP GET validates `Content-Range` and length, returning only the exact byte range requested or a typed refusal without forwarding unverified bytes.
/// Every server: the honest one's bytes are exactly the range asked; each other one is refused
/// by the name of what it did, its bytes not handed on.
#[test]
fn a_ranged_get_takes_only_the_bytes_asked() {
    blaze_sim::check(Config::cases(200), |tape| check(plan(tape), |plan, _| run(plan)));
}

/// The `Content-Range` check skipped: a server answering another range is believed, and the
/// oracle sees it.
#[test]
fn the_oracle_catches_a_range_unchecked() {
    blaze_sim::control(Config::cases(200), false, true, "", |tape, planted| {
        let plan = plan(tape);
        plant::with(planted, || run(&plan)).map_err(|why| Broke::Oracle(format!("{why}\nplan {plan:?}")))
    });
}
