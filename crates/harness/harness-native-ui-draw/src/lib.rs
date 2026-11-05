//! The draw pass, a hot-swappable module. It decides what is drawn and in what order, and says so on the nodes: [`Painted`]
//! (kind, paint order, opacity after every ancestor's `Shown`, the clip it sits in). It draws nothing and keeps no state; the
//! host reads the painted nodes in order ([`painted`]) and renders each from its own look and content components.
//!
//! One system, `UiPaint`, walks every root `Node` tree (a node whose parent is not a Node), root by root, stamping the
//! frame number the host set in the [`Paint`] singleton before `progress`; [`painted`] sorts roots by layer, `Z` and id.
use harness_native_flecs::prelude::*;
use harness_native_ui_draw_api::{Paint, PaintKind, Painted};
use harness_native_ui_look_api::{Edge, Fill, Ribbon, Scheme};
use harness_native_ui_layout_api::{Background, Overlay};
use harness_native_ui_motion_api::{Drawn, Pose, Reveal, Shown};
use harness_native_ui_node_api::{Clip, Hidden, Hosted, Laid, Layer, Level, Node, Offscreen, Z};
use harness_native_ui_text_api::{BlazeMark, Icon, Mark, Picture, Text};

/// A clip rectangle in window points.
#[derive(Clone, Copy)]
struct Cut {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Cut {
    fn of(l: &Laid) -> Self {
        Self { x: l.x, y: l.y, w: l.w, h: l.h }
    }

    fn within(self, o: Option<Self>) -> Self {
        let Some(o) = o else { return self };
        let (x0, y0) = (self.x.max(o.x), self.y.max(o.y));
        let (x1, y1) = ((self.x + self.w).min(o.x + o.w), (self.y + self.h).min(o.y + o.h));
        Self { x: x0, y: y0, w: (x1 - x0).max(0.0), h: (y1 - y0).max(0.0) }
    }
}

/// Where a root stands in the draw order, and the frame being painted.
#[derive(Clone, Copy)]
struct Stack {
    frame: u32,
    level: Level,
    z: f32,
    root: u32,
}

/// Create the draw systems in the module scope; deleting that scope removes them before unloading.
pub fn install(world: WorldRef<'_>) {
    world.system_named::<&Node>("UiPaint").without(Hosted::id()).without(Offscreen::id()).each_iter(|it, row, _| {
        let e = it.entity(row);
        if e.parent().is_some_and(|p| p.has(Node::id())) {
            return;
        }
        let Some(frame) = it.world().try_get::<&Paint>(|p| p.frame) else { return };
        let level = e.try_cloned::<&Layer>().map_or_else(Default::default, |l| l.level);
        let z = e.try_cloned::<&Z>().map_or(0.0, |z| z.z);
        let stack = Stack { frame, level, z, root: *e.id() as u32 };
        walk(e, 1.0, Affine::ID, None, 0, stack, &mut 0);
    });
}

/// A uniform scale then a move, window points: `p * s + t`. What a node's transitions (its own and its ancestors') do to it.
#[derive(Clone, Copy)]
struct Affine {
    s: f32,
    tx: f32,
    ty: f32,
}

impl Affine {
    const ID: Self = Self { s: 1.0, tx: 0.0, ty: 0.0 };

    fn at(self, x: f32, y: f32) -> (f32, f32) {
        (x * self.s + self.tx, y * self.s + self.ty)
    }

