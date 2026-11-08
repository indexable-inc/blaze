//! Visibility and transition components shared by native views and motion systems. Component names stay stable across the host and module images.
use harness_native_flecs::prelude::*;
use harness_native_ui_layout_api::{Padding, Size};
use harness_native_ui_look_api::Fill;
use harness_native_ui_spring_api::{Spring, Tune};

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

/// The axis a [`Posed`] node's `share` unfolds along: down from its top, or across from its leading edge.
#[derive(Layout, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PoseAxis {
    #[default]
    Y,
    X,
}

/// A node's own pose, which it glides to on springs (den's universal `x`, `y`, `scale` and `opacity`): drawn moved by `x`,
/// `y` points, scaled about its middle by `scale`, at `opacity`, and unfolded to the share `share` of its frame along
/// `axis` (cut from its far edge; 1 is whole). Whatever writes it, motion springs [`Posing`] (what draw folds) to it, on
/// `omega`, `zeta` (omega 0: the node's `Motion`, else the `ui.spring.pose` token). Draw folds it after [`Pose`], which other
/// owners write and which never springs, and mixes it toward a [`HoverPose`] by the node's `HoverAmt`. Layout never reads
/// it: posing a node never moves its neighbours.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct Posed {
    pub x: f32,
    pub y: f32,
    pub scale: f32,
    pub opacity: f32,
    pub share: f32,
    pub axis: PoseAxis,
    pub omega: f32,
    pub zeta: f32,
}

impl Posed {
    /// Where it stands with nothing said: in place, whole and opaque, on the default spring.
    pub const REST: Self = Self { x: 0.0, y: 0.0, scale: 1.0, opacity: 1.0, share: 1.0, axis: PoseAxis::Y, omega: 0.0, zeta: 0.0 };
}

impl Default for Posed {
    fn default() -> Self {
        Self::REST
    }
}

/// The springs a [`Posed`] node is drawn at, chasing its `Posed`; none moves before clock time `hold` (an entrance's
/// delay). A writer that wants a node to come in from somewhere sets this at that pose as it makes the node
/// ([`Posing::at`], den's `enter`); a node with none lands on its `Posed`.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Posing {
    pub x: Spring,
    pub y: Spring,
    pub scale: Spring,
    pub opacity: Spring,
    pub share: Spring,
    pub hold: f32,
}

impl Posing {
    /// Standing still at `p` until clock time `hold`.
    #[must_use]
    pub fn at(p: Posed, hold: f32) -> Self {
        Self { x: Spring::at(p.x), y: Spring::at(p.y), scale: Spring::at(p.scale), opacity: Spring::at(p.opacity), share: Spring::at(p.share), hold }
    }

    /// Where its springs are now, with `of`'s axis and tune.
    #[must_use]
    pub fn now(&self, of: Posed) -> Posed {
        Posed { x: self.x.x, y: self.y.x, scale: self.scale.x, opacity: self.opacity.x, share: self.share.x, ..of }
    }
}

/// A [`Posed`] node that glides from where it was laid to where it is laid now instead of jumping (FLIP, Svelte's
/// `animate:flip`): when its place in its parent moves, motion pushes its `Posing` back by the move, and it springs home.
/// `x`, `y` are where it was last laid in its parent (points from the parent's top left); `seen` is false until then.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Flip {
    pub x: f32,
    pub y: f32,
    pub seen: bool,
}

/// What a node's pose becomes under the pointer, mixed in by its `HoverAmt` (0 off it, 1 on it): each field `set` names
/// ([`HoverPose::X`], `Y`, `SCALE`, `OPACITY`) eased toward its value here, the rest left as its pose has them.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct HoverPose {
    pub x: f32,
    pub y: f32,
    pub scale: f32,
    pub opacity: f32,
    pub set: u8,
}

impl HoverPose {
    pub const X: u8 = 1;
    pub const Y: u8 = 2;
    pub const SCALE: u8 = 4;
    pub const OPACITY: u8 = 8;

