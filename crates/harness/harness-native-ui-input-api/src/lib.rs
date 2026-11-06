//! Pointer affordances and input relationships shared by native UI modules. Component names stay stable across the host and module images.
use harness_native_flecs::prelude::*;

/// Responds to the pointer: hit-tested topmost first, the `(Hovered, e)` target while under it.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Hoverable;

/// The system pointer's shape, as macOS names them (harness-native's window.rs maps each to an NSCursor). There is no wait
/// cursor: the app never blocks. macOS conventions, not the web's: controls (buttons, rows, chips) keep the arrow.
#[derive(Layout, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Pointer {
    #[default]
    Arrow,
    /// Over text to select or type into, and while a selecting drag goes on.
    Text,
    /// Over text set in vertical lines.
    VerticalText,
    /// Over a real link (words that open a URL or a file). Never on buttons, rows or chips: native macOS keeps the arrow
    /// over controls.
    Hand,
    /// A press on what can be carried (a tab, a sidebar row, a panel's head) that has nearly moved far enough to pick
    /// it up.
    OpenHand,
    /// While a tab or a panel is carried.
    Grabbing,
    /// Over a draggable edge between things side by side (the sidebar's, a divider), or dragging it.
    ResizeLeftRight,
    /// A draggable edge at its rightmost: it only goes left (NSSplitView's divider at its limit).
    ResizeLeft,
    /// A draggable edge at its leftmost: it only goes right.
    ResizeRight,
    /// Over a draggable edge between things one above the other, or dragging it.
    ResizeUpDown,
    /// A carry over a place that will not take it: letting go there does nothing.
    NotAllowed,
}

/// The pointer's shape over a node and everything inside it (`Cursor: {shape: Text}`), said on the primitives and prefabs
/// in script (prims.flecs: `ui.Field` is Text, `ui.Button` Arrow) so views inherit it. A Cursor makes its node a hit target
/// (Flecs adds Hoverable with it). The input pass (harness-native-ui-input) resolves the topmost hit node's Cursor, or its
/// nearest ancestor's, into [`PointerShape`]; a node that says none is the arrow.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub shape: Pointer,
}

impl Cursor {
    pub const ARROW: Self = Self { shape: Pointer::Arrow };
    pub const TEXT: Self = Self { shape: Pointer::Text };
}

/// The pointer's shape as the scene asks for it, singleton, written by the input pass when it changes: the topmost hit node's
/// `Cursor` (or its nearest ancestor's; the arrow when none says one), held from a press until its release so a drag keeps
/// the shape it began with. `node` is false where no hit node is under the pointer (or was pressed): the host's own zones
/// (a terminal grid, a draggable edge) decide there, and it composes the final shape (harness-native's
/// `App::pointer_shape`).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PointerShape {
    pub shape: Pointer,
    pub node: bool,
}

/// (Hovered, e) on the app: what the pointer is on.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Hovered;

/// A button-like node. Flecs adds Hoverable and Focusable with it.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Pressable;

/// A node that can receive keyboard focus.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Focusable;

/// `(Pressed, e)` on the app: the node the pointer went down on, until it is let go.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Pressed;

/// `(Focused, e)` on the app: the node that has the keys.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Focused;

/// The part of a node that takes the pointer, in points from its laid top left (a canvas shape's bounds, where the shape
/// is laid over its whole canvas). Without it the whole laid rectangle does.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct HitBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// A node that can be carried or dragged by a press-and-move.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Draggable;

/// Words shown in the system tooltip while the pointer rests on this node (or a node inside it).
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Tooltip {
    pub text: ScriptString,
}

/// What a Pressable does when activated: feature systems observe nodes carrying [`Activated`] and read `name`.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Action {
    pub name: ScriptString,
}

/// Added by the input system on the frame a Pressable was released over, removed at the start of the next. An event as a
/// tag: it changes on user actions only, never per frame.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Activated;

/// A Field's text and caret (a byte offset); Focusable comes with it.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Editable {
    pub buffer: ScriptString,
    pub caret: i32,
}

/// The pointer, singleton, written by the host from window events: position in window points, whether the primary button
/// is down, and whether it went down or up this frame.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct PointerState {
    pub x: f32,
    pub y: f32,
    pub down: bool,
    pub went_down: bool,
    pub went_up: bool,
}

hyperion_api_layouts::api_layouts! {
    "Pressed" => alias Pressed,
    "Focused" => alias Focused,
    "Draggable" => alias Draggable,
    "HitBox" => alias HitBox,
    "Action" => alias Action,
    "Activated" => alias Activated,
    "Editable" => alias Editable,
    "PointerState" => alias PointerState,
    "Pressable" => alias Pressable,
    "Focusable" => alias Focusable,
    "Hoverable" => alias Hoverable,
    "Cursor" => alias Cursor,
    "PointerShape" => alias PointerShape,
    "Hovered" => alias Hovered,
    Pointer,
}

/// Register input relationships before any query or scene uses them.
pub fn register(world: &World) {
    world.component::<Hoverable>().writable();
    world.component::<Focusable>().writable();
    world.component::<HitBox>().writable();
    world.component::<Pressable>().with(Hoverable::id()).with(Focusable::id()).writable();
    // A node that says how the pointer looks over it takes the pointer: the input pass hit-tests it.
    world.component::<Cursor>().with(Hoverable::id()).writable();
    world.component::<PointerShape>().writable();
    world.component::<Hovered>().add_trait::<flecs::Exclusive>().on_delete_target(flecs::Cleanup::Remove);
    world.component::<Pressed>().add_trait::<flecs::Exclusive>().on_delete_target(flecs::Cleanup::Remove);
    world.component::<Focused>().add_trait::<flecs::Exclusive>().on_delete_target(flecs::Cleanup::Remove);
    world.component::<Draggable>().writable();
    world.component::<Action>().writable();
    world.component::<Tooltip>().writable();
    world.component::<Activated>().writable();
    world.component::<Editable>().with(Focusable::id()).writable();
    world.component::<PointerState>().writable();
    // The pointer is a singleton the input pass runs on: present from the start, the host writes it from window events.
    world.set(PointerState::default());
    world.set(PointerShape::default());
}
