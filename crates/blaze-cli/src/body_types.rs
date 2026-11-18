//! A body's TYPES in its `BodyCheck` task, over records (drive.md, "End state: the whole
//! pipeline on the command", piece 7).
//!
//! On the main thread, once a task ([`prepare`]): the names world (made from the `UnitGraph`
//! record, which the task needs first, as names.rs `run` does), then the file's `Owners` record
//! (blaze-drive owners.rs: the crate roots whose module tree reaches the file, kept by the
//! machine from the outlines); each owner's `CrateScope` and its check scope's crates
//! (blaze-world check_scope.rs), read once a command from the store ([`Kept`]), are needed in
//! turn: no other crate's, so one failed scope blocks only the bodies its crates own. With all of
//! them handed in, the file's job gets a [`TypesJob`]: per owner, its crates' records in reader
//! order (the owner at 0), all of it read-only and `Sync`.
//!
//! On the pool, once a file job ([`FileTypes`]): per owner one front2 `Records` over
//! front_load's `StoreLoad` and one `ProjectWorld::from_records`; the file's bodies indexed by
//! (file, chain); then each body asked is checked ([`FileTypes::check`]) WITH every body it
//! covers (covered.rs: each fn declared in it at any depth; for the root's `fn main` an
//! attribute replaced, the body the expansion wrote, the user's body inside). Its diagnostics are
//! [`TYPES`] findings, entry-relative (rule 71). A body the check could not finish (a refusal,
//! a gap, a record that did not answer, an expansion not served, a `cfg` that does not read) is
//! one [`TYPES_NOT_CHECKED`] finding saying why: never a pass, and the command's closing line
//! then says "NOT checked: types" (main.rs `check_coverage`).
//!
//! Keys (rule 21): the body's part source is its item's text alone ([`body_source`]). Its read
//! list is every owner's crates' `CrateScope` (name, out), its own file's `Surface` (work.rs
//! `bodies_syntax`: a FIRST CUT, marked `COARSE(D114 e)` there), and each head it used, by its
//! `Signature` name and out (heads.rs; D114 (d)): a signature edited in another file or crate
//! moves exactly its readers. Named gaps that stay (drive.md, D114 rulings, "FACTS A BODY
//! MAKES"): a hidden type's auto traits, a const's value, an item written inside another body.

use blaze_drive::{Input, Key, Name, Task, Want};
use blaze_fetch::PackView;
use blaze_ids::ContentId;
use blaze_ladder::front2::borrows::BorrowRefused;
use blaze_ladder::front2::project::ProjectWorld;
use blaze_ladder::front2::project_records::{ByChain, Listed, index_by_chain};
use blaze_ladder::front2::source::{Records, SourceError};
use blaze_own::{AVec, Alloc, Vec};
use blaze_scope::crate_env::EnvLent;
use blaze_scope::items_record::{ItemsRecord, ItemsRecordError};
use blaze_scope::resolve::SourceTexts;
use blaze_surface::ItemKind;
use blaze_world::PackOrigin;
use blaze_world::crate_list::{CrateGraph, CrateList};
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadOnly, ReadWrite};
use bstd::mem::{Bytes, Frame};
use bstd::path::Path;
use std::alloc::Global;

use crate::CliError;
use crate::front_load::{CrateRecords, StoreLoad};
use crate::store::Store;
use crate::work::{Facts, Record, held_str};

/// The heads a body read, as reads at signature grain, and the `Signature` task (D114 (d)).
mod heads;
pub(crate) use heads::handed_reads;

/// Which bodies an outline entry's check covers: its root, and each fn declared in a body it
/// reaches (front2 every_body.rs's work list); the one home of it.
pub(crate) mod covered;

/// The check scopes of a deep crate list in a task-sized frame (`owner_wants`).
#[cfg(test)]
mod scope_tests;

/// The names and keys a body's check and its heads are filed under (`body_check_key`, signature.rs).
#[cfg(test)]
mod key_tests;

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

/// The domain of a body part's source ([`body_source`]). 5: the source is the item's text alone,
/// the file around the bodies gone (D114): a "4" record read no head and could stand after a
/// sibling's signature moved, so none is found again. (4: a body reads its file's `Owners` and
/// only the crates its owners' checks open, no longer every member.)
const SOURCE_DOMAIN: &str = "blaze-cli body check source 5";

/// The body checker's identity is not typed in here (decision D105): it is the table's
/// `Row::BodyChecker` (blaze_ids::producer), handed to the drive's tool by the entry
/// (main.rs, home.rs `tool`), and it moves with the checker's code, never by hand.

/// A body part's source: its item's text digest `item` alone. What else its check reads is in
/// its read list: its file's `Surface` and the `Signature` of each head it used (D114 (d);
/// `around_bodies`, the file's text around its bodies, is gone). The checker's own identity is
/// in the tool (`Row::BodyChecker`, D105).
pub(crate) fn body_source(item: ContentId) -> ContentId {
    let mut h = blaze_ids::Hasher::new(SOURCE_DOMAIN);
    h.update(&item.0);
    h.finish()
}

/// A `BodyCheck`'s key, as the drive forms it (request.rs `locate`): THE ONE DERIVATION in
/// blaze-cli, for whatever must name a body's record before the drive does (run_ids.rs hands
/// `blaze run` each body's key). `tool` is the `BodyCheck` kind's tool (home.rs `tool`); `body`
/// the body's name (work.rs `body_name`, under no condition); `item` its entry's text digest
/// (`Item::hash`); `file` its file's `FileItems` name (the condition's base); `graph` the
/// `UnitGraph`'s out and `owners` the file's `Owners` out this command ([`ABSENT`] for either
/// the command has none of). The task key is (tool, name, [`body_source`]); then the crate list
/// and the file's owners are folded in (blaze-drive key.rs `owned_key`): a body's read names
/// follow from them, and two values under one key are refused (rule 21).
///
/// [`ABSENT`]: blaze_drive::key::ABSENT
pub(crate) fn body_check_key(tool: blaze_drive::key::Tool, body: Name, item: ContentId, file: Name, graph: ContentId, owners: ContentId) -> Key {
    let task = blaze_drive::key::task_key(tool, body, &body_source(item));
    blaze_drive::key::owned_key(task, graph, blaze_drive::key::owners_of(file), owners)
}

/// A read list in its one canonical form, the form the drive stores (request.rs `made_split`):
/// sorted by name, one read a name, the first kept (two owners naming one head read it alike,
/// heads.rs), in place; it returns how many are kept (the caller truncates to it). A job's reads
/// pass through it ([`reads_of`]), so the order owners or crates were visited in never reaches
/// a record (rule 25). OWED: work.rs `bodies_syntax` sorts a body's reads inline the same way;
/// it should call this (not this lane's file).
pub(crate) fn canonical_reads(reads: &mut [(Name, ContentId)]) -> usize {
    reads.sort_by_key(|(name, _)| *name);
    let mut kept = 0;
    for at in 0..reads.len() {
        if kept == 0 || reads[kept - 1].0 != reads[at].0 {
            reads[kept] = reads[at];
            kept += 1;
        }
    }
    kept
}

