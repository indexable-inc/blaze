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
use bstd::sync::Once;
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

    /// Seeds `Kept` with an existing command-scoped [`SyncLazyTrees`] (such as the one
    /// `NamesWorld` created during the `Names` stage) so `Heads` and `Bodies` reuse its
    /// already-mapped/decompressed packs and cached checkout files instead of duplicating work.
    pub(crate) fn share_lazy_trees(
        &mut self,
        graph: ContentId,
        packs: &'static SyncLazyTrees<'static>,
        list: &CrateList<'static>,
        heap: &'static blaze_own::Heap,
    ) {
        if packs.trees_len() != list.trees() {
            return;
        }
        if let Some((out, Ok(_))) = self.lazy_trees
            && out == graph
        {
            return;
        }
        if let Ok(corrected) = corrected_of(list, heap) {
            self.lazy_trees = Some((graph, Ok((packs, corrected))));
        }
    }
}

/// Number of hash buckets in [`CheckoutCache`]: power of two, lazily allocated on the first
/// checkout read. Unbounded via per-bucket chains, keeping chains short even on large monorepos.
const CHECKOUT_BUCKETS: usize = 2048;

/// Deterministic hash of a checkout-relative path for [`CheckoutCache`] (rule 25, rule 75).
fn checkout_hash(inner: &str) -> u64 {
    core::hash::BuildHasher::hash_one(&blaze_hash::FixedState, inner)
}

/// Result of reading one checkout file (`/0/<inner>`) once per [`SyncLazyTrees`] lifetime
/// (rule 74: one `read_all`, one UTF-8 check, and one `FILE_DOMAIN` BLAKE3 digest).
pub(crate) enum CachedCheckout {
    /// Valid UTF-8 text and its precomputed `crate::sweep::FILE_DOMAIN` digest.
    Utf8 {
        text: blaze_own::String<blaze_own::SharedAlloc<'static>>,
        digest: ContentId,
    },
    /// Readable non-UTF-8 bytes (served to `read_data` / `include_bytes!`, and reported as
    /// `Found::Unknown` with the exact `Utf8Error` to `source`).
    NonUtf8 {
        bytes: Vec<u8, blaze_own::SharedAlloc<'static>>,
        utf8_error: core::str::Utf8Error,
    },
    /// Invalid path, `NotFound`, `IsADirectory`, or `NotADirectory`.
    Absent,
    /// Unreadable due to a filesystem error (permission denied, I/O error, etc.).
    FsError(bstd::fs::FsError),
}

impl CachedCheckout {
    fn load(cx: &Cx, root: &Dir<ReadWrite>, inner: &str, alloc: blaze_own::SharedAlloc<'static>) -> CachedCheckout {
        if inner.split('/').any(|part| part == ".." || part == ".") {
            return CachedCheckout::Absent;
        }
        let Ok(at) = Path::new(inner) else {
            return CachedCheckout::Absent;
        };
        let mut raw = bstd::mem::Bytes::new_in(cx.frame(), "checkout file cache");
        if let Err(error) = root.read_all(cx, &at, &mut raw) {
            return match error.kind {
                bstd::fs::FsKind::NotFound | bstd::fs::FsKind::IsADirectory | bstd::fs::FsKind::NotADirectory => CachedCheckout::Absent,
                _ => CachedCheckout::FsError(error),
            };
        }
        let slice = raw.as_slice();
        let entry = match core::str::from_utf8(slice) {
            Ok(text_str) => {
                let digest = ContentId::of(crate::sweep::FILE_DOMAIN, slice);
                let text = blaze_own::String::from_str_in(text_str, alloc);
                CachedCheckout::Utf8 { text, digest }
            }
            Err(utf8_error) => {
                let mut bytes = Vec::with_capacity_in(slice.len(), alloc);
                bytes.extend_from_slice(slice);
                CachedCheckout::NonUtf8 { bytes, utf8_error }
            }
        };
        drop(raw);
        entry
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            CachedCheckout::Utf8 { text, .. } => Some(text.as_str()),
            CachedCheckout::NonUtf8 { .. } | CachedCheckout::Absent | CachedCheckout::FsError(_) => None,
        }
    }

    pub(crate) fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            CachedCheckout::Utf8 { text, .. } => Some(text.as_bytes()),
            CachedCheckout::NonUtf8 { bytes, .. } => Some(bytes.as_slice()),
            CachedCheckout::Absent | CachedCheckout::FsError(_) => None,
        }
    }

    pub(crate) fn digest(&self) -> Option<ContentId> {
        match self {
            CachedCheckout::Utf8 { digest, .. } => Some(*digest),
            CachedCheckout::NonUtf8 { .. } | CachedCheckout::Absent | CachedCheckout::FsError(_) => None,
        }
    }

    pub(crate) fn as_source<'a>(&'a self, path: &'a str) -> blaze_scope::resolve::Found<&'a str, blaze_scope::resolve::Unread<'a>> {
        match self {
            CachedCheckout::Utf8 { text, .. } => blaze_scope::resolve::Found::Known(text.as_str()),
            CachedCheckout::NonUtf8 { utf8_error, .. } => {
                blaze_scope::resolve::Found::Unknown(blaze_scope::resolve::Unread { path, why: utf8_error })
            }
            CachedCheckout::Absent => blaze_scope::resolve::Found::Absent,
            CachedCheckout::FsError(why) => {
                blaze_scope::resolve::Found::Unknown(blaze_scope::resolve::Unread { path, why })
            }
        }
    }
}

