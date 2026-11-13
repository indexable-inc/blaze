//! One `Http`, whichever provider: [`Http::handler`] over a function and [`Http::over`] a
//! `Net` whose peer runs the same function behind HTTP/1.1 give the same answer to the same
//! request (status and body), whatever the method, the body, the answer's size and the way
//! its body is framed (a length, chunks, or the close). And narrowed to one origin, both
//! refuse a request to the other, the same way.
//!
//! The server is the test's: it accepts on the simulation's sockets, reads the request the
//! driver sent, runs the application and writes the answer. So what is compared went through
//! blaze-http's h1 core in both directions.
//!
//! Plant: [`Plant::HttpOnlyIgnored`], an `only` that hands back the capability it was given.

use hegel::TestCase;

use super::{Draw, Drawn, Seeds, check, control, root, world};
use crate::{
    Cx, Plant, Stopped,
    mem::Bytes,
    net::{Addr, Conn, HostPort, Listener},
    requests::{Http, HttpError, HttpKind, Method, Request, Response, Wire},
    roots::Roots,
    url::Url,
};

/// The two origins' addresses.
const AT: [&str; 2] = ["127.0.0.1:8001", "127.0.0.2:8002"];

const TARGETS: [&str; 4] = ["/ok", "/missing", "/big?x=1", "/echo"];

const METHODS: [Method; 4] = [Method::Get, Method::Post, Method::Put, Method::Delete];

/// How the tests' `Http::over` talks: every field said, as its caller must.
const WIRE: Wire = Wire { user_agent: "bstd-test", body_most: 1 << 20, reads_most: 1 << 10 };

/// How the test's server frames an answer's body.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Framing {
    Length,
    Chunked,
    Close,
}

#[derive(Clone, Copy, Debug)]
struct Ask {
    origin: usize,
    target: usize,
    method: usize,
    body: usize,
    framing: Framing,
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    /// The origin both capabilities are narrowed to, if they are.
    only: Option<usize>,
    asks: Vec<Ask>,
}

fn plan(d: &mut impl Draw) -> Plan {
    let seed = d.int(0, u64::MAX);
    let only = d.coin().then(|| d.int(0, 1) as usize);
    let asks = (0..d.int(1, 8))
        .map(|_| Ask {
            origin: d.int(0, 1) as usize,
            target: d.int(0, 3) as usize,
            method: d.int(0, 3) as usize,
            body: d.int(0, 300) as usize,
            framing: d.pick(&[Framing::Length, Framing::Chunked, Framing::Close]),
        })
        .collect();
    Plan { seed, only, asks }
}

/// The application both providers answer with: a status, and a body added to `into`.
fn application(request: &Request<'_>, into: &mut Bytes<'_>) -> Result<u16, Stopped> {
    let target = request.url.target();
    if target == "/ok" {
        into.extend(b"fine")?;
        Ok(200)
    } else if target.starts_with("/big") {
        // More than one receive's worth.
        for at in 0..40_000u32 {
            into.extend(&[(at % 251) as u8])?;
        }
        Ok(200)
    } else if target == "/echo" {
        into.extend(format!("{:?} of {} bytes:", request.method, request.body.len()).as_bytes())?;
        into.extend(request.body)?;
        Ok(201)
    } else {
        into.extend(b"no such thing")?;
        Ok(404)
    }
}

fn url(ask: &Ask) -> Url {
    Url::parse(&format!("http://{}{}", AT[ask.origin], TARGETS[ask.target])).expect("a test's URL")
}

fn addr(origin: usize) -> Addr {
    Addr::Tcp(HostPort::parse(AT[origin]).expect("a test's address"))
}

