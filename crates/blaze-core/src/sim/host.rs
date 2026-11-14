//! What the simulation is of the host beyond its files and processes: UDP sockets
//! ([`Effect::Bind`], [`Effect::SendTo`], [`Effect::RecvFrom`], [`Effect::Unbind`]), the
//! standard streams ([`Effect::ReadStdin`], [`Effect::WriteOut`]), the wall clock
//! ([`Effect::Now`]), random bytes ([`Effect::Random`]) and directories made empty
//! ([`Effect::MakeDir`], [`Effect::RemoveDir`]).
//!
//! A datagram sent is a copy on its way per copy the network makes: lost once in
//! [`Datagrams::lose_one_in`], doubled once in [`Datagrams::duplicate_one_in`], and each copy
//! delayed on its own up to [`Datagrams::max_delay`], so two sent one after the other arrive in
//! either order. One sent to an address the test cut off ([`Sim::black_hole`]) is lost, every
//! one, as on a path that drops everything. One arrives at the socket bound at its address, or
//! at the port on the unspecified address of its family; one nothing is bound for is gone, as
//! UDP's is. A datagram keeps its ECN codepoint on its way (a receiving socket bound without
//! [`dgram::Options::ecn`] reads none); one longer than its receive's slot arrives cut short and
//! said to be; a socket bound to coalesce gets runs of datagrams of one length from one peer as
//! one run, as Linux's `UDP_GRO` hands them up. A batch sent is its datagrams, each on its own
//! way.
//!
//! A name resolves from the table the test gives ([`Sim::name`]); an address resolves to itself,
//! and a name not in the table fails, naming it. The test
//! scripts the standard input ([`Sim::stdin`], [`Sim::stdin_ends`]) and reads what was written
//! ([`Sim::output`]); the wall clock is the seed's start ([`Sim::wall_start`]) plus the virtual
//! clock, and random bytes are the seed's numbers.
//!
//! The simulated disk's directories are those some file is under; a directory made empty is
//! one more, there until removed, through a crash too. Watches and journals are told of none,
//! and [`Effect::Entries`] and [`Effect::Stamp`] do not see one with nothing under it.

use std::{
    collections::VecDeque,
    mem,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

use super::{Of, Rng, Sim, Told};
use crate::{
    Completion, Dgram, Done, Effect, FileName, IoError, RANDOM_MOST, RECV_MOST, Stream, Tag,
    dgram::{self, Batch, Ecn, Head, Options, Slots},
};

/// The first port a bind of port 0 is given; the next is the one after, wrapping to this.
const FIRST_PORT: u16 = 40_000;

/// The network between the simulation's UDP sockets ([`Sim::datagrams`]).
#[derive(Clone, Copy, Debug)]
pub struct Datagrams {
    /// A datagram is lost once in this many sends (0: never).
    pub lose_one_in: u32,
    /// A datagram not lost arrives twice once in this many (0: never).
    pub duplicate_one_in: u32,
    /// Each copy of a datagram arrives after a delay drawn up to this, on its own.
    pub max_delay: Duration,
}

impl Default for Datagrams {
    fn default() -> Datagrams {
        Datagrams { lose_one_in: 0, duplicate_one_in: 0, max_delay: Duration::from_micros(200) }
    }
}

/// A bug a control plants in the simulation, for its oracle to catch ([`Sim::plant`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Plant {
    /// A datagram is told as from the port after its sender's.
    SenderLies,
    /// A datagram not lost arrives once more than the network said.
    ExtraCopies,
    /// A write to a standard stream that fails lands its bytes all the same.
    FailedWriteLands,
    /// A read of the standard input not at its end skips a byte.
    InputSkips,
    /// The wall clock runs at half the virtual clock's rate.
    WallLags,
    /// Every draw of random bytes is the same.
    RandomRepeats,
    /// A directory not empty is removed, and what was under it.
    RemovesFull,
}

/// A UDP socket bound, and the datagrams it got and no receive took yet.
struct Bound {
    id: u64,
    local: SocketAddr,
    options: Options,
    inbox: VecDeque<Landed>,
}

/// A datagram arrived at a socket: who sent it, the address it was sent to, its codepoint
/// and its bytes.
struct Landed {
    from: SocketAddr,
    to: SocketAddr,
    ecn: Ecn,
    bytes: Vec<u8>,
}

