//! A client's own check of an unchanged request: the request machine ([`Request`]) driven
//! over a slot ([`crate::slot`]) and `lstat`s, with no daemon, no store opened and no lease
//! (/tmp/plan/10-no-daemon-run.md, sections 1 and A; daemon.md 4.3).
//!
//! `Solo` adds no rule of its own. The machine's store is the slot's records (the gets the
//! daemon's machine made, with what its store held); each read the machine asks for is
//! valued from what the slot saved, and only where the stamps that vouch for it are now the
//! saved ones, whole, and the saved ones were steady when looked at (`blaze_vfs::racy`). Where
//! anything does not vouch, it does not answer: [`Verdict::Daemon`] names why, and the
//! request goes to the daemon as it would with no slot, which decides by its own first state.
//! That is the design's "on `Failed` the client sends `Build`", not a second route to an
//! answer.
//!
//! The steps:
//!
//! 1. **Open** ([`Solo::open`]): the slot's bytes, refused by name where they are not this
//!    request's to be checked by ([`Why::Layout`], [`Why::Traced`], [`Why::Engine`],
//!    [`Why::Names`], [`Why::Sweep`]).
//! 2. **The first look** ([`Look::Stamps`]): every premise, column name (as `dir/name`),
//!    local name and outside path, in that order. A saved stamp vouches when it is known,
//!    was steady at its `looked_ns`, and is the stamp now, whole; the first that does not is
//!    the verdict.
//! 3. **The machine**: `Request::new(slot.base, false, slot.unasked)` over the slot's records
//!    and `Evidence::seeded` from the slot's placed outputs. A node of the view is valued only
//!    at its root (`Read::Node("")`, the column's `root`); a variable from the request's own
//!    environment ([`env_digest`]); an outside read from the slot; an output's stamp by a
//!    look ([`Look::Stamps`] of its path); a launcher's code by its start file's head
//!    ([`Look::Head`]). An output no stamp vouches for is [`Why::Hashes`]: hashing it is the
//!    daemon's (it holds what it was hashed to), never a client's.
//!
//! The clock: none. The racy question is asked of each saved stamp at the time it was saved
//! at, which is when it was looked at: a stamp racy then never vouches, whatever the time
//! now, and a stamp steady then that equals the stamp now names the same file (an edit in a
//! steady stamp's own tick cannot leave it, since a steady stamp's tick is over).
//!
//! The invariant: [`Verdict::Hit`]`(a)` is given only where the machine driven by the
//! daemon's own looks at the world says `a` (solo/tests.rs).
//!
//! A sans-IO core (rule 14): every look is a [`Step::Look`] the driver answers with a
//! [`Seen`]. Bounded (rule 18): at most [`MOST_ROUNDS`] steps of the machine, each look one
//! batch, every count bounded by what the slot holds.

#![blaze::sans_io]

use std::{cell::RefCell, convert::Infallible, fmt};

use blaze_core::Stamp;
use blaze_ids::ContentId;
use blaze_record::{Digest, Kind, Store, Tally};

use crate::certificate::{self, ABSOLUTE, Answer, Evidence, LAUNCHER, MEMORY, Read, env_digest};
use crate::request::{Ask, Next, Request, Stopped, Told};
use crate::slot::{Slot, START_HEAD, START_MAGIC, Undecoded, absolute, names_of, start_file};

/// The most column names a client sweeps where a daemon holds `serve.lock`: past it the
/// request goes to the daemon, whose watch is O(changed). With no daemon the sweep is the
/// only evidence and is taken, however many.
pub const SWEEP_MOST: usize = 16_384;

/// The most steps of the machine one check takes: a check has five (find, value, stamp,
/// hash, stamp again); past it the verdict is [`Why::Stopped`].
pub const MOST_ROUNDS: u32 = 16;

/// A look at the world the driver takes for the check.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Look {
    /// The `lstat` of each path, in order: [`Seen::Stamps`], one a path, `Stamp::GONE`
    /// where nothing is there, `Stamp::UNKNOWN` where the system would not say.
    Stamps(Vec<String>),
    /// The first `len` bytes of the file at `path` ([`Seen::Head`]): fewer where it is
    /// shorter, `None` where no file is there.
    Head { path: String, len: usize },
}

