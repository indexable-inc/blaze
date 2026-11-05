//! Trackpad feedback, as data: which pulse to play, and one pulse per deliberate threshold crossing with room to retreat
//! before rearming (den's `haptic.rs`, after os's `haptic_threshold`). Pure: window.rs plays the pulses through
//! NSHapticFeedbackManager.

/// The NSHapticFeedbackPattern a pulse plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Haptic {
    /// Something lined up: the sidebar's edge latching closed or open.
    Alignment,
    /// A limit was reached: the sidebar's edge touching its widest.
    LevelChange,
}

/// Whether a value has touched a threshold, so it pulses once per crossing.
#[derive(Clone, Copy, Debug, Default)]
pub struct Threshold {
    contact: Option<f32>,
}

impl Threshold {
    /// A gesture starting at a limit must not pulse merely because it was grabbed.
    #[must_use]
    pub fn new(value: f32, threshold: f32) -> Self {
        Self { contact: (value >= threshold).then_some(threshold) }
    }

    /// True once when `value` reaches `threshold`; again only after it fell `rearm` below both the threshold it
    /// touched and the current one (the window's width can move the threshold under a still finger).
    pub fn crossed(&mut self, value: f32, threshold: f32, rearm: f32) -> bool {
        if !value.is_finite() || !threshold.is_finite() {
            return false;
        }
        if let Some(contact) = self.contact {
            if value <= threshold.min(contact) - rearm {
                self.contact = None;
            }
            return false;
        }
        if value >= threshold {
            self.contact = Some(threshold);
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holding_and_boundary_jitter_do_not_repeat_feedback() {
        let mut t = Threshold::default();
        assert!(!t.crossed(99.0, 100.0, 8.0));
        assert!(t.crossed(100.0, 100.0, 8.0));
        for v in [101.0, 500.0, 99.0, 100.0, 93.0, 105.0] {
            assert!(!t.crossed(v, 100.0, 8.0));
        }
        assert!(!t.crossed(92.0, 100.0, 8.0));
        assert!(t.crossed(100.0, 100.0, 8.0));
    }

    #[test]
    fn starting_at_the_limit_needs_a_retreat_first() {
        let mut t = Threshold::new(100.0, 100.0);
        assert!(!t.crossed(120.0, 100.0, 8.0));
        assert!(!t.crossed(91.0, 100.0, 8.0));
        assert!(t.crossed(100.0, 100.0, 8.0));
    }

    #[test]
    fn a_moving_threshold_does_not_rearm_a_still_finger() {
        let mut t = Threshold::default();
        assert!(t.crossed(44.0, 40.0, 8.0));
        for th in [60.0, 40.0, 60.0, 40.0] {
            assert!(!t.crossed(44.0, th, 8.0));
        }
        assert!(!t.crossed(32.0, 60.0, 8.0));
        assert!(t.crossed(44.0, 40.0, 8.0));
    }
}
