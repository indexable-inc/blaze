//! The WebTransport probe's echo (`blaze-explore-probe`), a synchronous state machine over
//! blaze-http's HTTP/3 core ([`Endpoint`]) with no socket, thread, timer or runtime in it: it
//! accepts every session, and echoes each bidirectional stream on itself ([`Bidi`]), each
//! unidirectional stream on one it opens ([`Uni`]) and each datagram ([`Datagrams`]). Each of
//! the three keeps a [`Tally`] per session, and a session's end hands back all three
//! ([`Ended`]), so the probe's exit says which failed. Lines to print come out of
//! [`Echo::take_said`]: what each CONNECT carried (path, Origin, User-Agent), so a run also
//! says what Origin each browser sends, and what each stream and datagram echoed.
//!
//! The echo holds back what it has not handed on: a stream's bytes are said taken
//! ([`Endpoint::consumed`]) only once QUIC took their echo ([`Event::StreamSent`]), so the
//! endpoint reads at most its window ([`blaze_http::h3::WINDOW`]) ahead of a page that reads
//! its echo slowly, and a unidirectional stream's bytes wait (at most a window) for the
//! probe's own stream when the page allows none yet. It has no deadline of its own: a stalled
//! stream ends with its connection's idle timeout, which the HTTP/3 driver loop keeps.
//!
//! # The floor, and what this costs against it
//!
//! The floor of an echo is handing each piece back once. A piece here is one map lookup by its
//! stream (O(log n), n the streams open), one copy into the endpoint's send buffer, and one
//! more lookup at its [`Event::StreamSent`]; a unidirectional stream's bytes that come before
//! its echo's stream opens are copied once more. The machine asks for no thread, timer or wake
//! per input: the loop that runs it adds nothing to the HTTP/3 driver loop's own poll.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::{alloc::Global, collections::BTreeMap, net::SocketAddr};

use blaze_http::h3::{Closed, Direction, Endpoint, Event, SessionId, StreamRef};

/// What one part of a session echoed: whole streams (or datagrams) echoed, their bytes, and
/// those that failed (reset or stopped by the page, refused by the endpoint, left unfinished by
/// the session's end).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    pub echoed: u64,
    pub bytes: u64,
    pub failed: u64,
}

/// A session that ended, and what each part of it echoed.
#[derive(Debug)]
pub struct Ended {
    pub session: SessionId,
    pub peer: SocketAddr,
    pub closed: Closed,
    pub bidi: Tally,
    pub uni: Tally,
    pub datagrams: Tally,
}

impl Ended {
    /// The parts that failed at least once, by name.
    pub fn failed(&self) -> impl Iterator<Item = (&'static str, Tally)> {
        [("bidirectional streams", self.bidi), ("unidirectional streams", self.uni), ("datagrams", self.datagrams)]
            .into_iter()
            .filter(|(_, it)| it.failed > 0)
    }
}

/// A test-only switch: a safeguard removed, which the simulation's oracle must catch.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    None,
    /// No stream's bytes are ever said taken: a stream longer than the window stalls.
    NoConsume,
    /// A bidirectional stream's end is not echoed: its echo never finishes.
    DropFin,
}

type Tallies = BTreeMap<SessionId, Tally, Global>;

fn tally(tallies: &mut Tallies, session: SessionId) -> &mut Tally {
    tallies.entry(session).or_default()
}

/// A bidirectional stream being echoed on itself.
struct Copying {
    session: SessionId,
    bytes: u64,
    /// Bytes echoed that QUIC has not yet taken, so not yet said taken.
    held: usize,
}

/// The bidirectional streams' echo.
struct Bidi {
    streams: BTreeMap<StreamRef, Copying, Global>,
    tallies: Tallies,
}

impl Bidi {
    fn new() -> Bidi {
        Bidi { streams: BTreeMap::new_in(Global), tallies: BTreeMap::new_in(Global) }
    }

    fn opened(&mut self, session: SessionId, stream: StreamRef) {
        self.streams.insert(stream, Copying { session, bytes: 0, held: 0 });
    }

    fn data(&mut self, stream: StreamRef, bytes: &[u8], endpoint: &mut Endpoint, said: &mut Vec<String, Global>) {
        let Some(copy) = self.streams.get_mut(&stream) else { return };
        if let Err(why) = endpoint.send(stream, bytes) {
            let why = format!("its echo was refused: {why}");
            self.fail(stream, &why, endpoint, said);
            return;
        }
        copy.bytes += bytes.len() as u64;
        copy.held += bytes.len();
    }

    fn sent(&mut self, stream: StreamRef, endpoint: &mut Endpoint, consume: bool) {
        let Some(copy) = self.streams.get_mut(&stream) else { return };
        if consume {
            endpoint.consumed(stream, copy.held);
        }
        copy.held = 0;
    }

