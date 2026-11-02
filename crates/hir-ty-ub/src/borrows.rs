//! Tree Borrows: every allocation's tree of tags with their per-byte permissions, and the
//! global state around them (the next tag, the protectors of each frame, exposure).
//!
//! A port of Miri, rule for rule: `src/tools/miri/src/borrow_tracker/tree_borrows/` and the
//! Tree Borrows parts of `src/tools/miri/src/borrow_tracker/mod.rs`, at `ad3d0bc141a0`, with
//! Miri's default parameters (precise interior mutability, no implicit writes unless asked,
//! `Box` with any allocator unique). The caller (`check`) runs memory's checks first: every
//! allocation, pointer and range given here is live and in bounds.
//!
//! - [`Borrows::new_alloc`]: a tree whose root tag is the allocation's own pointer;
//! - [`Borrows::access`]: a read or write through a pointer, on every tag of the allocation;
//! - [`Borrows::retag`]: a reference or `Box` made from a pointer gets a new child tag, with
//!   the implicit read (or write) a reborrow does, and on function entry a protector;
//! - [`Borrows::pop_frame`]: the frame's protectors end, with the access each does; each
//!   thread has its own stack of frames ([`Borrows::set_thread`]);
//! - [`Borrows::expose`] and [`Prov::Wildcard`]: permissive provenance;
//! - [`Borrows::gc`]: tags nothing holds any more are dropped and the trees compacted.

mod diagnostics;
mod foreign_access_skipping;
mod perms;
mod range_map;
mod tree;
mod tree_visitor;
mod unimap;
mod wildcard;

#[cfg(test)]
mod exhaustive;
#[cfg(test)]
mod tests;

use blaze_ids::bits::BitSet;
use smallvec::SmallVec;

pub use self::diagnostics::{AliasError, AliasErrorKind, Event};
use self::{
    diagnostics::AccessCause,
    foreign_access_skipping::IdempotentForeignAccess,
    perms::Permission,
    range_map::RangeMap,
    tree::{LocationState, Tree},
};
use crate::ids::{AccessKind, AllocId, BorTag, Prov, SiteId, Span, ThreadId};

/// How strongly a function-entry retag protects. Miri: `borrow_tracker/mod.rs`,
/// `ProtectorKind`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum ProtectorKind {
    /// Invalidation is UB, but the pointer may still free the memory (`Box` arguments:
    /// `noalias` without `dereferenceable`).
    WeakProtector,
    /// Invalidation and deallocation are UB (references: `noalias` and `dereferenceable`).
    StrongProtector,
}

/// What a retagged pointer is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PtrKind {
    /// `&T`.
    Shared,
    /// `&mut T`.
    Mut,
    /// `Box<T, A>`, any allocator.
    Box,
}

/// The retag a typed copy does (rustc_const_eval's `RetagMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RetagMode {
    Default,
    TwoPhase,
    /// A call's arguments: the new tag is protected until the frame returns.
    FnEntry,
    /// A reference cast to a raw pointer: Tree Borrows gives raw pointers no tag of their own
    /// (Miri: `tb_retag_ptr_value`, `ty::RawPtr`).
    Raw,
    None,
}

/// The permissions a retag gives: inside the pointee's range, per byte by whether it is in an
/// `UnsafeCell`, and outside it. Miri: `tree_borrows/mod.rs`, `NewPermission`.
#[derive(Clone, Debug)]
pub struct NewPermission {
    kind: PtrKind,
    mode: RetagMode,
    freeze: bool,
    unpin: bool,
    implicit_writes: bool,
    cells: SmallVec<[Span; 2]>,
}

/// Miri's `NewPermission` proper.
#[derive(Clone, Copy, Debug)]
struct Perms {
    freeze_perm: Permission,
    nonfreeze_perm: Permission,
    outside_perm: Permission,
    protector: Option<ProtectorKind>,
}

impl NewPermission {
    /// The retag of a `kind` pointer to a pointee whose type is `Freeze` (`freeze`) and
    /// `Unpin` and `UnsafeUnpin` (`unpin`), whose `UnsafeCell` bytes are `cells` (offsets
    /// from the pointee's start, in increasing order, not overlapping). The caller computes
    /// `cells` as Miri's `visit_freeze_sensitive`: an `UnsafeCell` field, and a whole union or
    /// multi-variant enum that is not `Freeze`, are cells. Miri: `NewPermission::new`.
    pub fn new(kind: PtrKind, mode: RetagMode, freeze: bool, unpin: bool, cells: &[Span]) -> NewPermission {
        debug_assert!(cells.windows(2).all(|w| w[0].end() <= w[1].start), "cells overlap or are unsorted");
        NewPermission { kind, mode, freeze, unpin, implicit_writes: false, cells: SmallVec::from_slice(cells) }
    }

