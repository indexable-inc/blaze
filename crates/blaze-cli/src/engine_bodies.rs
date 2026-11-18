//! The `Bodies` stage of `blaze engine-check` (engine.md E5/E6, "The design" 4): every body of
//! the workspace's files type-checked and borrow-checked as ITS OWN engine task, entered once, on
//! the pool, by lane VW's `check_body` (front2 tables/task.rs) over the read-only view.
//!
//! THE PLAN, from text: one `BodyCheck` task per body part of each file's outline (work.rs
//! `outline_file`: one part per entry with a body of its own, and per entry a macro takes), its
//! name and source the outline's (`body_name`, `body_source`: the item's text), so the task's
//! key is known before any phase runs ([`plan`]).
//!
//! THE PHASE, on the coordinator first (a constant per member crate, never a step per body):
//! the crate list (the `UnitGraph` record), its trees, and for each member crate its check scope
//! (blaze-world check_scope.rs: the one rule of which crates a body sees), each scope crate's
//! names and items records read from the store as the Names stage put them, and the member's
//! files (its items record's file rows) matched to the planner's outlines. Each body's name is
//! then matched to its item key through the records' listing (front2 project_records.rs
//! `list_with`, `index_by_chain`): the item key is what `check_body` finds a body by.
//!
//! THE RUNS: the bodies of one member cut into runs of at most [`RUN`], each run a task of one
//! bstd scope; a run opens its own `Records` over the shared bytes (`Records` opens lazily
//! through `RefCell`s: never shared between threads, view.rs), and every body of the run is
//! checked over one `Tables` with its own recorder (task.rs). Nothing mutable is shared between
//! bodies. A panic fails its body alone (rule 20).
//!
//! WHAT A BODY READS, as the engine keys its readers: the view records names, lang, head and
//! impl-bucket cells. A crate's `ImplBucket` cell is read as read: the Heads stage defines it
//! (engine_heads.rs), or states its absence (no impl of the crate in that bucket). Until the
//! Names stage defines cells finer than its record's one whole cell (E4's wiring), the other
//! reads are WIDENED to a cell some phase defines ([`widen`]): a `CrateScope` cell to that
//! record's whole cell, the whole graph's bucket fact (made from the names records) to every
//! scope crate's. A `Signature` read of a member's head is the cell of the head task of the
//! entry that covers it (engine_heads.rs `Owners::place`; the body still makes the heads it
//! needs in its own store, task.rs), so a signature edit enters exactly the bodies whose head
//! read moved; a dependency's head, or one a macro wrote, is its crate's names record whole.
//! Widened reads
//! are a superset of the true ones: an edit can enter a body that did not need it, never miss
//! one. So `Make` made nothing inline here: every cell a body reads is the engine's.

use core::convert::Infallible;
use core::num::NonZeroU32;

use blaze_drive::cells::Cell;
use blaze_drive::key::ABSENT;
use blaze_drive::{Input, Kind, Locate, Name};
use blaze_engine::{Buffer, CellId, Done, Made, Phase, Planned, SVec, Stage, Task, View};
use blaze_id::ItemKey;
use blaze_ids::{ContentId, Hasher};
use blaze_ladder::front2::buckets::{ImplShelf, ImplShelfOpts, TableSet};
use blaze_ladder::front2::heads::NeedsMet;
use blaze_ladder::front2::project::BodyRef;
use blaze_ladder::front2::project_records::{BodySource, Listing, index_by_chain, list_with};
use blaze_ladder::front2::source::NamesSource;
use blaze_ladder::front2::tables::recorder::{Recorder, RecorderOpts};
use blaze_ladder::front2::tables::task::{BodyFrame, Make, MakeRefused, Said as Line, SaidWhat, TaskError, check_body};
use blaze_ladder::front2::tables::view::{Tables, TablesOpts};
use blaze_own::{SharedAlloc, Vec};
use blaze_scope::crate_env::EnvLent;
use blaze_scope::items_record::ItemsRecord;
use blaze_world::crate_list::CrateGraph;
use bstd::clock::Clock;
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::Frame;
use bstd::task::Class;
use std::alloc::Global;

use crate::CliError;
use crate::body_types::Kept;
use crate::engine_check::EngineCheckError;
use crate::engine_names::NamesStage;
use crate::engine_records::EngineRecords;
use crate::engine_tasks::{CELLS, Outlined, WHOLE};
use crate::fetch::FetchEnv;
use crate::run_names::Crate;
use crate::work;

/// Where the rule that refuses a body on this path is.
const RULE: &str = "engine_bodies.rs";

/// The most bodies one run checks: a run opens its own records once, and its frame holds every
/// body's lowering until the run ends (a body's frame is the records' lifetime in `check_body`,
/// so it cannot be reset per body yet: see the report's brief), so runs are short.
const RUN: usize = 32;

/// A run's frame: its records' decoded parts and its bodies' lowerings (reserved, not touched).
const BODIES_FRAME: usize = 1 << 30;

/// The domain of the out of a body no crate of the graph holds.
const OWNERLESS_DOMAIN: &str = "blaze-cli engine bodies ownerless 1";

/// The domain of the verdict of an entry whose expansion wrote several bodies (or none).
const ENTRY_DOMAIN: &str = "blaze-cli engine bodies expansion entry 1";

/// One `BodyCheck` task a body part of each outlined file, sorted by name (one per name).
pub(crate) fn plan<'h>(outlined: &[Outlined<'_>], alloc: SharedAlloc<'h>) -> SVec<'h, Planned> {
    let mut planned: SVec<'h, Planned> = SVec::new_in(alloc);
    for one in outlined.iter() {
        let Ok(record) = &one.made else { continue };
        for part in record.parts.iter() {
            if let (Kind::BodyCheck, Locate::Source(source)) = (part.name.kind, part.locate) {
                let source = crate::engine_tasks::edition_source(source, one.edition);
                planned.push(Planned { name: part.name, phase: Phase::new(Stage::Bodies, 0), source, file: one.name });
            }
        }
    }
    planned.sort_unstable_by_key(|one| one.name);
    planned.dedup_by_key(|one| one.name);
    planned
}

/// Why a body gave no verdict: each named, printed as a note (rule 3: never a quiet pass).
#[derive(Debug)]
pub(crate) enum Said {
    /// `check_body` refused the task (a fault of the plan or the view, task.rs `TaskError`).
    Task(TaskError),
    /// The body's check panicked: this body alone failed (rule 20).
    Panicked,
    /// The body's check panicked with its stretch's frame past half its reserve: the frame was
    /// used up ([`BODIES_FRAME`]); this body alone failed, and the next body gets a new frame.
    FrameUsedUp { used: u64, reserved: u64 },
    /// This stage could not give the body a check, and why.
    Refused(&'static str),
    /// The checker gave a verdict that refused the body or left a gap in it (task.rs `Done`
    /// with `failed`): the body was not checked whole. Its verdict's words why (task.rs
    /// `BodyOut::said`'s `NotChecked` and `BorrowsNotChecked` lines, said.rs's words), each
    /// line "<cause>: <words>", joined by "; " ("" when the verdict said none).
    Unchecked(&'static str),
}

/// One body as the phase took it: what the printer and the profile read.
pub(crate) struct Ran<'f> {
    pub(crate) name: Name,
    /// Its item's name as written ("" when it was not found).
    pub(crate) label: &'f str,
    /// Its file: an index into the planner's outlines.
    pub(crate) file: u32,
    /// Its check's time on its worker, ns (0 when it was not checked).
    pub(crate) ns: u64,
    /// How many bytes its check grew its stretch's frame by (0 when it was not checked): what
    /// the profile's largest-frame line reads.
    pub(crate) bytes: u64,
    /// What the engine core is told (`Done::findings`): the verdict's type, lint and borrow
    /// findings, or 1 for a body refused.
    pub(crate) findings: u32,
    pub(crate) said: Option<Said>,
    /// Its entry's chain (work.rs `item_chain`; 0 when it was not found).
    chain: u64,
    /// Its verdict's findings, sorted by (start, end, rule, message): printed as `check` prints
    /// them ([`BodiesStage::lines`]).
    found: Vec<BodyFinding, Global>,
}

/// A body found in its crate's records: its name and its item's name as written.
struct Took<'f> {
    name: Name,
    label: &'f str,
    /// Its member crate (an index of [`Group`]s) and the item keys it checks (its own body's,
    /// or each body an expansion at its entry wrote), or why it has none.
    item: Result<(u32, &'f [ItemKey]), &'static str>,
    /// Its entry's place: the entry's chain (work.rs `item_chain`) and, for an entry whose
    /// bodies a macro wrote, each one's label beside its key ([`Place`]).
    place: Place<'f>,
}

/// Where a body task's findings are printed: in the entry's item, found by its chain in its
/// file's outline as it is when printed (engine_lines.rs, a placed finding; rule 71).
#[derive(Clone, Copy)]
struct Place<'f> {
    chain: u64,
    /// Empty for the entry's own body (its findings' ranges are from the item's start); else
    /// each body an expansion at the entry wrote, by its label (body_types/covered.rs
    /// `written_label`), in the order of its keys: their text's place in the file is not known,
    /// so each finding is at the item's start, said so (covered.rs `WRITTEN_UNPLACED`).
    written: &'f [&'f str],
}