/// A copy of a datagram on its way, arriving at `at`.
struct Flying {
    at: Duration,
    from: SocketAddr,
    to: SocketAddr,
    ecn: Ecn,
    bytes: Vec<u8>,
}

/// A receive of a socket waiting for a datagram.
struct Receiving {
    tag: Tag,
    sock: u64,
    into: Batch,
    slots: Slots,
}

/// A read of the standard input waiting for its bytes or its end.
struct Reading {
    tag: Tag,
    most: usize,
    into: Vec<u8>,
}

/// The host as the simulation has it.
pub(super) struct Host {
    net: Datagrams,
    socks: Vec<Bound>,
    next_sock: u64,
    next_port: u16,
    flying: Vec<Flying>,
    receiving: Vec<Receiving>,
    /// Addresses every datagram to is lost ([`Sim::black_hole`]).
    holes: Vec<SocketAddr>,
    /// The names a resolve answers from ([`Sim::name`]).
    names: Vec<(String, Vec<IpAddr>)>,
    input: Vec<u8>,
    /// How much of `input` reads took.
    input_at: usize,
    input_ended: bool,
    reading: Vec<Reading>,
    /// When the last read of the standard input was told: the next is told no sooner.
    read_told: Duration,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// The wall clock at the virtual clock's start.
    wall: Duration,
    /// Directories made empty and not removed, sorted.
    made: Vec<FileName>,
    plant: Option<Plant>,
}

impl Host {
    /// The host of the simulation of `seed`: its wall clock starts some time after 2023 drawn
    /// from the seed alone, so it takes nothing from the seed's numbers.
    pub(super) fn new(seed: u64) -> Host {
        Host {
            net: Datagrams::default(),
            // alloc: none until a socket is bound.
            socks: Vec::new(),
            next_sock: 1,
            next_port: FIRST_PORT,
            // alloc: none until a datagram is sent.
            flying: Vec::new(),
            // alloc: none until a datagram is received.
            receiving: Vec::new(),
            holes: Vec::new_in(std::alloc::Global),
            names: Vec::new_in(std::alloc::Global),
            // alloc: none until the test scripts the input.
            input: Vec::new(),
            input_at: 0,
            input_ended: false,
            // alloc: none until the input is read.
            reading: Vec::new(),
            read_told: Duration::ZERO,
            // alloc: none until the output is written.
            stdout: Vec::new(),
            // alloc: none until the error is written.
            stderr: Vec::new(),
            wall: Duration::from_secs(1_700_000_000 + seed % 1_000_000_000),
            // alloc: none until a directory is made.
            made: Vec::new(),
            plant: None,
        }
    }
}

/// Whether sockets at `a` and `b` would have one address: the same port and family, and the
/// same address or either the unspecified one.
fn clashes(a: SocketAddr, b: SocketAddr) -> bool {
    a.port() == b.port()
        && a.is_ipv4() == b.is_ipv4()
        && (a.ip() == b.ip() || a.ip().is_unspecified() || b.ip().is_unspecified())
}

/// Whether a socket bound at `local` gets a datagram sent to `to`.
fn reaches(local: SocketAddr, to: SocketAddr) -> bool {
    local.port() == to.port()
        && local.is_ipv4() == to.is_ipv4()
        && (local.ip() == to.ip() || local.ip().is_unspecified())
}

/// Who a datagram from a socket bound at `local` is from: the loopback address of its family
/// where it is bound at the unspecified one, as the only host the simulation has.
fn seen_from(local: SocketAddr) -> SocketAddr {
    match local.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            SocketAddr::new(Ipv4Addr::LOCALHOST.into(), local.port())
        }
        IpAddr::V6(ip) if ip.is_unspecified() => {
            SocketAddr::new(Ipv6Addr::LOCALHOST.into(), local.port())
        }
        _ => local,
    }
}

impl Sim {
    /// From now on, datagrams between the simulation's sockets cross `net`.
    pub fn datagrams(&mut self, net: Datagrams) {
        self.host.net = net;
    }

    /// From now on every datagram sent to `to` is lost: a path that drops everything.
    pub fn black_hole(&mut self, to: SocketAddr) {
        if !self.host.holes.contains(&to) {
            self.host.holes.push(to);
        }
    }

