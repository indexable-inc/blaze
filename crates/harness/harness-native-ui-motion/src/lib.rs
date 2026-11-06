//! Generic animation: visibility springs, transitions in and out, matched geometry and implicit animation of layout
//! inputs. All persistent state belongs to the host world (components on the nodes, or on one entity per matched id).
//!
//! Spring tunes are the node's own `Motion`, else a token prefab's (`ui.spring.transition`, `ui.spring.matched`,
//! `ui.spring.layout` in prims.flecs). With neither, the change lands at once.
//!
//! It also runs each scroll's overlay scroller (`Scroller`): when it shows and fades, how wide each axis's knob is
//! (`ScrollerState`), and its parts' fade.
use std::collections::HashMap;

use harness_native_api::{Clock, moving, reduced, settle};
use harness_native_flecs::prelude::*;
use harness_native_ui_layout_api::{
    Collapse, Flex, FlexAxis, NoScroller, Padding, ScrollAxis, ScrollExtent, ScrollState, Scroller, ScrollerGrip, ScrollerPart, ScrollerState, Size, Sizing,
    Viewport, axis_bit, overflows,
};
use harness_native_ui_look_api::{Fill, Rgba};
use harness_native_ui_motion_api::{
    Animate, CopyFlash, Drawn, FillTween, FromEdge, Leaving, Flip, Matched, MatchedGeometry, Motion, PadTween, Pose, Posed, Posing, Reveal, Settling, Shown, ShownWhen, SizeTween, Target,
    Transition, TransitionKind,
};
use harness_native_ui_node_api::{Hidden, Laid};
use harness_native_ui_spring_api::{Spring, Tune};

/// The token prefab `ui.spring.<name>`'s `Motion`.
fn token(w: WorldRef<'_>, name: &str) -> Option<Motion> {
    w.try_lookup(&format!("ui::spring::{name}")).and_then(|t| t.try_cloned::<&Motion>())
}

/// The spring a node moves on: its own `Motion`, else the token's.
fn tune_of(w: WorldRef<'_>, e: EntityView<'_>, name: &str) -> Option<Tune> {
    e.try_cloned::<&Motion>().or_else(|| token(w, name)).map(|m| Tune { omega: m.omega, zeta: m.zeta })
}

/// How a transition shows a node at 0 (gone): faded, off by its size, or scaled down.
fn reveal_of(t: Transition, l: Laid, leaving: bool) -> Reveal {
    let alpha0 = if t.fade || t.kind == TransitionKind::Opacity { 0.0 } else { 1.0 };
    let (dx, dy, scale0) = match t.kind {
        TransitionKind::Opacity => (0.0, 0.0, 1.0),
        TransitionKind::Slide => (if leaving { l.w } else { -l.w }, 0.0, 1.0),
        TransitionKind::Scale => (0.0, 0.0, t.scale.max(0.01)),
        TransitionKind::Move => match t.from {
            FromEdge::Top => (0.0, -l.h, 1.0),
            FromEdge::Bottom => (0.0, l.h, 1.0),
            FromEdge::Leading => (-l.w, 0.0, 1.0),
            FromEdge::Trailing => (l.w, 0.0, 1.0),
        },
        // Scaled about its middle, then moved so the `from` edge stays where it rests at every step.
        TransitionKind::Grow => {
            let s = t.scale.clamp(0.01, 1.0);
            let (hw, hh) = (l.w * (1.0 - s) / 2.0, l.h * (1.0 - s) / 2.0);
            match t.from {
                FromEdge::Top => (0.0, -hh, s),
                FromEdge::Bottom => (0.0, hh, s),
                FromEdge::Leading => (-hw, 0.0, s),
                FromEdge::Trailing => (hw, 0.0, s),
            }
        }
    };
    Reveal { alpha0, dx, dy, scale0 }
}

/// A value an `Animate` node eases between.
trait Tween: Copy + PartialEq {
    fn lerp(a: Self, b: Self, t: f32) -> Self;
}

