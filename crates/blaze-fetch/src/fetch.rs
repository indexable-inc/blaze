//! The fetch machine (sans-IO, rule 14): a plan's units in, effects out, completions in, one
//! [`Outcome`] a unit. It owns no socket, file or clock: the time comes with each call and each
//! request carries its deadline.
//!
//! A unit goes:
//!
//! 1. [`Effect::Lookup`]: does the store hold its key? Held, it is [`Outcome::Present`] and
//!    nothing is asked (a second run fetches nothing).
//! 2. [`Effect::Http`]: a crate's `.crate` from crates.io's static host; for a git commit, the
//!    advertisement (`info/refs`, protocol v2), then a `fetch` of the commit at depth 1. At most
//!    [`Limits::in_flight_most`] at once, each with a byte limit and a deadline. A request
//!    past its deadline is failed by the machine itself and its late answer ignored.
//! 3. A transient failure (no connection, a reset, a timeout, a 429 or a 5xx) is tried again
//!    after a backoff, at most [`Limits::tries_most`] tries in all; anything else fails the
//!    unit with a typed [`Why`] at once. A wrong digest is never tried again: it is the bytes'
//!    fault, not the network's.
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

use blaze_git_fetch::PktWhy;
use blaze_ids::ContentId;
use blaze_own::{Alloc, String, Vec, VecDeque};
use blaze_text::{Put, Show, Writes as _};

use crate::pack::Fixed;
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
    /// Tries of one request in all, the first included, for transient failures only.
    pub tries_most: u32,
    /// The wait before the second try; doubled for each try after.
    pub backoff_ns: u64,
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
        tries_most: 4,
        backoff_ns: 500_000_000,
        crate_most: 256 << 20,
        advert_most: 1 << 20,
        git_most: 1 << 30,
        // From blaze-core's `examples/reserve_cost.rs` on an M-series Mac (2026-11-16): a
        // reservation costs a `Put` of the same bytes, 0.88 to 1.01 of it, from 16 KiB to 16 MiB
        // (the two syncs dominate both), so from 16 KiB a record is never whole in the heap.
        reserve_from: 16 << 10,
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
    /// An HTTPS request: answer with [`Machine::answered`]. A body past `most` bytes is not
    /// read ([`NetFailure::TooBig`]); `deadline_ns` is on the clock the machine is given.
    Http {
        ask: Ask,
        method: Method,
        url: String<Alloc<'a>>,
        headers: &'static [(&'static str, &'static str)],
        body: Vec<u8, Alloc<'a>>,
        most: u64,
        deadline_ns: u64,
    },
    /// Keep `record` at `key`, whole or not at all: answer with [`Machine::stored`].
    Put { unit: u32, key: ContentId, record: Vec<u8, Alloc<'a>> },
    /// Reserve `room` bytes in place for `unit`'s record at `key` (a writable mapping of a
    /// scratch file, `blaze_core::reserve::Reserve`), keeping the body of the answer last
    /// given for `unit`: answer with [`Machine::reserved`], then finish the reservation at the
    /// length it gives (or abandon it at `None`), then [`Machine::stored`].
    Reserve { unit: u32, key: ContentId, room: u64 },
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
    fn transient(self) -> bool {
        matches!(self, NetFailure::Connect | NetFailure::Reset | NetFailure::Timeout)
    }

    fn name(self) -> &'static str {
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
    /// The server answered `status` (after `tries` tries).
    Status { status: u16, tries: u32 },
    Net { failure: NetFailure, tries: u32 },
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
            Why::Status { status, tries } => out.str("the server answered ").int(*status).str(" (").int(*tries).str(" tries)"),
            Why::Net { failure, tries } => out.str(failure.name()).str(" (").int(*tries).str(" tries)"),
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
    Ready(Step),
    Asked { step: Step, seq: u32, deadline: u64 },
    Backoff { step: Step, until: u64 },
    /// A reservation asked: the driver keeps the answer's body until [`Machine::reserved`].
    Reserving(Step),
    Storing(Made),
    Done,
}

#[derive(Clone, Copy)]
struct State {
    phase: Phase,
    /// Tries of the current step so far.
    tries: u32,
    seq: u32,
    down: u64,
    /// The next or current request's `most`: 0 for the step's first.
    most: u64,
    /// Bytes of the record made and not yet stored.
    held: u64,
}

