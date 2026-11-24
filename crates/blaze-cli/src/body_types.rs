//! What the engine's body and head stages take from the body check's first home (engine_bodies.rs,
//! engine_heads.rs, run_names.rs): the finding numbers of a body's types and borrows ([`TYPES`],
//! [`TYPES_NOT_CHECKED`], [`BORROWS`], [`BORROWS_NOT_CHECKED`], [`BORROWS_ARE_ERRORS`]); a body
//! part's source ([`body_source`]); the crate list and its trees as a command reads them once
//! ([`Kept`]), or each tree at its first read ([`LazyTrees`]); an owner crate's environment
//! ([`owner_env_by`]); the command's names world ([`names_world`]); and which bodies an entry's
//! check covers (covered.rs).
//!
//! Keys (rule 21): the body's part source is its item's text alone ([`body_source`]); what else
//! its check reads is in its read list.

use blaze_drive::{Input, Name};
use blaze_fetch::PackView;
use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc, Vec};
use blaze_scope::crate_env::EnvLent;
use blaze_world::PackOrigin;
use blaze_world::crate_list::{CrateList, TreeRow};
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadOnly, ReadWrite};
use bstd::mem::Frame;
use bstd::path::Path;
use std::alloc::Global;

use crate::CliError;
use crate::store::Store;
use crate::work::Facts;

/// Which bodies an outline entry's check covers: its root, and each fn declared in a body it
/// reaches (front2 every_body.rs's work list); the one home of it.
pub(crate) mod covered;

/// The names and outs a body's heads are filed under (signature.rs): what separates two equal
/// texts in two crates, and the out of an entry whose macro calls wrote heads.
#[cfg(test)]
mod key_tests;

/// The finding number of a body's type error: its arguments are the checker's code and words.
pub const TYPES: u16 = 17;
/// The finding number of a body whose types were not checked to the end: its one argument says
/// why. It is not an error of the program; it keeps `check` from saying `ok`.
pub const TYPES_NOT_CHECKED: u16 = 18;
/// The finding number of a body's move or borrow finding (blaze-ladder front2/borrows.rs): its
/// arguments are the code (`E0382`, `E0502`, .., or `outlives`) and the words; its range is
/// item-relative. A NOTE while [`BORROWS_ARE_ERRORS`] is false (print.rs).
pub const BORROWS: u16 = 19;
/// The finding number of a body whose moves and borrows were not checked to the end (the flow
/// lowering refused a form, a trait fact was unknown, the checker hit its bound): its one
/// argument says why. Always a note, never a pass: with the flip it keeps the borrows stage
/// "NOT checked".
pub const BORROWS_NOT_CHECKED: u16 = 20;
/// THE FLIP: whether a borrow finding ([`BORROWS`]) is an error of the program, and the
/// borrows stage is counted as run when no body filed [`BORROWS_NOT_CHECKED`].
/// Flipped to `true` under `OneRule` (`blaze-borrow` + `RegionSlots`) after both flow censuses
/// read 0 refused and 0 false findings:
/// - `bstd` (`flow_census`): `clean bodies 4305; borrow-checked whole 4305; refused 0; accepted 4305; findings 0 (in 0 bodies)`
/// - `core` (`flow_census_core`): `clean bodies 2922; borrow-checked whole 2922; refused 0; accepted 2922; findings 0 (in 0 bodies)`
pub const BORROWS_ARE_ERRORS: bool = true;

/// The domain of a body part's source ([`body_source`]). 6: a body stopped by a failed head now
/// places that head's cell to its owning `Signature` task before recording `Done::reads`
/// (engine_bodies.rs, rule 21); a "5" record could keep an unplaced phantom cell and stay stuck
/// on a stale head failure after the head was repaired. (5: the source is the item's text alone,
/// the file around the bodies gone (D114); 4: a body reads its file's `Owners` and only the
/// crates its owners' checks open, no longer every member.)
const SOURCE_DOMAIN: &str = "blaze-cli body check source 6";

/// The body checker's identity is not typed in here (decision D105): it is the table's
/// `Row::BodyChecker` (blaze_ids::producer), and it moves with the checker's code, never by hand.

