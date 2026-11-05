//! hyperion-ecs: the thin layer over flecs from hyperion: the raw [`World`] handle (entities, ids, pairs, children, names),
//! component identity by name and layout hash ([`component`]), checked field access, systems, observers and queries, component
//! traits, logging and faults. Typed, owning access lives in den-ecs (den-core's store) and den-flecs (den-native).
//!
//! The parts that need a flecs addon are behind the feature of the same name (`system`, `pipeline`, `meta`); the C must be built
//! with the matching `hyperion-flecs-c` feature by whoever links it.

pub use hyperion_ecs_sys as sys;

pub mod check;
pub mod component;
pub mod hooks;
pub mod module;
pub mod query;
pub mod system;
pub mod traits;
pub mod untyped;
pub mod world;

pub use component::{Error, register_component, register_component_with_hooks, register_raw, require_component, require_component_quiet, world_layout_hash};
#[cfg(feature = "meta")]
pub use component::{register_opaque, register_reflected};
pub use query::{Query, QueryBuilder};
#[cfg(feature = "system")]
pub use system::SystemBuilder;
pub use system::{Access, Iter, ObserverBuilder, Term};
pub use traits::{Cleanup, Trait};
pub use world::{Id, World, pair};
