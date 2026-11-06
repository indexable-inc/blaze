//! A text area that grows with its words and then scrolls (`ui.field.GrowingTextArea`, harness-native-ui-prims
//! scene/field.flecs). Its height is layout's (`GrowLimit`: up to a share of its pane); its scroll is a [`FieldScroll`] its
//! host keeps per area and steps each frame. A wheel over it runs AppKit's scroll physics (harness-native-scroll-physics,
//! as the sidebar's tree and a chat's transcript do), and when the caret moves out of view (typing, arrows, a paste) the
//! area glides on its `CaretScroll` spring until the caret's line shows `margin` points inside the view, wherever the
//! caret is. Following the caret moves it quietly (`scroll_quietly`); a wheel shows its overlay scroller.
use std::time::Instant;

use harness_native_api::settle;
use harness_native_api::spring::{Spring, Tune};
use harness_native_flecs::prelude::*;
use harness_native_scroll_physics::{Engine, Input, Mutation, Phase, SurfaceConfig, Vec2};
use harness_native_ui_layout_api::{CaretScroll, ScrollAxis, ScrollExtent, ScrollState, ScrollerGrip, overflows, scroll_quietly};
use harness_native_ui_node_api::Laid;

/// One growing text area's scroll: its physics, the origin it shows, a glide to the caret, and where the caret's line was.
pub struct FieldScroll {
    engine: Engine,
    clock: Instant,
    /// When it last stepped (seconds on `clock`), for the glide's spring.
    last: f64,
    /// The origin shown (points down from the content's top; past 0 or the end while a rubber band holds).
    y: f32,
    /// Bumped for each origin it sets itself (a glide's step, a pull back inside): the engine takes it as a command.
    rev: usize,
    /// A glide that brings the caret into view: its spring and where it goes.
    glide: Option<(Spring, f32)>,
    /// The caret's line at the last step (top and foot, points down the content): a glide starts only when it moves.
    caret: Option<[f32; 2]>,
}

impl Default for FieldScroll {
    fn default() -> Self {
        Self {
            engine: Engine::new(config([0.0; 2], 0.0, 0.0, 0, 0.5), Mutation::None),
            clock: Instant::now(),
            last: 0.0,
            y: 0.0,
            rev: 0,
            glide: None,
            caret: None,
        }
    }
}

/// The physics' surface for an area `view` (`[w, h]`) over `content` points of words at `origin`: down only.
fn config(view: [f32; 2], content: f32, origin: f32, rev: usize, points_per_pixel: f64) -> SurfaceConfig {
    SurfaceConfig {
        viewport: Vec2::new(f64::from(view[0]), f64::from(view[1])),
        content: Vec2::new(f64::from(view[0]), f64::from(content)),
        axes: [false, true],
        predominant_axis: true,
        origin: Vec2::new(0.0, f64::from(origin)),
        command_revision: rev,
        points_per_pixel,
        frame_interval: 1.0 / 120.0,
    }
}

/// The scroll that brings a line `[top, foot]` (content points) into a view `view` long at `at`, `margin` inside its
/// edges (less on a view too short for it), over content `len` long; None while it shows already.
fn caret_goal([top, foot]: [f32; 2], at: f32, view: f32, len: f32, margin: f32) -> Option<f32> {
    let m = margin.min(((view - (foot - top)) / 2.0).max(0.0));
    let goal = if top - m < at {
        top - m
    } else if foot + m > at + view {
        foot + m - view
    } else {
        return None;
    };
    Some(goal.clamp(0.0, (len - view).max(0.0)))
}

impl FieldScroll {
    /// A scroll event over `area`: deltas in view points when `precise`, else lines, with AppKit's gesture and momentum
    /// phases (`NSEventPhase` bits). It takes it only while the area has more words than it shows (else the wheel is the
    /// pane's); whether it did. A wheel lets go of a glide to the caret.
    pub fn wheel(&mut self, area: EntityView<'_>, delta: [f32; 2], precise: bool, phase: usize, momentum: usize) -> bool {
        let Some(l) = area.try_cloned::<&Laid>() else { return false };
        let content = area.try_cloned::<&ScrollExtent>().map_or(l.h, |x| x.h);
        if !overflows([l.w, l.h], [l.w, content], ScrollAxis::Y) {
            return false;
        }
        let t = self.clock.elapsed().as_secs_f64();
        self.glide = None;
        self.engine.wheel(
            Input {
                delta: Vec2::new(f64::from(delta[0]), f64::from(delta[1])),
                precise,
                direct: Phase::from_ns(phase),
                momentum: Phase::from_ns(momentum),
                timestamp: t,
            },
            t,
        );
        true
    }

