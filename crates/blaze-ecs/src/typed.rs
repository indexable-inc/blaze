//! Zero-string, zero-boilerplate typed Flecs 4.2 ergonomic layer:
//! - [`short_type_name`]: zero-allocation type-name shortening respecting generic nesting.
//! - [`Relation`], [`ChildOf`], [`IsA`]: typed relation markers with trait-const flags.
//! - [`Bundle`] and [`CopyBundle`]: tuple component bundles (0..=8 components) with no `<A>` bound.
//! - [`Prefab`], [`PrefabParent`], [`NoParent`]: typed prefab hierarchies resolved iteratively with
//!   zero recursion.
//! - [`EntityMut`]: fluent chainable entity handle.

use core::alloc::Allocator;
use core::any::TypeId;

use super::error::EcsError;
use super::id::Entity;
use super::world::World;

/// Bounded maximum typed prefab inheritance depth (`bounded-progress`).
pub(crate) const MAX_PREFAB_DEPTH: usize = 64;

/// Return the short Rust type name for `T` with leading module path segments stripped while
/// preserving generic argument lists cleanly (e.g. `"my_game::Pos"` -> `"Pos"`,
/// `"foo::Bar<baz::Qux>"` -> `"Bar<baz::Qux>"`).
///
/// Zero heap allocation, zero panics, zero integer casts.
#[inline]
pub fn short_type_name<T: 'static>() -> &'static str {
    short_type_name_str(core::any::type_name::<T>())
}

/// Strip leading module prefixes from a `core::any::type_name` string while respecting nested
/// `<...>`, `(...)`, and `[...]` brackets.
pub(crate) fn short_type_name_str(full: &str) -> &str {
    let bytes = full.as_bytes();
    let mut depth: usize = 0;
    let mut last_sep_end: usize = 0;
    let mut i: usize = 0;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'<' | b'(' | b'[' => {
                depth = depth.saturating_add(1);
                i = i.saturating_add(1);
            }
            b'>' | b')' | b']' => {
                depth = depth.saturating_sub(1);
                i = i.saturating_add(1);
            }
            b':' if depth == 0 && bytes.get(i.saturating_add(1)) == Some(&b':') => {
                i = i.saturating_add(2);
                last_sep_end = i;
            }
            _ => {
                i = i.saturating_add(1);
            }
        }
    }
    full.get(last_sep_end..).unwrap_or(full)
}

// -----------------------------------------------------------------------------
// Typed Relations
// -----------------------------------------------------------------------------

/// Typed Flecs 4.2 relation marker.
///
/// Implement on any zero-sized marker struct (`struct Ally;`) and override the desired Flecs
/// relation trait flags. Calling `world.relate::<R>(entity, target)` lazily spawns and caches the
/// relation entity with its trait tags attached automatically.
pub trait Relation: 'static {
    /// At most one target per entity; adding a new target replaces the previous one.
    const EXCLUSIVE: bool = false;
    /// Adding/removing `(R, target)` on `entity` mirrors `(R, entity)` on `target`.
    const SYMMETRIC: bool = false;
    /// Enables multi-hop `world.has_transitive_relation::<R>(entity, target)` queries.
    const TRANSITIVE: bool = false;
    /// Despawning `target` cascades despawn to all entities holding `(R, target)`.
    const ON_DELETE_CASCADE: bool = false;
}

/// Built-in typed marker for the Flecs `ChildOf` hierarchy relation (`Exclusive` +
/// `OnDeleteCascade`). Maps directly to `world.child_of()`.
pub struct ChildOf;

impl Relation for ChildOf {
    const EXCLUSIVE: bool = true;
    const ON_DELETE_CASCADE: bool = true;
}

/// Built-in typed marker for the Flecs `IsA` prefab inheritance relation. Maps directly to
/// `world.is_a()`.
pub struct IsA;

impl Relation for IsA {}

// -----------------------------------------------------------------------------
// Typed Component Bundles (0..=8 tuples, no allocator generic on trait)
// -----------------------------------------------------------------------------

/// Tuple of components that can be inserted into an entity in one call.
pub trait Bundle {
    /// Insert every component in `self` onto `entity`, lazily registering unknown types.
    fn insert_into<A: Allocator + Copy + 'static>(
        self,
        world: &mut World<A>,
        entity: Entity,
    ) -> Result<(), EcsError>;
}

/// Tuple of `Copy` components registered as copyable so prefab instantiation duplicates them
/// automatically.
pub trait CopyBundle {
    /// Insert every `Copy` component in `self` onto `entity` via [`World::set_copy`].
    fn insert_copy_into<A: Allocator + Copy + 'static>(
        self,
        world: &mut World<A>,
        entity: Entity,
    ) -> Result<(), EcsError>;
}

impl Bundle for () {
    #[inline]
    fn insert_into<A: Allocator + Copy + 'static>(
        self,
        _world: &mut World<A>,
        _entity: Entity,
    ) -> Result<(), EcsError> {
        Ok(())
    }
}

