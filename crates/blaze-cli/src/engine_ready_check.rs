//! The `check` edit path's kinds on `Ready` (blaze-engine ready.rs, shell.rs), D187: the drive
//! of `check --driver=ready` (engine_check.rs `ready_drive` is its coordinator). Its seams: the
//! read seam `blaze_engine::CellDigests` (engine.rs; `View` and [`HostCells`] answer it), the
//! host's unrecorded `Recorded::peek`/`row` and its `placed` (kinds.rs), `BodyRunner::run_body`
//! (engine_bodies.rs: the `Sync` part of a setup a run reads, the phase path's `Shared` but for
//! today's digests) and `Prepared::join_ready` (THE ONE JOIN both drivers call),
//! `LintsStage::lint_units`/`keep` (engine_lints.rs) and `engine_answer::save_changes` (the ready
//! commit: pages alone, the header index left `Headers::Absent`, no shard read or written).
//!
//! Here: the three kinds a one-function edit enters (`FileItems`, `Lint`, `BodyCheck`; the
//! precondition engine_ready_edit.rs `admit` refuses any other), the request-wide [`Sink`] (each
//! file's one lint walk parked before the drive, the body rows the bodies' setup decided at the
//! plan, each body's lines and findings, each kind's typed error), the pool ([`Workers`]), the
//! drive itself ([`drive_edit`]) and the drive's stop as the command's error ([`stopped`]). The
//! commit's planted bug is engine_ready_edit.rs `CommitPlant`.
//!
//! What a kind holds is `Sync` by construction: the sink behind its one lock holds only `Send`
//! parts (a kind's failure as [`KindError`], owned and `Copy`, never the command's `CliError`),
//! and the `BodyCheck` kind holds the setup's [`BodyRunner`], never the bodies' stage, whose
//! frame-held findings are the coordinator's alone.
//!
//! What still scales with the checkout on this path (engine_check.rs `checking` runs it before
//! the drive, as the phase path does): the layout, `discover`'s read back, the whole `plan`
//! (every file's outline read back or made) and the bodies' setup (`BodiesStage::prepare`:
//! every member's group from every outline). The floor needs those per member at need, from
//! records a body edit does not move: a member's setup keyed by its files' entry digests, its
//! files' heads from their stored `FileItems` rows' cells, the uncovered note a member's own, and
//! the names world's inputs from the `CrateScope` rows' pages.

use core::convert::Infallible;

use blaze_drive::key::{Key, Name};
use blaze_drive::{Kind, Locate};
// `Ran` here is the engine's (kinds.rs); engine_bodies.rs `Ran` is a body's row.
use blaze_engine::kinds::{Ask, Defines, Failure, KindFacts, Out, Outcome, Owed, ROW_CELL, Ran, Recorded, Refusal, TaskKind};
use blaze_engine::{CellDigests, CellId, Done, Planned, RowFacts, SVec, Stage, Stands, Task};
use blaze_ids::ContentId;
use blaze_own::SharedAlloc;
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};

use crate::CliError;
use crate::engine_bodies::{BodyRun, BodyRunner};
use crate::engine_check::EngineCheckError;
use crate::engine_lints::{LintsStage, UnitKept};
use crate::engine_tasks::Outlined;

/// How a run's result stands beside its kind's: `owed` when the stage said the result is right
/// for this request only (engine_bodies.rs `Buffer::owed`, a stopped slice), else `stands`.
#[derive(Clone, Copy)]
enum Standing {
    As(Stands),
    Owed,
}

