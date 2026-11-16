//! The request machine (drive.md, "The request machine").
//!
//! [`Request::step`] takes one [`Event`] and pushes the [`Effect`]s it leads to. Every job is one
//! name; a job that lacks a page, a name or its home waits on it by name and is woken when it
//! arrives, so the machine never blocks and holds no dependency graph: what it knows of other
//! names is their rows, read from the answer pages a job touches, and the readers posted this
//! run.

use core::alloc::Allocator;
use core::fmt;

use blaze_id::ContentId;
use blaze_ids::Hasher;
use blaze_own::{CollectIn, SortedMap, SortedSet, Vec, VecDeque, soa};

use crate::Plant;
use crate::answers::{self, Last, Page, Root, Row};
use crate::key::{self, ABSENT, Key, Kind, Name, Tool};
use crate::profile::Profile;
use crate::record::{self, Finding, Locate, Part, Stored, Whole, WholeView};
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
}

/// One file of the checkout, as the sweep found it.
#[derive(Clone, Copy, Debug)]
pub struct FileEntry<'a> {
    /// Relative to the checkout, `/`-separated.
    pub path: &'a [u8],
    pub id: ContentId,
    /// Its content differs from the leaf table the last answer was made with.
    pub changed: bool,
    /// A manifest (`Cargo.toml`, `Cargo.lock`): the units' source.
    pub manifest: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Ticket(pub u64);

/// One [`Effect::Run`]: a worker takes it whole and answers it whole.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct BatchId(pub u64);

/// Tasks of one kind and one home, about [`TARGET_BATCH_NS`] of work together (drive.md,
/// "Scheduling on many cores"): the coordinator's cost per task drops by the batch's size.
#[derive(Debug)]
pub struct Batch<A: Allocator> {
    pub id: BatchId,
    pub tasks: Vec<(Ticket, Task<A>), A>,
}

/// A batch is sized to about this much work, from this run's mean time of its kind.
pub const TARGET_BATCH_NS: u64 = 1_000_000;
/// A kind's time per task before any of it has been measured this run.
pub const DEFAULT_TASK_NS: u64 = 50_000;
/// The most tasks one batch holds.
pub const MOST_BATCH: usize = 256;

/// What happened.
pub enum Event<'a> {
    /// The sweep's files, sorted by path; the paths gone since the last answer; the `last` file.
    Start { files: &'a [FileEntry<'a>], gone: &'a [&'a [u8]], last: Option<&'a [u8]> },
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
    pub findings: &'a [Finding<'a>],
    pub payload: &'a [u8],
}

/// A name a task needs, and the record that lists it (its file's `FileItems`, a `Fetch`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Want {
    pub name: Name,
    pub home: Name,
}

/// A task that could not answer: not a user's error (that is a finding), the tool's.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TaskFailure {
    pub name: Name,
    /// What could not be done (`"the body checker over the store is not built"`).
    pub what: &'static str,
    /// The rule or the crate that says so.
    pub rule: &'static str,
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

/// A record a task is handed: where it is and the digest a read of it records.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Input {
    pub name: Name,
    pub at: Key,
    pub out: ContentId,
}

/// A task to run.
#[derive(Debug)]
pub struct Task<A: Allocator> {
    pub name: Name,
    pub key: Key,
    pub source: ContentId,
    pub class: Class,
    /// How many times it has been entered before (0 the first time).
    pub entry: u32,
    pub home: Name,
    pub place: u32,
    /// A `FileItems` task's file, an index for [`Request::path`].
    pub file: Option<u32>,
    pub edition: u8,
    /// Its inputs: last time's read list first, then what it needed since.
    pub inputs: Vec<Input, A>,
    /// A `Group`'s members.
    pub members: Vec<Name, A>,
}

/// A finding to print: the caller places it with the file's line table.
#[derive(Debug)]
pub struct Print<A: Allocator> {
    pub name: Name,
    /// The file it is in, an index for [`Request::path`]; `None` for a manifest's or a crate's.
    pub file: Option<u32>,
    /// Its item's start in the file.
    pub place: u32,
    pub rule: u16,
    pub start: u32,
    pub end: u32,
    pub message: Vec<u8, A>,
}

/// What the machine asks of its caller.
#[derive(Debug)]
pub enum Effect<A: Allocator> {
    Get { key: Key },
    /// Write `bytes` at `key` once; bytes already there under the key stand.
    Put { key: Key, bytes: Vec<u8, A> },
    Run { batch: Batch<A> },
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
    Print(Print<A>),
    /// Replace the checkout's `last` file with `bytes`, after every `Put` before it.
    WriteLast { bytes: Vec<u8, A> },
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
    Task(TaskFailure),
    TooManyJobs { most: u32, name: Name },
    /// A step after the answer.
    Finished,
    /// `Exited` with no program started, or a second time.
    NoProgram,
    /// The program wanted a name nothing settled after the check was done: code the check's
    /// walk does not reach is not built yet (drive.md, "The gate").
    LateWant { name: Name },
}

impl fmt::Display for DriveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DriveError::Wire { key, error } => write!(f, "the record at {key}: {error}"),
            DriveError::LastFile(error) => write!(f, "the checkout's last answer: {error}; remove .blaze/v2/last to start over"),
            DriveError::Unsorted { what } => write!(f, "{what} handed unsorted (drive.md: the sweep sorts by path)"),
            DriveError::NoFile { file, home } => write!(f, "{home} lists the file {file}, which is not in the checkout"),
            DriveError::NoPart { name, home } => write!(f, "a task wanted {name} of {home}, which does not list it"),
            DriveError::Missing { name, key, builder } => match builder {
                Builder::Pack { krate } => write!(f, "{name} (record {key}) is not in the store: it comes in {krate}'s pack; fetch it (rule 3: no local fallback)"),
                Builder::PrepareStd => write!(f, "{name} (record {key}) is not in the store: run `blaze prepare-std`"),
            },
            DriveError::WrongRecord { key, asked, held } => write!(f, "the record at {key} is {held}'s, asked as {asked}'s (rule 21: a key covers every input)"),
            DriveError::UnaskedOutput { name, by } => write!(f, "{by} answered a record of {name}, which no job asked"),
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
            DriveError::Task(failure) => write!(f, "{}: {} ({})", failure.name, failure.what, failure.rule),
            DriveError::TooManyJobs { most, name } => write!(f, "more than {most} jobs, at {name} (rule 18)"),
            DriveError::Finished => write!(f, "a step after the request answered"),
            DriveError::NoProgram => write!(f, "a program's exit, with no program running"),
            DriveError::LateWant { name } => write!(f, "the program wanted {name} after the check was done; a late want is not built (drive.md, \"The gate\")"),
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