/// One finding of a body task, as `check` files it (body_types.rs, where a body's findings are
/// filed): its rule, its range from its item's start, and its message's pieces joined.
struct BodyFinding {
    rule: u16,
    start: u32,
    end: u32,
    message: Vec<u8, Global>,
}

/// The item key `check_body` finds a listed body by: a free fn's own, a member's own key.
fn key_of<'n>(records: &'n blaze_ladder::front2::source::Records<'n>, body: BodyRef<'n>) -> Option<ItemKey> {
    match body {
        BodyRef::Free(key) => Some(key),
        BodyRef::Member { container, name, .. } => {
            let mut key = None;
            NamesSource::each_member(records, container, &mut |m| {
                if m.name == Some(name) {
                    key = Some(m.key);
                }
            });
            key
        }
    }
}

/// One member crate's check scope as a run reads it: all read-only, shared by its runs. Its
/// crates' records are borrowed in place from the store's mapping for `'r` (engine_records.rs).
struct Group<'f, 'r> {
    /// The member's own `CrateScope` name.
    own: Name,
    /// Its scope's crates in reader order, the member at 0.
    crates: Vec<Crate<'r>, Global>,
    /// The member's environment, what its bodies' `env!` read: its crate list facts with its
    /// build script's outputs joined (body_types.rs `owner_env_by`), lent to each run, and the
    /// script run read for it (a `Script` task's WHOLE cell), in every body's read list.
    env: Option<EnvLent<'f, 'f>>,
    script: Option<Name>,
    /// The places of its lang crates among `crates`.
    lang: Vec<usize, Global>,
    /// Each scope crate's `CrateScope` name, sorted: the whole graph's bucket fact widens to these.
    scope: Vec<Name, Global>,
}

/// One crate's check scope with its records opened: what the Heads stage files a crate's impls
/// over and the Bodies stage checks a member's bodies over ([`scope_of`], the one rule of both).
pub(crate) struct Scope<'r> {
    /// The crate's own records.
    pub(crate) own_records: crate::front_load::CrateRecords<'r>,
    /// Its scope's crates in reader order, the crate at 0 (cut at the first without records).
    pub(crate) crates: Vec<Crate<'r>, Global>,
    /// The places of its lang crates among `crates`.
    pub(crate) lang: Vec<usize, Global>,
    /// Each scope crate's `CrateScope` name, sorted.
    pub(crate) scope: Vec<Name, Global>,
    /// The first scope crate without records, by its world index, when `crates` was cut there:
    /// what a refusal names (rule 19, plan row A8).
    pub(crate) cut: Option<usize>,
}

/// Crate `at`'s check scope (blaze-world check_scope.rs `check_scope_in`), each crate's
/// records read in place through `read` (engine_records.rs), and whether every crate of it had
/// records (`false`: a `Names` task failed, and the caller refuses by name). `None`: the crate's
/// own names record or its scope could not be read.
#[expect(clippy::too_many_arguments, reason = "a stage's handles, each named")]
pub(crate) fn scope_of<'r>(read: &mut EngineRecords<'r>, cx: &Cx, store: &mut dyn crate::store::Store<'r>, names: &NamesStage<'_>, list: &blaze_world::crate_list::CrateList<'static>, at: usize, heap: &'static blaze_own::Heap) -> Option<(Scope<'r>, bool)> {
    let g = heap.alloc();
    let input_at = |at: usize| names.input_of(blaze_drive::key::crate_scope_name(&list.key(at)));
    let own = input_at(at)?;
    let own_records = read.records(cx, store, &own, list.key(at), g).ok()?;
    let no_core = blaze_scope::names_record::NamesRecord::open(own_records.names_proof).crate_root().head().no_core;
    let checked = blaze_world::check_scope::check_scope_in(list, at, no_core, g).ok()?;
    let mut scope = Scope { own_records, crates: Vec::new_in(Global), lang: Vec::new_in(Global), scope: Vec::new_in(Global), cut: None };
    scope.lang.extend(checked.lang_places());
    let mut whole = true;
    for one in checked.crates() {
        let Some(input) = input_at(one) else {
            whole = false;
            scope.cut = Some(one);
            break;
        };
        let Ok(records) = read.records(cx, store, &input, list.key(one), g) else {
            whole = false;
            scope.cut = Some(one);
            break;
        };
        scope.crates.push(Crate { records, name: CrateGraph::name(list, one), package: list.shown_from(one), world_at: one });
        scope.scope.push(input.name);
    }
    scope.scope.sort_unstable();
    Some((scope, whole))
}

/// One body a run checks.
#[derive(Clone, Copy)]
struct One<'f> {
    task: Task,
    /// The bodies it checks: one, or each an expansion at its entry wrote (none: a clean pass).
    items: &'f [ItemKey],
    group: u32,
    file: u32,
    label: &'f str,
    place: Place<'f>,
}

/// The phase-independent handles the Bodies phase reads.
pub(crate) struct Handles<'h, 'e, 'n, 'o> {
    pub(crate) root: &'h Dir<ReadWrite>,
    pub(crate) env: &'h FetchEnv<'e>,
    /// The `UnitGraph`'s record as a reader takes it; `None`: the command has no graph.
    pub(crate) graph: Option<Input>,
    pub(crate) names: Option<&'h NamesStage<'n>>,
    pub(crate) outlined: &'h [Outlined<'o>],
    pub(crate) workers: NonZeroU32,
    pub(crate) clock: &'h Clock,
}

/// The packs of the crate list's trees and its corrected files (body_types.rs `Kept::trees`).
type Trees = (&'static [blaze_fetch::PackView<'static>], &'static [(u32, &'static str, &'static str)]);