/// A body part's source: its item's text digest `item` alone. What else its check reads is in
/// its read list: its file's `Surface` and the `Signature` of each head it used (D114 (d);
/// `around_bodies`, the file's text around its bodies, is gone). The checker's own identity is
/// in the tool (`Row::BodyChecker`, D105).
pub(crate) fn body_source(item: ContentId) -> ContentId {
    let mut h = blaze_ids::Hasher::new(SOURCE_DOMAIN);
    h.update(&item.0);
    h.finish()
}

/// The crate list of the command's `UnitGraph` and its trees, each opened once a command
/// (`Global`, named: the command's own lifetime). `blaze run`'s parent reads its crates' records
/// in place (engine_records.rs `EngineRecords`, run_names.rs).
#[derive(Default)]
pub struct Kept {
    /// The crate list of the `UnitGraph` input's out and its crate list (graph_payload.rs), opened (checked
    /// whole) once a command; `None` in the list when the world could not be made.
    list: Option<(ContentId, Option<CrateList<'static>>)>,
    /// Lazy on-demand packs of that list's trees and its corrected files ([`Kept::lazy_trees`]),
    /// for the `UnitGraph` out they were prepared under; or why the table could not be made.
    lazy_trees: Option<(ContentId, Result<LazyTreesHeld, &'static str>)>,
}

/// On-demand pack opener and corrected files for a command's crate list, held on `env.heap`.
pub(crate) type LazyTreesHeld = (&'static SyncLazyTrees<'static>, &'static [(u32, &'static str, &'static str)]);

/// The packs could not be held: the heap refused the table's room.
const TREES_REFUSED: &str = "the heap refused the room for the crate list's trees (body_types.rs `open_trees`)";

impl Kept {
    /// The crate list of the `UnitGraph` input `graph`, read and opened once a command (its
    /// bytes copied out of `scratch` into `Global`, named: the command's own lifetime). `None`:
    /// the graph's world could not be made (its record says why).
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

    /// The on-demand pack opener for `list`'s trees and its corrected files, prepared once a
    /// command on `env.heap` without mapping any pack up front; each pack is mapped and verified
    /// at its first read.
    pub(crate) fn lazy_trees(&mut self, env: &crate::fetch::FetchEnv<'_>, graph: ContentId, list: CrateList<'static>) -> Result<LazyTreesHeld, &'static str> {
        if let Some((out, trees)) = self.lazy_trees
            && out == graph
        {
            return trees;
        }
        let trees = (|| {
            let packs = SyncLazyTrees::new_in_heap(env.home, env.heap, list)?;
            let corrected = corrected_of(&list, env.heap)?;
            Ok((packs, corrected))
        })();
        self.lazy_trees = Some((graph, trees));
        trees
    }
}

/// Thread-safe, lock-free on-demand pack opener: each tree `k` (1-based) is mapped and verified
/// on first read and published via `PublishOnce`.
pub(crate) struct SyncLazyTrees<'f> {
    home: Option<&'f str>,
    heap: &'static blaze_own::Heap,
    list: CrateList<'static>,
    slots: &'f [bstd::sync::PublishOnce<Result<PackView<'static>, &'static str>>],
    refused: bstd::sync::PublishOnce<&'static str>,
}

impl<'f> SyncLazyTrees<'f> {
    pub(crate) fn new(home: Option<&str>, heap: &'static blaze_own::Heap, list: CrateList<'static>, frame: &'f Frame) -> SyncLazyTrees<'f> {
        let mut slots: Vec<bstd::sync::PublishOnce<Result<PackView<'static>, &'static str>>, &'f Frame> = Vec::with_capacity_in(list.trees(), frame);
        slots.extend((0..list.trees()).map(|_| bstd::sync::PublishOnce::new()));
        let home = home.map(|h| crate::work::held_str(h, frame));
        SyncLazyTrees {
            home,
            heap,
            list,
            slots: slots.into_bump_slice(),
            refused: bstd::sync::PublishOnce::new(),
        }
    }

    pub(crate) fn new_in_heap(home: Option<&str>, heap: &'static blaze_own::Heap, list: CrateList<'static>) -> Result<&'static SyncLazyTrees<'static>, &'static str> {
        let g = heap.alloc();
        let mut slots: AVec<'static, bstd::sync::PublishOnce<Result<PackView<'static>, &'static str>>> = AVec::new_in(g);
        if slots.try_reserve_exact(list.trees()).is_err() {
            return Err(TREES_REFUSED);
        }
        slots.extend((0..list.trees()).map(|_| bstd::sync::PublishOnce::new()));
        let home: Option<&'static str> = home.map(|h| &*blaze_own::Box::leak_static(blaze_own::AString::from_str_in(h, g).into_boxed_str()));
        let lazy = SyncLazyTrees {
            home,
            heap,
            list,
            slots: slots.leak_static(),
            refused: bstd::sync::PublishOnce::new(),
        };
        Ok(blaze_own::Box::leak_static(blaze_own::Box::new_in(lazy, g)))
    }

