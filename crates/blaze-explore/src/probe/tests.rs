//! [`Echo`] under blaze-core's deterministic simulation: the probe's endpoint on its HTTP/3
//! driver loop and a page's client on its own, on one [`Sim`] whose network loses, duplicates
//! and reorders datagrams by the seed. The page sends bidirectional streams, unidirectional
//! streams and datagrams, reads their echoes, then closes its session.

use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, SocketAddr},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use blaze_core::{
    Driver as _,
    sim::{Datagrams, Faults, Sim},
};
use blaze_http::h3::{
    ClientConfig, Closed, Direction, Endpoint, Event, ServerConfig, SessionId, StreamRef,
    Transport, Trust,
    driver::{Loop, TAGS},
};
use hegel::{HealthCheck, TestCase, generators as gs};
use ix_prof::explore::cert::{Cert, Validity};

use super::{Echo, Ended, Plant, Tally};

const URL: &str = "https://127.0.0.1:4433/explore/probe";
/// The bodies of the page's bidirectional streams: empty, small, more than a datagram, and
/// more than the stream's window (so the echo's [`Endpoint::consumed`] is what moves it).
const BIDI: [usize; 4] = [0, 10, 70_000, 300_000];
const UNI: [usize; 3] = [0, 10, 70_000];
const DATAGRAMS: [usize; 3] = [1, 100, 1000];
/// By when on the virtual clock every echo is back and the session ended: well under a
/// second of the simulation's delays, a lost packet's recovery a few probe timeouts more.
const BOUND: Duration = Duration::from_secs(20);
/// How long the page waits for datagrams' echoes after its streams' before it closes.
const LINGER: Duration = Duration::from_millis(200);
const TURNS_MOST: usize = 400_000;

fn server_at() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 4433))
}

/// The body of stream (or datagram) `i` of `len` bytes.
fn body(len: usize, i: usize) -> Vec<u8> {
    (0..len).map(|at| (at.wrapping_mul(31) ^ i ^ len) as u8).collect()
}

/// What the page saw of one stream.
#[derive(Default, Debug)]
struct Seen {
    body: Vec<u8>,
    ends: u32,
    finished: bool,
}

impl Seen {
    fn end(&mut self, finished: bool) {
        self.ends += 1;
        self.finished |= finished && self.ends == 1;
    }
}

struct Page {
    session: SessionId,
    opened: bool,
    /// Streams to open: the direction and the index in BIDI or UNI.
    to_open: Vec<(Direction, usize)>,
    bidi: BTreeMap<StreamRef, usize>,
    bidi_seen: BTreeMap<usize, Seen>,
    /// The probe's unidirectional streams, as they came.
    uni_seen: BTreeMap<StreamRef, Seen>,
    datagrams: Vec<Vec<u8>>,
    streams_done_at: Option<Duration>,
    close_asked: bool,
    closed: Option<Closed>,
}

impl Page {
    fn open(&mut self, endpoint: &mut Endpoint) -> Result<(), String> {
        while let Some(&(direction, i)) = self.to_open.last() {
            let Some(stream) = endpoint.open(self.session, direction).map_err(|it| it.to_string())? else {
                return Ok(());
            };
            self.to_open.pop();
            let len = match direction {
                Direction::Bi => {
                    self.bidi.insert(stream, i);
                    self.bidi_seen.insert(i, Seen::default());
                    BIDI[i]
                }
                Direction::Uni => UNI[i],
            };
            endpoint.send(stream, &body(len, i)).map_err(|it| it.to_string())?;
            endpoint.finish(stream).map_err(|it| it.to_string())?;
        }
        Ok(())
    }

