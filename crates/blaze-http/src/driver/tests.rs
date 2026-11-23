//! [`Loop`] under blaze-core's deterministic simulation: a scripted HTTP/1.1 server on the
//! same [`Sim`] (its own tags, past the loop's), simulated TCP between them, a name from the
//! simulation's table, completions delayed and shuffled by the seed, and with faults on, failed
//! listens, connects, resolves, sends and receives. No socket, thread or real clock.

use std::{
    alloc::Global,
    collections::{BTreeMap, VecDeque},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use blaze_core::{
    Address, Completion as Finished, Conn, Done, Driver, Effect, Tag, Waker,
    chan::{self, Receiver},
    // The server is locked: a simulated process's step is `Send` (`Sim::other_process`).
    lock::Lock,
    sim::{Faults, Sim},
    wait::Deadline,
};
use blaze_sim::{Broke, Config as SimConfig, Tape};

use super::{Answer, Asker, Fetched, IN_FLIGHT_MOST, Inbox, Loop, Message, NEXT_EVENT, Plant, Told, serve};
use crate::{Client, Config, Event, Kind, Request, RequestId, Step, mailbox::Unanswered};

/// Every request's timeout: the client's deadline for an answer that does not come.
const TIMEOUT: Duration = Duration::from_secs(5);
/// What a request may take past [`TIMEOUT`] on the virtual clock: the simulation's latency on
/// each of its few effects, far under this.
const SLACK: Duration = Duration::from_secs(1);
/// The most turns one run takes: far more than the requests need, so a run that is not done
/// by then is one whose requests did not end (a deadline never reached).
const TURNS_MOST: usize = 20_000;
/// The server's tags: past every tag of the loop's (`0..TAGS`).
const SERVER: u64 = 1 << 50;
/// Seeds each property runs under.
const SEEDS: u64 = 48;

fn at() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 8080))
}

fn client() -> Client {
    let tls = crate::roots::with_roots(rustls::RootCertStore::empty()).unwrap();
    Client::new(Config::new(tls, "blaze-test", TIMEOUT))
}

/// The body the scripted server answers a path of `len` with.
fn body_of(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i.wrapping_mul(31) ^ len) as u8).collect()
}

/// What one of the server's effects was.
enum Did {
    Listen,
    Recv(Conn),
    Send(Conn),
    Hangup,
}

/// One connection the server accepted.
#[derive(Default)]
struct Peer {
    asked: Vec<u8>,
    sending: bool,
    queue: VecDeque<Vec<u8>>,
    /// Hang up once the queue is sent: a `/cut/` answer is on it.
    hang: bool,
}

/// The scripted server: `/n/<len>/<i>` is answered with a body of `len` bytes drawn from the
/// path, kept alive; `/cut/<len>/<i>` with the same head and only the first half of that body,
/// then the connection is hung up; `/silent/<i>` is never answered.
#[derive(Default)]
struct Server {
    next: u64,
    tags: BTreeMap<u64, Did>,
    peers: BTreeMap<Conn, Peer>,
    /// Each path answered, with the body sent for it.
    sent: BTreeMap<String, Vec<u8>>,
}

impl Server {
    fn tag(&mut self, did: Did) -> Tag {
        let tag = SERVER + self.next;
        self.next += 1;
        self.tags.insert(tag, did);
        Tag(tag)
    }

    fn listen(&mut self, effects: &mut Vec<Effect>) {
        let tag = self.tag(Did::Listen);
        effects.push(Effect::Listen { at: Address::Tcp(at()), options: blaze_core::sockopt::Tcp::DEFAULT, tag });
    }

    fn recv(&mut self, conn: Conn, effects: &mut Vec<Effect>) {
        let tag = self.tag(Did::Recv(conn));
        effects.push(Effect::Recv { conn, most: 1 << 16, into: Vec::new_in(Global), tag });
    }

    fn pump(&mut self, conn: Conn, effects: &mut Vec<Effect>) {
        let Some(peer) = self.peers.get_mut(&conn) else { return };
        if peer.sending {
            return;
        }
        let Some(bytes) = peer.queue.pop_front() else { return };
        peer.sending = true;
        let tag = self.tag(Did::Send(conn));
        effects.push(Effect::Send { conn, bytes, tag });
    }

    /// The bytes that answer `path`, and whether the connection is hung up after them.
    fn answer(&mut self, path: &str) -> Option<(Vec<u8>, bool)> {
        let mut parts = path.trim_start_matches('/').split('/');
        let cut = match parts.next()? {
            "n" => false,
            "cut" => true,
            _ => return None,
        };
        let len: usize = parts.next()?.parse().ok()?;
        let body = body_of(len);
        let mut bytes = format!("HTTP/1.1 200 OK\r\nContent-Length: {len}\r\n\r\n").into_bytes();
        let sent = if cut { &body[..len / 2] } else { &body[..] };
        bytes.extend_from_slice(sent);
        self.sent.insert(path.to_owned(), sent.to_vec());
        Some((bytes, cut))
    }