    pub(crate) fn trees_len(&self) -> usize {
        self.slots.len()
    }

    pub(crate) fn open_tree_with(
        &self,
        cx: &Cx,
        fs: &bstd::fs::Fs,
        dirs: Option<&SyncPackDirs>,
        scratch: &Frame,
        k: usize,
    ) -> Option<(PackView<'static>, bool)> {
        let slot = self.slots.get(k.checked_sub(1)?)?;
        let (got, newly_opened) = match slot.get() {
            Some(got) => (*got, false),
            None => {
                let from = PackHome { home: self.home, fs, heap: self.heap };
                let opened = match dirs {
                    Some(shared) => open_tree_shared(cx, from, &self.list, k, shared, scratch).map_err(|r| r.0),
                    None => {
                        let mut local = PackDirs::default();
                        open_tree(cx, from, &self.list, k, &mut local, scratch)
                    }
                };
                let newly_opened = slot.set(opened).is_ok();
                (*slot.get()?, newly_opened)
            }
        };
        match got {
            Ok(view) => Some((view, newly_opened)),
            Err(why) => {
                let _ = self.refused.set(why);
                None
            }
        }
    }

    pub(crate) fn tree(&self, cx: &Cx, fs: &bstd::fs::Fs, scratch: &Frame, k: usize) -> Option<PackView<'static>> {
        let (view, _) = self.open_tree_with(cx, fs, None, scratch, k)?;
        Some(view)
    }

    pub(crate) fn refused(&self) -> Option<&'static str> {
        self.refused.get().copied()
    }

    #[cfg(test)]
    pub(crate) fn opened(&self) -> usize {
        self.slots.iter().filter(|slot| slot.get().is_some()).count()
    }
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
    Ok(corrected.leak_static())
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

/// Thread-safe, command-scoped cache of the two read-only pack folders (`shelf` and `sources`),
/// each opened at most once via `Fs::home` and closed when this value drops. Never bump-allocate
/// or leak this: each opened `Dir<ReadOnly>` owns a secondary driver (`Io::beside`) with its own
/// file worker thread and descriptor table.
#[derive(Default)]
pub(crate) struct SyncPackDirs {
    shelf: bstd::sync::PublishOnce<Dir<ReadOnly>>,
    sources: bstd::sync::PublishOnce<Dir<ReadOnly>>,
}

impl SyncPackDirs {
    #[cfg(test)]
    pub(crate) fn opened_count(&self) -> usize {
        usize::from(self.shelf.get().is_some()) + usize::from(self.sources.get().is_some())
    }
}

/// Tree `k` (1-based) of `list` opened from where the table names it ([`open_trees`] says how
/// and what is refused): THE ONE OPENER of a tree, for both schedules (every tree at once,
/// [`open_trees`]; each on its first read, [`LazyTrees`]). Its folder is opened in `dirs`
/// the first time a tree of it is asked. A refusal's words are kept on `from.heap`.
fn open_tree(cx: &Cx, from: PackHome<'_>, list: &CrateList<'static>, k: usize, dirs: &mut PackDirs, scratch: &Frame) -> Result<PackView<'static>, &'static str> {
    let heap = from.heap;
    let kept = |words: &str| -> &'static str { blaze_own::Box::leak_static(blaze_own::AString::from_str_in(words, heap.alloc()).into_boxed_str()) };
    let Some(row) = list.tree(k) else {
        return Err(kept(bstd::mem::format_in!(scratch, "the crate list names no tree {k} of its {} (blaze-world crate_list.rs)", list.trees()).as_str()));
    };
    open_row(cx, from, row, k, dirs, scratch)
}

