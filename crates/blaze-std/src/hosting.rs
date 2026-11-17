//! The host's side of the hosted world (docs/v2/plan/design/run.md, "The hosted world"):
//! [`Hosting`] takes a world (any driver behind an [`Io`]: the process's real one, a
//! simulated one) and gives the [`HostTable`] a hosted program is started with. Every entry
//! answers from that world, so whatever holds the world (a record, a replay, a fake) holds
//! the program's run with no change to the program.
//!
//! The program is not trusted (the review rule "Trust"): each entry checks every range
//! against the exchange it made and every handle against those it knows before it reads a
//! byte, and answers anything else with a typed refusal ([`wire::code`]). An entry never
//! panics across the C boundary.

use alloc::{boxed::Box, string::String, vec::Vec};
use core::{ffi::c_void, time::Duration};

use blaze_core::{lock::Lock, wait::Deadline};
use blaze_own::SharedAlloc;

use crate::{
    io::Io,
    layers::{Decide, Fakes, Gate, Pass, Withheld},
    out::Stream,
    wire::{self, Answer, HostTable, code},
};

/// What answers a hosted run's calls (run.md, "How a run is held"): a driver the host wraps
/// around its world, with no change to the program.
pub enum Hold {
    /// The world answers: `blaze run`.
    Real,
    /// The world answers, and every call and its answer is written down ([`Hosting::record`]):
    /// `blaze run --record FILE`.
    Record,
    /// The record answers every call; the world is asked nothing. The first call that is not
    /// the record's next stops the replay, named ([`Hosting::diverged`]): `blaze replay FILE`.
    Replay(Record),
}

/// How a hosted run is held, whole: record or replay ([`Hold`]), the `dry` and `ask` layers
/// ([`crate::layers::Gate`]) and the stand-in providers ([`Fakes`]). Layers stack as run.md
/// says: record outside, then dry and ask, then the provider (a fake's or the world). A replay
/// answers from its record alone: no layer and no provider is asked.
pub struct Holding {
    pub hold: Hold,
    /// `--dry`: every changing effect withheld.
    pub dry: bool,
    /// `--ask`: what decides each changing effect (the CLI's terminal; a test's script).
    pub ask: Option<Box<dyn Decide + Send>>,
    /// `--fake name=provider`.
    pub fakes: Fakes,
}

impl Holding {
    /// `hold` with no layer and no fake.
    pub fn of(hold: Hold) -> Holding {
        Holding { hold, dry: false, ask: None, fakes: Fakes::default() }
    }

    /// The holding `spec` asks for: its provider and its layers (a hosted run is held as one;
    /// a grant's own provider or layers are refused until the wire carries grants' effects
    /// apart). `record` is the record a `replay` is answered from, read by the host; `ask` the
    /// decider an `ask` layer needs; `fakes` the stand-ins a `fake` provider names.
    pub fn of_spec<A: core::alloc::Allocator>(spec: &crate::spec::RunSpec<A>, record: Option<Record>, ask: Option<Box<dyn Decide + Send>>, fakes: Fakes) -> Result<Holding, HoldError> {
        use crate::spec::{Layer, Provider};
        for granted in &spec.grants {
            if granted.provider.is_some_and(|provider| provider != spec.provider) {
                return Err(HoldError::NotBuilt("a grant's own provider"));
            }
            if granted.layers.is_some_and(|layers| layers != spec.layers) {
                return Err(HoldError::NotBuilt("a grant's own layers"));
            }
        }
        let faked = fakes.clock.is_some() || fakes.files.is_some() || fakes.recorded.is_some();
        let hold = match (spec.provider, spec.layers.has(Layer::Record)) {
            (Provider::Replay, _) => Hold::Replay(record.ok_or(HoldError::NoRecord)?),
            (Provider::Fake, _) if !faked => return Err(HoldError::NoFake),
            (Provider::Real, _) if faked => return Err(HoldError::FakeNotAsked),
            (Provider::Real | Provider::Fake, true) => Hold::Record,
            (Provider::Real | Provider::Fake, false) => Hold::Real,
        };
        if matches!(hold, Hold::Replay(_)) && (spec.layers.has(Layer::Overlay) || spec.layers.has(Layer::Ask) || faked) {
            return Err(HoldError::ReplayHasNoLayers);
        }
        let ask = match (spec.layers.has(Layer::Ask), ask) {
            (true, Some(ask)) => Some(ask),
            (true, None) => return Err(HoldError::NoDecider),
            (false, _) => None,
        };
        Ok(Holding { hold, dry: spec.layers.has(Layer::Overlay), ask, fakes })
    }
}

