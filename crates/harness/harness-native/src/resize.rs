//! Dragging the sidebar's edge (den's `resize.rs`, after os's `sidebar_resize`): a pure state machine on pointer
//! positions, no AppKit and no clock. The edge stays under the pointer from where it was grabbed, between `min` and
//! `max`; dragging well past half of `min` toward the left closes it and back out reopens it, each needing `latch`
//! points of extra travel so it never flickers at the boundary; past `max` it stretches with diminishing gain (`give`
//! at most, half of it `soft` points past) and pulses once on contact, again only after a `rearm` retreat. The app
//! (src/app/side_drag.rs) turns an [`Update`] into the sidebar's width and fold, the window's rubber band and the
//! trackpad's pulses; every number comes from the scene's tokens (`side_*`, 00-tokens.flecs).
use crate::haptic::Threshold;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    pub min: f32,
    pub max: f32,
    pub latch: f32,
    pub give: f32,
    pub soft: f32,
    pub rearm: f32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Gesture {
    offset: f32,
    open: bool,
    limit: Threshold,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Update {
    /// The width it rests at (0 closed).
    pub width: f32,
    /// Where the edge is drawn: the width with the stretch past the maximum (the window takes the stretch).
    pub display: f32,
    /// It just closed or opened.
    pub snapped: bool,
    /// It just touched the maximum.
    pub reached_limit: bool,
}

impl Gesture {
    /// Grabbed at `pointer` (x, from the sidebar's left) with the edge at `width` (0: closed).
    #[must_use]
    pub fn new(pointer: f32, width: f32, limits: Limits) -> Self {
        Self { offset: width - pointer, open: width > 0.0, limit: Threshold::new(width, limits.max) }
    }

    /// The pointer moved to `pointer` (x, from where the sidebar's left rests).
    pub fn update(&mut self, pointer: f32, limits: Limits) -> Update {
        let max = limits.max.max(0.0);
        let min = limits.min.clamp(0.0, max);
        let requested = pointer + self.offset;
        let latch = limits.latch.max(0.0).min(min / 3.0);
        let boundary = min / 2.0;
        let was = self.open;
        if self.open && requested < boundary - latch {
            self.open = false;
        } else if !self.open && requested > boundary + latch {
            self.open = true;
        }
        let width = if self.open { requested.clamp(min, max) } else { 0.0 };
        let excess = (requested - max).max(0.0);
        let stretch = if self.open { limits.give.max(0.0) * excess / (limits.soft.max(1.0) + excess) } else { 0.0 };
        Update {
            width,
            display: width + stretch,
            snapped: self.open != was,
            reached_limit: self.open && self.limit.crossed(requested, max, limits.rearm.max(0.0)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: Limits = Limits { min: 200.0, max: 420.0, latch: 8.0, give: 24.0, soft: 48.0, rearm: 8.0 };

    #[test]
    fn the_edge_stays_where_it_was_grabbed_and_holds_to_the_bounds() {
        let mut g = Gesture::new(238.0, 240.0, L);
        assert_eq!(g.update(258.0, L).width, 260.0);
        assert_eq!(g.update(900.0, L).width, 420.0);
        assert_eq!(g.update(150.0, L).width, 200.0, "the minimum holds until the close boundary");
    }

    #[test]
    fn closing_and_reopening_need_extra_travel_and_snap_once() {
        let mut g = Gesture::new(240.0, 240.0, L);
        assert_eq!(g.update(93.0, L).width, 200.0);
        let closed = g.update(91.0, L);
        assert!(closed.snapped && closed.width == 0.0);
        assert!(!g.update(60.0, L).snapped);
        assert_eq!(g.update(107.0, L).width, 0.0, "reopening needs the latch past the boundary too");
        let open = g.update(109.0, L);
        assert!(open.snapped && open.width == 200.0);
        assert_eq!(g.update(300.0, L).width, 300.0);
    }

    #[test]
    fn jitter_at_the_boundary_never_flickers() {
        // A hand shaking up to the latch either side of the close boundary (100) snaps at most once, whichever side it
        // started on; it takes a deliberate move past the latch to change state again.
        for start in [240.0_f32, 0.0] {
            let mut g = Gesture::new(start, start, L);
            let mut snaps = 0;
            for i in 0..400 {
                let wobble = ((i as f32) * 0.7).sin() * 7.9;
                snaps += usize::from(g.update(100.0 + wobble, L).snapped);
            }
            assert_eq!(snaps, 0, "a wobble inside the latch from {start} never snaps");
            let mut g = Gesture::new(start, start, L);
            let mut snaps = 0;
            for i in 0..400 {
                // Drifting across the boundary while wobbling: one snap on the way through.
                let x = if start > 0.0 { 140.0 - i as f32 * 0.2 } else { 60.0 + i as f32 * 0.2 };
                snaps += usize::from(g.update(x + ((i as f32) * 1.3).sin() * 7.0, L).snapped);
            }
            assert_eq!(snaps, 1, "one snap crossing the boundary from {start}");
        }
    }

    #[test]
    fn past_the_maximum_it_stretches_a_bounded_amount_and_pulses_once() {
        let mut g = Gesture::new(240.0, 240.0, L);
        assert!(!g.update(419.0, L).reached_limit);
        let at = g.update(420.0, L);
        assert!(at.reached_limit && at.display == 420.0);
        let s = g.update(468.0, L);
        assert_eq!((s.width, s.display), (420.0, 432.0));
        assert!(!s.reached_limit);
        assert!(g.update(10_000.0, L).display < 444.0);
        assert_eq!(g.update(400.0, L).display, 400.0, "no dead zone on the way back");
        assert!(!g.update(411.0, L).reached_limit);
        assert!(g.update(420.0, L).reached_limit, "rearmed after an 8 pt retreat");
    }

    #[test]
    fn one_pulse_per_crossing_of_the_maximum() {
        let mut g = Gesture::new(300.0, 300.0, L);
        let mut pulses = 0;
        // Hovering around the maximum within the rearm: one pulse.
        for i in 0..200 {
            pulses += usize::from(g.update(420.0 + ((i as f32) * 0.9).sin() * 7.5, L).reached_limit);
        }
        assert_eq!(pulses, 1);
        // Three deliberate trips out and back: three more.
        for _ in 0..3 {
            for x in [300.0, 380.0, 430.0, 500.0] {
                pulses += usize::from(g.update(x, L).reached_limit);
            }
        }
        assert_eq!(pulses, 4);
    }

    #[test]
    fn a_closed_sidebar_never_pulses_or_stretches() {
        let mut g = Gesture::new(240.0, 240.0, L);
        assert!(g.update(10.0, L).snapped);
        let u = g.update(-500.0, L);
        assert_eq!((u.width, u.display, u.reached_limit), (0.0, 0.0, false));
    }

    #[test]
    fn grabbed_at_the_maximum_it_does_not_pulse_until_it_retreats() {
        let mut g = Gesture::new(420.0, 420.0, L);
        assert!(!g.update(440.0, L).reached_limit);
        assert!(!g.update(400.0, L).reached_limit);
        assert!(g.update(421.0, L).reached_limit);
    }

    #[test]
    fn the_stretch_is_monotonic_and_bounded() {
        let mut g = Gesture::new(300.0, 300.0, L);
        let mut last = 0.0_f32;
        for i in 0..20_000 {
            let x = 300.0 + i as f32 * 0.5;
            let u = g.update(x, L);
            assert!(u.display >= last, "the drawn edge never moves back while the pointer moves out ({x})");
            assert!(u.display - u.width < L.give, "the stretch stays under give ({x})");
            assert!(u.width <= L.max);
            last = u.display;
        }
        // And on the way back the drawn edge follows the pointer down, never up.
        for i in 0..20_000 {
            let x = 10_300.0 - i as f32 * 0.5;
            let u = g.update(x, L);
            assert!(u.display <= last, "{x}");
            last = u.display;
        }
    }

    #[test]
    fn reopening_from_closed_lands_the_edge_under_the_pointer() {
        let mut g = Gesture::new(2.0, 0.0, L);
        assert!(!g.update(90.0, L).snapped);
        let u = g.update(120.0, L);
        assert!(u.snapped && u.width == 200.0, "past the latch it opens at its minimum");
        assert_eq!(g.update(262.0, L).width, 260.0, "then the edge follows the pointer from where it was grabbed");
    }
}
