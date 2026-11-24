//! Queries, incremental [`CachedQuery`], wildcard pair matching, and cascade hierarchy ordering.

use core::alloc::Allocator;

use blaze_own::Vec as OwnVec;

use super::error::EcsError;
use super::id::{ComponentId, Entity, Id, TableId};
use super::table::Table;

/// Single filter term in a [`QuerySpec`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QueryTerm {
    /// Table must contain an ID matching this pattern (exact or wildcard pair).
    With(Id),
    /// Table must NOT contain any ID matching this pattern (exact or wildcard pair).
    Without(Id),
}

/// Declarative Flecs-style query specification over components, tags, exact/wildcard pairs,
/// prefab filtering, and optional hierarchical cascade ordering.
pub struct QuerySpec<A: Allocator + Copy> {
    pub(crate) terms: OwnVec<QueryTerm, A>,
    pub(crate) cascade_rel: Option<Entity>,
    pub(crate) include_prefabs: bool,
}

impl<A: Allocator + Copy> QuerySpec<A> {
    /// Create an empty query specification (excludes `PREFAB` entities by default).
    #[inline]
    pub fn new_in(alloc: A) -> Self {
        Self { terms: OwnVec::new_in(alloc), cascade_rel: None, include_prefabs: false }
    }

    /// Require entities matching `id` (may be a component, tag, exact pair, or wildcard pair).
    pub fn with(mut self, id: Id) -> Result<Self, EcsError> {
        self.terms.try_push(QueryTerm::With(id)).map_err(|r| EcsError::Alloc(r.why))?;
        Ok(self)
    }

    /// Exclude entities matching `id` (may be a component, tag, exact pair, or wildcard pair).
    pub fn without(mut self, id: Id) -> Result<Self, EcsError> {
        self.terms.try_push(QueryTerm::Without(id)).map_err(|r| EcsError::Alloc(r.why))?;
        Ok(self)
    }

    /// Require data-bearing component `cid`.
    #[inline]
    pub fn with_component(self, cid: ComponentId) -> Result<Self, EcsError> {
        self.with(Id::Component(cid))
    }

    /// Exclude data-bearing component `cid`.
    #[inline]
    pub fn without_component(self, cid: ComponentId) -> Result<Self, EcsError> {
        self.without(Id::Component(cid))
    }

    /// Require tag `tag`.
    #[inline]
    pub fn with_tag(self, tag: Entity) -> Result<Self, EcsError> {
        self.with(Id::Tag(tag))
    }

    /// Exclude tag `tag`.
    #[inline]
    pub fn without_tag(self, tag: Entity) -> Result<Self, EcsError> {
        self.without(Id::Tag(tag))
    }

    /// Require pair `(relation, target)` — either side may be `world.wildcard()`.
    #[inline]
    pub fn with_pair(self, relation: Entity, target: Entity) -> Result<Self, EcsError> {
        self.with(Id::Pair(relation, target))
    }

    /// Exclude pair `(relation, target)` — either side may be `world.wildcard()`.
    #[inline]
    pub fn without_pair(self, relation: Entity, target: Entity) -> Result<Self, EcsError> {
        self.without(Id::Pair(relation, target))
    }

    /// Enable topological parent-before-child cascade traversal over `relation` (typically
    /// `world.child_of()`).
    #[inline]
    pub fn cascade(mut self, relation: Entity) -> Self {
        self.cascade_rel = Some(relation);
        self
    }

    /// Control whether `PREFAB`-tagged entities are included (defaults to `false`, matching
    /// Flecs 4.2).
    #[inline]
    pub fn include_prefabs(mut self, include: bool) -> Self {
        self.include_prefabs = include;
        self
    }

    /// Configured filter terms.
    #[inline]
    pub fn terms(&self) -> &[QueryTerm] {
        self.terms.as_slice()
    }

    /// Configured cascade relation, if any.
    #[inline]
    pub fn cascade_relation(&self) -> Option<Entity> {
        self.cascade_rel
    }

    /// Whether prefab entities are included.
    #[inline]
    pub fn includes_prefabs(&self) -> bool {
        self.include_prefabs
    }

    /// Evaluate whether `table` matches this specification.
    pub fn matches_table(&self, table: &Table<A>, prefab_tag: Entity, wildcard: Entity) -> bool
    where
        A: 'static,
    {
        if !self.include_prefabs && table.has_id(Id::Tag(prefab_tag)) {
            return false;
        }
        for &term in self.terms.as_slice() {
            match term {
                QueryTerm::With(pattern) => {
                    if !table.matches_id_pattern(pattern, wildcard) {
                        return false;
                    }
                }
                QueryTerm::Without(pattern) => {
                    if table.matches_id_pattern(pattern, wildcard) {
                        return false;
                    }
                }
            }
        }
        true
    }
}

/// Incrementally cached archetype query.
///
/// Tracks `last_table_count` so syncing against a `World` only inspects newly created tables —
/// achieving O(1) steady-state frame cost when no new archetypes are introduced.
pub struct CachedQuery<A: Allocator + Copy> {
    spec: QuerySpec<A>,
    matched_tables: OwnVec<TableId, A>,
    last_table_count: usize,
}

impl<A: Allocator + Copy> CachedQuery<A> {
    /// Construct a new cached query wrapping `spec`.
    #[inline]
    pub fn new(spec: QuerySpec<A>, alloc: A) -> Self {
        Self { spec, matched_tables: OwnVec::new_in(alloc), last_table_count: 0 }
    }

    /// Access the underlying query specification.
    #[inline]
    pub fn spec(&self) -> &QuerySpec<A> {
        &self.spec
    }

    /// Cached matched table IDs from the most recent sync.
    #[inline]
    pub fn matched_table_ids(&self) -> &[TableId] {
        self.matched_tables.as_slice()
    }

    /// Number of world tables already inspected by this cached query.
    #[inline]
    pub fn synced_table_count(&self) -> usize {
        self.last_table_count
    }

    /// Incrementally match any tables added since the last sync. Returns the number of newly
    /// scanned tables.
    pub(crate) fn sync(
        &mut self,
        tables: &[Table<A>],
        prefab_tag: Entity,
        wildcard: Entity,
    ) -> Result<usize, EcsError>
    where
        A: 'static,
    {
        if self.last_table_count >= tables.len() {
            return Ok(0);
        }
        let new_slice = tables.get(self.last_table_count..).unwrap_or(&[]);
        let scanned = new_slice.len();
        for table in new_slice {
            if self.spec.matches_table(table, prefab_tag, wildcard) {
                self.matched_tables.try_push(table.id()).map_err(|r| EcsError::Alloc(r.why))?;
            }
        }
        self.last_table_count = tables.len();
        Ok(scanned)
    }
}