/// What the driver saw of the last [`Look`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Seen {
    Stamps(Vec<Stamp>),
    Head(Option<Vec<u8>>),
}

/// Why a client does not answer alone: the request goes to the daemon. Each names its thing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Why {
    /// The slot's bytes are not read.
    Layout(Undecoded),
    /// The request asks for the build's trace, which a build makes.
    Traced,
    /// The slot was written by the engine at `slot`; this client's is at `here`.
    Engine { slot: String, here: String },
    /// The variables set in the request's environment are not the ones the slot's were.
    Names,
    /// A daemon holds `serve.lock`, and the column has more than [`SWEEP_MOST`] names.
    Sweep { names: usize },
    /// The stamp of this premise (the engine, the library) is not the saved one.
    Premise(String),
    /// The stamp of this name of the checkout's column is not the saved one, or the machine
    /// asked for a node of the view below its root.
    Column(String),
    /// The stamp of this name of a path dependency's column is not the saved one.
    Local(String),
    /// The stamp of this path outside every view is not the saved one, or the machine asked
    /// for an outside read the slot does not hold.
    Outside(String),
    /// The machine asked for a record the slot does not hold.
    Record(ContentId),
    /// An output no stamp vouches for: its hash is the daemon's to take.
    Hashes(String),
    /// The code the daemon's code memory holds for this program: only the daemon can say.
    Memory(String),
    /// The machine missed: the text of its reason.
    Miss(String),
    /// The check stopped with no outcome: what stopped it.
    Stopped(String),
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Why::Layout(undecoded) => write!(f, "the request slot is not read: {undecoded}"),
            Why::Traced => f.write_str("the request asks for the build's trace, which a build makes"),
            Why::Engine { slot, here } => {
                write!(f, "the request slot was written by the blaze at {slot}, not this one at {here}")
            }
            Why::Names => f.write_str("the variables set are not the ones set when the request slot was written"),
            Why::Sweep { names } => write!(
                f,
                "the checkout has {names} names, more than the {SWEEP_MOST} a client looks at while a daemon watches it"
            ),
            Why::Premise(path) => write!(f, "{path} changed since the request slot was written"),
            Why::Column(name) => write!(f, "the checkout's {name} changed since the request slot was written"),
            Why::Local(path) => write!(f, "the path dependency's {path} changed since the request slot was written"),
            Why::Outside(path) => write!(f, "{path}, outside the checkout, is not as the request slot saw it"),
            Why::Record(key) => write!(f, "the request slot holds no record under {key:x}, which the check reads"),
            Why::Hashes(path) => write!(f, "no stamp vouches for the output {path}: the daemon hashes it"),
            Why::Memory(path) => write!(f, "only the daemon's code memory says which code of {path} it holds"),
            Why::Miss(why) => f.write_str(why),
            Why::Stopped(what) => write!(f, "the client's check stopped: {what}"),
        }
    }
}

/// A check's outcome.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// Certified: say again what the build said.
    Hit(Answer),
    /// The daemon answers: why.
    Daemon(Why),
}

/// What the driver does next.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Step {
    /// Look, then [`Solo::step`] with what was seen.
    Look(Look),
    Done(Verdict),
}

/// A bug planted in the client's check, for a test's control: with it the check gives a hit
/// it must not, and the test's oracle says so. Never planted outside a test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// A saved stamp taken inside its racy window vouches.
    TrustsRacy,
    /// The variables set are not compared with the slot's.
    SkipsNames,
    /// The premises (the engine, the library) are not looked at.
    SkipsPremises,
    /// The column's directories are not looked at: a file added is missed.
    SkipsDirectories,
}

#[cfg(any(test, feature = "plants"))]
thread_local! {
    pub static PLANT: std::cell::Cell<Plant> = const { std::cell::Cell::new(Plant::None) };
}

#[cfg(any(test, feature = "plants"))]
fn planted(plant: Plant) -> bool {
    PLANT.get() == plant
}

#[cfg(not(any(test, feature = "plants")))]
fn planted(plant: Plant) -> bool {
    let _ = plant;
    false
}

/// What refused a get or a put of the check's store.
enum Refusal<E> {
    /// The slot holds no record under the key.
    Unheld,
    /// The check puts nothing: a put is a bug in the machine's use, named.
    Put,
    /// The store handed in ([`rehearse`]) failed.
    Store(E),
}

