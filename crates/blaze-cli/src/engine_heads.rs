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
//!
//! THE HEAD TASKS (lane EY): one `Signature` task an outline entry that has a head, named and
//! keyed from text as the outline lists it ([`plan_items`]), in the same phase. Its record is the
//! frozen heads of every head cell the entry covers (front2 tables/head.rs `head_task`: a fn's
//! signature, an ADT, a trait with its members, an impl's header, an impl fn), each the cell
//! [`head_cell`] of the task with the frozen head's meaning, so a head re-made to equal bytes
//! moves no reader (early cutoff by digest, blaze-engine engine.rs). Its reads: each names cell its
//! freeze consulted, and for each head the freeze read (followed through the heads those read),
//! that head's file's `HEADS` cell (the Outlines phase's: a task reads no cell of its own phase)
//! or, for a dependency's head or one a macro wrote, its crate's names record whole. A body's
//! read of a member's head is the head's cell ([`Owners::place`], engine_bodies.rs `widen`); a
//! head that cannot be frozen is a failed row with its reason, and a body that read it fails by
//! that read. A dependency's head is read as its crate's names record whole (engine.md point 3).

use core::convert::Infallible;

use blaze_drive::cells::Cell;
use blaze_drive::{Input, Key, Kind, Locate, Name};
use blaze_engine::{Buffer, CellId, Done, Phase, Planned, SVec, Stage, Task, View};
use blaze_ids::ContentId;
use blaze_ladder::front2::buckets::{ImplShelf, framed, impl_tables_name, impl_tables_task};
use blaze_ladder::front2::by_origin;
use blaze_ladder::front2::heads::Want;
use blaze_ladder::front2::keys::{MemberKind, checker_of, member_key_of};
use blaze_ladder::front2::source::{NamesSource, Records};
use blaze_ladder::front2::tables::head::{HeadCells, SeededHeads, SeededHeadsOpts, cell_id as head_cell_id, head_task, want_bytes};
use blaze_ladder::front2::tables::recorder::{Recorder, RecorderOpts};
use blaze_ladder::front2::tables::task::cell_id;
use blaze_ladder::front2::tables::view::{Tables, TablesOpts};
use blaze_own::{Alloc, SharedAlloc, Vec};
use blaze_scope::items_record::ItemsRecord;
use blaze_scope::resolve::At;
use blaze_surface::ItemIndex;
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
use crate::engine_records::EngineRecords;
use crate::engine_tasks::{CELLS, Outlined, WHOLE};
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
struct Job<'r> {
    task: Task,
    krate: blaze_id::CrateItemKey,
    /// The crate's own `CrateScope` name: read whole.
    own: Name,
    /// Its check scope's crates, the crate at 0, their records in place in the store's mapping
    /// (engine_records.rs).
    crates: Vec<Crate<'r>, Global>,
    /// Each scope crate's `CrateScope` name, sorted: what a header read widens to.
    scope: Vec<Name, Global>,
    /// The cells a refusal of its tables is a function of ([`scope_cells`]).
    cells: std::vec::Vec<CellId, Global>,
}