    /// Step `area`'s scroll a frame from its last layout: `caret` is its caret's line in window points (top and foot;
    /// None while it has none laid out). A caret that moved out of view starts a glide to it; the glide or the physics
    /// advance and the area's `ScrollState` follows. Whether it still moves (the host keeps drawing frames).
    ///
    /// # Panics
    /// When `area` has no `CaretScroll` (`ui.field.GrowingTextArea` gives it one): the area names itself.
    pub fn step(&mut self, area: EntityView<'_>, caret: Option<[f32; 2]>, points_per_pixel: f64) -> bool {
        let Some(l) = area.try_cloned::<&Laid>().filter(|l| l.h > 0.0) else { return false };
        let look = area
            .try_cloned::<&CaretScroll>()
            .unwrap_or_else(|| panic!("text area {} has no CaretScroll (ui.field.GrowingTextArea sets it)", area.path()));
        let content = area.try_cloned::<&ScrollExtent>().map_or(l.h, |x| x.h);
        let max = (content - l.h).max(0.0);
        let now = self.clock.elapsed().as_secs_f64();
        let dt = (now - self.last).clamp(0.0, 0.1) as f32;
        self.last = now;
        // The scroll the last layout placed the words at: the caret's window place less it is a place in the content.
        let shown = area.try_cloned::<&ScrollState>().unwrap_or_default();
        let line = caret.map(|[top, foot]| [top - l.y + shown.y, foot - l.y + shown.y]);
        let moved = match (line, self.caret) {
            (Some(a), Some(b)) => (a[0] - b[0]).abs() > 0.5 || (a[1] - b[1]).abs() > 0.5,
            (a, b) => a.is_some() != b.is_some(),
        };
        self.caret = line;
        // A drag or a page on the overlay scroller moved it: that is the origin now (and the caret stays where it is put).
        if (shown.y - self.y).abs() > 0.01 && area.try_cloned::<&ScrollerGrip>().is_some_and(|g| g.drag != 0 || g.page != 0) {
            self.y = shown.y;
            self.glide = None;
            self.rev += 1;
        } else if moved && let Some(line) = line {
            let at = self.glide.map_or(self.y, |(_, goal)| goal);
            if let Some(goal) = caret_goal(line, at, l.h, content, look.margin) {
                let spring = self.glide.map_or(Spring::at(self.y), |(s, _)| s);
                self.glide = Some((spring, goal));
            }
        }
        let mut origin = self.y;
        let gliding = self.glide.is_some();
        if let Some((mut s, goal)) = self.glide.take() {
            let goal = goal.clamp(0.0, max);
            let going = settle(&mut s, goal, Tune { omega: look.omega, zeta: look.zeta }, dt);
            origin = if going { s.x } else { goal };
            self.rev += 1;
            if going {
                self.glide = Some((s, goal));
            }
        } else if self.y > max && !self.engine.needs_frame() {
            // Fewer words under the origin: back inside them.
            origin = max;
            self.rev += 1;
        }
        self.engine.configure(config([l.w, l.h], content, origin, self.rev, points_per_pixel));
        self.engine.advance(now);
        let y = self.engine.report().y as f32;
        self.y = y;
        if (shown.y - y).abs() > 0.001 || shown.x != 0.0 {
            let s = ScrollState { x: 0.0, y };
            // Following the caret moves it quietly; a wheel's scroll shows the scroller.
            if gliding || !self.engine.needs_frame() {
                scroll_quietly(area, s);
            } else {
                area.set(s);
            }
        }
        self.engine.needs_frame() || self.glide.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::caret_goal;

    /// A 100-point view over 300 points of words at 50: a line in view stays put; one above or below is brought in 6
    /// points inside the near edge; the ends clamp to the content; a view too short for the margin centres the line.
    #[test]
    fn the_caret_goal_moves_as_little_as_it_can() {
        assert_eq!(caret_goal([80.0, 100.0], 50.0, 100.0, 300.0, 6.0), None);
        assert_eq!(caret_goal([20.0, 40.0], 50.0, 100.0, 300.0, 6.0), Some(14.0));
        assert_eq!(caret_goal([160.0, 180.0], 50.0, 100.0, 300.0, 6.0), Some(86.0));
        assert_eq!(caret_goal([0.0, 20.0], 50.0, 100.0, 300.0, 6.0), Some(0.0));
        assert_eq!(caret_goal([280.0, 300.0], 50.0, 100.0, 300.0, 6.0), Some(200.0));
        assert_eq!(caret_goal([160.0, 180.0], 50.0, 24.0, 300.0, 6.0), Some(158.0));
    }
}
