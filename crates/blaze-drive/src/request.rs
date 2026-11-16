//! The request machine (drive.md, "The request machine").
//!
//! [`Request::step`] takes one [`Event`] and pushes the [`Effect`]s it leads to. Every job is one
//! name; a job that lacks a page, a name or its home waits on it by name and is woken when it
//! arrives, so the machine never blocks and holds no dependency graph: what it knows of other
//! names is their rows, read from the answer pages a job touches, and the readers posted this
//! run.

use core::fmt;

use blaze_id::ContentId;
use blaze_ids::Hasher;
use blaze_own::{AVec, Alloc, CollectIn, SortedMap, SortedSet, Vec, VecDeque, soa};

use crate::Plant;
use crate::answers::{self, Last, Page, Root, Row};
use crate::condition::{Condition, ConditionError, MOST_ATOMS, MOST_CONDITIONS, OVER_CAP_RULE, Space, Var};
use crate::key::{self, ABSENT, Key, Kind, Name, Tool};
use crate::profile::Profile;
use crate::record::{self, Finding, Locate, Part, PathOf, Stored, Whole, WholeView};
use crate::table::{Hashed, Set, Table, spread};
use crate::wire::{NAME_BYTES, Reader, WireError, Writer};

// --- What the caller gives and gets ---------------------------------------------------------

/// The machine's bounds and the tool.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub tool: Tool,
    /// Runs out at once: the machine's worker count (one code path sized by the machine).
    pub workers: u32,
    /// The most times one job may be run or settled: past it, [`DriveError::Reentries`].
    pub most_entries: u32,
    /// The most jobs one request holds (rule 18: a table has a cap).
    pub most_jobs: u32,
    /// Whether a finding of this rule is an error: findings carry no level (rule 81), the
    /// command's policy says which fail it (`blaze run` and `test` fail on any, lints included).
    pub is_error: fn(u16) -> bool,
}

/// How `run` and `test` hold the program to the check of its scope (drive.md, "The gate";
/// bottom.md 11): one seam, chosen at the request (rule 78).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Gate {
    /// The default. A stored error refuses the start at one read; otherwise the program starts
    /// once its units are known and the scope's check runs beside it, yielding to what the
    /// program waits on; the first error stops it; no success is reported before the check is
    /// done.
    Beside,
    /// The check is done before the program starts.
    Before,
    /// Only what runs is checked; the verdict says so.
    Reached,
}

/// What the command concluded.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Verdict {
    /// `check`: the findings are the answer.
    Checked,
    /// The program succeeded and its scope's check found no error.
    Passed,
    /// The program succeeded; only what ran was checked (`Gate::Reached`).
    PassedReached,
    /// The check found errors: the program was stopped, or never started.
    Failed { errors: u32, started: bool },
    /// The program itself failed, its scope's check clean.
    ProgramFailed,
}

/// What the command asks.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Command {
    Check,
    Run,
    Test,
}

impl Command {
    /// The task kinds the command is defined to cover: what its answer claims was done. A
    /// caller whose worker builds fewer says which stages it left out (blaze-cli's closing
    /// note), so the note shrinks as kinds are built and is gone when all are.
    pub fn needs(self) -> &'static [Kind] {
        const CHECK: [Kind; 10] = [
            Kind::Units,
            Kind::UnitGraph,
            Kind::Fetch,
            Kind::FileItems,
            Kind::ModuleScope,
            Kind::Group,
            Kind::Expansion,
            Kind::Signature,
            Kind::ImplBucket,
            Kind::BodyCheck,
        ];
        const RUN: [Kind; 11] = [
            Kind::Units,
            Kind::UnitGraph,
            Kind::Fetch,
            Kind::FileItems,
            Kind::ModuleScope,
            Kind::Group,
            Kind::Expansion,
            Kind::Signature,
            Kind::ImplBucket,
            Kind::BodyCheck,
            Kind::Code,
        ];
        match self {
            Command::Check => &CHECK,
            Command::Run => &RUN,
            Command::Test => &Kind::ALL,
        }
    }

    fn byte(self) -> u8 {
        match self {
            Command::Check => 1,
            Command::Run => 2,
            Command::Test => 3,
        }
    }
}

/// The command and its arguments as the shape of the request.
#[derive(Clone, Copy, Debug)]
pub struct Ask<'a> {
    pub command: Command,
    pub args: &'a [u8],
    /// For `run` and `test`; `check` ignores it.
    pub gate: Gate,
    /// Every configuration the request answers for at once (drive.md, "One check covers every
    /// configuration"): [`Space::NONE`] for one. Its implications are closed by the caller.
    pub space: Space<'a>,
}

/// One file of the checkout, as the sweep found it.
#[derive(Clone, Copy, Debug)]
pub struct FileEntry<'a> {
    /// Relative to the checkout, `/`-separated.
    pub path: &'a [u8],
    pub id: ContentId,
    /// Its content differs from the leaf table the last answer was made with.
    pub changed: bool,
    /// A manifest (`Cargo.toml`, `Cargo.lock`): its path is in the root units' source, and an
    /// edit of it re-reads its own units ([`Name::manifest`]).
    pub manifest: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Ticket(pub u64);

impl Hashed for Ticket {
    fn hash(&self) -> u64 {
        spread(self.0)
    }
}

/// One [`Effect::Run`]: a worker takes it whole and answers it whole.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct BatchId(pub u64);

/// Tasks of one kind and one home, about [`TARGET_BATCH_NS`] of work together (drive.md,
/// "Scheduling on many cores"): the coordinator's cost per task drops by the batch's size.
#[derive(Debug)]
pub struct Batch<'a> {
    pub id: BatchId,
    pub tasks: Vec<(Ticket, Task<'a>), Alloc<'a>>,
}

/// A batch is sized to about this much work, from this run's mean time of its kind.
pub const TARGET_BATCH_NS: u64 = 1_000_000;
/// A kind's time per task before any of it has been measured this run.
pub const DEFAULT_TASK_NS: u64 = 50_000;
/// The most tasks one batch holds.
pub const MOST_BATCH: usize = 256;

/// What happened.
pub enum Event<'a> {
    /// The files the last answer read (the leaf table's, each checked today), sorted by path; the
    /// paths gone since; the `last` file; the units' source (the digest of what the workspace
    /// layout read: manifests, absent ones, listings). `cold`: there was no leaf table, so
    /// `files` is not what an answer reads and no certificate is looked up (files are opened as
    /// the walk reaches them, [`Effect::Open`]).
    ///
    /// `graph`: the `UnitGraph` job's source (blaze_drive::unit_graph_task::source), given when
    /// the caller read a lock this command (the layout moved, or no table): one job of the
    /// person's class, which asks `Fetch` for only the packages the roots reach, level by
    /// level. A lock entry nothing reaches is never asked for. The lock is one of the layout's
    /// reads, so a lock edit moves `units`, and with it the certificate.
    Start { files: &'a [FileEntry<'a>], gone: &'a [&'a [u8]], last: Option<&'a [u8]>, units: ContentId, cold: bool, graph: Option<ContentId> },
    /// An [`Effect::Open`]'s answer: the file's content id, or none (no such file).
    Opened { name: Name, id: Option<ContentId> },
    /// A [`Effect::Get`]'s answer: the bytes at `key`, or none.
    Got { key: Key, bytes: Option<&'a [u8]> },
    /// A [`Effect::Run`]'s answer: one outcome per task of the batch, together.
    Ran { batch: BatchId, results: &'a [Outcome<'a>] },
    /// The running program waits on a name (a function's code, through blaze-run's Parent):
    /// its job is the person's class, ahead of the gate's.
    Want(Want),
    /// The program ended (after [`Effect::StartProgram`]), succeeding or not.
    Exited { ok: bool },
}

/// One task's answer in a batch.
pub struct Outcome<'a> {
    pub ticket: Ticket,
    pub ran: Ran<'a>,
    /// Times the worker re-entered it in place, its needs answered from the settled table
    /// (`worker::answer_in_place`): counted as entries, never seen by the coordinator.
    pub in_place: u32,
    /// Nanoseconds of its runs, as the worker measured (the machine has no clock, rule 75).
    pub ns: u64,
}

/// What a task answered.
pub enum Ran<'a> {
    /// Its records: one, or a group's one per member.
    Done(&'a [Output<'a>]),
    /// What it lacks; it is entered again from the top with them as inputs.
    Need(&'a [Want]),
    /// It read a cfg variable its [`Task::condition`] does not fix (`Need(Cfg(variable))`): the
    /// job is entered again once per value the request's space allows, each a job of its own.
    Cfg(Var),
    Failed(TaskFailure),
}

/// One record a task made.
#[derive(Clone, Copy, Debug)]
pub struct Output<'a> {
    pub name: Name,
    /// What a reader of it sees: the digest a read of it records.
    pub out: ContentId,
    /// What it read, sorted by name, each with the out digest its input had.
    pub reads: &'a [(Name, ContentId)],
    pub parts: &'a [Part],
    /// The path of each `Locate::File` part, so a walk can open it ([`PathOf`]).
    pub paths: &'a [PathOf<'a>],
    pub findings: &'a [Finding<'a>],
    pub payload: &'a [u8],
}

/// A name a task needs, and the record that lists it (its file's `FileItems`, a `Fetch`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Want {
    pub name: Name,
    pub home: Name,
}

/// A task that could not answer: not a user's error (that is a finding), the tool's. It fails
/// that one job (rule 20): the request goes on, every other record is written, and the answer
/// is not (no certificate, no `last`), so the next command runs the job again.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TaskFailure {
    pub name: Name,
    pub why: Why,
}

/// Why a task failed, typed (rule 19); the caller names the file ([`Request::file_of`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Why {
    /// No task of the name's kind is built on the new item layer yet (drive.md, "Tasks and Need").
    NotBuilt,
    /// The task panicked (its message went to the error stream): an allocation its frame
    /// refused, or a bug. `used` is the most its scratch frame held, `limit` that frame's size.
    Panicked { used: u64, limit: u64 },
    /// The task's input is one this build refuses: `what`, under `rule`.
    Refused { what: &'static str, rule: &'static str },
    /// A task this one needs failed this command (its own failure is printed); this one has
    /// no record and prints nothing of its own: the closing note counts it.
    Waited { on: Name },
}

impl fmt::Display for TaskFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.why {
            Why::NotBuilt => write!(f, "{}: this task kind is not built on the new item layer yet (drive.md, \"Tasks and Need\")", self.name),
            Why::Panicked { used, limit } => write!(
                f,
                "{}: the task panicked (its message is above) with {used} of its frame's {limit} bytes used; it failed alone (rule 20)",
                self.name
            ),
            Why::Refused { what, rule } => write!(f, "{}: {what} ({rule})", self.name),
            Why::Waited { on } => write!(f, "{}: needs {on}, which failed this command; no record is kept (rule 3)", self.name),
        }
    }
}

/// Which work a task is: the person's (what the command asked) or background (speculative code,
/// the optimizing tier), which runs only on workers the person's work does not need (adapted
/// from makepad/relax (MIT), libs/cpu_budget/src/lib.rs).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Class {
    /// What the command asked: `check`'s walk, or what a running program waits on.
    Foreground,
    /// `run`'s and `test`'s check of the program's scope beside it (drive.md, "The gate"):
    /// after what the program waits on, before speculative work.
    Gate,
    Background,
}

const CLASSES: usize = 3;
/// One ready ring per (class, kind).
const RINGS: usize = CLASSES * crate::key::KINDS;

/// The ring of `class` and `kind`, in run order.
fn ring(class: Class, kind: Kind) -> usize {
    class as usize * crate::key::KINDS + kind.index()
}

/// A record a task is handed: where it is and the digest a read of it records.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Input {
    pub name: Name,
    pub at: Key,
    pub out: ContentId,
}

/// A task to run.
#[derive(Debug)]
pub struct Task<'a> {
    /// The job's name: what its [`Output`] is of. Under a condition it is a fork's name
    /// (`key::at_condition`), and the item is [`Task::base`].
    pub name: Name,
    /// The item the task is about: `name` under [`Condition::ANY`]. Workers find the item, and
    /// name what they read and want, by base names; the machine maps them to the records the
    /// task's condition implies.
    pub base: Name,
    /// The cfg values the task may read; reading any other variable is [`Ran::Cfg`].
    pub condition: Condition,
    pub key: Key,
    pub source: ContentId,
    pub class: Class,
    /// How many times it has been entered before (0 the first time).
    pub entry: u32,
    pub home: Name,
    pub place: u32,
    /// A `FileItems` task's file, an index for [`Request::path`].
    pub file: Option<u32>,
    /// The edition its file is read in: a `FileItems` task's own file's, an item's task's
    /// home file's; 0 for a task of no file.
    pub edition: u8,
    /// Its inputs: last time's read list first, then what it needed since.
    pub inputs: Vec<Input, Alloc<'a>>,
    /// What it needed that settled with no record: each's task failed this command (rule 20),
    /// sorted. A task tells "not asked yet" from "asked and failed" by it, so it never asks
    /// again for what failed (drive.md, "Owed, step 1").
    pub failed: Vec<Name, Alloc<'a>>,
    /// A `Group`'s members.
    pub members: Vec<Name, Alloc<'a>>,
}

/// A finding to print: the caller places it with the file's line table.
#[derive(Debug)]
pub struct Print<'a> {
    /// The item (a base name, never a fork's).
    pub name: Name,
    /// The configurations it holds in: every one that implies this (bottom.md 11: a finding
    /// carries the reduced condition it was found under).
    pub condition: Condition,
    /// The file it is in, an index for [`Request::path`]; `None` for a manifest's or a crate's.
    pub file: Option<u32>,
    /// Its item's start in the file.
    pub place: u32,
    pub rule: u16,
    pub start: u32,
    pub end: u32,
    pub message: Vec<u8, Alloc<'a>>,
}