    /// From now on datagrams to `to` cross the network again.
    pub fn clear_black_hole(&mut self, to: SocketAddr) {
        self.host.holes.retain(|it| *it != to);
    }

    /// From now on [`Effect::Resolve`] of `name` answers `addresses`, in their order; none
    /// makes it a name that has no address.
    pub fn name(&mut self, name: &str, addresses: &[IpAddr]) {
        self.host.names.retain(|(it, _)| it != name);
        let mut kept = Vec::with_capacity_in(addresses.len(), std::alloc::Global);
        kept.extend_from_slice(addresses);
        self.host.names.push((name.to_owned(), kept));
    }

    /// `bytes` come next on the standard input, after what the test gave before.
    pub fn stdin(&mut self, bytes: &[u8]) {
        self.host.input.extend_from_slice(bytes);
        self.serve_input();
    }

    /// The standard input ends after what the test gave: a read then gets nothing.
    pub fn stdin_ends(&mut self) {
        self.host.input_ended = true;
        self.serve_input();
    }

    /// Everything written to `stream`, in the order written.
    pub fn output(&self, stream: Stream) -> &[u8] {
        match stream {
            Stream::Stdout => &self.host.stdout,
            Stream::Stderr => &self.host.stderr,
        }
    }

    /// The wall clock as the virtual clock started: [`Effect::Now`] is it plus the virtual
    /// clock.
    pub fn wall_start(&self) -> Duration {
        self.host.wall
    }

    /// The wall clock now: the seed's start plus the virtual clock (half of it under the
    /// [`Plant::WallLags`] control). One home for [`Effect::Now`] and [`Sim::wall_ns`].
    pub(super) fn wall_now(&self) -> Duration {
        match self.planted(Plant::WallLags) {
            true => self.host.wall + self.now / 2,
            false => self.host.wall + self.now,
        }
    }

    /// The simulation from now on has the bug `plant`, for a control.
    #[cfg(test)]
    pub(super) fn plant(&mut self, plant: Plant) {
        self.host.plant = Some(plant);
    }

    fn planted(&self, plant: Plant) -> bool {
        self.host.plant == Some(plant)
    }

    /// One of the host's effects, other than a directory's, which is pended as a file's is.
    pub(super) fn hosted(&mut self, effect: Effect) {
        match effect {
            Effect::Bind { at, options, tag } => {
                let done = self.bind(tag, at, options);
                self.tell(tag, done, Of::Op);
            }
            Effect::SendTo { sock, batch, tag } => {
                let done = self.send_to(sock, batch);
                self.tell(tag, done, Of::Op);
            }
            Effect::RecvFrom { sock, into, slots, tag } => {
                let found = self.host.socks.iter().find(|it| it.id == sock.0);
                match found.map(|it| (it.local, slots.refused(it.options.coalesce))) {
                    Some((_, None)) => {
                        self.host.receiving.push(Receiving { tag, sock: sock.0, into, slots });
                        self.serve_datagrams();
                    }
                    Some((local, Some(why))) => {
                        let done = Done::dgram_failed(into, true, sock, Some(local), None, None, why);
                        self.tell(tag, done, Of::Op);
                    }
                    None => {
                        let done = Done::dgram_failed(
                            into,
                            true,
                            sock,
                            None,
                            None,
                            None,
                            "no such socket: it is not bound",
                        );
                        self.tell(tag, done, Of::Op);
                    }
                }
            }
            Effect::Resolve { name, port, tag } => {
                let done = self.resolve_name(name, port);
                self.tell(tag, done, Of::Op);
            }
            Effect::Unbind { sock, tag } => {
                let done = self.unbind(sock);
                self.tell(tag, done, Of::Op);
            }
            Effect::ReadStdin { most, into, tag } => {
                self.host.reading.push(Reading { tag, most, into });
                self.serve_input();
            }
            Effect::WriteOut { stream, bytes, tag } => {
                let done = self.write_out(stream, bytes);
                self.tell(tag, done, Of::Op);
            }
            Effect::Now { tag } => {
                let done = Done::Wall(self.wall_now());
                self.tell(tag, done, Of::Op);
            }
            Effect::Random { len, tag } => {
                let done = self.random(len);
                self.tell(tag, done, Of::Op);
            }
            _ => unreachable!("only the host's effects are handed here"),
        }
    }