    fn complete(&mut self, completion: Finished, effects: &mut Vec<Effect>) {
        let Some(did) = self.tags.remove(&completion.tag.0) else { panic!("the server's {:?} was never asked", completion.tag) };
        match (did, completion.done) {
            (Did::Listen, Done::Accepted { conn }) => {
                // The listen stays in flight: its tag is kept for the next.
                self.tags.insert(completion.tag.0, Did::Listen);
                self.peers.insert(conn, Peer::default());
                self.recv(conn, effects);
            }
            (Did::Recv(conn), Done::Read(bytes)) if bytes.is_empty() => {
                self.peers.remove(&conn);
                let tag = self.tag(Did::Hangup);
                effects.push(Effect::Hangup { conn, tag });
            }
            (Did::Recv(conn), Done::Read(bytes)) => {
                let mut answers = Vec::new();
                if let Some(peer) = self.peers.get_mut(&conn) {
                    peer.asked.extend_from_slice(&bytes);
                    while let Some(end) = peer.asked.windows(4).position(|it| it == b"\r\n\r\n") {
                        let head: Vec<u8> = peer.asked.drain(..end + 4).collect();
                        let line = String::from_utf8_lossy(&head);
                        let path = line.split(' ').nth(1).unwrap_or("").to_owned();
                        answers.push(path);
                    }
                }
                for path in answers {
                    if let Some((bytes, cut)) = self.answer(&path)
                        && let Some(peer) = self.peers.get_mut(&conn)
                    {
                        peer.queue.push_back(bytes);
                        peer.hang |= cut;
                    }
                }
                self.pump(conn, effects);
                self.recv(conn, effects);
            }
            (Did::Send(conn), Done::Sent(_)) => {
                let hang = match self.peers.get_mut(&conn) {
                    Some(peer) => {
                        peer.sending = false;
                        peer.hang && peer.queue.is_empty()
                    }
                    None => false,
                };
                if hang {
                    // The peer closes mid-response: its client reads the end of the stream
                    // before the body its head promised.
                    self.peers.remove(&conn);
                    let tag = self.tag(Did::Hangup);
                    effects.push(Effect::Hangup { conn, tag });
                } else {
                    self.pump(conn, effects);
                }
            }
            // A fault the seed drew: the server lets go of the connection, and its client's
            // request ends by its deadline.
            (Did::Recv(conn) | Did::Send(conn), Done::Broken { .. } | Done::Failed(_)) => {
                self.peers.remove(&conn);
            }
            (Did::Listen, Done::Failed(_)) | (Did::Hangup, _) => {}
            (_, done) => panic!("the server's effect completed with {done:?}"),
        }
    }
}

/// What one request told.
#[derive(Default, Debug)]
struct Seen {
    status: Option<u16>,
    body: Vec<u8>,
    /// How many times it ended (`Done`, `Failed`, `TimedOut`).
    ends: u32,
    done: bool,
    ended_at: Option<Duration>,
    /// Something it told after it ended.
    after_end: bool,
}

/// The requests one run asks: paths of the scripted server, by origin.
fn requests() -> Vec<(String, String)> {
    let mut asked = Vec::new();
    for (i, len) in [0usize, 10, 70_000, 200_000].into_iter().enumerate() {
        asked.push((format!("http://127.0.0.1:8080/n/{len}/{i}"), format!("/n/{len}/{i}")));
    }
    asked.push(("http://origin.test:8080/n/5/4".to_owned(), "/n/5/4".to_owned()));
    asked.push(("http://127.0.0.1:8080/silent/5".to_owned(), "/silent/5".to_owned()));
    asked
}

