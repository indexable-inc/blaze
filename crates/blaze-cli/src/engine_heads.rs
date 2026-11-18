//! The `Heads` stage of `blaze engine-check` (engine.md E6): each crate's impl tables made ONCE
//! a request as its own task, and shared read-only by every body.
//!
//! THE PLAN, from text: one `ImplBucket` task a crate of the names world, named
//! [`impl_tables_name`] of its key, in `Phase { stage: Heads, wave: 0 }` (after every crate's
//! names), its source and its file the crate's names task's, so its key is known before any
//! phase runs and it enters with its crate's names ([`plan`]).
//!
//! THE TASK, on the pool: the crate's check scope (engine_bodies.rs `scope_of`, the rule the
//! Bodies stage takes too), its records opened in the task's frame, its impl rows collected
//! (`NamesSource::each_impl_row` of the crate at 0), and front2 buckets/records.rs
//! `impl_tables_task` run over the read-only view with its own recorder. Its row:
//! - defines: the tables' own cells (a bucket's, a trait's) and `WHOLE`, the three records' digest;
//! - reads: the crate's names record whole (its impl rows are its items record's), each names
//!   read of the filing as its crate's names record whole (the Names stage defines that cell),
//!   and a header text the filing lowered as every scope crate's names record whole (no task
//!   defines a head's cell yet: the header's crate is one of the scope's, whose records hold its
//!   text);
//! - its record in the store: the three records as one framed payload (buckets/records.rs
//!   `framed`), which a later request puts on its shelf while the task stands ([`complete`]).
//!
//! THE SHELF: each crate's records copied once onto engine_bodies.rs `BodiesStage::shelf` on the
//! coordinator after the phase; every body run reads it through its own `TableSet::lent`.
//! Nothing on it changes after the Heads phase, so nothing shared is mutable.

use core::convert::Infallible;

use blaze_drive::cells::Cell;
use blaze_drive::{Input, Key, Kind, Name};
use blaze_engine::{Buffer, CellId, Done, Phase, Planned, SVec, Stage, Task, View};
use blaze_ids::ContentId;
use blaze_ladder::front2::buckets::{ImplShelf, framed, impl_tables_name, impl_tables_task};
use blaze_ladder::front2::source::NamesSource;
use blaze_ladder::front2::tables::recorder::{Recorder, RecorderOpts};
use blaze_ladder::front2::tables::task::cell_id;
use blaze_ladder::front2::tables::view::{Tables, TablesOpts};
use blaze_own::{SharedAlloc, Vec};
use blaze_world::crate_list::CrateGraph;
use bstd::cx::Cx;
use bstd::mem::Frame;
use bstd::task::Class;
use std::alloc::Global;

use crate::CliError;
use crate::body_types::Kept;
use crate::engine_bodies::Handles;
use crate::engine_check::EngineCheckError;
use crate::engine_names::NamesStage;
use crate::engine_tasks::WHOLE;
use crate::run_names::Crate;
use crate::work;

/// A task's frame: its scope's records opened, its filing's scratch (blocks taken as used).
const HEADS_FRAME: usize = 1 << 30;

/// One `ImplBucket` task a crate of the names world (sorted by name).
pub(crate) fn plan<'h>(names: &NamesStage<'_>, alloc: SharedAlloc<'h>) -> Result<SVec<'h, Planned>, CliError> {
    let mut planned = SVec::new_in(alloc);
    let Some(world) = names.names.as_ref() else { return Ok(planned) };
    for (name, source) in crate::names::parts(world).iter() {
        let at = world.crate_at(name.subject).ok_or(EngineCheckError::Unseen { task: *name, read: *name })?;
        let tables = impl_tables_name(world.world().crates[at].input.key);
        planned.push(Planned { name: tables, phase: Phase::new(Stage::Heads, 0), source: *source, file: *name });
    }
    planned.sort_unstable_by_key(|one| one.name);
    Ok(planned)
}

/// One crate's tables as its task made them, all in `Global` (a worker's result crosses to the
/// coordinator, and the store's put reads it after the phase).
pub(crate) struct Made {
    pub(crate) name: Name,
    pub(crate) out: ContentId,
    /// Its read list as the store's record holds it: each read's record and digest, sorted.
    pub(crate) reads: std::vec::Vec<(Name, ContentId), Global>,
    /// The three records, [`framed`].
    pub(crate) payload: std::vec::Vec<u8, Global>,
}

