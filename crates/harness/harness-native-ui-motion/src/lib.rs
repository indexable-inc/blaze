//! Generic animation: visibility springs, transitions in and out, matched geometry and implicit animation of layout
//! inputs. All persistent state belongs to the host world (components on the nodes, or on one entity per matched id).
//!
//! Spring tunes are the node's own `Motion`, else a token prefab's (`ui.spring.transition`, `ui.spring.matched`,
//! `ui.spring.layout` in prims.flecs). With neither, the change lands at once.
use std::collections::HashMap;

use harness_native_api::{Clock, moving, settle};
use harness_native_flecs::prelude::*;
use harness_native_ui_layout_api::{Collapse, Flex, FlexAxis, Padding, Size, Sizing};
use harness_native_ui_look_api::{Fill, Rgba};
use harness_native_ui_motion_api::{
    Animate, Drawn, FillTween, FromEdge, Leaving, Matched, MatchedGeometry, Motion, PadTween, Reveal, Shown, ShownWhen, SizeTween, Target, Transition,
    TransitionKind,
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

    // Transitions: a node is made at 0 and springs to 1 (through Shown, Target and the Reveal its kind makes); a Leaving
    // node springs back to 0 and is deleted there.
    world.system_named::<&Transition>("Transition").each_iter(|it, row, t| {
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

    // Matched geometry: per id, the drawn frame springs to where the newest live node with that id is laid out.
    world.system_named::<&Clock>("Matched").each_iter(|it, _, _| {
        let (w, dt) = (it.world(), it.delta_time());
        let mut by: HashMap<String, (u64, Laid)> = HashMap::new();
        w.new_query::<(&MatchedGeometry, &Laid)>().each_entity(|e, (m, l)| {
            if e.has(Leaving::id()) || e.has(Hidden::id()) {
                return;
            }
            let k = m.id.as_str().to_owned();
            if by.get(&k).is_none_or(|(id, _)| *e.id() > *id) {
                by.insert(k, (*e.id(), *l));
            }
        });
        let mut still = false;
        for (k, (owner, l)) in by {
            let name: String = k.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
            let holder = w.entity_named(&format!("ui_matched::{name}"));
            let o = w.entity_from_id(owner);
            let mut st = holder.try_cloned::<&Matched>().unwrap_or_default();
            let tune = tune_of(w, o, "matched");
            match tune.filter(|_| st.started) {
                Some(tune) => {
                    for (s, v) in [(&mut st.x, l.x), (&mut st.y, l.y), (&mut st.w, l.w), (&mut st.h, l.h)] {
                        still |= settle(s, v, tune, dt);
                    }
                }
                None => {
                    st = Matched { x: Spring::at(l.x), y: Spring::at(l.y), w: Spring::at(l.w), h: Spring::at(l.h), owner, started: true };
                }
            }
            st.owner = owner;
            holder.set(st);
            let d = Drawn { x: st.x.x, y: st.y.x, w: st.w.x.max(0.0), h: st.h.x.max(0.0) };
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
