//! What a task is forecast to hold at most, the bytes memory admission grants it as it
//! starts ([`crate::plane::Plane`]): the one rule, as a sans-IO core on what is known of the
//! task.
//!
//! A forecast decides how many tasks run at once, never whether the process stays under its
//! limit: the ceiling on the footprint and its gate do that ([`crate::footprint`],
//! docs/memory-budget.md). So the forecast aims at the task's peak, not above it:
//!
//! - **Own** ([`Basis::Own`]): a task whose learn key (its own identity: a unit's phase, a
//!   program's part) has a record is forecast that record as it is, what its own last runs
//!   held ([`learn`]'s rule). Never maxed with a guess from other tasks: a record under what
//!   its size would guess is the better measure. The target: within [`WITHIN_TENTHS`] tenths of
//!   the peak it then holds.
//! - **First run**: a model of its inputs. Its source's KiB times what tasks of its kind held
//!   per KiB lately, measured ([`Basis::Sized`]), or [`UNSEEN_PER_KIB`] before one was
//!   ([`Basis::Unseen`]); with no size, its kind's measured peak ([`Basis::Kind`]) or
//!   [`GUESS_FLOOR`] ([`Basis::Unseen`]). At least [`GUESS_FLOOR`], and at most one work
//!   thread's share of the budget ([`share`]): with nothing measured every thread may run one.
//!   A guess never takes the whole budget and so never runs alone; nothing multiplies it.
//!
//! [`Check`] holds each basis to the peaks its tasks then measured, so how far the model is
//! off is a number the daemon says, not a guess.
//!
//! History: a guess over the budget was the whole budget and ran alone (the smoke's cold build,
//! 2026-11-12: `program (Codegen)` granted 205,049,909,248 bytes for a peak of 579,740,555, its
//! link 1,519 times its peak); a floor of half the budget's share raised every first guess; a
//! correction factor (deleted 2026-11-12) forecast 66 GB a task and stalled the build. Each
//! held tasks back that the budget held.
#![blaze::sans_io]

/// What a sized task of a kind never measured is forecast to hold per KiB of its source:
/// 1 MiB, a thousand bytes for each byte. Once one of the kind ended, the kind's own measured
/// bytes per KiB take its place.
pub const UNSEEN_PER_KIB: u64 = 1 << 20;

/// The least a guess forecasts, however small the task: 64 MiB, what a task of a kind never
/// seen is predicted (`blaze_history::Config::unseen_bytes`).
pub const GUESS_FLOOR: u64 = 64 << 20;

/// How close an own record's forecast is held to the peak its task then measures: within a
/// tenth of it (docs/v2/plan/design/end-state.md, "within 10% of the floor").
pub const WITHIN_TENTHS: u64 = 1;

/// Where a forecast came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Basis {
    /// Its own learn key's record: what its own last runs held.
    Own,
    /// Its source's size times its kind's measured bytes per KiB: a model checked against the
    /// kind's measured peaks.
    Sized,
    /// Its kind's measured peak, with no size of its own.
    Kind,
    /// Nothing of its kind was measured: the model's defaults.
    Unseen,
    /// Not forecast: the memory its node says, as given (a task that runs because memory is
    /// short, which a forecast would hold back behind the tasks it makes room for). Never
    /// checked against a peak.
    Given,
}

impl Basis {
    pub const ALL: [Basis; 5] =
        [Basis::Own, Basis::Sized, Basis::Kind, Basis::Unseen, Basis::Given];

    /// Its name, as the `task` and `admission` spans' `forecast_basis` say it.
    pub const fn name(self) -> &'static str {
        match self {
            Basis::Own => "own record",
            Basis::Sized => "size x kind's measured rate",
            Basis::Kind => "kind's measured peak",
            Basis::Unseen => "unseen kind's default",
            Basis::Given => "given",
        }
    }

    const fn at(self) -> usize {
        match self {
            Basis::Own => 0,
            Basis::Sized => 1,
            Basis::Kind => 2,
            Basis::Unseen => 3,
            Basis::Given => 4,
        }
    }
}

