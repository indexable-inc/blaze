//! The tree of one allocation: its nodes (one per tag), the per-location permissions of each,
//! and the traversals that apply an access, a deallocation, a protector's end and the GC.
//!
//! Miri: `tree_borrows/tree.rs`. Where this differs from Miri:
//! - `LocationState` is one byte (permission, accessed bit and SIFA packed in a `NonZeroU8`,
//!   so `Option<LocationState>` is one byte too) where Miri's is two; `packed_state_roundtrip`
//!   checks the packing against the unpacked fields over every state;
//! - nodes are columns indexed by `UniIndex` (tag, parent, children, defaults, exposure and
//!   protector hot; debug info and history cold) instead of a `UniValMap<Node>`;
//! - a node's protector is a column of its tree instead of Miri's global `protected_tags` map,
//!   so the protector check of every visited node is an index, not a hash lookup;
//! - the traversal stack is a buffer kept by the tree.

use std::{cmp, fmt, mem, num::NonZeroU8, ops::Range};

use smallvec::SmallVec;

use super::{
    ProtectorKind,
    diagnostics::{
        AccessCause, AliasError, DiagnosticInfo, NodeDebugInfo, TransitionError, build_error,
        no_valid_exposed_references_error,
    },
    foreign_access_skipping::IdempotentForeignAccess,
    perms::{PermTransition, Permission, PermissionPriv},
    range_map::RangeMap,
    tree_visitor::{ChildrenVisitMode, ContinueTraversal, NodeAppArgs, TreeVisitor, VisitStack},
    unimap::{UniIndex, UniKeyMap, UniValMap},
    wildcard::ExposedCache,
};
use crate::ids::{AccessKind, AllocId, BorTag, SiteId, Span};

/// The state of one tag at one location. Miri: `tree.rs`, `LocationState`.
///
/// A location is "accessed" once it is child-accessed (or covered by the initial retag), and
/// stays so. For a location not accessed yet, the permission is the "future initial
/// permission": foreign accesses still move it, but a protector only fires on accessed ones.
/// The root is accessed everywhere in bounds, as if the allocation were a write.
///
/// Bits: 0..=2 the permission, 3 accessed, 4..=5 the SIFA, 7 always set (the niche).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct LocationState(NonZeroU8);

const _: () = assert!(size_of::<LocationState>() == 1 && size_of::<Option<LocationState>>() == 1);

const ACCESSED: u8 = 1 << 3;
const SIFA_SHIFT: u8 = 4;
const SET: u8 = 1 << 7;

impl LocationState {
    pub(super) fn pack(
        permission: Permission,
        accessed: bool,
        sifa: IdempotentForeignAccess,
    ) -> LocationState {
        let bits = SET
            | permission.inner as u8
            | if accessed { ACCESSED } else { 0 }
            | ((sifa as u8) << SIFA_SHIFT);
        match NonZeroU8::new(bits) {
            Some(bits) => LocationState(bits),
            None => unreachable!("bit 7 is set"),
        }
    }

    /// A state never accessed nor foreign-accessed yet. Not `Unique`. Miri:
    /// `LocationState::new_non_accessed`.
    pub(super) fn new_non_accessed(
        permission: Permission,
        sifa: IdempotentForeignAccess,
    ) -> LocationState {
        assert!(permission.is_initial() || permission.is_disabled());
        assert!(!permission.is_unique());
        LocationState::pack(permission, false, sifa)
    }

    /// Miri: `LocationState::new_accessed`.
    pub(super) fn new_accessed(
        permission: Permission,
        sifa: IdempotentForeignAccess,
    ) -> LocationState {
        LocationState::pack(permission, true, sifa)
    }

    #[inline]
    pub(super) fn permission(self) -> Permission {
        Permission { inner: PermissionPriv::from_bits(self.0.get() & 0b111) }
    }

    #[inline]
    pub(super) fn accessed(self) -> bool {
        self.0.get() & ACCESSED != 0
    }

    #[inline]
    pub(super) fn idempotent_foreign_access(self) -> IdempotentForeignAccess {
        IdempotentForeignAccess::from_bits((self.0.get() >> SIFA_SHIFT) & 0b11)
    }

    fn set_permission(&mut self, permission: Permission) {
        *self = LocationState::pack(permission, self.accessed(), self.idempotent_foreign_access());
    }

    fn set_accessed(&mut self, accessed: bool) {
        *self = LocationState::pack(self.permission(), accessed, self.idempotent_foreign_access());
    }

    pub(super) fn set_idempotent_foreign_access(&mut self, sifa: IdempotentForeignAccess) {
        *self = LocationState::pack(self.permission(), self.accessed(), sifa);
    }

    /// Whether a real execution can reach this state: `Unique` only on accessed locations.
    pub(super) fn possible(self) -> bool {
        self.accessed() || !self.permission().is_unique()
    }

