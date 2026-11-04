//! Visibility and transition components shared by native views and motion systems. Component names stay stable across the host and module images.
use harness_native_flecs::prelude::*;
use harness_native_ui_layout_api::{Padding, Size};
use harness_native_ui_look_api::Fill;
use harness_native_ui_spring_api::Spring;

/// A spring: stiffness as angular frequency (rad/s) and damping ratio.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Motion {
    pub omega: f32,
    pub zeta: f32,
}

/// How a node appears as its group's spring goes 0 to 1: alpha at 0, offset at 0, scale at 0 (about its middle). The draw
/// pass folds all three into the node and everything under it. A `Transition` writes it from its kind.
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

/// A node whose frame animates from wherever the last node with the same `id` was drawn to where layout puts it (SwiftUI's
/// `matchedGeometryEffect`): the selection pill on one row and then another slides between them. The springs live on an
/// entity of their own per id (`Matched`), so the old node may be gone; the tune is the node's `Motion`, else the
/// `ui.spring.matched` token.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct MatchedGeometry {
    pub id: ScriptString,
}

/// The springs of one `MatchedGeometry` id (on `ui_matched::<id>`): the drawn frame chasing its node's `Laid`.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Matched {
    pub x: Spring,
    pub y: Spring,
    pub w: Spring,
    pub h: Spring,
    pub owner: u64,
    pub started: bool,
}

/// Where a node is drawn this frame when that is not its `Laid` (a `MatchedGeometry` mid-slide), window points. Its
/// children move with it. Written by motion, read by draw.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Drawn {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// What a `Transition` does to a node coming in or going out.
#[derive(Layout, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TransitionKind {
    /// Fades.
    #[default]
    Opacity,
    /// In from the leading edge, out to the trailing one, by its width.
    Slide,
    /// Grows from `scale` about its middle (and shrinks back to it going out).
    Scale,
    /// In from and out to `from`, by its size along that edge.
    Move,
}

/// The edge a `Move` transition comes from.
#[derive(Layout, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FromEdge {
    #[default]
    Top,
    Bottom,
    Leading,
    Trailing,
}

/// How a node comes in when it is made and goes out when it is removed with `harness_native_ui_view::leave` (or by a
/// `for_each` that no longer has its key), on its `Motion` spring (else the `ui.spring.transition` token). `fade` fades it
/// as well; `scale` is where a `Scale` starts. Motion drives it through `Shown`, `Target` and `Reveal`.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Transition {
    pub kind: TransitionKind,
    pub from: FromEdge,
    pub scale: f32,
    pub fade: bool,
}

/// A node on its way out: out of the flow (it keeps its last place, `Placed`), its `Transition` running back to 0, then
/// deleted by motion. A tag.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Leaving;

/// Changes to this node's `Size`, `Padding` and `Fill` animate on its `Motion` spring (else the `ui.spring.layout` token)
/// instead of jumping: whatever writes the new value, motion eases what layout and draw see to it (SwiftUI's implicit
/// `.animation`). Layout follows, so siblings move with it. A tag.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Animate;

/// An `Animate` node's `Size` easing `from` to `to`, `shown` the last value written, `t` 0 to 1.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct SizeTween {
    pub from: Size,
    pub to: Size,
    pub shown: Size,
    pub t: Spring,
}

/// An `Animate` node's `Padding` easing `from` to `to`.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct PadTween {
    pub from: Padding,
    pub to: Padding,
    pub shown: Padding,
    pub t: Spring,
}

/// An `Animate` node's `Fill` easing `from` to `to`.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct FillTween {
    pub from: Fill,
    pub to: Fill,
    pub shown: Fill,
    pub t: Spring,
}

hyperion_api_layouts::api_layouts! {
    "MatchedGeometry" => alias MatchedGeometry,
    "Matched" => alias Matched,
    "Drawn" => alias Drawn,
    "Transition" => alias Transition,
    "Leaving" => alias Leaving,
    "Animate" => alias Animate,
    "SizeTween" => alias SizeTween,
    "PadTween" => alias PadTween,
    "FillTween" => alias FillTween,
    TransitionKind,
    FromEdge,
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
    world.component::<MatchedGeometry>().writable();
    world.component::<Matched>();
    world.component::<Drawn>();
    world.component::<Transition>().writable();
    world.component::<Leaving>().writable();
    world.component::<Animate>().writable();
    world.component::<SizeTween>();
    world.component::<PadTween>();
    world.component::<FillTween>();
}
