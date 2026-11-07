//! Flex layout components (the Clay model, docs/ui-model.md section 4). Plain data: layout reads these and writes `Laid`.
use harness_native_flecs::prelude::*;
use harness_native_ui_spring_api::Spring;

/// Which way a container lays its children: along a row, down a column, or on top of one another (`ZStack`: placed across
/// by its `Flex.align` and down by its `Flex.justify`).
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

/// The room a windowed overlay (`Windowed`, node-api) is placed and kept in: the visible frame of the screen the main
/// window is on (less the menu bar and the Dock; the whole screen in full screen), in view points from the main view's
/// top-left corner, so `x` and `y` are usually negative. A singleton the host writes each frame. A world with no screen (a
/// headless test, a video) has none, and there the viewport is the screen.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Screen {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Caps a `Fit` height at `share` (0 to 1) of the height of its nearest ancestor whose height does not come from its
/// content (a `Grow`, `Fixed` or `Percent` one, such as a pane), less `keep` points (what else that ancestor shows
/// around it), and never under its `Size.h.min` or 1 point. Its `Size.h.max`, when set, caps it too; with no such ancestor
/// only that does. A text area that grows with its words stops this way near its pane's full height, then scrolls
/// (`ui.field.GrowingTextArea`). Solved in the height pass (`harness_native_ui_layout::solve`).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct GrowLimit {
    pub share: f32,
    pub keep: f32,
}

/// On a scroll that holds editable words (`ui.field.GrowingTextArea`): when the caret moves out of view its host glides
/// the scroll, on a spring of `omega` (rad/s) and `zeta`, until the caret's line shows `margin` points inside the view
/// (`harness_native_ui_view::field::FieldScroll`).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct CaretScroll {
    pub margin: f32,
    pub omega: f32,
    pub zeta: f32,
}

/// How far a `Scroll`'s children are shifted (points scrolled).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollState {
    pub x: f32,
    pub y: f32,
}

/// How big a scroll's content is (points, padding included, never less than the scroll itself): what its overlay scroller
/// and its host's physics measure against. Written by layout from the scroll's laid-out children, unless the scroll is
/// `ScrollFed`.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollExtent {
    pub w: f32,
    pub h: f32,
}

/// On a scroll whose content its host draws itself (a terminal's grid over its scrollback): the host writes its
/// `ScrollExtent` and `ScrollState`, and layout leaves the extent alone. A tag.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct ScrollFed;

/// An axis a scroll moves along.
#[derive(Layout, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ScrollAxis {
    /// Up and down: the knob on the trailing edge.
    #[default]
    Y,
    /// Across: the knob along the foot.
    X,
}

impl ScrollAxis {
    /// 0 across, 1 down: the index into `[w, h]` pairs.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::X => 0,
            Self::Y => 1,
        }
    }
}

/// A scroll's overlay scroller (macOS's overlay `NSScroller`), on every `ui.Scroll` through the `ui.Scroller` prefab
/// (prims.flecs; `NoScroller` leaves it off): a thin rounded knob over the content's trailing edge (along the foot for
/// the horizontal one) that shows while the content scrolls and `hold` seconds after, then fades on a spring; it takes no
/// layout space. With the pointer within `grab` points of the edge the knob widens from `thin` to `wide` over a faint
/// track; a drag on the knob scrolls in proportion, a press on the track pages toward it (keeping `keep` points of the
/// last page in view). The knob is never shorter than `min`, but shortens under a rubber band past an end. Sizes are points,
/// `inset` is the gap between the knob and the edge.
///
/// Its parts are child nodes (`ScrollerPart`): layout places them (`scroller_axis`), motion fades and widens them
/// (`ScrollerState`), input drives them (`ScrollerGrip`). A host that drives a scroll's physics itself adopts a
/// `ScrollState` it did not write as where the scroll now is.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Scroller {
    pub thin: f32,
    pub wide: f32,
    pub inset: f32,
    pub min: f32,
    pub grab: f32,
    pub hold: f32,
    pub keep: f32,
}

