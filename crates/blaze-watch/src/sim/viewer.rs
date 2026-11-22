//! The viewer core against pages and runs: a slow page against a fast run (the queue bound),
//! requests from every kind of page (the refusals and what they reveal), toggles from paired
//! and unpaired pages, and the expiry rule over schedules of runs and streams.

use std::alloc::Global;
use std::collections::BTreeMap;

use blaze_own::Vec as Own;
use blaze_sim::{Broke, Config, Tape, check, control};

use super::{Fixed, Lap, base64url, harness, key, oracle};
use crate::conn::Credit;
use crate::feed::Item;
use crate::frame::{self, Frame, Role};
use crate::http;
use crate::pairing::Key;
use crate::ring::Event as RingEvent;
use crate::run_wire::{self, Entry, RunName};
use crate::viewer::{self as v, Ask, Effect, Event, History, HistoryError, LocalId, Out, PageId, Past, Query, Viewer};
use crate::{Nanos, Plant, Seq};

const KEY: [u8; 32] = [7; 32];
const UID: u32 = 501;
const RUN_EPOCH: u64 = 99;

/// The viewer's config for a case: the shipped one, its window cut to 64 KiB and 1024 items.
/// The shipped window (4 MiB, zeroed when made, window.rs `Window::new_in`) costs every case a
/// 4 MiB fill per viewer (`requests` makes two); the three tests of this file that ran past
/// 60 s in a 228 s suite (wave-1119 t60 watch.txt) make thousands of them, unmeasured per test.
/// No property here but [`flow`]'s reads the window, and it sets its own.
fn sim_config(script: &'static [u8]) -> v::Config {
    v::Config { window_bytes: 64 << 10, window_items: 1024, ..v::Config::new(UID, script) }
}

struct NoHistory;

impl History for NoHistory {
    fn query<A: core::alloc::Allocator>(&mut self, _: &Query, _: &mut Own<Past, A>) -> Result<(), HistoryError> {
        Ok(())
    }
}

fn entry(pid: u32) -> Entry {
    Entry { name: RunName { pid, start: 1000 + u64::from(pid) }, checkout: key(1), program: key(2), began: 5 }
}

fn host() -> String {
    format!("127.0.0.1:{}", v::DEFAULT_PORT)
}