    /// `p` with this hover's fields mixed in by `t`.
    #[must_use]
    pub fn mix(&self, p: Posed, t: f32) -> Posed {
        let m = |bit: u8, a: f32, b: f32| if self.set & bit != 0 { a + (b - a) * t } else { a };
        Posed { x: m(Self::X, p.x, self.x), y: m(Self::Y, p.y, self.y), scale: m(Self::SCALE, p.scale, self.scale), opacity: m(Self::OPACITY, p.opacity, self.opacity), ..p }
    }
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

/// A copy's pulse on what was copied (a selected run of words, a table's row, a terminal): its clock, a fixed timeline
/// rather than a spring. What plays over it (the grow, the light sweeping across, the glow under Reduce Motion) is the
/// scene's `ui.selection` `CopyLook`, drawn over the selection plate by harness-native-render's select.rs. `age` is the
/// seconds since the copy; harness-native-ui-motion steps it and puts the pulse back at rest at its end, so nothing more is
/// drawn and the frame goes idle.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct CopyFlash {
    pub age: f32,
    /// How long the pulse lasts (0: at rest): its look's `CopyLook::length`.
    pub secs: f32,
    /// What was copied is the node's whole rect (a table's row), drawn as a selection plate over it while the pulse lasts;
    /// false: only the node's own selection (a `TextEdit`'s words, a terminal's cells), which its painter pulses.
    pub whole: bool,
}

impl CopyFlash {
    /// A pulse starting now that lasts `secs` (the scene's `ui.selection` CopyLook's `length`).
    #[must_use]
    pub fn kicked(secs: f32) -> Self {
        Self { age: 0.0, secs, whole: false }
    }

    /// Whether it is still playing.
    #[must_use]
    pub fn live(&self) -> bool {
        self.secs > 0.0 && self.age < self.secs
    }
}

/// The host's target for a node's spring when no state decides it (the composer's height, the scroller's fade).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Target(pub f32);

/// A node whose frame animates from wherever the last node with the same `id` was drawn to where layout puts it (SwiftUI's
/// `matchedGeometryEffect`): the selection pill on one row and then another slides between them. Its position springs;
/// its size is always its node's (never between two, so a pill is never shorter than the row it lands on), and inside a
/// [`Settling`] node it holds until that node's frame is its own. The springs live on an
/// entity of their own per id (`Matched`), so the old node may be gone; the tune is the node's `Motion`, else the
/// `ui.spring.matched` token.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct MatchedGeometry {
    pub id: ScriptString,
    /// From rest, a move of more than this many of its own heights (rows, for a selection pill) lands at once instead
    /// of sliding; mid-slide it never does. 0: always slides.
    pub jump: f32,
}

/// A tag on a node whose frame is not its own yet: its size is on a spring of its owner's (a sidebar row folding in
/// from no height), so what is laid inside it is squeezed. A `MatchedGeometry` node in it holds its slide where it was
/// going (and, with nowhere it was going, is drawn nowhere) instead of chasing a frame smaller than what it highlights;
/// once the tag goes, it slides to its node's frame from there.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Settling;

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
    /// Grows from `scale` with its `from` edge held still (a menu opening out of what it hangs from), shrinking back
    /// toward it going out.
    Grow,
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

/// How text that arrives in bursts (a model's reply: a sentence at once, then nothing for a second) is shown: at an even
/// pace about `lag` seconds behind what came, never slower than `min` units a second nor faster than `max` (so a whole
/// reply that lands at once still reads as written), but never more than `most` seconds behind. [`Paced`] steps it.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Pace {
    pub lag: f32,
    pub min: f32,
    pub max: f32,
    pub most: f32,
}

/// How much of a growing text shows, on a [`Pace`]: sans IO, stepped by the frame's time.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Paced {
    shown: f32,
}

impl Paced {
    /// `n` units shown, at rest (what was there before it began to grow).
    #[must_use]
    pub fn at(n: usize) -> Self {
        Self { shown: n as f32 }
    }

    /// One frame `dt` seconds long in which `have` units have come: how many show now. A text that shrank shows what it
    /// has; one caught up stays caught up.
    pub fn step(&mut self, have: usize, pace: Pace, dt: f32) -> usize {
        let have_f = have as f32;
        if have_f <= self.shown {
            self.shown = have_f;
            return have;
        }
        let behind = have_f - self.shown;
        let even = (behind / pace.lag.max(1e-3)).clamp(pace.min, pace.max.max(pace.min));
        let rate = even.max(behind / pace.most.max(1e-3));
        self.shown = (self.shown + rate * dt.max(0.0)).min(have_f);
        self.shown as usize
    }

    /// Whether it shows all of `have`.
    #[must_use]
    pub fn caught_up(&self, have: usize) -> bool {
        self.shown >= have as f32
    }
}

