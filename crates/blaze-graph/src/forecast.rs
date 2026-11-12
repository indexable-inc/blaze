//! A forecast corrected by what tasks really held (docs/v2/plan/design/memory.md, "Forecasts"):
//! a sans-IO core on the forecasts and peaks it is told.
//!
//! Admission grants a task what it is forecast to hold: its own key's record where one was
//! measured, else its unit's source size times what its kind held per KiB lately, else its
//! kind's peak, else a default for a kind never seen. On a cold build none of those knows
//! what a task pulls of the front end: the first task to ask for a dependency's bodies,
//! inference and MIR is charged for them. A cold build of tempo (2026-11-12) granted a
//! lowering 0.06 GiB that held 7.27, a compile 0.16 that held 6.71 and a link 0.06 that held
//! 4.73: forecasts 20 to 100 times low, each made the same way as the one before it.
//!
//! [`Correction`] closes that loop within the build. Each task that ends says its base
//! forecast (what admission's rules gave it before any correction) and its peak;
//! [`Correction::corrected`] is the next base forecast of that kind times the kind's
//! [`Scale`]:
//!
//! 1. A scale is never under 1: a correction never lowers a forecast.
//! 2. A peak the scale would not have covered raises it at once to what would have
//!    (`peak / base`, at most [`MOST`]).
//! 3. [`WINDOW`] peaks in a row each under half of what the scale forecast bring its excess
//!    over 1 down by half: one large task does not hold a kind at its ratio for good.
//! 4. A kind no task of which has ended is corrected by every kind's tasks together: on a
//!    cold build, what the first tasks showed of any kind is the best guess for the next
//!    kind's first.
//!
//! The base forecast, not the grant, is what a peak is compared with: grants made before a
//! correction and after it then teach the same ratio, and the ratio of a batch of tasks
//! pushed together does not multiply up as each ends.

#![blaze::sans_io]

use std::alloc::Global;

use blaze_hash::HashMap;

use crate::admission::TaskKind;

/// How many peaks under half their corrected forecast, in a row, bring a scale down a step:
/// `admission::WINDOW`'s 64.
pub const WINDOW: u32 = crate::admission::WINDOW;

/// One, in a scale's units: a scale is a ratio in 1024ths.
pub const ONE: u64 = 1024;

/// The most a scale is: 4096 times. A base forecast of 64 MiB then reaches 256 GiB, past any
/// budget, where the guess is the whole budget and the task runs alone.
pub const MOST: u64 = 4096 * ONE;

/// How far the tasks of one kind peaked past their base forecasts lately, as a ratio in
/// 1024ths, never under [`ONE`] (the module's rules 1 to 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scale {
    /// The ratio, in 1024ths.
    pub ratio: u64,
    /// How many peaks under half their corrected forecast came in a row.
    pub under: u32,
    /// How many tasks it was told.
    pub seen: u64,
}

impl Default for Scale {
    fn default() -> Scale {
        Scale { ratio: ONE, under: 0, seen: 0 }
    }
}

impl Scale {
    /// `base` corrected: times the ratio.
    pub fn apply(&self, base: u64) -> u64 {
        let scaled = u128::from(base) * u128::from(self.ratio) / u128::from(ONE);
        u64::try_from(scaled).unwrap_or(u64::MAX)
    }

    /// A task whose base forecast was `base` held `peak` at most. One with no base forecast
    /// (a task that asks no memory) teaches nothing.
    pub fn observe(&mut self, base: u64, peak: u64) {
        if base == 0 {
            return;
        }
        self.seen = self.seen.saturating_add(1);
        let forecast = self.apply(base);
        if peak > forecast {
            let needed = (u128::from(peak) * u128::from(ONE)).div_ceil(u128::from(base));
            self.ratio = u64::try_from(needed).unwrap_or(MOST).min(MOST).max(self.ratio);
            self.under = 0;
        } else if peak.saturating_mul(2) < forecast && self.ratio > ONE {
            self.under += 1;
            if self.under >= WINDOW {
                self.ratio -= (self.ratio - ONE).div_ceil(2);
                self.under = 0;
            }
        } else {
            self.under = 0;
        }
    }
}

/// Each kind's [`Scale`], and every kind's together (the module's rule 4). An observe of a
/// kind seen before allocates nothing.
#[derive(Debug)]
pub struct Correction {
    by_kind: HashMap<TaskKind, Scale, Global>,
    all: Scale,
}

impl Default for Correction {
    fn default() -> Correction {
        Correction::new()
    }
}

