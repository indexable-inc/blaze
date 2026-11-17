//! A body's TYPES in its `BodyCheck` task, over records (drive.md, "End state: the whole
//! pipeline on the command", piece 7).
//!
//! On the main thread, once a task ([`prepare`]): the names world (made from the `UnitGraph`
//! record, which the task needs first, as names.rs `run` does), then the `CrateScope` record of
//! each workspace member (needed by the task), read once a command from the store
//! ([`Kept`]); the crates whose items record holds the body's file are its OWNERS; each owner's
//! dependencies, followed to the end, and the sysroot's crates are needed in turn. With all of
//! them handed in, the file's job gets a [`TypesJob`]: per owner, its crates' records in reader
//! order (the owner at 0), all of it read-only and `Sync`.
//!
//! On the pool, once a file job ([`FileTypes`]): per owner one front2 `Records` over
//! front_load's `StoreLoad` and one `ProjectWorld::from_records`; the file's bodies indexed by
//! (file, chain); then each body asked is checked ([`FileTypes::check`]). Its diagnostics are
//! [`TYPES`] findings, entry-relative (rule 71). A body the check could not finish (a refusal,
//! a gap, a record that did not answer, an expansion not served, a `const` or `static` body) is
//! one [`TYPES_NOT_CHECKED`] finding saying why: never a pass, and the command's closing line
//! then says "NOT checked: types" (main.rs `check_coverage`).
//!
//! Keys (rule 21): the body's part source is its item's text and the digest of its file with
//! every checked body cut out ([`body_source`]): a signature edit in the same file re-runs its
//! bodies. Its read list is every owner's crates' `CrateScope` (name, out). KNOWN GAPS, both
//! named in RECORDS-PIPELINE.md "Runner 2": (a) a `CrateScope` out is its exports' digest, so a
//! signature edit in ANOTHER file or crate that leaves the exports moves no body's read
//! (piece 9, facts as readers); (b) a body edit leaves its crate's `CrateScope` standing, whose
//! items record holds the file's old digest: front2 reads texts by the whole file's digest, so
//! the edited file's bodies are "not checked: types" until front2 reads at item grain (piece
//! 4's reader half, `ItemsRecord::item_now`).

use core::cell::RefCell;

use blaze_drive::{Input, Key, Name, Task, Want};
use blaze_fetch::PackView;
use blaze_ids::ContentId;
use blaze_ladder::front2::borrows::BorrowRefused;
use blaze_ladder::front2::project::ProjectWorld;
use blaze_ladder::front2::project_records::{ByChain, Listed, index_by_chain};
use blaze_ladder::front2::source::{Log, Records, SourceError};
use blaze_own::{AVec, Alloc, Vec};
use blaze_scope::items_record::{ItemsRecord, ItemsRecordError};
use blaze_scope::resolve::SourceTexts;
use blaze_surface::ItemKind;
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::{Bytes, Frame};
use bstd::path::Path;
use std::alloc::Global;

use crate::CliError;
use crate::front_load::{CrateRecords, StoreLoad};
use crate::job_texts::JobTexts;
use crate::store::Store;
use crate::work::{Facts, Record, held_str};

/// The finding number of a body's type error: its arguments are the checker's code and words.
pub const TYPES: u16 = 17;
/// The finding number of a body whose types were not checked to the end: its one argument says
/// why. It is not an error of the program; it keeps `check` from saying `ok` (main.rs).
pub const TYPES_NOT_CHECKED: u16 = 18;
/// The finding number of a body's move or borrow finding (blaze-ladder front2/borrows.rs): its
/// arguments are the code (`E0382`, `E0502`, .., or `outlives`) and the words; its range is
/// item-relative. A NOTE while [`BORROWS_ARE_ERRORS`] is false (print.rs, main.rs `every_rule`).
pub const BORROWS: u16 = 19;
/// The finding number of a body whose moves and borrows were not checked to the end (the flow
/// lowering refused a form, a trait fact was unknown, the checker hit its bound): its one
/// argument says why. Always a note, never a pass: with the flip it keeps the borrows stage
/// "NOT checked" (main.rs `check_coverage`).
pub const BORROWS_NOT_CHECKED: u16 = 20;
/// THE FLIP: whether a borrow finding ([`BORROWS`]) is an error of the program, and the
/// borrows stage is counted as run when no body filed [`BORROWS_NOT_CHECKED`]. FALSE until the
/// flow census on blaze's own standard layer (crates/blaze-ladder/tests/flow_census.rs, its head
/// line's "findings N") reads 0 false findings: census 3 (docs/v2/plan/design/flow.md, 2026-11-16
/// 12:44, tree 11d224a93a) read "findings 536 (in 206 bodies)", every one false. Change it only
/// with that census's head line quoted from a run on the commit that flips it. Nothing else
/// changes at the flip: every body's borrow verdict is already in its record.
pub const BORROWS_ARE_ERRORS: bool = false;

