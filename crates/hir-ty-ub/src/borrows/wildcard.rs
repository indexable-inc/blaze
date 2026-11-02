//! Wildcard provenance: which exposed tags an access through a pointer cast from an integer
//! may have come through. Per location, each node counts the exposed nodes in its subtree
//! that allow reads and writes; an access is local to a node if every such exposed node is
//! below it, foreign if none is, and unknown otherwise.
//!
//! Miri: `tree_borrows/wildcard.rs`.

use super::{
    tree::{AccessRelatedness, Nodes, Tree},
    unimap::{UniIndex, UniValMap},
};
use crate::ids::{AccessKind, BorTag};

/// The strongest access possible. `None < Read < Write`; the derived order is relied on.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub(super) enum WildcardAccessLevel {
    #[default]
    None,
    Read,
    Write,
}

impl WildcardAccessLevel {
    pub(super) fn allows(self, kind: AccessKind) -> bool {
        let required_level = match kind {
            AccessKind::Read => WildcardAccessLevel::Read,
            AccessKind::Write => WildcardAccessLevel::Write,
        };
        required_level <= self
    }
}

/// Where a wildcard access happened relative to a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WildcardAccessRelatedness {
    LocalAccess,
    ForeignAccess,
    EitherAccess,
}

impl WildcardAccessRelatedness {
    pub(super) fn to_relatedness(self) -> Option<AccessRelatedness> {
        match self {
            WildcardAccessRelatedness::LocalAccess => Some(AccessRelatedness::LocalAccess),
            WildcardAccessRelatedness::ForeignAccess => Some(AccessRelatedness::ForeignAccess),
            WildcardAccessRelatedness::EitherAccess => None,
        }
    }
}

/// Per node at one range of locations: where the exposed nodes that may read or write are.
/// Empty (no memory) until the first node is exposed. Miri: `wildcard.rs`, `ExposedCache`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ExposedCache(pub(super) UniValMap<ExposedCacheNode>);

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(super) struct ExposedCacheNode {
    /// Exposed nodes in this subtree that allow writes.
    pub(super) local_writes: u16,
    /// Exposed nodes in this subtree that allow reads.
    pub(super) local_reads: u16,
}

impl ExposedCache {
    /// The relatedness of a wildcard access to node `id` of the tree of `root`; `None` if
    /// that tree has no exposed node allowing the access. In a wildcard subtree an access may
    /// always be foreign (from another tree); `only_foreign` rules out a local one. Miri:
    /// `ExposedCache::access_relatedness`.
    pub(super) fn access_relatedness(
        &self,
        root: UniIndex,
        id: UniIndex,
        kind: AccessKind,
        is_wildcard_tree: bool,
        only_foreign: bool,
    ) -> Option<WildcardAccessRelatedness> {
        let root = self.0.get(root).copied().unwrap_or_default();
        let node = self.0.get(id).copied().unwrap_or_default();
        let (total_num, local_num) = match kind {
            AccessKind::Read => (root.local_reads, node.local_reads),
            AccessKind::Write => (root.local_writes, node.local_writes),
        };
        // One more possible exposed node, foreign to the whole tree, in another tree; not in
        // the root's count, where it would also count as local when `id` is the root.
        let total_num = total_num + u16::from(is_wildcard_tree);
        use WildcardAccessRelatedness::*;
        let relatedness = if total_num == 0 {
            None
        } else {
            Some(if total_num == local_num {
                LocalAccess
            } else if local_num == 0 {
                ForeignAccess
            } else {
                EitherAccess
            })
        };
        if only_foreign {
            match relatedness {
                Some(LocalAccess) => None,
                Some(ForeignAccess) => Some(ForeignAccess),
                Some(EitherAccess) => Some(ForeignAccess),
                None => None,
            }
        } else {
            relatedness
        }
    }

