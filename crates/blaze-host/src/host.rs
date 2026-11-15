//! The host loop: one request, from its [`Plan`] to its [`Outcome`], every [`Effect`] of
//! blaze-keyed's driver performed on a real [`Store`], a real worker pool and a [`Front`].
//!
//! Each effect, and what answers it:
//!
//! - [`Effect::Get`]: a flat record (a body, a shape node, a mark, a chunk, a manifest root, the
//!   certificate) by [`Store::read`] (copied to 16-aligned bytes only when the store's buffer is
//!   not), a surface in place by [`Store::map`]; a surface this request wrote is answered from
//!   the bytes it filed (an uncommitted record has no mapped place).
//! - [`Effect::Answer`]: [`Universe::answer`] over the tables the effect names
//!   ([`TableRef`]): each surface mapped in place, each made table read once from its manifest
//!   root and kept for the request, the made facts of the asking unit and of every unit whose
//!   bodies are not asked (a `TableRef` with no manifest) as the effect gives them (its own
//!   made table merged with the makers demanded in the request), the asking
//!   unit's own late reads ([`Request::late`], one table per unit: a lookup in one table, the
//!   floor; no unit sees another's). No front end is asked.
//! - [`Effect::Run`]: the same tables handed to the task; on the request's thread while at most
//!   one task is ready, else on blaze-graph's pool ([`crate::workers`]), started once with
//!   [`Machine::workers_for`] threads. `remote` is refused: the executor is [`Local`].
//! - [`Effect::Put`]: [`Store::put`]; the certificate's value is the request's last put, and
//!   [`Store::commit`] runs before it is answered, so a failed sync is the driver's
//!   [`StoreFault`] and the request fails by it. A request that files no value (the shape slots
//!   are full, or an audit) commits at its end, and a failed sync then is [`HostError::Commit`].
//! - [`Effect::Outside`] and [`Effect::Reads`]: [`crate::outside::Answerer`].
//! - [`Effect::Link`] (a program's one link step, [`run_programs`]): each row's new code record
//!   and each span table that came or moved read from the store, then [`Front::link`] on the
//!   request's thread, its panic caught. The patch it returns is held until the driver's
//!   [`Effect::Image`] for that program.
//! - [`Effect::Image`]: [`Images::make`], the program's image made in the store under the kept
//!   manifest's key: the held patch applied over its base ([`PatchBase`]), a clone of the last
//!   image or an empty file.
//! - [`Effect::Place`]: [`Images::place`], the path the user runs made to name the image: the
//!   last effect of every answer, certified ones too.
//!   A plan task ([`Task::Plan`]) is handed its bodies' records read where the driver names them;
//!   a program's tasks and asks are of no unit ([`NO_UNIT`]) and see no late table, but a span
//!   task ([`Task::Spans`]) is its owner's unit's and sees that unit's.
//!
//! The driver names a task's tables only once they are settled (a unit's own surface is in its
//! bodies' universe only after it is proved), so nothing waits here: every effect is answered
//! when it is performed, and a task is ready when it is asked.
//!
//! The loop is bounded: each turn performs one effect the driver asked (finitely many, by its
//! plan), answers one finished task, or waits on the pool with a deadline that ends in
//! [`HostError::Stalled`]; a driver with nothing to do and nothing pending exits
//! [`Outcome::Stalled`] by itself.
//!
//! Floors (rule 67; host.md, "blaze-host"):
//! - A no-op: the certificate's shape read, one answer per outside read (one `lstat` for a file
//!   whose stamp the leaf memory holds), the value read; no task, no worker, no put, no commit.
//! - A unit settled by its mark: one read. Any other request: the tasks' own time, one store read
//!   per record the driver asks, one binary search per fact it asks answered, one put per record
//!   it files, one commit; a worker only while two or more tasks are ready. Beyond the floor:
//!   per answer or task, one vector of its tables (a pointer pair each, `O(universe)`) and the
//!   asking unit's made facts copied once into the task; per manifest root named, its made
//!   table copied once for the request; a surface this request wrote copied once to be shared
//!   by its readers.

use std::{
    alloc::{Allocator, Global},
    collections::VecDeque,
    fmt, io,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    time::Duration,
};

use blaze_core::wait::{Clock, Deadline, Monotonic, WaitError, Waited};
use blaze_format::Aligned;
use blaze_graph::{
    pool::{Pool as GraphPool, Unstopped},
    topology::Topology,
};
use blaze_ids::ContentId;
use blaze_keyed::{
    driver::{
        Asks, CertificateLookup, Done, Driver, Effect, Failure, Input, NO_UNIT, Outcome, Plan, PlanError, Step, StoreFault, TableRef,
        Task, TaskFailed, certificate_with,
    },
    exec::Local,
    front::{Front, Linked, Patch, PatchBase, Unanswerable, Universe},
    kind::{
        Body, Certificate, CodeRecord, Key, Kind, Layout, Mark, Namespace, PlanRecord, ProgramManifest, Shape, SpanTable, Surface, Tag,
        UnitChunk, UnitManifest,
    },
    machine::Machine,
    memo::Memo,
    outside::{MAX_SHAPES, shape_slot},
    pool::{Pool, Ticket},
    program::{NoWalk, ProgramPlan, Walker},
    proof::{AnyHolds, Certified, Holds, Snapshot},
    record::{self, Fact, StableId, Traced},
    store::{Put, Store, StoreError},
};

use crate::{
    outside::{self, Answerer, Ask, Files, Leaves, Looked, OutsideError},
    workers::{Bytes, Facts, Finish, Job, Made, Source, Workers, guarded, tables, unit_of},
};

/// One request as the command gives it.
#[derive(Clone, Copy, Debug)]
pub struct Request<'r> {
    pub plan: Plan<'r>,
    /// What the request read outside the store (its units' files, the variables its keys read):
    /// the shape its certificate is filed under, in this order.
    pub reads: &'r [Ask<'r>],
    /// The answer of each of `reads` when the request began (the bytes the command handed the
    /// front end, [`outside::file_answer`]): settled against the answers now before the
    /// certificate is filed, so a read that moved meanwhile files nothing.
    pub then: &'r [ContentId],
    /// The late reads each unit's key covers (`env!`, `include!`: [`record::late_id`]), indexed
    /// like `plan.units`: unit `u`'s table is `late[u]`, with the answers its key was made with,
    /// sorted by id, each once. What an ask of a late fact by unit `u` is answered from, by a
    /// task and by the driver alike, and by no other unit's table: two units read one name
    /// (`env!("CARGO_PKG_VERSION")`) under one id and see their own values. A late id names no
    /// unit because every table it is looked up in (a unit's dictionary, a record's reads, a
    /// task's universe) is one unit's.
    pub late: &'r [&'r [Fact]],
    /// The command's leaf memory: stamps that vouch for a file with no read.
    pub leaves: Leaves<'r>,
    /// The most one wait for a task may take with none finishing: past it the request stops,
    /// [`HostError::Stalled`].
    pub stall: Duration,
}