/// The one conversion from a stage's `Done` to the engine's result: its read list placed with
/// the host (`Recorded::placed`: each read final for the reader and at today's digest, K1, K2),
/// then a refused read ending the run in `Need`, then its defines checked sorted and once (K3).
/// Its findings count goes on `out`, the one place the engine's row takes it from (a row's
/// count is the phase path's `Done::findings`: one number, both drivers).
fn ran_of<'a>(src: &Recorded<'_, 'a>, done: Done<'a>, standing: Standing, out: &mut Out<'a>) -> Ran<'a> {
    if let Err(refusal) = src.placed(&done.reads) {
        return Ran::Failed(refusal);
    }
    if let Some(needs) = src.need() {
        return Ran::Need(needs);
    }
    out.set_findings(done.findings);
    let reads = match src.reads() {
        Ok(reads) => reads,
        Err(refusal) => return Ran::Failed(refusal),
    };
    match (done.failed, standing) {
        // A stopped run is no refusal of its reads: the machine stopped it (the stage's frame,
        // its deadline), so it is entered again by the next request.
        (true, Standing::Owed) => Ran::Done(Outcome::failed(done.out, Failure::Owed(Owed::Machine), reads)),
        // A kind that stands by its key fails by its key (a refusal of its own text).
        (true, Standing::As(Stands::ByKey)) => Ran::Done(Outcome::failed(done.out, Failure::ByKey, reads)),
        (true, Standing::As(_)) => Ran::Done(Outcome::failed(done.out, Failure::ByReads, reads)),
        (false, standing) => match Defines::sorted(done.name, done.defines) {
            Ok(defines) => {
                let stands = match standing {
                    Standing::As(stands) => stands,
                    Standing::Owed => Stands::OwedNextRequest,
                };
                Ran::Done(Outcome::made(done.out, defines, stands, reads))
            }
            Err(refusal) => Ran::Failed(refusal),
        },
    }
}

/// The task's file's place in the planner's outlines and its outline.
fn outline_of<'o, 'h>(outlined: &'o [Outlined<'h>], file: Name) -> Option<(usize, &'o Outlined<'h>)> {
    outlined.binary_search_by(|one| one.name.cmp(&file)).ok().map(|at| (at, &outlined[at]))
}

/// The source of the unit `name` as its file's outline lists it.
fn unit_of(one: &Outlined<'_>, name: Name) -> Option<ContentId> {
    one.made.as_ref().ok().and_then(|record| {
        record.parts.iter().find_map(|part| match (part.name == name, part.locate) {
            (true, Locate::Source(source)) => Some(source),
            _ => None,
        })
    })
}

/// One lint unit's row from its file's walk, parked in the [`Sink`] until its task runs.
pub(crate) struct Parked<'a> {
    name: Name,
    /// The key the walk made its row under: the task the engine runs must carry it (rule 21).
    key: Key,
    done: Option<Done<'a>>,
    kept: UnitKept,
    /// Its task ran and took `done`: only these are the stage's after the drive.
    ran: bool,
}

/// What the kinds' runs hand the coordinator beside their rows, behind one named lock (rule
/// 12): made before the drive, read whole after it by [`Sink::into_sunk`], and DROPPED AT THE
/// REQUEST'S END. It is a memo within one request, nothing of it is kept or read by the next
/// (rule 70); a row is the engine's alone, so the next request reuses rows, never this.
pub(crate) struct Sink<'a, 'f> {
    sunk: blaze_core::lock::Lock<Sunk<'a, 'f>>,
    /// The `BodyCheck` rows the bodies' setup decided before any check (engine_bodies.rs
    /// `prepare`: a refusal at the plan, a body no unit holds, a file no member holds), read only:
    /// a run of such a task answers this row, its read list read again through the host (a `Need`
    /// leaves it here for the run after).
    decided: blaze_engine::Buffer<'a>,
}

/// The sink's contents, taken whole after the drive.
pub(crate) struct Sunk<'a, 'f> {
    /// Each entered lint unit's row from its file's one walk ([`walk_lints`]), sorted by name.
    lints: SVec<'a, Parked<'a>>,
    /// Each body run's lines, findings and frozen-head counts, its row given to the engine.
    pub(crate) bodies: SVec<'a, BodyRun<'a, 'f>>,
    /// Each run's typed error, by its task (rule 19): the engine is told [`Refusal::Host`].
    failed: SVec<'a, (Name, KindError)>,
}

/// What a kind's run could not do (rule 19), as a worker hands it to the coordinator: owned,
/// `Copy` and `Send` (a run is on any thread, and the command's `CliError` is not `Send`), made
/// the command's `EngineCheckError` when the drive stops ([`stopped`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum KindError {
    /// The ready path's own inconsistency: `EngineCheckError::ReadyHost`.
    Host { task: Name, what: &'static str },
    /// A body its setup did not leave to check: `EngineCheckError::Unplaced`.
    NotInSetup(crate::engine_bodies::NotInSetup),
}

impl From<KindError> for EngineCheckError {
    fn from(error: KindError) -> EngineCheckError {
        match error {
            KindError::Host { task, what } => EngineCheckError::ReadyHost { task, what },
            KindError::NotInSetup(error) => error.into(),
        }
    }
}

