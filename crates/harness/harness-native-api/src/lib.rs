//! harness-native-api: what den-native's host and its hot-swappable modules share (docs/hot-reload.md, Native modules).
//!
//! The components a module's systems read and write live here, not in the host, and are listed once in [`API_LAYOUTS`]
//! (hyperion's `api_layouts!`): a module image embeds its copy of that table in its manifest, and the host refuses an image whose
//! table disagrees with the world. [`module`] is the ABI a module cdylib exports. Everything here is plain data or code every
//! image links its own copy of; the world, and so all state, belongs to the host.
pub mod module;
pub mod spring;

use harness_native_flecs::prelude::*;
pub use harness_native_ui_input_api::Hovered;
use spring::{Spring, Tune};

pub const APP: &str = "harness::app";

/// The frame clock (singleton): seconds since launch, and whether a system saw something still moving.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Clock {
    pub time: f32,
    pub moving: bool,
}

/// How hovered a Hoverable is (0 to 1, on a spring) and since when the pointer has been on it.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct HoverAmt {
    pub spring: Spring,
    pub since: f32,
}

/// A Hoverable whose hover spring follows [`Lit`] instead of the pointer: an item of a menu, whose one highlight the
/// pointer and the arrow keys both move (its owner keeps `Lit` on the highlighted item; harness_native_ui_view::menu).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct HoverSteered;

/// The highlighted item of a [`HoverSteered`] group (a tag).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Lit;

/// How folded an agent's sub-agents are (0 open to 1 folded, on a spring): its chevron's turn.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Fold(pub Spring);

/// An agent whose sub-agents are folded away in the sidebar.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Folded;

/// The system's Reduce Motion (singleton; System Settings, Accessibility, Display): while `on`, what would glide or fold
/// on a spring lands at once (a matched-geometry slide such as the selection pill, the sidebar's rows folding and its
/// chevrons turning, the tree's glide to the selection). The host keeps it in step with AppKit's
/// `accessibilityDisplayShouldReduceMotion` (src/window.rs).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct ReduceMotion {
    pub on: bool,
}

/// Whether the world is under Reduce Motion.
pub fn reduced(w: WorldRef<'_>) -> bool {
    w.try_get::<&ReduceMotion>(|r| r.on).unwrap_or(false)
}

hyperion_api_layouts::api_layouts! {
    "Clock" => alias Clock,
    "ReduceMotion" => alias ReduceMotion,
    "HoverAmt" => alias HoverAmt,
    "Fold" => alias Fold,
    "Folded" => alias Folded,
    "HoverSteered" => alias HoverSteered,
    "Lit" => alias Lit,
    "Hovered" => alias Hovered,
    Spring,
}

/// Register every component of [`API_LAYOUTS`] (the host, before any module resolves them).
pub fn register(world: &World) {
    world.component::<Clock>();
    world.component::<ReduceMotion>();
    world.component::<HoverAmt>();
    world.component::<Fold>();
    world.component::<Folded>();
    world.component::<HoverSteered>();
    world.component::<Lit>();
    harness_native_ui_input_api::register(world);
}

/// Tell the host a system saw something still moving (it keeps drawing frames).
pub fn moving(w: &WorldRef) {
    w.get::<&mut Clock>(|c| c.moving = true);
}

/// Step `s` toward `target`; true while it has not settled.
pub fn settle(s: &mut Spring, target: f32, tune: Tune, dt: f32) -> bool {
    s.step(target, tune, dt);
    !s.settled(target)
}
