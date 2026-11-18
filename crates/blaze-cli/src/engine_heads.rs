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
//!   text); at cell grain (engine_scope_reads.rs `ScopeReads::TABLES_AT_CELLS`) each names read
//!   is its cell instead and each header read the cell of the head task covering it;
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
//! that head's head task's cell (a same-stage read, entered after its definer; before lane BH1
//! its file's `HEADS` cell, which every sibling head added moved)
//! or, for a dependency's head, its crate's names record whole. A body's
//! read of a member's head is the head's cell ([`Owners::place`], engine_bodies.rs `widen`). A
//! head that cannot be frozen fails ALONE (A8): the task defines the heads it froze, and the
//! failed head's cell is defined by its reason's digest ([`HEAD_FAILED_DOMAIN`]), so a reader
//! moves when the reason does; the row counts one finding a failed head, and its reason is a
//! head note (engine_head_notes.rs) a body that read it quotes. A task that cannot run its heads
//! at all (its crate's records, a cap) is a failed row, named by its file. A dependency's head is
//! read as its crate's names record whole (engine.md point 3). The freeze has a constant
//! evaluator, and a head that evaluated reads what the evaluation read (engine_head_consts.rs).
//!
//! ONE FREEZE A UNIT (lane RU1): a member crate is checked as several units (its lib, its test
//! harness, a bin that `mod`s its files), each with its own crate key, and an item's key holds
//! its unit's prefix. A head task (one an entry, by text) freezes its heads once for EACH member
//! unit whose items record lists its file, under that unit's records, and defines one cell a
//! (unit, head) ([`head_cell`] holds the unit); its payload is each unit's runs, its reads the
//! union of each unit's, and a unit that stops holding an item (a `cfg`) drops its cell. Which
//! units hold the file is read too (lane RU2): each unit whose module walk reaches the file but
//! whose items record does not list it is read by its not-held cell (engine_body_plan.rs
//! `not_held_cell`, the one home the body plan reads), so a `cfg` taken off a `mod` enters the
//! task again and the newly holding unit gets its cell (rule 21). A
//! reader finds the cell by its own key ([`Owners::place`]), so it is never handed bytes frozen
//! under another unit's keys. The profile counts it: "heads frozen: N for M items in U units
//! (K items held by 2+ units)" ([`UnitCounts`]).
//! Within each unit the pool jobs take consecutive whole files in (file, name) order.
//! [`file_splits`] balances their task counts, and [`in_split_order`] restores that order
//! after the jobs finish. The "head jobs:" line counts files crossing a job boundary.
//!
//! WRITTEN HEADS (lane RH4): a head a macro wrote at module level (a derive's impl, a
//! `macro_rules!` call's struct, impl and fn, an attribute's output, a call nested in another's
//! output, an impl in a body's block that the output holds: serde's `const _: () = { impl .. }`)
//! is the head task's of the file entry its outermost call stands at, as a written head of that
//! entry ([`heads_of_crate`]: the walk up its calls and blocks, body_types/covered.rs
//! `written_path`, reads the crate's expansions directory alone). Its cell is [`head_cell`]'s
//! with where under the entry it was written ([`Written`]: the steps down, item-relative, rule
//! 71), so two derives at one struct and two calls of one macro are two cells; a file item's
//! cell is unchanged. The task's reads add, for each call under its entry, that call's
//! expansion record cell (blaze-scope names_cells.rs `NameCell::Expansion`: the record's key,
//! its text and column), and the freeze's own reads hold the written items' `Item` cells, whose
//! digests cover their text; an absent record refuses the task by name, the macro never run
//! again. A written head is held by the units that hold its entry's file, as a file item is.
//! A head whose walk fails or that no entry covers is [`HeadAt::Unlisted`]: its reader is
//! refused by name, never left to freeze it (rule 3).

use core::convert::Infallible;
use core::num::NonZeroU32;
use core::sync::atomic::{AtomicU32, Ordering};

use blaze_drive::cells::Cell;
use blaze_drive::{Input, Key, Kind, Locate, Name};
use blaze_engine::{Buffer, CellId, Done, Phase, Planned, SVec, Stage, Task, View};
use blaze_ids::ContentId;
use blaze_ladder::front2::buckets::{ImplShelf, framed, impl_tables_name, impl_tables_task};
use blaze_ladder::front2::by_origin;
use blaze_ladder::front2::heads::Want;
use blaze_ladder::front2::keys::{MemberKind, checker_of, member_key_of};
use blaze_ladder::front2::source::{NamesSource, Records};
use blaze_ladder::front2::tables::head::{ConstValues, SeededHeads, SeededHeadsOpts, cell_id as head_cell_id, want_bytes};
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
use crate::names::expansion_texts::ExpansionTexts;
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
        // A crate with no check scope (refused by the list, or a dependency's proc macro kept for
        // its macros only) has no tables task: it could only fail, and no body asks a proc
        // macro's or a refused crate's impls (blaze-world check_scope.rs `has_check_scope`).
        if !blaze_world::check_scope::has_check_scope(world.world(), at) {
            continue;
        }
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
    /// At cell grain: each header's head read where `Owners::place` put it (a head task's cell,
    /// or a dependency's record whole), read as is ([`HeadsStage::run_phase`]).
    heads: std::vec::Vec<CellId, Global>,
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
    /// Each task refused, and each head a task could not freeze: which and why (rule 19).
    pub(crate) refused: std::vec::Vec<crate::engine_head_notes::Refused, Global>,
    /// Crates whose standing tables were read back from the store this request.
    pub(crate) read_back: u32,
    /// Under which units a head task freezes (the command's: [`HeadUnits::Each`]).
    pub(crate) freeze: HeadUnits,
    /// What the head tasks entered this request froze, by unit.
    pub(crate) units: UnitCounts,
    /// How the `ImplBucket` tasks' reads are kept (engine_scope_reads.rs
    /// `ScopeReads::TABLES_AT_CELLS`), the command's choice, as the bodies'.
    scope_reads: crate::engine_scope_reads::ScopeReadsAs,
    /// The request's frozen heads, each cell once (engine_heads_once.rs), made at the first
    /// phase that runs head tasks and kept across its waves.
    pub(crate) once: Option<crate::engine_heads_once::HeadsOnce>,
    /// How a crate's head tasks are cut into pool jobs and its listing shared (the command's:
    /// [`HeadJobs::RULE`]; a test's plant otherwise).
    pub(crate) jobs: HeadJobs,
    /// The request's crate listings ([`Listings`]), each crate's once: made at the first phase
    /// that asks (a Heads phase, or a Bodies phase through [`HeadsStage::listings`]) and kept to
    /// the request's end, for every Heads job and every Bodies stretch.
    listings: Option<Listings>,
}

impl HeadsStage {
    pub(crate) fn new(scope_reads: crate::engine_scope_reads::ScopeReadsAs) -> HeadsStage {
        HeadsStage { made: std::vec::Vec::new_in(Global), refused: std::vec::Vec::new_in(Global), read_back: 0, freeze: HeadUnits::Each, units: UnitCounts::default(), scope_reads, once: None, jobs: HeadJobs::RULE, listings: None }
    }

    /// The request's crate listings over its heap (`heap`: the request's, the one the Heads
    /// phases are handed, `Handles::env.heap`), made on first ask: what the Bodies stage's
    /// stretches place their head reads by (engine_bodies.rs `run_stretch`, engine_seeds.rs
    /// `StretchHeads`), the same table the head tasks listed their crates into.
    pub(crate) fn listings(&mut self, heap: &'static blaze_own::Heap) -> &Listings {
        let plant = self.jobs.listings;
        self.listings.get_or_insert_with(|| Listings::new(heap, plant))
    }

    /// The head tasks' counts with the listings' as they stand now (the Bodies stretches ask the
    /// table after the Heads phases ran, so read at the profile, not at a phase's end).
    pub(crate) fn unit_counts(&self) -> UnitCounts {
        let mut counts = self.units;
        if let Some(listings) = &self.listings {
            counts.listings = listings.counts();
        }
        counts
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
        // Stopped while the head tasks ran (engine_status.rs): the crates' tables are not begun,
        // each refused and owed (no read), and the driver stops before the merge.
        if handles.board.stopping().is_some() {
            handles.board.refused(Stage::Heads, tasks.len() as u64);
            buffer.done.extend(tasks.iter().map(|task| crate::engine_tasks::failed(task, alloc)));
            return Ok(());
        }
        let heap = handles.env.heap;
        let at_cells = self.scope_reads.tables_at_cells();
        // The request's crate listings: a header's head read is placed by them (`Owners`).
        let listings_plant = self.jobs.listings;
        let listings: &Listings = self.listings.get_or_insert_with(|| Listings::new(heap, listings_plant));
        let refused = &mut self.refused;
        // A refusal with the cells it is a function of, each read as the view holds it today
        // (ABSENT where no task defines it: a crate whose names task failed or did not run), so
        // its row stands until one moves (plan row A8). None: owed, entered again each request,
        // said at each such call.
        let mut refuse = |task: &Task, why: &'static str, cells: &[CellId], buffer: &mut Buffer<'a>| {
            let mut reads: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(cells.len(), alloc);
            reads.extend(cells.iter().map(|id| (*id, view.cell(id))));
            buffer.done.push(crate::engine_tasks::failed_with(task, reads, alloc));
            refused.push(crate::engine_head_notes::Refused::whole(task.planned.name, "its crate's impl tables", why));
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
                Ok((scope, true)) => {
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
                // The cut's cause printed, typed, the crate named (rule 19).
                Ok((scope, false)) => {
                    let why = match scope.cut_cause(&list) {
                        Some(cause) => crate::engine_head_notes::leak(heap.alloc(), format_args!("its crate's {cause}")),
                        None => "its crate's check scope was cut and `scope_of` kept no cut (engine_bodies.rs `scope_of`)",
                    };
                    refuse(task, why, &scope_cells(graph.name, &list, at, Some(&scope)), buffer);
                }
                // `scope_of` reads the crate's own names record (ABSENT when its task failed)
                // and the check scope the graph makes: both cells. Its cause printed, typed.
                Err(cause) => refuse(task, crate::engine_head_notes::leak(heap.alloc(), format_args!("its crate's check scope was not read: {cause}")), &scope_cells(graph.name, &list, at, None), buffer),
            }
        }
        // At cell grain, the members' files a header's head read is placed by (`Owners::place`).
        let subjects = match at_cells && !jobs.is_empty() {
            true => member_subjects(read, &cx, store, names, &list, handles.outlined, heap),
            false => std::vec::Vec::new_in(Global),
        };
        let subjects: Option<&Subjects> = at_cells.then_some(&subjects[..]);
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
                    let made = work::unwinding(|| tables_of(cx, root, packs, corrected, procs, job, subjects, heap, listings));
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
                    // A names cell kept by `tables_of` (at cell grain) as its crate defines it.
                    let id = names_read(view, *id);
                    reads.push((id, view.cell(&id)));
                }
            }
            // Each header's head placed at its head task's cell (at cell grain, `tables_of`): a
            // head task of this phase, entered before these tables (`run_items`), is read at the
            // digest its row holds now, so the merge sends this task to no second wave for a
            // head it saw (as a head's read of a sibling head, `job_cells`).
            for id in one.heads.iter() {
                let made = buffer.done.iter().find(|done| done.name == id.of).and_then(|done| done.defines.binary_search_by_key(&id.cell, |(cell, _)| *cell).ok().map(|at| done.defines[at].1));
                reads.push((*id, made.unwrap_or_else(|| view.cell(id))));
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
/// `subjects`: `Some` at cell grain (engine_scope_reads.rs `ScopeReads::TABLES_AT_CELLS`): its
/// names reads kept as cells and each header's head read placed (`Owners::place`); `None`: each
/// names read its crate's record whole, each header read left for the coordinator to widen.
#[expect(clippy::too_many_arguments, reason = "a worker's edge: each argument one handle of it")]
fn tables_of(
    cx: &Cx,
    root: &bstd::fs::Dir<bstd::fs::ReadWrite>,
    packs: &'static [blaze_fetch::PackView<'static>],
    corrected: &'static [(u32, &'static str, &'static str)],
    procs: Option<&'static crate::proc_runner::ProcRunner<'static>>,
    job: &Job<'_>,
    subjects: Option<&Subjects>,
    heap: &'static blaze_own::Heap,
    listings: &Listings,
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
    let mut heads: std::vec::Vec<CellId, Global> = std::vec::Vec::new_in(Global);
    let mut owners = subjects.map(|subjects| Owners::new(subjects, &job.crates[..], listings, g));
    for read in done.reads.iter() {
        match cell_id(view, *read) {
            // A names read: at cell grain its cell (the coordinator keeps it where its crate
            // defines a cell table, `names_read`); else its crate's names record whole, the cell
            // the Names stage defines.
            Ok(Some(id)) if id.of.kind == Kind::CrateScope => reads.push(match subjects {
                Some(_) => id,
                None => CellId { of: id.of, cell: WHOLE },
            }),
            // A header's head read: at cell grain placed as a body's is; a head of no crate the
            // records hold (or `Whole`) is left for the coordinator, which widens it.
            Ok(Some(id)) if id.of.kind == Kind::Signature => match owners.as_mut().and_then(|owners| owners.place(records, &id, g)) {
                Some(HeadAt::Cell { cell, .. }) => heads.push(cell),
                // A body's own item or a head no task holds: its crate's record whole (a header
                // reading the latter is refused at its body, `HeadAt::Unlisted`).
                Some(HeadAt::Dependency(scope) | HeadAt::OfTheBody(scope) | HeadAt::Unlisted { scope, .. }) => heads.push(CellId { of: scope, cell: WHOLE }),
                None => reads.push(id),
            },
            Ok(Some(id)) => reads.push(id),
            Ok(None) => {}
            Err(_) => return Err("a read of its filing names a crate its scope does not hold (front2 tables/task.rs `cell_id`)"),
        }
    }
    let mut defines = std::vec::Vec::with_capacity_in(done.defines.len(), Global);
    defines.extend(done.defines.iter().copied());
    Ok(Worked { out: done.out, defines, reads, heads, payload: framed(tables, Global) })
}

// ---------------------------------------------------------------------------------------------
// The head tasks: one an outline entry that has a head (module docs, "THE HEAD TASKS").

/// Each member crate's files as its items record lists them, matched to the planner's outlines:
/// (its crate key's prefix, the record's file index, the outline's `FileItems` name), sorted.
pub(crate) type Subjects = [([u8; 8], u32, Name)];