/// One run under `seed`: the requests asked at once, driven to their ends, then the oracle.
fn run(seed: u64, faults: Faults, plant: Plant) -> Result<(), String> {
    let faulty = faults.fail_one_in != 0;
    let mut sim = Sim::new(seed, faults);
    sim.name("origin.test", &[IpAddr::V4(Ipv4Addr::LOCALHOST)]);
    let mut core = Loop::new(client(), 0, IN_FLIGHT_MOST);
    core.plant(plant);
    let mut server = Server::default();
    let mut effects = Vec::new();
    server.listen(&mut effects);
    sim.submit(&mut effects);
    let asked = requests();
    for (i, (url, _)) in asked.iter().enumerate() {
        core.ask(RequestId(i as u64), Request::get(url.clone()), None);
    }
    let mut seen: BTreeMap<RequestId, Seen> = BTreeMap::new();
    let mut done = Vec::new();
    for _ in 0..TURNS_MOST {
        core.step(sim.now(), &mut effects);
        while let Some(told) = core.next_told() {
            let (id, ends) = match &told {
                Told::Event(event) => (event.request(), event.ends()),
                Told::TimedOut(id) => (*id, true),
                Told::Woke(key) => return Err(format!("woke sleep {key}, which no one asked")),
            };
            let it = seen.entry(id).or_default();
            if it.ends > 0 {
                it.after_end = true;
            }
            match told {
                Told::Event(Event::Head { status, .. }) => it.status = Some(status),
                Told::Event(Event::Body { bytes, .. }) => {
                    it.body.extend_from_slice(&bytes);
                    core.consumed(id, bytes.len());
                }
                Told::Event(Event::Done { .. }) => it.done = true,
                Told::Event(Event::Failed { .. }) | Told::TimedOut(_) | Told::Woke(_) => {}
            }
            if ends {
                it.ends += 1;
                it.ended_at = Some(sim.now());
            }
        }
        if core.requests() == 0 && seen.len() == asked.len() {
            break;
        }
        sim.submit(&mut effects);
        sim.poll(&mut done, core.deadline());
        for completion in done.drain(..) {
            if core.owns(completion.tag) {
                core.complete(completion);
            } else {
                server.complete(completion, &mut effects);
            }
        }
    }
    for (i, (url, path)) in asked.iter().enumerate() {
        let id = RequestId(i as u64);
        let Some(it) = seen.get(&id) else { return Err(format!("seed {seed}: {url} told nothing")) };
        if it.ends != 1 || it.after_end {
            return Err(format!("seed {seed}: {url} ended {} times (told after its end: {})", it.ends, it.after_end));
        }
        let ended = it.ended_at.unwrap_or(Duration::MAX);
        if ended > TIMEOUT + SLACK {
            return Err(format!("seed {seed}: {url} ended at {ended:?}, past its deadline"));
        }
        let silent = path.starts_with("/silent");
        match (silent, it.done, server.sent.get(path)) {
            (true, true, _) => return Err(format!("seed {seed}: {url} is never answered, and was done")),
            (true, false, _) => {}
            (false, true, Some(body)) if it.status == Some(200) && it.body == *body => {}
            (false, true, sent) => {
                return Err(format!(
                    "seed {seed}: {url} was done with {:?} and {} bytes; the server sent {:?} bytes",
                    it.status,
                    it.body.len(),
                    sent.map(Vec::len)
                ));
            }
            (false, false, _) if faulty => {}
            (false, false, sent) => {
                return Err(format!("seed {seed}: {url} failed with no fault drawn; the server sent {:?} bytes", sent.map(Vec::len)));
            }
        }
    }
    Ok(())
}

fn faults(fail_one_in: u32) -> Faults {
    Faults { fail_one_in, max_latency: Duration::from_millis(2), ..Faults::default() }
}

/// The invariant: every request ends in exactly one outcome, within its deadline on the
/// virtual clock; one the server answered and that is done delivers exactly the bytes the
/// server sent; with no fault drawn, every answered request is done and the one never
/// answered fails by its timeout.
#[test]
fn every_request_ends_once_by_its_deadline_with_the_servers_bytes() {
    for seed in 0..SEEDS {
        run(seed, faults(0), Plant::None).unwrap();
        run(seed, faults(16), Plant::None).unwrap();
    }
}

/// Control: a receive's completion dropped leaves its request waiting for bytes that came;
/// the oracle sees it fail (or end short) where it should be done.
#[test]
fn the_oracle_catches_a_dropped_completion() {
    let caught = (0..SEEDS).filter(|seed| run(*seed, faults(0), Plant::DropCompletion { nth: 1 + seed % 3 }).is_err()).count();
    assert!(caught > 0, "a dropped receive completion went unseen under {SEEDS} seeds");
}

/// Control: with the deadline never armed, the silent request never ends (the simulation's
/// clock reaches no deadline it was not given), and the oracle says so.
#[test]
fn the_oracle_catches_a_deadline_never_armed() {
    let caught = (0..SEEDS).filter(|seed| run(*seed, faults(0), Plant::NoDeadline).is_err()).count();
    assert_eq!(caught as u64, SEEDS, "a deadline never armed went unseen");
}

