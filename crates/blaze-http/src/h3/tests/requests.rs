//! Plain requests between two cores, with resumption and 0-RTT: a deterministic simulation of
//! a client endpoint and a server endpoint over a seeded network that drops, duplicates,
//! reorders and delays datagrams, on one virtual clock; and the first-flight and refused-early
//! data cases on a clean network. No socket, no thread, no sleep.
//!
//! The network is the test's own: blaze-core's `sim::Sim` models datagrams too, but no h3
//! driver runs on its effects yet, so the two endpoints are joined here directly, as the
//! WebTransport tests above join them. A seed fixes the network, the objects and the workload;
//! TLS draws its own randomness, so a replayed seed repeats the run's shape, not its bytes.

use std::{
    alloc::Global,
    collections::BTreeMap,
    net::SocketAddr,
    time::{Duration, Instant},
};

use blaze_core::sim::Rng;

use super::{
    super::{
        ClientConfig, ConnId, Early, Endpoint, Event, Identity, Length, Range, Request,
        RequestEnd, RequestId, ServerConfig, Ticket, Transport, Trust, endpoint::Plant, varint,
    },
    CLIENT, SERVER, identity,
};

/// Seeds the lossy DST runs: each draws its own objects, ranges, cancels and network.
const SEEDS: u64 = 32;
/// One datagram in this many is lost: harsh enough that most handshakes and bodies lose
/// some, mild enough that QUIC's recovery finishes well inside the deadline.
const LOSE_ONE_IN: u64 = 8;
/// One datagram in this many arrives twice.
const DUPLICATE_ONE_IN: u64 = 16;
/// The most a datagram is delayed, in milliseconds: delays drawn up to it reorder datagrams
/// sent within it of each other.
const MOST_DELAY_MS: u64 = 40;
/// Objects the server holds.
const OBJECTS: usize = 6;
/// The most bytes of an object: past one stream window for some, so a body's flow control and
/// the asker's `consumed_response` are exercised.
const MOST_OBJECT: u64 = 300 << 10;
/// Requests asked on each connection of a run.
const REQUESTS: usize = 10;
/// One request in this many asks for an object the server lacks.
const MISSING_ONE_IN: u64 = 8;
/// One request in this many is a HEAD.
const HEAD_ONE_IN: u64 = 6;
/// One GET in this many asks for the whole object, not a range.
const WHOLE_ONE_IN: u64 = 4;
/// One request in this many is cancelled, at a random point of its first
/// [`MOST_CANCEL_AFTER_MS`].
const CANCEL_ONE_IN: u64 = 5;
/// The latest a cancel comes after its request, in milliseconds: around a lossy handshake and
/// a body's flight, so cancels land before, during and after the answer.
const MOST_CANCEL_AFTER_MS: u64 = 200;
/// How long a request may take: far past what a lossy run needs, so a time-out is a hang.
const DEADLINE: Duration = Duration::from_secs(20);
/// How long past the deadlines a run goes on, so a request that never ends is seen not to.
const AFTER: Duration = Duration::from_secs(5);
/// How long a holding server keeps its requests before it closes: long enough for every
/// request to reach it, far short of their deadline.
const HOLD: Duration = Duration::from_millis(300);
/// The most turns of the simulation's loop, so a run that livelocks fails rather than hangs.
const MOST_TURNS: usize = 1_000_000;
/// The server's name, as TLS and the tickets know it.
const NAME: &str = "localhost";
const AUTHORITY: &str = "localhost:4433";

/// The network: datagrams in flight, each with when it lands.
struct Net {
    rng: Rng,
    lose_one_in: u64,
    duplicate_one_in: u64,
    most_delay_ms: u64,
    /// (lands at, sent order, to, from, bytes).
    flight: Vec<(Duration, u64, SocketAddr, SocketAddr, Vec<u8>), Global>,
    sent: u64,
}

impl Net {
    fn new(seed: u64, lossy: bool) -> Net {
        Net {
            rng: Rng::new(seed),
            lose_one_in: if lossy { LOSE_ONE_IN } else { 0 },
            duplicate_one_in: if lossy { DUPLICATE_ONE_IN } else { 0 },
            most_delay_ms: if lossy { MOST_DELAY_MS } else { 0 },
            flight: Vec::new_in(Global),
            sent: 0,
        }
    }