/// What the machine asks of its caller.
#[derive(Debug)]
pub enum Effect<'a> {
    Get { key: Key },
    /// A part names a file the request does not hold: read it at `path` (a `module` slot is
    /// `x.rs`, else `x/mod.rs`), and answer [`Event::Opened`].
    Open { name: Name, path: Vec<u8, Alloc<'a>>, module: bool },
    /// Write `bytes` at `key` once; bytes already there under the key stand.
    Put { key: Key, bytes: Vec<u8, Alloc<'a>> },
    Run { batch: Batch<'a> },
    /// Write the name's row of the settled table (`Settled::publish`), after every `Put` before
    /// it has completed: a worker that reads the row reads the record at once.
    Publish(Input),
    /// Clear the name's row of the settled table (`Settled::clear`): dirtied or dropped.
    Unpublish(Name),
    /// Start the program whose entry the units' record at `units` names (blaze-run's Parent).
    /// Answered by [`Event::Exited`].
    StartProgram { units: Key },
    /// Stop the running program: the gate found `errors` error-level findings.
    Stop { errors: u32 },
    Print(Print<'a>),
    /// Replace the checkout's `last` file with `bytes`, after every `Put` before it.
    WriteLast { bytes: Vec<u8, Alloc<'a>> },
    Done(Answer),
}

/// The request's answer.
#[derive(Clone, Copy, Debug)]
pub struct Answer {
    pub profile: Profile,
    pub verdict: Verdict,
    pub findings: u32,
    /// Where the units' record is, for `run` and `test` to find the entry.
    pub units: Option<Key>,
    /// Tasks that failed ([`Request::failures`]): when any did, the answer was not written and
    /// the command fails.
    pub failed: u32,
}

/// Who makes a record a text-less crate lacks.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Builder {
    /// Its crate's pack, from the shared cache or CI.
    Pack { krate: Name },
    /// `blaze prepare-std`: the sysroot crates' tasks run over `library/`.
    PrepareStd,
}

/// Why a request stopped. Each names the thing and the rule (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DriveError {
    /// Bytes at `key` are not the record they are stored as.
    Wire { key: Key, error: WireError },
    /// The checkout's `last` file is not one.
    LastFile(WireError),
    /// The caller handed a list unsorted.
    Unsorted { what: &'static str },
    /// A file a module lists is not in the checkout.
    NoFile { file: Name, home: Name },
    /// A task wanted a name its home does not list.
    NoPart { name: Name, home: Name },
    /// A text-less crate's record is not in the store.
    Missing { name: Name, key: Key, builder: Builder },
    /// The record at `key` is another name's.
    WrongRecord { key: Key, asked: Name, held: Name },
    /// A task answered a record for a name no job asked.
    UnaskedOutput { name: Name, by: Name },
    /// An `Opened` for a file no `Open` asked for, or answered twice.
    UnaskedOpen { file: Name },
    /// A file's job settled with no edition to read it in: the machine's bug.
    Unlocated { name: Name },
    UnknownTicket(Ticket),
    /// An answer to a batch that is not out (answered twice, or never sent).
    UnknownBatch(BatchId),
    UnaskedGet(Key),
    /// A job ran or settled more than `most_entries` times.
    Reentries { name: Name, key: Key, entries: u32, last_needs: [Option<Name>; 4] },
    /// Jobs that wait on each other and are not module scopes.
    NeedCycle { names: [Option<Name>; 8] },
    /// A task needed itself.
    SelfNeed { name: Name },
    TooManyJobs { most: u32, name: Name },
    /// A step after the answer.
    Finished,
    /// `Exited` with no program started, or a second time.
    NoProgram,
    /// The program wanted a name nothing settled after the check was done: code the check's
    /// walk does not reach is not built yet (drive.md, "The gate").
    LateWant { name: Name },
    /// A condition of `name` could not be formed (too many atoms, a variable the space lacks).
    Condition { name: Name, error: ConditionError },
    /// A split record forks on a variable its own condition fixes: no task reads a fixed
    /// variable as unfixed.
    SplitOnFixed { name: Name, var: Var },
    /// The request's space has implications that are not closed (`Space::closed`).
    SpaceNotClosed,
}

impl fmt::Display for DriveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DriveError::Wire { key, error } => write!(f, "the record at {key}: {error}"),
            DriveError::LastFile(error) => write!(f, "the checkout's last answer .blaze/v2/last, of this build's form, is malformed: {error}"),
            DriveError::Unsorted { what } => write!(f, "{what} handed unsorted (drive.md: the sweep sorts by path)"),
            DriveError::NoFile { file, home } => write!(f, "{home} lists the file {file}, which is not in the checkout"),
            DriveError::NoPart { name, home } => write!(f, "a task wanted {name} of {home}, which does not list it"),
            DriveError::Missing { name, key, builder } => match builder {
                Builder::Pack { krate } => write!(f, "{name} (record {key}) is not in the store: it comes in {krate}'s pack; fetch it (rule 3: no local fallback)"),
                Builder::PrepareStd => write!(f, "{name} (record {key}) is not in the store: run `blaze prepare-std`"),
            },
            DriveError::WrongRecord { key, asked, held } => write!(f, "the record at {key} is {held}'s, asked as {asked}'s (rule 21: a key covers every input)"),
            DriveError::UnaskedOutput { name, by } => write!(f, "{by} answered a record of {name}, which no job asked"),
            DriveError::UnaskedOpen { file } => write!(f, "the file {file} was answered as opened, and no `Open` of it is waiting"),
            DriveError::Unlocated { name } => write!(f, "the file {name} settled with no edition to read it in (blaze-drive's bug)"),
            DriveError::UnknownTicket(ticket) => write!(f, "an answer to run {}, which is not out in that batch", ticket.0),
            DriveError::UnknownBatch(batch) => write!(f, "an answer to batch {}, which is not out (a batch is answered once)", batch.0),
            DriveError::UnaskedGet(key) => write!(f, "an answer to a get of {key}, which is not out"),
            DriveError::Reentries { name, key, entries, last_needs } => {
                write!(f, "{name} (key {key}) was entered {entries} times (rule 18); its last needs:")?;
                for need in last_needs.iter().flatten() {
                    write!(f, " {need}")?;
                }
                Ok(())
            }
            DriveError::NeedCycle { names } => {
                write!(f, "tasks that need each other:")?;
                for name in names.iter().flatten() {
                    write!(f, " {name}")?;
                }
                Ok(())
            }
            DriveError::SelfNeed { name } => write!(f, "{name} needed itself"),
            DriveError::TooManyJobs { most, name } => write!(f, "more than {most} jobs, at {name} (rule 18)"),
            DriveError::Finished => write!(f, "a step after the request answered"),
            DriveError::NoProgram => write!(f, "a program's exit, with no program running"),
            DriveError::LateWant { name } => write!(f, "the program wanted {name} after the check was done; a late want is not built (drive.md, \"The gate\")"),
            DriveError::Condition { name, error } => write!(f, "the conditions of {name}: {error}"),
            DriveError::SplitOnFixed { name, var } => {
                write!(f, "the record of {name} forks on cfg variable {var}, which its condition fixes (rule 21)")
            }
            DriveError::SpaceNotClosed => {
                write!(f, "the request's feature implications are not transitively closed (drive.md, \"One check covers every configuration\")")
            }
        }
    }
}

impl core::error::Error for DriveError {}

// --- The machine's own state ------------------------------------------------------------------

soa! {
    /// One file of the checkout: scanned by subject (a binary search), so a column.
    struct FileRow, FileRows, FileRowRef {
        subject: ContentId => subjects / subjects_mut,
        id: ContentId => ids / ids_mut,
        path_at: u32 => path_ats / path_ats_mut,
        path_len: u32 => path_lens / path_lens_mut,
        changed: bool => changeds / changeds_mut,
        manifest: bool => manifests / manifests_mut,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// Finding its source and its row.
    Start,
    /// Has its key: decide stand, look up or check by value.
    Decide,
    /// Waits for the digests of its shape's reads to compute its value key.
    Value,
    Looking,
    Valuing,
    /// Waits for its inputs to settle before a run.
    Gather,
    Queued,
    Running,
    Settled,
}

struct Job<'a> {
    state: State,
    home: Name,
    place: u32,
    locate: Option<Locate>,
    source: ContentId,
    key: Key,
    class: Class,
    seed: bool,
    /// Dirtied by a read that changed: check by value, not by key.
    recheck: bool,
    /// Dirtied while its run or lookup was out: decide again once it settles.
    redo: bool,
    entries: u32,
    settles: u32,
    /// Bumped each time the job is queued or leaves the queue: a ring entry of another
    /// generation is stale.
    queue_gen: u32,
    /// On one wait list since its last advance: an advance registers at most one wait, so the
    /// lists hold at most one entry per advance (no `contains` scan) and a job is woken by the
    /// first of its waits to settle, then waits again on the next.
    parked: bool,
    /// The names its next run is handed, sorted.
    needs: Vec<Name, Alloc<'a>>,
    /// The read names its value key is made of.
    shape: Vec<Name, Alloc<'a>>,
    /// Jobs waiting on this one.
    waiters: Vec<Name, Alloc<'a>>,
    members: Vec<Name, Alloc<'a>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Purpose {
    Cert,
    OldCert,
    Root,
    Page(u32),
    Look(Name),
    Value(Name),
}

enum PageState<'a> {
    Loading,
    Loaded(Page<'a>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Idle,
    Certifying,
    Working,
    Finishing,
    /// The walk is done and written; the answer waits for the program's end.
    Held,
    Answered,
}

/// A request's shape: its command, its arguments and the space it is checked under. A
/// certificate and a last answer are one shape's; the machine walks forward only from its own.
pub fn shape_of(ask: &Ask<'_>) -> ContentId {
    let mut h = Hasher::new(SHAPE_DOMAIN);
    h.update(&[ask.command.byte()]).part(ask.args).id(&key::space_digest(&ask.space));
    h.finish()
}

/// Whether the last answer `last` (a pointer's record) is `tool`'s and of `shape`: the one the
/// machine walks forward from. Another's is no answer of this request's: its caller starts
/// cold (the layout read, the dependency graph's job handed to `Start`), as with none.
pub fn walks_from(last: &[u8], tool: Tool, shape: &ContentId) -> Result<bool, WireError> {
    let last = answers::read_last(last)?;
    Ok(last.tool == tool.0 && last.shape == *shape)
}

/// A name's findings as printed: its file, its start, its findings.
struct Held<'a> {
    file: Name,
    place: u32,
    /// The item and the condition its record was found under: a fork's findings print as its
    /// base's, under its condition.
    base: Name,
    condition: Condition,
    findings: Vec<HeldFinding<'a>, Alloc<'a>>,
}

struct HeldFinding<'a> {
    rule: u16,
    start: u32,
    end: u32,
    message: Vec<u8, Alloc<'a>>,
}

/// Where a name's settled record is and what a read of it sees.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Now {
    at: Key,
    out: ContentId,
}

enum Digest {
    Ready(Option<Now>),
    Wait,
}

/// One command's request.
pub struct Request<'a> {
    alloc: Alloc<'a>,
    config: Config,
    command: Command,
    shape_digest: ContentId,
    units_source: ContentId,
    plant: Plant,
    phase: Phase,
    profile: Profile,

    /// The files held, in the order they joined (`Start`'s, then each `Opened`): a row's index is
    /// stable for the request ([`Task::file`]).
    files: FileRows<Alloc<'a>>,
    /// A file's name ([`Name::file`]) to its row.
    file_at: Table<Name, u32, Alloc<'a>>,
    /// The paths of the files and of the parts' files ([`PathOf`]), in one buffer.
    paths: Vec<u8, Alloc<'a>>,
    /// A `Locate::File` part's path (at, len in `paths`) and whether it is a module slot.
    part_paths: Table<Name, (u32, u32, bool), Alloc<'a>>,
    /// Files asked for by [`Effect::Open`] and not answered yet: the jobs that wait on each.
    opening: Table<Name, Vec<Name, Alloc<'a>>, Alloc<'a>>,
    /// Files opened and found absent.
    absent_files: Set<Name, Alloc<'a>>,
    /// No leaf table: no certificate is looked up, and `files` is not an answer's read set.
    cold: bool,
    gone: Vec<ContentId, Alloc<'a>>,
    /// The `UnitGraph` job's source `Start` gave (`unit_graph_task::source`): its one job at
    /// `begin`, which asks `Fetch` for each level's sources as it reaches them.
    graph: Option<ContentId>,
    cert: Key,
    last: Option<Last>,
    forward: bool,

    root: Option<Root<'a>>,
    root_loading: bool,
    root_waiters: Vec<Name, Alloc<'a>>,
    pages: Table<u32, PageState<'a>, Alloc<'a>>,
    page_waiters: Table<u32, Vec<Name, Alloc<'a>>, Alloc<'a>>,
    /// Rows this run changed or made.
    new: Table<Name, Row<'a>, Alloc<'a>>,
    /// What each settled name is now.
    now: Table<Name, Now, Alloc<'a>>,
    /// Names a re-made home no longer lists.
    dropped: Set<Name, Alloc<'a>>,
    /// The locates of the parts of homes loaded or made this run.
    part_locs: Table<Name, (Locate, u32, Name), Alloc<'a>>,
    /// Readers posted this run: read name to the names that read it (a name settled twice
    /// posts twice; `dirty_readers` dedups).
    posted: Table<Name, Vec<Name, Alloc<'a>>, Alloc<'a>>,

    jobs: Table<Name, Job<'a>, Alloc<'a>>,
    /// Jobs settled: `jobs.len() - settled` is how many are not (no scan).
    settled: usize,
    wake: VecDeque<Name, Alloc<'a>>,
    gets: Table<Key, Vec<Purpose, Alloc<'a>>, Alloc<'a>>,
    tickets: Table<Ticket, (Name, BatchId), Alloc<'a>>,
    next_ticket: u64,
    /// Queued jobs, one FIFO ring per (class, kind) in run order: the person's first, then by
    /// kind (files, scopes, signatures, bodies, code). An entry is (name, the job's queue
    /// generation); one whose generation is not the job's is stale and skipped, so a job leaves
    /// the queue in O(1) (`unqueue`).
    ready: Vec<VecDeque<(Name, u32), Alloc<'a>>, Alloc<'a>>,
    /// Live entries per ring and in all.
    ready_live: [u32; RINGS],
    queued: usize,
    /// Stale entries skipped: bounded by queue operations, counted for the linearity test.
    ready_ops: u64,
    /// Batches out: at most one per worker, so a sorted set of at most `workers`.
    batches: SortedSet<BatchId, Alloc<'a>>,
    next_batch: u64,
    /// The settled table's writes this step, last write per name: flushed as `Publish` and
    /// `Unpublish` at the step's end, after every `Put` of the step.
    publish: Table<Name, Option<Input>, Alloc<'a>>,

    findings: Table<Name, Held<'a>, Alloc<'a>>,
    old_findings: SortedMap<Name, Held<'a>, Alloc<'a>>,
    old_findings_loading: bool,
    units_at: Option<Key>,
    /// Every `Units` job this run, in the order added: a file no answer holds and no record
    /// listed yet waits on one still open, as a member's units may list it.
    units_jobs: AVec<'a, Name>,
    /// Jobs in `Value` whose shape came from the store with no row of this checkout (another
    /// worktree's, [`Request::value`]) and names a read nothing has listed yet this run: they
    /// wait for the walk to go quiet, when a name still not listed is absent today.
    unwalked: AVec<'a, Name>,
    /// The jobs [`Request::walk_quiet`] released, sorted: a read of theirs no one listed is
    /// `ABSENT`. A job leaves when its value key is asked for.
    walked: AVec<'a, Name>,

    gate: Gate,
    /// The class the walk's jobs take: `Gate` beside a running program, else the person's.
    walk: Class,
    program: Program,
    /// The answer once the walk is done, held until the program's end (`conclude`).
    held: Option<Answer>,
    /// Error-level findings settled this run.
    errors: u32,
    /// Tasks that failed this run, in the order they came (rule 20: each fails its own job).
    failures: Vec<TaskFailure, Alloc<'a>>,

    /// The request's space, owned (the caller's borrow ends with the constructor).
    space_values: Vec<u16, Alloc<'a>>,
    space_implications: Vec<(Var, Var), Alloc<'a>>,
    space_closed: bool,
    /// What a split record's read of `space_name` (`key::space_name`) sees.
    space_digest: ContentId,
    space_name: Name,
    /// Each fork made or walked this run: its base and its condition (a fork with no entry
    /// here is found by its row's `cond`).
    conditioned: Table<Name, (Name, Condition), Alloc<'a>>,
}

/// Where a need of a job under a condition leads (drive.md, "One check covers every
/// configuration": a lookup by implication).
enum Resolved {
    /// The record the asker's condition implies is settled (`emit` hands it by
    /// `resolve_settled`).
    At,
    /// A node on the way is not settled; the asker waits on it.
    Wait,
    /// The need splits on a variable the asker's condition does not fix: the asker forks.
    Fork(Var),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Program {
    Unstarted,
    Running,
    /// Told to stop by the gate; its `Exited` is still to come.
    Stopped,
    Ended { ok: bool },
}

/// The digest domain of a request's shape.
const SHAPE_DOMAIN: &str = "blaze-drive request shape 1";
const GROUP_DOMAIN: &str = "blaze-drive group 1";
/// 2: each finding's item carries its base and condition.
/// BDC3: the answer's root first. BDC2 had none.
pub(crate) const CERT_MAGIC: &[u8; 4] = b"BDC3";

impl<'a> Request<'a> {
    pub fn new_in(config: Config, ask: Ask<'_>, alloc: Alloc<'a>) -> Request<'a> {
        Request::planted_in(config, ask, Plant::None, alloc)
    }