    /// Performs an access and records it in the node's history and the exposure cache.
    /// Miri: `LocationState::perform_transition`.
    #[allow(clippy::too_many_arguments)]
    fn perform_transition(
        &mut self,
        idx: UniIndex,
        nodes: &mut Nodes,
        exposed_cache: &mut ExposedCache,
        access_kind: AccessKind,
        relatedness: AccessRelatedness,
        protected: bool,
        diagnostics: &DiagnosticInfo,
    ) -> Result<(), TransitionError> {
        // Only reached when `skip_if_known_noop` said `Recurse`, as `record_new_access` needs.
        self.record_new_access(access_kind, relatedness);
        let old_access_level = self.permission().strongest_allowed_local_access(protected);
        let transition = self.perform_access(access_kind, relatedness, protected)?;
        if !transition.is_noop() {
            nodes.debug[idx.index()]
                .history
                .push(diagnostics.create_event(transition, relatedness.is_foreign()));
            if nodes.exposed[idx.index()] {
                let access_level = self.permission().strongest_allowed_local_access(protected);
                exposed_cache.update_exposure(nodes, idx, old_access_level, access_level);
            }
        }
        Ok(())
    }

    /// Applies the transition, sets the accessed bit on child accesses, and raises the
    /// protector error, which only accessed locations can. Miri: `LocationState::perform_access`.
    ///
    /// Why only accessed: two `&mut` to disjoint parts of an allocation may each read all of
    /// it, but a write through one must record that the other can no longer use that
    /// location; for the other that is a "future initial state" of `Disabled`, which must not
    /// trip its protector (`two_mut_protected_same_alloc` in `tests/pass/tree_borrows`).
    pub(super) fn perform_access(
        &mut self,
        access_kind: AccessKind,
        rel_pos: AccessRelatedness,
        protected: bool,
    ) -> Result<PermTransition, TransitionError> {
        let old_perm = self.permission();
        let transition = Permission::perform_access(access_kind, rel_pos, old_perm, protected)
            .ok_or(TransitionError::ChildAccessForbidden(old_perm))?;
        if !rel_pos.is_foreign() {
            self.set_accessed(true);
        }
        self.set_permission(transition.applied(old_perm).unwrap());
        if protected && self.accessed() && transition.produces_disabled() {
            return Err(TransitionError::ProtectedDisabled(old_perm));
        }
        debug_assert!(self.possible());
        Ok(transition)
    }

    /// `perform_access` as state passing, without the error's cause. Miri:
    /// `LocationState::perform_access_no_fluff`.
    #[cfg(test)]
    pub(super) fn perform_access_no_fluff(
        mut self,
        access_kind: AccessKind,
        rel_pos: AccessRelatedness,
        protected: bool,
    ) -> Option<LocationState> {
        self.perform_access(access_kind, rel_pos, protected).ok().map(|_| self)
    }

    /// Whether a foreign access can skip this node and its subtree. Miri:
    /// `LocationState::skip_if_known_noop`.
    fn skip_if_known_noop(self, access_kind: AccessKind, rel_pos: AccessRelatedness) -> ContinueTraversal {
        if rel_pos.is_foreign() {
            let happening_now = IdempotentForeignAccess::from_foreign(access_kind);
            let mut new_access_noop =
                self.idempotent_foreign_access().can_skip_foreign_access(happening_now);
            // A `Disabled` node has no protected accessed children, and its children can never
            // be accessed again: the access only changes which tag an error names.
            if self.permission().is_disabled() {
                new_access_noop = true;
            }
            // A `Frozen` node has no `Unique` children, and a child write that a conflicted
            // `Reserved` below would reject is rejected here already.
            if self.permission().is_frozen() && access_kind == AccessKind::Read {
                new_access_noop = true;
            }
            if new_access_noop {
                ContinueTraversal::SkipSelfAndChildren
            } else {
                ContinueTraversal::Recurse
            }
        } else {
            ContinueTraversal::Recurse
        }
    }

    /// Miri: `LocationState::record_new_access`.
    fn record_new_access(&mut self, access_kind: AccessKind, rel_pos: AccessRelatedness) {
        debug_assert!(matches!(
            self.skip_if_known_noop(access_kind, rel_pos),
            ContinueTraversal::Recurse
        ));
        let mut sifa = self.idempotent_foreign_access();
        sifa.record_new(IdempotentForeignAccess::from_acc_and_rel(access_kind, rel_pos));
        self.set_idempotent_foreign_access(sifa);
    }
}

impl fmt::Debug for LocationState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocationState")
            .field("permission", &self.permission())
            .field("accessed", &self.accessed())
            .field("idempotent_foreign_access", &self.idempotent_foreign_access())
            .finish()
    }
}