/// A part of its parent scroll's overlay scroller: the knob, or the track under it, along `axis`. Layout places it and
/// hides it while the scroller is hidden; its pointer is the arrow (prims.flecs). A node.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollerPart {
    pub axis: ScrollAxis,
    pub track: bool,
}

/// Leave a scroll's overlay scroller off: its parts stay hidden and the pointer never wakes them. A tag.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct NoScroller;

/// How shown a scroller is, written by motion: `shown` 0 (gone) to 1, and how wide each axis's knob is, 0 thin to 1
/// wide over its track; the scroll it last saw, how long it has been still since (seconds), and whether it has started
/// (a scroller flashes once when its scroll first appears, as AppKit's do).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollerState {
    pub shown: Spring,
    pub wide_x: Spring,
    pub wide_y: Spring,
    pub seen_x: f32,
    pub seen_y: f32,
    pub still: f32,
    pub started: bool,
}

impl ScrollerState {
    /// How wide `axis`'s knob is (0 thin, 1 wide).
    #[must_use]
    pub fn wide(&self, axis: ScrollAxis) -> f32 {
        match axis {
            ScrollAxis::X => self.wide_x.x,
            ScrollAxis::Y => self.wide_y.x,
        }
        .clamp(0.0, 1.0)
    }
}

/// What the pointer does to a scroller, written by input: the axes it is near (bit 1 down, bit 2 across), the axis a knob
/// drag holds (the same bits, 0 for none) and how far along the knob it took hold, a press on the track paging along `page`
/// toward `goal` on `glide`, and whether the button's press has been taken (`held`, until it is let go).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollerGrip {
    pub near: u8,
    pub drag: u8,
    pub grab: f32,
    pub page: u8,
    pub goal: f32,
    pub glide: Spring,
    pub held: bool,
}

/// The bit of `axis` in `ScrollerGrip`'s masks.
#[must_use]
pub const fn axis_bit(axis: ScrollAxis) -> u8 {
    match axis {
        ScrollAxis::Y => 1,
        ScrollAxis::X => 2,
    }
}

/// One axis of an overlay scroller as laid out: its track and knob (`[x, y, w, h]`, window points), where the knob's travel
/// starts along the axis, how far it travels (track less the knob at rest, unbanded), and how far the content scrolls.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollerAxis {
    pub track: [f32; 4],
    pub knob: [f32; 4],
    pub start: f32,
    pub travel: f32,
    pub max: f32,
    pub view: f32,
}

impl ScrollerAxis {
    /// Where the knob starts along the axis.
    #[must_use]
    pub fn knob_at(&self, axis: ScrollAxis) -> f32 {
        self.knob[axis.index()]
    }

    /// The scroll that puts the knob's start at `at` along the axis (clamped to the content).
    #[must_use]
    pub fn scroll_for(&self, at: f32) -> f32 {
        if self.travel <= 0.0 { 0.0 } else { ((at - self.start) / self.travel).clamp(0.0, 1.0) * self.max }
    }
}

/// Whether a scroll `view` (`[w, h]`) over content `extent` moves along `axis`.
#[must_use]
pub fn overflows(view: [f32; 2], extent: [f32; 2], axis: ScrollAxis) -> bool {
    let a = axis.index();
    extent[a] > view[a] + 0.5
}

