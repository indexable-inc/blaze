//! One-of UI state as relations. `(On, Set)` on a Toggle means on, absence off; `(Selected, Set)` and `(Open, Set)` likewise.
//! Each relationship is Exclusive, so a node holds one target; closed enums add `OneOf` where they arrive.
use harness_native_flecs::prelude::*;

/// `(On, Set)`: a Toggle is on.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct On;

/// `(Selected, Set)`: a List row, tab or option is picked.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Selected;

/// `(Open, Set)`: a Disclosure, menu or sheet is open.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Open;

/// The one target of the state relations above.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Set;

/// A `List`'s data source: rows are instances of the row prefab, kept by key. `source` names a feature's collection.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Repeat {
    pub source: ScriptString,
    pub key: ScriptString,
}

hyperion_api_layouts::api_layouts! {
    "On" => alias On,
    "Selected" => alias Selected,
    "Open" => alias Open,
    "Set" => alias Set,
    "Repeat" => alias Repeat,
}

/// Register state relations before any scene or system uses them.
pub fn register(world: &World) {
    world.component::<Set>().writable();
    world.component::<Repeat>().writable();
    world.component::<On>().add_trait::<flecs::Exclusive>().writable();
    world.component::<Selected>().add_trait::<flecs::Exclusive>().writable();
    world.component::<Open>().add_trait::<flecs::Exclusive>().writable();
}