/// What one unit holds at the end: its manifest's proof, and every record proved, by item.
#[derive(Clone, Debug)]
pub struct UnitProved<'s> {
    pub manifest: Option<Holds<'s, UnitManifest>>,
    pub records: Vec<(StableId, AnyHolds<'s>), Global>,
}

/// A request's answer.
#[derive(Debug)]
pub struct Answered<'s> {
    pub outcome: Outcome,
    pub asks: Asks,
    /// The certificate's proof, when it held.
    pub certified: Option<Certified<'s>>,
    /// Each unit's manifest root: the certificate's list when it held, else what this request
    /// settled (`None` for a unit it did not reach). The next request's
    /// [`blaze_keyed::driver::UnitPlan::last`].
    pub manifests: Vec<Option<Key<UnitManifest>>, Global>,
    /// What each unit holds (empty when the certificate held: nothing below it was checked).
    pub units: Vec<UnitProved<'s>, Global>,
    /// Each program's kept manifest ([`run_programs`]): the certificate's list when it held,
    /// else what this request filed or kept (`None` for a program that failed). The next
    /// request's [`ProgramPlan::last`]. Empty for [`run`].
    pub programs: Vec<Option<Key<ProgramManifest>>, Global>,
    /// Every failure of a failed request, in the order of what failed (units, then programs):
    /// the outcome's `why` is the first. A failed task fails what depends on it; the rest went on.
    pub failures: Vec<Failure, Global>,
    /// The store's own error behind a [`StoreFault`] the driver was given, or of the commit at
    /// the end of a request that failed.
    pub store_error: Option<StoreError>,
    /// The files hashed, for the command's leaf memory.
    pub looked: Vec<Looked, Global>,
}