/// A request's bytes: the line, the headers given, and a body with its length.
fn request(method: &str, path: &str, headers: &[(&str, String)], body: &[u8]) -> std::vec::Vec<u8> {
    let mut out = format!("{method} {path} HTTP/1.1\r\n");
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    if !body.is_empty() {
        out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn paired_headers() -> std::vec::Vec<(&'static str, String)> {
    vec![
        ("Host", host()),
        ("Origin", v::ORIGIN.to_string()),
        ("Authorization", format!("Bearer {}", base64url(&KEY))),
    ]
}

/// What the effects of one step gave each page, and the rest.
#[derive(Default)]
struct Seen {
    pages: BTreeMap<u64, std::vec::Vec<u8>>,
    closed: std::vec::Vec<u64>,
    to_run: std::vec::Vec<u8>,
    toggles: std::vec::Vec<std::vec::Vec<u8>>,
    stored: bool,
    exit: bool,
    local: std::vec::Vec<std::vec::Vec<u8>>,
}

fn take(out: &mut Out<Global>, seen: &mut Seen) {
    for effect in out.effects.iter() {
        match *effect {
            Effect::ToPage { page, span } => seen.pages.entry(page.0).or_default().extend_from_slice(out.get(span)),
            Effect::ClosePage { page } => seen.closed.push(page.0),
            Effect::ToRun { span, .. } => seen.to_run.extend_from_slice(out.get(span)),
            Effect::Toggle { span, .. } => seen.toggles.push(out.get(span).to_vec()),
            Effect::StoreKeys { .. } => seen.stored = true,
            Effect::Exit => seen.exit = true,
            Effect::ToLocal { span, .. } => seen.local.push(out.get(span).to_vec()),
            Effect::Connect { .. } | Effect::Unlink { .. } => {}
        }
    }
    out.clear();
}

fn status(response: &[u8]) -> u16 {
    // 0 for an answer that is not an HTTP/1.1 status line: no status the model expects.
    response.get(9..12).and_then(|s| std::str::from_utf8(s).ok()).and_then(|s| s.parse().ok()).unwrap_or(0)
}

struct Rig {
    viewer: Viewer<Global>,
    out: Out<Global>,
    seen: Seen,
}

impl Rig {
    fn new(config: v::Config, random: u8, plant: Plant) -> Result<Rig, Broke> {
        let mut viewer = Viewer::new_in(config, &[Key(KEY)], Nanos(0), &mut Fixed(random), Global).map_err(harness)?;
        viewer.plant = plant;
        Ok(Rig { viewer, out: Out::new_in(Global), seen: Seen::default() })
    }

    fn step(&mut self, event: Event<'_>) -> Result<(), Broke> {
        self.viewer.step(event, &mut Fixed(3), &mut NoHistory, &mut self.out).map_err(harness)?;
        take(&mut self.out, &mut self.seen);
        Ok(())
    }

    /// A run listed and its link open.
    fn with_run(&mut self, pid: u32, now: Nanos) -> Result<RunName, Broke> {
        let live = [entry(pid)];
        self.step(Event::Listed { now, live: &live })?;
        let run = live[0].name;
        self.step(Event::RunFrame { run, frame: Frame::Hello { role: Role::Run, epoch: RUN_EPOCH }, now })?;
        Ok(run)
    }

    /// A page connection that sends `bytes`; its answer so far.
    fn ask(&mut self, page: u64, bytes: &[u8], now: Nanos) -> Result<std::vec::Vec<u8>, Broke> {
        self.step(Event::PageOpened { page: PageId(page), now })?;
        self.step(Event::PageBytes { page: PageId(page), bytes, now })?;
        Ok(self.seen.pages.remove(&page).unwrap_or_default())
    }
}

// ------------------------------------------------------------------ a slow page

fn flow(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut config = v::Config::new(UID, b"");
    config.page_queue = tape.range(1024..16_384, "page queue") as usize;
    config.window_bytes = 64 << 10;
    config.window_items = 1024;
    let bound = config.page_queue + http::sse_event_most(5, v::MOST_ITEM) + 8;
    let mut rig = Rig::new(config, 1, plant)?;
    let mut now = Nanos(0);
    let run = rig.with_run(10, now)?;
    let mut credit = Credit::new();
    rig.seen.to_run.clear();
    rig.ask(1, &request("GET", "/v1/events", &paired_headers(), b""), now)?;
    let page = PageId(1);
    let mut seq = 0u64;
    let mut item = Own::new_in(Global);
    for _ in 0..tape.range(1..600, "steps") {
        match tape.below(4, "step") {
            0 | 1 => {
                // The run sends an item, if its credit lets it.
                seq += 1;
                item.clear();
                let event = RingEvent { kind: 1, flags: 0, detail: 0, at: seq, value: seq, key: key(seq) };
                Item::Event { lane: 0, n: seq, event }.write(&mut item);
                let len = Frame::data_len(frame::EVENTS, Seq(seq), item.len());
                if !credit.fits(len) {
                    seq -= 1;
                } else {
                    credit.spend(frame::EVENTS, len).map_err(harness)?;
                    let frame = Frame::Data { chan: frame::EVENTS, seq: Seq(seq), bytes: &item };
                    rig.step(Event::RunFrame { run, frame, now })?;
                }
            }
            2 => {
                // The page's socket takes some of what it was given.
                let queued = rig.viewer.queued(page).unwrap_or(0);
                let wrote = tape.below(queued as u64 + 1, "bytes written") as usize;
                rig.step(Event::PageWrote { page, bytes: wrote, now })?;
            }
            _ => {
                now = now.after(Nanos::secs(tape.below(20, "seconds")));
                rig.step(Event::Tick { now })?;
            }
        }
        // Grants the viewer sent the run.
        let mut at = 0;
        while let Some((frame, used)) = frame::read(&rig.seen.to_run[at..]).map_err(harness)? {
            if let Frame::Credit { chan, bytes } = frame {
                credit.grant(chan, bytes).map_err(harness)?;
            }
            at += used;
        }
        rig.seen.to_run.drain(..at);
        rig.seen.pages.clear();
        let queued = rig.viewer.queued(page).unwrap_or(0);
        oracle(queued <= bound, || {
            format!("page queue: {queued} bytes given to a slow page and unwritten, past the bound {bound}")
        })?;
        oracle(rig.viewer.window().footprint() <= (64 << 10) + 1024 * 64, || {
            format!("the window takes {} bytes", rig.viewer.window().footprint())
        })?;
    }
    Ok(())
}

/// The invariant: the viewer bounds slow-page queues, checks origin and pairing keys before routing or toggling, and expires idle viewers without holding resources forever.
#[test]
fn a_slow_page_never_grows_the_viewer() {
    let lap = Lap::new("viewer: a slow page");
    check(Config::cases(1500), |tape| lap.case(flow(tape, Plant::None)));
}

#[test]
fn the_oracle_catches_a_page_queue_past_its_bound() {
    let lap = Lap::new("viewer: control, a page queue past its bound");
    control(Config::cases(1500), Plant::None, Plant::IgnorePageQueue, "page queue", |tape, plant| lap.case(flow(tape, plant)));
}

// ------------------------------------------------------------------ requests

const PATHS: [&str; 8] =
    ["/v1/runs", "/v1/events", "/v1/history", "/v1/toggle", "/v1/pair", "/v1/pair/00112233445566778899aabbccddeeff", v::SCRIPT, "/nope"];

/// What the model says a request must be answered, when it is refused; `None` when it is let
/// through (what it then gets is another test's).
fn expected(tape: &Tape) -> (std::vec::Vec<u8>, Option<u16>, &'static str) {
    let method = *tape.one_of(&["GET", "POST", "OPTIONS", "DELETE"], "method");
    let path = *tape.one_of(&PATHS, "path");
    let host_good = format!("127.0.0.1:{}", v::DEFAULT_PORT);
    let host = *tape.one_of(&[0u8, 1, 2, 3], "host");
    let origin = *tape.one_of(&[0u8, 1, 2, 3], "origin");
    let auth = *tape.one_of(&[0u8, 1, 2, 3], "authorization");
    let abuse = tape.below(8, "abuse");
    let mut headers: std::vec::Vec<(&str, String)> = std::vec::Vec::new();
    match host {
        0 => headers.push(("Host", host_good.clone())),
        1 => headers.push(("Host", format!("evil.example:{}", v::DEFAULT_PORT))),
        2 => headers.push(("Host", "127.0.0.1:1".to_string())),
        _ => {}
    }
    match origin {
        0 => headers.push(("Origin", v::ORIGIN.to_string())),
        1 => headers.push(("Origin", "https://evil.example".to_string())),
        2 => headers.push(("Origin", "null".to_string())),
        _ => {}
    }
    match auth {
        0 => headers.push(("Authorization", format!("Bearer {}", base64url(&KEY)))),
        1 => headers.push(("Authorization", format!("Bearer {}", base64url(&[8; 32])))),
        2 => headers.push(("Authorization", "Bearer !!".to_string())),
        _ => {}
    }
    if method == "OPTIONS" {
        headers.push(("Access-Control-Request-Method", "GET".to_string()));
    }
    let mut body: std::vec::Vec<u8> = std::vec::Vec::new();
    if method == "POST" && tape.coin("a body") {
        body = tape.bytes(1..=64, "body");
    }
    let refused = match abuse {
        0 => {
            headers.push(("X-Long", "a".repeat(http::MOST_LINE)));
            Some(431)
        }
        1 => {
            // One past the limit whatever else was drawn: with no Host, Origin or
            // Authorization, `MOST_HEADERS` lines alone are within it (`http::parse` refuses
            // only `count > MOST_HEADERS`).
            for _ in 0..=http::MOST_HEADERS {
                headers.push(("X-Many", "b".to_string()));
            }
            Some(431)
        }
        2 => {
            headers.push(("Content-Length", (http::MOST_BODY + 1).to_string()));
            body.clear();
            Some(413)
        }
        3 => {
            headers.push(("Transfer-Encoding", "chunked".to_string()));
            Some(400)
        }
        4 => {
            headers.push(("Host", host_good.clone()));
            headers.push(("Host", host_good));
            Some(400)
        }
        _ => None,
    };
    // The parser reads the request line before any header.
    let refused = match method == "DELETE" {
        true => Some(405),
        false => refused,
    };
    let refused = refused.or(match () {
        _ if host != 0 => Some(421),
        _ if method == "GET" && path == v::SCRIPT => None,
        _ if origin != 0 => Some(403),
        _ if method == "OPTIONS" => None,
        _ if (method, path) == ("POST", "/v1/pair") || (method == "GET" && path.starts_with("/v1/pair/")) => None,
        _ if auth != 0 => Some(401),
        _ => None,
    });
    (request(method, path, &headers, &body), refused, path)
}

fn requests(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    // Two viewers in different states: one fresh, one with a run, a pairing waiting and
    // another epoch. A refused page must not be able to tell them apart.
    let mut fresh = Rig::new(sim_config(b"script"), 1, plant)?;
    let mut busy = Rig::new(sim_config(b"script"), 2, plant)?;
    busy.with_run(77, Nanos(0))?;
    busy.ask(900, &request("POST", "/v1/pair", &paired_headers()[..2], b""), Nanos(0))?;
    busy.step(Event::Local { conn: LocalId(1), uid: UID, ask: Ask::Pending, now: Nanos(0) })?;
    for page in 0..tape.range(1..8, "requests") {
        let (bytes, refused, path) = expected(tape);
        let a = fresh.ask(page, &bytes, Nanos(1))?;
        let b = busy.ask(page, &bytes, Nanos(1))?;
        let Some(want) = refused else { continue };
        let got = status(&b);
        oracle(got == want, || {
            format!("answered {got} where {want}: {}", String::from_utf8_lossy(&bytes[..bytes.len().min(160)]))
        })?;
        oracle(a == b, || format!("a refused page learned the viewer's state from a {want} for {path}"))?;
        let text = String::from_utf8_lossy(&b);
        oracle(want != 403 || !text.contains("Access-Control-Allow-Origin"), || {
            "answered a page of another origin with CORS headers it can read".to_string()
        })?;
        oracle(text.ends_with("\r\n\r\n"), || format!("a {want} carried a body"))?;
        oracle(busy.seen.toggles.is_empty() && !busy.seen.stored, || {
            format!("a {want} for {path} still acted")
        })?;
    }
    Ok(())
}

#[test]
fn refused_requests_get_their_status_and_learn_nothing() {
    let lap = Lap::new("viewer: refused requests");
    check(Config::cases(3000), |tape| lap.case(requests(tape, Plant::None)));
}

#[test]
fn the_oracle_catches_a_route_before_the_origin_check() {
    let lap = Lap::new("viewer: control, a route before the origin check");
    control(Config::cases(3000), Plant::None, Plant::RouteBeforeOrigin, "answered", |tape, plant| lap.case(requests(tape, plant)));
}

// ------------------------------------------------------------------ toggles

fn toggles(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut rig = Rig::new(sim_config(b""), 1, plant)?;
    let run = rig.with_run(5, Nanos(0))?;
    for page in 0..tape.range(1..6, "toggles") {
        let slots: std::vec::Vec<u32> = (0..tape.below(5, "slots")).map(|s| s as u32 * 3).collect();
        let hooks = tape.below(8, "hooks") as u16;
        let on = tape.coin("on");
        let mut body = Own::new_in(Global);
        crate::cbor::array(&mut body, 6);
        crate::cbor::uint(&mut body, 1);
        crate::cbor::uint(&mut body, u64::from(run.pid));
        crate::cbor::uint(&mut body, run.start);
        crate::cbor::uint(&mut body, u64::from(hooks));
        crate::cbor::boolean(&mut body, on);
        crate::cbor::array(&mut body, slots.len() as u64);
        for slot in &slots {
            crate::cbor::uint(&mut body, u64::from(*slot));
        }
        let paired = tape.below(3, "who asks");
        let mut headers = paired_headers();
        match paired {
            0 => {}
            1 => headers[2].1 = format!("Bearer {}", base64url(&[9; 32])),
            _ => {
                headers.pop();
            }
        }
        let answer = rig.ask(page, &request("POST", "/v1/toggle", &headers, &body), Nanos(1))?;
        let toggles = std::mem::take(&mut rig.seen.toggles);
        if paired != 0 {
            oracle(toggles.is_empty(), || "forwarded a toggle from an unpaired page".to_string())?;
            oracle(status(&answer) == 401, || format!("an unpaired toggle was answered {}", status(&answer)))?;
            continue;
        }
        oracle(status(&answer) == 202, || format!("a paired toggle was answered {}", status(&answer)))?;
        let mut want = Own::new_in(Global);
        let mut request = Own::new_in(Global);
        run_wire::toggle(&slots, hooks, on, &mut request);
        Frame::Request { bytes: &request }.write(&mut want);
        oracle(toggles.len() == 1 && toggles[0][..] == want[..], || {
            format!("a paired toggle reached the run as {toggles:?}")
        })?;
    }
    Ok(())
}

#[test]
fn only_a_paired_page_toggles() {
    let lap = Lap::new("viewer: only a paired page toggles");
    check(Config::cases(1000), |tape| lap.case(toggles(tape, Plant::None)));
}

#[test]
fn the_oracle_catches_a_toggle_before_the_key_check() {
    let lap = Lap::new("viewer: control, a toggle before the key check");
    control(Config::cases(1000), Plant::None, Plant::ToggleBeforeKey, "unpaired page", |tape, plant| lap.case(toggles(tape, plant)));
}

// ------------------------------------------------------------------ expiry

fn expiry(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let config = sim_config(b"");
    let mut rig = Rig::new(config, 1, plant)?;
    let mut now = Nanos(0);
    // The rule, restated: empty since, streams open, the last stream's close.
    let mut empty_since = Some(now);
    let mut streams: std::vec::Vec<u64> = std::vec::Vec::new();
    let mut last_close = now;
    let mut next_page = 0u64;
    for _ in 0..tape.range(1..300, "steps") {
        now = now.after(Nanos::millis(tape.below(40_000, "milliseconds")));
        match tape.below(5, "step") {
            0 => {
                let live: std::vec::Vec<Entry> = match tape.coin("a run is live") {
                    true => vec![entry(1)],
                    false => vec![],
                };
                rig.step(Event::Listed { now, live: &live })?;
                match (live.is_empty(), empty_since) {
                    (true, None) => empty_since = Some(now),
                    (false, _) => empty_since = None,
                    _ => {}
                }
            }
            1 if streams.len() < 8 => {
                next_page += 1;
                rig.ask(next_page, &request("GET", "/v1/events", &paired_headers(), b""), now)?;
                streams.push(next_page);
            }
            2 if !streams.is_empty() => {
                let page = streams.remove(tape.index(streams.len(), "which stream closes"));
                rig.step(Event::PageClosed { page: PageId(page), now })?;
                last_close = now;
            }
            _ => {
                rig.step(Event::Tick { now })?;
                let due = empty_since.is_some_and(|since| {
                    now >= since.after(config.held_most)
                        || (streams.is_empty() && now >= since.after(config.idle) && now >= last_close.after(config.grace))
                });
                let exited = rig.seen.exit;
                oracle(!exited || due, || format!("expired early, at {}s", now.0 / 1_000_000_000))?;
                oracle(exited || !due, || {
                    format!("outlived the rule: still up at {}s with {} streams open", now.0 / 1_000_000_000, streams.len())
                })?;
                if exited {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

#[test]
fn the_viewer_expires_by_its_rule() {
    let lap = Lap::new("viewer: expiry by its rule");
    check(Config::cases(2000), |tape| lap.case(expiry(tape, Plant::None)));
}

#[test]
fn the_oracle_catches_a_page_that_holds_the_viewer_forever() {
    let lap = Lap::new("viewer: control, a page that holds the viewer forever");
    control(Config::cases(3000), Plant::None, Plant::PageHoldsForever, "outlived", |tape, plant| lap.case(expiry(tape, plant)));
}

// ------------------------------------------------------------------ pairing

#[test]
fn a_browser_pairs_once_through_a_local_confirm() {
    let mut rig = Rig::new(sim_config(b""), 1, Plant::None).expect("a viewer");
    let headers = &paired_headers()[..2];
    let answer = rig.ask(1, &request("POST", "/v1/pair", headers, b""), Nanos(0)).expect("pair");
    assert_eq!(status(&answer), 200);
    let body = &answer[answer.windows(4).position(|w| w == b"\r\n\r\n").expect("a head") + 4..];
    let mut r = crate::cbor::Reader::new(body);
    r.array_of(3).expect("[1, ticket, code]");
    assert_eq!(r.uint().expect("version"), 1);
    let ticket: [u8; 16] = r.fixed().expect("ticket");
    let code: [u8; 8] = r.text(8, "code").expect("code").as_bytes().try_into().expect("8 bytes");
    let hex: String = ticket.iter().map(|b| format!("{b:02x}")).collect();
    let poll = format!("/v1/pair/{hex}");
    assert_eq!(status(&rig.ask(2, &request("GET", &poll, headers, b""), Nanos(1)).expect("poll")), 200);
    // Another user's command cannot confirm.
    rig.step(Event::Local { conn: LocalId(1), uid: UID + 1, ask: Ask::Confirm(crate::pairing::Code(code)), now: Nanos(2) })
        .expect("local");
    assert!(!rig.seen.stored);
    rig.step(Event::Local { conn: LocalId(2), uid: UID, ask: Ask::Confirm(crate::pairing::Code(code)), now: Nanos(2) })
        .expect("local");
    assert!(rig.seen.stored);
    let given = rig.ask(3, &request("GET", &poll, headers, b""), Nanos(3)).expect("poll");
    assert_eq!(status(&given), 200);
    // Given once.
    assert_eq!(status(&rig.ask(4, &request("GET", &poll, headers, b""), Nanos(4)).expect("poll")), 404);
}
