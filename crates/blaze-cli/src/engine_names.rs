//! The `Names` stage of `blaze engine-check` (engine.md E6, second step): one `CrateScope` task a
//! crate of the names world (members, dependencies, the toolchain shelf's crates), in the phase
//! `Phase { stage: Names, wave: the crate's depth }`, so a crate is entered after every crate it
//! reads was merged and put to the store.
//!
//! A task wraps the old path's three steps (names/scope.rs `prepare`, `resolve`, `answer`) as
//! its one entry: the job made on the coordinator's thread from records read through the store
//! (`prepare`, which holds the command's names world), the resolutions of one wave at once on
//! the workers, then each answered on the coordinator's thread. Its inputs are handed whole
//! before the entry: every crate of its dependency closure (the crates its dependencies'
//! records can reach are in it), and for the answer each surface and data file its resolution
//! read, from the planner's outlines. A function that still answers `Need` names a read the
//! plan did not see: a typed stop ([`EngineCheckError::Unseen`]), never a second entry.
//!
//! A crate's macro expansion rounds stay inside its task for now. A crate whose package has a
//! build script fails, named: `engine-check` runs no build script yet (the `Scripts` stage is
//! the next piece), and the crates above it fail `Waited` on it, as the old path's do.

use core::convert::Infallible;

use blaze_drive::cells::Cell;
use blaze_drive::{Input, Key, Kind, Locate, Name, TaskFailure, Why};
use blaze_engine::{Buffer, CellId, Done, Phase, Planned, SVec, Stage, Task as EngineTask};
use blaze_ids::ContentId;
use blaze_own::{SharedAlloc, Vec};
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::{Frame, Tag};
use bstd::task::Class;

use crate::engine_check::EngineCheckError;
use crate::engine_tasks::{Outlined, WHOLE};
use crate::engine_units::{Failure, Published, drive_task, published};
use crate::fetch::FetchEnv;
use crate::names::{Names, Prepared, ScopeRound};
use crate::work::{self, Did};
use crate::CliError;

/// Where the rule that refuses a crate on this path is.
const RULE: &str = "engine_names.rs";

/// The stage over one command's names world.
pub(crate) struct NamesStage<'f> {
    pub(crate) names: Option<Names<'f>>,
    /// Each crate entered so far, by name: its key and out, `None` when it failed.
    made: Vec<(Name, Option<(Key, ContentId)>), &'f Frame>,
    /// Each crate's result, in the order entered.
    pub(crate) results: Vec<Published<'f>, &'f Frame>,
}

/// The phase-independent handles a names phase reads.
pub(crate) struct Handles<'h, 'f> {
    pub(crate) root: &'h Dir<ReadWrite>,
    pub(crate) lock: Option<&'f blaze_manifest::lock::Lock<'static>>,
    pub(crate) members: Option<&'h [(blaze_manifest::lock::LockId, blaze_unit_graph::Text<'static>)]>,
    pub(crate) env: &'h FetchEnv<'h>,
    pub(crate) side: Option<&'h crate::proc_runner::MacroSide<'h, 'h, 'h>>,
    pub(crate) outlined: &'h [Outlined<'h>],
    pub(crate) workers: core::num::NonZeroU32,
}

