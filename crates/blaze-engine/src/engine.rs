//! The plan, its phases and the bulk merge (engine.md, "The design" 2, 5 and 6).
//!
//! # The invariants (rule 53), each with its simulation in sim.rs
//!
//! 1. **The plan is made from text.** An [`Edit`] holds every planned task of each changed file
//!    (its outline's items), whatever the request's [`Scope`]: a changed file outside what the
//!    request runs is still planned, so its tasks' keys and its cells are compared
//!    (`a_shrinking_scope_answers_as_fresh_builds`; lane DR's finding 1, 2026-11-17).
//! 2. **Phases in order.** A task enters in its [`Phase`] only after every earlier phase merged,
//!    so it reads complete tables, and a part is never entered before its file's outline is made
//!    again (finding 2). A moved cell read by a task of the same or an earlier phase is
//!    [`EngineError::BackwardRead`], never a second entry: entries equal results.
//! 3. **Entered once.** Each task a phase names gets exactly one result ([`EngineError::NoResult`],
//!    [`EngineError::Unplanned`], [`EngineError::TwoValues`]); results are merged sorted by name.
//! 4. **Dirtying by cell, forward.** A result whose defined cells moved puts the cells' readers (the
//!    reverse index) in their phase's candidates. A candidate is entered when its key moved, it has
//!    no row, or a cell it read now holds another digest than its row saw. Nothing else enters.
//! 5. **The walk is global, the running scoped.** A candidate the scope does not run writes
//!    nothing. What makes its row not today's is kept in proportion to the edit, never to its
//!    readers: a moved cell is owed ([`Answer::owed_cells`], with the [`Gen`] it moved at) until
//!    every reader's row saw its digest, and a task whose key moved is owed by name
//!    ([`Answer::owed_tasks`]). Each request enters the owed cells' readers it runs: a reader held
//!    out of scope is entered when the scope reaches it (findings 3 and 4), and it never takes a
//!    read from its own row: validity compares every read with today's digest.
//! 6. **Prints by scope.** [`Answer::prints`] lists the findings of the rows the scope runs; a
//!    finding of a row outside it stays in the answer, unprinted, for the request that reaches
//!    it again (finding 5).
//! 7. **A failure is a row.** A failed task's result is its failure and its reads: it defines
//!    nothing (its old cells move to absent), prints as a finding, and stands until its key or a
//!    read moves, so the request after a failure with nothing changed is the certificate (tempo,
//!    tag g090627). The rest of the request is merged and compared like any (finding 6).
//! 8. **Inline needs.** A result may carry records the plan did not name ([`Made`]), each keyed
//!    from its own text; the merge takes them like any result, equal duplicates once.
//!
//! # Cost
//!
//! A phase costs a sort of its results and a constant; an edit costs its changed files' plans,
//! the moved cells and their readers. Nothing scans the answer's rows.
//!
//! # Threads
//!
//! A phase's tasks run on many workers at once: each reads the tables through one [`View`]
//! (`Copy + Sync`, read-only) and writes only its own [`Buffer`] (`Send`). So everything a view
//! reaches and everything a buffer holds is over a [`SharedAlloc`] (a `Sync` allocator's
//! handle); the work is `&(dyn Fn(..) + Sync)`, called from any worker.

use core::fmt;

use blaze_drive::cells::Cell;
use blaze_drive::key::{self, ABSENT, Key, Name, Tool};
use blaze_id::ContentId;
use blaze_own::{AVec, Alloc, BTreeMap, BTreeSet, SharedAlloc, Vec};

/// A vector over a `Sync` allocator: what a buffer holds and a view reads.
pub type SVec<'a, T> = Vec<T, SharedAlloc<'a>>;
/// A map over a `Sync` allocator: the answer's tables, read by every worker.
pub type SMap<'a, K, V> = BTreeMap<K, V, SharedAlloc<'a>>;
/// A set over a `Sync` allocator.
pub type SSet<'a, T> = BTreeSet<T, SharedAlloc<'a>>;