impl fmt::Display for LocationState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.permission())?;
        if !self.accessed() {
            write!(f, "?")?;
        }
        Ok(())
    }
}

/// Every node's state at one range of locations, and the wildcard cache there. Not every node
/// has a state at every range (they are inserted lazily); those that do are nodes of the tree.
/// Miri: `tree.rs`, `LocationTree`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LocationTree {
    pub(super) perms: UniValMap<LocationState>,
    pub(super) exposed_cache: ExposedCache,
}

const NO_PARENT: u32 = u32::MAX;
const VACANT: BorTag = BorTag::new(0);

/// The nodes of a tree as columns indexed by `UniIndex`. A vacant slot has tag 0, which no
/// pointer ever gets. Miri: `tree.rs`, `Node` in a `UniValMap`.
#[derive(Default, Debug)]
pub(super) struct Nodes {
    pub(super) tag: Vec<BorTag>,
    parent: Vec<u32>,
    pub(super) children: Vec<SmallVec<[UniIndex; 4]>>,
    /// What a location without a state starts as: `Reserved`, `Frozen`, `Cell` or `Unique`
    /// for a child, `Disabled` for a root (whose in-bounds locations all start `Unique`).
    pub(super) default_initial_perm: Vec<Permission>,
    /// The SIFA of locations without a state. See `foreign_access_skipping.rs`.
    pub(super) default_initial_idempotent_foreign_access: Vec<IdempotentForeignAccess>,
    /// Whether a wildcard access could happen through this node.
    pub(super) exposed: Vec<bool>,
    /// The protector of the frame that retagged this tag, until that frame returns.
    pub(super) protector: Vec<Option<ProtectorKind>>,
    pub(super) debug: Vec<NodeDebugInfo>,
}

impl Nodes {
    fn insert(
        &mut self,
        idx: UniIndex,
        parent: Option<UniIndex>,
        default_initial_perm: Permission,
        default_sifa: IdempotentForeignAccess,
        protector: Option<ProtectorKind>,
        debug: NodeDebugInfo,
    ) {
        let i = idx.index();
        if i >= self.tag.len() {
            let len = i + 1;
            self.tag.resize(len, VACANT);
            self.parent.resize(len, NO_PARENT);
            self.children.resize_with(len, SmallVec::new);
            self.default_initial_perm.resize(len, Permission::new_disabled());
            self.default_initial_idempotent_foreign_access
                .resize(len, IdempotentForeignAccess::None);
            self.exposed.resize(len, false);
            self.protector.resize(len, None);
            self.debug.resize_with(len, NodeDebugInfo::vacant);
        }
        debug_assert_eq!(self.tag[i], VACANT);
        self.tag[i] = debug.tag;
        self.parent[i] = parent.map_or(NO_PARENT, |p| p.idx);
        self.children[i].clear();
        self.default_initial_perm[i] = default_initial_perm;
        self.default_initial_idempotent_foreign_access[i] = default_sifa;
        self.exposed[i] = false;
        self.protector[i] = protector;
        self.debug[i] = debug;
    }

    /// Empties the slot, returning its tag.
    fn remove(&mut self, idx: UniIndex) -> BorTag {
        let i = idx.index();
        let tag = mem::replace(&mut self.tag[i], VACANT);
        debug_assert_ne!(tag, VACANT);
        self.parent[i] = NO_PARENT;
        self.children[i] = SmallVec::new();
        self.exposed[i] = false;
        self.protector[i] = None;
        self.debug[i] = NodeDebugInfo::vacant();
        tag
    }

    #[inline]
    pub(super) fn tag(&self, idx: UniIndex) -> BorTag {
        self.tag[idx.index()]
    }

    #[inline]
    pub(super) fn parent(&self, idx: UniIndex) -> Option<UniIndex> {
        let p = self.parent[idx.index()];
        (p != NO_PARENT).then_some(UniIndex { idx: p })
    }

    fn set_parent(&mut self, idx: UniIndex, parent: UniIndex) {
        self.parent[idx.index()] = parent.idx;
    }

    #[inline]
    pub(super) fn children(&self, idx: UniIndex) -> &[UniIndex] {
        &self.children[idx.index()]
    }

    #[inline]
    pub(super) fn is_exposed(&self, idx: UniIndex) -> bool {
        self.exposed[idx.index()]
    }

    #[inline]
    pub(super) fn is_protected(&self, idx: UniIndex) -> bool {
        self.protector[idx.index()].is_some()
    }

    /// Miri: `Node::default_location_state`.
    #[inline]
    pub(super) fn default_location_state(&self, idx: UniIndex) -> LocationState {
        LocationState::new_non_accessed(
            self.default_initial_perm[idx.index()],
            self.default_initial_idempotent_foreign_access[idx.index()],
        )
    }