/// The invariant: a request is timed from the step that starts it, wherever the client's clock
/// stood when it was asked. Here the driver's clock moves past a whole timeout with no step of
/// the loop's (as `serve`'s thread waits to a deadline, then reads its inbox), then a request
/// is asked of a port nothing listens on: it ends at once in the refused connect, naming the
/// address, the port and the step, and never in a timeout spent before it began.
///
/// This is the registry client's test on Linux (`requests_reuse_a_pooled_connection_and_a_
/// refused_one_says_where`), which failed with "no answer ... in 10s" when the driver's thread
/// slept to its old deadline: `Loop::ask` started the request on the client's clock as the last
/// step left it, so it was late by the whole wait. The control is the same run with the start
/// moved back into `Loop::ask`: the request then ends in `Kind::NoAnswer` at its first step.
#[test]
fn a_request_asked_after_a_long_wait_is_timed_from_its_start() {
    let refused = SocketAddr::from((Ipv4Addr::LOCALHOST, 9));
    let url = format!("http://{refused}/a");
    for seed in 0..SEEDS {
        let mut sim = Sim::new(seed, faults(0));
        let mut core = Loop::new(client(), 0, IN_FLIGHT_MOST);
        let mut effects = Vec::new();
        core.step(sim.now(), &mut effects);
        assert!(effects.is_empty(), "seed {seed}: an idle loop asked {effects:?}");
        // The driver's wait, on a timer of the test's own: past the client's whole timeout.
        let waited = TIMEOUT + SLACK;
        let mut waiting = vec![Effect::Timer { after: waited, tag: Tag(SERVER) }];
        sim.submit(&mut waiting);
        let mut done = Vec::new();
        sim.poll(&mut done, Deadline::NEVER);
        assert!(
            matches!(&done[..], [Finished { tag: Tag(SERVER), done: Done::Fired }]),
            "seed {seed}: the wait ended with {done:?}"
        );
        done.clear();
        let asked_at = sim.now();
        assert!(asked_at >= waited, "seed {seed}: the wait ended at {asked_at:?}");
        // Asked before the loop's next step, as a driver thread reads its inbox.
        core.ask(RequestId(0), Request::get(url.clone()), None);
        let mut told = Vec::new();
        for _ in 0..TURNS_MOST {
            core.step(sim.now(), &mut effects);
            while let Some(it) = core.next_told() {
                told.push((sim.now(), it));
            }
            if core.requests() == 0 {
                break;
            }
            sim.submit(&mut effects);
            sim.poll(&mut done, core.deadline());
            for completion in done.drain(..) {
                assert!(core.owns(completion.tag), "seed {seed}: {completion:?} is no effect of the loop's");
                core.complete(completion);
            }
        }
        let [(when, Told::Event(Event::Failed { request: RequestId(0), error }))] = &told[..] else {
            panic!("seed {seed}: the request told {told:?}, not one failure");
        };
        assert!(
            matches!(
                &error.kind,
                Kind::Io { step: Step::Connect, host, port: 9, why }
                    if host == "127.0.0.1" && why.contains("refused") && why.contains(&refused.to_string())
            ),
            "seed {seed}: the request failed with {error:?}, not the refused connect"
        );
        assert!(*when < asked_at + SLACK, "seed {seed}: the refusal came at {when:?}, asked at {asked_at:?}");
    }
}

/// Past `most` requests the rest wait in order, and an asker's deadline ends a request that
/// was never answered: [`Told::TimedOut`], once, at that deadline, and nothing after.
#[test]
fn requests_past_the_bound_wait_and_an_askers_deadline_ends_one() {
    let mut sim = Sim::new(7, faults(0));
    let mut core = Loop::new(client(), 0, 1);
    let mut server = Server::default();
    let mut effects = Vec::new();
    server.listen(&mut effects);
    sim.submit(&mut effects);
    let within = Duration::from_secs(1);
    core.ask(RequestId(0), Request::get("http://127.0.0.1:8080/silent/0"), Some(within));
    core.ask(RequestId(1), Request::get("http://127.0.0.1:8080/n/3/1"), None);
    let mut told = Vec::new();
    let mut done = Vec::new();
    for _ in 0..TURNS_MOST {
        core.step(sim.now(), &mut effects);
        while let Some(it) = core.next_told() {
            if let Told::Event(Event::Body { request, bytes }) = &it {
                core.consumed(*request, bytes.len());
            }
            told.push((sim.now(), it));
        }
        if core.requests() == 0 {
            break;
        }
        sim.submit(&mut effects);
        sim.poll(&mut done, core.deadline());
        for completion in done.drain(..) {
            if core.owns(completion.tag) {
                core.complete(completion);
            } else {
                server.complete(completion, &mut effects);
            }
        }
    }
    let first = told.iter().position(|(_, it)| matches!(it, Told::TimedOut(RequestId(0)))).expect("the first request timed out");
    let (when, _) = &told[first];
    assert!(*when >= within && *when < within + SLACK, "timed out at {when:?}");
    let second = told.iter().position(|(_, it)| matches!(it, Told::Event(Event::Head { request: RequestId(1), .. }))).expect("the second was answered");
    assert!(second > first, "the second started before the first ended: the bound was not held");
    assert!(told.iter().any(|(_, it)| matches!(it, Told::Event(Event::Done { request: RequestId(1) }))));
    assert_eq!(told.iter().filter(|(_, it)| matches!(it, Told::TimedOut(_))).count(), 1);
}