    fn events(&mut self, endpoint: &mut Endpoint, now: Duration) -> Result<(), String> {
        while let Some(event) = endpoint.next_event() {
            match event {
                Event::SessionOpened { session } if session == self.session => {
                    self.opened = true;
                    for (i, len) in DATAGRAMS.iter().enumerate() {
                        endpoint.send_datagram(session, &body(*len, 100 + i)).map_err(|it| it.to_string())?;
                    }
                    self.open(endpoint)?;
                }
                Event::StreamsAvailable { session } if session == self.session => self.open(endpoint)?,
                Event::StreamOpened { stream, .. } => {
                    if stream.direction() != Direction::Uni {
                        return Err(format!("the probe opened {stream:?}, which is not unidirectional"));
                    }
                    self.uni_seen.insert(stream, Seen::default());
                }
                Event::StreamData { stream, bytes } => {
                    endpoint.consumed(stream, bytes.len());
                    let seen = match self.bidi.get(&stream) {
                        Some(i) => self.bidi_seen.get_mut(i),
                        None => self.uni_seen.get_mut(&stream),
                    };
                    match seen {
                        Some(seen) if seen.ends == 0 => seen.body.extend_from_slice(&bytes),
                        _ => return Err(format!("{} bytes came on {stream:?}, which is no open stream", bytes.len())),
                    }
                }
                Event::StreamFinished { stream } | Event::StreamReset { stream, .. } => {
                    let finished = matches!(event, Event::StreamFinished { .. });
                    let seen = match self.bidi.get(&stream) {
                        Some(i) => self.bidi_seen.get_mut(i),
                        None => self.uni_seen.get_mut(&stream),
                    };
                    if let Some(seen) = seen {
                        seen.end(finished);
                    }
                }
                Event::Datagram { bytes, .. } => self.datagrams.push(bytes),
                Event::SessionClosed { session, closed } if session == self.session => {
                    for seen in self.bidi_seen.values_mut().chain(self.uni_seen.values_mut()).filter(|it| it.ends == 0) {
                        seen.end(false);
                    }
                    self.closed = Some(closed);
                }
                _ => {}
            }
        }
        let streams_done = self.to_open.is_empty()
            && self.bidi_seen.values().all(|it| it.ends > 0)
            && self.uni_seen.len() == UNI.len()
            && self.uni_seen.values().all(|it| it.ends > 0);
        if self.opened && streams_done && self.streams_done_at.is_none() {
            self.streams_done_at = Some(now);
        }
        if !self.close_asked && self.streams_done_at.is_some_and(|at| now >= at + LINGER) {
            self.close_asked = true;
            endpoint.close(self.session, 0, "probed").map_err(|it| it.to_string())?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct Net {
    lose_one_in: u32,
    duplicate_one_in: u32,
}

/// The tally a part was to end with: every one echoed whole, none failed.
fn whole(lens: &[usize]) -> Tally {
    Tally { echoed: lens.len() as u64, bytes: lens.iter().sum::<usize>() as u64, failed: 0 }
}

fn run(seed: u64, net: Net, plant: Plant) -> Result<(), String> {
    let fail = |why: String| format!("seed {seed}, {net:?}, {plant:?}: {why}");
    let mut sim = Sim::new(seed, Faults { max_latency: Duration::from_millis(1), ..Faults::default() });
    sim.datagrams(Datagrams { lose_one_in: net.lose_one_in, duplicate_one_in: net.duplicate_one_in, max_delay: Duration::from_millis(5) });
    let epoch = Instant::now();
    let now_s = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let cert = Cert::make(Validity::starting(now_s), crate::NAME).map_err(|it| it.to_string())?;
    let identity = cert.identity().map_err(|it| it.to_string())?;
    let server_config = ServerConfig { identity, transport: Transport::default(), requests: false };
    let server = Endpoint::new(epoch, Some(server_config), None).map_err(|it| it.to_string())?;
    let client_config = ClientConfig { trust: Trust::Pinned(vec![blaze_sha256::digest(&cert.der)]), transport: Transport::default() };
    let client = Endpoint::new(epoch, None, Some(client_config)).map_err(|it| it.to_string())?;
    let mut server = Loop::new(server, 0, server_at());
    let mut client = Loop::new(client, TAGS, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let mut echo = Echo::new();
    echo.plant(plant);
    let session = client.endpoint().connect(server_at(), URL, &[("origin", "https://ix.dev")]).map_err(|it| it.to_string())?;
    let mut to_open: Vec<(Direction, usize)> = (0..BIDI.len()).map(|i| (Direction::Bi, i)).collect();
    to_open.extend((0..UNI.len()).map(|i| (Direction::Uni, i)));
    let mut page = Page {
        session,
        opened: false,
        to_open,
        bidi: BTreeMap::new(),
        bidi_seen: BTreeMap::new(),
        uni_seen: BTreeMap::new(),
        datagrams: Vec::new(),
        streams_done_at: None,
        close_asked: false,
        closed: None,
    };
    let (mut effects, mut done, mut ended, mut said): (Vec<_>, Vec<_>, Vec<Ended>, Vec<String>) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for _ in 0..TURNS_MOST {
        let now = sim.now();
        if now > BOUND || (!ended.is_empty() && page.closed.is_some()) {
            break;
        }
        server.endpoint().advance_clock(now);
        client.endpoint().advance_clock(now);
        echo.step(server.endpoint());
        echo.take_said(&mut said);
        echo.take_ended(&mut ended);
        page.events(client.endpoint(), now).map_err(fail)?;
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
                return Err(fail(format!("{:?} is no loop's", completion.tag)));
            }
        }
        if let Some(failed) = server.failed().or(client.failed()) {
            return Err(fail(format!("a loop's socket failed: {failed}")));
        }
    }
    for (i, len) in BIDI.iter().enumerate() {
        let seen = page.bidi_seen.get(&i).ok_or_else(|| fail(format!("bidirectional stream {i} was never opened")))?;
        if seen.ends != 1 || !seen.finished || seen.body != body(*len, i) {
            return Err(fail(format!("bidirectional stream {i} of {len} bytes ended {} times, finished: {}, with {} bytes", seen.ends, seen.finished, seen.body.len())));
        }
    }
    // The probe's streams come in any order: their bodies, sorted, are the page's.
    let mut uni: Vec<&Seen> = page.uni_seen.values().collect();
    uni.sort_by(|a, b| a.body.len().cmp(&b.body.len()).then_with(|| a.body.cmp(&b.body)));
    let mut sent: Vec<Vec<u8>> = UNI.iter().enumerate().map(|(i, len)| body(*len, i)).collect();
    sent.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    let echoed: Vec<&Vec<u8>> = uni.iter().map(|it| &it.body).collect();
    if uni.iter().any(|it| it.ends != 1 || !it.finished) || echoed != sent.iter().collect::<Vec<_>>() {
        return Err(fail(format!("the unidirectional echoes came as {:?} bytes, finished: {:?}", echoed.iter().map(|it| it.len()).collect::<Vec<_>>(), uni.iter().map(|it| it.finished).collect::<Vec<_>>())));
    }
    let sent_datagrams: Vec<Vec<u8>> = DATAGRAMS.iter().enumerate().map(|(i, len)| body(*len, 100 + i)).collect();
    if page.datagrams.iter().any(|it| !sent_datagrams.contains(it)) {
        return Err(fail("a datagram came back that the page did not send".to_owned()));
    }
    let [ended] = ended.as_slice() else {
        return Err(fail(format!("{} sessions ended at the probe, not one", ended.len())));
    };
    if ended.session != session || !matches!(ended.closed, Closed::ByPeer { code: 0, .. }) {
        return Err(fail(format!("the probe's session ended {:?}", ended.closed)));
    }
    if ended.bidi != whole(&BIDI) || ended.uni != whole(&UNI) {
        return Err(fail(format!("the probe tallied bidirectional {:?} and unidirectional {:?}", ended.bidi, ended.uni)));
    }
    // A datagram the network lost never reached the probe; on a clean network every one is
    // echoed.
    let datagrams_right = if net.lose_one_in == 0 {
        ended.datagrams == whole(&DATAGRAMS) && page.datagrams.len() >= DATAGRAMS.len()
    } else {
        ended.datagrams.failed == 0 && ended.datagrams.echoed as usize <= DATAGRAMS.len() * 2
    };
    if !datagrams_right {
        return Err(fail(format!("the probe tallied datagrams {:?}; the page got {} back", ended.datagrams, page.datagrams.len())));
    }
    if ended.failed().next().is_some() {
        return Err(fail("a part failed".to_owned()));
    }
    if !said.iter().any(|it| it.starts_with("CONNECT /explore/probe")) {
        return Err(fail(format!("the probe did not say the CONNECT: {said:?}")));
    }
    Ok(())
}

/// The invariant: every stream the page sends is echoed exactly once, whole and finished, on
/// itself (bidirectional) or on one stream the probe opens (unidirectional); every datagram
/// that reaches the probe is echoed, and none comes back that was not sent; and the session's
/// end hands back one tally a part that counts each echo and no failure: whether the network is
/// clean or loses, duplicates and reorders datagrams.
#[hegel::test(test_cases = 16, suppress_health_check = [HealthCheck::TooSlow])]
fn every_stream_and_datagram_is_echoed_once_and_tallied(tc: TestCase) {
    let seed = tc.draw(gs::integers::<u64>());
    let net = if tc.draw(gs::booleans()) {
        Net { lose_one_in: tc.draw(gs::integers::<u32>().min_value(8).max_value(32)), duplicate_one_in: 8 }
    } else {
        Net { lose_one_in: 0, duplicate_one_in: 0 }
    };
    if let Err(why) = run(seed, net, Plant::None) {
        panic!("{why}");
    }
}

const CONTROL_SEEDS: u64 = 4;
const CLEAN: Net = Net { lose_one_in: 0, duplicate_one_in: 0 };

/// Control: with no stream's bytes said taken, the stream longer than the window stalls, and
/// the oracle sees its echo short.
#[test]
fn the_oracle_catches_bytes_never_said_taken() {
    for seed in 0..CONTROL_SEEDS {
        assert!(run(seed, CLEAN, Plant::NoConsume).is_err(), "seed {seed}: bytes never said taken went unseen");
    }
}

/// Control: with a bidirectional stream's end not echoed, its echo never finishes, and the
/// oracle says so.
#[test]
fn the_oracle_catches_an_end_not_echoed() {
    for seed in 0..CONTROL_SEEDS {
        assert!(run(seed, CLEAN, Plant::DropFin).is_err(), "seed {seed}: an end not echoed went unseen");
    }
}
