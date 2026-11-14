//! The runner's side of a [`Selection`] (docs/v2/plan/design/test-selection.md, section 5): it
//! performs the machine's effects and decides nothing. A record is got from and filed to the
//! runner's [`Records`]; the program's changes since a state, and a function's answer, are the
//! binary's [`Image`] now against the one the suite's head was filed at ([`Program`]); a read's
//! answer is what the files and environment hold now ([`Inputs`], the hashes a trace is
//! assembled with); a run is handed back to the runner ([`Out`]) and its end given in
//! ([`Driver::ran`]) as the machine takes it: the functions it executed with their bound code
//! ([`Image::executed`]), or what it reaches when its record is not whole, and its traced
//! reads as names and digests.
//!
//! A test's base key ([`base`]) is the core's `TestRun` rule over what the runner knows of it:
//! the toolchain and target, the suite's settings, the environment blaze gives it, its entry
//! by item path and its libtest attributes.
//!
//! Nothing here waits: each [`Driver::drive`] answers every effect it can at once and returns
//! at the machine's first wait, with the runs it asked for, or at its end. Every loop is
//! bounded by what the machine can ask for its tests ([`steps_for`]).

#![blaze::sans_io]

use core::fmt;
use std::alloc::{AllocatorClone, Global};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use blaze_core::Traced;
use blaze_format::Aligned;
use blaze_ids::{ContentId, Hasher};
use blaze_keyed::{
    kind::{Key, Kind, Namespace, Settings, Tag, TestInputs, TestRun, ToolId},
    outside::MAX_SHAPES,
    pool::Ticket,
    store::Put,
};
use blaze_usercode::trace::Inputs;

use crate::{
    ask::{self, ABSENT, Ask, ReadKind, Shape, UNLOGGED},
    machine::{Effect, Executed, Input, NotWhole, Ran, Read, SelectError, Selection, Step},
    record::Record,
    table::{Bound, FnId, Image, TableError},
};

#[cfg(test)]
mod tests;

/// Where the records of a selection are: the daemon's store, or a map in a test.
pub trait Records {
    type Error: fmt::Display;
    /// The record filed under `tag`, `key`; `None` when there is none.
    fn get(&mut self, tag: Tag, key: ContentId) -> Result<Option<&[u8]>, Self::Error>;
    /// Files `bytes` under `tag`, `key` in `namespace`: [`Put::Other`] when other bytes are
    /// filed there (nothing is overwritten).
    fn put(&mut self, tag: Tag, key: ContentId, namespace: Namespace, bytes: &[u8]) -> Result<Put, Self::Error>;
}

/// The program a selection is of: its binary's functions now, and the binary the suite's head
/// was filed at, when it is another.
#[derive(Clone, Copy)]
pub struct Program<'p> {
    pub now: &'p Image,
    pub before: Option<&'p Image>,
}

/// A run the machine asked for: the runner runs test `test` (its place among the tests asked)
/// and gives its end with `ticket` ([`Driver::ran`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Out {
    pub ticket: Ticket,
    pub test: u32,
}

/// Where a [`Driver::drive`] stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Drove {
    /// The machine waits for the runs out: what it says it waits for.
    Waiting(&'static str),
    /// The request ended: [`Selection::outcome`].
    Ended,
}

/// Why a selection's host stopped, naming the record, read, state or test.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HostError {
    /// The machine refused the request.
    Select(SelectError),
    /// The records could not be read or written: the store's own words.
    Records { tag: Tag, key: ContentId, why: String },
    /// A read could not be answered now (a file that is there and can't be read).
    Read { kind: ReadKind, name: Vec<u8, Global>, why: String },
    /// The changes were asked since a program state the host holds no binary of.
    UnknownState { since: ContentId },
    /// A test's execution record is not of this binary.
    Table { test: String, why: TableError },
    /// The machine asked more than its tests can: a bug in it.
    Unbounded { steps: u64 },
    /// The machine asked to run a test after its runs were all out.
    LateRun { test: u32 },
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostError::Select(why) => write!(f, "{why}"),
            HostError::Records { tag, key, why } => {
                write!(f, "the test record of kind {} under {key:?} could not be read or filed: {why}", tag.0)
            }
            HostError::Read { kind, name, why } => write!(
                f,
                "a test's {} read of `{}` could not be answered now: {why} (rule no-fallback: a read that \
                 can't be told is never taken as unchanged)",
                match kind {
                    ReadKind::File => "file",
                    ReadKind::Dir => "directory",
                    ReadKind::Env => "environment",
                },
                String::from_utf8_lossy(name)
            ),
            HostError::UnknownState { since } => write!(
                f,
                "the test index head was filed at program state {since:?}, whose binary this daemon does \
                 not hold (rule keys-cover-every-input: its changes can't be told)"
            ),
            HostError::Table { test, why } => write!(f, "what test `{test}` executed: {why}"),
            HostError::Unbounded { steps } => write!(
                f,
                "the test selection asked more than {steps} effects for its tests (rule bounded-progress)"
            ),
            HostError::LateRun { test } => {
                write!(f, "the test selection asked to run test {test} after its runs were out")
            }
        }
    }
}