/// Typed refusal from opening or verifying a single pack file.
#[derive(Clone, Copy, Debug)]
struct PackRefusal(&'static str);

fn open_tree_shared(cx: &Cx, from: PackHome<'_>, list: &CrateList<'static>, k: usize, dirs: &SyncPackDirs, scratch: &Frame) -> Result<PackView<'static>, PackRefusal> {
    let heap = from.heap;
    let kept = |words: &str| -> PackRefusal { PackRefusal(blaze_own::Box::leak_static(blaze_own::AString::from_str_in(words, heap.alloc()).into_boxed_str())) };
    let Some(row) = list.tree(k) else {
        return Err(kept(bstd::mem::format_in!(scratch, "the crate list names no tree {k} of its {} (blaze-world crate_list.rs)", list.trees()).as_str()));
    };
    open_row_shared(cx, from, row, k, dirs, scratch)
}

fn map_pack_in_dir(
    cx: &Cx,
    dir: &Dir<ReadOnly>,
    row: TreeRow,
    hex: &str,
    at: &str,
    heap: &'static blaze_own::Heap,
    scratch: &Frame,
) -> Result<PackView<'static>, PackRefusal> {
    let kept = |words: &str| -> PackRefusal { PackRefusal(blaze_own::Box::leak_static(blaze_own::AString::from_str_in(words, heap.alloc()).into_boxed_str())) };
    let file = Path::new(hex).map_err(|e| kept(bstd::mem::format_in!(scratch, "{at}: {e}").as_str()))?;
    let mapping = dir.map_read(cx, &file).map_err(|e| kept(bstd::mem::format_in!(scratch, "{at} could not be mapped: {e}").as_str()))?;
    // Kept for the command: every body reads the pack's files in place.
    let mapping: &'static blaze_core::Mapping = blaze_own::Box::leak_static(blaze_own::Box::new_in(mapping, heap.alloc()));
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

fn open_home_pack_dir(cx: &Cx, from: PackHome<'_>, folder: &str, at: &str, scratch: &Frame) -> Result<Dir<ReadOnly>, PackRefusal> {
    let heap = from.heap;
    let kept = |words: &str| -> PackRefusal { PackRefusal(blaze_own::Box::leak_static(blaze_own::AString::from_str_in(words, heap.alloc()).into_boxed_str())) };
    let Some(home) = from.home else {
        return Err(kept(bstd::mem::format_in!(scratch, "{at}: HOME is not set, and the pack is beneath it").as_str()));
    };
    let path = Path::new(folder).map_err(|e| kept(bstd::mem::format_in!(scratch, "{at}: {e}").as_str()))?;
    let opened = from.fs.home(cx, home, &path).map_err(|e| kept(bstd::mem::format_in!(scratch, "{at}: {e}").as_str()))?;
    Ok(opened.read_only())
}