/// What [`BodiesStage::groups`] found: each member's group, each body's place, the files of
/// member crates, and the members' names.
struct Found<'f, 'r> {
    groups: Vec<Group<'f, 'r>, Global>,
    /// One a body of the project: in the request's heap, never the command's fixed frame.
    took: Vec<Took<'f>, blaze_own::Alloc<'static>>,
    /// Each outline (by index) a member crate's records list, or why its bodies were not checked.
    files: Vec<(u32, Option<&'static str>), Global>,
    members: Vec<Name, Global>,
    /// Each member crate's outlined files by its key's prefix and its items record's file
    /// index, sorted: where a head read finds its head task (engine_heads.rs `Owners`, [`widen`]).
    heads: Vec<([u8; 8], u32, Name), Global>,
    trees: Trees,
}

/// The stage: the crates' records read once a command, and every body's result.
pub(crate) struct BodiesStage<'f> {
    /// The records read so far, shared with the Heads stage (engine_heads.rs).
    pub(crate) kept: Kept,
    /// Every crate's impl tables, put by the Heads stage (engine_heads.rs) and read only from
    /// the Bodies phase on: each run lends it (`TableSet::lent`).
    pub(crate) shelf: ImplShelf,
    /// Every body entered, sorted by name after its phase.
    pub(crate) ran: Vec<Ran<'f>, Global>,
    /// Bodies of files no member crate of the graph holds: checked by no crate, counted.
    pub(crate) ownerless: u32,
    /// Their files, by outline index, sorted and once each.
    ownerless_files: Vec<u32, Global>,
    /// The phase's wall time, ns, and the runs it was cut into.
    pub(crate) wall_ns: u64,
    pub(crate) runs: u32,
}