type CheckoutBucketSlot = Once<blaze_own::Box<CheckoutNode, blaze_own::SharedAlloc<'static>>>;

struct CheckoutNode {
    hash: u64,
    path: blaze_own::String<blaze_own::SharedAlloc<'static>>,
    value: Once<CachedCheckout>,
    next: CheckoutBucketSlot,
}

/// Command-scoped, thread-safe, lock-free-on-read cache of checkout files (`/0/<inner>`).
/// Allocated lazily on `SharedAlloc<'static>` and freed iteratively when [`SyncLazyTrees`] drops.
struct CheckoutCache {
    buckets: Once<Vec<CheckoutBucketSlot, blaze_own::SharedAlloc<'static>>>,
}

impl CheckoutCache {
    fn new() -> CheckoutCache {
        CheckoutCache { buckets: Once::new("checkout cache") }
    }

    fn bucket_index(hash: u64) -> usize {
        const CHECKOUT_BUCKET_MASK: u64 = 2047;
        const _: () = assert!((CHECKOUT_BUCKETS - 1) == 2047);
        usize::try_from(hash & CHECKOUT_BUCKET_MASK).unwrap_or(0)
    }

    fn get_node(&self, inner: &str) -> Option<&CheckoutNode> {
        let table = self.buckets.get()?;
        let hash = checkout_hash(inner);
        let mut slot = table.get(Self::bucket_index(hash))?;
        while let Some(node) = slot.get() {
            if node.hash == hash && node.path.as_str() == inner {
                return Some(node);
            }
            slot = &node.next;
        }
        None
    }

    fn get_digest(&self, inner: &str) -> Option<ContentId> {
        self.get_node(inner)?.value.get()?.digest()
    }

    fn cached_path(&self, inner: &str) -> Option<&str> {
        self.get_node(inner).map(|node| node.path.as_str())
    }

    fn get_or_load(
        &self,
        cx: &Cx,
        root: &Dir<ReadWrite>,
        inner: &str,
        alloc: blaze_own::SharedAlloc<'static>,
    ) -> Result<(&str, &CachedCheckout), bstd::sync::OnceError<core::convert::Infallible>> {
        let table = self.buckets.get_or_make(cx, |_| {
            let mut v = Vec::with_capacity_in(CHECKOUT_BUCKETS, alloc);
            v.resize_with(CHECKOUT_BUCKETS, || Once::new("checkout cache bucket"));
            Ok::<_, core::convert::Infallible>(v)
        })?;
        let hash = checkout_hash(inner);
        let Some(mut slot) = table.get(Self::bucket_index(hash)) else {
            unreachable!("checkout bucket table always has CHECKOUT_BUCKETS entries");
        };
        loop {
            let node = match slot.get() {
                Some(node) => node,
                None => slot.get_or_make(cx, |_| {
                    Ok::<_, core::convert::Infallible>(blaze_own::Box::new_in(
                        CheckoutNode {
                            hash,
                            path: blaze_own::String::from_str_in(inner, alloc),
                            value: Once::new("checkout file"),
                            next: Once::new("checkout cache bucket"),
                        },
                        alloc,
                    ))
                })?,
            };
            if node.hash == hash && node.path.as_str() == inner {
                let entry = node.value.get_or_make(cx, |cx| {
                    Ok::<_, core::convert::Infallible>(CachedCheckout::load(cx, root, inner, alloc))
                })?;
                return Ok((node.path.as_str(), entry));
            }
            slot = &node.next;
        }
    }
}