impl From<SelectError> for HostError {
    fn from(why: SelectError) -> HostError {
        HostError::Select(why)
    }
}

/// The most effects one drive of a selection of `tests` tests can answer: per test a lookup
/// (each shape slot read and answered, its value read), its settling, its filing (each slot
/// read, a shape and a value filed); and the index's own few.
pub fn steps_for(tests: usize) -> u64 {
    (tests as u64 + 1) * (4 * u64::from(MAX_SHAPES) + 16)
}

/// How one test ended, as the runner saw it.
pub struct Ended<'a> {
    pub ticket: Ticket,
    /// Its libtest name: what an error names.
    pub name: &'a str,
    pub passed: bool,
    pub micros: u64,
    /// What its fork server says it executed.
    pub record: &'a Record,
    /// The table indices its entry reaches (codegen's walk), asked only when its record is not
    /// whole and its pass can be kept.
    pub reached: &'a dyn Fn() -> Vec<u32, Global>, // dyn: one call at most per run, the walk the runner owns
    /// What it read, as the trace is assembled; `None` when the sandbox did not say.
    pub traced: Option<&'a Traced>,
    pub sandboxed: bool,
    /// Codegen marks it as reaching code the tracer cannot see.
    pub foreign: bool,
    /// Something `traced` names can have changed since the run began, so its hashes, taken
    /// as the trace is assembled, need not be what it read ([`Ran::moved`]).
    pub moved: bool,
}

/// What a run whose record is not whole says, as the machine names it: the binary has no slot
/// table ([`blaze_usercode::test::wire::UNRECORDED`]), or its process ended before its region
/// was read whole. The machine keys by the basis alone; the why is for the reader.
fn not_whole(why: &str) -> NotWhole {
    match why == blaze_usercode::test::wire::UNRECORDED {
        true => NotWhole::NoSlots,
        false => NotWhole::Cut,
    }
}

/// The scratch of one suite's selection across its drives.
pub struct Driver {
    aligned: Option<Aligned>,
    functions: Vec<FnId, Global>,
    now: Vec<ContentId, Global>,
}

impl Default for Driver {
    fn default() -> Driver {
        Driver { aligned: None, functions: Vec::new_in(Global), now: Vec::new_in(Global) }
    }
}

impl Driver {
    /// Answers every effect of `selection` it can, pushing each run it asks for to `runs`,
    /// until it waits for them or ends.
    pub fn drive<A: AllocatorClone, R: Records, I: Inputs>(
        &mut self,
        selection: &mut Selection<'_, A>,
        records: &mut R,
        program: Program<'_>,
        inputs: &mut I,
        runs: &mut Vec<Out, Global>,
    ) -> Result<Drove, HostError>
    where
        I::Error: fmt::Display,
    {
        let steps = steps_for(selection.tests());
        for _ in 0..steps {
            match selection.next() {
                Step::Exit => {
                    return match selection.outcome() {
                        Some(Err(why)) => Err(HostError::Select(why.clone())),
                        _ => Ok(Drove::Ended),
                    };
                }
                Step::Wait(what) => return Ok(Drove::Waiting(what)),
                Step::Do(Effect::Run { ticket, test }) => runs.push(Out { ticket, test }),
                Step::Do(Effect::Get { ticket, tag, key, .. }) => {
                    let got = records.get(tag, key).map_err(|why| HostError::Records { tag, key, why: why.to_string() })?;
                    let record = match got {
                        None => None,
                        // Records are read in place 16-aligned; one the store gives otherwise is
                        // copied once.
                        Some(bytes) if (bytes.as_ptr() as usize).is_multiple_of(16) => Some(bytes),
                        Some(bytes) => Some(&self.aligned.insert(Aligned::copy_from(bytes))[..]),
                    };
                    selection.give(Input::Got { ticket, record });
                }
                Step::Do(Effect::Changes { ticket, since }) => {
                    changes(program, since, &mut self.functions)?;
                    selection.give(Input::Changes { ticket, functions: &self.functions });
                }
                Step::Do(Effect::Answer { ticket, shape, .. }) => {
                    self.now.clear();
                    for asked in Shape::checked(shape).asks() {
                        self.now.push(answer(asked, program.now, inputs)?);
                    }
                    selection.give(Input::Answers { ticket, now: &self.now });
                }
                Step::Do(Effect::Put { ticket, tag, key, namespace, bytes, .. }) => {
                    let put = records
                        .put(tag, key, namespace, bytes)
                        .map_err(|why| HostError::Records { tag, key, why: why.to_string() })?;
                    selection.give(Input::Put { ticket, put: Ok(put) });
                }
            }
        }
        Err(HostError::Unbounded { steps })
    }