/// The domain of a body part's source ([`body_source`]). 3: every member asked for the file is
/// in the read list (`TypesJob::members`); a "2" record of a file no member listed read only the
/// graph and could stand after its crate came to list it, so none is found again.
const SOURCE_DOMAIN: &str = "blaze-cli body check source 3";

/// The body checker's identity is not typed in here (decision D105): it is the table's
/// `Row::BodyChecker` (blaze_ids::producer), handed to the drive's tool by the entry
/// (main.rs, home.rs `tool`), and it moves with the checker's code, never by hand.

/// A body part's source: its item's text digest `item` and `around`, the digest of its file
/// with every checked body cut out ([`around_bodies`]): what the body's check reads of its own
/// file. (Before piece 7 it was `item` alone, which no sibling signature edit moved.) The
/// checker's own identity is in the tool (`Row::BodyChecker`, D105).
pub(crate) fn body_source(item: ContentId, around: ContentId) -> ContentId {
    let mut h = blaze_ids::Hasher::new(SOURCE_DOMAIN);
    h.update(&item.0).update(&around.0);
    h.finish()
}

/// The digest of `text` with each range of `bodies` (absolute, sorted, not overlapping) cut
/// out: each piece kept is hashed after its length, so two cuts never read as one, and a
/// body's own length is not in it (a body's findings are item-relative, rule 71: a body that
/// grows moves no other body's answer, test `a_body_edit_reruns_no_surface_and_a_signature_edit_one`).
pub(crate) fn around_bodies(text: &str, bodies: &[(u32, u32)]) -> ContentId {
    let mut h = blaze_ids::Hasher::new("blaze-cli text around bodies 2");
    let bytes = text.as_bytes();
    let mut at = 0usize;
    for (start, end) in bodies.iter().copied() {
        let (start, end) = (start as usize, end as usize);
        if start < at || end > bytes.len() || end < start {
            continue;
        }
        h.update(&((start - at) as u64).to_le_bytes()).update(&bytes[at..start]);
        at = end;
    }
    h.update(&((bytes.len() - at) as u64).to_le_bytes()).update(&bytes[at..]);
    h.finish()
}

/// The `CrateScope` records this command read for its bodies, each copied once a command
/// (`Global`, named: the command's own lifetime), by the input's name and key.
#[derive(Default)]
pub struct Kept {
    read: std::vec::Vec<(Name, Key, CrateRecords<'static>), Global>,
    /// How many records were read from the store, and how many were found here again.
    pub gets: u32,
    pub hits: u32,
}

impl Kept {
    fn records(&mut self, cx: &Cx, store: &mut dyn Store, input: &Input, key: blaze_id::CrateItemKey, scratch: &Frame) -> Result<CrateRecords<'static>, &'static str> {
        if let Some((_, _, records)) = self.read.iter().find(|(name, at, _)| *name == input.name && *at == input.at) {
            self.hits += 1;
            return Ok(*records);
        }
        self.gets += 1;
        let records = crate::front_load::read_crate(cx, store, input, key, scratch, Alloc::global())
            .map_err(|_| "a crate's `CrateScope` record the machine handed in could not be read as its names and items (front_load.rs)")?;
        self.read.push((input.name, input.at, records));
        Ok(records)
    }
}

/// One crate as a body's check reads it: its `CrateScope` input and its records.
#[derive(Clone, Copy)]
pub(crate) struct Reader<'j> {
    name: Name,
    out: ContentId,
    crate_name: &'j str,
    /// The directory the world placed its package at (`CrateInput::shown_from`): what its
    /// items record's package paths are joined onto to read (D96).
    package: &'j str,
    records: CrateRecords<'j>,
}

/// One owner of a file: its crates in reader order (the owner at 0) and its lang crates there.
pub(crate) struct Owner<'j> {
    readers: &'j [Reader<'j>],
    lang: &'j [usize],
}