impl<'a, 'f> Sink<'a, 'f> {
    /// A request's sink over the lint rows `walk_lints` parked and the body rows `prepare` decided.
    pub(crate) fn new(lints: SVec<'a, Parked<'a>>, decided: blaze_engine::Buffer<'a>, alloc: SharedAlloc<'a>) -> Sink<'a, 'f> {
        Sink { sunk: blaze_core::lock::Lock::new("engine.ready.sink", Sunk { lints, bodies: SVec::new_in(alloc), failed: SVec::new_in(alloc) }), decided }
    }

    /// `task`'s row as the bodies' setup decided it, copied into `out`'s allocator with each read
    /// digest read again through the host: `None` when the setup left the body to check. The
    /// setup read its digests before the drive, through the engine's view of the last answer; the
    /// host holds each placed read to today's digest (K1), so the list is the setup's, its digests
    /// today's (a definer not final yet ends the run in `Need`, `ran_of`). How it stands is the
    /// phase merge's: owed when the setup listed it owed, else on its reads.
    fn decided(&self, src: &Recorded<'_, 'a>, task: &Task, alloc: SharedAlloc<'a>) -> Option<(Done<'a>, Standing)> {
        let name = task.planned.name;
        let row = self.decided.done.iter().find(|done| done.name == name)?;
        let mut defines = SVec::with_capacity_in(row.defines.len(), alloc);
        defines.extend_from_slice(&row.defines);
        let mut reads = SVec::with_capacity_in(row.reads.len(), alloc);
        reads.extend(row.reads.iter().map(|(id, _)| (*id, src.peek(id).unwrap_or(blaze_drive::key::ABSENT))));
        let done = Done { name, key: row.key, out: row.out, defines, reads, findings: row.findings, failed: row.failed };
        let standing = match self.decided.owed.contains(&name) {
            true => Standing::Owed,
            false => Standing::As(Stands::ByReads),
        };
        Some((done, standing))
    }

    /// `error` kept for `name`, and the refusal that stops the drive naming it.
    fn fail(&self, name: Name, error: KindError) -> Ran<'a> {
        self.sunk.lock().failed.push((name, error));
        Ran::Failed(Refusal::Host { name })
    }

    /// The row the task's file's walk parked for it, taken. A lint run reads nothing (it stands
    /// by its key), so it never ends in `Need` and is taken once.
    fn take_lint(&self, task: &Task) -> Result<Done<'a>, KindError> {
        let name = task.planned.name;
        let mut sunk = self.sunk.lock();
        let at = sunk.lints.binary_search_by_key(&name, |one| one.name).map_err(|_| KindError::Host {
            task: name,
            what: "a `Lint` task its file's walk did not park: the forward wake entered a unit the precondition did not name (engine_ready_check.rs `walk_lints`)",
        })?;
        let parked = &mut sunk.lints[at];
        if parked.key != task.key {
            return Err(KindError::Host { task: name, what: "its lint row was walked under another key than the task the engine runs" });
        }
        let done = parked.done.take().ok_or(KindError::Host { task: name, what: "its lint row was taken twice in one request" })?;
        parked.ran = true;
        Ok(done)
    }

    pub(crate) fn into_sunk(self) -> Sunk<'a, 'f> {
        self.sunk.into_inner()
    }
}

impl<'a, 'f> Sunk<'a, 'f> {
    /// The typed error a run kept for `name`, taken.
    pub(crate) fn failure(&mut self, name: Name) -> Option<KindError> {
        let at = self.failed.iter().position(|(one, _)| *one == name)?;
        Some(self.failed.swap_remove(at).1)
    }

    /// The lint units whose tasks ran, into the stage (`LintsStage::keep`), in name order; a
    /// unit parked but never entered is dropped with the sink.
    pub(crate) fn keep_lints(&mut self, lints: &mut LintsStage) {
        for parked in self.lints.drain(..) {
            if parked.ran {
                lints.keep(parked.kept);
            }
        }
    }
}

/// The drive's stop as the command's error: a kind's [`Refusal::Host`] is the typed error that
/// kind kept (rule 19), any other the driver's own, copied into the command's heap.
pub(crate) fn stopped(error: blaze_engine::ready::Error<'_>, sunk: &mut Sunk<'_, '_>, heap: SharedAlloc<'static>) -> CliError {
    if let blaze_engine::ready::Error::Refused { name, refusal: Refusal::Host { .. } } = &error
        && let Some(kept) = sunk.failure(*name)
    {
        return EngineCheckError::from(kept).into();
    }
    EngineCheckError::Ready(error.detach_in(heap)).into()
}

