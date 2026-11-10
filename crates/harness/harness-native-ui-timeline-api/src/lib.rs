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

/// A node's words arriving glyph by glyph: each rises `by` points into place and fades in over `dur` seconds on `ease`,
/// glyph `i` (in reading order) starting at `at + i * per`. The paint pass reads it against the frame clock; layout keeps
/// every glyph's room from the start, so nothing moves as they arrive. With `cool` above 0 each glyph arrives hot, in
/// the `hot` ink, and cools to its own over `cool` seconds.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct GlyphsIn {
    pub at: f32,
    pub per: f32,
    pub dur: f32,
    pub by: f32,
    pub ease: Ease,
    pub cool: f32,
    pub hot: harness_native_ui_look_api::Rgba,
}

impl GlyphsIn {
    /// Glyph `i`'s opacity, how far it still sits below its place (points), and how hot it still is (1 all `hot`, 0 its
    /// own ink) at frame time `time`.
    #[must_use]
    pub fn of(&self, i: usize, time: f32) -> (f32, f32, f32) {
        let since = time - self.at - i as f32 * self.per;
        let t = since / self.dur.max(1e-6);
        let p = self.ease.at(t);
        let heat = if self.cool > 0.0 { 1.0 - Ease::InOut.at(since / self.cool) } else { 0.0 };
        (t.clamp(0.0, 1.0).powf(0.6), self.by * (1.0 - p), heat)
    }
}

/// An instrument a [`Sound`] plays (harness-native-blaze's soundtrack voices); the members it reads are on each.
#[derive(Layout, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Voice {
    /// Risset's bell: Pitch, Lasts, Gain, Pan.
    #[default]
    Bell,
    /// A felt mallet on a marimba-like bar: Pitch, Lasts, Gain, Pan.
    Mallet,
    /// A round sine bass, centred: Pitch, Lasts, Gain.
    Bass,
    /// A short tick with a ringing tone: Pitch, Gain, Pan.
    Click,
    /// A low thump, centred, Gain its size.
    Thud,
    /// Filtered noise brightening to its Pitch: something passing (Pan's sweep moves it). Lasts, Gain.
    Whoosh,
    /// The physically modelled fire, burning for its Lasts.
    Fire,
}

/// A sound of the scene's soundtrack, as data, one component per concern so a prefab carries a voice's defaults and an
/// instance sets only what differs (`ding : voice.Bell { Onset: {2.0} Pitch: {1760} }`): which voice ([`Sound`]), when it
/// starts ([`Onset`], required), how long it rings ([`Lasts`]), its pitch ([`Pitch`]), level ([`Gain`]) and where it is
/// heard from ([`Pan`]). harness-native-ui-prims ships the `voice.*` prefabs, one a voice (scene/voice.flecs); harness-video
/// mixes every entity under its frame with a `Sound` and an `Onset` into the soundtrack, in the same room as the mark's score.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Sound {
    pub voice: Voice,
}

/// When a [`Sound`] starts, seconds on the frame clock.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Onset {
    pub at: f32,
}

/// How long a [`Sound`] rings or lasts, seconds.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Lasts {
    pub dur: f32,
}

/// A [`Sound`]'s pitch, Hz (a whoosh's brightest, a click's ring).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Pitch {
    pub hz: f32,
}

/// A [`Sound`]'s level (the mark's mallets are about 0.05; a thud's is its size).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Gain {
    pub amp: f32,
}

/// Where a [`Sound`] is heard from: `pos` across the frame (-1 its left edge, 1 its right; placed binaurally, so on
/// headphones it comes from there), moving by `sweep` over its length.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Pan {
    pub pos: f32,
    pub sweep: f32,
}

/// A node's words fading in as they grow (a reply streaming in): each glyph that was not there the frame before rises `by`
/// points into place and fades in over `dur` seconds on `ease`, from the frame it came. The paint pass keeps when each came
/// (by node), so the host only sets the longer text; glyphs already placed never move. A node first painted within `dur`
/// of `born` (frame-clock seconds: when its words began) fades its first glyphs in too; one first painted later (scrolled
/// back into view) shows them at once.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct GlyphsArrive {
    pub dur: f32,
    pub by: f32,
    pub ease: Ease,
    pub born: f32,
}

impl GlyphsArrive {
    /// A glyph that came at `came`: its opacity and how far it still sits below its place (points) at frame time `time`.
    #[must_use]
    pub fn of(&self, came: f32, time: f32) -> (f32, f32) {
        let t = (time - came) / self.dur.max(1e-6);
        (self.ease.at(t), self.by * (1.0 - self.ease.at(t)))
    }
}

hyperion_api_layouts::api_layouts! {
    "Cue" => alias Cue,
    "GlyphsIn" => alias GlyphsIn,
    "GlyphsArrive" => alias GlyphsArrive,
    "Sound" => alias Sound,
    "Onset" => alias Onset,
    "Lasts" => alias Lasts,
    "Pitch" => alias Pitch,
    "Gain" => alias Gain,
    "Pan" => alias Pan,
    Ease,
    Voice,
}

/// Register the timeline's components before any scene resolves them.
pub fn register(world: &World) {
    world.component::<Cue>().writable();
    world.component::<GlyphsIn>().writable();
    world.component::<GlyphsArrive>().writable();
    world.component::<Sound>().writable();
    world.component::<Onset>().writable();
    world.component::<Lasts>().writable();
    world.component::<Pitch>().writable();
    world.component::<Gain>().writable();
    world.component::<Pan>().writable();
}