/// What a file's body job needs for its bodies' types: all read-only and `Sync`.
pub(crate) struct TypesJob<'j> {
    owners: &'j [Owner<'j>],
    packs: &'j [PackView<'j>],
    corrected: &'j [(u32, &'j str, &'j str)],
    /// The file's path in the checkout (tree 0), as the items records list it after `/0/`.
    path: &'j str,
    /// The `UnitGraph` input (name, out): every body's first read; `None` when the command has
    /// no unit graph (no lock file to match), so no names and no types.
    graph: Option<(Name, ContentId)>,
    /// With no graph: the `UnitGraph` read as absent (`blaze_drive::ABSENT`), the body's one
    /// read, so a manifest edit that gives the command a graph (a dependency taken out, a lock
    /// written) moves the read and enters the body again instead of leaving its [`NO_GRAPH`]
    /// note standing (rule 21: the note's input is whether there is a graph; the root's units
    /// do not hold the dependencies, so their out does not move). `None` with a graph.
    units: Option<(Name, ContentId)>,
    /// Every member's `CrateScope` (name, out) whose items record was asked whether it lists
    /// the file: read by every body, owner or not, since "no crate's names record lists this
    /// file" is an answer from them (rule 21: without them a module file created under a
    /// standing crate left the note standing in that home, never in a fresh one).
    members: &'j [(Name, ContentId)],
    /// All of the above as each body reads them ([`reads_of`]): sorted by name, one each.
    reads: &'j [(Name, ContentId)],
}

impl TypesJob<'_> {
    /// Whether a body task handed `inputs` reads what this job was prepared from: every read
    /// it lists ([`FileTypes::new`]: the graph, the members asked, each owner's crates) is an
    /// input with the same out, and with no graph, the task was handed none either. Then
    /// [`prepare`] over `inputs` makes this job again, so the task joins it unprepared: one
    /// prepare a file a round, not one a body (RUNNER 17, the blaze clone: 331,542 prepares,
    /// 51.6 s of the main thread).
    /// O(inputs x log reads): each input is searched in the job's sorted reads.
    pub(crate) fn covered_by(&self, inputs: &[blaze_drive::Input]) -> bool {
        if self.graph.is_none() {
            return !inputs.iter().any(|input| input.name == Name::unit_graph());
        }
        // Each read marked once, so an input named twice cannot stand for a missing one.
        let mut seen: std::vec::Vec<bool, Global> = std::vec::Vec::new_in(Global);
        seen.resize(self.reads.len(), false);
        for input in inputs {
            if let Ok(at) = self.reads.binary_search_by_key(&input.name, |(name, _)| *name) {
                if self.reads[at].1 != input.out {
                    return false;
                }
                seen[at] = true;
            }
        }
        seen.iter().all(|s| *s)
    }
}

/// A job's reads, sorted by name, one each: the graph or the absent units, the members
/// asked, each owner's crates. Every body of the job reads exactly these.
fn reads_of<'j>(graph: Option<(Name, ContentId)>, units: Option<(Name, ContentId)>, members: &[(Name, ContentId)], owners: &[Owner<'_>], jobs: &'j Frame) -> &'j [(Name, ContentId)] {
    let mut reads: Vec<(Name, ContentId), &'j Frame> = Vec::new_in(jobs);
    reads.extend(graph);
    reads.extend(units);
    reads.extend(members.iter().copied());
    for owner in owners {
        reads.extend(owner.readers.iter().map(|r| (r.name, r.out)));
    }
    reads.sort_unstable_by_key(|(name, _)| *name);
    reads.dedup_by_key(|(name, _)| *name);
    reads.leak()
}

/// The note of a body in a command with no unit graph.
const NO_GRAPH: &str = "this command has no unit graph (no lock file to match), so no crate's names, so no types";

/// What a `BodyCheck` task is, before its file's job.
pub(crate) enum Prepared<'j> {
    Need(std::vec::Vec<Want, Global>),
    Waited(Name),
    Refused(&'static str),
    Ready(&'j TypesJob<'j>),
}

