//! [`Loop`] under blaze-core's deterministic simulation: a scripted HTTP/1.1 server on the
//! same [`Sim`] (its own tags, past the loop's), simulated TCP between them, a name from the
//! simulation's table, completions delayed and shuffled by the seed, and with faults on, failed
//! listens, connects, resolves, sends and receives. No socket, thread or real clock.

use std::{
    alloc::Global,
    collections::{BTreeMap, VecDeque},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};

use blaze_core::{
    Address, Completion as Finished, Conn, Done, Driver as _, Effect, Tag,
    sim::{Faults, Sim},
};

use super::{IN_FLIGHT_MOST, Loop, Plant, Told};
use crate::{Client, Config, Event, Request, RequestId};

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
}

/// The scripted server: `/n/<len>/<i>` is answered with a body of `len` bytes drawn from the
/// path, kept alive; `/silent/<i>` is never answered.
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
        effects.push(Effect::Listen { at: Address::Tcp(at()), tag });
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

    fn answer(&mut self, path: &str) -> Option<Vec<u8>> {
        let mut parts = path.trim_start_matches('/').split('/');
        if parts.next()? != "n" {
            return None;
        }
        let len: usize = parts.next()?.parse().ok()?;
        let body: Vec<u8> = (0..len).map(|i| (i.wrapping_mul(31) ^ len) as u8).collect();
        let mut bytes = format!("HTTP/1.1 200 OK\r\nContent-Length: {len}\r\n\r\n").into_bytes();
        bytes.extend_from_slice(&body);
        self.sent.insert(path.to_owned(), body);
        Some(bytes)
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
                    if let Some(bytes) = self.answer(&path)
                        && let Some(peer) = self.peers.get_mut(&conn)
                    {
                        peer.queue.push_back(bytes);
                    }
                }
                self.pump(conn, effects);
                self.recv(conn, effects);
            }
            (Did::Send(conn), Done::Sent(_)) => {
                if let Some(peer) = self.peers.get_mut(&conn) {
                    peer.sending = false;
                }
                self.pump(conn, effects);
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
