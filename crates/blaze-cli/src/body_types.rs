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

/// The domain of a body part's source ([`body_source`]).
const SOURCE_DOMAIN: &str = "blaze-cli body check source 2";

/// THE PRODUCER of a body's check record (D87): one named value for all the code a body's
/// answer depends on, until the build derives each stage's code identity (D87's end state).
/// It is in every key through the tool (home.rs `tool`), beside the record forms' magics: the
/// `FileItems` record that lists a body's source is made by this code too, so a key of the
/// body alone would find last time's file record and through it last time's body key.
/// BUMP IT (the date and a count) with any change to what a body check answers: blaze-typeck
/// (the checker and its rules), blaze-solve (the trait solver), blaze-lower (the body's
/// lowering), blaze-ladder front2 (heads, the records reader, project.rs `check`), and this
/// file's mapping of a verdict to findings. A record made by an older producer is then a miss
/// by key and the body is made again (test `a_body_record_of_an_older_checker_is_made_again`).
/// The binary's linker id, also in the tool, moves with any code today; this one keeps the
/// promise where it does not (a shared cache across builds of one checker).
/// 2026-11-16.1: built-in operators on literal variables answered without the solver;
/// `Deref { overloaded: Some(Mut) }` for mutable places through an overloaded deref; unsizing
/// through nested pointers; the impl prefilter.
/// 2026-11-16.2: `Self::X` in an impl takes the impl's own arguments (hello-D-2); a block's
/// final operand is chosen after cfg stripping (census14-F).
/// 2026-11-16.3: an async fn's hidden type is its block's future by identity
/// (`PNode::Coroutine`, async P3).
/// 2026-11-16.4: an opaque type's bounds elaborated with supertraits (walk AJ P2); an impl's
/// generics instantiated by kind, const parameters as consts (walk AJ P1).
/// 2026-11-16.5: constant promotion fills `Checked::promoted` (async P10); `AsyncFn*` bounds
/// answered by the solver's builtin rule and an async closure's future named
/// `PNode::Coroutine { ordinal: CLOSURE_BASE + n }` (async P9).
/// 2026-11-16.6: `asm!` lowers to an `Expr::Asm` row (intr P9a): a body with `asm!` is
/// refused by the checker's lowering bridge by name instead of at the macro call.
/// 2026-11-16.7: a C-variadic foreign fn's call takes its `...` arguments, held to C's
/// promotions (E0617, variadic-1).
/// 2026-11-16.8: one key per expansion: a body's expansion scopes are keyed by
/// `blaze_expand::call_key`, which now holds the expander's code (expansion key lane).
/// 2026-11-16.9: the move and borrow check (blaze-flow-lower, blaze-flow; front2/borrows.rs)
/// runs per body: its findings ([`BORROWS`]) and refusals ([`BORROWS_NOT_CHECKED`]) are in the
/// body's record. From now on a change to blaze-flow or blaze-flow-lower bumps this too.
/// 2026-11-16.10: `super let` kept on the lowered `let` row and lent as
/// `BodyView::super_lets`; the builder gives its binding its block's temporary scope (async P11).
pub const BODY_CHECKER: &str = "blaze body checker 2026-11-16.10";

