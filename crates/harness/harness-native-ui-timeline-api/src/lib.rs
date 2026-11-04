//! A timeline as data. A [`Cue`] is a child entity of the node it animates (`{ Cue: {...} }` in the node's scope, or a
//! `cue.*` template instance): over `at` to `at + dur` seconds of the frame clock it moves one member of one of the node's
//! components, named by path (`"Pose.fade"`, `"Size.w.value"`, `"Fill.dark.a"`), from `from` to `to` on an [`Ease`].
//! Any numeric member of any reflected component can be cued, so layout, looks and motion need nothing new to be animated:
//! a cued `Size` relays out its neighbours, a cued `Pose` moves only the node. `harness-native-ui-timeline` applies them.
use harness_native_flecs::prelude::*;

/// How a cue's progress (0 to 1 in time) maps to its value's (0 to 1 from `from` to `to`).
#[derive(Layout, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Ease {
    /// Even speed.
    #[default]
    Linear,
    /// Starts slow (cubic).
    In,
    /// Ends slow (cubic): things arriving.
    Out,
    /// Slow at both ends (cubic): things moving from one place to another.
    InOut,
    /// Overshoots a little, then settles: things popping in.
    Back,
    /// A critically damped spring's approach (exponential, settled at the end): UI motion.
    Spring,
}

impl Ease {
    /// The eased share of the way at time share `t` (clamped to 0..1).
    #[must_use]
    pub fn at(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::Linear => t,
            Self::In => t * t * t,
            Self::Out => 1.0 - (1.0 - t).powi(3),
            Self::InOut => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
                }
            }
            Self::Back => {
                let (c1, c3) = (1.70158_f32, 2.70158_f32);
                1.0 + c3 * (t - 1.0).powi(3) + c1 * (t - 1.0).powi(2)
            }
            Self::Spring => {
                // x(t) = 1 - (1 + wt) e^(-wt), normalised so it lands exactly on 1 at t = 1.
                let w = 8.0_f32;
                let x = |t: f32| 1.0 - (1.0 + w * t) * (-w * t).exp();
                x(t) / x(1.0)
            }
        }
    }
}

/// Move `member` (a component's name, then a dotted path into it) of the nearest `Node` this entity is under from `from`
/// to `to` over `at` to `at + dur` seconds, on `ease`. Cues on one member take turns by `at`: the latest one that has
/// begun holds the value (its `to` once it is over), and before the first has begun the member holds the first's `from`.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Cue {
    pub member: ScriptString,
    pub from: f32,
    pub to: f32,
    pub at: f32,
    pub dur: f32,
    pub ease: Ease,
}

hyperion_api_layouts::api_layouts! {
    "Cue" => alias Cue,
    Ease,
}

/// Register the timeline's components before any scene resolves them.
pub fn register(world: &World) {
    world.component::<Cue>().writable();
}