    /// Every live node.
    pub(super) fn iter(&self) -> impl Iterator<Item = UniIndex> + '_ {
        self.tag
            .iter()
            .enumerate()
            .filter(|&(_, &tag)| tag != VACANT)
            .map(|(i, _)| UniIndex { idx: i as u32 })
    }
}

/// The tree of one allocation. Miri: `tree.rs`, `Tree`.
#[derive(Debug)]
pub(super) struct Tree {
    /// From tags to the index of every per-node table of this tree.
    pub(super) tag_mapping: UniKeyMap<BorTag>,
    pub(super) nodes: Nodes,
    pub(super) locations: RangeMap<LocationTree>,
    /// The root of the main tree, then the roots of the wildcard subtrees, sorted by tag (so
    /// the main root is first). A retag of a wildcard pointer can't know where it attaches; it
    /// becomes the root of a subtree that is a child of *some* exposed tag smaller than it,
    /// so of a tree before it.
    pub(super) roots: SmallVec<[UniIndex; 2]>,
    stack: VisitStack,
}

impl Tree {
    /// A tree with only the root, `Unique` and accessed on every in-bounds location and
    /// `Disabled` outside. Miri: `Tree::new`.
    pub(super) fn new(root_tag: BorTag, size: u64, span: SiteId) -> Tree {
        let root_default_perm = Permission::new_disabled();
        let mut tag_mapping = UniKeyMap::default();
        let root_idx = tag_mapping.insert(root_tag);
        let mut nodes = Nodes::default();
        let mut debug_info = NodeDebugInfo::new(root_tag, root_default_perm, span);
        debug_info.add_name("root of the allocation");
        nodes.insert(
            root_idx,
            None,
            root_default_perm,
            // The root may never be skipped: every access is local to it.
            IdempotentForeignAccess::None,
            None,
            debug_info,
        );
        let mut perms = UniValMap::default();
        perms.insert(
            root_idx,
            LocationState::new_accessed(Permission::new_unique(), IdempotentForeignAccess::None),
        );
        let locations =
            RangeMap::new(size, LocationTree { perms, exposed_cache: ExposedCache::default() });
        Tree {
            tag_mapping,
            nodes,
            locations,
            roots: SmallVec::from_slice(&[root_idx]),
            stack: Vec::new(),
        }
    }

    pub(super) fn root_tag(&self) -> BorTag {
        self.nodes.tag(self.roots[0])
    }

    pub(super) fn index_of(&self, tag: BorTag) -> UniIndex {
        match self.tag_mapping.get(&tag) {
            Some(idx) => idx,
            None => panic!("tag {tag:?} is not in this allocation's tree"),
        }
    }

