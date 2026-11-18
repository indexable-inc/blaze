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
//! impl-bucket cells. Until the Names stage defines cells finer than its record's one whole cell
//! (E4's wiring) and a Heads stage defines the impl tables' buckets, each read is WIDENED to a
//! cell some phase defines ([`widen`]): a `CrateScope` cell to that record's whole cell, a
//! crate's `ImplBucket` cell to its crate's `CrateScope` whole cell (the buckets are made from
//! that record's items section), the whole graph's bucket fact to every scope crate's. A
//! `Signature` cell is defined by no phase yet (lane FJ; the body makes the heads it needs from
//! its crates' records, task.rs), so it widens to the `HEADS` cell of each outlined file of the
//! head's crate (engine_tasks.rs, INTERIM): a signature edit enters its callers. Widened reads
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
use blaze_ladder::front2::buckets::impl_tables_name;
use blaze_ladder::front2::heads::NeedsMet;
use blaze_ladder::front2::project::BodyRef;
use blaze_ladder::front2::project_records::{BodySource, Listing, index_by_chain, list_with};
use blaze_ladder::front2::source::NamesSource;
use blaze_ladder::front2::tables::recorder::{Recorder, RecorderOpts};
use blaze_ladder::front2::tables::task::{BodyFrame, LANG_CELL, Make, MakeRefused, SaidWhat, TaskError, check_body};
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
    pub(crate) findings: u32,
    pub(crate) said: Option<Said>,
}

/// A body found in its crate's records: its name and its item's name as written.
struct Took<'f> {
    name: Name,
    label: &'f str,
    /// Its member crate (an index of [`Group`]s) and the item keys it checks (its own body's,
    /// or each body an expansion at its entry wrote), or why it has none.
    item: Result<(u32, &'f [ItemKey]), &'static str>,
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

/// One member crate's check scope as a run reads it: all read-only, shared by its runs.
struct Group<'f> {
    /// The member's own `CrateScope` name.
    own: Name,
    /// Its scope's crates in reader order, the member at 0.
    crates: Vec<Crate<'static>, Global>,
    /// The member's environment, what its bodies' `env!` read: its crate list facts with its
    /// build script's outputs joined (body_types.rs `owner_env_by`), lent to each run, and the
    /// script run read for it (a `Script` task's WHOLE cell), in every body's read list.
    env: Option<EnvLent<'f, 'f>>,
    script: Option<Name>,
    /// The places of its lang crates among `crates`.
    lang: Vec<usize, Global>,
    /// Each scope crate's `CrateScope` name, sorted: the whole graph's bucket fact widens to these.
    scope: Vec<Name, Global>,
    /// Each scope crate's impl tables name and its `CrateScope` name, sorted by the first.
    buckets: Vec<(Name, Name), Global>,
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
struct Found<'f> {
    groups: Vec<Group<'f>, Global>,
    /// One a body of the project: in the request's heap, never the command's fixed frame.
    took: Vec<Took<'f>, blaze_own::Alloc<'static>>,
    /// Each outline (by index) a member crate's records list, or why its bodies were not checked.
    files: Vec<(u32, Option<&'static str>), Global>,
    members: Vec<Name, Global>,
    /// Each member crate's outlined files by its key's prefix, sorted: what a head read widens
    /// to (engine_tasks.rs `HEADS`, [`widen`]).
    heads: Vec<([u8; 8], Name), Global>,
    trees: Trees,
}

/// The stage: the crates' records read once a command, and every body's result.
pub(crate) struct BodiesStage<'f> {
    kept: Kept,
    /// Every body entered, sorted by name after its phase.
    pub(crate) ran: Vec<Ran<'f>, Global>,
    /// Bodies of files no member crate of the graph holds: checked by no crate, counted.
    pub(crate) ownerless: u32,
    /// The phase's wall time, ns, and the runs it was cut into.
    pub(crate) wall_ns: u64,
    pub(crate) runs: u32,
}