/// The `CrateScope` records this command read for its bodies, each copied once a command
/// (`Global`, named: the command's own lifetime), by the input's name and key; and the crate
/// list and its trees. OWED (runtime.md C3): [`Kept::records`] is the last COPYING reader, for
/// the drive's `BodyCheck` tasks ([`prepare`]) alone: they read `CrateScope` records the same
/// request put (in the arena, `NotInPlace::Arena`), and `Facts` holds no lifetime of the store's
/// mapping. `blaze run`'s parent reads in place (engine_records.rs `EngineRecords`, run_names.rs).
#[derive(Default)]
pub struct Kept {
    read: std::vec::Vec<(Name, Key, CrateRecords<'static>), Global>,
    /// The `UnitGraph` input's out and its crate list (graph_payload.rs), opened (checked
    /// whole) once a command; `None` in the list when the world could not be made.
    list: Option<(ContentId, Option<CrateList<'static>>)>,
    /// The packs of that list's trees and its corrected files ([`Kept::trees`]), for the
    /// `UnitGraph` out they were opened under; or why they could not be.
    trees: Option<(ContentId, Result<Trees, &'static str>)>,
    /// How many records were read from the store, and how many were found here again.
    pub gets: u32,
    pub hits: u32,
}

/// What a body job serves its dependencies' texts from (job_texts.rs `JobTexts::read`): tree
/// `k` of the crate list is pack `k - 1`, mapped in place; and the files read corrected (D58).
type Trees = (&'static [PackView<'static>], &'static [(u32, &'static str, &'static str)]);

/// The packs could not be held: the heap refused the table's room.
const TREES_REFUSED: &str = "the heap refused the room for the crate list's trees (body_types.rs `open_trees`)";

impl Kept {
    pub(crate) fn records(&mut self, cx: &Cx, store: &mut dyn Store<'_>, input: &Input, key: blaze_id::CrateItemKey, heap: &'static blaze_own::Heap) -> Result<CrateRecords<'static>, &'static str> {
        if let Some((_, _, records)) = self.read.iter().find(|(name, at, _)| *name == input.name && *at == input.at) {
            self.hits += 1;
            return Ok(*records);
        }
        self.gets += 1;
        let records = crate::front_load::read_crate(cx, store, input, key, heap.alloc())
            .map_err(|_| "a crate's `CrateScope` record the machine handed in could not be read as its names and items (front_load.rs)")?;
        self.read.push((input.name, input.at, records));
        Ok(records)
    }

    /// The crate list of the `UnitGraph` input `graph`, read and opened once a command (its
    /// bytes copied out of `scratch` into `Global`, named: the command's own lifetime, as the
    /// records above). `None`: the graph's world could not be made (its record says why).
    pub(crate) fn list(&mut self, cx: &Cx, store: &mut dyn Store<'_>, graph: &Input, scratch: &Frame) -> Result<Option<CrateList<'static>>, CliError> {
        if let Some((out, list)) = self.list
            && out == graph.out
        {
            return Ok(list);
        }
        let read = crate::work::payload(cx, store, graph, scratch)?;
        let sections = read.unit_graph(graph.name)?;
        let mut held = std::vec::Vec::with_capacity_in(sections.list.len(), Global);
        held.extend_from_slice(sections.list);
        let bytes: &'static [u8] = held.leak();
        let list = crate::graph_payload::Sections { graph: &[], list: bytes }.crates().map_err(|error| CliError::Input { name: graph.name, why: crate::work::InputWhy::Graph(error) })?;
        self.list = Some((graph.out, list));
        Ok(list)
    }

    /// The packs `list`'s trees name, mapped read-only in place and opened once a command (on
    /// `env.heap`, the command's own lifetime, never a task's frame and never per body), and its
    /// corrected files: what every body job of the command serves another tree's text from.
    /// `graph` is the `UnitGraph` input's out the list was read under: the packs are the list's,
    /// so a body's `(UnitGraph, out)` read is the read of every pack it opens (`TypesJob::reads`).
    pub(crate) fn trees(&mut self, cx: &Cx, env: &crate::fetch::FetchEnv<'_>, graph: ContentId, list: &CrateList<'static>, scratch: &Frame) -> Result<Trees, &'static str> {
        if let Some((out, trees)) = self.trees
            && out == graph
        {
            return trees;
        }
        let trees = open_trees(cx, env, list, scratch);
        self.trees = Some((graph, trees));
        trees
    }
}

/// Each tree of `list` opened from where the table names it (blaze-world crate_list.rs, the
/// tree table): a toolchain pack from the shelf by its digest, a fetched record from the sources
/// store by its key, each mapped whole (`Dir::map_read`, nothing copied) and kept on `env.heap`.
/// A pack is opened as a record verified when it was installed or fetched
/// (`PackView::open_verified`, rows and form checked, bytes not hashed again), and refused unless
/// its files are the ones the list's world read (`TreeRow::files`) and, on the shelf, its footer
/// is its name. A refusal names the tree, its file and why; nothing is served in its place.
fn open_trees(cx: &Cx, env: &crate::fetch::FetchEnv<'_>, list: &CrateList<'static>, scratch: &Frame) -> Result<Trees, &'static str> {
    let _opening = cx.span("body types: open the crate list's packs");
    let from = PackHome::of(env);
    let mut views: AVec<'static, PackView<'static>> = AVec::new_in(from.heap.alloc());
    if views.try_reserve_exact(list.trees()).is_err() {
        return Err(TREES_REFUSED);
    }
    let mut dirs = PackDirs::default();
    for k in 1..=list.trees() {
        // Within the room reserved above: allocates nothing.
        views.push(open_tree(cx, from, list, k, &mut dirs, scratch)?);
    }
    Ok((views.leak(), corrected_of(list, from.heap)?))
}

/// `list`'s corrected files (D58), on `heap` for the command: read from the list, nothing opened.
pub(crate) fn corrected_of(list: &CrateList<'static>, heap: &'static blaze_own::Heap) -> Result<&'static [(u32, &'static str, &'static str)], &'static str> {
    let mut corrected: AVec<'static, (u32, &'static str, &'static str)> = AVec::new_in(heap.alloc());
    for (tree, path, text) in list.corrected() {
        if corrected.try_reserve(1).is_err() {
            return Err(TREES_REFUSED);
        }
        corrected.push((tree, path, text));
    }
    Ok(corrected.leak())
}

/// Where a crate list's packs are opened from: the home (the shelf and the sources store are
/// beneath it), the file system it is reached through, and the heap each pack's mapping is kept
/// on for the command.
#[derive(Clone, Copy)]
pub(crate) struct PackHome<'e> {
    pub(crate) home: Option<&'e str>,
    pub(crate) fs: &'e bstd::fs::Fs,
    pub(crate) heap: &'static blaze_own::Heap,
}

impl<'e> PackHome<'e> {
    pub(crate) fn of(env: &'e crate::fetch::FetchEnv<'_>) -> PackHome<'e> {
        PackHome { home: env.home, fs: &env.fs, heap: env.heap }
    }
}

/// The two folders packs are opened from, each opened at the first pack asked of it.
#[derive(Default)]
pub(crate) struct PackDirs {
    shelf: Option<Dir<ReadOnly>>,
    sources: Option<Dir<ReadOnly>>,
}

/// Tree `k` (1-based) of `list` opened from where the table names it ([`open_trees`] says how
/// and what is refused): THE ONE OPENER of a tree, for both schedules (every tree before the
/// jobs, [`open_trees`]; each on its first read, [`LazyTrees`]). Its folder is opened in `dirs`
/// the first time a tree of it is asked. A refusal's words are kept on `from.heap`.
fn open_tree(cx: &Cx, from: PackHome<'_>, list: &CrateList<'static>, k: usize, dirs: &mut PackDirs, scratch: &Frame) -> Result<PackView<'static>, &'static str> {
    let heap = from.heap;
    let kept = |words: &str| -> &'static str { blaze_own::Box::leak(blaze_own::AString::from_str_in(words, heap.alloc()).into_boxed_str()) };
    let Some(row) = list.tree(k) else {
        return Err(kept(bstd::mem::format_in!(scratch, "the crate list names no tree {k} of its {} (blaze-world crate_list.rs)", list.trees()).as_str()));
    };
    let (folder, name, dir) = match row.origin {
        PackOrigin::Shelf { digest } => (crate::toolchain::SHELF, digest, &mut dirs.shelf),
        PackOrigin::Source { key } => (crate::fetch::SOURCES, key.0, &mut dirs.sources),
    };
    let hex = crate::fetch::key_hex(&ContentId(name));
    let hex = core::str::from_utf8(&hex).map_err(|_| "a pack's name is not hex (fetch.rs `key_hex`)")?;
    let at = bstd::mem::format_in!(scratch, "tree {k} of the crate list, `<home>/{folder}/{hex}`");
    if dir.is_none() {
        let Some(home) = from.home else {
            return Err(kept(bstd::mem::format_in!(scratch, "{at}: HOME is not set, and the pack is beneath it").as_str()));
        };
        let path = Path::new(folder).map_err(|e| kept(bstd::mem::format_in!(scratch, "{at}: {e}").as_str()))?;
        let opened = from.fs.home(cx, home, &path).map_err(|e| kept(bstd::mem::format_in!(scratch, "{at}: {e}").as_str()))?;
        *dir = Some(opened.read_only());
    }
    let Some(dir) = dir.as_ref() else {
        return Err(kept(bstd::mem::format_in!(scratch, "{at}: its folder was not opened").as_str()));
    };
    let file = Path::new(hex).map_err(|e| kept(bstd::mem::format_in!(scratch, "{at}: {e}").as_str()))?;
    let mapping = dir.map_read(cx, &file).map_err(|e| kept(bstd::mem::format_in!(scratch, "{at} could not be mapped: {e}").as_str()))?;
    // Kept for the command: every body job reads the pack's files in place.
    let mapping: &'static blaze_core::Mapping = blaze_own::Box::leak(blaze_own::Box::new_in(mapping, heap.alloc()));
    let bytes = mapping.bytes();
    if let PackOrigin::Shelf { digest } = row.origin
        && bytes.get(bytes.len().saturating_sub(32)..) != Some(&digest[..])
    {
        return Err(kept(bstd::mem::format_in!(scratch, "{at}: the shelf's file is not the pack of its name (its footer is another digest)").as_str()));
    }
    let view = PackView::open_verified(bytes).map_err(|e| kept(bstd::mem::format_in!(scratch, "{at} is no pack: {}", blaze_text::Shown(e)).as_str()))?;
    if view.tree().0 != row.files {
        return Err(kept(bstd::mem::format_in!(scratch, "{at} holds other files than the ones the crate list's world read (its listing's BLAKE3 differs)").as_str()));
    }
    Ok(view)
}

/// What opens tree `k` of a crate list at its first read ([`LazyTrees`]): the list's own
/// opener ([`ListTrees`], over [`open_tree`]), or a test's.
pub(crate) trait OpenTree {
    fn open(&self, k: usize) -> Result<PackView<'static>, &'static str>;
}

/// [`open_tree`] over one crate list, its folders opened at the first pack asked of each.
pub(crate) struct ListTrees<'n> {
    pub(crate) cx: &'n Cx,
    pub(crate) from: PackHome<'n>,
    pub(crate) list: &'n CrateList<'static>,
    pub(crate) scratch: &'n Frame,
    pub(crate) dirs: core::cell::RefCell<PackDirs>,
}

impl OpenTree for ListTrees<'_> {
    fn open(&self, k: usize) -> Result<PackView<'static>, &'static str> {
        let _opening = self.cx.span("run names: open a tree at its first read");
        // One job, one thread (the run's parent): the folders' borrow ends with this open.
        let mut dirs = self.dirs.borrow_mut();
        open_tree(self.cx, self.from, self.list, k, &mut dirs, self.scratch)
    }
}

/// A slot of [`LazyTrees`]: empty until its tree's first read, then the pack or why not.
pub(crate) type TreeSlot = core::cell::OnceCell<Result<PackView<'static>, &'static str>>;

/// A crate list's trees, each opened at the first read of a text from it and at most once
/// (`blaze run`'s parent, run_names.rs `open`): a run after an edit of one body maps the packs of
/// the crates its walk reads, not of the whole closure. Handed out by tree index as the opened
/// slice is ([`crate::job_texts::Packs`]). READS (the doc of [`Kept::trees`]): the read is
/// DECLARED UP FRONT, never at the open: the packs are the list's, so a reader's `(UnitGraph,
/// out)` read is the read of every pack it may open, whichever it opens. A tree that could not
/// be opened is kept as its refusal: [`TreesSeen::refused`] names it and the run stops on it.
pub(crate) struct LazyTrees<'n> {
    opener: &'n dyn OpenTree,
    /// One a tree of the list: its cap is the list's tree count, made at the start.
    slots: &'n [TreeSlot],
    /// What was opened and refused, kept by the caller past the names records' life.
    seen: &'n TreesSeen,
}

/// What a [`LazyTrees`] opened, of how many, and the first tree it could not open: the run's
/// closing line (run_names.rs `NamesRead`) and its stop (main.rs at `Effect::StartProgram`).
#[derive(Default, Debug)]
pub(crate) struct TreesSeen {
    opened: core::cell::Cell<u32>,
    trees: core::cell::Cell<u32>,
    refused: core::cell::Cell<Option<&'static str>>,
}