impl<'f> NamesStage<'f> {
    pub(crate) fn new(names: Names<'f>, frame: &'f Frame) -> NamesStage<'f> {
        NamesStage { names: Some(names), made: Vec::new_in(frame), results: Vec::new_in(frame) }
    }

    /// One `CrateScope` task a crate of the world, at its depth (sorted by name).
    pub(crate) fn plan<'h>(&self, alloc: SharedAlloc<'h>, frame: &Frame) -> Result<SVec<'h, Planned>, CliError> {
        let names = self.names.as_ref().ok_or(CliError::Text { what: "the names world, made by the planner, is gone (engine_names.rs `plan`)" })?;
        let world = names.world();
        let count = world.crates.len();
        let mut depth: Vec<u16, &Frame> = Vec::new_in(frame);
        depth.resize(count, 0);
        let mut edges: Vec<usize, &Frame> = Vec::new_in(frame);
        // Relaxed to the longest path: a crate's depth is one more than its deepest edge's. A
        // walk still moving after `count` rounds is a cycle (rule 18).
        let mut rounds = 0usize;
        loop {
            let mut moved = false;
            for at in 0..count {
                edges.clear();
                edges_of(names, at, &mut edges);
                let deepest = edges.iter().map(|g| depth[*g].saturating_add(1)).max().unwrap_or(0);
                if deepest > depth[at] {
                    depth[at] = deepest;
                    moved = true;
                }
            }
            if !moved {
                break;
            }
            rounds += 1;
            if rounds > count {
                return Err(EngineCheckError::Cycle { krate: crate::names::name_of(world.crates[0].input.key) }.into());
            }
        }
        let mut planned = SVec::new_in(alloc);
        for (name, source) in crate::names::parts(names).iter() {
            let at = names.crate_at(name.subject).ok_or(EngineCheckError::Unseen { task: *name, read: *name })?;
            planned.push(Planned { name: *name, phase: Phase::new(Stage::Names, depth[at]), source: *source, file: *name });
        }
        planned.sort_unstable_by_key(|one| one.name);
        Ok(planned)
    }

    /// One names phase: every task entered once, its result in `buffer`.
    pub(crate) fn run_phase<'a>(
        &mut self,
        cx: &Cx,
        store: &mut dyn crate::store::Store<'_>,
        handles: &Handles<'_, 'f>,
        tasks: &[EngineTask],
        buffer: &mut Buffer<'a>,
        alloc: SharedAlloc<'a>,
        frame: &'f Frame,
    ) -> Result<(), CliError> {
        let cx = cx.span("engine: one names phase");
        let heap = handles.env.heap.alloc();
        let own = Frame::with_reserve(crate::names::SCOPE_JOBS, Tag::Other).map_err(|_| CliError::Text { what: "a names phase's frame could not be reserved" })?;
        let mut round = ScopeRound::new(&own, false);
        // 1. Each task's job, on this thread.
        let mut jobs: Vec<(usize, blaze_drive::Task<'_>, &crate::names::ScopeJob<'_>), &Frame> = Vec::new_in(frame);
        let mut answered: Vec<(usize, Result<work::Made<'f>, Failure<'f>>), &Frame> = Vec::new_in(frame);
        let mut closure: Vec<usize, &Frame> = Vec::new_in(frame);
        for (i, task) in tasks.iter().enumerate() {
            let name = task.planned.name;
            let names = self.names.as_ref().ok_or(CliError::Text { what: "the names world is gone (engine_names.rs `run_phase`)" })?;
            let Some(at) = names.crate_at(name.subject) else {
                answered.push((i, Err(refusal(name, "a `CrateScope` task of a crate the world does not hold"))));
                continue;
            };
            if names.script_of(at).is_some() {
                answered.push((i, Err(refusal(name, "a crate with a build script: `engine-check` runs no build script yet (the `Scripts` stage)"))));
                continue;
            }
            closure_of(names, at, &mut closure, frame);
            let mut inputs = blaze_own::Vec::new_in(heap);
            let mut failed = blaze_own::Vec::new_in(heap);
            for g in closure.iter() {
                let dep = crate::names::name_of(names.world().crates[*g].input.key);
                match self.made.binary_search_by_key(&dep, |(name, _)| *name) {
                    Ok(at) => match self.made[at].1 {
                        Some((key, out)) => inputs.push(Input { name: dep, at: key, out }),
                        None => failed.push(dep),
                    },
                    Err(_) => return Err(EngineCheckError::Unseen { task: name, read: dep }.into()),
                }
            }
            failed.sort_unstable();
            let drive = drive_task(name, task.key, task.planned.source, inputs, failed, heap);
            let prepared = crate::names::prepare(&cx, handles.root, store, &mut self.names, handles.lock, handles.members, handles.env, handles.side, &drive, &mut round, frame)?;
            match prepared {
                Prepared::Resolve(job) => jobs.push((i, drive, job)),
                Prepared::Answer(did) => answered.push((i, published(name, did)?)),
                Prepared::Made => return Err(EngineCheckError::Twice { task: name }.into()),
            }
        }
        // 2. The wave's resolutions at once, on the workers.
        let mut resolutions: Vec<Result<crate::names::Resolution, work::Panicked>, &Frame> = Vec::new_in(frame);
        {
            let scope = cx.scope();
            scope.narrow_workers(handles.workers, Class::Foreground);
            let mut running = Vec::new_in(frame);
            let root = handles.root;
            for (_, _, job) in jobs.iter() {
                let job: &crate::names::ScopeJob<'_> = job;
                running.push(scope.may_overlap("crate scope", crate::names::SCOPE_FRAME, move |cx: &Cx| {
                    Ok::<_, Infallible>(work::unwinding(|| crate::names::resolve(cx, root, job, cx.frame()).0))
                }));
            }
            let mut stopped = None;
            for handle in running.into_iter() {
                match handle.join(&cx) {
                    Ok(resolution) => resolutions.push(resolution),
                    Err(error) => {
                        stopped.get_or_insert(error);
                    }
                }
            }
            scope.join(&cx).map_err(CliError::Pool)?;
            if let Some(error) = stopped {
                return Err(CliError::Pool(error));
            }
        }
        // 3. Each answered, on this thread, with the surfaces and data files it read.
        for ((i, mut drive, _), resolution) in jobs.into_iter().zip(resolutions) {
            let name = drive.name;
            let names = self.names.as_mut().ok_or(CliError::Text { what: "the names world is gone (engine_names.rs `run_phase`)" })?;
            let resolution = match resolution {
                Ok(resolution) => resolution,
                Err(work::Panicked) => {
                    answered.push((i, Err(Failure { failure: TaskFailure { name, why: Why::Panicked { used: 0, limit: 0 } }, words: None })));
                    continue;
                }
            };
            if let Some(did) = crate::names::settle(names, &drive, resolution, frame) {
                answered.push((i, published(name, did)?));
                continue;
            }
            if let Some((files, data)) = names.read_files(name.subject) {
                for path in files.iter() {
                    let surface = Name::new(Kind::Surface, Name::file(path.as_bytes()).subject);
                    let record = outline_of(handles.outlined, path).ok_or(EngineCheckError::Unseen { task: name, read: surface })?;
                    let digest = record.parts.iter().find_map(|part| match (part.name.kind, part.locate) {
                        (Kind::Surface, Locate::Source(digest)) => Some(digest),
                        _ => None,
                    });
                    let out = digest.ok_or(EngineCheckError::Unseen { task: name, read: surface })?;
                    drive.inputs.push(Input { name: surface, at: Key::default(), out });
                }
                for (path, _) in data.iter() {
                    let file = Name::file(path.as_bytes());
                    let record = outline_of(handles.outlined, path).ok_or(EngineCheckError::Unseen { task: name, read: file })?;
                    drive.inputs.push(Input { name: file, at: Key::default(), out: record.out });
                }
            }
            let did = crate::names::answer(names, &drive, frame)?;
            answered.push((i, published(name, did)?));
        }
        // Each result as the engine's row and this stage's record, in task order.
        answered.sort_unstable_by_key(|(i, _)| *i);
        for (i, made) in answered.into_iter() {
            let task = &tasks[i];
            let name = task.planned.name;
            let done = match &made {
                Ok(record) => {
                    let mut reads: SVec<'a, (CellId, ContentId)> = SVec::new_in(alloc);
                    for (read, digest) in record.reads.iter() {
                        reads.push(match read.kind {
                            // A file's surface is read as its outline (the planner's `Outlines`
                            // task defines the one whole cell), until the names record's cells
                            // are wired (E4).
                            Kind::Surface => {
                                let file = Name::new(Kind::FileItems, read.subject);
                                let out = handles.outlined.binary_search_by(|one| one.name.cmp(&file)).ok().and_then(|at| handles.outlined[at].made.as_ref().ok()).map_or(*digest, |record| record.out);
                                (CellId { of: file, cell: WHOLE }, out)
                            }
                            _ => (CellId { of: *read, cell: WHOLE }, *digest),
                        });
                    }
                    reads.sort_unstable_by_key(|(id, _)| *id);
                    reads.dedup_by_key(|(id, _)| *id);
                    let mut defines: SVec<'a, (Cell, ContentId)> = SVec::new_in(alloc);
                    defines.push((WHOLE, record.out));
                    self.mark(name, Some((task.key, record.out)));
                    Done { name, key: task.key, out: record.out, defines, reads, findings: u32::try_from(record.findings.len()).unwrap_or(u32::MAX), failed: false }
                }
                Err(_) => {
                    self.mark(name, None);
                    crate::engine_tasks::failed(task, alloc)
                }
            };
            buffer.done.push(done);
            self.results.push(Published { name, source: task.planned.source, wave: task.planned.phase.wave, made });
        }
        Ok(())
    }

    fn mark(&mut self, name: Name, made: Option<(Key, ContentId)>) {
        match self.made.binary_search_by_key(&name, |(name, _)| *name) {
            Ok(at) => self.made[at].1 = made,
            Err(at) => self.made.insert(at, (name, made)),
        }
    }
}