/// A scroll kept at its content's end as the content grows (a transcript following its agent): it glides to the end on a
/// spring instead of jumping there each time a line comes. Sans IO, stepped by the frame's time.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Follow {
    spring: Spring,
}

impl Follow {
    /// At rest at `y`.
    #[must_use]
    pub fn at(y: f32) -> Self {
        Self { spring: Spring::at(y) }
    }

    /// The scroll was moved by `by` under it (rows above it changed height): it follows on from there, as fast.
    pub fn shift(&mut self, by: f32) {
        self.spring.x += by;
    }

    /// One frame `dt` seconds long toward `end`: where the scroll is now, and whether it still moves. From rest, an end
    /// more than `far` away (a pane opened on a long transcript) is landed on at once; mid-glide it never is.
    pub fn step(&mut self, end: f32, tune: Tune, far: f32, dt: f32) -> (f32, bool) {
        let resting = self.spring.v == 0.0;
        if resting && (end - self.spring.x).abs() > far {
            self.spring = Spring::at(end);
        }
        self.spring.step(end, tune, dt);
        if self.spring.settled(end) || self.spring.x > end {
            // At the end exactly (never past it: there is no content there), so the scroll rests on the pixel grid.
            self.spring = Spring::at(end);
            return (end, false);
        }
        (self.spring.x, true)
    }
}

hyperion_api_layouts::api_layouts! {
    "MatchedGeometry" => alias MatchedGeometry,
    "Matched" => alias Matched,
    "Settling" => alias Settling,
    "Drawn" => alias Drawn,
    "Transition" => alias Transition,
    "Leaving" => alias Leaving,
    "Animate" => alias Animate,
    "SizeTween" => alias SizeTween,
    "PadTween" => alias PadTween,
    "FillTween" => alias FillTween,
    TransitionKind,
    FromEdge,
    PoseAxis,
    "Posed" => alias Posed,
    "Posing" => alias Posing,
    "HoverPose" => alias HoverPose,
    "Flip" => alias Flip,
    "Motion" => alias Motion,
    "Reveal" => alias Reveal,
    "Pose" => alias Pose,
    "Expand" => alias Expand,
    "ShownWhen" => alias ShownWhen,
    "Shown" => alias Shown,
    "Target" => alias Target,
    "CopyFlash" => alias CopyFlash,
    "Pace" => alias Pace,
}

/// Register the vocabulary before scenes and systems, retaining writable scene properties.
pub fn register(world: &World) {
    world.component::<Motion>().writable();
    world.component::<Reveal>().writable();
    world.component::<Pose>().writable();
    world.component::<Posed>().writable();
    world.component::<Posing>();
    world.component::<HoverPose>().writable();
    world.component::<Flip>().writable();
    world.component::<Expand>().writable();
    world.component::<ShownWhen>().writable();
    world.component::<Shown>();
    world.component::<Target>();
    world.component::<CopyFlash>();
    world.component::<Pace>().writable();
    world.component::<MatchedGeometry>().writable();
    world.component::<Matched>();
    world.component::<Settling>();
    world.component::<Drawn>();
    world.component::<Transition>().writable();
    world.component::<Leaving>().writable();
    world.component::<Animate>().writable();
    world.component::<SizeTween>();
    world.component::<PadTween>();
    world.component::<FillTween>();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A paragraph landing in one burst shows over about `lag` seconds, never in one frame and never later than `most`;
    /// a text that grows no further stays caught up.
    #[test]
    fn a_burst_shows_at_its_pace() {
        let pace = Pace { lag: 0.25, min: 30.0, max: 1400.0, most: 2.0 };
        let mut p = Paced::at(0);
        let first = p.step(600, pace, 1.0 / 60.0);
        assert!(first > 0 && first < 100, "{first}");
        let frames = (0..200).take_while(|_| p.step(600, pace, 1.0 / 60.0) < 600).count();
        assert!(frames < 120, "{frames}");
        assert!(p.caught_up(600) && p.step(600, pace, 1.0 / 60.0) == 600);
        // More than `max` a second can show in `most`: it goes faster than `max` (backlog over `most`) until it is not.
        let mut big = Paced::at(0);
        assert!(big.step(10_000, pace, 1.0 / 60.0) > (pace.max / 60.0) as usize);
        let frames = (0..600).take_while(|_| big.step(10_000, pace, 1.0 / 60.0) < 10_000).count();
        assert!(frames < 420, "{frames}");
    }
}