impl Drop for CheckoutCache {
    fn drop(&mut self) {
        let buckets = core::mem::replace(&mut self.buckets, Once::new("checkout cache"));
        let Some(mut table) = buckets.into_inner() else {
            return;
        };
        for bucket in table.drain(..) {
            let mut cur = bucket.into_inner();
            while let Some(node) = cur {
                let CheckoutNode { next, .. } = blaze_own::Box::into_inner(node);
                cur = next.into_inner();
            }
        }
    }
}

/// One pack entry materialized once on demand across all workers and stretches (rule 74:
/// decompress and check UTF-8 at most once per unique file in a pack).
enum CachedPackEntry {
    BorrowedStr(&'static str),
    BorrowedBytes(&'static [u8]),
    OwnedStr(blaze_own::String<blaze_own::SharedAlloc<'static>>),
    OwnedBytes(Vec<u8, blaze_own::SharedAlloc<'static>>),
    Failed,
}

impl CachedPackEntry {
    fn of(entry: blaze_fetch::pack::FileEntry<'static>, alloc: blaze_own::SharedAlloc<'static>) -> CachedPackEntry {
        match entry.read_in(alloc) {
            Ok(blaze_fetch::pack::FileBytes::Borrowed(bytes)) => match core::str::from_utf8(bytes) {
                Ok(text) => CachedPackEntry::BorrowedStr(text),
                Err(_) => CachedPackEntry::BorrowedBytes(bytes),
            },
            Ok(blaze_fetch::pack::FileBytes::Owned(bytes)) => match blaze_own::String::from_utf8(bytes) {
                Ok(text) => CachedPackEntry::OwnedStr(text),
                Err(error) => CachedPackEntry::OwnedBytes(error.into_bytes()),
            },
            Err(_) => CachedPackEntry::Failed,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            CachedPackEntry::BorrowedStr(s) => Some(s),
            CachedPackEntry::OwnedStr(s) => Some(s.as_str()),
            CachedPackEntry::BorrowedBytes(_) | CachedPackEntry::OwnedBytes(_) | CachedPackEntry::Failed => None,
        }
    }

    fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            CachedPackEntry::BorrowedStr(s) => Some(s.as_bytes()),
            CachedPackEntry::BorrowedBytes(b) => Some(b),
            CachedPackEntry::OwnedStr(s) => Some(s.as_bytes()),
            CachedPackEntry::OwnedBytes(b) => Some(b.as_slice()),
            CachedPackEntry::Failed => None,
        }
    }
}

/// One opened pack and its per-entry lazy cache of decompressed file bytes/UTF-8 text.
struct CachedPack {
    view: PackView<'static>,
    entries: Vec<Once<CachedPackEntry>, blaze_own::SharedAlloc<'static>>,
}

impl CachedPack {
    fn new(view: PackView<'static>, alloc: blaze_own::SharedAlloc<'static>) -> CachedPack {
        let mut entries = Vec::with_capacity_in(view.len(), alloc);
        entries.resize_with(view.len(), || Once::new("pack file entry"));
        CachedPack { view, entries }
    }

    fn get_or_load(
        &self,
        cx: &Cx,
        path: &str,
        alloc: blaze_own::SharedAlloc<'static>,
    ) -> Result<Option<&CachedPackEntry>, blaze_fetch::PackRefused> {
        let Some(at) = self.view.find_index(path) else {
            if let Some(sub) = self.view.submodule_covering(path) {
                let commit = sub.submodule_commit().unwrap_or([0u8; 20]);
                return Err(blaze_fetch::PackRefused::Submodule {
                    path: blaze_fetch::unpack::Cut::of(path.as_bytes()),
                    submodule: blaze_fetch::unpack::Cut::of(sub.path.as_bytes()),
                    commit,
                });
            }
            return Ok(None);
        };
        let entry = self.view.entry(at);
        if entry.mode == blaze_fetch::Mode::Submodule {
            let commit = entry.submodule_commit().unwrap_or([0u8; 20]);
            return Err(blaze_fetch::PackRefused::Submodule {
                path: blaze_fetch::unpack::Cut::of(path.as_bytes()),
                submodule: blaze_fetch::unpack::Cut::of(entry.path.as_bytes()),
                commit,
            });
        }
        let Some(slot) = self.entries.get(at) else {
            return Ok(None);
        };
        Ok(slot.get_or_make(cx, |_| Ok::<_, core::convert::Infallible>(CachedPackEntry::of(entry, alloc))).ok())
    }
}

/// Thread-safe, on-demand pack opener, per-entry decompressed pack file cache, and command-scoped
/// checkout file cache: each tree `k` (1-based) is mapped and verified once, each compressed pack
/// file is decompressed and UTF-8 checked at most once, and each checkout file (`/0/<inner>`) is
/// read from disk, UTF-8 checked, and BLAKE3-hashed (`FILE_DOMAIN`) at most once across all
/// workers, jobs, and stretches.
pub(crate) struct SyncLazyTrees<'f> {
    home: Option<&'f str>,
    heap: &'static blaze_own::Heap,
    list: CrateList<'static>,
    slots: Vec<Once<Result<CachedPack, &'static str>>, blaze_own::SharedAlloc<'static>>,
    refused: bstd::sync::PublishOnce<&'static str>,
    checkout: CheckoutCache,
}

impl<'f> SyncLazyTrees<'f> {
    pub(crate) fn new(home: Option<&str>, heap: &'static blaze_own::Heap, list: CrateList<'static>, frame: &'f Frame) -> SyncLazyTrees<'f> {
        let mut slots: Vec<Once<Result<CachedPack, &'static str>>, blaze_own::SharedAlloc<'static>> = Vec::with_capacity_in(list.trees(), heap.shared());
        slots.resize_with(list.trees(), || Once::new("source pack"));
        let home = home.map(|h| crate::work::held_str(h, frame));
        SyncLazyTrees {
            home,
            heap,
            list,
            slots,
            refused: bstd::sync::PublishOnce::new(),
            checkout: CheckoutCache::new(),
        }
    }

    pub(crate) fn new_in_heap(home: Option<&str>, heap: &'static blaze_own::Heap, list: CrateList<'static>) -> Result<&'static SyncLazyTrees<'static>, &'static str> {
        let g = heap.alloc();
        let mut slots: Vec<Once<Result<CachedPack, &'static str>>, blaze_own::SharedAlloc<'static>> = Vec::new_in(heap.shared());
        if slots.try_reserve_exact(list.trees()).is_err() {
            return Err(TREES_REFUSED);
        }
        slots.resize_with(list.trees(), || Once::new("source pack"));
        let home: Option<&'static str> = home.map(|h| &*blaze_own::Box::leak_static(blaze_own::AString::from_str_in(h, g).into_boxed_str()));
        let lazy = SyncLazyTrees {
            home,
            heap,
            list,
            slots,
            refused: bstd::sync::PublishOnce::new(),
            checkout: CheckoutCache::new(),
        };
        Ok(blaze_own::Box::leak_static(blaze_own::Box::new_in(lazy, g)))
    }