/// What a phase does, in the order a request runs them.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Stage {
    /// One file's outline: its items and the cells of their text.
    Outlines = 0,
    /// Names: crates in dependency waves, modules inside.
    Names = 1,
    /// Frozen heads and impl tables.
    Heads = 2,
    /// Bodies, at body grain.
    Bodies = 3,
}

/// One phase: a stage and a wave in it (a crate's names after its dependencies').
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Phase {
    pub stage: Stage,
    pub wave: u16,
}

impl Phase {
    pub const fn new(stage: Stage, wave: u16) -> Phase {
        Phase { stage, wave }
    }
}

/// A request's number: each answer is one more than the last.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Gen(pub u64);

/// One cell of one task's record: what a reader looked up.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CellId {
    pub of: Name,
    pub cell: Cell,
}

/// A task as the plan names it, from text: what it is, when it runs, its source and the file whose
/// outline lists it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Planned {
    pub name: Name,
    pub phase: Phase,
    pub source: ContentId,
    pub file: Name,
}

/// A task a phase enters: its plan and its key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Task {
    pub planned: Planned,
    pub key: Key,
}

/// One task's result, as a worker writes it into its buffer.
#[derive(PartialEq, Eq, Debug)]
pub struct Done<'a> {
    pub name: Name,
    pub key: Key,
    pub out: ContentId,
    /// The cells it defines and their digests, sorted by cell.
    pub defines: SVec<'a, (Cell, ContentId)>,
    /// The cells it read and the digest each held, sorted by cell id ([`ABSENT`]: none).
    pub reads: SVec<'a, (CellId, ContentId)>,
    pub findings: u32,
    /// The task failed on its reads: its row is the failure (invariant 7), its `defines` empty.
    pub failed: bool,
}

/// A record a task made inline that the plan did not name (engine.md, "The design" 3).
#[derive(PartialEq, Eq, Debug)]
pub struct Made<'a> {
    pub planned: Planned,
    pub done: Done<'a>,
}

/// One worker's results for a phase: `Send`, so a worker thread fills it.
pub struct Buffer<'a> {
    pub done: SVec<'a, Done<'a>>,
    pub made: SVec<'a, Made<'a>>,
}

impl<'a> Buffer<'a> {
    pub fn new_in(alloc: SharedAlloc<'a>) -> Buffer<'a> {
        Buffer { done: Vec::new_in(alloc), made: Vec::new_in(alloc) }
    }
}

/// One task's row in the answer.
#[derive(PartialEq, Eq, Debug)]
pub struct Row<'a> {
    pub key: Key,
    pub phase: Phase,
    pub file: Name,
    pub source: ContentId,
    pub out: ContentId,
    /// The request that made or checked it last.
    pub serial: Gen,
    pub defines: SVec<'a, (Cell, ContentId)>,
    pub reads: SVec<'a, (CellId, ContentId)>,
    pub findings: u32,
    /// Made inline: no file's plan lists it.
    pub inline: bool,
    /// The task failed on these reads: it defines nothing, its failure is a finding, and it
    /// stands like any row until a read or its key moves (invariant 7).
    pub failed: bool,
}

impl Row<'_> {
    fn planned(&self, name: Name) -> Planned {
        Planned { name, phase: self.phase, source: self.source, file: self.file }
    }
}

/// The answer: every row, every cell's digest, the reverse index, and what is owed. `Sync`: a
/// phase's workers read it at once through a [`View`].
pub struct Answer<'a> {
    pub serial: Gen,
    pub rows: SMap<'a, Name, Row<'a>>,
    /// Each cell's digest and the request it last moved in.
    pub cells: SMap<'a, CellId, (ContentId, Gen)>,
    /// Each cell's readers, sorted.
    pub readers: SMap<'a, CellId, SVec<'a, Name>>,
    /// Each file's planned tasks, sorted.
    pub by_file: SMap<'a, Name, SVec<'a, Name>>,
    /// Cells that moved while a reader of them was not run, with the request they moved in.
    pub owed_cells: SMap<'a, CellId, Gen>,
    /// Tasks whose key moved while the scope did not run them: no row of today's.
    pub owed_tasks: SMap<'a, Name, Planned>,
    pub with_findings: SSet<'a, Name>,
}

