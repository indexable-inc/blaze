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
    /// In a Row: children's first text baselines on one line (mixed text sizes line up). Elsewhere, `Start`.
    FirstBaseline,
    /// In a Row: children's last text baselines on one line. Elsewhere, `Start`.
    LastBaseline,
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

/// Out of room, this child gives up its length along its parent's main axis before its siblings do (down to its `min`):
/// a row's name cut with an ellipsis before its count or badge is. A tag: sugar for `LayoutPriority: {value: -1}` (a
/// `LayoutPriority` on the same node wins).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct ShrinkFirst;

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
    /// The first and last lines' baselines, points down from the top of the text (0: none; a box aligns by its bottom).
    pub base: f32,
    pub last: f32,
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

/// One child's cross-axis alignment, instead of its container's `Flex.align` (SwiftUI's per-child alignment).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct AlignSelf {
    pub align: Align,
}

/// Moves a child's alignment point by `(dx, dy)` inside it, so it sits that much the other way from where alignment alone
/// puts it, on its container's cross axis (both axes in a ZStack). SwiftUI's `.alignmentGuide`.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct AlignmentGuide {
    pub dx: f32,
    pub dy: f32,
}

/// When siblings do not fit along their container's main axis, lower `value`s give up room first (each down to its minimum)
/// before higher ones give any. Absent is 0. SwiftUI's `.layoutPriority`: the name truncates before the badge.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct LayoutPriority {
    pub value: f32,
}

/// Keep the natural size on an axis (`h` horizontal, `v` vertical): never shrunk or stretched there. SwiftUI's `.fixedSize`.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct FixedSize {
    pub h: bool,
    pub v: bool,
}

/// A two-dimensional table (SwiftUI's `Grid`): its Node children are rows (`GridRow`), their children cells. A column is as
/// wide as its widest cell (or as its `GridColumn` says), a row as tall as its tallest. `align` places cells across their
/// column, `valign` within their row; a `GridColumn` or a cell's `AlignSelf` overrides `align`.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Grid {
    pub hgap: f32,
    pub vgap: f32,
    pub align: Align,
    pub valign: Align,
}

/// A row of a `Grid`. A tag.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct GridRow;

/// One column of a `Grid`, as a non-Node child of it, in column order: how its cells align across it and how wide it is
/// (`Fit` its widest cell, `Fixed`, or `Grow` to share the grid's spare width).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct GridColumn {
    pub align: Align,
    pub w: Sizing,
}

/// Adaptive columns (SwiftUI's `LazyVGrid` with `.adaptive(minimum:maximum:)`): as many columns of at least `min` points as
/// fit across the container (gaps are its `Flex.gap`), each as wide as that leaves, up to `max` (0: no maximum). Children
/// fill them row by row. With `Lazy`, only the rows in view exist.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct GridColumns {
    pub min: f32,
    pub max: f32,
}

/// A virtualized stack (SwiftUI's `LazyVStack`): the container stands for `count` rows each `row` points along its main
/// axis, but only the rows in view (and `overscan` more either side) exist as entities, each placed by its `LazyIndex`.
/// Layout writes the range in view as `LazyRange`; `harness_native_ui_view::lazy_for_each` keeps the rows to it.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Lazy {
    pub count: u32,
    pub row: f32,
    pub overscan: u32,
}

/// Which of its `Lazy` container's rows a child is.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LazyIndex {
    pub i: u32,
}

/// The rows of a `Lazy` container in view, `first..last` (written by layout).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LazyRange {
    pub first: u32,
    pub last: u32,
}

/// A child laid out to its parent's whole frame and drawn behind the parent's other children, out of the flow (SwiftUI's
/// `.background`): `background : hn.Pill { Background }`. A tag.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Background;

/// A child laid out to its parent's whole frame and drawn over the parent's other children, out of the flow (SwiftUI's
/// `.overlay`). A tag.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Overlay;

/// How far in from each edge of this node content is safe (the title-bar strip on top): the host writes it on the window's
/// root. Nodes under it with `SafeAreaPadding` pad themselves out of it.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct SafeAreaInset {
    pub l: f32,
    pub r: f32,
    pub t: f32,
    pub b: f32,
}

/// Pad this node's content, on the edges set, by as much of the `SafeAreaInset` above it as the node overlaps (SwiftUI's
/// `.safeAreaPadding`): a node that reaches under the title bar keeps its frame, its content starts below the strip.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct SafeAreaPadding {
    pub l: bool,
    pub r: bool,
    pub t: bool,
    pub b: bool,
}

/// Its Node children are variants, widest first: layout shows the first whose natural width fits the width it is given
/// (else the last) and hides the rest (`Hidden`). SwiftUI's `ViewThatFits`. A tag.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct ViewThatFits;

hyperion_api_layouts::api_layouts! {
    "AlignSelf" => alias AlignSelf,
    "AlignmentGuide" => alias AlignmentGuide,
    "LayoutPriority" => alias LayoutPriority,
    "FixedSize" => alias FixedSize,
    "Grid" => alias Grid,
    "GridRow" => alias GridRow,
    "GridColumn" => alias GridColumn,
    "GridColumns" => alias GridColumns,
    "Lazy" => alias Lazy,
    "LazyIndex" => alias LazyIndex,
    "LazyRange" => alias LazyRange,
    "Background" => alias Background,
    "Overlay" => alias Overlay,
    "SafeAreaInset" => alias SafeAreaInset,
    "SafeAreaPadding" => alias SafeAreaPadding,
    "ViewThatFits" => alias ViewThatFits,
    "Flex" => alias Flex,
    "Sizing" => alias Sizing,
    "Size" => alias Size,
    "Padding" => alias Padding,
    "Aspect" => alias Aspect,
    "Centre" => alias Centre,
    "Intrinsic" => alias Intrinsic,
    "Viewport" => alias Viewport,
    "ScrollState" => alias ScrollState,
    "ShrinkFirst" => alias ShrinkFirst,
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
    world.component::<ShrinkFirst>().writable();
    world.component::<ScrollState>().writable();
    world.component::<Side>().writable();
    world.component::<AlignSelf>().writable();
    world.component::<AlignmentGuide>().writable();
    world.component::<LayoutPriority>().writable();
    world.component::<FixedSize>().writable();
    // Aliased (as `Node` is): each shares its name with the prefab that carries it (`prefab ui.Grid { Grid: ... }`).
    world.component::<Grid>().writable().aliased();
    world.component::<GridRow>().writable().aliased();
    world.component::<GridColumn>().writable().aliased();
    world.component::<GridColumns>().writable();
    world.component::<Lazy>().writable();
    world.component::<LazyIndex>().writable();
    world.component::<LazyRange>();
    // Aliased, as `Node` is: inside the `ui` scope the bare `Overlay` would otherwise find the prefab `ui.Overlay`.
    world.component::<Background>().writable().aliased();
    world.component::<Overlay>().writable().aliased();
    world.component::<SafeAreaInset>().writable();
    world.component::<SafeAreaPadding>().writable();
    world.component::<ViewThatFits>().writable().aliased();
    world.component::<AnchoredTo>().add_trait::<flecs::Exclusive>().on_delete_target(flecs::Cleanup::Remove);
}