    fn finished(&mut self, stream: StreamRef, endpoint: &mut Endpoint, said: &mut Vec<String, Global>) {
        let Some(copy) = self.streams.remove(&stream) else { return };
        let tally = tally(&mut self.tallies, copy.session);
        match endpoint.finish(stream) {
            Ok(()) => {
                tally.echoed += 1;
                tally.bytes += copy.bytes;
                said.push(format!("bidirectional stream: {} bytes echoed", copy.bytes));
            }
            Err(why) => {
                tally.failed += 1;
                said.push(format!("bidirectional stream: not finished after {} bytes echoed: {why}", copy.bytes));
            }
        }
    }

    /// The stream failed (`why`): both its sides end.
    fn fail(&mut self, stream: StreamRef, why: &str, endpoint: &mut Endpoint, said: &mut Vec<String, Global>) {
        let Some(copy) = self.streams.remove(&stream) else { return };
        // A side already ended has nothing left to end.
        _ = endpoint.reset(stream, 0);
        _ = endpoint.stop(stream, 0);
        tally(&mut self.tallies, copy.session).failed += 1;
        said.push(format!("bidirectional stream: failed after {} bytes echoed: {why}", copy.bytes));
    }

    /// The session's tally; a stream it left unfinished failed.
    fn close(&mut self, session: SessionId) -> Tally {
        let before = self.streams.len();
        self.streams.retain(|_, it| it.session != session);
        let mut tally = self.tallies.remove(&session).unwrap_or_default();
        tally.failed += (before - self.streams.len()) as u64;
        tally
    }
}

/// A unidirectional stream of the page's being echoed on one of the probe's.
struct Pair {
    session: SessionId,
    /// The probe's stream, once the page allows it.
    ours: Option<StreamRef>,
    /// Bytes come before `ours` opened.
    pending: Vec<u8, Global>,
    held: usize,
    bytes: u64,
    /// The page finished its stream.
    fin: bool,
}

/// The unidirectional streams' echo.
struct Uni {
    /// By the page's stream.
    pairs: BTreeMap<StreamRef, Pair, Global>,
    /// The probe's streams, each to the page's it echoes.
    ours: BTreeMap<StreamRef, StreamRef, Global>,
    tallies: Tallies,
    /// The page's streams waiting for one of the probe's, its buffer kept.
    waiting: Vec<StreamRef, Global>,
}

impl Uni {
    fn new() -> Uni {
        Uni {
            pairs: BTreeMap::new_in(Global),
            ours: BTreeMap::new_in(Global),
            tallies: BTreeMap::new_in(Global),
            waiting: Vec::new_in(Global),
        }
    }

    fn opened(&mut self, session: SessionId, peer: StreamRef, endpoint: &mut Endpoint, said: &mut Vec<String, Global>) {
        let pair = Pair { session, ours: None, pending: Vec::new_in(Global), held: 0, bytes: 0, fin: false };
        self.pairs.insert(peer, pair);
        self.open(peer, endpoint, said);
    }

    /// Opens the probe's stream for the page's `peer`, if the page allows one now.
    fn open(&mut self, peer: StreamRef, endpoint: &mut Endpoint, said: &mut Vec<String, Global>) {
        let Some(pair) = self.pairs.get_mut(&peer) else { return };
        if pair.ours.is_some() {
            return;
        }
        let ours = match endpoint.open(pair.session, Direction::Uni) {
            Ok(Some(ours)) => ours,
            // Event::StreamsAvailable says when to ask again.
            Ok(None) => return,
            Err(why) => {
                let why = format!("no stream of the probe's opened: {why}");
                self.fail(peer, &why, endpoint, said);
                return;
            }
        };
        pair.ours = Some(ours);
        self.ours.insert(ours, peer);
        let pending = std::mem::replace(&mut pair.pending, Vec::new_in(Global));
        let fin = pair.fin;
        if !pending.is_empty()
            && let Err(why) = endpoint.send(ours, &pending)
        {
            let why = format!("its echo was refused: {why}");
            self.fail(peer, &why, endpoint, said);
            return;
        }
        if fin {
            self.finish(peer, endpoint, said);
        }
    }

    /// The page allows more streams in `session`: each of its streams waiting gets one.
    fn available(&mut self, session: SessionId, endpoint: &mut Endpoint, said: &mut Vec<String, Global>) {
        let mut waiting = std::mem::replace(&mut self.waiting, Vec::new_in(Global));
        waiting.clear();
        waiting.extend(self.pairs.iter().filter(|(_, it)| it.session == session && it.ours.is_none()).map(|(peer, _)| *peer));
        for peer in waiting.drain(..) {
            self.open(peer, endpoint, said);
        }
        self.waiting = waiting;
    }