impl CopyBundle for () {
    #[inline]
    fn insert_copy_into<A: Allocator + Copy + 'static>(
        self,
        _world: &mut World<A>,
        _entity: Entity,
    ) -> Result<(), EcsError> {
        Ok(())
    }
}

macro_rules! impl_bundle_tuples {
    ($($idx:ident : $ty:ident),+ $(,)?) => {
        impl<$($ty: 'static),+> Bundle for ($($ty,)+) {
            fn insert_into<A: Allocator + Copy + 'static>(
                self,
                world: &mut World<A>,
                entity: Entity,
            ) -> Result<(), EcsError> {
                let ($($idx,)+) = self;
                $(
                    world.set(entity, $idx)?;
                )+
                Ok(())
            }
        }

        impl<$($ty: Copy + 'static),+> CopyBundle for ($($ty,)+) {
            fn insert_copy_into<A: Allocator + Copy + 'static>(
                self,
                world: &mut World<A>,
                entity: Entity,
            ) -> Result<(), EcsError> {
                let ($($idx,)+) = self;
                $(
                    world.set_copy(entity, $idx)?;
                )+
                Ok(())
            }
        }
    };
}

impl_bundle_tuples!(v0: T0);
impl_bundle_tuples!(v0: T0, v1: T1);
impl_bundle_tuples!(v0: T0, v1: T1, v2: T2);
impl_bundle_tuples!(v0: T0, v1: T1, v2: T2, v3: T3);
impl_bundle_tuples!(v0: T0, v1: T1, v2: T2, v3: T3, v4: T4);
impl_bundle_tuples!(v0: T0, v1: T1, v2: T2, v3: T3, v4: T4, v5: T5);
impl_bundle_tuples!(v0: T0, v1: T1, v2: T2, v3: T3, v4: T4, v5: T5, v6: T6);
impl_bundle_tuples!(v0: T0, v1: T1, v2: T2, v3: T3, v4: T4, v5: T5, v6: T6, v7: T7);

// -----------------------------------------------------------------------------
// Typed Prefabs (zero-recursion iterative descriptor chain)
// -----------------------------------------------------------------------------

/// Root marker for a [`Prefab`] with no parent prefab.
pub struct NoParent;

/// Iterative initialization descriptor for one link in a typed [`Prefab`] inheritance chain.
pub struct PrefabInit<A: Allocator + Copy + 'static> {
    pub(crate) type_id: TypeId,
    pub(crate) short_name: &'static str,
    pub(crate) full_name: &'static str,
    pub(crate) insert: fn(&mut World<A>, Entity) -> Result<(), EcsError>,
    pub(crate) parent: Option<fn() -> PrefabInit<A>>,
}

/// Marker trait for valid [`Prefab::Parent`] types (`NoParent`, `()`, or another [`Prefab`]).
pub trait PrefabParent: 'static {
    /// Return a function pointer producing the parent's [`PrefabInit`], or `None` at the root.
    fn parent_init<A: Allocator + Copy + 'static>() -> Option<fn() -> PrefabInit<A>>;
}

impl PrefabParent for NoParent {
    #[inline]
    fn parent_init<A: Allocator + Copy + 'static>() -> Option<fn() -> PrefabInit<A>> {
        None
    }
}

impl PrefabParent for () {
    #[inline]
    fn parent_init<A: Allocator + Copy + 'static>() -> Option<fn() -> PrefabInit<A>> {
        None
    }
}

/// Typed Flecs 4.2 prefab definition with optional `IsA` inheritance and copyable default
/// components.
pub trait Prefab: 'static {
    /// Parent prefab in the `IsA` hierarchy (`NoParent` or `()` for a root prefab).
    type Parent: PrefabParent;
    /// Tuple of `Copy` components attached to this prefab (auto-registered as copyable).
    type Components: CopyBundle;

    /// Construct the default component values owned directly by this prefab.
    fn components() -> Self::Components;
}

impl<P: Prefab> PrefabParent for P {
    #[inline]
    fn parent_init<A: Allocator + Copy + 'static>() -> Option<fn() -> PrefabInit<A>> {
        Some(prefab_init_for::<P, A>)
    }
}

fn insert_prefab_components<P: Prefab, A: Allocator + Copy + 'static>(
    world: &mut World<A>,
    entity: Entity,
) -> Result<(), EcsError> {
    P::components().insert_copy_into(world, entity)
}

/// Build the non-recursive [`PrefabInit`] descriptor for prefab `P`.
#[inline]
pub(crate) fn prefab_init_for<P: Prefab, A: Allocator + Copy + 'static>() -> PrefabInit<A> {
    PrefabInit {
        type_id: TypeId::of::<P>(),
        short_name: short_type_name::<P>(),
        full_name: core::any::type_name::<P>(),
        insert: insert_prefab_components::<P, A>,
        parent: P::Parent::parent_init::<A>(),
    }
}

