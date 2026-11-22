//! Cost-weighted LPT test execution planner and adaptive JIT tier-up advisor.
//!
//! - [`ExecutionPlan`]: schedules affected tests deterministically — **failed-last tests first**
//!   for fastest developer feedback, then **Longest-Processing-Time-First (LPT)** by
//!   `last_duration_us` (tie-broken by test [`Digest`]) to minimize parallel tail latency.
//! - [`advise_tier_up`] / [`TierAdvice`]: identifies hot [`crate::CellKind::Code`] symbols that
//!   are either reached by many affected tests (`fanout >= min_fanout`) or sit on the critical
//!   path of slow affected tests (`weighted_us >= threshold_us`), returning a deterministic
//!   priority-ordered list of code symbols to JIT tier-up or cross-function LTO optimize on the
//!   fly.

use core::alloc::Allocator;

use blaze_own::Vec;

use crate::decide::Because;
use crate::impact::{ImpactIndex, Selection};
use crate::Digest;

/// Timing and outcome history from a test's previous run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TestHistory {
    pub test: Digest,
    pub last_duration_us: u64,
    pub failed_last: bool,
}

/// One affected test in its scheduled execution order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PlannedTest {
    pub test_idx: u32,
    pub test: Digest,
    pub because: Because,
    pub estimated_us: u64,
    pub failed_last: bool,
}

/// Deterministic execution plan for affected tests: failed-last first, then Longest-Processing-
/// Time-First (LPT) by `estimated_us`, tie-broken by test [`Digest`] (and `test_idx`).
#[derive(Debug)]
pub struct ExecutionPlan<A: Allocator> {
    order: Vec<PlannedTest, A>,
}

impl<A: Allocator + Clone> ExecutionPlan<A> {
    /// Builds a deterministic LPT execution plan for the affected tests in `selection`, enriched
    /// with timing and prior-failure data from `history`.
    ///
    /// Tests missing from `history` default to `estimated_us = 0, failed_last = false`. If
    /// `history` contains multiple entries for the same test, the last entry wins.
    pub fn build<S: Allocator>(
        selection: &Selection<S>,
        history: &[TestHistory],
        alloc: A,
    ) -> ExecutionPlan<A> {
        let rerun = selection.rerun();
        if rerun.is_empty() {
            return ExecutionPlan { order: Vec::new_in(alloc) };
        }

        let hist_index = index_history(history, alloc.clone());
        let mut order = Vec::with_capacity_in(rerun.len(), alloc);

        for item in rerun {
            let (estimated_us, failed_last) = match find_history(&hist_index, history, item.test) {
                Some(h) => (h.last_duration_us, h.failed_last),
                None => (0, false),
            };
            order.push(PlannedTest {
                test_idx: item.test_idx,
                test: item.test,
                because: item.because,
                estimated_us,
                failed_last,
            });
        }

        // Priority: failed-last first, then LPT (longest estimated duration first), then test
        // digest ascending, then `test_idx` ascending for a strict deterministic total order.
        order.sort_unstable_by(|a, b| {
            b.failed_last
                .cmp(&a.failed_last)
                .then_with(|| b.estimated_us.cmp(&a.estimated_us))
                .then_with(|| a.test.cmp(&b.test))
                .then_with(|| a.test_idx.cmp(&b.test_idx))
        });

        ExecutionPlan { order }
    }
}

impl<A: Allocator> ExecutionPlan<A> {
    /// Affected tests in their scheduled execution order.
    pub fn order(&self) -> &[PlannedTest] {
        &self.order
    }

    /// Consumes the plan and returns the scheduled tests.
    pub fn into_order(self) -> Vec<PlannedTest, A> {
        self.order
    }

    /// Number of scheduled tests.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// True when no tests need to run.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Sum of `estimated_us` across all scheduled tests (saturating).
    pub fn total_estimated_us(&self) -> u64 {
        self.order
            .iter()
            .fold(0u64, |acc, item| acc.saturating_add(item.estimated_us))
    }

    /// Predicted parallel makespan (microseconds) when dispatching [`ExecutionPlan::order`]
    /// greedily across `workers` parallel workers (list scheduling).
    pub fn estimated_makespan_us<W: Allocator>(&self, workers: usize, scratch: W) -> u64 {
        if self.order.is_empty() {
            return 0;
        }
        if workers <= 1 {
            return self.total_estimated_us();
        }
        if workers >= self.order.len() {
            return self
                .order
                .iter()
                .map(|item| item.estimated_us)
                .max()
                .unwrap_or(0);
        }

        let mut loads = Vec::from_elem_in(0u64, workers, scratch);
        for item in &self.order {
            let mut min_idx = 0usize;
            let mut min_load = loads[0];
            for (idx, &load) in loads.iter().enumerate().skip(1) {
                if load < min_load {
                    min_load = load;
                    min_idx = idx;
                }
            }
            loads[min_idx] = loads[min_idx].saturating_add(item.estimated_us);
        }
        loads.into_iter().max().unwrap_or(0)
    }
}

/// Thresholds controlling which [`crate::CellKind::Code`] symbols [`advise_tier_up`] recommends
/// for JIT tier-up / cross-function LTO.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TierOpts {
    /// Recommend a code symbol when reached by at least `min_fanout` affected tests (set to
    /// `usize::MAX` to disable the fanout trigger).
    pub min_fanout: usize,
    /// Recommend a code symbol when the combined `last_duration_us` of affected tests reaching it
    /// is at least `threshold_us` (set to `u64::MAX` to disable the duration trigger).
    pub threshold_us: u64,
    /// Optional cap on how many top-priority symbols to return (`None` returns all qualifying
    /// symbols).
    pub limit: Option<usize>,
}