struct Job<A: Allocator> {
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
    /// The names its next run is handed, sorted.
    needs: Vec<Name, A>,
    /// The read names its value key is made of.
    shape: Vec<Name, A>,
    /// Jobs waiting on this one.
    waiters: Vec<Name, A>,
    members: Vec<Name, A>,
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

enum PageState<A: Allocator + Clone> {
    Loading,
    Loaded(Page<A>),
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

/// A name's findings as printed: its file, its start, its findings.
struct Held<A: Allocator> {
    file: Name,
    place: u32,
    findings: Vec<HeldFinding<A>, A>,
}

struct HeldFinding<A: Allocator> {
    rule: u16,
    start: u32,
    end: u32,
    message: Vec<u8, A>,
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
pub struct Request<A: Allocator + Clone> {
    alloc: A,
    config: Config,
    command: Command,
    shape_digest: ContentId,
    units_source: ContentId,
    plant: Plant,
    phase: Phase,
    profile: Profile,

    files: FileRows<A>,
    paths: Vec<u8, A>,
    gone: Vec<ContentId, A>,
    cert: Key,
    last: Option<Last>,
    forward: bool,

    root: Option<Root<A>>,
    root_loading: bool,
    root_waiters: Vec<Name, A>,
    pages: SortedMap<u32, PageState<A>, A>,
    page_waiters: SortedMap<u32, Vec<Name, A>, A>,
    /// Rows this run changed or made.
    new: SortedMap<Name, Row<A>, A>,
    /// What each settled name is now.
    now: SortedMap<Name, Now, A>,
    /// Names a re-made home no longer lists.
    dropped: SortedSet<Name, A>,
    /// The locates of the parts of homes loaded or made this run.
    part_locs: SortedMap<Name, (Locate, u32, Name), A>,
    /// Readers posted this run: read name to the names that read it.
    posted: SortedMap<Name, Vec<Name, A>, A>,

    jobs: SortedMap<Name, Job<A>, A>,
    wake: VecDeque<Name, A>,
    gets: SortedMap<Key, Vec<Purpose, A>, A>,
    tickets: SortedMap<Ticket, (Name, BatchId), A>,
    next_ticket: u64,
    /// Queued jobs, in run order: the person's first, then by kind (files, scopes, signatures,
    /// bodies, code), then by home, so a batch is one home's tasks of one kind.
    ready: SortedSet<(Class, Kind, Name, Name), A>,
    /// Batches out: at most one per worker.
    batches: SortedSet<BatchId, A>,
    next_batch: u64,
    /// The settled table's writes this step, last write per name: flushed as `Publish` and
    /// `Unpublish` at the step's end, after every `Put` of the step.
    publish: SortedMap<Name, Option<Input>, A>,

    findings: SortedMap<Name, Held<A>, A>,
    old_findings: SortedMap<Name, Held<A>, A>,
    old_findings_loading: bool,
    units_at: Option<Key>,

    gate: Gate,
    /// The class the walk's jobs take: `Gate` beside a running program, else the person's.
    walk: Class,
    program: Program,
    /// The answer once the walk is done, held until the program's end (`conclude`).
    held: Option<Answer>,
    /// Error-level findings settled this run.
    errors: u32,
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
const UNITS_SOURCE_DOMAIN: &str = "blaze-drive units source 1";
const GROUP_DOMAIN: &str = "blaze-drive group 1";
const CERT_MAGIC: &[u8; 4] = b"BDC1";

impl<A: Allocator + Clone> Request<A> {
    pub fn new_in(config: Config, ask: Ask<'_>, alloc: A) -> Request<A> {
        Request::planted_in(config, ask, Plant::None, alloc)
    }

    /// A request with `plant` planted: for a simulation's control only.
    pub fn planted_in(config: Config, ask: Ask<'_>, plant: Plant, alloc: A) -> Request<A> {
        let mut h = Hasher::new(SHAPE_DOMAIN);
        h.update(&[ask.command.byte()]).part(ask.args);
        let a = alloc.clone();
        Request {
            config,
            command: ask.command,
            shape_digest: h.finish(),
            units_source: ABSENT,
            plant,
            phase: Phase::Idle,
            profile: Profile::default(),
            files: FileRows::new_in(a.clone()),
            paths: Vec::new_in(a.clone()),
            gone: Vec::new_in(a.clone()),
            cert: Key::default(),
            last: None,
            forward: false,
            root: None,
            root_loading: false,
            root_waiters: Vec::new_in(a.clone()),
            pages: SortedMap::new_in(a.clone()),
            page_waiters: SortedMap::new_in(a.clone()),
            new: SortedMap::new_in(a.clone()),
            now: SortedMap::new_in(a.clone()),
            dropped: SortedSet::new_in(a.clone()),
            part_locs: SortedMap::new_in(a.clone()),
            posted: SortedMap::new_in(a.clone()),
            jobs: SortedMap::new_in(a.clone()),
            wake: VecDeque::new_in(a.clone()),
            gets: SortedMap::new_in(a.clone()),
            tickets: SortedMap::new_in(a.clone()),
            next_ticket: 0,
            ready: SortedSet::new_in(a.clone()),
            batches: SortedSet::new_in(a.clone()),
            next_batch: 0,
            publish: SortedMap::new_in(a.clone()),
            findings: SortedMap::new_in(a.clone()),
            old_findings: SortedMap::new_in(a.clone()),
            old_findings_loading: false,
            units_at: None,
            gate: ask.gate,
            walk: match (ask.command, ask.gate) {
                (Command::Run | Command::Test, Gate::Beside) => Class::Gate,
                _ => Class::Foreground,
            },
            program: Program::Unstarted,
            held: None,
            errors: 0,
            alloc,
        }
    }

    /// `run` or `test`: a program is part of the answer.
    fn runs_program(&self) -> bool {
        matches!(self.command, Command::Run | Command::Test)
    }