// -----------------------------------------------------------------------------
// Fluent EntityMut<'w, A>
// -----------------------------------------------------------------------------

/// Fluent mutable handle to a live [`Entity`] inside a [`World`], enabling chainable component,
/// bundle, tag, hierarchy, naming, and relation operations.
pub struct EntityMut<'w, A: Allocator + Copy + 'static> {
    world: &'w mut World<A>,
    entity: Entity,
}

impl<'w, A: Allocator + Copy + 'static> EntityMut<'w, A> {
    #[inline]
    pub(crate) fn new(world: &'w mut World<A>, entity: Entity) -> Self {
        Self { world, entity }
    }

    /// Extract the underlying generational [`Entity`] handle.
    #[inline]
    pub fn id(&self) -> Entity {
        self.entity
    }

    /// Set (insert or overwrite) component `T`, lazily registering `T` if needed.
    #[inline]
    pub fn with<T: 'static>(self, value: T) -> Result<Self, EcsError> {
        self.world.set(self.entity, value)?;
        Ok(self)
    }

    /// Set (insert or overwrite) `Copy` component `T`, lazily registering `T` as copyable.
    #[inline]
    pub fn with_copy<T: Copy + 'static>(self, value: T) -> Result<Self, EcsError> {
        self.world.set_copy(self.entity, value)?;
        Ok(self)
    }

    /// Insert every component in `bundle` onto this entity.
    #[inline]
    pub fn with_bundle<B: Bundle>(self, bundle: B) -> Result<Self, EcsError> {
        bundle.insert_into(self.world, self.entity)?;
        Ok(self)
    }

    /// Insert every `Copy` component in `bundle` onto this entity via [`World::set_copy`].
    #[inline]
    pub fn with_copy_bundle<B: CopyBundle>(self, bundle: B) -> Result<Self, EcsError> {
        bundle.insert_copy_into(self.world, self.entity)?;
        Ok(self)
    }

    /// Attach typed marker tag `T` (lazily spawns & caches the tag entity).
    #[inline]
    pub fn tag<T: 'static>(self) -> Result<Self, EcsError> {
        self.world.add_tag_typed::<T>(self.entity)?;
        Ok(self)
    }

    /// Remove typed marker tag `T`.
    #[inline]
    pub fn untag<T: 'static>(self) -> Result<Self, EcsError> {
        let _ = self.world.remove_tag_typed::<T>(self.entity)?;
        Ok(self)
    }

    /// Attach `(ChildOf, parent)` (exclusive hierarchy parent with cycle rejection).
    #[inline]
    pub fn child_of(self, parent: Entity) -> Result<Self, EcsError> {
        let rel = self.world.child_of();
        self.world.add_pair(self.entity, rel, parent)?;
        Ok(self)
    }

    /// Attach `(IsA, prefab)` inheritance link.
    #[inline]
    pub fn is_a(self, prefab: Entity) -> Result<Self, EcsError> {
        let rel = self.world.is_a();
        self.world.add_pair(self.entity, rel, prefab)?;
        Ok(self)
    }

    /// Attach typed relation pair `(R, target)` (lazily creates `R` with its trait flags).
    #[inline]
    pub fn relate<R: Relation>(self, target: Entity) -> Result<Self, EcsError> {
        self.world.relate::<R>(self.entity, target)?;
        Ok(self)
    }

    /// Remove typed relation pair `(R, target)`.
    #[inline]
    pub fn unrelate<R: Relation>(self, target: Entity) -> Result<Self, EcsError> {
        let _ = self.world.unrelate::<R>(self.entity, target)?;
        Ok(self)
    }

    /// Assign or replace the unique name of this entity.
    #[inline]
    pub fn named(self, name: &str) -> Result<Self, EcsError> {
        self.world.set_name(self.entity, name)?;
        Ok(self)
    }

    /// Borrow component `T` directly from this entity.
    #[inline]
    pub fn get<T: 'static>(&self) -> Result<Option<&T>, EcsError> {
        self.world.get::<T>(self.entity)
    }

    /// Mutably borrow component `T` directly from this entity.
    #[inline]
    pub fn get_mut<T: 'static>(&mut self) -> Result<Option<&mut T>, EcsError> {
        self.world.get_mut::<T>(self.entity)
    }

    /// Check whether this entity directly owns component `T`.
    #[inline]
    pub fn has<T: 'static>(&self) -> Result<bool, EcsError> {
        self.world.has::<T>(self.entity)
    }

    /// Check whether this entity has typed tag `T`.
    #[inline]
    pub fn has_tag<T: 'static>(&self) -> Result<bool, EcsError> {
        self.world.has_tag_typed::<T>(self.entity)
    }

    /// Check whether this entity has typed relation pair `(R, target)`.
    #[inline]
    pub fn has_relation<R: Relation>(&self, target: Entity) -> Result<bool, EcsError> {
        self.world.has_relation::<R>(self.entity, target)
    }
}