impl TreesSeen {
    /// (trees opened, trees of the list).
    pub(crate) fn opened(&self) -> (u32, u32) {
        (self.opened.get(), self.trees.get())
    }

    /// The first tree a read asked that could not be opened, by its words.
    pub(crate) fn refused(&self) -> Option<&'static str> {
        self.refused.get()
    }
}

impl<'n> LazyTrees<'n> {
    /// `trees` slots, none opened, in `frame`; counted in `seen`.
    pub(crate) fn new(opener: &'n dyn OpenTree, trees: usize, seen: &'n TreesSeen, frame: &'n Frame) -> LazyTrees<'n> {
        let mut slots: Vec<TreeSlot, &'n Frame> = Vec::with_capacity_in(trees, frame);
        slots.extend((0..trees).map(|_| TreeSlot::new()));
        seen.trees.set(u32::try_from(trees).unwrap_or(u32::MAX));
        LazyTrees { opener, slots: slots.leak(), seen }
    }

    /// Tree `k`'s pack (1-based), opened at its first ask; `None` for no such tree, or one that
    /// could not be opened (its refusal kept, the first one in [`TreesSeen::refused`]).
    pub(crate) fn tree(&self, k: usize) -> Option<PackView<'n>> {
        let slot = self.slots.get(k.checked_sub(1)?)?;
        let got = slot.get_or_init(|| {
            self.seen.opened.set(self.seen.opened.get() + 1);
            self.opener.open(k)
        });
        match got {
            Ok(view) => Some(*view),
            Err(why) => {
                if self.seen.refused.get().is_none() {
                    self.seen.refused.set(Some(*why));
                }
                None
            }
        }
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
    /// Its index in the names world: the proc-macro runner's key for a call written in it
    /// (one per unit: a package's lib and bin share `package` and differ here).
    world_at: usize,
    /// A workspace member's crate (`CrateList::member`: its root in the checkout): its heads are
    /// read by `Signature` (heads.rs); any other crate's are under its `CrateScope` identity.
    member: bool,
    records: CrateRecords<'j>,
}

/// One owner of a file: its crates in reader order (the owner at 0) and its lang crates there.
pub(crate) struct Owner<'j> {
    readers: &'j [Reader<'j>],
    lang: &'j [usize],
    /// Why this owner has no crates: its root is no crate of the names world ([`NOT_IN_WORLD`]).
    missing: Option<&'j str>,
    /// The body's file is this owner's crate root (its `CrateInput::root` after `/0/`): where
    /// the root's `fn main` is (covered.rs `root_of`).
    root_file: bool,
    /// The owner crate's environment, what its bodies' `env!`/`option_env!` read: made by
    /// blaze-scope's `CrateEnv::of` from the crate list's facts of it (the world's own, rule
    /// 30) with its build script's outputs joined ([`owner_env`]). Lent: each worker copies it
    /// into its frame. `None` for an owner with no crates.
    env: Option<EnvLent<'j, 'j>>,
    /// The owner's build-script run read for `env` (name, out): in every body's read list, so a
    /// script that sets another `rustc-env` value reruns the bodies ([`reads_of`]).
    script: Option<(Name, ContentId)>,
}

/// The `Script` part's name of the crate at list row `at`, when it has a build script whose run
/// is a task; refused by name when the row names a kind of no task.
fn owner_script(list: &CrateList<'_>, at: usize) -> Result<Option<Name>, &'static str> {
    match list.script(at) {
        None => Ok(None),
        Some(bytes) => crate::native_image::script_name(bytes).map(Some).ok_or("a crate list row names a build-script run of no kind (crate_list.rs `script`)"),
    }
}

/// The environment of the owner crate at list row `at`: `CrateEnv::of` of the list's facts of
/// it (the ones the world made the crate's item-level environment from) and, when it has a
/// build script, the outputs record of its run (`task`'s input, asked in [`prepare`]), whose
/// `OUT_DIR` files are read from the home's file area as the names task reads them
/// (names/scope.rs). Made in `jobs`, lent to the pool. Refused by name: a script record that is
/// not an outputs record, a version that is not SemVer.
fn owner_env<'j>(
    cx: &Cx,
    store: &mut dyn Store<'_>,
    task: &Task<'_>,
    list: &CrateList<'static>,
    at: usize,
    home: Option<&str>,
    jobs: &'j Frame,
) -> Result<Result<OwnerEnv<'j>, &'static str>, CliError> {
    owner_env_by(cx, store, &|script| task.inputs.iter().find(|input| input.name == script).copied(), list, at, home, jobs)
}

/// [`owner_env`] with the build-script run's input found by `handed` (a drive task's inputs, or
/// the engine's Scripts stage, engine_bodies.rs).
pub(crate) fn owner_env_by<'j>(
    cx: &Cx,
    store: &mut dyn Store<'_>,
    handed: &dyn Fn(Name) -> Option<Input>,
    list: &CrateList<'static>,
    at: usize,
    home: Option<&str>,
    jobs: &'j Frame,
) -> Result<Result<OwnerEnv<'j>, &'static str>, CliError> {
    let erased: &'j blaze_own::Erased<&'j Frame> = blaze_own::Box::leak(blaze_own::Box::new_in(blaze_own::Erased::new(jobs), jobs));
    let alloc = erased.handle();
    let mut facts: blaze_scope::crate_env::EnvFacts<'j> = list.env_facts(at);
    let mut read = None;
    let script = match owner_script(list, at) {
        Ok(script) => script,
        Err(refused) => return Ok(Err(refused)),
    };
    if let Some(script) = script {
        let Some(handed) = handed(script) else {
            return Ok(Err("a body's owner's build-script run, asked for, is not among its inputs"));
        };
        let payload = crate::work::payload(cx, store, &handed, jobs)?;
        let bytes: &'j [u8] = blaze_own::Vec::from_slice_in(payload.payload(), jobs).leak();
        let Some((_, record)) = crate::scripts::payload_parts(bytes) else {
            return Ok(Err("a body's owner's build-script record is not a `Script` payload (scripts.rs `payload_parts`)"));
        };
        let Ok(outputs) = blaze_build_script::outputs::OutputsRef::open(record) else {
            return Ok(Err("a body's owner's build-script record is not an outputs record (blaze-build-script `OutputsRef::open`)"));
        };
        let area: &'j blaze_world::StoredFiles = blaze_own::Box::leak(blaze_own::Box::new_in(crate::scripts::file_area(home, &outputs), jobs));
        facts.script = Some(outputs.with_area(area));
        read = Some((handed.name, handed.out));
    }
    let Ok(env) = blaze_scope::crate_env::CrateEnv::of(facts, alloc) else {
        return Ok(Err("a body's owner's package version is not MAJOR.MINOR.PATCH (the cargo book, \"The `version` field\")"));
    };
    let env: &'j blaze_scope::crate_env::CrateEnv<'j> = blaze_own::Box::leak(blaze_own::Box::new_in(env, jobs));
    Ok(Ok((Some(EnvLent::new(env)), read)))
}

/// An owner's lent environment and the script read it was made with ([`owner_env`]).
pub(crate) type OwnerEnv<'j> = (Option<EnvLent<'j, 'j>>, Option<(Name, ContentId)>);

/// The note of an owner whose root file the file's `Owners` names and no crate of the names
/// world has as its root (a unit the graph does not make, such as a test target's root).
const NOT_IN_WORLD: &str = "an owner of this file (a crate root its `Owners` record names) is no crate of the names world, so no crate's names are read for it";

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
    /// The file's `Owners` input (name, out): which crate roots reach the file (blaze-drive
    /// owners.rs). Its roots decide the owners below; none is an answer (the `Owners` record
    /// carries the ownerless finding), so a body of no owner is checked by no one and says
    /// nothing. `None` with no graph.
    owned: Option<(Name, ContentId)>,
    /// All of the above as each body reads them ([`reads_of`]): sorted by name, one each.
    reads: &'j [(Name, ContentId)],
    /// The command's proc-macro runner (`Names::procs`): a body's proc-macro calls run through it.
    procs: Option<&'j crate::proc_runner::ProcRunner<'static>>,
}

impl TypesJob<'_> {
    /// Whether a body task handed `inputs` reads what this job was prepared from: every read
    /// it lists ([`FileTypes::new`]: the graph, the file's `Owners`, each owner's crates) is an
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

/// A job's reads, sorted by name, one each: the graph or the absent units, the file's
/// `Owners`, the crates each owner's check opened. Every body of the job reads exactly these.
fn reads_of<'j>(graph: Option<(Name, ContentId)>, units: Option<(Name, ContentId)>, owned: Option<(Name, ContentId)>, owners: &[Owner<'_>], jobs: &'j Frame) -> &'j [(Name, ContentId)] {
    let mut reads: Vec<(Name, ContentId), &'j Frame> = Vec::new_in(jobs);
    reads.extend(graph);
    reads.extend(units);
    reads.extend(owned);
    for owner in owners {
        reads.extend(owner.readers.iter().map(|r| (r.name, r.out)));
        reads.extend(owner.script);
    }
    let kept = canonical_reads(&mut reads);
    reads.truncate(kept);
    reads.leak()
}

/// The note of a body in a command with no unit graph.
const NO_GRAPH: &str = "this command has no unit graph (no lock file to match), so no crate's names, so no types";

