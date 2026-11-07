//! A level slider's behaviour (level.flecs `ui.LevelSlider`): showing it on a stop ([`show`]), the stop under the pointer
//! for a press or a drag ([`stop_at`]), and a sideways two-finger swipe stepping it a stop per detent ([`Swipe`], sans IO:
//! the owner feeds it the wheel events and steps the level by what it says). Any control that sets one of a few ordered
//! values (an effort, a quality) uses it; the owner keeps which stop is chosen and what a change does.
use crate::View;
use harness_native_ui_layout_api::Size;
use harness_native_ui_node_api::Laid;

/// What a slider says in words: what it sets (`label`), the stop's name (`value`), and what its ends mean (`lo`, `hi`).
#[derive(Clone, Copy, Debug, Default)]
pub struct Words<'a> {
    pub label: &'a str,
    pub value: &'a str,
    pub lo: &'a str,
    pub hi: &'a str,
}

/// How far along the track stop `at` of `n` is (0 at the first, 1 at the last).
#[must_use]
pub fn share(at: usize, n: usize) -> f32 {
    if n < 2 { 0.0 } else { at.min(n - 1) as f32 / (n - 1) as f32 }
}

/// Grow 0 counts as a whole share (layout's rule), so an empty side keeps a sliver instead.
const SLIVER: f32 = 1e-4;

/// Show slider `v` with its knob on stop `at` of `n` (a dot per stop), and its `words`. The knob glides there: the fill's
/// and the rest's Grow weights ease on the level spring (their `Animate`).
pub fn show(v: View<'_>, at: usize, n: usize, words: Words<'_>) {
    let Some(track) = v.part("track") else { return };
    let f = share(at, n);
    for (part, w) in [("fill", f), ("rest", 1.0 - f)] {
        let Some(p) = track.part(part) else { continue };
        let mut s = p.entity().try_cloned::<&Size>().unwrap_or_default();
        let w = w.max(SLIVER);
        if (s.w.value - w).abs() > 1e-6 {
            s.w.value = w;
            p.set(s);
        }
    }
    if let Some(ticks) = track.part("ticks") {
        crate::for_each_of(ticks, (0..n).map(|i| (format!("t{i}"), "ui.LevelTick", ())), |_, (), _| {});
    }
    for (path, s) in [(["head", "label"], words.label), (["head", "value"], words.value), (["scale", "lo"], words.lo), (["scale", "hi"], words.hi)] {
        if let Some(p) = v.part(path[0]).and_then(|h| h.part(path[1])) {
            p.text(s);
        }
    }
}

/// The stop of `n` nearest window-point `x` on slider `v`'s track: where the knob's middle would be (its middle runs from
/// half a knob in at the first stop to half a knob in from the end at the last). None before it is laid or with no stops.
#[must_use]
pub fn stop_at(v: View<'_>, x: f32, n: usize) -> Option<usize> {
    let track = v.part("track")?;
    let l = track.entity().try_cloned::<&Laid>()?;
    let knob = track.part("knob").and_then(|k| k.entity().try_cloned::<&Laid>()).map_or(0.0, |k| k.w);
    nearest(x - l.x - knob / 2.0, l.w - knob, n)
}

/// The stop of `n` nearest `along` points into a run `len` long.
fn nearest(along: f32, len: f32, n: usize) -> Option<usize> {
    if n == 0 || len <= 0.0 || !along.is_finite() {
        return None;
    }
    let f = (along / len).clamp(0.0, 1.0);
    Some(((f * (n - 1) as f32).round() as usize).min(n - 1))
}

/// AppKit's scroll phases (NSEventPhase), as the window passes them.
const BEGAN: usize = 1;
const MAY_BEGIN: usize = 32;

/// Which way a two-finger swipe goes, decided once at its start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Axis {
    #[default]
    Undecided,
    /// Sideways: it steps the level.
    Across,
    /// Up or down: not the slider's (the owner passes it on: a scroll).
    Along,
}

/// How far a swipe must go before it says which way it goes (points).
const DECIDE: f32 = 3.0;
/// How much more sideways than up or down a swipe must start to be the slider's.
const ACROSS: f32 = 1.2;