// --- `serve`, the driver thread's loop, under the simulation's waker -----------------------

/// A port nothing listens on in the simulation: a connect there is refused.
const REFUSED: u16 = 9;
/// The longest idle the requests are posted after: past [`TIMEOUT`], so a request timed from a
/// clock set before the wait starts with its timeout spent.
const GAP_MOST: Duration = Duration::from_secs(15);
/// What the oracle says of a request that ended past its timeout.
const LATE: &str = "past its timeout";
/// What the oracle says of a request that ended in something other than what it asked.
const WRONG: &str = "not its answer";
/// The cases a control may spend to catch its plant.
const CONTROL_CASES: u32 = 64;

/// A fault the serve test can be run with: a safeguard removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None,
    /// [`Sim::lose_wakes_while_parked`]: the io_uring driver's lost wake.
    LoseParkedWakes,
    /// [`Plant::StaleClock`]: the request started on the clock the last step left.
    StaleClock,
}

/// What one asked request is.
#[derive(Clone, Copy, Debug)]
enum Asked {
    /// The scripted server answers `len` bytes.
    Answered { len: usize },
    /// Nothing listens: the connect is refused.
    Refused,
}

/// The simulation, as `serve` drives it, stamping each request's end with the virtual time it
/// was told at: `serve` tells its askers, then submits, at one time.
struct Timed<'a> {
    sim: &'a mut Sim,
    from: &'a Receiver<(u64, Fetched)>,
    ended: BTreeMap<u64, (Duration, Fetched)>,
    /// A request told an end twice: the first such.
    twice: Option<u64>,
}

impl Timed<'_> {
    fn stamp(&mut self) {
        while let Ok((key, fetched)) = self.from.try_recv() {
            let now = self.sim.now();
            if self.ended.insert(key, (now, fetched)).is_some() {
                self.twice.get_or_insert(key);
            }
        }
    }
}

impl Driver for Timed<'_> {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        self.stamp();
        self.sim.submit(effects);
    }

    fn poll(&mut self, out: &mut Vec<Finished>, until: Deadline) -> usize {
        self.sim.poll(out, until)
    }

    fn in_flight(&self) -> usize {
        self.sim.in_flight()
    }

    fn now(&self) -> Duration {
        self.sim.now()
    }

    fn wall_ns(&self) -> u64 {
        Driver::wall_ns(&*self.sim)
    }

    fn boot(&self) -> Result<blaze_core::Boot, blaze_core::NoBoot> {
        Driver::boot(&*self.sim)
    }

    fn waker(&self) -> Option<Waker> {
        self.sim.waker()
    }
}

