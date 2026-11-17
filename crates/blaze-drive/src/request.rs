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
use crate::answers::{self, HeldPage, Last, PageError, PageRow, Root, Row, RowRef, RowView};
use crate::cells::{Cell, CellAt, CellRead, CellReader, Cells, Seen};
use crate::condition::{Condition, ConditionError, MOST_ATOMS, MOST_CONDITIONS, OVER_CAP_RULE, Space, Var};
use crate::key::{self, ABSENT, Key, Kind, Name, Tool};
use crate::owners::{self, Root as OwnerRoot};
use crate::profile::Profile;
use crate::record::{self, Finding, Locate, Part, PathOf, Stored, Whole, WholeView};
use crate::run_scope::Closure;
use crate::table::{Hashed, Set, Table, spread};
use crate::wire::{NAME_BYTES, Reader, WireError, Writer};

// --- What the caller gives and gets ---------------------------------------------------------

/// The machine's bounds and the tool.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// The whole tool: every producer and every form. The certificate, the pointer and the walk
    /// from the last answer hold it: they speak for the whole command.
    pub tool: Tool,
    /// Each kind's tool, by [`Kind::index`]: the producer that makes that kind and every form
    /// (decision D105), so an edit to one producer moves only the keys of the kinds it makes.
    /// A task's key holds its kind's ([`Config::tool_of`]).
    pub tools: [Tool; key::KINDS],
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

impl Config {
    /// The tool a task of `kind` is keyed by.
    pub fn tool_of(&self, kind: Kind) -> Tool {
        self.tools[kind.index()]
    }
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
    /// The check is done before the program starts (`run`, run.md D124): it starts only over a
    /// check with no error, no failed task and no item it could not check
    /// ([`Request::with_not_checked`]); a standing certificate decides that at its one read.
    Before,
    /// Only what runs is checked; the verdict says so.
    Reached,
}