/// What a `BodyCheck` task is, before its file's job.
pub(crate) enum Prepared<'j> {
    Need(std::vec::Vec<Want, Global>),
    Waited(Name),
    Refused(&'static str),
    /// Not run: an input every body reads answered that it could not be made, and that
    /// input's own record holds the finding that says why. The body is no error of its own: it
    /// is filed as one `NOT_RUN` finding naming this cause (not_checked/tasks.rs), and the
    /// command prints the root finding once and one count of the tasks it stopped.
    NotRun(&'static str),
    Ready(&'j TypesJob<'j>),
}

/// The cause a body names when its `UnitGraph` input holds no crate list ([`Prepared::NotRun`]):
/// the graph's world could not be made, and the graph's record carries the finding that says why.
pub(crate) const WORLD_NOT_MADE: &str = "the crates' inputs (names.rs `world`; the `UnitGraph` record's finding above says why)";

/// The words of a body task whose file's list of owner scopes could not be had from the heap.
const SCOPES_REFUSED: &str = "the heap refused the list of a file's owner check scopes (blaze-cli body_types.rs `prepare`)";

/// The crates owner `at`'s check scope reads, each handed to `want` (`no_core` false: the
/// larger scope, so one need covers both; see [`prepare`]). The scope is made in `scratch` and
/// its bytes released before this returns (a frame scope): a scope over an `n`-crate list is
/// about `25 n` bytes and a command makes one per owner per file, so in a frame that never
/// frees they add up to the list's size times the files (tempo: the request's 64 MB frame
/// refused after 4.5 s). The high-water mark of `scratch` is one scope's.
pub(crate) fn owner_wants<'n>(list: &impl CrateGraph<'n>, at: usize, scratch: &mut Frame, mut want: impl FnMut(usize)) -> Result<(), blaze_world::check_scope::ScopeError<'n>> {
    scratch.scope(|scratch| {
        let erased = blaze_own::Erased::new(scratch);
        let scope = blaze_world::check_scope::check_scope_in(list, at, false, erased.handle())?;
        for seen in scope.crates() {
            want(seen);
        }
        Ok(())
    })
}

/// The command's names world (`facts.names`) over the unit graph `graph`, made once a command
/// when none is made yet: for a task that needs the world itself (work.rs, a `MacroModule`
/// task's script module); a body reads the crate list instead ([`prepare`]). Inner `Err`: the
/// world could not be made (the `CrateScope` tasks say why).
pub(crate) fn names_world<'f>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    store: &mut dyn Store<'_>,
    facts: &mut Facts<'f, impl crate::sweep::Stamps>,
    graph: &blaze_drive::Input,
    frame: &'f Frame,
    alloc: Alloc<'_>,
) -> Result<Result<(), &'static str>, CliError> {
    if facts.names.is_some() {
        return Ok(Ok(()));
    }
    if facts.laid.is_none() {
        facts.laid = Some(crate::units::lay(cx, root, &facts.stamps, &mut facts.rows, facts.fetch_env.toolchain, facts.fetch_env.heap, frame, alloc)?);
    }
    let lock = facts.laid.as_ref().and_then(|laid| laid.lock.as_ref()).map(|lockfile| lockfile.lock);
    let members = crate::names::member_dirs(facts.laid.as_ref());
    let read = crate::work::payload(cx, store, graph, frame)?;
    let made = {
        let _cx = cx.span("names: make the world");
        crate::names::world(cx, root, read.unit_graph(graph.name)?.graph, lock, members, &facts.fetch_env)
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
/// module docs). `real` is its file's path in the checkout; `jobs` the round's frame; `frame`
/// and `alloc` the request's (what the command keeps: its layout); `scratch` the task's scratch
/// frame, which each owner's first check scope is made in and released from ([`owner_wants`]).
/// The scopes it keeps to the end are on the heap the command was handed (`facts.fetch_env.heap`),
/// freed when it returns: never in a frame that does not free (tempo, 1,072 crates, 2026-11-17
/// 00:34: "the allocator refused 12448 bytes aligned to 8 for `(usize, ..::Seen)`" after 4.5 s,
/// the request's frame full of one file's scopes after another).
#[expect(clippy::too_many_arguments, reason = "a task's edge: each argument one handle or frame of it")]
pub(crate) fn prepare<'f: 'j, 'j>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    store: &mut dyn Store<'_>,
    facts: &mut Facts<'f, impl crate::sweep::Stamps>,
    task: &Task<'_>,
    real: &str,
    frame: &'f Frame,
    jobs: &'j Frame,
    scratch: &mut Frame,
    alloc: Alloc<'_>,
) -> Result<Prepared<'j>, CliError> {
    let cx = cx.span("body types: the crates a body reads");
    if let Some(on) = task.failed.first() {
        return Ok(Prepared::Waited(*on));
    }
    // The file's `Owners` (with `first` before it): asked beside the graph when both are
    // missing, one entry for the two (neither depends on the other).
    let need_owners = |first: Option<Name>| {
        let mut wants = std::vec::Vec::with_capacity_in(2, Global);
        wants.extend(first.map(|name| Want { name, home: Name::unit_graph() }));
        if !task.inputs.iter().any(|input| input.name.kind == blaze_drive::Kind::Owners) {
            wants.push(Want { name: blaze_drive::key::owners_of(task.home), home: task.home });
        }
        wants
    };
    // The graph is always read and read-listed: a body edit's task is entered with it among
    // its last reads, so it makes the world without a second entry to ask for it.
    let Some(graph) = task.inputs.iter().find(|input| input.name == Name::unit_graph()) else {
        if facts.laid.is_none() {
            facts.laid = Some(crate::units::lay(&cx, root, &facts.stamps, &mut facts.rows, facts.fetch_env.toolchain, facts.fetch_env.heap, frame, alloc)?);
        }
        // No graph this command (no lock matched: main.rs gives the drive none to `Start`): the
        // drive has no such task to answer a want of it; each body says so, reading nothing
        // more. A layout read late (here, when nothing it read moved) has a graph whenever the
        // lock matched (`Laid::has_graph`): the want is then answered by the last answer's row.
        if facts.laid.as_ref().is_some_and(|laid| !laid.has_graph()) {
            let units = Some((Name::unit_graph(), blaze_drive::key::ABSENT));
            let reads = reads_of(None, units, None, &[], jobs);
            let job = TypesJob { owners: &[], packs: &[], corrected: &[], path: held_str(real, jobs), graph: None, units, owned: None, reads, procs: None };
            return Ok(Prepared::Ready(blaze_own::Box::leak(blaze_own::Box::new_in(job, jobs))));
        }
        return Ok(Prepared::Need(need_owners(Some(Name::unit_graph()))));
    };
    let graph_read = (graph.name, graph.out);
    // The crates as the `UnitGraph` record lists them, one row a crate read in place (drive.md,
    // "The crate list is a record"): no world is made here, on a cold check or an edit.
    let heap = facts.fetch_env.heap;
    // Where a build script's stored `OUT_DIR` files are (an owner's environment, [`owner_env`]).
    let home = facts.fetch_env.home;
    let kept = &mut facts.bodies;
    let Some(list) = kept.list(&cx, store, graph, jobs)? else {
        return Ok(Prepared::NotRun(WORLD_NOT_MADE));
    };
    let name_at = |at: usize| blaze_drive::key::crate_scope_name(&list.key(at));
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
    // The file's owners (drive.md, "A body's owners, and what it waits for"): the crate roots
    // whose module tree reaches it, a fact the machine keeps from the outlines (blaze-drive
    // owners.rs), never a scan of the members' scopes. Each root is a crate of the world by its
    // root file (`/0/` and the checkout path the units record named it by, units.rs). The
    // `Owners` input is found by kind: a body reads one, its file's.
    let Some(owned) = task.inputs.iter().find(|input| input.name.kind == blaze_drive::Kind::Owners) else {
        return Ok(Prepared::Need(need_owners(None)));
    };
    let owned_read = (owned.name, owned.out);
    let roots = {
        let read = crate::work::payload(&cx, store, owned, jobs)?;
        let roots = blaze_drive::owners::roots(read.payload()).map_err(|error| CliError::Input { name: owned.name, why: crate::work::InputWhy::Malformed(error) })?;
        let mut held: Vec<blaze_drive::owners::Root, &'j Frame> = Vec::new_in(jobs);
        held.extend(roots);
        held.leak()
    };
    // Each root's crates (a root file may be more than one crate's: never assumed one), or the
    // root's path when the world has no crate of it: that owner's bodies say so.
    let mut owners: std::vec::Vec<usize, Global> = std::vec::Vec::new_in(Global);
    let mut missing: std::vec::Vec<blaze_drive::owners::Root, Global> = std::vec::Vec::new_in(Global);
    for root in roots.iter() {
        let before = owners.len();
        // A member's root is named by its checkout path (units.rs); any other crate's by its path
        // in the world's form (DRIVER runner 19, `names::dep_roots`, ruled 21:3x).
        let named = |path: &str| Name::file(path.strip_prefix("/0/").unwrap_or(path).as_bytes());
        owners.extend((0..CrateGraph::len(&list)).filter(|at| named(list.root(*at)) == root.file));
        if owners.len() == before {
            missing.push(*root);
        }
    }
    // Each owner's crate and its check scope's crates, asked in ONE need: the scope is a function
    // of the world alone but for `no_core` (its names record's, read below), and the scope
    // with the sysroot is the larger, so asking that one never needs a second entry (a third
    // need a body was one entry more than the members scan's two: blaze-cli test
    // `a_body_edit_reruns_no_surface_and_a_signature_edit_one`, 21:29). A `#![no_core]` owner
    // then waits on its sysroot's scopes, which it does not read.
    for at in owners.iter().copied() {
        if input_at(at).is_none() {
            want(at, &mut wants);
        }
        // The owner's build-script run, when it has one: its `OUT_DIR` and `rustc-env` pairs
        // are the crate's environment, which its bodies' `env!` reads ([`owner_env`]).
        match owner_script(&list, at) {
            Ok(Some(script)) if !task.inputs.iter().any(|input| input.name == script) && !wants.iter().any(|w| w.name == script) => {
                wants.push(Want { name: script, home: Name::unit_graph() });
            }
            Ok(_) => {}
            Err(refused) => return Ok(Prepared::Refused(refused)),
        }
        let asked = owner_wants(&list, at, scratch, |seen| {
            if input_at(seen).is_none() {
                want(seen, &mut wants);
            }
        });
        if let Err(refused) = asked {
            return Ok(Prepared::Refused(refused.rule()));
        }
    }
    if !wants.is_empty() {
        return Ok(Prepared::Need(wants));
    }
    // Each owner's crates and its lang crates: the ONE rule of which crates a body's checking
    // may see (blaze-world check_scope.rs): one side's dependency closure (a proc macro's bodies
    // on the macro side, rule 10), the target's sysroot on the target side, a proc macro the
    // owner names seen for its macros only, and the lang crates chosen by id and side, so the
    // two sides' cores never meet in one table (pm-small, 2026-11-16 19:51: "the lang item
    // `AsyncGenFinished` is declared twice"). The scope's inputs are in the body's read list
    // (the unit graph's out and each seen crate's `CrateScope`: `FileTypes::new`).
    // Kept until the owners are built below, then freed: on the heap, each scope too.
    let mut scopes: AVec<'static, blaze_world::check_scope::CheckScope<'static>> = AVec::new_in(heap.alloc());
    if scopes.try_reserve_exact(owners.len()).is_err() {
        return Ok(Prepared::Refused(SCOPES_REFUSED));
    }
    // A `#![no_core]` owner (its names record's root head says so) links no sysroot crate and
    // declares its own lang items, as front2's solved index takes it (`Index::new`).
    let mut no_core: std::vec::Vec<bool, Global> = std::vec::Vec::new_in(Global);
    for owner in owners.iter().copied() {
        let Some(input) = input_at(owner) else { continue };
        let records = match kept.records(&cx, store, input, list.key(owner), heap) {
            Ok(records) => records,
            Err(what) => return Ok(Prepared::Refused(what)),
        };
        let root = blaze_scope::names_record::NamesRecord::open(records.names_proof).crate_root();
        no_core.push(root.head().no_core);
    }
    for (owner, no_core) in owners.iter().copied().zip(no_core.iter().copied()) {
        let scope = match blaze_world::check_scope::check_scope_in(&list, owner, no_core, heap.alloc()) {
            Ok(scope) => scope,
            // A refused owner's own names task names it; a broken list is named by the rule.
            Err(refused) => return Ok(Prepared::Refused(refused.rule())),
        };
        for at in scope.crates() {
            if input_at(at).is_none() {
                want(at, &mut wants);
            }
        }
        // Within the room reserved above (one an owner): allocates nothing.
        scopes.push(scope);
    }
    if !wants.is_empty() {
        return Ok(Prepared::Need(wants));
    }
    let mut built: Vec<Owner<'j>, &'j Frame> = Vec::with_capacity_in(scopes.len(), jobs);
    for (scope, owner_at) in scopes.iter().zip(owners.iter().copied()) {
        let root_file = list.root(owner_at).strip_prefix("/0/") == Some(real);
        // The owner at 0, then the rest in the list's order, by crate key (the scope's order).
        let mut readers: Vec<Reader<'j>, &'j Frame> = Vec::with_capacity_in(scope.readers().len(), jobs);
        let mut lang: Vec<usize, &'j Frame> = Vec::new_in(jobs);
        lang.extend(scope.lang_places());
        for at in scope.crates() {
            let Some(input) = input_at(at) else { return Err(CliError::Text { what: "a crate needed above has no input" }) };
            let records = match kept.records(&cx, store, input, list.key(at), heap) {
                Ok(records) => records,
                Err(what) => return Ok(Prepared::Refused(what)),
            };
            // The list's strings live for the command (`Kept::list`, `Global`).
            let crate_name = CrateGraph::name(&list, at);
            let package = list.shown_from(at);
            readers.push(Reader { name: input.name, out: input.out, crate_name, package, world_at: at, member: list.member(at), records });
        }
        let (env, script) = match owner_env(&cx, store, task, &list, owner_at, home, jobs)? {
            Ok(made) => made,
            Err(refused) => return Ok(Prepared::Refused(refused)),
        };
        built.push(Owner { readers: readers.leak(), lang: lang.leak(), missing: None, root_file, env, script });
    }
    for _ in missing.iter() {
        built.push(Owner { readers: &[], lang: &[], missing: Some(NOT_IN_WORLD), root_file: false, env: None, script: None });
    }
    // The packs and corrections the list's tree table names, opened once a command and kept on
    // the heap (`Kept::trees`): no world is made for them (rule 25: one answer whatever made
    // the world first). Their identities are the list's bytes, so the `(UnitGraph, out)` read
    // every body lists is the read of each pack it may open: a pack is content (its name is its
    // digest or its lock-derived key, its files checked against the list), never edited in
    // place, and a different pack is a different list. Per-pack reads would be reads no task
    // of the request settles (heads.rs: the drive's stale loop refuses them).
    let (packs, corrected) = match facts.bodies.trees(&cx, &facts.fetch_env, graph.out, &list, jobs) {
        Ok(trees) => trees,
        Err(why) => return Ok(Prepared::Refused(why)),
    };
    let owners = built.leak();
    let reads = reads_of(Some(graph_read), None, Some(owned_read), owners, jobs);
    let job = TypesJob { owners, packs, corrected, path: held_str(real, jobs), graph: Some(graph_read), units: None, owned: Some(owned_read), reads, procs: facts.names.as_ref().and_then(|names| names.procs()) };
    Ok(Prepared::Ready(blaze_own::Box::leak(blaze_own::Box::new_in(job, jobs))))
}


