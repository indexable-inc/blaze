//! Harness-owned hover help. Dwell is a sans-IO state machine: a frame advances it, and an idle
//! native host arms its one outstanding deadline once. The card is an ordinary themed Flecs view.
use super::*;
use harness_native_ui_input_api::Tooltip;
use harness_native_ui_node_api::Placed;

/// Short enough for reading controls, long enough to pass across a row without flashing help.
const DWELL: f32 = 0.35;

#[derive(Default)]
pub(super) struct HoverHelp {
    dwell: Dwell,
    /// These entities exist only while shown: the point beside the pointer, and its floating card.
    shown: Option<(u64, u64)>,
}

/// The invariant: only an unchanged source under a resting pointer may open, after DWELL. A stale
/// one-shot can never open a replacement source; a click, key or blur suppresses help until movement.
/// No timers or clocks are owned here: frames supply elapsed seconds and a driver supplies expiry.
#[derive(Default)]
struct Dwell {
    source: Option<(u64, String)>,
    elapsed: f32,
    generation: u64,
    armed: bool,
    blocked: bool,
    visible: bool,
}

impl Dwell {
    fn reset(&mut self, blocked: bool) {
        self.source = None;
        self.elapsed = 0.0;
        self.generation = self.generation.wrapping_add(1);
        self.armed = false;
        self.visible = false;
        self.blocked = blocked;
    }

    fn observe(&mut self, source: Option<(u64, &str)>, elapsed: f32) {
        if self.blocked {
            return;
        }
        let unchanged = match (&self.source, source) {
            (Some((old, text)), Some((id, words))) => *old == id && text == words,
            (None, None) => true,
            _ => false,
        };
        if !unchanged {
            self.reset(false);
            // alloc: the current help's words, only when its identity or text changes.
            self.source = source.map(|(id, words)| (id, words.to_owned()));
        } else if self.source.is_some() {
            self.elapsed = (self.elapsed + elapsed.max(0.0)).min(DWELL);
        }
        self.visible = self.source.is_some() && self.elapsed >= DWELL;
    }

    fn arm(&mut self) -> Option<(u64, f64)> {
        if self.source.is_none() || self.visible || self.armed || self.blocked {
            return None;
        }
        self.armed = true;
        Some((self.generation, f64::from((DWELL - self.elapsed).max(0.0))))
    }

    fn expired(&mut self, generation: u64) -> bool {
        if generation != self.generation || !self.armed || self.source.is_none() || self.blocked || self.visible {
            return false;
        }
        self.elapsed = DWELL;
        self.armed = false;
        true
    }
}

