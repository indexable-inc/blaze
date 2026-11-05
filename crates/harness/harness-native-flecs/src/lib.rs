//! harness-native-flecs: den-native's world API over hyperion-ecs.
//!
//! A component is found in a world by its world name (`T::NAME`, the type's name) and checked against the layout hash recorded
//! there (`T::HASH`, from its recipe), never by a per-image static index: the host and every module image it loads resolve the
//! same type to the same id, and a module built against another layout is refused instead of reading the wrong bytes.
//!
//! The host owns the world and registers every component ([`World::component`]) with lifecycle hooks of its own image, so a
//! component may hold heap data (on the process allocator, which every image shares). A module image only resolves components;
//! asking for one the host has not registered is a fault there (see [`host`]). No flecs C is linked here: a host binary links it
//! (hyperion-flecs-c) and a module dylib resolves `ecs_*` against the host's.
//!
//! The method names follow flecs-rust (`try_cloned`, `each_child`, `system_named`, ...), the API den-native was written against.
mod checked;
mod entity;
pub mod host;
mod query;
mod reflect;
mod script;
mod shared;
mod string;
mod system;
mod world;

pub use harness_native_flecs_derive::{Component, Layout};
pub use checked::{FlecsError, checked, clear_error, set_message_source, take_error};
pub use entity::{Entity, EntityView};
pub use query::{Query, QueryBuilder, QueryTuple, Term};
pub use reflect::HOST_OWNED;
pub use script::{Prop, ScriptBuilder, ScriptEntityView};
pub use shared::{Json, Shared};
pub use string::ScriptString;
pub use system::{Event, ObserverBuilder, Phase, SystemBuilder, TableIter};
pub use world::{ComponentView, World, WorldRef};
pub mod migrate;

use core::marker::PhantomData;
use hyperion_api_layouts::ApiComponent;
use hyperion_ecs::{Id, sys};

#[doc(hidden)]
pub mod __private {
    pub use hyperion_api_layouts::ApiComponent;
    pub use crate::shared::json_of;
    pub use crate::migrate::{Golden, GoldenField};
    pub use inventory;
    pub use hyperion_layout::layout::{EnumKind, Field, Form, Layout, Prim, Recipe, Variant, raw_name, serialize};
}

pub mod prelude {
    pub use crate::flecs;
    pub use crate::{
        Component, ComponentId, Entity, EntityView, Get, IntoId, Layout, Phase, Prop, Query, QueryBuilder, ScriptEntityView, ScriptString, Shared, TableIter, World, WorldRef,
    };
}