/// What is known of a task as it is added.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Known {
    /// Its own learn key's record, if its key has one.
    pub own: Option<u64>,
    /// The bytes of its unit's source, 0 for a task not sized.
    pub source: u64,
    /// What tasks of its kind held per KiB of source lately, once one was measured.
    pub per_kib: Option<u64>,
    /// Its kind's measured peak, once one of the kind was measured.
    pub kind: Option<u64>,
    /// The least the task says it needs whatever is known (its spec's memory).
    pub least: u64,
}

/// A forecast: the bytes, and where they came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Forecast {
    pub bytes: u64,
    pub basis: Basis,
}

/// One work thread's share of a budget of `budget` bytes over `threads` work threads: the most
/// a guess forecasts. `u64::MAX` with no budget.
pub fn share(budget: Option<u64>, threads: usize) -> u64 {
    budget.map_or(u64::MAX, |it| (it / threads.max(1) as u64).max(1))
}

/// The forecast of a task known as `known` under a guess's most of `share` ([`share`]).
pub fn forecast(known: &Known, share: u64) -> Forecast {
    if let Some(own) = known.own {
        return Forecast { bytes: own.max(known.least), basis: Basis::Own };
    }
    let (guess, basis) = match (known.source, known.per_kib, known.kind) {
        (0, _, Some(kind)) => (kind, Basis::Kind),
        (0, _, None) => (GUESS_FLOOR, Basis::Unseen),
        (source, rate, kind) => {
            let basis = if rate.is_some() { Basis::Sized } else { Basis::Unseen };
            let sized = source.div_ceil(1024).saturating_mul(rate.unwrap_or(UNSEEN_PER_KIB));
            // A kind's measured peak is the most one of it held lately: no unit of it is
            // forecast past that by its size.
            (kind.map_or(sized, |it| sized.min(it.max(GUESS_FLOOR))), basis)
        }
    };
    let bytes = guess.max(GUESS_FLOOR).max(known.least).min(share);
    Forecast { bytes, basis }
}

/// What an own record becomes after a run that held `peak`: the peak, if it rose, else halfway
/// down to it, so one small run does not undo many large ones. The rule of
/// `blaze_history::Record::observe`.
pub fn learn(record: Option<u64>, peak: u64) -> u64 {
    match record {
        Some(record) if peak < record => record - (record - peak) / 2,
        _ => peak,
    }
}

/// Whether `forecast` is within [`WITHIN_TENTHS`] tenths of `peak`.
pub fn within(forecast: u64, peak: u64) -> bool {
    forecast.abs_diff(peak).saturating_mul(10) <= peak.saturating_mul(WITHIN_TENTHS)
}

/// How one basis's forecasts compared with the peaks their tasks measured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    pub tasks: u64,
    /// Within [`WITHIN_TENTHS`] tenths of the peak.
    pub within: u64,
    /// Over the peak by more than that, and the bytes forecast past the peaks.
    pub over: u64,
    pub over_bytes: u64,
    /// Under it by more than that, and the bytes the peaks passed the forecasts by.
    pub under: u64,
    pub under_bytes: u64,
}

/// Each basis's [`Tally`]: the model checked against what was measured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Check {
    tallies: [Tally; 5],
}

impl Check {
    /// A task forecast `forecast` ended, holding `peak` at most.
    pub fn observe(&mut self, forecast: Forecast, peak: u64) {
        let tally = &mut self.tallies[forecast.basis.at()];
        tally.tasks += 1;
        if within(forecast.bytes, peak) {
            tally.within += 1;
        } else if forecast.bytes > peak {
            tally.over += 1;
            tally.over_bytes += forecast.bytes - peak;
        } else {
            tally.under += 1;
            tally.under_bytes += peak - forecast.bytes;
        }
    }

    pub fn tally(&self, basis: Basis) -> Tally {
        self.tallies[basis.at()]
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_types, reason = "a test's own bookkeeping, off every hot path")]
mod tests;
