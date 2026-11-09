//! Dragging the sidebar's edge, den's feel: the thin driver around the pure gesture (src/resize.rs). A press on the
//! edge (or, folded, along the window's left edge) grabs it; the edge then follows the pointer between its tokens'
//! `min` and `max`; dragged well left it latches closed and the sidebar folds away on its spring (cmd+S's), and dragged
//! back out it latches open again; past `max` the whole window rubber-bands right by the stretch and springs back to
//! where it rested when let go (window.rs moves it, `window_shift`). The latch and touching `max` pulse the trackpad
//! (window.rs plays `take_haptics`); nothing else does. Every number is a token (`side_*`, `window_back_*`).
use super::*;
use crate::haptic::Haptic;
use crate::resize::{Gesture, Limits};

/// The drag under way (if any), the window's rubber band, and the pulses not yet played.
#[derive(Default)]
pub struct SideDrag {
    /// The gesture while the edge is held.
    gesture: Option<Gesture>,
    /// Where the sidebar's left rested at the press (view points), so its edge is measured from a fixed origin.
    left: f32,
    /// The open width at the press: what a drag that closes the sidebar leaves it to reopen to (cmd+S).
    grabbed: f32,
    /// A width to put back once the fold has hidden the sidebar (a drag closed it at its minimum).
    restore: Option<f32>,
    /// How far right of where it rests the window is pulled (view points): set while held, then springing back.
    shift: Spring,
    /// The shift the window last applied (window.rs; none offscreen or in full screen): the pointer is read in the
    /// window, which the shift moves under it, so this puts it back where the window rests.
    moved: f32,
    haptics: Vec<Haptic>,
}

impl SideDrag {
    pub fn active(&self) -> bool {
        self.gesture.is_some()
    }
}

impl App {
    /// The gesture's limits from the tokens; the widest leaves the main card its `keep` in the window as it is.
    fn side_limits(&self) -> Limits {
        let w = self.world.world_ref();
        let t = crate::look::side_drag(w);
        let view = w.try_get::<&Viewport>(|v| v.w).unwrap_or(f32::INFINITY);
        let max = t.max.min(view - t.keep).max(t.min);
        Limits { min: t.min, max, latch: t.latch, give: t.give, soft: t.soft, rearm: t.rearm }
    }

    /// The sidebar's width when open (points): as last dragged, else the scene's.
    pub(super) fn side_open_width(&self) -> f32 {
        let full = view::prefab(self.world.world_ref(), "hn.Sidebar").and_then(|p| p.try_cloned::<&Size>());
        self.side_w.unwrap_or_else(|| full.map_or(0.0, |s| s.w.value))
    }

    /// The fold's clip (hn.SideClip) as last laid out: its right edge is the sidebar's visible edge.
    pub(super) fn side_clip(&self) -> Option<Laid> {
        self.sidebar()?.parent()?.try_cloned::<&Laid>()
    }

    /// Whether the pointer is where a press grabs the sidebar's edge: within `grab` of it while open, within `reach` of
    /// the window's left edge while folded; only with the fold at rest (a grab mid-fold would jump the edge) and no
    /// palette over it.
    pub(super) fn on_side_edge(&self) -> bool {
        if self.palette.is_some() || (self.side_x.x - f32::from(u8::from(self.side_open))).abs() > 0.05 {
            return false;
        }
        let Some(clip) = self.side_clip() else { return false };
        let t = crate::look::side_drag(self.world.world_ref());
        let (x, y) = (self.pointer.x, self.pointer.y);
        let edge = clip.x + clip.w;
        let near = if self.side_open { (x - edge).abs() <= t.grab } else { x >= edge && x <= edge + t.reach };
        near && y >= clip.y && y <= clip.y + clip.h
    }

    /// The edge's pointer: column resize both ways, only rightward while folded, only leftward at its widest.
    pub(super) fn side_edge_pointer(&self) -> Pointer {
        if !self.side_open {
            return Pointer::ResizeRight;
        }
        if self.side_open_width() >= self.side_limits().max {
            return Pointer::ResizeLeft;
        }
        Pointer::ResizeLeftRight
    }