    /// A name from the test's table ([`Sim::name`]), or an address as itself.
    fn resolve_name(&mut self, name: String, port: u16) -> Done {
        if self.rng.one_in(u64::from(self.faults.fail_one_in)) {
            let why = "the resolver failed: injected by the simulation".to_owned();
            return Done::Unresolved(dgram::Unresolved { name, port, why });
        }
        if let Ok(ip) = name.parse::<IpAddr>() {
            let mut one = Vec::with_capacity_in(1, std::alloc::Global);
            one.push(SocketAddr::new(ip, port));
            return Done::Resolved(one);
        }
        let found = self.host.names.iter().find(|(it, _)| *it == name).map(|(_, ips)| ips);
        match found {
            Some(ips) if !ips.is_empty() => {
                let mut addresses = Vec::with_capacity_in(ips.len(), std::alloc::Global);
                addresses.extend(ips.iter().map(|&ip| SocketAddr::new(ip, port)));
                Done::Resolved(addresses)
            }
            Some(_) => {
                let why = "the name has no address".to_owned();
                Done::Unresolved(dgram::Unresolved { name, port, why })
            }
            None => {
                let why = "no such name in the simulation's table (Sim::name)".to_owned();
                Done::Unresolved(dgram::Unresolved { name, port, why })
            }
        }
    }

    fn bind(&mut self, tag: Tag, at: SocketAddr, options: Options) -> Done {
        if self.rng.one_in(u64::from(self.faults.fail_one_in)) {
            return self.fail(tag).done;
        }
        let port = match at.port() {
            0 => match self.free_port(at) {
                Some(port) => port,
                // alloc: an error's message.
                None => return Done::Failed(IoError(format!("{at}: no port is free to bind"))),
            },
            port => port,
        };
        let local = SocketAddr::new(at.ip(), port);
        if let Some(other) = self.host.socks.iter().find(|it| clashes(it.local, local)) {
            // alloc: an error's message.
            return Done::Failed(IoError(format!(
                "{local}: address already in use: {:?} is bound at {}",
                Dgram(other.id),
                other.local
            )));
        }
        let id = self.host.next_sock;
        self.host.next_sock += 1;
        // alloc: none until a datagram lands.
        self.host.socks.push(Bound { id, local, options, inbox: VecDeque::new() });
        Done::Bound { sock: Dgram(id), local }
    }

    /// The next port no socket at `at`'s address has, from the one after the last given: at
    /// most one look at each port.
    fn free_port(&mut self, at: SocketAddr) -> Option<u16> {
        for _ in FIRST_PORT..=u16::MAX {
            let port = self.host.next_port;
            self.host.next_port = if port == u16::MAX { FIRST_PORT } else { port + 1 };
            let local = SocketAddr::new(at.ip(), port);
            if !self.host.socks.iter().any(|it| clashes(it.local, local)) {
                return Some(port);
            }
        }
        None
    }

    fn send_to(&mut self, sock: Dgram, mut batch: Batch) -> Done {
        let peer = batch.heads.first().map(|it| it.peer);
        let Some((local, options)) =
            self.host.socks.iter().find(|it| it.id == sock.0).map(|it| (it.local, it.options))
        else {
            return Done::dgram_failed(
                batch,
                false,
                sock,
                None,
                peer,
                None,
                "no such socket: it is not bound",
            );
        };
        if let Some(refused) = batch.refused() {
            let why = format!("sending {refused}");
            return Done::dgram_failed(batch, false, sock, Some(local), peer, None, why);
        }
        if !options.ecn && batch.heads.iter().any(|it| it.ecn != Ecn::NotEct) {
            let why = "sending a datagram marked with ECN on a socket bound without Options::ecn";
            return Done::dgram_failed(batch, false, sock, Some(local), peer, None, why);
        }
        if self.rng.one_in(u64::from(self.faults.fail_one_in)) {
            // A failure part way: the datagrams before a drawn one were sent, as a system call
            // that took some of a batch and refused the rest.
            let taken = self.rng.below(batch.datagrams() as u64) as usize;
            self.fly(local, &batch, taken);
            let why = "injected by the simulation";
            return Done::dgram_failed(batch, false, sock, Some(local), peer, None, why);
        }
        self.fly(local, &batch, usize::MAX);
        batch.clear();
        Done::SentDatagrams(batch)
    }