    /// `-Zmiri-tree-borrows-implicit-writes`: a protected `&mut` or `Box` is `Unique` at once,
    /// its retag a write. Off in Miri by default; a function marked `#[rustc_no_writable]`
    /// turns it off for its arguments (the caller checks that).
    pub fn with_implicit_writes(mut self, on: bool) -> NewPermission {
        self.implicit_writes = on;
        self
    }

    /// Whether this retag of a non-empty range writes through the parent (an implicitly
    /// written `Unique`). Miri refuses such a retag into read-only memory as `WriteToReadOnly`
    /// before the tree sees it (`tb_reborrow`), so the caller checks mutability first.
    pub fn writes(&self) -> bool {
        self.perms().is_some_and(|p| p.freeze_perm.associated_access() == Some(AccessKind::Write))
    }

    /// `None`: the retag is a no-op.
    fn perms(&self) -> Option<Perms> {
        if matches!(self.mode, RetagMode::None | RetagMode::Raw) {
            return None;
        }
        let is_protected = self.mode == RetagMode::FnEntry;
        let implicit_writes_enabled = is_protected && self.implicit_writes;
        // A `&mut` or `Box` to a pinning type: no retag.
        if matches!(self.kind, PtrKind::Mut | PtrKind::Box) && !self.unpin {
            return None;
        }
        #[derive(PartialEq)]
        enum Part {
            InsideFrozen,
            InsideUnsafeCell,
            Outside,
        }
        let perm = |part: Part| {
            let frozen = match part {
                Part::InsideFrozen => true,
                Part::InsideUnsafeCell => false,
                Part::Outside => self.freeze,
            };
            match self.kind {
                PtrKind::Shared => {
                    if frozen {
                        Permission::new_frozen()
                    } else {
                        Permission::new_cell()
                    }
                }
                // Protected `&mut UnsafeCell` and `Box<UnsafeCell>` are `ReservedFrz` too, or
                // `noalias` would be unsound.
                PtrKind::Mut | PtrKind::Box => {
                    if implicit_writes_enabled && part != Part::Outside {
                        Permission::new_unique()
                    } else if is_protected || frozen {
                        Permission::new_reserved_frz()
                    } else {
                        Permission::new_reserved_im()
                    }
                }
            }
        };
        Some(Perms {
            freeze_perm: perm(Part::InsideFrozen),
            nonfreeze_perm: perm(Part::InsideUnsafeCell),
            outside_perm: perm(Part::Outside),
            protector: is_protected.then_some(match self.kind {
                PtrKind::Shared | PtrKind::Mut => ProtectorKind::StrongProtector,
                PtrKind::Box => ProtectorKind::WeakProtector,
            }),
        })
    }
}

/// Every allocation's tree and the global state. Miri: `borrow_tracker/mod.rs`,
/// `GlobalStateInner` and `FrameState`, and each allocation's `AllocState`.
#[derive(Debug)]
pub struct Borrows {
    next_tag: BorTag,
    /// By `AllocId::index`; `None` before the allocation and after its free.
    trees: Vec<Option<Tree>>,
    /// Each thread's frames, by `ThreadId::index`: frames of different threads return in any
    /// order.
    threads: Vec<ThreadFrames>,
    /// The thread `push_frame`, `pop_frame` and function-entry retags act on.
    thread: usize,
    retags_since_gc: u32,
    /// The GC's live set, a bit per tag; kept between runs.
    live: BitSet,
    /// The per-byte states of the retag being done; kept between retags.
    inside: RangeMap<LocationState>,
    /// The accesses the last retag did.
    retag_accesses: Vec<(Span, AccessKind)>,
}

/// One thread's stack of frames and their protected tags. Miri: each frame's
/// `FrameState::protected_tags`, here one list per thread with each frame's start in it.
#[derive(Debug, Default)]
struct ThreadFrames {
    frames: Vec<u32>,
    protected: Vec<(AllocId, BorTag)>,
}