/// The macro calls of a refused lowering whose expansion failed by its own words (blaze-ladder
/// body.rs `BodyFinding::Macro` whose cause is `Failed(Diagnostic ..`: an unset `env!`
/// variable, `compile_error!`, no rule matched): each call's range, path and the expansion's
/// words, and the first other finding (something blaze cannot take: a host refusal is blaze's
/// gap, not the program's error, so the body is then not checked too). `None` for any other
/// refusal, or a lowering with no such call.
fn failed_calls(refused: &blaze_ladder::front2::project::Refused) -> Option<FailedCalls<'_>> {
    use blaze_ladder::front2::body::{BodyError, BodyFinding};
    let blaze_ladder::front2::project::Refused::Lower(BodyError::Lowered { findings, .. }) = refused else { return None };
    let failed = |f: &&BodyFinding| matches!(f, BodyFinding::Macro { cause, .. } if cause.starts_with("Failed(Diagnostic"));
    if !findings.iter().any(|f| failed(&f)) {
        return None;
    }
    // A macro call node left unlowered with no expander finding of its own is the place a
    // failed expansion wrote nothing (the call of `dep::missing_of!()` whose `env!` failed):
    // the failed call's error stands for it. A call the expander could not run has its own
    // `Macro` finding, so it still stops the body below.
    let leftover = |f: &&BodyFinding| matches!(f, BodyFinding::Unhandled { syntax: blaze_file_items::SyntaxKind::MACRO_CALL, .. });
    let other = findings.iter().find(|f| !failed(f) && !leftover(f));
    // Where each failed call is shown: the user-written call that wrote nothing (the leftover
    // node) holding it, else the first such, else its own range. A call inside an expansion
    // has a range past the item's text (body.rs `BodyFinding`), which names no place in the
    // user's file; the call written in the file is what Rust points at ("in this macro
    // invocation").
    let sites: std::vec::Vec<blaze_surface::Range, Global> = findings
        .iter()
        .filter_map(|f| match f {
            BodyFinding::Unhandled { syntax: blaze_file_items::SyntaxKind::MACRO_CALL, range, .. } => Some(*range),
            _ => None,
        })
        .collect();
    let mut calls: std::vec::Vec<(blaze_surface::Range, &str, &str), Global> = std::vec::Vec::new();
    for f in findings.iter().filter(failed) {
        let BodyFinding::Macro { at, path, why, .. } = f else { continue };
        let site = sites.iter().find(|s| s.start <= at.start && at.end <= s.end).or(sites.first()).copied().unwrap_or(*at);
        let call = (site, &**path, &**why);
        // One call met twice (the expander's findings of a call and of its expansion) is one
        // error.
        if !calls.iter().any(|(r, p, w)| (r.start, r.end, *p, *w) == (site.start, site.end, call.1, call.2)) {
            calls.push(call);
        }
    }
    Some(FailedCalls { calls, other })
}

/// [`failed_calls`]'s answer: each failed call (range, path, the expansion's words), and the
/// first finding that is not one (the body is then also not checked, by that finding's class).
struct FailedCalls<'r> {
    calls: std::vec::Vec<(blaze_surface::Range, &'r str, &'r str), Global>,
    other: Option<&'r blaze_ladder::front2::body::BodyFinding>,
}

/// A lowering finding's class for a not-checked note (reach.rs `lower_class`'s form, one
/// finding): what groups items, with the macro's path for a call.
fn finding_class(f: &blaze_ladder::front2::body::BodyFinding) -> std::string::String {
    use blaze_ladder::front2::body::BodyFinding;
    match f {
        BodyFinding::Macro { path, cause, .. } => std::format!("Lowered: Macro {cause}, the call of `{path}!`"),
        BodyFinding::Unhandled { syntax, .. } => std::format!("Lowered: Unhandled {syntax:?}"),
        BodyFinding::Diag { kind, .. } => std::format!("Lowered: Diag {kind:?}"),
        BodyFinding::CfgOut => std::string::String::from("Lowered: CfgOut"),
    }
}

/// The words of a record error, in `frame`: for a file text the loader did not serve
/// (`ItemsRecordError::Gone` of file `n` of reader `krate`'s items record, no macro call), the
/// file's path as the world placed it (`/k/src/x.rs`), its record, and why no text is kept
/// there: the checkout has no such file, the pack of tree `k` holds none, or the crate list
/// names no tree `k`. Any other error in its own words. The mapping is here, where the readers
/// and the packs are, not in blaze-scope: the items record knows a package-relative path and
/// no tree.
fn record_words<'f>(error: &SourceError<'_>, readers: &[Reader<'_>], packs: &[PackView<'_>], frame: &'f Frame, g: Alloc<'_>) -> bstd::mem::String<&'f Frame> {
    let SourceError::Items { krate: at, name, error: ItemsRecordError::Gone { krate, file: Some(file), call: None } } = error else {
        return bstd::mem::format_in!(frame, "{error}");
    };
    let path = readers.get(*at).and_then(|reader| {
        let items = ItemsRecord::open(reader.records.items, g).ok()?;
        match items.file(*file) {
            Ok(Some(row)) => row.path.in_package(reader.package, g),
            _ => None,
        }
    });
    let Some(path) = path else {
        return bstd::mem::format_in!(frame, "the text of file {file} of the items record of crate `{name}` ({krate:?}) is not kept, and the record names no path for it");
    };
    let tree = path.strip_prefix('/').and_then(|p| p.split('/').next()).and_then(|t| t.parse::<usize>().ok());
    let why = match tree {
        Some(0) => "the checkout holds no such file now",
        Some(k) if k <= packs.len() => "the pack of its tree (the crate list's tree table) holds no such file",
        Some(_) => "the crate list's tree table names no such tree",
        None => "its path names no tree",
    };
    bstd::mem::format_in!(frame, "the text of `{path}` (file {file} of the items record of crate `{name}`, {krate:?}) is not kept: {why}")
}