/// The request `conn`'s peer sent, read whole and parsed: its method, target and body.
fn read_request(cx: &Cx, conn: &Conn) -> Result<(Method, String, Vec<u8>), String> {
    let mut into = cx.bytes();
    // Bounded: a request here is far under eight receives.
    for _ in 0..8 {
        let bytes = into.as_slice();
        if let Some(head_end) = bytes.windows(4).position(|it| it == b"\r\n\r\n") {
            let head = std::str::from_utf8(&bytes[..head_end]).map_err(|e| format!("harness: a request's head: {e}"))?;
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .map_or(Ok(0), str::parse::<usize>)
                .map_err(|e| format!("harness: a request's content-length: {e}"))?;
            let body = &bytes[head_end + 4..];
            if body.len() >= length {
                let mut line = head.lines().next().unwrap_or("").split(' ');
                let method = match line.next() {
                    Some("GET") => Method::Get,
                    Some("POST") => Method::Post,
                    Some("PUT") => Method::Put,
                    Some("DELETE") => Method::Delete,
                    other => return Err(format!("harness: a request's method {other:?}")),
                };
                let target = line.next().ok_or("harness: a request with no target")?.to_owned();
                return Ok((method, target, body[..length].to_vec()));
            }
        }
        conn.recv(cx, 64 << 10, &mut into).map_err(|e| format!("harness: the server's receive: {e}"))?;
    }
    Err("harness: a request was not whole after eight receives".to_owned())
}

/// The request on `conn` answered by [`application`], its body framed as `framing` says.
fn answer(cx: &Cx, conn: &Conn, origin: usize, framing: Framing) -> Result<(), String> {
    let (method, target, body) = read_request(cx, conn)?;
    let url = Url::parse(&format!("http://{}{target}", AT[origin])).map_err(|e| format!("harness: {e}"))?;
    let mut made = cx.bytes();
    let request = Request { method, url: &url, headers: &[], body: &body };
    let status = application(&request, &mut made).map_err(|e| format!("harness: {e}"))?;
    let made = made.as_slice();
    let mut wire = format!("HTTP/1.1 {status} Said\r\nserver: test\r\n").into_bytes();
    match framing {
        Framing::Length => {
            wire.extend_from_slice(format!("content-length: {}\r\n\r\n", made.len()).as_bytes());
            wire.extend_from_slice(made);
        }
        Framing::Chunked => {
            wire.extend_from_slice(b"transfer-encoding: chunked\r\n\r\n");
            // Chunks of a size that is no divisor of any body's here.
            for chunk in made.chunks(1009) {
                wire.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
                wire.extend_from_slice(chunk);
                wire.extend_from_slice(b"\r\n");
            }
            wire.extend_from_slice(b"0\r\n\r\n");
        }
        Framing::Close => {
            wire.extend_from_slice(b"connection: close\r\n\r\n");
            wire.extend_from_slice(made);
        }
    }
    conn.send_all(cx, &wire).map_err(|e| format!("harness: the server's send: {e}"))
}

/// One connection accepted by `listener` and served.
fn serve(cx: &Cx, listener: &mut Listener, origin: usize, framing: Framing) -> Result<(), String> {
    // The connection is linear: finished whatever its answer did, which is also what ends a
    // body framed by the close.
    let conn = match listener.accept(cx) {
        Ok(conn) => conn,
        Err(error) => return Err(format!("harness: accept: {error}")),
    };
    let answered = answer(cx, &conn, origin, framing);
    let finished = conn.finish(cx);
    match (answered, finished) {
        (Err(why), _) => Err(why),
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) => Err(format!("harness: the server's finish: {error}")),
    }
}

/// `ask` made of both providers, and their answers compared.
fn asked(
    plan: &Plan,
    cx: &Cx,
    fake: &Http,
    real: &Http,
    listener: &mut Listener,
    at: usize,
    ask: &Ask,
) -> Result<(), String> {
    let url = url(ask);
    let body = vec![ask.body as u8; ask.body];
    let request = Request { method: METHODS[ask.method], url: &url, headers: &[], body: &body };
    let (mut faked, mut made) = (cx.bytes(), cx.bytes());
    let fake: Result<Response, HttpError> = fake.request(cx, &request, &mut faked);
    // The exchange is linear: once begun it is served by the test's server, then finished.
    let real: Result<Response, HttpError> = match real.begin(cx, &request) {
        Err(error) => Err(error),
        Ok(exchange) => {
            let served = serve(cx, listener, ask.origin, ask.framing);
            let answered = exchange.finish(cx, &mut made);
            if let Err(why) = served {
                return Err(why);
            }
            answered
        }
    };
    let allowed = plan.only.is_none_or(|only| only == ask.origin);
    match (&fake, &real) {
        (Ok(fake), Ok(real)) if allowed && fake == real && faked.as_slice() == made.as_slice() => Ok(()),
        (Err(fake), Err(real)) if !allowed && fake == real && fake.kind == HttpKind::NotAllowed => Ok(()),
        _ => Err(format!(
            "ask {at} ({ask:?}, allowed: {allowed}): the handler gave {fake:?} with {} bytes and the Net {real:?} with {} bytes",
            faked.len(),
            made.len()
        )),
    }
}

