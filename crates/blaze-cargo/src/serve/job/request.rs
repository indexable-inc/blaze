//! The request machine: a request's first state is its certificate
//! (docs/v2/plan/design/certificate.md; end-state.md, level 1).
//!
//! One [`Request`] a request, stepped by the daemon's loop before any plan, view walk or job:
//!
//! 1. **Find** the read-lists filed under the request's base ([`certificate::base`]): the
//!    store's, read through `blaze_record::walk`.
//! 2. **Value** each read the lists name, as it is now: the machine asks ([`Ask`]), the
//!    driver looks and says ([`Request::told`]): a node of the pinned view, a variable, a
//!    file's or a listing's digest outside the views. A list that does not name a variable
//!    read unasked that is set now is passed over: it was unset when that list's build ran.
//! 3. The answer under the first list's bytes and those digests; then each **output**: a
//!    file's stamp, and where no local evidence vouches for the content under that stamp
//!    ([`Evidence`]), stat, hash, stat again; the code a place that is no file holds
//!    ([`certificate::holder_of`]: the launch host's program, the daemon's code memory), by
//!    the identity of the build it is of, asked once, with no stamp.
//! 4. [`Next::Hit`]: what the build said is said again; no job. Or [`Next::Miss`], naming
//!    what moved ([`Why`]): the reads whose digest is another than this checkout's last build
//!    had. The one walk is the engine's; it is handed those names.
//! 5. After the build: [`Request::built`], and where every step was traced the record is
//!    **written**: each node the build read valued in the view the build read with its own
//!    writes in it ([`View::Built`]), never the checkout as it is at the build's end, so a
//!    file saved under the build misses the next request. An untraced step writes nothing,
//!    by name ([`Unwritten::Untraced`]).
//!
//! The invariant: a hit is given only when a fresh build would say the same bytes and leave
//! the same outputs.
//!
//! A sans-IO core (rules 14, 64): the store is handed in as `blaze_record`'s get and put;
//! every look at the world is an ask the driver answers. Every wait has a name
//! ([`Request::waiting`]) and a bound ([`MOST_IDLE`] steps with nothing told end it in
//! [`Stopped::Stalled`]).

// Until `Jobs::begin` drives it (certificate.md, "State").
#![allow(dead_code)]

use std::{convert::Infallible, fmt};

use blaze_ids::ContentId;
use blaze_record::{AnswerKey, Bounds, Digest, Doing, Filing, Kept, Kind, Placed, Store, Walked};

use super::certificate::{self, Answer, Certificate, Evidence, Output, Read, Stamp, Undecoded};

/// The chain of read-lists under a base: past it the oldest list is no longer searched.
pub(super) const BOUNDS: Bounds = Bounds::STANDARD;

/// The most steps in a row with asks open and nothing told before the machine ends in
/// [`Stopped::Stalled`]: the bound of each of its waits (rule 18).
pub(super) const MOST_IDLE: u32 = 1 << 12;

/// Which state of the checkout a node is read in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(super) enum View {
    /// The request's pinned view: its checkout as of the watcher's flush for this request.
    Now = 0,
    /// The view the build read, with the build's own writes (its lock files) in it. A change
    /// made under the build by anyone else is not in it.
    Built = 1,
}

/// A look at the world the driver takes for the machine.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Ask {
    /// The digest of `read` ([`Told::Read`]). `view` says where a [`Read::Node`] is read; a
    /// variable is the request's own, which no build changes.
    Read { read: Read, view: View },
    /// The stamp of the output at `path` under base `base` ([`Told::Stamp`]).
    Stamp { base: u8, path: String },
    /// The hash of the bytes of the output at `path` under base `base` ([`Told::Content`]).
    Content { base: u8, path: String },
}

impl Ask {
    /// What orders asks and tells two apart: the kind, the view or base, the read's kind,
    /// the text.
    fn key(&self) -> (u8, u8, u8, &str) {
        match self {
            Ask::Read { read, view } => {
                let (kind, text) = read.parts();
                (0, *view as u8, kind, text)
            }
            Ask::Stamp { base, path } => (1, *base, 0, path.as_str()),
            Ask::Content { base, path } => (2, *base, 0, path.as_str()),
        }
    }
}

