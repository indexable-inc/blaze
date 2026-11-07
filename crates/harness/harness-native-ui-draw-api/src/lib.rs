//! What the draw pass decides, as components on the nodes themselves. `harness-native-ui-draw` (a hot-swappable module) walks the
//! trees and writes [`Painted`] on every node that draws something this frame: what kind of thing it is, where it comes in
//! paint order, how opaque it is after its ancestors' `Shown`, and the clip it sits in. The host reads the nodes in that
//! order and turns them into its own `Draw` for Metal or Vulkan (glyph atlas, image atlas and backdrop captures stay in
//! the host), reading the look and content components straight off each node. No command buffer, no handle between the
//! host and the module: the world is the only channel, so the module can be swapped at any frame.
use harness_native_flecs::prelude::*;
use harness_native_ui_node_api::Level;

/// What a painted node draws.
#[derive(Layout, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PaintKind {
    /// A look plate: `Fill`, `Edge`, `Shape`, `Lift`, `Frost`.
    #[default]
    Plate,
    /// Its `Text` in its `TextStyle`, inked by its `Fill`, wrapped at its `Laid` width.
    Glyphs,
    /// Its `Icon` glyph, inked by its `Fill`.
    Icon,
    /// Its `Mark`, a vector outline inked by its `Fill`.
    Mark,
    /// Its `Drawing`, a multi-colour vector drawing in its own colours.
    Drawing,
    /// Its `Picture`.
    Picture,
    /// A `Ribbon`: a band in its Fill between the nodes its `RibbonFrom` and `RibbonTo` name.
    Ribbon,
    /// Its `Vector`: SVG path data filled in its `Fill` and stroked in its `Stroke`, and its `VectorText`, if any.
    Vector,
}

/// A node as the draw pass resolved it for one frame (written by `harness-native-ui-draw`, read by the host's renderer). Only the nodes
/// whose `frame` is the [`Paint`] singleton's current frame are drawn, in the order of [`Painted::key`]: the root's stack and
/// `Z`, the root itself (its entity id), then the node's place in a walk of that root's tree.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Painted {
    pub frame: u32,
    pub level: Level,
    pub z: f32,
    pub root: u32,
    pub seq: u32,
    pub kind: PaintKind,
    /// Opacity with every ancestor's visibility folded in.
    pub alpha: f32,
    /// Whether an ancestor clips this node to the rectangle below (window points).
    pub clipped: bool,
    pub clip_x: f32,
    pub clip_y: f32,
    pub clip_w: f32,
    pub clip_h: f32,
    /// How far it thins out from that rectangle's top and foot (points; 0 is a hard cut): its clipping node's `ClipFade`
    /// at an edge with more scrolled beyond it.
    pub fade: [f32; 2],
    /// Where it is drawn (window points): its `Laid`, or its `Drawn` mid-slide, moved by every ancestor's transition.
    /// `w` and `h` are before `scale`; the host lays the node out in `w` by `h` at `(x, y)` and scales what it drew about
    /// `(x, y)`.
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Its own and its ancestors' transition scale (1 for none).
    pub scale: f32,
    /// The appearance its nearest `Scheme` says it is drawn in: 0 the window's, 1 dark, 2 light.
    pub scheme: u8,
    /// The window it is drawn in: 0 the main one, else the entity id of the `Windowed` overlay it is in (node-api), whose
    /// own window the host draws it into, over that overlay's `Laid`.
    pub window: u64,
}

impl Painted {
    /// Whether it is drawn in the dark half of its looks, in a window whose appearance is `window_dark`.
    #[must_use]
    pub fn dark(&self, window_dark: bool) -> bool {
        match self.scheme {
            1 => true,
            2 => false,
            _ => window_dark,
        }
    }

    /// The rectangle it is laid out in before `scale` (see [`Painted::x`]).
    #[must_use]
    pub fn rect(&self) -> [f32; 4] {
        [self.x, self.y, self.w, self.h]
    }
}

impl Painted {
    /// The key nodes are drawn in, low first.
    #[must_use]
    pub fn key(&self) -> (Level, i64, u32, u32) {
        (self.level, (self.z * 1024.0) as i64, self.root, self.seq)
    }
}

/// The draw pass's singleton: the frame number being painted, which the host advances before each `progress`. Nodes the pass
/// stopped painting (hidden, removed from a tree) are told apart from the live ones by an older `Painted.frame`.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Paint {
    pub frame: u32,
}

hyperion_api_layouts::api_layouts! {
    "Painted" => alias Painted,
    "Paint" => alias Paint,
    PaintKind,
}

/// Register the draw pass's components before any module or query uses them.
pub fn register(world: &World) {
    world.component::<Painted>();
    world.component::<Paint>().writable();
    world.set(Paint::default());
}