/// Why a run spec's holding cannot be made for a hosted run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HoldError {
    /// The spec's provider is `replay`, and the host was handed no record.
    NoRecord,
    /// What the hosted host does not build yet: a grant's own provider or layers.
    NotBuilt(&'static str),
    /// The spec's provider is `fake`, and no stand-in was named (`--fake name=provider`).
    NoFake,
    /// Stand-ins were handed in, and the spec's provider is `real`.
    FakeNotAsked,
    /// The spec has an `ask` layer, and the host was handed nothing to decide.
    NoDecider,
    /// A replay with a layer or a fake: a replay answers from its record alone.
    ReplayHasNoLayers,
}

impl core::fmt::Display for HoldError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            HoldError::NoRecord => f.write_str("bstd hosting: the run spec's provider is `replay`, and no record was handed to the host"),
            HoldError::NotBuilt(what) => write!(f, "bstd hosting: {what} is not built for a hosted run yet"),
            HoldError::NoFake => f.write_str("bstd hosting: the run spec's provider is `fake`, and no stand-in was named (`--fake name=provider`)"),
            HoldError::FakeNotAsked => f.write_str("bstd hosting: stand-ins were handed to the host, and the run spec's provider is `real`"),
            HoldError::NoDecider => f.write_str("bstd hosting: the run spec has the `ask` layer, and the host was handed nothing to ask"),
            HoldError::ReplayHasNoLayers => {
                f.write_str("bstd hosting: a replay answers from its record alone: it takes no `--dry`, `--ask` or `--fake`")
            }
        }
    }
}

impl core::error::Error for HoldError {}

/// One call a program made through the table, as its host took it: after the host's checks
/// (a call refused for its range or handle never reaches the world, and is not held), with
/// the bytes it named copied out of the exchange.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Call {
    Write { stream: u32, bytes: Vec<u8> },
    Clock { which: u32 },
    Arg { index: u64 },
    Grant { kind: Vec<u8>, name: Vec<u8> },
    Effect { bytes: Vec<u8>, budget_ns: u64 },
    Exit { status: i32 },
}

impl Call {
    /// The table's entry it was made through.
    pub fn entry(&self) -> &'static str {
        match self {
            Call::Write { .. } => "write",
            Call::Clock { .. } => "clock",
            Call::Arg { .. } => "arg",
            Call::Grant { .. } => "grant",
            Call::Effect { .. } => "effect",
            Call::Exit { .. } => "exit",
        }
    }

    /// Whether its answer's value counts bytes the host wrote at the exchange's start.
    fn answers_bytes(&self) -> bool {
        matches!(self, Call::Arg { .. } | Call::Effect { .. })
    }
}

/// A call and its answer, as held: the answer's code and value, and the bytes the host put
/// in the exchange with it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Called {
    pub call: Call,
    pub code: u32,
    pub value: u64,
    pub back: Vec<u8>,
}

/// A hosted run's calls in order: what `--record` writes and `replay` answers from. Its file
/// is strict and versioned ([`Record::write`], [`Record::read`]).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Record {
    pub calls: Vec<Called>,
}

/// A record's bytes are not one of this version's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RecordError {
    /// The byte offset where reading stopped.
    pub at: usize,
    pub why: &'static str,
}

impl core::fmt::Display for RecordError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "bstd record: at byte {}: {}", self.at, self.why)
    }
}

impl core::error::Error for RecordError {}

impl From<blaze_cbor::Error> for RecordError {
    fn from(error: blaze_cbor::Error) -> RecordError {
        RecordError { at: error.at, why: error.why }
    }
}

impl Record {
    /// The text a record starts with.
    pub const MAGIC: &str = "bstd.record";
    /// This format's version: a record of another is refused, never read across.
    pub const VERSION: u64 = 1;
    /// The most calls a record holds.
    pub const CALLS_MOST: usize = 1 << 24;
    /// The most bytes one call names or is answered with.
    pub const BYTES_MOST: usize = 1 << 24;