/// Why the host stopped a request: by name, never a string.
#[derive(Debug)]
pub enum HostError {
    Plan(PlanError),
    /// The request itself is not of its form: `what`.
    Request { what: &'static str },
    /// The driver waited for `waited_for` with nothing running and nothing ready, or a wait for
    /// a task passed the request's `stall`.
    Stalled { waited_for: &'static str, in_flight: u32 },
    Wait(WaitError),
    /// The store failed a read of `tag`, `key`.
    Get { tag: Tag, key: ContentId, error: StoreError },
    /// An effect of a kind the host does not perform.
    Kind { tag: Tag, what: &'static str },
    /// A record asked in another layout than its kind's.
    Layout { tag: Tag, asked: Layout },
    Outside(OutsideError),
    /// A fact asked of unit `unit`'s universe has no answer, by rule `why`.
    Answer { unit: u32, why: Unanswerable },
    /// A table of a universe names unit `unit`'s record of `tag` at `key`, which the store does
    /// not hold.
    Table { unit: u32, tag: Tag, key: ContentId },
    /// The record of `tag` at `key` does not open: `what`.
    Record { tag: Tag, key: ContentId, what: &'static str },
    /// A task asked to run remotely of a host with only the local executor.
    Remote { task: Task },
    /// The pool of `workers` threads could not be made.
    Pool { workers: u32, error: io::Error },
    Unstopped(Unstopped),
    /// The certificate held, but its value is not the manifests it names: `what`.
    Certificate { what: &'static str },
    /// The host broke its own protocol: `what`.
    Misused(&'static str),
    /// The request built and filed no certificate value (its slots are full, or an audit), and
    /// the commit of what it filed at its end failed.
    Commit(StoreError),
    /// The request stopped with `first`, and the commit of what it had filed failed too.
    Uncommitted { first: Box<HostError, Global>, commit: StoreError },
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostError::Plan(PlanError::DepOrder { unit, dep }) => write!(
                f,
                "unit {unit} of the plan depends on unit {dep}, not below it (rule: a plan lists units in dependency order)"
            ),
            HostError::Plan(PlanError::Owner { unit }) => write!(
                f,
                "unit {unit} of the plan has a reserved owner or another unit's (rule: each unit owns the facts its crate key begins)"
            ),
            HostError::Plan(PlanError::CertificateLookup { what }) => write!(
                f,
                "the certificate lookup handed to the request is wrong: {what} (rule: a lookup made beforehand stands in only for this request's own certificate key, finished)"
            ),
            HostError::Request { what } => write!(f, "the request is not of its form: {what}"),
            HostError::Stalled { waited_for, in_flight } => {
                write!(f, "the request stalled waiting for {waited_for}: {in_flight} tasks running (rule bounded-progress)")
            }
            HostError::Wait(error) => write!(f, "the wait for a task failed: {error}"),
            HostError::Get { tag, key, error } => write!(f, "the store's read of the record of kind {} key {key:?}: {error}", tag.0),
            HostError::Kind { tag, what } => write!(f, "kind {}: {what}", tag.0),
            HostError::Layout { tag, asked } => {
                write!(f, "the record of kind {} asked {asked:?}, which is not its kind's layout", tag.0)
            }
            HostError::Outside(error) => write!(f, "{error}"),
            HostError::Answer { unit, why } => {
                write!(f, "a fact asked as unit {unit} sees it has no answer: {why:?} ({})", why.failed().what())
            }
            HostError::Table { unit, tag, key } => write!(
                f,
                "the universe names unit {unit}'s record of kind {} at {key:?}, which the store does not hold (rule: a task reads only what is filed)",
                tag.0
            ),
            HostError::Record { tag, key, what } => write!(f, "the record of kind {} at {key:?}: {what}", tag.0),
            HostError::Remote { task } => write!(
                f,
                "{task:?} asked to run remotely, and this host runs only the local executor (rule no-fallback: never run here instead)"
            ),
            HostError::Pool { workers, error } => write!(f, "the pool of {workers} workers could not start: {error}"),
            HostError::Unstopped(unstopped) => write!(f, "the request's pool did not stop: {unstopped}"),
            HostError::Certificate { what } => write!(f, "the certificate held, and {what}"),
            HostError::Misused(what) => write!(f, "the host broke its protocol: {what}"),
            HostError::Commit(error) => write!(f, "the commit of the request's records at its end failed: {error}"),
            HostError::Uncommitted { first, commit } => {
                write!(f, "{first}; and the commit of what the request had filed failed: {commit}")
            }
        }
    }
}

impl std::error::Error for HostError {}

/// The driver the host runs: local tasks, allocating on the global heap (the request's frame is
/// the missing piece: the driver's vectors live for the request).
type Drv<'s, 'm, M, WK> = Driver<'s, 'm, M, Local, Global, WK>;

/// How a pump ended.
enum Pumped {
    Exit(Outcome),
    /// More than one task is ready and no pool is running: start one of this many workers.
    Workers(u32),
}

/// Where the bytes a [`Effect::Get`] is answered with are.
enum GotFrom<'a> {
    Nothing,
    Mapped(&'a [u8]),
    Written(Arc<Aligned, Global>),
    /// The host's buffer of the last flat read ([`Host::flat_bytes`]).
    Flat,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flat {
    None,
    Held,
    Aligned,
}

/// An effect performed, its answer not yet given: what the driver is given once its borrow of
/// the effect has ended.
enum Reply<'a> {
    Got { ticket: Ticket, got: GotFrom<'a> },
    /// The answers are in [`Host::answers`].
    Answers { ticket: Ticket },
    Reads { ticket: Ticket },
    /// A task queued: its answer comes when it finishes.
    Queued,
    Put { ticket: Ticket, put: Result<Put, StoreFault> },
    /// A link step ran: the linker's new state, or why it failed.
    Linked { ticket: Ticket, outcome: Result<Vec<u8, Global>, TaskFailed> },
    /// A program's image made and durable, or why not.
    Imaged { ticket: Ticket, outcome: Result<(), TaskFailed> },
    /// A program's path placed, or why not.
    Placed { ticket: Ticket, outcome: Result<(), TaskFailed> },
}

/// Where a program's images are kept and its path is placed: the IO of [`Effect::Image`] and
/// [`Effect::Place`], the command's to give.
///
/// The invariant: the path the user runs names the image of a kept manifest that is durable,
/// and after an answer the one it certified. An image is never written at the path: it is a
/// store file named by its kept manifest's key (O(1) to name, no hash of the image), made
/// before that manifest is filed, and the path is renamed onto it only after.
///
/// Floor: per request that links, one clone of the last image (a reflink or `clonefile`, O(1)
/// in its size where the filesystem has it) and the patch's bytes written, one sync; per answer
/// and program, one look at the path and a rename only when it names another image.
pub trait Images {
    /// Makes program `program`'s image `to` durable in the store: a clone of image `from` (none:
    /// an empty file) with `patch` applied, its writes then its length; with no patch, the clone
    /// alone (a kept manifest filed again with no link step). The host gives `from` with a patch
    /// only when the patch's base is [`PatchBase::Clone`]: for [`PatchBase::Empty`] it gives
    /// none, so the old image is never read under a whole relink. Where the filesystem clones
    /// nothing, a copy, which the implementation says it made. An image `to` already held is
    /// left as it is: two makers of one key make equal bytes. `from` not held is refused.
    fn make<A: Allocator>(&self, program: u32, from: Option<ContentId>, to: ContentId, patch: Option<&Patch<A>>) -> Result<(), TaskFailed>;

    /// Makes program `program`'s path name image `image`: one look, and when it names another
    /// (or nothing), a link of the image beside it renamed over it atomically, then the
    /// directory synced.
    fn place(&self, program: u32, image: ContentId) -> Result<(), TaskFailed>;
}

/// The [`Images`] of a request that builds no program ([`run`]): each effect is refused.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoImages;

impl Images for NoImages {
    fn make<A: Allocator>(&self, _: u32, _: Option<ContentId>, _: ContentId, _: Option<&Patch<A>>) -> Result<(), TaskFailed> {
        Err(TaskFailed::Refused { what: "a program's image asked made in a request that builds no program" })
    }

    fn place(&self, _: u32, _: ContentId) -> Result<(), TaskFailed> {
        Err(TaskFailed::Refused { what: "a program's path asked placed in a request that builds no program" })
    }
}

struct Host<'a, F, S: Store, W: Files, I: Images> {
    request: &'a Request<'a>,
    /// The programs the request builds ([`run_programs`]); empty for [`run`].
    programs: &'a [ProgramPlan],
    store: &'a S,
    front: &'a F,
    machine: Machine,
    answerer: Answerer<'a, 'a, W>,
    clock: Monotonic,
    /// The request's reads, encoded once: the shape its certificate is filed under.
    shape: Vec<u8, Global>,
    answers: Vec<ContentId, Global>,
    held: Option<S::Held<Global>>,
    aligned: Option<Aligned>,
    flat: Flat,
    /// Surfaces this request filed, by key, sorted.
    written: Vec<(ContentId, Arc<Aligned, Global>), Global>,
    /// Made tables read, by manifest root, sorted.
    made: Vec<(ContentId, Facts), Global>,
    runnable: VecDeque<(Ticket, Job<'a, F>), Global>,
    in_flight: u32,
    uncommitted: bool,
    store_error: Option<StoreError>,
    /// The patch each program's link step returned, by program, held until the driver asks the
    /// image made ([`Effect::Image`]): one link step per program per request.
    patches: Vec<(u32, Patch<Global>), Global>,
    images: &'a I,
}

impl<'a, F: Front, S: Store, W: Files, I: Images> Host<'a, F, S, W, I> {
    fn new(
        request: &'a Request<'a>,
        programs: &'a [ProgramPlan],
        store: &'a S,
        front: &'a F,
        files: &'a W,
        images: &'a I,
        machine: Machine,
    ) -> Self {
        let mut shape = Vec::new_in(Global);
        outside::encode(request.reads, &mut shape);
        Host {
            request,
            programs,
            store,
            front,
            machine,
            answerer: Answerer::new(files, request.leaves),
            clock: Monotonic::new(),
            shape,
            answers: Vec::new_in(Global),
            held: None,
            aligned: None,
            flat: Flat::None,
            written: Vec::new_in(Global),
            made: Vec::new_in(Global),
            runnable: VecDeque::new_in(Global),
            in_flight: 0,
            uncommitted: false,
            store_error: None,
            patches: Vec::new_in(Global),
            images,
        }
    }

    /// Runs the driver until it exits, or, with no pool, until two or more tasks are ready.
    fn pump<'scope, 'env, 's, M: Memo, WK: Walker>(
        &mut self,
        driver: &mut Drv<'s, '_, M, WK>,
        mut workers: Option<&mut Workers<'scope, 'env, F>>,
    ) -> Result<Pumped, HostError>
    where
        'a: 'env,
        F: 'env,
    {
        loop {
            if let Some(workers) = workers.as_deref() {
                while let Some((ticket, (_task, made))) = workers.finished() {
                    self.in_flight -= 1;
                    Self::ran(driver, ticket, made);
                }
            }
            let what = match driver.next() {
                Step::Exit(outcome) => return Ok(Pumped::Exit(outcome.clone())),
                Step::Do(effect) => {
                    let reply = self.perform(effect)?;
                    self.reply(driver, reply);
                    continue;
                }
                Step::Wait(what) => what,
            };
            match workers.as_deref_mut() {
                None => match self.runnable.len() {
                    0 => {}
                    1 => {
                        let Some((ticket, job)) = self.runnable.pop_front() else {
                            return Err(HostError::Misused("a ready task counted and not there"));
                        };
                        let made = guarded(&job);
                        Self::ran(driver, ticket, made);
                        continue;
                    }
                    queued => return Ok(Pumped::Workers(self.machine.workers_for(queued as u32).max(1))),
                },
                Some(workers) => {
                    while workers.free() > 0
                        && let Some((ticket, job)) = self.runnable.pop_front()
                    {
                        workers
                            .submit(ticket, job)
                            .map_err(|_full| HostError::Misused("a job submitted to a pool with no free worker"))?;
                        self.in_flight += 1;
                    }
                }
            }
            let stalled = HostError::Stalled { waited_for: what, in_flight: self.in_flight };
            let Some(workers) = workers.as_deref() else {
                return Err(stalled);
            };
            if self.in_flight == 0 {
                return Err(stalled);
            }
            if workers.any_finished() {
                continue;
            }
            let _span = tracing::info_span!(target: "blaze", "host: wait", waited_for = what, in_flight = self.in_flight).entered();
            let deadline = Deadline::after(self.clock.now(), self.request.stall);
            match workers.parker().park("host: a task to finish", Some(deadline), &self.clock).map_err(HostError::Wait)? {
                Waited::Ready(()) => {}
                Waited::Deadline => return Err(stalled),
            }
        }
    }

    /// Performs `effect` as far as needs no driver: what the driver is to be given.
    fn perform(&mut self, effect: Effect<'_>) -> Result<Reply<'a>, HostError> {
        match effect {
            Effect::Get { ticket, unit: _, tag, key, layout } => Ok(Reply::Got { ticket, got: self.get(tag, key, layout)? }),
            Effect::Outside { ticket, tag, key: _, shape } => {
                if tag != Certificate::TAG {
                    return Err(HostError::Kind { tag, what: "an outside kind whose asks this host does not answer" });
                }
                self.answerer.answer_shape(shape, &mut self.answers).map_err(HostError::Outside)?;
                Ok(Reply::Answers { ticket })
            }
            Effect::Answer { ticket, unit, asks, universe, made } => {
                self.answer(unit, asks, universe, made)?;
                Ok(Reply::Answers { ticket })
            }
            Effect::Reads { ticket } => {
                self.answers.clear();
                for ask in self.request.reads {
                    let answer = self.answerer.answer(*ask).map_err(HostError::Outside)?;
                    self.answers.push(answer);
                }
                Ok(Reply::Reads { ticket })
            }
            Effect::Run { ticket, task, remote, universe, made, bytes, plans, bodies } => {
                if remote {
                    return Err(HostError::Remote { task });
                }
                let sources = self.sources(universe)?;
                let mut own = Vec::with_capacity_in(made.len(), Global);
                own.extend_from_slice(made);
                // The task's own unit's late table, never another's (none for a program's).
                let late = self.late_of(unit_of(task))?;
                let mut copied = Vec::with_capacity_in(bytes.len(), Global);
                copied.extend_from_slice(bytes);
                // A plan or code task reads its bodies' records where the driver names them:
                // filed before it runs (this process reads its own puts at once).
                let mut given = Vec::with_capacity_in(bodies.len(), Global);
                for at in bodies {
                    let key = at.address.id();
                    let record = self.read_whole::<Body>(key, "a body record a plan or code task is given: the store does not hold it")?;
                    let result = blaze_format::open::<Traced>(&record)
                        .ok()
                        .and_then(|root| record::result_digest(root, &record).ok())
                        .ok_or(HostError::Record { tag: Body::TAG, key, what: "a body record a plan or code task is given does not open" })?;
                    given.push((at.item, record, result));
                }
                // A code task's callees' plans, each read where the driver names it.
                let mut callees = Vec::with_capacity_in(plans.len(), Global);
                for at in plans {
                    let what = "a callee's plan record a code task is given: the store does not hold it";
                    callees.push((at.instance, self.read_whole::<PlanRecord>(at.address, what)?));
                }
                let job = Job { front: self.front, task, sources, made: own, late, bytes: copied, bodies: given, plans: callees };
                self.runnable.push_back((ticket, job));
                Ok(Reply::Queued)
            }
            Effect::Put { ticket, tag, key, namespace, bytes } => Ok(Reply::Put { ticket, put: self.put(tag, key, namespace, bytes)? }),
            Effect::Link { ticket, program, changes, plans, spans, payload } => {
                let _span = tracing::info_span!(target: "blaze", "host: link", program, changes = changes.len(), spans = spans.len()).entered();
                // Each row's new code, read once; a row that went has none.
                let mut held = Vec::with_capacity_in(changes.len(), Global);
                for change in changes {
                    held.push(match change.now {
                        Some(code) => {
                            Some(self.read_whole::<CodeRecord>(code.id(), "the code record a link step is given: the store does not hold it")?)
                        }
                        None => None,
                    });
                }
                let mut code = Vec::with_capacity_in(held.len(), Global);
                code.extend(held.iter().map(|record: &Option<Aligned>| record.as_deref().unwrap_or(&[])));
                // The plans of the rows that came or moved and of their callees, read once.
                let mut read = Vec::with_capacity_in(plans.len(), Global);
                for at in plans {
                    let what = "a plan record a link step is given: the store does not hold it";
                    read.push((at.instance, self.read_whole::<PlanRecord>(at.address, what)?));
                }
                let mut given = Vec::with_capacity_in(read.len(), Global);
                given.extend(read.iter().map(|(instance, record)| (*instance, &record[..])));
                // The span tables that came or moved, each record read once, its payload (the
                // table) handed on.
                let mut tables = Vec::with_capacity_in(spans.len(), Global);
                for at in spans {
                    let what = "a span table a link step is given: the store does not hold it";
                    tables.push((at.anchor, at.address.id(), self.read_whole::<SpanTable>(at.address.id(), what)?));
                }
                let mut span_payloads = Vec::with_capacity_in(tables.len(), Global);
                for (anchor, key, record) in tables.iter() {
                    let payload = blaze_format::open::<Traced>(record).and_then(|root| root.payload.follow(record)).map_err(|_| HostError::Record {
                        tag: SpanTable::TAG,
                        key: *key,
                        what: "a span table a link step is given does not open",
                    })?;
                    span_payloads.push((*anchor, payload));
                }
                // On the request's thread: one step per program, after its code is quiet. A panic
                // fails this program's link step alone (rule 20).
                let front = self.front;
                let outcome = catch_unwind(AssertUnwindSafe(|| front.link(program, changes, &code, &given, &span_payloads, payload, Global)))
                    .unwrap_or(Err(TaskFailed::Panicked { what: "a link step" }));
                // The patch waits here for the driver's `Effect::Image`; the state goes to it.
                let outcome = outcome.map(|Linked { payload, patch }| {
                    match self.patches.binary_search_by_key(&program, |(held, _)| *held) {
                        Ok(at) => self.patches[at].1 = patch,
                        Err(at) => self.patches.insert(at, (program, patch)),
                    }
                    payload
                });
                Ok(Reply::Linked { ticket, outcome })
            }
            Effect::Image { ticket, program, from, to, patched } => {
                let _span = tracing::info_span!(target: "blaze", "host: make program image", program, patched).entered();
                let (images, from) = (self.images, from.map(|from| from.id()));
                let held = self.patches.binary_search_by_key(&program, |(held, _)| *held);
                // A panic in the command's images fails this program alone (rule 20).
                let outcome = match (patched, held) {
                    (true, Err(_)) => Err(TaskFailed::Refused { what: "a program image asked patched with no link step's patch in this request" }),
                    (true, Ok(at)) => {
                        let (_, patch) = self.patches.remove(at);
                        // The patch's base says what its writes start from: an empty file reads
                        // no old image; a clone needs one.
                        match (patch.base, from) {
                            (PatchBase::Clone, None) => {
                                Err(TaskFailed::Refused { what: "a link step's patch over a clone of the old image, with no old image" })
                            }
                            (base, from) => {
                                let from = from.filter(|_| base == PatchBase::Clone);
                                catch_unwind(AssertUnwindSafe(|| images.make(program, from, to.id(), Some(&patch))))
                                    .unwrap_or(Err(TaskFailed::Panicked { what: "a program image's making" }))
                            }
                        }
                    }
                    (false, _) => catch_unwind(AssertUnwindSafe(|| images.make::<Global>(program, from, to.id(), None)))
                        .unwrap_or(Err(TaskFailed::Panicked { what: "a program image's making" })),
                };
                Ok(Reply::Imaged { ticket, outcome })
            }
            Effect::Place { ticket, program, image } => {
                let _span = tracing::info_span!(target: "blaze", "host: place program path", program).entered();
                let images = self.images;
                let outcome = catch_unwind(AssertUnwindSafe(|| images.place(program, image.id())))
                    .unwrap_or(Err(TaskFailed::Panicked { what: "a program path's place" }));
                Ok(Reply::Placed { ticket, outcome })
            }
        }
    }

    /// The record of kind `K` at `key`, whole and 16-aligned, for a task to own: refused with
    /// `what` when the store does not hold it.
    fn read_whole<K: Kind>(&self, key: ContentId, what: &'static str) -> Result<Aligned, HostError> {
        let read = self
            .store
            .read::<K, Global>(&Key::from_id(key), Global)
            .map_err(|error| HostError::Get { tag: K::TAG, key, error })?
            .ok_or(HostError::Record { tag: K::TAG, key, what })?;
        Ok(Aligned::copy_from(&read.bytes))
    }

    /// Gives the driver the answer `reply` made.
    fn reply<M: Memo, WK: Walker>(&mut self, driver: &mut Drv<'_, '_, M, WK>, reply: Reply<'a>) {
        match reply {
            Reply::Got { ticket, got } => {
                let record: Option<&[u8]> = match &got {
                    GotFrom::Nothing => None,
                    GotFrom::Mapped(bytes) => Some(*bytes),
                    GotFrom::Written(record) => Some(&record[..]),
                    GotFrom::Flat => self.flat_bytes(),
                };
                driver.give(Input::Got { ticket, record });
            }
            Reply::Answers { ticket } => driver.give(Input::Answers { ticket, now: &self.answers }),
            Reply::Reads { ticket } => {
                driver.give(Input::Reads { ticket, shape: &self.shape, then: self.request.then, now: &self.answers });
            }
            Reply::Queued => {}
            Reply::Put { ticket, put } => driver.give(Input::Put { ticket, put }),
            Reply::Linked { ticket, outcome } => {
                driver.give(Input::Linked { ticket, outcome: outcome.as_deref().map_err(|failed| *failed) });
            }
            Reply::Imaged { ticket, outcome } => driver.give(Input::Imaged { ticket, outcome }),
            Reply::Placed { ticket, outcome } => driver.give(Input::Placed { ticket, outcome }),
        }
    }

    /// Gives the driver what a task made.
    fn ran<M: Memo, WK: Walker>(driver: &mut Drv<'_, '_, M, WK>, ticket: Ticket, made: Result<Made, TaskFailed>) {
        // A span task's tables, borrowed as the driver takes them.
        let mut tables = Vec::new_in(Global);
        if let Ok(Made::Spans(spans)) = &made {
            tables.extend(spans.tables.iter().map(|(anchor, bytes)| (*anchor, &bytes[..])));
        }
        let outcome = match &made {
            Ok(Made::Keyed(keyed)) => Ok(Done::Keyed {
                manifest: keyed.manifest,
                surface: keyed.surface,
                files: &keyed.files[..],
                items: &keyed.items[..],
                names: &keyed.names[..],
            }),
            Ok(Made::Wrote(record)) => Ok(Done::Wrote { record: &record[..] }),
            Ok(Made::Spans(spans)) => Ok(Done::Spans { reads: &spans.reads[..], tables: &tables[..] }),
            Err(failed) => Err(*failed),
        };
        driver.give(Input::Ran { ticket, outcome });
    }

    /// The answer now of each of `asks` as unit `unit` sees it, into [`Host::answers`].
    fn answer(&mut self, unit: u32, asks: &[ContentId], refs: &[TableRef], made: &[Fact]) -> Result<(), HostError> {
        let _span = tracing::info_span!(target: "blaze", "host: answer", unit, asks = asks.len()).entered();
        let sources = self.sources(refs)?;
        let tables = tables(&sources, made).map_err(|why| HostError::Answer { unit, why })?;
        let universe = Universe { asker: unit, tables: &tables, outside: self.late_of(unit)? };
        self.answers.clear();
        for id in asks {
            self.answers.push(universe.answer(id).map_err(|why| HostError::Answer { unit, why })?);
        }
        Ok(())
    }

    /// Unit `unit`'s own late table ([`Request::late`]): `run` checked there is one per unit, so
    /// a unit past them is the driver's misuse, never an empty table.
    fn late_of(&self, unit: u32) -> Result<&'a [Fact], HostError> {
        // A program's asks and tasks are of no unit: a plan may read no late fact, so none.
        if unit == NO_UNIT {
            return Ok(&[]);
        }
        self.request.late.get(unit as usize).copied().ok_or(HostError::Misused("a unit with no late table of its own"))
    }

    /// The tables `refs` name: a table with no surface yet (the asking unit's own, before its
    /// surface settles) is not in the universe.
    fn sources(&mut self, refs: &[TableRef]) -> Result<Vec<Source<'a>, Global>, HostError> {
        let mut sources = Vec::with_capacity_in(refs.len(), Global);
        for table in refs {
            let Some(surface) = table.surface else { continue };
            let bytes = self.surface_bytes(table.unit, surface)?;
            let made = match table.manifest {
                None => None,
                Some(manifest) => Some(self.made_of(table.unit, manifest)?),
            };
            sources.push(Source { unit: table.unit, owner: table.owner, surface: bytes, made });
        }
        Ok(sources)
    }

    /// Unit `unit`'s surface record `key`, in place or as this request wrote it.
    fn surface_bytes(&mut self, unit: u32, key: Key<Surface>) -> Result<Bytes<'a>, HostError> {
        if let Some(record) = self.written_of(key.id()) {
            return Ok(Bytes::Written(record));
        }
        let store = self.store;
        match store.map::<Surface>(&key) {
            Ok(Some(read)) => Ok(Bytes::Mapped(read.bytes)),
            Ok(None) => Err(HostError::Table { unit, tag: Surface::TAG, key: key.id() }),
            Err(error) => Err(HostError::Get { tag: Surface::TAG, key: key.id(), error }),
        }
    }

    /// The facts unit `unit`'s bodies made, as its manifest root `manifest` lists them: read once
    /// per root for the request.
    fn made_of(&mut self, unit: u32, manifest: Key<UnitManifest>) -> Result<Facts, HostError> {
        let id = manifest.id();
        let at = match self.made.binary_search_by(|(root, _)| root.cmp(&id)) {
            Ok(at) => return Ok(Arc::clone(&self.made[at].1)),
            Err(at) => at,
        };
        let tag = UnitManifest::TAG;
        let store = self.store;
        let read = store
            .read::<UnitManifest, Global>(&manifest, Global)
            .map_err(|error| HostError::Get { tag, key: id, error })?
            .ok_or(HostError::Table { unit, tag, key: id })?;
        let copy;
        let bytes: &[u8] = match read.bytes.as_ptr().addr() % blaze_format::ALIGN == 0 {
            true => &*read.bytes,
            false => {
                copy = Aligned::copy_from(&read.bytes);
                &copy[..]
            }
        };
        let root = record::open::<UnitManifest>(bytes, &manifest)
            .map_err(|_why| HostError::Record { tag, key: id, what: "a manifest root that does not open under its key" })?;
        let facts = root
            .made
            .follow(bytes)
            .map_err(|_why| HostError::Record { tag, key: id, what: "a manifest root whose made table is out of its bounds" })?;
        let mut made = Vec::with_capacity_in(facts.len(), Global);
        made.extend_from_slice(facts);
        let made = Arc::new_in(made, Global);
        self.made.insert(at, (id, Arc::clone(&made)));
        Ok(made)
    }

    fn written_of(&self, key: ContentId) -> Option<Arc<Aligned, Global>> {
        let at = self.written.binary_search_by(|(filed, _)| filed.cmp(&key)).ok()?;
        Some(Arc::clone(&self.written[at].1))
    }

    fn flat_bytes(&self) -> Option<&[u8]> {
        match self.flat {
            Flat::None => None,
            Flat::Held => self.held.as_deref(),
            Flat::Aligned => self.aligned.as_deref(),
        }
    }

    fn get(&mut self, tag: Tag, key: ContentId, layout: Layout) -> Result<GotFrom<'a>, HostError> {
        match tag {
            t if t == Surface::TAG => match self.written_of(key) {
                Some(record) => Ok(GotFrom::Written(record)),
                None => self.fetch::<Surface>(key, layout),
            },
            t if t == Body::TAG => self.fetch::<Body>(key, layout),
            t if t == Shape::TAG => self.fetch::<Shape>(key, layout),
            t if t == Mark::TAG => self.fetch::<Mark>(key, layout),
            t if t == UnitChunk::TAG => self.fetch::<UnitChunk>(key, layout),
            t if t == UnitManifest::TAG => self.fetch::<UnitManifest>(key, layout),
            t if t == Certificate::TAG => self.fetch::<Certificate>(key, layout),
            t if t == PlanRecord::TAG => self.fetch::<PlanRecord>(key, layout),
            t if t == CodeRecord::TAG => self.fetch::<CodeRecord>(key, layout),
            t if t == ProgramManifest::TAG => self.fetch::<ProgramManifest>(key, layout),
            t if t == SpanTable::TAG => self.fetch::<SpanTable>(key, layout),
            other => Err(HostError::Kind { tag: other, what: "a read of a kind the driver asks none of" }),
        }
    }

    fn fetch<K: Kind>(&mut self, key: ContentId, layout: Layout) -> Result<GotFrom<'a>, HostError> {
        if layout != K::LAYOUT {
            return Err(HostError::Layout { tag: K::TAG, asked: layout });
        }
        let store = self.store;
        let typed = Key::<K>::from_id(key);
        let failed = |error| HostError::Get { tag: K::TAG, key, error };
        match layout {
            Layout::Plain => match store.map::<K>(&typed).map_err(failed)? {
                Some(read) => Ok(GotFrom::Mapped(read.bytes)),
                None => Ok(GotFrom::Nothing),
            },
            Layout::Flat => {
                self.flat = Flat::None;
                let Some(read) = store.read::<K, Global>(&typed, Global).map_err(failed)? else {
                    return Ok(GotFrom::Nothing);
                };
                let bytes = read.bytes;
                // The store's buffer is 16-aligned by its contract; a copy only where it is not.
                if bytes.as_ptr().addr() % blaze_format::ALIGN == 0 {
                    self.held = Some(bytes);
                    self.flat = Flat::Held;
                } else {
                    self.aligned = Some(Aligned::copy_from(&bytes));
                    self.flat = Flat::Aligned;
                }
                Ok(GotFrom::Flat)
            }
        }
    }

