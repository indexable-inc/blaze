//! The fetch machine (sans-IO, rule 14): a plan's units in, effects out, completions in, one
//! [`Outcome`] a unit. It owns no socket, file or clock: the time comes with each call and each
//! request carries its deadline.
//!
//! A unit goes:
//!
//! 1. [`Effect::Lookup`]: does the store hold its key? Held, it is [`Outcome::Present`] and
//!    nothing is asked (a second run fetches nothing).
//!
//!    A crate not held: [`Effect::Local`], the machine's own local step (fetch.md, "End state:
//!    dependencies on the command path", place 2, piece 7): does another tool on this machine
//!    hold its `.crate` (cargo's `registry/cache`)? The bytes found are adopted only through
//!    [`local::adopt_in`] (their SHA-256 against the lock's before one is inflated), into the
//!    record and key the network makes ([`Outcome::Fetched`] with `down` 0,
//!    [`Machine::adopted`]); absent, unreadable or refused, the try is kept
//!    ([`Machine::tried`]) and the unit goes on to the next place in the order, HTTPS. That is
//!    the order's next place, not a retry: the place that refused is never asked again.
//! 2. [`Effect::Http`]: a crate's `.crate` from crates.io's static host; for a git commit, the
//!    advertisement (`info/refs`, protocol v2), then a `fetch` of the commit at depth 1. At most
//!    [`Limits::in_flight_most`] at once, each with a byte limit and a deadline. A request
//!    past its deadline is failed by the machine itself and its late answer ignored.
//! 3. One try (D172, rule 3): any failure (no connection, a reset, a timeout, a 429, a 5xx, a
//!    wrong digest) fails the unit at once with a typed [`Why`], which the caller names with
//!    the unit's source. Nothing is tried again in the command: a failure of the network is
//!    owed, so the NEXT command asks again (blaze-cli engine_check.rs, the Units phase's owed
//!    list), and one that succeeds stands by its key. (Until 2026-11-20 a transient failure
//!    was tried again after a backoff, four tries in all: a command offline waited out every
//!    backoff of every unit to say "no connection (4 tries)".) An answer past a first
//!    request's `most` is asked again sized to its length: a bigger buffer, not a second try.
//! 4. A `.crate`'s SHA-256 is compared with the lock's before a byte is inflated; a git
//!    answer's objects are read by their ids from the pinned commit down. Only then is it
//!    unpacked ([`crate::unpack`]) into a record.
//! 5. The record at its key, whole or not at all, then [`Outcome::Fetched`]. A small record
//!    (stated below [`Limits::reserve_from`]) is made in the heap and kept by [`Effect::Put`];
//!    a large one, and every git commit's, is made in place: [`Effect::Reserve`] asks the store
//!    for a file of the record's largest length, mapped (`blaze_core::reserve` in the
//!    driver), [`Machine::reserved`] writes the record into it, and the driver finishes it at
//!    the length made. Never the whole record in memory.

#![blaze::sans_io]

use core::net::{IpAddr, SocketAddr};

use blaze_git_fetch::PktWhy;
use blaze_ids::ContentId;
use blaze_own::{Alloc, String, Vec, VecDeque};
use blaze_text::{Put, Show, Writes as _};

use crate::local::{self, AdoptRefused, Miss, Try};
use crate::pack::{Fixed, Sink};
use crate::peer::{self, HAVE_MAX_HASHES, HasPacket};
use crate::plan::Unit;
use crate::unpack::{self, Made, UnpackError};
use crate::{Plant, planted};

/// crates.io's static host for `.crate` files (its index's `dl`).
pub const CRATES: &str = "https://static.crates.io/crates/";

const ADVERTISE: [(&str, &str); 1] = [("Git-Protocol", "version=2")];
const UPLOAD: [(&str, &str); 3] = [
    ("Content-Type", "application/x-git-upload-pack-request"),
    ("Accept", "application/x-git-upload-pack-result"),
    ("Git-Protocol", "version=2"),
];

/// Every bound of a fetch (rule 18).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// Requests in flight at once.
    pub in_flight_most: u32,
    /// Each request's deadline, from when it is asked.
    pub ask_ns: u64,
    /// Bytes of one `.crate`.
    pub crate_most: u64,
    /// Bytes of a git advertisement.
    pub advert_most: u64,
    /// Bytes of one git `fetch` answer.
    pub git_most: u64,
    /// Bytes in flight at once: the sum of every request's `most` (the most its driver may
    /// hold of it) and of every record made but not yet stored. A request waits until its
    /// `most` fits; one alone in flight always goes, so a unit past the budget still ends.
    pub bytes_most: u64,
    /// A first request's `most`, below its step's limit: an answer past it that states its
    /// length is asked again once with that length as its `most` (not counted as a try); one
    /// that does not is asked again with the step's limit held to the budget, past which it
    /// fails.
    pub first_most: u64,
    /// The same for a git `fetch` answer, which is sent in chunks and states no length.
    pub git_first_most: u64,
    /// A `.crate` whose gzip trailer states this many bytes or more is made in place
    /// ([`Effect::Reserve`]); below it, in the heap and put. Every git commit is made in
    /// place: its size is known only once its pack is indexed.
    pub reserve_from: u64,
    /// Whether to emit LAN UDP `PeerProbe`s in parallel with `static.crates.io` at `t = 0`.
    pub peer_race: bool,
    /// Per-request deadline for a LAN peer HTTP fetch, in nanoseconds.
    pub peer_ask_ns: u64,
    pub unpack: unpack::Limits,
}

impl Limits {
    /// 48 requests of at most 1 MiB first for a `.crate`, 32 MiB for a git pack (reth's at
    /// depth 1 is 22 MB), and 512 MiB in flight in all, so a typical fetch never waits on the
    /// budget (48 × 1 MiB and a few packs' 32 MiB), and a git pack past 512 MiB fails, by name.
    pub const REAL: Limits = Limits {
        bytes_most: 512 << 20,
        first_most: 1 << 20,
        git_first_most: 32 << 20,
        in_flight_most: 48,
        ask_ns: 120_000_000_000,
        crate_most: 256 << 20,
        advert_most: 1 << 20,
        git_most: 1 << 30,
        // From blaze-core's `examples/reserve_cost.rs` on an M-series Mac (2026-11-16): a
        // reservation costs a `Put` of the same bytes, 0.88 to 1.01 of it, from 16 KiB to 16 MiB
        // (the two syncs dominate both), so from 16 KiB a record is never whole in the heap.
        reserve_from: 16 << 10,
        peer_race: true,
        peer_ask_ns: 15_000_000_000,
        unpack: unpack::Limits::REAL,
    };
}

/// Which request of a unit an answer is to: a later request of the same unit has another
/// `seq`, so an answer that comes after its deadline is told apart and ignored.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ask {
    pub unit: u32,
    pub seq: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    Get,
    Post,
}