    /// The record as deterministic CBOR: `[MAGIC, VERSION, [call..]]`, each call
    /// `[entry, fields.., code, value, back]` with its entry's index first; made in `alloc`,
    /// the caller's (D102).
    pub fn write<A: core::alloc::Allocator>(&self, alloc: A) -> blaze_own::Vec<u8, A> {
        let mut out = blaze_own::Vec::new_in(alloc);
        blaze_cbor::array(&mut out, 3);
        blaze_cbor::text(&mut out, Record::MAGIC);
        blaze_cbor::uint(&mut out, Record::VERSION);
        blaze_cbor::array(&mut out, self.calls.len() as u64);
        for called in &self.calls {
            let fields: u64 = match called.call {
                Call::Grant { .. } | Call::Write { .. } | Call::Effect { .. } => 2,
                Call::Clock { .. } | Call::Arg { .. } | Call::Exit { .. } => 1,
            };
            blaze_cbor::array(&mut out, 1 + fields + 3);
            match &called.call {
                Call::Write { stream, bytes } => {
                    blaze_cbor::uint(&mut out, 0);
                    blaze_cbor::uint(&mut out, u64::from(*stream));
                    blaze_cbor::bytes(&mut out, bytes);
                }
                Call::Clock { which } => {
                    blaze_cbor::uint(&mut out, 1);
                    blaze_cbor::uint(&mut out, u64::from(*which));
                }
                Call::Arg { index } => {
                    blaze_cbor::uint(&mut out, 2);
                    blaze_cbor::uint(&mut out, *index);
                }
                Call::Grant { kind, name } => {
                    blaze_cbor::uint(&mut out, 3);
                    blaze_cbor::bytes(&mut out, kind);
                    blaze_cbor::bytes(&mut out, name);
                }
                Call::Effect { bytes, budget_ns } => {
                    blaze_cbor::uint(&mut out, 4);
                    blaze_cbor::bytes(&mut out, bytes);
                    blaze_cbor::uint(&mut out, *budget_ns);
                }
                Call::Exit { status } => {
                    blaze_cbor::uint(&mut out, 5);
                    // The status's bits, as the wire carries them.
                    blaze_cbor::uint(&mut out, u64::from(status.cast_unsigned()));
                }
            }
            blaze_cbor::uint(&mut out, u64::from(called.code));
            blaze_cbor::uint(&mut out, called.value);
            blaze_cbor::bytes(&mut out, &called.back);
        }
        out
    }

    /// The record `bytes` hold: every length bounded, the magic, the version and each call's
    /// shape checked, nothing after the record.
    pub fn read(bytes: &[u8]) -> Result<Record, RecordError> {
        let mut reader = blaze_cbor::Reader::new(bytes);
        reader.array_of(3)?;
        let at = reader.at();
        if reader.text(Record::MAGIC.len(), "a record's magic")? != Record::MAGIC {
            return Err(RecordError { at, why: "not a bstd record (its magic is not `bstd.record`)" });
        }
        let at = reader.at();
        if reader.uint()? != Record::VERSION {
            return Err(RecordError { at, why: "a record of another version than this bstd reads (1)" });
        }
        let len = reader.array(Record::CALLS_MOST, "a record's calls")?;
        let mut calls = Vec::new();
        for _ in 0..len {
            let at = reader.at();
            let shape = reader.array(6, "a recorded call")?;
            let entry = reader.uint_most(5, "a recorded call's entry")?;
            let fields = match entry {
                0 | 3 | 4 => 2,
                _ => 1,
            };
            if shape != 1 + fields + 3 {
                return Err(RecordError { at, why: "a recorded call with the wrong number of fields for its entry" });
            }
            let call = match entry {
                0 => {
                    let stream = reader.uint_most(u64::from(u32::MAX), "a stream handle")? as u32;
                    Call::Write { stream, bytes: reader.bytes(Record::BYTES_MOST, "written bytes")?.to_vec() }
                }
                1 => Call::Clock { which: reader.uint_most(u64::from(u32::MAX), "a clock")? as u32 },
                2 => Call::Arg { index: reader.uint()? },
                3 => {
                    let kind = reader.bytes(Record::BYTES_MOST, "a grant's kind")?.to_vec();
                    Call::Grant { kind, name: reader.bytes(Record::BYTES_MOST, "a grant's name")?.to_vec() }
                }
                4 => {
                    let bytes = reader.bytes(Record::BYTES_MOST, "an effect's bytes")?.to_vec();
                    Call::Effect { bytes, budget_ns: reader.uint()? }
                }
                _ => Call::Exit { status: (reader.uint_most(u64::from(u32::MAX), "an exit status")? as u32).cast_signed() },
            };
            let code = reader.uint_most(u64::from(u32::MAX), "an answer's code")? as u32;
            let value = reader.uint()?;
            let back = reader.bytes(Record::BYTES_MOST, "an answer's bytes")?.to_vec();
            if call.answers_bytes() && code == code::OK && value != back.len() as u64 {
                return Err(RecordError { at, why: "an answer whose value is not the length of its bytes" });
            }
            calls.push(Called { call, code, value, back });
        }
        reader.end()?;
        Ok(Record { calls })
    }
}