    /// Inserts `new_tag` under `parent` (`None`: a wildcard pointer, which makes it a new
    /// wildcard root). `inside_perms` are the states of `base_offset..base_offset + size`,
    /// `outside_perm` the default of every other location. Miri: `Tree::new_child`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new_child(
        &mut self,
        base_offset: u64,
        parent: Option<BorTag>,
        new_tag: BorTag,
        inside_perms: &RangeMap<LocationState>,
        outside_perm: Permission,
        protector: Option<ProtectorKind>,
        span: SiteId,
    ) {
        let protected = protector.is_some();
        let idx = self.tag_mapping.insert(new_tag);
        let parent_idx = parent.map(|parent| self.index_of(parent));
        assert!(outside_perm.is_initial());
        assert!(!outside_perm.is_unique());

        let default_strongest_idempotent = outside_perm.strongest_idempotent_foreign_access(protected);
        self.nodes.insert(
            idx,
            parent_idx,
            outside_perm,
            default_strongest_idempotent,
            protector,
            NodeDebugInfo::new(new_tag, outside_perm, span),
        );
        match parent_idx {
            Some(parent_idx) => self.nodes.children[parent_idx.index()].push(idx),
            // A new tag is greater than every earlier one, so `roots` stays sorted.
            None => self.roots.push(idx),
        }

        let mut min_sifa = default_strongest_idempotent;
        for (Range { start, end }, &perm) in inside_perms.iter_all() {
            assert!(perm.permission().is_initial());
            assert_eq!(
                perm.idempotent_foreign_access(),
                perm.permission().strongest_idempotent_foreign_access(protected)
            );
            min_sifa = cmp::min(min_sifa, perm.idempotent_foreign_access());
            for (_range, loc) in self.locations.iter_mut(start + base_offset, end - start) {
                loc.perms.insert(idx, perm);
            }
        }

        // The new node is not exposed and has no children: the exposure cache's zero counts
        // are right. A wildcard parent tracks no SIFA.
        if let Some(parent_idx) = parent_idx {
            self.update_idempotent_foreign_access_after_retag(parent_idx, min_sifa);
        }
    }

    /// Restores "a child's SIFA is at least its parent's" after a retag by weakening the SIFA
    /// of `current` and its parents to `strongest_allowed`. Miri:
    /// `Tree::update_idempotent_foreign_access_after_retag`.
    fn update_idempotent_foreign_access_after_retag(
        &mut self,
        mut current: UniIndex,
        strongest_allowed: IdempotentForeignAccess,
    ) {
        if strongest_allowed == IdempotentForeignAccess::Write {
            return;
        }
        loop {
            let mut any_change = false;
            for (_range, loc) in self.locations.iter_mut_all() {
                if let Some(perm) = loc.perms.get_mut(current) {
                    let mut sifa = perm.idempotent_foreign_access();
                    if sifa.ensure_no_stronger_than(strongest_allowed) {
                        perm.set_idempotent_foreign_access(sifa);
                        any_change = true;
                    }
                }
            }
            any_change |= self.nodes.default_initial_idempotent_foreign_access[current.index()]
                .ensure_no_stronger_than(strongest_allowed);
            if !any_change {
                break;
            }
            match self.nodes.parent(current) {
                Some(next) => current = next,
                None => break,
            }
        }
    }

    /// A deallocation: a write through `prov` to the whole range, and then UB if any tag of
    /// the allocation is strongly protected on an accessed location that is not a `Cell`.
    /// Miri: `Tree::dealloc`.
    pub(super) fn dealloc(
        &mut self,
        prov: Option<BorTag>,
        access_range: Span,
        alloc_id: AllocId,
        span: SiteId,
    ) -> Result<(), AliasError> {
        self.perform_access(prov, access_range, AccessKind::Write, AccessCause::Dealloc, alloc_id, span)?;

        let start_idx = prov.map(|tag| self.index_of(tag));
        let Tree { nodes, locations, roots, stack, .. } = self;
        for (loc_range, loc) in locations.iter_mut(access_range.start, access_range.len) {
            let diagnostics = DiagnosticInfo {
                alloc_id,
                span,
                transition_range: loc_range,
                access_range: Some(access_range),
                access_cause: AccessCause::Dealloc,
            };
            let mut check_tree = |idx: UniIndex| {
                TreeVisitor { nodes: &mut *nodes, data: &mut *loc, stack: &mut *stack }
                    .traverse_this_parents_children_other(
                        idx,
                        |_| ContinueTraversal::Recurse,
                        |args: NodeAppArgs<'_, LocationTree>| {
                            let perm = args
                                .data
                                .perms
                                .get(args.idx)
                                .copied()
                                .unwrap_or_else(|| args.nodes.default_location_state(args.idx));
                            // A `Cell` doesn't stop a deallocation (`unsafe_cell_deallocate`,
                            // rust-lang/rust#55005), and only accessed locations are protected
                            // (miri#4579), counting the write just done.
                            if args.nodes.protector[args.idx.index()]
                                == Some(ProtectorKind::StrongProtector)
                                && !perm.permission().is_cell()
                                && perm.accessed()
                            {
                                Err(build_error(
                                    TransitionError::ProtectedDealloc,
                                    &diagnostics,
                                    &args.nodes.debug[args.idx.index()],
                                    start_idx.map(|idx| &args.nodes.debug[idx.index()]),
                                ))
                            } else {
                                Ok(())
                            }
                        },
                    )
            };
            // The accessed tag's own tree first, so the error names the closest tag; then the
            // others, last first, so no parent comes before its child.
            let accessed_root = start_idx.map(&mut check_tree).transpose()?;
            for &root in roots.iter().rev() {
                if Some(root) == accessed_root {
                    continue;
                }
                check_tree(root)?;
            }
        }
        Ok(())
    }

    /// Applies an access through `prov` (`None`: wildcard) to every location of
    /// `access_range` on every tag. Miri: `Tree::perform_access`.
    pub(super) fn perform_access(
        &mut self,
        prov: Option<BorTag>,
        access_range: Span,
        access_kind: AccessKind,
        access_cause: AccessCause,
        alloc_id: AllocId,
        span: SiteId,
    ) -> Result<(), AliasError> {
        #[cfg(test)]
        if self.roots.len() > 1 || prov.is_none() {
            self.verify_wildcard_consistency();
        }
        let source_idx = prov.map(|tag| self.index_of(tag));
        let Tree { nodes, locations, roots, stack, .. } = self;
        for (loc_range, loc) in locations.iter_mut(access_range.start, access_range.len) {
            let diagnostics = DiagnosticInfo {
                access_cause,
                access_range: Some(access_range),
                alloc_id,
                span,
                transition_range: loc_range,
            };
            loc.perform_access(
                roots,
                nodes,
                stack,
                source_idx,
                access_kind,
                ChildrenVisitMode::VisitChildrenOfAccessed,
                &diagnostics,
                None,
            )?;
        }
        Ok(())
    }

    /// The access when `tag`'s protector ends: only on locations it accessed, invisible to its
    /// children, a write where it is `Unique` and a read elsewhere. Miri:
    /// `Tree::perform_protector_end_access`.
    pub(super) fn perform_protector_end_access(
        &mut self,
        tag: BorTag,
        alloc_id: AllocId,
        span: SiteId,
    ) -> Result<(), AliasError> {
        // No `verify_wildcard_consistency` here, unlike Miri: between the protector ends of
        // one frame, a released exposed tag is still marked protected but already counted
        // unprotected (`Borrows::pop_frame`), which the recount would take for a mismatch.
        let source_idx = self.index_of(tag);
        let min_exposed_child = if self.roots.len() > 1 {
            LocationTree::get_min_exposed_child(source_idx, &self.nodes)
        } else {
            None
        };
        // Not visible to children: `returned_mut_is_usable` in `tests/pass/tree_borrows`.
        let Tree { nodes, locations, roots, stack, .. } = self;
        for (loc_range, loc) in locations.iter_mut_all() {
            if let Some(p) = loc.perms.get(source_idx)
                && let Some(access_kind) = p.permission().associated_access()
                && p.accessed()
            {
                let diagnostics = DiagnosticInfo {
                    access_cause: AccessCause::FnExit(access_kind),
                    access_range: None,
                    alloc_id,
                    span,
                    transition_range: loc_range,
                };
                loc.perform_access(
                    roots,
                    nodes,
                    stack,
                    Some(source_idx),
                    access_kind,
                    ChildrenVisitMode::SkipChildrenOfAccessed,
                    &diagnostics,
                    min_exposed_child,
                )?;
            }
        }
        Ok(())
    }
}