/// Each edited file's entered lint units in ONE walk a file, before the drive (D187: lint woken
/// by file). A unit's scope comes from the units around it, so a walk a unit would read its file
/// once a unit; here each file's units are walked once, the files in parallel, and each row
/// parked by task name in the request's [`Sink`]. `tasks` are the entering `Lint` tasks, from
/// the request's one plan (engine_ready_edit.rs `file_tasks`, never planned again here), each
/// with the key the engine gives it (`Tools::key`), checked again when it runs. The files'
/// outlines are `outlined`, sorted by name.
pub(crate) fn walk_lints<'a>(cx: &Cx, lints: &LintsStage, root: &Dir<ReadWrite>, outlined: &[Outlined<'_>], tasks: &[Task], alloc: SharedAlloc<'a>) -> Result<SVec<'a, Parked<'a>>, CliError> {
    let mut asked: SVec<'a, (usize, Task, ContentId)> = SVec::with_capacity_in(tasks.len(), alloc);
    for task in tasks.iter() {
        let one = &task.planned;
        let (file, outline) = outline_of(outlined, one.file).ok_or(EngineCheckError::ReadyHost { task: one.name, what: "a `Lint` task whose file is none of the request's outlines" })?;
        let unit = unit_of(outline, one.name).ok_or(EngineCheckError::ReadyHost { task: one.name, what: "a `Lint` task its file's outline lists no unit of" })?;
        asked.push((file, *task, unit));
    }
    asked.sort_unstable_by_key(|(file, task, _)| (*file, task.planned.name));
    let mut jobs: SVec<'a, (usize, SVec<'a, (Task, ContentId)>)> = SVec::new_in(alloc);
    for group in asked.chunk_by(|a, b| a.0 == b.0) {
        let mut units: SVec<'a, (Task, ContentId)> = SVec::with_capacity_in(group.len(), alloc);
        units.extend(group.iter().map(|(_, task, unit)| (*task, *unit)));
        jobs.push((group[0].0, units));
    }
    if jobs.is_empty() {
        return Ok(SVec::new_in(alloc));
    }
    if let [(file, units)] = jobs.as_slice()
        && let Some(one) = outlined.get(*file)
    {
        let rows = lints.lint_units(cx, root, *file, one, units, alloc);
        let mut parked: SVec<'a, Parked<'a>> = SVec::with_capacity_in(rows.len(), alloc);
        parked.extend(rows.into_iter().map(|(done, kept)| Parked { name: done.name, key: done.key, done: Some(done), kept, ran: false }));
        parked.sort_unstable_by_key(|one| one.name);
        return Ok(parked);
    }
    let span = cx.span("engine: the ready lint walks");
    let scope = span.scope();
    // The handles live in the walks' span frame (rule 11).
    let mut running = blaze_own::Vec::with_capacity_in(jobs.len(), span.frame());
    for (file, units) in jobs.iter() {
        let (file, one, units) = (*file, &outlined[*file], &units[..]);
        running.push(scope.may_overlap("engine ready lint walk", READY_FRAME, move |cx: &Cx| Ok::<_, Infallible>(lints.lint_units(cx, root, file, one, units, alloc))));
    }
    let mut parked: SVec<'a, Parked<'a>> = SVec::with_capacity_in(asked.len(), alloc);
    let mut failed = None;
    for handle in running.into_iter() {
        match handle.join(&span) {
            Ok(rows) => parked.extend(rows.into_iter().map(|(done, kept)| Parked { name: done.name, key: done.key, done: Some(done), kept, ran: false })),
            Err(error) => {
                failed.get_or_insert(error);
            }
        }
    }
    scope.join(&span).map_err(CliError::Pool)?;
    if let Some(error) = failed {
        return Err(CliError::Pool(error));
    }
    parked.sort_unstable_by_key(|one| one.name);
    Ok(parked)
}