/// A crate refused on this path, naming why.
fn refusal(name: Name, what: &'static str) -> Failure<'static> {
    Failure { failure: TaskFailure { name, why: Why::Refused { what, rule: RULE } }, words: None }
}

/// The outline of the checkout file at `path`: by its name, else by the path it was read at.
fn outline_of<'o, 'h>(outlined: &'o [Outlined<'h>], path: &str) -> Option<&'o work::Record<SharedAlloc<'h>>> {
    let name = Name::file(path.as_bytes());
    let one = match outlined.binary_search_by(|one| one.name.cmp(&name)) {
        Ok(at) => &outlined[at],
        Err(_) => outlined.iter().find(|one| one.path.as_slice() == path.as_bytes())?,
    };
    one.made.as_ref().ok()
}

/// The crates the crate at `at` reads directly: its dependencies and, for a program crate, the
/// sysroot crates (names/scope.rs `locals`: a toolchain crate is given what its row names).
fn edges_of(names: &Names<'_>, at: usize, into: &mut Vec<usize, &Frame>) {
    let world = names.world();
    into.extend(world.crates[at].input.deps.iter().map(|(_, g)| *g));
    if !matches!(world.crates[at].origin, blaze_world::Origin::Toolchain(_)) {
        let s = world.sysroot;
        into.extend([s.core, s.alloc, s.std, s.bstd].into_iter().flatten().map(|id| id.index()).filter(|g| *g != at));
    }
}

/// Every crate the crate at `at` reaches through its edges, itself left out, sorted.
fn closure_of(names: &Names<'_>, at: usize, into: &mut Vec<usize, &Frame>, frame: &Frame) {
    into.clear();
    let mut seen: Vec<bool, &Frame> = Vec::new_in(frame);
    seen.resize(names.world().crates.len(), false);
    seen[at] = true;
    let mut stack: Vec<usize, &Frame> = Vec::new_in(frame);
    edges_of(names, at, &mut stack);
    while let Some(g) = stack.pop() {
        if seen[g] {
            continue;
        }
        seen[g] = true;
        into.push(g);
        edges_of(names, g, &mut stack);
    }
    into.sort_unstable();
}