/// Where a replay stopped: the first call that was not the record's next.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Diverged {
    /// Its place in the run: the record's calls before it were made the same.
    pub at: usize,
    /// What the record holds there; `None` past its end.
    pub recorded: Option<Call>,
    /// What the program made.
    pub made: Call,
}

impl core::fmt::Display for Diverged {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.recorded {
            Some(recorded) if recorded.entry() == self.made.entry() => write!(
                f,
                "bstd replay: call {} differs: the record's `{}` was {:?}, the program's is {:?}",
                self.at,
                recorded.entry(),
                recorded,
                self.made
            ),
            Some(recorded) => write!(
                f,
                "bstd replay: call {} differs: the record has a `{}`, the program made a `{}` ({:?})",
                self.at,
                recorded.entry(),
                self.made.entry(),
                self.made
            ),
            None => write!(f, "bstd replay: call {} is past the record's end: the program made a `{}` ({:?})", self.at, self.made.entry(), self.made),
        }
    }
}

/// The host's holding, as it goes.
struct Held {
    hold: Hold,
    /// What `Hold::Record` wrote down.
    kept: Record,
    /// The replay's next call.
    next: usize,
    diverged: Option<Diverged>,
    /// How many calls the program made: the next call's place.
    made: usize,
    /// A test's control: the replay answers the next record without comparing the call.
    planted_unchecked: bool,
    /// A test's control: the recorder does not write down an answer's bytes.
    planted_drops_back: bool,
    /// A test's control: the replay also asks the world.
    planted_reaches_host: bool,
    /// A test's control: an effect answered `No` is performed, and refused all the same.
    planted_no_reaches_host: bool,
}

/// A hosted program's host: the world it answers from, the command line and grants it
/// gives, the exchange it lends, and the status the program ended with.
pub struct Hosting {
    held: Lock<Held>,
    /// The `dry` and `ask` layers.
    gate: Lock<Gate>,
    /// The stand-in providers.
    fakes: Lock<Fakes>,
    io: Io,
    args: Vec<String>,
    /// `(kind, name)` of each parameter the run grants.
    grants: Vec<(String, String)>,
    /// The exchange: owned here, lent to the program by pointer for the run. A `Box` of
    /// cells, because the program writes it through the table's pointer while this holds it.
    exchange: Box<[core::cell::UnsafeCell<u8>]>,
    exited: Lock<Option<i32>>,
    /// Where the answers' encodings are made: the host's allocator, handed in by its caller
    /// (D102).
    alloc: SharedAlloc<'static>,
    /// A planted bug, only a test's control sets: `write` takes a stream handle it does not
    /// know for standard output.
    planted_stream_unchecked: bool,
}

impl Hosting {
    /// A host over `io` giving `args` and `grants`, with an exchange of `exchange_len` bytes;
    /// its answers made in `alloc`.
    pub fn new(io: Io, args: Vec<String>, grants: Vec<(String, String)>, exchange_len: usize, alloc: SharedAlloc<'static>) -> Box<Hosting> {
        Hosting::holding(io, args, grants, exchange_len, Holding::of(Hold::Real), alloc)
    }