/// What the machine asks its driver to do.
#[derive(Debug)]
pub enum Effect<'a> {
    /// Whether the store holds `key`: answer with [`Machine::looked`].
    Lookup { unit: u32, key: ContentId },
    /// An HTTP/HTTPS request: answer with [`Machine::answered`]. A body past `most` bytes is not
    /// read ([`NetFailure::TooBig`]); `deadline_ns` is on the clock the machine is given.
    /// `https_only` is `true` for WAN origins and `false` for plain-HTTP LAN peers; `redirects`
    /// is `0` for LAN peers so a rogue peer cannot redirect to SSRF targets.
    Http {
        ask: Ask,
        method: Method,
        url: String<Alloc<'a>>,
        headers: &'static [(&'static str, &'static str)],
        body: Vec<u8, Alloc<'a>>,
        most: u64,
        deadline_ns: u64,
        https_only: bool,
        redirects: u8,
    },
    /// Broadcast/multicast a `BLZP` `HAVE` datagram asking LAN peers which of `hashes`
    /// (at most 32 SHA-256 digests) they hold, tagged with `nonce`. Fire-and-forget at `t = 0`:
    /// never delays `static.crates.io`, and a driver without UDP simply ignores it.
    PeerProbe { nonce: u64, hashes: Vec<[u8; 32], Alloc<'a>> },
    /// Cancel a losing in-flight HTTP request (`ask`) once its racing counterpart won with a
    /// verified SHA-256, or once its peer was banned/timed out.
    Cancel { ask: Ask },
    /// Keep `record` at `key`, whole or not at all: answer with [`Machine::stored`].
    Put { unit: u32, key: ContentId, record: Vec<u8, Alloc<'a>> },
    /// Reserve `room` bytes in place for `unit`'s record at `key` (a writable mapping of a
    /// scratch file, `blaze_core::reserve::Reserve`), keeping the body of the answer last
    /// given for `unit`: answer with [`Machine::reserved`], then finish the reservation at the
    /// length it gives (or abandon it at `None`), then [`Machine::stored`].
    Reserve { unit: u32, key: ContentId, room: u64 },
    /// Whether this machine holds crate `unit`'s `.crate` in another tool's cache
    /// ([`crate::FetchIo::local_archive`]): answer with [`Machine::local_found`], and when it
    /// gives [`Adopt::Reserve`], reserve as for [`Effect::Reserve`], writing with
    /// [`Machine::adopt_into`]. Asked only for a [`Unit::Crate`] the store does not hold, once,
    /// before any HTTP for it.
    Local { unit: u32 },
}

/// The answer to [`Effect::Local`]: what the driver's IO found of a crate's archive on this
/// machine. The bytes of [`LocalArchive::Found`] are given beside it, and are believed of
/// nothing: the lock's SHA-256 decides.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LocalArchive {
    /// No place on the machine holds a file of that name.
    Absent,
    /// A file was read whole into the buffer the IO was given.
    Found,
    /// A file is there and could not be read (`why`: permissions, an IO error, past the IO's
    /// own bound): the try is named, and the unit goes on to HTTPS.
    Unreadable { why: &'static str },
}

/// What the driver does after [`Machine::local_found`].
#[must_use]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Adopt {
    /// Nothing more for this answer: the record is queued ([`Effect::Put`]) or the unit is on its
    /// way to HTTPS (the next [`Machine::poll`] says).
    Done,
    /// Reserve `room` bytes in place at `key` for the unit, now, while the archive's bytes are
    /// still the driver's, and write them with [`Machine::adopt_into`]; then finish, abandon or
    /// answer [`Machine::stored`] as for [`Effect::Reserve`].
    Reserve { key: ContentId, room: u64 },
}

/// A bug planted in the local step by a test, to show its oracle catches it: only ever in this
/// crate's tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LocalPlant {
    None,
    /// A found archive is made into a record without its SHA-256 compared to the lock's.
    AdoptBeforeChecksum,
}

#[cfg(test)]
thread_local! {
    pub(crate) static LOCAL_PLANT: core::cell::Cell<LocalPlant> = const { core::cell::Cell::new(LocalPlant::None) };
}

/// Whether `plant` is planted: only ever in this crate's tests.
#[cfg(test)]
fn local_planted(plant: LocalPlant) -> bool {
    LOCAL_PLANT.get() == plant
}

/// Whether `plant` is planted: never, outside tests.
#[cfg(not(test))]
#[inline(always)]
fn local_planted(_plant: LocalPlant) -> bool {
    false
}

/// One local try of a unit: what the place gave, and an unreadable file's reason.
#[derive(Clone, PartialEq, Eq, Debug)]
struct LocalTry {
    miss: Miss,
    why: Option<&'static str>,
}

/// An answer to [`Effect::Http`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reply<'b> {
    /// The final answer after redirects, its whole body.
    Body { status: u16, bytes: &'b [u8] },
    Failed(NetFailure),
}

/// Why a request had no answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NetFailure {
    /// No connection was made (name lookup, connect).
    Connect,
    /// The connection broke, or the body ended short of its length.
    Reset,
    /// The driver's own deadline passed.
    Timeout,
    /// The body was longer than the request's `most`; `said` is its length as the answer
    /// stated it (`Content-Length`), when it did, read before any of the body.
    TooBig { said: Option<u64> },
    /// Refused for good: TLS, a redirect off HTTPS, a malformed answer.
    Refused,
}

impl NetFailure {
    pub fn name(self) -> &'static str {
        match self {
            NetFailure::Connect => "no connection",
            NetFailure::Reset => "the connection broke",
            NetFailure::Timeout => "no answer by its deadline",
            NetFailure::TooBig { .. } => "an answer past its byte limit",
            NetFailure::Refused => "refused (TLS, a redirect off https, or a malformed answer)",
        }
    }
}

/// Why the store did not keep a record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StoreFailure {
    Io,
    Full,
}

/// Why a unit failed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Why {
    /// The server answered `status` to the one try (D172).
    Status { status: u16 },
    /// The one try had no answer (D172).
    Net { failure: NetFailure },
    /// A body longer than its limit came anyway.
    TooBig { most: u64, got: u64 },
    /// The `.crate`'s SHA-256 is not the lock's.
    Mismatch { want: [u8; 32], got: [u8; 32] },
    Unpack(UnpackError),
    /// git's advertisement is not protocol v2's.
    Git(PktWhy),
    Store(StoreFailure),
}

impl Show for Why {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match self {
            Why::Status { status } => out.str("the server answered ").int(*status),
            Why::Net { failure } => out.str(failure.name()),
            Why::TooBig { most, got } => out.str("an answer of ").byte_count(*got).str(", past its limit of ").byte_count(*most),
            Why::Mismatch { want, got } => {
                out.str("its sha256 is ").hex_bytes(got).str(", not the lock's ").hex_bytes(want)
            }
            Why::Unpack(why) => out.show(why),
            Why::Git(why) => out.str("git's advertisement is not protocol v2's: ").str(unpack::pkt_why(why)),
            Why::Store(StoreFailure::Io) => out.str("the store did not keep it: a write failed"),
            Why::Store(StoreFailure::Full) => out.str("the store did not keep it: the disk is full"),
        };
    }
}

