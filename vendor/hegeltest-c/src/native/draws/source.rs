//! Where the engine's own string generators draw their decisions from.
//!
//! The regex generator ([`super::regex`]), the internet generators
//! ([`super::internet`]) and the grammar generator (`lark::generate`)
//! make every random decision through [`Draws`], so that the same walk
//! can be driven by a test case, which records each decision as a choice
//! of its own, or by another source: an RNG alone, when a whole string is
//! to be one choice, or nothing at all, when the simplest string is
//! wanted. The trait holds exactly the operations the generators need,
//! each mirroring the [`NativeTestCase`] method of the same name;
//! [`RngDraws`] and [`SimplestDraws`] are the two other sources.

use crate::control::{hegel_internal_assert, hegel_internal_unwrap};
use crate::native::bignum::ToPrimitive;
use crate::native::core::choices::{BooleanChoice, IntegerChoice};
use crate::native::core::state::{
    IntegerGenerationParameters, NarrowRanges, biased_i64_sample, biased_string_sample,
    boolean_forced, codepoints_to_string, is_narrow, uniform_u64_inclusive,
    weighted_boolean_sample, weighted_boolean_sample_precise,
};
use crate::native::core::{EngineError, ManyState, NativeTestCase, Status, StringChoice};
use crate::native::intervalsets::IntervalSet;
use crate::native::rng::EngineRng;
use alloc::string::String;
use alloc::sync::Arc;

/// A source of the decisions the engine's string generators make. Every
/// method mirrors the [`NativeTestCase`] method of the same name, whose
/// contract it has; the test case itself implements it by forwarding.
pub(crate) trait Draws {
    /// A value drawn uniformly from `[min_value, max_value]`, as
    /// [`NativeTestCase::draw_integer_uniform`] draws it.
    fn draw_integer_uniform(&mut self, min_value: u64, max_value: u64) -> Result<u64, EngineError>;

    /// A value drawn from `[min_value, max_value]` with the engine's
    /// shaping towards small magnitudes and boundary values, as
    /// [`NativeTestCase::draw_integer_i64`] draws it.
    fn draw_integer_i64(&mut self, min_value: i64, max_value: i64) -> Result<i64, EngineError>;

    /// A boolean that is `true` with probability `p`, sampled from one
    /// byte, or `forced` when given, as [`NativeTestCase::weighted`]
    /// draws it.
    fn weighted(&mut self, p: f64, forced: Option<bool>) -> Result<bool, EngineError>;

    /// [`Draws::weighted`] against `p`'s one-byte `threshold` worked out
    /// by the caller, as [`NativeTestCase::weighted_against`] draws it.
    fn weighted_against(
        &mut self,
        p: f64,
        threshold: u8,
        forced: Option<bool>,
    ) -> Result<bool, EngineError>;

    /// A boolean that is `true` with probability `p` at full precision,
    /// or `forced` when given, as [`NativeTestCase::weighted_precise`]
    /// draws it.
    fn weighted_precise(&mut self, p: f64, forced: Option<bool>) -> Result<bool, EngineError>;

    /// A string of `min_size` to `max_size` codepoints over `intervals`,
    /// as [`NativeTestCase::draw_string`] draws it.
    fn draw_string(
        &mut self,
        intervals: Arc<IntervalSet>,
        min_size: usize,
        max_size: usize,
    ) -> Result<String, EngineError>;

    /// Open a span labelled `label`, as [`NativeTestCase::start_span`]
    /// does.
    fn start_span(&mut self, label: u64);

    /// Close the innermost open span, marking it discarded when
    /// `discard`, as [`NativeTestCase::stop_span`] does.
    fn stop_span(&mut self, discard: bool);

    /// Conclude the test case with `status`, as
    /// [`NativeTestCase::conclude`] does with no origin: the first
    /// conclusion stands.
    fn conclude(&mut self, status: Status);

    /// Whether every decision is answered with its simplest value, as
    /// [`SimplestDraws`] answers, so that a walk choosing among
    /// alternatives takes the one whose simplest string is least rather
    /// than drawing an index.
    fn simplest(&self) -> bool {
        false
    }

    /// Advance the many state by one element, answering whether another
    /// element should be drawn. Mirrors Hypothesis's `many.more()`.
    fn many_more(&mut self, state: &mut ManyState) -> Result<bool, EngineError> {
        let should_continue = if state.min_size as f64 == state.max_size {
            state.count < state.min_size
        } else {
            let forced = if state.force_stop {
                Some(false)
            } else if state.count < state.min_size {
                Some(true)
            } else if state.count as f64 >= state.max_size {
                Some(false)
            } else {
                None
            };
            self.weighted_against(state.p_continue(), state.continue_threshold(), forced)?
        };

        if should_continue {
            state.count += 1;
        }
        Ok(should_continue)
    }