    fn keep_error(&mut self, error: StoreError) {
        if self.store_error.is_none() {
            self.store_error = Some(error);
        }
    }

    /// Whether `key` is one of the certificate's shape slots (at most [`MAX_SHAPES`] hashes):
    /// else a certificate put is its value, the request's last put.
    fn is_shape_slot(&self, key: ContentId) -> bool {
        // The key the driver files under: the plan's base with which units' bodies are asked,
        // and which programs are built.
        let certificate = certificate_with(&self.request.plan, self.programs);
        (0..MAX_SHAPES).any(|slot| shape_slot(certificate, slot).id() == key)
    }

    fn put(&mut self, tag: Tag, key: ContentId, namespace: Namespace, bytes: &[u8]) -> Result<Result<Put, StoreFault>, HostError> {
        let store = self.store;
        let put = match tag {
            t if t == Surface::TAG => store.put::<Surface>(&Key::from_id(key), namespace, bytes),
            t if t == Body::TAG => store.put::<Body>(&Key::from_id(key), namespace, bytes),
            t if t == Shape::TAG => store.put::<Shape>(&Key::from_id(key), namespace, bytes),
            t if t == Mark::TAG => store.put::<Mark>(&Key::from_id(key), namespace, bytes),
            t if t == UnitChunk::TAG => store.put::<UnitChunk>(&Key::from_id(key), namespace, bytes),
            t if t == UnitManifest::TAG => store.put::<UnitManifest>(&Key::from_id(key), namespace, bytes),
            t if t == Certificate::TAG => store.put::<Certificate>(&Key::from_id(key), namespace, bytes),
            t if t == PlanRecord::TAG => store.put::<PlanRecord>(&Key::from_id(key), namespace, bytes),
            t if t == CodeRecord::TAG => store.put::<CodeRecord>(&Key::from_id(key), namespace, bytes),
            t if t == ProgramManifest::TAG => store.put::<ProgramManifest>(&Key::from_id(key), namespace, bytes),
            t if t == SpanTable::TAG => store.put::<SpanTable>(&Key::from_id(key), namespace, bytes),
            other => return Err(HostError::Kind { tag: other, what: "a put of a kind the driver files none of" }),
        };
        let put = match put {
            Ok(put) => put,
            Err(error) => {
                self.keep_error(error);
                return Ok(Err(StoreFault { what: "a put of a record" }));
            }
        };
        if put == Put::Other {
            return Ok(Ok(put));
        }
        if put == Put::Written {
            self.uncommitted = true;
        }
        if tag == Surface::TAG
            && let Err(at) = self.written.binary_search_by(|(filed, _)| filed.cmp(&key))
        {
            self.written.insert(at, (key, Arc::new_in(Aligned::copy_from(bytes), Global)));
        }
        if tag == Certificate::TAG && self.uncommitted && !self.is_shape_slot(key) {
            // The one commit of the request, before its last put is answered: a failed sync
            // fails the request by name (no retry).
            self.uncommitted = false;
            let _span = tracing::info_span!(target: "blaze", "host: commit").entered();
            if let Err(error) = store.commit() {
                self.keep_error(error);
                return Ok(Err(StoreFault { what: "the commit of the request's records" }));
            }
        }
        Ok(Ok(put))
    }