    /// Node `id`, exposed, changed its access level from `from` (`None`: newly exposed) to
    /// `to`; updates its counts and its ancestors'. Miri: `ExposedCache::update_exposure`.
    pub(super) fn update_exposure(
        &mut self,
        nodes: &Nodes,
        id: UniIndex,
        from: WildcardAccessLevel,
        to: WildcardAccessLevel,
    ) {
        if from == to {
            return;
        }
        let mut next_id = Some(id);
        while let Some(id) = next_id {
            let state = self.0.or_insert(id, ExposedCacheNode::default());
            use WildcardAccessLevel::*;
            match (from, to) {
                (None | Read, Write) => state.local_writes += 1,
                (Write, None | Read) => state.local_writes -= 1,
                _ => {}
            }
            match (from, to) {
                (None, Read | Write) => state.local_reads += 1,
                (Read | Write, None) => state.local_reads -= 1,
                _ => {}
            }
            next_id = nodes.parent(id);
        }
    }

    /// Drops a node without children. Miri: `ExposedCache::remove`.
    pub(super) fn remove(&mut self, idx: UniIndex) {
        self.0.remove(idx);
    }
}

impl Tree {
    /// Marks `tag` exposed and counts it at its access level everywhere. Miri:
    /// `Tree::expose_tag`.
    pub(super) fn expose_tag(&mut self, tag: BorTag) {
        let id = self.index_of(tag);
        if self.nodes.is_exposed(id) {
            return;
        }
        self.nodes.exposed[id.index()] = true;
        let protected = self.nodes.is_protected(id);
        let Tree { nodes, locations, .. } = self;
        for (_, loc) in locations.iter_mut_all() {
            let perm = loc
                .perms
                .get(id)
                .map_or_else(|| nodes.default_location_state(id).permission(), |p| p.permission());
            let access_level = perm.strongest_allowed_local_access(protected);
            loc.exposed_cache.update_exposure(nodes, id, WildcardAccessLevel::None, access_level);
        }
    }

    /// `tag`'s protector ended: its access level may have grown. Miri:
    /// `Tree::update_exposure_for_protector_release`.
    pub(super) fn update_exposure_for_protector_release(&mut self, tag: BorTag) {
        let idx = self.index_of(tag);
        if !self.nodes.is_exposed(idx) {
            return;
        }
        let Tree { nodes, locations, .. } = self;
        for (_, loc) in locations.iter_mut_all() {
            let perm = loc
                .perms
                .get(idx)
                .map_or_else(|| nodes.default_location_state(idx).permission(), |p| p.permission());
            let old_access_type = perm.strongest_allowed_local_access(true);
            let access_type = perm.strongest_allowed_local_access(false);
            loc.exposed_cache.update_exposure(nodes, idx, old_access_type, access_type);
        }
    }

    /// Checks the exposure cache against a recount, and that roots are sorted. Miri:
    /// `Tree::verify_wildcard_consistency` (its `expensive-consistency-checks`, on in tests).
    #[cfg(test)]
    pub(super) fn verify_wildcard_consistency(&self) {
        assert!(self.roots.is_sorted_by_key(|idx| self.nodes.tag(*idx)));
        for (_, loc) in self.locations.iter_all() {
            let exposed_cache = &loc.exposed_cache;
            let perms = &loc.perms;
            for id in self.nodes.iter() {
                let state = exposed_cache.0.get(id).copied().unwrap_or_default();
                let exposed_as = if self.nodes.is_exposed(id) {
                    let perm =
                        perms.get(id).copied().unwrap_or_else(|| self.nodes.default_location_state(id));
                    perm.permission().strongest_allowed_local_access(self.nodes.is_protected(id))
                } else {
                    WildcardAccessLevel::None
                };
                let (child_reads, child_writes) = self
                    .nodes
                    .children(id)
                    .iter()
                    .map(|&id| exposed_cache.0.get(id).copied().unwrap_or_default())
                    .fold((0, 0), |acc, wc| (acc.0 + wc.local_reads, acc.1 + wc.local_writes));
                let expected_reads = child_reads + u16::from(exposed_as >= WildcardAccessLevel::Read);
                let expected_writes =
                    child_writes + u16::from(exposed_as >= WildcardAccessLevel::Write);
                assert_eq!(state.local_reads, expected_reads, "local_reads of {:?}", self.nodes.tag(id));
                assert_eq!(
                    state.local_writes,
                    expected_writes,
                    "local_writes of {:?}",
                    self.nodes.tag(id)
                );
            }
        }
    }
}