/// Run a callback on a borrowed component (`get::<&T>` or `get::<&mut T>`). The callback's result is the trait's parameter,
/// as in flecs-rust, so a call names only the borrow.
pub trait Get<R> {
    fn try_get<Q: Access>(self, f: impl FnOnce(Q::Out<'_>) -> R) -> Option<R>;
    fn get<Q: Access>(self, f: impl FnOnce(Q::Out<'_>) -> R) -> R;
}

/// Ties a builder or query to its world's borrow and its query type.
pub(crate) type Marker<'a, Q> = PhantomData<(&'a (), fn() -> Q)>;

/// A component type: a world name, a layout, and what flecs needs to keep a value of it (Default to construct, Clone to copy).
/// Implemented for every `#[derive(Component)]` type.
pub trait ComponentId: ApiComponent + Default + Clone + 'static {
    /// The component as an id to add, remove, test or pair; resolved against the world it is used on.
    #[must_use]
    fn id() -> Comp<Self> {
        Comp(PhantomData)
    }
}

impl<T: ApiComponent + Default + Clone + 'static> ComponentId for T {}

/// [`ComponentId::id`]: component `T`, not yet resolved to a world's id.
pub struct Comp<T>(PhantomData<fn() -> T>);

impl<T> Clone for Comp<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Comp<T> {}

/// Something that names an id in a world: an entity, a component, a flecs built-in, or a pair `(relationship, target)` of two.
/// 0 when it cannot be resolved there (a component the world does not have).
pub trait IntoId: Copy {
    fn into_id(self, w: hyperion_ecs::World) -> Id;
}

impl IntoId for u64 {
    fn into_id(self, _: hyperion_ecs::World) -> Id {
        self
    }
}

impl<T: ComponentId> IntoId for Comp<T> {
    fn into_id(self, w: hyperion_ecs::World) -> Id {
        host::resolve::<T>(w)
    }
}

impl<A: IntoId, B: IntoId> IntoId for (A, B) {
    fn into_id(self, w: hyperion_ecs::World) -> Id {
        let (r, t) = (self.0.into_id(w), self.1.into_id(w));
        if r == 0 || t == 0 { 0 } else { hyperion_ecs::pair(r, t) }
    }
}

/// flecs built-ins by their flecs-rust names.
pub mod flecs {
    use super::{Builtin, sys};

    /// The `*` id: `(Wildcard, t)` matches a pair with any relationship.
    pub struct Wildcard;
    impl Wildcard {
        pub const ID: Builtin = Builtin::Wildcard;
    }

    /// On a parent entity (`e.add(flecs::OrderedChildren::ID)`, before its first child): its children keep creation order.
    pub struct OrderedChildren;
    impl OrderedChildren {
        pub const ID: Builtin = Builtin::OrderedChildren;
    }

    /// The parent relationship: `with_up(Node::id(), flecs::ChildOf::ID)` matches a node inside another.
    pub struct ChildOf;
    impl ChildOf {
        pub const ID: Builtin = Builtin::ChildOf;
    }

    /// The prefab relationship: `(flecs::IsA::ID, prefab)`.
    pub struct IsA;
    impl IsA {
        pub const ID: Builtin = Builtin::IsA;
    }

    /// The tag of a prefab entity.
    pub struct Prefab;
    impl Prefab {
        pub const ID: Builtin = Builtin::Prefab;
    }

    /// The script component flecs keeps on a script entity (`error` holds the last failure).
    pub struct Script;

    pub use hyperion_ecs::Cleanup;

    /// A trait [`ComponentView::add_trait`](crate::ComponentView::add_trait) can declare. The traits that name another id
    /// (`With`, `OneOf`, `OnDelete`, `OnDeleteTarget`) are methods of [`ComponentView`](crate::ComponentView) instead.
    pub trait FlecsTrait {
        const TRAIT: hyperion_ecs::Trait;
    }

    macro_rules! traits {
        ($($(#[$doc:meta])* $name:ident),* $(,)?) => {$(
            $(#[$doc])*
            pub struct $name;
            impl FlecsTrait for $name {
                const TRAIT: hyperion_ecs::Trait = hyperion_ecs::Trait::$name;
            }
        )*};
    }

    traits! {
        /// A relationship with at most one target per entity: adding a second target replaces the first.
        Exclusive,
        /// `R(a, b)` implies `R(b, a)`. Aborts if declared after the relationship is used.
        Symmetric,
        /// `R(a, b)` and `R(b, c)` imply `R(a, c)` in queries.
        Transitive,
        /// `R(x, x)` holds for every `x` in queries.
        Reflexive,
        /// The relationship forms no cycles.
        Acyclic,
        /// Queries may follow the relationship up (`up`, `cascade`).
        Traversable,
        /// A pair of the relationship never carries data. Aborts if declared after the relationship is used.
        PairIsTag,
        /// Only usable as a relationship.
        Relationship,
        /// Only usable as a relationship's target.
        Target,
        /// Only usable as a trait.
        Trait,
        /// No entity may `IsA` it.
        Final,
        /// One value per world, on the component entity.
        Singleton,
        /// `(OnInstantiate, Inherit)`: a prefab's instances share its value (edited once on the prefab).
        Inherit,
        /// `(OnInstantiate, DontInherit)`: a prefab's instances do not get it.
        DontInherit,
        /// `(OnInstantiate, Copy)`: a prefab's instances copy it (the default; `Override` before flecs 4.2).
        Copy,
        /// Stable storage, no table move on add or remove.
        Sparse,
        /// Does not fragment tables (implies Sparse; no `target_for`, inheritance or Or/Optional terms).
        DontFragment,
        /// Enabled and disabled without a table move.
        CanToggle,
        /// A component (not a tag) that `IsA` instances inherit.
        Inheritable,
    }

    pub(crate) fn script_id() -> hyperion_ecs::Id {
        // SAFETY: an immutable flecs global (the script addon's component id).
        unsafe { sys::FLECS_IDEcsScriptID_ }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Builtin {
    Wildcard,
    OrderedChildren,
    ChildOf,
    IsA,
    Prefab,
}

impl IntoId for Builtin {
    fn into_id(self, _: hyperion_ecs::World) -> Id {
        // SAFETY: immutable flecs globals.
        match self {
            Builtin::Wildcard => unsafe { sys::EcsWildcard },
            Builtin::OrderedChildren => unsafe { sys::EcsOrderedChildren },
            Builtin::ChildOf => unsafe { sys::EcsChildOf },
            Builtin::IsA => unsafe { sys::EcsIsA },
            Builtin::Prefab => unsafe { sys::EcsPrefab },
        }
    }
}

/// How a component is borrowed: `&T` or `&mut T` (the type argument of `get`, `try_cloned`, queries and systems).
pub trait Access {
    type C: 'static;
    type Out<'x>;
    const MUT: bool;
    /// The component's id in `w`, 0 when the world does not have it.
    fn id(w: hyperion_ecs::World) -> Id;
    /// # Safety
    /// `p` points to a live value of `Self::C` that outlives `'x` and is not aliased mutably.
    unsafe fn out<'x>(p: *mut Self::C) -> Self::Out<'x>;
}

impl<T: ComponentId> Access for &T {
    type C = T;
    type Out<'x> = &'x T;
    const MUT: bool = false;
    fn id(w: hyperion_ecs::World) -> Id {
        host::resolve::<T>(w)
    }
    unsafe fn out<'x>(p: *mut T) -> &'x T {
        // SAFETY: the caller's contract.
        unsafe { &*p }
    }
}

impl<T: ComponentId> Access for &mut T {
    type C = T;
    type Out<'x> = &'x mut T;
    const MUT: bool = true;
    fn id(w: hyperion_ecs::World) -> Id {
        host::resolve::<T>(w)
    }
    unsafe fn out<'x>(p: *mut T) -> &'x mut T {
        // SAFETY: the caller's contract.
        unsafe { &mut *p }
    }
}

impl Access for &flecs::Script {
    type C = sys::EcsScript;
    type Out<'x> = &'x sys::EcsScript;
    const MUT: bool = false;
    fn id(_: hyperion_ecs::World) -> Id {
        flecs::script_id()
    }
    unsafe fn out<'x>(p: *mut sys::EcsScript) -> &'x sys::EcsScript {
        // SAFETY: the caller's contract.
        unsafe { &*p }
    }
}