impl<E: fmt::Display> fmt::Display for Refusal<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Unheld => f.write_str("the request slot holds no record under it"),
            Refusal::Put => f.write_str("a check puts nothing in the store"),
            Refusal::Store(error) => error.fmt(f),
        }
    }
}

/// The slot's records as the machine's store: a get of a key it holds none of is refused
/// ([`Why::Record`]), never answered as absent. Every get is kept, in order.
struct Records {
    held: Vec<(ContentId, Option<Vec<u8>>)>,
    gets: RefCell<Vec<(ContentId, Option<Vec<u8>>)>>,
    tally: Tally,
}

impl Store for Records {
    type Held = Vec<u8>;
    type Error = Refusal<Infallible>;

    fn get(&self, _kind: Kind, key: &ContentId) -> Result<Option<Vec<u8>>, Self::Error> {
        let Some((_, held)) = self.held.iter().find(|(it, _)| it == key) else {
            return Err(Refusal::Unheld);
        };
        // clone: the machine's own copy and the log's, once a record a check
        self.gets.borrow_mut().push((*key, held.clone()));
        Ok(held.clone()) // clone: as above
    }

    fn put(&self, _kind: Kind, _key: ContentId, _bytes: &[u8]) -> Result<(), Self::Error> {
        Err(Refusal::Put)
    }

    fn bytes(held: &Vec<u8>) -> &[u8] {
        held
    }

    fn tally(&self) -> &Tally {
        &self.tally
    }
}

/// A store handed in ([`rehearse`]) with every get kept, in order: what a slot's records are.
struct Logged<'a, S> {
    inner: &'a S,
    gets: RefCell<Vec<(ContentId, Option<Vec<u8>>)>>,
}

impl<S: Store> Store for Logged<'_, S> {
    type Held = Vec<u8>;
    type Error = Refusal<S::Error>;

    fn get(&self, kind: Kind, key: &ContentId) -> Result<Option<Vec<u8>>, Self::Error> {
        let held = self.inner.get(kind, key).map_err(Refusal::Store)?;
        // alloc: the record's bytes, kept for the slot, once a get a rehearsal
        let held = held.as_ref().map(|held| S::bytes(held).to_vec());
        self.gets.borrow_mut().push((*key, held.clone())); // clone: the log's copy
        Ok(held)
    }

    fn put(&self, _kind: Kind, _key: ContentId, _bytes: &[u8]) -> Result<(), Self::Error> {
        Err(Refusal::Put)
    }

    fn bytes(held: &Vec<u8>) -> &[u8] {
        held
    }

    fn tally(&self) -> &Tally {
        self.inner.tally()
    }
}

/// Where the check is.
enum Phase {
    /// Nothing looked at.
    Opened,
    /// The first look is handed out.
    First,
    /// The machine runs; `handed` the look handed out for its asks, `open` its asks not yet
    /// handed.
    Machine { handed: Handed, open: Vec<Ask> },
    Done,
}

/// The look handed out for the machine's asks.
enum Handed {
    Nothing,
    /// A [`Look::Stamps`] of these [`Ask::Stamp`]s' paths, in order.
    Stamps(Vec<Ask>),
    /// A [`Look::Head`] of this [`Ask::Content`]'s start file.
    Head(Ask),
}

/// One check, with its slot.
struct Core {
    /// The slot, its records taken out into the store.
    slot: Slot,
    /// The request's environment.
    env: Vec<(String, String)>,
    machine: Request,
    evidence: Evidence,
    phase: Phase,
    rounds: u32,
}

/// The evidence of `placed` with every stamp kept, racy or not, under [`Plant::TrustsRacy`].
#[cfg(any(test, feature = "plants"))]
fn trusting_racy(mut evidence: Evidence, placed: &[crate::slot::Placed]) -> Evidence {
    if planted(Plant::TrustsRacy) {
        for it in placed {
            evidence.keep_racy(ABSOLUTE, &it.path, it.stamp, it.content);
        }
    }
    evidence
}

/// A path under `dir`, as a column's name is (`.` is `dir`).
fn under(dir: &str, name: &str) -> String {
    match name {
        "." => dir.to_owned(), // alloc: a path to look at, once a name a check
        _ => format!("{}/{name}", dir.trim_end_matches('/')), // alloc: as above
    }
}