impl<'a> Answer<'a> {
    pub fn new_in(alloc: SharedAlloc<'a>) -> Answer<'a> {
        Answer {
            serial: Gen(0),
            rows: BTreeMap::new_in(alloc),
            cells: BTreeMap::new_in(alloc),
            readers: BTreeMap::new_in(alloc),
            by_file: BTreeMap::new_in(alloc),
            owed_cells: BTreeMap::new_in(alloc),
            owed_tasks: BTreeMap::new_in(alloc),
            with_findings: BTreeSet::new_in(alloc),
        }
    }

    /// The cell's digest today ([`ABSENT`]: no task defines it).
    pub fn cell(&self, id: &CellId) -> ContentId {
        self.cells.get(id).map_or(ABSENT, |(digest, _)| *digest)
    }

    /// The rows with findings the scope runs, by name; a row outside it keeps its findings
    /// unprinted (invariant 6).
    pub fn prints<'p, S: Scope>(&self, scope: &S, alloc: Alloc<'p>) -> AVec<'p, (Name, u32)> {
        let mut out = Vec::new_in(alloc);
        for name in self.with_findings.iter() {
            if let Some(row) = self.rows.get(name)
                && (row.inline || scope.runs(&row.planned(*name)))
            {
                out.push((*name, row.findings));
            }
        }
        out
    }

    /// Whether `name`'s row saw today's digest of every cell it read.
    fn today(&self, row: &Row<'_>) -> bool {
        row.reads.iter().all(|(id, seen)| self.cell(id) == *seen)
    }
}

/// What a request runs (`check`: every body asked; `run`: its closure's). The walk is global:
/// what it does not run is still planned and compared, and owed (invariant 5).
pub trait Scope {
    fn runs(&self, task: &Planned) -> bool;
}

/// A scope that runs everything.
pub struct Everything;

impl Scope for Everything {
    fn runs(&self, _: &Planned) -> bool {
        true
    }
}

/// A request's input from the sweep: each changed, new or gone file with every task its outline
/// plans today (none for a gone file).
pub struct Edit<'e> {
    pub files: &'e [(Name, &'e [Planned])],
}

/// A bug the simulation plants to show its oracle catches it. Always `None` outside the tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// The owed cells are dropped at the finish: a reader a narrower scope did not run is never
    /// entered when the scope reaches it again.
    OwedForgotten,
    /// A candidate whose key stands is valid without its reads compared.
    ReadsNotCompared,
    /// Records made inline are not merged.
    MadeDropped,
    /// A failed task writes no row and is owed, so every later request enters it again (tempo,
    /// tag g090627: 744 bodies with no record made nothing-changed 25 s instead of 0.09 s).
    FailureNotKept,
}

/// What one request did.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Report {
    /// Tasks entered, planned or inline, and results merged: equal in every request.
    pub entered: u32,
    pub results: u32,
    pub made: u32,
    /// Candidates the scope did not run.
    pub not_run: u32,
    pub failed: u32,
    pub phases: u32,
    /// Nothing entered and nothing moved: the answer is the last one.
    pub certified: bool,
}