fn mixf(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

impl Tween for Size {
    fn lerp(a: Self, b: Self, t: f32) -> Self {
        // An axis that changes kind (Fit to Fixed) has nothing to ease between: it lands.
        let axis = |a: Sizing, b: Sizing| {
            if a.kind == b.kind { Sizing { kind: b.kind, value: mixf(a.value, b.value, t), min: mixf(a.min, b.min, t), max: mixf(a.max, b.max, t) } } else { b }
        };
        Self { w: axis(a.w, b.w), h: axis(a.h, b.h) }
    }
}

impl Tween for Padding {
    fn lerp(a: Self, b: Self, t: f32) -> Self {
        Self { l: mixf(a.l, b.l, t), r: mixf(a.r, b.r, t), t: mixf(a.t, b.t, t), b: mixf(a.b, b.b, t) }
    }
}

impl Tween for Fill {
    fn lerp(a: Self, b: Self, t: f32) -> Self {
        let c = |a: Rgba, b: Rgba| Rgba { r: mixf(a.r, b.r, t), g: mixf(a.g, b.g, t), b: mixf(a.b, b.b, t), a: mixf(a.a, b.a, t) };
        Self { dark: c(a.dark, b.dark), light: c(a.light, b.light) }
    }
}

/// One step of an eased value: `cur` is what the node holds now, `tw` the tween so far (from, to, last written, spring).
/// Returns the tween to keep, the value to write (if it differs from `cur`) and whether it is still moving. A `cur` that is
/// neither the last value written nor the target is a new target, eased to from where the last value was.
fn ease<T: Tween>(cur: T, tw: Option<(T, T, T, Spring)>, tune: Option<Tune>, dt: f32) -> ((T, T, T, Spring), Option<T>, bool) {
    let (mut from, mut to, mut shown, mut t) = tw.unwrap_or((cur, cur, cur, Spring::at(1.0)));
    if cur != shown && cur != to {
        from = shown;
        to = cur;
        t = Spring::at(0.0);
    }
    let going = match tune {
        Some(tune) if shown != to || !t.settled(1.0) => settle(&mut t, 1.0, tune, dt),
        _ => false,
    };
    if !going {
        t = Spring::at(1.0);
    }
    shown = if going { T::lerp(from, to, t.x) } else { to };
    ((from, to, shown, t), (shown != cur).then_some(shown), going)
}

/// How far `e`'s scrolling ancestors have moved it (the sum of their `ScrollState`s): its content-space frame is its
/// `Laid` plus this.
fn scrolled(e: EntityView<'_>) -> (f32, f32) {
    let (mut x, mut y) = (0.0, 0.0);
    let mut at = e.parent();
    while let Some(p) = at {
        if let Some(s) = p.try_cloned::<&ScrollState>() {
            x += s.x;
            y += s.y;
        }
        at = p.parent();
    }
    (x, y)
}

/// One frame of a matched id's springs toward `to` (content space); whether it still moves. Its position springs; its
/// size lands on `to`'s every frame (a slide between a session's row and a shorter run's never passes through a height
/// neither has, and a pill is never drawn shorter than the row under it). A slide retargets from where it is drawn and
/// how fast it goes. From rest (not moving last frame), a move of more than `jump` of its heights lands
/// at once; mid-slide it never does. Its height is the taller of where it is and where it goes: a node that is still
/// unfolding (a new row growing in from nothing, its pill with it) is not hundreds of its own heights away. With no
/// tune, on its first frame or when `land` (the view was rescaled under it: its node moved because the room did, not
/// because the selection did; or the system's Reduce Motion is on) it lands.
fn chase(st: &mut Matched, to: Laid, tune: Option<Tune>, jump: f32, land: bool, dt: f32) -> bool {
    let unit = to.h.max(st.h.x).max(1.0);
    let rows = |a: f32, b: f32| ((a - b).abs() / unit).floor();
    let far = jump > 0.0 && !st.moving && (rows(to.y, st.y.x) > jump || rows(to.x, st.x.x) > jump);
    let still = match tune.filter(|_| st.started && !far && !land) {
        Some(tune) => {
            let mut still = false;
            for (s, v) in [(&mut st.x, to.x), (&mut st.y, to.y)] {
                still |= settle(s, v, tune, dt);
            }
            (st.w, st.h) = (Spring::at(to.w), Spring::at(to.h));
            if !still {
                // At rest exactly on its node (a settled spring is only within 1e-3 of it), so the painter sees a node at
                // rest and puts it on the pixel grid with the row it sits behind.
                (st.x, st.y) = (Spring::at(to.x), Spring::at(to.y));
            }
            still
        }
        None => {
            (st.x, st.y, st.w, st.h) = (Spring::at(to.x), Spring::at(to.y), Spring::at(to.w), Spring::at(to.h));
            st.started = true;
            false
        }
    };
    st.moving = still;
    still
}

/// Whether a matched id lands this pass because the room changed under it: the room differs from the one it last chased
/// in, or it changed earlier this same frame (layout runs more than once a frame; a later pass may still move the node).
/// Records the room and when it came in.
fn rescaled(st: &mut Matched, room: (f32, f32), now: f32) -> bool {
    if st.started && (st.room_w, st.room_h) != room {
        st.room_at = now;
        st.rescaled = true;
    }
    (st.room_w, st.room_h) = room;
    st.rescaled && st.room_at == now
}

/// Install systems in the module scope; deleting that scope removes all callbacks before unloading.
pub fn install(world: WorldRef<'_>) {
    // Scene springs: toward the host's Target when it set one, else 1 while the app is in any (ShownWhen, state).
    let app_id = *world.lookup(harness_native_api::APP).id();
    world.system_named::<(&Motion, &mut Shown)>("Show").each_iter(move |it, row, (m, sh)| {
        let (e, w, dt) = (it.entity(row), it.world(), it.delta_time());
        let app = w.entity_from_id(app_id);
        let target = e.try_cloned::<&Target>().map_or_else(
            || {
                let mut on = false;
                e.each_target(ShownWhen::id(), |t| on |= app.has((flecs::Wildcard::ID, *t.id())));
                f32::from(u8::from(on))
            },
            |t| t.0,
        );
        if !sh.started {
            sh.spring = Spring::at(target);
            sh.started = true;
        }
        if settle(&mut sh.spring, target, Tune { omega: m.omega, zeta: m.zeta }, dt) {
            moving(&w);
        }
    });

    // A copy's pulse (CopyFlash): its timeline runs on the frame clock and lands exactly at rest at its end, so nothing
    // more is drawn and the frame goes idle.
    world.system_named::<&mut CopyFlash>("CopyFlash").each_iter(|it, _, f| {
        if f.secs == 0.0 {
            return;
        }
        f.age += it.delta_time();
        if f.live() {
            moving(&it.world());
        } else {
            *f = CopyFlash::default();
        }
    });

    // Transitions: a node is made at 0 and springs to 1 (through Shown, Target and the Reveal its kind makes); a Leaving
    // node springs back to 0 and is deleted there.
    world.system_named::<&Transition>("Transition").reads(Laid::id()).writes(Drawn::id()).each_iter(|it, row, t| {
        let (e, w) = (it.entity(row), it.world());
        let leaving = e.has(Leaving::id());
        let laid = e.try_cloned::<&Laid>().unwrap_or_default();
        let reveal = reveal_of(*t, laid, leaving);
        if e.try_cloned::<&Reveal>() != Some(reveal) {
            e.set(reveal);
        }
        let Some(sh) = e.try_cloned::<&Shown>() else {
            match (e.try_cloned::<&Motion>(), token(w, "transition")) {
                (None, None) => {
                    if leaving {
                        e.destruct();
                    }
                }
                (own, tok) => {
                    if own.is_none()
                        && let Some(m) = tok
                    {
                        e.set(m);
                    }
                    e.set(Shown { spring: Spring::at(0.0), started: true });
                    e.set(Target(if leaving { 0.0 } else { 1.0 }));
                    moving(&w);
                }
            }
            return;
        };
        if leaving {
            if e.try_cloned::<&Target>().map(|t| t.0) != Some(0.0) {
                e.set(Target(0.0));
            }
            // Leaving a flow, it folds away on the same spring as it fades: layout gives its neighbours what it lets go
            // (Collapse), and it is drawn (and clips its content) to what it still holds, from its leading edge.
            if let Some(c) = e.try_cloned::<&Collapse>() {
                let k = sh.spring.x.clamp(0.0, 1.0);
                if c.k != k {
                    e.set(Collapse { k });
                }
                let row = e.parent().and_then(|p| p.try_cloned::<&Flex>()).is_some_and(|f| f.axis == FlexAxis::Row);
                let d = Drawn { x: laid.x, y: laid.y, w: if row { laid.w * k } else { laid.w }, h: if row { laid.h } else { laid.h * k } };
                if e.try_cloned::<&Drawn>() != Some(d) {
                    e.set(d);
                }
            }
            if sh.spring.x <= 0.01 {
                e.destruct();
            } else {
                moving(&w);
            }
        }
    });

    // Matched geometry: per id, the drawn frame springs to where the newest live node with that id is laid out, in its
    // scroll content's space (see `Matched`).
    // After layout, reading this pass's Laid (layout `writes` it); it writes Drawn, which the draw pass reads this pass.
    world.system_named::<&Clock>("Matched").phase(Phase::AfterLayout).reads(Laid::id()).writes(Drawn::id()).each_iter(|it, _, _| {
        let (w, dt) = (it.world(), it.delta_time());
        let room = w.try_get::<&Viewport>(|v| (v.w, v.h)).unwrap_or((0.0, 0.0));
        let now = w.try_get::<&Clock>(|c| c.time).unwrap_or(0.0);
        let still_motion = reduced(w);
        // Per id, its newest live node and that node's Laid (None: made this frame, layout has not reached it yet).
        let mut by: HashMap<String, (u64, Option<Laid>, f32)> = HashMap::new();
        w.new_query::<&MatchedGeometry>().each_entity(|e, m| {
            if e.has(Leaving::id()) || e.has(Hidden::id()) {
                return;
            }
            let k = m.id.as_str().to_owned();
            if by.get(&k).is_none_or(|(id, _, _)| *e.id() > *id) {
                by.insert(k, (*e.id(), e.try_cloned::<&Laid>(), m.jump));
            }
        });
        let mut still = false;
        for (k, (owner, l, jump)) in by {
            let name: String = k.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
            let holder = w.entity_named(&format!("ui_matched::{name}"));
            let o = w.entity_from_id(owner);
            let (sx, sy) = scrolled(o);
            let mut st = holder.try_cloned::<&Matched>().unwrap_or_default();
            // Not laid out yet, or laid in a frame that is not its own yet (a Settling ancestor: a row still folding in):
            // drawn where the slide is, going on toward where it was last sent (a new selection's pill neither lands on
            // its row for a frame, nor loses one of the slide, nor squeezes into a row that has not opened). With
            // nowhere it was going, it is drawn nowhere until it has a frame.
            let settling = std::iter::successors(Some(o), |n| n.parent()).any(|n| n.has(Settling::id()));
            let to = match l.filter(|_| !settling) {
                Some(l) => Laid { x: l.x + sx, y: l.y + sy, ..l },
                None if st.started => Laid { x: st.gx, y: st.gy, w: st.gw, h: st.gh },
                None => {
                    let d = Drawn { x: 0.0, y: 0.0, w: 0.0, h: 0.0 };
                    if l.is_some() && o.try_cloned::<&Drawn>() != Some(d) {
                        o.set(d);
                    }
                    continue;
                }
            };
            (st.gx, st.gy, st.gw, st.gh) = (to.x, to.y, to.w, to.h);
            let land = rescaled(&mut st, room, now) || still_motion;
            still |= chase(&mut st, to, tune_of(w, o, "matched"), jump, land, dt);
            st.owner = owner;
            holder.set(st);
            let d = Drawn { x: st.x.x - sx, y: st.y.x - sy, w: st.w.x.max(0.0), h: st.h.x.max(0.0) };
            if o.try_cloned::<&Drawn>() != Some(d) {
                o.set(d);
            }
        }
        if still {
            moving(&w);
        }
    });

    // Implicit animation: an Animate node's Size, Padding and Fill ease to whatever was last written to them.
    world.system_named::<&Animate>("Animate").each_iter(|it, row, _| {
        let (e, w, dt) = (it.entity(row), it.world(), it.delta_time());
        let tune = tune_of(w, e, "layout");
        let mut still = false;
        if let Some(cur) = e.try_cloned::<&Size>() {
            let tw = e.try_cloned::<&SizeTween>().map(|t| (t.from, t.to, t.shown, t.t));
            let ((from, to, shown, t), write, going) = ease(cur, tw, tune, dt);
            e.set(SizeTween { from, to, shown, t });
            if let Some(v) = write {
                e.set(v);
            }
            still |= going;
        }
        if let Some(cur) = e.try_cloned::<&Padding>() {
            let tw = e.try_cloned::<&PadTween>().map(|t| (t.from, t.to, t.shown, t.t));
            let ((from, to, shown, t), write, going) = ease(cur, tw, tune, dt);
            e.set(PadTween { from, to, shown, t });
            if let Some(v) = write {
                e.set(v);
            }
            still |= going;
        }
        if let Some(cur) = e.try_cloned::<&Fill>() {
            let tw = e.try_cloned::<&FillTween>().map(|t| (t.from, t.to, t.shown, t.t));
            let ((from, to, shown, t), write, going) = ease(cur, tw, tune, dt);
            e.set(FillTween { from, to, shown, t });
            if let Some(v) = write {
                e.set(v);
            }
            still |= going;
        }
        if still {
            moving(&w);
        }
    });

    // Poses on springs: a Posed node's Posing chases its Posed, on the Posed's own tune (else the node's Motion, else the
    // `ui.spring.pose` token; with none it lands), still until its `hold` (an entrance's delay). A node with no Posing yet
    // lands on its Posed. At rest nothing is written, so the frame goes idle.
    world.system_named::<&Posed>("Posing").each_iter(|it, row, p| {
        let (e, w, dt) = (it.entity(row), it.world(), it.delta_time());
        let Some(mut s) = e.try_cloned::<&Posing>() else {
            e.set(Posing::at(*p, 0.0));
            return;
        };
        let pairs = |s: &Posing| [(s.x, p.x), (s.y, p.y), (s.scale, p.scale), (s.opacity, p.opacity), (s.share, p.share)];
        if pairs(&s).iter().all(|(sp, to)| sp.x == *to && sp.v == 0.0) {
            return;
        }
        let now = w.try_get::<&Clock>(|c| c.time).unwrap_or(0.0);
        if now < s.hold {
            moving(&w);
            return;
        }
        let tune = if p.omega > 0.0 { Some(Tune { omega: p.omega, zeta: if p.zeta > 0.0 { p.zeta } else { 1.0 } }) } else { tune_of(w, e, "pose") };
        let Some(tune) = tune else {
            e.set(Posing::at(*p, s.hold));
            return;
        };
        let mut going = false;
        for (sp, to) in [(&mut s.x, p.x), (&mut s.y, p.y), (&mut s.scale, p.scale), (&mut s.opacity, p.opacity), (&mut s.share, p.share)] {
            if settle(sp, to, tune, dt) {
                going = true;
            } else {
                *sp = Spring::at(to);
            }
        }
        e.set(s);
        if going {
            moving(&w);
        }
    });

    // FLIP: after layout, a Flip node whose place in its parent moved has its Posing pushed back by the move, so it is
    // drawn where it was this pass and springs to where it is (the Posing system, next frame on). The draw pass reads it.
    world.system_named::<(&Laid, &mut Flip)>("Flip").phase(Phase::AfterLayout).reads(Laid::id()).writes(Posing::id()).each_iter(|it, row, (l, f)| {
        let (e, w) = (it.entity(row), it.world());
        let p = e.parent().and_then(|p| p.try_cloned::<&Laid>()).unwrap_or_default();
        let (x, y) = (l.x - p.x, l.y - p.y);
        let (dx, dy) = (f.x - x, f.y - y);
        let seen = f.seen;
        *f = Flip { x, y, seen: true };
        if !seen || (dx.abs() < 0.01 && dy.abs() < 0.01) {
            return;
        }
        if let Some(mut s) = e.try_cloned::<&Posing>() {
            s.x.x += dx;
            s.y.x += dy;
            e.set(s);
            moving(&w);
        }
    });

    // Overlay scrollers (`Scroller`, layout-api): shown while the scroll moves, `hold` seconds after and while the pointer
    // is near or holds it, then faded (the `ui.spring.scroller_show` / `scroller_hide` springs); each axis's knob wide
    // over its track while the pointer is near that edge or drags it (`ui.spring.scroller_wide`). The parts fade by
    // their Pose: the knob with the scroller, the track with it and its axis's width.
    world
        .system_named::<(&Scroller, &ScrollState)>("Scroller")
        .without(NoScroller::id())
        .reads(ScrollerGrip::id())
        .reads(ScrollExtent::id())
        .writes(ScrollerState::id())
        .writes(Pose::id())
        .each_iter(|it, row, (c, s)| {
        let (e, w, dt) = (it.entity(row), it.world(), it.delta_time());
        let grip = e.try_cloned::<&ScrollerGrip>().unwrap_or_default();
        let was = e.try_cloned::<&ScrollerState>();
        let mut st = was.unwrap_or_default();
        if !st.started {
            // A scroller flashes once as its scroll first shows, as AppKit's do.
            st.started = true;
            (st.seen_x, st.seen_y, st.still) = (s.x, s.y, 0.0);
        } else if (s.x - st.seen_x).abs() > 0.01 || (s.y - st.seen_y).abs() > 0.01 {
            (st.seen_x, st.seen_y, st.still) = (s.x, s.y, 0.0);
        } else {
            st.still += dt;
        }
        let view = e.try_cloned::<&Laid>().map_or([0.0; 2], |l| [l.w, l.h]);
        let ext = e.try_cloned::<&ScrollExtent>().map_or(view, |x| [x.w, x.h]);
        let moves = |a: ScrollAxis| overflows(view, ext, a);
        let any = moves(ScrollAxis::Y) || moves(ScrollAxis::X);
        let holding = st.still < c.hold;
        let show = any && (holding || grip.near != 0 || grip.drag != 0 || grip.page != 0);
        let target = f32::from(u8::from(show));
        let spring = |name: &str, fallback: Tune| token(w, name).map_or(fallback, |m| Tune { omega: m.omega, zeta: m.zeta });
        let shown_tune = spring(if target > st.shown.x { "scroller_show" } else { "scroller_hide" }, Tune { omega: 30.0, zeta: 1.0 });
        let wide_tune = spring("scroller_wide", Tune { omega: 30.0, zeta: 1.0 });
        let mut going = settle(&mut st.shown, target, shown_tune, dt);
        if !going {
            st.shown = Spring::at(target);
        }
        for (a, sp) in [(ScrollAxis::Y, &mut st.wide_y), (ScrollAxis::X, &mut st.wide_x)] {
            let bit = axis_bit(a);
            let want = f32::from(u8::from(show && moves(a) && (grip.near & bit != 0 || grip.drag == bit)));
            if settle(sp, want, wide_tune, dt) {
                going = true;
            } else {
                *sp = Spring::at(want);
            }
        }
        // Through the hold nothing moves, but its end must be drawn: the fade starts then.
        if going || (show && holding) {
            moving(&w);
        }
        if was != Some(st) {
            e.set(st);
        }
        let shown = st.shown.x.clamp(0.0, 1.0);
        e.each_child(|p| {
            let Some(part) = p.try_cloned::<&ScrollerPart>() else { return };
            let a = if part.track { shown * st.wide(part.axis) } else { shown };
            let pose = Pose { fade: 1.0 - a, ..p.try_cloned::<&Pose>().unwrap_or_default() };
            if p.try_cloned::<&Pose>() != Some(pose) {
                p.set(pose);
            }
        });
    });
}

harness_native_api::module!(
    "motion",
    install,
    [
        harness_native_api::API_LAYOUTS,
        harness_native_ui_motion_api::API_LAYOUTS,
        harness_native_ui_node_api::API_LAYOUTS,
        harness_native_ui_layout_api::API_LAYOUTS,
        harness_native_ui_look_api::API_LAYOUTS
    ]
);

#[cfg(test)]
mod tests {
    use super::*;

    /// Rapid presses: each retarget keeps the slide's position and velocity (no restart, no instant land mid-flight), so
    /// the drawn y moves one way only; from rest, 3 rows slides and 5 rows lands at once.
    #[test]
    fn matched_retargets_mid_slide_and_lands_far_from_rest() {
        let tune = Some(Tune { omega: 32.0, zeta: 0.86 });
        let row = |i: f32| Laid { x: 0.0, y: i * 30.0, w: 200.0, h: 28.0 };
        let mut st = Matched::default();
        assert!(!chase(&mut st, row(0.0), tune, 3.0, false, 1.0 / 60.0));
        let mut last = st.y.x;
        for k in 1..=8 {
            for _ in 0..3 {
                chase(&mut st, row(k as f32), tune, 3.0, false, 1.0 / 60.0);
                assert!(st.y.x >= last - 1e-3, "press {k}: {} after {last}", st.y.x);
                assert!(st.y.x - last < 30.0, "a jump of {}", st.y.x - last);
                last = st.y.x;
            }
        }
        for _ in 0..240 {
            chase(&mut st, row(8.0), tune, 3.0, false, 1.0 / 60.0);
        }
        assert!(!st.moving);
        chase(&mut st, row(5.0), tune, 3.0, false, 1.0 / 60.0);
        assert!(st.moving && st.y.x > row(5.0).y);
        for _ in 0..240 {
            chase(&mut st, row(5.0), tune, 3.0, false, 1.0 / 60.0);
        }
        chase(&mut st, row(0.0), tune, 3.0, false, 1.0 / 60.0);
        assert!(!st.moving && st.y.x == 0.0);
    }

    /// A move to a node laid next to nothing tall, two rows down: it slides there from rest instead of counting the move
    /// in that sliver of a height and landing at once.
    #[test]
    fn matched_slides_into_a_row_that_unfolds() {
        let tune = Some(Tune { omega: 26.0, zeta: 1.0 });
        let mut st = Matched::default();
        chase(&mut st, Laid { x: 0.0, y: 0.0, w: 200.0, h: 24.0 }, tune, 3.0, false, 1.0 / 60.0);
        let sliver = Laid { x: 0.0, y: 60.0, w: 200.0, h: 0.5 };
        assert!(chase(&mut st, sliver, tune, 3.0, false, 1.0 / 60.0));
        assert!(st.moving && st.y.x > 0.0 && st.y.x < 60.0, "{}", st.y.x);
    }

    /// A slide from a session's row to a shorter run's under it: its position springs, its size is the run's from the
    /// first frame (never a height between the two, never shorter than the row it lands on).
    #[test]
    fn matched_springs_position_and_lands_size() {
        let tune = Some(Tune { omega: 26.0, zeta: 1.0 });
        let mut st = Matched::default();
        chase(&mut st, Laid { x: 0.0, y: 0.0, w: 200.0, h: 26.0 }, tune, 8.0, false, 1.0 / 60.0);
        let run = Laid { x: 8.0, y: 30.0, w: 192.0, h: 20.0 };
        for frame in 0..40 {
            chase(&mut st, run, tune, 8.0, false, 1.0 / 60.0);
            assert_eq!((st.w.x, st.h.x), (run.w, run.h), "frame {frame}: size between rows");
            if frame == 0 {
                assert!(st.y.x < run.y && st.x.x < run.x, "the position landed at once instead of springing");
            }
        }
        assert!(!st.moving && st.y.x == run.y);
    }

    /// A zoom (the room changes) lands a slide on its node at once, mid-slide too, and through every layout pass of that
    /// frame; the next frame in the same room slides again.
    #[test]
    fn a_rescale_lands_instead_of_sliding() {
        let tune = Some(Tune { omega: 32.0, zeta: 0.86 });
        let row = |y: f32| Laid { x: 0.0, y, w: 200.0, h: 24.0 };
        let mut st = Matched::default();
        let mut step = |st: &mut Matched, to: Laid, room: (f32, f32), now: f32| {
            let land = rescaled(st, room, now);
            chase(st, to, tune, 3.0, land, 1.0 / 60.0)
        };
        step(&mut st, row(0.0), (1280.0, 800.0), 0.0);
        // A select one row down: it slides.
        assert!(step(&mut st, row(24.0), (1280.0, 800.0), 0.1));
        assert!(st.y.x < 24.0);
        // Mid-slide the view zooms to 1.25: the room shrinks and the row moved; it lands there.
        let (z, at) = (1.25, row(24.0 - 32.0 * (1.0 - 1.0 / 1.25)));
        assert!(!step(&mut st, at, (1280.0 / z, 800.0 / z), 0.2));
        assert_eq!((st.y.x, st.h.x, st.moving), (at.y, at.h, false));
        // A second layout pass in the same frame moves it again: still the zoom's, still landed.
        let later = row(at.y + 3.0);
        assert!(!step(&mut st, later, (1280.0 / z, 800.0 / z), 0.2));
        assert_eq!(st.y.x, later.y);
        // The next frame, in the same room, a new selection slides as before.
        assert!(step(&mut st, row(later.y + 24.0), (1280.0 / z, 800.0 / z), 0.3));
        assert!(st.y.x < later.y + 24.0);
    }

    /// A width written mid-way is a new target: the eased value starts from where it was, reaches the target, and a
    /// target rewritten with the same value does not restart it.
    #[test]
    fn ease_retargets_from_where_it_was() {
        let tune = Some(Tune { omega: 30.0, zeta: 1.0 });
        let p = |l: f32| Padding { l, r: 0.0, t: 0.0, b: 0.0 };
        let (tw, w, _) = ease(p(0.0), None, tune, 1.0 / 60.0);
        assert!(w.is_none());
        let (tw, w, going) = ease(p(100.0), Some(tw), tune, 1.0 / 60.0);
        let first = w.expect("eases").l;
        assert!(going && first > 0.0 && first < 50.0, "{first}");
        // The host writes the target again (as View setters do while the value differs): the ease goes on.
        let (tw, w, _) = ease(p(100.0), Some(tw), tune, 1.0 / 60.0);
        assert!(w.expect("goes on").l > first);
        let mut tw = tw;
        let mut cur = p(100.0);
        for _ in 0..240 {
            let (t, w, _) = ease(cur, Some(tw), tune, 1.0 / 60.0);
            tw = t;
            if let Some(v) = w {
                cur = v;
            }
        }
        assert_eq!(cur, p(100.0));
    }
}