/// The command's names world (`facts.names`) over the unit graph `graph`, made once a command
/// when none is made yet: its own step, so the work list times it apart from the reads of the
/// crates' records ([`prepare`]; work.rs's row `BODY_WORLD`). Inner `Err`: the world could not
/// be made (the `CrateScope` tasks say why).
pub(crate) fn names_world<'f>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    store: &mut dyn Store,
    facts: &mut Facts<'f, impl crate::sweep::Stamps>,
    graph: &blaze_drive::Input,
    frame: &'f Frame,
    alloc: Alloc<'_>,
) -> Result<Result<(), &'static str>, CliError> {
    if facts.names.is_some() {
        return Ok(Ok(()));
    }
    if facts.laid.is_none() {
        facts.laid = Some(crate::units::lay(cx, root, &facts.stamps, &mut facts.rows, facts.fetch_env.toolchain, frame, alloc)?);
    }
    let lock = facts.laid.as_ref().and_then(|laid| laid.lock.as_ref()).map(|lockfile| lockfile.lock);
    let members = crate::names::member_dirs(facts.laid.as_ref());
    let read = crate::work::payload(cx, store, graph, frame)?;
    let made = {
        let _cx = cx.span("names: make the world");
        crate::names::world(cx, root, read.payload(), lock, members, &facts.fetch_env)
    };
    match made {
        Ok(made) => {
            crate::names::tell_public(store, &made);
            facts.names = Some(made);
            Ok(Ok(()))
        }
        Err(_) => Ok(Err("the crates' inputs could not be made (names.rs `world`): the `CrateScope` tasks say why")),
    }
}