impl Correction {
    pub fn new() -> Correction {
        // alloc: the plane's, an entry a kind, kept for the daemon's life
        Correction { by_kind: HashMap::new_in(Global), all: Scale::default() }
    }

    /// The scale a task of `kind` is corrected by: its kind's once a task of it ended, else
    /// every kind's.
    pub fn scale(&self, kind: TaskKind) -> Scale {
        match self.by_kind.get(&kind) {
            Some(scale) if scale.seen > 0 => *scale,
            _ => self.all,
        }
    }

    /// `base`, the forecast admission's rules give a task of `kind`, corrected.
    pub fn corrected(&self, kind: TaskKind, base: u64) -> u64 {
        self.scale(kind).apply(base)
    }

    /// A task of `kind` whose base forecast was `base` ended having held `peak` at most.
    pub fn observe(&mut self, kind: TaskKind, base: u64, peak: u64) {
        // alloc: none past the kinds seen
        self.by_kind.entry(kind).or_default().observe(base, peak);
        self.all.observe(base, peak);
    }
}

#[cfg(test)]
mod tests {
    use blaze_rng::SplitMix64;

    use super::*;
    use crate::admission::kinds;

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;

    /// The cold build of tempo, 2026-11-12: a lowering forecast 0.06 GiB held 7.27.
    #[test]
    fn a_kind_that_held_a_hundred_times_its_forecast_is_forecast_that_from_its_next_task() {
        let mut correction = Correction::new();
        let base = 64 * MIB;
        // Before any task ended: the forecast as admission's rules give it (the control).
        assert_eq!(correction.corrected(kinds::LOWER, base), base);
        let peak = 7 * GIB + 276 * MIB;
        correction.observe(kinds::LOWER, base, peak);
        assert!(correction.corrected(kinds::LOWER, base) >= peak);
        // A kind none of whose tasks ended takes every kind's ratio: no 0.06 GiB for it.
        assert!(correction.corrected(kinds::UNIT_LINK, base) >= peak);
        // Once one of its own ended within its forecast, its own ratio stands.
        correction.observe(kinds::UNIT_LINK, base, base / 2);
        assert_eq!(correction.corrected(kinds::UNIT_LINK, base), base);
    }

    /// Tasks pushed together, each forecast before any ended, teach the largest of their
    /// ratios, not their product.
    #[test]
    fn a_batch_of_tasks_forecast_together_teaches_its_largest_ratio() {
        let mut correction = Correction::new();
        for ratio in [50, 20, 100, 30] {
            correction.observe(kinds::COMPILE, 64 * MIB, ratio * 64 * MIB);
        }
        assert_eq!(correction.scale(kinds::COMPILE).ratio, 100 * ONE);
    }

    /// The invariant: over any run of tasks a correction never lowers a forecast; a forecast
    /// made right after a task ended covers that task's peak (short of [`MOST`]); a scale
    /// moves down only at the [`WINDOW`]th peak in a row under half its forecast, and then by
    /// half its excess over one.
    #[test]
    fn a_scale_covers_the_last_peak_and_falls_only_after_a_window_of_small_ones() {
        for seed in 0..256 {
            let mut random = SplitMix64::new(seed);
            let mut scale = Scale::default();
            let mut small = 0u32;
            for step in 0..600 {
                let base = (1 + random.below(64)) * MIB;
                let before = scale;
                // Mostly within the forecast; now and then far past it, or far under.
                let forecast = before.apply(base);
                let peak = match random.below(10) {
                    0 => forecast.saturating_mul(1 + random.below(8)),
                    1..=6 => forecast / 3,
                    _ => forecast / 2 + random.below(forecast / 2 + 1),
                };
                scale.observe(base, peak);
                let said = format!("seed {seed}, step {step}: {before:?} told {base} held {peak} gave {scale:?}");
                assert!(scale.ratio >= ONE && scale.ratio <= MOST, "{said}");
                assert!(scale.apply(base) >= base, "{said}");
                if scale.ratio < MOST {
                    assert!(scale.apply(base) >= peak, "{said}");
                }
                let under = peak.saturating_mul(2) < forecast && before.ratio > ONE;
                small = if under && peak <= forecast { small + 1 } else { 0 };
                if scale.ratio < before.ratio {
                    assert_eq!(small, WINDOW, "{said}: it fell before a window of small peaks");
                    assert_eq!(scale.ratio, before.ratio - (before.ratio - ONE).div_ceil(2), "{said}");
                    small = 0;
                } else {
                    assert!(small < WINDOW, "{said}: a window of small peaks left it standing");
                }
            }
        }
    }
}