/// One owner's front end over records, made once a file job.
struct Checker<'n> {
    records: &'n Records<'n>,
    load: &'n StoreLoad<'n>,
    /// The owner's front end, OWNED here so it is dropped with the checker when the file job
    /// ends: its impl buckets and head store are maps on the global heap, which no frame reset
    /// frees. Leaked into the job's frame (`Box::leak`, as it was until 2026-11-17), every file
    /// job's buckets stayed for the rest of the command: tempo's malloc history at 240 s
    /// (port27/tempo-deps/mem-g081341-msl/calltree-240.txt) had 7.9 GB of 17.9 GB of the heap
    /// under `pooled_bodies`, 8.1 GB of it filed by `Buckets::admit_asked`/`put`/`push`, and
    /// growing with every job (1.0 -> 1.7 -> 3.4 GB at 150, 195, 240 s for `admit_asked`).
    world: blaze_own::Box<ProjectWorld<'n, &'n Records<'n>>, &'n Frame>,
    by_chain: ByChain<'n>,
    /// The file's index in the owner's items record.
    file: u32,
    /// The listed bodies whose entry is in this file: what `by_chain` indexes, and a root's
    /// `Listed` (its place) is taken from (covered.rs `root_of`).
    mine: AVec<'n, Listed<'n>>,
    /// [`Owner::root_file`].
    root_file: bool,
    /// The owner's crates in reader order (the owner at 0): where a head's file is found.
    readers: &'n [Reader<'n>],
    /// The job's packs ([`TypesJob::packs`]): what [`record_words`] says a text was not in.
    packs: &'n [PackView<'n>],
    /// The heads' files and outs read so far this job (heads.rs).
    heads: heads::Heads<'n>,
    /// The bodies macros of the owner's crate wrote at module level, each by the entry its call
    /// is at (covered.rs `written_by`): made at the first entry that can have expansions, once
    /// a job.
    written: Option<AVec<'n, covered::WrittenBy<'n>>>,
    /// The owner's crate's cfg (its items record's atoms): which entries its crate holds
    /// ([`Checker::holds`]).
    cfg: blaze_scope::task::Cfg<'n, 'n>,
    /// The allocator the checker was made on (`FileTypes::heap`): the heads' files and digests too.
    heap: Alloc<'n>,
}

/// One owner of a file as its job holds it.
///
/// An owner is a crate root whose module tree reaches the file in its TEXT (blaze-drive
/// owners.rs, read from the outlines, under no cfg); its crate is that tree under ITS cfg (the
/// Reference, "Conditional compilation": an item whose `cfg` is false "is not compiled", and a
/// `mod` item is one). A library and its test unit own `#[cfg(test)] mod tests;` alike, and only
/// the test unit's crate holds it: in the library's, the file and its items do not exist, so
/// that owner has nothing there to check, and the test unit checks them. Whether an owner's crate
/// holds the file is its items record's answer ([`checker`]); whether it holds an entry, the
/// entry's and its enclosing entries' `cfg` under the crate's atoms ([`Checker::holds`]).
enum OwnerCheck<'n> {
    /// The owner's checker over the file.
    Checks(Checker<'n>),
    /// The owner's crate does not hold the file: its items record lists no such file (the `mod`
    /// that reaches it is left out under the crate's cfg).
    NotInCrate,
    /// Why no checker could be made: each entry's note names it.
    Refused(&'n str),
}

impl<'n> OwnerCheck<'n> {
    fn checker(&self) -> Option<&Checker<'n>> {
        match self {
            OwnerCheck::Checks(checker) => Some(checker),
            OwnerCheck::NotInCrate | OwnerCheck::Refused(_) => None,
        }
    }

    fn checker_mut(&mut self) -> Option<&mut Checker<'n>> {
        match self {
            OwnerCheck::Checks(checker) => Some(checker),
            OwnerCheck::NotInCrate | OwnerCheck::Refused(_) => None,
        }
    }
}

/// What an owner's crate holds of one outline entry ([`Checker::holds`]).
enum Holds {
    /// The entry is in the crate: it is checked.
    Yes,
    /// A `cfg` on the entry or an entry it is written in is false under the crate's atoms (or it
    /// is a `#[test]` fn outside test mode, the Reference `attributes.testing.test.enabled`): it
    /// is not in this crate, and this owner has nothing to check there.
    No,
    /// A `cfg` predicate there does not read (a form blaze-scope's `Cfg::eval` does not know):
    /// the entry is named, never passed over (rule 3).
    Unread,
}

/// The note of an entry whose `cfg` predicate does not read ([`Holds::Unread`]).
const CFG_UNREAD: &str = "a `cfg` predicate on it or on an item it is written in does not read";

/// A file job's type checks: per owner its checker, or why none could be made.
pub(crate) struct FileTypes<'n> {
    owners: std::vec::Vec<OwnerCheck<'n>, Global>,
    /// The job's reads ([`TypesJob::reads`]): each body's read list.
    reads: &'n [(Name, ContentId)],
    frame: &'n Frame,
    /// The command has no unit graph ([`NO_GRAPH`]).
    graphless: bool,
    /// The job's context: each body's check is a span of it.
    cx: &'n Cx,
    /// PLANT (work::Plant::HeadReadLeftOut): each body drops its first head read.
    left_out: bool,
    /// The heap the checkers' worlds and each body's check are made on (the process's, handed
    /// down through the job): freed as each is dropped, where the job's frame would keep them.
    heap: Alloc<'n>,
}