impl<'f> BodiesStage<'f> {
    pub(crate) fn new() -> BodiesStage<'f> {
        BodiesStage { kept: Kept::default(), shelf: ImplShelf::new(ImplShelfOpts {}), ran: Vec::new_in(Global), ownerless: 0, ownerless_files: Vec::new_in(Global), wall_ns: 0, runs: 0 }
    }

    /// The paths of the first files (8 at most) whose bodies no member crate holds.
    pub(crate) fn ownerless_paths(&self, outlined: &[Outlined<'_>]) -> std::vec::Vec<std::string::String, Global> {
        let mut paths = std::vec::Vec::new_in(Global);
        for at in self.ownerless_files.iter().take(8) {
            paths.push(outlined.get(*at as usize).map_or_else(|| std::string::String::from("?"), |one| std::string::String::from_utf8_lossy(&one.path).into_owned()));
        }
        paths
    }

    /// Whether every body entered was checked: none refused, none in a file no crate holds.
    pub(crate) fn whole(&self) -> bool {
        self.ownerless == 0 && self.ran.iter().all(|one| one.said.is_none())
    }

    /// One Bodies phase: every task entered once, its result in `buffer`.
    #[expect(clippy::too_many_arguments, reason = "a phase's edge: each argument one handle of it")]
    pub(crate) fn run_phase<'a, 'r>(
        &mut self,
        cx: &Cx,
        store: &mut dyn crate::store::Store<'r>,
        read: &mut EngineRecords<'r>,
        handles: &Handles<'_, '_, '_, '_>,
        tasks: &[Task],
        view: View<'_, 'a>,
        buffer: &mut Buffer<'a>,
        times: &mut crate::engine_times::Times,
        alloc: SharedAlloc<'a>,
        frame: &'f Frame,
    ) -> Result<(), CliError> {
        let cx = cx.span("engine: one bodies phase");
        let began = handles.clock.now();
        let heap = handles.env.heap;
        // 1. Each member's check scope and files, and each body's item key, on this thread.
        let found = self.groups(&cx, store, read, handles, frame)?;
        let (found, stopped) = match found {
            Ok(found) => (Some(found), None),
            Err(why) => (None, Some(why)),
        };
        // 2. Each task: a body to check, or its result now.
        let mut ones: Vec<One<'f>, Global> = Vec::new_in(Global);
        for task in tasks.iter() {
            let name = task.planned.name;
            let file = handles.outlined.binary_search_by(|one| one.name.cmp(&task.planned.file)).map_or(u32::MAX, |at| u32::try_from(at).unwrap_or(u32::MAX));
            let refused = match (stopped, found.as_ref()) {
                (Some(why), _) => why,
                (None, None) => "the stage's crates were not read (engine_bodies.rs `groups`)",
                (None, Some(found)) => {
                    let took = found.took.binary_search_by_key(&name, |one| one.name).ok().map(|at| &found.took[at]);
                    let member_file = found.files.iter().find(|(at, _)| *at == file).map(|(_, why)| *why);
                    match (took, member_file) {
                        (Some(one), _) => match one.item {
                            Ok((group, items)) => {
                                ones.push(One { task: *task, items, group, file, label: one.label, place: one.place });
                                continue;
                            }
                            Err(why) => why,
                        },
                        (None, Some(Some(why))) => why,
                        (None, Some(None)) => "its crate's records list no entry with a body at this name (a `cfg` left it out, or a macro wrote it)",
                        // No member crate's records hold the body's file: checked by no crate. It
                        // reads every member's names record, so a `mod` taking the file in enters it.
                        (None, None) if found.members.is_empty() => "no member crate's records could be read, so no crate holds this body's file",
                        (None, None) => {
                            self.ownerless += 1;
                            if let Err(slot) = self.ownerless_files.binary_search(&file) {
                                self.ownerless_files.insert(slot, file);
                            }
                            let mut reads: SVec<'a, (CellId, ContentId)> = SVec::new_in(alloc);
                            for member in found.members.iter() {
                                let id = CellId { of: *member, cell: WHOLE };
                                reads.push((id, view.cell(&id)));
                            }
                            let out = ContentId::of(OWNERLESS_DOMAIN, &name.subject.0);
                            buffer.done.push(Done { name, key: task.key, out, defines: SVec::new_in(alloc), reads, findings: 0, failed: false });
                            self.ran.push(Ran { name, label: "", file, ns: 0, bytes: 0, findings: 0, said: None, chain: 0, found: Vec::new_in(Global) });
                            continue;
                        }
                    }
                }
            };
            buffer.done.push(crate::engine_tasks::failed(task, alloc));
            self.ran.push(Ran { name, label: "", file, ns: 0, bytes: 0, findings: 1, said: Some(Said::Refused(refused)), chain: 0, found: Vec::new_in(Global) });
        }
        // 3. Your code first is the next piece (engine.md 4); today: by member, file, name.
        ones.sort_unstable_by_key(|one| (one.group, one.file, one.task.planned.name));
        let mut runs: Vec<&[One<'f>], Global> = Vec::new_in(Global);
        let mut from = 0;
        while from < ones.len() {
            let group = ones[from].group;
            let mut to = from;
            while to < ones.len() && ones[to].group == group && to - from < RUN {
                to += 1;
            }
            runs.push(&ones[from..to]);
            from = to;
        }
        self.runs = u32::try_from(runs.len()).unwrap_or(u32::MAX);
        let mut buffers: Vec<(Buffer<'a>, Vec<Ran<'f>, Global>), Global> = Vec::new_in(Global);
        for _ in 0..runs.len() {
            buffers.push((Buffer::new_in(alloc), Vec::new_in(Global)));
        }
        let Some(found) = found.as_ref() else {
            // Nothing to run: every task's result is in `buffer` above.
            self.ran.sort_unstable_by_key(|one| one.name);
            return Ok(());
        };
        let (packs, corrected) = found.trees;
        let procs = handles.names.and_then(|stage| stage.names.as_ref()).and_then(|names| names.procs());
        let shared = Shared { root: handles.root, packs, corrected, procs, groups: &found.groups, heads: &found.heads, view, heap, shelf: &self.shelf };
        {
            let scope = cx.scope();
            scope.narrow_workers(handles.workers, Class::Foreground);
            // One handle a run: as many as the project's bodies over `RUN`, so in the heap.
            let mut running = Vec::new_in(heap.alloc());
            for (run, (out, ran)) in runs.iter().zip(buffers.iter_mut()) {
                let shared = &shared;
                let clock = handles.clock.clone();
                running.push(scope.may_overlap("engine bodies run", BODIES_FRAME, move |cx: &Cx| {
                    run_bodies(cx, shared, run, &clock, out, ran, alloc);
                    Ok::<(), Infallible>(())
                }));
            }
            let mut failed = None;
            for handle in running.into_iter() {
                if let Err(error) = handle.join(&cx) {
                    failed.get_or_insert(error);
                }
            }
            scope.join(&cx).map_err(CliError::Pool)?;
            if let Some(error) = failed {
                return Err(CliError::Pool(error));
            }
        }
        for (mut out, ran) in buffers.into_iter() {
            buffer.done.append(&mut out.done);
            buffer.made.append(&mut out.made);
            for one in ran.iter() {
                times.push(one.name, one.ns);
            }
            self.ran.extend(ran);
        }
        self.ran.sort_unstable_by_key(|one| one.name);
        self.wall_ns = self.wall_ns.saturating_add(handles.clock.now().since(began).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)));
        Ok(())
    }

    /// Each member crate's [`Group`] and each body of its files as a [`Took`]; `Err(why)` when
    /// no body can be checked this command (the stop is every body's note).
    fn groups<'r>(&mut self, cx: &Cx, store: &mut dyn crate::store::Store<'r>, read: &mut EngineRecords<'r>, handles: &Handles<'_, '_, '_, '_>, frame: &'f Frame) -> Result<Result<Found<'f, 'r>, &'static str>, CliError> {
        let heap = handles.env.heap;
        let g = heap.alloc();
        let Some(graph) = handles.graph else {
            return Ok(Err("this command has no unit graph (no lock file to match), so no crate's names, so no types"));
        };
        let Some(names) = handles.names else {
            return Ok(Err("this command resolved no crate's names, so no body is checked"));
        };
        let Some(list) = self.kept.list(cx, store, &graph, frame)? else {
            return Ok(Err("the crates' inputs could not be made (names.rs `world`; the `UnitGraph` record's finding says why)"));
        };
        let (packs, corrected) = match self.kept.trees(cx, handles.env, graph.out, &list, frame) {
            Ok(trees) => trees,
            Err(why) => return Ok(Err(why)),
        };
        let mut groups: Vec<Group<'f, 'r>, Global> = Vec::new_in(Global);
        let mut took: Vec<Took<'f>, blaze_own::Alloc<'static>> = Vec::new_in(g);
        // The first list that could not grow: the stage's typed stop (rule 3).
        let mut grown: Result<(), EngineCheckError> = Ok(());
        let mut member_files: Vec<(u32, Option<&'static str>), Global> = Vec::new_in(Global);
        let mut members: Vec<Name, Global> = Vec::new_in(Global);
        let mut heads: Vec<([u8; 8], u32, Name), Global> = Vec::new_in(Global);
        let procs = names.names.as_ref().and_then(|names| names.procs());
        let input_at = |at: usize| names.input_of(blaze_drive::key::crate_scope_name(&list.key(at)));
        for member in (0..CrateGraph::len(&list)).filter(|at| list.member(*at)) {
            // The member's files: what its module tree reaches from its root, the walk the
            // planner took (the `Owners` fact `check` reads, blaze-drive owners.rs): each is
            // this member's, checked or refused by name, never in "no crate holds" (rule 3).
            let reach = reached(handles.outlined, Name::file(list.root(member).strip_prefix("/0/").unwrap_or(list.root(member)).as_bytes()));
            let mut refuse_all = |why: &'static str| member_files.extend(reach.iter().map(|at| (*at, Some(why))));
            let Some(own) = input_at(member) else {
                // Its crate's own names failed (a build script, its resolution): that failure
                // is the crate's `CrateScope` row, reported there once; its bodies say so.
                refuse_all(if names.failed(blaze_drive::key::crate_scope_name(&list.key(member))) {
                    "its crate has no names record this command: its `CrateScope` task failed, and that failure is reported at it"
                } else {
                    "its crate has no `Names` task this command (engine_names.rs `plan`)"
                });
                continue;
            };
            members.push(own.name);
            let Some((scope, whole)) = scope_of(read, cx, store, names, &list, member, heap) else {
                refuse_all("its crate's records or its check scope could not be read (engine_bodies.rs `scope_of`)");
                continue;
            };
            let own_records = scope.own_records;
            let cut = scope.cut;
            // The member's environment, its script run's record handed by the Scripts stage.
            let env = crate::body_types::owner_env_by(cx, store, &|script| names.input_of(script), &list, member, handles.env.home, frame)?;
            let (env, script, env_refused) = match env {
                Ok((env, script)) => (env, script.map(|(name, _)| name), None),
                Err(why) => (None, None, Some(why)),
            };
            let group = Group { own: own.name, crates: scope.crates, env, script, lang: scope.lang, scope: scope.scope };
            // The member's files: its items record's file rows, matched to the planner's outlines.
            let Ok(items) = ItemsRecord::open(own_records.items, g) else {
                refuse_all("its crate's items record does not open (blaze-scope items_record.rs `ItemsRecord::open`)");
                continue;
            };
            let mut files: Vec<(u32, u32), Global> = Vec::new_in(Global);
            for file in 0..u32::try_from(items.files()).unwrap_or(u32::MAX) {
                let Ok(Some(row)) = items.file(file) else { continue };
                let Some(path) = row.path.in_package(list.shown_from(member), g) else { continue };
                let Some(path) = path.strip_prefix("/0/") else { continue };
                if let Some(at) = handles.outlined.iter().position(|one| one.path.as_slice() == path.as_bytes()) {
                    files.push((file, u32::try_from(at).unwrap_or(u32::MAX)));
                    heads.push((list.key(member).prefix(), file, handles.outlined[at].name));
                }
            }
            // A file the module tree reaches that the items record lists at no path of it.
            for at in reach.iter().filter(|at| !files.iter().any(|(_, outline)| outline == *at)) {
                member_files.push((*at, Some("its crate's module tree reaches its file, but its crate's items record lists no file at its path (engine_bodies.rs `groups`)")));
            }
            let at = u32::try_from(groups.len()).unwrap_or(u32::MAX);
            // A member whose environment could not be made is refused by name, never checked
            // under none (rule 3).
            // The note names the crate that failed (rule 19), once per member, never per body.
            let refused = match (whole, env_refused) {
                (false, _) => Some(match cut {
                    Some(one) => {
                        let words = bstd::mem::format_in!(g, "its member's check scope holds `{}`, which has no names record this command (its `CrateScope` task failed: that failure is reported at it)", CrateGraph::name(&list, one));
                        core::str::from_utf8(words.into_bytes().leak()).unwrap_or("")
                    }
                    None => "a crate of its member's check scope has no names record this command (its `CrateScope` task failed)",
                }),
                (true, Some(why)) => Some(why),
                (true, None) => None,
            };
            // Each body's item key, through the member's records (opened here once).
            let opened = crate::run_names::names_records(cx, handles.root, &group.crates, packs, corrected, procs, std::vec::Vec::new_in(Global), env_in(group.env, frame).ok().flatten(), frame, g);
            let (records, refused) = match (refused, opened) {
                (Some(why), _) => (None, Some(why)),
                (None, Ok(opened)) => (Some(opened.records), None),
                (None, Err(_)) => (None, Some("its member's items record does not open for its edition (run_names.rs `names_records`)")),
            };
            let expansions = group.crates.first().map(|own| own.records.expansions);
            let index = records.map(|records| {
                let listed = list_with(records, Listing::FnsAndInitializers, g);
                let mut mine = blaze_own::AVec::new_in(g);
                mine.extend(listed.iter().copied().filter(|l| l.entry.is_some_and(|(f, _)| files.iter().any(|(file, _)| *file == f))));
                // The bodies a macro wrote at module level, each at the entry its call is at
                // (D122; body_types/covered.rs `written_by`, the one walk `check` takes).
                let written = match &expansions {
                    Some(texts) => crate::body_types::covered::written_by(records, texts, &listed, g),
                    None => blaze_own::AVec::new_in(g),
                };
                (records, index_by_chain(records, &mine, &blaze_file_items::item_chain, g), written)
            });
            for &(file, outline_at) in files.iter() {
                let file_name = handles.outlined[outline_at as usize].name;
                let Some((records, chains, written)) = index.as_ref() else {
                    // Its file is the member's; its scope could not be read: every body says why.
                    member_files.push((outline_at, Some(refused.unwrap_or(RULE))));
                    continue;
                };
                let records = *records;
                member_files.push((outline_at, None));
                let read = records.with_outline(file, g, &mut |text, outline| {
                    for (entry, item) in outline.items().iter().enumerate() {
                        let body = work::checked_body(outline, item).is_some();
                        if !body && !work::expansion_entry(outline, item, text) {
                            continue;
                        }
                        let name = work::body_name(file_name, outline, item, text);
                        // The bodies an expansion at this entry wrote: the entry's task checks
                        // each (one task an entry, planned from text; covered.rs).
                        let top = Ok((file, blaze_surface::ItemIndex(u32::try_from(entry).unwrap_or(u32::MAX))));
                        let by = written.iter().find(|w| w.top == top).map(|w| w.by);
                        // One a body: in the request's heap with its list, never the fixed frame.
                        let shown = crate::item_name::shown_name(item, text);
                        let label = match (shown.is_empty(), by) {
                            (false, _) => bstd::mem::format_in!(g, "{shown}"),
                            (true, Some(by)) => bstd::mem::format_in!(g, "{{the expansion of `{by}!`}}"),
                            (true, None) => bstd::mem::format_in!(g, "{{a macro call}}"),
                        };
                        let label: &'f str = core::str::from_utf8(label.into_bytes().leak()).unwrap_or("");
                        let found = match chains {
                            Err(_) => Err("its file's bodies could not be indexed by their outline entries (front2 project_records.rs `index_by_chain`)"),
                            Ok(_) if !body => {
                                let mut keys = Vec::new_in(g);
                                let mut missing = None;
                                for w in written.iter().filter(|w| w.top == top) {
                                    match key_of(records, w.listed.body) {
                                        Some(key) => keys.push(key),
                                        None => missing = Some("a member a macro wrote has no key of its own in its crate's records"),
                                    }
                                }
                                match missing {
                                    Some(why) => Err(why),
                                    None => Ok((at, &*keys.leak())),
                                }
                            }
                            Ok(chains) => match chains.get(file, blaze_file_items::item_chain(outline, item, text)) {
                                Some(body) => key_of(records, body).map(|key| (at, &*Vec::from_slice_in(&[key], g).leak())).ok_or("a member the records list has no key of its own"),
                                None => Err("no body of its crate's records is at this entry (a `cfg` left it out, or a macro replaced it)"),
                            },
                        };
                        // Where its findings are printed: the entry by its chain; a body a macro
                        // wrote by its label, as `check` names it (covered.rs `written_label`).
                        let mut labels: Vec<&'f str, _> = Vec::new_in(g);
                        if !body {
                            for w in written.iter().filter(|w| w.top == top) {
                                let shown = bstd::mem::format_in!(g, "{}", crate::body_types::covered::written_label(records, w));
                                labels.push(core::str::from_utf8(shown.into_bytes().leak()).unwrap_or(""));
                            }
                        }
                        let place = Place { chain: work::item_chain(outline, item, text), written: &*labels.leak() };
                        if grown.is_ok() {
                            grown = EngineCheckError::push(&mut took, Took { name, label, item: found, place }, "the bodies found in their crates' records (engine_bodies.rs `groups`)");
                        }
                    }
                });
                core::mem::replace(&mut grown, Ok(()))?;
                if !read && let Some(last) = member_files.last_mut() {
                    last.1 = Some("its file's text could not be read through its crate's records");
                }
            }
            groups.push(group);
        }
        // A file's bodies taken by two members are checked under the first (the lib's, by order).
        took.sort_by_key(|one| one.name);
        took.dedup_by_key(|one| one.name);
        // A file one member checks and another refuses is the checking member's.
        member_files.sort_by_key(|(at, why)| (*at, why.is_some()));
        member_files.dedup_by_key(|(at, _)| *at);
        heads.sort_unstable();
        heads.dedup();
        Ok(Ok(Found { groups, took, files: member_files, members, heads, trees: (packs, corrected) }))
    }

    /// Body `name`'s block of the request's lines (engine_lines.rs): each finding of its verdict
    /// as `check` files it, placed in its entry's item by the item's chain in its file (a
    /// placed finding: printed, counted and stored as an outline's or a names task's findings
    /// are), then its note when it has one. Nothing when it was not entered. A finding whose
    /// file or edition is not known is a typed stop naming the body, never dropped (rule 3).
    pub(crate) fn lines(&self, name: Name, outlined: &[Outlined<'_>], lines: &mut crate::engine_lines::Lines<'_>, frame: &Frame) -> Result<(), EngineCheckError> {
        let Ok(at) = self.ran.binary_search_by_key(&name, |one| one.name) else { return Ok(()) };
        let one = &self.ran[at];
        if !one.found.is_empty() {
            let file = outlined.get(one.file as usize).ok_or(EngineCheckError::Unplaced { body: name, what: "its file is none of the planner's outlines" })?;
            let edition = blaze_surface::Edition::from_byte(file.edition).ok_or(EngineCheckError::Unplaced { body: name, what: "its file's edition byte is no edition" })?;
            for found in one.found.iter() {
                lines.placed(name, found.rule, found.start, found.end, &file.path, one.chain, edition, &found.message);
            }
        }
        if let Some(words) = self.note(name, outlined, frame) {
            let class = if self.not_checked(name) { crate::engine_lines::Class::Unchecked } else { crate::engine_lines::Class::Failure };
            lines.note(name, class, words.as_bytes());
        }
        Ok(())
    }

    /// The note of a body that has one: its refusal, or why it was not checked whole. Its
    /// findings are its lines' ([`BodiesStage::lines`]), not a note.
    pub(crate) fn note<'p>(&self, name: Name, outlined: &[Outlined<'_>], frame: &'p Frame) -> Option<bstd::mem::String<&'p Frame>> {
        let at = self.ran.binary_search_by_key(&name, |one| one.name).ok()?;
        let one = &self.ran[at];
        let path = outlined.get(one.file as usize).map_or("?", |file| core::str::from_utf8(&file.path).unwrap_or("?"));
        let item = one.label;
        match &one.said {
            Some(Said::Task(error)) => Some(bstd::mem::format_in!(frame, "`{path}`: the body of `{item}` was not checked: {error} ({RULE})")),
            Some(Said::Panicked) => Some(bstd::mem::format_in!(frame, "`{path}`: the check of the body of `{item}` panicked; that body alone failed ({RULE})")),
            Some(Said::FrameUsedUp { used, reserved }) => Some(bstd::mem::format_in!(
                frame,
                "`{path}`: the check of the body of `{item}` used up its frame ({used} of {reserved} bytes reserved, engine_bodies.rs `BODIES_FRAME`); that body alone failed ({RULE})"
            )),
            Some(Said::Refused(why)) => Some(bstd::mem::format_in!(frame, "`{path}`: the body of `{item}` was not checked: {why} ({RULE})")),
            Some(Said::Unchecked("")) => Some(bstd::mem::format_in!(frame, "`{path}`: body `{item}`: not checked whole: its verdict failed and said no cause (front2 tables/task.rs `said_of`; {RULE})")),
            Some(Said::Unchecked(why)) => Some(bstd::mem::format_in!(frame, "`{path}`: body `{item}`: {why}")),
            None => None,
        }
    }

    /// Body `name`'s item as written, when it ran.
    pub(crate) fn label(&self, name: Name) -> Option<&'f str> {
        self.ran.binary_search_by_key(&name, |one| one.name).ok().map(|at| self.ran[at].label).filter(|label| !label.is_empty())
    }

    /// Whether body `name` was entered this request (else its row stands from the stored answer).
    pub(crate) fn entered(&self, name: Name) -> bool {
        self.ran.binary_search_by_key(&name, |one| one.name).is_ok()
    }

    /// Whether body `name` is an item the check could not check (refused by this stage or by
    /// the checker), which `check` counts apart from failures (main.rs `UNCHECKED_ITEMS`); a
    /// finding, a task error or a panic is a failure instead.
    pub(crate) fn not_checked(&self, name: Name) -> bool {
        self.ran.binary_search_by_key(&name, |one| one.name).is_ok_and(|at| matches!(self.ran[at].said, Some(Said::Refused(_) | Said::Unchecked(_))))
    }

    /// Every body's note ([`BodiesStage::note`]) in name order, and how many bodies were
    /// checked with no finding and no note: what the command's tests read.
    pub(crate) fn notes(&self, outlined: &[Outlined<'_>], frame: &Frame) -> (std::vec::Vec<std::string::String, Global>, u32) {
        let mut notes = std::vec::Vec::new_in(Global);
        let mut clean = 0u32;
        for one in self.ran.iter() {
            match self.note(one.name, outlined, frame) {
                Some(words) => notes.push(std::string::String::from(words.as_str())),
                None => clean += u32::from(one.said.is_none() && one.findings == 0),
            }
        }
        // A body in a file no member holds has no note and was not checked (never by its time:
        // a simulated clock gives a fast body 0 ns).
        (notes, clean.saturating_sub(self.ownerless))
    }

    /// The profile's lines: per body microseconds (sum, median, the 10 slowest with their
    /// paths), bodies per second of CPU, and the phase's wall against its CPU.
    pub(crate) fn profile(&self, outlined: &[Outlined<'_>], workers: NonZeroU32, into: &mut blaze_own::String<&Frame>) {
        use core::fmt::Write as _;
        let mut checked: std::vec::Vec<(u64, Name, u32), Global> = std::vec::Vec::new_in(Global);
        checked.extend(self.ran.iter().filter(|one| one.said.is_none() && one.ns > 0).map(|one| (one.ns, one.name, one.file)));
        checked.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let sum: u64 = checked.iter().map(|one| one.0).sum();
        let median = checked.get(checked.len() / 2).map_or(0, |one| one.0);
        let per_cpu_s = if sum == 0 { 0 } else { (checked.len() as u64).saturating_mul(1_000_000_000) / sum };
        let parallel = if self.wall_ns == 0 { 0.0 } else { sum as f64 / self.wall_ns as f64 };
        let _ = writeln!(into, "\nbodies: {} entered, {} checked, {} refused, {} in files no crate holds; {} run(s) on {} worker(s)", self.ran.len(), checked.len(), self.ran.iter().filter(|one| one.said.is_some()).count(), self.ownerless, self.runs, workers);
        for path in self.ownerless_paths(outlined).iter() {
            let _ = writeln!(into, "bodies: in no member's module tree: {path}");
        }
        let _ = writeln!(into, "bodies: us a body: sum {}, median {}; {} bodies a CPU second; phase wall {} us against {} us of body CPU ({parallel:.1}x)", sum / 1000, median / 1000, per_cpu_s, self.wall_ns / 1000, sum / 1000);
        for (ns, _, file) in checked.iter().take(10) {
            let path = outlined.get(*file as usize).map_or("?", |one| core::str::from_utf8(&one.path).unwrap_or("?"));
            let _ = writeln!(into, "bodies: slow  {:>9} us  {path}", ns / 1000);
        }
        // The most one body grew its stretch's frame by, and which body (the frame's cap is
        // `BODIES_FRAME`; a stretch ends past `STRETCH_FULL`).
        if let Some(most) = self.ran.iter().max_by(|a, b| a.bytes.cmp(&b.bytes).then(b.name.cmp(&a.name))).filter(|one| one.bytes > 0) {
            let path = outlined.get(most.file as usize).map_or("?", |one| core::str::from_utf8(&one.path).unwrap_or("?"));
            let used_up = self.ran.iter().filter(|one| matches!(one.said, Some(Said::FrameUsedUp { .. }))).count();
            let _ = writeln!(into, "bodies: largest frame a body used: {} bytes, `{}` in {path} (of {BODIES_FRAME} reserved a stretch, a stretch ends past {STRETCH_FULL}); {used_up} body(ies) used one up", most.bytes, most.label);
        }
    }
}

/// What every run reads: all read-only and shared.
struct Shared<'s, 'a, 'f, 'r> {
    root: &'s Dir<ReadWrite>,
    packs: &'s [blaze_fetch::PackView<'static>],
    corrected: &'s [(u32, &'static str, &'static str)],
    procs: Option<&'static crate::proc_runner::ProcRunner<'static>>,
    groups: &'s [Group<'f, 'r>],
    heads: &'s crate::engine_heads::Subjects,
    view: View<'s, 'a>,
    heap: &'static blaze_own::Heap,
    /// Every crate's impl tables (the Heads stage's), read only.
    shelf: &'s ImplShelf,
}