/// The domain of a head cell ([`head_cell`]). 3: the cell holds its unit's crate prefix.
const HEAD_CELL_DOMAIN: &str = "blaze-cli engine head cell 3";
/// The domain of a head's item, of no unit ([`head_item`]).
const HEAD_ITEM_DOMAIN: &str = "blaze-cli engine head item 1";
/// The domain of a head task's source ([`plan_items`]). 2: its payload holds each run's cell
/// ([`WorkedHead::payload`]), so no record of the old form is read by its key. 3: one run a
/// (unit, cell), each frozen under its unit's records ([`HeadsStage::run_items`]).
/// 4: the signature part selects its body's contribution while outlining.
/// 5: a frozen fn record identifies whether its digest covers the signature or the whole item.
const HEAD_SOURCE_DOMAIN: &str = "blaze-cli engine head source 5";
/// The domain of one unit's freeze of a head task: its cells and their digests, in order.
const HEAD_OUT_DOMAIN: &str = "blaze-cli engine head entry out 1";
/// The domain of a head task's out: each unit's freeze's out, in unit order.
const HEAD_UNITS_OUT_DOMAIN: &str = "blaze-cli engine head entry units out 1";

/// The out of a head task whose file no unit holds ([`HeadsStage::run_items`]): its name's alone.
const HEAD_NO_UNIT_DOMAIN: &str = "blaze-cli engine head no unit's 1";

/// The out of the head task `name` when no unit holds its file: a function of its name alone,
/// so the heads table knows such a row without a record (engine_seeds.rs `RequestTable::build`).
pub(crate) fn no_units_out(name: Name) -> ContentId {
    ContentId::of(HEAD_NO_UNIT_DOMAIN, &name.subject.0)
}
/// The tool `head_task` folds into a key the engine does not use (the engine keys the task by
/// its name and source, `Tools::key`).
const HEAD_TOOL_DOMAIN: &str = "blaze-cli engine head task";

/// The cell of a head in the task of the outline entry that covers it: by what the entry's text
/// names it (the want's kind, the item's name, an impl fn's name), never by its checker key. A
/// crate's item keys are of the unit that holds it: `b`'s view of `a` (a dependency's unit) keys
/// `a::one` apart from `a`'s own check scope, where its head task runs, so a cell by key was one
/// no task defines (lane EY, the callers test). `part`: `WHOLE`, the whole head, the only part
/// today, hashed in so a head can be split later (engine.md "ASPECTS"). Two heads of one entry
/// under one cell are refused, both named ([`heads_of`]). The cell is injective over one
/// entry's heads because an entry is ONE item of its text: its own head, or an impl fn's, or
/// an `extern` block's foreign items (one namespace's names, distinct). That rests on each
/// entry having a task name of its own (signature.rs `signature_name`, lane RU3: before it, two
/// unnamed impls of one file were one entry, their two headers one cell).
///
/// ONE CELL A UNIT (lane RU1): a member's item is held by each unit whose records list its file
/// (the lib, the test harness, a bin that `mod`s it), and its keys there hold that unit's crate
/// prefix, so a head frozen under one unit's records names that unit's keys and cannot be read
/// through another's names. The cell holds the unit: its crate prefix, the first 8 bytes of
/// the want's key (front2 tables/head.rs `want_bytes`, bytes 1 to 8; front2 keys.rs
/// `checker_of`), which a head task's freeze under a unit and a reader in that unit both take
/// from their own records' want, so a reader's want places the cell of ITS unit, by
/// construction. Nothing of the machine (rule 25): a crate prefix is a hash of the crate's text.
pub(crate) fn head_cell(want: Want, item: &str, member: Option<&str>, part: Cell) -> Cell {
    head_cell_written(want, item, member, None, part)
}

/// [`head_cell`] of a head a macro (or a body's block) wrote under its entry: the cell holds
/// where it was written ([`Written`]), so the heads of two derives at one struct, of two calls of
/// one macro in an impl block or of two items of one expansion are two cells (rule 21). A file
/// item's (`written: None`) is [`head_cell`]'s, unchanged: the written part is hashed only when
/// there is one, after the fixed-size part, so no file item's cell moved and no domain is bumped.
fn head_cell_written(want: Want, item: &str, member: Option<&str>, written: Option<Written>, part: Cell) -> Cell {
    let bytes = want_bytes(want);
    let mut h = blaze_ids::Hasher::new(HEAD_CELL_DOMAIN);
    h.update(&bytes[1..9]).update(&head_item(want, item, member, written, part).0);
    let digest = h.finish();
    let mut cell = [0u8; 16];
    cell.copy_from_slice(&digest.0[..16]);
    Cell(cell)
}

/// Where under its covering entry a macro (or a body's block) wrote a head: the steps down from
/// the outermost call (body_types/covered.rs `WrittenPath::steps`) and the outermost call's
/// entry relative to the covering entry (0: the entry itself, a derive or an attribute on it, or
/// the entry is the call; else its place among the entry's descendants, a call in an impl's
/// block). Item-relative (rule 71): nothing of the file outside the entry.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Written {
    steps: ContentId,
    from_entry: u32,
}

/// A head's item as an entry's text names it, of no unit: the want's kind, the item's name, an
/// impl fn's name, the part, and where a macro wrote it ([`Written`]). Two units' cells of one
/// item ([`head_cell`]) share it; the profile counts the items held by two units and more by it
/// ([`UnitCounts`]).
fn head_item(want: Want, item: &str, member: Option<&str>, written: Option<Written>, part: Cell) -> Cell {
    let mut h = blaze_ids::Hasher::new(HEAD_ITEM_DOMAIN);
    h.update(&want_bytes(want)[..1]).part(item.as_bytes()).opt_part(member).update(&part.0);
    if let Some(written) = written {
        h.update(&written.steps.0).update(&written.from_entry.to_le_bytes());
    }
    let digest = h.finish();
    let mut cell = [0u8; 16];
    cell.copy_from_slice(&digest.0[..16]);
    Cell(cell)
}

/// Under which units a head task freezes its heads: the rule, or a test's plant (rule 23:
/// the plant removes the cause and the tests watch the effect come back).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HeadUnits {
    /// Once for each member unit whose records hold its file (the rule, [`head_cell`]).
    Each,
    /// THE PLANT: under the first holding unit by crate prefix only (the form before lane RU1).
    #[cfg_attr(not(test), expect(dead_code, reason = "the plant: only the tests take it (rule 23)"))]
    OnlyFirst,
    /// THE PLANT: under the last holding unit by crate prefix only.
    #[cfg_attr(not(test), expect(dead_code, reason = "the plant: only the tests take it (rule 23)"))]
    OnlyLast,
    /// THE PLANT of a written head's reads: under each unit as [`HeadUnits::Each`], and no
    /// expansion record's cell read (`heads_of`'s `CrateRow::Call` reads dropped), so a
    /// `macro_rules!` body edited reruns no head task of its calls and the bodies read stale
    /// heads (tests/engine_expansion_heads_tests.rs: the oracle, a whole check, must catch it).
    #[cfg_attr(not(test), expect(dead_code, reason = "the plant: only the tests take it (rule 23)"))]
    NoExpansionReads,
}

/// What the head tasks froze this request, by unit (the profile's "heads frozen" line, rule 53).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct UnitCounts {
    /// Cells defined: one a (unit, head), a head that failed too.
    pub(crate) frozen: u32,
    /// Heads by item, of no unit ([`head_item`]): each head task's items, summed.
    pub(crate) items: u32,
    /// Distinct units that froze a head.
    pub(crate) units: u32,
    /// Items frozen under two units or more (each a freeze a head form relative to "this crate"
    /// would share).
    pub(crate) shared: u32,
    /// Head tasks given to split jobs (one a (task, unit): a task two units hold, twice),
    /// summed over the phases.
    pub(crate) tasks: u32,
    /// Crate jobs (one a unit with head tasks: its setup), summed over the phases.
    pub(crate) crate_jobs: u32,
    /// Split jobs (each a run of one crate's tasks, [`split_count`]), summed over the phases.
    pub(crate) split_jobs: u32,
    /// Split jobs that ran a task, summed over the phases.
    pub(crate) busy: u32,
    /// The floor of `busy`: min(workers, head tasks to run), summed over the phases.
    pub(crate) busy_floor: u32,
    /// The most split jobs one crate's tasks were cut into, in any phase.
    pub(crate) most_splits: u32,
    /// Files in multiple splits, counted once per file within each crate job and phase.
    pub(crate) split_files: u32,
    /// The request's crate listings ([`Listings`]).
    pub(crate) listings: ListingCounts,
}

/// The profile's lines of the head tasks' freezes by unit, their jobs and their listings.
pub(crate) fn unit_profile(counts: &UnitCounts, into: &mut blaze_own::String<&Frame>) {
    use core::fmt::Write as _;
    let _ = writeln!(into, "heads frozen: {} for {} items in {} units ({} items held by 2+ units)", counts.frozen, counts.items, counts.units, counts.shared);
    let _ = writeln!(
        into,
        "head jobs: {} tasks in {} split jobs over {} crate jobs, {} busy (floor min(workers, tasks) = {}: {}), at most {} splits in one crate, {} files in more than one split (floor 0)",
        counts.tasks,
        counts.split_jobs,
        counts.crate_jobs,
        counts.busy,
        counts.busy_floor,
        Hundredths::of(counts.busy, counts.busy_floor),
        counts.most_splits,
        counts.split_files
    );
    let l = &counts.listings;
    let _ = writeln!(
        into,
        "head listings: {} made for {} crates (floor 1 a crate: {}), {} twice by a race, {} unequal, {} refused full, {} asks",
        l.made,
        l.crates,
        Hundredths::of(l.made, l.crates),
        l.twice,
        l.unequal,
        l.full,
        l.asks
    );
}

/// A ratio printed to two places, `n/a` over nothing (rule 67: the ratio to the floor).
struct Hundredths(Option<u64>);

impl Hundredths {
    fn of(top: u32, floor: u32) -> Hundredths {
        Hundredths((floor > 0).then(|| u64::from(top) * 100 / u64::from(floor)))
    }
}

impl core::fmt::Display for Hundredths {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Some(at) => write!(f, "ratio {}.{:02}", at / 100, at % 100),
            None => write!(f, "ratio n/a"),
        }
    }
}

/// How a crate's head tasks are cut into pool jobs ([`split_count`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeadSplit {
    /// The rule: a crate's share of the phase's jobs by its share of the tasks.
    Rule,
    /// Request this many jobs using the same whole-file rule as the command.
    #[cfg_attr(not(test), expect(dead_code, reason = "a test's knob"))]
    Jobs(NonZeroU32),
    /// Control for file ownership. Cut at evenly spaced task indexes.
    #[cfg(test)]
    Even(NonZeroU32),
}

/// How the split jobs' results are merged ([`in_split_order`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MergePlant {
    /// The rule: each in its split's place, whatever finished first.
    Rule,
    /// As they arrived: the control of the merge's test (rule 23).
    #[cfg_attr(not(test), expect(dead_code, reason = "a test's plant (rule 23)"))]
    Arrival,
}

/// The Heads stage's jobs: how a crate's tasks are split, and the listings' and the merge's
/// plants (the command's: [`HeadJobs::RULE`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HeadJobs {
    pub(crate) split: HeadSplit,
    pub(crate) listings: ListingsPlant,
    pub(crate) merge: MergePlant,
}

impl HeadJobs {
    pub(crate) const RULE: HeadJobs = HeadJobs { split: HeadSplit::Rule, listings: ListingsPlant::Rule, merge: MergePlant::Rule };
}

/// The phase's jobs a worker under the rule ([`split_count`]): enough that a crate whose tasks
/// are slow does not hold the phase's end, few enough that each job's own source (its records
/// reopened, its tables and lang index, its recorders) is small beside its tasks.
const JOBS_A_WORKER: usize = 2;

/// The requested number of jobs. A crate gets its share of `workers * JOBS_A_WORKER`
/// proportionally to its share of the phase's tasks. [`file_splits`] chooses the actual cuts.
pub(crate) fn split_count(split: HeadSplit, tasks: usize, total: usize, workers: usize) -> usize {
    if tasks == 0 {
        return 0;
    }
    match split {
        HeadSplit::Jobs(n) => usize::try_from(n.get()).unwrap_or(usize::MAX).min(tasks),
        #[cfg(test)]
        HeadSplit::Even(n) => usize::try_from(n.get()).unwrap_or(usize::MAX).min(tasks),
        HeadSplit::Rule => workers.saturating_mul(JOBS_A_WORKER).saturating_mul(tasks).div_ceil(total.max(tasks)).clamp(1, tasks),
    }
}

/// Greedily take the longest prefix of whole files within `limit`. A file larger than
/// `target` always stands alone. Each cut leaves the shortest possible suffix, so this
/// uses the fewest jobs allowed by that limit and the singleton rule.
fn greedy_file_splits(files: &[usize], target: usize, limit: usize) -> impl Iterator<Item = core::ops::Range<usize>> + '_ {
    let mut files = files.iter().copied().peekable();
    let mut end = 0;
    core::iter::from_fn(move || {
        let start = end;
        let first = files.next()?;
        end += first;
        if first <= target {
            while let Some(next) = files.next_if(|&next| next <= target && end - start + next <= limit) {
                end += next;
            }
        }
        Some(start..end)
    })
}

/// Fill toward the target, `ceil(tasks / requested)`, in sorted file order. Whole files can
/// yield fewer jobs than requested. If packing to the target needs too many jobs, find the
/// smallest larger limit that fits the budget. The largest split is at most the target plus
/// the largest file. If keeping oversized files alone requires more jobs even at that bound
/// (e.g. 1, 8, 1 tasks and two jobs), use the fewest jobs consistent with both guarantees.
pub(crate) fn file_splits(files: &[usize], tasks: usize, requested: usize) -> impl Iterator<Item = core::ops::Range<usize>> + '_ {
    let target = tasks.div_ceil(requested.max(1));
    let runs = |limit| greedy_file_splits(files, target, limit);
    let mut limit = target;
    if files.len() > requested && runs(limit).count() > requested {
        let mut upper = target.saturating_add(files.iter().copied().max().unwrap_or(0)).min(tasks);
        let budget = requested.max(runs(upper).count());
        while limit < upper {
            let mid = limit + (upper - limit) / 2;
            if runs(mid).count() <= budget {
                upper = mid;
            } else {
                limit = mid + 1;
            }
        }
    }
    runs(limit)
}

/// Why the split jobs' results could not be put in split order ([`in_split_order`]): the
/// merge's own inconsistency, named (rule 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MergeError {
    /// A split's result came twice.
    Twice { split: u32 },
    /// A split's result is past the splits made.
    Past { split: u32, splits: u32 },
    /// A split's result never came.
    Missing { split: u32 },
}

impl core::fmt::Display for MergeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MergeError::Twice { split } => write!(f, "head split job {split}'s result came twice (engine_heads.rs `in_split_order`)"),
            MergeError::Past { split, splits } => write!(f, "head split job {split}'s result is past the {splits} split jobs made (engine_heads.rs `in_split_order`)"),
            MergeError::Missing { split } => write!(f, "head split job {split}'s result never came (engine_heads.rs `in_split_order`)"),
        }
    }
}