/// The GC. Miri: `tree.rs`, "Integration with the BorTag garbage collector".
impl Tree {
    /// Drops every node without children whose tag is not live, and merges a dead node into
    /// its only child where that is sound; never the roots. Miri:
    /// `Tree::remove_unreachable_tags`.
    pub(super) fn remove_unreachable_tags(&mut self, live: &impl Fn(BorTag) -> bool) {
        for i in 0..self.roots.len() {
            self.remove_useless_children(self.roots[i], live);
        }
        self.locations.merge_adjacent_thorough();
    }

    /// Miri: `Tree::is_useless`.
    fn is_useless(&self, idx: UniIndex, live: &impl Fn(BorTag) -> bool) -> bool {
        self.nodes.children(idx).is_empty() && !live(self.nodes.tag(idx))
    }

    /// The only child of `idx`, if `idx` may be replaced by it. Protected nodes are live, so
    /// `idx` is not protected. Miri: `Tree::can_be_replaced_by_single_child`.
    fn can_be_replaced_by_single_child(
        &self,
        idx: UniIndex,
        live: &impl Fn(BorTag) -> bool,
    ) -> Option<UniIndex> {
        let [child_idx] = self.nodes.children(idx)[..] else { return None };
        // Never the root: it is also each allocation's root tag.
        if live(self.nodes.tag(idx)) || self.nodes.parent(idx).is_none() {
            return None;
        }
        for (_range, loc) in self.locations.iter_all() {
            let parent_perm = loc
                .perms
                .get(idx)
                .map_or(self.nodes.default_initial_perm[idx.index()], |x| x.permission());
            let child_perm = loc
                .perms
                .get(child_idx)
                .map_or(self.nodes.default_initial_perm[child_idx.index()], |x| x.permission());
            if !parent_perm.can_be_replaced_by_child(child_perm) {
                return None;
            }
        }
        Some(child_idx)
    }

    /// Removes a node from every table (its children, if any, were moved elsewhere). Miri:
    /// `Tree::remove_useless_node`.
    fn remove_useless_node(&mut self, this: UniIndex) {
        for (_range, loc) in self.locations.iter_mut_all() {
            loc.perms.remove(this);
            loc.exposed_cache.remove(this);
        }
        let tag = self.nodes.remove(this);
        self.tag_mapping.remove(&tag);
    }

    /// Post-order over the subtree of `root`, dropping useless children and merging dead
    /// nodes into their only child. A dead node with several children stays: merging
    /// `root - parent(Frozen) - child(Reserved)` into `root - child` would allow writes.
    /// Miri: `Tree::remove_useless_children`.
    fn remove_useless_children(&mut self, root: UniIndex, live: &impl Fn(BorTag) -> bool) {
        let mut stack = vec![(root, 0usize)];
        while let Some((tag, nth_child)) = stack.last_mut() {
            let tag = *tag;
            if *nth_child < self.nodes.children(tag).len() {
                let next_child = self.nodes.children(tag)[*nth_child];
                *nth_child += 1;
                stack.push((next_child, 0));
                continue;
            }
            let mut children_of_node = mem::take(&mut self.nodes.children[tag.index()]);
            children_of_node.retain(|idx| {
                if self.is_useless(*idx, live) {
                    self.remove_useless_node(*idx);
                    false
                } else {
                    if let Some(nextchild) = self.can_be_replaced_by_single_child(*idx, live) {
                        self.remove_useless_node(*idx);
                        self.nodes.set_parent(nextchild, tag);
                        *idx = nextchild;
                    }
                    true
                }
            });
            self.nodes.children[tag.index()] = children_of_node;
            stack.pop();
        }
    }