fn open_row_shared(cx: &Cx, from: PackHome<'_>, row: TreeRow, k: usize, dirs: &SyncPackDirs, scratch: &Frame) -> Result<PackView<'static>, PackRefusal> {
    let (folder, name, slot) = match row.origin {
        PackOrigin::Shelf { digest } => (crate::toolchain::SHELF, digest, &dirs.shelf),
        PackOrigin::Source { key } => (crate::fetch::SOURCES, key.0, &dirs.sources),
    };
    let hex = crate::fetch::key_hex(&ContentId(name));
    let hex = core::str::from_utf8(&hex).map_err(|_| PackRefusal("a pack's name is not hex (fetch.rs `key_hex`)"))?;
    let at = bstd::mem::format_in!(scratch, "tree {k} of the crate list, `<home>/{folder}/{hex}`");
    let dir = match slot.get() {
        Some(dir) => dir,
        None => {
            let opened = open_home_pack_dir(cx, from, folder, at.as_str(), scratch)?;
            // If another thread raced and published first, `opened` drops here and closes its driver.
            let _ = slot.set(opened);
            let Some(dir) = slot.get() else {
                let kept = |words: &str| -> PackRefusal { PackRefusal(blaze_own::Box::leak_static(blaze_own::AString::from_str_in(words, from.heap.alloc()).into_boxed_str())) };
                return Err(kept(bstd::mem::format_in!(scratch, "{at}: its folder was not opened").as_str()));
            };
            dir
        }
    };
    map_pack_in_dir(cx, dir, row, hex, at.as_str(), from.heap, scratch)
}

/// Tree `k`'s pack as its row `row` names it, opened and checked as [`open_trees`] says: THE
/// ONE OPENER of a pack, for a list of any lifetime (native_runner_c.rs lays a fetched
/// package's files from it). Its folder is opened in `dirs` the first time a pack of it is
/// asked; a refusal's words are kept on `from.heap`.
pub(crate) fn open_row(cx: &Cx, from: PackHome<'_>, row: TreeRow, k: usize, dirs: &mut PackDirs, scratch: &Frame) -> Result<PackView<'static>, &'static str> {
    let heap = from.heap;
    let kept = |words: &str| -> &'static str { blaze_own::Box::leak_static(blaze_own::AString::from_str_in(words, heap.alloc()).into_boxed_str()) };
    let (folder, name, dir) = match row.origin {
        PackOrigin::Shelf { digest } => (crate::toolchain::SHELF, digest, &mut dirs.shelf),
        PackOrigin::Source { key } => (crate::fetch::SOURCES, key.0, &mut dirs.sources),
    };
    let hex = crate::fetch::key_hex(&ContentId(name));
    let hex = core::str::from_utf8(&hex).map_err(|_| "a pack's name is not hex (fetch.rs `key_hex`)")?;
    let at = bstd::mem::format_in!(scratch, "tree {k} of the crate list, `<home>/{folder}/{hex}`");
    if dir.is_none() {
        *dir = Some(open_home_pack_dir(cx, from, folder, at.as_str(), scratch).map_err(|r| r.0)?);
    }
    let Some(dir) = dir.as_ref() else {
        return Err(kept(bstd::mem::format_in!(scratch, "{at}: its folder was not opened").as_str()));
    };
    map_pack_in_dir(cx, dir, row, hex, at.as_str(), heap, scratch).map_err(|r| r.0)
}

/// Command-wide lazy pack view with per-session open/refusal accounting ([`TreesSeen`]).
/// Holds only shared references (no `Drop` resources), so bump-allocating it per suite never
/// leaks secondary drivers or file descriptors while sharing one [`SyncLazyTrees`] mapping cache
/// and one command-scoped [`SyncPackDirs`] across all suites and code workers.
#[derive(Clone, Copy)]
pub(crate) struct SharedTrees<'n> {
    packs: &'static SyncLazyTrees<'static>,
    fs: &'n bstd::fs::Fs,
    dirs: &'n SyncPackDirs,
    seen: &'n TreesSeen,
}

impl<'n> SharedTrees<'n> {
    pub(crate) fn new(
        packs: &'static SyncLazyTrees<'static>,
        fs: &'n bstd::fs::Fs,
        dirs: &'n SyncPackDirs,
        seen: &'n TreesSeen,
    ) -> SharedTrees<'n> {
        seen.trees.set(u32::try_from(packs.trees_len()).unwrap_or(u32::MAX));
        SharedTrees { packs, fs, dirs, seen }
    }

    pub(crate) fn tree(&self, cx: &Cx, scratch: &Frame, k: usize) -> Option<PackView<'static>> {
        match self.packs.open_tree_with(cx, self.fs, Some(self.dirs), scratch, k) {
            Some((view, newly_opened)) => {
                if newly_opened {
                    self.seen.opened.set(self.seen.opened.get() + 1);
                }
                Some(view)
            }
            None => {
                if let Some(why) = self.packs.refused()
                    && self.seen.refused.get().is_none()
                {
                    self.seen.refused.set(Some(why));
                }
                None
            }
        }
    }

    pub(crate) fn refused(&self) -> Option<&'static str> {
        self.seen.refused().or_else(|| self.packs.refused())
    }
}

/// What opens tree `k` of a crate list at its first read ([`LazyTrees`]).
pub(crate) trait OpenTree {
    fn open(&self, k: usize) -> Result<PackView<'static>, &'static str>;
}

/// A slot of [`LazyTrees`]: empty until its tree's first read, then the pack or why not.
pub(crate) type TreeSlot = core::cell::OnceCell<Result<PackView<'static>, &'static str>>;