/// Whether the stamp `saved`, looked at at `looked_ns`, vouches for the file whose stamp is
/// `now`: known, steady when looked at, and the same whole.
fn vouches(saved: &Stamp, looked_ns: u64, now: &Stamp) -> bool {
    !saved.is_unknown() && (planted(Plant::TrustsRacy) || !blaze_vfs::racy(saved, looked_ns)) && now == saved
}

impl Core {
    fn new(slot: Slot, env: &[(String, String)]) -> Core {
        let evidence = Evidence::seeded(
            ABSOLUTE,
            slot.placed.iter().map(|it| (it.path.as_str(), it.stamp, it.looked_ns, it.content)),
        );
        #[cfg(any(test, feature = "plants"))]
        let evidence = trusting_racy(evidence, &slot.placed);
        // clone: the machine's own list of the variables read unasked, once a check
        let machine = Request::new(slot.base, false, slot.unasked.clone());
        Core {
            slot,
            env: env.to_vec(), // alloc: the request's environment, once a check
            machine,
            evidence,
            phase: Phase::Opened,
            rounds: 0,
        }
    }

    /// How many paths the first look has.
    fn first_len(&self) -> usize {
        let slot = &self.slot;
        slot.premises.len()
            + slot.column.names.len()
            + slot.locals.iter().map(|it| it.names.len()).sum::<usize>()
            + slot.outside.len()
    }

    /// Every path of the first look, in its order: premises, column, locals, outside.
    fn first(&self) -> Vec<String> {
        let slot = &self.slot;
        let mut paths = Vec::with_capacity(self.first_len()); // alloc: the first look, once a check
        // alloc: a premise's path, once a check
        paths.extend(slot.premises.iter().map(|it| it.path.clone()));
        paths.extend(slot.column.names.iter().map(|(name, _)| under(&slot.column.dir, name)));
        for local in &slot.locals {
            paths.extend(local.names.iter().map(|(name, _)| under(&local.at, name)));
        }
        // alloc: an outside path, once a check
        paths.extend(slot.outside.iter().map(|it| it.read.parts().1.to_owned()));
        paths
    }

    /// The first thing the first look's stamps `now` do not vouch for, in the look's order.
    fn first_moved(&self, now: &[Stamp]) -> Option<Why> {
        let slot = &self.slot;
        let mut now = now.iter();
        for premise in &slot.premises {
            let stamp = now.next()?;
            if !planted(Plant::SkipsPremises) && !vouches(&premise.stamp, premise.looked_ns, stamp) {
                return Some(Why::Premise(premise.path.clone())); // clone: a reason's name, once
            }
        }
        let column = &slot.column;
        for (name, saved) in &column.names {
            let stamp = now.next()?;
            let skipped = planted(Plant::SkipsDirectories) && saved.is_dir();
            if !skipped && !vouches(saved, column.looked_ns, stamp) {
                return Some(Why::Column(name.clone())); // clone: as above
            }
        }
        for local in &slot.locals {
            for (name, saved) in &local.names {
                let stamp = now.next()?;
                if !vouches(saved, local.looked_ns, stamp) {
                    return Some(Why::Local(under(&local.at, name)));
                }
            }
        }
        for outside in &slot.outside {
            let stamp = now.next()?;
            // A file's content is read through every link: an `lstat` of the link vouches
            // for where it points, not for what is there.
            let through = matches!(outside.read, Read::Outside(_)) && outside.stamp.is_link();
            if through || !vouches(&outside.stamp, outside.looked_ns, stamp) {
                return Some(Why::Outside(outside.read.parts().1.to_owned())); // alloc: a reason's name, once
            }
        }
        None
    }

    /// The digest of `read` as the slot holds it, where the first look vouched for it.
    fn value(&self, read: &Read) -> Result<Digest, Why> {
        match read {
            Read::Node(path) if path.is_empty() => Ok(Digest::Is(self.slot.column.root)),
            Read::Node(path) => Err(Why::Column(path.clone())), // clone: a reason's name, once
            Read::Env(name) => Ok(env_digest(&self.env, name)),
            Read::Drawn => Ok(Digest::Is(self.slot.drawn)),
            Read::Outside(path) | Read::Listing(path) | Read::Entry(path) => {
                match self.slot.outside.iter().find(|it| it.read == *read) {
                    Some(it) => Ok(it.digest),
                    None => Err(Why::Outside(path.clone())), // clone: as above
                }
            }
            Read::Tree(path) | Read::Package(path) => Err(Why::Outside(path.clone())), // clone: as above
        }
    }

