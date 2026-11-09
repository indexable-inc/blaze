//! Appearance components shared by native views and UI modules. Component names stay stable across the host and module images.
use harness_native_flecs::prelude::*;

#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

/// Fill colour per appearance (sRGB, drawn as given).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Fill {
    pub dark: Rgba,
    pub light: Rgba,
}

/// The ink a `Vector`'s outline is stroked in, per appearance (sRGB, drawn as given; alpha 0: no stroke).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Stroke {
    pub dark: Rgba,
    pub light: Rgba,
}

/// Border colour (panels), ring colour (orbs).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Edge {
    pub dark: Rgba,
    pub light: Rgba,
}

/// radius, squircle exponent (2 is a circle arc, 5 is iOS-like continuous), border width, edge feather (points).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Shape {
    pub radius: f32,
    pub n: f32,
    pub border: f32,
    pub feather: f32,
}

/// The shadow a floating panel casts: `blur` points of it at `alpha` strength, `dy` points lower than the panel. All
/// zero (as Surface has) casts none.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Lift {
    pub blur: f32,
    pub alpha: f32,
    pub dy: f32,
}

/// How a Glass panel frosts what is behind it (the `Popover` material, scene/00-tokens): the backdrop blurred about
/// `radius` points out (each doubling of it is one more halving of the backdrop, so it is rounded to a power of two
/// in pixels), made `saturation` times as colourful (0 takes the tokens' `glass_saturation`) under the Fill's tint,
/// and a hairline just inside the edge lit `sheen` bright (an alpha, 0 for none), most along the top.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Frost {
    pub radius: f32,
    pub saturation: f32,
    pub sheen: f32,
}

/// Liquid glass over a Glass panel (os's backdrop glass): within `bevel` points of its edge the surface domes down,
/// bending what is behind it inward by up to `magnify` times its slope (each colour channel `dispersion` apart), and
/// the rim is lit by `rim`. The middle is frosted; `frost` is how frosted the bevel is (0 clear, 1 as the middle).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Lens {
    pub bevel: f32,
    pub magnify: f32,
    pub dispersion: f32,
    pub rim: f32,
    pub frost: f32,
}

/// The ink of a control's words and glyph: `rest`, and `hover` under the pointer.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Inks {
    pub rest: Fill,
    pub hover: Fill,
}

/// How a control gives under a press, with no plate (an icon button): held, it scales to `scale` and its opacity to
/// `dim`, easing there and springing back when let go on the spring `omega`, `zeta`.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct PressGive {
    pub scale: f32,
    pub dim: f32,
    pub omega: f32,
    pub zeta: f32,
}

/// A control's plate while it is pressed (the colours of a Fill, per appearance).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct PressLook {
    pub dark: Rgba,
    pub light: Rgba,
}

/// What a control shows while it is on or has the keys (colours per appearance): a switch's track, a field's hairline.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct OnLook {
    pub dark: Rgba,
    pub light: Rgba,
}

/// How a node's words are set: `size` and `line` height in points, `bold`, the colour its Fill.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct TextStyle {
    pub size: f32,
    pub line: f32,
    /// The line height of code blocks in markdown text (0: the text's own `line`).
    pub code_line: f32,
    /// Which surface the style was written for (0 transcript, 1 composer, 2 title, 3 rail names, 4 permission, 5
    /// palette, 6 chips and digits, 7 the Settings page), a note the text ignores, but for two that pick its face: 9 is
    /// code (the monospace face), 10 tabular figures (the text face, every digit as wide: a column of numbers).
    pub role: i32,
    pub bold: i32,
    /// The weight the sidebar's labels set their text in: 0 as `bold` says, 500 medium (the open row's label).
    pub weight: i32,
}

/// The inks a node takes under the pointer, mixed in by its `HoverAmt` (0 off it, 1 on it): its `Fill` (a plate's, a
/// vector's, words') toward `fill` when `set` has [`HoverInk::FILL`], its outline (a plate's `Edge`, a vector's `Stroke`)
/// toward `stroke` when it has [`HoverInk::STROKE`]. harness-native-render mixes them as it paints.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct HoverInk {
    pub fill: Fill,
    pub stroke: Fill,
    pub set: u8,
}

impl HoverInk {
    pub const FILL: u8 = 1;
    pub const STROKE: u8 = 2;
}