    fn send(&mut self, now: Duration, from: SocketAddr, to: SocketAddr, bytes: Vec<u8>) {
        if self.lose_one_in > 0 && self.rng.one_in(self.lose_one_in) {
            return;
        }
        let twice = self.duplicate_one_in > 0 && self.rng.one_in(self.duplicate_one_in);
        for _ in 0..if twice { 2 } else { 1 } {
            let delay = Duration::from_millis(self.rng.below(self.most_delay_ms + 1));
            self.flight.push((now + delay, self.sent, to, from, bytes.clone()));
            self.sent += 1;
        }
    }

    fn next_landing(&self) -> Option<Duration> {
        self.flight.iter().map(|it| it.0).min()
    }

    /// The datagrams landed by `now`, in landing order: (to, from, bytes).
    fn landed(&mut self, now: Duration) -> Vec<(SocketAddr, SocketAddr, Vec<u8>), Global> {
        self.flight.sort_by_key(|it| (it.0, it.1));
        let split = self.flight.partition_point(|it| it.0 <= now);
        let mut out = Vec::with_capacity_in(split, Global);
        out.extend(self.flight.drain(..split).map(|(_, _, to, from, bytes)| (to, from, bytes)));
        out
    }
}

/// How the test's server answers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Serving {
    /// From its objects, as asked.
    Honest,
    /// One byte past each range asked, with no content-length to give it away.
    OneByteMore,
    /// Not at all: requests are held unanswered until the server closes.
    Hold,
}

/// A request the workload asked, and what it expects.
struct Asked {
    object: String,
    range: Option<Range>,
    head: bool,
    deadline: Duration,
}

/// What the client heard of one request.
#[derive(Default)]
struct Heard {
    status: Option<u16>,
    body: Vec<u8>,
    ends: Vec<(Duration, RequestEnd)>,
}

/// The client, the server, the network and the clock.
struct World {
    epoch: Instant,
    identity: Identity,
    pin: [u8; 32],
    now: Duration,
    client: Endpoint,
    server: Endpoint,
    net: Net,
    rng: Rng,
    objects: BTreeMap<String, Vec<u8>>,
    serving: Serving,
    asked: BTreeMap<RequestId, Asked>,
    heard: BTreeMap<RequestId, Heard>,
    /// Cancels to make: when, and of which.
    cancels: Vec<(Duration, RequestId), Global>,
    /// Each request the server was handed: its method and whether it came early.
    served: Vec<(String, bool), Global>,
    tickets: Vec<Ticket, Global>,
    /// Every datagram the current client sent, and whether a server datagram had reached it
    /// by then.
    client_sent: Vec<(Vec<u8>, bool), Global>,
    heard_server: bool,
}

fn client(epoch: Instant, pin: [u8; 32]) -> Endpoint {
    let config = ClientConfig { trust: Trust::Pinned(vec![pin]), transport: Transport::default() };
    Endpoint::new(epoch, None, Some(config)).unwrap()
}

fn server(epoch: Instant, identity: Identity) -> Endpoint {
    let config = ServerConfig { identity, transport: Transport::default(), requests: true };
    Endpoint::new(epoch, Some(config), None).unwrap()
}

impl World {
    fn new(seed: u64, lossy: bool) -> World {
        let epoch = Instant::now();
        let (identity, pin) = identity();
        let mut rng = Rng::new(seed ^ 0x5eed);
        let mut objects = BTreeMap::new();
        for i in 0..OBJECTS {
            let len = 1 + rng.below(MOST_OBJECT);
            let bytes: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
            objects.insert(format!("/o/{i}"), bytes);
        }
        World {
            epoch,
            client: client(epoch, pin),
            server: server(epoch, identity.clone()),
            identity,
            pin,
            now: Duration::ZERO,
            net: Net::new(seed, lossy),
            rng,
            objects,
            serving: Serving::Honest,
            asked: BTreeMap::new(),
            heard: BTreeMap::new(),
            cancels: Vec::new_in(Global),
            served: Vec::new_in(Global),
            tickets: Vec::new_in(Global),
            client_sent: Vec::new_in(Global),
            heard_server: false,
        }
    }