    /// A request with `plant` planted: for a simulation's control only.
    pub fn planted_in(config: Config, ask: Ask<'_>, plant: Plant, alloc: Alloc<'a>) -> Request<'a> {
        // The space is part of the request's shape: a certificate answers for the space it
        // was checked under, and no other.
        let space_digest = key::space_digest(&ask.space);
        let shape_digest = shape_of(&ask);
        let a = alloc.clone();
        let mut now = Table::new_in(a.clone());
        now.insert(key::space_name(), Now { at: Key::default(), out: space_digest });
        Request {
            space_values: Vec::from_slice_in(ask.space.values, a.clone()),
            space_implications: Vec::from_slice_in(ask.space.implications, a.clone()),
            space_closed: ask.space.closed(),
            space_digest,
            space_name: key::space_name(),
            conditioned: Table::new_in(a.clone()),
            config,
            command: ask.command,
            shape_digest,
            units_source: ABSENT,
            plant,
            phase: Phase::Idle,
            profile: Profile::default(),
            files: FileRows::new_in(a.clone()),
            file_at: Table::new_in(a.clone()),
            paths: Vec::new_in(a.clone()),
            part_paths: Table::new_in(a.clone()),
            opening: Table::new_in(a.clone()),
            absent_files: Set::new_in(a.clone()),
            cold: false,
            gone: Vec::new_in(a.clone()),
            graph: None,
            cert: Key::default(),
            last: None,
            forward: false,
            root: None,
            root_loading: false,
            root_waiters: Vec::new_in(a.clone()),
            pages: Table::new_in(a.clone()),
            page_waiters: Table::new_in(a.clone()),
            new: Table::new_in(a.clone()),
            now,
            dropped: Set::new_in(a.clone()),
            part_locs: Table::new_in(a.clone()),
            posted: Table::new_in(a.clone()),
            jobs: Table::new_in(a.clone()),
            settled: 0,
            wake: VecDeque::new_in(a.clone()),
            gets: Table::new_in(a.clone()),
            tickets: Table::new_in(a.clone()),
            next_ticket: 0,
            ready: {
                let mut rings = Vec::with_capacity_in(RINGS, a.clone());
                for _ in 0..RINGS {
                    rings.push(VecDeque::new_in(a.clone()));
                }
                rings
            },
            ready_live: [0; RINGS],
            queued: 0,
            ready_ops: 0,
            batches: SortedSet::new_in(a.clone()),
            next_batch: 0,
            publish: Table::new_in(a.clone()),
            findings: Table::new_in(a.clone()),
            old_findings: SortedMap::new_in(a.clone()),
            old_findings_loading: false,
            units_at: None,
            units_jobs: Vec::new_in(a.clone()),
            unwalked: Vec::new_in(a.clone()),
            walked: Vec::new_in(a.clone()),
            gate: ask.gate,
            // Beside a program the walk is the gate's; with no program it is the answer itself.
            walk: match (starts_program(ask.command, plant), ask.gate) {
                (true, Gate::Beside) => Class::Gate,
                _ => Class::Foreground,
            },
            program: Program::Unstarted,
            held: None,
            errors: 0,
            failures: Vec::new_in(alloc.clone()),
            alloc,
        }
    }

    /// The tasks that failed this run, in the order they came.
    pub fn failures(&self) -> &[TaskFailure] {
        &self.failures
    }

    fn failed(&self) -> u32 {
        u32::try_from(self.failures.len()).unwrap_or(u32::MAX)
    }

    /// The path of the file a task is about or in: a `FileItems` task's own, else its home's
    /// when that is a file (an item's task); `None` for a manifest's or a crate's.
    pub fn file_of(&self, task: &Task<'_>) -> Option<&[u8]> {
        if let Some(index) = task.file {
            return Some(self.path(index));
        }
        let index = self.file_index(&file_of(task.base, task.home).subject)?;
        Some(self.path(u32::try_from(index).ok()?))
    }

    /// `run`: a program is part of the answer. `test` starts none: a package's `main` is not
    /// its tests, and until tests run on this path (docs/v2/plan/design/test.md) `test` is the
    /// check of all it reaches, answered [`Verdict::Checked`] or failed, never passed (decision
    /// D61 item 5: no pass over a stage that did not run).
    fn runs_program(&self) -> bool {
        starts_program(self.command, self.plant)
    }

    /// The units are known: under `Beside` and `Reached` the program starts now, unless an
    /// error is already found.
    fn units_known(&mut self, at: Key, out: &mut AVec<'_, Effect<'a>>) {
        if self.runs_program()
            && self.gate != Gate::Before
            && self.program == Program::Unstarted
            && self.errors == 0
            && self.failures.is_empty()
        {
            self.program = Program::Running;
            out.push(Effect::StartProgram { units: at });
        }
    }

    /// A settled record held `errors` error-level findings: under `Beside`, the first stops the
    /// running program.
    fn found_errors(&mut self, errors: u32, out: &mut AVec<'_, Effect<'a>>) {
        if errors == 0 {
            return;
        }
        self.errors += errors;
        if self.gate == Gate::Beside && self.program == Program::Running {
            self.program = Program::Stopped;
            out.push(Effect::Stop { errors: self.errors });
        }
    }

    /// The program ended.
    fn exited(&mut self, ok: bool, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        match self.program {
            Program::Running | Program::Stopped => {}
            Program::Unstarted | Program::Ended { .. } => return Err(DriveError::NoProgram),
        }
        // Stopped or not, it has ended now: what `conclude` waits for.
        self.program = Program::Ended { ok };
        if self.plant == Plant::EarlySuccess && ok && self.phase != Phase::Held {
            // The planted bug: success reported at the program's exit, the gate's work not done.
            self.phase = Phase::Answered;
            let answer = Answer { profile: self.profile, verdict: Verdict::Passed, findings: self.errors, units: self.units_at, failed: self.failed() };
            out.push(Effect::Done(answer));
            return Ok(());
        }
        if self.gate == Gate::Reached && self.phase != Phase::Held {
            // Only what ran was checked: the verdict is the program's, said as such.
            self.phase = Phase::Answered;
            let verdict = if ok { Verdict::PassedReached } else { Verdict::ProgramFailed };
            out.push(Effect::Done(Answer { profile: self.profile, verdict, findings: self.errors, units: self.units_at, failed: self.failed() }));
            return Ok(());
        }
        self.conclude(out);
        Ok(())
    }

    /// The answer, once both the check and the program (if any) are done: a success is never
    /// reported while an error is found or could still be.
    fn conclude(&mut self, out: &mut AVec<'_, Effect<'a>>) {
        let Some(mut answer) = self.held else { return };
        let started = self.program != Program::Unstarted;
        let verdict = match (self.runs_program(), self.program) {
            (false, _) => Verdict::Checked,
            // A started program is waited for, stopped or not: its `Exited` is the last event,
            // and the answer comes after it. Answering first left the caller holding a start it
            // had not performed, and its exit stepped an answered machine (`blaze run` after
            // `blaze check` with a stood error, 2026-11-16: "a step after the request answered").
            (true, Program::Running | Program::Stopped) if self.plant != Plant::AnswerWhileRunning => return,
            // A failed task is a scope not checked: no success is reported over it.
            _ if self.errors > 0 || !self.failures.is_empty() => Verdict::Failed { errors: self.errors, started },
            // Only under the plant: a running program without an error is still waited for.
            (true, Program::Running | Program::Stopped) => return,
            (true, Program::Unstarted) => {
                // Before: the check is clean, the program starts now.
                let units = answer.units.unwrap_or_default();
                self.program = Program::Running;
                out.push(Effect::StartProgram { units });
                return;
            }
            (true, Program::Ended { ok: true }) => Verdict::Passed,
            (true, Program::Ended { ok: false }) => Verdict::ProgramFailed,
        };
        answer.verdict = verdict;
        answer.profile = self.profile;
        answer.failed = self.failed();
        self.held = None;
        self.phase = Phase::Answered;
        out.push(Effect::Done(answer));
    }

    /// An `Open` answered: the file joins the table (new to this answer, so `changed`) or is
    /// absent; the jobs that waited on it go on.
    fn opened(&mut self, name: Name, id: Option<ContentId>) -> Result<(), DriveError> {
        let waiters = self.opening.remove(&name).ok_or(DriveError::UnaskedOpen { file: name })?;
        let (at, len, _) = *self.part_paths.get(&name).expect("an opened file's path was a part's");
        match id {
            Some(id) => {
                let path = Vec::from_slice_in(&self.paths[at as usize..(at + len) as usize], self.alloc);
                self.add_file(&path, id, true, false);
            }
            None => {
                self.absent_files.insert(name);
            }
        }
        for waiter in waiters.iter() {
            self.wake.push_back(*waiter);
        }
        Ok(())
    }

    /// The running program waits on `want`: its job runs in the person's class.
    fn wanted(&mut self, want: Want) -> Result<(), DriveError> {
        if self.phase == Phase::Held {
            return match self.now.contains_key(&want.name) {
                true => Ok(()),
                false => Err(DriveError::LateWant { name: want.name }),
            };
        }
        match self.jobs.get(&want.name).map(|job| (job.class, job.state)) {
            None => self.add_job(want.name, want.home, None, false, Class::Foreground),
            Some((class, state)) if class > Class::Foreground => {
                match state {
                    // Out of its ring (O(1): the entry goes stale), into the person's.
                    State::Queued => self.queue(Class::Foreground, want.name),
                    _ => self.jobs.get_mut(&want.name).expect("a job").class = Class::Foreground,
                }
                Ok(())
            }
            Some(_) => Ok(()),
        }
    }

    /// The path of file `index` (a [`Task::file`] or [`Print::file`]).
    pub fn path(&self, index: u32) -> &[u8] {
        let at = self.files.path_ats()[index as usize] as usize;
        let len = self.files.path_lens()[index as usize] as usize;
        &self.paths[at..at + len]
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// The request's certificate key, known after `Start`.
    pub fn certificate(&self) -> Key {
        self.cert
    }

    /// One event in; its effects pushed onto `out`.
    pub fn step(&mut self, event: Event<'_>, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        if self.phase == Phase::Answered {
            return Err(DriveError::Finished);
        }
        self.profile.steps += 1;
        match event {
            Event::Start { files, gone, last, units, cold, graph } => {
                self.graph = graph;
                self.start(files, gone, last, units, cold, out)?
            }
            Event::Opened { name, id } => self.opened(name, id)?,
            Event::Got { key, bytes } => self.got(key, bytes, out)?,
            Event::Ran { batch, results } => self.ran_batch(batch, results, out)?,
            Event::Want(want) => self.wanted(want)?,
            Event::Exited { ok } => {
                self.exited(ok, out)?;
                if self.phase == Phase::Answered {
                    return Ok(());
                }
            }
        }
        if self.phase == Phase::Held {
            return Ok(());
        }
        self.pump(out)
    }

    /// The table's writes of this step, after its `Put`s.
    fn flush_publish(&mut self, out: &mut AVec<'_, Effect<'a>>) {
        if self.publish.is_empty() {
            return;
        }
        for (name, input) in self.publish.take_all() {
            out.push(match input {
                Some(input) => Effect::Publish(input),
                None => Effect::Unpublish(name),
            });
        }
    }

    fn ran_batch(&mut self, batch: BatchId, results: &[Outcome<'_>], out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        if !self.batches.remove(&batch) {
            return Err(DriveError::UnknownBatch(batch));
        }
        self.profile.batches += 1;
        for result in results {
            match self.tickets.get(&result.ticket) {
                Some((_, of)) if *of == batch => {}
                _ => return Err(DriveError::UnknownTicket(result.ticket)),
            }
        }
        for result in results {
            let (name, _) = *self.tickets.get(&result.ticket).expect("checked above");
            self.profile.ran(name.kind, result.ns);
            if result.in_place > 0 {
                self.profile.of(name.kind).entries += result.in_place;
                self.profile.in_place += result.in_place;
                if let Some(job) = self.jobs.get_mut(&name) {
                    job.entries += result.in_place;
                }
            }
            self.ran(result.ticket, &result.ran, out)?;
        }
        if self.plant == Plant::BatchTwice {
            // The planted bug: the batch's records applied a second time, the check skipped.
            for result in results {
                if let Ran::Done(outputs) = result.ran {
                    for output in outputs {
                        self.made(output, out)?;
                    }
                }
            }
        }
        Ok(())
    }

    // --- Start and the certificate ------------------------------------------------------------

    fn start(
        &mut self,
        files: &[FileEntry<'_>],
        gone: &[&[u8]],
        last: Option<&[u8]>,
        units: ContentId,
        cold: bool,
        out: &mut AVec<'_, Effect<'a>>,
    ) -> Result<(), DriveError> {
        if files.windows(2).any(|w| w[0].path >= w[1].path) {
            return Err(DriveError::Unsorted { what: "the sweep's files" });
        }
        if !self.space_closed {
            return Err(DriveError::SpaceNotClosed);
        }
        for file in files {
            self.add_file(file.path, file.id, file.changed, file.manifest);
        }
        for path in gone {
            self.gone.push(Name::file(path).subject);
        }
        // The root units' source (drive.md, "Units"): what the workspace's layout read, as the
        // cli found it today (each manifest, each manifest probed absent, each listing).
        self.units_source = units;
        self.cold = cold;
        self.cert = self.cert_of(files.iter().map(|f| (f.path, &f.id)));
        self.last = match last {
            None => None,
            Some(bytes) => Some(answers::read_last(bytes).map_err(DriveError::LastFile)?),
        };
        if cold {
            // No leaf table: the files are not an answer's read set, so no certificate is keyed
            // by them. The one written at the finish is keyed by the files the answer read.
            return self.begin(out);
        }
        self.phase = Phase::Certifying;
        self.get(self.cert, Purpose::Cert, out);
        Ok(())
    }

    /// The certificate key of `files` (sorted by path): the tool, the command and its shape, the
    /// units' source, and each (path, id).
    fn cert_of<'p>(&self, files: impl Iterator<Item = (&'p [u8], &'p ContentId)>) -> Key {
        let mut command = [0u8; 65];
        command[0] = self.command.byte();
        command[1..33].copy_from_slice(&self.shape_digest.0);
        command[33..].copy_from_slice(&self.units_source.0);
        key::cert_key(self.config.tool, &command, files)
    }

    /// A file joins the table: `Start`'s, or an `Opened` one.
    fn add_file(&mut self, path: &[u8], id: ContentId, changed: bool, manifest: bool) {
        let name = Name::file(path);
        let at = u32::try_from(self.paths.len()).expect("the checkout's paths fit 4 GiB");
        self.paths.extend_from_slice(path);
        let len = u32::try_from(path.len()).expect("a path fits 4 GiB");
        let index = u32::try_from(self.files.len()).expect("the checkout's files fit u32");
        self.files.push(FileRow { subject: name.subject, id, path_at: at, path_len: len, changed, manifest });
        self.file_at.insert(name, index);
    }

    /// Whether the answer reads the file at `path`: it was in the last answer's read set and
    /// stands, or this run settled its job; and no home dropped it. A manifest is the layout's,
    /// held while it is. What the cli's leaf table keeps (drive.md, "The system side").
    pub fn holds(&self, path: &[u8]) -> bool {
        let name = Name::file(path);
        let Some(&index) = self.file_at.get(&name) else { return false };
        let i = index as usize;
        // A certificate's key is every file of the checkout by path and content: on a hit
        // (an undo back to a certified tree among them) each file it names is held as it is now,
        // changed since `last` or not, though nothing was walked. Dropping a changed one here
        // left it out of the leaf table, and the next request's walk met its part with no file
        // (`NoFile`, tempo, 2026-11-16).
        if self.files.manifests()[i] || self.profile.certified {
            return true;
        }
        if self.dropped.contains(&name) {
            return false;
        }
        self.now.contains_key(&name) || !self.files.changeds()[i]
    }

    /// The certificate missed: start the walk.
    fn begin(&mut self, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        self.phase = Phase::Working;
        let last = self.last.filter(|last| last.tool == self.config.tool.0);
        self.last = last;
        if let Some(last) = last {
            self.root_loading = true;
            self.get(last.root, Purpose::Root, out);
            self.old_findings_loading = true;
            self.get(last.cert, Purpose::OldCert, out);
            self.forward = last.shape == self.shape_digest;
        }
        if self.forward {
            // A changed or gone file seeds its outline. A seed no answer holds is outside the
            // build and goes (`locate`). A manifest is read by the layout alone: its change
            // moves the units' source, which re-makes the root units below, and the root's
            // record re-locates each member's units by its share.
            let mut seeds = Vec::new_in(self.alloc);
            let rows = self.files.subjects().iter().zip(self.files.changeds()).zip(self.files.manifests());
            for ((subject, changed), manifest) in rows {
                if *changed && !*manifest {
                    seeds.push(Name::new(Kind::FileItems, *subject));
                }
            }
            for subject in self.gone.iter() {
                seeds.push(Name::new(Kind::FileItems, *subject));
            }
            for name in seeds.iter() {
                self.add_job(*name, *name, None, true, self.walk)?;
            }
            // What the layout read moved: the units are read again (parts that stand cost no read).
            if self.last.is_some_and(|last| last.units != self.units_source) {
                self.add_root_units()?;
            }
        } else {
            self.add_root_units()?;
        }
        // The dependency graph, ahead of what reads it (unit_graph_task.rs): a graph whose
        // record stands costs one get; one the store lacks asks `Fetch` for what it reaches.
        if let Some(source) = self.graph.take() {
            let name = Name::unit_graph();
            self.add_job(name, name, Some(Locate::Source(source)), false, Class::Foreground)?;
        }
        Ok(())
    }

    /// The root units' job, located by today's source: never by its row, whose source is the
    /// last answer's.
    fn add_root_units(&mut self) -> Result<(), DriveError> {
        let units = Name::units();
        self.add_job(units, units, Some(Locate::Source(self.units_source)), false, Class::Foreground)
    }

    fn certified(&mut self, bytes: &[u8], out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        let (root, units, held) = read_cert(bytes, self.alloc).map_err(|error| DriveError::Wire { key: self.cert, error })?;
        self.profile.certified = true;
        // A `last` that names another answer (none, an older form's, or the tree's before an
        // undo) is replaced by this one's, so the next edit walks forward from this answer and
        // the leaf table written beside it matches the tree.
        let tool = self.config.tool.0;
        if self.last.is_none_or(|last| last.cert != self.cert || last.tool != tool || last.root != root) {
            let last = Last { tool, shape: self.shape_digest, units: self.units_source, cert: self.cert, root };
            out.push(Effect::WriteLast { bytes: answers::write_last(&last, self.alloc) });
        }
        let (count, errors) = self.print(&held, out);
        self.errors = errors;
        self.units_at = units;
        // The repeat: the walk stands whole. A stored error refuses the start at this one read
        // (every gate); a clean one starts the program now, nothing left to check beside it.
        self.held = Some(Answer { profile: self.profile, verdict: Verdict::Checked, findings: count, units, failed: 0 });
        self.phase = Phase::Held;
        if self.runs_program() && self.errors == 0 {
            match units {
                Some(at) => {
                    self.program = Program::Running;
                    out.push(Effect::StartProgram { units: at });
                }
                None => return Err(DriveError::NoProgram),
            }
        }
        self.conclude(out);
        Ok(())
    }

    // --- Events -------------------------------------------------------------------------------

    fn get(&mut self, key: Key, purpose: Purpose, out: &mut AVec<'_, Effect<'a>>) {
        match self.gets.get_mut(&key) {
            Some(purposes) => purposes.push(purpose),
            None => {
                let mut purposes = Vec::new_in(self.alloc);
                purposes.push(purpose);
                self.gets.insert(key, purposes);
                self.profile.gets += 1;
                out.push(Effect::Get { key });
            }
        }
    }

    fn put(&mut self, key: Key, bytes: Vec<u8, Alloc<'a>>, out: &mut AVec<'_, Effect<'a>>) {
        self.profile.puts += 1;
        out.push(Effect::Put { key, bytes });
    }

    fn got(&mut self, key: Key, bytes: Option<&[u8]>, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        let purposes = self.gets.remove(&key).ok_or(DriveError::UnaskedGet(key))?;
        if bytes.is_some() {
            self.profile.hits += 1;
        }
        for purpose in purposes.iter() {
            match *purpose {
                Purpose::Cert => match bytes {
                    Some(bytes) => return self.certified(bytes, out),
                    None => self.begin(out)?,
                },
                Purpose::OldCert => {
                    self.old_findings_loading = false;
                    if let Some(bytes) = bytes {
                        self.old_findings = read_cert(bytes, self.alloc).map_err(|error| DriveError::Wire { key, error })?.2;
                    }
                }
                Purpose::Root => {
                    self.root_loading = false;
                    // A root the store lost is an empty table: every job is made or found again.
                    self.root = match bytes {
                        Some(bytes) => Some(answers::read_root(bytes, self.alloc).map_err(|error| DriveError::Wire { key, error })?),
                        None => None,
                    };
                    if self.root.is_none() {
                        // No old table to walk forward from: walk the whole build from the
                        // units, as a first build does (the seeds find no row and go).
                        self.last = None;
                        if self.forward {
                            self.forward = false;
                            self.add_root_units()?;
                        }
                    }
                    let waiters = core::mem::replace(&mut self.root_waiters, Vec::new_in(self.alloc));
                    for name in waiters.iter() {
                        self.wake.push_back(*name);
                    }
                }
                Purpose::Page(index) => {
                    self.profile.pages += 1;
                    let page = match bytes {
                        Some(bytes) => answers::read_page(bytes, self.alloc).map_err(|error| DriveError::Wire { key, error })?,
                        None => SortedMap::new_in(self.alloc),
                    };
                    self.pages.insert(index, PageState::Loaded(page));
                    if let Some(waiters) = self.page_waiters.remove(&index) {
                        for name in waiters.iter() {
                            self.wake.push_back(*name);
                        }
                    }
                }
                Purpose::Look(name) => self.looked(name, key, bytes, out)?,
                Purpose::Value(name) => self.valued(name, key, bytes, out)?,
            }
        }
        Ok(())
    }

    fn ran(&mut self, ticket: Ticket, ran: &Ran<'_>, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        let (name, _) = self.tickets.remove(&ticket).ok_or(DriveError::UnknownTicket(ticket))?;
        if let Some(job) = self.jobs.get(&name)
            && job.entries > self.config.most_entries
        {
            let mut last_needs = [None; 4];
            for (slot, need) in last_needs.iter_mut().zip(job.needs.iter()) {
                *slot = Some(*need);
            }
            return Err(DriveError::Reentries { name, key: job.key, entries: job.entries, last_needs });
        }
        // A run of a name its home dropped while it was out answers for nothing in the build:
        // not an error, and not quietly kept (its record is not written, its row is gone).
        if self.dropped.contains(&name) {
            return Ok(());
        }
        match *ran {
            // Rule 20: the job settles with no record (a read of it sees none), its waiters go
            // on, and the answer is not written (`finish`), so the next command runs it again.
            Ran::Failed(failure) => {
                self.failures.push(failure);
                self.now.remove(&name);
                self.set_state(name, State::Settled);
                self.wake_waiters(name);
                Ok(())
            }
            Ran::Cfg(var) => {
                self.profile.of(name.kind).needs += 1;
                self.split(name, var, out)
            }
            Ran::Done(outputs) => {
                for output in outputs {
                    // Its own record, or a member's of the group it ran, not yet settled.
                    let ours = output.name == name
                        || self.jobs.get(&name).is_some_and(|by| by.members.contains(&output.name));
                    let open = self.jobs.get(&output.name).is_some_and(|job| job.state != State::Settled);
                    let asked = ours && open;
                    if !asked {
                        return Err(DriveError::UnaskedOutput { name: output.name, by: name });
                    }
                    self.made(output, out)?;
                }
                if name.kind == Kind::Group {
                    // A group is no record of its own: its members are.
                    self.set_state(name, State::Settled);
                    self.wake_waiters(name);
                }
                Ok(())
            }
            Ran::Need(wants) => {
                self.profile.of(name.kind).needs += 1;
                for want in wants {
                    if want.name == name {
                        return Err(DriveError::SelfNeed { name });
                    }
                    if !self.jobs.contains_key(&want.name) {
                        let class = self.jobs.get(&name).map_or(self.walk, |job| job.class);
                        // A name whose subject is its source is located by it, never by a home.
                        let locate = want.name.located_by_subject().then_some(Locate::Source(want.name.subject));
                        self.add_job(want.name, want.home, locate, false, class)?;
                    } else if self.plant == Plant::DoubleFlight
                        && self.jobs.get(&want.name).is_some_and(|job| job.state == State::Running)
                    {
                        let mut tasks = Vec::new_in(self.alloc);
                        tasks.push(self.emit(want.name));
                        self.send(tasks, out);
                    }
                    self.jobs.get_mut(&name).expect("a running job's ticket names it").needs.push(want.name);
                }
                // Sorted once per answer, not per want: O(d log d) for d needs.
                let job = self.jobs.get_mut(&name).expect("a running job's ticket names it");
                job.needs.sort_unstable();
                job.needs.dedup();
                self.set_state(name, State::Gather);
                self.wake.push_back(name);
                Ok(())
            }
        }
    }

    // --- Jobs ---------------------------------------------------------------------------------

    fn add_job(&mut self, name: Name, home: Name, locate: Option<Locate>, seed: bool, class: Class) -> Result<(), DriveError> {
        if self.jobs.contains_key(&name) {
            return Ok(());
        }
        if self.jobs.len() as u64 >= u64::from(self.config.most_jobs) {
            return Err(DriveError::TooManyJobs { most: self.config.most_jobs, name });
        }
        let a = self.alloc;
        self.jobs.insert(
            name,
            Job {
                state: State::Start,
                home,
                place: 0,
                locate,
                source: ABSENT,
                key: Key::default(),
                class,
                seed,
                recheck: false,
                redo: false,
                entries: 0,
                settles: 0,
                queue_gen: 0,
                parked: false,
                needs: Vec::new_in(a.clone()),
                shape: Vec::new_in(a.clone()),
                waiters: Vec::new_in(a.clone()),
                members: Vec::new_in(a),
            },
        );
        if name.kind == Kind::Units {
            self.units_jobs.push(name);
        }
        self.wake.push_back(name);
        Ok(())
    }

    /// A `Units` job not settled yet: what lists files may still list a file wanted now.
    fn units_open(&self) -> Option<Name> {
        self.units_jobs.iter().copied().find(|units| self.state(*units).is_some_and(|s| s != State::Settled))
    }

    /// Every state change goes here, so the settled count is kept without a scan.
    fn set_state(&mut self, name: Name, state: State) {
        if let Some(job) = self.jobs.get_mut(&name) {
            match (job.state == State::Settled, state == State::Settled) {
                (false, true) => self.settled += 1,
                (true, false) => self.settled -= 1,
                _ => {}
            }
            job.state = state;
        }
    }

    fn state(&self, name: Name) -> Option<State> {
        self.jobs.get(&name).map(|job| job.state)
    }

    // --- Conditions (drive.md, "One check covers every configuration") ----------------------

    fn space(&self) -> Space<'_> {
        Space { values: &self.space_values, implications: &self.space_implications }
    }

    /// The item a job is about and the condition it runs under: a fork's from this run's
    /// splits or its row, every other name its own base under [`Condition::ANY`].
    fn cond_of(&self, name: Name) -> (Name, Condition) {
        if let Some(at) = self.conditioned.get(&name) {
            return *at;
        }
        match self.current(name).and_then(|row| row.cond) {
            Some(at) => at,
            None => (name, Condition::ANY),
        }
    }

    /// The forks of `name`, which split on `var`: one per value the space allows under its
    /// condition, sorted by name, each registered with its base and condition.
    fn forks(&mut self, name: Name, var: Var) -> Result<Vec<(Name, Condition), Alloc<'a>>, DriveError> {
        let (base, under) = self.cond_of(name);
        if under.get(var).is_some() {
            return Err(DriveError::SplitOnFixed { name, var });
        }
        let mut forks = Vec::new_in(self.alloc);
        for fork in self.space().fork(&under, var).map_err(|error| DriveError::Condition { name: base, error })? {
            forks.push((key::at_condition(base, &fork), fork));
        }
        forks.sort_unstable_by_key(|(fork, _)| *fork);
        for (fork, condition) in forks.iter() {
            self.conditioned.insert(*fork, (base, *condition));
        }
        Ok(forks)
    }

    /// One step down a split: the fork of `at` the condition `under` implies, or the variable
    /// it does not fix.
    fn fork_of(&self, at: Name, var: Var, under: &Condition) -> Result<Result<Name, Var>, DriveError> {
        let Some(value) = under.get(var) else { return Ok(Err(var)) };
        let (base, path) = self.cond_of(at);
        let next = path
            .with(var, value)
            .map_err(|error| DriveError::Condition { name: base, error })?
            .ok_or(DriveError::SplitOnFixed { name: at, var })?;
        Ok(Ok(key::at_condition(base, &next)))
    }

    /// A lookup by implication: from `need` down the splits on the way to the record whose
    /// condition `under` implies. A split's forks are its parts, so each is a job once the split
    /// settles; at most [`MOST_ATOMS`] splits lie on one way (each fixes one more variable).
    fn resolve(
        &mut self,
        need: Name,
        waiter: Name,
        under: &Condition,
        out: &mut AVec<'_, Effect<'a>>,
    ) -> Result<Resolved, DriveError> {
        let mut at = need;
        for _ in 0..=MOST_ATOMS {
            if let Digest::Wait = self.digest(at, waiter, out) {
                return Ok(Resolved::Wait);
            }
            let Some(var) = self.current(at).and_then(|row| row.split) else { return Ok(Resolved::At) };
            match self.fork_of(at, var, under)? {
                Ok(next) => at = next,
                // The planted bug: an asker that does not fix the variable is served the first
                // fork, a record whose condition it does not imply.
                Err(var) if self.plant == Plant::RanUnderCondition => match self.forks(at, var)?.first() {
                    Some((first, _)) => at = *first,
                    None => return Ok(Resolved::Fork(var)),
                },
                Err(var) => return Ok(Resolved::Fork(var)),
            }
        }
        let (base, _) = self.cond_of(need);
        Err(DriveError::Condition { name: base, error: ConditionError::TooManyAtoms { var: MOST_ATOMS as Var } })
    }

    /// [`Request::resolve`] over what is settled now, with no wait: where a task's input or read
    /// of `need` leads. Where the way is not settled (it changed since the task was handed its
    /// inputs) it stops there, and the read's digest is then not today's (`made` marks it stale).
    fn resolve_settled(&self, need: Name, under: &Condition) -> Name {
        let mut at = need;
        for _ in 0..=MOST_ATOMS {
            let Some(var) = self.current(at).and_then(|row| row.split) else { return at };
            match self.fork_of(at, var, under) {
                Ok(Ok(next)) => at = next,
                Ok(Err(var)) if self.plant == Plant::RanUnderCondition => {
                    let (base, path) = self.cond_of(at);
                    let first = self.space().fork(&path, var).ok().and_then(|mut forks| forks.next());
                    match first {
                        Some(first) => at = key::at_condition(base, &first),
                        None => return at,
                    }
                }
                _ => return at,
            }
        }
        at
    }

    /// The job read `var` unfixed: its record is a split, read-addressed by the space (its forks
    /// are what the space allows), whose parts are the job once per allowed value. Past the cap
    /// its record is instead the typed finding ([`ConditionError::OverCap`]), and the forks are
    /// not made.
    fn split(&mut self, name: Name, var: Var, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        let (_, under) = self.cond_of(name);
        let span = self.space().span(&under, var);
        let space_read = [(self.space_name, self.space_digest)];
        if span > MOST_CONDITIONS {
            let mut message = [0u8; MESSAGE_BYTES];
            let len = over_cap_message(&mut message, span, &under, var);
            let findings = [Finding { rule: OVER_CAP_RULE, start: 0, end: 0, message: &message[..len] }];
            let output = Output {
                name,
                out: key::digest("blaze-drive over cap 1", &message[..len]),
                reads: &space_read,
                parts: &[],
                paths: &[],
                findings: &findings,
                payload: &[],
            };
            return self.made(&output, out);
        }
        let job = self.jobs.get(&name).expect("a split job is a job");
        let locate = job.locate.expect("a job runs once located");
        let place = job.place;
        let forks = self.forks(name, var)?;
        let mut parts = Vec::with_capacity_in(forks.len(), self.alloc);
        let mut h = Hasher::new("blaze-drive split out 1");
        h.update(&var.to_le_bytes());
        for (fork, _) in forks.iter() {
            parts.push(Part { name: *fork, locate, place });
            h.update(&[fork.kind as u8]).id(&fork.subject);
        }
        let output = Output { name, out: h.finish(), reads: &space_read, parts: &parts, paths: &[], findings: &[], payload: &[] };
        self.made_split(&output, Some(var), out)
    }

    /// Whether `waiter` may register a wait now: once per advance (`Job::parked`). A name with
    /// no job (the cli's own) always may.
    fn park(&mut self, waiter: Name) -> bool {
        match self.jobs.get_mut(&waiter) {
            Some(job) if job.parked => false,
            Some(job) => {
                job.parked = true;
                true
            }
            None => true,
        }
    }

    fn wait_on(&mut self, target: Name, waiter: Name) {
        if !self.jobs.contains_key(&target) || !self.park(waiter) {
            return;
        }
        if let Some(job) = self.jobs.get_mut(&target) {
            job.waiters.push(waiter);
        }
    }

    fn wake_waiters(&mut self, name: Name) {
        let Some(job) = self.jobs.get_mut(&name) else { return };
        let waiters = core::mem::replace(&mut job.waiters, Vec::new_in(self.alloc));
        for waiter in waiters.iter() {
            self.wake.push_back(*waiter);
        }
    }

    /// The machine's own bookkeeping so far: probes of every table, ring entries skipped, wakes
    /// and waits (the linearity test's count; parallel.md change 3).
    pub fn bookkeeping(&self) -> u64 {
        self.jobs.ops()
            + self.now.ops()
            + self.new.ops()
            + self.dropped.ops()
            + self.part_locs.ops()
            + self.posted.ops()
            + self.gets.ops()
            + self.tickets.ops()
            + self.publish.ops()
            + self.findings.ops()
            + self.pages.ops()
            + self.page_waiters.ops()
            + self.ready_ops
    }

    /// Whether the old table is known for `name`: `Some(row or none)` when it is, `None` when
    /// `waiter` now waits for the root or the page.
    fn old_ready(&mut self, name: Name, waiter: Name, out: &mut AVec<'_, Effect<'a>>) -> bool {
        if self.last.is_none() {
            return true;
        }
        if self.root.is_none() {
            if self.park(waiter) {
                self.root_waiters.push(waiter);
            }
            return false;
        }
        let Some(root) = &self.root else { return false };
        let index = answers::page_of(name, root.bits);
        match self.pages.get(&index) {
            Some(PageState::Loaded(_)) => true,
            Some(PageState::Loading) => {
                self.page_wait(index, waiter);
                false
            }
            None => {
                let key = root.pages[index as usize];
                self.pages.insert(index, PageState::Loading);
                self.page_wait(index, waiter);
                match key == Key::default() {
                    true => {
                        self.pages.insert(index, PageState::Loaded(SortedMap::new_in(self.alloc)));
                        self.wake.push_back(waiter);
                    }
                    false => self.get(key, Purpose::Page(index), out),
                }
                false
            }
        }
    }

    fn page_wait(&mut self, index: u32, waiter: Name) {
        if !self.park(waiter) {
            return;
        }
        match self.page_waiters.get_mut(&index) {
            Some(list) => list.push(waiter),
            None => {
                let mut list = Vec::new_in(self.alloc);
                list.push(waiter);
                self.page_waiters.insert(index, list);
            }
        }
    }

    /// The old row of `name`, its page loaded; a watch row (readers only) is no row.
    fn old(&self, name: Name) -> Option<&Row<'a>> {
        self.old_or_watch(name).filter(|row| !row.watching())
    }

    /// The old row of `name` or its watch row ([`Row::watching`]): for its readers.
    fn old_or_watch(&self, name: Name) -> Option<&Row<'a>> {
        let root = self.root.as_ref()?;
        match self.pages.get(&answers::page_of(name, root.bits))? {
            PageState::Loaded(page) => page.get(&name),
            PageState::Loading => None,
        }
    }

    /// The row this run knows for `name`: made this run, else the old one.
    fn current(&self, name: Name) -> Option<&Row<'a>> {
        match self.new.get(&name) {
            Some(row) => (!row.watching()).then_some(row),
            None => self.old(name),
        }
    }

    /// What a read of `read` sees now, or that `waiter` waits for it.
    fn digest(&mut self, read: Name, waiter: Name, out: &mut AVec<'_, Effect<'a>>) -> Digest {
        if read == self.space_name {
            return Digest::Ready(Some(Now { at: Key::default(), out: self.space_digest }));
        }
        if self.dropped.contains(&read) {
            return Digest::Ready(None);
        }
        if let Some(state) = self.state(read) {
            return match (state, self.now.get(&read).copied()) {
                (State::Settled, Some(now)) => Digest::Ready(Some(now)),
                (State::Settled, None) => Digest::Ready(None),
                _ => {
                    self.wait_on(read, waiter);
                    Digest::Wait
                }
            };
        }
        if !self.old_ready(read, waiter, out) {
            return Digest::Wait;
        }
        Digest::Ready(self.old(read).map(|row| Now { at: row.at, out: row.out }))
    }

    fn file_index(&self, subject: &ContentId) -> Option<usize> {
        self.file_at.get(&Name::new(Kind::FileItems, *subject)).map(|i| *i as usize)
    }

    fn pump(&mut self, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        loop {
            while let Some(name) = self.wake.pop_front() {
                self.ready_ops += 1;
                self.advance(name, out)?;
            }
            self.emit_ready(out);
            if self.phase == Phase::Working && self.quiet() {
                if self.walk_quiet() {
                    continue;
                }
                if self.unsettled() {
                    self.break_cycle()?;
                    continue;
                }
                self.phase = Phase::Finishing;
            }
            if self.phase == Phase::Finishing && self.gets.is_empty() {
                self.finish(out)?;
            }
            self.flush_publish(out);
            return Ok(());
        }
    }

    fn quiet(&self) -> bool {
        self.wake.is_empty() && self.gets.is_empty() && self.tickets.is_empty() && self.queued == 0 && self.opening.is_empty()
    }

    /// The walk is quiet: every record found or made so far has listed its parts and each part
    /// has a job (`settle`), so a name with no job now is listed by nothing today. The jobs that
    /// waited for this ([`Request::value`], `unwalked`) are woken to look up their value with
    /// such a read as `ABSENT`. `true` when one was woken. Each wake asks for one value key, and
    /// a job comes back only by being dirtied (capped by its settles), so this ends.
    fn walk_quiet(&mut self) -> bool {
        let waiting = core::mem::replace(&mut self.unwalked, Vec::new_in(self.alloc));
        let mut woke = false;
        for name in waiting.iter() {
            if self.state(*name) != Some(State::Value) {
                continue;
            }
            if let Err(at) = self.walked.binary_search(name) {
                self.walked.insert(at, *name);
                self.wake.push_back(*name);
                woke = true;
            }
        }
        woke
    }

    /// `waiter` waits on the file `file`, opening it if no one has yet: `Open` is emitted once.
    /// `false` when the path is not known (no part this run gave one).
    fn wait_on_open(&mut self, file: Name, waiter: Name, out: &mut AVec<'_, Effect<'a>>) -> bool {
        let Some(&(at, len, module)) = self.part_paths.get(&file) else { return false };
        if !self.opening.contains_key(&file) {
            let path = Vec::from_slice_in(&self.paths[at as usize..(at + len) as usize], self.alloc);
            out.push(Effect::Open { name: file, path, module });
            self.opening.insert(file, Vec::new_in(self.alloc));
            self.profile.opens += 1;
        }
        if self.park(waiter) {
            self.opening.get_mut(&file).expect("inserted above").push(waiter);
        }
        true
    }

    /// Queue a job to run: its class's ring of its kind, at the back.
    fn queue(&mut self, class: Class, name: Name) {
        self.unqueue(name);
        let Some(job) = self.jobs.get_mut(&name) else { return };
        job.queue_gen = job.queue_gen.wrapping_add(1);
        job.class = class;
        let r = ring(class, name.kind);
        self.ready[r].push_back((name, job.queue_gen));
        self.ready_live[r] += 1;
        self.queued += 1;
    }

    /// Take the job out of the queue if it is in it: its entry goes stale, skipped when reached.
    fn unqueue(&mut self, name: Name) {
        let Some(job) = self.jobs.get_mut(&name) else { return };
        // An odd generation is in a ring; even is out.
        if job.queue_gen & 1 == 0 {
            return;
        }
        job.queue_gen = job.queue_gen.wrapping_add(1);
        self.ready_live[ring(job.class, name.kind)] -= 1;
        self.queued -= 1;
    }

    /// The next live entry of ring `r`, stale ones dropped.
    fn pop_ready(&mut self, r: usize) -> Option<Name> {
        while let Some((name, generation)) = self.ready[r].pop_front() {
            self.ready_ops += 1;
            let Some(job) = self.jobs.get_mut(&name) else { continue };
            if job.queue_gen == generation {
                job.queue_gen = job.queue_gen.wrapping_add(1);
                self.ready_live[r] -= 1;
                self.queued -= 1;
                if job.state == State::Queued {
                    return Some(name);
                }
            }
        }
        None
    }

    /// This run's mean time of a task of `kind`, or the default before one is measured.
    fn task_ns(&self, kind: Kind) -> u64 {
        let k = self.profile.kinds[kind.index()];
        match (k.ns, k.entries) {
            (ns, entries) if ns > 0 && entries > 0 => (ns / u64::from(entries)).max(1),
            _ => DEFAULT_TASK_NS,
        }
    }

    fn unsettled(&self) -> bool {
        self.jobs.len() > self.settled
    }

    /// Batches out while workers are free: every foreground task before any background one
    /// (the rings are in class order, so a background batch goes only when no foreground task
    /// is queued). A batch is the next tasks of one class and kind in queue order (a home's
    /// parts are queued together, so mostly one home's), as
    /// many as make [`TARGET_BATCH_NS`] at this run's mean time of the kind, and no more than
    /// an even share of that kind's queue over the free workers, so no worker idles while
    /// another holds a long batch.
    fn emit_ready(&mut self, out: &mut AVec<'_, Effect<'a>>) {
        let workers = self.config.workers.max(1) as usize;
        while self.batches.len() < workers {
            // The first ring with a live entry: at most `RINGS` (33) looks, a constant.
            let Some(r) = (0..RINGS).find(|r| self.ready_live[*r] > 0) else { return };
            let kind = Kind::ALL[r % crate::key::KINDS];
            let free = workers - self.batches.len();
            let of_kind = self.ready_live[r] as usize;
            let by_time = usize::try_from(TARGET_BATCH_NS / self.task_ns(kind)).unwrap_or(MOST_BATCH);
            let size = by_time.clamp(1, MOST_BATCH).min(of_kind.div_ceil(free)).max(1);
            let mut tasks = Vec::with_capacity_in(size, self.alloc);
            while tasks.len() < size {
                let Some(name) = self.pop_ready(r) else { break };
                tasks.push(self.emit(name));
            }
            if tasks.is_empty() {
                // The live count said an entry was there: the counts are the machine's bug, and
                // the request stalls visibly (`quiet` stays false) instead of spinning here.
                return;
            }
            self.send(tasks, out);
        }
    }

    fn send(&mut self, tasks: Vec<(Ticket, Task<'a>), Alloc<'a>>, out: &mut AVec<'_, Effect<'a>>) {
        let id = BatchId(self.next_batch);
        self.next_batch += 1;
        for (ticket, _) in tasks.iter() {
            if let Some(entry) = self.tickets.get_mut(ticket) {
                entry.1 = id;
            }
        }
        self.batches.insert(id);
        out.push(Effect::Run { batch: Batch { id, tasks } });
    }

    fn emit(&mut self, name: Name) -> (Ticket, Task<'a>) {
        let ticket = Ticket(self.next_ticket);
        self.next_ticket += 1;
        let a = self.alloc;
        let mut inputs = Vec::new_in(a.clone());
        let mut failed = Vec::new_in(a.clone());
        let mut members = Vec::new_in(a.clone());
        let (base, condition) = self.cond_of(name);
        let job = self.jobs.get(&name).expect("a queued job is a job");
        for need in job.needs.iter() {
            // Handed by its base name, with what the record the task's condition implies
            // holds (`gather` waited for every node on the way).
            let leaf = self.resolve_settled(*need, &condition);
            match self.now.get(&leaf).copied().or_else(|| self.old(leaf).map(|row| Now { at: row.at, out: row.out })) {
                Some(now) => inputs.push(Input { name: self.cond_of(*need).0, at: now.at, out: now.out }),
                // Settled with no record: its task failed this command (rule 20).
                None => failed.push(self.cond_of(*need).0),
            }
        }
        inputs.sort_unstable_by_key(|input| input.name);
        inputs.dedup_by_key(|input| input.name);
        failed.sort_unstable();
        failed.dedup();
        members.extend_from_slice(&job.members);
        // A file's own, or an item's home file's: an item's task (a body's check) reads its
        // text in the edition its file is read in.
        let file_edition = |locate: Option<Locate>| match locate {
            Some(Locate::File { edition }) => Some(edition),
            _ => None,
        };
        let edition = file_edition(job.locate).or_else(|| self.jobs.get(&job.home).and_then(|home| file_edition(home.locate))).unwrap_or(0);
        // A file's outline and a manifest's units read their file; the root units read the root
        // manifest.
        let file = base.text_file().and_then(|file| self.file_index(&file.subject)).map(|i| i as u32);
        let task = Task {
            name,
            base,
            condition,
            key: job.key,
            source: job.source,
            class: job.class,
            entry: job.entries,
            home: job.home,
            place: job.place,
            file,
            edition,
            inputs,
            failed,
            members,
        };
        let job = self.jobs.get_mut(&name).expect("a queued job is a job");
        job.entries += 1;
        job.state = State::Running;
        self.profile.of(name.kind).entries += 1;
        self.profile.tasks += 1;
        self.tickets.insert(ticket, (name, BatchId(u64::MAX)));
        (ticket, task)
    }

    fn advance(&mut self, name: Name, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        let Some(job) = self.jobs.get_mut(&name) else { return Ok(()) };
        job.parked = false;
        let state = job.state;
        match state {
            State::Start => self.locate(name, out),
            State::Decide => self.decide(name, out),
            State::Value => self.value(name, out),
            State::Gather => self.gather(name, out),
            State::Looking | State::Valuing | State::Queued | State::Running | State::Settled => Ok(()),
        }
    }

    /// Start: find the job's source and key.
    fn locate(&mut self, name: Name, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        if self.dropped.contains(&name) {
            self.gone_job(name);
            return Ok(());
        }
        if !self.old_ready(name, name, out) {
            return Ok(());
        }
        let job = self.jobs.get(&name).expect("advanced jobs are jobs");
        let (home, seed) = (job.home, job.seed);
        let mut locate = job.locate;
        let mut place = job.place;
        if locate.is_none()
            && let Some((found, at, by)) = self.part_locs.get(&name).copied()
        {
            locate = Some(found);
            place = at;
            self.jobs.get_mut(&name).expect("a job").home = by;
        }
        if locate.is_none() {
            // Not listed by a home this run: wait for a home being made, else take the row.
            if home != name && self.state(home).is_some_and(|s| s != State::Settled) {
                self.wait_on(home, name);
                return Ok(());
            }
            // Its home failed this command (rule 20): it lists nothing today, and a row of last
            // time's text is not today's. The part fails `Waited` on it, alone: its waiters see
            // it in `Task::failed` and fail in turn; the request goes on.
            if home != name && self.state(home) == Some(State::Settled) && self.failures.iter().any(|failure| failure.name == home) {
                self.failures.push(TaskFailure { name, why: Why::Waited { on: home } });
                self.now.remove(&name);
                self.set_state(name, State::Settled);
                self.wake_waiters(name);
                return Ok(());
            }
            match self.old(name).map(|row| (row.source, row.place, row.home)) {
                Some((source, at, by)) => {
                    // A file's text is found by today's sweep, never by the row's source.
                    locate = Some(match name.kind {
                        Kind::FileItems => Locate::File { edition: at as u8 },
                        Kind::Units if name == Name::units() => Locate::Source(self.units_source),
                        // A member's units are found by their share of the layout, which the
                        // root's record lists ([`Locate::Source`]): the row's is the last one's.
                        _ => Locate::Source(source),
                    });
                    place = at;
                    self.jobs.get_mut(&name).expect("a job").home = by;
                }
                None if seed => {
                    // A changed file no answer holds: outside the build, until a module lists it.
                    // It is in `Start`, so the settled count is untouched.
                    self.jobs.remove(&name);
                    return Ok(());
                }
                None if home != name && self.state(home).is_none() => {
                    let class = self.jobs.get(&name).map_or(self.walk, |job| job.class);
                    self.add_job(home, home, None, false, class)?;
                    self.wait_on(home, name);
                    return Ok(());
                }
                // A workspace's member units list their files after the root lists them: a file
                // wanted before then waits for them; with every units settled it is no part.
                None => match self.units_open() {
                    Some(units) => {
                        self.wait_on(units, name);
                        return Ok(());
                    }
                    // Its home settled today and lists no such part (a module file at both its
                    // places has no `Surface`, though a resolver read one of them): the want fails
                    // alone, by name (rule 20); its waiters see it in `Task::failed`.
                    None if home != name && self.state(home) == Some(State::Settled) => {
                        self.failures.push(TaskFailure { name, why: Why::Refused { what: "a part its file's record does not list", rule: "keys-cover-every-input" } });
                        self.now.remove(&name);
                        self.set_state(name, State::Settled);
                        self.wake_waiters(name);
                        return Ok(());
                    }
                    None => return Err(DriveError::NoPart { name, home }),
                },
            }
        }
        let locate = locate.expect("found above");
        let (source, key) = match locate {
            Locate::Source(source) => (source, key::task_key(self.config.tool, name, &source)),
            Locate::Key(key) => (key.0, key),
            // A fork of a file's task reads its base's file.
            Locate::File { edition } => match self.file_index(&self.cond_of(name).0.subject) {
                Some(index) => {
                    let source = key::file_source(self.files.ids()[index], edition);
                    (source, key::task_key(self.config.tool, name, &source))
                }
                // A file the request does not hold yet, its path named by a part: open it.
                None if !self.absent_files.contains(&Name::new(Kind::FileItems, self.cond_of(name).0.subject))
                    && self.wait_on_open(Name::new(Kind::FileItems, self.cond_of(name).0.subject), name, out) =>
                {
                    return Ok(());
                }
                // A file gone from the checkout: an error only if its home, decided this run,
                // still lists it; a home re-made without it drops it first.
                None => {
                    let home = self.jobs.get(&name).map_or(home, |job| job.home);
                    // A part its settled home lists, opened and found at neither place: a
                    // finding at its `mod` item; the rest of the crate goes on.
                    let file = Name::new(Kind::FileItems, self.cond_of(name).0.subject);
                    if home != name && self.state(home) == Some(State::Settled) && self.absent_files.contains(&file) {
                        return self.absent_module(name, file, edition, place, out);
                    }
                    return match self.state(home) {
                        Some(State::Settled) => Err(DriveError::NoFile { file: name, home }),
                        Some(_) => {
                            self.wait_on(home, name);
                            Ok(())
                        }
                        None if home == name => Err(DriveError::NoFile { file: name, home }),
                        None => {
                            self.add_job(home, home, None, false, self.walk)?;
                            self.wait_on(home, name);
                            Ok(())
                        }
                    };
                }
            },
        };
        let job = self.jobs.get_mut(&name).expect("a job");
        job.locate = Some(locate);
        job.place = place;
        job.source = source;
        job.key = key;
        job.state = State::Decide;
        self.decide(name, out)
    }

    /// A module file its settled home lists, opened and found at neither of its places: by the
    /// Reference ("Modules", "Module source filenames") a `mod x;` is read from `x.rs` or else
    /// `x/mod.rs` (a `#[path]` from its one path). Its record is one finding, placed at the `mod`
    /// item in the home's file, naming the places looked at; no request error, so the rest of
    /// the crate is still checked. Keyed by those paths: creating the file gives the part a
    /// file's key, and a reader that looked for it read it absent (`key::ABSENT`), so reruns.
    fn absent_module(&mut self, name: Name, file: Name, edition: u8, place: u32, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        let home = self.jobs.get(&name).expect("a job").home;
        let Some(&(at, len, module)) = self.part_paths.get(&file) else {
            return Err(DriveError::NoFile { file: name, home });
        };
        let path = &self.paths[at as usize..(at + len) as usize];
        let mut message = Vec::with_capacity_in(2 * path.len() + 8, self.alloc);
        message.extend_from_slice(path);
        let rule = match module {
            true => {
                message.push(0x1f);
                message.extend_from_slice(path.strip_suffix(b".rs").unwrap_or(path));
                message.extend_from_slice(b"/mod.rs");
                MODULE_FILE_MISSING_RULE
            }
            false => PATH_FILE_MISSING_RULE,
        };
        let source = key::digest("blaze-drive absent module file 1", &message);
        let job = self.jobs.get_mut(&name).expect("a job");
        job.locate = Some(Locate::File { edition });
        job.place = place;
        job.source = source;
        job.key = key::task_key(self.config.tool, name, &source);
        let findings = [Finding { rule, start: 0, end: 0, message: &message }];
        let output = Output { name, out: source, reads: &[], parts: &[], paths: &[], findings: &findings, payload: &[] };
        self.made(&output, out)
    }

    fn decide(&mut self, name: Name, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        let job = self.jobs.get(&name).expect("a job");
        let (key, recheck) = (job.key, job.recheck);
        if name.kind == Kind::Group {
            self.set_state(name, State::Queued);
            self.queue(self.walk, name);
            return Ok(());
        }
        // A split (or an over-cap finding) read the space: under another space its forks are
        // others, so it stands only on the same space. No reader of the space is posted (it is
        // no job), so this is where that read is checked.
        let (space_name, space_digest) = (self.space_name, self.space_digest);
        let row = self.current(name).map(|row| {
            let other_space = row
                .reads
                .binary_search_by_key(&space_name, |(n, _)| *n)
                .is_ok_and(|at| row.reads[at].1 != space_digest);
            (row.key, other_space, row.reads.iter().map(|(n, _)| *n).collect_in(self.alloc))
        });
        match row {
            Some((held, false, _)) if held == key && !recheck => self.stand(name, out),
            Some((held, _, reads)) if held == key && !reads.is_empty() => {
                let job = self.jobs.get_mut(&name).expect("a job");
                job.shape = reads;
                job.state = State::Value;
                self.value(name, out)
            }
            _ => {
                self.set_state(name, State::Looking);
                self.get(key, Purpose::Look(name), out);
                Ok(())
            }
        }
    }

    /// Compute the value key from today's digests of the shape's reads, waiting for any not
    /// settled, then look it up.
    fn value(&mut self, name: Name, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        let shape = core::mem::replace(&mut self.jobs.get_mut(&name).expect("a job").shape, Vec::new_in(self.alloc));
        let mut reads = Vec::with_capacity_in(shape.len(), self.alloc);
        let mut waiting = false;
        // A shape with no row of this checkout behind it is another checkout's (a second
        // worktree's cold command) or an unanswered command's: no watch row here says which of
        // its reads were absent. A read nothing has listed YET is then not known absent: the
        // file that lists it may still be on its way through the walk, and a value key made
        // with `ABSENT` for it misses the record the store holds. Such a job waits for the walk
        // to go quiet ([`Request::walk_quiet`]); only then is an unlisted name absent today.
        let unanswered = self.current(name).is_none() && self.walked.binary_search(&name).is_err();
        let mut unlisted = false;
        for read in shape.iter() {
            match self.digest(*read, name, out) {
                Digest::Ready(None) if unanswered && *read != self.space_name && self.state(*read).is_none() && !self.dropped.contains(read) => unlisted = true,
                Digest::Ready(now) => reads.push((*read, now.map_or(ABSENT, |now| now.out))),
                Digest::Wait => waiting = true,
            }
        }
        let job = self.jobs.get_mut(&name).expect("a job");
        job.shape = shape;
        if waiting {
            job.state = State::Value;
            return Ok(());
        }
        if unlisted {
            job.state = State::Value;
            self.unwalked.push(name);
            return Ok(());
        }
        let at = key::value_key(job.key, &reads);
        job.state = State::Valuing;
        if let Ok(released) = self.walked.binary_search(&name) {
            self.walked.remove(released);
        }
        self.get(at, Purpose::Value(name), out);
        Ok(())
    }

    fn looked(&mut self, name: Name, key: Key, bytes: Option<&[u8]>, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        if self.state(name) != Some(State::Looking) {
            return Ok(());
        }
        let Some(bytes) = bytes else { return self.to_run(name, out) };
        match record::open(bytes).map_err(|error| DriveError::Wire { key, error })? {
            Stored::Whole(view) => self.found(name, key, view, out),
            Stored::Shape(shape) => {
                if shape.name != name {
                    return Err(DriveError::WrongRecord { key, asked: name, held: shape.name });
                }
                let mut names = Vec::with_capacity_in(shape.names().len(), self.alloc);
                names.extend(shape.names());
                let job = self.jobs.get_mut(&name).expect("a job");
                job.shape = names;
                job.state = State::Value;
                self.value(name, out)
            }
        }
    }

    fn valued(&mut self, name: Name, key: Key, bytes: Option<&[u8]>, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        if self.state(name) != Some(State::Valuing) {
            return Ok(());
        }
        let Some(bytes) = bytes else { return self.to_run(name, out) };
        match record::open(bytes).map_err(|error| DriveError::Wire { key, error })? {
            Stored::Whole(view) => self.found(name, key, view, out),
            Stored::Shape(shape) => Err(DriveError::WrongRecord { key, asked: name, held: shape.name }),
        }
    }

    /// The record at `at` was found: it is the job's answer.
    fn found(&mut self, name: Name, at: Key, view: WholeView<'_>, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        if view.name != name {
            return Err(DriveError::WrongRecord { key: at, asked: name, held: view.name });
        }
        let fetched = matches!(self.jobs.get(&name).and_then(|job| job.locate), Some(Locate::Key(_)));
        match fetched {
            true => self.profile.of(name.kind).fetched += 1,
            false => self.profile.of(name.kind).found += 1,
        }
        self.settle(name, Some((at, view)), false, out)
    }

    /// No record: hand the task its hints and run it.
    fn to_run(&mut self, name: Name, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        let job = self.jobs.get(&name).expect("a job");
        if let Some(Locate::Key(key)) = job.locate {
            return Err(DriveError::Missing { name, key, builder: Builder::Pack { krate: job.home } });
        }
        // A read that found nothing is no hint: the task asks for it again if it still looks,
        // and a name that is not there would fail the run that was handed it.
        let hints = self.current(name).map(|row| row.reads.iter().filter(|(_, saw)| *saw != ABSENT || self.plant == Plant::HintAbsentReads).map(|(n, _)| *n).collect_in(self.alloc));
        if let Some(hints) = hints {
            let job = self.jobs.get_mut(&name).expect("a job");
            job.needs.extend_from_slice(&hints);
            job.needs.sort_unstable();
            job.needs.dedup();
        }
        self.set_state(name, State::Gather);
        self.gather(name, out)
    }

    /// Wait for every need to settle, then queue the run.
    fn gather(&mut self, name: Name, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        let needs = core::mem::replace(&mut self.jobs.get_mut(&name).expect("a job").needs, Vec::new_in(self.alloc));
        let (_, under) = self.cond_of(name);
        let mut waiting = false;
        let mut fork = None;
        for need in needs.iter() {
            match self.resolve(*need, name, &under, out) {
                Ok(Resolved::Wait) => waiting = true,
                Ok(Resolved::At) => {}
                Ok(Resolved::Fork(var)) => fork = fork.or(Some(var)),
                Err(error) => {
                    self.jobs.get_mut(&name).expect("a job").needs = needs;
                    return Err(error);
                }
            }
        }
        let job = self.jobs.get_mut(&name).expect("a job");
        job.needs = needs;
        if let Some(var) = fork {
            // A need splits on a variable this job does not fix: what it reads differs by that
            // variable's value, so it forks on it as if it had read it.
            return self.split(name, var, out);
        }
        if waiting {
            return Ok(());
        }
        if job.entries >= self.config.most_entries {
            let mut last_needs = [None; 4];
            for (slot, need) in last_needs.iter_mut().zip(job.needs.iter()) {
                *slot = Some(*need);
            }
            return Err(DriveError::Reentries { name, key: job.key, entries: job.entries, last_needs });
        }
        job.state = State::Queued;
        let class = job.class;
        self.queue(class, name);
        Ok(())
    }

    fn stand(&mut self, name: Name, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        self.profile.of(name.kind).standing += 1;
        self.settle(name, None, false, out)
    }

    /// A task made `output`: store it and settle its job.
    fn made(&mut self, output: &Output<'_>, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        self.made_split(output, None, out)
    }

    /// [`Request::made`], for a split's record too (`split` its variable).
    fn made_split(&mut self, output: &Output<'_>, split: Option<Var>, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        let job = self.jobs.get(&output.name).expect("checked by the caller");
        let (key, source) = (job.key, job.source);
        // A worker names its reads by base name; the record holds the names its condition
        // implies (stored under the condition it read), so a fork's change dirties exactly the
        // readers of that fork.
        let (_, under) = self.cond_of(output.name);
        let mut reads = Vec::with_capacity_in(output.reads.len(), self.alloc);
        for (read, saw) in output.reads {
            reads.push((self.resolve_settled(*read, &under), *saw));
        }
        reads.sort_unstable_by_key(|(read, _)| *read);
        reads.dedup_by_key(|(read, _)| *read);
        // A read whose digest is not today's makes a record valid for its own reads, stored,
        // but not the answer: the job is decided again.
        let mut stale = false;
        for (read, saw) in reads.iter() {
            match self.now.get(read) {
                Some(now) if now.out == *saw => {}
                None if *saw == ABSENT || self.old(*read).is_some_and(|row| row.out == *saw) => {}
                _ => stale = true,
            }
        }
        let whole = Whole {
            name: output.name,
            source,
            out: output.out,
            reads: &reads,
            parts: output.parts,
            paths: output.paths,
            findings: output.findings,
            payload: output.payload,
            split,
        };
        let bytes = record::write_whole(&whole, self.alloc);
        let at = match reads.is_empty() {
            true => key,
            false => {
                let shape = record::write_shape(output.name, &reads, self.alloc);
                self.put(key, shape, out);
                key::value_key(key, &reads)
            }
        };
        // The view is of the bytes written: the same path as a found record (one home).
        let view = match record::open(&bytes).map_err(|error| DriveError::Wire { key: at, error })? {
            Stored::Whole(view) => view,
            Stored::Shape(_) => unreachable!("write_whole writes a whole record"),
        };
        self.profile.of(output.name.kind).made += 1;
        self.settle(output.name, Some((at, view)), stale, out)?;
        self.put(at, bytes, out);
        Ok(())
    }

    /// The job's answer is in: its row, its findings, its parts, its readers.
    fn settle(
        &mut self,
        name: Name,
        record: Option<(Key, WholeView<'_>)>,
        stale: bool,
        out: &mut AVec<'_, Effect<'a>>,
    ) -> Result<(), DriveError> {
        let a = self.alloc;
        let before = self.now.get(&name).copied().or_else(|| self.old(name).map(|row| Now { at: row.at, out: row.out }));
        let job = self.jobs.get(&name).expect("a job");
        let (key, source, home, settles) = (job.key, job.source, job.home, job.settles);
        // A file's row keeps the edition it is read in (a repeat that does not run its home finds
        // it there, [`Request::locate`]); its findings are placed in its own text, from 0. Any
        // other name's row and findings keep its start in its home's file.
        // A module file at neither place ([`Request::absent_module`]): its finding is placed at
        // its `mod` item in its home's file.
        let absent = name.kind == Kind::FileItems && self.absent_files.contains(&Name::new(Kind::FileItems, self.cond_of(name).0.subject));
        let (place, held_place) = match (name.kind, job.locate) {
            (Kind::FileItems, Some(Locate::File { edition })) if absent => (u32::from(edition), job.place),
            (Kind::FileItems, Some(Locate::File { .. })) if self.plant == Plant::EditionAsPlace => (job.place, job.place),
            (Kind::FileItems, Some(Locate::File { edition })) => (u32::from(edition), 0),
            (Kind::FileItems, _) => return Err(DriveError::Unlocated { name }),
            _ => (job.place, job.place),
        };
        if settles >= self.config.most_entries {
            let mut last_needs = [None; 4];
            for (slot, need) in last_needs.iter_mut().zip(job.needs.iter()) {
                *slot = Some(*need);
            }
            return Err(DriveError::Reentries { name, key, entries: settles, last_needs });
        }
        let mut parts_now = Vec::new_in(a.clone());
        let (base, condition) = self.cond_of(name);
        let cond = (condition != Condition::ANY).then_some((base, condition));
        let split = match &record {
            None => self.current(name).and_then(|row| row.split),
            Some((_, view)) => view.split,
        };
        if let Some(var) = split {
            // Its parts are its forks: each runs under its condition, walked as any part.
            self.forks(name, var)?;
        }
        let now = match &record {
            None => {
                let row = self.current(name).expect("a standing job has a row");
                parts_now.extend_from_slice(&row.parts);
                Now { at: row.at, out: row.out }
            }
            Some((at, view)) => {
                let old = self.current(name);
                // A name read absent before it existed keeps those readers (its watch row's).
                let readers = self.new.get(&name).or_else(|| self.old_or_watch(name)).map(|row| row.readers.clone()).unwrap_or_else(|| Vec::new_in(a.clone()));
                let old_parts = old.map(|row| row.parts.clone()).unwrap_or_else(|| Vec::new_in(a.clone()));
                let mut reads = Vec::with_capacity_in(view.reads().len(), a.clone());
                reads.extend(view.reads());
                let mut parts = Vec::with_capacity_in(view.parts().len(), a.clone());
                for part in view.parts() {
                    parts.push(part.name);
                    self.part_locs.insert(part.name, (part.locate, part.place, name));
                }
                // Where each file part's file is, for one the request does not hold yet.
                for path in view.paths() {
                    let file = Name::file(path.path);
                    if !self.file_at.contains_key(&file) && !self.part_paths.contains_key(&file) {
                        let at = u32::try_from(self.paths.len()).expect("the checkout's paths fit 4 GiB");
                        self.paths.extend_from_slice(path.path);
                        let len = u32::try_from(path.path.len()).expect("a path fits 4 GiB");
                        self.part_paths.insert(file, (at, len, path.module));
                    }
                }
                parts.sort_unstable();
                parts_now.extend_from_slice(&parts);
                for gone in old_parts.iter() {
                    if parts.binary_search(gone).is_err() {
                        self.drop_name(*gone)?;
                    }
                }
                for (read, _) in reads.iter() {
                    match self.posted.get_mut(read) {
                        // No `contains`: a name settled twice posts twice, `dirty_readers` dedups.
                        Some(list) => list.push(name),
                        None => {
                            let mut list = Vec::new_in(a.clone());
                            list.push(name);
                            self.posted.insert(*read, list);
                        }
                    }
                }
                let held = Held { file: if absent { home } else { file_of(base, home) }, place: held_place, base, condition, findings: hold(view, a.clone()) };
                let is_error = self.config.is_error;
                let errors = held.findings.iter().filter(|f| is_error(f.rule)).count();
                self.findings.insert(name, held);
                self.found_errors(u32::try_from(errors).unwrap_or(u32::MAX), out);
                self.new.insert(
                    name,
                    Row { key, at: *at, source, out: view.out, home, place, reads, parts, readers, split, cond },
                );
                Now { at: *at, out: view.out }
            }
        };
        self.now.insert(name, now);
        self.dropped.remove(&name);
        if name == Name::units() {
            // Standing or made, the root units' record is where the program's entry is.
            self.units_at = Some(now.at);
            self.units_known(now.at, out);
        }
        // Every part of a record walked: a standing one stands on its row at no store read.
        for part in parts_now.iter() {
            match self.state(*part) {
                None => self.add_job(*part, name, None, false, self.walk)?,
                Some(State::Start) => self.wake.push_back(*part),
                Some(_) => {}
            }
        }
        self.set_state(name, State::Settled);
        let job = self.jobs.get_mut(&name).expect("a job");
        job.settles += 1;
        job.recheck = false;
        let redo = core::mem::replace(&mut job.redo, false) || stale;
        let entries = job.entries;
        self.profile.entries_seen(entries);
        if self.program == Program::Running && job.class == Class::Gate {
            self.profile.checked_beside += 1;
        }
        // A record about to be decided again is not one a worker should read; nor is a split,
        // which answers no read itself (a worker asks by base name, and only the machine knows
        // which fork the task's condition implies).
        if split.is_some() {
            self.publish.insert(name, None);
        } else if !redo {
            let input = Input { name, at: now.at, out: now.out };
            match self.plant {
                // The planted bug: the row published at once, ahead of the record's `Put`.
                Plant::PublishBeforePut => out.push(Effect::Publish(input)),
                _ => {
                    self.publish.insert(name, Some(input));
                }
            }
        }
        self.wake_waiters(name);
        if before.map(|b| b.out) != Some(now.out) {
            self.dirty_readers(name)?;
        }
        if redo {
            self.dirty(name)?;
        }
        Ok(())
    }

    /// A re-made home no longer lists `name`: its row goes, with the rows it listed whose pages
    /// this run holds, and its readers are checked again (a read of it is now `ABSENT`).
    fn drop_name(&mut self, name: Name) -> Result<(), DriveError> {
        let mut work = Vec::new_in(self.alloc);
        work.push(name);
        while let Some(gone) = work.pop() {
            if !self.dropped.insert(gone) {
                continue;
            }
            if let Some(row) = self.current(gone) {
                work.extend_from_slice(&row.parts);
            }
            self.gone_job(gone);
            self.dirty_readers(gone)?;
        }
        Ok(())
    }

    /// `name` is no longer settled: out of `now`, and its row of the settled table cleared at
    /// the step's end.
    fn unsettle(&mut self, name: Name) {
        if self.now.remove(&name).is_some() {
            self.publish.insert(name, None);
        }
    }

    /// A job of a dropped name settles with no record.
    fn gone_job(&mut self, name: Name) {
        self.unsettle(name);
        self.unqueue(name);
        self.set_state(name, State::Settled);
        self.wake_waiters(name);
    }

    /// `name`'s out digest changed: every reader is checked again.
    fn dirty_readers(&mut self, name: Name) -> Result<(), DriveError> {
        if self.plant == Plant::SkipChangedRead {
            return Ok(());
        }
        let mut readers = Vec::new_in(self.alloc);
        if let Some(row) = self.old_or_watch(name) {
            readers.extend_from_slice(&row.readers);
        }
        if let Some(posted) = self.posted.get(&name) {
            readers.extend_from_slice(posted);
        }
        readers.sort_unstable();
        readers.dedup();
        for reader in readers.iter() {
            if *reader != name {
                self.dirty(*reader)?;
            }
        }
        Ok(())
    }

    fn dirty(&mut self, name: Name) -> Result<(), DriveError> {
        match self.state(name) {
            None => {
                let home = self.old(name).map_or(name, |row| row.home);
                self.add_job(name, home, None, false, self.walk)?;
                if let Some(job) = self.jobs.get_mut(&name) {
                    job.recheck = true;
                }
            }
            Some(State::Settled) => {
                self.jobs.get_mut(&name).expect("a job").recheck = true;
                self.set_state(name, State::Decide);
                self.unsettle(name);
                self.wake.push_back(name);
            }
            Some(State::Start | State::Decide) => {
                self.jobs.get_mut(&name).expect("a job").recheck = true;
            }
            Some(_) => {
                let job = self.jobs.get_mut(&name).expect("a job");
                job.recheck = true;
                job.redo = true;
            }
        }
        Ok(())
    }

    /// Nothing is out and jobs are unsettled: they wait on each other. A cycle of module scopes
    /// becomes one `Group` task; any other is an error naming it.
    fn break_cycle(&mut self) -> Result<(), DriveError> {
        let mut cycle = Vec::new_in(self.alloc);
        for (name, job) in self.jobs.iter() {
            if job.state != State::Settled {
                cycle.push(*name);
            }
        }
        // The group's name hashes its members: in name order, not the table's (rule 25).
        // One scan per quiet point with jobs left, not per event.
        cycle.sort_unstable();
        if cycle.iter().all(|name| name.kind == Kind::ModuleScope) {
            let mut h = Hasher::new(GROUP_DOMAIN);
            for member in cycle.iter() {
                h.id(&member.subject);
            }
            let group = Name::new(Kind::Group, h.finish());
            let mut sources = Hasher::new(GROUP_DOMAIN);
            for member in cycle.iter() {
                sources.id(&self.jobs.get(member).expect("a job").source);
            }
            self.add_job(group, group, Some(Locate::Source(sources.finish())), false, self.walk)?;
            let job = self.jobs.get_mut(&group).expect("just added");
            job.members.extend_from_slice(&cycle);
            for member in cycle.iter() {
                self.set_state(*member, State::Running);
            }
            return Ok(());
        }
        let mut names = [None; 8];
        for (slot, name) in names.iter_mut().zip(cycle.iter()) {
            *slot = Some(*name);
        }
        Err(DriveError::NeedCycle { names })
    }

    // --- Finish -------------------------------------------------------------------------------

    /// Every job is settled: write the reverse index, the touched pages, the root, the
    /// certificate and `last`, then print.
    fn finish(&mut self, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        let a = self.alloc;
        // Items by how many conditions they were answered under: each settled record that is
        // not a split is one condition of its base.
        let mut per_item: Table<Name, u32, Alloc<'a>> = Table::new_in(a.clone());
        for (name, job) in self.jobs.iter() {
            if job.state == State::Settled && name.kind != Kind::Group && self.current(*name).is_some_and(|row| row.split.is_none()) {
                let base = self.cond_of(*name).0;
                let n = per_item.get(&base).copied().unwrap_or(0);
                per_item.insert(base, n + 1);
            }
        }
        for n in per_item.values() {
            self.profile.conditions_seen(*n);
        }
        // The reverse index: each made row's reads against its old row's (both sorted by name,
        // so one binary search a read).
        let mut updates: Vec<(Name, Name, bool), Alloc<'a>> = Vec::new_in(a.clone());
        let has = |reads: &[(Name, ContentId)], read: &Name| reads.binary_search_by_key(read, |(r, _)| *r).is_ok();
        for (name, row) in self.new.iter() {
            let old = self.old(*name);
            for (read, _) in row.reads.iter() {
                if !old.is_some_and(|old| has(&old.reads, read)) {
                    updates.push((*read, *name, true));
                }
            }
            if let Some(old) = old {
                for (read, _) in old.reads.iter() {
                    if !has(&row.reads, read) {
                        updates.push((*read, *name, false));
                    }
                }
            }
        }
        for gone in self.dropped.iter() {
            if let Some(old) = self.old(*gone) {
                for (read, _) in old.reads.iter() {
                    updates.push((*read, *gone, false));
                }
            }
        }
        // Every page the update touches must be loaded first.
        let old_rows = self.root.as_ref().map_or(0, |root| root.rows);
        let mut added = 0u64;
        for (name, row) in self.new.iter() {
            if self.old(*name).is_none() && !row.watching() {
                added += 1;
            }
        }
        let removed = self.dropped.iter().filter(|name| self.old(**name).is_some()).count() as u64;
        let rows = (old_rows + added).saturating_sub(removed);
        let bits = answers::bits_for(rows);
        let old_bits = self.root.as_ref().map(|root| root.bits);
        // Page indices collected, then sorted once (an insert into a sorted set per name would
        // shift: O(names x pages)).
        let mut wanted = Vec::new_in(a.clone());
        match old_bits {
            Some(old_bits) if old_bits != bits => wanted.extend(0..(1u32 << old_bits)),
            Some(old_bits) => {
                for name in self.new.keys().chain(self.dropped.iter()).chain(updates.iter().map(|(t, _, _)| t)) {
                    wanted.push(answers::page_of(*name, old_bits));
                }
            }
            None => {}
        }
        wanted.sort_unstable();
        wanted.dedup();
        let mut requests = Vec::new_in(a.clone());
        if let Some(root) = &self.root {
            for index in wanted.iter() {
                if !self.pages.contains_key(index) {
                    requests.push((*index, root.pages[*index as usize]));
                }
            }
        }
        let mut loading = false;
        for (index, key) in requests.iter() {
            if *key == Key::default() {
                self.pages.insert(*index, PageState::Loaded(SortedMap::new_in(a.clone())));
            } else {
                self.pages.insert(*index, PageState::Loading);
                self.get(*key, Purpose::Page(*index), out);
                loading = true;
            }
        }
        if loading {
            return Ok(());
        }
        // A dropped name's readers read it absent now: a watch row keeps them, so the name's
        // coming back reruns them (rule 21: a read that found nothing is a read).
        if self.plant != Plant::ForgetAbsentReaders {
            let gone: Vec<Name, Alloc<'a>> = self.dropped.iter().copied().collect_in(a.clone());
            for name in gone.iter() {
                if self.new.get(name).is_some_and(|row| row.watching()) {
                    continue;
                }
                let Some(was) = self.new.get(name).or_else(|| self.old_or_watch(*name)) else { continue };
                if was.readers.is_empty() {
                    continue;
                }
                let readers = was.readers.clone();
                self.new.insert(*name, watch_row(*name, readers, a.clone()));
            }
        }
        // Apply the reverse index's updates: sorted by target once, then each target's readers
        // rebuilt in one merge, so a name read by r readers costs O(r + its updates), never an
        // insert per update.
        updates.sort_unstable();
        let mut at = 0;
        while at < updates.len() {
            let target = updates[at].0;
            let end = at + updates[at..].iter().take_while(|(t, _, _)| *t == target).count();
            let group = &updates[at..end];
            at = end;
            // A target with no record (never made, or dropped this run) keeps its readers in a
            // watch row: a read that found nothing is a read, so the name's appearing reruns
            // them (rule 21). The planted bug forgets them.
            let dropped = self.dropped.contains(&target);
            if self.new.get(&target).is_none_or(|row| dropped && !row.watching()) {
                let held = match dropped {
                    false => self.old_or_watch(target).cloned(),
                    true => None,
                };
                let row = match held {
                    Some(row) => row,
                    None if self.plant == Plant::ForgetAbsentReaders => continue,
                    None => watch_row(target, Vec::new_in(a.clone()), a.clone()),
                };
                self.new.insert(target, row);
            }
            let row = self.new.get_mut(&target).expect("inserted above");
            let mut readers = Vec::with_capacity_in(row.readers.len() + group.len(), a.clone());
            let (mut i, mut j) = (0, 0);
            while i < row.readers.len() || j < group.len() {
                let held = row.readers.get(i).copied();
                let update = group.get(j).map(|(_, reader, add)| (*reader, *add));
                match (held, update) {
                    (Some(h), Some((u, add))) if u == h => {
                        // Removed, or added again: an add keeps it once.
                        if add {
                            readers.push(h);
                        }
                        i += 1;
                        j += 1;
                    }
                    (Some(h), Some((u, _))) if h < u => {
                        readers.push(h);
                        i += 1;
                    }
                    (Some(h), None) => {
                        readers.push(h);
                        i += 1;
                    }
                    (_, Some((u, add))) => {
                        if add && readers.last() != Some(&u) {
                            readers.push(u);
                        }
                        j += 1;
                    }
                    (None, None) => break,
                }
            }
            row.readers = readers;
            // A watch row no one reads any more goes.
            if row.watching() && row.readers.is_empty() {
                self.new.remove(&target);
                self.dropped.insert(target);
            }
        }
        // The touched pages, rewritten whole, and the root.
        let mut touched = Vec::new_in(a.clone());
        match old_bits {
            Some(old_bits) if old_bits == bits => {
                for name in self.new.keys().chain(self.dropped.iter()) {
                    touched.push(answers::page_of(*name, bits));
                }
            }
            _ => touched.extend(0..(1u32 << bits)),
        }
        touched.sort_unstable();
        touched.dedup();
        let mut page_keys = match (&self.root, old_bits) {
            (Some(root), Some(old_bits)) if old_bits == bits => root.pages.clone(),
            _ => {
                let mut keys = Vec::new_in(a.clone());
                keys.resize(1usize << bits, Key::default());
                keys
            }
        };
        // Every row of a touched page, bucketed by page once: O(rows), not pages x rows.
        let mut buckets: Table<u32, Vec<(Name, Row<'a>), Alloc<'a>>, Alloc<'a>> = Table::new_in(a.clone());
        let bucket = |buckets: &mut Table<u32, Vec<(Name, Row<'a>), Alloc<'a>>, Alloc<'a>>, name: Name, row: &Row<'a>| {
            let index = answers::page_of(name, bits);
            if touched.binary_search(&index).is_err() {
                return;
            }
            match buckets.get_mut(&index) {
                Some(list) => list.push((name, row.clone())),
                None => {
                    let mut list = Vec::new_in(a.clone());
                    list.push((name, row.clone()));
                    buckets.insert(index, list);
                }
            }
        };
        for state in self.pages.values() {
            if let PageState::Loaded(old) = state {
                for (name, row) in old.iter() {
                    if !self.dropped.contains(name) && !self.new.contains_key(name) {
                        bucket(&mut buckets, *name, row);
                    }
                }
            }
        }
        for (name, row) in self.new.iter() {
            if !self.dropped.contains(name) || row.watching() {
                bucket(&mut buckets, *name, row);
            }
        }
        for index in touched.iter() {
            let mut rows = buckets.remove(index).unwrap_or_else(|| Vec::new_in(a.clone()));
            rows.sort_unstable_by_key(|(name, _)| *name);
            rows.dedup_by_key(|(name, _)| *name);
            let page: Page<'a> = SortedMap::from_sorted(rows).expect("sorted and deduplicated above");
            let key = match page.is_empty() {
                true => Key::default(),
                false => {
                    let bytes = answers::write_page(page.iter(), a.clone());
                    let key = key::content_key(&bytes);
                    self.profile.pages_written += 1;
                    self.put(key, bytes, out);
                    key
                }
            };
            page_keys[*index as usize] = key;
        }
        let root = Root { bits, rows, pages: page_keys };
        let root_bytes = answers::write_root(&root, a.clone());
        let root_key = key::content_key(&root_bytes);
        self.put(root_key, root_bytes, out);

        // The findings: this run's records over the last certificate's, less what went.
        // Merged in one pass: this run's, sorted once, over the last certificate's (sorted).
        let old = core::mem::replace(&mut self.old_findings, SortedMap::new_in(a.clone()));
        let mut made = Vec::with_capacity_in(self.findings.len(), a.clone());
        made.extend(self.findings.take_all());
        made.sort_unstable_by_key(|(name, _)| *name);
        let mut merged = Vec::with_capacity_in(old.len() + made.len(), a.clone());
        let mut old = old.into_iter().peekable();
        let mut made = made.into_iter().peekable();
        loop {
            let take_made = match (old.peek(), made.peek()) {
                (Some((o, _)), Some((m, _))) => m <= o,
                (None, Some(_)) => true,
                (Some(_), None) => false,
                (None, None) => break,
            };
            let (name, held) = match take_made {
                true => {
                    let entry = made.next().expect("peeked");
                    if old.peek().is_some_and(|(o, _)| *o == entry.0) {
                        old.next();
                    }
                    entry
                }
                false => {
                    let (name, mut held) = old.next().expect("peeked");
                    // A part whose record stands (its text unchanged, so it did not run) keeps
                    // its findings, item-relative; where they print is its start in its file
                    // today, which an edit above it moves: its home's parts, settled this run,
                    // say where. A file's findings are placed in its own text, from 0.
                    if name.kind != Kind::FileItems
                        && let Some((_, place, _)) = self.part_locs.get(&name)
                    {
                        held.place = *place;
                    }
                    (name, held)
                }
            };
            if !held.findings.is_empty() && !self.dropped.contains(&name) {
                merged.push((name, held));
            }
        }
        let all = SortedMap::from_sorted(merged).expect("merged in name order, one entry a name");
        for (name, job) in self.jobs.iter() {
            if name.kind == Kind::Test {
                match self.new.get(name).is_some_and(|row| Some(row.key) == Some(job.key)) && job.entries > 0 {
                    true => self.profile.tests_run += 1,
                    false => self.profile.tests_reused += 1,
                }
            }
        }
        let units = self.units_at.or_else(|| self.old(Name::units()).map(|row| row.at));
        let cert = write_cert(&all, root_key, units, a.clone());
        // Keyed by the files this answer read, the leaf table the cli writes beside it: the next
        // request's `Start` hands exactly these when none changed (`Request::holds`).
        let mut held_files = Vec::with_capacity_in(self.files.len(), a.clone());
        for i in 0..self.files.len() {
            let path = self.path(i as u32);
            if self.holds(path) {
                held_files.push((path, self.files.ids()[i]));
            }
        }
        held_files.sort_unstable_by(|x, y| x.0.cmp(y.0));
        let read = self.cert_of(held_files.iter().map(|(path, id)| (*path, id)));
        drop(held_files);
        // A failed task leaves a hole in the answer: neither the certificate nor `last` is
        // written, so the next command walks from the last whole answer and runs it again.
        // The records this run made stay in the store under their keys and are found there.
        if self.failures.is_empty() {
            self.cert = read;
            self.put(self.cert, cert, out);
            let last = Last {
                tool: self.config.tool.0,
                shape: self.shape_digest,
                units: self.units_source,
                cert: self.cert,
                root: root_key,
            };
            out.push(Effect::WriteLast { bytes: answers::write_last(&last, a) });
        }
        // Every finding of the answer counts, the stood ones too: an unchanged item's error is
        // still an error of the scope. One finding held by two names counts once (`print`).
        let (count, errors) = self.print(&all, out);
        self.errors = errors;
        if self.gate == Gate::Beside && self.program == Program::Running && self.errors > 0 {
            self.program = Program::Stopped;
            out.push(Effect::Stop { errors: self.errors });
        }
        self.held = Some(Answer { profile: self.profile, verdict: Verdict::Checked, findings: count, units, failed: self.failed() });
        self.phase = Phase::Held;
        self.conclude(out);
        Ok(())
    }

    /// `Print` effects for `held`, sorted by path and position; how many, and how many of them
    /// are errors. An item's or a manifest's finding is its own: its name is where it is. A
    /// crate's finding carries its file and item in its words, so it is its rule, range, words
    /// and condition, whichever crate's record holds it: two crates holding one finding (a
    /// library and its test unit each resolve the same `use` of the same file) print it once
    /// and count it once, as the name first in the order. (Owed: `test` as a variable of one
    /// crate's condition, "One check covers every configuration", so there is one record.)
    fn print(&self, held: &SortedMap<Name, Held<'a>, Alloc<'a>>, out: &mut AVec<'_, Effect<'a>>) -> (u32, u32) {
        // (path, place, name, the finding's index in its record, file, the record's findings).
        let mut order = Vec::with_capacity_in(held.len(), self.alloc);
        for (name, entry) in held.iter() {
            let file = match entry.file.kind {
                Kind::FileItems => self.file_index(&entry.file.subject).map(|i| i as u32),
                _ => None,
            };
            let path: &[u8] = file.map_or(&[], |i| self.path(i));
            for nth in 0..entry.findings.len() {
                order.push((path, entry.place, *name, nth, file, entry));
            }
        }
        order.sort_unstable_by(|x, y| (x.0, x.1, x.2, x.3).cmp(&(y.0, y.1, y.2, y.3)));
        // What a finding is: rows equal in it are one finding. Found by sorting the rows'
        // indices by it (ties in print order, so the first printed is the one kept).
        let what = |at: u32| {
            let row = &order[at as usize];
            let finding = &row.5.findings[row.3];
            let of = (row.5.base.kind != Kind::CrateScope).then_some((row.2, row.3));
            (row.0, row.1, of, finding.rule, finding.start, finding.end, finding.message.as_slice(), row.5.condition)
        };
        let mut by_what = Vec::with_capacity_in(order.len(), self.alloc);
        let mut again = Vec::with_capacity_in(order.len(), self.alloc);
        for at in 0..order.len() {
            by_what.push(u32::try_from(at).expect("an answer's findings fit 32 bits"));
            again.push(false);
        }
        by_what.sort_unstable_by(|x, y| what(*x).cmp(&what(*y)).then(x.cmp(y)));
        // The planted bug: each name's copy is a finding of its own.
        if self.plant != Plant::PrintEachName {
            for at in 1..by_what.len() {
                if what(by_what[at - 1]) == what(by_what[at]) {
                    again[by_what[at] as usize] = true;
                }
            }
        }
        let is_error = self.config.is_error;
        let (mut count, mut errors) = (0, 0);
        for (at, (_, place, _, nth, file, entry)) in order.iter().enumerate() {
            if again[at] {
                continue;
            }
            let finding = &entry.findings[*nth];
            let mut message = Vec::with_capacity_in(finding.message.len(), self.alloc);
            message.extend_from_slice(&finding.message);
            out.push(Effect::Print(Print {
                name: entry.base,
                condition: entry.condition,
                file: *file,
                place: *place,
                rule: finding.rule,
                start: finding.start,
                end: finding.end,
                message,
            }));
            count += 1;
            errors += u32::from(is_error(finding.rule));
        }
        (count, errors)
    }
}

/// The file a name's findings are placed in: its own for a file, its home's for an item.
/// The longest message a finding the machine makes holds (it is written in place, no
/// allocation); a longer one is cut at an ASCII boundary.
const MESSAGE_BYTES: usize = 192;

/// The rule of a `mod x;` whose file is at neither `x.rs` nor `x/mod.rs` (arguments: the two
/// paths, `\x1f` between); an error under every command.
pub const MODULE_FILE_MISSING_RULE: u16 = 0xC0F7;
/// The rule of a `#[path]` module whose file is not in the checkout (argument: the path).
pub const PATH_FILE_MISSING_RULE: u16 = 0xC0F8;

/// The over-cap finding's text: [`ConditionError::OverCap`] with the variables the item reads,
/// `var` included.
fn over_cap_message(buf: &mut [u8; MESSAGE_BYTES], span: u32, under: &Condition, var: Var) -> usize {
    struct Cut<'b> {
        buf: &'b mut [u8],
        len: usize,
    }
    impl fmt::Write for Cut<'_> {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            let n = s.len().min(self.buf.len() - self.len);
            self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
            self.len += n;
            Ok(())
        }
    }
    let vars = under.with(var, 0).ok().flatten().unwrap_or(*under);
    let mut cut = Cut { buf, len: 0 };
    fmt::Write::write_fmt(&mut cut, format_args!("{}", ConditionError::OverCap { conditions: span, vars }))
        .expect("a cut writer takes every byte (it drops what does not fit)");
    cut.len
}