/// A hover affordance: the overlay drawn over (or around, `pad`) the node while the pointer is on it, fading on a
/// spring. `radius` < 0 keeps the node's own corner radius.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct HoverLook {
    pub dark: Rgba,
    pub light: Rgba,
    pub pad: f32,
    pub radius: f32,
}

/// A copy's pulse over the selection (`ui.selection` in prims.flecs; den's FlashLook, with a light that sweeps). The
/// selection's shape and glyphs grow by the share `grow` over `secs`, peaking `rise` of the way in, and come back
/// ([`CopyLook::grow_at`], den's `pulse`). Over `sweep` seconds a soft band of light crosses the whole shape left to right
/// ([`CopyLook::sweep_at`]): `band` points either side of its middle, leaning `tilt` points back for every point down, so
/// the rows below light a moment after the ones above, in `dark` or `light` (the theme's `tone_copy_sheen`; its alpha the
/// light's peak). It lies over the plate and the glyphs: `add_dark` and `add_light` say how much of it adds to what is
/// under (1: light on a dark page) rather than lying over it (0: a tint on a light page), and the glyphs it crosses take
/// the share `ink` of its colour at its middle. Under Reduce Motion nothing grows or moves: the whole shape brightens by
/// the light and fades back over `secs` ([`CopyLook::glow_at`]). The pulse's clock is a `CopyFlash`
/// (harness-native-ui-motion-api).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct CopyLook {
    pub secs: f32,
    pub rise: f32,
    pub grow: f32,
    pub sweep: f32,
    pub band: f32,
    pub tilt: f32,
    pub ink: f32,
    pub dark: Rgba,
    pub light: Rgba,
    pub add_dark: f32,
    pub add_light: f32,
}

impl CopyLook {
    /// How long a pulse lasts: the longer of the grow and the sweep, or under Reduce Motion (`still`) the glow's `secs`.
    #[must_use]
    pub fn length(&self, still: bool) -> f32 {
        if still { self.secs } else { self.secs.max(self.sweep) }
    }

    /// The grow `age` seconds in, 0 at rest and 1 at its peak: den's `pulse` (the os `selection_copy_scale` curve), up on
    /// an ease-out cubic over the first `rise` of `secs`, then down as `(1 - t)^3`.
    #[must_use]
    pub fn grow_at(&self, age: f32) -> f32 {
        if self.secs <= 0.0 || !(0.0..self.secs).contains(&age) {
            return 0.0;
        }
        let (p, rise) = (age / self.secs, self.rise.clamp(1e-3, 0.999));
        if p <= rise { 1.0 - (1.0 - p / rise).powi(3) } else { (1.0 - (p - rise) / (1.0 - rise)).powi(3) }
    }

    /// The sweep `age` seconds in: how far across the band is (0 just before the shape, 1 just past it, on an ease-in-out
    /// sine) and how bright it is (`sin(pi * t)`: it comes up and dies away rather than switching). None outside it.
    #[must_use]
    pub fn sweep_at(&self, age: f32) -> Option<(f32, f32)> {
        if self.sweep <= 0.0 || !(0.0..self.sweep).contains(&age) {
            return None;
        }
        let turn = std::f32::consts::PI * age / self.sweep;
        Some((0.5 - 0.5 * turn.cos(), turn.sin()))
    }

    /// Under Reduce Motion, how bright the shape is `age` seconds in: up and back as `sin(pi * t)` over `secs`.
    #[must_use]
    pub fn glow_at(&self, age: f32) -> f32 {
        if self.secs <= 0.0 || !(0.0..self.secs).contains(&age) {
            return 0.0;
        }
        (std::f32::consts::PI * age / self.secs).sin()
    }
}