    /// Reject the last drawn element. Mirrors Hypothesis's
    /// `many.reject()`: too many rejections stop the collection, or
    /// conclude the test case invalid when it is still short of its
    /// minimum.
    fn many_reject(&mut self, state: &mut ManyState) -> Result<(), EngineError> {
        hegel_internal_assert!(state.count > 0);
        state.count -= 1;
        state.rejections += 1;
        if state.rejections > core::cmp::max(3, 2 * state.count) {
            if state.count < state.min_size {
                self.conclude(Status::Invalid);
                return Err(EngineError::InvalidTestCase);
            } else {
                state.force_stop = true;
            }
        }
        Ok(())
    }
}

impl Draws for NativeTestCase {
    fn draw_integer_uniform(&mut self, min_value: u64, max_value: u64) -> Result<u64, EngineError> {
        NativeTestCase::draw_integer_uniform(self, min_value, max_value)
    }

    fn draw_integer_i64(&mut self, min_value: i64, max_value: i64) -> Result<i64, EngineError> {
        NativeTestCase::draw_integer_i64(self, min_value, max_value)
    }

    fn weighted(&mut self, p: f64, forced: Option<bool>) -> Result<bool, EngineError> {
        NativeTestCase::weighted(self, p, forced)
    }

    fn weighted_against(
        &mut self,
        p: f64,
        threshold: u8,
        forced: Option<bool>,
    ) -> Result<bool, EngineError> {
        NativeTestCase::weighted_against(self, p, threshold, forced)
    }

    fn weighted_precise(&mut self, p: f64, forced: Option<bool>) -> Result<bool, EngineError> {
        NativeTestCase::weighted_precise(self, p, forced)
    }

    fn draw_string(
        &mut self,
        intervals: Arc<IntervalSet>,
        min_size: usize,
        max_size: usize,
    ) -> Result<String, EngineError> {
        NativeTestCase::draw_string(self, intervals, min_size, max_size)
    }

    fn start_span(&mut self, label: u64) {
        NativeTestCase::start_span(self, label);
    }

    fn stop_span(&mut self, discard: bool) {
        NativeTestCase::stop_span(self, discard);
    }

    fn conclude(&mut self, status: Status) {
        NativeTestCase::conclude(self, status, None);
    }
}

/// Keep `status` as the conclusion in `slot` unless one is there already:
/// the first conclusion stands, as it does for a test case.
fn record_conclusion(slot: &mut Option<Status>, status: Status) {
    if slot.is_none() {
        *slot = Some(status);
    }
}

/// A source that samples every decision from an RNG alone, as a test case
/// generating fresh does — the same samplers, the same entropy — but
/// records nothing: spans are no-ops, and a conclusion is kept on the
/// source itself (see [`RngDraws::status`]) since there is no test case
/// to conclude. The shaped integer draw samples with the given swarm
/// parameters, as the test case's own does with its family's.
pub(crate) struct RngDraws<'a> {
    rng: &'a mut EngineRng,
    params: IntegerGenerationParameters,
    narrow_ranges: NarrowRanges,
    status: Option<Status>,
}

impl<'a> RngDraws<'a> {
    /// A source sampling from `rng`, shaping its integer draws by `params`.
    pub(crate) fn new(rng: &'a mut EngineRng, params: IntegerGenerationParameters) -> Self {
        RngDraws {
            rng,
            params,
            narrow_ranges: NarrowRanges::default(),
            status: None,
        }
    }

    /// The status the draws concluded with, or `None` while none has.
    pub(crate) fn status(&self) -> Option<Status> {
        self.status
    }
}

