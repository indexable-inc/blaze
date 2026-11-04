//! Component traits (Final, Singleton, (OnInstantiate, Inherit), Sparse, ...) declared safely.
//!
//! flecs (4.1.6 and 4.2.0) aborts the process (`flecs_assert_relation_unused`, "cannot change trait ... component is already in use") when a trait is
//! added to a component or relationship that already has instances or pairs, and (debug builds) when it is already queried. Adding a trait twice, or late, was the hazard; [`World::traits`] is the one place that adds traits:
//!
//! * idempotent: a trait the component already has is skipped without touching the world;
//! * safe: a trait that would be added to a component already in use is NOT added (flecs would abort); an error naming the component and
//!   the trait is logged instead and `traits` returns `false`. Declare the trait earlier (host init, before any query or entity uses it).
//!
//! Call it in the same step that registers the component, before any entity or query uses it.

use crate::sys;
use crate::world::{Id, World, pair};

/// A flecs component trait. Resolving the id reads a flecs global, so the enum keeps the `unsafe` out of callers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trait {
    /// No entity may inherit (IsA) from the component.
    Final,
    /// One value per world, stored on the component entity itself.
    Singleton,
    /// `(OnInstantiate, Inherit)`: prefab instances read the prefab's value without copying it.
    Inherit,
    /// `(OnInstantiate, DontInherit)`.
    DontInherit,
    /// `(OnInstantiate, Copy)`: instances copy the prefab's value (the default; `Override` before flecs 4.2).
    Copy,
    /// Sparse storage (stable pointers, no archetype move).
    Sparse,
    /// The relationship does not fragment tables.
    DontFragment,
    /// A relationship with at most one target per entity.
    Exclusive,
    /// `R(a, b)` implies `R(b, a)`: adding one end adds the other.
    Symmetric,
    /// Queries may follow the relationship up (`up`, `cascade`); implies Acyclic.
    Traversable,
    /// `R(a, b)` and `R(b, c)` imply `R(a, c)` in queries; implies Traversable.
    Transitive,
    /// `R(x, x)` holds for every `x` in queries.
    Reflexive,
    /// The relationship forms no cycles.
    Acyclic,
    /// A pair of the relationship never carries data, even when its target is a component.
    PairIsTag,
    /// The entity may only be used as a relationship.
    Relationship,
    /// The entity may only be used as a relationship's target.
    Target,
    /// The entity may only be used as a trait.
    Trait,
    /// On a parent: its children keep creation (or a set) order.
    OrderedChildren,
    CanToggle,
    /// Components may be inherited from (opt-in inheritance for a component that is not a tag).
    Inheritable,
    /// `(With, id)`: adding the component also adds `id`.
    With(Id),
    /// `(OneOf, parent)`: a pair's target must be a child of `parent`.
    OneOf(Id),
    /// `OneOf`: a pair's target must be a child of the relationship itself.
    OneOfSelf,
    /// `(OnDelete, action)`: what deleting the component (or relationship) does to the entities that have it.
    OnDelete(Cleanup),
    /// `(OnDeleteTarget, action)`: what deleting a pair's target does to the entities that have the pair.
    OnDeleteTarget(Cleanup),
}

/// A cleanup action of [`Trait::OnDelete`] and [`Trait::OnDeleteTarget`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cleanup {
    /// Remove the id from the entities that have it (the default for tags and pairs).
    Remove,
    /// Delete the entities that have it (ownership: `ChildOf` is `(OnDeleteTarget, Delete)`).
    Delete,
    /// Abort: deleting it while in use is a bug.
    Panic,
}

impl Cleanup {
    fn id(self) -> Id {
        // SAFETY: immutable flecs globals.
        unsafe {
            match self {
                Cleanup::Remove => sys::EcsRemove,
                Cleanup::Delete => sys::EcsDelete,
                Cleanup::Panic => sys::EcsPanic,
            }
        }
    }
}