/// What the driver saw.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Told {
    Read(Digest),
    /// None: nothing is there. `looked_ns`: when it looked, by the clock change times are of.
    Stamp { stamp: Option<Stamp>, looked_ns: u64 },
    /// None: nothing is there.
    Content(Option<ContentId>),
}

/// The driver used the machine out of turn: a bug in the driver, named.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Misused(pub(super) &'static str);

impl fmt::Display for Misused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the request machine was {}", self.0)
    }
}

/// The asks open in the machine's present state, each with what was told of it.
#[derive(Default)]
struct Pending {
    /// Sorted by [`Ask::key`], each once.
    asks: Vec<(Ask, Option<Told>)>,
    left: usize,
    /// Whether the open asks were handed to the driver.
    given: bool,
}

impl Pending {
    fn new(mut asks: Vec<Ask>) -> Pending {
        asks.sort_by(|a, b| a.key().cmp(&b.key()));
        asks.dedup_by(|a, b| a.key() == b.key());
        let left = asks.len();
        // alloc: one slot an ask of the state, once a state
        Pending { asks: asks.into_iter().map(|ask| (ask, None)).collect(), left, given: false }
    }

    fn at(&self, key: (u8, u8, u8, &str)) -> Option<usize> {
        self.asks.binary_search_by(|(ask, _)| ask.key().cmp(&key)).ok()
    }

    fn told(&mut self, ask: &Ask, told: Told) -> Result<(), Misused> {
        let fits = matches!(
            (ask, &told),
            (Ask::Read { .. }, Told::Read(_))
                | (Ask::Stamp { .. }, Told::Stamp { .. })
                | (Ask::Content { .. }, Told::Content(_))
        );
        if !fits {
            return Err(Misused("told another kind of thing than it asked"));
        }
        let Some(at) = self.at(ask.key()) else {
            return Err(Misused("told of a look it did not ask for"));
        };
        if self.asks[at].1.replace(told).is_some() {
            return Err(Misused("told of one look twice"));
        }
        self.left -= 1;
        Ok(())
    }

    fn open(&self) -> Vec<Ask> {
        // alloc: the asks handed to the driver, once a state
        self.asks.iter().filter(|(_, told)| told.is_none()).map(|(ask, _)| ask.clone()).collect()
    }

    fn get(&self, key: (u8, u8, u8, &str)) -> Result<Told, Misused> {
        self.at(key)
            .and_then(|at| self.asks[at].1)
            .ok_or(Misused("stepped past a look it was not told of"))
    }

    fn read(&self, read: &Read, view: View) -> Result<Digest, Misused> {
        let (kind, text) = read.parts();
        match self.get((0, view as u8, kind, text))? {
            Told::Read(digest) => Ok(digest),
            _ => Err(Misused("told another kind of thing than it asked")),
        }
    }

    fn stamp(&self, output: &Output) -> Result<(Option<Stamp>, u64), Misused> {
        match self.get((1, output.base, 0, output.path.as_str()))? {
            Told::Stamp { stamp, looked_ns } => Ok((stamp, looked_ns)),
            _ => Err(Misused("told another kind of thing than it asked")),
        }
    }

    fn content(&self, output: &Output) -> Result<Option<ContentId>, Misused> {
        match self.get((2, output.base, 0, output.path.as_str()))? {
            Told::Content(content) => Ok(content),
            _ => Err(Misused("told another kind of thing than it asked")),
        }
    }
}

/// How an output is not the one the certificate names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum How {
    Gone,
    /// Other bytes are there.
    Changed,
    /// Its stamp moved between the two looks around its hash: it is being written now.
    Unsteady,
}