/// One case: `serve` itself, on the simulation with its waker handed out, a scripted HTTP/1.1
/// server as another process on the same machine; requests posted to its inbox by other
/// threads' wakes at drawn times, after drawn idles, then `Stop`; then the oracle.
fn served_run(tape: &Tape, fault: Fault) -> Result<(), Broke> {
    let faults = Faults { max_latency: Duration::from_micros(tape.int_in(0..=2_000u64, "max latency us")), ..Faults::default() };
    let mut sim = Sim::new(tape.next_u64("the simulation's seed"), faults);
    sim.give_wakers();
    if fault == Fault::LoseParkedWakes {
        sim.lose_wakes_while_parked();
    }
    let Some(waker) = sim.waker() else {
        return Err(Broke::Harness("Sim::give_wakers gave no waker".to_owned()));
    };
    let server = Arc::new_in(Lock::new("test.server", Server::default()), Global);
    // clone: the server, for the process that runs it
    let running = Arc::clone(&server);
    sim.other_process(SERVER..u64::MAX, move |_, completion, effects| running.lock().complete(completion, effects));
    let mut effects = Vec::new_in(Global);
    server.lock().listen(&mut effects);
    sim.submit(&mut effects);
    let inbox: Arc<Inbox, Global> = Arc::new_in(Inbox::new(waker), Global);
    let (to, from) = chan::unbounded::<(u64, Fetched)>();
    let asked = tape.vec(1..=8, "requests", |tape| {
        let gap = Duration::from_micros(tape.int_in(0..=GAP_MOST.as_micros() as u64, "idle before it, us"));
        let asked = match tape.index(3, "what it asks") {
            0 => Asked::Answered { len: 10 },
            1 => Asked::Answered { len: 70_000 },
            _ => Asked::Refused,
        };
        (gap, asked)
    });
    let mut posted = Vec::new_in(Global);
    let mut at = Duration::ZERO;
    for (i, (gap, what)) in asked.iter().enumerate() {
        at += *gap;
        if *gap > TIMEOUT {
            tape.reached("a request was posted after an idle past its timeout");
        }
        let url = match what {
            Asked::Answered { len } => format!("http://127.0.0.1:8080/n/{len}/{i}"),
            Asked::Refused => format!("http://127.0.0.1:{REFUSED}/r/{i}"),
        };
        // clone: the inbox, the answer's sender and the address, for the other thread's act
        let (inbox, to, asking) = (Arc::clone(&inbox), to.clone(), url.clone());
        let key = i as u64;
        sim.wake_at(at, move || {
            let asker = Asker::Collect { key, to: super::CollectTo::Direct(to), head: None, body: Vec::new_in(Global) };
            inbox.post(Message::Ask { id: RequestId(key), request: Request::get(asking), within: None, asker });
        });
        posted.push((at, *what, url));
    }
    drop(to);
    // Every request has ended by then, its timeout and the simulation's latency past its post.
    let stop_at = at + TIMEOUT + SLACK * 2;
    // clone: the inbox, for the act that stops the loop
    let stopping = Arc::clone(&inbox);
    sim.wake_at(stop_at, move || stopping.post(Message::Stop));
    // A sleep of the loop's own past the stop: with every wake lost, the loop still turns, reads
    // the stop and returns, so a lost wake is a late request the oracle names, never a hang.
    let (woke, _sleeping) = chan::bounded(1);
    inbox.post(Message::Sleep { after: stop_at + TIMEOUT * 3, woke });
    let mut core = Loop::new(client(), 0, IN_FLIGHT_MOST);
    if fault == Fault::StaleClock {
        core.plant(Plant::StaleClock);
    }
    let mut timed = Timed { sim: &mut sim, from: &from, ended: BTreeMap::new_in(Global), twice: None };
    serve(&mut timed, core, &inbox);
    timed.stamp();
    let Timed { ended, twice, .. } = timed;
    if let Some(key) = twice {
        return Err(Broke::Oracle(format!("request {key} told two ends")));
    }
    if sim.wakes_landed().parked > 0 {
        tape.reached("a wake landed while the loop was parked");
    }
    let server = server.lock();
    for (i, (at, what, url)) in posted.iter().enumerate() {
        let Some((when, fetched)) = ended.get(&(i as u64)) else {
            return Err(Broke::Oracle(format!("{url}, posted at {at:?}, never ended: {LATE}")));
        };
        if *when > *at + TIMEOUT + SLACK {
            return Err(Broke::Oracle(format!("{url}, posted at {at:?}, ended at {when:?}: {LATE}")));
        }
        match (what, fetched) {
            (Asked::Answered { len }, Fetched::Answered { status: 200, body, .. })
                if server.sent.get(&format!("/n/{len}/{i}")) == Some(body) =>
            {
                tape.reached("a request was answered");
            }
            (Asked::Refused, Fetched::Failed(error))
                if matches!(&error.kind, Kind::Io { step: Step::Connect, port: REFUSED, why, .. } if why.contains("refused")) =>
            {
                tape.reached("a request was refused");
            }
            (_, fetched) => {
                return Err(Broke::Oracle(format!("{url}, posted at {at:?}, ended at {when:?} in {fetched:?}: {WRONG}")));
            }
        }
    }
    Ok(())
}

/// The invariant: every request posted to `serve`'s inbox at virtual time t, with a wake of its
/// driver, ends by t plus its timeout, with the server's bytes or the refusal of its connect,
/// whatever the seed places each wake at (before the poll looks, between its look and its
/// park, or while it is parked) and however long the loop was idle before it. `serve` runs as
/// on its thread, on the simulation's waker (`blaze_core::sim::Sim::give_wakers`), the server
/// a second process on the machine.
#[test]
fn every_request_posted_to_serve_ends_by_its_timeout_wherever_its_wake_lands() {
    let config = SimConfig::cases(128).must_reach(&[
        "a request was answered",
        "a request was refused",
        "a wake landed while the loop was parked",
        "a request was posted after an idle past its timeout",
    ]);
    blaze_sim::check(config, |tape| served_run(tape, Fault::None));
}

/// Control: a wake that lands while the driver is parked is lost (the io_uring driver before
/// its fix); a request posted then waits for the loop's next timer, and ends past its timeout.
#[test]
fn the_oracle_catches_a_wake_lost_while_the_driver_is_parked() {
    blaze_sim::control(SimConfig::cases(CONTROL_CASES), Fault::None, Fault::LoseParkedWakes, LATE, served_run);
}