/// A crate list's trees, each opened at the first read of a text from it and at most once
/// (`blaze run`'s parent, run_names.rs `open`): a run after an edit of one body maps the packs of
/// the crates its walk reads, not of the whole closure. Handed out by tree index as the opened
/// slice is. READS (the doc of [`Kept::trees`]): the read is DECLARED UP FRONT, never at the
/// open: the packs are the list's, so a reader's `(UnitGraph, out)` read is the read of every
/// pack it may open, whichever it opens. A tree that could not be opened is kept as its refusal:
/// [`TreesSeen::refused`] names it and the run stops on it.
pub(crate) struct LazyTrees<'n> {
    opener: &'n dyn OpenTree,
    /// One a tree of the list: its cap is the list's tree count, made at the start.
    slots: &'n [TreeSlot],
    /// What was opened and refused, kept by the caller past the names records' life.
    seen: &'n TreesSeen,
}

/// What a [`LazyTrees`] opened, of how many, and the first tree it could not open: the run's
/// closing line (run_names.rs `NamesRead`) and its stop.
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
        LazyTrees { opener, slots: slots.into_bump_slice(), seen }
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

/// The `Script` part's name of the crate at list row `at`, when it has a build script whose run
/// is a task; refused by name when the row names a kind of no task.
pub(crate) fn owner_script(list: &CrateList<'_>, at: usize) -> Result<Option<Name>, &'static str> {
    match list.script(at) {
        None => Ok(None),
        Some(bytes) => crate::native_image::script_name(bytes).map(Some).ok_or("a crate list row names a build-script run of no kind (crate_list.rs `script`)"),
    }
}

/// The environment of the owner crate at list row `at`: `CrateEnv::of` of the list's facts of
/// it (the ones the world made the crate's item-level environment from) and, when it has a
/// build script, the outputs record of its run (found by `handed`: the engine's Scripts stage,
/// engine_bodies.rs), whose `OUT_DIR` files are read from the home's file area as the names task
/// reads them. Made in `jobs`. Refused by name: a script record that is not an outputs record, a
/// version that is not SemVer.
pub(crate) fn owner_env_by<'j>(
    cx: &Cx,
    store: &mut dyn Store<'_>,
    handed: &dyn Fn(Name) -> Option<Input>,
    list: &CrateList<'static>,
    at: usize,
    home: Option<&str>,
    jobs: &'j Frame,
) -> Result<Result<OwnerEnv<'j>, &'static str>, CliError> {
    let script = match owner_script(list, at) {
        Ok(script) => script,
        Err(refused) => return Ok(Err(refused)),
    };
    let mut read = None;
    let payload: Option<&'j [u8]> = match script {
        None => None,
        Some(script) => {
            let Some(handed) = handed(script) else {
                return Ok(Err("a body's owner's build-script run, asked for, is not among its inputs"));
            };
            let payload = crate::work::payload(cx, store, &handed, jobs)?;
            read = Some((handed.name, handed.out));
            let bytes: &'j [u8] = jobs.alloc_slice_copy(payload.payload());
            Some(bytes)
        }
    };
    match row_env(list, at, payload, home, jobs) {
        Ok(env) => Ok(Ok((Some(EnvLent::new(env)), read))),
        Err(refused) => Ok(Err(refused)),
    }
}