impl<'n> FileTypes<'n> {
    /// Each owner's checker over `job`, the file's text `text` as the job read it, made on `heap`
    /// (`'static`: each checker's head store keeps `'static` tables, front2 `Store::new`).
    pub(crate) fn new(cx: &'n Cx, root: &'n Dir<ReadWrite>, job: &'n TypesJob<'n>, text: &'n str, frame: &'n Frame, heap: Alloc<'static>) -> FileTypes<'n> {
        let mut owners = std::vec::Vec::with_capacity_in(job.owners.len(), Global);
        for owner in job.owners.iter() {
            let _built = cx.span("body types: build a reader");
            owners.push(match checker(cx, root, job, owner, text, frame, heap) {
                Ok(Some(checker)) => OwnerCheck::Checks(checker),
                Ok(None) => OwnerCheck::NotInCrate,
                Err(why) => OwnerCheck::Refused(why),
            });
        }
        FileTypes { owners, reads: job.reads, frame, graphless: job.graph.is_none(), cx, left_out: false, heap }
    }

    /// The types of the body at `item` of `outline` (over `text`) into `made`, its record so
    /// far (its syntax): its findings, and its read list. `handed`: the `Signature`s its task
    /// was handed (heads.rs `handed_reads`), each head read at its out; a head whose
    /// `Signature` was not handed is pushed to `needs`, and the task answers those instead.
    pub(crate) fn check(
        &mut self,
        outline: &blaze_file_items::Outline<'_>,
        item: &blaze_surface::Item,
        text: &str,
        made: &mut Record<Global>,
        handed: &[(Name, ContentId)],
        needs: &mut std::vec::Vec<Want, Global>,
    ) {
        made.reads.extend(self.reads.iter().copied());
        // The item as a reader finds it: every note below names it (not_checked/items.rs).
        let item_path = crate::not_checked::items::item_path(outline, item, text, self.frame);
        let item_path = item_path.as_str();
        if self.owners.is_empty() {
            // With a graph, no owner is the file's `Owners` answer: that record carries the one
            // ownerless finding (blaze-drive `OWNERLESS_RULE`), so its bodies say nothing more.
            if self.graphless {
                crate::not_checked::items::file(made, (0, 0), item_path, NO_GRAPH, "");
            }
            return;
        }
        // An entry with a body of its own: a fn's, or a `const`'s or `static`'s initializer
        // (the Reference, "Constant items", "Static items": an expression of the declared type,
        // checked as a fn's body is, its own task keyed by its text like a fn's).
        let body_entry = matches!(item.kind(), ItemKind::Function | ItemKind::Const | ItemKind::Static);
        // The entry's index in its file's outline: what an expansion's origins name as the
        // entry its call is at (covered.rs `callers`). Only an entry a macro can take
        // (work.rs `may_expand`) asks for the crate's macro-written bodies.
        let entry_at = outline.items().iter().position(|i| core::ptr::eq(i, item)).map(|i| blaze_surface::ItemIndex(i as u32));
        let expands = crate::work::may_expand(outline, item, text);
        let chain = blaze_file_items::item_chain(outline, item, text);
        let name = crate::item_name::shown_name(item, text);
        let g = self.heap;
        let (cx, frame) = (self.cx, self.frame);
        for owner in self.owners.iter_mut() {
            let checker = match owner {
                OwnerCheck::Checks(checker) => checker,
                // Not this owner's file: another owner's crate holds it (enum doc).
                OwnerCheck::NotInCrate => continue,
                OwnerCheck::Refused(why) => {
                    crate::not_checked::items::file(made, (0, 0), item_path, "its owner's front end over records could not be made", why);
                    continue;
                }
            };
            match checker.holds(outline, item, text) {
                Holds::Yes => {}
                // Not in this owner's crate: nothing of it to check here (enum doc).
                Holds::No => continue,
                Holds::Unread => {
                    crate::not_checked::items::file(made, (0, 0), item_path, CFG_UNREAD, "");
                    continue;
                }
            }
            // Which bodies this entry covers: one home, covered.rs. Its own body (a fn's), then
            // every body an expansion at it wrote at module level (one `BodyCheck` an entry, all
            // its bodies: a macro call's, a derive's impl fns, an attribute's output). An entry
            // with no root and no expansion body is a note naming the item: never a pass.
            let mut roots: AVec<'_, covered::Root<'_>> = AVec::new_in(g);
            let mut no_root = None;
            if body_entry {
                match covered::root_of(checker.records, &checker.by_chain, &checker.mine, checker.file, chain, item, text, checker.root_file) {
                    Ok(root) => roots.push(root),
                    Err(no) => no_root = Some(no),
                }
            }
            if expands && let Some(at) = entry_at {
                let file = checker.file;
                let mut calls: AVec<'_, blaze_id::ItemKey> = AVec::new_in(g);
                for w in checker.written_now().iter().filter(|w| w.top == Ok((file, at))) {
                    if !roots.iter().any(|r| r.listed().body == w.listed.body) {
                        roots.push(covered::Root::Expanded { listed: w.listed, by: w.by });
                    }
                    if !calls.contains(&w.call) {
                        calls.push(w.call);
                    }
                }
                if !calls.is_empty() {
                    checker.rules_reads(&calls, handed, made, needs, item_path, frame);
                }
            }
            if roots.is_empty()
                && let Some(no) = no_root
            {
                let words = bstd::mem::format_in!(frame, "{no}");
                crate::not_checked::items::file(made, (0, 0), item_path, no.cause(), words.as_str());
                continue;
            }
            for root in roots.iter().copied() {
            let (errors, unserved) = (checker.records.errors().len(), checker.load.unserved().len());
            // The root and every fn declared in a body it reaches, at any depth (front2
            // every_body.rs's work list): each one's types, then (when they were checked clean)
            // its moves and borrows over the same lowered body and `Checked` record (front2/
            // borrows.rs `check_borrows`).
            let reached = covered::each_covered(&mut checker.world, root, g, |w, body| {
                let _checking = cx.span("body types: check a body");
                w.check_borrows(body, g)
            });
            let new_error = checker.records.errors().get(errors).map(|error| {
                let asked = checker.load.unserved();
                let asked = asked.get(unserved..).unwrap_or(&[]);
                match error {
                    SourceError::Items { error: ItemsRecordError::Gone { krate, file: None, call: Some(call) }, name: krate_name, .. } => {
                        // `ItemsRecord::whole` files `Gone` for an expansion in two places: no
                        // row for the call, or the loader gave no text. The loader files every
                        // call it was asked in `unserved`, so which one it is shows here.
                        let which = if asked.iter().any(|(_, c)| c == call) {
                            "the record has the call's expansion row but its expansion texts section holds no text for it (StoreLoad::expansion, names/expansion_texts.rs)"
                        } else {
                            "the record has no expansion row for the call (the loader was never asked)"
                        };
                        bstd::mem::format_in!(frame, "the records did not answer: the expansion text of macro call {call:x} in crate `{krate_name}` ({krate:x}), read while checking item `{name}`: {which}")
                    }
                    _ => {
                        let said = record_words(error, checker.readers, checker.packs, frame, checker.heap);
                        bstd::mem::format_in!(frame, "the records did not answer while checking item `{name}`: {said}")
                    }
                }
            });
            // The heads each covered body used, each a read by its `Signature` (heads.rs):
            // listed whatever the verdict, so a body not checked to the end is entered again
            // when a head moves.
            let mut reads = heads::HeadReads::new(handed, &mut *needs, self.left_out);
            for r in reached.iter() {
                heads::read_heads(checker, &r.got.0, item, item_path, frame, made, &mut reads);
            }
            if let Some(words) = new_error {
                crate::not_checked::items::file(made, (0, 0), item_path, "the records did not answer", words.as_str());
                continue;
            }
            let unserved_now = checker.load.unserved().len() > unserved;
            // The root's label: the entry's name, or the expansion and the body it wrote.
            let label = match root {
                covered::Root::Expanded { .. } => bstd::mem::format_in!(frame, "{}", covered::Expanded { records: checker.records, root }),
                _ => bstd::mem::format_in!(frame, "{}", root.label(name)),
            };
            for (at, r) in reached.iter().enumerate() {
                // The checker key of each fn this check covered (the entry's and every fn a body
                // it reached declares), filed so the record names it (work.rs `Record::covered`,
                // sealed sorted). A member whose key its world never filed has none to name.
                let covered = match r.listed.body {
                    blaze_ladder::front2::project::BodyRef::Free(lib) => Some(blaze_ladder::front2::keys::checker_of(lib)),
                    blaze_ladder::front2::project::BodyRef::Member { container, kind, name, .. } => {
                        checker.world.front.keys.filed_member(blaze_ladder::front2::keys::checker_of(container), kind, name)
                    }
                };
                made.covered.extend(covered);
                let (verdict, borrows) = &r.got;
                let shown = covered::Shown { reached: &reached, at, root: label.as_str() };
                let place = covered::place(root, r);
                // The entry's own written body says its findings as before; every other body
                // covered says which it is, and a body a macro wrote that its place is not
                // known (covered.rs, "WHERE A FINDING GOES").
                let prefix = match place {
                    Some(_) if r.within.is_none() => bstd::mem::format_in!(frame, ""),
                    Some(_) => bstd::mem::format_in!(frame, "in `{shown}`: "),
                    None => {
                        let unplaced = covered::WRITTEN_UNPLACED;
                        bstd::mem::format_in!(frame, "in `{shown}`, {unplaced}")
                    }
                };
                let at_entry = |start: u32, end: u32| -> (u32, u32) {
                    match place.and_then(|shift| Some((shift.checked_add(start)?, shift.checked_add(end)?))) {
                        Some(range) => range,
                        None => (0, 0),
                    }
                };
                // A name that a refused macro call of its module may have made is no error of
                // the code: the refusal is its cause, said once, and the body is not checked,
                // counted under it (not_checked/items.rs), once a body and call however many
                // names. The cause is the words after the marker: the call and why it was refused
                // (blaze-ladder front2 path_miss_shown.rs `refused_call`), so one call's items
                // group under one line that names it.
                let mut refused_causes: blaze_own::Vec<&str, _> = blaze_own::Vec::new_in(frame);
                for d in verdict.diagnostics.iter() {
                    if let Some(at) = d.text.find(blaze_ladder::front2::MACRO_REFUSED) {
                        let cause = &d.text[at + blaze_ladder::front2::MACRO_REFUSED.len()..];
                        if !refused_causes.contains(&cause) {
                            crate::not_checked::items::file(made, at_entry(d.start, d.end), item_path, cause, d.text.as_str());
                            refused_causes.push(cause);
                        }
                        continue;
                    }
                    covered::find_once(made, TYPES, at_entry(d.start, d.end), d.code, prefix.as_str(), d.text.as_str());
                }
                // The typed lints' findings (blaze-typeck rules/args_inline.rs), numbered as the
                // lint they are (lints.rs `number_of`): filed at every step, no level kept; print.rs
                // gives each the step's level (rule 81). The message is one piece, whole.
                for d in verdict.findings.iter() {
                    match blaze_lint::Rule::from_name(d.lint) {
                        Some(rule) => made.find(crate::lints::number_of(rule), at_entry(d.start, d.end), &[prefix.as_str(), d.text.as_str()]),
                        // Never quietly dropped (rule 3): the body is then not checked to its end.
                        None => crate::not_checked::items::file(made, (0, 0), item_path, "a typed lint's finding names no rule of blaze-lint", d.lint),
                    }
                }
                // What the check could not decide (blaze-typeck rules/unsafety.rs `Undecided`):
                // "not checked" at its place, never a pass (rule 3).
                for u in verdict.undecided.iter() {
                    crate::not_checked::items::file(made, at_entry(u.start, u.end), item_path, u.cause, u.text.as_str());
                }
                if unserved_now {
                    // Said once, after every body (below).
                } else if let Some(failed) = verdict.refused.as_ref().and_then(failed_calls) {
                    // A macro call whose expansion FAILED by its own words (an unset `env!`
                    // variable, `compile_error!`, no rule matched) is Rust's error in the
                    // program, never "not checked" (core's `env!` doc: "a compilation error will
                    // be emitted"). Filed as the failed call an item-level call files
                    // (names/failed_call.rs, rule 22), at the call, with the expansion's words.
                    for (range, path, why) in failed.calls.iter() {
                        // Rule 22's two arguments (blaze-report words.rs): the call, the reason;
                        // once, though two owners of the file (a binary and its test unit) or
                        // two bodies reached both meet it.
                        let call = bstd::mem::format_in!(frame, "{prefix}the call of `{path}!`");
                        covered::find_once(made, crate::names::failed_call::FAILED_CALL, at_entry(range.start, range.end), call.as_str(), "", why);
                    }
                    // What else the lowering did not take: the body is not checked by it.
                    if let Some(other) = failed.other {
                        let cause = bstd::mem::format_in!(frame, "lowering refused ({})", finding_class(other).replace('\x1f', " "));
                        let words = bstd::mem::format_in!(frame, "body `{shown}`: {other:?}");
                        crate::not_checked::items::file(made, (0, 0), item_path, cause.as_str(), words.as_str());
                    }
                } else if let Some(refused) = &verdict.refused {
                    // The words and the cause from their one home (blaze-ladder front2 said.rs),
                    // which the body task carries for `engine-check` too.
                    let words = blaze_ladder::front2::said::refused_words(checker.records, refused);
                    let words = bstd::mem::format_in!(frame, "body `{shown}`: {words}");
                    let cause = blaze_ladder::front2::said::refused_cause(checker.records, &checker.world.front.keys, refused);
                    crate::not_checked::items::file(made, (0, 0), item_path, cause.as_str(), words.as_str());
                } else if let Some(words) = blaze_ladder::front2::said::gap_words(&verdict.gaps) {
                    let words = bstd::mem::format_in!(frame, "body `{shown}`: {words}");
                    crate::not_checked::items::file(made, (0, 0), item_path, blaze_ladder::front2::said::GAP_CAUSE, words.as_str());
                }
                // The move and borrow check's verdict, filed whatever `BORROWS_ARE_ERRORS` says
                // (print.rs decides how it is shown), so the day it flips no body is made again.
                // A body whose types were not checked clean was not borrow checked: its type
                // errors or its `TYPES_NOT_CHECKED` note stand for it, so no note repeats it.
                for d in borrows.findings.iter() {
                    covered::find_once(made, BORROWS, at_entry(d.start, d.end), d.code, prefix.as_str(), d.text);
                }
                if let Some(refused) = &borrows.refused
                    && !matches!(refused, BorrowRefused::Untyped(_))
                {
                    let words = bstd::mem::format_in!(frame, "body `{shown}`: {}: {refused:?}", refused.text());
                    let range = refused.range().map_or((0, 0), |(start, end)| at_entry(start, end));
                    made.find(BORROWS_NOT_CHECKED, range, &[words.as_str()]);
                }
            }
            if unserved_now {
                crate::not_checked::items::file(made, (0, 0), item_path, "a macro call's expansion text is not in its crate's record", "StoreLoad::expansion, names/expansion_texts.rs");
            }
            }
        }
    }
}

impl<'n> Checker<'n> {
    /// Whether the owner's crate holds the entry `item` of `outline` (over `text`): the
    /// attributes of it and of each entry it is written in, read under the crate's cfg as the
    /// names library lowered the crate (blaze-scope `item_attrs`, the one reading of them).
    /// A walk up the parents, bounded by the outline's length.
    fn holds(&self, outline: &blaze_file_items::Outline<'_>, item: &blaze_surface::Item, text: &str) -> Holds {
        let all = outline.items();
        let mut at = Some(item);
        let mut steps = 0usize;
        while let Some(entry) = at {
            let attrs = blaze_scope::task::item_attrs(&self.cfg, outline, entry, text, self.heap);
            if attrs.cfg_unread > 0 {
                return Holds::Unread;
            }
            if !attrs.cfg_ok {
                return Holds::No;
            }
            steps += 1;
            if steps > all.len() {
                return Holds::Unread;
            }
            at = entry.parent.and_then(|p| all.get(p.0 as usize));
        }
        Holds::Yes
    }