/// `FileItems`: an edited file's outline, made by the planner and published here as the phase
/// path's `Tasks::outline` does: the same function, so the two drivers' rows are one code's.
///
/// None of the three kinds plans: an edited file's tasks are planned whole by
/// engine_ready_edit.rs `file_tasks`, the one plan of a file both drivers make (its heads too,
/// which no kind here runs: the precondition keeps a moved head on the phase path), and none of
/// them grows, so `Ask::Grown` never comes and `Ask::File` is never asked by the shell.
pub(crate) struct FileItemsKind<'p, 'h> {
    pub(crate) outlined: &'p [Outlined<'h>],
}

impl<'a> TaskKind<'a, Cx> for FileItemsKind<'_, '_> {
    fn facts(&self) -> KindFacts {
        KindFacts { kind: Kind::FileItems, stage: Stage::Outlines, grows_into: None }
    }

    fn plan(&self, ask: Ask, _out: &mut SVec<'a, Planned>) -> Result<(), Refusal> {
        Err(Refusal::CannotPlan { kind: Kind::FileItems, ask })
    }

    /// Reads nothing: it stands by its key (the file's text and edition, the module form).
    fn run(&self, _cx: &Cx, src: &Recorded<'_, 'a>, task: &Task, out: &mut Out<'a>) -> Ran<'a> {
        let done = crate::engine_tasks::Tasks { outlined: self.outlined }.outline(task, out.alloc());
        ran_of(src, done, Standing::As(Stands::ByKey), out)
    }
}

/// `Given`: one checkout read (planned by `engine_script_reads::plan` from today's leaf rows,
/// e.g. a data file read by a body via `include_str!`). Defines its `WHOLE` cell as today's
/// answer digest, reading nothing: stands by its key, identical to the phase path's `ScriptsStage`.
pub(crate) struct GivenKind<'p, 'a, 'f> {
    pub(crate) given: &'p [(Name, ContentId)],
    pub(crate) sink: &'p Sink<'a, 'f>,
}

impl<'a> TaskKind<'a, Cx> for GivenKind<'_, 'a, '_> {
    fn facts(&self) -> KindFacts {
        KindFacts { kind: Kind::Given, stage: Stage::Scripts, grows_into: None }
    }

    fn plan(&self, ask: Ask, _out: &mut SVec<'a, Planned>) -> Result<(), Refusal> {
        Err(Refusal::CannotPlan { kind: Kind::Given, ask })
    }

    fn run(&self, _cx: &Cx, src: &Recorded<'_, 'a>, task: &Task, out: &mut Out<'a>) -> Ran<'a> {
        let name = task.planned.name;
        let Some(&(_, answer)) = self.given.binary_search_by_key(&name, |(one, _)| *one).ok().and_then(|at| self.given.get(at)) else {
            return self.sink.fail(name, KindError::Host { task: name, what: "a `Given` task whose answer is not in today's planned checkout reads (engine_ready_check.rs `GivenKind`)" });
        };
        let done = crate::engine_script_reads::given_done(task, answer, out.alloc());
        ran_of(src, done, Standing::As(Stands::ByKey), out)
    }
}

/// `Lint`: one lint unit of an edited file (planned by `file_tasks`, engine_lints.rs `plan`).
/// Its row was made by its file's one walk before the drive ([`walk_lints`]) and parked in the
/// [`Sink`]; the run takes it (a kind answers one name a run, shell.rs `TaskKind::run`, so the
/// file's siblings are answered from the one walk this way, not by a run each). Reads nothing:
/// it stands by its key.
pub(crate) struct LintKind<'p, 'a, 'f> {
    pub(crate) sink: &'p Sink<'a, 'f>,
}

impl<'a> TaskKind<'a, Cx> for LintKind<'_, 'a, '_> {
    fn facts(&self) -> KindFacts {
        KindFacts { kind: Kind::Lint, stage: Stage::Lints, grows_into: None }
    }

    fn plan(&self, ask: Ask, _out: &mut SVec<'a, Planned>) -> Result<(), Refusal> {
        Err(Refusal::CannotPlan { kind: Kind::Lint, ask })
    }

    fn run(&self, _cx: &Cx, src: &Recorded<'_, 'a>, task: &Task, out: &mut Out<'a>) -> Ran<'a> {
        match self.sink.take_lint(task) {
            Ok(done) => ran_of(src, done, Standing::As(Stands::ByKey), out),
            Err(error) => self.sink.fail(task.planned.name, error),
        }
    }
}