/// The tags a [`Borrows::gc`] keeps, which its caller marks every tag still held in.
pub struct LiveTags<'a>(&'a mut BitSet);

impl LiveTags<'_> {
    /// Keeps `tag`; a tag newer than the GC's start has nothing to drop.
    #[inline]
    pub fn mark(&mut self, tag: BorTag) {
        let n = tag.get() as usize;
        if n < self.0.capacity() {
            self.0.insert(n);
        }
    }
}

impl Default for Borrows {
    fn default() -> Borrows {
        Borrows::new()
    }
}

impl Borrows {
    /// How many retags between garbage collections (Miri: `-Zmiri-provenance-gc=10000`).
    pub const GC_EVERY: u32 = 10_000;

    pub fn new() -> Borrows {
        Borrows {
            next_tag: BorTag::new(1),
            trees: Vec::new(),
            threads: vec![ThreadFrames::default()],
            thread: ThreadId::MAIN.index(),
            retags_since_gc: 0,
            live: BitSet::default(),
            inside: RangeMap::new(0, LocationState::new_accessed(Permission::new_disabled(), IdempotentForeignAccess::None)),
            retag_accesses: Vec::new(),
        }
    }

    /// Miri: `GlobalStateInner::new_ptr`.
    fn new_ptr(&mut self) -> BorTag {
        let id = self.next_tag;
        self.next_tag = id.succ().expect("ran out of pointer tags");
        id
    }

    fn tree_mut(&mut self, alloc: AllocId) -> &mut Tree {
        match self.trees.get_mut(alloc.index()) {
            Some(Some(tree)) => tree,
            _ => panic!("Tree Borrows: {alloc:?} is not live"),
        }
    }

    fn live_tree_mut(&mut self, alloc: AllocId) -> Option<&mut Tree> {
        self.trees.get_mut(alloc.index()).and_then(Option::as_mut)
    }

    /// A new allocation of `size` bytes; returns its root tag, the tag of the pointer the
    /// allocation hands out. Miri: `Tree::new_allocation`, `root_ptr_tag`.
    pub fn new_alloc(&mut self, alloc: AllocId, size: u64, site: SiteId) -> BorTag {
        let tag = self.new_ptr();
        let i = alloc.index();
        if i >= self.trees.len() {
            self.trees.resize_with(i + 1, || None);
        }
        assert!(self.trees[i].is_none(), "Tree Borrows: {alloc:?} allocated twice");
        self.trees[i] = Some(Tree::new(tag, size, site));
        tag
    }

    /// The root tag of a live allocation.
    pub fn root(&self, alloc: AllocId) -> Option<BorTag> {
        self.trees.get(alloc.index()).and_then(Option::as_ref).map(Tree::root_tag)
    }

    /// The tag `prov` accesses `alloc` through; `None` for a wildcard.
    fn source(alloc: AllocId, prov: Prov) -> Option<BorTag> {
        match prov {
            Prov::Concrete { alloc: of, tag } => {
                debug_assert_eq!(of, alloc, "a pointer into {alloc:?} with provenance of {of:?}");
                Some(tag)
            }
            Prov::Wildcard => None,
        }
    }

    /// A read or write of `range` of `alloc` through `prov`. Miri:
    /// `Tree::before_memory_access`.
    pub fn access(
        &mut self,
        alloc: AllocId,
        prov: Prov,
        kind: AccessKind,
        range: Span,
        site: SiteId,
    ) -> Result<(), AliasError> {
        let source = Borrows::source(alloc, prov);
        self.tree_mut(alloc).perform_access(source, range, kind, AccessCause::Explicit(kind), alloc, site)
    }

