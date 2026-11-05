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

/// How a node appears as its group's spring goes 0 to 1: alpha at 0, offset at 0, scale at 0 (about its middle; 0 or less
/// is no scale). The draw pass folds all three into the node and everything under it. A `Transition` writes it.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Reveal {
    pub alpha0: f32,
    pub dx: f32,
    pub dy: f32,
    pub scale0: f32,
}

/// Where a node stands off its laid-out place, as the draw pass folds it into the node and everything under it: moved by
/// `dx`, `dy` (points), scaled about its middle by `1 - shrink`, faded by `fade` (0 opaque, 1 gone), and, for words, the
/// share `cut` of their glyphs from the end not drawn yet (typed on). Every field is 0 at rest, so a script that writes
/// only some of them leaves the rest still. Layout never reads it: posing a node never moves its neighbours. A timeline's
/// `Cue`s animate it.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Pose {
    pub dx: f32,
    pub dy: f32,
    pub shrink: f32,
    pub fade: f32,
    pub cut: f32,
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

/// A copy's pulse on what was copied (a selected run of words, a table's row, a terminal): a spring at rest at 0, kicked so
/// it peaks at 1 and settles back (critically damped on the `ui.spring.copy` token, stepped by harness-native-ui-motion),
/// and `age`, the seconds since the kick, which the light sweeping each row follows. harness-native-render's select.rs
/// draws it over the selection plate; at rest it draws nothing and asks for no frames.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct CopyFlash {
    pub spring: Spring,
    pub age: f32,
}

impl CopyFlash {
    /// A pulse kicked now on a spring of `omega` rad/s: a critically damped spring pushed at `omega * e` from rest peaks at 1,
    /// `1 / omega` seconds later.
    #[must_use]
    pub fn kicked(omega: f32) -> Self {
        Self { spring: Spring { x: 0.0, v: omega * std::f32::consts::E }, age: 0.0 }
    }

    /// How far into the pulse it is: 0 at rest, 1 at its peak.
    #[must_use]
    pub fn amount(&self) -> f32 {
        self.spring.x.clamp(0.0, 1.0)
    }
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
    /// From rest, a move of more than this many of its own heights (rows, for a selection pill) lands at once instead
    /// of sliding; mid-slide it never does. 0: always slides.
    pub jump: f32,
}

/// The springs of one `MatchedGeometry` id (on `ui_matched::<id>`): the drawn frame chasing its node's `Laid`, in its
/// scroll content's space (the `Laid` plus every scrolling ancestor's `ScrollState`), so a scroll under a slide carries
/// the frame with its rows instead of fighting it.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Matched {
    pub x: Spring,
    pub y: Spring,
    pub w: Spring,
    pub h: Spring,
    pub owner: u64,
    pub started: bool,
    /// Still sliding as of the last frame (a far move then retargets the slide instead of landing).
    pub moving: bool,
    /// Where it was last sent (content space): a frame whose newest node is not laid out yet keeps sliding there.
    pub gx: f32,
    pub gy: f32,
    pub gw: f32,
    pub gh: f32,
    /// The room (`Viewport`, view points) it last chased in, and the clock time that room came in (`rescaled` once it
    /// has changed): a frame whose room differs (the UI zoomed, the window resized) lands the frame on its node, and so
    /// does every later layout pass of that same frame, so a zoom is a uniform scale and never a slide.
    pub room_w: f32,
    pub room_h: f32,
    pub room_at: f32,
    pub rescaled: bool,
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
    "Pose" => alias Pose,
    "Expand" => alias Expand,
    "ShownWhen" => alias ShownWhen,
    "Shown" => alias Shown,
    "Target" => alias Target,
    "CopyFlash" => alias CopyFlash,
}

/// Register the vocabulary before scenes and systems, retaining writable scene properties.
pub fn register(world: &World) {
    world.component::<Motion>().writable();
    world.component::<Reveal>().writable();
    world.component::<Pose>().writable();
    world.component::<Expand>().writable();
    world.component::<ShownWhen>().writable();
    world.component::<Shown>();
    world.component::<Target>();
    world.component::<CopyFlash>();
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