/// The split jobs' results (`arrived`: (split, result) as each finished) in split order, which
/// is task order: what the merge reads, so no order of completion shows in a row (rule 25).
/// Each split exactly once, or a typed refusal. Under [`MergePlant::Arrival`], as they arrived.
pub(crate) fn in_split_order<'a, T>(arrived: SVec<'a, (u32, T)>, splits: usize, plant: MergePlant, alloc: SharedAlloc<'a>) -> Result<SVec<'a, T>, MergeError> {
    let count = u32::try_from(splits).unwrap_or(u32::MAX);
    let mut slots: SVec<'a, Option<T>> = SVec::with_capacity_in(splits, alloc);
    slots.extend((0..splits).map(|_| None));
    let mut order: SVec<'a, u32> = SVec::with_capacity_in(arrived.len(), alloc);
    for (split, one) in arrived.into_iter() {
        let Some(slot) = slots.get_mut(split as usize) else {
            return Err(MergeError::Past { split, splits: count });
        };
        if slot.is_some() {
            return Err(MergeError::Twice { split });
        }
        *slot = Some(one);
        order.push(split);
    }
    if let Some(split) = slots.iter().position(Option::is_none) {
        return Err(MergeError::Missing { split: u32::try_from(split).unwrap_or(u32::MAX) });
    }
    if plant == MergePlant::Arrival {
        let mut out: SVec<'a, T> = SVec::with_capacity_in(splits, alloc);
        for split in order.iter() {
            if let Some(one) = slots.get_mut(*split as usize).and_then(Option::take) {
                out.push(one);
            }
        }
        return Ok(out);
    }
    let mut out: SVec<'a, T> = SVec::with_capacity_in(splits, alloc);
    out.extend(slots.into_iter().flatten());
    Ok(out)
}

/// A fn head records its signature and whether it has a definition. A block that supplies
/// other heads is tracked through the names and expansion cells the entry reads.
pub(crate) fn outline_head_source(outline: &blaze_file_items::Outline<'_>, at: ItemIndex, text: &str) -> ContentId {
    use blaze_surface::{ItemDetail, ItemKind};

    let item = outline.item(at);
    let mut h = blaze_ids::Hasher::new(HEAD_SOURCE_DOMAIN);
    h.update(&crate::signature::signature_source(outline, at, text).0);
    if let ItemDetail::Fn { .. } = item.detail {
        h.update(&[u8::from(item.body.is_some())]);
    } else if work::checked_body(outline, item).is_some() || work::expansion_entry(outline, item, text) {
        h.update(&item.hash.0);
    }
    // A trait owns its methods' signatures, including whether each has a default.
    if item.kind() == ItemKind::Trait {
        for (_, member) in outline.children(Some(at)) {
            if let ItemDetail::Fn { .. } = member.detail {
                h.update(&[u8::from(member.body.is_some())]);
            }
        }
    }
    h.finish()
}

/// One `Signature` task an outline entry that has a head (signature.rs `is_signature_entry`), in
/// `Phase { stage: Heads, wave: 0 }`, named as the outline lists it (signature.rs
/// `signature_name`: the file and the entry's chain), so its name and key are known from text
/// before any phase runs. Its source is the persisted signature part ([`outline_head_source`])
/// and the edition.
pub(crate) fn plan_items<'h>(outlined: &[Outlined<'_>], alloc: SharedAlloc<'h>) -> SVec<'h, Planned> {
    let mut planned = SVec::new_in(alloc);
    for one in outlined.iter() {
        let Ok(record) = &one.made else { continue };
        for part in record.parts.iter() {
            let (Kind::Signature, Locate::Source(head)) = (part.name.kind, part.locate) else { continue };
            let source = crate::engine_tasks::edition_source(head, one.edition);
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
    /// A member crate's item declared in a body (its key's `INNER` bit, front2 locals.rs): no
    /// names row and no head task; the body that declares it makes its head in its own store
    /// (front2 tables/seeds.rs `of_the_body`), from its own text, which its key covers. A read
    /// of it from that body is placed as its crate's names record whole (engine_bodies.rs
    /// `widen`), as before this placing.
    OfTheBody(Name),
    /// A member crate's head that no head task holds, and why, named (rule 19): its walk up the
    /// calls that wrote it failed (a text that keeps no origins: a statement call's items), or no
    /// outline entry covers where it stands. A body that needs it is refused by these words
    /// (engine_seeds.rs `StretchHeads::known`), never frozen by the body (rule 3); a read of it
    /// is its crate's names record whole, the reads of the refusal.
    Unlisted { scope: Name, why: &'static str },
}

/// Each head of the member crates a task reads, by its item's head name: each crate's listing
/// taken from the request's table ([`Listings`]) on its first ask here, and every place after a
/// binary search in it. A reader's own table of the crates it asked of, so a place takes no
/// shared lookup; the listing itself is made once a request (the table's floor).
pub(crate) struct Owners<'s> {
    subjects: &'s Subjects,
    /// The crates the records were opened over, in the records' order (run_names.rs
    /// `names_records`: crate `k` of the records is `crates[k]`): their expansion directories,
    /// which the walk of a written head reads.
    over: &'s [Crate<'s>],
    /// The request's listings, every crate's once.
    listings: &'s Listings,
    /// The crates (by key prefix) asked of so far, sorted by prefix: each one's listing, or the
    /// words its listing was refused with ([`HeadAt::Unlisted`]).
    filled: Vec<([u8; 8], Result<&'static Listing, &'static str>), Alloc<'static>>,
}

impl<'s> Owners<'s> {
    /// `over`: the crates the readers' records are opened over, in their order; `listings`: the
    /// request's table ([`HeadsStage::listings`]).
    pub(crate) fn new(subjects: &'s Subjects, over: &'s [Crate<'s>], listings: &'s Listings, g: Alloc<'static>) -> Owners<'s> {
        Owners { subjects, over, listings, filled: Vec::new_in(g) }
    }

    /// Where the head read `id` is read from; `None`: its crate is none of `records`'. By the
    /// reader's OWN key: `id`'s crate prefix is the reader's unit's view of the item's crate,
    /// so a member's head is the cell its head task froze under THAT unit ([`head_cell`]).
    pub(crate) fn place<'n>(&mut self, records: &'n Records<'n>, id: &CellId, g: Alloc<'static>) -> Option<HeadAt>
    where
        's: 'n,
    {
        let k = NamesSource::crate_of(records, blaze_id::ItemKey(id.of.subject))?;
        let key = NamesSource::crate_key(records, k)?;
        let prefix = key.prefix();
        let scope = blaze_drive::key::crate_scope_name(&key);
        // A crate with no file among the members' outlines (a dependency) has no head tasks.
        // Judged by the files, not by the `CELLS` mark of the scope named here: by that mark,
        // `b`'s caller of `a::one` read `a` whole and missed `one`'s new head (lane EY,
        // engine_check_rechecks_the_callers_of_an_edited_signature).
        let from = self.subjects.partition_point(|(p, _, _)| *p < prefix);
        if self.subjects.get(from).is_none_or(|(p, _, _)| *p != prefix) {
            return Some(HeadAt::Dependency(scope));
        }
        // `g`: the listing's lists are the request heap's, `Listings`' own (kept for the call's
        // shape, which the head tasks and the stretches share: engine_heads_once.rs `one_head`).
        let _ = g;
        let listing = match self.filled.binary_search_by_key(&prefix, |(p, _)| *p) {
            Ok(at) => self.filled[at].1,
            Err(at) => {
                let listing = match self.over.get(k).filter(|one| one.records.key == key) {
                    Some(one) => self.listings.listing(records, &one.records.expansions, k, key, self.subjects),
                    None => Err(OVER_NOT_RECORDS),
                };
                // Linear in the crates this reader asked of (a scope's crates, once each).
                self.filled.insert_shift(at, (prefix, listing));
                listing
            }
        };
        Some(match listing {
            Ok(listing) => listing.place(id, scope),
            Err(why) => HeadAt::Unlisted { scope, why },
        })
    }
}

/// How many crate listings one request's table holds (rule 18): one a (crate, unit view) the
/// request reads heads of. Tempo's workspace has about 700 crates, a few views each; past this
/// a reader's head is refused by [`LISTINGS_FULL`], never listed outside the table (rule 3).
pub(crate) const MOST_LISTINGS: usize = 1 << 16;

/// [`HeadAt::Unlisted`]'s words for a crate whose listing the full table could not take.
const LISTINGS_FULL: &str = "the request's table of crate listings is full, so this crate's heads were not listed (engine_heads.rs `MOST_LISTINGS`)";

/// [`HeadAt::Unlisted`]'s words for a crate whose two listings, made at once by two jobs under
/// one key, were not equal: two values under one key (rule 21), the key short of an input.
const LISTINGS_UNEQUAL: &str = "two listings of this crate's heads made under one key (its crate key and its member files) were not equal, so its key misses an input (engine_heads.rs `Listings::listing`)";

/// [`HeadAt::Unlisted`]'s words for a reader whose records name a crate first that is not its
/// own unit's (run_names.rs `names_records` opens a job's own crate at 0).
const NOT_OWN_FIRST: &str = "its records' first crate is not the unit its head tasks freeze under (engine_heads.rs `crate_setup`)";

/// The domain of a listing's digest ([`Listing::digest`]): what two makes are compared by.
const LISTING_DOMAIN: &str = "blaze-cli engine heads: a crate's listing 1";

/// The domain of the digest of a crate's member files ([`ListingKey::subjects`]).
const LISTING_SUBJECTS_DOMAIN: &str = "blaze-cli engine heads: a listing's member files 1";

/// One head of a crate's listing ([`Listing::heads`]): a [`HeadRow`] whose words are the
/// request heap's, so it outlives the records it was read from.
#[derive(Clone, Copy)]
pub(crate) struct ListedHead {
    entry: Name,
    cell: Cell,
    sig: Name,
    file: Name,
    want: Want,
    item: Cell,
    label: HeadLabel<'static>,
}

/// A crate's heads, listed once a request ([`heads_of_crate`], through [`Listings::listing`]):
/// what its head tasks freeze, by entry, and where every reader places a read of one, by head
/// name. Every list is the request heap's and sorted, so it is read in place by any worker.
pub(crate) struct Listing {
    /// Each head by (entry, cell, head name), once each; equal keys keep the least by every
    /// field (rule 25: no order of the walk shows).
    heads: &'static [ListedHead],
    /// Indexes into `heads` sorted by (head name, entry, file, cell), one a head name: the
    /// least row of each, as `Owners` placed before.
    by_sig: &'static [u32],
    /// (head name, why no head task holds it), sorted, one a head name.
    unlisted: &'static [(Name, &'static str)],
    /// (entry, a call whose walk ends under it), sorted: the expansion records its task reads.
    calls: &'static [(Name, blaze_id::ItemKey)],
    /// Of every row, in order: two makes under one key are compared by it.
    digest: ContentId,
}

impl Listing {
    /// Where the head read `id` of this crate is read from (`scope`: the crate's names record).
    fn place(&self, id: &CellId, scope: Name) -> HeadAt {
        match self.by_sig.binary_search_by(|at| self.heads[*at as usize].sig.cmp(&id.of)) {
            // The only part is the whole head (`WHOLE`, front2 head.rs `cell_id`).
            Ok(at) => {
                let head = &self.heads[self.by_sig[at] as usize];
                HeadAt::Cell { cell: CellId { of: head.entry, cell: head.cell }, file: head.file }
            }
            // A body's own item: its subject is the checker key's bytes (front2 head.rs
            // `signature_name`), byte 8 its `INNER` mark.
            Err(_) if id.of.subject.0[8] & blaze_id::ItemKey::INNER != 0 => HeadAt::OfTheBody(scope),
            Err(_) => match self.unlisted.binary_search_by_key(&id.of, |(sig, _)| *sig) {
                Ok(at) => HeadAt::Unlisted { scope, why: self.unlisted[at].1 },
                Err(_) => HeadAt::Unlisted { scope, why: NO_HEAD_ROW },
            },
        }
    }

    /// The heads entry `entry`'s task freezes, by cell.
    fn of_entry(&self, entry: Name) -> &'static [ListedHead] {
        let heads = self.heads;
        let from = heads.partition_point(|one| one.entry < entry);
        let len = heads[from..].iter().take_while(|one| one.entry == entry).count();
        &heads[from..from + len]
    }

    /// The calls whose walk ends under entry `entry` (`CrateRow::Call`).
    fn calls_of(&self, entry: Name) -> &'static [(Name, blaze_id::ItemKey)] {
        let calls = self.calls;
        let from = calls.partition_point(|(one, _)| *one < entry);
        let len = calls[from..].iter().take_while(|(one, _)| *one == entry).count();
        &calls[from..from + len]
    }

    /// Whether `entry` is an entry of this crate that freezes a head.
    fn holds_entry(&self, entry: Name) -> bool {
        !self.of_entry(entry).is_empty()
    }
}

/// A name's bytes into a digest (its kind's wire byte, its subject).
fn name_part(h: &mut blaze_ids::Hasher, name: Name) {
    h.part(&[name.kind as u8]).part(&name.subject.0);
}