    fn data(&mut self, peer: StreamRef, bytes: &[u8], endpoint: &mut Endpoint, said: &mut Vec<String, Global>) {
        let Some(pair) = self.pairs.get_mut(&peer) else { return };
        pair.bytes += bytes.len() as u64;
        pair.held += bytes.len();
        let Some(ours) = pair.ours else {
            pair.pending.extend_from_slice(bytes);
            return;
        };
        if let Err(why) = endpoint.send(ours, bytes) {
            let why = format!("its echo was refused: {why}");
            self.fail(peer, &why, endpoint, said);
        }
    }

    /// QUIC took what was sent on the probe's stream `ours`.
    fn sent(&mut self, ours: StreamRef, endpoint: &mut Endpoint, consume: bool) {
        let Some(peer) = self.ours.get(&ours) else { return };
        let Some(pair) = self.pairs.get_mut(peer) else { return };
        if consume {
            endpoint.consumed(*peer, pair.held);
        }
        pair.held = 0;
    }

    fn finished(&mut self, peer: StreamRef, endpoint: &mut Endpoint, said: &mut Vec<String, Global>) {
        let Some(pair) = self.pairs.get_mut(&peer) else { return };
        pair.fin = true;
        if pair.ours.is_some() {
            self.finish(peer, endpoint, said);
        }
    }

    /// The page's `peer` finished and its echo's stream is open: the echo finishes.
    fn finish(&mut self, peer: StreamRef, endpoint: &mut Endpoint, said: &mut Vec<String, Global>) {
        let Some(pair) = self.pairs.remove(&peer) else { return };
        let Some(ours) = pair.ours else { return };
        self.ours.remove(&ours);
        let tally = tally(&mut self.tallies, pair.session);
        match endpoint.finish(ours) {
            Ok(()) => {
                tally.echoed += 1;
                tally.bytes += pair.bytes;
                said.push(format!("unidirectional stream: {} bytes echoed on one the probe opened", pair.bytes));
            }
            Err(why) => {
                tally.failed += 1;
                said.push(format!("unidirectional stream: not finished after {} bytes echoed: {why}", pair.bytes));
            }
        }
    }

    /// The page's `peer` failed (`why`): it is stopped and its echo reset.
    fn fail(&mut self, peer: StreamRef, why: &str, endpoint: &mut Endpoint, said: &mut Vec<String, Global>) {
        let Some(pair) = self.pairs.remove(&peer) else { return };
        // A side already ended has nothing left to end.
        _ = endpoint.stop(peer, 0);
        if let Some(ours) = pair.ours {
            self.ours.remove(&ours);
            _ = endpoint.reset(ours, 0);
        }
        tally(&mut self.tallies, pair.session).failed += 1;
        said.push(format!("unidirectional stream: failed after {} bytes: {why}", pair.bytes));
    }

    /// The page stopped the probe's stream `ours`.
    fn stopped(&mut self, ours: StreamRef, endpoint: &mut Endpoint, said: &mut Vec<String, Global>) {
        let Some(&peer) = self.ours.get(&ours) else { return };
        self.fail(peer, "the page stopped its echo", endpoint, said);
    }

    fn close(&mut self, session: SessionId) -> Tally {
        let before = self.pairs.len();
        self.pairs.retain(|_, it| it.session != session);
        self.ours.retain(|_, peer| self.pairs.contains_key(peer));
        let mut tally = self.tallies.remove(&session).unwrap_or_default();
        tally.failed += (before - self.pairs.len()) as u64;
        tally
    }
}

/// The datagrams' echo.
struct Datagrams {
    tallies: Tallies,
}

impl Datagrams {
    fn datagram(&mut self, session: SessionId, bytes: &[u8], endpoint: &mut Endpoint, said: &mut Vec<String, Global>) {
        let tally = tally(&mut self.tallies, session);
        match endpoint.send_datagram(session, bytes) {
            Ok(()) => {
                tally.echoed += 1;
                tally.bytes += bytes.len() as u64;
                said.push(format!("datagram: {} bytes echoed", bytes.len()));
            }
            Err(why) => {
                tally.failed += 1;
                said.push(format!("datagram of {} bytes not echoed: {why}", bytes.len()));
            }
        }
    }

    fn close(&mut self, session: SessionId) -> Tally {
        self.tallies.remove(&session).unwrap_or_default()
    }
}

/// The probe's echo: see the module's doc.
pub struct Echo {
    peers: BTreeMap<SessionId, SocketAddr, Global>,
    bidi: Bidi,
    uni: Uni,
    datagrams: Datagrams,
    said: Vec<String, Global>,
    ended: Vec<Ended, Global>,
    #[cfg(test)]
    plant: Plant,
}

