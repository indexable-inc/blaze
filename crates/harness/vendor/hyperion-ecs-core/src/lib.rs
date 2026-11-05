//! hyperion-ecs-core: the base every module-side SDK crate builds on (docs/host-diet.md 10.10 item 1).
//!
//! One job: the world handle, ids, errors, the module scope ([`ModuleCtx`]) and component identity ([`identity`]: world name plus layout hash).
//! It re-exports the raw `hyperion-ecs` handles a module may read (`World`, `Id`, `Iter`, `Term`, `sys`, `hooks`, `check`, `untyped`,
//! `world`). It does not depend on the host, and no other SDK crate depends on anything in the host closure but `hyperion-ecs`.
//!
//! Extension points (F-sdk): name-based identity helpers (`NAME`/`HASH` tables) go in `identity`; new raw re-exports go here.

pub mod identity;
pub mod module;
pub mod scope;

pub use hyperion_ecs::sys;
pub use hyperion_ecs::{check, hooks, untyped, world};
pub use hyperion_ecs::component::{Error, world_layout_hash};
pub use hyperion_ecs::system::{Access, Iter, Term};
pub use hyperion_ecs::world::{Id, World, pair};
pub use identity::{ApiComponent, ApiK, LocalComponent, LocalK, Resolve, assert_home, component_id, id_or_fault};
pub use module::ModuleCtx;
pub use scope::Scope;