/// Crate `k`'s listing, made from `records` by [`heads_of_crate`] (the one walk), sorted, its
/// words copied to the request heap (`g`), its digest taken.
fn list_crate<'n>(records: &'n Records<'n>, texts: &ExpansionTexts<'n>, k: usize, prefix: [u8; 8], subjects: &Subjects, g: Alloc<'static>) -> &'static Listing {
    let text = |words: &str| -> &'static str {
        match words.is_empty() {
            true => "",
            false => crate::engine_head_notes::leak(g, format_args!("{words}")),
        }
    };
    let mut heads: Vec<ListedHead, Alloc<'static>> = Vec::new_in(g);
    let mut unlisted: Vec<(Name, &'static str), Alloc<'static>> = Vec::new_in(g);
    let mut calls: Vec<(Name, blaze_id::ItemKey), Alloc<'static>> = Vec::new_in(g);
    heads_of_crate(records, texts, k, prefix, subjects, g, &mut |one| match one {
        CrateRow::Head(one) => heads.push(ListedHead {
            entry: one.entry,
            cell: one.cell,
            sig: one.sig,
            file: one.file,
            want: one.want,
            item: one.item,
            label: HeadLabel { want: Some(one.want), name: text(one.name), member: one.member.map(text), entry_name: text(one.entry_name), by: one.by.map(|by| by.map(text)) },
        }),
        CrateRow::Unlisted { sig, why } => unlisted.push((sig, why)),
        CrateRow::Call { entry, call } => calls.push((entry, call)),
    });
    heads.sort_unstable_by(|a, b| (a.entry, a.cell, a.sig, want_bytes(a.want), a.item, a.file).cmp(&(b.entry, b.cell, b.sig, want_bytes(b.want), b.item, b.file)));
    heads.dedup_by_key(|one| (one.entry, one.cell, one.sig));
    let mut by_sig: Vec<u32, Alloc<'static>> = Vec::with_capacity_in(heads.len(), g);
    by_sig.extend((0..heads.len()).map(|at| u32::try_from(at).unwrap_or(u32::MAX)));
    by_sig.sort_unstable_by_key(|at| {
        let one = &heads[*at as usize];
        (one.sig, one.entry, one.file, one.cell)
    });
    by_sig.dedup_by_key(|at| heads[*at as usize].sig);
    unlisted.sort_unstable();
    unlisted.dedup_by_key(|(sig, _)| *sig);
    calls.sort_unstable();
    calls.dedup();
    let mut h = blaze_ids::Hasher::new(LISTING_DOMAIN);
    h.part(&u64::try_from(heads.len()).unwrap_or(u64::MAX).to_le_bytes());
    for one in heads.iter() {
        name_part(&mut h, one.entry);
        h.part(&one.cell.0);
        name_part(&mut h, one.sig);
        name_part(&mut h, one.file);
        h.part(&want_bytes(one.want)).part(&one.item.0);
        h.part(one.label.name.as_bytes()).part(one.label.member.unwrap_or("").as_bytes()).part(one.label.entry_name.as_bytes());
    }
    h.part(&u64::try_from(unlisted.len()).unwrap_or(u64::MAX).to_le_bytes());
    for (sig, why) in unlisted.iter() {
        name_part(&mut h, *sig);
        h.part(why.as_bytes());
    }
    h.part(&u64::try_from(calls.len()).unwrap_or(u64::MAX).to_le_bytes());
    for (entry, call) in calls.iter() {
        name_part(&mut h, *entry);
        h.part(&call.0.0);
    }
    let digest = h.finish();
    let mut one: Vec<Listing, Alloc<'static>> = Vec::with_capacity_in(1, g);
    one.push(Listing { heads: heads.leak(), by_sig: by_sig.leak(), unlisted: unlisted.leak(), calls: calls.leak(), digest });
    let one: &'static [Listing] = one.leak();
    &one[0]
}

/// What a listing is kept under: the crate's key as the reader's unit views it (its prefix is
/// what every head name of it begins with) and the digest of the crate's member files the walk
/// was given (`Subjects`' rows of that prefix: the Heads stage and the impl tables make theirs
/// apart, so the files are part of the key, never assumed one list; rule 21).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ListingKey {
    krate: [u8; 32],
    subjects: [u8; 32],
}

/// The digest of `subjects`' rows of crate `prefix` (each file's index and outline name).
fn subjects_digest(subjects: &Subjects, prefix: [u8; 8]) -> ContentId {
    let from = subjects.partition_point(|(p, _, _)| *p < prefix);
    let mut h = blaze_ids::Hasher::new(LISTING_SUBJECTS_DOMAIN);
    for (_, file, name) in subjects[from..].iter().take_while(|(p, _, _)| *p == prefix) {
        h.part(&file.to_le_bytes());
        name_part(&mut h, *name);
    }
    h.finish()
}

/// Whether a listing is the table's (the rule) or made at each ask (the control of the test
/// that a crate is listed once a request: engine_owners_once_tests.rs, rule 23).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ListingsPlant {
    Rule,
    /// Every ask makes its own listing and the table is never asked; a head task asks for its
    /// crate's listing at each of its tasks.
    #[cfg_attr(not(test), expect(dead_code, reason = "a test's plant (rule 23)"))]
    PerAsk,
}

/// The listings' counts (the profile's "head listings" line; floor: `made == crates`).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct ListingCounts {
    /// Listings the table holds: one a (crate, unit view, member files).
    pub(crate) crates: u32,
    /// Walks made ([`heads_of_crate`] through the table).
    pub(crate) made: u32,
    /// Seconds of a race: two jobs made one key at once, the two equal, the first kept.
    pub(crate) twice: u32,
    /// Seconds of a race whose listing was not equal (refused: [`LISTINGS_UNEQUAL`]).
    pub(crate) unequal: u32,
    /// Asks: one a reader's first place in a crate, one a crate's head setup.
    pub(crate) asks: u32,
    /// Asks refused because the table was full ([`MOST_LISTINGS`]).
    pub(crate) full: u32,
}

/// THE LISTINGS: each crate's listing of heads ([`Listing`]), request-wide and insert-only, in
/// the tree's concurrent map, its lists lent from the request heap (as [`HeadsOnce`]'s cells).
/// Every Heads job (a crate's setup, each split's `Owners`) and every Bodies stretch
/// (`Owners` in engine_bodies.rs and engine_seeds.rs) asks it, so the walk is made once a crate a
/// request (the floor) where before it was made by every job and every stretch that placed a
/// read in the crate. Two jobs that make one key at once: the first is kept, the second, equal,
/// is counted; unequal is refused by name (rule 21). No lock is held: the map's own pins.
///
/// [`HeadsOnce`]: crate::engine_heads_once::HeadsOnce
pub(crate) struct Listings {
    heap: &'static blaze_own::Heap,
    plant: ListingsPlant,
    lists: blaze_core::SharedMap<ListingKey, &'static Listing, SharedAlloc<'static>>,
    made: AtomicU32,
    twice: AtomicU32,
    unequal: AtomicU32,
    asks: AtomicU32,
    full: AtomicU32,
}

impl Listings {
    /// An empty table over the request's heap.
    pub(crate) fn new(heap: &'static blaze_own::Heap, plant: ListingsPlant) -> Listings {
        Listings {
            heap,
            plant,
            lists: blaze_core::SharedMap::new_in("engine.heads: each crate's listing once a request", heap.shared()),
            made: AtomicU32::new(0),
            twice: AtomicU32::new(0),
            unequal: AtomicU32::new(0),
            asks: AtomicU32::new(0),
            full: AtomicU32::new(0),
        }
    }

    /// Crate `k` of `records`' listing (`key`: its key there; `texts`: its expansion
    /// directory), from the table or made now and put; `Err`: why none (rule 3).
    fn listing<'n>(&self, records: &'n Records<'n>, texts: &ExpansionTexts<'n>, k: usize, key: blaze_id::CrateItemKey, subjects: &Subjects) -> Result<&'static Listing, &'static str> {
        self.asks.fetch_add(1, Ordering::Relaxed);
        let prefix = key.prefix();
        let at = ListingKey { krate: key.0.0, subjects: subjects_digest(subjects, prefix).0 };
        if self.plant == ListingsPlant::Rule
            && let Some(found) = self.lists.pin().get(&at).copied()
        {
            return Ok(found);
        }
        let made = list_crate(records, texts, k, prefix, subjects, self.heap.alloc());
        self.made.fetch_add(1, Ordering::Relaxed);
        if self.plant == ListingsPlant::PerAsk {
            return Ok(made);
        }
        let lists = self.lists.pin();
        if lists.len() >= MOST_LISTINGS {
            self.full.fetch_add(1, Ordering::Relaxed);
            return Err(LISTINGS_FULL);
        }
        match lists.try_insert(at, made) {
            Ok(_) => Ok(made),
            Err(occupied) => {
                let held: &'static Listing = *occupied.current;
                match held.digest == made.digest {
                    true => {
                        self.twice.fetch_add(1, Ordering::Relaxed);
                        Ok(held)
                    }
                    false => {
                        self.unequal.fetch_add(1, Ordering::Relaxed);
                        Err(LISTINGS_UNEQUAL)
                    }
                }
            }
        }
    }

    /// The counts as they stand.
    pub(crate) fn counts(&self) -> ListingCounts {
        ListingCounts {
            crates: u32::try_from(self.lists.len()).unwrap_or(u32::MAX),
            made: self.made.load(Ordering::Relaxed),
            twice: self.twice.load(Ordering::Relaxed),
            unequal: self.unequal.load(Ordering::Relaxed),
            asks: self.asks.load(Ordering::Relaxed),
            full: self.full.load(Ordering::Relaxed),
        }
    }
}

/// [`HeadAt::Unlisted`]'s words for a crate the records hold and `Owners` was given no
/// directory of: the engine's own inconsistency, named.
const OVER_NOT_RECORDS: &str = "its crate is among the reader's records but not among the crates `Owners` was given (engine_heads.rs `Owners::new`: the two lists are one list, in one order)";

/// [`HeadAt::Unlisted`]'s words for a member crate's head the crate lists no head of: no item
/// of the crate's names records is the item read, or it has no head (engine_heads.rs
/// `heads_of_crate`).
const NO_HEAD_ROW: &str = "no item of its member crate's names records has this head (engine_heads.rs `heads_of_crate`: an item with no head, or a key the records do not hold)";

/// One head cell [`heads_of_crate`] lists.
struct HeadRow<'n> {
    /// Its item's head name in the records read (front2 head.rs `cell_id`; of their unit).
    sig: Name,
    /// The task of the outline entry that covers it.
    entry: Name,
    /// Its file's outline.
    file: Name,
    /// Its cell in `entry`'s task ([`head_cell`]; of the unit of `records`' crate).
    cell: Cell,
    /// Its item, of no unit ([`head_item`]).
    item: Cell,
    want: Want,
    /// Its item's name and an impl fn's name, as its entry's text names them (what
    /// [`head_item`] hashed): the words a refusal of two heads under one cell prints.
    name: &'n str,
    member: Option<&'n str>,
    /// Its covering entry's name as its file writes it (item_name.rs `shown_name`).
    entry_name: &'n str,
    /// A head a macro wrote: the innermost call's macro as written (`None`: a body's block's
    /// module wrote it, no call); `None` outside: a head written in its file.
    by: Option<Option<&'n str>>,
}

/// What [`heads_of_crate`] lists of a crate.
enum CrateRow<'n> {
    /// A head, and the entry whose head task freezes it.
    Head(HeadRow<'n>),
    /// A head no head task holds, and why ([`HeadAt::Unlisted`]).
    Unlisted { sig: Name, why: &'static str },
    /// A macro call whose walk up ends under the entry `entry` (its own, or a call or body's block
    /// it stands in): the entry's head task reads its expansion record's cell (`heads_of`), so
    /// an edit of what it expands to (a `macro_rules!` body, a derive's output) reruns exactly
    /// the head tasks whose entries hold its calls.
    Call { entry: Name, call: blaze_id::ItemKey },
}

/// What a head's want is, in words, for a refusal that names it ([`heads_of`]).
fn want_words(want: Want) -> &'static str {
    match want {
        Want::Head(_) => "the alias",
        Want::Adt(_) => "the type",
        Want::Impl(_) => "the impl header",
        Want::Fn(_) => "the fn, const or static",
        Want::Members(_) => "the trait",
        Want::MemberFn(..) => "the impl fn",
    }
}

/// A head as its entry's text names it, for a refusal ([`heads_of`]): its want's kind in words,
/// its item's name and an impl fn's name. `want: None`: no row of the head (never, as
/// `heads_of` asks only of its own rows; said, not hidden).
#[derive(Clone, Copy)]
struct HeadLabel<'n> {
    want: Option<Want>,
    name: &'n str,
    member: Option<&'n str>,
    /// Its covering entry's name and, for a head a macro wrote, the macro ([`HeadRow::by`]).
    entry_name: &'n str,
    by: Option<Option<&'n str>>,
}

impl core::fmt::Display for HeadLabel<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let words = self.want.map_or("a head with no row", want_words);
        // "`Point` (written by `Serialize`): the impl header": the entry the user wrote first,
        // so a head no name of its own says (an impl) is still found in the text.
        match self.by {
            Some(Some(by)) => write!(f, "`{}` (written by `{by}`): ", self.entry_name)?,
            Some(None) => write!(f, "`{}` (written in a body's block): ", self.entry_name)?,
            None => {}
        }
        match (self.member, self.name) {
            (Some(member), "") => write!(f, "{words} `{member}`"),
            (Some(member), name) => write!(f, "{words} `{name}::{member}`"),
            (None, "") => write!(f, "{words}"),
            (None, name) => write!(f, "{words} `{name}`"),
        }
    }
}

/// [`CrateRow::Unlisted`]'s words for a head whose call (or body) stands where no outline entry
/// of its file covers it, nor holds it in its body ([`holding_entry`]).
const NOT_COVERED: &str = "the call that wrote it (or the body whose block it is in) stands where no outline entry of its file has a head task, and no entry's body holds it (signature.rs `covering_entry`, engine_heads.rs `holding_entry`)";

/// The domain of the steps of a head declared in a body's text as the file holds it (no call
/// and no block step wrote it: [`Written::steps`] is this tag, `from_entry` its place).
const IN_A_BODY_DOMAIN: &str = "blaze-cli head declared in a body 1";

/// The entry whose body holds the outline entry `at`: its nearest container an outline entry
/// covers (signature.rs `covering_entry`), whose head task freezes the heads declared in its
/// body. `None`: none (a file's outline holds no such container). Bounded by the outline's
/// entries: each step goes to a parent.
fn holding_entry(outline: &blaze_file_items::Outline<'_>, at: ItemIndex) -> Option<ItemIndex> {
    let mut parent = outline.items().get(at.0 as usize)?.parent;
    for _ in 0..outline.items().len() {
        let up = parent?;
        if let Some(cover) = crate::signature::covering_entry(outline, up) {
            return Some(cover);
        }
        parent = outline.item(up).parent;
    }
    None
}