    /// A fresh client, as a new process is: it knows only the tickets given it.
    fn new_client(&mut self) {
        self.client = client(self.epoch, self.pin);
        for ticket in &self.tickets {
            self.client.add_ticket(ticket).unwrap();
        }
        self.heard_server = false;
        self.client_sent.clear();
        self.served.clear();
    }

    /// Moves every datagram each side has to send onto the network, and hands each side's
    /// events to the test.
    fn exchange(&mut self) {
        while let Some(transmit) = self.client.next_transmit() {
            assert_eq!(transmit.to, SERVER, "the client sends only to its server");
            self.client_sent.push((transmit.bytes.clone(), self.heard_server));
            self.net.send(self.now, CLIENT, SERVER, transmit.bytes);
        }
        while let Some(transmit) = self.server.next_transmit() {
            self.net.send(self.now, SERVER, transmit.to, transmit.bytes);
        }
        while let Some(event) = self.server.next_event() {
            if let Event::Request { request, served } = event {
                self.served.push((served.method.clone(), served.early));
                self.answer(request, &served.method, &served.path, served.range.as_deref());
            }
        }
        while let Some(event) = self.client.next_event() {
            match event {
                Event::ResponseHead { request, head } => {
                    self.heard.entry(request).or_default().status = Some(head.status);
                }
                Event::ResponseData { request, bytes } => {
                    self.client.consumed_response(request, bytes.len());
                    self.heard.entry(request).or_default().body.extend_from_slice(&bytes);
                }
                Event::RequestEnded { request, end } => {
                    self.heard.entry(request).or_default().ends.push((self.now, end));
                }
                Event::Ticket { ticket } => self.tickets.push(ticket),
                _ => {}
            }
        }
    }

    /// The server's answer. A request whose client cancelled has no stream left to answer
    /// on, so a failed `respond` is not the test's error.
    fn answer(&mut self, request: RequestId, method: &str, path: &str, range: Option<&str>) {
        if self.serving == Serving::Hold {
            return;
        }
        let Some(object) = self.objects.get(path) else {
            _ = self.server.respond(request, 404, &[], &[]);
            return;
        };
        let etag = format!("\"{}\"", object.len());
        if method == "HEAD" {
            let len = object.len().to_string();
            _ = self.server.respond(request, 200, &[("content-length", &len), ("etag", &etag)], &[]);
            return;
        }
        if method != "GET" {
            _ = self.server.respond(request, 405, &[], &[]);
            return;
        }
        let range = range.map(|it| Range::parse(it).expect("the client sends one closed range"));
        let (status, at, len) = match range {
            Some(range) => (206, range.at as usize, range.len as usize),
            None => (200, 0, object.len()),
        };
        let mut body = object[at..at + len].to_vec();
        if self.serving == Serving::OneByteMore {
            body.push(0xa5);
            _ = self.server.respond(request, status, &[("etag", &etag)], &body);
            return;
        }
        let length = body.len().to_string();
        let headers = [("content-length", length.as_str()), ("etag", &etag)];
        _ = self.server.respond(request, status, &headers, &body);
    }

    /// Lands what is due, makes the cancels due, then moves the clock to the next thing due
    /// (a landing, a deadline or a cancel), at most to `until`. False when nothing is due by
    /// `until`.
    fn turn(&mut self, until: Duration) -> bool {
        for (to, from, bytes) in self.net.landed(self.now) {
            if to == SERVER {
                self.server.received(from, &bytes);
            } else {
                self.heard_server = true;
                self.client.received(from, &bytes);
            }
        }
        let now = self.now;
        let mut due = Vec::new_in(Global);
        due.extend(self.cancels.iter().filter(|it| it.0 <= now).map(|it| it.1));
        self.cancels.retain(|it| it.0 > now);
        for request in due {
            // One already ended stays as it ended; its connection may be gone.
            _ = self.client.cancel(request);
        }
        self.exchange();
        let next = [
            self.net.next_landing(),
            self.client.next_deadline(),
            self.server.next_deadline(),
            self.cancels.iter().map(|it| it.0).min(),
        ]
        .into_iter()
        .flatten()
        .min();
        let Some(next) = next else { return false };
        if next > until {
            self.now = self.now.max(until);
            return false;
        }
        self.now = next.max(self.now);
        self.client.advance_clock(self.now);
        self.server.advance_clock(self.now);
        self.exchange();
        true
    }