/// The machine: see the module.
pub struct Machine<'p, 'l, 'a> {
    units: &'p [Unit<'l, 'a>],
    states: Vec<State, Alloc<'a>>,
    outcomes: Vec<Outcome, Alloc<'a>>,
    limits: Limits,
    /// Units wanted and not yet looked up, in the order wanted.
    lookups: VecDeque<u32, Alloc<'a>>,
    ready: VecDeque<u32, Alloc<'a>>,
    asked: Vec<u32, Alloc<'a>>,
    waiting: Vec<u32, Alloc<'a>>,
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
        for _ in units {
            states.push(State { phase: Phase::Idle, tries: 0, seq: 0, down: 0, most: 0, held: 0 });
            outcomes.push(Outcome::Pending);
        }
        Machine {
            units,
            states,
            outcomes,
            limits,
            lookups: VecDeque::new_in(alloc),
            ready: VecDeque::new_in(alloc),
            asked: Vec::new_in(alloc),
            waiting: Vec::new_in(alloc),
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
        if let Some(unit) = self.lookups.pop_front() {
            self.states[unit as usize].phase = Phase::Looking;
            return Some(Effect::Lookup { unit, key: self.units[unit as usize].key() });
        }
        if let Some((unit, key, record)) = self.puts.pop_front() {
            return Some(Effect::Put { unit, key, record });
        }
        if let Some((unit, key, room)) = self.reserves.pop_front() {
            return Some(Effect::Reserve { unit, key, room });
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
        let step = match self.units[unit as usize] {
            Unit::Crate { .. } => Step::Crate,
            Unit::Git { .. } => Step::Advert,
        };
        self.states[unit as usize].phase = Phase::Ready(step);
        self.ready.push_back(unit);
    }

    /// The answer to `ask`, at `now`.
    pub fn answered(&mut self, ask: Ask, now: u64, reply: Reply<'_>) {
        let Some(state) = self.states.get(ask.unit as usize).copied() else { return };
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
        let tries = state.tries;
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
                        self.fail(unit, Why::TooBig { most: limit, got: said });
                        return;
                    }
                    Some(said) if first && said > state.most => said,
                    _ if limit.min(self.limits.bytes_most) > state.most => limit.min(self.limits.bytes_most),
                    _ => {
                        self.fail(unit, Why::Net { failure: NetFailure::TooBig { said }, tries });
                        return;
                    }
                };
                let state = &mut self.states[unit as usize];
                state.most = most;
                state.tries -= 1;
                state.phase = Phase::Ready(step);
                self.ready.push_back(unit);
            }
            Reply::Failed(failure) if failure.transient() => self.again(unit, step, now, Why::Net { failure, tries }),
            Reply::Failed(failure) => self.fail(unit, Why::Net { failure, tries }),
            Reply::Body { status, .. } if status == 429 || (500..600).contains(&status) => {
                self.again(unit, step, now, Why::Status { status, tries })
            }
            Reply::Body { status, .. } if status != 200 => self.fail(unit, Why::Status { status, tries }),
            Reply::Body { bytes, .. } => {
                let most = self.most(step);
                if bytes.len() as u64 > most {
                    self.fail(unit, Why::TooBig { most, got: bytes.len() as u64 });
                    return;
                }
                self.states[unit as usize].down += bytes.len() as u64;
                self.take(unit, step, bytes);
            }
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

    /// The earliest time something is due without a completion: a request's deadline or a
    /// backoff's end.
    pub fn next_deadline(&self) -> Option<u64> {
        let asked = self.asked.iter().filter_map(|unit| match self.states[*unit as usize].phase {
            Phase::Asked { deadline, .. } => Some(deadline),
            _ => None,
        });
        let waiting = self.waiting.iter().filter_map(|unit| match self.states[*unit as usize].phase {
            Phase::Backoff { until, .. } => Some(until),
            _ => None,
        });
        asked.chain(waiting).min()
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

    /// Requests past their deadline failed as timeouts; backoffs ended made ready.
    fn expire(&mut self, now: u64) {
        let mut at = 0;
        while at < self.asked.len() {
            let unit = self.asked[at];
            let state = self.states[unit as usize];
            match state.phase {
                Phase::Asked { step, deadline, .. } if deadline <= now => {
                    self.asked.swap_remove(at);
                    self.in_flight -= state.most;
                    self.again(unit, step, now, Why::Net { failure: NetFailure::Timeout, tries: state.tries });
                }
                _ => at += 1,
            }
        }
        let mut at = 0;
        while at < self.waiting.len() {
            let unit = self.waiting[at];
            match self.states[unit as usize].phase {
                Phase::Backoff { step, until } if until <= now => {
                    self.waiting.swap_remove(at);
                    self.states[unit as usize].phase = Phase::Ready(step);
                    self.ready.push_back(unit);
                }
                _ => at += 1,
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
        state.tries += 1;
        state.phase = Phase::Asked { step, seq: state.seq, deadline };
        let ask = Ask { unit, seq: state.seq };
        self.asked.push(unit);
        Effect::Http { ask, method, url, headers, body, most, deadline_ns: deadline }
    }

    /// A transient failure: tried again after a backoff, or failed once tries are spent.
    fn again(&mut self, unit: u32, step: Step, now: u64, why: Why) {
        let tries = self.states[unit as usize].tries;
        if tries >= self.limits.tries_most {
            self.fail(unit, why);
            return;
        }
        let wait = self.limits.backoff_ns.saturating_mul(1 << tries.saturating_sub(1).min(16));
        self.states[unit as usize].phase = Phase::Backoff { step, until: now.saturating_add(wait) };
        self.waiting.push(unit);
    }

    /// A whole answer of `step`, its length checked: verified, then unpacked or followed.
    fn take(&mut self, unit: u32, step: Step, bytes: &[u8]) {
        let limits = self.limits.unpack;
        let made = match (&self.units[unit as usize], step) {
            (Unit::Crate { name, version, sha256, .. }, _) => {
                let got = blaze_sha256::digest(bytes);
                if got != *sha256 && !planted(Plant::StoreBeforeChecksum) {
                    self.fail(unit, Why::Mismatch { want: *sha256, got });
                    return;
                }
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
                        state.tries = 0;
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
