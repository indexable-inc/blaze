//! `bstd::ecs`: zero-dependency, `#![no_std]`, `#![forbid(unsafe_code)]`, `#![blaze::sans_io]`
//! Flecs 4.2-equivalent archetype Entity-Component-System library.
//!
//! - [`id`]: 64-bit generational [`Entity`] handles, typed [`ComponentId`] / [`TableId`],
//!   unified Flecs-style [`Id`]s (components, tags, and `(relation, target)` pairs), and
//!   deterministic [`Builtins`] (`ChildOf`, `IsA`, `Prefab`, `Wildcard`, `Exclusive`,
//!   `Transitive`, `Symmetric`, `OnDeleteCascade`).
//! - [`column`]: 100% safe contiguous SoA [`AnyColumn`] / [`TypedColumn`] storage over
//!   `blaze_own::Vec<T, A>`.
//! - [`table`]: Archetype SoA [`Table`]s with Flecs-style cached `add_edges` / `remove_edges`
//!   and safe disjoint column borrowing (`columns_2_mut`, `columns_1_mut_1_ref`).
//! - [`query`]: Declarative [`QuerySpec`] (exact & wildcard pairs, prefab filtering, topological
//!   `cascade` ordering) and incrementally synced [`CachedQuery`].
//! - [`observer`]: Reactive [`EventKind`] (`OnAdd`, `OnSet`, `OnRemove`) observers with exact
//!   and wildcard pair matching and bounded zero-recursion dispatch.
//! - [`pipeline`]: Canonical Flecs 4.2 [`Phase`]s and deterministic conflict-free parallel
//!   [`Pipeline`] batch scheduling (with automatic typed [`Pipeline::add_query`] access sets).
//! - [`typed`]: Zero-string [`short_type_name`], typed [`Relation`] / [`Bundle`] / [`Prefab`]
//!   hierarchies, and fluent [`EntityMut`] builder.
//! - [`fetch`]: 1-line typed [`QueryFetch`] (`&T`, `&mut T`, 2..=4 tuples) and chainable
//!   [`TypedQuery`] filters.
//! - [`world`]: Core [`World`] API with prefab inheritance (`IsA`), hierarchy cascade (`ChildOf`),
//!   arbitrary relations, and deterministic [`World::snapshot_tab`] summaries.
//! - [`reflect`]: Live `bstd::data`-backed component reflection ([`ReflectComponent`],
//!   [`ReflectError`], and [`reflect_struct!`]).
//! - [`explorer`]: Pure sans-IO Flecs Explorer HTTP/JSON/TAB API ([`handle_request`],
//!   [`handle_http_request`], [`ExplorerResponse`]) and embedded Web UI ([`EXPLORER_HTML`]).


pub mod column;
pub mod error;
pub mod explorer;
pub mod fetch;
pub mod id;
pub(crate) mod map;
pub mod observer;
pub mod pipeline;
pub mod query;
pub mod reflect;
pub mod table;
pub mod typed;
pub mod world;

#[cfg(test)]
mod tests;

pub use crate::reflect_struct;
pub use column::{AnyColumn, ComponentMeta, TypedColumn};
pub use error::EcsError;
pub use explorer::{EXPLORER_HTML, ExplorerResponse, handle_http_request, handle_request};
pub use fetch::{FetchIds, QueryFetch, TypedQuery};
pub use id::{Builtins, ComponentId, Entity, Id, TableId};
pub use observer::{DEFAULT_OBSERVER_CAP, Event, EventKind, ObserverCallback, ObserverId};
pub use pipeline::{Batch, Batches, Phase, PhaseSchedule, Pipeline, SystemDesc, SystemFn};
pub use query::{CachedQuery, QuerySpec, QueryTerm};
pub use reflect::{ReflectComponent, ReflectError};
pub use table::Table;
pub use typed::{
    Bundle, ChildOf, CopyBundle, EntityMut, IsA, NoParent, Prefab, PrefabInit, PrefabParent,
    Relation, short_type_name,
};
pub use world::World;
