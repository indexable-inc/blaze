//! Springs: one clock, closed-form integration, velocity kept when the target changes (interruptible).

#[derive(Clone, Copy, Debug, Default, PartialEq, harness_native_flecs::Layout)]
pub struct Spring {
    pub x: f32,
    pub v: f32,
}

/// Stiffness (angular frequency, rad/s) and damping ratio, the two numbers a scene token carries.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tune {
    pub omega: f32,
    pub zeta: f32,
}

impl Spring {
    #[must_use]
    pub const fn at(x: f32) -> Self {
        Self { x, v: 0.0 }
    }

    /// Advance by `dt` seconds towards `target`: the exact solution of the damped oscillator, so any `dt` is stable.
    pub fn step(&mut self, target: f32, tune: Tune, dt: f32) {
        let Tune { omega: w, zeta: z } = tune;
        let x0 = self.x - target;
        let v0 = self.v;
        let (x, v) = if z < 0.999 {
            let wd = w * (1.0 - z * z).sqrt();
            let e = (-z * w * dt).exp();
            let (s, c) = (wd * dt).sin_cos();
            let b = (v0 + z * w * x0) / wd;
            let x = e * (x0 * c + b * s);
            let v = e * (-z * w * (x0 * c + b * s) + (-x0 * wd * s + b * wd * c));
            (x, v)
        } else if z <= 1.001 {
            let e = (-w * dt).exp();
            let b = v0 + w * x0;
            (e * (x0 + b * dt), e * (v0 - w * b * dt))
        } else {
            let q = (z * z - 1.0).sqrt();
            let (r1, r2) = (-w * (z - q), -w * (z + q));
            let c2 = (v0 - r1 * x0) / (r2 - r1);
            let c1 = x0 - c2;
            let (e1, e2) = ((r1 * dt).exp(), (r2 * dt).exp());
            (c1 * e1 + c2 * e2, c1 * r1 * e1 + c2 * r2 * e2)
        };
        self.x = target + x;
        self.v = v;
    }

    #[must_use]
    pub fn settled(&self, target: f32) -> bool {
        (self.x - target).abs() < 1e-3 && self.v.abs() < 1e-2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CRIT: Tune = Tune { omega: 18.0, zeta: 1.0 };

    fn run(s: &mut Spring, target: f32, tune: Tune, secs: f32) {
        let steps = (secs * 240.0) as usize;
        for _ in 0..steps {
            s.step(target, tune, 1.0 / 240.0);
        }
    }

    #[test]
    fn settles_on_target_without_overshoot_when_critically_damped() {
        let mut s = Spring::at(0.0);
        let mut max = 0.0f32;
        for _ in 0..480 {
            s.step(1.0, CRIT, 1.0 / 240.0);
            max = max.max(s.x);
        }
        assert!(max <= 1.0 + 1e-4, "overshoot {max}");
        assert!(s.settled(1.0), "{s:?}");
    }

    #[test]
    fn underdamped_overshoots_and_overdamped_does_not() {
        let mut a = Spring::at(0.0);
        let mut peak = 0.0f32;
        for _ in 0..480 {
            a.step(1.0, Tune { omega: 18.0, zeta: 0.5 }, 1.0 / 240.0);
            peak = peak.max(a.x);
        }
        assert!(peak > 1.05);
        let mut b = Spring::at(0.0);
        run(&mut b, 1.0, Tune { omega: 18.0, zeta: 2.0 }, 2.0);
        assert!(b.x <= 1.0 + 1e-4 && b.settled(1.0), "{b:?}");
    }

    #[test]
    fn step_size_does_not_change_the_answer() {
        let (mut a, mut b) = (Spring::at(0.0), Spring::at(0.0));
        for _ in 0..30 {
            a.step(1.0, CRIT, 1.0 / 120.0);
        }
        for _ in 0..60 {
            b.step(1.0, CRIT, 1.0 / 240.0);
        }
        assert!((a.x - b.x).abs() < 1e-4 && (a.v - b.v).abs() < 1e-3);
    }

    #[test]
    fn retargeting_keeps_velocity() {
        let mut s = Spring::at(0.0);
        run(&mut s, 1.0, CRIT, 0.08);
        let v = s.v;
        assert!(v > 0.0);
        s.step(0.0, CRIT, 1.0 / 240.0);
        // velocity changes smoothly (no jump to zero) and x keeps moving the same way for a moment
        assert!(s.v > 0.0 && s.v < v * 1.5);
    }
}
