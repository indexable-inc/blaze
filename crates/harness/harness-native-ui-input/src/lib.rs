//! The input pass, a hot-swappable module. A collect system gathers every hoverable `Node` with its `Laid` and draw key; a
//! step system, run once a frame by the `PointerState` singleton the host writes from window events, resolves the topmost
//! hit (by layer, `Z`, depth, then creation order; a node on its way out, `Leaving` or under one, is never hit) into
//! `(Hovered, node)` on the app entity; a press on a Pressable sets
//! `(Pressed, node)`, on a Focusable `(Focused, node)`; letting go over the same Pressable tags it `Activated` so feature
//! systems can observe the `Action`. With no hoverable node in the world the pass does nothing (the chrome that is not yet
//! nodes keeps its own `Hovered`). The host ends the frame with [`end_frame`].
//!
//! The same step resolves the pointer's shape: the topmost hit's `Cursor`, or its nearest ancestor's (the arrow when none
//! says one), held from a press until its release, into the `PointerShape` singleton when it changes. The host puts its own
//! zones and drags over it and sets the system pointer (harness-native's `App::pointer_shape` and window.rs).
//!
//! The state is the per-frame hit list shared by the two systems, rebuilt every frame, and the shape a press holds;
//! everything else is in the world.
use core::cell::Cell;
use core::cell::RefCell;
use std::rc::Rc;

use harness_native_flecs::prelude::*;
use harness_native_api::APP;
use harness_native_ui_input_api::{
    Activated, Cursor, Focusable, Focused, Hoverable, Hovered, Pointer, PointerShape, PointerState, Pressable, Pressed,
};
use harness_native_ui_motion_api::Leaving;
use harness_native_ui_node_api::{Clip, Hosted, Laid, Layer, Level, Node, Offscreen, Z};

struct Hit {
    id: u64,
    laid: Laid,
    /// Where its `Clip` ancestors let it be hit (their rects intersected); None: nowhere clips it.
    clip: Option<Laid>,
    /// Paint order: bigger is on top.
    key: (Level, f32, u32, u64),
    pressable: bool,
    focusable: bool,
    /// Its `Cursor`, or its nearest ancestor's.
    cursor: Option<Pointer>,
}

/// What the topmost hit under the pointer is: its id, whether it is Pressable and Focusable, and its pointer shape.
#[derive(Clone, Copy)]
struct Top {
    id: u64,
    pressable: bool,
    focusable: bool,
    cursor: Pointer,
}

/// Create the input systems in the module scope; deleting that scope removes them before unloading.
pub fn install(world: WorldRef<'_>) {
    let hits: Rc<RefCell<Vec<Hit>>> = Rc::default();
    let sink = hits.clone();
    world.system_named::<(&Node, &Laid)>("UiHits").with(Hoverable::id()).each_entity(move |e, (_, laid)| {
        let (mut depth, mut root) = (0u32, e);
        let mut clip: Option<Laid> = None;
        let mut cursor = e.try_cloned::<&Cursor>().map(|c| c.shape);
        // A view going out on its Transition (an overlay closing, a row folding away) still draws but takes no pointer.
        let mut leaving = e.has(Leaving::id());
        while let Some(p) = root.parent().filter(|p| p.has(Node::id())) {
            depth += 1;
            root = p;
            leaving |= p.has(Leaving::id());
            if cursor.is_none() {
                cursor = p.try_cloned::<&Cursor>().map(|c| c.shape);
            }
            if p.has(Clip::id()) && let Some(c) = p.try_cloned::<&Laid>() {
                clip = Some(match clip {
                    None => c,
                    Some(a) => {
                        let (x, y) = (a.x.max(c.x), a.y.max(c.y));
                        Laid { x, y, w: ((a.x + a.w).min(c.x + c.w) - x).max(0.0), h: ((a.y + a.h).min(c.y + c.h) - y).max(0.0) }
                    }
                });
            }
        }
        if leaving || root.has(Offscreen::id()) || root.has(Hosted::id()) {
            return;
        }
        let level = root.try_cloned::<&Layer>().map_or_else(Default::default, |l| l.level);
        let z = e.try_cloned::<&Z>().map_or(0.0, |z| z.z);
        sink.borrow_mut().push(Hit {
            id: *e.id(),
            laid: *laid,
            clip,
            key: (level, z, depth, *e.id()),
            pressable: e.has(Pressable::id()),
            focusable: e.has(Focusable::id()),
            cursor,
        });
    });
    let app = *world.lookup(APP).id();
    // The shape a press holds until its release (None: the press was on no node), while the button is down.
    let held: Cell<Option<Option<Pointer>>> = Cell::new(None);
    world.system_named::<&PointerState>("UiPointer").each_iter(move |it, _, p| {
        let mut hits = hits.borrow_mut();
        hits.sort_by(|a, b| a.key.0.cmp(&b.key.0).then(a.key.1.total_cmp(&b.key.1)).then(a.key.2.cmp(&b.key.2)).then(a.key.3.cmp(&b.key.3)));
        let top = hits
            .iter()
            .rev()
            .find(|h| h.laid.contains(p.x, p.y) && h.clip.is_none_or(|c| c.contains(p.x, p.y)))
            .map(|h| Top { id: h.id, pressable: h.pressable, focusable: h.focusable, cursor: h.cursor.unwrap_or_default() });
        let any = !hits.is_empty();
        hits.clear();
        let w = it.world();
        shape(w, &held, top.map(|t| t.cursor), *p);
        if any {
            apply(w.entity_from_id(app), top, *p);
        }
    });
}