/// A band of light sweeping through a node's words, left to right (Claude's and Cursor's "thinking..." shimmer): the sign
/// that something is at work, drawn as an effect on the words themselves rather than a mark beside them (harness-native-render
/// paint.rs lights each glyph by where its middle is). A pass crosses the words in `period` seconds, from just before
/// their first ink to just past their last, so it comes in from nothing and leaves to nothing; then the words rest `rest`
/// seconds before the next. `ease` bends the crossing from constant speed (0) toward an ease-in-out sine (1), the slow parts
/// falling where the band is half off the words. The band reaches `band` points either side of its middle on a smooth
/// falloff, and a glyph under its middle takes the share `strength` of `dark` or `light` (the appearance's ink: a theme
/// role), one outside it keeps its own ink. `bands` (1 to [`Sheen::MAX_BANDS`]) chase one another a `period / bands` apart.
/// Each node is `phase` of the way into its cycle at time 0, and with `scatter` also its id's own share of it, so several
/// never sweep in step. Under Reduce Motion nothing moves: every glyph takes `still` of `strength`, steady. A `strength`
/// or `period` of 0 is no sheen ([`Sheen::live`]): how a template's re-run stops one (it cannot remove it), and such a
/// node costs nothing to paint and keeps no frames coming. Colour glyphs (emoji) keep their own colours.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Sheen {
    pub period: f32,
    pub rest: f32,
    pub band: f32,
    pub strength: f32,
    pub still: f32,
    pub ease: f32,
    pub bands: u32,
    pub phase: f32,
    pub scatter: bool,
    pub dark: Rgba,
    pub light: Rgba,
}

impl Sheen {
    /// The most bands one sheen chases.
    pub const MAX_BANDS: u32 = 4;

    /// Whether it lights anything: a strength and a period.
    #[must_use]
    pub fn live(&self) -> bool {
        self.strength > 0.0 && self.period > 0.0
    }

    /// One pass and the rest after it, seconds.
    #[must_use]
    pub fn cycle(&self) -> f32 {
        self.period.max(0.0) + self.rest.max(0.0)
    }

    /// How far into its cycle node `id` starts (seconds, added to the clock): its `phase`, and with `scatter` its id's
    /// golden-ratio share of the cycle (Fibonacci hashing of its low bits), so neighbours land far apart.
    #[must_use]
    pub fn offset(&self, id: u64) -> f32 {
        let scattered = if self.scatter { (id as u32).wrapping_mul(0x9E37_79B9) as f32 / 4_294_967_296.0 } else { 0.0 };
        (self.phase + scattered).rem_euclid(1.0) * self.cycle()
    }

    /// Where band `k`'s middle is at time `t` (seconds, offset already added), in points from the words' left over words
    /// `span` points wide: from `band` before them to `band` past them. None while that band rests.
    #[must_use]
    pub fn middle(&self, t: f32, k: u32, span: f32) -> Option<f32> {
        if !self.live() {
            return None;
        }
        let n = self.bands.clamp(1, Self::MAX_BANDS);
        let u = (t - k as f32 * self.period / n as f32).rem_euclid(self.cycle()) / self.period;
        if u >= 1.0 {
            return None;
        }
        let sine = 0.5 - 0.5 * (std::f32::consts::PI * u).cos();
        let across = u + (sine - u) * self.ease.clamp(0.0, 1.0);
        let band = self.band.max(1.0);
        Some(-band + (span.max(0.0) + 2.0 * band) * across)
    }

    /// How far toward its ink a glyph whose middle is `x` points from the words' left goes at time `t` (0 to
    /// `strength`): the brightest band over it, each falling off smoothly to nothing `band` points from its middle. Under
    /// Reduce Motion (`still`), the same steady share everywhere.
    #[must_use]
    pub fn lit(&self, t: f32, span: f32, x: f32, still: bool) -> f32 {
        if !self.live() {
            return 0.0;
        }
        if still {
            return self.strength * self.still.clamp(0.0, 1.0);
        }
        let band = self.band.max(1.0);
        let most = (0..self.bands.clamp(1, Self::MAX_BANDS))
            .filter_map(|k| self.middle(t, k, span))
            .map(|m| {
                let d = ((x - m) / band).abs().min(1.0);
                1.0 - d * d * (3.0 - 2.0 * d)
            })
            .fold(0.0, f32::max);
        self.strength * most
    }
}

/// The text caret's one look (`ui.caret` in prims.flecs; den's CaretLook, shaders/den.slang's caret): a bar `w` points
/// wide (whole device pixels), `h` of the font's size tall about its line's middle, corners `radius` round. A jump farther
/// than its own height sweeps from where it was into where it is over `glide` seconds on an ease-out cubic (Ghostty's
/// cursor_sweep), the sweep's tail at half the bar's ink. It stays solid `hold` seconds after it last moved, then blinks
/// once every `blink` seconds, on for the first half, each turn eased over `fade` of the cycle. Drawn in the field's own
/// caret ink (`TextEdit`), by the shader (harness.slang, panel kind 4), so the blink costs no paint pass.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct CaretLook {
    pub w: f32,
    pub h: f32,
    pub radius: f32,
    pub glide: f32,
    pub hold: f32,
    pub blink: f32,
    pub fade: f32,
}

