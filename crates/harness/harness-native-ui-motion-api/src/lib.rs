//! Visibility and transition components shared by native views and motion systems. Component names stay stable across the host and module images.
use harness_native_flecs::prelude::*;
use harness_native_ui_spring_api::Spring;

/// A spring: stiffness as angular frequency (rad/s) and damping ratio.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Motion {
    pub omega: f32,
    pub zeta: f32,
}

/// How a node appears as its group's spring goes 0 to 1: alpha at 0, offset at 0, scale at 0.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Reveal {
    pub alpha0: f32,
    pub dx: f32,
    pub dy: f32,
    pub scale0: f32,
}

/// The width (points) a node with a Motion widens to as it shows: its Place `w` eased to this by how shown it is.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Expand {
    pub w: f32,
}

/// Relationship: a node is shown while the app is in the target state (any of several pairs).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct ShownWhen;

/// The spring of a node with Motion: 0 hidden to 1 shown (the composer's: its height). `started` is false until the
/// first step, which lands it on its target (no animation on launch or after a reload).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Shown {
    pub spring: Spring,
    pub started: bool,
}

/// The host's target for a node's spring when no state decides it (the composer's height, the scroller's fade).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Target(pub f32);

hyperion_api_layouts::api_layouts! {
    "Motion" => alias Motion,
    "Reveal" => alias Reveal,
    "Expand" => alias Expand,
    "ShownWhen" => alias ShownWhen,
    "Shown" => alias Shown,
    "Target" => alias Target,
}

/// Register the vocabulary before scenes and systems, retaining writable scene properties.
pub fn register(world: &World) {
    world.component::<Motion>().writable();
    world.component::<Reveal>().writable();
    world.component::<Expand>().writable();
    world.component::<ShownWhen>().writable();
    world.component::<Shown>();
    world.component::<Target>();
}