/// A watch row for `name` ([`Row::watching`]): no record, its readers only.
fn watch_row<'a>(name: Name, readers: Vec<Name, Alloc<'a>>, alloc: Alloc<'a>) -> Row<'a> {
    Row {
        key: Key::default(),
        at: Key::default(),
        source: ABSENT,
        out: ABSENT,
        home: name,
        place: 0,
        reads: Vec::new_in(alloc.clone()),
        parts: Vec::new_in(alloc),
        readers,
        split: None,
        cond: None,
    }
}

/// Whether `command` has a program in its answer: `run` alone. The one home of the rule, asked
/// by the walk's class and by every place a program could start. Under
/// [`Plant::TestStartsProgram`] `test` starts one too (the simulation's control).
fn starts_program(command: Command, plant: Plant) -> bool {
    match command {
        Command::Run => true,
        Command::Test => plant == Plant::TestStartsProgram,
        Command::Check => false,
    }
}

fn file_of(name: Name, home: Name) -> Name {
    // A manifest's units are read from that manifest: their findings are on its lines.
    name.text_file().unwrap_or(home)
}

fn hold<'a>(view: &WholeView<'_>, alloc: Alloc<'a>) -> Vec<HeldFinding<'a>, Alloc<'a>> {
    let mut held = Vec::with_capacity_in(view.finding_count(), alloc.clone());
    for finding in view.findings() {
        let mut message = Vec::with_capacity_in(finding.message.len(), alloc.clone());
        message.extend_from_slice(finding.message);
        held.push(HeldFinding { rule: finding.rule, start: finding.start, end: finding.end, message });
    }
    held
}