    /// Runs until `done`, or nothing is due by `until`.
    fn run(&mut self, until: Duration, done: impl Fn(&World) -> bool) {
        for _ in 0..MOST_TURNS {
            if done(self) || !self.turn(until) {
                return;
            }
        }
        panic!("the simulation took {MOST_TURNS} turns by {:?}", self.now);
    }

    fn all_ended(&self) -> bool {
        self.asked.keys().all(|it| self.heard.get(it).is_some_and(|heard| !heard.ends.is_empty()))
    }

    fn note(&mut self, id: RequestId, object: String, range: Option<Range>, head: bool) {
        let deadline = self.now + DEADLINE;
        self.asked.insert(id, Asked { object, range, head, deadline });
    }

    /// Asks one request of the workload on `conn`: a ranged GET of a random object (sometimes
    /// one the server lacks), sometimes the whole object or a HEAD; early when `early`.
    fn ask_one(&mut self, conn: ConnId, early: bool) -> RequestId {
        let object = if self.rng.one_in(MISSING_ONE_IN) {
            "/o/missing".to_owned()
        } else {
            format!("/o/{}", self.rng.index(OBJECTS))
        };
        let size = self.objects.get(&object).map_or(MOST_OBJECT, |it| it.len() as u64);
        let head = self.rng.one_in(HEAD_ONE_IN);
        let range = (!head && !self.rng.one_in(WHOLE_ONE_IN)).then(|| {
            let at = self.rng.below(size);
            Range { at, len: 1 + self.rng.below(size - at) }
        });
        let length = Length::Exactly(range.map_or(size, |it| it.len));
        let deadline = self.now + DEADLINE;
        let asked = if head {
            Early::head(AUTHORITY.to_owned(), object.clone(), deadline)
        } else {
            Early::get(AUTHORITY.to_owned(), object.clone(), range, length, deadline)
        };
        let id = if early {
            self.client.request_early(conn, asked).unwrap()
        } else {
            self.client.request(conn, asked.request().clone()).unwrap()
        };
        if self.rng.one_in(CANCEL_ONE_IN) {
            let at = self.now + Duration::from_millis(self.rng.below(MOST_CANCEL_AFTER_MS));
            self.cancels.push((at, id));
        }
        self.note(id, object, range, head);
        id
    }

    /// The invariant, checked over everything the run heard; the first breach found.
    fn check(&self) -> Result<(), String> {
        if let Some((method, _)) =
            self.served.iter().find(|(method, early)| *early && method != "GET" && method != "HEAD")
        {
            return Err(format!("a {method} came to the server in early data"));
        }
        for (id, asked) in &self.asked {
            let object = &asked.object;
            let heard = self.heard.get(id);
            let ends = heard.map_or(0, |it| it.ends.len());
            if ends != 1 {
                return Err(format!("request {id:?} of {object} ended {ends} times"));
            }
            let heard = heard.unwrap();
            let (at, end) = &heard.ends[0];
            if *at > asked.deadline {
                return Err(format!(
                    "request {id:?} of {object} ended at {at:?}, past its deadline {:?}",
                    asked.deadline
                ));
            }
            let bytes = self.objects.get(object);
            let expected: &[u8] = match (bytes, asked.range) {
                _ if asked.head => &[],
                (None, _) => &[],
                (Some(bytes), Some(range)) => &bytes[range.at as usize..(range.at + range.len) as usize],
                (Some(bytes), None) => bytes,
            };
            if heard.body.len() > expected.len() {
                return Err(format!(
                    "request {id:?} of {object} was handed {} bytes, past the {} it asked",
                    heard.body.len(),
                    expected.len()
                ));
            }
            if *end != RequestEnd::Done {
                continue;
            }
            let status = heard.status;
            let fine = match bytes {
                None => status == Some(404) && heard.body.is_empty(),
                Some(_) => status.is_some_and(|it| (200..=299).contains(&it)) && heard.body == expected,
            };
            if !fine {
                return Err(format!(
                    "request {id:?} of {object} ended Done with {status:?} and {} bytes, not the {} asked",
                    heard.body.len(),
                    expected.len()
                ));
            }
        }
        Ok(())
    }
}