/// Struck-through words' one look (`ui.strike` in prims.flecs; inline markdown's `~~struck~~`): a hairline `w` points
/// thick (at least one device pixel), its middle `rise` of the words' size over their baseline (about the middle of the
/// x-height), drawn over the glyphs in the text's own ink at `alpha` of it.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct StrikeLook {
    pub w: f32,
    pub rise: f32,
    pub alpha: f32,
}

/// LaTeX math's one look (`ui.formula` in prims.flecs; harness-native-render's math.rs and paint.rs): a formula is set at
/// `inline_scale` (in a line of words) or `display_scale` (displayed, on a line of its own) times its words' size (Computer
/// Modern's x-height is smaller than Inter's, hence above 1), keeps `inline_pad` or `display_pad` points clear above and
/// below in its line, and the typesetter's strokes (a bracket's curve, a stretchy arrow) are `stroke` ems thick, as KaTeX
/// draws them.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct MathLook {
    pub inline_scale: f32,
    pub display_scale: f32,
    pub inline_pad: f32,
    pub display_pad: f32,
    pub stroke: f32,
}

/// A band across the node's width that joins two other nodes' heights, as a merge view joins a chunk in one pane to the same
/// chunk in the next: its left end spans the height of `(RibbonFrom, a)`, its right end that of `(RibbonTo, b)`, and its edges
/// ease between them. Drawn in the node's Fill, cut to the clip it sits in. An end shorter than `min` (a side with no lines
/// there) is drawn `min` tall about its middle.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Ribbon {
    pub min: f32,
}

/// The appearance a subtree is drawn in, whatever the window's (SwiftUI's `.environment(\.colorScheme, ...)`): every
/// Fill, Edge and look in it takes its `dark` half when `dark`, else its `light` half; the nearest ancestor's wins. A view
/// on a ground of its own says which half reads on it (the chat pane sits on the terminal's ground, which is dark in both
/// appearances unless the Ghostty theme says otherwise).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scheme {
    pub dark: bool,
}

/// `(RibbonFrom, node)` and `(RibbonTo, node)` on a [`Ribbon`]: the nodes its ends span. Exclusive; deleting either node
/// deletes the ribbon (it joins nothing then).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct RibbonFrom;
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct RibbonTo;

/// How Kip (harness-native-kip, the `Kip` content) is seen and lit, on the scene's `ui.kip` (kip.flecs; den's
/// MascotLook). The view looks down on him by `pitch` degrees with him turned `yaw` degrees from facing you. The key light
/// comes from `key_x`, `key_y`, `key_z` (the view's space: x right, y up, z toward you) at `key` strength; the fill from
/// the sky and the ground at `fill`; the rim from `rim_x`, `rim_y`, `rim_z` behind him at `rim`, as tight as `rim_power`. A
/// corner between cells darkens by `ao` (0 never, 1 to black) and a line shows between cells by `seams` once a cell is
/// several pixels across. His colours are `ui.kip`'s children's `Fill`s (`mascot_key_light`, `mascot_sky`,
/// `mascot_ground`, `mascot_rim`, and `mascot_shadow` for the contact shadow, its alpha how dark), his inks the
/// `MascotInk` children of `inks`. He comes and goes over `fade` seconds where a view shows and hides him.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct MascotLook {
    pub pitch: f32,
    pub yaw: f32,
    pub key_x: f32,
    pub key_y: f32,
    pub key_z: f32,
    pub key: f32,
    pub fill: f32,
    pub rim_x: f32,
    pub rim_y: f32,
    pub rim_z: f32,
    pub rim: f32,
    pub rim_power: f32,
    pub ao: f32,
    pub seams: f32,
    pub fade: f32,
}