/// Why a request stopped. Each names the thing (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EngineError {
    /// A task the phase named got no result.
    NoResult { name: Name },
    /// A result for a task the phase did not name.
    Unplanned { name: Name },
    /// Two results under one name differ (rule 21).
    TwoValues { name: Name },
    /// A result's key is not its task's.
    KeyMoved { name: Name, planned: Key, got: Key },
    /// A cell moved in `phase` and `reader` reads it in the same or an earlier phase.
    BackwardRead { reader: Name, read: CellId, reader_phase: Phase, phase: Phase },
    /// A phase was merged with no phase open, or a phase opened with one not merged.
    OutOfTurn,
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::NoResult { name } => write!(f, "{name:?} was entered and gave no result (engine.rs `merge`)"),
            EngineError::Unplanned { name } => write!(f, "a result for {name:?}, which the phase did not enter (engine.rs `merge`)"),
            EngineError::TwoValues { name } => write!(f, "two different results for {name:?} (rule 21, engine.rs `merge`)"),
            EngineError::KeyMoved { name, planned, got } => {
                write!(f, "{name:?}'s result has key {got:?}, its task {planned:?} (engine.rs `merge`)")
            }
            EngineError::BackwardRead { reader, read, reader_phase, phase } => write!(
                f,
                "{read:?} moved in {phase:?} and {reader:?} reads it in {reader_phase:?}: a phase reads only earlier ones (engine.md, invariant 2)"
            ),
            EngineError::OutOfTurn => write!(f, "a merge with no phase open, or a phase opened before the last merged (engine.rs)"),
        }
    }
}

/// What a finished request gives.
pub struct Finished<'a> {
    pub answer: Answer<'a>,
    pub report: Report,
    /// The names entered, in phase order and name order within a phase.
    pub entered: SVec<'a, Name>,
}

/// The read-only view a task reads the tables through (E2 grows it): `Copy + Sync`, one per
/// phase, shared by every worker.
#[derive(Clone, Copy)]
pub struct View<'e, 'a> {
    answer: &'e Answer<'a>,
}

/// Held at compile time: a view is shared by the workers and a buffer sent to one.
const _: () = {
    const fn sync<T: Sync + Copy>() {}
    const fn send<T: Send>() {}
    sync::<View<'static, 'static>>();
    send::<Buffer<'static>>();
};

impl View<'_, '_> {
    /// The cell's digest today: complete for every earlier phase.
    pub fn cell(&self, id: &CellId) -> ContentId {
        self.answer.cell(id)
    }
}

/// The pool behind a small trait, so the simulation drives it with 1 worker, N, or reversed, and
/// the driver with real threads.
pub trait Pool<'a> {
    /// What a worker hands each task: the system and the task's own frame on the driver's pool
    /// (bstd's `Cx`), nothing in the simulation's.
    type Cx: ?Sized;
    /// Runs every task once, each into one of `buffers` (a worker owns one buffer while it runs;
    /// `work` may be called from many threads at once).
    fn run(&mut self, tasks: &[Task], buffers: &mut [Buffer<'a>], work: &(dyn Fn(&Self::Cx, &Task, &mut Buffer<'a>) + Sync));
}

/// One request's engine.
pub struct Engine<'a, S: Scope> {
    alloc: SharedAlloc<'a>,
    tool: Tool,
    plant: Plant,
    scope: S,
    answer: Answer<'a>,
    serial: Gen,
    /// Candidates by phase.
    pending: SMap<'a, Phase, SSet<'a, Name>>,
    /// This request's plan of its changed files.
    planned: SMap<'a, Name, Planned>,
    /// The open phase and its tasks, sorted by name.
    open: Option<Phase>,
    tasks: SVec<'a, Task>,
    /// Cells moved this request.
    moved: SSet<'a, CellId>,
    entered: SVec<'a, Name>,
    report: Report,
}