    /// Gives `selection` how a run ended, against `image`, the binary it ran.
    pub fn ran<A: AllocatorClone>(
        &mut self,
        selection: &mut Selection<'_, A>,
        image: &Image,
        ended: &Ended<'_>,
    ) -> Result<(), HostError> {
        let mut functions: Vec<(FnId, Bound), Global> = Vec::new_in(Global);
        let mut whole = true;
        let mut why = NotWhole::Cut;
        // What it executed is worked out only for a run that can be kept: a failure, a run
        // outside the sandbox or one whose reads are not known never is, and walking what a test
        // reaches is a walk of the program (the machine decides the same and ignores the rest).
        let keepable = ended.passed
            && ended.sandboxed
            && !ended.foreign
            && !ended.moved
            && ended.traced.is_some_and(|traced| traced.untraceable.is_empty());
        if keepable {
            let set = match ended.record {
                Record::Complete { slots, hits } => image.executed(*slots, hits),
                Record::Incomplete { why: said } => {
                    whole = false;
                    why = not_whole(said);
                    image.reached(&(ended.reached)())
                }
            };
            let set = set.map_err(|why| HostError::Table { test: ended.name.to_owned(), why })?;
            functions.reserve(set.fns.len());
            functions.extend(set.fns.iter().map(|version| (version.id, version.bound)));
        }
        let executed = match whole {
            true => Executed::Whole(&functions),
            false => Executed::Reached { why, functions: &functions },
        };
        let mut reads: Vec<Read<'_>, Global> = Vec::new_in(Global);
        let untraceable = match ended.traced {
            Some(traced) => {
                reads_of(traced, &mut reads);
                &traced.untraceable[..]
            }
            None => &[][..],
        };
        let ran = Ran {
            passed: ended.passed,
            micros: ended.micros,
            executed,
            unlogged: image.unlogged(),
            reads: &reads,
            untraceable,
            sandboxed: ended.sandboxed,
            traced: ended.traced.is_some(),
            foreign: ended.foreign,
            moved: ended.moved,
        };
        selection.give(Input::Ran { ticket: ended.ticket, ran });
        match selection.outcome() {
            Some(Err(why)) => Err(HostError::Select(why.clone())),
            _ => Ok(()),
        }
    }
}

/// Every read of `traced` as the machine asks it: a file and a directory by path with the
/// trace's hash, a variable by name with [`ask::env_digest`] of its value.
pub fn reads_of<'t>(traced: &'t Traced, out: &mut Vec<Read<'t>, Global>) {
    out.reserve(traced.files.len() + traced.dirs.len() + traced.env.len());
    for (path, hash) in &traced.files {
        out.push(Read { kind: ReadKind::File, name: path.as_os_str().as_bytes(), digest: hash.map(ContentId) });
    }
    for (path, hash) in &traced.dirs {
        out.push(Read { kind: ReadKind::Dir, name: path.as_os_str().as_bytes(), digest: Some(ContentId(*hash)) });
    }
    for (name, value) in &traced.env {
        let digest = value.as_ref().map(|value| ask::env_digest(value.as_bytes()));
        out.push(Read { kind: ReadKind::Env, name: name.as_bytes(), digest });
    }
}

