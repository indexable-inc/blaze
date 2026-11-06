//! Geometry and draw order shared by layout, drawing and hit testing. Component names stay stable across the host and module images.
use harness_native_flecs::prelude::*;

/// Where a node was drawn last frame (view points) and its opacity: the hit-test truth.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub alpha: f32,
}

impl Rect {
    #[must_use]
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }
}

/// Draw order (low first) within its layer; on a part prefab of a generated element (`StatLabel`), its place among the parts.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Z {
    pub z: f32,
}

/// Marks an entity as a UI node: laid out, drawn and hit-tested by the generic systems. Every primitive prefab carries it.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Node;

/// On the root of a node tree whose geometry and paint belong to a host that lays it out inside something else (the
/// transcript's generated UI sits in its message's flow): the generic layout and draw passes skip the tree, so it holds
/// the primitives' components without being solved against the window. Remove it and the tree is an ordinary root.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Hosted;

/// A node whose `Laid` is written by someone else (the host's typesetter bridge puts a code view's squiggles on the glyphs they
/// underline, a card's rectangle on its code document): layout does not flow it into its parent but lays it out as a root of
/// its own. Without `(AnchoredTo, node)` it keeps the `Laid` position and size it has, and its children are laid out inside;
/// with one it opens against the anchor, as any overlay does. Draw still paints it with its parent's tree. A tag.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Placed;

/// On a floating node (a menu, a popover's plate, a hover card: an anchored or `Placed` node with a `Layer` of its own): it
/// and everything in it are hosted in a window of their own above the main one (on macOS a borderless child `NSPanel`,
/// harness-native's overlay.rs), so it reaches past the main window's edge as NSMenu and NSPopover do. Layout places and
/// clamps it against the screen (`Screen`, layout-api) instead of the viewport; draw marks every node in it with its id
/// (`Painted::window`), and the host draws those into its window, laid over its `Laid`. Geometry and hit testing stay in
/// the main view's points: the host reports the pointer over that window as a point of the main view. A tag, carried by
/// the overlay prefabs (`ui.Menu`, `ui.Overlay`, `hn.GenFloat`), so any overlay made from them is windowed by data.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Windowed;

/// On the root of a node tree that is out of the frame (a code card scrolled out of view, or one a definition made for a file
/// it is not showing): layout, draw and hit testing skip the whole tree. A tag.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Offscreen;

/// Where layout put a node, in window points (written by `harness-native-ui-layout`, read by draw and input). Hot per frame, so a
/// field value, never a tag or pair.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Laid {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Laid {
    #[must_use]
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }
}

/// A node layout left out this frame (a `ViewThatFits` variant that was not chosen): draw and hit testing skip it and
/// everything under it. Written by layout. A tag.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Hidden;

/// Children are cut to this node's `Laid` (a `Scroll`, a rounded plate). A tag.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Clip;

/// On a `Clip` node that scrolls: its children thin out over `len` points at an edge with more of them scrolled beyond it,
/// and cut hard at an edge they end at, so what goes under a neighbour (the chat's composer) fades instead of stopping
/// on a line.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct ClipFade {
    pub len: f32,
}

/// Which stack a root node is drawn and hit-tested in, bottom first.
#[derive(Layout, Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Level {
    #[default]
    Base,
    Overlay,
    Top,
}

/// The stack a node is drawn and hit-tested in (`Layer: {level: Overlay}`); absent means its root's (`Base` on a root). On a
/// node inside a tree it lifts that node and all in it to its level when that is above its tree's (a float a view opens).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Layer {
    pub level: Level,
}

hyperion_api_layouts::api_layouts! {
    "Rect" => alias Rect,
    "Z" => alias Z,
    "Node" => alias Node,
    "Hosted" => alias Hosted,
    "Placed" => alias Placed,
    "Windowed" => alias Windowed,
    "Offscreen" => alias Offscreen,
    "Laid" => alias Laid,
    "Clip" => alias Clip,
    "ClipFade" => alias ClipFade,
    "Hidden" => alias Hidden,
    "Layer" => alias Layer,
    Level,
}

/// The Node children of `e` in layout and paint order: flecs's own order when `e` carries `OrderedChildren`, else creation
/// order (entity id), which is the order a script declares them in.
#[must_use]
pub fn children<'a>(e: EntityView<'a>) -> Vec<EntityView<'a>> {
    let mut kids = Vec::new();
    e.each_child(|c| {
        if c.has(Node::id()) {
            kids.push(c);
        }
    });
    if !e.has(flecs::OrderedChildren::ID) {
        kids.sort_by_key(|c| *c.id());
    }
    kids
}

/// Geometry is written by layout; draw order is scene configuration.
///
/// The parts of a composite keep the order the scene declares them in, at every depth, because flecs copies `OrderedChildren`
/// onto every instance before it instantiates the parts (`(OnInstantiate, Copy)`, set in the prefab addon's bootstrap in our flecs fork: the
/// trait cannot be changed once the world is up). `Node` is a world alias of its own name: the primitives' script declares
/// it inside the `ui` scope (`prefab ui.Node { Node }`), where the bare name would otherwise find the prefab `ui.Node`
/// itself, and no primitive, nor any part of a composite, would be a node.
pub fn register(world: &World) {
    world.component::<Rect>();
    world.component::<Z>().writable();
    world.component::<Node>().writable().aliased();
    world.component::<Hosted>().writable();
    world.component::<Placed>().writable();
    world.component::<Windowed>().writable();
    world.component::<Offscreen>().writable();
    world.component::<Laid>().writable();
    world.component::<Clip>().writable();
    world.component::<ClipFade>().writable();
    world.component::<Hidden>();
    world.component::<Layer>().writable();
}