fn source(w: WorldRef<'_>) -> Option<EntityView<'_>> {
    let hovered = w.entity_named(APP).target(Hovered::id(), 0)?;
    std::iter::successors(Some(hovered), |n| n.parent())
        .take(HOVER_DEPTH)
        .find(|n| n.has(Tooltip::id()))
}

fn fill(card: View<'_>, text: &str) {
    let text = text.trim();
    let (title, detail) = text.split_once('\n').unwrap_or((text, ""));
    card.part("title").expect("hn.HoverHelp.title").text(title.trim());
    let support = card.part("detail").expect("hn.HoverHelp.detail");
    if detail.trim().is_empty() {
        support.entity().destruct();
    } else {
        support.text(detail.trim());
    }
}

impl App {
    /// The nearest help under the pointer, for offscreen validation and the inspection log.
    pub fn tooltip(&self) -> String {
        source(self.world.world_ref())
            .and_then(|e| e.try_get::<&Tooltip>(|t| t.text.as_str().to_owned()))
            .unwrap_or_default()
    }

    /// An actual pointer movement dismisses help and starts a fresh bounded dwell at its new point.
    pub(super) fn tooltip_moved(&mut self) {
        self.hover_help.dwell.reset(false);
        self.remove_tooltip();
    }

    /// Press, typing, scrolling or blur: it stays dismissed until the pointer moves again.
    pub fn dismiss_tooltip(&mut self) -> bool {
        self.hover_help.dwell.reset(true);
        self.remove_tooltip()
    }

    fn remove_tooltip(&mut self) -> bool {
        let Some((anchor, card)) = self.hover_help.shown.take() else { return false };
        let w = self.world.world_ref();
        w.entity_from_id(card).destruct();
        w.entity_from_id(anchor).destruct();
        self.poke_why("hover help");
        true
    }

    /// A single idle wake, carrying its generation so an obsolete deadline does nothing.
    pub fn tooltip_arm(&mut self) -> Option<(u64, f64)> {
        self.hover_help.dwell.arm()
    }

    pub fn tooltip_woke(&mut self, generation: u64) -> bool {
        if !self.hover_help.dwell.expired(generation) {
            return false;
        }
        self.poke_why("hover help dwell");
        true
    }

    /// After hits resolve, reconcile the one tooltip view. Nothing is shaped while its words stay the same.
    pub(super) fn sync_tooltip(&mut self, dt: f32) -> bool {
        let w = self.world.world_ref();
        let candidate = self.focused.then(|| source(w)).flatten();
        if let Some(e) = candidate {
            e.get::<&Tooltip>(|t| {
                let words = t.text.as_str().trim();
                self.hover_help.dwell.observe((!words.is_empty()).then_some((*e.id(), words)), dt);
            });
        } else {
            self.hover_help.dwell.observe(None, dt);
        }
        if !self.hover_help.dwell.visible {
            return self.remove_tooltip();
        }
        if self.hover_help.shown.is_some() {
            return false;
        }
        let Some((_, words)) = &self.hover_help.dwell.source else { return false };
        let anchor = View::root(w, "ui.Node").add(Placed::id()).set(Laid {
            x: self.pointer.x, y: self.pointer.y, w: 0.0, h: 0.0,
        });
        let card = View::root(w, "hn.HoverHelp").anchored(anchor.id());
        fill(card, words);
        self.hover_help.shown = Some((anchor.id(), card.id()));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_native_ui_input_api::{Hoverable, Pressable};
    use harness_native_ui_layout_api::{Intrinsic, Screen};
    use harness_native_ui_look_api::TextStyle;
    use harness_native_ui_node_api::{Node, Windowed};

    /// The invariant: a resting source opens once after its deadline; movement, replacement,
    /// click/key/blur and obsolete callbacks cannot open it early or resurrect dismissed help.
    #[test]
    fn hover_help_dwell_and_stale_deadlines() {
        // Deterministic virtual-frame schedules exercise the same core as the native one-shot.
        for frames in 1..128 {
            let mut d = Dwell::default();
            d.observe(Some((1, "Title\nDetail")), 0.0);
            let (old, _) = d.arm().expect("one dwell deadline");
            assert!(d.arm().is_none(), "idle must not poll or rearm");
            for _ in 0..frames {
                d.observe(Some((1, "Title\nDetail")), DWELL * 0.9 / frames as f32);
                assert!(!d.visible, "help opened before the dwell");
            }
            d.observe(Some((1, "Changed detail")), 0.0);
            assert!(!d.expired(old), "a stale timer opened replaced text");
            let (current, _) = d.arm().unwrap();
            assert!(d.expired(current));
            d.observe(Some((1, "Changed detail")), 0.0);
            assert!(d.visible);
            d.reset(true); // click, key, wheel and blur all take this path
            d.observe(Some((1, "Changed detail")), 10.0);
            assert!(!d.visible && d.arm().is_none());
            assert!(!d.expired(current), "dismissed help resurrected");
            d.reset(false); // pointer moves, even inside the same control
            d.observe(Some((1, "Changed detail")), 0.0);
            d.observe(Some((1, "Changed detail")), DWELL + 0.01);
            assert!(d.visible, "offscreen frames use the same bounded dwell");
            d.observe(None, 1.0);
            assert!(!d.visible && d.arm().is_none(), "leaving the source closes help");
        }
    }

    /// The invariant: help uses the app's wrapping typography and never becomes an input target;
    /// its actual scene layout flips and clamps against the screen, or the viewport without one.
    #[test]
    fn hover_help_scene_places_without_intercepting_the_source() {
        let world = scene_world().unwrap_or_else(|e| panic!("{e}"));
        let w = world.world_ref();
        harness_native_ui_layout::install(w);
        harness_native_ui_input::install(w);
        world.set(Viewport { w: 240.0, h: 160.0 });
        let control = View::root(w, "ui.Node").add(Placed::id()).add(Hoverable::id())
            .set(Laid { x: -200.0, y: -200.0, w: 1000.0, h: 1000.0 })
            .set(Tooltip { text: "Model\nSupporting context".into() });
        let anchor = View::root(w, "ui.Node").add(Placed::id())
            .set(Laid { x: 200.0, y: 120.0, w: 0.0, h: 0.0 });
        let card = View::root(w, "hn.HoverHelp").anchored(anchor.id());
        fill(card, "Model\nSupporting context");
        card.part("title").unwrap().set(Intrinsic { w: 160.0, h: 16.0, ..Intrinsic::default() });
        card.part("detail").unwrap().set(Intrinsic { w: 190.0, h: 32.0, ..Intrinsic::default() });
        assert!(card.entity().has(Windowed::id()));
        assert!(card.part("title").unwrap().entity().try_cloned::<&TextStyle>().unwrap().bold > 0);
        fn passive(e: EntityView<'_>) {
            assert!(!e.has(Hoverable::id()) && !e.has(Pressable::id()), "hover help intercepted its source");
            e.each_child(passive);
        }
        passive(card.entity());
        world.set(PointerState { x: 200.0, y: 120.0, ..PointerState::default() });
        for screen in [None, Some([-100.0, -80.0, 360.0, 260.0])] {
            if let Some([x, y, sw, sh]) = screen { world.set(Screen { x, y, w: sw, h: sh }); }
            world.progress_time(0.0);
            let l = card.entity().try_cloned::<&Laid>().expect("tooltip laid out");
            let [x, y, sw, sh] = screen.unwrap_or([0.0, 0.0, 240.0, 160.0]);
            assert!(l.x >= x && l.y >= y && l.x + l.w <= x + sw + 0.01 && l.y + l.h <= y + sh + 0.01, "tooltip escaped room: {l:?}");
            // Even when a screen clamp places the card over a control, its words and plate never steal that control.
            world.set(PointerState { x: l.x + 1.0, y: l.y + 1.0, ..PointerState::default() });
            world.progress_time(0.0);
            assert_eq!(w.entity_named(APP).target(Hovered::id(), 0).map(|e| *e.id()), Some(control.id()));
            assert_eq!(source(w).map(|e| *e.id()), Some(control.id()));
        }
        let help = floats::finish(w, vec![(card.id(), Draw::default())], 2.0);
        assert!(help[0].ignores_mouse, "the native hover panel must pass pointer events through");
        let menu = View::root(w, "ui.Menu").set(Laid { x: 0.0, y: 0.0, w: 100.0, h: 100.0 });
        let menu_draw = floats::finish(w, vec![(menu.id(), Draw::default())], 2.0);
        assert!(!menu_draw[0].ignores_mouse, "model/context popovers retain native input");
        // The point anchor itself has no input role either.
        assert!(anchor.entity().has(Node::id()) && !anchor.entity().has(Hoverable::id()));
    }
}