/// The host as the body's read seam (`blaze_engine::CellDigests`): each digest PEEKED, not
/// recorded (a body places its own read list after reading, engine_bodies.rs `row_of`, and
/// [`ran_of`] hands that list to `Recorded::placed`, which holds each to today's digest). A read
/// the host refuses (its definer not final, K2) is remembered by the host and answered here as
/// ABSENT; [`ran_of`] asks the host's need before taking any result, so a result computed over
/// such a placeholder is never kept: the run is dropped whole and run again when the definer is
/// final.
struct HostCells<'s, 'r, 'a> {
    src: &'s Recorded<'r, 'a>,
}

impl CellDigests for HostCells<'_, '_, '_> {
    fn cell(&self, id: &CellId) -> ContentId {
        self.src.peek(id).unwrap_or(blaze_drive::key::ABSENT)
    }

    fn row_failed(&self, name: Name) -> Option<bool> {
        self.src.row(name).ok().flatten().map(|row| row.failed)
    }

    fn hit(&self, name: Name, key: blaze_drive::Key) -> Option<RowFacts> {
        self.src.hit(&CellId { of: name, cell: ROW_CELL }, key).ok().flatten()
    }
}

/// `BodyCheck`: one body, entered by the forward wake, run by `run_body` over the request's one
/// setup (the coordinator's `prepare`, its `Sync` part [`BodyRunner`]: `None` when the setup
/// stopped, every row then decided), every cell read through the host: a cell whose definer is
/// not final ends the run in `Need`, and `Ready` runs it again when it is (K2). Its lines and
/// findings go to the [`Sink`] once its result is the engine's (a `Need` drops them with the
/// run's `Out`); its typed error is kept there too (rule 19).
pub(crate) struct BodyKind<'p, 'f, 'r, 'a> {
    pub(crate) runner: Option<BodyRunner<'p, 'f, 'r>>,
    pub(crate) sink: &'p Sink<'a, 'f>,
    pub(crate) given_entering: &'p [Name],
}

impl<'a> TaskKind<'a, Cx> for BodyKind<'_, '_, '_, 'a> {
    fn facts(&self) -> KindFacts {
        KindFacts { kind: Kind::BodyCheck, stage: Stage::Bodies, grows_into: None }
    }

    fn plan(&self, ask: Ask, _out: &mut SVec<'a, Planned>) -> Result<(), Refusal> {
        Err(Refusal::CannotPlan { kind: Kind::BodyCheck, ask })
    }