    fn done(&mut self, verdict: Verdict) -> Step {
        self.phase = Phase::Done;
        Step::Done(verdict)
    }

    fn stopped(&mut self, what: impl fmt::Display) -> Step {
        self.done(Verdict::Daemon(Why::Stopped(what.to_string())))
    }

    fn step<E: fmt::Display, S: Store<Error = Refusal<E>>>(&mut self, store: &S, seen: Option<Seen>) -> Step {
        match std::mem::replace(&mut self.phase, Phase::Done) {
            Phase::Opened => {
                if seen.is_some() {
                    return self.stopped("told of a look before it looked");
                }
                self.phase = Phase::First;
                Step::Look(Look::Stamps(self.first()))
            }
            Phase::First => {
                let Some(Seen::Stamps(now)) = seen else {
                    return self.stopped("told another thing than the first look's stamps");
                };
                let wanted = self.first_len();
                if now.len() != wanted {
                    return self.stopped(format_args!("{} stamps came back for {wanted} paths", now.len()));
                }
                if let Some(why) = self.first_moved(&now) {
                    return self.done(Verdict::Daemon(why));
                }
                self.phase = Phase::Machine { handed: Handed::Nothing, open: Vec::new() }; // alloc: empty
                self.drive(store)
            }
            Phase::Machine { handed, open } => {
                if let Err(what) = self.tell(handed, seen) {
                    return self.stopped(what);
                }
                self.phase = Phase::Machine { handed: Handed::Nothing, open };
                self.drive(store)
            }
            Phase::Done => self.stopped("stepped past its end"),
        }
    }

    /// Tells the machine what was seen of the look handed out.
    fn tell(&mut self, handed: Handed, seen: Option<Seen>) -> Result<(), String> {
        match (handed, seen) {
            (Handed::Nothing, None) => Ok(()),
            (Handed::Stamps(asks), Some(Seen::Stamps(stamps))) if asks.len() == stamps.len() => {
                for (ask, stamp) in asks.iter().zip(stamps) {
                    // As the daemon's driver tells it (certify.rs): no regular file is none.
                    let stamp = (!stamp.is_gone() && !stamp.is_unknown() && stamp.is_file()).then_some(stamp);
                    // No clock: an output's stamp vouches only through the slot's evidence,
                    // and one it does not vouch for is hashed by the daemon (`Why::Hashes`).
                    self.machine.told(ask, Told::Stamp { stamp, looked_ns: 0 }).map_err(|it| it.to_string())?;
                }
                Ok(())
            }
            (Handed::Head(ask), Some(Seen::Head(head))) => {
                let content = head
                    .filter(|it| it.len() >= START_HEAD && it[..8] == START_MAGIC[..])
                    .map(|it| {
                        let mut record = [0; 32];
                        record.copy_from_slice(&it[72..START_HEAD]);
                        ContentId(record)
                    });
                self.machine.told(&ask, Told::Content(content)).map_err(|it| it.to_string())
            }
            // alloc: an error, once
            _ => Err("told another thing than the look it handed out".to_owned()),
        }
    }