/// The plan's asks, with a listener at each origin.
fn asks(plan: &Plan, cx: &Cx, fake: &Http, real: &Http, first: &mut Listener, second: &mut Listener) -> Result<(), String> {
    for (at, ask) in plan.asks.iter().enumerate() {
        let listener = if ask.origin == 0 { &mut *first } else { &mut *second };
        asked(plan, cx, fake, real, listener, at, ask)?;
    }
    Ok(())
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let io = world(plan.seed, plant);
    let cx = root(&io);
    let net = io.need_net().map_err(|e| format!("harness: {e}"))?;
    let clock = io.need_clock().map_err(|e| format!("harness: {e}"))?;
    let fake = Http::handler(|_: &Cx, request: &Request<'_>, into: &mut Bytes<'_>| application(request, into)).expect("room for a handler");
    let real = Http::over(&net, &clock, &Roots::none(), WIRE);
    let (fake, real) = match plan.only {
        Some(only) => {
            let url = url(&Ask { origin: only, target: 0, method: 0, body: 0, framing: Framing::Length });
            let origin = url.origin().ok_or("harness: a test's URL has an origin")?;
            (fake.only(origin), real.only(origin))
        }
        None => (fake, real),
    };
    // The listeners are linear: each is closed whatever the asks found.
    let mut first = match net.listen(&cx, &addr(0)) {
        Ok(listener) => listener,
        Err(error) => return Err(format!("harness: {error}")),
    };
    let asked = match net.listen(&cx, &addr(1)) {
        Ok(mut second) => {
            let asked = asks(plan, &cx, &fake, &real, &mut first, &mut second);
            match (asked, second.close(&cx)) {
                (Err(why), _) => Err(why),
                (Ok(()), Ok(())) => Ok(()),
                (Ok(()), Err(error)) => Err(format!("harness: close: {error}")),
            }
        }
        Err(error) => Err(format!("harness: {error}")),
    };
    match (asked, first.close(&cx)) {
        (Err(why), _) => Err(why),
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) => Err(format!("harness: close: {error}")),
    }
}

/// The refinement: `Http::handler` over a function and `Http::over` a `Net` whose peer runs the
/// same function behind HTTP/1.1 give the same answer to the same request (status and body),
/// and narrowed to one origin both refuse a request to the other, the same way.
#[hegel::test(test_cases = 100)]
fn a_handler_and_a_net_answer_alike(tc: TestCase) {
    check(plan(&mut Drawn(&tc)), run);
}

#[test]
fn the_oracle_catches_an_only_that_is_ignored() {
    control(Plant::HttpOnlyIgnored, |seeds: &mut Seeds| plan(seeds), run);
}

#[test]
fn over_a_net_refuses_https_naming_what_is_missing() {
    let io = world(1, Plant::None);
    let cx = root(&io);
    let (net, clock) = (io.need_net().expect("the simulation has a net"), io.need_clock().expect("and a clock"));
    let http = Http::over(&net, &clock, &Roots::none(), WIRE);
    let url = Url::parse("https://127.0.0.1/secure").expect("a test's URL");
    let error = http.request(&cx, &Request::get(&url), &mut cx.bytes()).expect_err("https is refused");
    assert_eq!(error.kind, HttpKind::NoTls);
    assert!(error.to_string().contains("TLS"), "{error}");
}
