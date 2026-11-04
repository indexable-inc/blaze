//! The draw pass, a hot-swappable module. It decides what is drawn and in what order, and says so on the nodes: [`Painted`]
//! (kind, paint order, opacity after every ancestor's `Shown`, the clip it sits in). It draws nothing and keeps no state; the
//! host reads the painted nodes in order ([`painted`]) and renders each from its own look and content components.
//!
//! One system, `UiPaint`, walks every root `Node` tree (a node whose parent is not a Node), root by root, stamping the
//! frame number the host set in the [`Paint`] singleton before `progress`; [`painted`] sorts roots by layer, `Z` and id.
use harness_native_flecs::prelude::*;
use harness_native_ui_draw_api::{Paint, PaintKind, Painted};
use harness_native_ui_look_api::{Edge, Fill, Ribbon};
use harness_native_ui_motion_api::{Reveal, Shown};
use harness_native_ui_node_api::{Clip, Offscreen, Hosted, Laid, Layer, Level, Node, Z};
use harness_native_ui_text_api::{Icon, Picture, Text};

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
        walk(e, 1.0, None, stack, &mut 0);
    });
}

/// How shown a node is (1 without a Motion), eased from its Reveal's starting alpha.
fn shown(e: EntityView<'_>) -> f32 {
    let Some(s) = e.try_cloned::<&Shown>() else { return 1.0 };
    let from = e.try_cloned::<&Reveal>().map_or(0.0, |r| r.alpha0);
    from + (1.0 - from) * s.spring.x.clamp(0.0, 1.0)
}

fn walk(e: EntityView<'_>, parent_alpha: f32, clip: Option<Cut>, stack: Stack, seq: &mut u32) {
    let alpha = parent_alpha * shown(e);
    if alpha <= 0.001 {
        return;
    }
    let Some(laid) = e.try_cloned::<&Laid>() else { return };
    let kind = if e.has(Text::id()) {
        Some(PaintKind::Glyphs)
    } else if e.has(Icon::id()) {
        Some(PaintKind::Icon)
    } else if e.has(Picture::id()) {
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
        };
        if e.try_cloned::<&Painted>() != Some(next) {
            e.set(next);
        }
        *seq += 1;
    }
    // A clipping node cuts its children (not itself) to its rectangle, inside any clip it is already in.
    let inner = if e.has(Clip::id()) { Some(Cut::of(&laid).within(clip)) } else { clip };
    let mut kids: Vec<_> = harness_native_ui_node_api::children(e).into_iter().map(|c| (c.try_cloned::<&Z>().map_or(0.0, |z| z.z), c)).collect();
    kids.sort_by(|a, b| a.0.total_cmp(&b.0));
    for (_, c) in kids {
        walk(c, alpha, inner, stack, seq);
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
        harness_native_ui_draw_api::API_LAYOUTS
    ]
);