/// THE ONE CONSTRUCTION of the environment of the crate at list row `at` a body's macro calls
/// read (rule 30): `blaze check`'s body owner ([`owner_env_by`]) and every crate of a run whose
/// body is lowered by need (run_names.rs `RunEnvs`) are made here, so a body sees one
/// environment whichever path lowers it. `CrateEnv::of` of the list's facts of the crate (the
/// ones the world made its item-level environment from) and, when it has a build script,
/// `script`: the payload of its run's `Script` record, whose `rustc-env` pairs and `OUT_DIR`
/// files join (the files read from the home's file area as the names task reads them). Made in
/// `jobs`. Refused by name: a script record that is not an outputs record, a version that is
/// not SemVer.
pub(crate) fn row_env<'j>(list: &CrateList<'static>, at: usize, script: Option<&'j [u8]>, home: Option<&str>, jobs: &'j Frame) -> Result<&'j blaze_scope::crate_env::CrateEnv<'j>, &'static str> {
    let erased: &'j blaze_own::Erased<&'j Frame> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(blaze_own::Erased::new(jobs), jobs));
    let alloc = erased.handle();
    let mut facts: blaze_scope::crate_env::EnvFacts<'j> = list.env_facts(at);
    if let Some(bytes) = script {
        let Some((_, record)) = crate::scripts::payload_parts(bytes) else {
            return Err("a crate's build-script record is not a `Script` payload (scripts.rs `payload_parts`)");
        };
        let Ok(outputs) = blaze_build_script::outputs::OutputsRef::open(record) else {
            return Err("a crate's build-script record is not an outputs record (blaze-build-script `OutputsRef::open`)");
        };
        let area: &'j blaze_world::StoredFiles = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(crate::scripts::file_area(home, &outputs), jobs));
        facts.script = Some(outputs.with_area(area));
    }
    let Ok(env) = blaze_scope::crate_env::CrateEnv::of(facts, alloc) else {
        return Err("a crate's package version is not MAJOR.MINOR.PATCH (the cargo book, \"The `version` field\")");
    };
    Ok(blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(env, jobs)))
}

/// An owner's lent environment and the script read it was made with ([`owner_env_by`]).
pub(crate) type OwnerEnv<'j> = (Option<EnvLent<'j, 'j>>, Option<(Name, ContentId)>);

/// The command's names world (`facts.names`) over the unit graph `graph`, made once a command
/// when none is made yet: for a reader that needs the world itself (run_names.rs). Inner `Err`:
/// the world could not be made (the `CrateScope` tasks say why).
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

#[cfg(test)]
mod shared_pack_tests {
    use super::*;
    use blaze_fetch::Mode;
    use blaze_fetch::pack::{Builder, Kind as PackKind, Limits as PackLimits};
    use bstd::fs::{Content, Durable};
    use bstd::io::{Io, Limits, SimWorld};

    /// Regression for multi-suite `blaze test` FD/driver exhaustion: opening the same pack
    /// across many suites through one command-scoped `SyncPackDirs` opens the home folder at
    /// most once (not once per suite) and cleanly drops it at scope exit.
    #[test]
    fn shared_pack_dirs_opens_each_home_folder_at_most_once() {
        let io = Io::simulated(SimWorld::new(91, Limits::USUAL));
        let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("root cx");
        let fs = io.need_fs().expect("fs");
        let frame = Frame::new(bstd::mem::Tag::Other).expect("frame");
        let heap = blaze_own::testing::heap();

        let limits = PackLimits { most_files: 8, most_bytes: 1 << 16, most_path: 128 };
        let mut builder = Builder::new(PackKind::Crate, [0x42; 32], limits, 0, heap.alloc()).expect("builder");
        builder.file("src/lib.rs", Mode::File, b"pub fn ok() {}\n").expect("file");
        let pack_bytes = builder.finish().expect("pack bytes");
        let verified = PackView::open_verified(&pack_bytes).expect("valid pack");
        let key = ContentId::of("shared_pack_dirs test", &pack_bytes);
        let hex_bytes = crate::fetch::key_hex(&key);
        let hex = core::str::from_utf8(&hex_bytes).expect("hex");

        let home = "/h";
        let sources_dir = fs.home(&cx, home, &Path::new(crate::fetch::SOURCES).expect("sources path")).expect("sources home");
        sources_dir.publish(&cx, &Path::new(hex).expect("hex path"), Content::bytes(&pack_bytes), Durable::No).expect("publish pack");
        drop(sources_dir);

        let dirs = SyncPackDirs::default();
        assert_eq!(dirs.opened_count(), 0);
        let from = PackHome { home: Some(home), fs: &fs, heap };
        let row = TreeRow { origin: PackOrigin::Source { key }, kind: PackKind::Crate, files: verified.tree().0 };

        for _ in 0..64 {
            let view = open_row_shared(&cx, from, row, 1, &dirs, &frame).unwrap_or_else(|e| panic!("open pack: {}", e.0));
            let file = view.find("src/lib.rs").expect("src/lib.rs");
            assert_eq!(file.bytes, b"pub fn ok() {}\n");
        }
        assert_eq!(dirs.opened_count(), 1, "64 suite opens must share one sources Dir, never open 64 drivers");
    }
}
