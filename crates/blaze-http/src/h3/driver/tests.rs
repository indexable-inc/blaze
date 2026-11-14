//! [`Loop`] under blaze-core's deterministic simulation: a server [`Endpoint`] and a client
//! one, each on its own loop (its own tags) on one [`Sim`], their datagrams crossing the
//! simulation's network, lost, duplicated and delayed by the seed. No socket, thread or real
//! clock: QUIC's own timers (its handshake, its losses, its idle timeout) run on the virtual
//! clock, reached only through the loops' armed deadlines.

use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, SocketAddr},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use blaze_core::{
    Driver as _,
    sim::{Datagrams, Faults, Sim},
};

use super::{Loop, Plant, TAGS};
use crate::h3::{ClientConfig, Endpoint, Event, Identity, ServerConfig, SessionId, StreamRef, Transport, Trust};

const URL: &str = "https://localhost:4433/explore/v1?x=1";
/// The most turns one run takes: far more than the streams need, so a run that is not done by
/// then is one whose streams did not end (a deadline never reached, a receive never asked
/// again).
const TURNS_MOST: usize = 200_000;
/// By when on the virtual clock every stream has ended: the handshake and the bodies take
/// well under a second of the simulation's delays, and a lost packet's recovery a few probe
/// timeouts more; the connection's idle timeout (30 s) is the latest any stream could fail.
const BOUND: Duration = Duration::from_secs(40);
/// Seeds each property runs under.
const SEEDS: u64 = 24;
/// The bodies the server answers with: empty, small, more than a datagram, and more than the
/// stream's window (so the reader's [`Endpoint::consumed`] is what moves it).
const LENGTHS: [usize; 4] = [0, 10, 70_000, 300_000];

fn server_at() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 4433))
}

/// A P-256 certificate valid from a minute ago for 13 days (rustls checks it against the wall
/// clock), and its SHA-256.
fn identity() -> (Identity, [u8; 32]) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
    let epoch = rcgen::date_time_ymd(1970, 1, 1);
    params.not_before = epoch + (now - Duration::from_secs(60));
    params.not_after = epoch + (now + Duration::from_secs(13 * 86_400));
    let cert = params.self_signed(&key).unwrap();
    let der = cert.der().to_vec();
    let hash = blaze_sha256::digest(&der);
    (Identity { cert_der: der, key_der: key.serialize_der() }, hash)
}

/// The body the server answers stream `i` with: `len` bytes drawn from both.
fn body(len: usize, i: usize) -> Vec<u8> {
    (0..len).map(|at| (at.wrapping_mul(31) ^ i ^ len) as u8).collect()
}

/// What one of the client's streams saw.
#[derive(Default, Debug)]
struct Seen {
    body: Vec<u8>,
    /// How many times it ended: finished, reset, or its session closed.
    ends: u32,
    finished: bool,
    ended_at: Option<Duration>,
    /// Something it told after it ended.
    after_end: bool,
}

impl Seen {
    fn end(&mut self, now: Duration, finished: bool) {
        self.ends += 1;
        self.finished |= finished && self.ends == 1;
        self.ended_at.get_or_insert(now);
    }
}

/// The server's side: each stream's request as it comes, answered once it is finished.
#[derive(Default)]
struct Served {
    asked: BTreeMap<StreamRef, Vec<u8>>,
    /// Each stream's index, with the body sent on it.
    sent: BTreeMap<usize, Vec<u8>>,
}