/// Lay out `axis` of the overlay scroller `c` of a scroll laid at `laid` (`[x, y, w, h]`) over content `extent`, scrolled
/// `scroll`, its knob `wide` of the way from thin to wide. None when the content does not move along `axis`. The track is
/// a strip `wide + 2 * inset` across at the trailing edge (the foot, across), short of the corner when the other axis
/// scrolls too; the knob is as long as the share of the content in view (at least `min`), shortened by the part a rubber
/// band holds past an end (down to its thickness).
#[must_use]
pub fn scroller_axis(c: &Scroller, laid: [f32; 4], extent: [f32; 2], scroll: [f32; 2], wide: f32, axis: ScrollAxis) -> Option<ScrollerAxis> {
    let [x, y, w, h] = laid;
    let view = [w, h];
    if !overflows(view, extent, axis) {
        return None;
    }
    let other = match axis {
        ScrollAxis::X => ScrollAxis::Y,
        ScrollAxis::Y => ScrollAxis::X,
    };
    let strip = c.wide + 2.0 * c.inset;
    let corner = if overflows(view, extent, other) { strip } else { 0.0 };
    let a = axis.index();
    let thick = c.thin + (c.wide - c.thin) * wide.clamp(0.0, 1.0);
    let (lead, len) = ([x, y][a], view[a]);
    let start = lead + c.inset;
    let room = (len - corner - 2.0 * c.inset).max(0.0);
    let max = extent[a] - len;
    let rest = (room * len / extent[a]).max(c.min.min(room)).min(room);
    let travel = room - rest;
    let s = scroll[a];
    let over = if s < 0.0 { -s } else { (s - max).max(0.0) };
    let knob_len = (rest - over).max(thick.min(room));
    let at = if s <= 0.0 {
        0.0
    } else if s >= max {
        room - knob_len
    } else {
        s / max * travel
    };
    let (track, knob) = match axis {
        ScrollAxis::Y => ([x + w - strip, y, strip, h - corner], [x + w - c.inset - thick, start + at, thick, knob_len]),
        ScrollAxis::X => ([x, y + h - strip, w - corner, strip], [start + at, y + h - c.inset - thick, knob_len, thick]),
    };
    Some(ScrollerAxis { track, knob, start, travel, max, view: len })
}

/// Set a scroll's `ScrollState` without waking its scroller: a host following its content (a transcript stuck to its
/// foot as words arrive, a terminal on its live screen) moves it without the knob flashing at each step. A move with
/// `set` shows the knob.
pub fn scroll_quietly(e: EntityView<'_>, s: ScrollState) {
    if e.try_cloned::<&ScrollState>() != Some(s) {
        e.set(s);
    }
    if let Some(mut st) = e.try_cloned::<&ScrollerState>().filter(|st| st.started && (st.seen_x != s.x || st.seen_y != s.y)) {
        st.seen_x = s.x;
        st.seen_y = s.y;
        e.set(st);
    }
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

/// The side an anchored node opens on, its nudge, and where along that side it sits against its anchor: `align` Start
/// lines up their leading edges (left, or top on a Left or Right side), Center centres it on the anchor, End lines up
/// their trailing edges (any other `Align` is Start). Layout flips it to the other side and slides it to stay on screen.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Side {
    pub edge: Opens,
    pub dx: f32,
    pub dy: f32,
    pub align: Align,
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

/// A virtualized stack of rows of any height (a chat's transcript): only the rows in view, and `overscan` points more
/// either side, exist; the rows above and below them are spacers as tall as those rows were measured (`estimate` points
/// each before they ever were). Kept by `harness_native_ui_view::lazy_stack`; put it in a `ui.Scroll`. Layout lays it out as
/// a plain column.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct LazyStack {
    pub estimate: f32,
    pub overscan: f32,
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

/// A `Placed` node that still holds part of its place in its parent's flow: `k` of the length it was laid out at along the
/// parent's main axis (1 all of it, 0 none), at its spot among its siblings. Layout keeps its own size and moves it with
/// the flow; its neighbours get the rest. SwiftUI's list removal: a row going out folds away as it fades and the rows
/// after it slide up into the space. `harness_native_ui_view::leave` puts it on a node leaving a flow; motion eases it.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Collapse {
    pub k: f32,
}

/// The length this node's neighbours lay out against while its `Size` along its parent's main axis moves (a fold's spring
/// writes it each frame): layout gives the node its `Size`, and the children of every `LaysAtRest` node beside it (or under
/// a sibling of it) the room they have when it is this long. A sidebar folding on a spring says here the width at which its
/// fold leaves the card least room, so a terminal pane's grid resizes once per fold. Everything else, words included,
/// follows the spring: prose re-wraps live at the width it has each frame.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct RestSize {
    pub len: f32,
}