impl<'a, S: Scope> Engine<'a, S> {
    /// Plans a request from the last answer and the edit (invariant 1): each changed file's
    /// tasks whose key moved, its tasks gone (their cells move to absent), the owed tasks, and
    /// the readers of the owed cells.
    pub fn new_in(tool: Tool, answer: Answer<'a>, edit: &Edit<'_>, scope: S, plant: Plant, alloc: SharedAlloc<'a>) -> Result<Engine<'a, S>, EngineError> {
        let serial = Gen(answer.serial.0 + 1);
        let mut engine = Engine {
            alloc,
            tool,
            plant,
            scope,
            answer,
            serial,
            pending: BTreeMap::new_in(alloc),
            planned: BTreeMap::new_in(alloc),
            open: None,
            tasks: Vec::new_in(alloc),
            moved: BTreeSet::new_in(alloc),
            entered: Vec::new_in(alloc),
            report: Report::default(),
        };
        for (file, plan) in edit.files.iter() {
            let old = engine.answer.by_file.remove(file).unwrap_or_else(|| Vec::new_in(alloc));
            let mut names: SVec<'a,Name> = Vec::with_capacity_in(plan.len(), alloc);
            for planned in plan.iter() {
                names.push(planned.name);
                engine.planned.insert(planned.name, *planned);
            }
            names.sort_unstable();
            for name in old.iter() {
                if names.binary_search(name).is_err() {
                    engine.answer.owed_tasks.remove(name);
                    engine.remove_row(*name, None)?;
                }
            }
            for planned in plan.iter() {
                let key = key::task_key(tool, planned.name, &planned.source);
                if engine.answer.rows.get(&planned.name).is_none_or(|row| row.key != key) {
                    engine.candidate(*planned, None)?;
                }
            }
            if !names.is_empty() {
                engine.answer.by_file.insert(*file, names);
            }
        }
        let owed: SVec<'a,Planned> = {
            let mut owed = Vec::new_in(alloc);
            for planned in engine.answer.owed_tasks.values() {
                owed.push(*planned);
            }
            owed
        };
        for planned in owed.iter() {
            let planned = engine.planned.get(&planned.name).copied().unwrap_or(*planned);
            engine.candidate(planned, None)?;
        }
        let mut cells: SVec<'a,CellId> = Vec::new_in(alloc);
        for id in engine.answer.owed_cells.keys() {
            cells.push(*id);
        }
        for id in cells.iter() {
            engine.readers_of(*id, None)?;
        }
        Ok(engine)
    }

    /// Opens the next phase with anything to enter: `None` when none is left. A candidate the
    /// scope does not run is left as it is (invariant 5); one whose row is today's is not
    /// entered (invariant 4).
    pub fn next_phase(&mut self) -> Result<Option<Phase>, EngineError> {
        if self.open.is_some() {
            return Err(EngineError::OutOfTurn);
        }
        while let Some((phase, names)) = self.pending.pop_first() {
            self.tasks.clear();
            for name in names.iter() {
                let Some(planned) = self.planned_of(*name) else { continue };
                if !self.scope.runs(&planned) {
                    self.report.not_run += 1;
                    let key = key::task_key(self.tool, planned.name, &planned.source);
                    if self.answer.rows.get(name).is_none_or(|row| row.key != key) {
                        self.answer.owed_tasks.insert(*name, planned);
                    }
                    continue;
                }
                let key = key::task_key(self.tool, planned.name, &planned.source);
                let stands = self.answer.rows.get(name).is_some_and(|row| {
                    row.key == key && !self.answer.owed_tasks.contains_key(name) && (self.plant == Plant::ReadsNotCompared || self.answer.today(row))
                });
                if stands {
                    continue;
                }
                self.tasks.push(Task { planned, key });
            }
            if !self.tasks.is_empty() {
                self.report.phases += 1;
                self.report.entered += self.tasks.len() as u32;
                for task in self.tasks.iter() {
                    self.entered.push(task.planned.name);
                }
                self.open = Some(phase);
                return Ok(Some(phase));
            }
        }
        Ok(None)
    }

    /// The open phase's tasks, sorted by name.
    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    /// The tables as they stand: complete for every phase before the open one.
    pub fn view(&self) -> View<'_, 'a> {
        View { answer: &self.answer }
    }