    /// The units are known: under `Beside` and `Reached` the program starts now, unless an
    /// error is already found.
    fn units_known<B: Allocator>(&mut self, at: Key, out: &mut Vec<Effect<A>, B>) {
        if self.runs_program() && self.gate != Gate::Before && self.program == Program::Unstarted && self.errors == 0 {
            self.program = Program::Running;
            out.push(Effect::StartProgram { units: at });
        }
    }

    /// A settled record held `errors` error-level findings: under `Beside`, the first stops the
    /// running program.
    fn found_errors<B: Allocator>(&mut self, errors: u32, out: &mut Vec<Effect<A>, B>) {
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
    fn exited<B: Allocator>(&mut self, ok: bool, out: &mut Vec<Effect<A>, B>) -> Result<(), DriveError> {
        match self.program {
            Program::Running | Program::Stopped => {}
            Program::Unstarted | Program::Ended { .. } => return Err(DriveError::NoProgram),
        }
        if self.program == Program::Running {
            self.program = Program::Ended { ok };
        }
        if self.plant == Plant::EarlySuccess && ok && self.phase != Phase::Held {
            // The planted bug: success reported at the program's exit, the gate's work not done.
            self.phase = Phase::Answered;
            let answer = Answer { profile: self.profile, verdict: Verdict::Passed, findings: self.errors, units: self.units_at };
            out.push(Effect::Done(answer));
            return Ok(());
        }
        if self.gate == Gate::Reached && self.phase != Phase::Held {
            // Only what ran was checked: the verdict is the program's, said as such.
            self.phase = Phase::Answered;
            let verdict = if ok { Verdict::PassedReached } else { Verdict::ProgramFailed };
            out.push(Effect::Done(Answer { profile: self.profile, verdict, findings: self.errors, units: self.units_at }));
            return Ok(());
        }
        self.conclude(out);
        Ok(())
    }

    /// The answer, once both the check and the program (if any) are done: a success is never
    /// reported while an error is found or could still be.
    fn conclude<B: Allocator>(&mut self, out: &mut Vec<Effect<A>, B>) {
        let Some(mut answer) = self.held else { return };
        let started = self.program != Program::Unstarted;
        let verdict = match (self.runs_program(), self.program) {
            (false, _) => Verdict::Checked,
            _ if self.errors > 0 => Verdict::Failed { errors: self.errors, started },
            (true, Program::Unstarted) => {
                // Before: the check is clean, the program starts now.
                let units = answer.units.unwrap_or_default();
                self.program = Program::Running;
                out.push(Effect::StartProgram { units });
                return;
            }
            (true, Program::Running) => return,
            (true, Program::Ended { ok: true }) => Verdict::Passed,
            (true, Program::Ended { ok: false } | Program::Stopped) => Verdict::ProgramFailed,
        };
        answer.verdict = verdict;
        answer.profile = self.profile;
        self.held = None;
        self.phase = Phase::Answered;
        out.push(Effect::Done(answer));
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
                let home = self.jobs.get(&want.name).map_or(want.name, |job| job.home);
                self.jobs.get_mut(&want.name).expect("a job").class = Class::Foreground;
                if state == State::Queued && self.ready.remove(&(class, want.name.kind, home, want.name)) {
                    self.queue(Class::Foreground, want.name);
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
    pub fn step<B: Allocator>(&mut self, event: Event<'_>, out: &mut Vec<Effect<A>, B>) -> Result<(), DriveError> {
        if self.phase == Phase::Answered {
            return Err(DriveError::Finished);
        }
        self.profile.steps += 1;
        match event {
            Event::Start { files, gone, last } => self.start(files, gone, last, out)?,
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
    fn flush_publish<B: Allocator>(&mut self, out: &mut Vec<Effect<A>, B>) {
        let pending = core::mem::replace(&mut self.publish, SortedMap::new_in(self.alloc.clone()));
        for (name, input) in pending.into_iter() {
            out.push(match input {
                Some(input) => Effect::Publish(input),
                None => Effect::Unpublish(name),
            });
        }
    }

    fn ran_batch<B: Allocator>(&mut self, batch: BatchId, results: &[Outcome<'_>], out: &mut Vec<Effect<A>, B>) -> Result<(), DriveError> {
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

    fn start<B: Allocator>(
        &mut self,
        files: &[FileEntry<'_>],
        gone: &[&[u8]],
        last: Option<&[u8]>,
        out: &mut Vec<Effect<A>, B>,
    ) -> Result<(), DriveError> {
        if files.windows(2).any(|w| w[0].path >= w[1].path) {
            return Err(DriveError::Unsorted { what: "the sweep's files" });
        }
        let mut units = Hasher::new(UNITS_SOURCE_DOMAIN);
        let mut rows = Vec::with_capacity_in(files.len(), self.alloc.clone());
        for file in files {
            if file.manifest {
                units.part(file.path).id(&file.id);
            }
            let at = u32::try_from(self.paths.len()).expect("the checkout's paths fit 4 GiB");
            self.paths.extend_from_slice(file.path);
            let len = u32::try_from(file.path.len()).expect("a path fits 4 GiB");
            rows.push(FileRow { subject: Name::file(file.path).subject, id: file.id, path_at: at, path_len: len, changed: file.changed });
        }
        // Sorted by subject, the way a job finds its file.
        rows.sort_unstable_by_key(|row| row.subject);
        for row in rows {
            self.files.push(row);
        }
        for path in gone {
            self.gone.push(Name::file(path).subject);
        }
        self.units_source = units.finish();
        let mut command = [0u8; 33];
        command[0] = self.command.byte();
        command[1..].copy_from_slice(&self.shape_digest.0);
        self.cert = key::cert_key(self.config.tool, &command, files.iter().map(|f| (f.path, &f.id)));
        self.last = match last {
            None => None,
            Some(bytes) => Some(answers::read_last(bytes).map_err(DriveError::LastFile)?),
        };
        self.phase = Phase::Certifying;
        self.get(self.cert, Purpose::Cert, out);
        Ok(())
    }

    /// The certificate missed: start the walk.
    fn begin<B: Allocator>(&mut self, out: &mut Vec<Effect<A>, B>) -> Result<(), DriveError> {
        self.phase = Phase::Working;
        let last = self.last.filter(|last| last.tool == self.config.tool.0);
        self.last = last;
        if let Some(last) = last {
            self.root_loading = true;
            self.get(last.root, Purpose::Root, out);
            self.old_findings_loading = true;
            self.get(last.cert, Purpose::OldCert, out);
            self.forward = last.shape == self.shape_digest && last.units == self.units_source;
        }
        if self.forward {
            let mut seeds = Vec::new_in(self.alloc.clone());
            for (subject, changed) in self.files.subjects().iter().zip(self.files.changeds()) {
                if *changed {
                    seeds.push(*subject);
                }
            }
            seeds.extend_from_slice(&self.gone);
            for subject in seeds.iter() {
                let name = Name::new(Kind::FileItems, *subject);
                self.add_job(name, name, None, true, self.walk)?;
            }
        } else {
            let units = Name::units();
            self.add_job(units, units, Some(Locate::Source(self.units_source)), false, Class::Foreground)?;
        }
        Ok(())
    }

    fn certified<B: Allocator>(&mut self, bytes: &[u8], out: &mut Vec<Effect<A>, B>) -> Result<(), DriveError> {
        let (units, held) = read_cert(bytes, self.alloc.clone()).map_err(|error| DriveError::Wire { key: self.cert, error })?;
        self.profile.certified = true;
        let count = self.print(&held, out);
        let is_error = self.config.is_error;
        let errors = held.values().flat_map(|entry| entry.findings.iter()).filter(|f| is_error(f.rule)).count();
        self.errors = u32::try_from(errors).unwrap_or(u32::MAX);
        self.units_at = units;
        // The repeat: the walk stands whole. A stored error refuses the start at this one read
        // (every gate); a clean one starts the program now, nothing left to check beside it.
        self.held = Some(Answer { profile: self.profile, verdict: Verdict::Checked, findings: count, units });
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

    fn get<B: Allocator>(&mut self, key: Key, purpose: Purpose, out: &mut Vec<Effect<A>, B>) {
        match self.gets.get_mut(&key) {
            Some(purposes) => purposes.push(purpose),
            None => {
                let mut purposes = Vec::new_in(self.alloc.clone());
                purposes.push(purpose);
                self.gets.insert(key, purposes);
                self.profile.gets += 1;
                out.push(Effect::Get { key });
            }
        }
    }

    fn put<B: Allocator>(&mut self, key: Key, bytes: Vec<u8, A>, out: &mut Vec<Effect<A>, B>) {
        self.profile.puts += 1;
        out.push(Effect::Put { key, bytes });
    }

    fn got<B: Allocator>(&mut self, key: Key, bytes: Option<&[u8]>, out: &mut Vec<Effect<A>, B>) -> Result<(), DriveError> {
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
                        self.old_findings = read_cert(bytes, self.alloc.clone()).map_err(|error| DriveError::Wire { key, error })?.1;
                    }
                }
                Purpose::Root => {
                    self.root_loading = false;
                    // A root the store lost is an empty table: every job is made or found again.
                    self.root = match bytes {
                        Some(bytes) => Some(answers::read_root(bytes, self.alloc.clone()).map_err(|error| DriveError::Wire { key, error })?),
                        None => None,
                    };
                    if self.root.is_none() {
                        // No old table to walk forward from: walk the whole build from the
                        // units, as a first build does (the seeds find no row and go).
                        self.last = None;
                        if self.forward {
                            self.forward = false;
                            let units = Name::units();
                            self.add_job(units, units, Some(Locate::Source(self.units_source)), false, Class::Foreground)?;
                        }
                    }
                    let waiters = core::mem::replace(&mut self.root_waiters, Vec::new_in(self.alloc.clone()));
                    for name in waiters.iter() {
                        self.wake.push_back(*name);
                    }
                }
                Purpose::Page(index) => {
                    self.profile.pages += 1;
                    let page = match bytes {
                        Some(bytes) => answers::read_page(bytes, self.alloc.clone()).map_err(|error| DriveError::Wire { key, error })?,
                        None => SortedMap::new_in(self.alloc.clone()),
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

    fn ran<B: Allocator>(&mut self, ticket: Ticket, ran: &Ran<'_>, out: &mut Vec<Effect<A>, B>) -> Result<(), DriveError> {
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
            return match ran {
                Ran::Failed(failure) => Err(DriveError::Task(*failure)),
                Ran::Done(_) | Ran::Need(_) => Ok(()),
            };
        }
        match *ran {
            Ran::Failed(failure) => Err(DriveError::Task(failure)),
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
                        self.add_job(want.name, want.home, None, false, class)?;
                    } else if self.plant == Plant::DoubleFlight
                        && self.jobs.get(&want.name).is_some_and(|job| job.state == State::Running)
                    {
                        let mut tasks = Vec::new_in(self.alloc.clone());
                        tasks.push(self.emit(want.name));
                        self.send(tasks, out);
                    }
                    let job = self.jobs.get_mut(&name).expect("a running job's ticket names it");
                    if let Err(at) = job.needs.binary_search(&want.name) {
                        job.needs.insert(at, want.name);
                    }
                }
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
        let a = self.alloc.clone();
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
                needs: Vec::new_in(a.clone()),
                shape: Vec::new_in(a.clone()),
                waiters: Vec::new_in(a.clone()),
                members: Vec::new_in(a),
            },
        );
        self.wake.push_back(name);
        Ok(())
    }

    fn set_state(&mut self, name: Name, state: State) {
        if let Some(job) = self.jobs.get_mut(&name) {
            job.state = state;
        }
    }

    fn state(&self, name: Name) -> Option<State> {
        self.jobs.get(&name).map(|job| job.state)
    }

    fn wait_on(&mut self, target: Name, waiter: Name) {
        if let Some(job) = self.jobs.get_mut(&target)
            && !job.waiters.contains(&waiter)
        {
            job.waiters.push(waiter);
        }
    }

    fn wake_waiters(&mut self, name: Name) {
        let Some(job) = self.jobs.get_mut(&name) else { return };
        let waiters = core::mem::replace(&mut job.waiters, Vec::new_in(self.alloc.clone()));
        for waiter in waiters.iter() {
            self.wake.push_back(*waiter);
        }
    }

    /// Whether the old table is known for `name`: `Some(row or none)` when it is, `None` when
    /// `waiter` now waits for the root or the page.
    fn old_ready<B: Allocator>(&mut self, name: Name, waiter: Name, out: &mut Vec<Effect<A>, B>) -> bool {
        if self.last.is_none() {
            return true;
        }
        let Some(root) = &self.root else {
            if !self.root_waiters.contains(&waiter) {
                self.root_waiters.push(waiter);
            }
            return false;
        };
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
                        self.pages.insert(index, PageState::Loaded(SortedMap::new_in(self.alloc.clone())));
                        self.wake.push_back(waiter);
                    }
                    false => self.get(key, Purpose::Page(index), out),
                }
                false
            }
        }
    }

    fn page_wait(&mut self, index: u32, waiter: Name) {
        match self.page_waiters.get_mut(&index) {
            Some(list) => list.push(waiter),
            None => {
                let mut list = Vec::new_in(self.alloc.clone());
                list.push(waiter);
                self.page_waiters.insert(index, list);
            }
        }
    }

    /// The old row of `name`, its page loaded.
    fn old(&self, name: Name) -> Option<&Row<A>> {
        let root = self.root.as_ref()?;
        match self.pages.get(&answers::page_of(name, root.bits))? {
            PageState::Loaded(page) => page.get(&name),
            PageState::Loading => None,
        }
    }

    /// The row this run knows for `name`: made this run, else the old one.
    fn current(&self, name: Name) -> Option<&Row<A>> {
        self.new.get(&name).or_else(|| self.old(name))
    }

    /// What a read of `read` sees now, or that `waiter` waits for it.
    fn digest<B: Allocator>(&mut self, read: Name, waiter: Name, out: &mut Vec<Effect<A>, B>) -> Digest {
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
        self.files.subjects().binary_search(subject).ok()
    }

    fn pump<B: Allocator>(&mut self, out: &mut Vec<Effect<A>, B>) -> Result<(), DriveError> {
        loop {
            while let Some(name) = self.wake.pop_front() {
                self.advance(name, out)?;
            }
            self.emit_ready(out);
            if self.phase == Phase::Working && self.quiet() {
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
        self.wake.is_empty() && self.gets.is_empty() && self.tickets.is_empty() && self.ready.is_empty()
    }

    /// Queue a job to run, keyed for its batch.
    fn queue(&mut self, class: Class, name: Name) {
        let home = self.jobs.get(&name).map_or(name, |job| job.home);
        self.ready.insert((class, name.kind, home, name));
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
        self.jobs.values().any(|job| job.state != State::Settled)
    }

    /// Batches out while workers are free: every foreground task before any background one
    /// (the ready set sorts the person's first, so a background batch goes only when no
    /// foreground task is queued). A batch is the next tasks of one class, kind and home, as
    /// many as make [`TARGET_BATCH_NS`] at this run's mean time of the kind, and no more than
    /// an even share of that kind's queue over the free workers, so no worker idles while
    /// another holds a long batch.
    fn emit_ready<B: Allocator>(&mut self, out: &mut Vec<Effect<A>, B>) {
        let workers = self.config.workers.max(1) as usize;
        while self.batches.len() < workers {
            let Some(&(class, kind, home, _)) = self.ready.iter().next() else { return };
            let free = workers - self.batches.len();
            let of_kind = self.ready.iter().take_while(|(c, k, _, _)| *c == class && *k == kind).count();
            let by_time = usize::try_from(TARGET_BATCH_NS / self.task_ns(kind)).unwrap_or(MOST_BATCH);
            let size = by_time.clamp(1, MOST_BATCH).min(of_kind.div_ceil(free)).max(1);
            let mut names = Vec::with_capacity_in(size, self.alloc.clone());
            for (c, k, h, name) in self.ready.iter() {
                if names.len() == size || *c != class || *k != kind {
                    break;
                }
                // One home per batch while that home has tasks; then the next home's.
                if *h != home && !names.is_empty() && size < of_kind {
                    break;
                }
                names.push((*c, *k, *h, *name));
            }
            let mut tasks = Vec::with_capacity_in(names.len(), self.alloc.clone());
            for entry in names.iter() {
                self.ready.remove(entry);
                tasks.push(self.emit(entry.3));
            }
            self.send(tasks, out);
        }
    }

    fn send<B: Allocator>(&mut self, tasks: Vec<(Ticket, Task<A>), A>, out: &mut Vec<Effect<A>, B>) {
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

    fn emit(&mut self, name: Name) -> (Ticket, Task<A>) {
        let ticket = Ticket(self.next_ticket);
        self.next_ticket += 1;
        let a = self.alloc.clone();
        let mut inputs = Vec::new_in(a.clone());
        let mut members = Vec::new_in(a.clone());
        let job = self.jobs.get(&name).expect("a queued job is a job");
        for need in job.needs.iter() {
            if let Some(now) = self.now.get(need).copied().or_else(|| self.old(*need).map(|row| Now { at: row.at, out: row.out })) {
                inputs.push(Input { name: *need, at: now.at, out: now.out });
            }
        }
        members.extend_from_slice(&job.members);
        let edition = match job.locate {
            Some(Locate::File { edition }) => edition,
            _ => 0,
        };
        let file = match name.kind {
            Kind::FileItems => self.file_index(&name.subject).map(|i| i as u32),
            _ => None,
        };
        let task = Task {
            name,
            key: job.key,
            source: job.source,
            class: job.class,
            entry: job.entries,
            home: job.home,
            place: job.place,
            file,
            edition,
            inputs,
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

    fn advance<B: Allocator>(&mut self, name: Name, out: &mut Vec<Effect<A>, B>) -> Result<(), DriveError> {
        let Some(state) = self.state(name) else { return Ok(()) };
        match state {
            State::Start => self.locate(name, out),
            State::Decide => self.decide(name, out),
            State::Value => self.value(name, out),
            State::Gather => self.gather(name, out),
            State::Looking | State::Valuing | State::Queued | State::Running | State::Settled => Ok(()),
        }
    }

    /// Start: find the job's source and key.
    fn locate<B: Allocator>(&mut self, name: Name, out: &mut Vec<Effect<A>, B>) -> Result<(), DriveError> {
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
            match self.old(name).map(|row| (row.source, row.place, row.home)) {
                Some((source, at, by)) => {
                    locate = Some(match name.kind {
                        Kind::FileItems => Locate::File { edition: at as u8 },
                        _ => Locate::Source(source),
                    });
                    place = at;
                    self.jobs.get_mut(&name).expect("a job").home = by;
                }
                None if seed => {
                    // A changed file no answer holds: outside the build, until a module lists it.
                    self.jobs.remove(&name);
                    return Ok(());
                }
                None if home != name && self.state(home).is_none() => {
                    let class = self.jobs.get(&name).map_or(self.walk, |job| job.class);
                    self.add_job(home, home, None, false, class)?;
                    self.wait_on(home, name);
                    return Ok(());
                }
                None => return Err(DriveError::NoPart { name, home }),
            }
        }
        let locate = locate.expect("found above");
        let (source, key) = match locate {
            Locate::Source(source) => (source, key::task_key(self.config.tool, name, &source)),
            Locate::Key(key) => (key.0, key),
            Locate::File { edition } => match self.file_index(&name.subject) {
                Some(index) => {
                    let source = key::file_source(self.files.ids()[index], edition);
                    (source, key::task_key(self.config.tool, name, &source))
                }
                // A file gone from the checkout: an error only if its home, decided this run,
                // still lists it; a home re-made without it drops it first.
                None => {
                    let home = self.jobs.get(&name).map_or(home, |job| job.home);
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

    fn decide<B: Allocator>(&mut self, name: Name, out: &mut Vec<Effect<A>, B>) -> Result<(), DriveError> {
        let job = self.jobs.get(&name).expect("a job");
        let (key, recheck) = (job.key, job.recheck);
        if name.kind == Kind::Group {
            self.set_state(name, State::Queued);
            self.queue(self.walk, name);
            return Ok(());
        }
        let row = self.current(name).map(|row| (row.key, row.reads.iter().map(|(n, _)| *n).collect_in(self.alloc.clone())));
        match row {
            Some((held, _)) if held == key && !recheck => self.stand(name, out),
            Some((held, reads)) if held == key && !reads.is_empty() => {
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
    fn value<B: Allocator>(&mut self, name: Name, out: &mut Vec<Effect<A>, B>) -> Result<(), DriveError> {
        let shape = core::mem::replace(&mut self.jobs.get_mut(&name).expect("a job").shape, Vec::new_in(self.alloc.clone()));
        let mut reads = Vec::with_capacity_in(shape.len(), self.alloc.clone());
        let mut waiting = false;
        for read in shape.iter() {
            match self.digest(*read, name, out) {
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
        let at = key::value_key(job.key, &reads);
        job.state = State::Valuing;
        self.get(at, Purpose::Value(name), out);
        Ok(())
    }

    fn looked<B: Allocator>(&mut self, name: Name, key: Key, bytes: Option<&[u8]>, out: &mut Vec<Effect<A>, B>) -> Result<(), DriveError> {
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
                let mut names = Vec::with_capacity_in(shape.names().len(), self.alloc.clone());
                names.extend(shape.names());
                let job = self.jobs.get_mut(&name).expect("a job");
                job.shape = names;
                job.state = State::Value;
                self.value(name, out)
            }
        }
    }

    fn valued<B: Allocator>(&mut self, name: Name, key: Key, bytes: Option<&[u8]>, out: &mut Vec<Effect<A>, B>) -> Result<(), DriveError> {
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
    fn found<B: Allocator>(&mut self, name: Name, at: Key, view: WholeView<'_>, out: &mut Vec<Effect<A>, B>) -> Result<(), DriveError> {
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
    fn to_run<B: Allocator>(&mut self, name: Name, out: &mut Vec<Effect<A>, B>) -> Result<(), DriveError> {
        let job = self.jobs.get(&name).expect("a job");
        if let Some(Locate::Key(key)) = job.locate {
            return Err(DriveError::Missing { name, key, builder: Builder::Pack { krate: job.home } });
        }
        let hints = self.current(name).map(|row| row.reads.iter().map(|(n, _)| *n).collect_in(self.alloc.clone()));
        if let Some(hints) = hints {
            let job = self.jobs.get_mut(&name).expect("a job");
            for hint in hints.iter() {
                if let Err(at) = job.needs.binary_search(hint) {
                    job.needs.insert(at, *hint);
                }
            }
        }
        self.set_state(name, State::Gather);
        self.gather(name, out)
    }

    /// Wait for every need to settle, then queue the run.
    fn gather<B: Allocator>(&mut self, name: Name, out: &mut Vec<Effect<A>, B>) -> Result<(), DriveError> {
        let needs = core::mem::replace(&mut self.jobs.get_mut(&name).expect("a job").needs, Vec::new_in(self.alloc.clone()));
        let mut waiting = false;
        for need in needs.iter() {
            if let Digest::Wait = self.digest(*need, name, out) {
                waiting = true;
            }
        }
        let job = self.jobs.get_mut(&name).expect("a job");
        job.needs = needs;
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

    fn stand<B: Allocator>(&mut self, name: Name, out: &mut Vec<Effect<A>, B>) -> Result<(), DriveError> {
        self.profile.of(name.kind).standing += 1;
        self.settle(name, None, false, out)
    }

    /// A task made `output`: store it and settle its job.
    fn made<B: Allocator>(&mut self, output: &Output<'_>, out: &mut Vec<Effect<A>, B>) -> Result<(), DriveError> {
        let job = self.jobs.get(&output.name).expect("checked by the caller");
        let (key, source) = (job.key, job.source);
        // A read whose digest is not today's makes a record valid for its own reads, stored,
        // but not the answer: the job is decided again.
        let mut stale = false;
        for (read, saw) in output.reads {
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
            reads: output.reads,
            parts: output.parts,
            findings: output.findings,
            payload: output.payload,
        };
        let bytes = record::write_whole(&whole, self.alloc.clone());
        let at = match output.reads.is_empty() {
            true => key,
            false => {
                let shape = record::write_shape(output.name, output.reads, self.alloc.clone());
                self.put(key, shape, out);
                key::value_key(key, output.reads)
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
    fn settle<B: Allocator>(
        &mut self,
        name: Name,
        record: Option<(Key, WholeView<'_>)>,
        stale: bool,
        out: &mut Vec<Effect<A>, B>,
    ) -> Result<(), DriveError> {
        let a = self.alloc.clone();
        let before = self.now.get(&name).copied().or_else(|| self.old(name).map(|row| Now { at: row.at, out: row.out }));
        let job = self.jobs.get(&name).expect("a job");
        let (key, source, home, place, settles) = (job.key, job.source, job.home, job.place, job.settles);
        if settles >= self.config.most_entries {
            let mut last_needs = [None; 4];
            for (slot, need) in last_needs.iter_mut().zip(job.needs.iter()) {
                *slot = Some(*need);
            }
            return Err(DriveError::Reentries { name, key, entries: settles, last_needs });
        }
        let mut parts_now = Vec::new_in(a.clone());
        let now = match &record {
            None => {
                let row = self.current(name).expect("a standing job has a row");
                parts_now.extend_from_slice(&row.parts);
                Now { at: row.at, out: row.out }
            }
            Some((at, view)) => {
                let old = self.current(name);
                let readers = old.map(|row| row.readers.clone()).unwrap_or_else(|| Vec::new_in(a.clone()));
                let old_parts = old.map(|row| row.parts.clone()).unwrap_or_else(|| Vec::new_in(a.clone()));
                let mut reads = Vec::with_capacity_in(view.reads().len(), a.clone());
                reads.extend(view.reads());
                let mut parts = Vec::with_capacity_in(view.parts().len(), a.clone());
                for part in view.parts() {
                    parts.push(part.name);
                    self.part_locs.insert(part.name, (part.locate, part.place, name));
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
                        Some(list) if list.contains(&name) => {}
                        Some(list) => list.push(name),
                        None => {
                            let mut list = Vec::new_in(a.clone());
                            list.push(name);
                            self.posted.insert(*read, list);
                        }
                    }
                }
                let held = Held { file: file_of(name, home), place, findings: hold(view, a.clone()) };
                let is_error = self.config.is_error;
                let errors = held.findings.iter().filter(|f| is_error(f.rule)).count();
                self.findings.insert(name, held);
                self.found_errors(u32::try_from(errors).unwrap_or(u32::MAX), out);
                self.new.insert(
                    name,
                    Row { key, at: *at, source, out: view.out, home, place, reads, parts, readers },
                );
                Now { at: *at, out: view.out }
            }
        };
        self.now.insert(name, now);
        self.dropped.remove(&name);
        if name.kind == Kind::Units {
            // Standing or made, the units' record is where the program's entry is.
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
        let job = self.jobs.get_mut(&name).expect("a job");
        job.settles += 1;
        job.state = State::Settled;
        job.recheck = false;
        let redo = core::mem::replace(&mut job.redo, false) || stale;
        let entries = job.entries;
        self.profile.entries_seen(entries);
        if self.program == Program::Running && job.class == Class::Gate {
            self.profile.checked_beside += 1;
        }
        // A record about to be decided again is not one a worker should read.
        if !redo {
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
        let mut work = Vec::new_in(self.alloc.clone());
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
        if let Some(job) = self.jobs.get_mut(&name) {
            job.state = State::Settled;
        }
        self.ready.retain(|(_, _, _, queued)| *queued != name);
        self.wake_waiters(name);
    }

    /// `name`'s out digest changed: every reader is checked again.
    fn dirty_readers(&mut self, name: Name) -> Result<(), DriveError> {
        if self.plant == Plant::SkipChangedRead {
            return Ok(());
        }
        let mut readers = Vec::new_in(self.alloc.clone());
        if let Some(row) = self.old(name) {
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
                let job = self.jobs.get_mut(&name).expect("a job");
                job.recheck = true;
                job.state = State::Decide;
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
        let mut cycle = Vec::new_in(self.alloc.clone());
        for (name, job) in self.jobs.iter() {
            if job.state != State::Settled {
                cycle.push(*name);
            }
        }
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
    fn finish<B: Allocator>(&mut self, out: &mut Vec<Effect<A>, B>) -> Result<(), DriveError> {
        let a = self.alloc.clone();
        // The reverse index: each made row's reads against its old row's.
        let mut updates: Vec<(Name, Name, bool), A> = Vec::new_in(a.clone());
        for (name, row) in self.new.iter() {
            let old_reads = self.old(*name).map(|old| old.reads.clone());
            for (read, _) in row.reads.iter() {
                if !old_reads.as_ref().is_some_and(|old| old.iter().any(|(r, _)| r == read)) {
                    updates.push((*read, *name, true));
                }
            }
            if let Some(old) = old_reads {
                for (read, _) in old.iter() {
                    if !row.reads.iter().any(|(r, _)| r == read) {
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
        for name in self.new.keys() {
            if self.old(*name).is_none() {
                added += 1;
            }
        }
        let removed = self.dropped.iter().filter(|name| self.old(**name).is_some()).count() as u64;
        let rows = (old_rows + added).saturating_sub(removed);
        let bits = answers::bits_for(rows);
        let old_bits = self.root.as_ref().map(|root| root.bits);
        let mut wanted = SortedSet::new_in(a.clone());
        match old_bits {
            Some(old_bits) if old_bits != bits => {
                for index in 0..(1u32 << old_bits) {
                    wanted.insert(index);
                }
            }
            Some(old_bits) => {
                for name in self.new.keys().chain(self.dropped.iter()).chain(updates.iter().map(|(t, _, _)| t)) {
                    wanted.insert(answers::page_of(*name, old_bits));
                }
            }
            None => {}
        }
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
        // Apply the reverse index's updates.
        for (target, reader, add) in updates.iter() {
            if self.dropped.contains(target) {
                continue;
            }
            if !self.new.contains_key(target) {
                let Some(old) = self.old(*target).cloned() else { continue };
                self.new.insert(*target, old);
            }
            let row = self.new.get_mut(target).expect("inserted above");
            match (*add, row.readers.binary_search(reader)) {
                (true, Err(at)) => row.readers.insert(at, *reader),
                (false, Ok(at)) => {
                    row.readers.remove(at);
                }
                _ => {}
            }
        }
        // The touched pages, rewritten whole, and the root.
        let mut touched = SortedSet::new_in(a.clone());
        match old_bits {
            Some(old_bits) if old_bits == bits => {
                for name in self.new.keys().chain(self.dropped.iter()) {
                    touched.insert(answers::page_of(*name, bits));
                }
            }
            _ => {
                for index in 0..(1u32 << bits) {
                    touched.insert(index);
                }
            }
        }
        let mut page_keys = match (&self.root, old_bits) {
            (Some(root), Some(old_bits)) if old_bits == bits => root.pages.clone(),
            _ => {
                let mut keys = Vec::new_in(a.clone());
                keys.resize(1usize << bits, Key::default());
                keys
            }
        };
        for index in touched.iter() {
            let mut page: Page<A> = SortedMap::new_in(a.clone());
            for state in self.pages.values() {
                if let PageState::Loaded(old) = state {
                    for (name, row) in old.iter() {
                        if answers::page_of(*name, bits) == *index && !self.dropped.contains(name) && !self.new.contains_key(name) {
                            page.insert(*name, row.clone());
                        }
                    }
                }
            }
            for (name, row) in self.new.iter() {
                if answers::page_of(*name, bits) == *index && !self.dropped.contains(name) {
                    page.insert(*name, row.clone());
                }
            }
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
        let mut all = core::mem::replace(&mut self.old_findings, SortedMap::new_in(a.clone()));
        let made = core::mem::replace(&mut self.findings, SortedMap::new_in(a.clone()));
        for (name, held) in made.into_iter() {
            all.insert(name, held);
        }
        all.retain(|name, held| !held.findings.is_empty() && !self.dropped.contains(name));
        for (name, job) in self.jobs.iter() {
            if name.kind == Kind::Test {
                match self.new.get(name).is_some_and(|row| Some(row.key) == Some(job.key)) && job.entries > 0 {
                    true => self.profile.tests_run += 1,
                    false => self.profile.tests_reused += 1,
                }
            }
        }
        let units = self.units_at.or_else(|| self.old(Name::units()).map(|row| row.at));
        let cert = write_cert(&all, units, a.clone());
        self.put(self.cert, cert, out);
        let last = Last {
            tool: self.config.tool.0,
            shape: self.shape_digest,
            units: self.units_source,
            cert: self.cert,
            root: root_key,
        };
        out.push(Effect::WriteLast { bytes: answers::write_last(&last, a) });
        let count = self.print(&all, out);
        // Every finding of the answer counts, the stood ones too: an unchanged item's error is
        // still an error of the scope.
        let is_error = self.config.is_error;
        let errors = all.values().flat_map(|entry| entry.findings.iter()).filter(|f| is_error(f.rule)).count();
        self.errors = u32::try_from(errors).unwrap_or(u32::MAX);
        if self.gate == Gate::Beside && self.program == Program::Running && self.errors > 0 {
            self.program = Program::Stopped;
            out.push(Effect::Stop { errors: self.errors });
        }
        self.held = Some(Answer { profile: self.profile, verdict: Verdict::Checked, findings: count, units });
        self.phase = Phase::Held;
        self.conclude(out);
        Ok(())
    }

    /// `Print` effects for `held`, sorted by path and position; how many.
    fn print<B: Allocator>(&self, held: &SortedMap<Name, Held<A>, A>, out: &mut Vec<Effect<A>, B>) -> u32 {
        let mut order = Vec::with_capacity_in(held.len(), self.alloc.clone());
        for (name, entry) in held.iter() {
            let file = match entry.file.kind {
                Kind::FileItems => self.file_index(&entry.file.subject).map(|i| i as u32),
                _ => None,
            };
            let path: &[u8] = file.map_or(&[], |i| self.path(i));
            order.push((path, entry.place, *name, file));
        }
        order.sort_unstable_by(|x, y| (x.0, x.1, x.2).cmp(&(y.0, y.1, y.2)));
        let mut count = 0;
        for (_, place, name, file) in order.iter() {
            let entry = held.get(name).expect("listed above");
            for finding in entry.findings.iter() {
                let mut message = Vec::with_capacity_in(finding.message.len(), self.alloc.clone());
                message.extend_from_slice(&finding.message);
                out.push(Effect::Print(Print {
                    name: *name,
                    file: *file,
                    place: *place,
                    rule: finding.rule,
                    start: finding.start,
                    end: finding.end,
                    message,
                }));
                count += 1;
            }
        }
        count
    }
}

/// The file a name's findings are placed in: its own for a file, its home's for an item.
fn file_of(name: Name, home: Name) -> Name {
    match name.kind {
        Kind::FileItems => name,
        _ => home,
    }
}

fn hold<A: Allocator + Clone>(view: &WholeView<'_>, alloc: A) -> Vec<HeldFinding<A>, A> {
    let mut held = Vec::with_capacity_in(view.finding_count(), alloc.clone());
    for finding in view.findings() {
        let mut message = Vec::with_capacity_in(finding.message.len(), alloc.clone());
        message.extend_from_slice(finding.message);
        held.push(HeldFinding { rule: finding.rule, start: finding.start, end: finding.end, message });
    }
    held
}

/// The certificate: the units' record (so a repeat `run` starts at one read), then every
/// finding of the answer.
fn write_cert<A: Allocator + Clone>(held: &SortedMap<Name, Held<A>, A>, units: Option<Key>, alloc: A) -> Vec<u8, A> {
    let mut w = Writer::new_in(CERT_MAGIC, alloc);
    w.u32(u32::from(units.is_some()));
    w.raw(&units.unwrap_or_default().0.0);
    w.count(held.len());
    for (name, entry) in held.iter() {
        w.name(*name);
        w.name(entry.file);
        w.u32(entry.place);
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

type Cert<A> = (Option<Key>, SortedMap<Name, Held<A>, A>);

fn read_cert<A: Allocator + Clone>(bytes: &[u8], alloc: A) -> Result<Cert<A>, WireError> {
    let mut r = Reader::new(bytes, CERT_MAGIC, "certificate")?;
    let has_units = r.u32()? == 1;
    let mut units = [0u8; 32];
    units.copy_from_slice(r.take_bytes(32)?);
    let units = has_units.then_some(Key(ContentId(units)));
    let n = r.count(NAME_BYTES * 2 + 8)?;
    let mut entries = Vec::with_capacity_in(n, alloc.clone());
    for _ in 0..n {
        let name = r.name()?;
        let file = r.name()?;
        let place = r.u32()?;
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
        entries.push((name, Held { file, place, findings }));
    }
    r.done()?;
    let held = SortedMap::from_sorted(entries).map_err(|_| r.error(crate::wire::WireWhy::Unsorted))?;
    Ok((units, held))
}