/// Resolve the pointer's shape from `over` (the topmost hit's) and the press that may hold one, into `PointerShape` when it
/// changes.
fn shape(w: WorldRef<'_>, held: &Cell<Option<Option<Pointer>>>, over: Option<Pointer>, p: PointerState) {
    if p.went_down && p.down {
        held.set(Some(over));
    } else if !p.down {
        held.set(None);
    }
    let now = held.get().unwrap_or(over);
    let want = PointerShape { shape: now.unwrap_or_default(), node: now.is_some() };
    if w.try_get::<&PointerShape>(|s| *s) != Some(want) {
        w.set(want);
    }
}

fn apply(app: EntityView<'_>, top: Option<Top>, p: PointerState) {
    let now = top.map(|t| t.id);
    let was = app.target(Hovered::id(), 0).map(|e| *e.id());
    // What the chrome that is not yet nodes has under the pointer is its own: only a node's hover is taken off here.
    let was_node = app.target(Hovered::id(), 0).is_some_and(|e| e.has(Node::id()));
    if now != was && (now.is_some() || was_node) {
        match now {
            Some(id) => {
                app.add((Hovered::id(), id));
            }
            None => {
                app.remove((Hovered::id(), flecs::Wildcard::ID));
            }
        }
    }
    // Both in one frame (a tap delivers its press and release a few milliseconds apart): the button that is still down says
    // which came last. The press's `(Pressed, node)` is deferred until the system ends, so a release after it in the same
    // frame is given the node pressed rather than reading the pair back.
    if p.went_down && p.went_up && p.down {
        release(app, now, app.target(Pressed::id(), 0).map(|e| *e.id()));
        press(app, top);
    } else {
        let pressed = if p.went_down { press(app, top) } else { app.target(Pressed::id(), 0).map(|e| *e.id()) };
        if p.went_up {
            release(app, now, pressed);
        }
    }
}

/// The button went down over `top`: it is Pressed if Pressable, and focus goes to it or nowhere. The node pressed, if any.
fn press(app: EntityView<'_>, top: Option<Top>) -> Option<u64> {
    let Some(Top { id, pressable, focusable, .. }) = top else {
        app.remove((Focused::id(), flecs::Wildcard::ID));
        return None;
    };
    if focusable {
        app.add((Focused::id(), id));
    } else {
        app.remove((Focused::id(), flecs::Wildcard::ID));
    }
    pressable.then(|| {
        app.add((Pressed::id(), id));
        id
    })
}

/// The button went up over `now`: `pressed`, let go over itself, is Activated.
fn release(app: EntityView<'_>, now: Option<u64>, pressed: Option<u64>) {
    if let Some(id) = pressed.filter(|&id| Some(id) == now) {
        let e = app.world().entity_from_id(id);
        if e.is_alive() {
            e.add(Activated::id());
        }
    }
    app.remove((Pressed::id(), flecs::Wildcard::ID));
}

/// End the host's frame: the one-frame `Activated` tags come off, and the pointer's went-down and went-up are spent.
pub fn end_frame(world: WorldRef<'_>) {
    let mut done = Vec::new();
    world.new_query::<&Activated>().each_entity(|e, _| done.push(*e.id()));
    for id in done {
        let e = world.entity_from_id(id);
        if e.is_alive() {
            e.remove(Activated::id());
        }
    }
    if let Some(p) = world.try_get::<&PointerState>(|p| *p).filter(|p| p.went_down || p.went_up) {
        world.set(PointerState { went_down: false, went_up: false, ..p });
    }
}

/// Move keyboard focus to the next (or previous) Focusable, wrapping. Order is entity creation order, which is the order
/// a script declares them in.
pub fn cycle_focus(world: WorldRef<'_>, forward: bool) {
    let mut order = Vec::new();
    world.new_query::<(&Node, &Focusable)>().each_entity(|e, _| order.push(*e.id()));
    if order.is_empty() {
        return;
    }
    order.sort_unstable();
    let app = world.lookup(APP);
    let at = app.target(Focused::id(), 0).and_then(|f| order.iter().position(|&i| i == *f.id()));
    let n = order.len();
    let next = match (at, forward) {
        (None, true) => 0,
        (None, false) => n - 1,
        (Some(i), true) => (i + 1) % n,
        (Some(i), false) => (i + n - 1) % n,
    };
    app.add((Focused::id(), order[next]));
}

harness_native_api::module!(
    "input",
    install,
    [harness_native_api::API_LAYOUTS, harness_native_ui_node_api::API_LAYOUTS, harness_native_ui_input_api::API_LAYOUTS]
);