    /// covered.rs `written_by` over the owner's crate, once a job.
    fn written_now(&mut self) -> &[covered::WrittenBy<'n>] {
        if self.written.is_none() {
            let made = match self.readers.first() {
                Some(own) => {
                    let listed = self.world.listed(self.heap);
                    covered::written_by(self.records, &own.records.expansions, &listed, self.heap)
                }
                None => AVec::new_in(self.heap),
            };
            self.written = Some(made);
        }
        match &self.written {
            Some(written) => &written[..],
            None => &[],
        }
    }

    /// The `Surface` of each file of the owner's crate that defines a macro the expansions of
    /// `calls` (an entry's, and each one's callers) were made by (covered.rs `rules_files`),
    /// read at the out the body's task was handed, or needed of its file's `FileItems`: an
    /// edit of such a macro's body in ANOTHER file reruns the entry's check, which neither its
    /// key (the call's text) nor its crate's `CrateScope` out (the exports) moves for. Its own
    /// file's `Surface` is every body's read already (work.rs `bodies_syntax`).
    fn rules_reads(&self, calls: &[blaze_id::ItemKey], handed: &[(Name, ContentId)], made: &mut Record<Global>, needs: &mut std::vec::Vec<Want, Global>, item_path: &str, frame: &Frame) {
        let g = self.heap;
        let Some(own) = self.readers.first() else { return };
        let mut called: AVec<'_, &str> = AVec::new_in(g);
        for call in calls.iter().copied() {
            // A call whose chain cannot be walked has no `WrittenBy` at this entry: never here.
            let _ = covered::callers(&own.records.expansions, call, &mut |by| called.push(by));
        }
        let items = match ItemsRecord::open(own.records.items, g) {
            Ok(items) => items,
            Err(e) => {
                let words = bstd::mem::format_in!(frame, "the items record of `{}`: {e:?}", own.crate_name);
                crate::not_checked::items::file(made, (0, 0), item_path, "the files of the macros its expansions used are not known", words.as_str());
                return;
            }
        };
        let mut files: AVec<'_, u32> = AVec::new_in(g);
        covered::rules_files(&items, &called, &mut |file| {
            if file != self.file && !files.contains(&file) {
                files.push(file);
            }
        });
        for file in files.iter().copied() {
            let real = match items.file(file) {
                Ok(Some(row)) => row.path.in_package(own.package, g),
                _ => {
                    let words = bstd::mem::format_in!(frame, "file {file} of the items record of `{}`, which defines a macro its expansions used, is not in the record", own.crate_name);
                    crate::not_checked::items::file(made, (0, 0), item_path, "the files of the macros its expansions used are not known", words.as_str());
                    continue;
                }
            };
            // A file outside the checkout is of a tree its crate's identity covers.
            let Some(real) = real.as_deref().and_then(|p| p.strip_prefix("/0/")) else { continue };
            let home = Name::file(real.as_bytes());
            let surface = Name::new(blaze_drive::key::Kind::Surface, home.subject);
            match handed.binary_search_by_key(&surface, |(name, _)| *name) {
                Ok(at) => made.reads.push(handed[at]),
                Err(_) => needs.push(Want { name: surface, home }),
            }
        }
    }
}

/// One owner's checker, or why none: its records, its world, its file's bodies by chain.
/// `Ok(None)`: the owner's crate does not hold the file ([`OwnerCheck::NotInCrate`]).
fn checker<'n>(cx: &'n Cx, root: &'n Dir<ReadWrite>, job: &'n TypesJob<'n>, owner: &'n Owner<'n>, text: &'n str, frame: &'n Frame, g: Alloc<'static>) -> Result<Option<Checker<'n>>, &'n str> {
    if let Some(missing) = owner.missing {
        return Err(missing);
    }
    let words = |what: core::fmt::Arguments<'_>| held_str(bstd::mem::format_in!(frame, "{what}").as_str(), frame);
    // The names records, made ONE way for `check` and `run` (run_names.rs, rule 30).
    let mut crates: Vec<crate::run_names::Crate<'n>, &'n Frame> = Vec::with_capacity_in(owner.readers.len(), frame);
    for reader in owner.readers.iter() {
        crates.push(crate::run_names::Crate { records: reader.records, name: reader.crate_name, package: reader.package, world_at: reader.world_at });
    }
    let mut read = std::vec::Vec::new_in(Global);
    read.push((job.path, Some(text)));
    // The owner's environment, copied into this worker's frame (`EnvLent`: no allocator of the
    // preparing thread is called here).
    let env = match owner.env {
        None => None,
        Some(lent) => {
            let erased: &'n blaze_own::Erased<&'n Frame> = blaze_own::Box::leak(blaze_own::Box::new_in(blaze_own::Erased::new(frame), frame));
            let env = lent.copy_in(erased.handle()).map_err(|error| words(format_args!("the environment of `{}`: its build-script record did not open again: {error:?}", owner.readers.first().map_or("?", |reader| reader.crate_name))))?;
            Some(&*blaze_own::Box::leak(blaze_own::Box::new_in(env, frame)))
        }
    };
    let opened = crate::run_names::names_records(cx, root, &crates, job.packs, job.corrected, job.procs, read, env, frame, g).map_err(|error| match error {
        crate::run_names::NamesError::Items { at, why } => words(format_args!("the items record of `{}`: {why:?}", owner.readers.get(at).map_or("?", |reader| reader.crate_name))),
    })?;
    let (records, load) = (opened.records, opened.load);
    let world = {
        let _from = cx.span("body types: the world from records");
        ProjectWorld::from_records(records, frame, owner.lang, g).map_err(|e| words(format_args!("the front end over records: {e}")))?
    };
    // On the frame, owned by the checker: dropped (its global-heap maps freed) when the job's
    // checker is, never leaked (`Checker::world`).
    let mut world = blaze_own::Box::new_in(world, frame);
    // A body's macro calls expand with the same texts, environment and proc-macro runner as
    // its crate's item-level expansion (front2 body_host.rs; pm-call-expand), set before any body.
    world.set_body_sources(opened.sources);
    let own = owner.readers.first().ok_or("an owner with no crates")?;
    let items = ItemsRecord::open(own.records.items, g).map_err(|e| words(format_args!("the items record of `{}`: {e:?}", own.crate_name)))?;
    // The items record is the crate under its cfg, the `Owners` record its module tree in text
    // (`OwnerCheck`'s doc): a file it does not list is no file of this owner's crate, and the
    // owner whose crate holds it checks it. Both are inputs of this task, made from one snapshot.
    let Some(file) =
        (0..items.files() as u32).find(|i| matches!(items.file(*i), Ok(Some(row)) if row.path.in_package(own.package, g).as_deref().and_then(|p| p.strip_prefix("/0/")) == Some(job.path)))
    else {
        return Ok(None);
    };
    let cfg = match blaze_ladder::front2::source::NamesSource::cfg_atoms(records, 0) {
        Some(atoms) => blaze_scope::task::Cfg::from_atoms(atoms, g),
        None => return Err(words(format_args!("the cfg atoms of `{}` are not in its records", own.crate_name))),
    };
    let mine: AVec<'n, Listed<'n>> = {
        // An entry's own body is a fn's, a `const`'s or a `static`'s (work.rs `checked_body`).
        let listed = world.listed_with(blaze_ladder::front2::project_records::Listing::FnsAndInitializers, g);
        let mut mine = AVec::with_capacity_in(listed.len(), g);
        mine.extend(listed.iter().copied().filter(|l| l.entry.is_some_and(|(f, _)| f == file)));
        mine
    };
    let by_chain = index_by_chain(records, &mine, &blaze_file_items::item_chain, g).map_err(|e| words(format_args!("the file's bodies by chain: {e}")))?;
    if let Some(error) = records.errors().first() {
        let said = record_words(error, owner.readers, job.packs, frame, g);
        return Err(words(format_args!("the records did not answer: {said}")));
    }
    Ok(Some(Checker { records, load, world, by_chain, file, mine, root_file: owner.root_file, readers: owner.readers, packs: job.packs, heads: heads::Heads::default(), written: None, cfg, heap: g }))
}