impl Trait {
    pub fn id(self) -> Id {
        // SAFETY: flecs trait entity ids are immutable globals of the linked flecs.
        unsafe {
            match self {
                Trait::Final => sys::EcsFinal,
                Trait::Singleton => sys::EcsSingleton,
                Trait::Inherit => pair(sys::EcsOnInstantiate, sys::EcsInherit),
                Trait::DontInherit => pair(sys::EcsOnInstantiate, sys::EcsDontInherit),
                Trait::Copy => pair(sys::EcsOnInstantiate, sys::EcsCopy),
                Trait::Sparse => sys::EcsSparse,
                Trait::DontFragment => sys::EcsDontFragment,
                Trait::Exclusive => sys::EcsExclusive,
                Trait::Symmetric => sys::EcsSymmetric,
                Trait::Traversable => sys::EcsTraversable,
                Trait::Transitive => sys::EcsTransitive,
                Trait::Reflexive => sys::EcsReflexive,
                Trait::Acyclic => sys::EcsAcyclic,
                Trait::PairIsTag => sys::EcsPairIsTag,
                Trait::Relationship => sys::EcsRelationship,
                Trait::Target => sys::EcsTarget,
                Trait::Trait => sys::EcsTrait,
                Trait::OrderedChildren => sys::EcsOrderedChildren,
                Trait::CanToggle => sys::EcsCanToggle,
                Trait::Inheritable => sys::EcsInheritable,
                Trait::With(id) => pair(sys::EcsWith, id),
                Trait::OneOf(id) => pair(sys::EcsOneOf, id),
                Trait::OneOfSelf => sys::EcsOneOf,
                Trait::OnDelete(action) => pair(sys::EcsOnDelete, action.id()),
                Trait::OnDeleteTarget(action) => pair(sys::EcsOnDeleteTarget, action.id()),
            }
        }
    }

    /// Whether flecs lets this trait be added to a component already in use. Only `With` is: the rest abort or are refused.
    pub fn allowed_in_use(self) -> bool {
        matches!(self, Trait::With(_))
    }

    pub fn name(self) -> &'static str {
        match self {
            Trait::Final => "Final",
            Trait::Singleton => "Singleton",
            Trait::Inherit => "(OnInstantiate, Inherit)",
            Trait::DontInherit => "(OnInstantiate, DontInherit)",
            Trait::Copy => "(OnInstantiate, Copy)",
            Trait::Sparse => "Sparse",
            Trait::DontFragment => "DontFragment",
            Trait::Exclusive => "Exclusive",
            Trait::Symmetric => "Symmetric",
            Trait::Traversable => "Traversable",
            Trait::Transitive => "Transitive",
            Trait::Reflexive => "Reflexive",
            Trait::Acyclic => "Acyclic",
            Trait::PairIsTag => "PairIsTag",
            Trait::Relationship => "Relationship",
            Trait::Target => "Target",
            Trait::Trait => "Trait",
            Trait::OrderedChildren => "OrderedChildren",
            Trait::CanToggle => "CanToggle",
            Trait::Inheritable => "Inheritable",
            Trait::With(_) => "(With, ..)",
            Trait::OneOf(_) => "(OneOf, ..)",
            Trait::OneOfSelf => "OneOf",
            Trait::OnDelete(Cleanup::Remove) => "(OnDelete, Remove)",
            Trait::OnDelete(Cleanup::Delete) => "(OnDelete, Delete)",
            Trait::OnDelete(Cleanup::Panic) => "(OnDelete, Panic)",
            Trait::OnDeleteTarget(Cleanup::Remove) => "(OnDeleteTarget, Remove)",
            Trait::OnDeleteTarget(Cleanup::Delete) => "(OnDeleteTarget, Delete)",
            Trait::OnDeleteTarget(Cleanup::Panic) => "(OnDeleteTarget, Panic)",
        }
    }
}

impl World {
    /// Is `comp` in use (an entity has it, a pair with it as relationship exists, or something inherits from it)? Traits can no longer change then.
    pub fn component_in_use(self, comp: Id) -> bool {
        // SAFETY: world live.
        unsafe {
            sys::ecs_id_in_use(self.as_ptr(), comp)
                || sys::ecs_id_in_use(self.as_ptr(), pair(comp, sys::EcsWildcard))
                || sys::ecs_id_in_use(self.as_ptr(), pair(sys::EcsIsA, comp))
        }
    }

    /// Declare `traits` on `comp` (idempotent, never aborts); see the module docs. `true` when every trait is present afterwards.
    pub fn traits(self, comp: Id, traits: &[Trait]) -> bool {
        let mut ok = true;
        for &t in traits {
            let id = t.id();
            if self.has_id(comp, id) {
                continue;
            }
            if !t.allowed_in_use() && self.component_in_use(comp) {
                crate::hooks::log(
                    2,
                    &format!(
                        "ERROR [traits] cannot add trait {} to '{}': the component is already in use (flecs would abort); declare it when the component is registered, before any entity or query uses it",
                        t.name(),
                        self.name(comp).unwrap_or_else(|| comp.to_string())
                    ),
                );
                ok = false;
                continue;
            }
            self.add_id(comp, id);
        }
        ok
    }
}