/// A body's cells as the engine's view holds them today: every cell a body reads is a cell an
/// earlier phase defined or none does ([`ABSENT`]); nothing is made inline on this path.
struct ViewMake<'s, 'a> {
    view: View<'s, 'a>,
}

impl<'a> Make<'a> for ViewMake<'_, 'a> {
    fn make(&self, want: &CellId, _made: &mut SVec<'a, Made<'a>>) -> Result<ContentId, MakeRefused> {
        Ok(self.view.cell(want))
    }
}

/// A stretch's frame past this many bytes takes no further body: the next body opens its
/// records into a new frame. A body's lowerings and the records' copied segments live in the
/// frame until the records go (`check_body` borrows them for the records' lifetime), so a frame
/// is reset by ending its stretch, never per body; half the reserve leaves the next body the
/// other half (tempo, g124650: three 32-body runs used up the whole 1 GiB in one frame).
const STRETCH_FULL: usize = BODIES_FRAME / 2;

/// One run: its bodies checked in stretches, each over a frame of its own that ends with it
/// ([`STRETCH_FULL`]), so no run uses up a frame however many bodies it holds.
fn run_bodies<'a, 'f>(cx: &Cx, shared: &Shared<'_, 'a, '_, '_>, run: &[One<'f>], clock: &Clock, out: &mut Buffer<'a>, ran: &mut Vec<Ran<'f>, Global>, alloc: SharedAlloc<'a>) {
    let mut at = 0;
    while at < run.len() {
        let rest = &run[at..];
        let took = match Frame::with_reserve(BODIES_FRAME, bstd::mem::Tag::Other) {
            Ok(stretch) => run_stretch(cx, shared, rest, &stretch, clock, out, ran, alloc),
            Err(_) => refuse(rest, "the frame of its run's stretch could not be reserved (engine_bodies.rs `BODIES_FRAME`)", out, ran, alloc),
        };
        at += took.max(1);
    }
}

/// Every body of `rest` refused for one reason, each named (rule 3): how many it took (all).
fn refuse<'a, 'f>(rest: &[One<'f>], why: &'static str, out: &mut Buffer<'a>, ran: &mut Vec<Ran<'f>, Global>, alloc: SharedAlloc<'a>) -> usize {
    for one in rest {
        out.done.push(crate::engine_tasks::failed(&one.task, alloc));
        ran.push(Ran { name: one.task.planned.name, label: one.label, file: one.file, ns: 0, bytes: 0, findings: 1, said: Some(Said::Refused(why)), chain: one.place.chain, found: Vec::new_in(Global) });
    }
    rest.len()
}

/// One stretch of a run: its member's records opened once into `frame`, each body checked over
/// one view until the frame is past [`STRETCH_FULL`]. How many bodies it took (at least one).
fn run_stretch<'a, 'f>(cx: &Cx, shared: &Shared<'_, 'a, '_, '_>, run: &[One<'f>], frame: &Frame, clock: &Clock, out: &mut Buffer<'a>, ran: &mut Vec<Ran<'f>, Global>, alloc: SharedAlloc<'a>) -> usize {
    let Some(first) = run.first() else { return 0 };
    let Some(group) = shared.groups.get(first.group as usize) else {
        return refuse(run, "a body's member group is gone", out, ran, alloc);
    };
    let g = shared.heap.alloc();
    // The impls from the request's shelf: each impl question reads its crate's bucket cell
    // (front2 tables/task.rs `cell_id`), which the Heads stage defined. Held in the stretch's
    // frame, so it lives as long as the records it is read beside (`Tables` takes both for one
    // lifetime); the frame runs no drop, so its notes stay in the request's heap until it ends.
    let impls: &TableSet<'_> = frame.alloc(TableSet::lent(shared.shelf, g));
    let Ok(env) = env_in(group.env, frame) else {
        return refuse(run, "its member's environment did not copy into the run (blaze-scope crate_env.rs `EnvLent::copy_in`)", out, ran, alloc);
    };
    let Ok(opened) = crate::run_names::names_records(cx, shared.root, &group.crates, shared.packs, shared.corrected, shared.procs, std::vec::Vec::new_in(Global), env, frame, g) else {
        return refuse(run, "its member's records could not be opened in the run", out, ran, alloc);
    };
    let unused = Recorder::new(RecorderOpts { alloc: g });
    let tables = Tables::new(TablesOpts { records: opened.records, impls: Some(impls), lang_crates: &group.lang, reads: &unused });
    let make = ViewMake { view: shared.view };
    // `First`: under `All` clap-root's `Cmd` was refused with a false cycle among core's
    // iterator traits (front2 heads.rs `Store::new`; lane FL).
    let body_frame = BodyFrame { frame, alloc: g, plant_drop: None, cost: None, needs: NeedsMet::First, heads: None, sources: Some(opened.sources) };
    let mut owners = crate::engine_heads::Owners::new(shared.heads);
    let mut taken = 0usize;
    for one in run {
        if taken > 0 && frame.used() > STRETCH_FULL {
            break;
        }
        taken += 1;
        let at_start = frame.used();
        let _body = cx.span("engine: one body");
        let began = clock.now();
        let name = one.task.planned.name;
        // Each body the task checks (one; or each an expansion at its entry wrote), merged into
        // the task's one result: its reads the union, its verdict the digest of theirs.
        let mut whole = Hasher::new(ENTRY_DOMAIN);
        let mut reads: SVec<'a, (CellId, ContentId)> = SVec::new_in(alloc);
        let mut single = None;
        let (mut findings, mut failed, mut said) = (0u32, false, None);
        let mut why = std::string::String::new();
        let mut found: Vec<BodyFinding, Global> = Vec::new_in(Global);
        for (at, item) in one.items.iter().enumerate() {
            let mut asked = one.task;
            asked.planned.name = Name::new(Kind::BodyCheck, item.0);
            match work::unwinding(|| check_body(tables, &asked, &body_frame, &make, alloc)) {
                Ok(Ok(body)) => {
                    whole.update(&body.done.out.0);
                    reads.extend_from_slice(&body.done.reads);
                    findings = findings.saturating_add(body.done.findings);
                    // Its findings as `check` files them; a type error a refused macro call may
                    // have caused is that call's not-checked line instead (body_types.rs).
                    let written = one.place.written.get(at).copied();
                    for line in body.said.iter() {
                        if let SaidWhat::Type { .. } = line.what
                            && let Some(cut) = line.words.as_str().find(blaze_ladder::front2::MACRO_REFUSED)
                        {
                            let cause = &line.words.as_str()[cut + blaze_ladder::front2::MACRO_REFUSED.len()..];
                            if !why.contains(cause) {
                                if !why.is_empty() {
                                    why.push_str("; ");
                                }
                                why.push_str(cause);
                                why.push_str(": ");
                                why.push_str(line.words.as_str());
                            }
                            continue;
                        }
                        match found_of(line, written) {
                            Ok(Some(one_found)) => found.push(one_found),
                            Ok(None) => {}
                            // Never quietly dropped (rule 3): the body is then not checked whole.
                            Err(cause) => {
                                if !why.is_empty() {
                                    why.push_str("; ");
                                }
                                why.push_str(cause);
                            }
                        }
                    }
                    // The first failed body's words why (rule 19): its verdict's not-checked lines.
                    if body.done.failed && !failed {
                        for line in body.said.iter() {
                            let cause = match &line.what {
                                SaidWhat::NotChecked { cause } => cause.as_str(),
                                SaidWhat::BorrowsNotChecked => "borrows not checked",
                                SaidWhat::Type { .. } | SaidWhat::Lint { .. } | SaidWhat::Borrow { .. } => continue,
                            };
                            if !why.is_empty() {
                                why.push_str("; ");
                            }
                            why.push_str(cause);
                            why.push_str(": ");
                            why.push_str(line.words.as_str());
                        }
                    }
                    failed |= body.done.failed;
                    out.made.extend(body.made);
                    if one.items.len() == 1 {
                        single = Some(body.done);
                    }
                }
                Ok(Err(error)) => {
                    said = Some(Said::Task(error));
                    break;
                }
                Err(work::Panicked) => {
                    // A frame past half its reserve after a panic: its body used it up (a stretch
                    // takes no body past [`STRETCH_FULL`]), said as that; the stretch then ends.
                    said = Some(match frame.used() > STRETCH_FULL {
                        true => Said::FrameUsedUp { used: frame.used() as u64, reserved: frame.reserved() as u64 },
                        false => Said::Panicked,
                    });
                    break;
                }
            }
        }
        let ns = clock.now().since(began).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
        let bytes = frame.used().saturating_sub(at_start) as u64;
        if let Some(said) = said {
            out.done.push(crate::engine_tasks::failed(&one.task, alloc));
            ran.push(Ran { name, label: one.label, file: one.file, ns, bytes, findings: 1, said: Some(said), chain: one.place.chain, found: Vec::new_in(Global) });
            continue;
        }
        // Rule 25: the lines record's bytes a function of the findings alone.
        found.sort_unstable_by(|a, b| (a.start, a.end, a.rule, &a.message[..]).cmp(&(b.start, b.end, b.rule, &b.message[..])));
        let mut done = match single {
            Some(done) => done,
            None => {
                // An entry's expansion bodies (or none: an expansion that wrote no body is a
                // pass that read its crate's names): one verdict cell, the merged digest.
                let out_id = whole.finish();
                let mut defines = SVec::new_in(alloc);
                defines.push((blaze_ladder::front2::tables::task::VERDICT_CELL, out_id));
                Done { name, key: one.task.key, out: out_id, defines, reads, findings, failed }
            }
        };
        done.name = name;
        if one.items.is_empty() {
            done.reads.push((CellId { of: group.own, cell: WHOLE }, ABSENT));
        }
        let (wide, unfrozen) = widen(&done.reads, group, &mut owners, opened.records, shared.view, g, alloc);
        done.reads = wide;
        // A head it read has no frozen head (its head task failed): the body fails by that read
        // (rule 3), its reads kept so the head's repair enters it again.
        if unfrozen && !done.failed {
            done.failed = true;
            done.defines.clear();
            done.findings = done.findings.max(1);
            why.push_str("a head it reads could not be frozen: its head task failed (engine_heads.rs; the Heads stage's refusals name it)");
        }
        // One a body not checked whole (its verdict failed, or a line of it could not be said
        // as a finding): in the request's heap, as the labels are.
        let said = (done.failed || !why.is_empty()).then(|| Said::Unchecked(core::str::from_utf8(bstd::mem::format_in!(g, "{why}").into_bytes().leak()).unwrap_or("")));
        ran.push(Ran { name, label: one.label, file: one.file, ns, bytes, findings: done.findings, said, chain: one.place.chain, found });
        out.done.push(done);
    }
    taken
}

/// The outlines (by index, sorted) a crate's module tree reaches from its root file `root`: the
/// root and every file a reached outline names as a file part, transitively, as the planner
/// walked them (engine_check.rs `plan`). A file two crates reach is in both crates' walks.
fn reached(outlined: &[Outlined<'_>], root: Name) -> Vec<u32, Global> {
    let mut seen: Vec<u32, Global> = Vec::new_in(Global);
    let mut next: Vec<Name, Global> = Vec::new_in(Global);
    next.push(root);
    while let Some(name) = next.pop() {
        let Ok(at) = outlined.binary_search_by(|one| one.name.cmp(&name)) else { continue };
        let at32 = u32::try_from(at).unwrap_or(u32::MAX);
        let Err(slot) = seen.binary_search(&at32) else { continue };
        seen.insert(slot, at32);
        if let Ok(record) = &outlined[at].made {
            next.extend(record.parts.iter().filter(|part| matches!(part.locate, Locate::File { .. })).map(|part| part.name));
        }
    }
    seen
}

/// A verdict's line as `check` files it (body_types.rs, where a body's findings are filed; the
/// same rule numbers and message pieces): a type error as `TYPES` with its code and words, a
/// typed lint's finding by its lint's number (lints.rs `number_of`; no level kept: rule 81), a
/// move or borrow finding as `BORROWS` (a note while `BORROWS_ARE_ERRORS` is false: print.rs);
/// `None` for a not-checked line, which the body's note says. `written`: the label of a body a
/// macro wrote, whose findings are at its item's start, said so. `Err`: the line cannot be
/// said as a finding, and why (the body is then not checked whole: rule 3).
fn found_of(line: &Line<'_>, written: Option<&str>) -> Result<Option<BodyFinding>, &'static str> {
    let (rule, code) = match &line.what {
        SaidWhat::Type { code } => (crate::body_types::TYPES, Some(*code)),
        SaidWhat::Borrow { code } => (crate::body_types::BORROWS, Some(*code)),
        SaidWhat::Lint { lint } => match blaze_lint::Rule::from_name(lint) {
            Some(rule) => (crate::lints::number_of(rule), None),
            None => return Err("a typed lint's finding names no rule of blaze-lint"),
        },
        SaidWhat::NotChecked { .. } | SaidWhat::BorrowsNotChecked => return Ok(None),
    };
    let mut message = Vec::new_in(Global);
    if let Some(code) = code {
        message.extend_from_slice(code.as_bytes());
        message.push(0x1F);
    }
    let (start, end) = match written {
        None => (line.start, line.end),
        Some(label) => {
            message.extend_from_slice(b"in `");
            message.extend_from_slice(label.as_bytes());
            message.extend_from_slice(b"`, ");
            message.extend_from_slice(crate::body_types::covered::WRITTEN_UNPLACED.as_bytes());
            (0, 0)
        }
    };
    message.extend_from_slice(line.words.as_str().as_bytes());
    Ok(Some(BodyFinding { rule, start, end, message }))
}

/// A member's lent environment copied into `frame` (no allocator of the lending thread is
/// called here, crate_env.rs `EnvLent`), as body_types.rs does for its workers.
fn env_in<'l: 'n, 'n>(lent: Option<EnvLent<'_, 'l>>, frame: &'n Frame) -> Result<Option<&'n blaze_scope::crate_env::CrateEnv<'n>>, blaze_build_script::outputs::RecordError> {
    let Some(lent) = lent else { return Ok(None) };
    let erased: &'n blaze_own::Erased<&'n Frame> = blaze_own::Box::leak(blaze_own::Box::new_in(blaze_own::Erased::new(frame), frame));
    let env = lent.copy_in(erased.handle())?;
    Ok(Some(&*blaze_own::Box::leak(blaze_own::Box::new_in(env, frame))))
}

/// A body's reads widened to cells a phase defines (module docs), each with the digest the view
/// holds, sorted and one each; and whether a head it read is a failed head task's.
#[expect(clippy::too_many_arguments, reason = "a body's reads and every table that names them")]
fn widen<'a, 'n>(
    reads: &[(CellId, ContentId)],
    group: &Group<'_, '_>,
    owners: &mut crate::engine_heads::Owners<'_>,
    records: &'n blaze_ladder::front2::source::Records<'n>,
    view: View<'_, 'a>,
    g: blaze_own::Alloc<'static>,
    alloc: SharedAlloc<'a>,
) -> (SVec<'a, (CellId, ContentId)>, bool) {
    let mut unfrozen = false;
    let mut wide: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(reads.len(), alloc);
    let mut push = |of: Name, cell: Cell| {
        let id = CellId { of, cell };
        wide.push((id, view.cell(&id)));
    };
    for (id, _) in reads.iter() {
        match id.of.kind {
            // A names read of a crate whose task defines its cell table (engine_names.rs
            // `cells_of`, marked by `CELLS`) stays its cell: the table holds every cell a walk
            // can name that holds something, each digest over all the cell answers, so the cell
            // moves whenever the answer can (rule 21); a cell it does not hold is the name not
            // found, read ABSENT, and the name added defines it, which moves it. A crate's lang
            // rows are one such cell (blaze-scope names_cells.rs `NameCell::Lang`).
            Kind::CrateScope if id.cell != WHOLE && view.cell(&CellId { of: id.of, cell: CELLS }) != ABSENT => push(id.of, id.cell),
            Kind::CrateScope => push(id.of, WHOLE),
            Kind::ImplBucket if id.of.subject == ABSENT => {
                for name in group.scope.iter() {
                    push(*name, WHOLE);
                }
            }
            // A head (engine_heads.rs `Owners::place`): a member's, the cell of the head task of
            // the entry that covers it; a dependency's, or one a macro wrote, its crate's names
            // record whole. A head of a crate none of the records holds is read as read.
            Kind::Signature => match owners.place(records, id, g) {
                Some(crate::engine_heads::HeadAt::Cell { cell, .. }) => {
                    unfrozen |= view.row(cell.of).is_some_and(|row| row.failed);
                    push(cell.of, cell.cell);
                }
                Some(crate::engine_heads::HeadAt::Dependency(scope) | crate::engine_heads::HeadAt::Unplaced(scope)) => push(scope, WHOLE),
                None => push(id.of, id.cell),
            },
            // A crate's bucket: its cell as the crate's Heads task defined it (engine_heads.rs),
            // or ABSENT when the crate files no impl there (a later impl defines it, and the
            // read moves).
            _ => push(id.of, id.cell),
        }
    }
    // The environment every body was checked under: its script run, whole.
    if let Some(script) = group.script {
        push(script, WHOLE);
    }
    wide.sort_unstable_by_key(|(id, _)| *id);
    wide.dedup_by_key(|(id, _)| *id);
    (wide, unfrozen)
}
