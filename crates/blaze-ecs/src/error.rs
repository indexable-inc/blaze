//! Typed errors for `blaze-ecs` (`errors-name-the-thing`).

use core::fmt;

use blaze_own::AllocFailure;

use super::id::{ComponentId, Entity, Id, TableId};

/// Typed error returned by `blaze-ecs` operations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EcsError {
    /// Referenced an entity that is null, out of bounds, or has a stale generation.
    DeadEntity(Entity),
    /// Attempted to register a Rust type that was already registered under a component ID.
    ComponentAlreadyRegistered(ComponentId),
    /// Referenced a component ID not registered in this world.
    UnknownComponent(ComponentId),
    /// Rust type `T` does not match the registered type of the component column.
    ComponentTypeMismatch { component: ComponentId },
    /// Expected a component on a table or entity that does not possess it.
    MissingComponent(ComponentId),
    /// Requested simultaneous mutable or conflicting borrows of the same component column.
    AliasedComponentAccess(ComponentId),
    /// Query requested conflicting access to the same ID (e.g. `(&mut T, &T)` or `(&mut T, &mut T)`).
    ConflictingAccess { id: Id },
    /// Adding `(relation, target)` to `entity` would introduce a cycle in a hierarchy/inheritance graph.
    HierarchyCycle { entity: Entity, relation: Entity, target: Entity },
    /// Used the `WILDCARD` sentinel where a concrete entity or relation was required.
    InvalidWildcardUse,
    /// Entity name was already bound to another live entity.
    DuplicateEntityName,
    /// Table ID was out of bounds.
    InvalidTable(TableId),
    /// Row index was out of bounds for its table.
    RowOutOfBounds { table: TableId, row: usize, len: usize },
    /// Entity capacity (`u32::MAX`) exceeded.
    EntityCapacityExceeded,
    /// Component registry capacity (`u32::MAX`) exceeded.
    ComponentCapacityExceeded,
    /// System declared conflicting access to the same ID (e.g., duplicate write or read+write).
    SystemAccessConflict { system: &'static str, id: Id },
    /// Graph traversal exceeded the world's bounded step cap.
    TraversalBoundExceeded { limit: usize },
    /// Caller-provided allocator refused an allocation.
    Alloc(AllocFailure),
}

impl From<AllocFailure> for EcsError {
    #[inline]
    fn from(err: AllocFailure) -> Self {
        Self::Alloc(err)
    }
}

impl fmt::Display for EcsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::DeadEntity(e) => write!(f, "dead or stale entity {e}"),
            Self::ComponentAlreadyRegistered(cid) => {
                write!(f, "component type already registered as {cid:?}")
            }
            Self::UnknownComponent(cid) => write!(f, "unknown component {cid:?}"),
            Self::ComponentTypeMismatch { component } => {
                write!(f, "component type mismatch for {component:?}")
            }
            Self::MissingComponent(cid) => write!(f, "missing component {cid:?}"),
            Self::AliasedComponentAccess(cid) => {
                write!(f, "aliased mutable access requested for component {cid:?}")
            }
            Self::ConflictingAccess { id } => {
                write!(f, "conflicting query access requested for {id:?}")
            }
            Self::HierarchyCycle { entity, relation, target } => {
                write!(f, "hierarchy cycle adding ({relation}, {target}) to {entity}")
            }
            Self::InvalidWildcardUse => {
                write!(f, "wildcard entity may only appear in query or observer patterns")
            }
            Self::DuplicateEntityName => write!(f, "entity name is already in use"),
            Self::InvalidTable(tid) => write!(f, "invalid table {tid:?}"),
            Self::RowOutOfBounds { table, row, len } => {
                write!(f, "row {row} out of bounds for {table:?} of length {len}")
            }
            Self::EntityCapacityExceeded => write!(f, "entity slot capacity exceeded"),
            Self::ComponentCapacityExceeded => write!(f, "component registry capacity exceeded"),
            Self::SystemAccessConflict { system, id } => {
                write!(f, "system `{system}` declared conflicting access to {id:?}")
            }
            Self::TraversalBoundExceeded { limit } => {
                write!(f, "graph traversal exceeded bound {limit}")
            }
            Self::Alloc(err) => write!(f, "allocation failed: {err}"),
        }
    }
}

impl core::error::Error for EcsError {}