    /// [`Hosting::new`], its calls answered as `holding` says.
    pub fn holding(
        io: Io,
        args: Vec<String>,
        grants: Vec<(String, String)>,
        exchange_len: usize,
        holding: Holding,
        alloc: SharedAlloc<'static>,
    ) -> Box<Hosting> {
        let Holding { hold, dry, ask, fakes } = holding;
        // alloc: once per run, the exchange.
        let exchange = (0..exchange_len).map(|_| core::cell::UnsafeCell::new(0)).collect();
        let held = Held {
            hold,
            kept: Record::default(),
            next: 0,
            diverged: None,
            made: 0,
            planted_unchecked: false,
            planted_drops_back: false,
            planted_reaches_host: false,
            planted_no_reaches_host: false,
        };
        // alloc: once per run, the host.
        Box::new(Hosting {
            held: Lock::new("bstd.hosting.held", held),
            gate: Lock::new("bstd.hosting.gate", Gate::new(dry, ask)),
            fakes: Lock::new("bstd.hosting.fakes", fakes),
            io,
            args,
            grants,
            exchange,
            exited: Lock::new("bstd.hosting.exited", None),
            alloc,
            planted_stream_unchecked: false,
        })
    }

    /// This host with its planted bug: a test's control.
    #[cfg(test)]
    pub(crate) fn plant_stream_unchecked(mut self: Box<Hosting>) -> Box<Hosting> {
        self.planted_stream_unchecked = true;
        self
    }

    /// The table a program is started with. It points into `self`, which must outlive every
    /// call the program makes through it.
    pub fn table(&self) -> HostTable {
        HostTable {
            version: wire::VERSION,
            size: size_of::<HostTable>() as u32,
            host: core::ptr::from_ref(self).cast_mut().cast::<c_void>(),
            exchange: core::cell::UnsafeCell::raw_get(self.exchange.as_ptr()),
            exchange_len: self.exchange.len(),
            write,
            clock,
            arg,
            grant,
            effect,
            exit,
        }
    }

    /// This host with its replay's planted bug: a test's control.
    #[cfg(test)]
    pub(crate) fn plant_replay_unchecked(self: Box<Hosting>) -> Box<Hosting> {
        self.held.lock().planted_unchecked = true;
        self
    }

    /// This host with its recorder's planted bug: the bytes an answer put in the exchange are
    /// not written down. A test's control.
    #[cfg(test)]
    pub(crate) fn plant_record_drops_back(self: Box<Hosting>) -> Box<Hosting> {
        self.held.lock().planted_drops_back = true;
        self
    }

    /// This host with its replay's other planted bug: the replay also asks the world. A
    /// test's control.
    #[cfg(test)]
    pub(crate) fn plant_replay_reaches_host(self: Box<Hosting>) -> Box<Hosting> {
        self.held.lock().planted_reaches_host = true;
        self
    }

    /// This host with its dry layer's planted bug: the first changing effect is performed.
    /// A test's control.
    #[cfg(test)]
    pub(crate) fn plant_dry_lets_through(self: Box<Hosting>) -> Box<Hosting> {
        self.gate.lock().plant_lets_through();
        self
    }

    /// This host with its ask layer's planted bug: the first changing effect is performed
    /// unasked. A test's control.
    #[cfg(test)]
    pub(crate) fn plant_ask_skips(self: Box<Hosting>) -> Box<Hosting> {
        self.gate.lock().plant_unasked();
        self
    }

    /// This host with the ask layer's planted bug: an effect answered `No` reaches the world.
    /// A test's control.
    #[cfg(test)]
    pub(crate) fn plant_no_reaches_host(self: Box<Hosting>) -> Box<Hosting> {
        self.held.lock().planted_no_reaches_host = true;
        self
    }

    /// This host with the ask layer's other planted bug: an effect answered `No` is answered
    /// as performed. A test's control.
    #[cfg(test)]
    pub(crate) fn plant_no_says_ok(self: Box<Hosting>) -> Box<Hosting> {
        self.gate.lock().plant_no_says_ok();
        self
    }

    /// Every changing effect not performed so far (`--dry`; `--ask` answered `No` or
    /// `Pretend`), in order.
    pub fn withheld(&self) -> Vec<Withheld> {
        // clone: a report.
        self.gate.lock().withheld().to_vec()
    }

    /// What the run prints at its end under `--dry` or `--ask`: its first line says how many
    /// later reads may differ from a real run, when any may, then each effect not performed.
    pub fn report(&self) -> crate::layers::Report {
        self.gate.lock().report()
    }

    /// The fake folder as the run left it, if the run had one.
    pub fn folder(&self) -> Option<crate::layers::Folder> {
        // clone: a report.
        self.fakes.lock().files.clone()
    }