    /// The first `most` datagrams of `batch` from the socket at `local` put on their way, each
    /// lost, doubled and delayed on its own.
    fn fly(&mut self, local: SocketAddr, batch: &Batch, most: usize) {
        let net = self.host.net;
        let from = seen_from(local);
        let from = match self.planted(Plant::SenderLies) {
            true => SocketAddr::new(from.ip(), from.port().wrapping_add(1)),
            false => from,
        };
        for (head, bytes) in batch.each().take(most) {
            let to = head.peer;
            let holed = self.host.holes.contains(&to);
            let mut copies = match holed || self.rng.one_in(u64::from(net.lose_one_in)) {
                true => 0,
                false => 1 + u32::from(self.rng.one_in(u64::from(net.duplicate_one_in))),
            };
            if copies > 0 && self.planted(Plant::ExtraCopies) {
                copies += 1;
            }
            for _ in 0..copies {
                let delay =
                    Duration::from_nanos(self.rng.below(net.max_delay.as_nanos() as u64 + 1));
                let at = self.now + delay;
                let mut copy = Vec::with_capacity_in(bytes.len(), std::alloc::Global);
                copy.extend_from_slice(bytes);
                self.host.flying.push(Flying { at, from, to, ecn: head.ecn, bytes: copy });
            }
        }
    }

    fn unbind(&mut self, sock: Dgram) -> Done {
        let Some(at) = self.host.socks.iter().position(|it| it.id == sock.0) else {
            // alloc: an error's message.
            return Done::Failed(IoError(format!("{sock:?}: no such socket: it is not bound")));
        };
        let local = self.host.socks.remove(at).local;
        let mut at = 0;
        while at < self.host.receiving.len() {
            if self.host.receiving[at].sock != sock.0 {
                at += 1;
                continue;
            }
            let Receiving { tag, into, .. } = self.host.receiving.remove(at);
            let why = "unbound with the receive in flight";
            let done = Done::dgram_failed(into, true, sock, Some(local), None, None, why);
            self.tell(tag, done, Of::Op);
        }
        Done::Unbound
    }

    /// Each waiting receive whose socket has a datagram is told what it holds, up to the
    /// receive's slots, in the order asked: a drawn number of them, at least one, as a system
    /// call takes what has arrived by then.
    fn serve_datagrams(&mut self) {
        let mut at = 0;
        while at < self.host.receiving.len() {
            let sock = self.host.receiving[at].sock;
            let Some(index) = self.host.socks.iter().position(|it| it.id == sock) else {
                at += 1;
                continue;
            };
            if self.host.socks[index].inbox.is_empty() {
                at += 1;
                continue;
            }
            let Receiving { tag, mut into, slots, .. } = self.host.receiving.remove(at);
            let waiting = self.host.socks[index].inbox.len();
            let most = (slots.most as usize).min(waiting);
            let runs = 1 + self.rng.below(most as u64) as usize;
            into.clear();
            let bound = &mut self.host.socks[index];
            let options = bound.options;
            for _ in 0..runs {
                let Some(first) = bound.inbox.pop_front() else { break };
                let size = slots.size as usize;
                let mut run = Head {
                    peer: first.from,
                    local: options.local_address.then_some(first.to.ip()),
                    ecn: if options.ecn { first.ecn } else { Ecn::NotEct },
                    len: 0,
                    segment: 0,
                    truncated: first.bytes.len() > size,
                };
                let kept = first.bytes.len().min(size);
                into.bytes.extend_from_slice(&first.bytes[..kept]);
                run.len = kept as u32;
                run.segment = kept as u32;
                // A coalescing socket takes the datagrams after it of the same length, from
                // the same peer with the same codepoint, into the run, as UDP_GRO does; the
                // last may be shorter, and ends it.
                while options.coalesce
                    && !run.truncated
                    && run.segment > 0
                    && run.len % run.segment == 0
                    && run.datagrams() < dgram::SEGMENTS_MOST
                    && let Some(next) = bound.inbox.front()
                    && next.from == first.from
                    && next.ecn == first.ecn
                    && next.bytes.len() <= kept
                    && !next.bytes.is_empty()
                    && run.len as usize + next.bytes.len() <= size
                {
                    let next = bound.inbox.pop_front().expect("looked at above");
                    into.bytes.extend_from_slice(&next.bytes);
                    run.len += next.bytes.len() as u32;
                }
                into.heads.push(run);
            }
            self.tell(tag, Done::Datagrams(into), Of::Op);
        }
    }