    pub(crate) fn trees_len(&self) -> usize {
        self.slots.len()
    }

    fn open_cached(
        &self,
        cx: &Cx,
        fs: &bstd::fs::Fs,
        dirs: Option<&SyncPackDirs>,
        scratch: &Frame,
        k: usize,
    ) -> Option<(&CachedPack, bool)> {
        let slot = self.slots.get(k.checked_sub(1)?)?;
        let mut newly_opened = false;
        let got = slot.get_or_make(cx, |cx| {
            newly_opened = true;
            let from = PackHome { home: self.home, fs, heap: self.heap };
            let opened = match dirs {
                Some(shared) => open_tree_shared(cx, from, &self.list, k, shared, scratch).map_err(|r| r.0),
                None => {
                    let mut local = PackDirs::default();
                    open_tree(cx, from, &self.list, k, &mut local, scratch)
                }
            };
            Ok::<_, core::convert::Infallible>(opened.map(|view| CachedPack::new(view, self.heap.shared())))
        }).ok()?;
        match got {
            Ok(pack) => Some((pack, newly_opened)),
            Err(why) => {
                let _ = self.refused.set(*why);
                None
            }
        }
    }

    pub(crate) fn open_tree_with(
        &self,
        cx: &Cx,
        fs: &bstd::fs::Fs,
        dirs: Option<&SyncPackDirs>,
        scratch: &Frame,
        k: usize,
    ) -> Option<(PackView<'static>, bool)> {
        let (pack, newly_opened) = self.open_cached(cx, fs, dirs, scratch, k)?;
        Some((pack.view, newly_opened))
    }