/// A connection, its first requests, and a ticket from it: the first process of a run.
fn first_connection(world: &mut World, early: bool) {
    let conn = world.client.connect_requests(SERVER, NAME).unwrap();
    for _ in 0..REQUESTS {
        let early = early && world.rng.one_in(2);
        world.ask_one(conn, early);
    }
    let until = world.now + DEADLINE + AFTER;
    world.run(until, |it| it.all_ended() && !it.tickets.is_empty());
    assert!(!world.tickets.is_empty(), "the first connection got no ticket");
}

/// A run of `seed`: a first process that gets tickets from an honest server, then a second
/// that resumes with them and sends all its requests as early data to a server `serving`, the
/// second client's connection carrying `plant`.
fn run(seed: u64, lossy: bool, serving: Serving, plant: Plant) -> World {
    let mut world = World::new(seed, lossy);
    first_connection(&mut world, true);
    world.serving = serving;
    world.new_client();
    let conn = world.client.connect_requests(SERVER, NAME).unwrap();
    world.client.plant(conn, plant);
    for _ in 0..REQUESTS {
        world.ask_one(conn, true);
    }
    if serving == Serving::Hold {
        let held = world.now + HOLD;
        world.run(held, |_| false);
        world.server.close_all(0, "the test's server stops");
    }
    let until = world.now + DEADLINE + AFTER;
    world.run(until, World::all_ended);
    world
}

/// The invariant: every completed read returns exactly the asked bytes of the asked object,
/// and no request is handed a byte past what it asked; every request ends in exactly one
/// outcome, within its deadline on the virtual clock; and no request other than a GET or a
/// HEAD is ever carried in early data.
#[test]
fn requests_over_a_lossy_network_end_once_with_their_bytes() {
    let mut early = 0;
    for seed in 0..SEEDS {
        let world = run(seed, true, Serving::Honest, Plant::None);
        if let Err(why) = world.check() {
            panic!("seed {seed}: {why}");
        }
        early += world.served.iter().filter(|it| it.1).count();
    }
    assert!(early > 0, "no request of {SEEDS} seeds came early: the 0-RTT path was not run");
}

#[test]
fn the_oracle_catches_a_body_delivered_past_its_asked_length() {
    let honest = run(1, false, Serving::OneByteMore, Plant::None);
    assert_eq!(honest.check(), Ok(()), "the core refuses the extra byte, so the oracle passes");
    let planted = run(1, false, Serving::OneByteMore, Plant::NoBodyBound);
    let caught = planted.check().expect_err("a body past its length is caught");
    assert!(caught.contains("past the"), "{caught}");
}

#[test]
fn the_oracle_catches_a_request_left_without_an_outcome_when_its_connection_closes() {
    let honest = run(2, false, Serving::Hold, Plant::None);
    assert_eq!(honest.check(), Ok(()), "every held request ends as its connection closes");
    let planted = run(2, false, Serving::Hold, Plant::NoOutcomeOnClose);
    let caught = planted.check().expect_err("a request with no outcome is caught");
    assert!(caught.contains("ended 0 times"), "{caught}");
}

#[test]
fn the_oracle_catches_a_non_idempotent_request_sent_early() {
    let mut world = World::new(3, false);
    first_connection(&mut world, false);
    world.new_client();
    let conn = world.client.connect_requests(SERVER, NAME).unwrap();
    let post = Request {
        method: http::Method::POST,
        authority: AUTHORITY.to_owned(),
        path: "/o/0".to_owned(),
        range: None,
        length: Length::AtMost(0),
        deadline: world.now + DEADLINE,
    };
    let id = world.client.plant_early(conn, post).unwrap();
    world.note(id, "/o/0".to_owned(), None, true);
    world.run(world.now + DEADLINE + AFTER, World::all_ended);
    let caught = world.check().expect_err("a POST in early data is caught");
    assert!(caught.contains("a POST came to the server in early data"), "{caught}");
}