    /// The exchange's first `len` bytes: a test's view of what an answer put there.
    #[cfg(test)]
    pub(crate) fn head(&self, len: usize) -> Vec<u8> {
        self.range(0, len).map(<[u8]>::to_vec).unwrap_or_default()
    }

    /// How many of a replay's recorded calls the program has not made yet: a replay is whole
    /// when the run ends with none left and nothing [`Hosting::diverged`].
    pub fn unreplayed(&self) -> usize {
        let held = self.held.lock();
        match &held.hold {
            Hold::Replay(record) => record.calls.len() - held.next,
            Hold::Real | Hold::Record => 0,
        }
    }

    /// The status the program exited with through the table's `exit`, if it did.
    pub fn exited(&self) -> Option<i32> {
        *self.exited.lock()
    }

    /// What a `Hold::Record` host wrote down so far: every call and its answer, in order.
    pub fn record(&self) -> Record {
        // clone: the record handed out; the host keeps writing its own.
        self.held.lock().kept.clone()
    }

    /// Where a replay stopped, if it did.
    pub fn diverged(&self) -> Option<Diverged> {
        // clone: a report.
        self.held.lock().diverged.clone()
    }

    /// `call`'s answer as the holding says: the world's (`real`), written down under
    /// `Record`, or the record's next under `Replay`. A replay that meets a call the record
    /// does not have next stops there: that call and every later one is refused
    /// ([`code::FAILED`]), and [`Hosting::diverged`] names it.
    /// `effect` is the effect an escaped call makes, as its entry read it.
    fn through(&self, call: Call, effect: Option<&blaze_core::Effect>, real: impl FnOnce() -> Answer) -> Answer {
        let mut held = self.held.lock();
        let at = held.made;
        held.made += 1;
        let Held { hold, next, diverged, planted_unchecked, planted_drops_back, planted_reaches_host, .. } = &mut *held;
        match hold {
            Hold::Real => {
                drop(held);
                self.layered(at, &call, effect, real)
            }
            Hold::Record => {
                let drops_back = *planted_drops_back;
                drop(held);
                let answer = self.layered(at, &call, effect, real);
                let back = match (call.answers_bytes() && !drops_back, answer.code) {
                    (true, code::OK) => self.range(0, answer.value as usize).map(<[u8]>::to_vec).unwrap_or_default(),
                    _ => Vec::new(),
                };
                self.held.lock().kept.calls.push(Called { call, code: answer.code, value: answer.value, back });
                answer
            }
            Hold::Replay(record) => {
                if diverged.is_some() {
                    return Answer::refused(code::FAILED);
                }
                match record.calls.get(*next) {
                    Some(recorded) if recorded.call == call || *planted_unchecked => {
                        *next += 1;
                        let answer = Answer { code: recorded.code, value: recorded.value };
                        // clone: the recorded bytes, put in the exchange after the lock goes.
                        let back = recorded.back.clone();
                        let reaches_host = *planted_reaches_host;
                        drop(held);
                        if reaches_host {
                            let _ = real();
                        }
                        if call.answers_bytes() && answer.code == code::OK {
                            return self.answer_with(&back);
                        }
                        answer
                    }
                    recorded => {
                        // clone: the report's copy of the record's call.
                        *diverged = Some(Diverged { at: *next, recorded: recorded.map(|it| it.call.clone()), made: call });
                        Answer::refused(code::FAILED)
                    }
                }
            }
        }
    }

    /// `call`, the `at`th, through the layers inside the record: `dry` and `ask` first (a
    /// changing effect withheld is answered as performed and never reaches the provider), then
    /// the provider: a fake standing in for the call's capability, else the world (`real`).
    fn layered(&self, at: usize, call: &Call, effect: Option<&blaze_core::Effect>, real: impl FnOnce() -> Answer) -> Answer {
        if let Some(effect) = effect
            && let mut gate = self.gate.lock()
            && !gate.is_open()
        {
            if !crate::layers::changes(effect) {
                gate.read(effect);
            } else {
                let pass = gate.pass(at, effect);
                drop(gate);
                match pass {
                    Pass::Perform => {}
                    Pass::Pretend => return self.answer_done(&crate::layers::performed(effect)),
                    Pass::Refuse => {
                        if self.held.lock().planted_no_reaches_host {
                            let _ = self.faked(call, Some(effect)).unwrap_or_else(real);
                        }
                        return Answer::refused(code::REFUSED);
                    }
                }
            }
        }
        match self.faked(call, effect) {
            Some(answer) => answer,
            None => real(),
        }
    }

