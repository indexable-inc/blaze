//! The text caret, den's (den-native ui.rs `Glide`, ui/fields.rs `caret_inst`): one look for every field, text area and
//! the chat's composer (`ui.caret`'s `CaretLook`, prims.flecs), drawn as a caret panel (pack.rs kind 4) whose blink and
//! sweep the shader runs (harness.slang), so a resting caret costs no paint pass. What a node keeps between frames is
//! its [`Glide`]: where the caret was when it last jumped, and when.
use std::collections::HashMap;

use harness_native_flecs::prelude::*;
use harness_native_ui_look_api::CaretLook;

use crate::pack::PanelInst;

/// The scene's caret look (prims.flecs).
pub const LOOK: &str = "ui::caret";

/// `ui.caret`'s look. The scene is compiled in, so one without it is a bug: this panics naming it.
#[must_use]
pub fn look(w: WorldRef<'_>) -> CaretLook {
    w.try_lookup(LOOK)
        .unwrap_or_else(|| panic!("the scene has no {LOOK} (prims.flecs)"))
        .try_cloned::<&CaretLook>()
        .unwrap_or_else(|| panic!("{LOOK} has no CaretLook"))
}

/// A caret's sweep, in its text node's own coordinates so that the node moving (the sidebar pushing the pane, the
/// composer growing) never sweeps: where the caret was when it last jumped, where it is, and when it moved. How long the
/// sweep takes is the look's `glide`, the `span` its methods are given.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Glide {
    from: Option<[f32; 4]>,
    to: Option<[f32; 4]>,
    start: f32,
}

impl Glide {
    fn active(&self, now: f32, span: f32) -> bool {
        self.from.is_some() && now - self.start < span
    }

    /// Where the sweep's tail is now (ease-out cubic toward the caret), while it runs.
    fn tail(&self, now: f32, span: f32) -> Option<[f32; 4]> {
        let (from, to) = (self.from?, self.to?);
        let p = (now - self.start) / span;
        (p < 1.0).then(|| {
            let e = 1.0 - (1.0 - p.max(0.0)).powi(3);
            std::array::from_fn(|i| from[i] + (to[i] - from[i]) * e)
        })
    }

    /// Follow the caret to `rest`. A jump farther than one caret height sweeps from the current tail; shorter moves
    /// (typing a character) snap.
    fn track(&mut self, rest: [f32; 4], now: f32, span: f32) {
        if self.to == Some(rest) {
            return;
        }
        let tail = self.tail(now, span).or(self.to);
        let centre = |b: [f32; 4]| (b[0] + b[2] * 0.5, b[1] + b[3] * 0.5);
        self.from = tail.filter(|t| {
            let ((ax, ay), (bx, by)) = (centre(*t), centre(rest));
            (ax - bx).hypot(ay - by) > rest[3]
        });
        self.to = Some(rest);
        self.start = now;
    }
}

/// Every focused caret's [`Glide`], by its text node, and which were drawn this frame: a caret not drawn (its field lost
/// the keys, scrolled away or went) is forgotten, so it comes back at rest rather than sweeping from where it was.
#[derive(Default)]
pub struct Carets {
    glides: HashMap<u64, (Glide, bool)>,
    /// The last frame's clock and sweep's seconds, for [`Carets::end_frame`].
    now: f32,
    span: f32,
}

/// One caret to draw: its bar at rest in its node's own points (`rest`), the node's origin (`at`), the box its sweep
/// stays inside (`clip`, view points), its ink, opacity, when it last moved (its blink starts over from then, held solid)
/// and whether it blinks.
pub struct Bar {
    pub rest: [f32; 4],
    pub at: (f32, f32),
    pub clip: [f32; 4],
    pub ink: [f32; 4],
    pub alpha: f32,
    pub moved: f32,
    pub blink: bool,
}

impl Carets {
    /// The caret panel for node `node`'s caret `b` at frame time `now`: its bar, and its sweep while one runs.
    pub fn panel(&mut self, node: u64, b: &Bar, look: &CaretLook, now: f32) -> PanelInst {
        // A glide of 0 turns the sweep off (the default, 00-tokens.flecs caret_glide): the caret only ever snaps.
        let sweeps = look.glide > 0.0;
        let span = look.glide.max(1e-3);
        (self.now, self.span) = (now, if sweeps { span } else { 0.0 });
        let (g, seen) = self.glides.entry(node).or_default();
        *seen = true;
        g.track(b.rest, now, span);
        let at = |r: [f32; 4]| [b.at.0 + r[0], b.at.1 + r[1], r[2], r[3]];
        let to = at(b.rest);
        let from = g.from.filter(|_| sweeps && g.active(now, span)).map_or([0.0; 4], at);
        let mut bound = to;
        if from[2] > 0.0 {
            let (x0, y0) = (to[0].min(from[0]), to[1].min(from[1]));
            let (x1, y1) = ((to[0] + to[2]).max(from[0] + from[2]), (to[1] + to[3]).max(from[1] + from[3]));
            bound = [x0, y0, x1 - x0, y1 - y0];
        }
        PanelInst {
            rect: bound,
            fill_d: b.ink,
            fill_l: b.ink,
            // The sweep's start (none: width 0) and the bar at rest.
            edge_d: from,
            edge_l: to,
            // The bar's corners, the sweep's seconds, the solid hold's, and the share of a blink each turn eases over.
            shape: [look.radius, span, look.hold, look.fade],
            // A caret that does not blink holds solid for good.
            fx: [0.0, if b.blink { look.blink } else { 1.0 }, 0.0, 4.0],
            fx2: [0.0, b.alpha, g.start, if b.blink { b.moved } else { f32::MAX }],
            lens: b.clip,
            fx3: [0.0; 4],
            clip: [0.0; 4],
            clipx: [0.0; 4],
        }
    }

    /// After a frame: carets not drawn in it are forgotten, and whether one is still sweeping (the host draws another
    /// frame while one is).
    pub fn end_frame(&mut self) -> bool {
        self.glides.retain(|_, (_, seen)| std::mem::take(seen));
        let (now, span) = (self.now, self.span);
        span > 0.0 && self.glides.values().any(|(g, _)| g.active(now, span))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Typing a character snaps (no sweep); a jump past the caret's height sweeps from where it was, eased into the
    /// new place, and is done after `glide`.
    #[test]
    fn a_jump_sweeps_and_a_step_snaps() {
        let mut g = Glide::default();
        g.track([0.0, 0.0, 1.5, 16.0], 0.0, 0.2);
        assert_eq!(g.from, None);
        g.track([7.0, 0.0, 1.5, 16.0], 0.1, 0.2);
        assert_eq!(g.from, None, "a step shorter than the caret's height snaps");
        g.track([7.0, 40.0, 1.5, 16.0], 0.2, 0.2);
        assert_eq!(g.from, Some([7.0, 0.0, 1.5, 16.0]));
        assert!(g.active(0.3, 0.2));
        let mid = g.tail(0.3, 0.2).unwrap();
        assert!(mid[1] > 20.0 && mid[1] < 40.0, "ease-out: past halfway at half the time ({mid:?})");
        assert!(!g.active(0.41, 0.2));
    }
}