    /// Merges the open phase's buffers in bulk, sorted by name (invariant 3): rows, cells, the
    /// reverse index, and the readers of every moved cell as candidates of later phases.
    pub fn merge(&mut self, buffers: &mut [Buffer<'a>]) -> Result<(), EngineError> {
        let phase = self.open.take().ok_or(EngineError::OutOfTurn)?;
        let mut done: SVec<'a,Done<'a>> = Vec::new_in(self.alloc);
        let mut made: SVec<'a,Made<'a>> = Vec::new_in(self.alloc);
        for buffer in buffers.iter_mut() {
            done.append(&mut buffer.done);
            made.append(&mut buffer.made);
        }
        done.sort_unstable_by_key(|d| d.name);
        made.sort_unstable_by_key(|m| m.planned.name);
        let tasks = core::mem::replace(&mut self.tasks, Vec::new_in(self.alloc));
        // Both sorted by name: matched one to one, or the first mismatch named.
        for at in 0..tasks.len().max(done.len()) {
            match (tasks.get(at), done.get(at)) {
                (Some(task), Some(result)) if task.planned.name == result.name => {
                    if result.key != task.key {
                        return Err(EngineError::KeyMoved { name: result.name, planned: task.key, got: result.key });
                    }
                }
                (_, Some(result)) if at > 0 && done[at - 1].name == result.name => {
                    return Err(EngineError::TwoValues { name: result.name });
                }
                (Some(task), Some(result)) if result.name < task.planned.name => return Err(EngineError::Unplanned { name: result.name }),
                (Some(task), _) => return Err(EngineError::NoResult { name: task.planned.name }),
                (None, Some(result)) => return Err(EngineError::Unplanned { name: result.name }),
                (None, None) => break,
            }
        }
        // Results are in task order now: both are sorted by name and matched one to one.
        self.report.results += done.len() as u32;
        let mut done_iter = done.into_iter();
        for task in tasks.iter() {
            let result = done_iter.next().ok_or(EngineError::NoResult { name: task.planned.name })?;
            self.apply(task.planned, result, phase, false)?;
        }
        if self.plant != Plant::MadeDropped {
            let mut last: Option<Name> = None;
            let mut kept: SVec<'a,Made<'a>> = Vec::new_in(self.alloc);
            for one in made.into_iter() {
                let key = key::task_key(self.tool, one.planned.name, &one.planned.source);
                if one.done.key != key {
                    return Err(EngineError::KeyMoved { name: one.planned.name, planned: key, got: one.done.key });
                }
                if last == Some(one.planned.name) {
                    // Records are equal bytes by key: a duplicate made by two workers is one.
                    if kept.last().is_some_and(|prev| *prev != one) {
                        return Err(EngineError::TwoValues { name: one.planned.name });
                    }
                    continue;
                }
                last = Some(one.planned.name);
                kept.push(one);
            }
            for one in kept.into_iter() {
                if tasks.iter().any(|task| task.planned.name == one.planned.name) {
                    continue;
                }
                self.report.made += 1;
                self.report.entered += 1;
                self.report.results += 1;
                self.entered.push(one.planned.name);
                self.apply(one.planned, one.done, phase, true)?;
            }
        }
        Ok(())
    }

    /// Ends the request: the owed cells are this request's moved ones and the earlier ones, less
    /// each whose every reader's row saw its digest.
    pub fn finish(mut self) -> Result<Finished<'a>, EngineError> {
        if self.open.is_some() {
            return Err(EngineError::OutOfTurn);
        }
        for id in self.moved.iter() {
            self.answer.owed_cells.insert(*id, self.serial);
        }
        let answer = &self.answer;
        let mut settled: SVec<'a,CellId> = Vec::new_in(self.alloc);
        for id in answer.owed_cells.keys() {
            let digest = answer.cell(id);
            let all = answer.readers.get(id).is_none_or(|readers| {
                readers.iter().all(|reader| {
                    answer.rows.get(reader).is_none_or(|row| row.reads.binary_search_by_key(id, |(read, _)| *read).is_ok_and(|at| row.reads[at].1 == digest))
                })
            });
            if all {
                settled.push(*id);
            }
        }
        for id in settled.iter() {
            self.answer.owed_cells.remove(id);
        }
        if self.plant == Plant::OwedForgotten {
            self.answer.owed_cells.clear();
        }
        self.answer.serial = self.serial;
        self.report.certified = self.report.entered == 0 && self.moved.is_empty();
        Ok(Finished { answer: self.answer, report: self.report, entered: self.entered })
    }