    /// `call` answered by the stand-in for its capability; `None` when it has none.
    fn faked(&self, call: &Call, effect: Option<&blaze_core::Effect>) -> Option<Answer> {
        let mut fakes = self.fakes.lock();
        if let Call::Clock { .. } = call {
            return fakes.clock.map(Answer::ok);
        }
        let effect = effect?;
        if fakes.clock.is_some() && matches!(effect, blaze_core::Effect::Timer { .. }) {
            // A fixed clock: no time passes, so a timer fires at once.
            return Some(self.answer_done(&blaze_core::Done::Fired));
        }
        if let Some(folder) = &mut fakes.files
            && let Some(done) = folder.answer(effect)
        {
            return Some(self.answer_done(&done));
        }
        if let (Some(recorded), Call::Effect { bytes, .. }) = (&mut fakes.recorded, call)
            && recorded.capability() == crate::layers::capability(effect)
        {
            return Some(match recorded.answer(bytes) {
                Some((code::OK, _, back)) => self.answer_with(&back),
                Some((code, value, _)) => Answer { code, value },
                None => Answer::refused(code::FAILED),
            });
        }
        None
    }

    /// `done` encoded at the exchange's start, as the escape answers.
    fn answer_done(&self, done: &blaze_core::Done) -> Answer {
        let mut out = blaze_own::Vec::new_in(self.alloc);
        match crate::guest::codec::write_done(&mut out, done) {
            Some(()) => self.answer_with(&out),
            None => Answer::refused(code::FAILED),
        }
    }

    /// `exchange[at..at + len]`, or `None` when that is not inside it (or the sum overflows).
    fn range(&self, at: usize, len: usize) -> Option<&[u8]> {
        let end = at.checked_add(len)?;
        if end > self.exchange.len() {
            return None;
        }
        // SAFETY: `at..end` is inside the exchange, checked above. The program writes the
        // exchange only between calls and is inside this one, so nothing writes it while the
        // slice lives (it ends before the entry returns).
        Some(unsafe { core::slice::from_raw_parts(core::cell::UnsafeCell::raw_get(self.exchange.as_ptr()).add(at), len) })
    }

    /// `bytes` at `at` in the exchange, as a program would put them: a test's hostile program.
    #[cfg(test)]
    pub(crate) fn put(&self, at: usize, bytes: &[u8]) {
        assert!(at.checked_add(bytes.len()).is_some_and(|end| end <= self.exchange.len()), "a test puts inside the exchange");
        for (cell, byte) in self.exchange[at..].iter().zip(bytes) {
            // SAFETY: no view of the exchange lives outside an entry, and none is running.
            unsafe { *cell.get() = *byte };
        }
    }

    /// `bytes` at the exchange's start, and the answer saying how many; [`code::TOO_SMALL`]
    /// when they do not fit.
    fn answer_with(&self, bytes: &[u8]) -> Answer {
        if bytes.len() > self.exchange.len() {
            return Answer::refused(code::TOO_SMALL);
        }
        // SAFETY: fits, checked above; nothing else views the exchange during this entry.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), core::cell::UnsafeCell::raw_get(self.exchange.as_ptr()), bytes.len());
        }
        Answer::ok(bytes.len() as u64)
    }
}

/// The host behind `host`.
///
/// # Safety
/// `host` is the pointer [`Hosting::table`] put in the table, and the `Hosting` lives.
unsafe fn of<'a>(host: *mut c_void) -> &'a Hosting {
    // SAFETY: the caller's.
    unsafe { &*host.cast::<Hosting>() }
}

// Each entry checks the program's handles and ranges first (a refusal there never reaches
// the world and is not held), then answers through the holding (`Hosting::through`).