impl Gate {
    /// The gate in a `run`'s or `test`'s shape ([`shape_of`]): a program started under one gate
    /// is another answer than under another (rule 21), so each has its own pointer and
    /// certificate.
    fn byte(self) -> u8 {
        match self {
            Gate::Beside => 1,
            Gate::Before => 2,
            Gate::Reached => 3,
        }
    }
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
        const CHECK: [Kind; 12] = [
            Kind::Units,
            Kind::UnitGraph,
            Kind::Fetch,
            Kind::Script,
            Kind::FileItems,
            Kind::ModuleScope,
            Kind::Group,
            Kind::Expansion,
            Kind::Signature,
            Kind::ImplBucket,
            Kind::BodyCheck,
            Kind::Lint,
        ];
        const RUN: [Kind; 14] = [
            Kind::Units,
            Kind::UnitGraph,
            Kind::Fetch,
            Kind::Script,
            Kind::FileItems,
            Kind::ModuleScope,
            Kind::Group,
            Kind::Expansion,
            Kind::Signature,
            Kind::ImplBucket,
            Kind::BodyCheck,
            Kind::Code,
            Kind::Links,
            Kind::Native,
        ];
        // Every kind before `Given`, which is no task (key.rs), and `Lint`, which only `check`
        // asks: `ALL` lists the two last.
        const TEST: &[Kind] = Kind::ALL.split_at(Kind::Given as usize - 1).0;
        match self {
            Command::Check => &CHECK,
            Command::Run => &RUN,
            Command::Test => TEST,
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
    /// For `run`: the program's native image ([`Name::native`], made by the command from the
    /// package and binary asked), a job from the start so the program's first call finds it.
    /// Part of the shape: an answer certifies one program's image. `None` for `check`, `test`.
    pub program: Option<Name>,
    /// What the command gives a task as a fact ([`key::fact_of`]: an environment variable, the
    /// target), each with its value's digest, sorted by name (refused unsorted at `Start`).
    /// Answered before the store: a reader of one sees this value, never an ambient input. Not
    /// part of the shape: a moved fact misses the certificate (its key holds them) and walks
    /// forward, rerunning only its readers.
    pub facts: &'a [(Name, ContentId)],
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
    /// What it read whole, sorted by name, each with the out digest its input had.
    pub reads: &'a [(Name, ContentId)],
    /// What it read of one cell of a record (cells.rs), each with what the cell held: a
    /// change of the record moves this reader only when its cell moved. Empty for a task of
    /// whole reads only.
    pub cells: &'a [CellRead],
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
    /// The task's file (its name's) changed between the sweep and the task's read: the bytes
    /// read are not the content id its key holds (`verified::verify`). It puts nothing, and the
    /// command's answer is refused naming the file: no re-read, no retry (rule 3).
    Changed,
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
            Why::Waited { on } => write!(f, "{}: needs {on}, which failed", self.name),
            Why::Changed => write!(f, "{}: changed while the command ran; run again", self.name),
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
    /// Start the program whose entry the units' record at `units` names, from the native image
    /// the `Native` record at `native` holds (blaze-run's Parent). Both keys are the effect's
    /// own, so the runner never asks the request for either. Answered by [`Event::Exited`].
    StartProgram { units: Key, native: Key },
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
/// How many needs of one entry [`DriveError::Reentries`] names.
pub const ENTRY_NEEDS: usize = 8;

/// One entry of a job, as [`DriveError::Reentries`] reports it: the names its run was handed,
/// then those its answer asked, each marked `true` when that entry asked it and was not handed it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct EntryNeeds {
    /// The entry's number (1: the first run); 0: none recorded.
    pub entry: u32,
    pub needs: [Option<(Name, bool)>; ENTRY_NEEDS],
    /// Needs past the first [`ENTRY_NEEDS`].
    pub more: u32,
}

impl EntryNeeds {
    fn push(&mut self, name: Name, new: bool) {
        match self.needs.iter_mut().find(|slot| slot.is_none()) {
            Some(slot) => *slot = Some((name, new)),
            None => self.more += 1,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DriveError {
    /// Bytes at `key` are not the record they are stored as.
    Wire { key: Key, error: WireError },
    /// Bytes at `key` are not an answer page of this form ([`answers::PageView::open`]).
    Page { key: Key, error: PageError },
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
    /// A job ran or settled more than `most_entries` times: what its last two entries were
    /// handed and asked (`entry` 0: not recorded).
    Reentries { name: Name, key: Key, entries: u32, last: [EntryNeeds; 2] },
    /// `by` reported reading `read` at `saw`, and its run was handed `read` at `handed`: a read's
    /// digest is the out of the input it was handed, never one the task computed itself (rule
    /// 21: a read list is a function of the text and the inputs). Such a task re-enters for ever,
    /// its record stale against its own input (tempo's `BodyCheck`, 2026-11-17).
    ReadNotHanded { by: Name, read: Name, saw: ContentId, handed: ContentId },
    /// Jobs that wait on each other and are not module scopes.
    NeedCycle { names: [Option<Name>; 8] },
    /// A task needed itself.
    SelfNeed { name: Name },
    TooManyJobs { most: u32, name: Name },
    /// A step after the answer.
    Finished,
    /// `Exited` with no program started, or a second time.
    NoProgram,
    /// A program to start whose native image has no key: the ask named none (`program:
    /// None`), or the certificate holds no native slot for the program it names.
    NoNativeImage { program: Option<Name> },
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
    /// An ask's fact is not a [`Kind::Given`] name ([`key::fact_of`]).
    NotGiven { name: Name },
    /// `by` read the given `name` (a file's content, a fact) as `saw`, and this command holds
    /// it as `held` (`None`: absent): the file moved while the command ran, or a path a task
    /// read was not joined to the leaf table, so the sweep did not follow it. Never re-decided:
    /// what a command gives is fixed for the command.
    GivenDiffers { name: Name, by: Name, saw: ContentId, held: Option<ContentId> },
    /// A task read a cell of `name`, whose kind has no cell lookup ([`Request::with_cells`]).
    NoCells { name: Name, by: Name },
    /// The record of `name` refused the lookup of a cell its reader read: its payload is not
    /// of the form its kind's [`crate::CellOf`] reads.
    CellRefused { name: Name, cell: Cell, what: &'static str },
}

impl fmt::Display for DriveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DriveError::Wire { key, error } => write!(f, "the record at {key}: {error}"),
            DriveError::Page { key, error } => write!(f, "the record at {key}: {error}"),
            DriveError::LastFile(error) => write!(f, "the checkout's last answer (a pointer in .blaze/v2/at), of this build's form, is malformed: {error}"),
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
            DriveError::Reentries { name, key, entries, last } => {
                write!(f, "{name} (key {key}) was entered {entries} times (rule 18)")?;
                for entry in last.iter().filter(|entry| entry.entry > 0) {
                    write!(f, "; entry {} needed", entry.entry)?;
                    let mut any = false;
                    for (need, new) in entry.needs.iter().flatten() {
                        any = true;
                        write!(f, " {need}{}", if *new { " (newly asked)" } else { "" })?;
                    }
                    if entry.more > 0 {
                        write!(f, " and {} more", entry.more)?;
                    }
                    if !any {
                        write!(f, " nothing")?;
                    }
                }
                let recorded = last.iter().any(|entry| entry.entry > 0);
                let asked = last.iter().any(|entry| entry.needs.iter().flatten().any(|(_, new)| *new));
                if recorded && !asked {
                    write!(f, "; neither entry asked a name it was not handed: each answer was decided again, so what the task reports reading is not a function of its text and inputs (rule 21)")?;
                }
                Ok(())
            }
            DriveError::ReadNotHanded { by, read, saw, handed } => write!(
                f,
                "{by} read {read} as {:02x}{:02x}{:02x}{:02x}, and its run was handed {read} as {:02x}{:02x}{:02x}{:02x}: a read's digest is its input's out, never one the task computed (rule 21)",
                saw.0[0], saw.0[1], saw.0[2], saw.0[3], handed.0[0], handed.0[1], handed.0[2], handed.0[3]
            ),
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
            DriveError::NoNativeImage { program: None } => write!(f, "a program to start, and the ask names no native image (`Ask::program`)"),
            DriveError::NoNativeImage { program: Some(name) } => write!(f, "the program {name} to start, and the certificate holds no native image for it"),
            DriveError::LateWant { name } => write!(f, "the program wanted {name} after the check was done; a late want is not built (drive.md, \"The gate\")"),
            DriveError::Condition { name, error } => write!(f, "the conditions of {name}: {error}"),
            DriveError::SplitOnFixed { name, var } => {
                write!(f, "the record of {name} forks on cfg variable {var}, which its condition fixes (rule 21)")
            }
            DriveError::SpaceNotClosed => {
                write!(f, "the request's feature implications are not transitively closed (drive.md, \"One check covers every configuration\")")
            }
            DriveError::NotGiven { name } => write!(f, "the ask's fact {name} is not a Given name (key::fact_of)"),
            DriveError::GivenDiffers { name, by, saw, held } => {
                write!(f, "{by} read {name} as {:02x}{:02x}{:02x}{:02x}, and this command gives ", saw.0[0], saw.0[1], saw.0[2], saw.0[3])?;
                match held {
                    Some(held) => write!(f, "{:02x}{:02x}{:02x}{:02x}", held.0[0], held.0[1], held.0[2], held.0[3])?,
                    None => write!(f, "it absent")?,
                }
                write!(f, ": the file moved during the command, or a path a task read was not joined to the leaf table (rule 21)")
            }
            DriveError::NoCells { name, by } => {
                write!(f, "{by} read a cell of {name}, and {} records have no cells (drive.md, \"Part reads\")", name.kind.name())
            }
            DriveError::CellRefused { name, cell, what } => {
                write!(f, "the record of {name} refused the lookup of cell {:02x}{:02x}{:02x}{:02x}: {what}", cell.0[0], cell.0[1], cell.0[2], cell.0[3])
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
    /// Its last settle stood on its row ([`Request::stand`]), counted in the profile's
    /// `standing`: a dirty after it takes the count back, so `standing` counts jobs that stood
    /// to the end, never one decided again (sim_cells' `CellsAsWhole` control).
    stood: bool,
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
    /// The cells its value key reads, sorted by (name, cell).
    shape_cells: Vec<(Name, Cell), Alloc<'a>>,
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
    /// An `Owners` record read for its roots (owners.rs): a standing row's, which this run did
    /// not compute.
    Roots(Name),
    /// A standing home's record read for its parts' locates: a part of it this run asks for
    /// (a body made a job only now, by a need) whose home was not re-made.
    Parts(Name),
    /// A settled record read for the cells its readers' value keys read (cells.rs): one that
    /// stood, so its bytes were not in hand when it settled.
    Cells(Name),
}

enum PageState<'a> {
    Loading,
    /// Checked once as it came in, then read in place.
    Loaded(HeldPage<'a>),
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

/// A request's shape: its command, its arguments, the space it is checked under and, for `run`
/// and `test`, its gate. A certificate and a last answer are one shape's; the machine walks
/// forward only from its own. `check` ignores the gate ([`Ask::gate`]), so its shape holds none.
pub fn shape_of(ask: &Ask<'_>) -> ContentId {
    let mut h = Hasher::new(SHAPE_DOMAIN);
    h.update(&[ask.command.byte()]).part(ask.args).id(&key::space_digest(&ask.space));
    if ask.command != Command::Check {
        h.update(&[ask.gate.byte()]);
    }
    if let Some(program) = ask.program {
        h.update(&[program.kind as u8]).id(&program.subject);
    }
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
    /// The program's native image ([`Ask::program`]): seeded at `begin` when a program runs.
    program_name: Option<Name>,
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
    /// Every home this run settled that lists each file part, sorted: a file that is a module
    /// of two crates has two (`part_locs` keeps where to find a part, this who lists it).
    parents: Table<Name, AVec<'a, Name>, Alloc<'a>>,
    /// Outline jobs (`Units`, `UnitGraph`, `FileItems`) not settled: an `Owners` with no row
    /// waits for zero, the one barrier allowed (outlines only, never a scope).
    outlines_open: usize,
    /// `Owners` jobs waiting for `outlines_open` to reach zero.
    owners_waiting: AVec<'a, Name>,
    /// Each `Owners` name's roots known this run: computed here, or read from its record.
    owner_roots: Table<Name, AVec<'a, OwnerRoot>, Alloc<'a>>,
    /// `Owners` jobs whose record the store lacked at its value key: computed again, it is
    /// made (`owners`, `valued`).
    owners_missed: Set<Name, Alloc<'a>>,
    /// `Owners` records being read for their roots, and the jobs waiting on each.
    roots_loading: Table<Name, AVec<'a, Name>, Alloc<'a>>,
    /// A file's body parts not made jobs yet, each with its home: made once the file's owners
    /// are known and one of them is asked (a member's); any other body runs only when Needed.
    gated: Table<Name, AVec<'a, (Name, Name)>, Alloc<'a>>,
    /// `run`'s member closure ([`Request::with_member_closure`]): only the crate roots it lists
    /// are asked. `None`: every member's are.
    closure: Option<Closure<'a>>,
    /// Standing homes whose records are being read for their parts' locates, with the jobs
    /// waiting; and the homes read so.
    parts_loading: Table<Name, AVec<'a, Name>, Alloc<'a>>,
    parts_read: Set<Name, Alloc<'a>>,
    /// Readers posted this run: read name to the names that read it (a name settled twice
    /// posts twice; `dirty_readers` dedups).
    posted: Table<Name, Vec<Name, Alloc<'a>>, Alloc<'a>>,
    /// Cell readers posted this run: read name to (reader, cell, what it saw).
    posted_cells: Table<Name, Vec<CellReader, Alloc<'a>>, Alloc<'a>>,
    /// Each kind's cell lookup ([`Request::with_cells`]).
    cells: Cells,
    /// Today's digest of a name's cell, with the record (`at`) it was looked up in: valid while
    /// the name is settled at that record.
    cells_now: Table<CellAt, (Key, ContentId), Alloc<'a>>,
    /// Settled records being read for cells (`Purpose::Cells`): the cells wanted and the jobs
    /// waiting.
    cells_loading: Table<Name, (AVec<'a, Cell>, AVec<'a, Name>), Alloc<'a>>,
    /// The ask's facts ([`Ask::facts`]), owned; checked sorted at `Start`.
    facts: AVec<'a, (Name, ContentId)>,
    /// `Given` names settled with no answer yet (no fact, no sweep row, never joined to the leaf
    /// table): a value key over one is a miss, never `ABSENT`; the first task's read of it this
    /// command answers it (`given_read`).
    given_unknown: Set<Name, Alloc<'a>>,

    jobs: Table<Name, Job<'a>, Alloc<'a>>,
    /// Jobs settled: `jobs.len() - settled` is how many are not (no scan).
    settled: usize,
    wake: VecDeque<Name, Alloc<'a>>,
    gets: Table<Key, Vec<Purpose, Alloc<'a>>, Alloc<'a>>,
    tickets: Table<Ticket, (Name, BatchId), Alloc<'a>>,
    /// Each running job's inputs as `emit` handed them (base name, out), sorted: a read of a
    /// handed name must be at the out it was handed (`made_split`, [`DriveError::ReadNotHanded`]).
    /// Inserted at `emit`, removed when the run answers: it holds only what is out.
    handed: Table<Name, Vec<(Name, ContentId), Alloc<'a>>, Alloc<'a>>,
    /// The last two entries of each job within two of `most_entries` (and only those: no room
    /// is spent on a job that settles early), for [`DriveError::Reentries`].
    near_limit: Table<Name, [EntryNeeds; 2], Alloc<'a>>,
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
    /// The program's `Native` record (`program_name`'s), standing or made this run, or read from
    /// the certificate: a repeat `run` takes the image at this key with no walk.
    native_at: Option<Key>,
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
    /// The answer's findings that say an item was not checked ([`Request::with_not_checked`]),
    /// counted with `errors` when the answer is printed: under `Before` one keeps the program
    /// from starting ([`Request::may_start`]).
    unchecked: u32,
    /// Whether a finding of this rule says its item was not checked: the command's policy, beside
    /// [`Config::is_error`] (findings carry no level, rule 81).
    is_unchecked: fn(u16) -> bool,
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

/// A request's not-checked rule before [`Request::with_not_checked`]: no finding says its item
/// was not checked.
fn no_rule(_: u16) -> bool {
    false
}

/// The digest domain of a request's shape.
const SHAPE_DOMAIN: &str = "blaze-drive request shape 1";
const GROUP_DOMAIN: &str = "blaze-drive group 1";
/// 2: each finding's item carries its base and condition.
/// BDC4: the program's `Native` record after the units'. BDC3 had none; BDC2 had no root.
pub(crate) const CERT_MAGIC: &[u8; 4] = b"BDC4";

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
            program_name: ask.program,
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
            parents: Table::new_in(a.clone()),
            outlines_open: 0,
            owners_waiting: Vec::new_in(a.clone()),
            owner_roots: Table::new_in(a.clone()),
            owners_missed: Set::new_in(a.clone()),
            roots_loading: Table::new_in(a.clone()),
            gated: Table::new_in(a.clone()),
            closure: None,
            parts_loading: Table::new_in(a.clone()),
            parts_read: Set::new_in(a.clone()),
            posted: Table::new_in(a.clone()),
            posted_cells: Table::new_in(a.clone()),
            cells: Cells::NONE,
            cells_now: Table::new_in(a.clone()),
            cells_loading: Table::new_in(a.clone()),
            facts: Vec::from_slice_in(ask.facts, a.clone()),
            given_unknown: Set::new_in(a.clone()),
            jobs: Table::new_in(a.clone()),
            settled: 0,
            wake: VecDeque::new_in(a.clone()),
            gets: Table::new_in(a.clone()),
            tickets: Table::new_in(a.clone()),
            handed: Table::new_in(a.clone()),
            near_limit: Table::new_in(a.clone()),
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
            native_at: None,
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
            unchecked: 0,
            is_unchecked: no_rule,
            failures: Vec::new_in(alloc.clone()),
            alloc,
        }
    }

    /// The request with the command's rule for a finding that says its item was not checked
    /// (blaze-cli's `types-not-checked`, what makes `check` exit 4): under [`Gate::Before`] the
    /// program starts only over an answer that holds none (`Request::may_start`). Without it
    /// no finding says so: a caller whose check cannot leave an item unchecked (the sims of one
    /// stage) gives none.
    pub fn with_not_checked(mut self, is_unchecked: fn(u16) -> bool) -> Request<'a> {
        self.is_unchecked = is_unchecked;
        self
    }

    /// `run`'s scope (run_scope.rs): only the crates of `package` (the program package's `Units`
    /// name, [`Name::manifest`] of its manifest) and of the workspace members it depends on,
    /// as their `Units` records list them, are asked; any other member's bodies are never made,
    /// and their errors do not hold the start. `check` and `test` ignore it: they ask every
    /// member. Given only once the members' `Units` records list their member dependencies:
    /// without those listings the closure is the package alone. Not in the shape: whether a
    /// command gives it is fixed by the binary, and the whole tool is in every certificate's
    /// and pointer's key, so no answer of one form is read by the other.
    pub fn with_member_closure(mut self, package: Name) -> Request<'a> {
        if self.command == Command::Run {
            self.closure = Some(Closure::new_in(package, self.plant == Plant::ReachedSkipped, self.alloc));
        }
        self
    }

    /// The gate's one fact (rule 30): the program may start. No error-level finding, no failed
    /// task, and under `Before` no item the check could not check: a check that left an item out
    /// is not a check the program may start over. Under `Beside` and `Reached` an unchecked item
    /// does not hold the start (the check runs beside, or only what runs is checked).
    fn may_start(&self) -> bool {
        let complete = self.gate != Gate::Before || self.unchecked == 0 || self.plant == Plant::StartOverIncomplete;
        self.errors == 0 && self.failures.is_empty() && complete
    }

    /// The request with each kind's cell lookup (cells.rs): the kinds whose records are read by
    /// part. Every kind not given is read whole only.
    pub fn with_cells(mut self, cells: Cells) -> Request<'a> {
        self.cells = cells;
        self
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

    /// The units or the program's image settled: once both are known, under `Beside` and
    /// `Reached` the program starts now, unless an error is already found.
    fn program_known(&mut self, out: &mut AVec<'_, Effect<'a>>) {
        let (Some(units), Some(native)) = (self.units_at, self.native_at) else { return };
        if self.runs_program()
            && self.gate != Gate::Before
            && self.program == Program::Unstarted
            && self.errors == 0
            && self.failures.is_empty()
        {
            self.program = Program::Running;
            out.push(Effect::StartProgram { units, native });
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
            // Before: the check is clean of errors and left an item unchecked (its findings are
            // printed, each by cause): the program is not started over it.
            (true, Program::Unstarted) if !self.may_start() => Verdict::Failed { errors: self.errors, started: false },
            (true, Program::Unstarted) => match (answer.units, self.native_at) {
                // Before: the check is clean, the program starts now.
                (Some(units), Some(native)) => {
                    self.program = Program::Running;
                    out.push(Effect::StartProgram { units, native });
                    return;
                }
                // The walk is done and a key the start needs has no record: the run fails by
                // name, never starts on a default key.
                _ => {
                    let name = self.program_name.unwrap_or_else(Name::units);
                    self.failures.push(TaskFailure { name, why: Why::Refused { what: "a program to start whose units or native image has no record", rule: "run" } });
                    Verdict::Failed { errors: self.errors, started: false }
                }
            },
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

    /// The content id the sweep gave the file [`Request::file_of`] names: what a task reading
    /// that file by path compares its bytes with (`verified::verify`).
    pub fn file_content_of(&self, task: &Task<'_>) -> Option<ContentId> {
        let index = match task.file {
            Some(index) => index as usize,
            None => self.file_index(&file_of(task.base, task.home).subject)?,
        };
        Some(self.files.ids()[index])
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

    /// The record `name` stands at once the request has answered: made or kept this run, else
    /// the last answer's; `None` when no record holds it (never asked, or its task failed). How
    /// `test` reads its suites' `CrateScope` records after the check.
    pub fn settled_input(&self, name: Name) -> Option<Input> {
        let now = self.now.get(&name).copied().or_else(|| self.old(name).map(|row| Now { at: row.at(), out: row.out() }))?;
        Some(Input { name, at: now.at, out: now.out })
    }

    /// The request's certificate key, known after `Start`.
    pub fn certificate(&self) -> Key {
        self.cert
    }

    /// What cell `cell` of `name`'s record holds today (`Seen::digest`), when this run looked it
    /// up in the record `name` stands at now; `None` otherwise.
    pub fn cell_now(&self, name: Name, cell: Cell) -> Option<ContentId> {
        let at = self.settled_input(name)?.at;
        self.cells_now.get(&CellAt { name, cell }).filter(|(held, _)| *held == at).map(|(_, saw)| *saw)
    }

    /// The key of the program's native image (its `Native` record), once known: from the
    /// certificate on a repeat, else when the record stands or is made. `None` for a request
    /// that names no program.
    pub fn native_image(&self) -> Option<Key> {
        self.native_at
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
        if self.facts.windows(2).any(|w| w[0].0 >= w[1].0) {
            return Err(DriveError::Unsorted { what: "the ask's facts" });
        }
        if let Some((name, _)) = self.facts.iter().find(|(name, _)| name.kind != Kind::Given) {
            return Err(DriveError::NotGiven { name: *name });
        }
        // A program starts from its units and its image (`Effect::StartProgram`): a request
        // that would start one names the image it starts.
        if self.runs_program() && self.program_name.is_none() {
            return Err(DriveError::NoNativeImage { program: None });
        }
        // Answered before the store: a reader's value key holds the fact as the command gives it.
        for (name, value) in self.facts.iter() {
            self.now.insert(*name, Now { at: Key::default(), out: *value });
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
        // Absence is omission: a gone path is in no certificate key, as an answer's absent read
        // is in none (`finish`), so a key names exactly the files present.
        let mut swept = Vec::with_capacity_in(files.len(), self.alloc);
        swept.extend(files.iter().map(|f| (Name::file(f.path).subject, f.id)));
        self.cert = self.cert_of(&mut swept);
        drop(swept);
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

    /// The certificate key of `files`, each a file's subject ([`Name::file`]) and its content id
    /// (sorted here, by subject): the tool, the command and its shape, the units' source, the
    /// ask's facts when it has any, and each (subject, id). By subject, not path, so a file the
    /// answer read by content only ([`key::content_of`], a `Given` name: its subject and no
    /// path) is keyed as the sweep's row of the same path is.
    fn cert_of(&self, files: &mut [(ContentId, ContentId)]) -> Key {
        files.sort_unstable_by(|x, y| x.0.cmp(&y.0));
        let mut command = [0u8; 97];
        command[0] = self.command.byte();
        command[1..33].copy_from_slice(&self.shape_digest.0);
        command[33..65].copy_from_slice(&self.units_source.0);
        // An ask with no facts keys as before they existed: its certificates stand.
        let len = match self.facts.is_empty() {
            true => 65,
            false => {
                command[65..].copy_from_slice(&key::facts_digest(&self.facts).0);
                97
            }
        };
        key::cert_key(self.config.tool, &command[..len], files.iter().map(|(subject, id)| (&subject.0[..], id)))
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
        // A file read by content only (`key::content_of`) is held by its `Given` name: a changed
        // one has no outline job, and dropping it would leave its readers' file unfollowed.
        self.now.contains_key(&name) || self.now.contains_key(&Name::new(Kind::Given, name.subject)) || !self.files.changeds()[i]
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
        self.profile.forward = self.forward;
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
            // What the command gives, each a `Given` seed: a changed or gone file's content (a
            // reader by content only, `key::content_of`, is the same subject's) and every fact.
            // Each compares today's with its watch row's `out` and dirties its readers only
            // when it moved (`given`).
            let mut given = Vec::with_capacity_in(seeds.len() + self.facts.len(), self.alloc);
            if self.plant != Plant::GivenNotSeeded {
                given.extend(seeds.iter().map(|seed| Name::new(Kind::Given, seed.subject)));
            }
            given.extend(self.facts.iter().map(|(name, _)| *name));
            for name in given.iter() {
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
        // The program's native image (run.md, "What a run takes from the request"): the
        // person's, as the program's first call waits on it. Located by its name; a repeat
        // stands on its row, or is found by value from its reads (the crate list, each closure
        // crate's `Links` and `Script`): only a moved read runs it.
        if self.runs_program()
            && let Some(native) = self.program_name
        {
            self.add_job(native, Name::unit_graph(), Some(Locate::Source(native.subject)), false, Class::Foreground)?;
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
        let (root, units, native, held) = read_cert(bytes, self.alloc).map_err(|error| DriveError::Wire { key: self.cert, error })?;
        self.native_at = native;
        self.profile.certified = true;
        // A `last` that names another answer (none, an older form's, or the tree's before an
        // undo) is replaced by this one's, so the next edit walks forward from this answer and
        // the leaf table written beside it matches the tree.
        let tool = self.config.tool.0;
        if self.last.is_none_or(|last| last.cert != self.cert || last.tool != tool || last.root != root) {
            let last = Last { tool, shape: self.shape_digest, units: self.units_source, cert: self.cert, root };
            out.push(Effect::WriteLast { bytes: answers::write_last(&last, self.alloc) });
        }
        let (count, errors, unchecked) = self.print(&held, out);
        self.errors = errors;
        self.unchecked = unchecked;
        self.units_at = units;
        // The repeat: the walk stands whole. A stored error refuses the start at this one read
        // (every gate), and under `Before` so does a stored unchecked item; a clean one starts
        // the program now, nothing left to check beside it, no task run.
        self.held = Some(Answer { profile: self.profile, verdict: Verdict::Checked, findings: count, units, failed: 0 });
        self.phase = Phase::Held;
        if self.runs_program() && self.may_start() {
            match (units, native) {
                (Some(units), Some(native)) => {
                    self.program = Program::Running;
                    out.push(Effect::StartProgram { units, native });
                }
                (None, _) => return Err(DriveError::NoProgram),
                (Some(_), None) => return Err(DriveError::NoNativeImage { program: self.program_name }),
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
        if self.plant == Plant::DropPutsOnFailure && !self.failures.is_empty() {
            return;
        }
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
                        self.old_findings = read_cert(bytes, self.alloc).map_err(|error| DriveError::Wire { key, error })?.3;
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
                            self.profile.forward = false;
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
                        Some(bytes) => HeldPage::open_in(bytes, self.alloc).map_err(|error| DriveError::Page { key, error })?,
                        None => HeldPage::empty(),
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
                Purpose::Roots(name) => self.roots_got(name, key, bytes, out)?,
                Purpose::Parts(home) => self.parts_got(home, key, bytes)?,
                Purpose::Cells(name) => self.cells_got(name, key, bytes)?,
            }
        }
        Ok(())
    }

    fn ran(&mut self, ticket: Ticket, ran: &Ran<'_>, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        let (name, _) = self.tickets.remove(&ticket).ok_or(DriveError::UnknownTicket(ticket))?;
        // Handed to this run only: the run is answered.
        let handed = self.handed.remove(&name);
        if let Some(job) = self.jobs.get(&name)
            && job.entries > self.config.most_entries
        {
            let last = self.near_limit.get(&name).copied().unwrap_or_default();
            return Err(DriveError::Reentries { name, key: job.key, entries: job.entries, last });
        }
        // Each read of a name it was handed is at the out it was handed (rule 21). A read of
        // one that moved since is stale and decided again (`made_split`); a read at an out it
        // was never handed is the task computing its input itself, which re-enters for ever.
        if let (Ran::Done(outputs), Some(handed)) = (ran, handed.as_ref()) {
            for output in outputs.iter().filter(|output| output.name == name) {
                for (read, saw) in output.reads.iter() {
                    if read.kind == Kind::Given {
                        continue;
                    }
                    if let Ok(at) = handed.binary_search_by_key(read, |(handed, _)| *handed) {
                        let was = handed[at].1;
                        if was != *saw {
                            return Err(DriveError::ReadNotHanded { by: name, read: *read, saw: *saw, handed: was });
                        }
                    }
                }
            }
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
                // The needs it was handed: sorted at the last answer, untouched by `emit`.
                let handed_needs = self.jobs.get(&name).map_or(0, |job| job.needs.len());
                for want in wants {
                    if want.name == name {
                        return Err(DriveError::SelfNeed { name });
                    }
                    if let Some(last) = self.near_limit.get_mut(&name) {
                        let new = self.jobs.get(&name).is_some_and(|job| job.needs[..handed_needs].binary_search(&want.name).is_err());
                        last[1].push(want.name, new);
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
        // An `Owners` is located by its subject and homed at its file, however it was asked
        // (a body's want, a gate, a child's owners, a dirtied part).
        let (home, locate) = match name.kind {
            Kind::Owners => (owners::file_of(name), locate.or(Some(Locate::Source(name.subject)))),
            _ => (home, locate),
        };
        if outline(name.kind) {
            self.outlines_open += 1;
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
                stood: false,
                queue_gen: 0,
                parked: false,
                needs: Vec::new_in(a.clone()),
                shape: Vec::new_in(a.clone()),
                shape_cells: Vec::new_in(a.clone()),
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
    /// Settling frees the job's needs: a later run of it is handed its record's reads as
    /// hints (`to_run`), never these. Kept, they were the clone's largest holder: 5181M in
    /// 111,403 lists at 45 s (RUNNER 17, `malloc_history`, 2026-11-16).
    fn set_state(&mut self, name: Name, state: State) {
        let (alloc, keep) = (self.alloc, self.plant == Plant::KeepSettledNeeds);
        let mut closed = false;
        if let Some(job) = self.jobs.get_mut(&name) {
            match (job.state == State::Settled, state == State::Settled) {
                (false, true) => {
                    self.settled += 1;
                    closed = true;
                    if !keep {
                        job.needs = Vec::new_in(alloc);
                    }
                }
                (true, false) => {
                    self.settled -= 1;
                    if outline(name.kind) {
                        self.outlines_open += 1;
                    }
                }
                _ => {}
            }
            job.state = state;
        }
        if closed && outline(name.kind) {
            self.outline_closed();
        }
    }

    /// An outline job settled or left: at zero open, the `Owners` waiting on the walk go on.
    fn outline_closed(&mut self) {
        self.outlines_open -= 1;
        if self.outlines_open == 0 {
            let waiting = core::mem::replace(&mut self.owners_waiting, Vec::new_in(self.alloc));
            for name in waiting.iter() {
                self.wake.push_back(*name);
            }
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
        match self.current(name).and_then(|row| row.cond()) {
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
            let Some(var) = self.current(at).and_then(|row| row.split()) else { return Ok(Resolved::At) };
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
            let Some(var) = self.current(at).and_then(|row| row.split()) else { return at };
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
                cells: &[],
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
        let output = Output { name, out: h.finish(), reads: &space_read, cells: &[], parts: &parts, paths: &[], findings: &[], payload: &[] };
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
    /// Room the settled jobs' need lists still hold, in names: zero once settling frees them.
    pub fn kept_needs(&self) -> usize {
        self.jobs.iter().filter(|(_, job)| job.state == State::Settled).map(|(_, job)| job.needs.capacity()).sum()
    }

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
                        self.pages.insert(index, PageState::Loaded(HeldPage::empty()));
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
    fn old(&self, name: Name) -> Option<RowView<'_>> {
        self.old_or_watch(name).filter(|row| !row.watching())
    }

    /// The old row of `name` or its watch row ([`Row::watching`]): for its readers. In place in
    /// its page: a search of the row table, no copy.
    fn old_or_watch(&self, name: Name) -> Option<RowView<'_>> {
        self.held_page(name)?.view().row(name)
    }

    /// The loaded page that holds `name`'s old row, if any.
    fn held_page(&self, name: Name) -> Option<&HeldPage<'a>> {
        let root = self.root.as_ref()?;
        match self.pages.get(&answers::page_of(name, root.bits))? {
            PageState::Loaded(page) => Some(page),
            PageState::Loading => None,
        }
    }

    /// The row this run knows for `name`: made this run, else the old one.
    fn current(&self, name: Name) -> Option<RowRef<'_, 'a>> {
        match self.new.get(&name) {
            Some(row) => (!row.watching()).then_some(RowRef::Made(row)),
            None => self.old(name).map(RowRef::Old),
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
        Digest::Ready(self.old(read).map(|row| Now { at: row.at(), out: row.out() }))
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

    /// The next live entry of ring `r`, left in the ring; stale ones before it dropped.
    fn peek_ready(&mut self, r: usize) -> Option<Name> {
        // Drops exactly what `pop_ready` would skip, the same way, so the `pop_ready` after a
        // peek returns the peeked name.
        while let Some(&(name, generation)) = self.ready[r].front() {
            let current = self.jobs.get(&name).is_some_and(|job| job.queue_gen == generation);
            if current && self.jobs.get(&name).is_some_and(|job| job.state == State::Queued) {
                return Some(name);
            }
            self.ready[r].pop_front();
            self.ready_ops += 1;
            if current && let Some(job) = self.jobs.get_mut(&name) {
                job.queue_gen = job.queue_gen.wrapping_add(1);
                self.ready_live[r] -= 1;
                self.queued -= 1;
            }
        }
        None
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
            let mut tasks;
            if kind.batches_by_home() {
                // One home's ready tasks, whole (up to MOST_BATCH): the runner's per-home setup
                // is paid once a batch, so the batch is cut where the home changes, not by time.
                let Some(first) = self.pop_ready(r) else { return };
                let home = self.jobs.get(&first).map(|job| job.home);
                tasks = Vec::with_capacity_in(of_kind.min(MOST_BATCH), self.alloc);
                tasks.push(self.emit(first));
                while tasks.len() < MOST_BATCH {
                    let Some(next) = self.peek_ready(r) else { break };
                    if self.jobs.get(&next).map(|job| job.home) != home {
                        break;
                    }
                    let Some(name) = self.pop_ready(r) else { break };
                    tasks.push(self.emit(name));
                }
            } else {
                let by_time = usize::try_from(TARGET_BATCH_NS / self.task_ns(kind)).unwrap_or(MOST_BATCH);
                let size = by_time.clamp(1, MOST_BATCH).min(of_kind.div_ceil(free)).max(1);
                tasks = Vec::with_capacity_in(size, self.alloc);
                while tasks.len() < size {
                    let Some(name) = self.pop_ready(r) else { break };
                    tasks.push(self.emit(name));
                }
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
            match self.now.get(&leaf).copied().or_else(|| self.old(leaf).map(|row| Now { at: row.at(), out: row.out() })) {
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
        let mut handed = Vec::with_capacity_in(task.inputs.len(), a.clone());
        handed.extend(task.inputs.iter().map(|input| (input.name, input.out)));
        self.handed.insert(name, handed);
        let job = self.jobs.get_mut(&name).expect("a queued job is a job");
        job.entries += 1;
        // Within two of the limit, this entry's needs are kept for `Reentries`.
        if job.entries.max(job.settles).saturating_add(2) >= self.config.most_entries {
            let mut entry = EntryNeeds { entry: job.entries, ..EntryNeeds::default() };
            for need in task.inputs.iter().map(|input| input.name).chain(task.failed.iter().copied()) {
                entry.push(need, false);
            }
            let before = self.near_limit.get(&name).map_or(EntryNeeds::default(), |last| last[1]);
            self.near_limit.insert(name, [before, entry]);
        }
        let job = self.jobs.get_mut(&name).expect("a queued job is a job");
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
        if name.kind == Kind::Given {
            return self.given(name);
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
            match self.old(name).map(|row| (row.source(), row.place(), row.home())) {
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
                    // It is in `Start`, so the settled count is untouched; it was an open outline.
                    self.jobs.remove(&name);
                    if outline(name.kind) {
                        self.outline_closed();
                    }
                    return Ok(());
                }
                None if home != name && self.state(home).is_none() => {
                    let class = self.jobs.get(&name).map_or(self.walk, |job| job.class);
                    self.add_job(home, home, None, false, class)?;
                    self.wait_on(home, name);
                    return Ok(());
                }
                // A part its home's row lists, asked only now (a body made a job by a need) of a
                // home that stood: the home's record says where it is.
                None if self.parts_unread(home, name) => {
                    self.read_parts(home, name, out);
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
                    // The dependency graph wanted (a `Native` task's first need) by a command
                    // whose `Start` gave no graph source and whose last answer has no row of
                    // it: no job can make it, so the want fails alone, by name (rule 20), and
                    // the wanter stops `Waited` on it; never the whole request as a `NoPart` of
                    // the graph by itself (native_image_tests.rs, 2026-11-17).
                    None if name == Name::unit_graph() => {
                        self.failures.push(TaskFailure {
                            name,
                            why: Why::Refused { what: "the dependency graph, which this command has no source for (`Start` gave no `graph`: no lock was read with the layout)", rule: "no-fallback" },
                        });
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
            Locate::Source(source) => (source, key::task_key(self.config.tool_of(name.kind), name, &source)),
            Locate::Key(key) => (key.0, key),
            // A fork of a file's task reads its base's file.
            Locate::File { edition } => match self.file_index(&self.cond_of(name).0.subject) {
                Some(index) => {
                    let source = key::file_source(self.files.ids()[index], edition);
                    (source, key::task_key(self.config.tool_of(name.kind), name, &source))
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
        // A body's read names follow from its owners (it reads their crates' scopes), which no
        // text of its own holds: its key holds its file's `Owners` out, so one key never names
        // two read lists (rule 21) and a body whose owners moved is a new key.
        let key = match name.kind {
            Kind::BodyCheck | Kind::Signature => {
                let home = self.jobs.get(&name).expect("a job").home;
                // Its file's Surface remade this request: when it settles moved, its crates'
                // scopes are dirtied (their jobs made) before this body reads them, so it never
                // runs over a scope about to be remade and then again (PIPELINE runner 7,
                // owners_tests.rs). Owed: an edit in ANOTHER file of the crate still races.
                let surface = Name::new(Kind::Surface, self.cond_of(home).0.subject);
                if self.state(surface).is_some_and(|state| state != State::Settled) {
                    self.wait_on(surface, name);
                    return Ok(());
                }
                let owned = owners::of(self.cond_of(home).0);
                if self.state(owned).is_none() {
                    self.add_job(owned, home, None, false, self.walk)?;
                }
                let owners_out = match self.digest(owned, name, out) {
                    Digest::Wait => return Ok(()),
                    Digest::Ready(now) => now.map_or(ABSENT, |now| now.out),
                };
                // Which crates those owners are: the crate list's out (key.rs `owned_key`).
                // Asked after the Owners, whose roots are parts of the `UnitGraph` record, so a
                // graph remade this request has settled; no graph this command: absent.
                let graph = match self.digest(Name::unit_graph(), name, out) {
                    Digest::Wait => return Ok(()),
                    Digest::Ready(now) => now.map_or(ABSENT, |now| now.out),
                };
                key::owned_key(key, graph, owned, owners_out)
            }
            _ => key,
        };
        let job = self.jobs.get_mut(&name).expect("a job");
        job.locate = Some(locate);
        job.place = place;
        job.source = source;
        job.key = key;
        job.state = State::Decide;
        self.decide(name, out)
    }

    /// What the command gives for the `Given` name `name`, from what needs no page: a fact or a
    /// task's read this command (`now`), the sweep's row of the file of its subject, a file gone
    /// since the last answer. `Some(None)`: absent; `None`: not known from these.
    fn given_known(&self, name: Name) -> Option<Option<ContentId>> {
        if let Some(now) = self.now.get(&name) {
            return Some(Some(now.out));
        }
        if let Some(index) = self.file_index(&name.subject) {
            return Some(Some(self.files.ids()[index]));
        }
        if self.gone.contains(&name.subject) {
            return Some(None);
        }
        None
    }

    /// [`Request::given_known`], then the old table (its page loaded): a name with a watch row
    /// was read last time, so its file was joined to the leaf table, which the sweep follows
    /// whole; not in today's sweep, it is absent. No row either: not known.
    fn given_today(&self, name: Name) -> Option<Option<ContentId>> {
        self.given_known(name).or_else(|| self.old_or_watch(name).map(|_| None))
    }

    /// A `Given` job (`Kind::Given`): never a task and never a record. It settles at once with
    /// what the command gives ([`Request::given_today`]); one not known settles in
    /// `given_unknown`. A seed (a changed or gone file, a fact) dirties its readers when that is
    /// not what its watch row's `out` says the last answer read.
    fn given(&mut self, name: Name) -> Result<(), DriveError> {
        let seed = self.jobs.get(&name).expect("a job").seed;
        let today = self.given_today(name);
        match today {
            Some(Some(out)) => {
                self.now.insert(name, Now { at: Key::default(), out });
            }
            Some(None) => {
                self.now.remove(&name);
            }
            None => {
                self.given_unknown.insert(name);
            }
        }
        self.set_state(name, State::Settled);
        self.wake_waiters(name);
        let was = self.old_or_watch(name).map(|row| row.out());
        let moved = match (today, was) {
            (Some(today), Some(was)) => today.unwrap_or(ABSENT) != was,
            _ => false,
        };
        if seed && moved {
            self.dirty_readers(name)?;
        }
        Ok(())
    }

    /// `by`'s task read the `Given` name `name` as `saw`. The first read this command of one
    /// with no answer (`given_unknown`, or not asked yet: a run with no shape) answers it, so
    /// every later reader this command sees the same bytes; any other read must be what the
    /// command gives ([`DriveError::GivenDiffers`]), never decided again.
    fn given_read(&mut self, name: Name, saw: ContentId, by: Name) -> Result<(), DriveError> {
        // Its job, made now if nothing asked it (a run with no shape, a fact), settles before
        // the finish, so the answer's watch row keeps what the command gave (`finish`).
        if self.state(name).is_none() {
            self.add_job(name, name, None, false, self.walk)?;
        }
        let settled = self.state(name) == Some(State::Settled);
        // An absence read is held too (out `ABSENT`): the path was looked at, so the file's
        // appearing moves the reader.
        if settled && self.given_unknown.remove(&name) {
            self.now.insert(name, Now { at: Key::default(), out: saw });
            return Ok(());
        }
        let held = match settled {
            true => Some(self.now.get(&name).map(|now| now.out)),
            false => self.given_known(name),
        };
        match held {
            Some(held) if held.unwrap_or(ABSENT) == saw => Ok(()),
            Some(held) => Err(DriveError::GivenDiffers { name, by, saw, held }),
            None => {
                // Its job, when it locates, finds this answer first (`given_known`).
                self.now.insert(name, Now { at: Key::default(), out: saw });
                Ok(())
            }
        }
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
        job.key = key::task_key(self.config.tool_of(name.kind), name, &source);
        let findings = [Finding { rule, start: 0, end: 0, message: &message }];
        // A data file (`include_str!`, `include_bytes!`, [`crate::record::DATA_FILE`]) that is
        // not there is no finding of the drive's: the expansion that reads it reports it at its
        // call, and a call the cfg turns off reads nothing and reports nothing, as rustc.
        let findings: &[Finding<'_>] = if edition == crate::record::DATA_FILE { &[] } else { &findings };
        let output = Output { name, out: source, reads: &[], cells: &[], parts: &[], paths: &[], findings, payload: &[] };
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
        // An `Owners` row stands unless dirtied (a parent added or removed, a parent's row
        // moved); else the machine computes it, never from the store: its parents are not in
        // its key, so a record found by value could be another checkout's tree.
        if name.kind == Kind::Owners {
            if !recheck && self.current(name).is_some_and(|row| row.key() == key) {
                return self.stand(name, out);
            }
            self.set_state(name, State::Gather);
            return self.owners(name, out);
        }
        // A split (or an over-cap finding) read the space: under another space its forks are
        // others, so it stands only on the same space. No reader of the space is posted (it is
        // no job), so this is where that read is checked.
        let (space_name, space_digest) = (self.space_name, self.space_digest);
        let row = self.current(name).map(|row| {
            let reads = row.reads();
            let other_space = reads.binary_search_by_key(&space_name, |(n, _)| n).is_ok_and(|at| reads.get(at).1 != space_digest);
            let cells: AVec<'a, (Name, Cell)> = row.cells().iter().map(|read| (read.name, read.cell)).collect_in(self.alloc);
            (row.key(), other_space, (reads.iter().map(|(n, _)| n).collect_in(self.alloc), cells))
        });
        match row {
            Some((held, false, _)) if held == key && !recheck => self.stand(name, out),
            Some((held, _, (reads, cells))) if held == key && !(reads.is_empty() && cells.is_empty()) => {
                let job = self.jobs.get_mut(&name).expect("a job");
                job.shape = reads;
                job.shape_cells = cells;
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
        // A `Given` read with no answer yet makes the value key a miss: the task runs, and its
        // read answers it (`given_read`). Never `ABSENT`: a file the sweep never saw may be there.
        let mut miss = false;
        let mut given_jobs = Vec::new_in(self.alloc);
        for read in shape.iter() {
            if read.kind == Kind::Given {
                match self.state(*read) {
                    None => {
                        given_jobs.push(*read);
                        waiting = true;
                    }
                    Some(State::Settled) if self.given_unknown.contains(read) => miss = true,
                    Some(State::Settled) => reads.push((*read, self.now.get(read).map_or(ABSENT, |now| now.out))),
                    Some(_) => {
                        self.wait_on(*read, name);
                        waiting = true;
                    }
                }
                continue;
            }
            match self.digest(*read, name, out) {
                Digest::Ready(None) if unanswered && *read != self.space_name && self.state(*read).is_none() && !self.dropped.contains(read) => unlisted = true,
                Digest::Ready(now) => reads.push((*read, now.map_or(ABSENT, |now| now.out))),
                Digest::Wait => waiting = true,
            }
        }
        let shape_cells = core::mem::replace(&mut self.jobs.get_mut(&name).expect("a job").shape_cells, Vec::new_in(self.alloc));
        let mut cells = Vec::with_capacity_in(shape_cells.len(), self.alloc);
        for (read, cell) in shape_cells.iter() {
            let now = match self.digest(*read, name, out) {
                Digest::Ready(None) if unanswered && self.state(*read).is_none() && !self.dropped.contains(read) => {
                    unlisted = true;
                    continue;
                }
                Digest::Ready(now) => now,
                Digest::Wait => {
                    waiting = true;
                    continue;
                }
            };
            match self.cell_digest(*read, *cell, now, name, out) {
                Ok(Some(saw)) => cells.push(CellRead { name: *read, cell: *cell, saw }),
                Ok(None) => waiting = true,
                Err(error) => {
                    self.jobs.get_mut(&name).expect("a job").shape_cells = shape_cells;
                    return Err(error);
                }
            }
        }
        let job = self.jobs.get_mut(&name).expect("a job");
        job.shape = shape;
        job.shape_cells = shape_cells;
        if waiting {
            job.state = State::Value;
            for given in given_jobs.iter() {
                self.add_job(*given, *given, None, false, self.walk)?;
                self.wait_on(*given, name);
            }
            return Ok(());
        }
        if miss {
            if let Ok(released) = self.walked.binary_search(&name) {
                self.walked.remove(released);
            }
            return self.to_run(name, out);
        }
        if unlisted {
            job.state = State::Value;
            self.unwalked.push(name);
            return Ok(());
        }
        let at = key::value_key_with_cells(job.key, &reads, &cells);
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
                let mut cells = Vec::with_capacity_in(shape.cells().len(), self.alloc);
                cells.extend(shape.cells());
                let job = self.jobs.get_mut(&name).expect("a job");
                job.shape = names;
                job.shape_cells = cells;
                job.state = State::Value;
                self.value(name, out)
            }
        }
    }

    fn valued(&mut self, name: Name, key: Key, bytes: Option<&[u8]>, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        if self.state(name) != Some(State::Valuing) {
            return Ok(());
        }
        // An `Owners` record the store lacks is made by the machine, not run (`owners`).
        if bytes.is_none() && name.kind == Kind::Owners {
            self.owners_missed.insert(name);
            self.set_state(name, State::Gather);
            return self.owners(name, out);
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
        let hints = self.current(name).map(|row| {
            // What the command gives is no record to hand: the task reads it as given.
            let mut hints: AVec<'a, Name> = row
                .reads()
                .iter()
                .filter(|(n, saw)| n.kind != Kind::Given && (*saw != ABSENT || self.plant == Plant::HintAbsentReads))
                .map(|(n, _)| n)
                .collect_in(self.alloc);
            // A cell read is of a record: the record is the hint, whatever the cell held (an
            // absent cell of a record that is there is a read of that record).
            hints.extend(row.cells().iter().filter(|read| read.saw != ABSENT || self.now.get(&read.name).is_some()).map(|read| read.name));
            hints
        });
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
        if name.kind == Kind::Owners {
            return self.owners(name, out);
        }
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
            let last = self.near_limit.get(&name).copied().unwrap_or_default();
            return Err(DriveError::Reentries { name, key: job.key, entries: job.entries, last });
        }
        job.state = State::Queued;
        let class = job.class;
        self.queue(class, name);
        Ok(())
    }

    fn stand(&mut self, name: Name, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        self.profile.of(name.kind).standing += 1;
        self.jobs.get_mut(&name).expect("a job").stood = true;
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
        // What the command gives, read by the task: the first read of one with no answer answers
        // it; any other must agree.
        for (read, saw) in reads.iter().filter(|(read, _)| read.kind == Kind::Given) {
            self.given_read(*read, *saw, output.name)?;
        }
        // A read whose digest is not today's makes a record valid for its own reads, stored,
        // but not the answer: the job is decided again.
        let mut stale = false;
        for (read, saw) in reads.iter() {
            match self.now.get(read) {
                Some(now) if now.out == *saw => {}
                None if *saw == ABSENT || self.old(*read).is_some_and(|row| row.out() == *saw) => {}
                // Checked against what the command gives by `given_read` above.
                _ if read.kind == Kind::Given => {}
                _ => stale = true,
            }
        }
        let mut cells = Vec::with_capacity_in(output.cells.len(), self.alloc);
        for read in output.cells {
            if self.cells.of(read.name.kind).is_none() {
                return Err(DriveError::NoCells { name: read.name, by: output.name });
            }
            cells.push(CellRead { name: self.resolve_settled(read.name, &under), cell: read.cell, saw: read.saw });
        }
        cells.sort_unstable_by_key(|read| (read.name, read.cell));
        cells.dedup_by_key(|read| (read.name, read.cell));
        // A cell read is today's when the cell's digest was looked up in the record the name is
        // settled at now; one not looked up yet is decided again, by value, which looks it up.
        for read in cells.iter() {
            let today = match self.now.get(&read.name) {
                Some(now) => self.cells_now.get(&CellAt { name: read.name, cell: read.cell }).filter(|(at, _)| *at == now.at).map(|(_, saw)| *saw),
                None if self.old(read.name).is_none() => Some(ABSENT),
                None => None,
            };
            if today != Some(read.saw) {
                stale = true;
            }
        }
        let whole = Whole {
            name: output.name,
            source,
            out: output.out,
            reads: &reads,
            cells: &cells,
            parts: output.parts,
            paths: output.paths,
            findings: output.findings,
            payload: output.payload,
            split,
        };
        let bytes = record::write_whole(&whole, self.alloc);
        let at = match reads.is_empty() && cells.is_empty() {
            true => key,
            // An `Owners` key holds no text its parents follow from, so its read names move
            // under one key: no shape there (it is never looked up by one, `decide`), only the
            // record at its value key, which its reads make unique.
            false if output.name.kind == Kind::Owners => key::value_key_with_cells(key, &reads, &cells),
            false => {
                let shape = record::write_shape(output.name, &reads, &cells, self.alloc);
                self.put(key, shape, out);
                key::value_key_with_cells(key, &reads, &cells)
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
        let before = self.now.get(&name).copied().or_else(|| self.old(name).map(|row| Now { at: row.at(), out: row.out() }));
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
            let last = self.near_limit.get(&name).copied().unwrap_or_default();
            return Err(DriveError::Reentries { name, key, entries: settles, last });
        }
        let mut parts_now = Vec::new_in(a.clone());
        let (base, condition) = self.cond_of(name);
        let cond = (condition != Condition::ANY).then_some((base, condition));
        let split = match &record {
            None => self.current(name).and_then(|row| row.split()),
            Some((_, view)) => view.split,
        };
        if let Some(var) = split {
            // Its parts are its forks: each runs under its condition, walked as any part.
            self.forks(name, var)?;
        }
        let now = match &record {
            None => {
                let row = self.current(name).expect("a standing job has a row");
                parts_now.extend(row.parts().iter());
                Now { at: row.at(), out: row.out() }
            }
            Some((at, view)) => {
                let old = self.current(name);
                // A name read absent before it existed keeps those readers (its watch row's): the
                // made row carries them to the page it is written into.
                let held = self.new.get(&name).map(RowRef::Made).or_else(|| self.old_or_watch(name).map(RowRef::Old));
                let readers = held.map(|row| row.readers().to_vec_in(a.clone())).unwrap_or_else(|| Vec::new_in(a.clone()));
                let cell_readers = held.map(|row| row.cell_readers().to_vec_in(a.clone())).unwrap_or_else(|| Vec::new_in(a.clone()));
                let old_parts = old.map(|row| row.parts().to_vec_in(a.clone())).unwrap_or_else(|| Vec::new_in(a.clone()));
                let mut reads = Vec::with_capacity_in(view.reads().len(), a.clone());
                reads.extend(view.reads());
                let mut cells = Vec::with_capacity_in(view.cells().len(), a.clone());
                cells.extend(view.cells());
                // Today's digest of each cell its readers read, looked up once a cell while the
                // bytes are in hand: what `dirty_readers` compares, and a value key reads.
                self.look_up_cells(name, *at, view.payload, &cell_readers)?;
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
                        // A file another home may still list stays: its `Owners`, dirtied,
                        // reads every parent and drops it only if none lists it now.
                        if gone.kind == Kind::FileItems {
                            self.unparent(*gone, name);
                            self.dirty_owners(*gone)?;
                            if self.plant != Plant::OwnersDropListedElsewhere && self.maybe_listed(*gone, name) {
                                continue;
                            }
                        }
                        // A member's `Units` that another member's record lists (a dependency,
                        // run_scope.rs) is the root units' part too: it goes only when the
                        // root's listing drops it.
                        if gone.kind == Kind::Units && name.kind == Kind::Units && name != Name::units() {
                            continue;
                        }
                        self.drop_name(*gone)?;
                    }
                }
                // A file this home lists anew has a new parent: its owners are computed again.
                if self.plant != Plant::OwnersAddedNotDirtied {
                    for added in parts.iter().filter(|part| part.kind == Kind::FileItems) {
                        if old_parts.binary_search(added).is_err() {
                            self.dirty_owners(*added)?;
                        }
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
                for read in cells.iter() {
                    let entry = CellReader { reader: name, cell: read.cell, saw: read.saw };
                    match self.posted_cells.get_mut(&read.name) {
                        Some(list) => list.push(entry),
                        None => {
                            let mut list = Vec::new_in(a.clone());
                            list.push(entry);
                            self.posted_cells.insert(read.name, list);
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
                    Row { key, at: *at, source, out: view.out, home, place, reads, cells, parts, readers, cell_readers, split, cond },
                );
                Now { at: *at, out: view.out }
            }
        };
        self.now.insert(name, now);
        self.dropped.remove(&name);
        if name == Name::units() {
            // Standing or made, the root units' record is where the program's entry is.
            self.units_at = Some(now.at);
            self.program_known(out);
        }
        if Some(name) == self.program_name {
            // Standing or made, the program's image: the certificate names it beside the units.
            self.native_at = Some(now.at);
            self.program_known(out);
        }
        self.note_parents(name, &parts_now);
        // A `Units` record, standing or made, is one listing of `run`'s closure: a member it
        // brings in releases the bodies held for it.
        if name.kind == Kind::Units
            && let Some(closure) = self.closure.as_mut()
            && closure.listed(name, &parts_now)
        {
            self.release_held(out)?;
        }
        // Every part of a record walked: a standing one stands on its row at no store read. A
        // file's bodies and lint units wait for its owners: a job only under an asked root
        // (`release`), so a dependency's are never made.
        let gates = name.kind == Kind::FileItems && self.plant != Plant::EveryPartAJob;
        let file = self.cond_of(name).0;
        for part in parts_now.iter() {
            // A part dropped earlier this run and listed again (its home made twice): it is a
            // part again, entered from its start to find where it is now; a dropped name is
            // otherwise never entered again (`dirty`).
            let revived = self.plant != Plant::DirtyDropped && self.dropped.remove(part);
            match self.state(*part) {
                None if gates && matches!(part.kind, Kind::BodyCheck | Kind::Signature | Kind::Lint) => self.gate(file, *part, name),
                None => self.add_job(*part, name, None, false, self.walk)?,
                Some(State::Settled) if revived => {
                    self.set_state(*part, State::Start);
                    let job = self.jobs.get_mut(part).expect("a job");
                    job.recheck = true;
                    job.locate = None;
                    job.home = name;
                    self.wake.push_back(*part);
                }
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
        match name.kind {
            Kind::FileItems => self.release(file, out)?,
            Kind::Owners => self.release(owners::file_of(name), out)?,
            _ => {}
        }
        Ok(())
    }

    // --- Owners (owners.rs) --------------------------------------------------------------------

    /// `home` lists `parts`: each file part has it as a parent (sorted, no repeat).
    fn note_parents(&mut self, home: Name, parts: &[Name]) {
        for part in parts.iter().filter(|part| part.kind == Kind::FileItems) {
            match self.parents.get_mut(part) {
                Some(list) if self.plant == Plant::OwnersLastParentWins => {
                    list.clear();
                    list.push(home);
                }
                Some(list) => {
                    if let Err(at) = list.binary_search(&home) {
                        list.insert(at, home);
                    }
                }
                None => {
                    let mut list = Vec::new_in(self.alloc);
                    list.push(home);
                    self.parents.insert(*part, list);
                }
            }
        }
    }

    /// `home` no longer lists `file`.
    fn unparent(&mut self, file: Name, home: Name) {
        if let Some(list) = self.parents.get_mut(&file)
            && let Ok(at) = list.binary_search(&home)
        {
            list.remove(at);
        }
    }

    /// `file`'s parents moved: its `Owners` is computed again. Only one this run made or one an
    /// old answer may hold: a first build computes each when asked, after the walk.
    fn dirty_owners(&mut self, file: Name) -> Result<(), DriveError> {
        let name = owners::of(self.cond_of(file).0);
        if self.jobs.contains_key(&name) || self.last.is_some() {
            self.dirty(name)?;
        }
        Ok(())
    }

    /// Whether `file`, no longer listed by `home`, may still be listed by another home: a parent
    /// this run noted, or one its `Owners` row read; unknown (its page not loaded) counts as
    /// may. Then its `Owners` decides, reading every parent (`owners`).
    fn maybe_listed(&self, file: Name, home: Name) -> bool {
        if self.parents.get(&file).is_some_and(|list| list.iter().any(|parent| *parent != home)) {
            return true;
        }
        let name = owners::of(self.cond_of(file).0);
        let other = |row: RowRef<'_, 'a>| row.reads().iter().any(|(parent, _)| parent != home && parent.kind != Kind::Owners);
        if let Some(row) = self.new.get(&name) {
            return other(RowRef::Made(row));
        }
        let Some(root) = self.root.as_ref() else { return false };
        match self.pages.get(&answers::page_of(name, root.bits)) {
            Some(PageState::Loaded(page)) => page.view().row(name).is_some_and(|row| other(RowRef::Old(row))),
            _ => true,
        }
    }

    /// A body part of `file` (listed by `home`) waits for the file's owners.
    fn gate(&mut self, file: Name, body: Name, home: Name) {
        match self.gated.get_mut(&file) {
            Some(list) => list.push((body, home)),
            None => {
                let mut list = Vec::new_in(self.alloc);
                list.push((body, home));
                self.gated.insert(file, list);
            }
        }
    }

    /// Makes jobs of `file`'s gated bodies once its owners are known, if one is asked; drops
    /// them unmade otherwise (a dependency's body runs only when a task Needs it), or when its
    /// owners failed (that failure is printed by name).
    fn release(&mut self, file: Name, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        if !self.gated.contains_key(&file) {
            return Ok(());
        }
        let name = owners::of(file);
        match self.state(name) {
            None => return self.add_job(name, file, None, false, self.walk),
            Some(State::Settled) => {}
            // It releases them as it settles.
            Some(_) => return Ok(()),
        }
        let closure = self.closure.as_ref();
        let (asked, held) = match (self.now.get(&name).copied(), self.owner_roots.get(&name)) {
            (None, _) => (false, false),
            (Some(_), Some(roots)) => {
                // Under `run`'s closure, a member's root it does not list yet is held: a record
                // settled later may bring its member in (`release_held`).
                let asked = roots.iter().any(|root| root.asked && closure.is_none_or(|closure| closure.asks(root.file)));
                (asked, !asked && closure.is_some() && roots.iter().any(|root| root.asked))
            }
            (Some(now), None) => {
                self.load_roots(name, now.at, None, out);
                return Ok(());
            }
        };
        if held {
            return Ok(());
        }
        let bodies = self.gated.remove(&file).expect("checked above");
        if asked {
            for (body, home) in bodies.iter() {
                if self.state(*body).is_none() {
                    self.add_job(*body, *home, None, false, self.walk)?;
                }
            }
        }
        Ok(())
    }

    /// `run`'s closure grew: each file whose bodies are held is released again, in name order
    /// (rule 25). Bounded: the closure grows at most once a member.
    fn release_held(&mut self, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        let mut files: AVec<'a, Name> = self.gated.keys().copied().collect_in(self.alloc);
        files.sort_unstable();
        for file in files {
            self.release(file, out)?;
        }
        Ok(())
    }

    /// Reads an `Owners` record for its roots; `waiter` (an `Owners` computing) waits for it.
    fn load_roots(&mut self, name: Name, at: Key, waiter: Option<Name>, out: &mut AVec<'_, Effect<'a>>) {
        let waiter = waiter.filter(|waiter| self.park(*waiter));
        match self.roots_loading.get_mut(&name) {
            Some(list) => list.extend(waiter),
            None => {
                let mut list = Vec::new_in(self.alloc);
                list.extend(waiter);
                self.roots_loading.insert(name, list);
                self.get(at, Purpose::Roots(name), out);
            }
        }
    }

    fn roots_got(&mut self, name: Name, key: Key, bytes: Option<&[u8]>, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        // The row stood on this record: the store lost it, which no rebuild here repairs.
        let Some(bytes) = bytes else {
            return Err(DriveError::Wire { key, error: WireError { what: "owners record", at: 0, why: crate::wire::WireWhy::Short } });
        };
        let view = match record::open(bytes).map_err(|error| DriveError::Wire { key, error })? {
            Stored::Whole(view) if view.name == name => view,
            Stored::Whole(view) => return Err(DriveError::WrongRecord { key, asked: name, held: view.name }),
            Stored::Shape(shape) => return Err(DriveError::WrongRecord { key, asked: name, held: shape.name }),
        };
        let mut roots = Vec::new_in(self.alloc);
        roots.extend(owners::roots(view.payload).map_err(|error| DriveError::Wire { key, error })?);
        self.owner_roots.insert(name, roots);
        if let Some(waiters) = self.roots_loading.remove(&name) {
            for waiter in waiters.iter() {
                self.wake.push_back(*waiter);
            }
        }
        self.release(owners::file_of(name), out)
    }

    /// Whether `name`'s home stood this run and its row lists `name`, its record not read for
    /// its parts' locates yet.
    fn parts_unread(&self, home: Name, name: Name) -> bool {
        home != name
            && self.state(home) == Some(State::Settled)
            && !self.parts_read.contains(&home)
            && self.current(home).is_some_and(|row| row.parts().binary_search(&name).is_ok())
    }

    fn read_parts(&mut self, home: Name, waiter: Name, out: &mut AVec<'_, Effect<'a>>) {
        let parked = self.park(waiter);
        match self.parts_loading.get_mut(&home) {
            Some(list) => {
                if parked {
                    list.push(waiter);
                }
            }
            None => {
                let mut list = Vec::new_in(self.alloc);
                if parked {
                    list.push(waiter);
                }
                self.parts_loading.insert(home, list);
                let at = self.current(home).expect("checked by `parts_unread`").at();
                self.get(at, Purpose::Parts(home), out);
            }
        }
    }

    fn parts_got(&mut self, home: Name, key: Key, bytes: Option<&[u8]>) -> Result<(), DriveError> {
        let Some(bytes) = bytes else {
            return Err(DriveError::Wire { key, error: WireError { what: "home record", at: 0, why: crate::wire::WireWhy::Short } });
        };
        let view = match record::open(bytes).map_err(|error| DriveError::Wire { key, error })? {
            Stored::Whole(view) if view.name == home => view,
            Stored::Whole(view) => return Err(DriveError::WrongRecord { key, asked: home, held: view.name }),
            Stored::Shape(shape) => return Err(DriveError::WrongRecord { key, asked: home, held: shape.name }),
        };
        for part in view.parts() {
            if !self.part_locs.contains_key(&part.name) {
                self.part_locs.insert(part.name, (part.locate, part.place, home));
            }
        }
        self.parts_read.insert(home);
        if let Some(waiters) = self.parts_loading.remove(&home) {
            for waiter in waiters.iter() {
                self.wake.push_back(*waiter);
            }
        }
        Ok(())
    }

    /// Computes the `Owners` row `name` (owners.rs): the union of its file's parents' owners,
    /// a parent that names crates adding the file itself. Its job is in `Gather` while it waits.
    fn owners(&mut self, name: Name, out: &mut AVec<'_, Effect<'a>>) -> Result<(), DriveError> {
        let a = self.alloc;
        let file = owners::file_of(name);
        if self.dropped.contains(&file) {
            return self.drop_name(name);
        }
        let old_reads: Option<AVec<'a, Name>> =
            self.current(name).map(|row| row.reads().iter().map(|(read, _)| read).filter(|read| read.kind != Kind::Owners).collect_in(a));
        // A first computation waits for the outline walk: the one barrier allowed (outlines
        // only, never a scope). A row's parents are known: no wait.
        if old_reads.is_none() && self.outlines_open > 0 {
            if self.park(name) {
                self.owners_waiting.push(name);
            }
            return Ok(());
        }
        let mut candidates: AVec<'a, Name> = Vec::new_in(a);
        if let Some(list) = self.parents.get(&file) {
            candidates.extend_from_slice(list);
        }
        if self.plant != Plant::OwnersLastParentWins {
            if let Some(reads) = &old_reads {
                candidates.extend_from_slice(reads);
            }
            let homes = [self.jobs.get(&file).map(|job| job.home), self.current(file).map(|row| row.home())];
            candidates.extend(homes.into_iter().flatten().filter(|home| *home != file));
        }
        candidates.sort_unstable();
        candidates.dedup();
        let by_parent = candidates.iter().any(|parent| parent.kind == Kind::FileItems);
        let mut reads: AVec<'a, (Name, ContentId)> = Vec::new_in(a);
        let mut roots: AVec<'a, OwnerRoot> = Vec::new_in(a);
        let (mut waiting, mut listed) = (false, false);
        for parent in candidates.iter().copied() {
            let now = match self.digest(parent, name, out) {
                Digest::Wait => {
                    waiting = true;
                    continue;
                }
                Digest::Ready(None) => continue,
                Digest::Ready(Some(now)) => now,
            };
            if !self.current(parent).is_some_and(|row| row.parts().binary_search(&file).is_ok()) {
                continue;
            }
            listed = true;
            reads.push((parent, now.out));
            if owners::names_crates(parent.kind) {
                roots.push(OwnerRoot { file, asked: parent.kind == Kind::Units });
                continue;
            }
            if parent.kind != Kind::FileItems {
                continue;
            }
            let up = owners::of(parent);
            match self.state(up) {
                None => {
                    self.add_job(up, parent, None, false, self.walk)?;
                    self.wait_on(up, name);
                    waiting = true;
                    continue;
                }
                Some(State::Settled) => {}
                Some(_) => {
                    self.wait_on(up, name);
                    waiting = true;
                    continue;
                }
            }
            // Its owners failed (a cycle, named): no owner comes through it.
            let Some(up_now) = self.now.get(&up).copied() else { continue };
            reads.push((up, up_now.out));
            match self.owner_roots.get(&up) {
                Some(theirs) => roots.extend_from_slice(theirs),
                None => {
                    self.load_roots(up, up_now.at, Some(name), out);
                    waiting = true;
                }
            }
        }
        if waiting {
            return Ok(());
        }
        if !listed && by_parent && self.current(file).is_some_and(|row| row.home() != file) {
            // No home lists it any more: it goes, and its owners with it.
            self.drop_name(file)?;
            return self.drop_name(name);
        }
        owners::merge(&mut roots);
        reads.sort_unstable_by_key(|(read, _)| *read);
        let payload = owners::write_roots(&roots, a);
        // A file a parent reaches that no root owns: its bodies are not checked, and it says so.
        let named = roots.is_empty() && by_parent;
        let path = match (named, self.file_index(&file.subject)) {
            (true, Some(index)) => self.path(index as u32),
            _ => &[],
        };
        let mut message = Vec::with_capacity_in(path.len(), a);
        message.extend_from_slice(path);
        let finding = [Finding { rule: OWNERLESS_RULE, start: 0, end: 0, message: &message }];
        let findings: &[Finding<'_>] = if named { &finding } else { &[] };
        let output = Output { name, out: owners::out_of(&roots), reads: &reads, cells: &[], parts: &[], paths: &[], findings, payload: &payload };
        self.owner_roots.insert(name, roots.clone());
        // Rule 77: the record these reads make may be in the store already (another worktree's
        // walk of the same files, or this checkout's before an undo): its value key is its reads
        // (`made_split`), so it is looked up there first and found, never written again. Only a
        // miss makes it (`valued`). Written each time, a second worktree's cold check put its
        // three `Owners` again (store_worktree_tests.rs, 2026-11-17).
        if !self.owners_missed.remove(&name) && !reads.is_empty() {
            let at = key::value_key_with_cells(self.jobs.get(&name).expect("a job").key, &reads, &[]);
            self.set_state(name, State::Valuing);
            self.get(at, Purpose::Value(name), out);
            return Ok(());
        }
        self.made(&output, out)
    }

    /// At a quiet point with the outline walk done, `Owners` jobs still waiting wait on each
    /// other: their files are each other's ancestors (a cycle of `mod` or `#[path]` files).
    /// Each fails by name (rule 20) and its bodies are not run; `true` when any did.
    fn owners_cycle(&mut self) -> bool {
        if self.outlines_open > 0 {
            return false;
        }
        let mut stuck: AVec<'a, Name> =
            self.jobs.iter().filter(|(name, job)| name.kind == Kind::Owners && job.state == State::Gather).map(|(name, _)| *name).collect_in(self.alloc);
        stuck.sort_unstable();
        for name in stuck.iter() {
            self.failures.push(TaskFailure {
                name: *name,
                why: Why::Refused { what: "a module file that is its own ancestor (a cycle of `mod` or `#[path]` files)", rule: "owners" },
            });
            self.now.remove(name);
            self.set_state(*name, State::Settled);
            self.wake_waiters(*name);
            self.gated.remove(&owners::file_of(*name));
        }
        !stuck.is_empty()
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
                work.extend(row.parts().iter());
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

    /// `name`'s out digest changed: every whole reader is checked again, and each cell reader
    /// whose cell's digest today is not what it saw (cells.rs). A cell's digest today is the
    /// one `look_up_cells` found in the record `name` is settled at; a name with no record
    /// holds every cell `ABSENT`; one settled where no cell was looked up (it stood) dirties
    /// every cell reader.
    fn dirty_readers(&mut self, name: Name) -> Result<(), DriveError> {
        if self.plant == Plant::SkipChangedRead {
            return Ok(());
        }
        // The page is held by its count, not copied: the row's readers are walked in place while
        // each one is dirtied (which may load other pages).
        let page = self.held_page(name).cloned();
        let row = page.as_ref().and_then(|page| page.view().row(name));
        let settled_at = match self.dropped.contains(&name) {
            true => None,
            false => self.now.get(&name).map(|now| now.at),
        };
        // This run's few beside the row's: the readers posted, and each cell reader (the row's,
        // in place, and those posted) whose cell moved; sorted once.
        let mut more = Vec::new_in(self.alloc);
        if let Some(posted) = self.posted.get(&name) {
            more.extend_from_slice(posted);
        }
        if let Some(row) = row {
            for entry in row.cell_readers().iter() {
                if self.cell_moved(name, settled_at, entry) {
                    more.push(entry.reader);
                }
            }
        }
        if let Some(posted) = self.posted_cells.get(&name) {
            for entry in posted.iter() {
                if self.cell_moved(name, settled_at, *entry) {
                    more.push(entry.reader);
                }
            }
        }
        more.sort_unstable();
        more.dedup();
        // The row's readers (sorted) and those, two sorted sequences merged: each distinct
        // reader dirtied once, in name order.
        let held = row.map(|row| row.readers());
        let held_len = held.map_or(0, |held| held.len());
        let (mut i, mut j) = (0, 0);
        let mut last = None;
        loop {
            let h = match held {
                Some(held) if i < held_len => Some(held.get(i)),
                _ => None,
            };
            let next = match (h, more.get(j).copied()) {
                (Some(h), Some(m)) if h == m => {
                    i += 1;
                    j += 1;
                    h
                }
                (Some(h), Some(m)) if h < m => {
                    i += 1;
                    h
                }
                (Some(h), None) => {
                    i += 1;
                    h
                }
                (_, Some(m)) => {
                    j += 1;
                    m
                }
                (None, None) => break,
            };
            if last == Some(next) {
                continue;
            }
            last = Some(next);
            if next != name {
                self.dirty(next)?;
            }
        }
        Ok(())
    }

    /// Whether a cell reader of `name` saw other than what its cell holds today
    /// ([`Request::dirty_readers`]); `settled_at` is the record `name` is settled at, `None` for
    /// one with no record (every cell `ABSENT`).
    fn cell_moved(&self, name: Name, settled_at: Option<Key>, entry: CellReader) -> bool {
        let today = match settled_at {
            None => Some(ABSENT),
            Some(at) => self.cells_now.get(&CellAt { name, cell: entry.cell }).filter(|(held, _)| *held == at).map(|(_, saw)| *saw),
        };
        match self.plant {
            Plant::CellMovedNotDirtied => false,
            Plant::CellsAsWhole => true,
            _ => today != Some(entry.saw),
        }
    }

    /// Looks up, in the record of `name` at `at` (its payload in hand), each cell a reader of it
    /// read: its old row's cell readers and those posted this run. Each distinct cell is looked
    /// up once.
    fn look_up_cells(&mut self, name: Name, at: Key, payload: &[u8], held: &[CellReader]) -> Result<(), DriveError> {
        let mut wanted: AVec<'a, Cell> = held.iter().map(|entry| entry.cell).collect_in(self.alloc);
        if let Some(posted) = self.posted_cells.get(&name) {
            wanted.extend(posted.iter().map(|entry| entry.cell));
        }
        if let Some((cells, _)) = self.cells_loading.get(&name) {
            wanted.extend_from_slice(cells);
        }
        if wanted.is_empty() {
            return Ok(());
        }
        wanted.sort_unstable();
        wanted.dedup();
        self.cells_in(name, at, payload, &wanted)
    }

    /// Each of `cells` of the record of `name` at `at`, by its kind's lookup, into `cells_now`.
    fn cells_in(&mut self, name: Name, at: Key, payload: &[u8], cells: &[Cell]) -> Result<(), DriveError> {
        let Some(of) = self.cells.of(name.kind) else {
            return Err(DriveError::NoCells { name, by: name });
        };
        for cell in cells.iter() {
            let seen = of(payload, *cell).map_err(|refused| DriveError::CellRefused { name, cell: *cell, what: refused.what })?;
            self.cells_now.insert(CellAt { name, cell: *cell }, (at, seen.digest()));
        }
        Ok(())
    }

    /// Today's digest of `cell` of `read`, settled as `now`: `Ok(None)` while its record is read
    /// for it (`Purpose::Cells`), `waiter` woken when it is in.
    fn cell_digest(&mut self, read: Name, cell: Cell, now: Option<Now>, waiter: Name, out: &mut AVec<'_, Effect<'a>>) -> Result<Option<ContentId>, DriveError> {
        let Some(now) = now else { return Ok(Some(Seen::Absent.digest())) };
        match self.cells_now.get(&CellAt { name: read, cell }).copied() {
            Some((at, saw)) if at == now.at => return Ok(Some(saw)),
            _ => {}
        }
        if self.cells.of(read.kind).is_none() {
            return Err(DriveError::NoCells { name: read, by: waiter });
        }
        let parked = self.park(waiter);
        match self.cells_loading.get_mut(&read) {
            Some((cells, waiters)) => {
                cells.push(cell);
                if parked {
                    waiters.push(waiter);
                }
            }
            None => {
                let mut cells = Vec::new_in(self.alloc);
                cells.push(cell);
                let mut waiters = Vec::new_in(self.alloc);
                if parked {
                    waiters.push(waiter);
                }
                self.cells_loading.insert(read, (cells, waiters));
                self.get(now.at, Purpose::Cells(read), out);
            }
        }
        Ok(None)
    }

    fn cells_got(&mut self, name: Name, key: Key, bytes: Option<&[u8]>) -> Result<(), DriveError> {
        // The name is settled at this record: the store lost it, which no rebuild here repairs.
        let Some(bytes) = bytes else {
            return Err(DriveError::Wire { key, error: WireError { what: "cell-read record", at: 0, why: crate::wire::WireWhy::Short } });
        };
        let view = match record::open(bytes).map_err(|error| DriveError::Wire { key, error })? {
            Stored::Whole(view) if view.name == name => view,
            Stored::Whole(view) => return Err(DriveError::WrongRecord { key, asked: name, held: view.name }),
            Stored::Shape(shape) => return Err(DriveError::WrongRecord { key, asked: name, held: shape.name }),
        };
        let Some((mut cells, waiters)) = self.cells_loading.remove(&name) else { return Ok(()) };
        cells.sort_unstable();
        cells.dedup();
        self.cells_in(name, key, view.payload, &cells)?;
        for waiter in waiters.iter() {
            self.wake.push_back(*waiter);
        }
        Ok(())
    }

    fn dirty(&mut self, name: Name) -> Result<(), DriveError> {
        // A name its home dropped this run is settled with no record, and its readers were
        // dirtied as it went (`drop_name`): a read of its that moves enters nothing again. Entered
        // again it was decided and run as a part no home lists, and its run's answer is for
        // nothing (`ran`), so the job stayed out with nothing out: the request stopped as a
        // `NeedCycle` of that one body (a removed fn's body, which read its file's moved
        // `Surface` and another input, 2026-11-17). A home that lists it again revives it
        // (`settle`).
        if self.dropped.contains(&name) && self.plant != Plant::DirtyDropped {
            return Ok(());
        }
        match self.state(name) {
            None => {
                let home = self.old(name).map_or(name, |row| row.home());
                self.add_job(name, home, None, false, self.walk)?;
                if let Some(job) = self.jobs.get_mut(&name) {
                    job.recheck = true;
                }
            }
            Some(State::Settled) => {
                let job = self.jobs.get_mut(&name).expect("a job");
                job.recheck = true;
                // A job that stood and is decided again did not stand: every part of a re-made
                // home stands at once on its row, before the read that dirties it settles, so
                // without this a dirtied reader found by value still counted as standing.
                if core::mem::replace(&mut job.stood, false) {
                    let counts = self.profile.of(name.kind);
                    counts.standing = counts.standing.saturating_sub(1);
                }
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
        if self.owners_cycle() {
            return Ok(());
        }
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
        // `run`'s closure never saw the program package's record: it asked nothing, so no check
        // covers the program. A failure by name (rule 3), never a start over it.
        if let Some(closure) = self.closure.as_ref()
            && !closure.found()
        {
            let why = Why::Refused { what: "the program package, which no units record lists", rule: "run_scope.rs" };
            self.failures.push(TaskFailure { name: closure.package(), why });
        }
        // Items by how many conditions they were answered under: each settled record that is
        // not a split is one condition of its base.
        let mut per_item: Table<Name, u32, Alloc<'a>> = Table::new_in(a.clone());
        for (name, job) in self.jobs.iter() {
            if job.state == State::Settled && name.kind != Kind::Group && self.current(*name).is_some_and(|row| row.split().is_none()) {
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
                if !old.is_some_and(|old| old.reads().binary_search_by_key(read, |(r, _)| r).is_ok()) {
                    updates.push((*read, *name, true));
                }
            }
            if let Some(old) = old {
                for (read, _) in old.reads().iter() {
                    if !has(&row.reads, &read) {
                        updates.push((read, *name, false));
                    }
                }
            }
        }
        for gone in self.dropped.iter() {
            if let Some(old) = self.old(*gone) {
                for (read, _) in old.reads().iter() {
                    updates.push((read, *gone, false));
                }
            }
        }
        // The reverse index of part reads the same way, an entry being (reader, cell, what it
        // saw): a reader that saw its cell move is one entry out and one in.
        let mut cell_updates: Vec<(Name, CellReader, bool), Alloc<'a>> = Vec::new_in(a.clone());
        let reader_of = |name: Name, read: &CellRead| (read.name, CellReader { reader: name, cell: read.cell, saw: read.saw });
        for (name, row) in self.new.iter() {
            let old = self.old(*name);
            for read in row.cells.iter() {
                if !old.is_some_and(|old| old.cells().binary_search(read).is_ok()) {
                    let (target, entry) = reader_of(*name, read);
                    cell_updates.push((target, entry, true));
                }
            }
            if let Some(old) = old {
                for read in old.cells().iter() {
                    if row.cells.binary_search(&read).is_err() {
                        let (target, entry) = reader_of(*name, &read);
                        cell_updates.push((target, entry, false));
                    }
                }
            }
        }
        for gone in self.dropped.iter() {
            if let Some(old) = self.old(*gone) {
                for read in old.cells().iter() {
                    let (target, entry) = reader_of(*gone, &read);
                    cell_updates.push((target, entry, false));
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
                for name in self.new.keys().chain(self.dropped.iter()).chain(updates.iter().map(|(t, _, _)| t)).chain(cell_updates.iter().map(|(t, _, _)| t)) {
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
                self.pages.insert(*index, PageState::Loaded(HeldPage::empty()));
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
                let Some(was) = self.new.get(name).map(RowRef::Made).or_else(|| self.old_or_watch(*name).map(RowRef::Old)) else { continue };
                if was.readers().is_empty() && was.cell_readers().is_empty() {
                    continue;
                }
                let (readers, cell_readers) = (was.readers().to_vec_in(a.clone()), was.cell_readers().to_vec_in(a.clone()));
                self.new.insert(*name, watch_row(*name, readers, cell_readers, a.clone()));
            }
        }
        // The reverse index's updates, sorted by target once: each target's slice is merged with
        // its row's readers as its page is written (`answers::write_page`), so a name read by r
        // readers costs O(r + its updates), and no old row is copied to take them.
        updates.sort_unstable();
        cell_updates.sort_unstable();
        // A `Given` name's watch row keeps what the command gave this time as its `out`: the
        // next command's seed of it dirties its readers only when that moved (`given`).
        let given: Vec<(Name, ContentId), Alloc<'a>> = self
            .jobs
            .iter()
            .filter(|(name, job)| name.kind == Kind::Given && job.state == State::Settled && !self.given_unknown.contains(name))
            .map(|(name, _)| (*name, self.now.get(name).map_or(ABSENT, |now| now.out)))
            .collect_in(a.clone());
        for (name, today) in given.iter() {
            if let Some(row) = self.new.get_mut(name) {
                row.out = *today;
                continue;
            }
            let held = self.old_or_watch(*name).map(|row| (row.out() != *today).then(|| row.to_row(a.clone())));
            match held {
                Some(Some(mut row)) => {
                    row.out = *today;
                    self.new.insert(*name, row);
                }
                Some(None) => {}
                // A name first read this run: the watch row its readers make carries today's out.
                None if self.plant != Plant::ForgetAbsentReaders
                    && (updates.binary_search_by_key(name, |(t, _, _)| *t).is_ok() || cell_updates.binary_search_by_key(name, |(t, _, _)| *t).is_ok()) =>
                {
                    let mut row = watch_row(*name, Vec::new_in(a.clone()), Vec::new_in(a.clone()), a.clone());
                    row.out = *today;
                    self.new.insert(*name, row);
                }
                None => {}
            }
        }
        // The touched pages, rewritten whole, and the root: those of the rows made or dropped
        // and of each update's target; a page none of them is on is not rewritten.
        let mut touched = Vec::new_in(a.clone());
        match old_bits {
            Some(old_bits) if old_bits == bits => {
                for name in self.new.keys().chain(self.dropped.iter()).chain(updates.iter().map(|(t, _, _)| t)).chain(cell_updates.iter().map(|(t, _, _)| t)) {
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
        // Every row of a touched page, bucketed by page once: O(rows), not pages x rows. Each is
        // a base read in place (an old row is never copied) and its slices of the updates.
        let mut buckets: Table<u32, Vec<PageRow<'_, 'a>, Alloc<'a>>, Alloc<'a>> = Table::new_in(a.clone());
        for state in self.pages.values() {
            if let PageState::Loaded(old) = state {
                for row in old.view().rows() {
                    let name = row.name();
                    if !self.dropped.contains(&name) && !self.new.contains_key(&name) {
                        bucket_row(&mut buckets, &touched, bits, page_row(name, Some(RowRef::Old(row)), &updates, &cell_updates), a.clone());
                    }
                }
            }
        }
        for (name, row) in self.new.iter() {
            if !self.dropped.contains(name) || row.watching() {
                bucket_row(&mut buckets, &touched, bits, page_row(*name, Some(RowRef::Made(row)), &updates, &cell_updates), a.clone());
            }
        }
        // A target with no record (never made, or dropped this run) keeps its readers in a watch
        // row: a read that found nothing is a read, so the name's appearing reruns them (rule
        // 21). The planted bug forgets them. A target with a row above is the same row again.
        let mut last = None;
        for target in updates.iter().map(|(t, _, _)| *t).chain(cell_updates.iter().map(|(t, _, _)| *t)) {
            if last == Some(target) {
                continue;
            }
            last = Some(target);
            let dropped = self.dropped.contains(&target);
            let base = match self.new.get(&target) {
                Some(row) if !(dropped && !row.watching()) => Some(RowRef::Made(row)),
                _ => match (dropped, self.old_or_watch(target)) {
                    (false, Some(view)) => Some(RowRef::Old(view)),
                    _ if self.plant == Plant::ForgetAbsentReaders => continue,
                    _ => None,
                },
            };
            bucket_row(&mut buckets, &touched, bits, page_row(target, base, &updates, &cell_updates), a.clone());
        }
        // Written while the rows are borrowed; stored after.
        let mut written: Vec<(u32, Option<AVec<'a, u8>>), Alloc<'a>> = Vec::with_capacity_in(touched.len(), a.clone());
        let mut unread: Vec<Name, Alloc<'a>> = Vec::new_in(a.clone());
        for index in touched.iter() {
            let mut page_rows = buckets.remove(index).unwrap_or_else(|| Vec::new_in(a.clone()));
            page_rows.sort_unstable_by_key(|row| row.name);
            page_rows.dedup_by_key(|row| row.name);
            // A watch row no one reads any more, whole or by cell, goes.
            unread.extend(page_rows.iter().filter(|row| !row.kept()).map(|row| row.name));
            written.push((*index, answers::write_page(&page_rows, a.clone())));
        }
        drop(buckets);
        for target in unread.iter() {
            self.new.remove(target);
            self.dropped.insert(*target);
        }
        for (index, bytes) in written {
            let key = match bytes {
                None => Key::default(),
                Some(bytes) => {
                    let key = key::content_key(&bytes);
                    self.profile.pages_written += 1;
                    self.put(key, bytes, out);
                    key
                }
            };
            page_keys[index as usize] = key;
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
        let units = self.units_at.or_else(|| self.old(Name::units()).map(|row| row.at()));
        let native = self.native_at.or_else(|| self.program_name.and_then(|name| self.old(name)).map(|row| row.at()));
        let cert = write_cert(&all, root_key, units, native, a.clone());
        // Keyed by the files this answer read, the leaf table the cli writes beside it: the next
        // request's `Start` hands exactly these when none changed (`Request::holds`).
        let mut held_files = Vec::with_capacity_in(self.files.len(), a.clone());
        for i in 0..self.files.len() {
            let path = self.path(i as u32);
            if self.holds(path) {
                held_files.push((self.files.subjects()[i], self.files.ids()[i]));
            }
        }
        // A file read by content only that the sweep did not hand (its first read: the cli
        // joins it to the leaf table beside this answer, and the next `Start` hands it) is a
        // file this answer read, present as it saw it. Left out, the key named the checkout
        // without it, so a later sweep with the file gone met this certificate and every reader
        // of it stood (sim_given's removal case, `Some(1) to None`). An absent read is left out
        // on both sides: absence is omission. The ask's facts are keyed by `facts_digest`.
        for (name, now) in self.now.iter() {
            let content = name.kind == Kind::Given && self.facts.binary_search_by(|(fact, _)| fact.cmp(name)).is_err();
            if content && now.out != ABSENT && self.file_index(&name.subject).is_none() {
                held_files.push((name.subject, now.out));
            }
        }
        let read = self.cert_of(&mut held_files);
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
        let (count, errors, unchecked) = self.print(&all, out);
        self.errors = errors;
        self.unchecked = unchecked;
        if self.gate == Gate::Beside && self.program == Program::Running && self.errors > 0 {
            self.program = Program::Stopped;
            out.push(Effect::Stop { errors: self.errors });
        }
        self.held = Some(Answer { profile: self.profile, verdict: Verdict::Checked, findings: count, units, failed: self.failed() });
        self.phase = Phase::Held;
        self.conclude(out);
        Ok(())
    }

    /// `Print` effects for `held`, sorted by path and position; how many, how many of them are
    /// errors, and how many say their item was not checked (`is_unchecked`). An item's or a
    /// manifest's finding is its own: its name is where it is. A crate's finding carries its file and item in its words, so it is its rule, range, words
    /// and condition, whichever crate's record holds it: two crates holding one finding (a
    /// library and its test unit each resolve the same `use` of the same file) print it once
    /// and count it once, as the name first in the order. (Owed: `test` as a variable of one
    /// crate's condition, "One check covers every configuration", so there is one record.)
    fn print(&self, held: &SortedMap<Name, Held<'a>, Alloc<'a>>, out: &mut AVec<'_, Effect<'a>>) -> (u32, u32, u32) {
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
        let is_unchecked = self.is_unchecked;
        let (mut count, mut errors, mut unchecked) = (0, 0, 0);
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
            unchecked += u32::from(is_unchecked(finding.rule));
        }
        (count, errors, unchecked)
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
/// The rule of a file a `mod` (or `#[path]`) parent reaches and no crate root owns: its bodies
/// are not checked (argument: the file's path). On its file's `Owners` record (owners.rs).
pub const OWNERLESS_RULE: u16 = 0xC110;

/// Whether a job of `kind` is the outline walk's: what a first `Owners` waits for.
fn outline(kind: Kind) -> bool {
    matches!(kind, Kind::Units | Kind::UnitGraph | Kind::FileItems)
}

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
fn watch_row<'a>(name: Name, readers: Vec<Name, Alloc<'a>>, cell_readers: Vec<CellReader, Alloc<'a>>, alloc: Alloc<'a>) -> Row<'a> {
    Row {
        key: Key::default(),
        at: Key::default(),
        source: ABSENT,
        out: ABSENT,
        home: name,
        place: 0,
        reads: Vec::new_in(alloc.clone()),
        cells: Vec::new_in(alloc.clone()),
        parts: Vec::new_in(alloc),
        readers,
        cell_readers,
        split: None,
        cond: None,
    }
}

/// The slice of the sorted `all` whose target is `name`: binary searched, never scanned.
fn updates_of<T>(all: &[(Name, T, bool)], name: Name) -> &[(Name, T, bool)] {
    let start = all.partition_point(|(target, _, _)| *target < name);
    let end = start + all[start..].partition_point(|(target, _, _)| *target == name);
    &all[start..end]
}

/// `name`'s row of a page being written: `base` and its slices of the sorted updates.
fn page_row<'r, 'x>(name: Name, base: Option<RowRef<'r, 'x>>, updates: &'r [(Name, Name, bool)], cell_updates: &'r [(Name, CellReader, bool)]) -> PageRow<'r, 'x> {
    PageRow { name, base, readers: updates_of(updates, name), cell_readers: updates_of(cell_updates, name) }
}

/// `row` into its page's bucket, when that page is one of the `touched` (sorted).
fn bucket_row<'r, 'x, 'b>(buckets: &mut Table<u32, Vec<PageRow<'r, 'x>, Alloc<'b>>, Alloc<'b>>, touched: &[u32], bits: u8, row: PageRow<'r, 'x>, alloc: Alloc<'b>) {
    let index = answers::page_of(row.name, bits);
    if touched.binary_search(&index).is_err() {
        return;
    }
    match buckets.get_mut(&index) {
        Some(list) => list.push(row),
        None => {
            let mut list = Vec::new_in(alloc);
            list.push(row);
            buckets.insert(index, list);
        }
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
/// units' record (so a repeat `run` starts at one read), the program's `Native` record (so it
/// takes its image with no walk), then every finding of the answer.
fn write_cert<'a>(held: &SortedMap<Name, Held<'a>, Alloc<'a>>, root: Key, units: Option<Key>, native: Option<Key>, alloc: Alloc<'a>) -> Vec<u8, Alloc<'a>> {
    let mut w = Writer::new_in(CERT_MAGIC, alloc);
    w.raw(&root.0.0);
    for at in [units, native] {
        w.u32(u32::from(at.is_some()));
        w.raw(&at.unwrap_or_default().0.0);
    }
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

/// A certificate read: the answer's root, the units' record, the program's `Native` record, the
/// findings.
type Cert<'a> = (Key, Option<Key>, Option<Key>, SortedMap<Name, Held<'a>, Alloc<'a>>);

fn read_cert<'a>(bytes: &[u8], alloc: Alloc<'a>) -> Result<Cert<'a>, WireError> {
    let mut r = Reader::new(bytes, CERT_MAGIC, "certificate")?;
    let mut root = [0u8; 32];
    root.copy_from_slice(r.take_bytes(32)?);
    let root = Key(ContentId(root));
    let mut keys = [None; 2];
    for at in keys.iter_mut() {
        let has = r.u32()? == 1;
        let mut key = [0u8; 32];
        key.copy_from_slice(r.take_bytes(32)?);
        *at = has.then_some(Key(ContentId(key)));
    }
    let [units, native] = keys;
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
    Ok((root, units, native, held))
}