impl Served {
    fn events(&mut self, endpoint: &mut Endpoint) -> Result<(), String> {
        while let Some(event) = endpoint.next_event() {
            match event {
                Event::SessionRequest { session, .. } => endpoint.accept(session).map_err(|it| it.to_string())?,
                Event::StreamOpened { stream, .. } => {
                    self.asked.insert(stream, Vec::new());
                }
                Event::StreamData { stream, bytes } => {
                    endpoint.consumed(stream, bytes.len());
                    self.asked.entry(stream).or_default().extend_from_slice(&bytes);
                }
                Event::StreamFinished { stream } => {
                    let asked = self.asked.remove(&stream).unwrap_or_default();
                    let asked = String::from_utf8(asked).map_err(|it| it.to_string())?;
                    let Some((len, i)) = asked.split_once('/') else { return Err(format!("the server was asked {asked:?}")) };
                    let (len, i): (usize, usize) = (len.parse().map_err(|_| asked.clone())?, i.parse().map_err(|_| asked.clone())?);
                    let answer = body(len, i);
                    if !answer.is_empty() {
                        endpoint.send(stream, &answer).map_err(|it| it.to_string())?;
                    }
                    endpoint.finish(stream).map_err(|it| it.to_string())?;
                    if self.sent.insert(i, answer).is_some() {
                        return Err(format!("the server was asked for stream {i} twice"));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// The client's side: one session, a stream per length, each asking for its body.
struct Asker {
    session: SessionId,
    /// Streams still to open, by index.
    to_open: Vec<usize>,
    streams: BTreeMap<StreamRef, usize>,
    seen: BTreeMap<usize, Seen>,
}

impl Asker {
    fn open(&mut self, endpoint: &mut Endpoint) -> Result<(), String> {
        while let Some(&i) = self.to_open.last() {
            let Some(stream) = endpoint.open(self.session, crate::h3::Direction::Bi).map_err(|it| it.to_string())? else {
                return Ok(());
            };
            self.to_open.pop();
            self.streams.insert(stream, i);
            self.seen.insert(i, Seen::default());
            endpoint.send(stream, format!("{}/{i}", LENGTHS[i]).as_bytes()).map_err(|it| it.to_string())?;
            endpoint.finish(stream).map_err(|it| it.to_string())?;
        }
        Ok(())
    }

    fn seen(&mut self, stream: StreamRef) -> Option<&mut Seen> {
        let i = self.streams.get(&stream)?;
        let seen = self.seen.get_mut(i)?;
        if seen.ends > 0 {
            seen.after_end = true;
        }
        Some(seen)
    }

    fn events(&mut self, endpoint: &mut Endpoint, now: Duration) -> Result<(), String> {
        while let Some(event) = endpoint.next_event() {
            match event {
                Event::SessionOpened { session } | Event::StreamsAvailable { session } if session == self.session => self.open(endpoint)?,
                Event::StreamData { stream, bytes } => {
                    endpoint.consumed(stream, bytes.len());
                    if let Some(seen) = self.seen(stream) {
                        seen.body.extend_from_slice(&bytes);
                    }
                }
                Event::StreamFinished { stream } => {
                    if let Some(seen) = self.seen(stream) {
                        seen.end(now, true);
                    }
                }
                Event::StreamReset { stream, .. } => {
                    if let Some(seen) = self.seen(stream) {
                        seen.end(now, false);
                    }
                }
                Event::SessionClosed { session, .. } if session == self.session => {
                    // Every stream not yet ended ends with its session.
                    for seen in self.seen.values_mut().filter(|it| it.ends == 0) {
                        seen.end(now, false);
                    }
                    for i in self.to_open.drain(..) {
                        self.seen.entry(i).or_default().end(now, false);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn ended(&self) -> bool {
        self.to_open.is_empty() && self.seen.len() == LENGTHS.len() && self.seen.values().all(|it| it.ends > 0)
    }
}

fn net(lose_one_in: u32, duplicate_one_in: u32) -> Datagrams {
    Datagrams { lose_one_in, duplicate_one_in, max_delay: Duration::from_millis(5) }
}

/// One run under `seed`: a session, a stream per length, driven to their ends, then the
/// oracle.
fn run(seed: u64, net: Datagrams, plant: Plant) -> Result<(), String> {
    let mut sim = Sim::new(seed, Faults { max_latency: Duration::from_millis(1), ..Faults::default() });
    sim.datagrams(net);
    let epoch = Instant::now();
    let (identity, pin) = identity();
    let server_config = ServerConfig { identity, transport: Transport::default(), requests: false };
    let server = Endpoint::new(epoch, Some(server_config), None).map_err(|it| it.to_string())?;
    let client_config = ClientConfig { trust: Trust::Pinned(vec![pin]), transport: Transport::default() };
    let client = Endpoint::new(epoch, None, Some(client_config)).map_err(|it| it.to_string())?;
    let mut server = Loop::new(server, 0, server_at());
    let mut client = Loop::new(client, TAGS, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    client.plant(plant);
    let session = client.endpoint().connect(server_at(), URL, &[]).map_err(|it| it.to_string())?;
    let mut served = Served::default();
    let mut asker = Asker { session, to_open: (0..LENGTHS.len()).rev().collect(), streams: BTreeMap::new(), seen: BTreeMap::new() };
    let mut effects = Vec::new();
    let mut done = Vec::new();
    for _ in 0..TURNS_MOST {
        let now = sim.now();
        if now > BOUND {
            break;
        }
        server.endpoint().advance_clock(now);
        client.endpoint().advance_clock(now);
        served.events(server.endpoint())?;
        asker.events(client.endpoint(), now)?;
        if asker.ended() {
            break;
        }
        server.step(now, &mut effects);
        client.step(now, &mut effects);
        sim.submit(&mut effects);
        sim.poll(&mut done, server.deadline().min(client.deadline()));
        let now = sim.now();
        server.endpoint().advance_clock(now);
        client.endpoint().advance_clock(now);
        for completion in done.drain(..) {
            if server.owns(completion.tag) {
                server.complete(completion);
            } else if client.owns(completion.tag) {
                client.complete(completion);
            } else {
                return Err(format!("seed {seed}: {:?} is no loop's", completion.tag));
            }
        }
        if let Some(failed) = server.failed().or(client.failed()) {
            return Err(format!("seed {seed}: a loop's socket failed: {failed}"));
        }
    }
    for (i, len) in LENGTHS.iter().enumerate() {
        let Some(seen) = asker.seen.get(&i) else { return Err(format!("seed {seed}: stream {i} was never opened")) };
        if seen.ends != 1 || seen.after_end {
            return Err(format!("seed {seed}: stream {i} ended {} times (told after its end: {})", seen.ends, seen.after_end));
        }
        let ended = seen.ended_at.unwrap_or(Duration::MAX);
        if ended > BOUND {
            return Err(format!("seed {seed}: stream {i} ended at {ended:?}, past {BOUND:?}"));
        }
        // QUIC recovers what the network loses: every stream finishes, lossy or not.
        if !seen.finished {
            return Err(format!("seed {seed}: stream {i} ended without finishing, at {ended:?}"));
        }
        match served.sent.get(&i) {
            Some(sent) if *sent == seen.body => {}
            sent => {
                return Err(format!(
                    "seed {seed}: stream {i} ({len} bytes asked) finished with {} bytes; the server sent {:?}",
                    seen.body.len(),
                    sent.map(Vec::len)
                ));
            }
        }
    }
    Ok(())
}

/// The invariant: every stream the client opens ends exactly once, within its bound on the
/// virtual clock, finished with exactly the bytes the server sent on it, whether the network
/// is clean or loses, duplicates and reorders datagrams.
#[test]
fn every_stream_ends_once_by_its_bound_with_the_servers_bytes() {
    for seed in 0..SEEDS {
        run(seed, net(0, 0), Plant::None).unwrap();
        run(seed, net(16, 8), Plant::None).unwrap();
    }
}

/// Control: a receive's completion dropped leaves the client deaf (no receive is asked
/// again), and its streams end only as the connection idles out; the oracle sees them end
/// unfinished.
#[test]
fn the_oracle_catches_a_dropped_completion() {
    let caught = (0..SEEDS).filter(|seed| run(*seed, net(0, 0), Plant::DropCompletion { nth: 1 + (*seed as u32 % 3) }).is_err()).count();
    assert_eq!(caught as u64, SEEDS, "a dropped receive completion went unseen");
}

/// Control: with the client's deadline never armed, a lost packet is never sent again (QUIC's
/// probe timeout is a deadline the simulation's clock never reaches), and the oracle says so.
#[test]
fn the_oracle_catches_a_deadline_never_armed() {
    let caught = (0..SEEDS).filter(|seed| run(*seed, net(4, 0), Plant::NoDeadline).is_err()).count();
    assert!(caught > 0, "a deadline never armed went unseen under {SEEDS} seeds");
}