    /// The tags the GC must keep whatever the program holds: the roots, and every exposed
    /// tag an access could still come through. Miri: `impl VisitProvenance for Tree`.
    pub(super) fn visit_provenance(&self, visit: &mut impl FnMut(BorTag)) {
        for &id in &self.roots {
            visit(self.nodes.tag(id));
        }
        for idx in self.nodes.iter() {
            if self.nodes.is_exposed(idx) {
                visit(self.nodes.tag(idx));
            }
        }
    }
}

impl LocationTree {
    /// The smallest exposed tag in the subtree of `root`. Miri:
    /// `LocationTree::get_min_exposed_child`.
    fn get_min_exposed_child(root: UniIndex, nodes: &Nodes) -> Option<BorTag> {
        let mut stack = vec![root];
        let mut min_tag: Option<BorTag> = None;
        while let Some(idx) = stack.pop() {
            let tag = nodes.tag(idx);
            if min_tag.is_some_and(|min| min < tag) {
                continue;
            }
            stack.extend_from_slice(nodes.children(idx));
            if nodes.is_exposed(idx) {
                min_tag = match min_tag {
                    Some(prev) if prev < tag => Some(prev),
                    _ => Some(tag),
                };
            }
        }
        min_tag
    }

    /// An access at this location: a normal access on the tree of `access_source`, and a
    /// wildcard access on every other tree. Miri: `LocationTree::perform_access`.
    #[allow(clippy::too_many_arguments)]
    fn perform_access(
        &mut self,
        roots: &[UniIndex],
        nodes: &mut Nodes,
        stack: &mut VisitStack,
        access_source: Option<UniIndex>,
        access_kind: AccessKind,
        visit_children: ChildrenVisitMode,
        diagnostics: &DiagnosticInfo,
        min_exposed_child: Option<BorTag>,
    ) -> Result<(), AliasError> {
        let accessed_root = if let Some(idx) = access_source {
            Some(self.perform_normal_access(idx, nodes, stack, access_kind, visit_children, diagnostics)?)
        } else {
            // Only a protector's end skips children, and it always has a tag.
            assert!(matches!(visit_children, ChildrenVisitMode::VisitChildrenOfAccessed));
            None
        };
        let accessed_root_tag = accessed_root.map(|idx| nodes.tag(idx));
        for (i, &root) in roots.iter().enumerate() {
            let tag = nodes.tag(root);
            // At a protector's end the children of the accessed tag are skipped, and wildcard
            // subtrees whose root is greater than its smallest exposed child may be among
            // them; roots are sorted, so stop at the first.
            if matches!(visit_children, ChildrenVisitMode::SkipChildrenOfAccessed)
                && let Some(min_exposed_child) = min_exposed_child
                && tag > min_exposed_child
            {
                break;
            }
            if Some(root) == accessed_root {
                continue;
            }
            // Only nodes that are parents of the accessed node can see a local access, and in
            // other trees those are parents of `accessed_root` too, which is smaller.
            self.perform_wildcard_access(
                root,
                access_source,
                accessed_root_tag,
                nodes,
                stack,
                access_kind,
                diagnostics,
                i != 0,
            )?;
        }
        Ok(())
    }