/// The QUIC v1 long-header packet types in a datagram, coalesced packets included: 0 Initial,
/// 1 0-RTT, 2 Handshake, 3 Retry (RFC 9000, 17.2). A short header ends the datagram, as it
/// takes the rest of it.
fn long_types(mut datagram: &[u8]) -> Vec<u8, Global> {
    let mut out = Vec::new_in(Global);
    while let Some(&first) = datagram.first() {
        if first & 0x80 == 0 {
            break;
        }
        let ty = (first >> 4) & 0x3;
        out.push(ty);
        if ty == 3 {
            break;
        }
        // The first byte, then the version.
        let mut at = 1 + 4;
        let Some(&dcid) = datagram.get(at) else { break };
        at += 1 + usize::from(dcid);
        let Some(&scid) = datagram.get(at) else { break };
        at += 1 + usize::from(scid);
        if ty == 0 {
            let Some((token, n)) = datagram.get(at..).and_then(varint::get) else { break };
            at += n + token as usize;
        }
        let Some((len, n)) = datagram.get(at..).and_then(varint::get) else { break };
        at += n + len as usize;
        let Some(rest) = datagram.get(at..) else { break };
        datagram = rest;
    }
    out
}

#[test]
fn a_resumed_connection_sends_its_first_get_as_0rtt_before_hearing_the_server() {
    let mut world = World::new(4, false);
    first_connection(&mut world, false);
    world.new_client();
    let conn = world.client.connect_requests(SERVER, NAME).unwrap();
    let range = Some(Range { at: 0, len: 1 });
    let deadline = world.now + DEADLINE;
    let get = Early::get(AUTHORITY.to_owned(), "/o/0".to_owned(), range, Length::Exactly(1), deadline);
    let id = world.client.request_early(conn, get).unwrap();
    world.note(id, "/o/0".to_owned(), range, false);
    world.client.advance_clock(world.now);
    world.exchange();
    let first_flight = world.client_sent.iter().filter(|(_, heard)| !heard).count();
    assert!(
        world.client_sent.iter().filter(|(_, heard)| !heard).any(|(it, _)| long_types(it).contains(&1)),
        "no 0-RTT packet in the {first_flight} datagrams sent before the server was heard"
    );
    assert_eq!(world.client.sent_log(conn).as_slice(), &[(0, true)], "the GET's HEADERS went early, once");
    world.run(world.now + DEADLINE + AFTER, World::all_ended);
    assert_eq!(world.check(), Ok(()));
    assert_eq!(world.heard[&id].ends[0].1, RequestEnd::Done);
    assert_eq!(world.served.as_slice(), &[("GET".to_owned(), true)], "the server took it as early data");
}

#[test]
fn refused_early_data_sends_each_early_request_again_exactly_once() {
    let mut world = World::new(5, false);
    first_connection(&mut world, false);
    // Another server process with the same certificate: it cannot open the ticket, so it
    // refuses the early data and makes a full handshake.
    world.server = server(world.epoch, world.identity.clone());
    world.new_client();
    let conn = world.client.connect_requests(SERVER, NAME).unwrap();
    let mut ids = Vec::new_in(Global);
    for i in 0..3 {
        let object = format!("/o/{i}");
        let length = Length::Exactly(world.objects[&object].len() as u64);
        let deadline = world.now + DEADLINE;
        let get = Early::get(AUTHORITY.to_owned(), object.clone(), None, length, deadline);
        let id = world.client.request_early(conn, get).unwrap();
        world.note(id, object, None, false);
        ids.push(id);
    }
    world.run(world.now + DEADLINE + AFTER, World::all_ended);
    assert_eq!(world.check(), Ok(()));
    for id in &ids {
        assert_eq!(world.heard[id].ends[0].1, RequestEnd::Done, "{id:?}");
    }
    assert_eq!(
        world.client.sent_log(conn).as_slice(),
        &[(0, true), (1, true), (2, true), (0, false), (1, false), (2, false)],
        "each early request went early once and again once after the handshake"
    );
    assert_eq!(world.served.len(), 3, "the server saw each request once: {:?}", world.served);
    assert!(world.served.iter().all(|(method, early)| method == "GET" && !early), "{:?}", world.served);
}
