//! Flex layout components (the Clay model, docs/ui-model.md section 4). Plain data: layout reads these and writes `Laid`.
use harness_native_flecs::prelude::*;

/// Which way a container lays its children: along a row, down a column, or on top of one another (`ZStack`).
#[derive(Layout, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FlexAxis {
    Row,
    #[default]
    Column,
    Z,
}

/// Where children sit across the main axis (start, centre, end, stretched to the container).
#[derive(Layout, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
    Stretch,
}

/// Where children sit along the main axis when there is room left over.
#[derive(Layout, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Justify {
    #[default]
    Start,
    Center,
    End,
    SpaceBetween,
}

/// A container's flow. `gap` is between children; `wrap` starts a new line when a Row is out of room.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Flex {
    pub axis: FlexAxis,
    pub gap: f32,
    pub align: Align,
    pub justify: Justify,
    pub wrap: bool,
}

/// How one axis of a node is sized.
#[derive(Layout, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SizeKind {
    /// Its content (children plus padding and gaps, or the host-measured `Intrinsic`), within `min` and `max`.
    #[default]
    Fit,
    /// A share of the room left over, by `value` as weight (0 counts as 1), within `min` and `max`.
    Grow,
    /// Exactly `value` points.
    Fixed,
    /// `value` (0 to 1) of the parent's inner size.
    Percent,
}

/// One axis of [`Size`]. `max` of 0 means no maximum.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Sizing {
    pub kind: SizeKind,
    pub value: f32,
    pub min: f32,
    pub max: f32,
}

impl Sizing {
    pub const FIT: Self = Self { kind: SizeKind::Fit, value: 0.0, min: 0.0, max: 0.0 };
    pub const GROW: Self = Self { kind: SizeKind::Grow, value: 1.0, min: 0.0, max: 0.0 };

    #[must_use]
    pub const fn fixed(v: f32) -> Self {
        Self { kind: SizeKind::Fixed, value: v, min: 0.0, max: 0.0 }
    }

    /// `v` clamped to `min` and `max`.
    #[must_use]
    pub fn clamp(&self, v: f32) -> f32 {
        let v = v.max(self.min);
        if self.max > 0.0 { v.min(self.max) } else { v }
    }
}

/// A node's size on each axis.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub w: Sizing,
    pub h: Sizing,
}

/// Room inside a node, before its children, per side.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Padding {
    pub l: f32,
    pub r: f32,
    pub t: f32,
    pub b: f32,
}

/// Where a container's children sit along its main axis instead of its `Justify`: their middle `at` of the way along the
/// container (0.5 is the middle; padding counts toward the length), moved only as far as it takes to stay out of the
/// padding and inside the container. The empty window's block sits a little above the middle of the room it has this way.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Centre {
    pub at: f32,
}

/// Width over height: when one axis is `Grow` or `Fixed` and the other `Fit`, the other follows.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Aspect {
    pub ratio: f32,
}

/// Content size the host measured (a `Text` at its wrapped width, an `Icon`'s box): `w` is the unwrapped width and `h` the
/// height wrapped at `at` points, the width layout gave it when the host measured (see `harness_native_ui_layout::typeset`).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Intrinsic {
    pub w: f32,
    pub h: f32,
    pub at: f32,
}

/// The window's size in points (singleton, written by the host each resize).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Viewport {
    pub w: f32,
    pub h: f32,
}

/// How far a `Scroll`'s children are shifted (points scrolled).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollState {
    pub x: f32,
    pub y: f32,
}

/// `(AnchoredTo, node)`: an overlay is placed against `node`'s `Laid`. Exclusive; deleting the anchor removes the link.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct AnchoredTo;

/// Which side of its anchor an overlay opens on.
#[derive(Layout, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Opens {
    Top,
    #[default]
    Bottom,
    Left,
    Right,
}

/// The side an anchored node opens on, its nudge, and whether it may flip or slide to stay on screen.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Side {
    pub edge: Opens,
    pub dx: f32,
    pub dy: f32,
}

hyperion_api_layouts::api_layouts! {
    "Flex" => alias Flex,
    "Sizing" => alias Sizing,
    "Size" => alias Size,
    "Padding" => alias Padding,
    "Aspect" => alias Aspect,
    "Centre" => alias Centre,
    "Intrinsic" => alias Intrinsic,
    "Viewport" => alias Viewport,
    "ScrollState" => alias ScrollState,
    "AnchoredTo" => alias AnchoredTo,
    "Side" => alias Side,
    FlexAxis,
    Align,
    Justify,
    SizeKind,
    Opens,
}

/// Register the layout vocabulary before any scene or system resolves it.
pub fn register(world: &World) {
    world.component::<Sizing>().writable();
    world.component::<Flex>().writable();
    world.component::<Size>().writable();
    world.component::<Padding>().writable();
    world.component::<Aspect>().writable();
    world.component::<Centre>().writable();
    world.component::<Intrinsic>().writable();
    world.component::<Viewport>().writable();
    world.component::<ScrollState>().writable();
    world.component::<Side>().writable();
    world.component::<AnchoredTo>().add_trait::<flecs::Exclusive>().on_delete_target(flecs::Cleanup::Remove);
}