/// How Kip moves, on `ui.kip` (harness-native-kip's anim.rs; den's MascotMotion). He breathes `breath_hz` times a second,
/// swelling by `breath` of his height; he blinks every `blink_min` to `blink_max` seconds (sometimes twice), each blink
/// `blink_len` seconds. His look turns toward the pointer on a critically damped spring of `look_omega` (radians a
/// second), as far as `look_yaw` degrees aside and `look_pitch` up or down, and his body turns to walk on one of
/// `turn_omega`. The antennae and arms are springs ringing at `*_hz` with damping ratio `*_damping`, pushed by the body's
/// acceleration times `*_gain`. A walk's stride is `stride` cells each way at `cadence` steps a second, reached and lost
/// at `accel` cells a second squared; a hop crouches for `anticipation` seconds, then flies `hop_time` seconds as high as
/// `hop_height` of his height. Idle for `sleep_after` seconds he dozes off; after something happens he breathes for
/// `awake` seconds, then rests still with a breath or a glance every `fidget_every`.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct MascotMotion {
    pub breath_hz: f32,
    pub breath: f32,
    pub blink_min: f32,
    pub blink_max: f32,
    pub blink_len: f32,
    pub look_omega: f32,
    pub look_yaw: f32,
    pub look_pitch: f32,
    pub turn_omega: f32,
    pub antenna_hz: f32,
    pub antenna_damping: f32,
    pub antenna_gain: f32,
    pub arm_hz: f32,
    pub arm_damping: f32,
    pub arm_gain: f32,
    pub stride: f32,
    pub cadence: f32,
    pub accel: f32,
    pub hop_height: f32,
    pub hop_time: f32,
    pub anticipation: f32,
    pub sleep_after: f32,
    pub awake: f32,
    pub fidget_every: f32,
}

/// One of Kip's moods (a child of `ui.kip` named for it: idle, thinking, working, happy, confused, sleeping, wave): a
/// change to it crossfades over `blend` seconds; it plays `speed` times as fast and moves `amount` as much as it was made
/// to; a one-shot (happy, wave) lasts `length` seconds, and confused settles after it.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct MascotClip {
    pub blend: f32,
    pub speed: f32,
    pub amount: f32,
    pub length: f32,
}

/// One of Kip's inks (a child of `ui.kip.inks` named `mascot_ink_<name>` for the ink his model names), coloured by its
/// `Fill`: it glows by `glow` (0 to 1) while he does (thinking).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct MascotInk {
    pub glow: f32,
}

hyperion_api_layouts::api_layouts! {
    "Ribbon" => alias Ribbon,
    "Rgba" => alias Rgba,
    "Fill" => alias Fill,
    "HoverInk" => alias HoverInk,
    "Edge" => alias Edge,
    "Stroke" => alias Stroke,
    "Shape" => alias Shape,
    "Lift" => alias Lift,
    "Frost" => alias Frost,
    "Lens" => alias Lens,
    "Inks" => alias Inks,
    "PressLook" => alias PressLook,
    "PressGive" => alias PressGive,
    "OnLook" => alias OnLook,
    "TextStyle" => alias TextStyle,
    "HoverLook" => alias HoverLook,
    "CopyLook" => alias CopyLook,
    "Sheen" => alias Sheen,
    "CaretLook" => alias CaretLook,
    "StrikeLook" => alias StrikeLook,
    "MathLook" => alias MathLook,
    "Scheme" => alias Scheme,
    "MascotLook" => alias MascotLook,
    "MascotMotion" => alias MascotMotion,
    "MascotClip" => alias MascotClip,
    "MascotInk" => alias MascotInk,
}

/// Register the appearance vocabulary before scenes or systems resolve it.
pub fn register(world: &World) {
    world.component::<Rgba>().writable();
    world.component::<Fill>().writable();
    world.component::<Edge>().writable();
    world.component::<Stroke>().writable();
    world.component::<Shape>().writable();
    world.component::<Lift>().writable();
    world.component::<Frost>().writable();
    world.component::<Lens>().writable();
    world.component::<Inks>().writable();
    world.component::<PressLook>().writable();
    world.component::<PressGive>().writable();
    world.component::<OnLook>().writable();
    world.component::<TextStyle>().writable();
    world.component::<HoverLook>().writable();
    world.component::<HoverInk>().writable();
    world.component::<CopyLook>().writable();
    world.component::<Sheen>().writable();
    world.component::<CaretLook>().writable();
    world.component::<StrikeLook>().writable();
    world.component::<MathLook>().writable();
    world.component::<Ribbon>().writable();
    world.component::<Scheme>().writable();
    world.component::<MascotLook>().writable();
    world.component::<MascotMotion>().writable();
    world.component::<MascotClip>().writable();
    world.component::<MascotInk>().writable();
    world.component::<RibbonFrom>().add_trait::<flecs::Exclusive>().on_delete_target(flecs::Cleanup::Delete);
    world.component::<RibbonTo>().add_trait::<flecs::Exclusive>().on_delete_target(flecs::Cleanup::Delete);
}

