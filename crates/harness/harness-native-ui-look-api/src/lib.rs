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
    world.component::<RibbonFrom>().add_trait::<flecs::Exclusive>().on_delete_target(flecs::Cleanup::Delete);
    world.component::<RibbonTo>().add_trait::<flecs::Exclusive>().on_delete_target(flecs::Cleanup::Delete);
}