/// The cells a refusal over crate `at`'s check scope is a function of (plan row A8): the
/// graph's (the crate list, the check scope and the packs are its record's), the crate's own
/// names cell, and with its `scope` each scope crate's names cell and the crate it was cut at
/// ([`crate::engine_bodies::Scope::cut`]), each `WHOLE`. A crate whose names task failed or did
/// not run defines no cell: read ABSENT, so its task that comes, or its fix, moves the row.
/// Unsorted: `failed_with` sorts and dedups.
fn scope_cells(graph: Name, list: &blaze_world::crate_list::CrateList<'static>, at: usize, scope: Option<&crate::engine_bodies::Scope<'_>>) -> std::vec::Vec<CellId, Global> {
    let mut cells: std::vec::Vec<CellId, Global> = std::vec::Vec::with_capacity_in(2 + scope.map_or(1, |scope| scope.scope.len() + 1), Global);
    cells.push(CellId { of: graph, cell: WHOLE });
    cells.push(CellId { of: blaze_drive::key::crate_scope_name(&list.key(at)), cell: WHOLE });
    if let Some(scope) = scope {
        cells.extend(scope.scope.iter().map(|one| CellId { of: *one, cell: WHOLE }));
        cells.extend(scope.cut_name(list).map(|one| CellId { of: one, cell: WHOLE }));
    }
    cells
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
    pub(crate) fn run_phase<'a, 'r>(
        &mut self,
        cx: &Cx,
        store: &mut dyn crate::store::Store<'r>,
        read: &mut EngineRecords<'r>,
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
        // The per-item head tasks first ([`HeadsStage::run_items`]), then the crates' tables.
        let (items, tasks): (std::vec::Vec<Task, Global>, std::vec::Vec<Task, Global>) = tasks.iter().copied().partition(|task| task.planned.name.kind == Kind::Signature);
        let tasks = &tasks[..];
        self.run_items(&cx, store, read, handles, kept, &items, view, buffer, times, alloc, frame)?;
        let heap = handles.env.heap;
        let refused = &mut self.refused;
        // A refusal with the cells it is a function of, each read as the view holds it today
        // (ABSENT where no task defines it: a crate whose names task failed or did not run), so
        // its row stands until one moves (plan row A8). None: owed, entered again each request,
        // said at each such call.
        let mut refuse = |task: &Task, why: &'static str, cells: &[CellId], buffer: &mut Buffer<'a>| {
            let mut reads: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(cells.len(), alloc);
            reads.extend(cells.iter().map(|id| (*id, view.cell(id))));
            buffer.done.push(crate::engine_tasks::failed_with(task, reads, alloc));
            refused.push((task.planned.name, why));
        };
        // 1. Each task's scope, on this thread.
        let (Some(graph), Some(names)) = (handles.graph, handles.names) else {
            // No `UnitGraph` task planned, or no names stage: no cell of this command says so,
            // so owed until the plan's inputs are cells (plan row D2).
            for task in tasks.iter() {
                refuse(task, "this command has no unit graph or no crate's names, so no crate's impl tables", &[], buffer);
            }
            return Ok(());
        };
        // The crate list, each crate's check scope and the packs are the graph record's.
        let graph_cell = CellId { of: graph.name, cell: WHOLE };
        let Some(list) = kept.list(&cx, store, &graph, frame)? else {
            for task in tasks.iter() {
                refuse(task, "the crates' inputs could not be made (names.rs `world`; the `UnitGraph` record's finding says why)", &[graph_cell], buffer);
            }
            return Ok(());
        };
        let (packs, corrected) = match kept.trees(&cx, handles.env, graph.out, &list, frame) {
            Ok(trees) => trees,
            Err(why) => {
                // A pack missing from the shelf or the sources store, or not the one the list's
                // world read (body_types.rs `open_trees`): the machine's files, not a cell, so
                // owed (rule 3: no invented read).
                for task in tasks.iter() {
                    refuse(task, why, &[], buffer);
                }
                return Ok(());
            }
        };
        // Each crate of the list by its tables' name.
        let mut by_name: std::vec::Vec<(Name, usize), Global> = std::vec::Vec::with_capacity_in(CrateGraph::len(&list), Global);
        by_name.extend((0..CrateGraph::len(&list)).map(|at| (impl_tables_name(list.key(at)), at)));
        by_name.sort_unstable();
        let mut jobs: std::vec::Vec<Job<'r>, Global> =std::vec::Vec::with_capacity_in(tasks.len(), Global);
        for task in tasks.iter() {
            let Ok(found) = by_name.binary_search_by_key(&task.planned.name, |(name, _)| *name) else {
                // The list is the graph's: a new graph record moves it.
                refuse(task, "an `ImplBucket` task of a crate the crate list does not hold", &[graph_cell], buffer);
                continue;
            };
            let at = by_name[found].1;
            match crate::engine_bodies::scope_of(read, &cx, store, names, &list, at, heap) {
                Some((scope, true)) => {
                    let cells = scope_cells(graph.name, &list, at, Some(&scope));
                    let own = names.input_of(blaze_drive::key::crate_scope_name(&list.key(at))).map(|input| input.name);
                    let Some(own) = own else {
                        refuse(task, "its crate has no names record this command (its `Names` task failed)", &cells, buffer);
                        continue;
                    };
                    jobs.push(Job { task: *task, krate: list.key(at), own, crates: scope.crates, scope: scope.scope, cells });
                }
                // The crate's names cell and each scope crate's that had records, and the first
                // that had none read ABSENT (`Scope::cut`): its task that comes, or its fix,
                // moves the row (engine_bodies.rs `scope_of`).
                Some((scope, false)) => refuse(task, "a crate of its check scope has no names record this command (its `Names` task failed)", &scope_cells(graph.name, &list, at, Some(&scope)), buffer),
                // `scope_of` reads the crate's own names record (ABSENT when its task failed)
                // and the check scope the graph makes: both cells.
                None => refuse(task, "its crate's names record or check scope could not be read (engine_bodies.rs `scope_of`)", &scope_cells(graph.name, &list, at, None), buffer),
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
                // `tables_of` over the scope's records: a refusal of them is a function of the
                // graph's cell (the packs) and each scope crate's names cell.
                Ok(Err(why)) => {
                    refuse(task, why, &job.cells, buffer);
                    continue;
                }
                // A panic is not a function of reads: owed, entered again each request (rule 20).
                Err(work::Panicked) => {
                    refuse(task, "its impl tables' task panicked; that crate's tables alone failed", &[], buffer);
                    continue;
                }
            };
            if shelf.put_framed(job.krate, &one.payload).is_err() {
                // The request's shelf, filled this request (its cap, its order): no cell, so owed.
                refuse(task, "its tables could not be put on the request's shelf (front2 buckets/records.rs `ImplShelf::put_framed`)", &[], buffer);
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
    job: &Job<'_>,
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

// ---------------------------------------------------------------------------------------------
// The head tasks: one an outline entry that has a head (module docs, "THE HEAD TASKS").

/// Each member crate's files as its items record lists them, matched to the planner's outlines:
/// (its crate key's prefix, the record's file index, the outline's `FileItems` name), sorted.
pub(crate) type Subjects = [([u8; 8], u32, Name)];

/// The domain of a head cell ([`head_cell`]).
const HEAD_CELL_DOMAIN: &str = "blaze-cli engine head cell 2";
/// The domain of a head task's source ([`plan_items`]).
const HEAD_SOURCE_DOMAIN: &str = "blaze-cli engine head source 1";
/// The domain of a head task's out: its cells and their digests, in order.
const HEAD_OUT_DOMAIN: &str = "blaze-cli engine head entry out 1";
/// The tool `head_task` folds into a key the engine does not use (the engine keys the task by
/// its name and source, `Tools::key`).
const HEAD_TOOL_DOMAIN: &str = "blaze-cli engine head task";

/// The cell of a head in the task of the outline entry that covers it: by what the entry's text
/// names it (the want's kind, the item's name, an impl fn's name), never by its checker key. A
/// crate's item keys are of the unit that holds it: `b`'s view of `a` (a dependency's unit) keys
/// `a::one` apart from `a`'s own check scope, where its head task runs, so a cell by key was one
/// no task defines (lane EY, the callers test). `part`: `WHOLE`, the whole head, the only part
/// today, hashed in so a head can be split later (engine.md "ASPECTS"). Two heads of one entry
/// under one cell are refused ([`heads_of`]).
pub(crate) fn head_cell(want: Want, item: &str, member: Option<&str>, part: Cell) -> Cell {
    let mut h = blaze_ids::Hasher::new(HEAD_CELL_DOMAIN);
    h.update(&want_bytes(want)[..1]).part(item.as_bytes()).opt_part(member).update(&part.0);
    let digest = h.finish();
    let mut cell = [0u8; 16];
    cell.copy_from_slice(&digest.0[..16]);
    Cell(cell)
}

/// One `Signature` task an outline entry that has a head (signature.rs `is_signature_entry`), in
/// `Phase { stage: Heads, wave: 0 }`, named as the outline lists it (signature.rs
/// `signature_name`: the file and the entry's chain), so its name and key are known from text
/// before any phase runs. Its source (rule 21): the outline's head source (the entry's head bytes,
/// its containers' heads and the file's surface, signature.rs `signature_source`), the item's
/// whole text when it has a body part (a frozen fn head holds its text's digest), and the edition.
pub(crate) fn plan_items<'h>(outlined: &[Outlined<'_>], alloc: SharedAlloc<'h>) -> SVec<'h, Planned> {
    let mut planned = SVec::new_in(alloc);
    for one in outlined.iter() {
        let Ok(record) = &one.made else { continue };
        for part in record.parts.iter() {
            let (Kind::Signature, Locate::Source(head)) = (part.name.kind, part.locate) else { continue };
            let body = record.parts.iter().find_map(|other| match (other.name.kind, other.locate) {
                (Kind::BodyCheck, Locate::Source(text)) if other.place == part.place => Some(text),
                _ => None,
            });
            let mut h = blaze_ids::Hasher::new(HEAD_SOURCE_DOMAIN);
            h.update(&head.0).update(&body.unwrap_or(blaze_drive::key::ABSENT).0);
            let source = crate::engine_tasks::edition_source(h.finish(), one.edition);
            planned.push(Planned { name: part.name, phase: Phase::new(Stage::Heads, 0), source, file: one.name });
        }
    }
    planned.sort_unstable_by_key(|one| one.name);
    planned.dedup_by_key(|one| one.name);
    planned
}

/// Where a read of a head (a `Signature` cell named by its item, front2 tables/head.rs
/// `cell_id`) is read from in this engine.
#[derive(Clone, Copy)]
pub(crate) enum HeadAt {
    /// A member crate's head: the cell of the head task of the entry that covers it, and that
    /// entry's file (its outline's `FileItems` name).
    Cell { cell: CellId, file: Name },
    /// A dependency's head (its crate defines no cell table): its crate's names record, whole
    /// (nothing of a dependency moves unless its record does).
    Dependency(Name),
    /// A member crate's head no entry of its files covers (a macro wrote it): its crate's names
    /// record whole, which holds every expansion's text.
    Unplaced(Name),
}

/// Each head of the member crates a task reads, by its item's head name, filled one crate at a
/// time on first ask, over a task's own records.
pub(crate) struct Owners<'s> {
    subjects: &'s Subjects,
    /// The crates (by key prefix) filled so far.
    crates: std::vec::Vec<[u8; 8], Global>,
    /// (head name, its entry's task, its file, its cell in that task), sorted by head name.
    rows: std::vec::Vec<(Name, Name, Name, Cell), Global>,
}

impl<'s> Owners<'s> {
    pub(crate) fn new(subjects: &'s Subjects) -> Owners<'s> {
        Owners { subjects, crates: std::vec::Vec::new_in(Global), rows: std::vec::Vec::new_in(Global) }
    }

    /// Where the head read `id` is read from; `None`: its crate is none of `records`'.
    pub(crate) fn place<'n>(&mut self, records: &'n Records<'n>, id: &CellId, g: Alloc<'static>) -> Option<HeadAt> {
        let mut prefix = [0u8; 8];
        prefix.copy_from_slice(&id.of.subject.0[..8]);
        let (k, key) = (0..NamesSource::crate_count(records)).find_map(|k| NamesSource::crate_key(records, k).filter(|key| key.prefix() == prefix).map(|key| (k, key)))?;
        let scope = blaze_drive::key::crate_scope_name(&key);
        // A crate with no file among the members' outlines (a dependency) has no head tasks.
        // Judged by the files, not by the `CELLS` mark of the scope named here: by that mark,
        // `b`'s caller of `a::one` read `a` whole and missed `one`'s new head (lane EY,
        // engine_check_rechecks_the_callers_of_an_edited_signature).
        let from = self.subjects.partition_point(|(p, _, _)| *p < prefix);
        if self.subjects.get(from).is_none_or(|(p, _, _)| *p != prefix) {
            return Some(HeadAt::Dependency(scope));
        }
        if !self.crates.contains(&prefix) {
            self.crates.push(prefix);
            let rows = &mut self.rows;
            heads_of_crate(records, k, prefix, self.subjects, g, &mut |one| rows.push((one.sig, one.entry, one.file, one.cell)));
            rows.sort_unstable();
            rows.dedup_by_key(|(sig, _, _, _)| *sig);
        }
        Some(match self.rows.binary_search_by_key(&id.of, |(sig, _, _, _)| *sig) {
            // The only part is the whole head (`WHOLE`, front2 head.rs `cell_id`).
            Ok(at) => HeadAt::Cell { cell: CellId { of: self.rows[at].1, cell: self.rows[at].3 }, file: self.rows[at].2 },
            Err(_) => HeadAt::Unplaced(scope),
        })
    }
}

/// One head cell [`heads_of_crate`] lists.
struct HeadRow {
    /// Its item's head name in the records read (front2 head.rs `cell_id`; of their unit).
    sig: Name,
    /// The task of the outline entry that covers it.
    entry: Name,
    /// Its file's outline.
    file: Name,
    /// Its cell in `entry`'s task ([`head_cell`]; of no unit).
    cell: Cell,
    want: Want,
}

/// Every head cell of crate `k` of `records` whose item is written in a file of `subjects`, in
/// file order. An item a macro wrote (its text an expansion's) or one no entry covers is not
/// listed ([`HeadAt::Unplaced`]).
fn heads_of_crate<'n>(records: &'n Records<'n>, k: usize, prefix: [u8; 8], subjects: &Subjects, g: Alloc<'static>, each: &mut dyn FnMut(HeadRow)) {
    let mut keys: Vec<blaze_id::ItemKey, Alloc<'static>> = Vec::new_in(g);
    NamesSource::each_item(records, k, &mut |key, _| keys.push(key));
    // (file, item index, want, the item's name, an impl fn's name).
    let mut placed: std::vec::Vec<(u32, u32, Want, &'n str, Option<&'n str>), Global> = std::vec::Vec::new_in(Global);
    for key in keys.iter().copied() {
        let Ok(want) = by_origin::kind_of_lib(records, key).and_then(|kind| by_origin::item_want(records, kind)) else { continue };
        let Some(place) = NamesSource::place_of(records, key) else { continue };
        let At::File(file) = place.at else { continue };
        let item = NamesSource::name_of(records, key).unwrap_or("");
        placed.push((file, place.item.0, want, item, None));
        if let Want::Impl(lib) = want {
            NamesSource::each_member(records, lib, &mut |m| {
                if let (blaze_surface::ItemKind::Function, None, Some(name)) = (m.kind, m.written_by, m.name) {
                    placed.push((file, m.item.0, Want::MemberFn(lib, member_key_of(checker_of(lib), MemberKind::Fn, name)), item, Some(name)));
                }
            });
        }
    }
    placed.sort_unstable_by_key(|(file, item, want, _, _)| (*file, *item, want_bytes(*want)));
    let edition = NamesSource::edition_of(records, k);
    for run in placed.chunk_by(|a, b| a.0 == b.0) {
        let file = run[0].0;
        let Ok(at) = subjects.binary_search_by(|(p, f, _)| (*p, *f).cmp(&(prefix, file))) else { continue };
        let outline_name = subjects[at].2;
        let (Some(text), Some(edition)) = (NamesSource::file_text(records, k, file), edition) else { continue };
        let outline = blaze_file_items::outline(text, edition, g);
        for (_, item, want, name, member) in run.iter() {
            let Some(cover) = crate::signature::covering_entry(&outline, ItemIndex(*item)) else { continue };
            let entry = crate::signature::signature_name(outline_name.subject, &outline, outline.item(cover), text);
            each(HeadRow { sig: head_cell_id(*want).of, entry, file: outline_name, cell: head_cell(*want, name, *member, WHOLE), want: *want });
        }
    }
}

/// A names read as its crate's task defines it (engine_bodies.rs `widen`'s rule): a cell of a
/// crate with a cell table stays its cell (its lang rows one of them); any other its record whole.
fn names_read(view: View<'_, '_>, id: CellId) -> CellId {
    match id.of.kind {
        Kind::CrateScope if id.cell != WHOLE && view.cell(&CellId { of: id.of, cell: CELLS }) != blaze_drive::key::ABSENT => id,
        Kind::CrateScope => CellId { of: id.of, cell: WHOLE },
        _ => id,
    }
}

/// One member crate's head tasks, made on one worker over its check scope's records (in place
/// in the store's mapping, engine_records.rs).
struct ItemJob<'r> {
    prefix: [u8; 8],
    crates: Vec<Crate<'r>, Global>,
    lang: Vec<usize, Global>,
    /// Every crate of its scope had records (else its tasks are refused by name).
    whole: bool,
    /// The cells a refusal of its tasks is a function of ([`scope_cells`]): at 0 the graph's,
    /// at 1 the member's own names cell.
    cells: std::vec::Vec<CellId, Global>,
    /// Its tasks, sorted by name.
    tasks: std::vec::Vec<Task, Global>,
}