/// Its flow children are laid out in the room it gets while every moving `RestSize` (a sibling's, or a sibling's of an
/// ancestor) is at that length, not in the room it has this frame, for content that cannot follow a moving box: a terminal
/// pane's grid resizes once per fold, not every frame, while the pane's frame and its `Background` and `Overlay` children
/// follow the spring. Words never take it: they re-wrap live at the width their box has each frame (only their lines are
/// broken again, over glyphs shaped once), so a view of words and fields leaves it off. It places that content in the
/// frame it has now by its own `Flex` (a Start one rides its leading edge); past its frame it is clipped by whatever clips
/// it. A tag.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct LaysAtRest;

hyperion_api_layouts::api_layouts! {
    "RestSize" => alias RestSize,
    "LaysAtRest" => alias LaysAtRest,
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
    "LazyStack" => alias LazyStack,
    "LazyRange" => alias LazyRange,
    "Background" => alias Background,
    "Overlay" => alias Overlay,
    "SafeAreaInset" => alias SafeAreaInset,
    "SafeAreaPadding" => alias SafeAreaPadding,
    "ViewThatFits" => alias ViewThatFits,
    "Flex" => alias Flex,
    "Sizing" => alias Sizing,
    "Size" => alias Size,
    "GrowLimit" => alias GrowLimit,
    "CaretScroll" => alias CaretScroll,
    "Padding" => alias Padding,
    "Aspect" => alias Aspect,
    "Centre" => alias Centre,
    "Intrinsic" => alias Intrinsic,
    "Viewport" => alias Viewport,
    "Screen" => alias Screen,
    "ScrollState" => alias ScrollState,
    "ScrollExtent" => alias ScrollExtent,
    "ScrollFed" => alias ScrollFed,
    "Scroller" => alias Scroller,
    "ScrollerPart" => alias ScrollerPart,
    "NoScroller" => alias NoScroller,
    "ScrollerState" => alias ScrollerState,
    "ScrollerGrip" => alias ScrollerGrip,
    "ShrinkFirst" => alias ShrinkFirst,
    "AnchoredTo" => alias AnchoredTo,
    "Side" => alias Side,
    "Collapse" => alias Collapse,
    FlexAxis,
    Align,
    Justify,
    SizeKind,
    Opens,
    ScrollAxis,
}

/// Register the layout vocabulary before any scene or system resolves it.
pub fn register(world: &World) {
    world.component::<Sizing>().writable();
    world.component::<Flex>().writable();
    world.component::<Size>().writable();
    world.component::<GrowLimit>().writable();
    world.component::<CaretScroll>().writable();
    world.component::<Padding>().writable();
    world.component::<Aspect>().writable();
    world.component::<Centre>().writable();
    world.component::<Intrinsic>().writable();
    world.component::<Viewport>().writable();
    world.component::<Screen>().writable();
    world.component::<ShrinkFirst>().writable();
    world.component::<Collapse>().writable();
    world.component::<RestSize>().writable();
    world.component::<LaysAtRest>().writable();
    world.component::<ScrollState>().writable();
    world.component::<ScrollExtent>().writable();
    world.component::<ScrollFed>().writable();
    // Aliased (as `Grid` is): it shares its name with the prefab that carries it (`prefab ui.Scroller { Scroller: ... }`).
    world.component::<Scroller>().writable().aliased();
    world.component::<ScrollerPart>().writable();
    world.component::<NoScroller>().writable();
    world.component::<ScrollerState>();
    world.component::<ScrollerGrip>();
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
    world.component::<LazyStack>().writable();
    world.component::<LazyRange>();
    // Aliased, as `Node` is: inside the `ui` scope the bare `Overlay` would otherwise find the prefab `ui.Overlay`.
    world.component::<Background>().writable().aliased();
    world.component::<Overlay>().writable().aliased();
    world.component::<SafeAreaInset>().writable();
    world.component::<SafeAreaPadding>().writable();
    world.component::<ViewThatFits>().writable().aliased();
    world.component::<AnchoredTo>().add_trait::<flecs::Exclusive>().on_delete_target(flecs::Cleanup::Remove);
}