/// The crates a `BodyCheck` task reads, from its inputs, or what it needs first (see the
/// module docs). `real` is its file's path in the checkout; `jobs` the round's frame.
#[expect(clippy::too_many_arguments, reason = "a task's edge: each argument one handle or frame of it")]
pub(crate) fn prepare<'f: 'j, 'j>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    store: &mut dyn Store,
    facts: &mut Facts<'f, impl crate::sweep::Stamps>,
    task: &Task<'_>,
    real: &str,
    frame: &'f Frame,
    jobs: &'j Frame,
    alloc: Alloc<'_>,
) -> Result<Prepared<'j>, CliError> {
    let cx = cx.span("body types: the crates a body reads");
    if let Some(on) = task.failed.first() {
        return Ok(Prepared::Waited(*on));
    }
    let need_one = |name: Name| {
        let mut wants = std::vec::Vec::with_capacity_in(1, Global);
        wants.push(Want { name, home: Name::unit_graph() });
        wants
    };
    // The graph is always read and read-listed: a body edit's task is entered with it among
    // its last reads, so it makes the world without a second entry to ask for it.
    let Some(graph) = task.inputs.iter().find(|input| input.name == Name::unit_graph()) else {
        if facts.laid.is_none() {
            facts.laid = Some(crate::units::lay(&cx, root, &facts.stamps, &mut facts.rows, facts.fetch_env.toolchain, frame, alloc)?);
        }
        // No graph this command (no lock matched: main.rs gives the drive none to `Start`): the
        // drive has no such task to answer a want of it; each body says so, reading nothing
        // more. A layout read late (here, when nothing it read moved) has a graph whenever the
        // lock matched (`Laid::has_graph`): the want is then answered by the last answer's row.
        if facts.laid.as_ref().is_some_and(|laid| !laid.has_graph()) {
            let units = Some((Name::unit_graph(), blaze_drive::key::ABSENT));
            let reads = reads_of(None, units, &[], &[], jobs);
            let job = TypesJob { owners: &[], packs: &[], corrected: &[], path: held_str(real, jobs), graph: None, units, members: &[], reads };
            return Ok(Prepared::Ready(blaze_own::Box::leak(blaze_own::Box::new_in(job, jobs))));
        }
        return Ok(Prepared::Need(need_one(Name::unit_graph())));
    };
    let graph_read = (graph.name, graph.out);
    if let Err(what) = names_world(&cx, root, store, facts, graph, frame, alloc)? {
        return Ok(Prepared::Refused(what));
    }
    let Some(names) = facts.names.as_ref() else { return Err(CliError::Text { what: "the names world, made above, is gone" }) };
    let kept = &mut facts.bodies;
    let world = names.world();
    let crates = &world.crates;
    let name_at = |at: usize| crate::names::name_of(crates[at].input.key);
    // The name is hashed once per lookup, never per input compared: inside `find` it was a
    // blake3 key derivation per (crate, input) pair, per member, per body (RUNNER 16: 1226 of
    // 3213 main-thread samples of a cold clone check in `derive_key_with`).
    let input_at = |at: usize| {
        let name = name_at(at);
        task.inputs.iter().find(|input| input.name == name)
    };
    let mut wants: std::vec::Vec<Want, Global> = std::vec::Vec::new_in(Global);
    let want = |at: usize, wants: &mut std::vec::Vec<Want, Global>| {
        let name = name_at(at);
        if !wants.iter().any(|w| w.name == name) {
            wants.push(Want { name, home: Name::unit_graph() });
        }
    };
    // The workspace's crates: those whose root is in the checkout (tree 0).
    let mut members: std::vec::Vec<usize, Global> = std::vec::Vec::new_in(Global);
    members.extend((0..crates.len()).filter(|at| crates[*at].input.root.starts_with("/0/")));
    for at in members.iter().copied() {
        if input_at(at).is_none() {
            want(at, &mut wants);
        }
    }
    if !wants.is_empty() {
        return Ok(Prepared::Need(wants));
    }
    // The owners: the members whose items record lists the file; `asked`: every member asked,
    // the body's reads whatever the answer (`TypesJob::members`).
    let mut owners: std::vec::Vec<usize, Global> = std::vec::Vec::new_in(Global);
    let mut asked: Vec<(Name, ContentId), &'j Frame> = Vec::with_capacity_in(members.len(), jobs);
    for at in members.iter().copied() {
        let Some(input) = input_at(at) else { continue };
        let records = match kept.records(&cx, store, input, crates[at].input.key, jobs) {
            Ok(records) => records,
            Err(what) => return Ok(Prepared::Refused(what)),
        };
        let Ok(items) = ItemsRecord::open(records.items, Alloc::global()) else {
            return Ok(Prepared::Refused("a member's items record does not open (body_types.rs)"));
        };
        // A row holds the file's path in the member's package (D96): joined onto the member's
        // directory, past the checkout's tree `/0/`.
        let package = crates[at].input.shown_from;
        // Each joined path is freed as soon as compared (`Global`, named): this runs per body
        // and member, and the command's frame must not grow with that (a 4,000-body check
        // filled it: draw_tests, 2026-11-16).
        let holds = (0..items.files()).any(|i| matches!(items.file(i as u32), Ok(Some(row)) if row.path.in_package(package, Alloc::global()).as_deref().and_then(|p| p.strip_prefix("/0/")) == Some(real)));
        asked.push((input.name, input.out));
        if holds {
            owners.push(at);
        }
    }
    // Each owner's crates and its lang crates: the ONE rule of which crates a body's checking
    // may see (blaze-world check_scope.rs): one side's dependency closure (a proc macro's bodies
    // on the macro side, rule 10), the target's sysroot on the target side, a proc macro the
    // owner names seen for its macros only, and the lang crates chosen by id and side, so the
    // two sides' cores never meet in one table (pm-small, 2026-11-16 19:51: "the lang item
    // `AsyncGenFinished` is declared twice"). The scope's inputs are in the body's read list
    // (the unit graph's out and each seen crate's `CrateScope`: `FileTypes::new`).
    let mut scopes: std::vec::Vec<blaze_world::check_scope::CheckScope<'_>, Global> = std::vec::Vec::new_in(Global);
    // A `#![no_core]` owner (its names record's root head says so) links no sysroot crate and
    // declares its own lang items, as front2's solved index takes it (`Index::new`).
    let mut no_core: std::vec::Vec<bool, Global> = std::vec::Vec::new_in(Global);
    for owner in owners.iter().copied() {
        let Some(input) = input_at(owner) else { continue };
        let records = match kept.records(&cx, store, input, crates[owner].input.key, jobs) {
            Ok(records) => records,
            Err(what) => return Ok(Prepared::Refused(what)),
        };
        let Ok(names) = blaze_scope::names_record::NamesRecord::open(records.names, Alloc::global()) else {
            return Ok(Prepared::Refused("a member's names record does not open (body_types.rs)"));
        };
        let Ok(root) = names.crate_root() else {
            return Ok(Prepared::Refused("a member's names record holds no crate root (body_types.rs)"));
        };
        no_core.push(root.head.no_core);
    }
    for (owner, no_core) in owners.iter().copied().zip(no_core.iter().copied()) {
        let scope = match blaze_world::check_scope::check_scope(world, owner, no_core, alloc) {
            Ok(scope) => scope,
            // A refused owner's own names task names it; a broken list is named by the rule.
            Err(refused) => return Ok(Prepared::Refused(refused.rule())),
        };
        for at in scope.crates() {
            if input_at(at).is_none() {
                want(at, &mut wants);
            }
        }
        scopes.push(scope);
    }
    if !wants.is_empty() {
        return Ok(Prepared::Need(wants));
    }
    let mut built: Vec<Owner<'j>, &'j Frame> = Vec::with_capacity_in(scopes.len(), jobs);
    for scope in scopes.iter() {
        // The owner at 0, then the rest in the world's order (the scope's order).
        let mut readers: Vec<Reader<'j>, &'j Frame> = Vec::with_capacity_in(scope.readers().len(), jobs);
        let mut lang: Vec<usize, &'j Frame> = Vec::new_in(jobs);
        lang.extend(scope.lang_places());
        for at in scope.crates() {
            let Some(input) = input_at(at) else { return Err(CliError::Text { what: "a crate needed above has no input" }) };
            let records = match kept.records(&cx, store, input, crates[at].input.key, jobs) {
                Ok(records) => records,
                Err(what) => return Ok(Prepared::Refused(what)),
            };
            let crate_name = crates[at].input.name;
            let package = held_str(crates[at].input.shown_from, jobs);
            readers.push(Reader { name: input.name, out: input.out, crate_name, package, records });
        }
        built.push(Owner { readers: readers.leak(), lang: lang.leak() });
    }
    // The packs' views and the corrections, copied into the round's frame (each a few words):
    // the world is borrowed through `facts` here, the job outlives that borrow.
    let packs: &'j [PackView<'j>] = Vec::from_slice_in(world.texts.packs(), jobs).leak();
    let corrected: &'j [(u32, &'j str, &'j str)] = Vec::from_slice_in(world.texts.corrected(), jobs).leak();
    let (owners, members) = (built.leak(), asked.leak());
    let reads = reads_of(Some(graph_read), None, members, owners, jobs);
    let job = TypesJob { owners, packs, corrected, path: held_str(real, jobs), graph: Some(graph_read), units: None, members, reads };
    Ok(Prepared::Ready(blaze_own::Box::leak(blaze_own::Box::new_in(job, jobs))))
}