    /// Commits what a request that ended before its certificate's value had filed.
    fn commit_left(&mut self) -> Result<(), StoreError> {
        if !self.uncommitted {
            return Ok(());
        }
        self.uncommitted = false;
        let _span = tracing::info_span!(target: "blaze", "host: commit").entered();
        self.store.commit().map(|_generation| ())
    }

    fn finish<'s, M: Memo, WK: Walker>(mut self, driver: &Drv<'s, '_, M, WK>, outcome: Outcome) -> Result<Answered<'s>, HostError> {
        if let Err(error) = self.commit_left() {
            match outcome {
                Outcome::Failed { .. } => self.keep_error(error),
                _ => return Err(HostError::Commit(error)),
            }
        }
        let count = self.request.plan.units.len();
        let mut units = Vec::with_capacity_in(count, Global);
        for unit in 0..count as u32 {
            let mut records = Vec::new_in(Global);
            records.extend(driver.proofs(unit));
            units.push(UnitProved { manifest: driver.manifest(unit), records });
        }
        let programs = self.programs.len();
        let (manifests, kept) = match outcome {
            Outcome::Certified { .. } => certified_manifests(driver.certified_value(), count, programs)?,
            _ => {
                let mut manifests = Vec::with_capacity_in(count, Global);
                manifests.extend(units.iter().map(|unit: &UnitProved<'s>| unit.manifest.map(|holds| holds.key())));
                let mut kept = Vec::with_capacity_in(programs, Global);
                kept.extend((0..programs as u32).map(|program| driver.program_manifest(program)));
                (manifests, kept)
            }
        };
        let mut failures = Vec::new_in(Global);
        failures.extend(driver.failures().cloned());
        Ok(Answered {
            outcome,
            asks: driver.asks(),
            certified: driver.certified(),
            manifests,
            units,
            programs: kept,
            failures,
            store_error: self.store_error,
            looked: self.answerer.looked(),
        })
    }
}

/// What a certificate's value names: each unit's manifest root, then each program's kept
/// manifest.
type Named = (Vec<Option<Key<UnitManifest>>, Global>, Vec<Option<Key<ProgramManifest>>, Global>);

/// The unit manifest roots and then the kept program manifests the certificate's value names,
/// 32 bytes each in plan order.
fn certified_manifests(value: Option<&[u8]>, count: usize, programs: usize) -> Result<Named, HostError> {
    let bytes = value.ok_or(HostError::Certificate { what: "the driver holds no value of it" })?;
    let root =
        blaze_format::open::<Traced>(bytes).map_err(|_why| HostError::Certificate { what: "its value does not open as a traced record" })?;
    let payload = root
        .payload
        .follow(bytes)
        .map_err(|_why| HostError::Certificate { what: "its value's payload is out of its bounds" })?;
    if payload.len() != (count + programs) * 32 {
        return Err(HostError::Certificate { what: "its value names another number of manifests than the plan has units and programs" });
    }
    let id = |chunk: &[u8]| {
        let mut id = [0u8; 32];
        id.copy_from_slice(chunk);
        ContentId(id)
    };
    let (units, kept) = payload.split_at(count * 32);
    let mut manifests = Vec::with_capacity_in(count, Global);
    manifests.extend(units.chunks_exact(32).map(|chunk| Some(Key::from_id(id(chunk)))));
    let mut programs = Vec::with_capacity_in(programs, Global);
    programs.extend(kept.chunks_exact(32).map(|chunk| Some(Key::from_id(id(chunk)))));
    Ok((manifests, programs))
}

/// Runs one request to its outcome in snapshot `snap`: the certificate checked, else every
/// unit keyed and settled by its mark, or its records found by what they read or made, and
/// filed, its marks and certificate last; with the records proved per unit.
///
/// `certificate_lookup`: the certificate's lookup the caller made in this snapshot before it
/// gathered anything (`None`: the driver looks it up itself). It must be finished and of this
/// request's certificate key, checked once ([`HostError::Plan`] with
/// `PlanError::CertificateLookup` else): the request then reads nothing of the certificate
/// again, so a no-op is the sweep and one lookup.
///
/// Floors: as the module's. Threads: none unless two or more tasks are ready at once; then one
/// pool of [`Machine::workers_for`] threads, for the rest of the request, stopped before return.
#[expect(clippy::too_many_arguments, reason = "the request's parts, each its own borrow; a struct would only rename them")]
pub fn run<'s, F: Front, S: Store, M: Memo, W: Files>(
    snap: Snapshot<'s>,
    request: &Request<'_>,
    certificate_lookup: Option<CertificateLookup<'s>>,
    store: &S,
    front: &F,
    memo: &mut M,
    files: &W,
    machine: Machine,
) -> Result<Answered<'s>, HostError> {
    run_programs(snap, request, certificate_lookup, &[], NoWalk, store, front, memo, files, &NoImages, machine)
}