/// Into `out`: the functions changed since program state `since`, which is the binary's now
/// (nothing) or the one before.
fn changes(program: Program<'_>, since: ContentId, out: &mut Vec<FnId, Global>) -> Result<(), HostError> {
    if since == program.now.state() {
        out.clear();
        return Ok(());
    }
    match program.before {
        Some(before) if before.state() == since => {
            program.now.changed_since(before, out);
            Ok(())
        }
        _ => Err(HostError::UnknownState { since }),
    }
}

/// What `asked` answers now: a function's bound code in `image` ([`ABSENT`] when gone, the
/// unlogged hash for [`UNLOGGED`]); a read's digest as `inputs` has it ([`ABSENT`] for nothing
/// there).
pub fn answer<I: Inputs>(asked: Ask<'_>, image: &Image, inputs: &mut I) -> Result<ContentId, HostError>
where
    I::Error: fmt::Display,
{
    let unreadable = |kind: ReadKind, name: &[u8], why: I::Error| {
        let mut owned = Vec::with_capacity_in(name.len(), Global);
        owned.extend_from_slice(name);
        HostError::Read { kind, name: owned, why: why.to_string() }
    };
    Ok(match asked {
        Ask::Function(function) if function == UNLOGGED => image.unlogged(),
        Ask::Function(function) => image.bound_of(&function).map_or(ABSENT, |bound| bound.0),
        Ask::Read(ReadKind::File, name) => inputs
            .file(Path::new(OsStr::from_bytes(name)))
            .map_err(|why| unreadable(ReadKind::File, name, why))?
            .map_or(ABSENT, ContentId),
        Ask::Read(ReadKind::Dir, name) => inputs
            .dir(Path::new(OsStr::from_bytes(name)))
            .map_err(|why| unreadable(ReadKind::Dir, name, why))?
            .map_or(ABSENT, ContentId),
        Ask::Read(ReadKind::Env, name) => {
            inputs.env(OsStr::from_bytes(name)).map_or(ABSENT, |value| ask::env_digest(value.as_bytes()))
        }
    })
}

/// What a test's base key is made from, beside its toolchain.
pub struct BaseOf<'a> {
    /// blaze's record makers, and the suite's target and codegen context.
    pub tool: ToolId,
    /// The suite: its package, target and context ([`blaze_usercode::test::Suite::given`]).
    pub settings: ContentId,
    /// The variables blaze gives the test, in any order, each once.
    pub env: &'a [(OsString, OsString)],
    /// Its libtest name: its entry's item path in its crate, which the suite's settings name
    /// (a `#[test]` function is never generic, so it has no type arguments).
    pub name: &'a str,
    /// Its `#[ignore]` and `#[should_panic]` ([`blaze_usercode::test::Attrs::id`]).
    pub attrs: ContentId,
}

/// The domain of a test's declared environment in its base key.
pub const ENV_SET_DOMAIN: &str = "blaze-testsel test env 1";
/// The domain of a test's entry, by its item path, in its base key.
pub const ITEM_DOMAIN: &str = "blaze-testsel test item 1";
/// The domain of a test's name and attributes in its base key.
pub const ATTRS_DOMAIN: &str = "blaze-testsel test attrs 1";

/// A test's base key: the core's [`TestRun`] rule over its identity. Nothing its code is: what
/// it executes and reads are its asks.
pub fn base(of: &BaseOf<'_>) -> Key<TestRun> {
    let mut order: Vec<u32, Global> = Vec::with_capacity_in(of.env.len(), Global);
    order.extend(0..of.env.len() as u32);
    order.sort_unstable_by(|a, b| of.env[*a as usize].0.cmp(&of.env[*b as usize].0));
    let mut env = Hasher::new(ENV_SET_DOMAIN);
    env.update(&(of.env.len() as u64).to_le_bytes());
    for at in order {
        let (name, value) = &of.env[at as usize];
        env.part(name.as_bytes()).part(value.as_bytes());
    }
    let mut item = Hasher::new(ITEM_DOMAIN);
    item.part(of.name.as_bytes());
    let mut attrs = Hasher::new(ATTRS_DOMAIN);
    attrs.part(of.name.as_bytes()).id(&of.attrs);
    TestRun::key(&TestInputs {
        tool: of.tool,
        settings: Settings(of.settings),
        env: env.finish(),
        item: item.finish(),
        entry_args: ABSENT,
        attrs: attrs.finish(),
        args: &[],
    })
}