/// Control: the order before the fix (the request started in `Loop::ask` on the client's clock
/// as the last step left it, and `serve` not advancing the loop as a wait ends); a request
/// posted after an idle past its timeout ends at once in "no answer", not its answer.
#[test]
fn the_oracle_catches_a_request_timed_from_a_clock_set_before_the_wait() {
    blaze_sim::control(SimConfig::cases(CONTROL_CASES), Fault::None, Fault::StaleClock, WRONG, served_run);
}

// --- the stepped driver, read through its handle's answer -------------------------------------

/// How long a test's receive waits for what is already there once `serve` has returned: never
/// reached.
const THERE: Duration = Duration::from_secs(5);
/// Seeds each of these cases runs under.
const SERVED_SEEDS: u64 = 8;

/// The scripted server on `sim` as another process, listening; the inbox `serve` drains, on
/// the simulation's waker; a sleep of the loop's own at `guard` (so the loop turns then,
/// whatever else it waits for); and the stop at `stop`.
fn served(sim: &mut Sim, guard: Duration, stop: Duration) -> Arc<Inbox, Global> {
    sim.give_wakers();
    let Some(waker) = sim.waker() else { panic!("Sim::give_wakers gave no waker") };
    let server = Arc::new_in(Lock::new("test.server", Server::default()), Global);
    // clone: the server, for the process that runs it
    let running = Arc::clone(&server);
    sim.other_process(SERVER..u64::MAX, move |_, completion, effects| running.lock().complete(completion, effects));
    let mut effects = Vec::new_in(Global);
    server.lock().listen(&mut effects);
    sim.submit(&mut effects);
    let inbox: Arc<Inbox, Global> = Arc::new_in(Inbox::new(waker), Global);
    // The sleeper goes at once: its timer is the loop's guard, its wake no one's.
    let (woke, _sleeping) = chan::bounded(1);
    inbox.post(Message::Sleep { after: guard, woke });
    // clone: the inbox, for the act that stops the loop
    let stopping = Arc::clone(&inbox);
    sim.wake_at(stop, move || stopping.post(Message::Stop));
    inbox
}

/// `url` asked through `serve` on `sim`, its events to an [`Answer`]: once `serve` has
/// returned, every event the request told is in the answer's channel.
fn asked_through_serve(sim: &mut Sim, url: &str) -> Answer {
    let inbox = served(sim, TIMEOUT * 3, TIMEOUT * 4);
    let (events, from_loop) = chan::unbounded();
    inbox.post(Message::Ask { id: RequestId(0), request: Request::get(url), within: None, asker: Asker::Events(events) });
    serve(&mut *sim, Loop::new(client(), 0, IN_FLIGHT_MOST), &inbox);
    Answer { id: RequestId(0), from_loop, inbox, taken: 0, over: false }
}

/// `/silent/0` asked through `serve` on the simulation under `seed`, `plant` in its loop: when
/// (on the virtual clock) and how it ended; `None` when it never did.
fn silent_run(seed: u64, plant: Plant) -> Option<(Duration, Fetched)> {
    let mut sim = Sim::new(seed, faults(0));
    let inbox = served(&mut sim, TIMEOUT * 3, TIMEOUT * 4);
    let (to, from) = chan::unbounded::<(u64, Fetched)>();
    let asker = Asker::Collect { key: 0, to: super::CollectTo::Direct(to), head: None, body: Vec::new_in(Global) };
    inbox.post(Message::Ask { id: RequestId(0), request: Request::get("http://127.0.0.1:8080/silent/0"), within: None, asker });
    let mut core = Loop::new(client(), 0, IN_FLIGHT_MOST);
    core.plant(plant);
    let mut timed = Timed { sim: &mut sim, from: &from, ended: BTreeMap::new_in(Global), twice: None };
    serve(&mut timed, core, &inbox);
    timed.stamp();
    timed.ended.remove(&0)
}

/// A request through the stepped driver (`serve` on the simulation, the scripted server its
/// peer) is read whole through its handle's answer, one timed receive per event: the head, the
/// server's bytes, the end, then the answer's end once the driver is gone. Prints nothing; a
/// wrong driver panics with "seed N: the answer read <what> before its end".
#[test]
fn a_request_through_the_stepped_driver_is_read_whole_through_its_answer() {
    for seed in 0..SERVED_SEEDS {
        let mut sim = Sim::new(seed, faults(0));
        let mut answer = asked_through_serve(&mut sim, "http://127.0.0.1:8080/n/10/0");
        let (mut status, mut body, mut done) = (None, Vec::new(), false);
        while !done {
            match answer.next(THERE) {
                Ok(Some(Event::Head { status: it, .. })) => status = Some(it),
                Ok(Some(Event::Body { bytes, .. })) => body.extend_from_slice(&bytes),
                Ok(Some(Event::Done { .. })) => done = true,
                other => panic!("seed {seed}: the answer read {other:?} before its end"),
            }
        }
        assert_eq!((status, body), (Some(200), body_of(10)), "seed {seed}");
        assert!(matches!(answer.next(THERE), Ok(None)), "seed {seed}: the answer read past its end");
    }
}