/// What a worker hands back for one task.
struct Worked {
    out: ContentId,
    defines: std::vec::Vec<(Cell, ContentId), Global>,
    reads: std::vec::Vec<CellId, Global>,
    payload: std::vec::Vec<u8, Global>,
}

/// One task's inputs, made on the coordinator.
struct Job {
    task: Task,
    krate: blaze_id::CrateItemKey,
    /// The crate's own `CrateScope` name: read whole.
    own: Name,
    /// Its check scope's crates, the crate at 0.
    crates: Vec<Crate<'static>, Global>,
    /// Each scope crate's `CrateScope` name, sorted: what a header read widens to.
    scope: Vec<Name, Global>,
}

/// The stage: each task's result and refusal.
pub(crate) struct HeadsStage {
    /// Each task entered and made, sorted by name after its phase.
    made: std::vec::Vec<Made, Global>,
    /// Each task refused, and why (rule 19).
    pub(crate) refused: std::vec::Vec<(Name, &'static str), Global>,
    /// Crates whose standing tables were read back from the store this request.
    pub(crate) read_back: u32,
}

impl HeadsStage {
    pub(crate) fn new() -> HeadsStage {
        HeadsStage { made: std::vec::Vec::new_in(Global), refused: std::vec::Vec::new_in(Global), read_back: 0 }
    }

    /// Task `name`'s record, when it was entered and made this request.
    pub(crate) fn result(&self, name: Name) -> Option<&Made> {
        self.made.binary_search_by_key(&name, |one| one.name).ok().map(|at| &self.made[at])
    }