    /// A reference or `Box` made from a pointer with provenance `parent` to `range` of
    /// `alloc`: a new tag under the parent's with the permissions of `new`, after the
    /// implicit access the reborrow does on the parent (a read, or a write for an implicitly
    /// written `Unique`; none on `Cell` bytes). `Ok(None)`: no retag, the pointer keeps its
    /// tag. A retag of a pointer into an allocation that is no longer live must be of size 0
    /// and only makes the tag. Miri: `tb_retag_ptr_value`, `tb_retag_place`, `tb_reborrow`.
    pub fn retag(
        &mut self,
        alloc: AllocId,
        parent: Prov,
        new: NewPermission,
        range: Span,
        site: SiteId,
    ) -> Result<Option<BorTag>, AliasError> {
        self.retag_accesses.clear();
        let Some(perms) = new.perms() else { return Ok(None) };
        let new_tag = self.new_ptr();
        self.retags_since_gc = self.retags_since_gc.saturating_add(1);
        if perms.protector.is_some() {
            assert!(
                !self.threads[self.thread].frames.is_empty(),
                "Tree Borrows: a function-entry retag outside any frame"
            );
        }
        let parent_tag = Borrows::source(alloc, parent);
        let Borrows { trees, inside, retag_accesses, threads, thread, .. } = self;
        // Miri registers the protector before the parent's access; here it is registered once
        // the tag is in the tree, so a refused retag leaves no protected tag that isn't (the
        // run stops at the error either way).
        let protect = |threads: &mut Vec<ThreadFrames>| {
            if perms.protector.is_some() {
                threads[*thread].protected.push((alloc, new_tag));
            }
        };
        let Some(tree) = trees.get_mut(alloc.index()).and_then(Option::as_mut) else {
            assert!(range.is_empty(), "Tree Borrows: a retag of {} bytes of dead {alloc:?}", range.len);
            protect(threads);
            return Ok(Some(new_tag));
        };
        let protected = perms.protector.is_some();
        let loc_state = |frozen: bool| {
            let perm = if frozen { perms.freeze_perm } else { perms.nonfreeze_perm };
            let sifa = perm.strongest_idempotent_foreign_access(protected);
            if perm.associated_access().is_some() {
                LocationState::new_accessed(perm, sifa)
            } else {
                LocationState::new_non_accessed(perm, sifa)
            }
        };
        // Miri's `visit_freeze_sensitive`: frozen up to each cell, the cell, and frozen after
        // the last; each written over a map that starts `Disabled`.
        inside.reset(
            range.len,
            LocationState::new_accessed(Permission::new_disabled(), IdempotentForeignAccess::None),
        );
        let mut cur = 0;
        let mut set = |start: u64, end: u64, frozen: bool| {
            let state = loc_state(frozen);
            for (_, loc) in inside.iter_mut(start, end - start) {
                *loc = state;
            }
        };
        for cell in &new.cells {
            if cell.is_empty() {
                continue;
            }
            assert!(cell.start >= cur && cell.end() <= range.len, "cell {cell:?} out of order or bounds");
            if cell.start > cur {
                set(cur, cell.start, true);
            }
            set(cell.start, cell.end(), false);
            cur = cell.end();
        }
        if range.len > cur {
            set(cur, range.len, true);
        }
        for (perm_range, loc_state) in inside.iter_all() {
            if let Some(access) = loc_state.permission().associated_access() {
                let range_in_alloc = Span::new(range.start + perm_range.start, perm_range.end - perm_range.start);
                tree.perform_access(parent_tag, range_in_alloc, access, AccessCause::Reborrow(access), alloc, site)?;
                retag_accesses.push((range_in_alloc, access));
            }
        }
        tree.new_child(range.start, parent_tag, new_tag, inside, perms.outside_perm, perms.protector, site);
        protect(threads);
        Ok(Some(new_tag))
    }

    /// The accesses the last [`Borrows::retag`] did on the parent, which the race detector
    /// sees as non-atomic accesses of a retag (and which, as writes, need mutable memory).
    pub fn retag_accesses(&self) -> &[(Span, AccessKind)] {
        &self.retag_accesses
    }

    /// The free of `alloc` (`size` bytes) through `prov`: a write to all of it, refused if a
    /// strong protector covers an accessed byte. Then the tree is gone. Miri:
    /// `Tree::before_memory_deallocation`, `Tree::dealloc`.
    pub fn free(&mut self, alloc: AllocId, prov: Prov, size: u64, site: SiteId) -> Result<(), AliasError> {
        let source = Borrows::source(alloc, prov);
        self.tree_mut(alloc).dealloc(source, Span::new(0, size), alloc, site)?;
        self.trees[alloc.index()] = None;
        Ok(())
    }

    /// `tag` was exposed (cast to an integer): wildcard accesses may come through it. Freed
    /// allocations have no tree to expose in. Miri: `tb_expose_tag`.
    pub fn expose(&mut self, alloc: AllocId, tag: BorTag) {
        if let Some(tree) = self.live_tree_mut(alloc) {
            tree.expose_tag(tag);
        }
    }