/// A body part's source: its item's text digest `item` and `around`, the digest of its file
/// with every checked body cut out ([`around_bodies`]): what the body's check reads of its own
/// file. (Before piece 7 it was `item` alone, which no sibling signature edit moved.) The
/// checker's own identity is in the tool ([`BODY_CHECKER`]).
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
        // No graph this command (main.rs gives the drive none to `Start`): the drive has no
        // such task to answer a want of it; each body says so, reading nothing more.
        if facts.laid.as_ref().is_some_and(|laid| laid.graph.is_none()) {
            let units = Some((Name::unit_graph(), blaze_drive::key::ABSENT));
            let job = TypesJob { owners: &[], packs: &[], corrected: &[], path: held_str(real, jobs), graph: None, units };
            return Ok(Prepared::Ready(blaze_own::Box::leak(blaze_own::Box::new_in(job, jobs))));
        }
        return Ok(Prepared::Need(need_one(Name::unit_graph())));
    };
    let graph_read = (graph.name, graph.out);
    if facts.names.is_none() {
        if facts.laid.is_none() {
            facts.laid = Some(crate::units::lay(&cx, root, &facts.stamps, &mut facts.rows, facts.fetch_env.toolchain, frame, alloc)?);
        }
        let lock = facts.laid.as_ref().and_then(|laid| laid.lock.as_ref()).map(|lockfile| lockfile.lock);
        let members = crate::names::member_dirs(facts.laid.as_ref());
        let read = crate::work::payload(&cx, store, graph, frame)?;
        let made = {
            let _cx = cx.span("names: make the world");
            crate::names::world(&cx, root, read.payload(), lock, members, &facts.fetch_env)
        };
        match made {
            Ok(made) => {
                crate::names::tell_public(store, &made);
                facts.names = Some(made);
            }
            Err(_) => return Ok(Prepared::Refused("the crates' inputs could not be made (names.rs `world`): the `CrateScope` tasks say why")),
        }
    }
    let Some(names) = facts.names.as_ref() else { return Err(CliError::Text { what: "the names world, made above, is gone" }) };
    let kept = &mut facts.bodies;
    let world = names.world();
    let crates = &world.crates;
    let name_at = |at: usize| crate::names::name_of(crates[at].input.key);
    let input_at = |at: usize| task.inputs.iter().find(|input| input.name == name_at(at));
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
    // The owners: the members whose items record lists the file.
    let mut owners: std::vec::Vec<usize, Global> = std::vec::Vec::new_in(Global);
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
        if holds {
            owners.push(at);
        }
    }
    // Each owner's crates: its dependencies followed to the end, and the sysroot's.
    let sysroot = [world.sysroot.core, world.sysroot.alloc, world.sysroot.std, world.sysroot.bstd];
    let mut sets: std::vec::Vec<std::vec::Vec<usize, Global>, Global> = std::vec::Vec::new_in(Global);
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
        let mut set: std::vec::Vec<usize, Global> = std::vec::Vec::new_in(Global);
        set.push(owner);
        if !no_core {
            set.extend(sysroot.iter().flatten().map(|id| id.0 as usize).filter(|at| *at != owner));
        }
        let mut next = 0;
        while next < set.len() {
            let at = set[next];
            next += 1;
            for (_, dep) in crates[at].input.deps.iter() {
                if !set.contains(dep) {
                    set.push(*dep);
                }
            }
        }
        for at in set.iter().copied() {
            if input_at(at).is_none() {
                want(at, &mut wants);
            }
        }
        sets.push(set);
    }
    if !wants.is_empty() {
        return Ok(Prepared::Need(wants));
    }
    let mut built: Vec<Owner<'j>, &'j Frame> = Vec::with_capacity_in(sets.len(), jobs);
    for (set, no_core) in sets.iter_mut().zip(no_core.iter().copied()) {
        // The owner at 0, then the rest in the world's order.
        set[1..].sort_unstable();
        let mut readers: Vec<Reader<'j>, &'j Frame> = Vec::with_capacity_in(set.len(), jobs);
        let mut lang: Vec<usize, &'j Frame> = Vec::new_in(jobs);
        for (place, at) in set.iter().copied().enumerate() {
            let Some(input) = input_at(at) else { return Err(CliError::Text { what: "a crate needed above has no input" }) };
            let records = match kept.records(&cx, store, input, crates[at].input.key, jobs) {
                Ok(records) => records,
                Err(what) => return Ok(Prepared::Refused(what)),
            };
            let crate_name = crates[at].input.name;
            if place == 0 || (!no_core && matches!(crate_name, "core" | "alloc" | "std" | "bstd" | "blaze_std")) {
                lang.push(place);
            }
            let package = held_str(crates[at].input.shown_from, jobs);
            readers.push(Reader { name: input.name, out: input.out, crate_name, package, records });
        }
        built.push(Owner { readers: readers.leak(), lang: lang.leak() });
    }
    // The packs' views and the corrections, copied into the round's frame (each a few words):
    // the world is borrowed through `facts` here, the job outlives that borrow.
    let packs: &'j [PackView<'j>] = Vec::from_slice_in(world.texts.packs(), jobs).leak();
    let corrected: &'j [(u32, &'j str, &'j str)] = Vec::from_slice_in(world.texts.corrected(), jobs).leak();
    let job = TypesJob { owners: built.leak(), packs, corrected, path: held_str(real, jobs), graph: Some(graph_read), units: None };
    Ok(Prepared::Ready(blaze_own::Box::leak(blaze_own::Box::new_in(job, jobs))))
}