    /// `inner` first, then `self`.
    fn then(self, inner: Self) -> Self {
        Self { s: self.s * inner.s, tx: inner.tx * self.s + self.tx, ty: inner.ty * self.s + self.ty }
    }
}

/// How shown a node is (1 without a Motion or a Pose) and what its Reveal and Pose do to it there: its alpha eased from the
/// Reveal's and faded by the Pose's, and its move and scale about the middle of `r` (its rectangle).
fn shown(e: EntityView<'_>, r: Cut) -> (f32, Affine) {
    let (alpha, affine) = revealed(e, r);
    let Some(p) = e.try_cloned::<&Pose>() else { return (alpha, affine) };
    // Its Pose after its Reveal: scaled about its middle, then moved.
    let sc = 1.0 - p.shrink;
    let (cx, cy) = (r.x + r.w * 0.5, r.y + r.h * 0.5);
    let pose = Affine { s: sc, tx: cx - cx * sc + p.dx, ty: cy - cy * sc + p.dy };
    (alpha * (1.0 - p.fade).clamp(0.0, 1.0), pose.then(affine))
}

/// The Reveal half of [`shown`].
fn revealed(e: EntityView<'_>, r: Cut) -> (f32, Affine) {
    let Some(s) = e.try_cloned::<&Shown>() else { return (1.0, Affine::ID) };
    let k = s.spring.x.clamp(0.0, 1.0);
    let rv = e.try_cloned::<&Reveal>().unwrap_or(Reveal { alpha0: 0.0, dx: 0.0, dy: 0.0, scale0: 1.0 });
    let alpha = rv.alpha0 + (1.0 - rv.alpha0) * k;
    // A scale0 of 0 or less is no scale (a Reveal written only for its alpha); a Scale transition writes at least 0.01.
    let scale0 = if rv.scale0 > 0.0 { rv.scale0 } else { 1.0 };
    let sc = scale0 + (1.0 - scale0) * k;
    let (cx, cy) = (r.x + r.w * 0.5, r.y + r.h * 0.5);
    let off = 1.0 - k;
    (alpha, Affine { s: sc, tx: cx - cx * sc + rv.dx * off, ty: cy - cy * sc + rv.dy * off })
}

fn walk(e: EntityView<'_>, parent_alpha: f32, parent: Affine, clip: Option<Cut>, scheme: u8, stack: Stack, seq: &mut u32) {
    if e.has(Hidden::id()) {
        return;
    }
    let Some(laid) = e.try_cloned::<&Laid>() else { return };
    // Mid-slide (MatchedGeometry) it is drawn at its Drawn frame and its children move with it.
    let drawn = e.try_cloned::<&Drawn>();
    let frame = drawn.map_or(Cut::of(&laid), |d| Cut { x: d.x, y: d.y, w: d.w, h: d.h });
    let (own_alpha, own) = shown(e, frame);
    let alpha = parent_alpha * own_alpha;
    if alpha <= 0.001 {
        return;
    }
    let mine = parent.then(own);
    let scheme = e.try_cloned::<&Scheme>().map_or(scheme, |s| if s.dark { 1 } else { 2 });
    let (vx, vy) = mine.at(frame.x, frame.y);
    // Children: laid out against `laid`, drawn where the frame went.
    let kids_affine = mine.then(Affine { s: 1.0, tx: frame.x - laid.x, ty: frame.y - laid.y });
    let kind = if e.has(Text::id()) {
        Some(PaintKind::Glyphs)
    } else if e.has(Mark::id()) {
        Some(PaintKind::Mark)
    } else if e.has(Icon::id()) {
        Some(PaintKind::Icon)
    } else if e.has(Picture::id()) || e.has(BlazeMark::id()) {
        Some(PaintKind::Picture)
    } else if e.has(Ribbon::id()) {
        Some(PaintKind::Ribbon)
    } else if e.has(Fill::id()) || e.has(Edge::id()) {
        Some(PaintKind::Plate)
    } else {
        None
    };
    if let Some(kind) = kind {
        let c = clip.unwrap_or(Cut { x: 0.0, y: 0.0, w: 0.0, h: 0.0 });
        let next = Painted {
            frame: stack.frame,
            level: stack.level,
            z: stack.z,
            root: stack.root,
            seq: *seq,
            kind,
            alpha,
            clipped: clip.is_some(),
            clip_x: c.x,
            clip_y: c.y,
            clip_w: c.w,
            clip_h: c.h,
            x: vx,
            y: vy,
            w: frame.w,
            h: frame.h,
            scale: mine.s,
            scheme,
        };
        if e.try_cloned::<&Painted>() != Some(next) {
            e.set(next);
        }
        *seq += 1;
    }
    // A clipping node cuts its children (not itself) to its rectangle, inside any clip it is already in.
    let seen = Cut { x: vx, y: vy, w: frame.w * mine.s, h: frame.h * mine.s };
    let inner = if e.has(Clip::id()) { Some(seen.within(clip)) } else { clip };
    // A Background child first, an Overlay child last, the rest by Z in between.
    let layer = |c: EntityView<'_>| if c.has(Background::id()) { 0u8 } else if c.has(Overlay::id()) { 2 } else { 1 };
    let mut kids: Vec<_> = harness_native_ui_node_api::children(e).into_iter().map(|c| (layer(c), c.try_cloned::<&Z>().map_or(0.0, |z| z.z), c)).collect();
    kids.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
    for (_, _, c) in kids {
        walk(c, alpha, kids_affine, inner, scheme, stack, seq);
    }
}

/// The nodes painted this frame, in paint order: each one's entity id and what the draw pass said of it. The host calls this
/// after `progress` and renders each from the node's own components.
#[must_use]
pub fn painted(world: WorldRef<'_>) -> Vec<(u64, Painted)> {
    let Some(frame) = world.try_get::<&Paint>(|p| p.frame) else { return Vec::new() };
    let mut out = Vec::new();
    world.new_query::<&Painted>().each_entity(|e, p| {
        if p.frame == frame {
            out.push((*e.id(), *p));
        }
    });
    out.sort_by_key(|(_, p)| p.key());
    out
}

harness_native_api::module!(
    "draw",
    install,
    [
        harness_native_api::API_LAYOUTS,
        harness_native_ui_node_api::API_LAYOUTS,
        harness_native_ui_look_api::API_LAYOUTS,
        harness_native_ui_text_api::API_LAYOUTS,
        harness_native_ui_motion_api::API_LAYOUTS,
        harness_native_ui_layout_api::API_LAYOUTS,
        harness_native_ui_draw_api::API_LAYOUTS
    ]
);