/// The certificate: the answer's root (so one that stands can write `last` naming it), the
/// units' record (so a repeat `run` starts at one read), then every finding of the answer.
fn write_cert<'a>(held: &SortedMap<Name, Held<'a>, Alloc<'a>>, root: Key, units: Option<Key>, alloc: Alloc<'a>) -> Vec<u8, Alloc<'a>> {
    let mut w = Writer::new_in(CERT_MAGIC, alloc);
    w.raw(&root.0.0);
    w.u32(u32::from(units.is_some()));
    w.raw(&units.unwrap_or_default().0.0);
    w.count(held.len());
    for (name, entry) in held.iter() {
        w.name(*name);
        w.name(entry.file);
        w.u32(entry.place);
        w.name(entry.base);
        w.condition(&entry.condition);
        w.count(entry.findings.len());
        for finding in entry.findings.iter() {
            w.u16(finding.rule);
            w.u32(finding.start);
            w.u32(finding.end);
            w.bytes(&finding.message);
        }
    }
    w.finish()
}

/// A certificate read: the answer's root, the units' record, the findings.
type Cert<'a> = (Key, Option<Key>, SortedMap<Name, Held<'a>, Alloc<'a>>);

fn read_cert<'a>(bytes: &[u8], alloc: Alloc<'a>) -> Result<Cert<'a>, WireError> {
    let mut r = Reader::new(bytes, CERT_MAGIC, "certificate")?;
    let mut root = [0u8; 32];
    root.copy_from_slice(r.take_bytes(32)?);
    let root = Key(ContentId(root));
    let has_units = r.u32()? == 1;
    let mut units = [0u8; 32];
    units.copy_from_slice(r.take_bytes(32)?);
    let units = has_units.then_some(Key(ContentId(units)));
    let n = r.count(NAME_BYTES * 3 + 1 + 8)?;
    let mut entries = Vec::with_capacity_in(n, alloc.clone());
    for _ in 0..n {
        let name = r.name()?;
        let file = r.name()?;
        let place = r.u32()?;
        let base = r.name()?;
        let condition = r.condition()?;
        let nf = r.count(14)?;
        let mut findings = Vec::with_capacity_in(nf, alloc.clone());
        for _ in 0..nf {
            let rule = r.u16()?;
            let start = r.u32()?;
            let end = r.u32()?;
            let text = r.bytes()?;
            let mut message = Vec::with_capacity_in(text.len(), alloc.clone());
            message.extend_from_slice(text);
            findings.push(HeldFinding { rule, start, end, message });
        }
        entries.push((name, Held { file, place, base, condition, findings }));
    }
    r.done()?;
    let held = SortedMap::from_sorted(entries).map_err(|_| r.error(crate::wire::WireWhy::Unsorted))?;
    Ok((root, units, held))
}