    /// The thread whose stack the next `push_frame`, `pop_frame` and function-entry retags
    /// act on; the main thread's until set. Miri keeps each frame's protected tags in the
    /// frame, so each thread's in its own stack.
    pub fn set_thread(&mut self, thread: ThreadId) {
        let i = thread.index();
        if i >= self.threads.len() {
            self.threads.resize_with(i + 1, ThreadFrames::default);
        }
        self.thread = i;
    }

    /// A call on the current thread: its function-entry retags are protected until the
    /// matching `pop_frame`.
    pub fn push_frame(&mut self) {
        let t = &mut self.threads[self.thread];
        t.frames.push(t.protected.len() as u32);
    }

    /// The current thread's innermost frame returns: each of its protected tags, in the order
    /// they were made, does its protector's end access (only on locations it accessed, a write
    /// where it is `Unique`), and then they are unprotected. Every tag of the frame stays
    /// protected until all have done theirs. Miri: `on_stack_pop`, `release_protector`,
    /// `end_call`.
    pub fn pop_frame(&mut self, site: SiteId) -> Result<(), AliasError> {
        let Borrows { trees, threads, thread, .. } = self;
        let ThreadFrames { frames, protected } = &mut threads[*thread];
        let start = frames.pop().expect("Tree Borrows: pop_frame without a frame") as usize;
        for &(alloc, tag) in &protected[start..] {
            // A weak protector allows the free, so the allocation may be gone.
            if let Some(tree) = trees.get_mut(alloc.index()).and_then(Option::as_mut) {
                tree.perform_protector_end_access(tag, alloc, site)?;
                tree.update_exposure_for_protector_release(tag);
            }
        }
        for &(alloc, tag) in &protected[start..] {
            if let Some(tree) = trees.get_mut(alloc.index()).and_then(Option::as_mut) {
                let idx = tree.index_of(tag);
                tree.nodes.protector[idx.index()] = None;
            }
        }
        protected.truncate(start);
        Ok(())
    }

    /// Whether [`Borrows::GC_EVERY`] retags happened since the last [`Borrows::gc`].
    pub fn gc_due(&self) -> bool {
        self.retags_since_gc >= Borrows::GC_EVERY
    }

    /// Drops every tag nothing holds. `live` is called once with the set the caller marks
    /// every tag still held in, in memory and in the evaluator's values; protected tags, roots
    /// and exposed tags are kept anyway. Miri: `run_provenance_gc`, `remove_unreachable_tags`.
    pub fn gc(&mut self, live: impl FnOnce(&mut LiveTags<'_>)) {
        self.retags_since_gc = 0;
        let Borrows { trees, threads, live: set, next_tag, .. } = self;
        set.reset(next_tag.get() as usize);
        let mut tags = LiveTags(set);
        live(&mut tags);
        // Protected tags still have an access coming, and the tree compaction relies on them
        // being live (Miri: `FrameState::visit_provenance`).
        for &(_, tag) in threads.iter().flat_map(|t| &t.protected) {
            tags.mark(tag);
        }
        for tree in trees.iter().flatten() {
            tree.visit_provenance(&mut |tag| tags.mark(tag));
        }
        let set = &*set;
        let is_live = |tag: BorTag| set.contains(tag.get() as usize);
        for tree in trees.iter_mut().flatten() {
            tree.remove_unreachable_tags(&is_live);
        }
    }

    /// Names the `nth_parent` of `tag` (0: itself) in reports and the printed tree (Miri:
    /// `miri_pointer_name`); whether there was such a tag.
    pub fn name_tag(&mut self, alloc: AllocId, tag: BorTag, nth_parent: u8, name: &str) -> bool {
        match self.live_tree_mut(alloc) {
            Some(tree) if tree.tag_mapping.contains_key(&tag) => {
                tree.give_pointer_debug_name(tag, nth_parent, name)
            }
            _ => false,
        }
    }

    /// The tree of a live allocation as Miri's `miri_print_borrow_state` prints it, only the
    /// named tags unless `show_unnamed`.
    pub fn print_tree(&self, alloc: AllocId, show_unnamed: bool) -> Option<String> {
        self.trees.get(alloc.index()).and_then(Option::as_ref).map(|tree| tree.print_tree(show_unnamed))
    }

    /// How many tags the tree of a live allocation holds (for tests of the GC).
    pub fn tag_count(&self, alloc: AllocId) -> Option<usize> {
        self.trees.get(alloc.index()).and_then(Option::as_ref).map(|tree| tree.tag_mapping.len())
    }
}
