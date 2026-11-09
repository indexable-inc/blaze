//! Dragging the browser pane's left edge (the sidebar's edge mirrored, src/app/side_drag.rs, without its latch or rubber
//! band): a press within `grab` of the edge while the pane rests open holds it, the edge then follows the pointer with
//! the width between the tokens' `min` and `max` (less in a narrow window, so the main card keeps `keep`), and the page
//! is told its new box as it goes. Let go, the width is kept for the next launch (native.json `web_width`). Every number
//! is a token (`web_*`, `hn.Theme.web_drag`).
use super::*;

/// A held edge: the pointer's x and the pane's width at the press.
#[derive(Clone, Copy, Debug)]
pub(super) struct Grab {
    x: f32,
    width: f32,
}

impl App {
    /// The width's range now: the tokens', its widest lowered so the main card keeps `keep` between the sidebar and the
    /// pane.
    fn web_limits(&self) -> (f32, f32) {
        let w = self.world.world_ref();
        let t = crate::look::web_drag(w);
        let right = w.entity_from_id(self.browser.clip).try_cloned::<&Laid>().map(|c| c.x + c.w);
        let left = self.side_clip().map_or(0.0, |c| c.x + c.w);
        let room = right.or_else(|| w.try_get::<&Viewport>(|v| v.w)).map_or(f32::INFINITY, |r| r - left - t.keep);
        (t.min, t.max.min(room).max(t.min))
    }

    /// The pane's width (points): as last dragged, else hn.WebPane's, inside the range.
    pub(super) fn web_width(&self) -> f32 {
        let s = self.web();
        let width = if s.width > 0.0 {
            s.width
        } else {
            view::prefab(self.world.world_ref(), "hn.WebPane").and_then(|p| p.try_cloned::<&Size>()).map_or(0.0, |z| z.w.value)
        };
        let (min, max) = self.web_limits();
        width.clamp(min, max)
    }

    /// The width kept from the last launch, onto `hn.web`.
    pub(in crate::app) fn web_restore_width(&mut self) {
        let Some(v) = crate::prefs::get("web_width").and_then(|v| v.as_f64()) else { return };
        let s = self.web();
        self.set_web(WebState { width: v as f32, ..s });
    }

    pub(in crate::app) fn web_dragging(&self) -> bool {
        self.browser.drag.is_some()
    }

    /// Whether the pointer is where a press grabs the pane's edge: within `grab` of the clip's left edge (its hairline),
    /// with the pane resting open and no palette over it.
    pub(in crate::app) fn on_web_edge(&self) -> bool {
        if self.palette.is_some() || !self.web_shown() || (self.browser.x.x - 1.0).abs() > 0.05 {
            return false;
        }
        let w = self.world.world_ref();
        let Some(clip) = w.entity_from_id(self.browser.clip).try_cloned::<&Laid>() else { return false };
        let t = crate::look::web_drag(w);
        let (x, y) = (self.pointer.x, self.pointer.y);
        (x - clip.x).abs() <= t.grab && y >= clip.y && y <= clip.y + clip.h
    }

    /// The edge's pointer: column resize both ways, only leftward (wider) at its narrowest, only rightward at its widest.
    pub(in crate::app) fn web_edge_pointer(&self) -> Pointer {
        let (min, max) = self.web_limits();
        let width = self.web_width();
        if width <= min {
            Pointer::ResizeLeft
        } else if width >= max {
            Pointer::ResizeRight
        } else {
            Pointer::ResizeLeftRight
        }
    }

    /// A press on the edge: hold it at the width it has.
    pub(in crate::app) fn web_drag_start(&mut self) {
        self.browser.drag = Some(Grab { x: self.pointer.x, width: self.web_width() });
        self.poke();
    }

    /// The held edge follows the pointer, the width inside the range.
    pub(in crate::app) fn web_drag_move(&mut self) {
        let Some(g) = self.browser.drag else { return };
        let (min, max) = self.web_limits();
        let width = (g.width + g.x - self.pointer.x).clamp(min, max).round();
        let s = self.web();
        if s.width != width {
            self.set_web(WebState { width, ..s });
        }
    }

    /// Let go: the width is kept for the next launch (not from an offscreen shot's drag, `persist` off).
    pub(in crate::app) fn web_drag_end(&mut self) {
        if self.browser.drag.take().is_none() {
            return;
        }
        let width = self.web().width;
        if self.persist && width > 0.0 {
            crate::prefs::set("web_width", serde_json::json!(width));
        }
        self.poke();
    }
}