    fn planned_of(&self, name: Name) -> Option<Planned> {
        if let Some(planned) = self.planned.get(&name) {
            return Some(*planned);
        }
        if let Some(planned) = self.answer.owed_tasks.get(&name) {
            return Some(*planned);
        }
        self.answer.rows.get(&name).map(|row| row.planned(name))
    }

    /// Puts `planned` in its phase's candidates; `moved_in` is the phase merging, whose readers
    /// must come later (invariant 2).
    fn candidate(&mut self, planned: Planned, moved_in: Option<(Phase, CellId)>) -> Result<(), EngineError> {
        if let Some((phase, read)) = moved_in
            && planned.phase <= phase
        {
            return Err(EngineError::BackwardRead { reader: planned.name, read, reader_phase: planned.phase, phase });
        }
        let alloc = self.alloc;
        self.pending.entry(planned.phase).or_insert_with(|| BTreeSet::new_in(alloc)).insert(planned.name);
        Ok(())
    }

    fn readers_of(&mut self, id: CellId, moved_in: Option<Phase>) -> Result<(), EngineError> {
        let Some(readers) = self.answer.readers.get(&id) else { return Ok(()) };
        let mut found: SVec<'a,Planned> = Vec::with_capacity_in(readers.len(), self.alloc);
        for reader in readers.iter() {
            if let Some(planned) = self.planned_of(*reader) {
                found.push(planned);
            }
        }
        for planned in found.iter() {
            self.candidate(*planned, moved_in.map(|phase| (phase, id)))?;
        }
        Ok(())
    }

    /// A cell's digest changed (or it went): recorded, owed, and its readers made candidates.
    fn move_cell(&mut self, id: CellId, digest: Option<ContentId>, phase: Option<Phase>) -> Result<(), EngineError> {
        match digest {
            Some(digest) => {
                self.answer.cells.insert(id, (digest, self.serial));
            }
            None => {
                self.answer.cells.remove(&id);
            }
        }
        self.moved.insert(id);
        self.readers_of(id, phase)
    }

    /// Removes `name`'s row: its reads leave the reverse index and its cells move to absent.
    fn remove_row(&mut self, name: Name, phase: Option<Phase>) -> Result<(), EngineError> {
        let Some(row) = self.answer.rows.remove(&name) else { return Ok(()) };
        self.answer.with_findings.remove(&name);
        for (id, _) in row.reads.iter() {
            self.unread(*id, name);
        }
        for (cell, _) in row.defines.iter() {
            self.move_cell(CellId { of: name, cell: *cell }, None, phase)?;
        }
        Ok(())
    }

    fn unread(&mut self, id: CellId, reader: Name) {
        let empty = match self.answer.readers.get_mut(&id) {
            Some(readers) => {
                if let Ok(at) = readers.binary_search(&reader) {
                    readers.remove(at);
                }
                readers.is_empty()
            }
            None => false,
        };
        if empty {
            self.answer.readers.remove(&id);
        }
    }