    /// The pointer's x as the gesture reads it: from the sidebar's left as it rested at the press, with the window's
    /// shift put back.
    fn side_drag_x(&self) -> f32 {
        self.pointer.x - self.drag.left + self.drag.moved
    }

    /// A press on the edge: hold it from where it is (0 while folded).
    pub(super) fn side_drag_start(&mut self) {
        let open = self.side_open_width();
        self.drag.left = self.side_clip().map_or(0.0, |c| c.x);
        self.drag.grabbed = open;
        self.drag.restore = None;
        let width = if self.side_open { open } else { 0.0 };
        self.drag.gesture = Some(Gesture::new(self.side_drag_x(), width, self.side_limits()));
        self.poke();
    }

    /// The held edge follows the pointer: the width, the latch closed or open (the fold's spring animates it), the
    /// window's stretch past the widest, and their pulses.
    pub(super) fn side_drag_move(&mut self) {
        let Some(mut g) = self.drag.gesture else { return };
        let limits = self.side_limits();
        let u = g.update(self.side_drag_x(), limits);
        self.drag.gesture = Some(g);
        if u.width > 0.0 {
            let v = u.width.round();
            if let Some(side) = self.sidebar() {
                set_side_width(side, v);
            }
            self.side_w = Some(v);
        }
        self.drag.shift = Spring::at(u.display - u.width);
        if u.snapped {
            self.side_open = u.width > 0.0;
            // Closed at its minimum: once folded away it gets back the width it had when grabbed.
            self.drag.restore = (!self.side_open).then_some(self.drag.grabbed);
            self.drag.haptics.push(Haptic::Alignment);
        } else if u.reached_limit {
            self.drag.haptics.push(Haptic::LevelChange);
        }
        self.poke();
    }

    /// Let go: the window springs back, and the width it will open at is kept for the next launch (not from an offscreen
    /// shot's drag, `persist` off).
    pub(super) fn side_drag_end(&mut self) {
        if self.drag.gesture.take().is_none() {
            return;
        }
        let keep = self.drag.restore.or(self.side_w);
        if self.persist
            && let Some(v) = keep
        {
            crate::prefs::set("sidebar_width", serde_json::json!(v));
        }
        self.poke();
    }

    /// One frame: the window's shift springs back once let go, and a drag-closed sidebar gets its width back once the
    /// fold has hidden it (or cmd+S brought it back first).
    pub(super) fn step_side_drag(&mut self, dt: f32) {
        if let Some(v) = self.drag.restore
            && !self.drag.active()
            && (self.side_open || self.side_x.x <= 1e-3)
        {
            self.drag.restore = None;
            if let Some(side) = self.sidebar() {
                set_side_width(side, v);
            }
            self.side_w = Some(v);
            self.chrome_laid = None;
        }
        if self.drag.active() || self.drag.shift.settled(0.0) {
            return;
        }
        let tune = crate::look::tune(self.world.world_ref(), "window_back");
        // Under a twentieth of a point it is home.
        if !settle(&mut self.drag.shift, 0.0, tune, dt) || self.drag.shift.x.abs() < 0.05 {
            self.drag.shift = Spring::at(0.0);
        }
        self.poke();
    }

    /// How far right of where it rests the window is pulled (view points; window.rs scales it by the zoom).
    pub fn window_shift(&self) -> f32 {
        self.drag.shift.x
    }

    /// The shift the window applied (view points), which pointer events are read under.
    pub fn set_window_moved(&mut self, dx: f32) {
        self.drag.moved = dx;
    }

    /// Ask for a trackpad pulse (window.rs plays it after the event that asked, or the next).
    pub(in crate::app) fn pulse(&mut self, h: Haptic) {
        self.drag.haptics.push(h);
    }

    /// The trackpad pulses asked for since the last call (window.rs plays them).
    pub fn take_haptics(&mut self) -> Vec<Haptic> {
        std::mem::take(&mut self.drag.haptics)
    }
}