impl Default for Echo {
    fn default() -> Echo {
        Echo::new()
    }
}

impl Echo {
    pub fn new() -> Echo {
        Echo {
            peers: BTreeMap::new_in(Global),
            bidi: Bidi::new(),
            uni: Uni::new(),
            datagrams: Datagrams { tallies: BTreeMap::new_in(Global) },
            said: Vec::new_in(Global),
            ended: Vec::new_in(Global),
            #[cfg(test)]
            plant: Plant::None,
        }
    }

    #[cfg(test)]
    pub(crate) fn plant(&mut self, plant: Plant) {
        self.plant = plant;
    }

    fn consume(&self) -> bool {
        #[cfg(test)]
        if self.plant == Plant::NoConsume {
            return false;
        }
        true
    }

    /// Takes every event the endpoint has, including those the echo's own sends make.
    pub fn step(&mut self, endpoint: &mut Endpoint) {
        let consume = self.consume();
        while let Some(event) = endpoint.next_event() {
            let said = &mut self.said;
            match event {
                Event::SessionRequest { session, request } => {
                    said.push(format!(
                        "CONNECT {} from {}: Origin {:?}, User-Agent {:?}",
                        request.path, request.peer, request.origin, request.user_agent
                    ));
                    match endpoint.accept(session) {
                        Ok(()) => {
                            self.peers.insert(session, request.peer);
                        }
                        Err(why) => said.push(format!("the session did not open: {why}")),
                    }
                }
                Event::StreamOpened { session, stream } => match stream.direction() {
                    Direction::Bi => self.bidi.opened(session, stream),
                    Direction::Uni => self.uni.opened(session, stream, endpoint, said),
                },
                Event::StreamData { stream, bytes } => match stream.direction() {
                    Direction::Bi => self.bidi.data(stream, &bytes, endpoint, said),
                    Direction::Uni => self.uni.data(stream, &bytes, endpoint, said),
                },
                Event::StreamFinished { stream } => match stream.direction() {
                    Direction::Bi => {
                        #[cfg(test)]
                        if self.plant == Plant::DropFin {
                            continue;
                        }
                        self.bidi.finished(stream, endpoint, said);
                    }
                    Direction::Uni => self.uni.finished(stream, endpoint, said),
                },
                Event::StreamReset { stream, code } => {
                    let why = format!("the page reset it (code {code:?})");
                    match stream.direction() {
                        Direction::Bi => self.bidi.fail(stream, &why, endpoint, said),
                        Direction::Uni => self.uni.fail(stream, &why, endpoint, said),
                    }
                }
                Event::StreamStopped { stream, code } => match stream.direction() {
                    Direction::Bi => {
                        self.bidi.fail(stream, &format!("the page stopped its echo (code {code:?})"), endpoint, said);
                    }
                    Direction::Uni => self.uni.stopped(stream, endpoint, said),
                },
                Event::StreamSent { stream } => match stream.direction() {
                    Direction::Bi => self.bidi.sent(stream, endpoint, consume),
                    Direction::Uni => self.uni.sent(stream, endpoint, consume),
                },
                // An echo is done once QUIC took it: nothing waits on the page's
                // acknowledgement (the session's end, the page's, comes after its reads).
                Event::StreamAcked { .. } => {}
                Event::StreamsAvailable { session } => self.uni.available(session, endpoint, said),
                Event::Datagram { session, bytes } => self.datagrams.datagram(session, &bytes, endpoint, said),
                Event::SessionClosed { session, closed } => {
                    let Some(peer) = self.peers.remove(&session) else { continue };
                    said.push(format!("session ended: {closed}"));
                    self.ended.push(Ended {
                        session,
                        peer,
                        closed,
                        bidi: self.bidi.close(session),
                        uni: self.uni.close(session),
                        datagrams: self.datagrams.close(session),
                    });
                }
                // A server is told of no session it opened; a connection's end comes as its
                // sessions' ends; a request the core refused itself was answered by it.
                Event::SessionOpened { .. }
                | Event::ConnectionClosed { .. }
                | Event::Refused { .. }
                | Event::Ticket { .. } => {}
                Event::Request { .. }
                | Event::ResponseHead { .. }
                | Event::ResponseData { .. }
                | Event::RequestEnded { .. } => unreachable!(
                    "the probe's endpoint takes no plain request (`ServerConfig::requests` is false) and asks none"
                ),
            }
        }
    }

    /// Moves the lines to print to `out`.
    pub fn take_said(&mut self, out: &mut Vec<String>) {
        out.append(&mut self.said);
    }

    /// Moves the sessions that ended to `out`.
    pub fn take_ended(&mut self, out: &mut Vec<Ended>) {
        out.append(&mut self.ended);
    }
}

#[cfg(test)]
mod tests;