/// [`run`], and then each of `programs` built once every unit is complete: its plans and code
/// found or made, walked by `walker` (blaze-program's walk), and one link step given the rows
/// that came, went or moved ([`Front::plan`], [`Front::code`], [`Front::link`]). The certificate
/// names each program's kept manifest after the units' ([`Answered::programs`]). Each program's
/// image is made in `images` before its kept manifest is filed, and its path placed after the
/// certificate, on every answer, a certified one too.
///
/// Floors: as [`run`]'s, and per program: a request whose units all settle by their marks reads
/// its kept manifest and answers its dictionary once, and looks at its path once; a one-body edit
/// costs that body, the plans that read it, their code, one link step, one image clone with the
/// patch's bytes written and one rename. Past the floor: a plan task's body record and a link
/// step's code records are copied once each from the store.
#[expect(clippy::too_many_arguments, reason = "the request's parts, each its own borrow; a struct would only rename them")]
pub fn run_programs<'s, F: Front, S: Store, M: Memo, W: Files, I: Images, WK: Walker>(
    snap: Snapshot<'s>,
    request: &Request<'_>,
    certificate_lookup: Option<CertificateLookup<'s>>,
    programs: &[ProgramPlan],
    walker: WK,
    store: &S,
    front: &F,
    memo: &mut M,
    files: &W,
    images: &I,
    machine: Machine,
) -> Result<Answered<'s>, HostError> {
    if request.then.len() != request.reads.len() {
        return Err(HostError::Request { what: "`then` gives another number of answers than `reads` has asks" });
    }
    if request.late.len() != request.plan.units.len() {
        return Err(HostError::Request { what: "`late` gives another number of tables than the plan has units" });
    }
    if request.late.iter().any(|table| record::sorted(table).is_err()) {
        return Err(HostError::Request { what: "a unit's `late` table is not sorted by id, each once" });
    }
    // The pool is the bound on tasks at once: the driver asks every ready task, and the host
    // queues what the pool has no worker for.
    let mut driver =
        Driver::build(snap, memo, Local { workers: u32::MAX }, request.plan, programs, walker, certificate_lookup, Global).map_err(HostError::Plan)?;
    let mut host = Host::new(request, programs, store, front, files, images, machine);
    let pumped = match host.pump(&mut driver, None) {
        Ok(Pumped::Exit(outcome)) => Ok(outcome),
        Ok(Pumped::Workers(threads)) => pooled(&mut host, &mut driver, threads),
        Err(error) => Err(error),
    };
    match pumped {
        Ok(outcome) => host.finish(&driver, outcome),
        Err(first) => match host.commit_left() {
            Ok(()) => Err(first),
            Err(commit) => Err(HostError::Uncommitted { first: Box::new_in(first, Global), commit }),
        },
    }
}

/// The rest of a request on a pool of `threads` workers, stopped before return.
fn pooled<'a, 's, F: Front, S: Store, M: Memo, W: Files, I: Images, WK: Walker>(
    host: &mut Host<'a, F, S, W, I>,
    driver: &mut Drv<'s, '_, M, WK>,
    threads: u32,
) -> Result<Outcome, HostError> {
    let pool = GraphPool::new(&Topology::flat(threads as usize), false).map_err(|error| HostError::Pool { workers: threads, error })?;
    let finish = Finish::new();
    let pumped = pool.scope(|scope| {
        let mut workers = Workers::new(scope, &finish, threads);
        host.pump(driver, Some(&mut workers))
    });
    let stopped = {
        let _span = tracing::info_span!(target: "blaze", "host: stop the pool", threads).entered();
        pool.stop(Duration::from_secs(10))
    };
    let pumped = pumped?;
    stopped.map_err(HostError::Unstopped)?;
    match pumped {
        Pumped::Exit(outcome) => Ok(outcome),
        Pumped::Workers(_) => Err(HostError::Misused("a pump with a pool asked for one")),
    }
}