/// A unit's end, or that it has none yet.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    Pending,
    /// The store held its record: nothing was asked.
    Present,
    /// Fetched, checked and kept: `down` bytes downloaded, `files` and `bytes` in the record;
    /// for a git commit, how its pack's base cache did.
    Fetched { down: u64, files: u32, bytes: u64, cache: Option<blaze_git_fetch::CacheStats> },
    Failed(Why),
}

/// A unit's failure and the unit, for printing.
pub struct Failed<'u, 'l, 'a> {
    pub unit: &'u Unit<'l, 'a>,
    pub why: &'u Why,
}

impl Show for Failed<'_, '_, '_> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.show(self.unit).str(": ").show(self.why);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    Crate,
    Advert,
    GitFetch,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// Not wanted.
    Idle,
    /// Wanted, its lookup not yet asked.
    Wanted,
    Looking,
    /// Not held: the driver is asked for a local archive ([`Effect::Local`]).
    Local,
    /// A local archive found and a reservation asked for its record ([`Adopt::Reserve`]): the
    /// driver keeps the archive's bytes until [`Machine::adopt_into`].
    Adopting,
    Ready(Step),
    Asked { step: Step, seq: u32, deadline: u64 },
    /// WAN request failed (deferred in `wan_deferred`), waiting on an in-flight LAN peer request.
    WaitingPeer,
    /// A reservation asked: the driver keeps the answer's body until [`Machine::reserved`].
    Reserving(Step),
    Storing(Made),
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct PeerFlight {
    seq: u32,
    peer: SocketAddr,
    most: u64,
    deadline: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct PendingPeer {
    unit: u32,
    peer: SocketAddr,
    most: u64,
    active_transfers: u32,
}

struct ProbeBatch<'a> {
    nonce: u64,
    units: Vec<u32, Alloc<'a>>,
}

#[derive(Clone, Copy)]
struct State {
    phase: Phase,
    seq: u32,
    down: u64,
    /// The next or current request's `most`: 0 for the step's first.
    most: u64,
    /// Bytes of the record made and not yet stored.
    held: u64,
    /// The record was made from an archive found on the machine, not downloaded.
    adopted: bool,
    /// Whether this crate unit was already included in a LAN `PeerProbe`.
    probed: bool,
    /// Active LAN peer HTTP request racing WAN for this unit.
    peer: Option<PeerFlight>,
}

/// The machine: see the module.
pub struct Machine<'p, 'l, 'a> {
    units: &'p [Unit<'l, 'a>],
    states: Vec<State, Alloc<'a>>,
    outcomes: Vec<Outcome, Alloc<'a>>,
    limits: Limits,
    /// Units wanted and not yet looked up, in the order wanted.
    lookups: VecDeque<u32, Alloc<'a>>,
    /// Crates not held, their local archive not yet asked for, in order.
    locals: VecDeque<u32, Alloc<'a>>,
    /// A unit's local try, once its local step gave nothing adopted.
    tried: Vec<Option<LocalTry>, Alloc<'a>>,
    /// Deferred WAN failure while a LAN peer request is still in flight for the unit.
    wan_deferred: Vec<Option<Why>, Alloc<'a>>,
    ready: VecDeque<u32, Alloc<'a>>,
    asked: Vec<u32, Alloc<'a>>,
    /// Units with an active LAN peer HTTP request in flight.
    peer_asked: Vec<u32, Alloc<'a>>,
    /// Queued LAN peer HTTP requests waiting for an in-flight slot / budget.
    peer_ready: VecDeque<PendingPeer, Alloc<'a>>,
    /// In-flight cancellation effects waiting to be emitted.
    cancels: VecDeque<Ask, Alloc<'a>>,
    /// Sent UDP `HAVE` probe batches, mapping `nonce` -> probed unit indices.
    probes: Vec<ProbeBatch<'a>, Alloc<'a>>,
    next_probe_nonce: u64,
    /// Peer IPs permanently banned for this command (SHA-256 mismatch, redirect, or refusal).
    banned_peers: Vec<IpAddr, Alloc<'a>>,
    peer_wins: u64,
    rejected_peers: u64,
    puts: VecDeque<(u32, ContentId, Vec<u8, Alloc<'a>>), Alloc<'a>>,
    reserves: VecDeque<(u32, ContentId, u64), Alloc<'a>>,
    left: usize,
    stale: u64,
    /// Bytes in flight: requests' `most`s and records not yet stored ([`Limits::bytes_most`]).
    in_flight: u64,
    /// The most `in_flight` reached.
    in_flight_peak: u64,
    alloc: Alloc<'a>,
}

