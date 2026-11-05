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
//! A third system drives every scroll's overlay scroller (`Scroller`, layout-api): the pointer near an edge widens it, a
//! press on its knob drags it, a press on its track pages (`ScrollerGrip`, and `ScrollState` itself).
//!
//! The state is the per-frame hit list shared by the systems, rebuilt every frame, the topmost hit and the shape a press
//! holds; everything else is in the world.
use core::cell::Cell;
use core::cell::RefCell;
use std::rc::Rc;

use harness_native_flecs::prelude::*;
use harness_native_api::APP;
use harness_native_ui_input_api::{
    Activated, Cursor, Focusable, Focused, HitBox, Hoverable, Hovered, Pointer, PointerShape, PointerState, Pressable, Pressed,
};
use harness_native_ui_layout_api::{
    NoScroller, ScrollAxis, ScrollExtent, ScrollState, Scroller, ScrollerGrip, ScrollerPart, ScrollerState, axis_bit, scroller_axis,
};
use harness_native_ui_motion_api::{Leaving, Motion};
use harness_native_ui_node_api::{Clip, Hidden, Hosted, Laid, Layer, Level, Node, Offscreen, Z};
use harness_native_ui_spring_api::{Spring, Tune};

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
    // After layout (this pass's places; its `(&Node, &Laid)` terms make flecs merge layout's writes first), then the
    // pointer's resolve after every hit is in.
    world.system_named::<(&Node, &Laid)>("UiHits").with(Hoverable::id()).phase(Phase::AfterLayout).each_entity(move |e, (_, laid)| {
        let (mut depth, mut root) = (0u32, e);
        let mut cursor = e.try_cloned::<&Cursor>().map(|c| c.shape);
        // A view going out on its Transition (an overlay closing, a row folding away) still draws but takes no pointer; one
        // layout left out (Hidden: a ViewThatFits variant, a scroller's part while it is hidden) takes none either.
        let mut leaving = e.has(Leaving::id()) || e.has(Hidden::id());
        // The highest Layer on it or around it: a node inside a lifted float (a popover's row) is hit in the float's stack.
        let mut level = e.try_cloned::<&Layer>().map(|l| l.level);
        while let Some(p) = root.parent().filter(|p| p.has(Node::id())) {
            depth += 1;
            root = p;
            if let Some(l) = p.try_cloned::<&Layer>() {
                level = level.max(Some(l.level));
            }
            leaving |= p.has(Leaving::id()) || p.has(Hidden::id());
            if cursor.is_none() {
                cursor = p.try_cloned::<&Cursor>().map(|c| c.shape);
            }
        }
        let clip = cut(e);
        if leaving || root.has(Offscreen::id()) || root.has(Hosted::id()) {
            return;
        }
        let level = level.unwrap_or_default();
        let z = e.try_cloned::<&Z>().map_or(0.0, |z| z.z);
        // A HitBox narrows it to its part of its rectangle.
        let laid = e.try_cloned::<&HitBox>().map_or(*laid, |b| Laid { x: laid.x + b.x, y: laid.y + b.y, w: b.w, h: b.h });
        sink.borrow_mut().push(Hit {
            id: *e.id(),
            laid,
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
    // The topmost hit this pass, for the scrollers after it.
    let topmost: Rc<Cell<Option<u64>>> = Rc::default();
    let top_out = topmost.clone();
    world.system_named::<&PointerState>("UiPointer").phase(Phase::Resolve).each_iter(move |it, _, p| {
        let mut hits = hits.borrow_mut();
        hits.sort_by(|a, b| a.key.0.cmp(&b.key.0).then(a.key.1.total_cmp(&b.key.1)).then(a.key.2.cmp(&b.key.2)).then(a.key.3.cmp(&b.key.3)));
        let top = hits
            .iter()
            .rev()
            .find(|h| h.laid.contains(p.x, p.y) && h.clip.is_none_or(|c| c.contains(p.x, p.y)))
            .map(|h| Top { id: h.id, pressable: h.pressable, focusable: h.focusable, cursor: h.cursor.unwrap_or_default() });
        let any = !hits.is_empty();
        hits.clear();
        top_out.set(top.map(|t| t.id));
        let w = it.world();
        shape(w, &held, top.map(|t| t.cursor), *p);
        if any {
            apply(w.entity_from_id(app), top, *p);
        }
    });
    world
        .system_named::<(&Scroller, &ScrollState, &Laid)>("UiScroller")
        .without(NoScroller::id())
        // After the pointer's resolve (flecs orders a phase's systems by id), so `topmost` is this pass's.
        .phase(Phase::Draw)
        .reads(ScrollExtent::id())
        .reads(ScrollerState::id())
        .each_iter(move |it, row, (c, s, l)| {
            let (e, w) = (it.entity(row), it.world());
            let Some(p) = w.try_get::<&PointerState>(|p| *p) else { return };
            scroller(e, w, *c, *s, *l, p, topmost.get(), it.delta_time());
        });
}

/// One pass of a scroll's overlay scroller under the pointer (`Scroller`, layout-api): which edges the pointer is near
/// (they widen; only when what is on top is the scroll or inside it), a press on its knob taking hold of it and a press on
/// its track paging toward the press (a glide on the `ui.spring.scroller_page` spring), the held knob scrolling in
/// proportion as the pointer moves. It writes `ScrollState` itself; a host that runs a scroll's physics adopts that.
#[allow(clippy::too_many_arguments)]
fn scroller(e: EntityView<'_>, w: WorldRef<'_>, c: Scroller, s: ScrollState, l: Laid, p: PointerState, top: Option<u64>, dt: f32) {
    let Some(x) = e.try_cloned::<&ScrollExtent>() else { return };
    let st = e.try_cloned::<&ScrollerState>().unwrap_or_default();
    let was = e.try_cloned::<&ScrollerGrip>().unwrap_or_default();
    let mut g = was;
    let (laid, ext, at) = ([l.x, l.y, l.w, l.h], [x.w, x.h], [s.x, s.y]);
    let axes = [ScrollAxis::Y, ScrollAxis::X];
    let geom = |a: ScrollAxis| scroller_axis(&c, laid, ext, at, st.wide(a), a);
    // What is on top is this scroll or inside it (or nothing), and the pointer is inside every clip around it.
    let ours = top.is_none_or(|t| std::iter::successors(Some(w.entity_from_id(t)), |n| n.parent()).any(|n| n.id() == e.id()));
    let seen = clipped(e, l);
    g.near = 0;
    if ours && seen.contains(p.x, p.y) {
        for a in axes {
            let edge = match a {
                ScrollAxis::Y => p.x >= l.x + l.w - c.grab,
                ScrollAxis::X => p.y >= l.y + l.h - c.grab,
            };
            if edge && geom(a).is_some() {
                g.near |= axis_bit(a);
            }
        }
    }
    if !p.down {
        g.held = false;
        g.drag = 0;
    } else if p.went_down && !g.held {
        // The press is taken once, however many passes the frame runs.
        g.held = true;
        let part = top.map(|t| w.entity_from_id(t)).filter(|t| t.parent().is_some_and(|q| q.id() == e.id()));
        if let Some(part) = part.and_then(|t| t.try_cloned::<&ScrollerPart>())
            && let Some(ax) = geom(part.axis)
        {
            let along = [p.x, p.y][part.axis.index()];
            let knob = ax.knob_at(part.axis);
            g.page = 0;
            if part.track && !(along >= knob && along <= knob + ax.knob[2 + part.axis.index()]) {
                let page = (ax.view - c.keep).max(ax.view * 0.5);
                let now = at[part.axis.index()];
                g.page = axis_bit(part.axis);
                g.goal = (if along < knob { now - page } else { now + page }).clamp(0.0, ax.max);
                g.glide = Spring::at(now);
            } else {
                g.drag = axis_bit(part.axis);
                g.grab = along - knob;
            }
        }
    }
    let mut next = s;
    for a in axes {
        let i = a.index();
        if g.drag == axis_bit(a)
            && let Some(ax) = geom(a)
        {
            let to = ax.scroll_for([p.x, p.y][i] - g.grab);
            if (to - at[i]).abs() > 0.01 {
                if i == 0 { next.x = to } else { next.y = to }
            }
        } else if g.page == axis_bit(a) {
            // Moved by someone else (a wheel) since the glide's last step: the glide lets go.
            if (at[i] - g.glide.x).abs() > 0.5 {
                g.page = 0;
                continue;
            }
            let tune = w
                .try_lookup("ui::spring::scroller_page")
                .and_then(|t| t.try_cloned::<&Motion>())
                .map_or(Tune { omega: 24.0, zeta: 1.0 }, |m| Tune { omega: m.omega, zeta: m.zeta });
            if harness_native_api::settle(&mut g.glide, g.goal, tune, dt) {
                harness_native_api::moving(&w);
            } else {
                g.glide = Spring::at(g.goal);
                g.page = 0;
            }
            if i == 0 { next.x = g.glide.x } else { next.y = g.glide.x }
        }
    }
    if next != s {
        e.set(next);
    }
    if g != was {
        e.set(g);
    }
}

/// `a` within `b`.
fn within(a: Laid, b: Laid) -> Laid {
    let (x, y) = (a.x.max(b.x), a.y.max(b.y));
    Laid { x, y, w: ((a.x + a.w).min(b.x + b.w) - x).max(0.0), h: ((a.y + a.h).min(b.y + b.h) - y).max(0.0) }
}

/// Where the clips around `e` let the pointer reach it (their rects intersected), as the draw pass cuts it: from its root
/// down, every `Clip` ancestor cuts it, except that a node a `Layer` lifts above its tree's level (a float) starts again
/// uncut, so a popover opened in a scroll is hit where it draws. None: nothing cuts it.
fn cut(e: EntityView<'_>) -> Option<Laid> {
    reach(e).1
}

/// The level node `e` is drawn at and the clip that cuts it ([`cut`]).
fn reach(e: EntityView<'_>) -> (Level, Option<Laid>) {
    let own = e.try_cloned::<&Layer>().map(|l| l.level);
    let Some(p) = e.parent().filter(|p| p.has(Node::id())) else { return (own.unwrap_or_default(), None) };
    let (level, above) = reach(p);
    match own {
        Some(l) if l > level => (l, None),
        _ => {
            let mine = match (p.has(Clip::id()), p.try_cloned::<&Laid>()) {
                (true, Some(c)) => Some(above.map_or(c, |a| within(a, c))),
                _ => above,
            };
            (level, mine)
        }
    }
}

/// `l` cut to every `Clip` around `e` ([`cut`]): where the pointer can reach it.
fn clipped(e: EntityView<'_>, l: Laid) -> Laid {
    cut(e).map_or(l, |c| within(l, c))
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
    [
        harness_native_api::API_LAYOUTS,
        harness_native_ui_node_api::API_LAYOUTS,
        harness_native_ui_input_api::API_LAYOUTS,
        harness_native_ui_layout_api::API_LAYOUTS,
        harness_native_ui_motion_api::API_LAYOUTS
    ]
);