/// One owner's front end over records, made once a file job.
struct Checker<'n> {
    records: &'n Records<'n>,
    load: &'n StoreLoad<'n>,
    world: &'n mut ProjectWorld<'n, &'n Records<'n>>,
    by_chain: ByChain<'n>,
    /// The file's index in the owner's items record.
    file: u32,
}

/// A file job's type checks: per owner its checker, or why none could be made.
pub(crate) struct FileTypes<'n> {
    owners: std::vec::Vec<Result<Checker<'n>, &'n str>, Global>,
    /// The job's reads ([`TypesJob::reads`]): each body's read list.
    reads: &'n [(Name, ContentId)],
    frame: &'n Frame,
    /// The command has no unit graph ([`NO_GRAPH`]).
    graphless: bool,
    /// The job's context: each body's check is a span of it.
    cx: &'n Cx,
}

impl<'n> FileTypes<'n> {
    /// Each owner's checker over `job`, the file's text `text` as the job read it.
    pub(crate) fn new(cx: &'n Cx, root: &'n Dir<ReadWrite>, job: &'n TypesJob<'n>, text: &'n str, frame: &'n Frame) -> FileTypes<'n> {
        let g = Alloc::global();
        let mut owners = std::vec::Vec::with_capacity_in(job.owners.len(), Global);
        for owner in job.owners.iter() {
            let _built = cx.span("body types: build a reader");
            owners.push(checker(cx, root, job, owner, text, frame, g));
        }
        FileTypes { owners, reads: job.reads, frame, graphless: job.graph.is_none(), cx }
    }

    /// The types of the body at `item` of `outline` (over `text`) into `made`, its record so
    /// far (its syntax): its findings, and its read list.
    pub(crate) fn check(&mut self, outline: &blaze_file_items::Outline<'_>, item: &blaze_surface::Item, text: &str, made: &mut Record<Global>) {
        made.reads.extend(self.reads.iter().copied());
        if self.owners.is_empty() {
            made.find(TYPES_NOT_CHECKED, (0, 0), &[if self.graphless { NO_GRAPH } else { "no crate's names record lists this file" }]);
            return;
        }
        if !matches!(item.kind(), ItemKind::Function) {
            made.find(TYPES_NOT_CHECKED, (0, 0), &["a `const` or `static` body: front2 checks fn bodies only"]);
            return;
        }
        let chain = blaze_file_items::item_chain(outline, item, text);
        let g = Alloc::global();
        for owner in self.owners.iter_mut() {
            let checker = match owner {
                Ok(checker) => checker,
                Err(why) => {
                    made.find(TYPES_NOT_CHECKED, (0, 0), &[why]);
                    continue;
                }
            };
            let Some(body) = checker.by_chain.get(checker.file, chain) else {
                made.find(TYPES_NOT_CHECKED, (0, 0), &["front2 lists no body at this item"]);
                continue;
            };
            let (errors, unserved) = (checker.records.errors().len(), checker.load.unserved().len());
            // The types, then (when they were checked clean) the moves and borrows over the same
            // lowered body and `Checked` record (front2/borrows.rs `check_borrows`).
            let (verdict, borrows) = {
                let _checking = self.cx.span("body types: check a body");
                checker.world.check_borrows(body, g)
            };
            let new_error = checker.records.errors().get(errors).map(|error| {
                let asked = checker.load.unserved();
                let asked = asked.get(unserved..).unwrap_or(&[]);
                match error {
                    SourceError::Items { error: ItemsRecordError::Gone { krate, file: None, call: Some(call) }, name, .. } => {
                        // `ItemsRecord::whole` files `Gone` for an expansion in two places: no
                        // row for the call, or the loader gave no text. The loader files every
                        // call it was asked in `unserved`, so which one it is shows here.
                        let which = if asked.iter().any(|(_, c)| c == call) {
                            "the record has the call's expansion row but its expansion texts section holds no text for it (StoreLoad::expansion, names/expansion_texts.rs)"
                        } else {
                            "the record has no expansion row for the call (the loader was never asked)"
                        };
                        bstd::mem::format_in!(self.frame, "the records did not answer: the expansion text of macro call {call:x} in crate `{name}` ({krate:x}), read while checking item `{}`: {which}", crate::item_name::shown_name(item, text))
                    }
                    _ => bstd::mem::format_in!(self.frame, "the records did not answer: {error}"),
                }
            });
            if let Some(words) = new_error {
                made.find(TYPES_NOT_CHECKED, (0, 0), &[words.as_str()]);
                continue;
            }
            // One finding per place and words: two owners of the file (a library and its test
            // unit) that agree on a body say it once.
            for d in verdict.diagnostics.iter() {
                let said = made.findings.iter().any(|&(rule, start, end, from, to)| {
                    let words = &made.messages[from..to];
                    rule == TYPES
                        && (start, end) == (d.start, d.end)
                        && words.len() == d.code.len() + 1 + d.text.len()
                        && words.starts_with(d.code.as_bytes())
                        && words.ends_with(d.text.as_bytes())
                });
                if !said {
                    made.find(TYPES, (d.start, d.end), &[d.code, "\x1f", d.text.as_str()]);
                }
            }
            if checker.load.unserved().len() > unserved {
                made.find(TYPES_NOT_CHECKED, (0, 0), &["a macro call's expansion text is not in its crate's record (StoreLoad::expansion, names/expansion_texts.rs)"]);
            } else if let Some(refused) = &verdict.refused {
                // A head that did not resolve names its item by name and quotes the item's
                // text from the records (rule 19): a key alone says nothing to a reader.
                let named = match refused {
                    blaze_ladder::front2::project::Refused::Check(blaze_ladder::front2::check::CheckFail::Head {
                        error: blaze_ladder::front2::heads::HeadError::Item(key, _),
                        ..
                    }) => blaze_ladder::front2::source::NamesSource::text_of(checker.records, *key),
                    _ => None,
                };
                let words = match named {
                    Some(it) => {
                        let shown = it.text.get(..it.text.len().min(240)).unwrap_or(it.text);
                        bstd::mem::format_in!(self.frame, "{}: {refused:?}; the item is `{}`: {shown:?}", refused.text(), it.name.unwrap_or("_"))
                    }
                    None => bstd::mem::format_in!(self.frame, "{}: {refused:?}", refused.text()),
                };
                made.find(TYPES_NOT_CHECKED, (0, 0), &[words.as_str()]);
            } else if let Some(gap) = verdict.gaps.first() {
                let words = bstd::mem::format_in!(self.frame, "{} questions the front end cannot answer yet, the first {gap:?}", verdict.gaps.len());
                made.find(TYPES_NOT_CHECKED, (0, 0), &[words.as_str()]);
            }
            // The move and borrow check's verdict, filed whatever `BORROWS_ARE_ERRORS` says
            // (print.rs decides how it is shown), so the day it flips no body is made again. One
            // finding per place and words, as the types' above. A body whose types were not
            // checked clean was not borrow checked: its type errors or its `TYPES_NOT_CHECKED`
            // note stand for it (the borrows of an ill-typed body are not asked), so no note
            // repeats it.
            for d in borrows.findings.iter() {
                let said = made.findings.iter().any(|&(rule, start, end, from, to)| {
                    let words = &made.messages[from..to];
                    rule == BORROWS
                        && (start, end) == (d.start, d.end)
                        && words.len() == d.code.len() + 1 + d.text.len()
                        && words.starts_with(d.code.as_bytes())
                        && words.ends_with(d.text.as_bytes())
                });
                if !said {
                    made.find(BORROWS, (d.start, d.end), &[d.code, "\x1f", d.text]);
                }
            }
            if let Some(refused) = &borrows.refused
                && !matches!(refused, BorrowRefused::Untyped(_))
            {
                let name = crate::item_name::shown_name(item, text);
                let words = bstd::mem::format_in!(self.frame, "body `{name}`: {}: {refused:?}", refused.text());
                made.find(BORROWS_NOT_CHECKED, refused.range().unwrap_or((0, 0)), &[words.as_str()]);
            }
        }
    }
}

