//! Generational entities, typed component/table IDs, unified Flecs-style `Id`s, and built-in
//! relations/traits (`ChildOf`, `IsA`, `Prefab`, `Wildcard`, `Exclusive`, `Transitive`,
//! `Symmetric`, `OnDeleteCascade`).

use core::fmt;

/// Lossless `u32 -> usize` index conversion (saturates to `usize::MAX` on 16-bit targets).
#[inline]
pub(crate) fn u32_to_usize(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

/// Lossless `usize -> u32` index conversion (`None` if `v > u32::MAX`).
#[inline]
pub(crate) fn usize_to_u32(v: usize) -> Option<u32> {
    u32::try_from(v).ok()
}

/// 64-bit generational entity handle packing `(slot: u32, generation: u32)`.
///
/// Live entities always have `generation >= 1`, so [`Entity::NULL`] (`0`) is never a valid live
/// entity.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Entity(u64);

impl Entity {
    /// Sentinel null entity (slot 0, generation 0).
    pub const NULL: Self = Self(0);

    /// Construct an entity handle from its 32-bit slot index and 32-bit generation.
    #[inline]
    pub const fn from_parts(slot: u32, generation: u32) -> Self {
        let [s0, s1, s2, s3] = slot.to_le_bytes();
        let [g0, g1, g2, g3] = generation.to_le_bytes();
        Self(u64::from_le_bytes([s0, s1, s2, s3, g0, g1, g2, g3]))
    }

    /// Reconstruct from a raw 64-bit value.
    #[inline]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Raw packed 64-bit representation.
    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// 32-bit slot index (low 32 bits).
    #[inline]
    pub const fn slot(self) -> u32 {
        let [s0, s1, s2, s3, _, _, _, _] = self.0.to_le_bytes();
        u32::from_le_bytes([s0, s1, s2, s3])
    }

    /// 32-bit generation counter (high 32 bits).
    #[inline]
    pub const fn generation(self) -> u32 {
        let [_, _, _, _, g0, g1, g2, g3] = self.0.to_le_bytes();
        u32::from_le_bytes([g0, g1, g2, g3])
    }

    /// Whether this entity handle is [`Entity::NULL`].
    #[inline]
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Debug for Entity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Entity({}v{})", self.slot(), self.generation())
    }
}

impl fmt::Display for Entity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "e{}v{}", self.slot(), self.generation())
    }
}

/// Typed component registry identifier.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct ComponentId(pub u32);

impl fmt::Debug for ComponentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Comp#{}", self.0)
    }
}

/// Typed archetype table identifier (`TableId(0)` is always the empty archetype).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct TableId(pub u32);

impl fmt::Debug for TableId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Table#{}", self.0)
    }
}

/// Unified Flecs-style archetype ID: a data-bearing component, a zero-sized tag entity, or a
/// typed `(relation, target)` pair.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Id {
    /// Data-bearing component registered in the world.
    Component(ComponentId),
    /// Zero-sized tag entity attached to an entity.
    Tag(Entity),
    /// Directed `(relation, target)` pair attached to an entity.
    Pair(Entity, Entity),
}

impl Id {
    /// Wrap a [`ComponentId`] as an [`Id`].
    #[inline]
    pub const fn component(cid: ComponentId) -> Self {
        Self::Component(cid)
    }

    /// Wrap a tag [`Entity`] as an [`Id`].
    #[inline]
    pub const fn tag(entity: Entity) -> Self {
        Self::Tag(entity)
    }

    /// Construct a `(relation, target)` pair [`Id`].
    #[inline]
    pub const fn pair(relation: Entity, target: Entity) -> Self {
        Self::Pair(relation, target)
    }

    /// Extract the inner [`ComponentId`] if this is a component ID.
    #[inline]
    pub const fn as_component(self) -> Option<ComponentId> {
        match self {
            Self::Component(cid) => Some(cid),
            Self::Tag(_) | Self::Pair(_, _) => None,
        }
    }

    /// Extract `(relation, target)` if this is a pair ID.
    #[inline]
    pub const fn as_pair(self) -> Option<(Entity, Entity)> {
        match self {
            Self::Pair(r, t) => Some((r, t)),
            Self::Component(_) | Self::Tag(_) => None,
        }
    }

    /// Return whether concrete ID `self` matches `pattern`, treating `wildcard` as matching any
    /// relation or target position in a [`Id::Pair`].
    #[inline]
    pub fn matches_pattern(self, pattern: Self, wildcard: Entity) -> bool {
        if self == pattern {
            return true;
        }
        match (self, pattern) {
            (Self::Pair(r, t), Self::Pair(pr, pt)) => {
                (pr == wildcard || pr == r) && (pt == wildcard || pt == t)
            }
            _ => false,
        }
    }
}

/// Deterministic built-in Flecs 4.2 relation and trait entities created on `World::new_in`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Builtins {
    /// Exclusive parent-child hierarchy relation (with `Exclusive` and `OnDeleteCascade`).
    pub child_of: Entity,
    /// Prefab inheritance relation (`(IsA, prefab)`).
    pub is_a: Entity,
    /// Tag marking an entity as a prefab (excluded from default queries).
    pub prefab: Entity,
    /// Wildcard sentinel `*` for queries and observers (`(Rel, WILDCARD)`, `(WILDCARD, Target)`).
    pub wildcard: Entity,
    /// Relation trait: an entity may have at most one target for this relation.
    pub exclusive: Entity,
    /// Relation trait: `(a, rel, b)` and `(b, rel, c)` imply `has_transitive(a, rel, c)`.
    pub transitive: Entity,
    /// Relation trait: adding/removing `(rel, b)` on `a` mirrors `(rel, a)` on `b`.
    pub symmetric: Entity,
    /// Relation trait: despawning `target` cascades despawn to all entities with `(rel, target)`.
    pub on_delete_cascade: Entity,
}