/// A deadline that passes with no bytes is a typed timeout naming what was waited for, at both
/// waits: the client's, on the simulation's clock (a request the server never answers ends in
/// `Kind::NoAnswer` naming the host and port, by its timeout), and an asker's (a receive on an
/// answer nothing is told on ends at its `within` in `Unanswered::TimedOut` naming
/// [`NEXT_EVENT`]). Prints nothing; a wrong one panics with "seed N: it ended in <what>".
#[test]
fn a_deadline_that_passes_with_no_bytes_is_the_typed_timeout_naming_what_was_waited_for() {
    for seed in 0..SERVED_SEEDS {
        let Some((when, fetched)) = silent_run(seed, Plant::None) else { panic!("seed {seed}: the silent request never ended") };
        assert!(when >= TIMEOUT && when <= TIMEOUT + SLACK, "seed {seed}: it ended at {when:?}");
        assert!(
            matches!(&fetched, Fetched::Failed(error) if matches!(&error.kind, Kind::NoAnswer { host, port: 8080, .. } if host == "127.0.0.1")),
            "seed {seed}: it ended in {fetched:?}"
        );
    }
    let mut sim = Sim::new(0, faults(0));
    sim.give_wakers();
    let Some(waker) = sim.waker() else { panic!("Sim::give_wakers gave no waker") };
    let (told, from_loop) = chan::unbounded::<Event>();
    let inbox: Arc<Inbox, Global> = Arc::new_in(Inbox::new(waker), Global);
    let mut answer = Answer { id: RequestId(0), from_loop, inbox, taken: 0, over: false };
    let within = Duration::from_millis(5);
    assert_eq!(answer.next(within).map(|_| ()), Err(Unanswered::TimedOut { what: NEXT_EVENT, within }));
    drop(told);
    assert!(matches!(answer.next(within), Ok(None)), "an answer whose driver is gone did not read its end");
}

/// A peer that closes mid-response is the typed error: the server sends the head and half the
/// body its `Content-Length` promised, then hangs up; the answer reads the head, at most that
/// half, then `Event::Failed` in `Kind::Io { step: Step::Body, .. }` naming the host and port.
/// Prints nothing; a wrong one panics with "seed N: it failed with <what>".
#[test]
fn a_peer_that_closes_mid_response_is_the_typed_error() {
    for seed in 0..SERVED_SEEDS {
        let mut sim = Sim::new(seed, faults(0));
        let mut answer = asked_through_serve(&mut sim, "http://127.0.0.1:8080/cut/1000/0");
        let mut body = Vec::new();
        let failed = loop {
            match answer.next(THERE) {
                Ok(Some(Event::Head { status: 200, .. })) => {}
                Ok(Some(Event::Body { bytes, .. })) => body.extend_from_slice(&bytes),
                Ok(Some(Event::Failed { error, .. })) => break error,
                other => panic!("seed {seed}: the answer read {other:?}, not the failure of a body cut short"),
            }
        };
        assert!(
            matches!(&failed.kind, Kind::Io { step: Step::Body, host, port: 8080, .. } if host == "127.0.0.1"),
            "seed {seed}: it failed with {failed:?}"
        );
        assert!(body.len() <= 500 && body_of(1000).starts_with(&body), "seed {seed}: it read {} bytes of the half sent", body.len());
    }
}

/// Control: the loop's wait given no deadline ([`Plant::NoDeadline`]: no timer armed at the
/// client's deadline, the driver's poll to `Deadline::NEVER`) leaves the silent request
/// waiting past its timeout: it ends only when the loop's guard sleep turns the loop, at three
/// timeouts, or never, which the case above refuses. Prints nothing; a planted wait the oracle
/// misses panics with "seed N: a wait with no deadline went unseen".
#[test]
fn the_oracle_catches_a_wait_with_no_deadline() {
    for seed in 0..SERVED_SEEDS {
        let ended = silent_run(seed, Plant::NoDeadline);
        assert!(
            ended.as_ref().is_none_or(|(when, _)| *when > TIMEOUT + SLACK),
            "seed {seed}: a wait with no deadline went unseen: {ended:?}"
        );
    }
}