/// One owner's checker, or why none: its records, its world, its file's bodies by chain.
fn checker<'n>(cx: &'n Cx, root: &'n Dir<ReadWrite>, job: &'n TypesJob<'n>, owner: &'n Owner<'n>, text: &'n str, frame: &'n Frame, g: Alloc<'n>) -> Result<Checker<'n>, &'n str> {
    let words = |what: core::fmt::Arguments<'_>| held_str(bstd::mem::format_in!(frame, "{what}").as_str(), frame);
    let mut crates: Vec<CrateRecords<'n>, &'n Frame> = Vec::with_capacity_in(owner.readers.len(), frame);
    let mut crate_names: Vec<&'n str, &'n Frame> = Vec::with_capacity_in(owner.readers.len(), frame);
    let mut packages: Vec<&'n str, &'n Frame> = Vec::with_capacity_in(owner.readers.len(), frame);
    for reader in owner.readers.iter() {
        crates.push(reader.records);
        crate_names.push(reader.crate_name);
        packages.push(reader.package);
    }
    let (crates, crate_names, packages): (&'n [CrateRecords<'n>], &'n [&'n str], &'n [&'n str]) = (crates.leak(), crate_names.leak(), packages.leak());
    let mut read = std::vec::Vec::new_in(Global);
    read.push((job.path, Some(text)));
    let texts: &'n JobTexts<'n> = blaze_own::Box::leak(blaze_own::Box::new_in(
        JobTexts { cx, root, frame, packs: job.packs, corrected: job.corrected, env: None, read: RefCell::new(read) },
        frame,
    ));
    let load: &'n StoreLoad<'n> = blaze_own::Box::leak(blaze_own::Box::new_in(StoreLoad::new(crates, packages, texts, g), frame));
    let log: &'n Log<'n> = blaze_own::Box::leak(blaze_own::Box::new_in(Log::new(crate_names, g), frame));
    let keys = load.keys(g);
    let records: &'n Records<'n> = blaze_own::Box::leak(blaze_own::Box::new_in(Records::new(keys, load, log, frame, g), frame));
    let world = {
        let _from = cx.span("body types: the world from records");
        ProjectWorld::from_records(records, frame, owner.lang, g).map_err(|e| words(format_args!("the front end over records: {e}")))?
    };
    let world: &'n mut ProjectWorld<'n, &'n Records<'n>> = blaze_own::Box::leak(blaze_own::Box::new_in(world, frame));
    let own = owner.readers.first().ok_or("an owner with no crates")?;
    let items = ItemsRecord::open(own.records.items, g).map_err(|e| words(format_args!("the items record of `{}`: {e:?}", own.crate_name)))?;
    let file = (0..items.files() as u32)
        .find(|i| matches!(items.file(*i), Ok(Some(row)) if row.path.in_package(own.package, g).as_deref().and_then(|p| p.strip_prefix("/0/")) == Some(job.path)))
        .ok_or("the owner's items record no longer lists this file")?;
    let mine: AVec<'n, Listed<'n>> = {
        let listed = world.listed(g);
        let mut mine = AVec::with_capacity_in(listed.len(), g);
        mine.extend(listed.iter().copied().filter(|l| l.entry.is_some_and(|(f, _)| f == file)));
        mine
    };
    let by_chain = index_by_chain(records, &mine, &blaze_file_items::item_chain, g).map_err(|e| words(format_args!("the file's bodies by chain: {e}")))?;
    if let Some(error) = records.errors().first() {
        return Err(words(format_args!("the records did not answer: {error}")));
    }
    Ok(Checker { records, load, world, by_chain, file })
}