/// Every head of crate `k` of `records` whose item is written in a file of `subjects`, or that a
/// macro (or a body's block) wrote under an entry of one, in file order, with the entry whose
/// head task freezes it (rule 30: the one listing of a crate's heads; the head tasks freeze
/// these and the readers place their reads by these, engine_seeds.rs and engine_bodies.rs
/// `widen`). A written head is walked up its calls and blocks to the file entry its outermost
/// call stands at (body_types/covered.rs `written_path`, `texts`: the crate's directory, no
/// expansion record read), and its cell holds where under that entry it was written
/// ([`Written`]). A head whose walk fails, or that no entry covers, is listed as
/// [`CrateRow::Unlisted`] with why, never left out (rule 3). Each call of the crate whose walk
/// ends under an entry is listed as [`CrateRow::Call`].
fn heads_of_crate<'n>(records: &'n Records<'n>, texts: &ExpansionTexts<'n>, k: usize, prefix: [u8; 8], subjects: &Subjects, g: Alloc<'static>, each: &mut dyn FnMut(CrateRow<'n>)) {
    let mut keys: Vec<blaze_id::ItemKey, Alloc<'static>> = Vec::new_in(g);
    NamesSource::each_item(records, k, &mut |key, _| keys.push(key));
    let krate = NamesSource::crate_key(records, k);
    // (its text, its entry there, want, the item's name, an impl fn's name).
    let mut wanted: Vec<(At, u32, Want, &'n str, Option<&'n str>), Alloc<'static>> = Vec::new_in(g);
    // Each item's and member's block text key, with where its body is written
    // (body_types/covered.rs `BlockHomes`): a key no block has is never asked.
    let mut blocks: Vec<(blaze_id::ItemKey, At, ItemIndex), Alloc<'static>> = Vec::new_in(g);
    for key in keys.iter().copied() {
        let Ok(want) = by_origin::kind_of_lib(records, key).and_then(|kind| by_origin::item_want(records, kind)) else { continue };
        let Some(place) = NamesSource::place_of(records, key) else { continue };
        let item = NamesSource::name_of(records, key).unwrap_or("");
        wanted.push((place.at, place.item.0, want, item, None));
        if let Some(krate) = krate {
            blocks.push((crate::body_types::covered::block_key(krate, key), place.at, place.item));
        }
        if let Want::Impl(lib) | Want::Members(lib) = want {
            NamesSource::each_member(records, lib, &mut |m| {
                // A member a call in the container's block wrote is in that call's text.
                let text = m.written_by.map_or(place.at, At::Expansion);
                if let Some(krate) = krate {
                    blocks.push((crate::body_types::covered::block_key(krate, m.key), text, m.item));
                }
                if let (Want::Impl(_), blaze_surface::ItemKind::Function, Some(name)) = (want, m.kind, m.name) {
                    wanted.push((text, m.item.0, Want::MemberFn(lib, member_key_of(checker_of(lib), MemberKind::Fn, name)), item, Some(name)));
                }
            });
        }
    }
    blocks.sort_unstable_by_key(|(block, _, _)| *block);
    blocks.dedup_by_key(|(block, _, _)| *block);
    // (file, the file entry it or its outermost call is at, want, name, impl fn's name, the walk).
    type Walk<'n> = Option<(ContentId, Option<&'n str>)>;
    let mut placed: Vec<(u32, u32, Want, &'n str, Option<&'n str>, Walk<'n>), Alloc<'static>> = Vec::with_capacity_in(wanted.len(), g);
    for &(at, item, want, name, member) in wanted.iter() {
        match at {
            At::File(file) => placed.push((file, item, want, name, member, None)),
            At::Expansion(from) => match crate::body_types::covered::written_path(texts, &blocks, from, Some(ItemIndex(item)), &mut |_| {}) {
                Ok(path) => placed.push((path.top.0, path.top.1.0, want, name, member, Some((path.steps, path.by)))),
                Err(why) => each(CrateRow::Unlisted { sig: head_cell_id(want).of, why }),
            },
        }
    }
    placed.sort_unstable_by_key(|(file, item, want, _, _, walk)| (*file, *item, want_bytes(*want), walk.map(|(steps, _)| steps)));
    // (file, the file entry the outermost call is at, the call): every call of the crate.
    let mut calls: Vec<(u32, u32, blaze_id::ItemKey), Alloc<'static>> = Vec::with_capacity_in(texts.len(), g);
    for at in 0..texts.len() {
        let Ok(call) = texts.call_at(at) else { continue };
        // A call whose walk fails (a call inside a body, whose text keeps no origins) is under
        // no head task: the heads it wrote are each listed `Unlisted` above.
        if let Ok(path) = crate::body_types::covered::written_path(texts, &blocks, call, None, &mut |_| {}) {
            calls.push((path.top.0, path.top.1.0, call));
        }
    }
    calls.sort_unstable_by_key(|(file, item, call)| (*file, *item, *call));
    let mut files: Vec<u32, Alloc<'static>> = Vec::with_capacity_in(placed.len() + calls.len(), g);
    files.extend(placed.iter().map(|one| one.0));
    files.extend(calls.iter().map(|one| one.0));
    files.sort_unstable();
    files.dedup();
    let edition = NamesSource::edition_of(records, k);
    for file in files.iter().copied() {
        let heads = &placed[placed.partition_point(|one| one.0 < file)..placed.partition_point(|one| one.0 <= file)];
        let at_calls = &calls[calls.partition_point(|one| one.0 < file)..calls.partition_point(|one| one.0 <= file)];
        let Ok(at) = subjects.binary_search_by(|(p, f, _)| (*p, *f).cmp(&(prefix, file))) else { continue };
        let outline_name = subjects[at].2;
        let (Some(text), Some(edition)) = (NamesSource::file_text(records, k, file), edition) else { continue };
        let outline = blaze_file_items::outline(text, edition, g);
        let entry_of = |top: u32| crate::signature::covering_entry(&outline, ItemIndex(top)).map(|cover| (cover, crate::signature::signature_name(outline_name.subject, &outline, outline.item(cover), text)));
        for &(_, top, call) in at_calls.iter() {
            if let Some((_, entry)) = entry_of(top) {
                each(CrateRow::Call { entry, call });
            }
        }
        for &(_, top, want, name, member, walk) in heads.iter() {
            // An entry no outline entry covers stands inside a body (an impl in a fn's body, a
            // body's block a call or `const _` wrote): a body's items have no `Signature` of their
            // own (D114 FACTS (iii)), so the head task of the entry whose body holds it freezes it
            // (`holding_entry`), under a cell of its place in that entry. The freeze records
            // its reads of the block's expansion, items and members.
            let placed = match entry_of(top) {
                Some((cover, entry)) => Some((cover, entry, walk.map(|(steps, _)| Written { steps, from_entry: top - cover.0 }))),
                None => holding_entry(&outline, ItemIndex(top)).and_then(|holder| entry_of(holder.0)).map(|(cover, entry)| {
                    let steps = walk.map_or_else(|| ContentId::of(IN_A_BODY_DOMAIN, b""), |(steps, _)| steps);
                    (cover, entry, Some(Written { steps, from_entry: top - cover.0 }))
                }),
            };
            let Some((cover, entry, written)) = placed else {
                let why = if walk.is_some() { NOT_COVERED } else { crate::signature::uncovered_why(&outline, ItemIndex(top)) };
                each(CrateRow::Unlisted { sig: head_cell_id(want).of, why });
                continue;
            };
            each(CrateRow::Head(HeadRow {
                sig: head_cell_id(want).of,
                entry,
                file: outline_name,
                cell: head_cell_written(want, name, member, written, WHOLE),
                item: head_item(want, name, member, written, WHOLE),
                want,
                name,
                member,
                entry_name: crate::item_name::shown_name(outline.item(cover), text),
                by: walk.map(|(_, by)| by),
            }));
        }
    }
}

/// A names read as its crate's task defines it (engine_bodies.rs `widen`'s rule): a cell of a
/// crate with a cell table stays its cell (its lang rows one of them); any other its record whole.
pub(crate) fn names_read(view: View<'_, '_>, id: CellId) -> CellId {
    match id.of.kind {
        Kind::CrateScope if id.cell != WHOLE && view.cell(&CellId { of: id.of, cell: CELLS }) != blaze_drive::key::ABSENT => id,
        Kind::CrateScope => CellId { of: id.of, cell: WHOLE },
        _ => id,
    }
}

/// Every member crate's files as its items record lists them, matched to the planner's outlines
/// ([`Subjects`], sorted): what [`Owners::place`] places an `ImplBucket` task's header read by,
/// at cell grain ([`HeadsStage::run_phase`]). The same match `run_items` and engine_bodies.rs
/// `groups` make in their member loops (to be this one home once lane RU1's per-unit heads land
/// there). A member whose records do not open lists no file: its heads are then placed as a
/// dependency's (its record whole), never missed.
fn member_subjects<'r>(
    read: &mut EngineRecords<'r>,
    cx: &Cx,
    store: &mut dyn crate::store::Store<'r>,
    names: &NamesStage<'_>,
    list: &blaze_world::crate_list::CrateList<'static>,
    outlined: &[Outlined<'_>],
    heap: &'static blaze_own::Heap,
) -> std::vec::Vec<([u8; 8], u32, Name), Global> {
    let g = heap.alloc();
    let mut subjects: std::vec::Vec<([u8; 8], u32, Name), Global> = std::vec::Vec::new_in(Global);
    for member in (0..CrateGraph::len(list)).filter(|at| list.member(*at)) {
        // A member whose scope is refused lists no file here (its heads placed as a
        // dependency's, coarser, never missed); its typed cause is printed by the refusal of
        // that member's own head tasks and bodies ([`HeadsStage::run_items`], engine_bodies.rs
        // `groups`), which call `scope_of` for it the same way: nothing is dropped unsaid.
        let Ok((scope, _)) = crate::engine_bodies::scope_of(read, cx, store, names, list, member, heap) else { continue };
        let Ok(items) = ItemsRecord::open(scope.own_records.items, g) else { continue };
        let prefix = list.key(member).prefix();
        for file in 0..u32::try_from(items.files()).unwrap_or(u32::MAX) {
            let Ok(Some(row)) = items.file(file) else { continue };
            let Some(path) = row.path.in_package(list.shown_from(member), g) else { continue };
            let Some(path) = path.strip_prefix("/0/") else { continue };
            if let Some(one) = outlined.iter().find(|one| one.path.as_slice() == path.as_bytes()) {
                subjects.push((prefix, file, one.name));
            }
        }
    }
    subjects.sort_unstable();
    subjects
}

/// One member crate's head tasks, made over its check scope's records (in place in the store's
/// mapping, engine_records.rs): its setup on one worker, its tasks in split jobs on any.
struct ItemJob<'r> {
    prefix: [u8; 8],
    crates: Vec<Crate<'r>, Global>,
    lang: Vec<usize, Global>,
    /// Every crate of its scope had records (else its tasks are refused by name).
    whole: bool,
    /// When not `whole`: the cut's cause in words, the crate named (engine_bodies.rs
    /// `Scope::cut_cause`), what its tasks' refusal prints.
    cut_words: Option<&'static str>,
    /// The cells a refusal of its tasks is a function of ([`scope_cells`]): at 0 the graph's,
    /// at 1 the member's own names cell.
    cells: std::vec::Vec<CellId, Global>,
    /// Its tasks, sorted by (file, name) before splitting.
    tasks: std::vec::Vec<Task, Global>,
}

/// One head task as a worker made it.
struct WorkedHead {
    out: ContentId,
    defines: std::vec::Vec<(Cell, ContentId), Global>,
    /// Each cell's item ([`head_item`]) and its unit's crate prefix, as `defines` lists them:
    /// what the profile counts by. Over the handed heap's `Sync` handle: it crosses back from
    /// the worker.
    items: SVec<'static, (Cell, [u8; 8])>,
    reads: std::vec::Vec<CellId, Global>,
    /// Each cell's want (front2 head.rs `want_bytes`), its cell ([`head_cell`], 16 bytes), its
    /// frozen bytes' length (u32 LE) and the frozen bytes, in cell order.
    payload: std::vec::Vec<u8, Global>,
    /// Each head it could not freeze: its cell, the head, why (engine_head_notes.rs).
    failed: SVec<'static, (Cell, &'static str, crate::engine_head_notes::Cause)>,
}

/// The domain of a head's cell digest when it could not be frozen: of its reason, so a reader
/// moves when the reason does, and when the head is frozen at last.
const HEAD_FAILED_DOMAIN: &str = "blaze-cli engine heads: a head not frozen";

/// A head task that could not freeze a head: why (rule 19), and what it read until then.
struct HeadFailed {
    why: &'static str,
    reads: std::vec::Vec<CellId, Global>,
}

/// One head task's freezes under each unit that holds its file, gathered in unit order (lane
/// RU1, [`head_cell`]): its row is their union, each unit's cells its own. Allocates through
/// the phase's handed `alloc` only; the first unit's [`WorkedHead`] holds the union of the
/// rest (its vectors are the worker's).
struct UnitsMade<'a> {
    alloc: SharedAlloc<'a>,
    /// Its share of each unit's worker time.
    ns: u64,
    /// The first unit's failure, the cells every failing unit's refusal is a function of, and
    /// whether one is owed (a panic): the task is refused whole.
    failed: Option<(&'static str, SVec<'a, CellId>, bool)>,
    /// Each unit's crate prefix and out, in unit order ([`HEAD_UNITS_OUT_DOMAIN`]).
    out: blaze_ids::Hasher,
    /// What every unit read, and the refusal cells of a holding unit that was not whole.
    reads: SVec<'a, CellId>,
    /// The first unit's freeze, the later units' defines, items, runs and failed heads
    /// appended in unit order ([`WorkedHead::payload`]: one run a (unit, cell)).
    first: Option<WorkedHead>,
}