impl<'f> BodiesStage<'f> {
    pub(crate) fn new() -> BodiesStage<'f> {
        BodiesStage { kept: Kept::default(), ran: Vec::new_in(Global), ownerless: 0, wall_ns: 0, runs: 0 }
    }

    /// Whether every body entered was checked: none refused, none in a file no crate holds.
    pub(crate) fn whole(&self) -> bool {
        self.ownerless == 0 && self.ran.iter().all(|one| one.said.is_none())
    }

    /// One Bodies phase: every task entered once, its result in `buffer`.
    #[expect(clippy::too_many_arguments, reason = "a phase's edge: each argument one handle of it")]
    pub(crate) fn run_phase<'a>(
        &mut self,
        cx: &Cx,
        store: &mut dyn crate::store::Store<'_>,
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
        let found = self.groups(&cx, store, handles, frame)?;
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
                                ones.push(One { task: *task, items, group, file, label: one.label });
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
                            let mut reads: SVec<'a, (CellId, ContentId)> = SVec::new_in(alloc);
                            for member in found.members.iter() {
                                let id = CellId { of: *member, cell: WHOLE };
                                reads.push((id, view.cell(&id)));
                            }
                            let out = ContentId::of(OWNERLESS_DOMAIN, &name.subject.0);
                            buffer.done.push(Done { name, key: task.key, out, defines: SVec::new_in(alloc), reads, findings: 0, failed: false });
                            self.ran.push(Ran { name, label: "", file, ns: 0, findings: 0, said: None });
                            continue;
                        }
                    }
                }
            };
            buffer.done.push(crate::engine_tasks::failed(task, alloc));
            self.ran.push(Ran { name, label: "", file, ns: 0, findings: 1, said: Some(Said::Refused(refused)) });
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
        let shared = Shared { root: handles.root, packs, corrected, procs, groups: &found.groups, heads: &found.heads, view, heap };
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
    fn groups(&mut self, cx: &Cx, store: &mut dyn crate::store::Store<'_>, handles: &Handles<'_, '_, '_, '_>, frame: &'f Frame) -> Result<Result<Found<'f>, &'static str>, CliError> {
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
        let mut groups: Vec<Group<'f>, Global> = Vec::new_in(Global);
        let mut took: Vec<Took<'f>, blaze_own::Alloc<'static>> = Vec::new_in(g);
        // The first list that could not grow: the stage's typed stop (rule 3).
        let mut grown: Result<(), EngineCheckError> = Ok(());
        let mut member_files: Vec<(u32, Option<&'static str>), Global> = Vec::new_in(Global);
        let mut members: Vec<Name, Global> = Vec::new_in(Global);
        let mut heads: Vec<([u8; 8], Name), Global> = Vec::new_in(Global);
        let procs = names.names.as_ref().and_then(|names| names.procs());
        let input_at = |at: usize| names.input_of(blaze_drive::key::crate_scope_name(&list.key(at)));
        for member in (0..CrateGraph::len(&list)).filter(|at| list.member(*at)) {
            let Some(own) = input_at(member) else { continue };
            members.push(own.name);
            let Ok(own_records) = self.kept.records(cx, store, &own, list.key(member), heap) else { continue };
            let no_core = blaze_scope::names_record::NamesRecord::open(own_records.names_proof).crate_root().head().no_core;
            let Ok(scope) = blaze_world::check_scope::check_scope_in(&list, member, no_core, g) else { continue };
            // The member's environment, its script run's record handed by the Scripts stage.
            let env = crate::body_types::owner_env_by(cx, store, &|script| names.input_of(script), &list, member, handles.env.home, frame)?;
            let (env, script, env_refused) = match env {
                Ok((env, script)) => (env, script.map(|(name, _)| name), None),
                Err(why) => (None, None, Some(why)),
            };
            let mut group = Group { own: own.name, crates: Vec::new_in(Global), env, script, lang: Vec::new_in(Global), scope: Vec::new_in(Global), buckets: Vec::new_in(Global) };
            group.lang.extend(scope.lang_places());
            let mut whole = true;
            for at in scope.crates() {
                let Some(input) = input_at(at) else {
                    whole = false;
                    break;
                };
                let Ok(records) = self.kept.records(cx, store, &input, list.key(at), heap) else {
                    whole = false;
                    break;
                };
                group.crates.push(Crate { records, name: CrateGraph::name(&list, at), package: list.shown_from(at), world_at: at });
                group.scope.push(input.name);
                group.buckets.push((impl_tables_name(list.key(at)), input.name));
            }
            group.scope.sort_unstable();
            group.buckets.sort_unstable();
            // The member's files: its items record's file rows, matched to the planner's outlines.
            let Ok(items) = ItemsRecord::open(own_records.items, g) else { continue };
            let mut files: Vec<(u32, u32), Global> = Vec::new_in(Global);
            for file in 0..u32::try_from(items.files()).unwrap_or(u32::MAX) {
                let Ok(Some(row)) = items.file(file) else { continue };
                let Some(path) = row.path.in_package(list.shown_from(member), g) else { continue };
                let Some(path) = path.strip_prefix("/0/") else { continue };
                if let Some(at) = handles.outlined.iter().position(|one| one.path.as_slice() == path.as_bytes()) {
                    files.push((file, u32::try_from(at).unwrap_or(u32::MAX)));
                    heads.push((list.key(member).prefix(), handles.outlined[at].name));
                }
            }
            let at = u32::try_from(groups.len()).unwrap_or(u32::MAX);
            // A member whose environment could not be made is refused by name, never checked
            // under none (rule 3).
            let refused = match (whole, env_refused) {
                (false, _) => Some("a crate of its member's check scope has no names record this command (its `Names` task failed)"),
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
                        if grown.is_ok() {
                            grown = EngineCheckError::push(&mut took, Took { name, label, item: found }, "the bodies found in their crates' records (engine_bodies.rs `groups`)");
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
        member_files.sort_by_key(|(at, _)| *at);
        member_files.dedup_by_key(|(at, _)| *at);
        heads.sort_unstable();
        heads.dedup();
        Ok(Ok(Found { groups, took, files: member_files, members, heads, trees: (packs, corrected) }))
    }

    /// The note of a body that has one: its refusal, or its findings, whose words `check_body`
    /// does not hand out yet (task.rs `BodyOut` holds their count).
    pub(crate) fn note<'p>(&self, name: Name, outlined: &[Outlined<'_>], frame: &'p Frame) -> Option<bstd::mem::String<&'p Frame>> {
        let at = self.ran.binary_search_by_key(&name, |one| one.name).ok()?;
        let one = &self.ran[at];
        let path = outlined.get(one.file as usize).map_or("?", |file| core::str::from_utf8(&file.path).unwrap_or("?"));
        let item = one.label;
        match &one.said {
            Some(Said::Task(error)) => Some(bstd::mem::format_in!(frame, "`{path}`: the body of `{item}` was not checked: {error} ({RULE})")),
            Some(Said::Panicked) => Some(bstd::mem::format_in!(frame, "`{path}`: the check of the body of `{item}` panicked; that body alone failed ({RULE})")),
            Some(Said::Refused(why)) => Some(bstd::mem::format_in!(frame, "`{path}`: the body of `{item}` was not checked: {why} ({RULE})")),
            Some(Said::Unchecked("")) => Some(bstd::mem::format_in!(frame, "`{path}`: body `{item}`: not checked whole: its verdict failed and said no cause (front2 tables/task.rs `said_of`; {RULE})")),
            Some(Said::Unchecked(why)) => Some(bstd::mem::format_in!(frame, "`{path}`: body `{item}`: {why}")),
            None if one.findings > 0 => Some(bstd::mem::format_in!(
                frame,
                "`{path}`: the body of `{item}` has {} finding(s) of types or borrows; their words are not carried out of the body task yet (front2 tables/task.rs `BodyOut` holds their count) ({RULE})",
                one.findings
            )),
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
    /// checked with no finding: what the command's tests read.
    pub(crate) fn notes(&self, outlined: &[Outlined<'_>], frame: &Frame) -> (std::vec::Vec<std::string::String, Global>, u32) {
        let mut notes = std::vec::Vec::new_in(Global);
        let mut clean = 0u32;
        for one in self.ran.iter() {
            match self.note(one.name, outlined, frame) {
                Some(words) => notes.push(std::string::String::from(words.as_str())),
                None => clean += u32::from(one.said.is_none()),
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
        let _ = writeln!(into, "bodies: us a body: sum {}, median {}; {} bodies a CPU second; phase wall {} us against {} us of body CPU ({parallel:.1}x)", sum / 1000, median / 1000, per_cpu_s, self.wall_ns / 1000, sum / 1000);
        for (ns, _, file) in checked.iter().take(10) {
            let path = outlined.get(*file as usize).map_or("?", |one| core::str::from_utf8(&one.path).unwrap_or("?"));
            let _ = writeln!(into, "bodies: slow  {:>9} us  {path}", ns / 1000);
        }
    }
}

/// What every run reads: all read-only and shared.
struct Shared<'s, 'a, 'f> {
    root: &'s Dir<ReadWrite>,
    packs: &'s [blaze_fetch::PackView<'static>],
    corrected: &'s [(u32, &'static str, &'static str)],
    procs: Option<&'static crate::proc_runner::ProcRunner<'static>>,
    groups: &'s [Group<'f>],
    heads: &'s [([u8; 8], Name)],
    view: View<'s, 'a>,
    heap: &'static blaze_own::Heap,
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

/// One run: its member's records opened once, each body checked over one view.
fn run_bodies<'a, 'f>(cx: &Cx, shared: &Shared<'_, 'a, '_>, run: &[One<'f>], clock: &Clock, out: &mut Buffer<'a>, ran: &mut Vec<Ran<'f>, Global>, alloc: SharedAlloc<'a>) {
    // Every body of the run refused for one reason, each named (rule 3).
    let mut refuse = |why: &'static str| {
        for one in run {
            out.done.push(crate::engine_tasks::failed(&one.task, alloc));
            ran.push(Ran { name: one.task.planned.name, label: one.label, file: one.file, ns: 0, findings: 1, said: Some(Said::Refused(why)) });
        }
    };
    let Some(first) = run.first() else { return };
    let Some(group) = shared.groups.get(first.group as usize) else {
        return refuse("a body's member group is gone");
    };
    let frame = cx.frame();
    let g = shared.heap.alloc();
    let Ok(env) = env_in(group.env, frame) else {
        return refuse("its member's environment did not copy into the run (blaze-scope crate_env.rs `EnvLent::copy_in`)");
    };
    let Ok(opened) = crate::run_names::names_records(cx, shared.root, &group.crates, shared.packs, shared.corrected, shared.procs, std::vec::Vec::new_in(Global), env, frame, g) else {
        return refuse("its member's records could not be opened in the run");
    };
    let unused = Recorder::new(RecorderOpts { alloc: g });
    let tables = Tables::new(TablesOpts { records: opened.records, impls: None, lang_crates: &group.lang, reads: &unused });
    let make = ViewMake { view: shared.view };
    // `First`: under `All` clap-root's `Cmd` was refused with a false cycle among core's
    // iterator traits (front2 heads.rs `Store::new`; lane FL).
    let body_frame = BodyFrame { frame, alloc: g, plant_drop: None, cost: None, needs: NeedsMet::First, heads: None, sources: Some(opened.sources) };
    for one in run {
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
        for item in one.items.iter() {
            let mut asked = one.task;
            asked.planned.name = Name::new(Kind::BodyCheck, item.0);
            match work::unwinding(|| check_body(tables, &asked, &body_frame, &make, alloc)) {
                Ok(Ok(body)) => {
                    whole.update(&body.done.out.0);
                    reads.extend_from_slice(&body.done.reads);
                    findings = findings.saturating_add(body.done.findings);
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
                    said = Some(Said::Panicked);
                    break;
                }
            }
        }
        let ns = clock.now().since(began).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
        if let Some(said) = said {
            out.done.push(crate::engine_tasks::failed(&one.task, alloc));
            ran.push(Ran { name, label: one.label, file: one.file, ns, findings: 1, said: Some(said) });
            continue;
        }
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
        done.reads = widen(&done.reads, group, shared.heads, shared.view, alloc);
        // One a failed body: in the request's heap, as the labels are.
        let said = done.failed.then(|| Said::Unchecked(core::str::from_utf8(bstd::mem::format_in!(g, "{why}").into_bytes().leak()).unwrap_or("")));
        ran.push(Ran { name, label: one.label, file: one.file, ns, findings: done.findings, said });
        out.done.push(done);
    }
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
/// holds, sorted and one each.
fn widen<'a>(reads: &[(CellId, ContentId)], group: &Group<'_>, heads: &[([u8; 8], Name)], view: View<'_, 'a>, alloc: SharedAlloc<'a>) -> SVec<'a, (CellId, ContentId)> {
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
            // found, read ABSENT, and the name added defines it, which moves it. The lang rows
            // (`LANG_CELL`) are no row of the table: read whole.
            Kind::CrateScope if id.cell != WHOLE && id.cell != LANG_CELL && view.cell(&CellId { of: id.of, cell: CELLS }) != ABSENT => push(id.of, id.cell),
            Kind::CrateScope => push(id.of, WHOLE),
            Kind::ImplBucket if id.of.subject == ABSENT => {
                for name in group.scope.iter() {
                    push(*name, WHOLE);
                }
            }
            Kind::ImplBucket => match group.buckets.binary_search_by_key(&id.of, |(bucket, _)| *bucket) {
                Ok(at) => push(group.buckets[at].1, WHOLE),
                // A crate outside the scope: its record is whole-read as its own name.
                Err(_) => push(id.of, id.cell),
            },
            // A head no task defines yet (INTERIM, engine_tasks.rs `HEADS`): its crate's files'
            // heads cells, by the key's crate prefix (blaze-id `ItemKey::crate_prefix`). A head
            // of a crate no outline covers (the shelf's) is the toolchain's, in every key.
            Kind::Signature => {
                let mut prefix = [0u8; 8];
                prefix.copy_from_slice(&id.of.subject.0[..8]);
                let from = heads.partition_point(|(p, _)| *p < prefix);
                let files = heads[from..].iter().take_while(|(p, _)| *p == prefix);
                let mut any = false;
                for (_, file) in files {
                    push(*file, crate::engine_tasks::HEADS);
                    any = true;
                }
                if !any {
                    push(id.of, id.cell);
                }
            }
            _ => push(id.of, id.cell),
        }
    }
    // The environment every body was checked under: its script run, whole.
    if let Some(script) = group.script {
        push(script, WHOLE);
    }
    wide.sort_unstable_by_key(|(id, _)| *id);
    wide.dedup_by_key(|(id, _)| *id);
    wide
}