    /// Steps the machine until it asks for a look or ends.
    fn drive<E: fmt::Display, S: Store<Error = Refusal<E>>>(&mut self, store: &S) -> Step {
        loop {
            let Phase::Machine { open, .. } = &mut self.phase else {
                return self.stopped("drove the machine out of turn");
            };
            // The stamps open first, in one look; then each start file's head, one a look.
            let stamps: Vec<Ask> = open.extract_if(.., |ask| matches!(ask, Ask::Stamp { .. })).collect(); // alloc: one look's asks
            let handed = match (stamps.is_empty(), open.pop()) {
                (false, head) => {
                    open.extend(head);
                    let paths = stamps
                        .iter()
                        .map(|ask| match ask {
                            Ask::Stamp { path, .. } => path.clone(), // clone: the look's own path
                            _ => unreachable!("only stamps were taken"),
                        })
                        .collect(); // alloc: as above
                    Some((Handed::Stamps(stamps), Look::Stamps(paths)))
                }
                (true, Some(ask @ Ask::Content { .. })) => {
                    let Ask::Content { path, .. } = &ask else { unreachable!("matched as content") };
                    let look = Look::Head { path: start_file(path), len: START_HEAD };
                    Some((Handed::Head(ask), look))
                }
                (true, Some(_)) => return self.stopped("a read was left open"),
                (true, None) => None,
            };
            if let Some((handed, look)) = handed {
                let Phase::Machine { open, .. } = std::mem::replace(&mut self.phase, Phase::Done) else {
                    return self.stopped("drove the machine out of turn");
                };
                self.phase = Phase::Machine { handed, open };
                return Step::Look(look);
            }
            self.rounds += 1;
            if self.rounds > MOST_ROUNDS {
                return self.stopped(format_args!("the machine took more than {MOST_ROUNDS} steps"));
            }
            match self.machine.step(store, &mut self.evidence) {
                Ok(Next::Asks(asks)) => {
                    if let Err(why) = self.asked(asks) {
                        return self.done(Verdict::Daemon(why));
                    }
                }
                Ok(Next::Hit(answer)) => return self.done(Verdict::Hit(answer)),
                Ok(Next::Miss(miss)) => return self.done(Verdict::Daemon(Why::Miss(miss.why.to_string()))),
                Ok(Next::Waiting { on, left }) => return self.stopped(format_args!("{on}: {left} left untold")),
                Ok(Next::Written { .. } | Next::Unwritten(_)) => return self.stopped("the machine wrote, with no build"),
                Err(Stopped::Record(blaze_record::Error::Store { key, error: Refusal::Unheld, .. })) => {
                    return self.done(Verdict::Daemon(Why::Record(key)));
                }
                Err(stopped) => return self.stopped(stopped),
            }
        }
    }

    /// Answers each read the machine asks for, and keeps each look it asks for open.
    fn asked(&mut self, asks: Vec<Ask>) -> Result<(), Why> {
        let mut looks = Vec::new(); // alloc: the asks a look answers, once a state
        for ask in asks {
            let look = match &ask {
                Ask::Read { read, .. } => {
                    let digest = self.value(read)?;
                    self.machine.told(&ask, Told::Read(digest)).map_err(|it| Why::Stopped(it.to_string()))?;
                    false
                }
                Ask::Stamp { base, path } if *base == ABSOLUTE && absolute(path) => true,
                Ask::Stamp { base, path } | Ask::Content { base, path }
                    if certificate::holder_of(*base).is_none() && (*base != ABSOLUTE || !absolute(path)) =>
                {
                    return Err(Why::Stopped(format!("the output {path} under base {base} is no file a client can look at")));
                }
                Ask::Content { base: LAUNCHER, path } if absolute(path) => true,
                Ask::Content { base: MEMORY, path } => return Err(Why::Memory(path.clone())), // clone: a reason's name
                Ask::Content { path, .. } | Ask::Stamp { path, .. } => return Err(Why::Hashes(path.clone())), // clone: as above
            };
            if look {
                looks.push(ask);
            }
        }
        let Phase::Machine { open, .. } = &mut self.phase else {
            return Err(Why::Stopped("asked out of turn".to_owned())); // alloc: an error, once
        };
        open.extend(looks);
        Ok(())
    }
}

/// A client's check of one request over its slot.
pub struct Solo {
    core: Core,
    store: Records,
    /// Every get the machine made, in order.
    records: Vec<(ContentId, Option<Vec<u8>>)>,
}

impl Solo {
    /// The check of the request of key `key` (`slot::key_of`) over the slot `bytes`, for a
    /// request whose environment is `env`, by the engine at `engine`. `daemon`: whether one
    /// holds `serve.lock`. `Err`: the slot is not this request's to be checked by.
    pub fn open(bytes: &[u8], key: &ContentId, env: &[(String, String)], engine: &str, daemon: bool) -> Result<Solo, Why> {
        let mut slot = Slot::decode(bytes, key).map_err(Why::Layout)?;
        if slot.traced {
            return Err(Why::Traced);
        }
        let written_by = slot.premises.first().map_or("", |it| it.path.as_str());
        if written_by != engine {
            // alloc: a reason's names, once
            return Err(Why::Engine { slot: written_by.to_owned(), here: engine.to_owned() });
        }
        if !planted(Plant::SkipsNames) && names_of(env) != slot.names {
            return Err(Why::Names);
        }
        if daemon && slot.column.names.len() > SWEEP_MOST {
            return Err(Why::Sweep { names: slot.column.names.len() });
        }
        let held = std::mem::take(&mut slot.records);
        let store = Records { held, gets: RefCell::new(Vec::new()), tally: Tally::new() }; // alloc: empty
        Ok(Solo { core: Core::new(slot, env), store, records: Vec::new() }) // alloc: empty
    }