/// Why a request is not certified. These and no other: not a restart, not another request's
/// build, not the build's own writes, not a variable or a file no step read, not a file
/// touched and left as it was.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Why {
    /// The request asks for the build's trace, which a build makes.
    Traced,
    /// Nothing is filed under its shape and tool: the first build of that shape by that tool.
    Unfiled,
    /// These reads are not as this checkout's last build of the shape read them: where the
    /// engine's walk starts. Sorted; never empty.
    Moved(Vec<Read>),
    /// No list filed has an answer under what its reads are now, and this daemon holds no
    /// build of this checkout to compare with (a restart, then a change): nothing is named.
    Unnamed,
    /// An output the certificate names is not there as it left it: it is placed again from
    /// the store; no read moved.
    Output { base: u8, path: String, how: How },
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Why::Traced => f.write_str("the request asks for the build's trace, which a build makes"),
            Why::Unfiled => f.write_str("no build of this request's shape by this blaze has answered yet"),
            Why::Moved(reads) => {
                let first = reads.first().map(ToString::to_string).unwrap_or_default();
                match reads.len() {
                    0 | 1 => write!(f, "{first} changed, which the last build read"),
                    n => write!(f, "{first} and {} more input(s) changed, which the last build read", n - 1),
                }
            }
            Why::Unnamed => f.write_str(
                "an input changed since the builds on record, and this daemon has seen no build of this checkout to say which",
            ),
            Why::Output { base, path, how } if certificate::holder_of(*base).is_some() => {
                match (certificate::holder_of(*base), how) {
                    (Some(holder), How::Gone) => write!(f, "{holder} holds no code of {path}, which a run of it executes"),
                    (Some(holder), _) => write!(f, "{holder} holds the code of another build of {path} than this one left"),
                    (None, _) => write!(f, "the output {path} is not as the build left it"),
                }
            }
            Why::Output { path, how: How::Gone, .. } => write!(f, "the output {path} is gone"),
            Why::Output { path, how: How::Changed, .. } => write!(f, "the output {path} holds other bytes than the build left"),
            Why::Output { path, how: How::Unsteady, .. } => write!(f, "the output {path} is being written"),
        }
    }
}

/// A request not certified: why, and what of the store's record was refused on the way.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Miss {
    pub(super) why: Why,
    /// Each part of the record that was not read, by its reason (a previous layout, by
    /// name): said as a warning, never passed over quietly.
    pub(super) refused: Vec<Undecoded>,
    /// How many read-lists were searched.
    pub(super) lists: u32,
}

/// Why a build that ended left no certificate.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Unwritten {
    /// The build failed.
    Failed,
    /// These units ran a step whose reads were not traced (sorted): its inputs cannot be
    /// claimed. The driver says so once, naming them, and counts them in the build's
    /// profile row.
    Untraced(Vec<String>),
    /// The build read this at two values: it moved under the build.
    TwoValues(Read),
    /// Another answer is under this key already: the build read something its list does not
    /// name, or said something that is no function of what it read (rule 21).
    TwoAnswers(ContentId),
    /// The chain under this base is at its bound.
    Full(ContentId),
}

impl fmt::Display for Unwritten {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unwritten::Failed => f.write_str("the build failed: it leaves no request certificate"),
            Unwritten::Untraced(units) => write!(
                f,
                "no request certificate is written: {} ran a step whose reads blaze could not trace, so the next request builds again",
                units.join(", "),
            ),
            Unwritten::TwoValues(read) => {
                write!(f, "no request certificate is written: {read} changed while the build read it")
            }
            Unwritten::TwoAnswers(key) => write!(
                f,
                "no request certificate is written: another answer is under {key:x} for the same reads; the build read or said something its reads do not name",
            ),
            Unwritten::Full(base) => write!(
                f,
                "no request certificate is written: every slot for read-lists under {base:x} is taken",
            ),
        }
    }
}

/// What the driver does next.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Next {
    /// Look at each and say what is there ([`Request::told`]), in any order, then step.
    Asks(Vec<Ask>),
    /// Stepped with `left` asks still open; `on` names the wait.
    Waiting { on: &'static str, left: usize },
    /// Certified: say again what the build said. No plan, no job.
    Hit(Answer),
    /// Not certified: build, then [`Request::built`] or [`Request::failed`], then step.
    Miss(Miss),
    /// The build's certificate is in the store under `key`.
    Written { key: ContentId, list: Placed, answer: Kept },
    Unwritten(Unwritten),
}

/// Why the machine stopped with no outcome. Never a miss.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Stopped<E> {
    /// The store, or what a slot of the chain holds.
    Record(blaze_record::Error<E>),
    /// [`MOST_IDLE`] steps with `left` asks open and nothing told, while waiting for `on`.
    Stalled { on: &'static str, left: usize },
    Misused(Misused),
}

impl<E> From<blaze_record::Error<E>> for Stopped<E> {
    fn from(error: blaze_record::Error<E>) -> Stopped<E> {
        Stopped::Record(error)
    }
}

impl<E> From<Misused> for Stopped<E> {
    fn from(misused: Misused) -> Stopped<E> {
        Stopped::Misused(misused)
    }
}