/// One head task as a worker made it.
struct WorkedHead {
    out: ContentId,
    defines: std::vec::Vec<(Cell, ContentId), Global>,
    reads: std::vec::Vec<CellId, Global>,
    /// Each cell's want (front2 head.rs `want_bytes`), its frozen bytes' length (u32 LE) and
    /// the frozen bytes, in cell order.
    payload: std::vec::Vec<u8, Global>,
}

/// A head task that could not freeze a head: why (rule 19), and what it read until then.
struct HeadFailed {
    why: &'static str,
    reads: std::vec::Vec<CellId, Global>,
}

impl HeadsStage {
    /// The head tasks of one Heads phase (module docs, "THE HEAD TASKS"): each member crate's
    /// tasks on one worker, each result its row in `buffer`.
    #[expect(clippy::too_many_arguments, reason = "a phase's edge: each argument one handle of it")]
    fn run_items<'a, 'r>(
        &mut self,
        cx: &Cx,
        store: &mut dyn crate::store::Store<'r>,
        read: &mut EngineRecords<'r>,
        handles: &Handles<'_, '_, '_, '_>,
        kept: &mut Kept,
        tasks: &[Task],
        view: View<'_, 'a>,
        buffer: &mut Buffer<'a>,
        times: &mut crate::engine_times::Times,
        alloc: SharedAlloc<'a>,
        frame: &Frame,
    ) -> Result<(), CliError> {
        if tasks.is_empty() {
            return Ok(());
        }
        let heap = handles.env.heap;
        let g = heap.alloc();
        let refused = &mut self.refused;
        // A refusal with the cells it is a function of, each as the view holds it today (ABSENT
        // where no task defines it), through `failed_with` (sorted, once each): its row stands
        // until one moves (plan row A8). None: owed, said at each such call.
        let mut refuse = |task: &Task, why: &'static str, cells: &[CellId], buffer: &mut Buffer<'a>| {
            let mut reads: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(cells.len(), alloc);
            reads.extend(cells.iter().map(|id| (*id, view.cell(id))));
            buffer.done.push(crate::engine_tasks::failed_with(task, reads, alloc));
            refused.push((task.planned.name, why));
        };
        let (Some(graph), Some(names)) = (handles.graph, handles.names) else {
            // No `UnitGraph` task planned, or no names stage: no cell of this command says so,
            // so owed until the plan's inputs are cells (plan row D2).
            for task in tasks.iter() {
                refuse(task, "this command has no unit graph or no crate's names, so no head", &[], buffer);
            }
            return Ok(());
        };
        // The crate list, each member's check scope and the packs are the graph record's.
        let graph_cell = CellId { of: graph.name, cell: WHOLE };
        let Some(list) = kept.list(cx, store, &graph, frame)? else {
            for task in tasks.iter() {
                refuse(task, "the crates' inputs could not be made (names.rs `world`; the `UnitGraph` record's finding says why)", &[graph_cell], buffer);
            }
            return Ok(());
        };
        let (packs, corrected) = match kept.trees(cx, handles.env, graph.out, &list, frame) {
            Ok(trees) => trees,
            Err(why) => {
                // The machine's packs (body_types.rs `open_trees`), not a cell: owed (rule 3).
                for task in tasks.iter() {
                    refuse(task, why, &[], buffer);
                }
                return Ok(());
            }
        };
        // 1. Every member's files and check scope, on this thread: each scope crate's records
        // read in place through `read`, opened and checked once a request however many members'
        // scopes hold it (engine_records.rs), and the workers below handed shared borrows of them.
        let mut subjects: std::vec::Vec<([u8; 8], u32, Name), Global> = std::vec::Vec::new_in(Global);
        let mut jobs: std::vec::Vec<ItemJob<'r>, Global> = std::vec::Vec::new_in(Global);
        // What "its file is in no member crate's items record" is a function of: the graph's
        // cell (which crates are members, their roots) and every member's names cell (its items
        // record; ABSENT for a member whose names task failed, so its fix moves the row).
        let mut no_member: std::vec::Vec<CellId, Global> = std::vec::Vec::new_in(Global);
        no_member.push(graph_cell);
        for member in (0..CrateGraph::len(&list)).filter(|at| list.member(*at)) {
            no_member.push(CellId { of: blaze_drive::key::crate_scope_name(&list.key(member)), cell: WHOLE });
            let Some((scope, whole)) = crate::engine_bodies::scope_of(read, cx, store, names, &list, member, heap) else { continue };
            let cells = scope_cells(graph.name, &list, member, Some(&scope));
            let Ok(items) = ItemsRecord::open(scope.own_records.items, g) else { continue };
            let prefix = list.key(member).prefix();
            for file in 0..u32::try_from(items.files()).unwrap_or(u32::MAX) {
                let Ok(Some(row)) = items.file(file) else { continue };
                let Some(path) = row.path.in_package(list.shown_from(member), g) else { continue };
                let Some(path) = path.strip_prefix("/0/") else { continue };
                if let Some(one) = handles.outlined.iter().find(|one| one.path.as_slice() == path.as_bytes()) {
                    subjects.push((prefix, file, one.name));
                }
            }
            jobs.push(ItemJob { prefix, crates: scope.crates, lang: scope.lang, whole, cells, tasks: std::vec::Vec::new_in(Global) });
        }
        subjects.sort_unstable();
        jobs.sort_unstable_by_key(|job| job.prefix);
        // Each task to the first member (by prefix) whose files hold its file.
        let mut by_file: std::vec::Vec<(Name, [u8; 8]), Global> = std::vec::Vec::with_capacity_in(subjects.len(), Global);
        by_file.extend(subjects.iter().map(|(prefix, _, file)| (*file, *prefix)));
        by_file.sort_unstable();
        for task in tasks.iter() {
            let at = by_file.partition_point(|(file, _)| *file < task.planned.file);
            let job = by_file.get(at).filter(|(file, _)| *file == task.planned.file).and_then(|(_, prefix)| jobs.binary_search_by_key(prefix, |job| job.prefix).ok());
            match job {
                Some(at) if jobs[at].whole => jobs[at].tasks.push(*task),
                // Its member's names cell, each scope crate's that had records, and the one the
                // scope was cut at, read ABSENT: its task that comes, or its fix, moves the row.
                Some(at) => refuse(task, "a crate of its crate's check scope has no names record this command (its `Names` task failed)", &jobs[at].cells, buffer),
                None => refuse(task, "its file is in no member crate's items record (engine_heads.rs `run_items`)", &no_member, buffer),
            }
        }
        jobs.retain(|job| !job.tasks.is_empty());
        // 2. Each member's heads, at once, on the workers.
        let procs = names.names.as_ref().and_then(|names| names.procs());
        let root = handles.root;
        let clock = handles.clock;
        let subjects = &subjects[..];
        type Ran = Result<Result<std::vec::Vec<Result<WorkedHead, HeadFailed>, Global>, &'static str>, work::Panicked>;
        let mut worked: std::vec::Vec<(Ran, u64), Global> = std::vec::Vec::with_capacity_in(jobs.len(), Global);
        {
            let scope = cx.scope();
            scope.narrow_workers(handles.workers, Class::Foreground);
            let mut running = std::vec::Vec::with_capacity_in(jobs.len(), Global);
            for job in jobs.iter() {
                let clock = clock.clone();
                running.push(scope.may_overlap("engine heads", HEADS_FRAME, move |cx: &Cx| {
                    let began = clock.now();
                    let made = work::unwinding(|| heads_of(cx, root, packs, corrected, procs, job, subjects, view, heap));
                    Ok::<_, Infallible>((made, clock.now().since(began).map_or(0, crate::engine_times::ns)))
                }));
            }
            let mut stopped = None;
            for handle in running.into_iter() {
                match handle.join(cx) {
                    Ok(one) => worked.push(one),
                    Err(error) => {
                        stopped.get_or_insert(error);
                    }
                }
            }
            scope.join(cx).map_err(CliError::Pool)?;
            if let Some(error) = stopped {
                return Err(CliError::Pool(error));
            }
        }
        // 3. Each result as the engine's row, in task order.
        for (job, (ran, ns)) in jobs.iter().zip(worked) {
            let each = u64::try_from(job.tasks.len()).unwrap_or(1).max(1);
            for task in job.tasks.iter() {
                times.push(task.planned.name, ns / each);
            }
            let made = match ran {
                Ok(Ok(made)) => made,
                // The scope's records not opened (`heads_of`): a function of the graph's cell
                // (the packs) and each scope crate's names cell.
                Ok(Err(why)) => {
                    for task in job.tasks.iter() {
                        refuse(task, why, &job.cells, buffer);
                    }
                    continue;
                }
                // A panic is not a function of reads: owed, entered again each request (rule 20).
                Err(work::Panicked) => {
                    for task in job.tasks.iter() {
                        refuse(task, "its crate's head tasks panicked; that crate's heads alone failed", &[], buffer);
                    }
                    continue;
                }
            };
            for (task, one) in job.tasks.iter().zip(made) {
                let one = match one {
                    Ok(one) => one,
                    // What its freeze read until it failed, and its member's names cell: its
                    // heads' rows are that crate's records' (`heads_of_crate`), so a refusal
                    // that read nothing else (two heads under one cell) stands on it too.
                    Err(mut failed) => {
                        failed.reads.push(job.cells[1]);
                        refuse(task, failed.why, &failed.reads, buffer);
                        continue;
                    }
                };
                let mut defines: SVec<'a, (Cell, ContentId)> = SVec::with_capacity_in(one.defines.len(), alloc);
                defines.extend(one.defines.iter().copied());
                defines.sort_unstable_by_key(|(cell, _)| *cell);
                defines.dedup_by_key(|(cell, _)| *cell);
                let mut reads: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(one.reads.len(), alloc);
                reads.extend(one.reads.iter().map(|id| (*id, view.cell(id))));
                reads.sort_unstable_by_key(|(id, _)| *id);
                reads.dedup_by_key(|(id, _)| *id);
                let mut record_reads: std::vec::Vec<(Name, ContentId), Global> = std::vec::Vec::with_capacity_in(reads.len(), Global);
                record_reads.extend(reads.iter().map(|(id, digest)| (id.of, *digest)));
                record_reads.dedup_by_key(|(of, _)| *of);
                let name = task.planned.name;
                buffer.done.push(Done { name, key: task.key, out: one.out, defines, reads, findings: 0, failed: false });
                self.made.push(Made { name, out: one.out, reads: record_reads, payload: one.payload });
            }
        }
        self.made.sort_unstable_by_key(|one| one.name);
        Ok(())
    }
}