    /// The next thing to do: `seen` is what the driver saw of the last [`Step::Look`], `None`
    /// at the first step.
    pub fn step(&mut self, seen: Option<Seen>) -> Step {
        let step = self.core.step(&self.store, seen);
        self.records.append(&mut self.store.gets.borrow_mut());
        step
    }

    /// Every get the machine made of the slot's records, in order.
    pub fn records(&self) -> &[(ContentId, Option<Vec<u8>>)] {
        &self.records
    }
}

/// The check a client would make over `slot` (its records aside) with `store` in place of
/// its records, while nothing has moved: every look answered from the slot's own stamps (an
/// output's from its placed stamp, `Stamp::UNKNOWN` for one the slot does not hold) and a
/// launcher's code from the certificate's own output. `Ok(gets)`, only on a hit: every get
/// the machine made, with what `store` held, which are the slot's records. `Err`: why a
/// client would not hit on this slot, so it is not written.
pub fn rehearse<S: Store>(slot: &Slot, store: &S, env: &[(String, String)]) -> Result<Vec<(ContentId, Option<Vec<u8>>)>, Why>
where
    S::Error: fmt::Display,
{
    if slot.traced {
        return Err(Why::Traced);
    }
    if names_of(env) != slot.names {
        return Err(Why::Names);
    }
    // clone: the check's own slot, once a rehearsal (its records are what is made)
    let mut slot = slot.clone();
    slot.records = Vec::new(); // alloc: empty
    let saved = first_saved(&slot);
    let logged = Logged { inner: store, gets: RefCell::new(Vec::new()) }; // alloc: empty
    let mut core = Core::new(slot, env);
    let mut seen = None;
    loop {
        match core.step(&logged, seen.take()) {
            Step::Look(Look::Stamps(paths)) if matches!(core.phase, Phase::First) => {
                if paths.len() != saved.len() {
                    return Err(Why::Stopped("the first look is not the slot's".to_owned())); // alloc: an error, once
                }
                seen = Some(Seen::Stamps(saved.clone())); // clone: the look's answer, once
            }
            Step::Look(Look::Stamps(paths)) => {
                let stamps = paths
                    .iter()
                    .map(|path| {
                        core.slot.placed.iter().find(|it| it.path == *path).map_or(Stamp::UNKNOWN, |it| it.stamp)
                    })
                    .collect(); // alloc: the look's answer
                seen = Some(Seen::Stamps(stamps));
            }
            Step::Look(Look::Head { .. }) => {
                let Phase::Machine { handed: Handed::Head(Ask::Content { base, path }), .. } = &core.phase else {
                    return Err(Why::Stopped("a head was asked for with no code open".to_owned())); // alloc: as above
                };
                let head = core.machine.wanted(*base, path).map(|record| {
                    let mut head = Vec::with_capacity(START_HEAD); // alloc: the head, once a launcher
                    head.extend_from_slice(START_MAGIC);
                    head.resize(72, 0);
                    head.extend_from_slice(&record.0);
                    head
                });
                seen = Some(Seen::Head(head));
            }
            Step::Done(Verdict::Hit(_)) => return Ok(logged.gets.into_inner()),
            Step::Done(Verdict::Daemon(why)) => return Err(why),
        }
    }
}

/// The slot's own stamps of its first look, in the look's order.
fn first_saved(slot: &Slot) -> Vec<Stamp> {
    let mut saved: Vec<Stamp> = slot.premises.iter().map(|it| it.stamp).collect(); // alloc: once a rehearsal
    saved.extend(slot.column.names.iter().map(|(_, stamp)| *stamp));
    for local in &slot.locals {
        saved.extend(local.names.iter().map(|(_, stamp)| *stamp));
    }
    saved.extend(slot.outside.iter().map(|it| it.stamp));
    saved
}

#[cfg(test)]
mod tests;