    pub(crate) fn tree(&self, cx: &Cx, fs: &bstd::fs::Fs, scratch: &Frame, k: usize) -> Option<PackView<'static>> {
        let (pack, _) = self.open_cached(cx, fs, None, scratch, k)?;
        Some(pack.view)
    }

    fn record_pack_refusal(&self, why: &blaze_fetch::PackRefused) {
        if self.refused.get().is_some() {
            return;
        }
        let mut msg = blaze_own::AString::new_in(self.heap.alloc());
        blaze_text::Writes::show(&mut msg, why);
        let leaked: &'static str = blaze_own::Box::leak_static(msg.into_boxed_str());
        let _ = self.refused.set(leaked);
    }

    /// Reads file `path` of pack `k` as UTF-8 text, decompressing and verifying UTF-8 at most
    /// once across all workers and stretches (zero bytes allocated in `scratch`).
    pub(crate) fn read_str(&self, cx: &Cx, fs: &bstd::fs::Fs, scratch: &Frame, k: usize, path: &str) -> Option<&str> {
        let (pack, _) = self.open_cached(cx, fs, None, scratch, k)?;
        match pack.get_or_load(cx, path, self.heap.shared()) {
            Ok(Some(entry)) => entry.as_str(),
            Ok(None) => None,
            Err(why) => {
                self.record_pack_refusal(&why);
                None
            }
        }
    }

    /// Reads file `path` of pack `k` as raw bytes, decompressing at most once across all workers
    /// and stretches (zero bytes allocated in `scratch`).
    pub(crate) fn read_slice(&self, cx: &Cx, fs: &bstd::fs::Fs, scratch: &Frame, k: usize, path: &str) -> Option<&[u8]> {
        let (pack, _) = self.open_cached(cx, fs, None, scratch, k)?;
        match pack.get_or_load(cx, path, self.heap.shared()) {
            Ok(Some(entry)) => entry.as_bytes(),
            Ok(None) => None,
            Err(why) => {
                self.record_pack_refusal(&why);
                None
            }
        }
    }

    /// Reads checkout file `inner` (`/0/<inner>`) from `root` at most once per `SyncLazyTrees`
    /// lifetime (rule 74): one `read_all`, one UTF-8 check, and one `FILE_DOMAIN` BLAKE3 digest.
    /// Returns `(cached_path, entry)` borrowed from `&self` (zero bytes in any stretch `Frame`).
    pub(crate) fn read_checkout(
        &self,
        cx: &Cx,
        root: &Dir<ReadWrite>,
        inner: &str,
    ) -> Result<(&str, &CachedCheckout), bstd::sync::OnceError<core::convert::Infallible>> {
        self.checkout.get_or_load(cx, root, inner, self.heap.shared())
    }

    /// Returns the precomputed `crate::sweep::FILE_DOMAIN` digest for an already-cached UTF-8
    /// checkout file `inner`, with zero locks, zero allocation, and zero BLAKE3 recomputation.
    pub(crate) fn checkout_digest(&self, inner: &str) -> Option<ContentId> {
        self.checkout.get_digest(inner)
    }