impl<'p, 'l, 'a> Machine<'p, 'l, 'a> {
    /// A machine wanting every unit of `units`, in order: a whole lock's fetch.
    pub fn new(units: &'p [Unit<'l, 'a>], limits: Limits, alloc: Alloc<'a>) -> Machine<'p, 'l, 'a> {
        let mut machine = Machine::wanting(units, limits, alloc);
        for unit in 0..units.len() as u32 {
            machine.want(unit);
        }
        machine
    }

    /// A machine over `units` wanting none yet: each is fetched once [`Machine::want`] names
    /// it, so a command whose `Fetch` tasks stand on their records asks for nothing, and one
    /// that runs a few shares one budget and one set of in-flight limits among them.
    pub fn wanting(units: &'p [Unit<'l, 'a>], limits: Limits, alloc: Alloc<'a>) -> Machine<'p, 'l, 'a> {
        let mut states = Vec::with_capacity_in(units.len(), alloc);
        let mut outcomes = Vec::with_capacity_in(units.len(), alloc);
        let mut tried = Vec::with_capacity_in(units.len(), alloc);
        let mut wan_deferred = Vec::with_capacity_in(units.len(), alloc);
        for _ in units {
            states.push(State {
                phase: Phase::Idle,
                seq: 0,
                down: 0,
                most: 0,
                held: 0,
                adopted: false,
                probed: false,
                peer: None,
            });
            outcomes.push(Outcome::Pending);
            tried.push(None);
            wan_deferred.push(None);
        }
        Machine {
            units,
            states,
            outcomes,
            limits,
            lookups: VecDeque::new_in(alloc),
            locals: VecDeque::new_in(alloc),
            tried,
            wan_deferred,
            ready: VecDeque::new_in(alloc),
            asked: Vec::new_in(alloc),
            peer_asked: Vec::new_in(alloc),
            peer_ready: VecDeque::new_in(alloc),
            cancels: VecDeque::new_in(alloc),
            probes: Vec::new_in(alloc),
            next_probe_nonce: 1,
            banned_peers: Vec::new_in(alloc),
            peer_wins: 0,
            rejected_peers: 0,
            puts: VecDeque::new_in(alloc),
            reserves: VecDeque::new_in(alloc),
            left: 0,
            stale: 0,
            in_flight: 0,
            in_flight_peak: 0,
            alloc,
        }
    }

    /// The next effect to do at `now`, or `None` until a completion comes or
    /// [`Machine::next_deadline`] passes.
    pub fn poll(&mut self, now: u64) -> Option<Effect<'a>> {
        self.expire(now);
        if let Some(ask) = self.cancels.pop_front() {
            return Some(Effect::Cancel { ask });
        }
        if let Some(unit) = self.lookups.pop_front() {
            self.states[unit as usize].phase = Phase::Looking;
            return Some(Effect::Lookup { unit, key: self.units[unit as usize].key() });
        }
        if let Some(unit) = self.locals.pop_front() {
            return Some(Effect::Local { unit });
        }
        if let Some((unit, key, record)) = self.puts.pop_front() {
            return Some(Effect::Put { unit, key, record });
        }
        if let Some((unit, key, room)) = self.reserves.pop_front() {
            return Some(Effect::Reserve { unit, key, room });
        }
        // Emit a fire-and-forget LAN UDP `HAVE` probe batch (at most 32 hashes) at `t = 0`
        // before draining `ready`, so LAN discovery runs strictly in parallel with WAN and
        // never delays `static.crates.io`.
        if self.limits.peer_race
            && let Some(probe) = self.next_peer_probe()
        {
            return Some(probe);
        }
        // Launch queued LAN peer HTTP races while a peer slot and byte budget permit.
        if let Some(effect) = self.next_peer_http(now) {
            return Some(effect);
        }
        // In order: the first ready unit waits for its bytes to fit, and none passes it.
        if self.asked.len() < self.limits.in_flight_most as usize
            && let Some(&unit) = self.ready.front()
        {
            let alone = self.in_flight == 0;
            if alone || self.in_flight.saturating_add(self.next_most(unit)) <= self.limits.bytes_most {
                self.ready.pop_front();
                return Some(self.ask(unit, now));
            }
        }
        None
    }

    /// Batches up to [`HAVE_MAX_HASHES`] unprobed `Unit::Crate` units currently in `ready`
    /// into one [`Effect::PeerProbe`].
    fn next_peer_probe(&mut self) -> Option<Effect<'a>> {
        let mut probe_units = Vec::new_in(self.alloc);
        let mut hashes = Vec::new_in(self.alloc);
        for &unit in self.ready.iter() {
            if hashes.len() >= HAVE_MAX_HASHES {
                break;
            }
            let Ok(idx) = usize::try_from(unit) else { continue };
            let Some(st) = self.states.get_mut(idx) else { continue };
            if st.probed {
                continue;
            }
            if let Some(&Unit::Crate { sha256, .. }) = self.units.get(idx) {
                st.probed = true;
                probe_units.push(unit);
                hashes.push(sha256);
            }
        }
        if hashes.is_empty() {
            return None;
        }
        let nonce = self.next_probe_nonce;
        self.next_probe_nonce = self.next_probe_nonce.wrapping_add(1).max(1);
        self.probes.push(ProbeBatch { nonce, units: probe_units });
        Some(Effect::PeerProbe { nonce, hashes })
    }

    /// Launches the next eligible LAN peer HTTP request from `peer_ready`, if slots and byte
    /// budget allow.
    fn next_peer_http(&mut self, now: u64) -> Option<Effect<'a>> {
        let cap = usize::try_from(self.limits.in_flight_most).unwrap_or(usize::MAX);
        while self.peer_asked.len() < cap {
            let Some(&pending) = self.peer_ready.front() else {
                break;
            };
            let Ok(idx) = usize::try_from(pending.unit) else {
                self.peer_ready.pop_front();
                continue;
            };
            let Some(&state) = self.states.get(idx) else {
                self.peer_ready.pop_front();
                continue;
            };
            let eligible = matches!(
                state.phase,
                Phase::Ready(Step::Crate) | Phase::Asked { step: Step::Crate, .. } | Phase::WaitingPeer
            ) && state.peer.is_none()
                && !self.is_peer_banned(pending.peer.ip());
            if !eligible {
                self.peer_ready.pop_front();
                continue;
            }
            let most = if pending.most != 0 { pending.most } else { self.first_most(Step::Crate) };
            let alone = self.in_flight == 0;
            if !alone && self.in_flight.saturating_add(most) > self.limits.bytes_most {
                // Byte budget tight right now; wait without blocking WAN.
                break;
            }
            self.peer_ready.pop_front();
            let Some(&Unit::Crate { sha256, .. }) = self.units.get(idx) else { continue };
            let mut url = String::new_in(self.alloc);
            peer::format_crate_url(pending.peer, &sha256, &mut url);
            let deadline = now.saturating_add(self.limits.peer_ask_ns.min(self.limits.ask_ns));
            self.hold(most);
            let Some(st) = self.states.get_mut(idx) else { continue };
            st.seq = st.seq.saturating_add(1);
            let seq = st.seq;
            st.peer = Some(PeerFlight { seq, peer: pending.peer, most, deadline });
            self.peer_asked.push(pending.unit);
            return Some(Effect::Http {
                ask: Ask { unit: pending.unit, seq },
                method: Method::Get,
                url,
                headers: &[],
                body: Vec::new_in(self.alloc),
                most,
                deadline_ns: deadline,
                https_only: false,
                redirects: 0,
            });
        }
        None
    }

    /// Processes a unicast `HAS` reply from `peer_addr`: validates LAN-only SSRF filter,
    /// ban list, and probe `nonce`, then queues plain-HTTP `GET /v1/crate/<sha256>` races
    /// against `SocketAddr::new(peer_addr.ip(), has.tcp_port)` for matching still-in-flight
    /// crate units.
    pub fn peer_replied(&mut self, peer_addr: SocketAddr, has: HasPacket, _now: u64) {
        if !self.limits.peer_race {
            return;
        }
        let ip = peer_addr.ip();
        if !peer::is_lan_ip(ip) {
            self.rejected_peers = self.rejected_peers.saturating_add(1);
            return;
        }
        if has.tcp_port == 0 || has.mask == 0 || self.is_peer_banned(ip) {
            return;
        }
        let Some(probe_idx) = self.probes.iter().position(|p| p.nonce == has.nonce) else {
            self.stale = self.stale.saturating_add(1);
            return;
        };
        let peer_http = SocketAddr::new(ip, has.tcp_port);
        let Some(probe) = self.probes.get(probe_idx) else { return };
        for (bit, &unit) in probe.units.iter().enumerate() {
            let Ok(shift) = u32::try_from(bit) else { break };
            if shift >= 32 || (has.mask & (1u32 << shift)) == 0 {
                continue;
            }
            let Ok(idx) = usize::try_from(unit) else { continue };
            let Some(&state) = self.states.get(idx) else { continue };
            let eligible = matches!(
                state.phase,
                Phase::Ready(Step::Crate) | Phase::Asked { step: Step::Crate, .. } | Phase::WaitingPeer
            ) && state.peer.is_none();
            if !eligible {
                continue;
            }
            if let Some(existing) = self.peer_ready.iter_mut().find(|p| p.unit == unit) {
                if has.active_transfers < existing.active_transfers {
                    existing.peer = peer_http;
                    existing.active_transfers = has.active_transfers;
                }
            } else {
                self.peer_ready.push_back(PendingPeer {
                    unit,
                    peer: peer_http,
                    most: 0,
                    active_transfers: has.active_transfers,
                });
            }
        }
    }

    /// Number of units won by a SHA-256-verified LAN peer.
    pub fn peer_wins(&self) -> u64 {
        self.peer_wins
    }

    /// Number of peer replies rejected by the LAN/SSRF IP filter.
    pub fn rejected_peers(&self) -> u64 {
        self.rejected_peers
    }

    /// Whether `ip` has been permanently banned for this command (mismatch, redirect, or refusal).
    pub fn is_peer_banned(&self, ip: IpAddr) -> bool {
        self.banned_peers.iter().any(|&banned| banned == ip)
    }

    fn ban_peer(&mut self, ip: IpAddr) {
        if !self.is_peer_banned(ip) {
            self.banned_peers.push(ip);
        }
        self.peer_ready.retain(|p| p.peer.ip() != ip);
        let mut at = 0;
        while let Some(&unit) = self.peer_asked.get(at) {
            let Ok(idx) = usize::try_from(unit) else {
                at += 1;
                continue;
            };
            if let Some(st) = self.states.get_mut(idx)
                && let Some(pf) = st.peer
                && pf.peer.ip() == ip
            {
                st.peer = None;
                self.peer_asked.remove_swap(at);
                self.in_flight -= pf.most;
                self.cancels.push_back(Ask { unit, seq: pf.seq });
                self.finish_peer_failure(unit);
            } else {
                at += 1;
            }
        }
    }

    fn cancel_peer_flight(&mut self, unit: u32) {
        let Ok(idx) = usize::try_from(unit) else { return };
        self.peer_ready.retain(|p| p.unit != unit);
        if let Some(st) = self.states.get_mut(idx)
            && let Some(pf) = st.peer.take()
        {
            self.peer_asked.retain(|&u| u != unit);
            self.in_flight -= pf.most;
            self.cancels.push_back(Ask { unit, seq: pf.seq });
        }
    }

    fn finish_peer_failure(&mut self, unit: u32) {
        let Ok(idx) = usize::try_from(unit) else { return };
        let Some(&st) = self.states.get(idx) else { return };
        if st.phase == Phase::WaitingPeer
            && st.peer.is_none()
            && !self.peer_ready.iter().any(|p| p.unit == unit)
        {
            let why = self
                .wan_deferred
                .get_mut(idx)
                .and_then(Option::take)
                .unwrap_or(Why::Net { failure: NetFailure::Reset });
            self.fail(unit, why);
        }
    }

    fn wan_fail_or_wait(&mut self, unit: u32, why: Why) {
        let Ok(idx) = usize::try_from(unit) else { return };
        if let Some(st) = self.states.get_mut(idx)
            && st.peer.is_some()
        {
            if let Some(slot) = self.wan_deferred.get_mut(idx) {
                *slot = Some(why);
            }
            st.phase = Phase::WaitingPeer;
        } else {
            self.fail(unit, why);
        }
    }

    /// The `most` of `unit`'s next request.
    fn next_most(&self, unit: u32) -> u64 {
        let state = &self.states[unit as usize];
        let Phase::Ready(step) = state.phase else { unreachable!("only a ready unit is queued") };
        if state.most != 0 { state.most } else { self.first_most(step) }
    }

    fn first_most(&self, step: Step) -> u64 {
        let first = if step == Step::GitFetch { self.limits.git_first_most } else { self.limits.first_most };
        self.most(step).min(first)
    }

    /// The most bytes in flight at once so far ([`Limits::bytes_most`]).
    pub fn in_flight_peak(&self) -> u64 {
        self.in_flight_peak
    }

    fn hold(&mut self, bytes: u64) {
        self.in_flight += bytes;
        self.in_flight_peak = self.in_flight_peak.max(self.in_flight);
    }

    /// Wants `unit`: its record is looked up, then fetched if the store lacks it, and it has an
    /// outcome before [`Machine::is_done`] is true again. False, and nothing done, for a unit
    /// that is not one of the machine's or is wanted already.
    pub fn want(&mut self, unit: u32) -> bool {
        match self.states.get_mut(unit as usize) {
            Some(state) if state.phase == Phase::Idle => {
                state.phase = Phase::Wanted;
                self.lookups.push_back(unit);
                self.left += 1;
                true
            }
            _ => false,
        }
    }

    /// The store holds `unit`'s record, or not.
    pub fn looked(&mut self, unit: u32, held: bool) {
        let Some(state) = self.states.get_mut(unit as usize) else { return };
        if state.phase != Phase::Looking {
            self.stale += 1;
            return;
        }
        if held {
            self.finish(unit, Outcome::Present);
            return;
        }
        match self.units[unit as usize] {
            // The order's second place before the network: this machine's archives.
            Unit::Crate { .. } => {
                self.states[unit as usize].phase = Phase::Local;
                self.locals.push_back(unit);
            }
            Unit::Git { .. } => {
                self.states[unit as usize].phase = Phase::Ready(Step::Advert);
                self.ready.push_back(unit);
            }
        }
    }

    /// The unit `unit` names: what [`Effect::Local`]'s driver looks for.
    pub fn unit(&self, unit: u32) -> &'p Unit<'l, 'a> {
        &self.units[unit as usize]
    }

    /// The allocator the machine's buffers come from: a driver's buffer for a local archive is
    /// made in it.
    pub fn alloc(&self) -> Alloc<'a> {
        self.alloc
    }

    /// The answer to [`Effect::Local`] for `unit`: `found`, and with [`LocalArchive::Found`]
    /// the archive's whole bytes. Found bytes are adopted only through [`local::adopt_in`] (the
    /// lock's SHA-256 first); a small record is queued as [`Effect::Put`], a large one asks the
    /// driver to reserve it now ([`Adopt::Reserve`]). Absent, unreadable or refused, the try is
    /// kept ([`Machine::tried`]) and the unit goes on to HTTPS.
    pub fn local_found(&mut self, unit: u32, found: LocalArchive, archive: &[u8]) -> Adopt {
        let Some(state) = self.states.get(unit as usize).copied() else { return Adopt::Done };
        if state.phase != Phase::Local {
            self.stale += 1;
            return Adopt::Done;
        }
        match found {
            LocalArchive::Absent => self.to_network(unit, Miss::Absent, None),
            LocalArchive::Unreadable { why } => self.to_network(unit, Miss::Unreadable, Some(why)),
            LocalArchive::Found => {
                let room = unpack::crate_room(archive, &self.limits.unpack);
                if room.stated >= self.limits.reserve_from {
                    // Made in place: the archive's bytes, still the driver's, are held against
                    // the budget until `adopt_into`.
                    let key = self.units[unit as usize].key();
                    let body = archive.len() as u64;
                    self.hold(body);
                    let state = &mut self.states[unit as usize];
                    state.held = body;
                    state.phase = Phase::Adopting;
                    return Adopt::Reserve { key, room: room.most };
                }
                match self.adopt(unit, archive, Vec::new_in(self.alloc)) {
                    Ok((record, made)) => {
                        let key = self.units[unit as usize].key();
                        self.hold(record.len() as u64);
                        let state = &mut self.states[unit as usize];
                        state.held = record.len() as u64;
                        state.adopted = true;
                        state.phase = Phase::Storing(made);
                        self.puts.push_back((unit, key, record));
                    }
                    Err(refused) => self.to_network(unit, Miss::Refused(refused), None),
                }
            }
        }
        Adopt::Done
    }

    /// The reservation [`Adopt::Reserve`] asked for `unit`, or why the store made none, with
    /// `archive`, the bytes the driver kept from [`Effect::Local`]. They are adopted into the
    /// reservation's bytes only through [`local::adopt_in`]: `Some(len)` is the length to
    /// finish it at, then answer [`Machine::stored`]; `None`, the reservation is abandoned and
    /// the unit failed (the store's [`Why`]), went on to HTTPS (the archive refused, the try
    /// kept), or the call was stale.
    pub fn adopt_into(&mut self, unit: u32, archive: &[u8], room: Result<&mut [u8], StoreFailure>) -> Option<u64> {
        let state = self.states.get(unit as usize).copied()?;
        if state.phase != Phase::Adopting {
            self.stale += 1;
            return None;
        }
        self.in_flight -= state.held;
        self.states[unit as usize].held = 0;
        let room = match room {
            Ok(room) => room,
            Err(failure) => {
                self.fail(unit, Why::Store(failure));
                return None;
            }
        };
        match self.adopt(unit, archive, Fixed::new(room)) {
            Ok((out, made)) => {
                let len = out.written();
                self.hold(len);
                let state = &mut self.states[unit as usize];
                state.held = len;
                state.adopted = true;
                state.phase = Phase::Storing(made);
                Some(len)
            }
            Err(refused) => {
                self.to_network(unit, Miss::Refused(refused), None);
                None
            }
        }
    }

    /// `unit`'s record made from `archive`, found on this machine, into `out`: by
    /// [`local::adopt_in`] alone, which compares its SHA-256 with the lock's before a byte is
    /// inflated.
    fn adopt<S: Sink>(&self, unit: u32, archive: &[u8], out: S) -> Result<(S, Made), AdoptRefused> {
        let unit = &self.units[unit as usize];
        if local_planted(LocalPlant::AdoptBeforeChecksum) {
            let Unit::Crate { name, version, sha256, .. } = unit else { return Err(AdoptRefused::NotACrate) };
            return unpack::crate_record_in(name, version.as_str(), *sha256, archive, &self.limits.unpack, out, self.alloc)
                .map_err(AdoptRefused::Unpack);
        }
        local::adopt_in(unit, archive, &self.limits.unpack, out, self.alloc)
    }

    /// The local step gave nothing adopted: the try is kept and `unit` goes on to the order's
    /// next place, the registry over HTTPS (never this place again).
    fn to_network(&mut self, unit: u32, miss: Miss, why: Option<&'static str>) {
        self.tried[unit as usize] = Some(LocalTry { miss, why });
        self.states[unit as usize].phase = Phase::Ready(Step::Crate);
        self.ready.push_back(unit);
    }

    /// What the local step gave `unit` when it adopted nothing, as one try of the finding that
    /// names every place ([`local::Tried`]); `None` when it was not asked or adopted.
    pub fn tried(&self, unit: u32) -> Option<Try<'static>> {
        let tried = self.tried.get(unit as usize)?.as_ref()?;
        Some(Try { place: local::CARGO_CACHE, miss: tried.miss.clone() })
    }

    /// Why `unit`'s local archive could not be read ([`LocalArchive::Unreadable`]), as the IO
    /// said.
    pub fn unreadable(&self, unit: u32) -> Option<&'static str> {
        self.tried.get(unit as usize)?.as_ref()?.why
    }

    /// Whether `unit`'s record was made from an archive found on this machine (its outcome is
    /// [`Outcome::Fetched`] with `down` 0) rather than downloaded.
    pub fn adopted(&self, unit: u32) -> bool {
        self.states.get(unit as usize).is_some_and(|state| state.adopted)
    }

    /// The answer to `ask`. No time is taken: nothing waits on an answer (no backoff, D172), and
    /// a deadline is checked at the next [`Machine::poll`].
    pub fn answered(&mut self, ask: Ask, reply: Reply<'_>) {
        let Ok(idx) = usize::try_from(ask.unit) else { return };
        let Some(state) = self.states.get(idx).copied() else { return };
        if let Some(pf) = state.peer
            && pf.seq == ask.seq
        {
            self.peer_answered(ask.unit, pf, reply);
            return;
        }
        let Phase::Asked { step, seq, .. } = state.phase else {
            self.stale += 1;
            return;
        };
        if seq != ask.seq {
            self.stale += 1;
            return;
        }
        self.asked.retain(|it| *it != ask.unit);
        self.in_flight -= state.most;
        let unit = ask.unit;
        match reply {
            // Past a first request's `most`: asked again once, sized to the answer.
            Reply::Failed(NetFailure::TooBig { said }) if state.most < self.most(step) => {
                // At most three requests: the first, then the stated length (once, from the
                // first; past the budget it waits to go alone), then, for an answer that states
                // none, the step's limit held to the budget. Each `most` is above the last.
                let limit = self.most(step);
                let first = state.most == self.first_most(step);
                let most = match said {
                    Some(said) if said > limit => {
                        self.wan_fail_or_wait(unit, Why::TooBig { most: limit, got: said });
                        return;
                    }
                    Some(said) if first && said > state.most => said,
                    _ if limit.min(self.limits.bytes_most) > state.most => limit.min(self.limits.bytes_most),
                    _ => {
                        self.wan_fail_or_wait(unit, Why::Net { failure: NetFailure::TooBig { said } });
                        return;
                    }
                };
                let state = &mut self.states[idx];
                state.most = most;
                state.phase = Phase::Ready(step);
                self.ready.push_back(unit);
            }
            // One try (D172): every other failure, the network's or the server's, is the unit's
            // outcome now, never a backoff and a second ask.
            Reply::Failed(failure) => self.wan_fail_or_wait(unit, Why::Net { failure }),
            Reply::Body { status, .. } if status != 200 => self.wan_fail_or_wait(unit, Why::Status { status }),
            Reply::Body { bytes, .. } => {
                let most = self.most(step);
                let got_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                if got_len > most {
                    self.wan_fail_or_wait(unit, Why::TooBig { most, got: got_len });
                    return;
                }
                self.states[idx].down += got_len;
                self.take(unit, step, bytes);
            }
        }
    }

    /// Handles a reply to a LAN peer HTTP request. Never fails the unit on peer errors,
    /// timeouts, redirects, or SHA-256 mismatches; bans malicious/corrupted peers permanently
    /// for the command and lets `static.crates.io` win.
    fn peer_answered(&mut self, unit: u32, pf: PeerFlight, reply: Reply<'_>) {
        let Ok(idx) = usize::try_from(unit) else { return };
        self.peer_asked.retain(|&u| u != unit);
        self.in_flight -= pf.most;
        if let Some(st) = self.states.get_mut(idx) {
            st.peer = None;
        }
        match reply {
            Reply::Body { status: 200, bytes } => {
                let got_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                if got_len > self.limits.crate_most {
                    self.ban_peer(pf.peer.ip());
                    self.finish_peer_failure(unit);
                    return;
                }
                let Some(&Unit::Crate { sha256, .. }) = self.units.get(idx) else {
                    self.finish_peer_failure(unit);
                    return;
                };
                let got = blaze_sha256::digest(bytes);
                if got != sha256 && !planted(Plant::StoreBeforeChecksum) {
                    self.ban_peer(pf.peer.ip());
                    self.finish_peer_failure(unit);
                    return;
                }
                // SHA-256 verified! Cancel the racing WAN request and unpack this `.crate`.
                let Some(st) = self.states.get_mut(idx) else { return };
                match st.phase {
                    Phase::Asked { seq: wan_seq, .. } => {
                        let wan_most = st.most;
                        self.asked.retain(|&u| u != unit);
                        self.in_flight -= wan_most;
                        self.cancels.push_back(Ask { unit, seq: wan_seq });
                    }
                    Phase::Ready(Step::Crate) => {
                        self.ready.retain(|&u| u != unit);
                    }
                    Phase::WaitingPeer => {}
                    _ => {
                        self.stale += 1;
                        return;
                    }
                }
                self.peer_ready.retain(|p| p.unit != unit);
                if let Some(slot) = self.wan_deferred.get_mut(idx) {
                    *slot = None;
                }
                self.peer_wins = self.peer_wins.saturating_add(1);
                if let Some(st) = self.states.get_mut(idx) {
                    st.down += got_len;
                }
                self.unpack_verified_crate(unit, bytes);
            }
            Reply::Body { status, .. } if (300..400).contains(&status) => {
                // Redirects from a LAN peer are forbidden (SSRF vector) -> permanent ban.
                self.ban_peer(pf.peer.ip());
                self.finish_peer_failure(unit);
            }
            Reply::Failed(NetFailure::Refused) => {
                // Redirect refused (`redirects: 0`) or malformed peer HTTP -> permanent ban.
                self.ban_peer(pf.peer.ip());
                self.finish_peer_failure(unit);
            }
            Reply::Failed(NetFailure::TooBig { said: Some(said) })
                if pf.most < self.limits.crate_most && said > pf.most && said <= self.limits.crate_most =>
            {
                if !self.is_peer_banned(pf.peer.ip()) {
                    self.peer_ready.push_back(PendingPeer {
                        unit,
                        peer: pf.peer,
                        most: said,
                        active_transfers: 0,
                    });
                }
                self.finish_peer_failure(unit);
            }
            Reply::Body { .. } | Reply::Failed(_) => {
                self.finish_peer_failure(unit);
            }
        }
    }

    /// Unpacks a `.crate` whose SHA-256 has ALREADY been verified against the lock.
    fn unpack_verified_crate(&mut self, unit: u32, bytes: &[u8]) {
        let Ok(idx) = usize::try_from(unit) else { return };
        let limits = self.limits.unpack;
        let Some(Unit::Crate { name, version, sha256, .. }) = self.units.get(idx) else { return };
        let room = unpack::crate_room(bytes, &limits);
        if room.stated >= self.limits.reserve_from {
            let body_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            self.reserve(unit, Step::Crate, body_len, room.most);
            return;
        }
        match unpack::crate_record(name, version, *sha256, bytes, &limits, self.alloc) {
            Ok((record, made)) => {
                let Some(u) = self.units.get(idx) else { return };
                let key = u.key();
                let rec_len = u64::try_from(record.len()).unwrap_or(u64::MAX);
                self.hold(rec_len);
                if let Some(st) = self.states.get_mut(idx) {
                    st.held = rec_len;
                    st.phase = Phase::Storing(made);
                }
                self.puts.push_back((unit, key, record));
            }
            Err(why) => self.fail(unit, Why::Unpack(why)),
        }
    }

    /// The store kept `unit`'s record, or did not.
    pub fn stored(&mut self, unit: u32, kept: Result<(), StoreFailure>) {
        let Some(state) = self.states.get(unit as usize).copied() else { return };
        let Phase::Storing(made) = state.phase else {
            self.stale += 1;
            return;
        };
        self.in_flight -= state.held;
        self.states[unit as usize].held = 0;
        match kept {
            Ok(()) => {
                self.finish(unit, Outcome::Fetched { down: state.down, files: made.files, bytes: made.bytes, cache: made.cache })
            }
            Err(failure) => self.fail(unit, Why::Store(failure)),
        }
    }

    /// The earliest time something is due without a completion: a request's deadline (there is
    /// no backoff: one try, D172).
    pub fn next_deadline(&self) -> Option<u64> {
        let wan = self.asked.iter().filter_map(|unit| match self.states[*unit as usize].phase {
            Phase::Asked { deadline, .. } => Some(deadline),
            _ => None,
        });
        let peer = self.peer_asked.iter().filter_map(|unit| {
            let idx = usize::try_from(*unit).ok()?;
            self.states.get(idx)?.peer.map(|pf| pf.deadline)
        });
        wan.chain(peer).min()
    }

    /// Every unit wanted has its outcome.
    pub fn is_done(&self) -> bool {
        self.left == 0
    }

    pub fn outcome(&self, unit: u32) -> &Outcome {
        &self.outcomes[unit as usize]
    }

    pub fn outcomes(&self) -> &[Outcome] {
        &self.outcomes
    }

    /// Answers that came for no request in flight (after their deadline, or twice).
    pub fn stale(&self) -> u64 {
        self.stale
    }

    fn most(&self, step: Step) -> u64 {
        match step {
            Step::Crate => self.limits.crate_most,
            Step::Advert => self.limits.advert_most,
            Step::GitFetch => self.limits.git_most,
        }
    }

    /// Requests past their deadline failed as timeouts, at once (one try, D172).
    fn expire(&mut self, now: u64) {
        let mut at = 0;
        while at < self.asked.len() {
            let unit = self.asked[at];
            let state = self.states[unit as usize];
            match state.phase {
                Phase::Asked { deadline, .. } if deadline <= now => {
                    self.asked.swap_remove(at);
                    self.in_flight -= state.most;
                    self.wan_fail_or_wait(unit, Why::Net { failure: NetFailure::Timeout });
                }
                _ => at += 1,
            }
        }
        let mut pat = 0;
        while let Some(&unit) = self.peer_asked.get(pat) {
            let Ok(idx) = usize::try_from(unit) else {
                pat += 1;
                continue;
            };
            if let Some(st) = self.states.get_mut(idx)
                && let Some(pf) = st.peer
                && pf.deadline <= now
            {
                st.peer = None;
                self.peer_asked.remove_swap(pat);
                self.in_flight -= pf.most;
                self.cancels.push_back(Ask { unit, seq: pf.seq });
                self.finish_peer_failure(unit);
            } else {
                pat += 1;
            }
        }
    }

    fn ask(&mut self, unit: u32, now: u64) -> Effect<'a> {
        let Phase::Ready(step) = self.states[unit as usize].phase else {
            unreachable!("only a ready unit is queued")
        };
        let alloc = self.alloc;
        let mut url = String::new_in(alloc);
        let mut body = Vec::new_in(alloc);
        let (method, headers): (Method, &'static [(&'static str, &'static str)]) = match (&self.units[unit as usize], step) {
            (Unit::Crate { name, version, .. }, _) => {
                url.str(CRATES).str(name).str("/").str(name).str("-").str(version).str(".crate");
                (Method::Get, &[])
            }
            (Unit::Git { repo, .. }, Step::Advert) => {
                url.str(repo.trim_end_matches('/')).str("/info/refs?service=git-upload-pack");
                (Method::Get, &ADVERTISE)
            }
            (Unit::Git { repo, commit, .. }, _) => {
                url.str(repo.trim_end_matches('/')).str("/git-upload-pack");
                blaze_git_fetch::fetch_request(*commit, 1, &mut body).expect("a fetch request of one commit fits any limit");
                (Method::Post, &UPLOAD)
            }
        };
        let deadline = now.saturating_add(self.limits.ask_ns);
        let most = self.next_most(unit);
        self.hold(most);
        let state = &mut self.states[unit as usize];
        state.most = most;
        state.seq += 1;
        state.phase = Phase::Asked { step, seq: state.seq, deadline };
        let ask = Ask { unit, seq: state.seq };
        self.asked.push(unit);
        Effect::Http {
            ask,
            method,
            url,
            headers,
            body,
            most,
            deadline_ns: deadline,
            https_only: true,
            redirects: 10,
        }
    }

    /// A whole answer of `step`, its length checked: verified, then unpacked or followed.
    fn take(&mut self, unit: u32, step: Step, bytes: &[u8]) {
        let limits = self.limits.unpack;
        let made = match (&self.units[unit as usize], step) {
            (Unit::Crate { name, version, sha256, .. }, _) => {
                let got = blaze_sha256::digest(bytes);
                if got != *sha256 && !planted(Plant::StoreBeforeChecksum) {
                    self.wan_fail_or_wait(unit, Why::Mismatch { want: *sha256, got });
                    return;
                }
                // WAN succeeded with verified SHA-256: cancel any racing LAN peer request.
                self.cancel_peer_flight(unit);
                let room = unpack::crate_room(bytes, &limits);
                if room.stated >= self.limits.reserve_from {
                    self.reserve(unit, step, bytes.len() as u64, room.most);
                    return;
                }
                unpack::crate_record(name, version, *sha256, bytes, &limits, self.alloc)
            }
            (Unit::Git { .. }, Step::Advert) => {
                match blaze_git_fetch::check_advertisement(bytes) {
                    Ok(()) => {
                        let state = &mut self.states[unit as usize];
                        state.most = 0;
                        state.phase = Phase::Ready(Step::GitFetch);
                        self.ready.push_back(unit);
                    }
                    Err(why) => self.fail(unit, Why::Git(why)),
                }
                return;
            }
            (Unit::Git { .. }, _) => {
                self.reserve(unit, step, bytes.len() as u64, unpack::git_room(&limits));
                return;
            }
        };
        match made {
            Ok((record, made)) => {
                let key = self.units[unit as usize].key();
                self.hold(record.len() as u64);
                self.states[unit as usize].held = record.len() as u64;
                self.states[unit as usize].phase = Phase::Storing(made);
                self.puts.push_back((unit, key, record));
            }
            Err(why) => self.fail(unit, Why::Unpack(why)),
        }
    }

    /// Whether `unit` waits for a reservation ([`Effect::Reserve`], asked or about to be): the
    /// answer last given for it is to be kept until [`Machine::reserved`], and no other is.
    pub fn reserving(&self, unit: u32) -> bool {
        self.states.get(unit as usize).is_some_and(|state| matches!(state.phase, Phase::Reserving(_)))
    }

    /// Asks for `unit`'s record to be made in place, in `room` bytes; the answer's `body`
    /// bytes, which the driver keeps until [`Machine::reserved`], are held against the budget.
    fn reserve(&mut self, unit: u32, step: Step, body: u64, room: u64) {
        let key = self.units[unit as usize].key();
        self.hold(body);
        let state = &mut self.states[unit as usize];
        state.held = body;
        state.phase = Phase::Reserving(step);
        self.reserves.push_back((unit, key, room));
    }

    /// The reservation [`Effect::Reserve`] asked for `unit`, or why the store made none, with
    /// `body`, the answer the driver kept. The record is written into the reservation's bytes:
    /// `Some(len)` is the length to finish it at, then answer [`Machine::stored`]; `None`, the
    /// unit failed (a typed [`Why`]) or the call was stale, and the reservation is abandoned.
    pub fn reserved(&mut self, unit: u32, body: &[u8], room: Result<&mut [u8], StoreFailure>) -> Option<u64> {
        let state = self.states.get(unit as usize).copied()?;
        let Phase::Reserving(step) = state.phase else {
            self.stale += 1;
            return None;
        };
        self.in_flight -= state.held;
        self.states[unit as usize].held = 0;
        let room = match room {
            Ok(room) => room,
            Err(failure) => {
                self.fail(unit, Why::Store(failure));
                return None;
            }
        };
        let limits = self.limits.unpack;
        let out = Fixed::new(room);
        let made = match (&self.units[unit as usize], step) {
            (Unit::Crate { name, version, sha256, .. }, _) => {
                unpack::crate_record_in(name, version, *sha256, body, &limits, out, self.alloc)
            }
            (Unit::Git { commit, .. }, _) => {
                unpack::git_record_in(*commit, body, self.limits.git_most, &limits, out, self.alloc)
            }
        };
        match made {
            Ok((out, made)) => {
                let len = out.written();
                self.hold(len);
                let state = &mut self.states[unit as usize];
                state.held = len;
                state.phase = Phase::Storing(made);
                Some(len)
            }
            Err(why) => {
                self.fail(unit, Why::Unpack(why));
                None
            }
        }
    }

    fn fail(&mut self, unit: u32, why: Why) {
        self.finish(unit, Outcome::Failed(why));
    }

    fn finish(&mut self, unit: u32, outcome: Outcome) {
        self.states[unit as usize].phase = Phase::Done;
        self.outcomes[unit as usize] = outcome;
        self.left -= 1;
    }
}