    /// When the next datagram on its way arrives.
    pub(super) fn first_landing(&self) -> Option<Duration> {
        self.host.flying.iter().map(|it| it.at).min()
    }

    /// The datagrams due by now arrive, in the order they are due, and the receives waiting
    /// for them are told.
    pub(super) fn land(&mut self) {
        let now = self.now;
        if self.host.flying.iter().all(|it| it.at > now) {
            return;
        }
        let flying = mem::take(&mut self.host.flying);
        // alloc: a poll at which datagrams arrive, both parts of what was on its way.
        let (mut landed, left): (Vec<Flying>, Vec<Flying>) =
            flying.into_iter().partition(|it| it.at <= now);
        self.host.flying = left;
        // Stable: two due at once arrive in the order sent.
        landed.sort_by_key(|it| it.at);
        for Flying { from, to, ecn, bytes, .. } in landed {
            if let Some(sock) = self.host.socks.iter_mut().find(|it| reaches(it.local, to)) {
                sock.inbox.push_back(Landed { from, to, ecn, bytes });
            }
        }
        self.serve_datagrams();
    }

    /// Each read of the standard input that can be answered is, in the order asked: with at
    /// least a byte and at most what it asked for where the input has bytes left, a drawn
    /// number of them, as a pipe gives what it holds; with nothing once it ended.
    fn serve_input(&mut self) {
        while let Some(first) = self.host.reading.first() {
            let left = self.host.input.len() - self.host.input_at;
            let most = first.most.min(RECV_MOST);
            if left == 0 && most > 0 && !self.host.input_ended {
                break;
            }
            let n = match most.min(left) {
                0 => 0,
                can => 1 + self.rng.below(can as u64) as usize,
            };
            let skip = usize::from(n < left && self.planted(Plant::InputSkips));
            let Reading { tag, mut into, .. } = self.host.reading.remove(0);
            let start = self.host.input_at + skip;
            // alloc: none once the asker's buffer has room for what came.
            into.extend_from_slice(&self.host.input[start..start + n]);
            self.host.input_at = start + n;
            let at = (self.now + self.delay()).max(self.host.read_told);
            self.host.read_told = at;
            let completion = Completion { tag, done: Done::Read(into) };
            self.told.push(Told { at, completion, of: Of::Op });
        }
    }

    /// A write to `stream`: all of it, or, failed, none of it.
    fn write_out(&mut self, stream: Stream, mut bytes: Vec<u8>) -> Done {
        if self.rng.one_in(u64::from(self.faults.fail_one_in)) {
            if self.planted(Plant::FailedWriteLands) {
                self.output_mut(stream).extend_from_slice(&bytes);
            }
            return Done::unsent(bytes, format_args!("{stream}: injected by the simulation"));
        }
        self.output_mut(stream).extend_from_slice(&bytes);
        bytes.clear();
        Done::Sent(bytes)
    }

    fn output_mut(&mut self, stream: Stream) -> &mut Vec<u8> {
        match stream {
            Stream::Stdout => &mut self.host.stdout,
            Stream::Stderr => &mut self.host.stderr,
        }
    }

    fn random(&mut self, len: usize) -> Done {
        if len > RANDOM_MOST {
            // alloc: an error's message.
            return Done::Failed(IoError(format!(
                "{len} random bytes asked for: more than one draw gives ({RANDOM_MOST})"
            )));
        }
        // alloc: the draw, which the completion carries.
        let mut bytes = vec![0; len];
        match self.planted(Plant::RandomRepeats) {
            true => Rng::new(0).fill(&mut bytes),
            false => self.rng.fill(&mut bytes),
        }
        Done::Random(bytes)
    }