impl Draws for RngDraws<'_> {
    fn draw_integer_uniform(&mut self, min_value: u64, max_value: u64) -> Result<u64, EngineError> {
        hegel_internal_assert!(
            min_value <= max_value,
            "Invalid range [{min_value:?}, {max_value:?}]"
        );
        Ok(uniform_u64_inclusive(min_value, max_value, self.rng))
    }

    fn draw_integer_i64(&mut self, min_value: i64, max_value: i64) -> Result<i64, EngineError> {
        hegel_internal_assert!(
            min_value <= max_value,
            "Invalid range [{min_value:?}, {max_value:?}]"
        );
        let (lo, hi) = (i128::from(min_value), i128::from(max_value));
        let narrow = is_narrow(lo, hi)
            .then(|| self.narrow_ranges.get(lo, hi))
            .transpose()?;
        let sample =
            biased_i64_sample(min_value, max_value, narrow.as_ref(), self.rng, self.params);
        Ok(sample?)
    }

    fn weighted(&mut self, p: f64, forced: Option<bool>) -> Result<bool, EngineError> {
        Ok(boolean_forced(p, forced).unwrap_or_else(|| weighted_boolean_sample(p, self.rng)))
    }

    fn weighted_against(
        &mut self,
        p: f64,
        threshold: u8,
        forced: Option<bool>,
    ) -> Result<bool, EngineError> {
        Ok(boolean_forced(p, forced).unwrap_or_else(|| self.rng.next_byte() >= threshold))
    }

    fn weighted_precise(&mut self, p: f64, forced: Option<bool>) -> Result<bool, EngineError> {
        Ok(boolean_forced(p, forced)
            .unwrap_or_else(|| weighted_boolean_sample_precise(p, self.rng)))
    }

    fn draw_string(
        &mut self,
        intervals: Arc<IntervalSet>,
        min_size: usize,
        max_size: usize,
    ) -> Result<String, EngineError> {
        let kind = StringChoice {
            intervals,
            min_size,
            max_size,
            constraint: None,
        };
        let codepoints = biased_string_sample(&kind, self.rng)?;
        Ok(codepoints_to_string(&codepoints))
    }

    fn start_span(&mut self, _label: u64) {}

    fn stop_span(&mut self, _discard: bool) {}

    fn conclude(&mut self, status: Status) {
        record_conclusion(&mut self.status, status);
    }
}

/// A source that answers every decision with the simplest value of the
/// choice a test case would record for it: integers at zero clamped into
/// their range, booleans `false` unless forced, strings at their minimum
/// length over their smallest codepoint, so a collection stops at its
/// minimum. Spans are no-ops and a conclusion is kept on the source (see
/// [`SimplestDraws::status`]). What a generator draws from it is its
/// simplest string: a walk that chooses among alternatives, told by
/// [`Draws::simplest`] that it is drawing from this source, takes the
/// alternative whose own simplest string is least rather than the
/// first.
#[derive(Debug, Default)]
pub(crate) struct SimplestDraws {
    status: Option<Status>,
}

impl SimplestDraws {
    /// The status the draws concluded with, or `None` while none has.
    #[cfg(test)]
    pub(crate) fn status(&self) -> Option<Status> {
        self.status
    }
}

impl Draws for SimplestDraws {
    fn draw_integer_uniform(&mut self, min_value: u64, max_value: u64) -> Result<u64, EngineError> {
        hegel_internal_assert!(
            min_value <= max_value,
            "Invalid range [{min_value:?}, {max_value:?}]"
        );
        let simplest = IntegerChoice::new(min_value, max_value, 0u64).simplest();
        Ok(hegel_internal_unwrap!(
            simplest.to_u64(),
            "SimplestDraws: the simplest value {simplest} is outside its own range"
        ))
    }

    fn draw_integer_i64(&mut self, min_value: i64, max_value: i64) -> Result<i64, EngineError> {
        hegel_internal_assert!(
            min_value <= max_value,
            "Invalid range [{min_value:?}, {max_value:?}]"
        );
        let simplest = IntegerChoice::small(min_value, max_value, 0).simplest();
        Ok(hegel_internal_unwrap!(
            simplest.to_i64(),
            "SimplestDraws: the simplest value {simplest} is outside its own range"
        ))
    }

    fn weighted(&mut self, p: f64, forced: Option<bool>) -> Result<bool, EngineError> {
        Ok(boolean_forced(p, forced).unwrap_or_else(|| BooleanChoice { p }.simplest()))
    }

    fn weighted_against(
        &mut self,
        p: f64,
        _threshold: u8,
        forced: Option<bool>,
    ) -> Result<bool, EngineError> {
        self.weighted(p, forced)
    }

    fn weighted_precise(&mut self, p: f64, forced: Option<bool>) -> Result<bool, EngineError> {
        self.weighted(p, forced)
    }

    fn draw_string(
        &mut self,
        intervals: Arc<IntervalSet>,
        min_size: usize,
        max_size: usize,
    ) -> Result<String, EngineError> {
        let kind = StringChoice {
            intervals,
            min_size,
            max_size,
            constraint: None,
        };
        Ok(codepoints_to_string(&kind.simplest()?))
    }

    fn start_span(&mut self, _label: u64) {}

    fn stop_span(&mut self, _discard: bool) {}

    fn conclude(&mut self, status: Status) {
        record_conclusion(&mut self.status, status);
    }

    fn simplest(&self) -> bool {
        true
    }
}

#[cfg(test)]
#[path = "../../../tests/embedded/native/draws/source_tests.rs"]
mod tests;