/// One member crate's head tasks on a worker: its scope's records opened in the task's frame,
/// each task's heads frozen (front2 tables/head.rs `head_task`), each over a source of its own
/// so its read list is its own: every names cell its freeze consulted (the recorder), and for
/// each head its freeze read, the `HEADS` cell of that head's file (an earlier phase's, so no
/// head task reads a cell of its own phase; engine.rs invariant 2), or its crate's names record.
#[expect(clippy::too_many_arguments, reason = "a worker's edge: each argument one handle of it")]
fn heads_of(
    cx: &Cx,
    root: &bstd::fs::Dir<bstd::fs::ReadWrite>,
    packs: &'static [blaze_fetch::PackView<'static>],
    corrected: &'static [(u32, &'static str, &'static str)],
    procs: Option<&'static crate::proc_runner::ProcRunner<'static>>,
    job: &ItemJob<'_>,
    subjects: &Subjects,
    view: View<'_, '_>,
    heap: &'static blaze_own::Heap,
) -> Result<std::vec::Vec<Result<WorkedHead, HeadFailed>, Global>, &'static str> {
    let frame = cx.frame();
    let g = heap.alloc();
    let opened = crate::run_names::names_records(cx, root, &job.crates, packs, corrected, procs, std::vec::Vec::new_in(Global), None, frame, g).map_err(|_| "its crate's records could not be opened (run_names.rs `names_records`)")?;
    let records = opened.records;
    let mut lang: Vec<usize, Alloc<'static>> = Vec::with_capacity_in(job.lang.len(), g);
    lang.extend_from_slice(&job.lang);
    let lang: &'static [usize] = lang.leak();
    let mut sized = None;
    for at in lang.iter() {
        NamesSource::each_lang(records, *at, &mut |name, key| {
            if name == "sized" {
                sized = Some(key);
            }
        });
    }
    let unused = Recorder::new(RecorderOpts { alloc: g });
    let base = Tables::new(TablesOpts { records, impls: None, lang_crates: lang, reads: &unused });
    // (entry, its cell, head name, want) of the crate at 0, sorted.
    let mut rows: std::vec::Vec<(Name, Cell, Name, Want), Global> = std::vec::Vec::new_in(Global);
    heads_of_crate(records, 0, job.prefix, subjects, g, &mut |one| rows.push((one.entry, one.cell, one.sig, one.want)));
    rows.sort_unstable_by_key(|(entry, cell, sig, _)| (*entry, *cell, *sig));
    rows.dedup_by_key(|(entry, cell, sig, _)| (*entry, *cell, *sig));
    let mut owners = Owners::new(subjects);
    let tool = blaze_drive::key::Tool(ContentId::of(HEAD_TOOL_DOMAIN, b""));
    let mut done = std::vec::Vec::with_capacity_in(job.tasks.len(), Global);
    for task in job.tasks.iter() {
        let name = task.planned.name;
        let from = rows.partition_point(|(entry, _, _, _)| *entry < name);
        let mut out = blaze_ids::Hasher::new(HEAD_OUT_DOMAIN);
        let mut worked = WorkedHead { out: ContentId([0; 32]), defines: std::vec::Vec::new_in(Global), reads: std::vec::Vec::new_in(Global), payload: std::vec::Vec::new_in(Global) };
        let mut failed: Option<&'static str> = None;
        let mine = &rows[from..from + rows[from..].iter().take_while(|(entry, _, _, _)| *entry == name).count()];
        // Two heads under one cell would be two values under one key (rule 21).
        if mine.windows(2).any(|two| two[0].1 == two[1].1) {
            failed = Some("two heads of its entry fall under one cell: the same kind and name (engine_heads.rs `head_cell`)");
        }
        let mine = if failed.is_some() { &mine[..0] } else { mine };
        for (_, cell, _, want) in mine.iter() {
            let recorder = Recorder::new(RecorderOpts { alloc: g });
            let names = base.reading_into(&recorder);
            let mut source = SeededHeads::new(SeededHeadsOpts { names, sized, frame, scratch: g, alloc: g, consts: None });
            let made = head_task(names, &mut source, *want, tool, g);
            // Every head its freeze read, through the heads they read in turn: a head's bytes
            // can hold what a head it read was made from (an alias's target), so each is read.
            let mut seen: std::vec::Vec<Want, Global> = std::vec::Vec::new_in(Global);
            let mut next: std::vec::Vec<Want, Global> = std::vec::Vec::new_in(Global);
            next.push(*want);
            while let Some(w) = next.pop() {
                if seen.contains(&w) {
                    continue;
                }
                if seen.len() >= MOST_HEAD_READS {
                    failed.get_or_insert("its head's freeze read more heads than a head task follows (engine_heads.rs `MOST_HEAD_READS`)");
                    break;
                }
                seen.push(w);
                if let Ok((_, read)) = source.cell(w) {
                    next.extend(read.heads.iter().copied());
                }
            }
            drop(source);
            for w in seen.iter().filter(|w| **w != *want) {
                let id = head_cell_id(*w);
                worked.reads.push(match owners.place(records, &id, g) {
                    Some(HeadAt::Cell { file, .. }) => CellId { of: file, cell: crate::engine_tasks::HEADS },
                    Some(HeadAt::Dependency(scope) | HeadAt::Unplaced(scope)) => CellId { of: scope, cell: WHOLE },
                    None => id,
                });
            }
            match recorder.finish() {
                Ok(read) => {
                    for one in read.iter() {
                        match cell_id(base, *one) {
                            Ok(Some(id)) => worked.reads.push(names_read(view, id)),
                            Ok(None) => {}
                            Err(_) => {
                                failed.get_or_insert("a names read of its head names a crate its scope does not hold (front2 tables/task.rs `cell_id`)");
                            }
                        }
                    }
                }
                Err(_) => {
                    failed.get_or_insert("its head read more names cells than the recorder holds (front2 tables/recorder.rs)");
                }
            }
            if let Some(error) = &made.failed {
                let label = NamesSource::name_of(records, want.key()).unwrap_or("?");
                let why = bstd::mem::format_in!(g, "the head of `{label}` could not be frozen: {error} (front2 tables/head.rs `head_task`)");
                failed.get_or_insert(core::str::from_utf8(why.into_bytes().leak()).unwrap_or("a head could not be frozen"));
                continue;
            }
            let cell = *cell;
            worked.defines.push((cell, made.out));
            out.update(&cell.0).update(&made.out.0);
            worked.payload.extend_from_slice(&want_bytes(*want));
            worked.payload.extend_from_slice(&u32::try_from(made.bytes.len()).unwrap_or(u32::MAX).to_le_bytes());
            worked.payload.extend_from_slice(&made.bytes);
        }
        done.push(match failed {
            Some(why) => Err(HeadFailed { why, reads: worked.reads }),
            None => {
                worked.out = out.finish();
                Ok(worked)
            }
        });
    }
    Ok(done)
}

/// The most heads one head's freeze is followed through ([`heads_of`], rule 18): past it the
/// task fails by name.
const MOST_HEAD_READS: usize = 1 << 12;