/// A hot [`crate::CellKind::Code`] symbol recommended for JIT tier-up / cross-function LTO.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TierAdvice {
    /// The code symbol's digest (matching [`crate::Read::name`] for [`crate::CellKind::Code`]).
    pub symbol: Digest,
    /// Number of affected tests that reach this code symbol.
    pub fanout: usize,
    /// Saturating sum of `last_duration_us` across affected tests reaching this code symbol.
    pub weighted_us: u64,
    /// Longest single affected test reaching this code symbol (critical-path duration).
    pub max_test_us: u64,
}

/// Identifies hot [`crate::CellKind::Code`] symbols reached by affected tests in `selection` that
/// satisfy `fanout >= opts.min_fanout || weighted_us >= opts.threshold_us` (always requiring
/// `fanout > 0`), returned in deterministic priority order:
/// `weighted_us` descending, then `fanout` descending, then `max_test_us` descending, then
/// `symbol` ascending.
pub fn advise_tier_up<A: Allocator, S: Allocator, OutA: Allocator + Clone>(
    index: &ImpactIndex<A>,
    selection: &Selection<S>,
    history: &[TestHistory],
    opts: TierOpts,
    alloc: OutA,
) -> Vec<TierAdvice, OutA> {
    let rerun = selection.rerun();
    if rerun.is_empty() || opts.limit == Some(0) {
        return Vec::new_in(alloc);
    }

    let hist_index = index_history(history, alloc.clone());
    let mut affected = Vec::from_elem_in(None, index.test_count(), alloc.clone());
    for item in rerun {
        if let Some(slot) = affected.get_mut(item.test_idx as usize) {
            let dur = find_history(&hist_index, history, item.test)
                .map_or(0, |h| h.last_duration_us);
            *slot = Some(dur);
        }
    }

    advise_from_affected(index, &affected, opts, alloc)
}

/// Same as [`advise_tier_up`], reusing the durations already resolved in an [`ExecutionPlan`].
pub fn advise_tier_up_for_plan<A: Allocator, P: Allocator, OutA: Allocator + Clone>(
    index: &ImpactIndex<A>,
    plan: &ExecutionPlan<P>,
    opts: TierOpts,
    alloc: OutA,
) -> Vec<TierAdvice, OutA> {
    if plan.is_empty() || opts.limit == Some(0) {
        return Vec::new_in(alloc);
    }

    let mut affected = Vec::from_elem_in(None, index.test_count(), alloc.clone());
    for item in plan.order() {
        if let Some(slot) = affected.get_mut(item.test_idx as usize) {
            *slot = Some(item.estimated_us);
        }
    }

    advise_from_affected(index, &affected, opts, alloc)
}

fn advise_from_affected<A: Allocator, OutA: Allocator>(
    index: &ImpactIndex<A>,
    affected: &[Option<u64>],
    opts: TierOpts,
    alloc: OutA,
) -> Vec<TierAdvice, OutA> {
    let mut advice = Vec::new_in(alloc);
    let mut csr = index.code_symbols().peekable();
    let extra = index.extra_code_edges();
    let mut e = 0usize;

    loop {
        let next_csr = csr.peek().map(|&(sym, _)| sym);
        let next_extra = extra.get(e).map(|&(sym, _)| sym);

        let symbol = match (next_csr, next_extra) {
            (None, None) => break,
            (Some(c), None) => c,
            (None, Some(x)) => x,
            (Some(c), Some(x)) => c.min(x),
        };

        let mut fanout = 0usize;
        let mut weighted_us = 0u64;
        let mut max_test_us = 0u64;

        if next_csr == Some(symbol)
            && let Some((_, postings)) = csr.next()
        {
            for posting in postings {
                if let Some(Some(dur)) = affected.get(posting.test_idx as usize) {
                    fanout += 1;
                    weighted_us = weighted_us.saturating_add(*dur);
                    if *dur > max_test_us {
                        max_test_us = *dur;
                    }
                }
            }
        }

        while e < extra.len() && extra[e].0 == symbol {
            let test_idx = extra[e].1;
            e += 1;
            if let Some(Some(dur)) = affected.get(test_idx as usize) {
                fanout += 1;
                weighted_us = weighted_us.saturating_add(*dur);
                if *dur > max_test_us {
                    max_test_us = *dur;
                }
            }
        }

        if fanout > 0 && (fanout >= opts.min_fanout || weighted_us >= opts.threshold_us) {
            advice.push(TierAdvice { symbol, fanout, weighted_us, max_test_us });
        }
    }

    advice.sort_unstable_by(|a, b| {
        b.weighted_us
            .cmp(&a.weighted_us)
            .then_with(|| b.fanout.cmp(&a.fanout))
            .then_with(|| b.max_test_us.cmp(&a.max_test_us))
            .then_with(|| a.symbol.cmp(&b.symbol))
    });

    if let Some(limit) = opts.limit {
        advice.truncate(limit);
    }

    advice
}

/// Builds a sorted `(test, history_index)` lookup table in `alloc` (last entry wins on duplicates).
fn index_history<A: Allocator>(history: &[TestHistory], alloc: A) -> Vec<(Digest, usize), A> {
    let mut index = Vec::with_capacity_in(history.len(), alloc);
    for (i, item) in history.iter().enumerate() {
        index.push((item.test, i));
    }
    index.sort_unstable();
    index
}

fn find_history(index: &[(Digest, usize)], history: &[TestHistory], test: Digest) -> Option<TestHistory> {
    let pos = index.partition_point(|&(d, _)| d <= test);
    let &(found, idx) = pos.checked_sub(1).and_then(|p| index.get(p))?;
    if found == test {
        Some(history[idx])
    } else {
        None
    }
}