    /// Returns the cached path slice for an already-cached checkout file `inner`, avoiding
    /// re-copying the path into a job's stretch `Frame`.
    pub(crate) fn cached_checkout_path(&self, inner: &str) -> Option<&str> {
        self.checkout.cached_path(inner)
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
    let dir_owned;
    let dir = match slot.get() {
        Some(dir) => dir,
        None => {
            let opened = open_home_pack_dir(cx, from, folder, at.as_str(), scratch)?;
            // If another thread raced and is still in `WRITING`, `slot.set` returns `Err(mine)`
            // while `slot.get()` is briefly `None`; borrow `dir_owned` for this call so it never
            // fails spuriously, then drop it cleanly on return.
            match slot.set(opened) {
                Ok(()) => match slot.get() {
                    Some(dir) => dir,
                    None => {
                        let kept = |words: &str| -> PackRefusal { PackRefusal(blaze_own::Box::leak_static(blaze_own::AString::from_str_in(words, from.heap.alloc()).into_boxed_str())) };
                        return Err(kept(bstd::mem::format_in!(scratch, "{at}: its folder was not opened").as_str()));
                    }
                },
                Err(mine) => {
                    dir_owned = mine;
                    slot.get().unwrap_or(&dir_owned)
                }
            }
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

    fn open_cached(&self, cx: &Cx, scratch: &Frame, k: usize) -> Option<&'static CachedPack> {
        match self.packs.open_cached(cx, self.fs, Some(self.dirs), scratch, k) {
            Some((pack, newly_opened)) => {
                if newly_opened {
                    self.seen.opened.set(self.seen.opened.get() + 1);
                }
                Some(pack)
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

    pub(crate) fn tree(&self, cx: &Cx, scratch: &Frame, k: usize) -> Option<PackView<'static>> {
        self.open_cached(cx, scratch, k).map(|pack| pack.view)
    }

    /// Reads file `path` of immutable pack `k` as UTF-8 text from the command-wide cache
    /// (zero bytes allocated in `scratch`, zero repeat Zstd decode across workers/suites).
    pub(crate) fn read_str(&self, cx: &Cx, scratch: &Frame, k: usize, path: &str) -> Option<&'static str> {
        let pack = self.open_cached(cx, scratch, k)?;
        match pack.get_or_load(cx, path, self.packs.heap.shared()) {
            Ok(Some(entry)) => entry.as_str(),
            Ok(None) => None,
            Err(why) => {
                self.packs.record_pack_refusal(&why);
                None
            }
        }
    }

    /// Reads file `path` of immutable pack `k` as raw bytes from the command-wide cache
    /// (zero bytes allocated in `scratch`, zero repeat Zstd decode across workers/suites).
    pub(crate) fn read_slice(&self, cx: &Cx, scratch: &Frame, k: usize, path: &str) -> Option<&'static [u8]> {
        let pack = self.open_cached(cx, scratch, k)?;
        match pack.get_or_load(cx, path, self.packs.heap.shared()) {
            Ok(Some(entry)) => entry.as_bytes(),
            Ok(None) => None,
            Err(why) => {
                self.packs.record_pack_refusal(&why);
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
            let file = view.read("src/lib.rs", &frame).expect("read").expect("src/lib.rs");
            assert_eq!(file.bytes.as_slice(), b"pub fn ok() {}\n");
        }
        assert_eq!(dirs.opened_count(), 1, "64 suite opens must share one sources Dir, never open 64 drivers");
    }

    #[test]
    fn cached_pack_fails_closed_on_submodule_paths() {
        let io = Io::simulated(SimWorld::new(93, Limits::USUAL));
        let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("root cx");
        let heap = blaze_own::testing::heap();
        let limits = PackLimits { most_files: 8, most_bytes: 1 << 16, most_path: 128 };
        let sub_oid = [0x33u8; 20];
        let mut builder = Builder::new(PackKind::Git, [0x42; 32], limits, 0, heap.alloc()).expect("builder");
        builder.file("src/lib.rs", Mode::File, b"pub fn ok() {}\n").expect("file");
        builder.file("v8", Mode::Submodule, &sub_oid).expect("submodule");
        let pack_bytes = builder.finish().expect("pack bytes");
        let leaked: &'static [u8] = blaze_own::Box::leak_static(pack_bytes.into_boxed_slice());
        let view = PackView::open_verified(leaked).expect("valid pack");
        let cached = CachedPack::new(view, heap.shared());

        let ok = cached.get_or_load(&cx, "src/lib.rs", heap.shared()).expect("normal file").expect("present");
        assert_eq!(ok.as_str(), Some("pub fn ok() {}\n"));
        assert!(cached.get_or_load(&cx, "v8_extra/foo.rs", heap.shared()).expect("non-submodule miss").is_none());

        for bad in ["v8", "v8/src/lib.rs"] {
            let Err(err) = cached.get_or_load(&cx, bad, heap.shared()) else {
                panic!("submodule path `{bad}` must fail closed");
            };
            assert_eq!(
                err,
                blaze_fetch::PackRefused::Submodule {
                    path: blaze_fetch::unpack::Cut::of(bad.as_bytes()),
                    submodule: blaze_fetch::unpack::Cut::of(b"v8"),
                    commit: sub_oid,
                }
            );
        }
    }

    #[test]
    fn checkout_cache_reads_once_caches_digest_and_catches_stale_on_fresh_cache() {
        let io = Io::simulated(SimWorld::new(97, Limits::USUAL));
        let Ok(cx) = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME) else {
            panic!("root cx");
        };
        let Ok(fs) = io.need_fs() else {
            panic!("fs");
        };
        let Ok(ws_path) = Path::new("ws") else {
            panic!("ws path");
        };
        let Ok(root) = fs.open_dir(&cx, &ws_path) else {
            panic!("open ws");
        };
        let heap = blaze_own::testing::heap();
        let alloc = heap.shared();

        let Ok(lib_path) = Path::new("src/lib.rs") else {
            panic!("lib path");
        };
        let Ok(bin_path) = Path::new("src/data.bin") else {
            panic!("bin path");
        };
        let first_text: &[u8] = b"pub fn one() -> u32 { 1 }\n";
        let second_text: &[u8] = b"pub fn one() -> u32 { 2 }\n";
        let raw_bin: &[u8] = &[0xff, 0xfe, 0x00, 0x41];
        if let Err(err) = root.publish(&cx, &lib_path, Content::bytes(first_text), Durable::No) {
            panic!("publish lib.rs: {err}");
        }
        if let Err(err) = root.publish(&cx, &bin_path, Content::bytes(raw_bin), Durable::No) {
            panic!("publish data.bin: {err}");
        }

        let cache = CheckoutCache::new();
        assert_eq!(cache.get_digest("src/lib.rs"), None);
        let Ok((path, entry)) = cache.get_or_load(&cx, &root, "src/lib.rs", alloc) else {
            panic!("load lib.rs");
        };
        assert_eq!(path, "src/lib.rs");
        assert_eq!(entry.as_str(), Some("pub fn one() -> u32 { 1 }\n"));
        assert_eq!(entry.as_bytes(), Some(first_text));
        let expected_first = ContentId::of(crate::sweep::FILE_DOMAIN, first_text);
        assert_eq!(entry.digest(), Some(expected_first));
        assert_eq!(cache.get_digest("src/lib.rs"), Some(expected_first));
        assert_eq!(cache.cached_path("src/lib.rs"), Some("src/lib.rs"));

        // Mutate file on disk: same command's cache must serve the first snapshot without re-reading.
        if let Err(err) = root.publish(&cx, &lib_path, Content::bytes(second_text), Durable::No) {
            panic!("republish lib.rs: {err}");
        }
        let Ok((_, again)) = cache.get_or_load(&cx, &root, "src/lib.rs", alloc) else {
            panic!("reload lib.rs");
        };
        assert_eq!(again.as_str(), Some("pub fn one() -> u32 { 1 }\n"));
        assert_eq!(cache.get_digest("src/lib.rs"), Some(expected_first));

        // Non-UTF-8 binary file: readable as bytes, None as str/digest, Unknown(Utf8Error) as source.
        let Ok((bpath, bentry)) = cache.get_or_load(&cx, &root, "src/data.bin", alloc) else {
            panic!("load data.bin");
        };
        assert_eq!(bpath, "src/data.bin");
        assert_eq!(bentry.as_str(), None);
        assert_eq!(bentry.as_bytes(), Some(raw_bin));
        assert_eq!(bentry.digest(), None);
        assert!(matches!(bentry.as_source(bpath), blaze_scope::resolve::Found::Unknown(_)));

        // Absent and dot-segment paths map to Absent.
        let Ok((_, missing)) = cache.get_or_load(&cx, &root, "src/missing.rs", alloc) else {
            panic!("load missing.rs");
        };
        assert!(matches!(missing.as_source("src/missing.rs"), blaze_scope::resolve::Found::Absent));
        assert_eq!(missing.as_bytes(), None);
        let Ok((_, dotdot)) = cache.get_or_load(&cx, &root, "../escape.rs", alloc) else {
            panic!("load escape.rs");
        };
        assert!(matches!(dotdot.as_source("../escape.rs"), blaze_scope::resolve::Found::Absent));

        // Drop cleans up without panic; a fresh cache for a new command observes the mutated file.
        drop(cache);
        let fresh = CheckoutCache::new();
        let Ok((_, fresh_entry)) = fresh.get_or_load(&cx, &root, "src/lib.rs", alloc) else {
            panic!("fresh load lib.rs");
        };
        let expected_second = ContentId::of(crate::sweep::FILE_DOMAIN, second_text);
        assert_eq!(fresh_entry.as_str(), Some("pub fn one() -> u32 { 2 }\n"));
        assert_eq!(fresh_entry.digest(), Some(expected_second));
        assert_eq!(fresh.get_digest("src/lib.rs"), Some(expected_second));
    }
}