    /// The Heads phase: every task entered once, its result in `buffer`, its tables on `shelf`.
    #[expect(clippy::too_many_arguments, reason = "a phase's edge: each argument one handle of it")]
    pub(crate) fn run_phase<'a>(
        &mut self,
        cx: &Cx,
        store: &mut dyn crate::store::Store<'_>,
        handles: &Handles<'_, '_, '_, '_>,
        kept: &mut Kept,
        shelf: &mut ImplShelf,
        tasks: &[Task],
        view: View<'_, 'a>,
        buffer: &mut Buffer<'a>,
        times: &mut crate::engine_times::Times,
        alloc: SharedAlloc<'a>,
        frame: &Frame,
    ) -> Result<(), CliError> {
        let cx = cx.span("engine: the heads phase");
        let heap = handles.env.heap;
        let refused = &mut self.refused;
        let mut refuse = |task: &Task, why: &'static str, buffer: &mut Buffer<'a>| {
            buffer.done.push(crate::engine_tasks::failed(task, alloc));
            refused.push((task.planned.name, why));
        };
        // 1. Each task's scope, on this thread.
        let (Some(graph), Some(names)) = (handles.graph, handles.names) else {
            for task in tasks.iter() {
                refuse(task, "this command has no unit graph or no crate's names, so no crate's impl tables", buffer);
            }
            return Ok(());
        };
        let Some(list) = kept.list(&cx, store, &graph, frame)? else {
            for task in tasks.iter() {
                refuse(task, "the crates' inputs could not be made (names.rs `world`; the `UnitGraph` record's finding says why)", buffer);
            }
            return Ok(());
        };
        let (packs, corrected) = match kept.trees(&cx, handles.env, graph.out, &list, frame) {
            Ok(trees) => trees,
            Err(why) => {
                for task in tasks.iter() {
                    refuse(task, why, buffer);
                }
                return Ok(());
            }
        };
        // Each crate of the list by its tables' name.
        let mut by_name: std::vec::Vec<(Name, usize), Global> = std::vec::Vec::with_capacity_in(CrateGraph::len(&list), Global);
        by_name.extend((0..CrateGraph::len(&list)).map(|at| (impl_tables_name(list.key(at)), at)));
        by_name.sort_unstable();
        let mut jobs: std::vec::Vec<Job, Global> = std::vec::Vec::with_capacity_in(tasks.len(), Global);
        for task in tasks.iter() {
            let Ok(found) = by_name.binary_search_by_key(&task.planned.name, |(name, _)| *name) else {
                refuse(task, "an `ImplBucket` task of a crate the crate list does not hold", buffer);
                continue;
            };
            let at = by_name[found].1;
            match crate::engine_bodies::scope_of(kept, &cx, store, names, &list, at, heap) {
                Some((scope, true)) => {
                    let own = names.input_of(blaze_drive::key::crate_scope_name(&list.key(at))).map(|input| input.name);
                    let Some(own) = own else {
                        refuse(task, "its crate has no names record this command (its `Names` task failed)", buffer);
                        continue;
                    };
                    jobs.push(Job { task: *task, krate: list.key(at), own, crates: scope.crates, scope: scope.scope });
                }
                Some((_, false)) => refuse(task, "a crate of its check scope has no names record this command (its `Names` task failed)", buffer),
                None => refuse(task, "its crate's names record or check scope could not be read (engine_bodies.rs `scope_of`)", buffer),
            }
        }
        // 2. The tables, at once, on the workers.
        let procs = names.names.as_ref().and_then(|names| names.procs());
        let root = handles.root;
        let clock = handles.clock;
        let mut worked: std::vec::Vec<(Result<Result<Worked, &'static str>, work::Panicked>, u64), Global> = std::vec::Vec::with_capacity_in(jobs.len(), Global);
        {
            let scope = cx.scope();
            scope.narrow_workers(handles.workers, Class::Foreground);
            let mut running = std::vec::Vec::with_capacity_in(jobs.len(), Global);
            for job in jobs.iter() {
                let clock = clock.clone();
                running.push(scope.may_overlap("engine impl tables", HEADS_FRAME, move |cx: &Cx| {
                    let began = clock.now();
                    let made = work::unwinding(|| tables_of(cx, root, packs, corrected, procs, job, heap));
                    Ok::<_, Infallible>((made, clock.now().since(began).map_or(0, crate::engine_times::ns)))
                }));
            }
            let mut stopped = None;
            for handle in running.into_iter() {
                match handle.join(&cx) {
                    Ok(one) => worked.push(one),
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
        // 3. Each result as the engine's row, its tables on the shelf, in task order.
        for (job, (made, ns)) in jobs.iter().zip(worked) {
            let task = &job.task;
            let name = task.planned.name;
            times.push(name, ns);
            let one = match made {
                Ok(Ok(one)) => one,
                Ok(Err(why)) => {
                    refuse(task, why, buffer);
                    continue;
                }
                Err(work::Panicked) => {
                    refuse(task, "its impl tables' task panicked; that crate's tables alone failed", buffer);
                    continue;
                }
            };
            if shelf.put_framed(job.krate, &one.payload).is_err() {
                refuse(task, "its tables could not be put on the request's shelf (front2 buckets/records.rs `ImplShelf::put_framed`)", buffer);
                continue;
            }
            let mut defines: SVec<'a, (Cell, ContentId)> = SVec::with_capacity_in(one.defines.len() + 1, alloc);
            defines.push((WHOLE, one.out));
            defines.extend(one.defines.iter().copied());
            defines.sort_unstable_by_key(|(cell, _)| *cell);
            defines.dedup_by_key(|(cell, _)| *cell);
            let mut reads: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(one.reads.len() + 1, alloc);
            let own = CellId { of: job.own, cell: WHOLE };
            reads.push((own, view.cell(&own)));
            for id in one.reads.iter() {
                if id.of.kind == Kind::Signature {
                    for crate_scope in job.scope.iter() {
                        let whole = CellId { of: *crate_scope, cell: WHOLE };
                        reads.push((whole, view.cell(&whole)));
                    }
                } else {
                    reads.push((*id, view.cell(id)));
                }
            }
            reads.sort_unstable_by_key(|(id, _)| *id);
            reads.dedup_by_key(|(id, _)| *id);
            let mut record_reads: std::vec::Vec<(Name, ContentId), Global> = std::vec::Vec::with_capacity_in(reads.len(), Global);
            record_reads.extend(reads.iter().map(|(id, digest)| (id.of, *digest)));
            record_reads.dedup_by_key(|(of, _)| *of);
            buffer.done.push(Done { name, key: task.key, out: one.out, defines, reads, findings: 0, failed: false });
            self.made.push(Made { name, out: one.out, reads: record_reads, payload: one.payload });
        }
        self.made.sort_unstable_by_key(|one| one.name);
        Ok(())
    }
}

/// Before a Bodies phase: the tables of each crate whose Heads task stands from the stored
/// answer (not entered this request), read back from its record in the store by the key the
/// answer keeps (`record`), so every body reads one shelf holding every crate. A crate whose
/// task failed, or that has no row, is left off: a body asking it fails naming the crate
/// (buckets/records.rs `NoTables`), never answered from less (rule 3).
pub(crate) fn complete(cx: &Cx, store: &mut dyn crate::store::Store<'_>, names: &NamesStage<'_>, shelf: &mut ImplShelf, record: &dyn Fn(Name) -> Option<Key>, view: View<'_, '_>, frame: &Frame) -> Result<u32, CliError> {
    let Some(world) = names.names.as_ref() else { return Ok(0) };
    let mut read_back = 0u32;
    for krate in world.world().crates.iter() {
        let key = krate.input.key;
        if shelf.holds(key) {
            continue;
        }
        let name = impl_tables_name(key);
        let Some(row) = view.row(name) else { continue };
        if row.failed {
            continue;
        }
        let Some(at) = record(name) else {
            return Err(CliError::Text { what: "a standing `ImplBucket` row has no record key (pages.rs `records`; engine_heads.rs `complete`)" });
        };
        let read = work::payload(cx, store, &Input { name, at, out: row.out }, frame)?;
        shelf.put_framed(key, read.payload()).map_err(|_| CliError::Text { what: "a standing crate's impl tables read back malformed from the store (engine_heads.rs `complete`)" })?;
        read_back += 1;
    }
    Ok(read_back)
}

/// One crate's tables on a worker: its scope's records opened in the task's frame, its rows filed.
fn tables_of(
    cx: &Cx,
    root: &bstd::fs::Dir<bstd::fs::ReadWrite>,
    packs: &'static [blaze_fetch::PackView<'static>],
    corrected: &'static [(u32, &'static str, &'static str)],
    procs: Option<&'static crate::proc_runner::ProcRunner<'static>>,
    job: &Job,
    heap: &'static blaze_own::Heap,
) -> Result<Worked, &'static str> {
    let frame = cx.frame();
    let g = heap.alloc();
    let opened = crate::run_names::names_records(cx, root, &job.crates, packs, corrected, procs, std::vec::Vec::new_in(Global), None, frame, g).map_err(|_| "its crate's records could not be opened (run_names.rs `names_records`)")?;
    let records = opened.records;
    let mut rows = Vec::new_in(g);
    if !NamesSource::each_impl_row(records, 0, &mut |row| rows.push(row)) {
        return Err("its crate's records hold no impl rows (front2 source.rs `each_impl_row`)");
    }
    let done = impl_tables_task(records, 0, &rows, g);
    if done.failed.is_some() {
        return Err("its impl tables were not written (front2 buckets/records.rs `impl_tables_task`: a row its writer refuses)");
    }
    if done.name != job.task.planned.name {
        return Err("its tables' crate key is not its task's (front2 buckets/records.rs `impl_tables_name`)");
    }
    let tables = done.records.as_ref().ok_or("its impl tables' task made no records")?;
    let unused = Recorder::new(RecorderOpts { alloc: g });
    let view = Tables::new(TablesOpts { records, impls: None, lang_crates: &[], reads: &unused });
    let mut reads: std::vec::Vec<CellId, Global> = std::vec::Vec::with_capacity_in(done.reads.len(), Global);
    for read in done.reads.iter() {
        match cell_id(view, *read) {
            // A names read: its crate's names record whole, the cell the Names stage defines.
            Ok(Some(id)) if id.of.kind == Kind::CrateScope => reads.push(CellId { of: id.of, cell: WHOLE }),
            Ok(Some(id)) => reads.push(id),
            Ok(None) => {}
            Err(_) => return Err("a read of its filing names a crate its scope does not hold (front2 tables/task.rs `cell_id`)"),
        }
    }
    let mut defines = std::vec::Vec::with_capacity_in(done.defines.len(), Global);
    defines.extend(done.defines.iter().copied());
    Ok(Worked { out: done.out, defines, reads, payload: framed(tables, Global) })
}