/// The world's texts as a pool worker reads them: the packs and corrections the command read,
/// and the checkout's files read here, each once a job, into the job's frame.
struct JobTexts<'n> {
    cx: &'n Cx,
    root: &'n Dir<ReadWrite>,
    frame: &'n Frame,
    packs: &'n [PackView<'n>],
    corrected: &'n [(u32, &'n str, &'n str)],
    read: RefCell<std::vec::Vec<(&'n str, Option<&'n str>), Global>>,
}

impl<'n> JobTexts<'n> {
    fn checkout(&self, inner: &str) -> Option<&'n str> {
        if inner.split('/').any(|part| part == ".." || part == ".") {
            return None;
        }
        let at = Path::new(inner).ok()?;
        let mut bytes = Bytes::new_in(self.frame, "a body check's checkout files");
        self.root.read_all(self.cx, &at, &mut bytes).ok()?;
        let held: &'n Bytes<'n> = blaze_own::Box::leak(blaze_own::Box::new_in(bytes, self.frame));
        core::str::from_utf8(held.as_slice()).ok()
    }
}

impl<'n> SourceTexts<'n> for JobTexts<'n> {
    fn read(&self, _krate: usize, path: &str) -> Option<&'n str> {
        let rest = path.strip_prefix('/')?;
        let (tree, inner) = rest.split_once('/')?;
        let tree: usize = tree.parse().ok()?;
        if let Some((_, _, text)) = self.corrected.iter().find(|(t, p, _)| *t as usize == tree && *p == inner) {
            return Some(text);
        }
        if tree != 0 {
            return core::str::from_utf8(self.packs.get(tree - 1)?.find(inner)?.bytes).ok();
        }
        if let Some((_, text)) = self.read.borrow().iter().find(|(p, _)| *p == inner) {
            return *text;
        }
        let text = self.checkout(inner);
        self.read.borrow_mut().push((held_str(inner, self.frame), text));
        text
    }
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
    /// Every owner's crates' (`CrateScope` name, out), sorted: each body's read list.
    reads: std::vec::Vec<(Name, ContentId), Global>,
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
        let mut reads: std::vec::Vec<(Name, ContentId), Global> = std::vec::Vec::new_in(Global);
        reads.extend(job.graph);
        reads.extend(job.units);
        let mut owners = std::vec::Vec::with_capacity_in(job.owners.len(), Global);
        for owner in job.owners.iter() {
            reads.extend(owner.readers.iter().map(|r| (r.name, r.out)));
            let _built = cx.span("body types: build a reader");
            owners.push(checker(cx, root, job, owner, text, frame, g));
        }
        reads.sort_unstable_by_key(|(name, _)| *name);
        reads.dedup_by_key(|(name, _)| *name);
        FileTypes { owners, reads, frame, graphless: job.graph.is_none(), cx }
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
                        bstd::mem::format_in!(self.frame, "the records did not answer: the expansion text of macro call {call:x} in crate `{name}` ({krate:x}), read while checking item `{}`: {which}", item.name.map_or("<unnamed>", |r| text.get(r.start as usize..r.end as usize).unwrap_or("<cut>")))
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
                let words = bstd::mem::format_in!(self.frame, "{}: {refused:?}", refused.text());
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
                let name = item.name.map_or("<unnamed>", |r| text.get(r.start as usize..r.end as usize).unwrap_or("<cut>"));
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
        JobTexts { cx, root, frame, packs: job.packs, corrected: job.corrected, read: RefCell::new(read) },
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