    fn apply(&mut self, planned: Planned, done: Done<'a>, phase: Phase, inline: bool) -> Result<(), EngineError> {
        let name = planned.name;
        if done.failed {
            self.report.failed += 1;
            if self.plant == Plant::FailureNotKept {
                self.answer.owed_tasks.insert(name, planned);
                return self.remove_row(name, Some(phase));
            }
        }
        self.answer.owed_tasks.remove(&name);
        let alloc = self.alloc;
        let old = self.answer.rows.remove(&name);
        // The reverse index: old reads out, new reads in.
        if let Some(old) = old.as_ref() {
            for (id, _) in old.reads.iter() {
                self.unread(*id, name);
            }
        }
        for (id, _) in done.reads.iter() {
            let readers = self.answer.readers.entry(*id).or_insert_with(|| Vec::new_in(alloc));
            if let Err(at) = readers.binary_search(&name) {
                readers.insert(at, name);
            }
        }
        // The defined cells: each that moved is recorded and its readers made candidates.
        let mut changes: SVec<'a,(CellId, Option<ContentId>)> = Vec::new_in(alloc);
        let empty: &[(Cell, ContentId)] = &[];
        let before = old.as_ref().map_or(empty, |old| old.defines.as_slice());
        let (mut i, mut j) = (0, 0);
        while i < before.len() || j < done.defines.len() {
            match (before.get(i), done.defines.get(j)) {
                (Some(b), Some(n)) if b.0 == n.0 => {
                    if b.1 != n.1 || self.answer.cells.get(&CellId { of: name, cell: n.0 }).is_none() {
                        changes.push((CellId { of: name, cell: n.0 }, Some(n.1)));
                    }
                    i += 1;
                    j += 1;
                }
                (Some(b), Some(n)) if b.0 < n.0 => {
                    changes.push((CellId { of: name, cell: b.0 }, None));
                    i += 1;
                }
                (Some(b), None) => {
                    changes.push((CellId { of: name, cell: b.0 }, None));
                    i += 1;
                }
                (_, Some(n)) => {
                    changes.push((CellId { of: name, cell: n.0 }, Some(n.1)));
                    j += 1;
                }
                (None, None) => break,
            }
        }
        if done.findings > 0 || done.failed {
            self.answer.with_findings.insert(name);
        } else {
            self.answer.with_findings.remove(&name);
        }
        self.answer.rows.insert(
            name,
            Row {
                key: done.key,
                phase: planned.phase,
                file: planned.file,
                source: planned.source,
                out: done.out,
                serial: self.serial,
                defines: done.defines,
                reads: done.reads,
                findings: done.findings,
                inline,
                failed: done.failed,
            },
        );
        for (id, digest) in changes.iter() {
            // An inline record is read in its own phase by the task that made it: it moves no
            // reader of an earlier entry this request (its readers are later or its makers).
            let moved_in = if inline { None } else { Some(phase) };
            match moved_in {
                Some(phase) => self.move_cell(*id, *digest, Some(phase))?,
                None => {
                    match digest {
                        Some(digest) => {
                            self.answer.cells.insert(*id, (*digest, self.serial));
                        }
                        None => {
                            self.answer.cells.remove(id);
                        }
                    }
                    self.moved.insert(*id);
                    self.inline_readers(*id, name, phase)?;
                }
            }
        }
        Ok(())
    }

    /// The readers of an inline record's moved cell: those of later phases are candidates; one
    /// of its own phase read it as it made it (or made it itself), so it is today's.
    fn inline_readers(&mut self, id: CellId, made: Name, phase: Phase) -> Result<(), EngineError> {
        let Some(readers) = self.answer.readers.get(&id) else { return Ok(()) };
        let mut found: SVec<'a,Planned> = Vec::with_capacity_in(readers.len(), self.alloc);
        for reader in readers.iter() {
            if *reader != made
                && let Some(planned) = self.planned_of(*reader)
                && planned.phase > phase
            {
                found.push(planned);
            }
        }
        for planned in found.iter() {
            self.candidate(*planned, None)?;
        }
        Ok(())
    }
}

/// One request's loop: each phase opened, run on the pool into the buffers, merged.
pub fn drive<'a, S: Scope, P: Pool<'a>>(
    engine: &mut Engine<'a, S>,
    pool: &mut P,
    buffers: &mut [Buffer<'a>],
    work: &(dyn Fn(&P::Cx, View<'_, 'a>, &Task, &mut Buffer<'a>) + Sync),
) -> Result<(), EngineError> {
    while engine.next_phase()?.is_some() {
        {
            let view = engine.view();
            let tasks = engine.tasks();
            pool.run(tasks, buffers, &|cx: &P::Cx, task: &Task, buffer: &mut Buffer<'a>| work(cx, view, task, buffer));
        }
        engine.merge(buffers)?;
    }
    Ok(())
}