    /// What of the host is in flight: receives and reads waiting.
    pub(super) fn host_in_flight(&self) -> usize {
        self.host.receiving.len() + self.host.reading.len()
    }

    /// The machine loses power: every socket goes, with what was on its way to one and every
    /// receive and read waiting. What was written stays written, what was read stays read, and
    /// a directory made stays made.
    pub(super) fn host_crash(&mut self) {
        self.host.socks.clear();
        self.host.flying.clear();
        self.host.receiving.clear();
        self.host.reading.clear();
        self.host.read_told = Duration::ZERO;
    }

    /// Whether `name` is a directory: one made, or one some file or directory made is under.
    pub(super) fn is_dir(&self, name: &FileName) -> bool {
        self.host.made.binary_search(name).is_ok() || self.anything_under(name)
    }

    /// Whether a file, or a directory made, is under `dir`, not `dir` itself.
    fn anything_under(&self, dir: &FileName) -> bool {
        self.files.keys().any(|it| it != dir && it.is_under(dir))
            || self.host.made.iter().any(|it| it != dir && it.is_under(dir))
    }

    /// The directories made under `dir`, `dir` not one of them.
    pub(super) fn made_under<'a>(
        &'a self,
        dir: &'a FileName,
    ) -> impl Iterator<Item = &'a FileName> + 'a {
        self.host.made.iter().filter(move |it| *it != dir && it.is_under(dir))
    }

    /// Whether `dir` is a directory made.
    pub(super) fn was_made(&self, dir: &FileName) -> bool {
        self.host.made.binary_search(dir).is_ok()
    }

    /// [`Effect::MakeDir`] on the simulated disk.
    pub(super) fn make_dir(&mut self, dir: FileName) -> Done {
        if self.files.contains_key(&dir) || self.is_dir(&dir) {
            // alloc: an error's message.
            return Done::Failed(IoError(format!("{dir}: it is there already")));
        }
        if let Some((parent, _)) = dir.as_str().rsplit_once('/') {
            let parent = FileName::new(parent).expect("a part of a name is a name");
            if self.files.contains_key(&parent) {
                // alloc: an error's message.
                return Done::Failed(IoError(format!(
                    "{dir}: its parent {parent} is no directory"
                )));
            }
            if !self.is_dir(&parent) {
                // alloc: an error's message.
                return Done::Failed(IoError(format!("{dir}: its parent {parent} is not there")));
            }
        }
        let at = self.host.made.binary_search(&dir).expect_err("not made, looked up above");
        self.host.made.insert(at, dir);
        Done::MadeDir
    }

    /// [`Effect::RemoveDir`] on the simulated disk.
    pub(super) fn remove_dir(&mut self, dir: &FileName) -> Done {
        if self.files.contains_key(dir) {
            // alloc: an error's message.
            return Done::Failed(IoError(format!("{dir}: it is no directory")));
        }
        if self.anything_under(dir) {
            if !self.planted(Plant::RemovesFull) {
                // alloc: an error's message.
                return Done::Failed(IoError(format!("{dir}: the directory is not empty")));
            }
            // alloc: a planted bug's, the names it removes.
            let under: Vec<FileName> = self
                .files
                .keys()
                .filter(|it| it.is_under(dir))
                .cloned() // clone: a name removed below, out of the table it is a key of.
                .collect();
            for file in under {
                self.files.remove(&file);
                self.unlink_locks(&file);
                self.changed(&file);
            }
            self.host.made.retain(|it| !it.is_under(dir));
            return Done::RemovedDir;
        }
        match self.host.made.binary_search(dir) {
            Ok(at) => {
                self.host.made.remove(at);
                Done::RemovedDir
            }
            // alloc: an error's message.
            Err(_) => Done::Failed(IoError(format!("{dir}: no such directory"))),
        }
    }

    /// [`Effect::Remove`] of a directory made: removed where empty, as `remove` does one.
    pub(super) fn remove_made(&mut self, dir: &FileName) -> Done {
        match self.anything_under(dir) {
            // alloc: an error's message.
            true => Done::Failed(IoError(format!("{dir}: the directory is not empty"))),
            false => self.remove_dir(dir),
        }
    }
}

#[cfg(test)]
mod tests;
