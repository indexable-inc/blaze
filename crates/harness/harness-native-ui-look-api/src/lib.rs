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

/// A hover affordance: the overlay drawn over (or around, `pad`) the node while the pointer is on it, fading on a
/// spring. `radius` < 0 keeps the node's own corner radius.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct HoverLook {
    pub dark: Rgba,
    pub light: Rgba,
    pub pad: f32,
    pub radius: f32,
}

/// A copy's pulse over the selection (`ui.selection` in prims.flecs; den's FlashLook): it lasts `secs`; the selection's
/// shape and glyphs grow by the share `grow`, peaking `rise` of the way in, and come back; a light of the plate's colour at
/// alpha `light` crosses each row over the whole pulse, the rows joining `stagger` seconds apart, each taking `row_in`
/// seconds to come in. The pulse itself is a `CopyFlash` (harness-native-ui-motion-api).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct CopyLook {
    pub secs: f32,
    pub rise: f32,
    pub grow: f32,
    pub light: f32,
    pub stagger: f32,
    pub row_in: f32,
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
    world.component::<CopyLook>().writable();
    world.component::<Ribbon>().writable();
    world.component::<Scheme>().writable();
    world.component::<MascotLook>().writable();
    world.component::<MascotMotion>().writable();
    world.component::<MascotClip>().writable();
    world.component::<MascotInk>().writable();
    world.component::<RibbonFrom>().add_trait::<flecs::Exclusive>().on_delete_target(flecs::Cleanup::Delete);
    world.component::<RibbonTo>().add_trait::<flecs::Exclusive>().on_delete_target(flecs::Cleanup::Delete);
}