    fn run(&self, cx: &Cx, src: &Recorded<'_, 'a>, task: &Task, out: &mut Out<'a>) -> Ran<'a> {
        let name = task.planned.name;
        // A row the setup decided (no check to run): the phase path's same row.
        if let Some((done, standing)) = self.sink.decided(src, task, out.alloc()) {
            return ran_of(src, done, standing, out);
        }
        let Some(runner) = self.runner.as_ref() else {
            return self.sink.fail(name, KindError::Host { task: name, what: "a body to check, but its setup stopped before any crate was read and decided every row (engine_bodies.rs `groups`)" });
        };
        // Check whether the body's own `FileItems` (`task.planned.file`), any entering `Given`
        // cells, and the member's `CrateScope` are final before opening scope crates or
        // typechecking: when an edited file's `FileItems` or `Given` is still running in Tick 1,
        // every `BodyCheck` would otherwise open its scope crates, typecheck, and discard all work
        // with `Ran::Need` at `ran_of`. Peeking finality up front fails in O(1) and parks the body
        // until Tick 1 merges so each body runs once.
        let _ = src.row(task.planned.file);
        for &given in self.given_entering {
            let _ = src.peek(&CellId { of: given, cell: crate::engine_tasks::WHOLE });
        }
        if let Some(own) = runner.own_scope_cell(name) {
            let _ = src.peek(&own);
        }
        if let Some(needs) = src.need() {
            return Ran::Need(needs);
        }
        let cells = HostCells { src };
        let mut run = match runner.run_body(cx, &cells, task, out.alloc()) {
            Ok(run) => run,
            Err(error) => return self.sink.fail(name, KindError::NotInSetup(error)),
        };
        // No body makes a record inline today (engine_script_reads.rs is the one maker); one
        // that did would need `Out::make` and its maker's read (invariant 12): refused, named.
        if !run.out.made.is_empty() {
            return self.sink.fail(name, KindError::Host { task: name, what: "a body made a record inline; the ready path carries none yet" });
        }
        let Some(at) = run.out.done.iter().position(|done| done.name == name) else {
            return self.sink.fail(name, KindError::Host { task: name, what: "its body run filed no row of it" });
        };
        let done = run.out.done.swap_remove(at);
        let standing = match run.out.owed.contains(&name) {
            true => Standing::Owed,
            false => Standing::As(Stands::ByReads),
        };
        let ran = ran_of(src, done, standing, out);
        if let Ran::Done(_) = &ran {
            self.sink.sunk.lock().bodies.push(run);
        }
        ran
    }
}

/// What the ready drive of one edit gives the coordinator: the engine's finished answer (its
/// `causes` the tasks entered), what the kinds sank (the lint units to keep, the bodies' lines
/// and findings) and the tasks whose run panicked, each answered by the shell's fault.
pub(crate) struct Driven<'a, 'f> {
    pub(crate) finished: blaze_engine::Finished<'a>,
    pub(crate) sunk: Sunk<'a, 'f>,
    pub(crate) panicked: SVec<'a, Name>,
}

/// The stages a ready edit reads, made by the coordinator before the drive: each entered file's
/// parked lint rows ([`walk_lints`]), and the bodies' one setup (`BodiesStage::prepare` over the
/// ready engine's view before the drive, which takes any `CellDigests`) as its runs read it
/// (`Prepared::runner` over `BodiesStage::reads_view`) with the rows that setup decided with no
/// check to run (its buffer).
pub(crate) struct Stages<'p, 'f, 'r, 'a> {
    pub(crate) parked: SVec<'a, Parked<'a>>,
    pub(crate) runner: Option<BodyRunner<'p, 'f, 'r>>,
    pub(crate) decided: blaze_engine::Buffer<'a>,
}

/// One edit on `Ready` (D187): `engine` (made by the coordinator over the last answer with the
/// edited files' plans, each from engine_ready_edit.rs `file_tasks`, and scoped to the forward
/// wake's entering names, which the precondition `admit` let through: every one a `FileItems`,
/// `Given`, `Lint` or `BodyCheck` task) driven on `pool` by its four kinds, and finished. A stop
/// is the command's typed error: a kind's own (rule 19) or the driver's ([`stopped`]), copied
/// into `heap`. Nothing here reads a header shard or a page beyond what the host reads for a
/// woken task's row and its reads' definers.
#[expect(clippy::too_many_arguments, reason = "the request's edge: each argument one handle of it")]
pub(crate) fn drive_edit<'a, 'f>(
    engine: blaze_engine::Engine<'a, crate::engine_ready_edit::Entering<'_>>,
    given_answers: &[(Name, ContentId)],
    given_entering: &[Name],
    outlined: &[Outlined<'_>],
    stages: Stages<'_, 'f, '_, 'a>,
    pool: &mut Workers<'_>,
    alloc: SharedAlloc<'a>,
    heap: SharedAlloc<'static>,
) -> Result<Driven<'a, 'f>, CliError> {
    let sink: Sink<'a, 'f> = Sink::new(stages.parked, stages.decided, alloc);
    // The kinds borrow the sink for the drive alone: it is taken whole after it.
    let drove = {
        let outlines = FileItemsKind { outlined };
        let given = GivenKind { given: given_answers, sink: &sink };
        let lint = LintKind { sink: &sink };
        let body = BodyKind { runner: stages.runner, sink: &sink, given_entering };
        let list: [&dyn TaskKind<'a, Cx>; 4] = [&outlines, &given, &lint, &body];
        let kinds = blaze_engine::kinds::Kinds::new(&list).map_err(EngineCheckError::ReadyKinds)?;
        // Every other kind is the plan's and stood (kinds.rs `KindTable::stood`): its rows served
        // as stored, and a name of it with no row that nothing plans or owes read as absent, as
        // the phase path reads a body's read of a head entry no head task is named for, or of the
        // impl bucket of a crate with no impl. A stood task that would move is entered by the wake
        // and refused by its kind before any run (engine_ready_edit.rs `admit`).
        let table = kinds.table().map_err(EngineCheckError::ReadyKinds)?.stood(&blaze_drive::Kind::ALL);
        // On threads the core's workers are the tick (ready.rs `Options::workers`): every task of
        // a tick starts at its opening and a tick is one batch of the pool, which narrows it to
        // the command's threads (`Workers::run`).
        let limits = blaze_engine::ready::Limits::SHIPPED;
        let options = blaze_engine::ready::Options { alloc, workers: limits.tick, kinds: table, limits };
        match blaze_engine::ready::Ready::new(engine, options, blaze_own::planted::NoPlant) {
            Ok(ready) => blaze_engine::shell::drive(ready, &kinds, pool, alloc),
            Err(error) => Err(error),
        }
    };
    let mut sunk = sink.into_sunk();
    let blaze_engine::shell::Drove { quiet, panicked } = drove.map_err(|error| stopped(error, &mut sunk, heap))?;
    let finished = quiet.finish()?;
    Ok(Driven { finished, sunk, panicked })
}

/// A lint walk's frame, as the phase path's: one file a job.
const READY_FRAME: usize = 1 << 28;
/// A ready engine task's frame: `FileItems`, `Given`, and `Lint` use almost none, and `BodyCheck`
/// reserves its own stretch/scratch frames (`BODIES_FRAME`/`BODY_SCRATCH`), so a 4 MiB reserve
/// avoids 256 MiB mmap/munmap churn on every tick's jobs.
const READY_TASK_FRAME: usize = 4 << 20;

/// The command's workers as the shell's pool (blaze-engine shell.rs `Pool`): each job of a
/// batch spawned on the request's scope with its own frame, its run timed on the command's
/// clock (rule 75: the clock is handed in), a run that unwinds caught at its edge
/// (`work::unwinding`, rule 20) and its job marked panicked. The clock is in nanoseconds from
/// `start`, the units the request's deadline is given in.
pub(crate) struct Workers<'c> {
    pub(crate) cx: &'c Cx,
    pub(crate) clock: &'c bstd::clock::Clock,
    pub(crate) start: bstd::clock::Instant,
    pub(crate) workers: core::num::NonZeroU32,
}

/// `now` on the request's clock: nanoseconds from `start`.
fn at(start: bstd::clock::Instant, now: bstd::clock::Instant) -> blaze_engine::code_pool::Time {
    blaze_engine::code_pool::Time(now.since(start).map_or(0, crate::engine_times::ns))
}

impl<'a> blaze_engine::shell::Pool<'a> for Workers<'_> {
    type Cx = Cx;

    fn now(&self) -> blaze_engine::code_pool::Time {
        at(self.start, self.clock.now())
    }

    fn run(&mut self, jobs: &mut [blaze_engine::shell::Job<'a>], until: blaze_engine::code_pool::Time, work: &(dyn Fn(&Cx, &mut blaze_engine::shell::Job<'a>) + Sync)) {
        let start = self.start;
        if jobs.len() <= 1 || self.workers.get() <= 1 {
            for job in jobs.iter_mut() {
                let began = at(start, self.clock.now());
                if began.0 > until.0 {
                    continue;
                }
                if crate::work::unwinding(|| work(self.cx, job)).is_err() {
                    job.panicked();
                }
                job.timed(began, at(start, self.clock.now()));
            }
            return;
        }
        let cx = self.cx.span("engine: a ready batch");
        let scope = cx.scope();
        scope.narrow_workers(self.workers, bstd::task::Class::Foreground);
        // The handles live in the batch's span frame (rule 11), not a global heap.
        let mut running = blaze_own::Vec::with_capacity_in(jobs.len(), cx.frame());
        for job in jobs.iter_mut() {
            let clock = self.clock.clone();
            running.push(scope.may_overlap("engine ready task", READY_TASK_FRAME, move |cx: &Cx| {
                let began = at(start, clock.now());
                // A job not started by the deadline is left unrun (shell.rs `Pool::run`).
                if began.0 > until.0 {
                    return Ok::<_, Infallible>(());
                }
                if crate::work::unwinding(|| work(cx, job)).is_err() {
                    job.panicked();
                }
                job.timed(began, at(start, clock.now()));
                Ok(())
            }));
        }
        // A spawn the scope refused ends as a job left unrun: the shell names it
        // (`Error::Unrun`), never a run quietly dropped (rule 3).
        for handle in running.into_iter() {
            let _unrun_is_named_by_the_shell = handle.join(&cx);
        }
        // The scope is linear: finished here. Each job's closure returns `Ok` and catches its
        // own unwind, so the one failure it can report is a task still running at the cx's
        // deadline, whose job is unrun or untimed: the shell names that job too.
        let _unended_is_named_by_the_shell = scope.join(&cx);
    }
}