/// A two-finger swipe over a level slider, as detents: each `step` points the fingers travel sideways is one stop, the
/// rest kept toward the next (so a finger resting on a boundary does not flip back and forth: going back takes a whole
/// step). A swipe that starts up or down is not the slider's; momentum after a sideways swipe is swallowed, never
/// stepping (a flick does not fly to the end). A mouse's wheel (no phases) steps one stop a notch.
#[derive(Clone, Copy, Debug, Default)]
pub struct Swipe {
    axis: Axis,
    /// Sideways travel not yet a stop (points, fingers' direction: + right).
    acc: f32,
    /// Travel before the axis is decided, sideways and along.
    seen: (f32, f32),
}

impl Swipe {
    /// One wheel event: `dx`, `dy` in points the fingers moved (+ right, + down), `precise` (a trackpad's), its `phase`
    /// and `momentum` (NSEventPhase bits). None when the swipe is not the slider's; else how many stops it steps (signed,
    /// often 0).
    pub fn wheel(&mut self, dx: f32, dy: f32, precise: bool, phase: usize, momentum: usize, step: f32) -> Option<i32> {
        if !precise && phase == 0 && momentum == 0 {
            // A mouse's wheel: a notch is a stop; a wheel turned up or down is a scroll.
            return (dx.abs() > dy.abs()).then(|| if dx > 0.0 { 1 } else { -1 });
        }
        if momentum != 0 {
            return (self.axis == Axis::Across).then_some(0);
        }
        if phase & (BEGAN | MAY_BEGIN) != 0 {
            *self = Self::default();
        }
        if self.axis == Axis::Undecided {
            self.seen.0 += dx.abs();
            self.seen.1 += dy.abs();
            if self.seen.0 + self.seen.1 < DECIDE {
                self.acc += dx;
                return Some(0);
            }
            self.axis = if self.seen.0 > self.seen.1 * ACROSS { Axis::Across } else { Axis::Along };
        }
        if self.axis == Axis::Along {
            return None;
        }
        self.acc += dx;
        let n = (self.acc / step.max(1.0)).trunc();
        self.acc -= n * step.max(1.0);
        Some(n as i32)
    }

    /// The level met an end: the travel past it is dropped, so turning back steps at once.
    pub fn hold(&mut self) {
        self.acc = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHANGED: usize = 4;

    #[test]
    fn a_resting_finger_on_a_detent_does_not_flicker() {
        let mut s = Swipe::default();
        assert_eq!(s.wheel(0.0, 0.0, true, BEGAN, 0, 20.0), Some(0));
        let mut stops = 0;
        for dx in [2.0, 4.0, 6.0, 9.0] {
            stops += s.wheel(dx, 0.0, true, CHANGED, 0, 20.0).unwrap();
        }
        assert_eq!(stops, 1, "21 points sideways: one stop");
        for dx in [-1.5, 1.5, -1.5, 1.5, -3.0, 3.0] {
            assert_eq!(s.wheel(dx, 0.0, true, CHANGED, 0, 20.0), Some(0), "jitter at the boundary steps nothing");
        }
        assert_eq!(s.wheel(-21.0, 0.0, true, CHANGED, 0, 20.0), Some(-1), "a whole step back steps back");
    }

    #[test]
    fn up_or_down_is_not_the_sliders_and_momentum_never_steps() {
        let mut s = Swipe::default();
        s.wheel(0.0, 0.0, true, BEGAN, 0, 20.0);
        s.wheel(0.5, 4.0, true, CHANGED, 0, 20.0);
        assert_eq!(s.wheel(30.0, 2.0, true, CHANGED, 0, 20.0), None, "a swipe that began down stays a scroll");
        let mut s = Swipe::default();
        s.wheel(5.0, 0.0, true, BEGAN, 0, 20.0);
        s.wheel(10.0, 0.0, true, CHANGED, 0, 20.0);
        assert_eq!(s.wheel(400.0, 0.0, true, 0, CHANGED, 20.0), Some(0), "momentum is swallowed");
    }

    #[test]
    fn the_stop_under_the_pointer_is_the_nearest() {
        assert_eq!(nearest(0.0, 100.0, 5), Some(0));
        assert_eq!(nearest(12.4, 100.0, 5), Some(0));
        assert_eq!(nearest(12.6, 100.0, 5), Some(1));
        assert_eq!(nearest(-40.0, 100.0, 5), Some(0));
        assert_eq!(nearest(140.0, 100.0, 5), Some(4));
        assert_eq!(nearest(50.0, 100.0, 0), None);
    }
}