impl<E: fmt::Display> fmt::Display for Stopped<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stopped::Record(error) => write!(f, "the request certificate: {error}"),
            Stopped::Stalled { on, left } => {
                write!(f, "{on}: {left} still open after {MOST_IDLE} steps with nothing told")
            }
            Stopped::Misused(misused) => misused.fmt(f),
        }
    }
}

/// What a build that answered hands the machine.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub(super) struct Built {
    /// The nodes of the view the build read, by path ([`Read::Node`]): valued by the machine
    /// in [`View::Built`].
    pub(super) nodes: Vec<String>,
    /// Every other read with the digest the build read it at: a variable from the request's
    /// environment; a file or a listing outside the views from the traced step's own
    /// record of what it read, never from a look at the build's end.
    pub(super) valued: Vec<(Read, Digest)>,
    /// The outputs, each with the stamp it was placed under and when that was seen (the
    /// placement record's), where known.
    pub(super) outputs: Vec<(Output, Option<(Stamp, u64)>)>,
    /// The units' record keys, in the plan's order.
    pub(super) units: Vec<ContentId>,
    /// What a build with nothing to do says of this state, relocated.
    pub(super) said: Vec<u8>,
    /// Each unit that ran a step whose reads were not traced, by name.
    pub(super) untraced: Vec<String>,
}

/// A read-list as the store holds it.
struct Filed {
    bytes: Vec<u8>,
    reads: Vec<Read>,
}

/// An answer found, with the reads it is under as they are now.
struct Found {
    answer: Answer,
    reads: Vec<(Read, Digest)>,
}

enum State {
    Finding,
    Valuing { lists: Vec<Filed> },
    /// The first look at each output.
    Stamping { found: Found },
    /// `need`: the outputs (by index) no evidence vouches for, each with its first stamp.
    Hashing { found: Found, need: Vec<(usize, Stamp)> },
    /// The second look, with the hash taken between.
    Restamping { found: Found, need: Vec<(usize, Stamp, Option<ContentId>)> },
    Building,
    Rooting { built: Built },
    Unwritten(Unwritten),
    Done,
}

/// A bug planted in the machine, for a test's control: with it the machine gives a hit it
/// must not, and the test's oracle says so. Never planted outside a test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Plant {
    None,
    /// The record leaves out every variable read.
    DropsEnv,
    /// The record leaves out every read that found nothing (a negative probe).
    DropsProbes,
    /// The nodes are valued in the checkout as it is at the build's end, not in the view the
    /// build read.
    RootAfterBuild,
    /// The outputs are not looked at.
    IgnoresOutputs,
    /// A stamp taken within [`certificate::RACY_NS`] of the change vouches.
    TrustsRacyStamps,
    /// A variable read unasked that is set now and that the list does not name is let by.
    ForgetsUnasked,
    /// A build's outputs leave out the code a place that is no file holds
    /// (`certify::outputs_of`): the launcher's file alone is recorded, as it was until
    /// 2026-11-12.
    LeavesCodeOut,
}

#[cfg(test)]
thread_local! {
    pub(super) static PLANT: std::cell::Cell<Plant> = const { std::cell::Cell::new(Plant::None) };
}

#[cfg(test)]
pub(super) fn planted(plant: Plant) -> bool {
    PLANT.get() == plant
}

#[cfg(not(test))]
pub(super) fn planted(plant: Plant) -> bool {
    let _ = plant;
    false
}

fn names_env(reads: &[Read], name: &str) -> bool {
    reads.iter().any(|read| matches!(read, Read::Env(it) if it == name))
}

/// One request against its certificate.
pub(super) struct Request {
    base: ContentId,
    traced: bool,
    /// The variables read unasked (the plan's, `BLAZE_*`) that are set in the request's
    /// environment, sorted.
    unasked: Vec<String>,
    state: State,
    pending: Pending,
    idle: u32,
    refused: Vec<Undecoded>,
    searched: u32,
}

impl Request {
    /// A request under `base` ([`certificate::base`]). `traced`: it asks for the build's
    /// trace. `unasked`: the name of each variable set in its environment that a build reads
    /// without a step asking for it by name.
    pub(super) fn new(base: ContentId, traced: bool, mut unasked: Vec<String>) -> Request {
        unasked.sort();
        unasked.dedup();
        Request {
            base,
            traced,
            unasked,
            state: State::Finding,
            pending: Pending::default(),
            idle: 0,
            refused: Vec::new(), // alloc: empty
            searched: 0,
        }
    }