impl<'a> UnitsMade<'a> {
    fn new(alloc: SharedAlloc<'a>) -> UnitsMade<'a> {
        UnitsMade { alloc, ns: 0, failed: None, out: blaze_ids::Hasher::new(HEAD_UNITS_OUT_DOMAIN), reads: SVec::new_in(alloc), first: None }
    }

    /// Unit `prefix`'s freeze of the task (its items carry `prefix` already, `heads_of`).
    fn take(&mut self, prefix: [u8; 8], mut one: WorkedHead) {
        self.out.update(&prefix).update(&one.out.0);
        self.reads.extend(one.reads.iter().copied());
        // Its list is in the union now: freed here, not kept a second time to the phase's merge
        // with the first unit's freeze (lane H8: each list was held three times until its row).
        one.reads.clear();
        one.reads.shrink_to_fit();
        match &mut self.first {
            None => self.first = Some(one),
            Some(all) => {
                all.defines.extend(one.defines.iter().copied());
                all.items.extend(one.items.iter().copied());
                all.payload.extend_from_slice(&one.payload);
                all.failed.extend(one.failed.iter().copied());
            }
        }
    }

    /// A unit could not freeze the task: why, and the cells that is a function of.
    fn fail(&mut self, why: &'static str, cells: &[CellId], owed: bool) {
        match &mut self.failed {
            Some((_, all, was_owed)) => {
                all.extend_from_slice(cells);
                *was_owed |= owed;
            }
            None => {
                let mut all = SVec::with_capacity_in(cells.len(), self.alloc);
                all.extend_from_slice(cells);
                self.failed = Some((why, all, owed));
            }
        }
    }
}

impl HeadsStage {
    /// The head tasks of one Heads phase (module docs, "THE HEAD TASKS"): each member crate's
    /// setup once ([`crate_setup`]), its tasks cut into split jobs over every worker
    /// ([`split_count`]), merged in task order ([`in_split_order`]), each result its row in
    /// `buffer`.
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
            // The whole task failed: named by the file its entry is in.
            let file = handles.outlined.binary_search_by(|one| one.name.cmp(&task.planned.file)).ok().and_then(|at| core::str::from_utf8(&handles.outlined[at].path).ok());
            let head = match file {
                Some(path) => crate::engine_head_notes::leak(g, format_args!("a head of `{path}`")),
                None => "a head of a file the outlines do not hold",
            };
            refused.push(crate::engine_head_notes::Refused::whole(task.planned.name, head, why));
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
        // Every member's items record was read: then a file none lists is no unit's (a `cfg` on
        // its `mod` cut it in every unit, engine_body_plan.rs), not a refusal.
        let mut every_member_read = true;
        // Why the first member whose records could not be read was not (its typed cause, the
        // crate named) and how many were not: what a task no unit holds then prints (rule 19).
        let mut unread: Option<&'static str> = None;
        let mut unread_count = 0u32;
        // Each member unit's cfg-blind walk of its module tree (engine_body_plan.rs `reached`,
        // the planner's walk): with its names task and crate prefix. A unit whose walk reaches a
        // task's file but whose items record does not list it (a `#[cfg(test)]` on its `mod`)
        // is a unit the task's heads are NOT frozen under, and that is a fact of the unit's
        // records: the row reads the unit's not-held cell (engine_body_plan.rs `not_held_cell`,
        // the one home Bodies reads too), so a cfg taken off enters the task again (rule 21).
        let mut walks: Vec<(Name, [u8; 8], Vec<u32, Alloc<'static>>), Alloc<'static>> = Vec::new_in(g);
        for member in (0..CrateGraph::len(&list)).filter(|at| list.member(*at)) {
            let root = list.root(member);
            let reach = crate::engine_body_plan::reached(handles.outlined, Name::file(root.strip_prefix("/0/").unwrap_or(root).as_bytes()), g);
            walks.push((blaze_drive::key::crate_scope_name(&list.key(member)), list.key(member).prefix(), reach));
            no_member.push(CellId { of: blaze_drive::key::crate_scope_name(&list.key(member)), cell: WHOLE });
            let (scope, whole) = match crate::engine_bodies::scope_of(read, cx, store, names, &list, member, heap) {
                Ok(made) => made,
                Err(cause) => {
                    every_member_read = false;
                    unread_count = unread_count.saturating_add(1);
                    unread.get_or_insert_with(|| crate::engine_head_notes::leak(g, format_args!("its check scope was not read: {cause}")));
                    continue;
                }
            };
            let cells = scope_cells(graph.name, &list, member, Some(&scope));
            let cut_words = scope.cut_cause(&list).map(|cause| crate::engine_head_notes::leak(g, format_args!("its crate's {cause}")));
            let Ok(items) = ItemsRecord::open(scope.own_records.items, g) else {
                every_member_read = false;
                unread_count = unread_count.saturating_add(1);
                unread.get_or_insert_with(|| crate::engine_head_notes::leak(g, format_args!("the items record of crate `{}` does not open (blaze-scope items_record.rs `ItemsRecord::open`)", CrateGraph::name(&list, member))));
                continue;
            };
            let prefix = list.key(member).prefix();
            for file in 0..u32::try_from(items.files()).unwrap_or(u32::MAX) {
                let Ok(Some(row)) = items.file(file) else { continue };
                let Some(path) = row.path.in_package(list.shown_from(member), g) else { continue };
                let Some(path) = path.strip_prefix("/0/") else { continue };
                if let Some(one) = handles.outlined.iter().find(|one| one.path.as_slice() == path.as_bytes()) {
                    subjects.push((prefix, file, one.name));
                }
            }
            jobs.push(ItemJob { prefix, crates: scope.crates, lang: scope.lang, whole, cut_words, cells, tasks: std::vec::Vec::new_in(Global) });
        }
        subjects.sort_unstable();
        jobs.sort_unstable_by_key(|job| job.prefix);
        // Each task to EVERY member unit (by prefix) whose files hold its file (lane RU1,
        // `head_cell`): each freezes the task's heads under its own records, one cell a unit.
        let mut by_file: std::vec::Vec<(Name, [u8; 8]), Global> = std::vec::Vec::with_capacity_in(subjects.len(), Global);
        by_file.extend(subjects.iter().map(|(prefix, _, file)| (*file, *prefix)));
        by_file.sort_unstable();
        by_file.dedup();
        // Each task that some unit runs, sorted by name, and what its units made (step 3).
        let mut accs: SVec<'a, (Task, UnitsMade<'a>)> = SVec::with_capacity_in(tasks.len(), alloc);
        let mut holding: SVec<'a, usize> = SVec::new_in(alloc);
        // Each task's (name, the prefix of the unit that shows it on the status board): the
        // first whole unit that holds it, so a task frozen under two units is one task started
        // and done there (engine_status.rs), as the phase's known count counts it.
        let mut shows: SVec<'a, (Name, [u8; 8])> = SVec::with_capacity_in(tasks.len(), alloc);
        for task in tasks.iter() {
            let at = by_file.partition_point(|(file, _)| *file < task.planned.file);
            holding.clear();
            holding.extend(by_file[at..].iter().take_while(|(file, _)| *file == task.planned.file).filter_map(|(_, prefix)| jobs.binary_search_by_key(prefix, |job| job.prefix).ok()));
            // The plant (`HeadUnits`): one unit alone, as before the rule.
            match self.freeze {
                HeadUnits::Each | HeadUnits::NoExpansionReads => {}
                HeadUnits::OnlyFirst => holding.truncate(1),
                HeadUnits::OnlyLast => {
                    if let Some(last) = holding.last().copied() {
                        holding.clear();
                        holding.push(last);
                    }
                }
            }
            let whole = holding.iter().copied().find(|at| jobs[*at].whole);
            match (holding.first().copied(), whole) {
                // No unit holds its file under its cfg: no unit's head, a row with no finding on
                // the cells that say so (each member's names cell, the graph's), so a unit that
                // comes to hold it enters it again (engine_body_plan.rs, as its bodies).
                (None, _) if every_member_read => {
                    let mut reads: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(no_member.len(), alloc);
                    reads.extend(no_member.iter().map(|id| (*id, view.cell(id))));
                    reads.sort_unstable_by_key(|(id, _)| *id);
                    reads.dedup_by_key(|(id, _)| *id);
                    let out = no_units_out(task.planned.name);
                    buffer.done.push(Done { name: task.planned.name, key: task.key, out, defines: SVec::new_in(alloc), reads, findings: 0, failed: false });
                }
                (None, _) => {
                    let why = crate::engine_head_notes::leak(
                        g,
                        format_args!(
                            "its file is in no member crate's items record, and {unread_count} member(s)' records could not be read, the first because {} (engine_heads.rs `run_items`)",
                            unread.unwrap_or("of no cause kept (engine_heads.rs `run_items` keeps the first)")
                        ),
                    );
                    refuse(task, why, &no_member, buffer);
                }
                // Its member's names cell, each scope crate's that had records, and the one the
                // scope was cut at, read ABSENT: its task that comes, or its fix, moves the row.
                // The cut's cause printed, the crate named (rule 19).
                (Some(at), None) => {
                    let why = jobs[at].cut_words.unwrap_or("its crate's check scope was cut and `scope_of` kept no cut (engine_bodies.rs `scope_of`)");
                    refuse(task, why, &jobs[at].cells, buffer);
                }
                // Frozen under each whole unit; a unit whose scope lacks a names record defines
                // no cell (its readers read it ABSENT), and the row reads that unit's refusal
                // cells, so the fix that makes it whole enters the task again (rule 21).
                (Some(_), Some(first_whole)) => {
                    shows.push((task.planned.name, jobs[first_whole].prefix));
                    let mut made = UnitsMade::new(alloc);
                    for at in holding.iter().copied() {
                        match jobs[at].whole {
                            true => jobs[at].tasks.push(*task),
                            false => made.reads.extend(jobs[at].cells.iter().copied()),
                        }
                    }
                    // The units its file is NOT frozen under though their walk reaches it: their
                    // not-held cells, so one that comes to hold the file moves the row.
                    let held = &by_file[at..];
                    let held = &held[..held.partition_point(|(file, _)| *file == task.planned.file)];
                    if let Ok(file_at) = handles.outlined.binary_search_by(|one| one.name.cmp(&task.planned.file)) {
                        let file_at = u32::try_from(file_at).unwrap_or(u32::MAX);
                        for (unit, prefix, reach) in walks.iter() {
                            if reach.binary_search(&file_at).is_ok() && !held.iter().any(|(_, one)| one == prefix) {
                                made.reads.push(crate::engine_body_plan::not_held_cell(view, *unit, *prefix));
                            }
                        }
                    }
                    accs.push((*task, made));
                }
            }
        }
        accs.sort_unstable_by_key(|(task, _)| task.planned.name);
        accs.dedup_by_key(|(task, _)| task.planned.name);
        shows.sort_unstable();
        shows.dedup_by_key(|(name, _)| *name);
        let shows = &shows[..];
        jobs.retain(|job| !job.tasks.is_empty());
        // Group the tasks of each file before choosing whole-file cuts.
        for job in jobs.iter_mut() {
            job.tasks.sort_unstable_by_key(|task| (task.planned.file, task.planned.name));
        }
        // 2. Each member's heads, at once, on the workers: first each crate's setup, once a crate
        // (`crate_setup`: its listing from the request's table, its lang index's reads, its
        // receiver pointers and theirs), then its tasks cut into split jobs (`split_count`),
        // each over a source and recorders of its own (`heads_of`), all crates' splits at once.
        let procs = names.names.as_ref().and_then(|names| names.procs());
        let root = handles.root;
        let clock = handles.clock;
        let subjects = &subjects[..];
        let expansion_reads = self.freeze != HeadUnits::NoExpansionReads;
        type Ran = Result<Result<std::vec::Vec<Result<WorkedHead, HeadFailed>, Global>, &'static str>, work::Panicked>;
        type Setup = Result<Result<CrateSetup, &'static str>, work::Panicked>;
        // The request's table of frozen heads, lent to every worker (engine_heads_once.rs).
        let once_table = self.once.take().unwrap_or_else(|| crate::engine_heads_once::HeadsOnce::new(heap, crate::engine_heads_once::OncePlant::Rule, crate::engine_heads_once::Sources::Job));
        let once = &once_table;
        // The request's crate listings, lent to every worker as the frozen heads are.
        let head_jobs = self.jobs;
        let listings: &Listings = self.listings.get_or_insert_with(|| Listings::new(heap, head_jobs.listings));
        let workers = usize::try_from(handles.workers.get()).unwrap_or(usize::MAX);
        let board = handles.board;
        // Each crate's setup and its time, in job order.
        let mut setups: SVec<'static, (Setup, u64)> = SVec::with_capacity_in(jobs.len(), heap.shared());
        {
            let scope = cx.scope();
            scope.narrow_workers(handles.workers, Class::Foreground);
            let mut running = Vec::with_capacity_in(jobs.len(), g);
            for job in jobs.iter() {
                let clock = clock.clone();
                running.push(scope.may_overlap("engine heads: a crate's setup", HEADS_FRAME, move |cx: &Cx| {
                    let began = clock.now();
                    let made = work::unwinding(|| crate_setup(cx, root, packs, corrected, procs, job, subjects, view, heap, listings));
                    Ok::<_, Infallible>((made, clock.now().since(began).map_or(0, crate::engine_times::ns)))
                }));
            }
            let mut stopped = None;
            for handle in running.into_iter() {
                match handle.join(cx) {
                    Ok(one) => setups.push(one),
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
        // Each crate whose setup was made, cut into split jobs, in job order and task order.
        let total: usize = jobs.iter().zip(setups.iter()).filter(|(_, (setup, _))| matches!(setup, Ok(Ok(_)))).map(|(job, _)| job.tasks.len()).sum();
        let mut splits: SVec<'static, SplitAt> = SVec::new_in(heap.shared());
        let mut most_splits = 0usize;
        let mut split_files = 0usize;
        let mut file_sizes = Vec::new_in(g);
        for (at, (job, (setup, _))) in jobs.iter().zip(setups.iter()).enumerate() {
            if !matches!(setup, Ok(Ok(_))) {
                continue;
            }
            let len = job.tasks.len();
            let n = split_count(head_jobs.split, len, total, workers);
            let before = splits.len();
            let mut last_crossed = None;
            let mut append = |range: core::ops::Range<usize>| {
                let file = job.tasks[range.start].planned.file;
                if range.start > 0 && job.tasks[range.start - 1].planned.file == file && last_crossed != Some(file) {
                    split_files += 1;
                    last_crossed = Some(file);
                }
                splits.push(SplitAt {
                    job: u32::try_from(at).unwrap_or(u32::MAX),
                    from: u32::try_from(range.start).unwrap_or(u32::MAX),
                    to: u32::try_from(range.end).unwrap_or(u32::MAX),
                });
            };
            match head_jobs.split {
                #[cfg(test)]
                HeadSplit::Even(_) => {
                    for cut in 0..n {
                        append(cut * len / n..(cut + 1) * len / n);
                    }
                }
                HeadSplit::Rule | HeadSplit::Jobs(_) if n == 1 => append(0..len),
                HeadSplit::Rule | HeadSplit::Jobs(_) => {
                    file_sizes.clear();
                    file_sizes.extend(job.tasks.chunk_by(|a, b| a.planned.file == b.planned.file).map(|file| file.len()));
                    file_splits(&file_sizes, len, n).for_each(append);
                }
            }
            most_splits = most_splits.max(splits.len() - before);
        }
        // Each split's result as it was joined, then in split order ([`in_split_order`]).
        let mut arrived: SVec<'static, (u32, (Ran, u64))> = SVec::with_capacity_in(splits.len(), heap.shared());
        {
            let scope = cx.scope();
            scope.narrow_workers(handles.workers, Class::Foreground);
            let mut running = Vec::with_capacity_in(splits.len(), g);
            for (at, split) in splits.iter().enumerate() {
                let job = &jobs[split.job as usize];
                // Only a crate whose setup was made has splits (above).
                let Ok(Ok(setup)) = &setups[split.job as usize].0 else { continue };
                let tasks = &job.tasks[split.from as usize..split.to as usize];
                let clock = clock.clone();
                let at = u32::try_from(at).unwrap_or(u32::MAX);
                running.push((
                    at,
                    scope.may_overlap("engine heads", HEADS_FRAME, move |cx: &Cx| {
                        let began = clock.now();
                        let on_board = OnBoard { board, clock: &clock, shows };
                        let made = work::unwinding(|| heads_of(cx, root, packs, corrected, procs, job, setup, tasks, subjects, view, expansion_reads, heap, once, listings, head_jobs.listings, on_board));
                        Ok::<_, Infallible>((made, clock.now().since(began).map_or(0, crate::engine_times::ns)))
                    }),
                ));
            }
            let mut stopped = None;
            for (at, handle) in running.into_iter() {
                match handle.join(cx) {
                    Ok(one) => arrived.push((at, one)),
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
        self.once = Some(once_table);
        let worked = in_split_order(arrived, splits.len(), head_jobs.merge, heap.shared()).map_err(|error| CliError::Text { what: crate::engine_head_notes::leak(g, format_args!("{error}")) })?;
        // 3. Each unit's results gathered by task (units in prefix order, a unit's splits in task
        // order), then each task as the engine's row, in task order.
        let acc = |accs: &[(Task, UnitsMade<'a>)], task: &Task| -> Option<usize> { accs.binary_search_by_key(&task.planned.name, |(one, _)| one.planned.name).ok() };
        let mut worked = worked.into_iter();
        let mut cursor = 0usize;
        let mut busy = 0usize;
        for (at, (job, (setup, setup_ns))) in jobs.iter().zip(setups.iter()).enumerate() {
            // The setup's time shared by the crate's tasks, as a job's was.
            let each = u64::try_from(job.tasks.len()).unwrap_or(1).max(1);
            for task in job.tasks.iter() {
                if let Some(i) = acc(&accs, task) {
                    accs[i].1.ns = accs[i].1.ns.saturating_add(*setup_ns / each);
                }
            }
            match setup {
                Ok(Ok(_)) => {}
                // The scope's records not opened, or its lang index or listing refused
                // (`crate_setup`): a function of the graph's cell (the packs) and each scope
                // crate's names cell.
                Ok(Err(why)) => {
                    let why = *why;
                    for task in job.tasks.iter() {
                        if let Some(i) = acc(&accs, task) {
                            accs[i].1.fail(why, &job.cells, false);
                        }
                    }
                    continue;
                }
                // A panic is not a function of reads: owed, entered again each request (rule 20).
                Err(work::Panicked) => {
                    for task in job.tasks.iter() {
                        if let Some(i) = acc(&accs, task) {
                            accs[i].1.fail("its crate's head setup panicked; that crate's heads alone failed", &[], true);
                        }
                    }
                    continue;
                }
            }
            while let Some(split) = splits.get(cursor).filter(|split| split.job as usize == at) {
                cursor += 1;
                let Some((ran, ns)) = worked.next() else { break };
                let tasks = &job.tasks[split.from as usize..split.to as usize];
                let each = u64::try_from(tasks.len()).unwrap_or(1).max(1);
                for task in tasks.iter() {
                    if let Some(i) = acc(&accs, task) {
                        accs[i].1.ns = accs[i].1.ns.saturating_add(ns / each);
                    }
                }
                let made = match ran {
                    Ok(Ok(made)) => made,
                    // The split's records not opened again (`heads_of`): as the setup's.
                    Ok(Err(why)) => {
                        for task in tasks.iter() {
                            if let Some(i) = acc(&accs, task) {
                                accs[i].1.fail(why, &job.cells, false);
                            }
                        }
                        continue;
                    }
                    // A panic fails its split's tasks alone, owed (rule 20): the crate's other
                    // splits, and every other crate's, stand.
                    Err(work::Panicked) => {
                        for task in tasks.iter() {
                            if let Some(i) = acc(&accs, task) {
                                accs[i].1.fail("a run of its crate's head tasks panicked; those tasks alone failed", &[], true);
                            }
                        }
                        continue;
                    }
                };
                if !made.is_empty() {
                    busy += 1;
                }
                // A stop refused the tasks from `ran` on before they began (`heads_of`): each owed
                // (no read), and the driver stops before the merge (engine_status.rs).
                let ran = made.len();
                for task in tasks.iter().skip(ran) {
                    if let Some(i) = acc(&accs, task) {
                        accs[i].1.fail(NOT_BEGUN, &[], true);
                    }
                }
                for (task, one) in tasks.iter().zip(made) {
                    let Some(i) = acc(&accs, task) else { continue };
                    // The tests' observer of one unit's read list (engine_heads_once_tests.rs:
                    // which task and unit hold the longest list, under each strategy).
                    #[cfg(test)]
                    crate::engine_heads_once::OnceCounts::observe_unit_reads(
                        task.planned.name,
                        job.prefix,
                        match &one {
                            Ok(head) => &head.reads,
                            Err(failed) => &failed.reads,
                        },
                    );
                    match one {
                        Ok(one) => accs[i].1.take(job.prefix, one),
                        Err(failed) => accs[i].1.fail(failed.why, &failed.reads, false),
                    }
                }
            }
        }
        // Every split was merged (`in_split_order` answered each): one left is this function's
        // bug, named (rule 3).
        if cursor != splits.len() || worked.next().is_some() {
            return Err(CliError::Text { what: "the head split jobs' results were not each merged into their crate's tasks (engine_heads.rs `run_items`)" });
        }
        let mut heads_failed: std::vec::Vec<crate::engine_head_notes::Refused, Global> = std::vec::Vec::new_in(Global);
        let mut counts = UnitCounts::default();
        let mut units: SVec<'a, [u8; 8]> = SVec::new_in(alloc);
        // The cells this phase's head tasks define, every unit's (a refused task defines none): a
        // head's read of another head of this phase is recorded at today's digest (lane BH1),
        // so the merge never sends it to a second wave for a cell it saw (blaze-engine
        // engine.rs `readers_of`). Since heads are frozen per unit (lane RU1) a task's cells
        // come from several jobs, so this is one table for the phase, not one a job.
        let today = crate::engine_scope_reads::job_cells(accs.iter().map(|(task, made)| (task.planned.name, made.first.as_ref().filter(|_| made.failed.is_none()).map(|one| &one.defines[..]))));
        for (task, made) in accs.into_iter() {
            times.push(task.planned.name, made.ns);
            // One unit's failure fails the task whole: its row is refused on the union of what
            // each failing unit read (owed when one panicked).
            if let Some((why, cells, owed)) = made.failed {
                let cells: &[CellId] = if owed { &[] } else { &cells };
                refuse(&task, why, cells, buffer);
                continue;
            }
            // Each whole holding unit ran it and none failed, so one froze it (`heads_of` makes
            // one result a task); none is a bug of this function, said by name (rule 3).
            let Some(one) = made.first else {
                refuse(&task, "no unit that holds its file froze it, and none failed (engine_heads.rs `run_items`)", &made.reads, buffer);
                continue;
            };
            let mut defines: SVec<'a, (Cell, ContentId)> = SVec::with_capacity_in(one.defines.len(), alloc);
            defines.extend(one.defines.iter().copied());
            defines.sort_unstable_by_key(|(cell, _)| *cell);
            defines.dedup_by_key(|(cell, _)| *cell);
            // Each unit's reads, each cell once (rule 21: the row moves when any unit's do).
            let mut reads: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(made.reads.len(), alloc);
            reads.extend(made.reads.iter().map(|id| (*id, crate::engine_scope_reads::today(&today, id).unwrap_or_else(|| view.cell(id)))));
            reads.sort_unstable_by_key(|(id, _)| *id);
            reads.dedup_by_key(|(id, _)| *id);
            let mut record_reads: std::vec::Vec<(Name, ContentId), Global> = std::vec::Vec::with_capacity_in(reads.len(), Global);
            record_reads.extend(reads.iter().map(|(id, digest)| (id.of, *digest)));
            record_reads.dedup_by_key(|(of, _)| *of);
            let name = task.planned.name;
            // Each head it could not freeze, under each unit: a head note a cell (a reader of
            // either unit's cell quotes its own), one finding a head by path and reason.
            heads_failed.extend(one.failed.iter().map(|&(cell, head, cause)| crate::engine_head_notes::Refused { name, cell: Some(cell), head, cause }));
            let mut said: SVec<'a, (&'static str, &'static str)> = SVec::with_capacity_in(one.failed.len(), alloc);
            said.extend(one.failed.iter().map(|(_, head, cause)| (*head, cause.group)));
            said.sort_unstable();
            said.dedup();
            let findings = u32::try_from(said.len()).unwrap_or(u32::MAX);
            // The counts: each (item, unit) once; an item frozen under 2+ units counted shared.
            let mut items = one.items;
            items.sort_unstable();
            items.dedup();
            counts.frozen = counts.frozen.saturating_add(u32::try_from(items.len()).unwrap_or(u32::MAX));
            for run in items.chunk_by(|a, b| a.0 == b.0) {
                counts.items = counts.items.saturating_add(1);
                if run.len() > 1 {
                    counts.shared = counts.shared.saturating_add(1);
                }
            }
            units.extend(items.iter().map(|(_, unit)| *unit));
            let out = made.out.finish();
            buffer.done.push(Done { name, key: task.key, out, defines, reads, findings, failed: false });
            self.made.push(Made { name, out, reads: record_reads, payload: one.payload });
        }
        units.sort_unstable();
        units.dedup();
        counts.units = u32::try_from(units.len()).unwrap_or(u32::MAX);
        // Summed over the request's Heads phases (the units: the most one phase froze under).
        // The jobs: summed over the phases, as the freezes (the most splits: the most of any).
        let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        self.units = UnitCounts {
            frozen: self.units.frozen.saturating_add(counts.frozen),
            items: self.units.items.saturating_add(counts.items),
            units: self.units.units.max(counts.units),
            shared: self.units.shared.saturating_add(counts.shared),
            tasks: self.units.tasks.saturating_add(count(total)),
            crate_jobs: self.units.crate_jobs.saturating_add(count(jobs.len())),
            split_jobs: self.units.split_jobs.saturating_add(count(splits.len())),
            busy: self.units.busy.saturating_add(count(busy)),
            busy_floor: self.units.busy_floor.saturating_add(count(workers.min(total))),
            most_splits: self.units.most_splits.max(count(most_splits)),
            split_files: self.units.split_files.saturating_add(count(split_files)),
            listings: listings.counts(),
        };
        self.refused.extend(heads_failed);
        self.made.sort_unstable_by_key(|one| one.name);
        Ok(())
    }
}

/// What a head task not begun because of a stop is refused for (engine_status.rs; as
/// engine_bodies.rs `run_bodies` refuses a body).
pub(crate) const NOT_BEGUN: &str = "not checked: the command was stopped by a signal before this head task began (engine_status.rs)";

/// Where a job's head tasks say they run (engine_status.rs `Board`): each task started and done
/// on the board by the one unit that shows it (`run_items`'s `shows`, by (name, prefix)), and
/// the stop asked before each task, in every unit.
#[derive(Clone, Copy)]
struct OnBoard<'b> {
    board: &'b crate::engine_status::Board<'b>,
    clock: &'b bstd::clock::Clock,
    shows: &'b [(Name, [u8; 8])],
}

impl OnBoard<'_> {
    /// Whether this unit shows task `name` on the board.
    fn shows(&self, name: Name, prefix: [u8; 8]) -> bool {
        self.shows.binary_search(&(name, prefix)).is_ok()
    }
}

/// A head task's label on the board: its entry's name as its text shows it, else the task.
fn board_label(entry_name: Option<&str>, name: Name) -> crate::engine_status::Label {
    match entry_name {
        Some(shown) if !shown.is_empty() => crate::engine_status::Label::of(shown),
        _ => crate::engine_status::Label::debug(&name),
    }
}

/// One split job's place in its crate's (file, name) order. Both ends are file boundaries
/// under the production rule.
#[derive(Clone, Copy, Debug)]
struct SplitAt {
    job: u32,
    from: u32,
    to: u32,
}

/// What a crate's split jobs share, made once a crate a phase by [`crate_setup`], each list
/// the request heap's so every split reads it in place on any worker. What cannot be shared:
/// the records themselves (a reader is one thread's, front2 source.rs `Records`), so a split
/// opens them again over the same bytes and builds its own tables and lang index from them (the
/// index holds only the lang rows; what building it reads is `lang_reads`, read here once).
#[derive(Clone, Copy)]
struct CrateSetup {
    /// The crates whose lang rows name the lang items.
    lang: &'static [usize],
    sized: Option<blaze_id::ItemKey>,
    /// The program's receiver pointer types (`Pointers::find`, once a crate).
    pointers: blaze_ladder::front2::receiver::Pointers,
    /// What the lang index read, each coarse (engine_heads_once.rs `JobCx::lang_reads`).
    lang_reads: &'static [CellId],
    /// What finding the pointers read, with `lang_reads` (engine_heads_once.rs `JobCx::pointer_reads`).
    pointer_reads: &'static [CellId],
    /// The scope names of the job's crates that are members, sorted.
    members: &'static [Name],
    /// The crate's listing, from the request's table.
    listing: &'static Listing,
}

/// A job's scope's records opened in `frame` (run_names.rs `names_records`): what the setup and
/// each split open, the one place a head job opens them.
#[expect(clippy::too_many_arguments, reason = "the names records' edge: each argument one input of them")]
fn open_scope<'n>(
    cx: &'n Cx,
    root: &'n bstd::fs::Dir<bstd::fs::ReadWrite>,
    crates: &[Crate<'n>],
    packs: &'static [blaze_fetch::PackView<'static>],
    corrected: &'static [(u32, &'static str, &'static str)],
    procs: Option<&'static crate::proc_runner::ProcRunner<'static>>,
    frame: &'n Frame,
    g: Alloc<'static>,
) -> Result<&'n Records<'n>, &'static str> {
    let opened = crate::run_names::names_records(cx, root, crates, packs, corrected, procs, std::vec::Vec::new_in(Global), None, frame, g).map_err(|_| "its crate's records could not be opened (run_names.rs `names_records`)")?;
    Ok(opened.records)
}

/// One member crate's setup for its head tasks, on a worker, once a crate a phase: its records
/// opened in the job's frame, its lang rows, the `Sized` item, its lang index's reads, its
/// receiver pointers and their reads, its member crates, and its listing from the request's
/// table (made here when no job made it yet: [`Listings::listing`]).
#[expect(clippy::too_many_arguments, reason = "a worker's edge: each argument one handle of it")]
fn crate_setup(
    cx: &Cx,
    root: &bstd::fs::Dir<bstd::fs::ReadWrite>,
    packs: &'static [blaze_fetch::PackView<'static>],
    corrected: &'static [(u32, &'static str, &'static str)],
    procs: Option<&'static crate::proc_runner::ProcRunner<'static>>,
    job: &ItemJob<'_>,
    subjects: &Subjects,
    view: View<'_, '_>,
    heap: &'static blaze_own::Heap,
    listings: &Listings,
) -> Result<CrateSetup, &'static str> {
    let frame = cx.frame();
    let g = heap.alloc();
    let records = open_scope(cx, root, &job.crates, packs, corrected, procs, frame, g)?;
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
    // The constant evaluator's lang index; what it read is read by every head that evaluates
    // (engine_head_consts.rs).
    let lang_recorder = Recorder::new(RecorderOpts { alloc: g });
    let index = crate::engine_head_consts::lang_index(base, &lang_recorder, lang).map_err(|_| "two crates of its scope declare one lang item, so its constants have no index (front2 index.rs `LangTwice`)")?;
    // The scope names of the job's crates that are members, sorted: every other crate's names
    // reads are kept at its record's `WHOLE` (engine_heads_once.rs `coarse`).
    let mut members: Vec<Name, Alloc<'static>> = Vec::new_in(g);
    members.extend(job.crates.iter().filter(|one| subjects.binary_search_by(|(p, _, _)| p.cmp(&one.records.key.prefix())).is_ok()).map(|one| blaze_drive::key::crate_scope_name(&one.records.key)));
    members.sort_unstable();
    members.dedup();
    let members: &'static [Name] = members.leak();
    let mut lang_reads: Vec<CellId, Alloc<'static>> = Vec::new_in(g);
    for one in lang_recorder.finish().map_err(|_| "its lang index read more names cells than the recorder holds (front2 tables/recorder.rs)")?.iter() {
        if let Some(id) = cell_id(base, *one).map_err(|_| "a names read of its lang index names a crate its scope does not hold (front2 tables/task.rs `cell_id`)")? {
            lang_reads.push(crate::engine_heads_once::coarse(members, names_read(view, id)));
        }
    }
    lang_reads.sort_unstable();
    lang_reads.dedup();
    // The program's receiver pointer types (`Box`, `Rc`, `Arc`, `Pin`), once a crate as front2
    // project.rs `ProjectWorld::new` finds them: without them every struct receiver
    // (`self: Pin<&mut Self>`) is a gap and `Future`'s and `Stream`'s dyn-compatibility verdicts
    // are `Gap` (lane FL2's finding, fixed by lane H9). What finding them read beyond the lang
    // index (alloc's `crate::rc::Rc` and `crate::sync::Arc` resolved from `Box`'s module) is
    // every cell's read (engine_heads_once.rs `JobCx::pointer_reads`); the lang rows they take are
    // `lang_reads`', which every cell then reads too.
    let pointer_recorder = Recorder::new(RecorderOpts { alloc: g });
    let pointers = blaze_ladder::front2::receiver::Pointers::find(&index, blaze_ladder::resolve::Paths { names: base.reading_into(&pointer_recorder) });
    let mut pointer_reads: Vec<CellId, Alloc<'static>> = Vec::new_in(g);
    pointer_reads.extend_from_slice(&lang_reads);
    for one in pointer_recorder.finish().map_err(|_| "finding its receiver pointer types read more names cells than the recorder holds (front2 tables/recorder.rs)")?.iter() {
        if let Some(id) = cell_id(base, *one).map_err(|_| "a names read of finding its receiver pointer types names a crate its scope does not hold (front2 tables/task.rs `cell_id`)")? {
            pointer_reads.push(crate::engine_heads_once::coarse(members, names_read(view, id)));
        }
    }
    pointer_reads.sort_unstable();
    pointer_reads.dedup();
    // The crate at 0 is this job's unit (run_names.rs `names_records` opens the job's own first).
    let crate_key = NamesSource::crate_key(records, 0).ok_or("its crate has no key in its own records (front2 source.rs `crate_key`)")?;
    if crate_key.prefix() != job.prefix {
        return Err(NOT_OWN_FIRST);
    }
    let texts = job.crates.first().filter(|one| one.records.key == crate_key).map(|one| one.records.expansions).ok_or("its crate is not the first of the crates its records were opened over (run_names.rs `names_records`)")?;
    let listing = listings.listing(records, &texts, 0, crate_key, subjects)?;
    Ok(CrateSetup { lang, sized, pointers, lang_reads: lang_reads.leak(), pointer_reads: pointer_reads.leak(), members, listing })
}

/// One run of a member crate's head tasks (`tasks`, a split of `job`'s: [`split_count`]) on a
/// worker: its scope's records opened again in the job's frame over the crate's `setup`, each
/// task's heads frozen (front2 tables/head.rs `head_task`) through the split's one source, so
/// each cell's read list is its own: every names cell its freeze consulted (the recorder), and
/// for each head its freeze read, the cell of the head task that covers it (a same-stage read:
/// the engine enters the reader at the wave after its definer, engine.rs), or its crate's names
/// record. A panic here fails this split's tasks alone (`run_items`).
#[expect(clippy::too_many_arguments, reason = "a worker's edge: each argument one handle of it")]
fn heads_of(
    cx: &Cx,
    root: &bstd::fs::Dir<bstd::fs::ReadWrite>,
    packs: &'static [blaze_fetch::PackView<'static>],
    corrected: &'static [(u32, &'static str, &'static str)],
    procs: Option<&'static crate::proc_runner::ProcRunner<'static>>,
    job: &ItemJob<'_>,
    setup: &CrateSetup,
    tasks: &[Task],
    subjects: &Subjects,
    view: View<'_, '_>,
    expansion_reads: bool,
    heap: &'static blaze_own::Heap,
    once: &crate::engine_heads_once::HeadsOnce,
    listings: &Listings,
    plant: ListingsPlant,
    on_board: OnBoard<'_>,
) -> Result<std::vec::Vec<Result<WorkedHead, HeadFailed>, Global>, &'static str> {
    let frame = cx.frame();
    let g = heap.alloc();
    let records = open_scope(cx, root, &job.crates, packs, corrected, procs, frame, g)?;
    let CrateSetup { lang, sized, pointers, lang_reads, pointer_reads, members, listing } = *setup;
    let unused = Recorder::new(RecorderOpts { alloc: g });
    let base = Tables::new(TablesOpts { records, impls: None, lang_crates: lang, reads: &unused });
    // The split's own lang index over its own records (the index borrows them); what building
    // it reads is the setup's `lang_reads`, so this recorder's are dropped.
    let index_recorder = Recorder::new(RecorderOpts { alloc: g });
    let index = crate::engine_head_consts::lang_index(base, &index_recorder, lang).map_err(|_| "two crates of its scope declare one lang item, so its constants have no index (front2 index.rs `LangTwice`)")?;
    let crate_key = NamesSource::crate_key(records, 0).ok_or("its crate has no key in its own records (front2 source.rs `crate_key`)")?;
    let texts = job.crates.first().filter(|one| one.records.key == crate_key).map(|one| one.records.expansions).ok_or("its crate is not the first of the crates its records were opened over (run_names.rs `names_records`)")?;
    let scope_name = blaze_drive::key::crate_scope_name(&crate_key);
    let mut owners = Owners::new(subjects, &job.crates[..], listings, g);
    let tool = blaze_drive::key::Tool(ContentId::of(HEAD_TOOL_DOMAIN, b""));
    // ONE SOURCE A SPLIT over the request's table (engine_heads_once.rs): every names read by
    // the cell it is made for, so each cell's reads are its own.
    let owned = || blaze_ladder::front2::tables::owned_reads::OwnedRecorder::new(blaze_ladder::front2::tables::owned_reads::OwnedRecorderOpts { alloc: g, owning: blaze_ladder::front2::tables::owned_reads::Owning::Each });
    let (freeze_names, eval_names) = (owned(), owned());
    let both = blaze_ladder::front2::tables::owned_reads::BothOwners { names: &freeze_names, evals: &eval_names };
    let shared = base.reading_into(&freeze_names);
    // An entry of the crate (any split's): the whole crate's listing, not this split's tasks.
    let same_crate = |of: Name| listing.holds_entry(of);
    let job_cx = crate::engine_heads_once::JobCx {
        base,
        shared,
        view,
        records,
        subjects,
        crates: &job.crates[..],
        index: &index,
        lang_reads,
        sized,
        pointers,
        pointer_reads,
        frame,
        g,
        tool,
        once,
        names: &freeze_names,
        evals: &eval_names,
        attempt_reads: core::cell::RefCell::new(blaze_hash::HashMap::new_in(g)),
        engine_refusal: core::cell::Cell::new(None),
        same_crate: &same_crate,
        members,
    };
    let mut once_consts = crate::engine_heads_once::OnceConsts { cx, job: &job_cx };
    let job_heads = crate::engine_heads_once::JobHeads { cx, job: &job_cx };
    let mut source = SeededHeads::new(SeededHeadsOpts { names: shared, sized, pointers, frame, scratch: g, alloc: g, consts: Some(&mut once_consts as &mut dyn ConstValues<'_>), known: Some(&job_heads), owners: Some(&both) });
    let mut done = std::vec::Vec::with_capacity_in(tasks.len(), Global);
    for (at_task, task) in tasks.iter().enumerate() {
        let name = task.planned.name;
        // The workers' scheduling point (engine_status.rs): stopped, this task and the rest are
        // not begun; `run_items` refuses them (owed) and the driver stops before the merge.
        if on_board.board.stopping().is_some() {
            let rest = tasks[at_task..].iter().filter(|one| on_board.shows(one.planned.name, job.prefix)).count();
            on_board.board.refused(Stage::Heads, rest as u64);
            break;
        }
        // The crate's listing: the setup's, or under the plant one made for this task alone
        // (the control of engine_owners_once_tests.rs).
        let listing = match plant {
            ListingsPlant::Rule => listing,
            ListingsPlant::PerAsk => listings.listing(records, &texts, 0, crate_key, subjects)?,
        };
        let mine = listing.of_entry(name);
        // Shown on the board by the one unit that shows it, labelled by its entry's name.
        let running = on_board.shows(name, job.prefix).then(|| {
            let entry_name = mine.first().map(|one| one.label.entry_name);
            on_board.board.show(Stage::Heads, board_label(entry_name, name), frame.used() as u64, on_board.board.now(on_board.clock))
        });
        let mut out = blaze_ids::Hasher::new(HEAD_OUT_DOMAIN);
        let mut worked = WorkedHead {
            out: ContentId([0; 32]),
            defines: std::vec::Vec::new_in(*job.tasks.allocator()),
            items: SVec::new_in(heap.shared()),
            reads: std::vec::Vec::new_in(*job.tasks.allocator()),
            payload: std::vec::Vec::new_in(*job.tasks.allocator()),
            failed: SVec::new_in(heap.shared()),
        };
        let mut failed: Option<&'static str> = None;
        // How long `worked.reads` was when it was last settled (`once_each`).
        let mut settled = 0usize;
        // Two heads under one cell would be two values under one key (rule 21): the whole
        // entry is refused, the two heads named as its text names them.
        if let Some(two) = mine.windows(2).find(|two| two[0].cell == two[1].cell) {
            let (one, other) = (two[0].label, two[1].label);
            failed = Some(crate::engine_head_notes::leak(g, format_args!("two heads of its entry fall under one cell: {one} and {other}, the same kind and name (engine_heads.rs `head_cell`)")));
        }
        let mine = if failed.is_some() { &mine[..0] } else { mine };
        // The expansion record of each call under its entry (`CrateRow::Call`): the heads its
        // calls write are made from what the records hold, so a macro's new output (its
        // `macro_rules!` body edited, a derive's crate changed) moves this task and no other;
        // read even when they write no head yet, so a call that comes to write one reruns it.
        // A record the store lacks refuses the task by name: a head task never runs a macro
        // again (rule 3).
        for (_, call) in listing.calls_of(name).iter() {
            if !expansion_reads {
                break;
            }
            if let Err(error) = texts.text(*call) {
                failed.get_or_insert_with(|| crate::engine_head_notes::leak(g, format_args!("a macro call under its entry has no expansion record to make its heads from: {error} (a head task never runs a macro again; engine_heads.rs `heads_of`)")));
            }
            worked.reads.push(names_read(view, CellId { of: scope_name, cell: Cell(blaze_scope::names_cells::NameCell::Expansion { call: *call }.key()) }));
        }
        let mine = if failed.is_some() { &mine[..0] } else { mine };
        for head in mine.iter() {
            let (cell, want) = (&head.cell, &head.want);
            worked.items.push((head.item, job.prefix));
            // Frozen through the split's one source, its reads its cells' own (engine_heads_once.rs
            // `one_head`; a head that does not freeze is frozen again alone, as before).
            let made = crate::engine_heads_once::one_head(&job_cx, &mut source, &mut owners, *want, name, &mut |id| worked.reads.push(id), &mut failed);
            if let Some(why) = job_cx.engine_refusal.get() {
                return Err(why);
            }
            // Each cell once so far, whenever the list has doubled since it last was.
            if worked.reads.len() > settled.saturating_mul(2).max(SETTLE_AT) {
                once_each(&mut worked.reads);
                settled = worked.reads.len();
            }
            let cell = *cell;
            // A head not frozen fails alone (module docs): its cell the digest of its reason.
            let made = match made {
                Ok(made) => made,
                Err(cause) => {
                    let head = crate::engine_head_consts::head_label(records, *want, g);
                    let digest = cause.digest(HEAD_FAILED_DOMAIN);
                    worked.defines.push((cell, digest));
                    out.update(&cell.0).update(&digest.0);
                    worked.failed.push((cell, head, cause));
                    continue;
                }
            };
            worked.defines.push((cell, made.out));
            out.update(&cell.0).update(&made.out.0);
            // The cell too: a body finds a member's head by it (engine_seeds.rs `StretchHeads`).
            worked.payload.extend_from_slice(&want_bytes(*want));
            worked.payload.extend_from_slice(&cell.0);
            worked.payload.extend_from_slice(&u32::try_from(made.bytes.len()).unwrap_or(u32::MAX).to_le_bytes());
            worked.payload.extend_from_slice(&made.bytes);
        }
        // A read offered with no owner, or past a cap, fails the task by name (rule 3).
        if let Some(why) = crate::engine_heads_once::job_refusal(&freeze_names, &eval_names, g) {
            failed.get_or_insert(why);
        }
        // What the phase keeps of this task until its row is written: each cell once, in a
        // block of that size.
        if failed.is_some() {
            // Even a refusal before the first freeze depends on its member's names record.
            worked.reads.push(job.cells[1]);
        }
        once_each(&mut worked.reads);
        worked.reads.shrink_to_fit();
        once.read_list(&worked.reads);
        done.push(match failed {
            Some(why) => Err(HeadFailed { why, reads: worked.reads }),
            None => {
                worked.out = out.finish();
                Ok(worked)
            }
        });
        if let Some(running) = running {
            running.end(on_board.board.now(on_board.clock));
        }
    }
    // Every lowering the source made, its constant evaluator's too (front2 head.rs
    // `Frozen::eval_lowered`): the work the stage did.
    once.job_done(source.total_lowered(), source.taken);
    Ok(done)
}

/// The most heads one head's freeze is followed through ([`heads_of`], rule 18): past it the
/// task fails by name.
pub(crate) const MOST_HEAD_READS: usize = 1 << 12;

/// A task's reads so far, each cell once. A read list is a set (`run_items` sorts and dedups
/// its row's; a refusal's goes through `failed_with`), and a head's freeze pushes one entry a
/// names read: thousands, which land on a few records' `WHOLE` cells (`names_read`). Left as
/// pushed, the lists of a phase's tasks were 4.6 GB of the 9.0 GB live four minutes into a real
/// workspace's check (tempo, 2026-11-17: 1125 lists of about 4 MB each, growing until the
/// memory guard stopped the run).
fn once_each<A: core::alloc::Allocator>(reads: &mut std::vec::Vec<CellId, A>) {
    reads.sort_unstable();
    reads.dedup();
}

/// A read list is settled ([`once_each`]) when it passes this many entries, and then each time
/// it doubles, so the sorting stays linear in what was pushed.
const SETTLE_AT: usize = 1 << 10;