#[cfg(test)]
mod tests {
    use super::Sheen;

    fn sheen(period: f32, rest: f32, bands: u32) -> Sheen {
        Sheen { period, rest, band: 20.0, strength: 0.8, still: 0.4, ease: 0.5, bands, ..Sheen::default() }
    }

    /// A pass comes in from nothing before the words' first ink and leaves to nothing past their last, then the words
    /// rest unlit until the next pass.
    #[test]
    fn a_pass_crosses_from_nothing_to_nothing_then_rests() {
        let s = sheen(2.0, 1.0, 1);
        let span = 100.0;
        assert_eq!(s.middle(0.0, 0, span), Some(-20.0), "starts a band before the words");
        assert!((s.middle(2.0 - 1e-4, 0, span).unwrap() - 120.0).abs() < 0.01, "ends a band past them");
        for x in [0.0, 50.0, 100.0] {
            assert_eq!(s.lit(0.0, span, x, false), 0.0, "nothing lit as it starts: {x}");
            assert!(s.lit(2.0 - 1e-4, span, x, false) < 1e-3, "nothing lit as it ends: {x}");
            assert_eq!(s.lit(2.5, span, x, false), 0.0, "nothing lit while it rests: {x}");
        }
        assert_eq!(s.middle(2.5, 0, span), None, "it rests");
        assert_eq!(s.middle(3.0, 0, span), Some(-20.0), "and starts again a cycle on");
        // Halfway through the pass the band's middle is on the words' middle, lit at the full strength.
        assert!((s.lit(1.0, span, 50.0, false) - 0.8).abs() < 1e-4);
        assert_eq!(s.lit(1.0, span, 75.0, false), 0.0, "and nothing a band away");
    }

    /// The band moves only forward, left to right, through every pass.
    #[test]
    fn the_band_only_moves_right() {
        let s = sheen(1.3, 0.0, 1);
        let mut last = f32::MIN;
        for i in 0..130 {
            let m = s.middle(i as f32 * 0.01, 0, 80.0).expect("no rest: always crossing");
            assert!(m >= last, "went back at {i}");
            last = m;
        }
    }

    /// Two bands chase one another half a pass apart.
    #[test]
    fn two_bands_chase_half_a_pass_apart() {
        let s = Sheen { ease: 0.0, ..sheen(2.0, 0.0, 2) };
        let (a, b) = (s.middle(1.5, 0, 100.0).unwrap(), s.middle(1.5, 1, 100.0).unwrap());
        assert!(a > b, "the second trails the first");
        assert!((a - b - 70.0).abs() < 1e-3, "by half the travel ({a} {b})");
    }

    /// Under Reduce Motion every glyph is lit the same steady share, whatever the time; no strength is no sheen at all.
    #[test]
    fn reduce_motion_is_steady_and_strength_0_is_nothing() {
        let s = sheen(2.0, 1.0, 1);
        for t in [0.0, 0.7, 2.5] {
            for x in [0.0, 60.0] {
                assert!((s.lit(t, 100.0, x, true) - 0.32).abs() < 1e-6);
            }
        }
        let off = Sheen { strength: 0.0, ..s };
        assert!(!off.live());
        assert_eq!(off.lit(1.0, 100.0, 50.0, false), 0.0);
        assert_eq!(off.lit(1.0, 100.0, 50.0, true), 0.0);
    }

    /// Scattered, neighbouring nodes start far apart in their cycle; unscattered, only `phase` moves it.
    #[test]
    fn scatter_spreads_neighbours() {
        let s = Sheen { scatter: true, ..sheen(2.0, 1.0, 1) };
        let (a, b) = (s.offset(1000), s.offset(1001));
        assert!((0.0..3.0).contains(&a) && (0.0..3.0).contains(&b));
        assert!((a - b).abs() > 0.5, "{a} {b}");
        let fixed = Sheen { phase: 0.25, ..sheen(2.0, 2.0, 1) };
        assert_eq!((fixed.offset(1), fixed.offset(2)), (1.0, 1.0));
    }
}