    /// A request under `base` that is built without its check (the daemon's first build,
    /// which mounts the library its shape names; a build whose check was not run): it is
    /// in its build, and [`Request::built`] writes its record as any other's.
    pub(super) fn building(base: ContentId, unasked: Vec<String>) -> Request {
        let mut request = Request::new(base, false, unasked);
        request.state = State::Building;
        request
    }

    /// The content the record found names for the output at `path` under `base`, while the
    /// machine looks at the outputs: the form the driver says the file's identity in (a file
    /// hashed whole and a file blaze's linker made are named by different hashes of the
    /// same bytes).
    pub(super) fn wanted(&self, base: u8, path: &str) -> Option<ContentId> {
        let found = match &self.state {
            State::Stamping { found } | State::Hashing { found, .. } | State::Restamping { found, .. } => found,
            _ => return None,
        };
        found.answer.outputs.iter().find(|it| it.base == base && it.path == path).map(|it| it.content)
    }

    /// What the machine waits for now: the name of the driver's span around the wait
    /// (rule 15).
    pub(super) fn waiting(&self) -> &'static str {
        match self.state {
            State::Finding => "request certificate: the read-lists filed under the request's shape",
            State::Valuing { .. } => "request certificate: the value now of each read on record",
            State::Stamping { .. } | State::Restamping { .. } => "request certificate: a look at each output",
            State::Hashing { .. } => "request certificate: the hash of each output no stamp vouches for",
            State::Building => "request certificate: the build",
            State::Rooting { .. } => "request certificate: the build's view of each node it read",
            State::Unwritten(_) | State::Done => "request certificate: nothing",
        }
    }

    /// What the driver saw of `ask`.
    pub(super) fn told(&mut self, ask: &Ask, told: Told) -> Result<(), Misused> {
        self.pending.told(ask, told)?;
        self.idle = 0;
        Ok(())
    }

    /// The build this request's miss asked for answered. Then step: the record is written.
    pub(super) fn built(&mut self, mut built: Built) -> Result<(), Misused> {
        if !matches!(self.state, State::Building) {
            return Err(Misused("handed a build it did not ask for"));
        }
        if !built.untraced.is_empty() {
            built.untraced.sort();
            built.untraced.dedup();
            self.state = State::Unwritten(Unwritten::Untraced(built.untraced));
            return Ok(());
        }
        let view = if planted(Plant::RootAfterBuild) { View::Now } else { View::Built };
        let mut asks = Vec::with_capacity(built.nodes.len() + self.unasked.len()); // alloc: once a build
        for path in &built.nodes {
            // clone: an ask's own name, once a node a build
            asks.push(Ask::Read { read: Read::Node(path.clone()), view });
        }
        if !planted(Plant::ForgetsUnasked) {
            for name in &self.unasked {
                if !built.valued.iter().any(|(read, _)| matches!(read, Read::Env(it) if it == name)) {
                    // clone: as above, a variable
                    asks.push(Ask::Read { read: Read::Env(name.clone()), view: View::Now });
                }
            }
        }
        self.pending = Pending::new(asks);
        self.state = State::Rooting { built };
        Ok(())
    }

    /// The build this request's miss asked for failed. Then step: [`Unwritten::Failed`].
    pub(super) fn failed(&mut self) -> Result<(), Misused> {
        if !matches!(self.state, State::Building) {
            return Err(Misused("told of a build it did not ask for"));
        }
        self.state = State::Unwritten(Unwritten::Failed);
        Ok(())
    }

    /// The next thing to do. `evidence`: what this machine knows beside the records of this
    /// request's checkout and shape.
    pub(super) fn step<S: Store>(
        &mut self,
        store: &S,
        evidence: &mut Evidence,
    ) -> Result<Next, Stopped<S::Error>> {
        if self.pending.left > 0 {
            if !self.pending.given {
                self.pending.given = true;
                return Ok(Next::Asks(self.pending.open()));
            }
            self.idle += 1;
            let (on, left) = (self.waiting(), self.pending.left);
            if self.idle > MOST_IDLE {
                self.state = State::Done;
                self.pending = Pending::default();
                return Err(Stopped::Stalled { on, left });
            }
            return Ok(Next::Waiting { on, left });
        }
        self.idle = 0;
        // An error below leaves the machine done: nothing is tried again.
        match std::mem::replace(&mut self.state, State::Done) {
            State::Finding => self.find(store, evidence),
            State::Valuing { lists } => self.answer(store, evidence, lists),
            State::Stamping { found } => self.stamped(store, evidence, found),
            State::Hashing { found, need } => self.hashed(store, evidence, found, need),
            State::Restamping { found, need } => {
                self.restamped(evidence, found, need).map_err(Stopped::Misused)
            }
            State::Building => {
                self.state = State::Building;
                Err(Stopped::Misused(Misused("stepped while its build runs")))
            }
            State::Rooting { built } => self.write(store, evidence, built),
            State::Unwritten(why) => Ok(Next::Unwritten(why)),
            State::Done => Err(Stopped::Misused(Misused("stepped past its end"))),
        }
    }

    /// Opens `asks` for `state` and steps: the asks are handed over, or with none the state
    /// goes on at once.
    fn ask<S: Store>(
        &mut self,
        asks: Vec<Ask>,
        state: State,
        store: &S,
        evidence: &mut Evidence,
    ) -> Result<Next, Stopped<S::Error>> {
        self.pending = Pending::new(asks);
        self.state = state;
        self.step(store, evidence)
    }

    fn miss(&mut self, why: Why) -> Next {
        self.state = State::Building;
        self.pending = Pending::default();
        Next::Miss(Miss { why, refused: std::mem::take(&mut self.refused), lists: self.searched })
    }

    /// Whether a list of `reads` names every variable read unasked that is set now.
    fn covers(&self, reads: &[Read]) -> bool {
        planted(Plant::ForgetsUnasked) || self.unasked.iter().all(|name| names_env(reads, name))
    }

    fn find<S: Store>(&mut self, store: &S, evidence: &mut Evidence) -> Result<Next, Stopped<S::Error>> {
        if self.traced {
            return Ok(self.miss(Why::Traced));
        }
        let mut lists: Vec<Filed> = Vec::new(); // alloc: the lists searched, at most `3 * width - 1`
        let mut refused = Vec::new(); // alloc: a refusal a list not read, as above
        let walked = blaze_record::walk::<S, Infallible, Stopped<S::Error>, _>(
            store,
            BOUNDS,
            &self.base,
            |_at, bytes, list| {
                match certificate::reads_of(list) {
                    // alloc: the list's bytes, which its answer is keyed by
                    Ok(reads) => lists.push(Filed { bytes: bytes.to_vec(), reads }),
                    Err(why) => refused.push(why),
                }
                Ok(None)
            },
        )?;
        self.searched = match walked {
            Walked::Passed { lists, .. } => lists,
            Walked::Found { found, .. } => match found {},
        };
        self.refused = refused;
        if lists.is_empty() {
            return Ok(self.miss(Why::Unfiled));
        }
        let mut asks = Vec::new(); // alloc: an ask a read on record, once a request
        for list in &lists {
            if self.covers(&list.reads) {
                // clone: an ask's own name
                asks.extend(list.reads.iter().map(|read| Ask::Read { read: read.clone(), view: View::Now }));
            }
        }
        if let Some(last) = &evidence.last {
            // clone: as above
            asks.extend(last.iter().map(|(read, _)| Ask::Read { read: read.clone(), view: View::Now }));
        }
        self.ask(asks, State::Valuing { lists }, store, evidence)
    }

    fn answer<S: Store>(
        &mut self,
        store: &S,
        evidence: &mut Evidence,
        lists: Vec<Filed>,
    ) -> Result<Next, Stopped<S::Error>> {
        let mut valued = 0_u64;
        let mut found = None;
        for list in &lists {
            if !self.covers(&list.reads) {
                continue;
            }
            let mut key = AnswerKey::new(&list.bytes);
            let mut reads = Vec::with_capacity(list.reads.len()); // alloc: a list's reads as they are now
            for read in &list.reads {
                let digest = self.pending.read(read, View::Now)?;
                key.value(digest);
                reads.push((read.clone(), digest)); // clone: kept as the checkout's last reads
            }
            valued += reads.len() as u64;
            let key = key.finish();
            let held = store.get(Kind::Answer, &key).map_err(|error| {
                Stopped::Record(blaze_record::Error::Store { doing: Doing::ReadingAnswer, key, error })
            })?;
            let Some(held) = held else { continue };
            match Answer::decode(S::bytes(&held)) {
                Ok(answer) => {
                    found = Some(Found { answer, reads });
                    break;
                }
                Err(why) => self.refused.push(why),
            }
        }
        store.tally().valued(valued);
        let Some(found) = found else {
            let why = self.moved(evidence)?;
            return Ok(self.miss(why));
        };
        if planted(Plant::IgnoresOutputs) || found.answer.outputs.is_empty() {
            return Ok(self.hit(evidence, found));
        }
        let asks = found
            .answer
            .outputs
            .iter()
            // clone: an ask's own name, once an output a request
            .map(|output| match certificate::holder_of(output.base) {
                // The code a holder holds is read in place, once: no stamp vouches for it.
                Some(_) => Ask::Content { base: output.base, path: output.path.clone() },
                None => Ask::Stamp { base: output.base, path: output.path.clone() },
            })
            .collect(); // alloc: an ask an output
        self.ask(asks, State::Stamping { found }, store, evidence)
    }

    /// What no answer was found for: the reads that are not as this checkout's last build
    /// read them.
    fn moved(&self, evidence: &Evidence) -> Result<Why, Misused> {
        let Some(last) = &evidence.last else { return Ok(Why::Unnamed) };
        let mut moved = Vec::new(); // alloc: the reads that moved, once a miss
        for (read, was) in last {
            if self.pending.read(read, View::Now)? != *was {
                moved.push(read.clone()); // clone: a reason's name, once
            }
        }
        for name in &self.unasked {
            if !last.iter().any(|(read, _)| matches!(read, Read::Env(it) if it == name)) {
                moved.push(Read::Env(name.clone())); // clone: as above
            }
        }
        moved.sort();
        moved.dedup();
        Ok(if moved.is_empty() { Why::Unnamed } else { Why::Moved(moved) })
    }

    fn hit(&mut self, evidence: &mut Evidence, found: Found) -> Next {
        evidence.last = Some(found.reads);
        evidence.only(&found.answer.outputs);
        self.state = State::Done;
        Next::Hit(found.answer)
    }

    fn output_miss(&mut self, output: &Output, how: How) -> Next {
        // clone: a reason's name, once
        self.miss(Why::Output { base: output.base, path: output.path.clone(), how })
    }

    fn stamped<S: Store>(
        &mut self,
        store: &S,
        evidence: &mut Evidence,
        found: Found,
    ) -> Result<Next, Stopped<S::Error>> {
        let mut need = Vec::new(); // alloc: an entry an output no stamp vouches for
        for (at, output) in found.answer.outputs.iter().enumerate() {
            if certificate::holder_of(output.base).is_some() {
                match self.pending.content(output)? {
                    Some(content) if content == output.content => continue,
                    Some(_) => return Ok(self.output_miss(output, How::Changed)),
                    None => return Ok(self.output_miss(output, How::Gone)),
                }
            }
            let Some(stamp) = self.pending.stamp(output)?.0 else {
                return Ok(self.output_miss(output, How::Gone));
            };
            match evidence.vouched(output.base, &output.path, stamp) {
                Some(content) if content == output.content => {}
                Some(_) => return Ok(self.output_miss(output, How::Changed)),
                None => need.push((at, stamp)),
            }
        }
        if need.is_empty() {
            return Ok(self.hit(evidence, found));
        }
        let asks = need
            .iter()
            .map(|&(at, _)| {
                let output = &found.answer.outputs[at];
                Ask::Content { base: output.base, path: output.path.clone() } // clone: an ask's own name
            })
            .collect(); // alloc: an ask an output hashed
        self.ask(asks, State::Hashing { found, need }, store, evidence)
    }

    fn hashed<S: Store>(
        &mut self,
        store: &S,
        evidence: &mut Evidence,
        found: Found,
        need: Vec<(usize, Stamp)>,
    ) -> Result<Next, Stopped<S::Error>> {
        let mut hashed = Vec::with_capacity(need.len()); // alloc: as `need`
        let mut asks = Vec::with_capacity(need.len()); // alloc: an ask an output hashed
        for (at, stamp) in need {
            let output = &found.answer.outputs[at];
            hashed.push((at, stamp, self.pending.content(output)?));
            asks.push(Ask::Stamp { base: output.base, path: output.path.clone() }); // clone: an ask's own name
        }
        self.ask(asks, State::Restamping { found, need: hashed }, store, evidence)
    }

    fn restamped(
        &mut self,
        evidence: &mut Evidence,
        found: Found,
        need: Vec<(usize, Stamp, Option<ContentId>)>,
    ) -> Result<Next, Misused> {
        for &(at, first, content) in &need {
            let output = &found.answer.outputs[at];
            let (second, looked_ns) = self.pending.stamp(output)?;
            let how = match (second, content) {
                (None, _) | (_, None) => Some(How::Gone),
                (Some(second), _) if second != first => Some(How::Unsteady),
                (_, Some(content)) if content != output.content => Some(How::Changed),
                _ => None,
            };
            if let Some(how) = how {
                evidence.forget(output.base, &output.path);
                return Ok(self.output_miss(output, how));
            }
            if planted(Plant::TrustsRacyStamps) {
                #[cfg(test)]
                evidence.keep_racy(output.base, &output.path, first, output.content);
            } else {
                evidence.keep(output.base, &output.path, first, output.content, looked_ns);
            }
        }
        Ok(self.hit(evidence, found))
    }

    fn write<S: Store>(
        &mut self,
        store: &S,
        evidence: &mut Evidence,
        built: Built,
    ) -> Result<Next, Stopped<S::Error>> {
        let Built { nodes, valued: mut reads, outputs, units, said, untraced: _ } = built;
        let view = if planted(Plant::RootAfterBuild) { View::Now } else { View::Built };
        for path in nodes {
            let read = Read::Node(path);
            let digest = self.pending.read(&read, view)?;
            reads.push((read, digest));
        }
        if !planted(Plant::ForgetsUnasked) {
            for name in &self.unasked {
                if !reads.iter().any(|(read, _)| matches!(read, Read::Env(it) if it == name)) {
                    let read = Read::Env(name.clone()); // clone: a read's own name, once a build
                    let digest = self.pending.read(&read, View::Now)?;
                    reads.push((read, digest));
                }
            }
        }
        if planted(Plant::DropsEnv) {
            reads.retain(|(read, _)| !matches!(read, Read::Env(_)));
        }
        if planted(Plant::DropsProbes) {
            reads.retain(|(_, digest)| *digest != Digest::Nothing);
        }
        // clone: the record's own outputs; the stamps stay beside, in `outputs`
        let answer = Answer { outputs: outputs.iter().map(|(output, _)| output.clone()).collect(), units, said };
        let certificate = match Certificate::new(reads, answer) {
            Ok(it) => it,
            Err(read) => {
                evidence.last = None;
                return Ok(Next::Unwritten(Unwritten::TwoValues(read)));
            }
        };
        let list = certificate.list(&self.base);
        let placed = Filing::new(self.base, BOUNDS).place(store, &list)?;
        if placed == Placed::Full {
            evidence.last = None;
            return Ok(Next::Unwritten(Unwritten::Full(self.base)));
        }
        let key = certificate.answer_key(&list);
        let kept = match blaze_record::keep_answer(store, key, &certificate.answer().encode()) {
            Ok(kept) => kept,
            Err(blaze_record::Error::TwoAnswers { key }) => {
                evidence.last = None;
                return Ok(Next::Unwritten(Unwritten::TwoAnswers(key)));
            }
            Err(error) => return Err(Stopped::Record(error)),
        };
        evidence.last = Some(certificate.reads().to_vec()); // alloc: the checkout's last reads, once a build
        evidence.only(&certificate.answer().outputs);
        for (output, seen) in &outputs {
            match *seen {
                #[cfg(test)]
                Some((stamp, _)) if planted(Plant::TrustsRacyStamps) => {
                    evidence.keep_racy(output.base, &output.path, stamp, output.content);
                }
                Some((stamp, looked_ns)) => {
                    evidence.keep(output.base, &output.path, stamp, output.content, looked_ns);
                }
                None => evidence.forget(output.base, &output.path),
            }
        }
        Ok(Next::Written { key, list: placed, answer: kept })
    }
}

#[cfg(test)]
mod tests;