    /// A normal access on the tree containing `access_source`; returns its root. Miri:
    /// `LocationTree::perform_normal_access`.
    fn perform_normal_access(
        &mut self,
        access_source: UniIndex,
        nodes: &mut Nodes,
        stack: &mut VisitStack,
        access_kind: AccessKind,
        visit_children: ChildrenVisitMode,
        diagnostics: &DiagnosticInfo,
    ) -> Result<UniIndex, AliasError> {
        let node_skipper = |args: &NodeAppArgs<'_, LocationTree>| -> ContinueTraversal {
            let old_state = args
                .data
                .perms
                .get(args.idx)
                .copied()
                .unwrap_or_else(|| args.nodes.default_location_state(args.idx));
            old_state.skip_if_known_noop(access_kind, args.rel_pos)
        };
        let node_app = |args: NodeAppArgs<'_, LocationTree>| -> Result<(), AliasError> {
            let default = args.nodes.default_location_state(args.idx);
            let protected = args.nodes.is_protected(args.idx);
            let LocationTree { perms, exposed_cache } = args.data;
            let state = perms.or_insert(args.idx, default);
            state
                .perform_transition(
                    args.idx,
                    args.nodes,
                    exposed_cache,
                    access_kind,
                    args.rel_pos,
                    protected,
                    diagnostics,
                )
                .map_err(|error_kind| {
                    build_error(
                        error_kind,
                        diagnostics,
                        &args.nodes.debug[args.idx.index()],
                        Some(&args.nodes.debug[access_source.index()]),
                    )
                })
        };
        let visitor = TreeVisitor { nodes, data: self, stack };
        match visit_children {
            ChildrenVisitMode::VisitChildrenOfAccessed => {
                visitor.traverse_this_parents_children_other(access_source, node_skipper, node_app)
            }
            ChildrenVisitMode::SkipChildrenOfAccessed => {
                visitor.traverse_nonchildren(access_source, node_skipper, node_app)
            }
        }
    }

    /// A wildcard access on the tree of `root`, each node's relatedness taken from the
    /// exposure cache. `max_local_tag`: the access is local only to nodes up to that tag.
    /// Children go before parents so that exposed nodes losing permissions further down are
    /// seen by their parents (`cross_tree_update_older_invalid_exposed2.rs`). Miri:
    /// `LocationTree::perform_wildcard_access`.
    #[allow(clippy::too_many_arguments)]
    fn perform_wildcard_access(
        &mut self,
        root: UniIndex,
        access_source: Option<UniIndex>,
        max_local_tag: Option<BorTag>,
        nodes: &mut Nodes,
        stack: &mut VisitStack,
        access_kind: AccessKind,
        diagnostics: &DiagnosticInfo,
        is_wildcard_tree: bool,
    ) -> Result<(), AliasError> {
        let get_relatedness = |idx: UniIndex, tag: BorTag, loc: &LocationTree| {
            let only_foreign = max_local_tag.is_some_and(|max_local_tag| max_local_tag < tag);
            loc.exposed_cache.access_relatedness(root, idx, access_kind, is_wildcard_tree, only_foreign)
        };
        let mut has_valid_exposed = false;
        TreeVisitor { data: self, nodes, stack }.traverse_children_this(
            root,
            |args| -> ContinueTraversal {
                let old_state = args
                    .data
                    .perms
                    .get(args.idx)
                    .copied()
                    .unwrap_or_else(|| args.nodes.default_location_state(args.idx));
                if let Some(relatedness) =
                    get_relatedness(args.idx, args.nodes.tag(args.idx), &*args.data)
                    && let Some(relatedness) = relatedness.to_relatedness()
                {
                    old_state.skip_if_known_noop(access_kind, relatedness)
                } else {
                    ContinueTraversal::Recurse
                }
            },
            |args| -> Result<(), AliasError> {
                let tag = args.nodes.tag(args.idx);
                let protected = args.nodes.is_protected(args.idx);
                // Every wildcard root allows a foreign access, so this only fails in the main
                // tree.
                let Some(wildcard_relatedness) = get_relatedness(args.idx, tag, &*args.data) else {
                    return Err(no_valid_exposed_references_error(diagnostics));
                };
                let default = args.nodes.default_location_state(args.idx);
                let LocationTree { perms, exposed_cache } = args.data;
                let perm = perms.or_insert(args.idx, default);
                if args.nodes.is_exposed(args.idx)
                    && perm.permission().strongest_allowed_local_access(protected).allows(access_kind)
                    && max_local_tag.is_none_or(|max_local_tag| max_local_tag >= tag)
                {
                    has_valid_exposed = true;
                }
                // Not knowing whether it is local or foreign, apply nothing here (an
                // imprecision Miri has too) but go on to the children.
                let Some(relatedness) = wildcard_relatedness.to_relatedness() else {
                    return Ok(());
                };
                perm.perform_transition(
                    args.idx,
                    args.nodes,
                    exposed_cache,
                    access_kind,
                    relatedness,
                    protected,
                    diagnostics,
                )
                .map_err(|trans| {
                    build_error(
                        trans,
                        diagnostics,
                        &args.nodes.debug[args.idx.index()],
                        access_source.map(|idx| &args.nodes.debug[idx.index()]),
                    )
                })
            },
        )?;
        // No exposed node allowing this access: in the main tree, which has no ancestors to
        // be foreign to, that is UB.
        if !has_valid_exposed && !is_wildcard_tree {
            return Err(no_valid_exposed_references_error(diagnostics));
        }
        Ok(())
    }
}

/// Where an access is relative to a node. Miri: `tree.rs`, `AccessRelatedness`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum AccessRelatedness {
    /// Through the node itself or one of its children.
    LocalAccess,
    /// Through an ancestor, a sibling, a cousin.
    ForeignAccess,
}

impl AccessRelatedness {
    pub(super) fn is_foreign(self) -> bool {
        matches!(self, AccessRelatedness::ForeignAccess)
    }
}