unsafe extern "C" fn write(host: *mut c_void, stream: u32, at: usize, len: usize) -> Answer {
    // SAFETY: the table's own pointer (`Hosting::table`).
    let hosting = unsafe { of(host) };
    let to = match stream {
        wire::stream::OUT => Stream::Out,
        wire::stream::ERR => Stream::Err,
        _ if hosting.planted_stream_unchecked => Stream::Out,
        _ => return Answer::refused(code::UNKNOWN),
    };
    let Some(bytes) = hosting.range(at, len) else { return Answer::refused(code::OUT_OF_RANGE) };
    hosting.through(Call::Write { stream, bytes: bytes.to_vec() }, None, || match hosting.io.write_out(to, &[bytes]) {
        Ok(()) => Answer::ok(len as u64),
        Err(_) => Answer::refused(code::FAILED),
    })
}

unsafe extern "C" fn clock(host: *mut c_void, which: u32) -> Answer {
    // SAFETY: the table's own pointer.
    let hosting = unsafe { of(host) };
    if which != wire::clock::MONOTONIC && which != wire::clock::WALL {
        return Answer::refused(code::UNKNOWN);
    }
    hosting.through(Call::Clock { which }, None, || match which {
        wire::clock::MONOTONIC => Answer::ok(u64::try_from(hosting.io.now().as_nanos()).unwrap_or(u64::MAX)),
        _ => Answer::ok(hosting.io.wall_ns()),
    })
}

unsafe extern "C" fn arg(host: *mut c_void, index: u64) -> Answer {
    // SAFETY: the table's own pointer.
    let hosting = unsafe { of(host) };
    hosting.through(Call::Arg { index }, None, || match usize::try_from(index).ok().and_then(|index| hosting.args.get(index)) {
        Some(arg) => hosting.answer_with(arg.as_bytes()),
        None => Answer::refused(code::END),
    })
}

unsafe extern "C" fn grant(host: *mut c_void, kind_at: usize, kind_len: usize, name_at: usize, name_len: usize) -> Answer {
    // SAFETY: the table's own pointer.
    let hosting = unsafe { of(host) };
    let (Some(kind), Some(name)) = (hosting.range(kind_at, kind_len), hosting.range(name_at, name_len)) else {
        return Answer::refused(code::OUT_OF_RANGE);
    };
    hosting.through(Call::Grant { kind: kind.to_vec(), name: name.to_vec() }, None, || {
        // A kind that fills itself reaches nothing outside the process: the launcher fills it
        // with no grant, and so does the host (D104: `fn main(cx: &Cx, out: Out)` runs with no
        // `--grant`).
        let given = crate::grant::Kind::fills_itself_named(kind)
            || hosting.grants.iter().any(|(granted_kind, granted)| granted_kind.as_bytes() == kind && granted.as_bytes() == name);
        if given { Answer::ok(0) } else { Answer::refused(code::REFUSED) }
    })
}

unsafe extern "C" fn effect(host: *mut c_void, at: usize, len: usize, budget_ns: u64) -> Answer {
    // SAFETY: the table's own pointer.
    let hosting = unsafe { of(host) };
    let Some(bytes) = hosting.range(at, len) else { return Answer::refused(code::OUT_OF_RANGE) };
    // The effect is read with a placeholder tag, then made again under the world's own.
    let escaped = match crate::guest::codec::read_effect(bytes) {
        Ok(escaped) => escaped,
        Err(_) => return Answer::refused(code::BAD_BYTES),
    };
    // clone: the layers' view of the effect; the world's copy is moved into its submission.
    let seen = escaped.clone();
    hosting.through(Call::Effect { bytes: bytes.to_vec(), budget_ns }, Some(seen.effect()), || {
        let io = &hosting.io;
        let tag = io.submit(|tag| escaped.under(tag));
        let until = Deadline::after(io.now(), Duration::from_nanos(budget_ns).saturating_add(io.limits().abandon_wait));
        let done = match io.settle(tag, until, "bstd.hosting.effect") {
            Ok(done) => done,
            Err(_) => return Answer::refused(code::FAILED),
        };
        let mut out = blaze_own::Vec::new_in(hosting.alloc);
        match crate::guest::codec::write_done(&mut out, &done) {
            Some(()) => hosting.answer_with(&out),
            None => Answer::refused(code::FAILED),
        }
    })
}

unsafe extern "C-unwind" fn exit(host: *mut c_void, status: i32) -> ! {
    // SAFETY: the table's own pointer.
    let hosting = unsafe { of(host) };
    // Held like any call; the run ends either way (a replay that differs here says so).
    hosting.through(Call::Exit { status }, None, || Answer::ok(0));
    *hosting.exited.lock() = Some(status);
    hosting.io.exit(status)
}
