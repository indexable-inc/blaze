//! The ask log and what it decides (typeck.md, "Re-entry, and what decides saved state").
//!
//! A run with the log on records every question the walk asks the world, in order: the
//! question, its record kind (item or body), where in the walk it was asked (the step of the
//! work stack, and the caller's clock in nanoseconds) and the checker's state at that point in
//! bytes. From one complete run's log, [`restarts`] computes what re-entry from the top costs a
//! body under an order of record arrival: the work each restart repeats, the number of entries
//! and the longest chain of serial rounds. That ratio, wasted over total work, decides whether a
//! checker ever saves its state across a missing record: re-entry stays while it is at most
//! [`R_THRESHOLD`].
//!
//! The log is a measurement, not an input: no answer and no record depends on it.

use blaze_own::{Alloc, Vec};

use crate::world::{Question, RecordKind};

/// The checker's clock for the log (rule 75: passed in, never ambient).
pub trait Clock {
    fn now_ns(&self) -> u64;
}

/// One question asked during a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ask {
    pub question: Question,
    pub kind: RecordKind,
    /// The work-stack step the walk was at (0 before the first frame and in the end sequence's
    /// prologue; the end sequence counts on from the walk's last step).
    pub step: u32,
    /// The clock at the ask, relative to the run's start.
    pub at_ns: u64,
    /// The checker's live state at the ask: its frames and columns, in bytes.
    pub state_bytes: u64,
    /// Whether the world had no answer (the run stopped here).
    pub missing: bool,
    /// Whether the ask came from a solver goal that holds none of the body's inference
    /// variables: one that can become its own goal task (solve.md, "Goal tasks").
    pub ground_goal: bool,
}

/// The log of one run.
pub struct AskLog<'a> {
    pub asks: Vec<Ask, Alloc<'a>>,
    /// The run's total: its last step and clock.
    pub end_step: u32,
    pub end_ns: u64,
}

/// Where the walk is, as the machine last told the recorder.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Cursor {
    pub step: u32,
    pub state_bytes: u64,
    pub ground_goal: bool,
}

/// The order in which a cold body's records arrive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    /// Nothing is there before the body runs: each run fetches what it stopped on, plus every
    /// question it asked before (the hints).
    Pull,
    /// Item records settle before the body is queued: only body records can be missing.
    Kind,
    /// Kind order, and a ground goal's miss is its own goal task: the body waits on it once,
    /// with every other ground goal's facts fetched in the same round.
    KindWithGoals,
}

/// The cost of re-entry for one body under one order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Restarts {
    /// Runs of the body (1: it never stopped).
    pub entries: u32,
    /// Steps of the walk spent in runs that stopped.
    pub wasted_steps: u64,
    /// Steps of the run that finished.
    pub total_steps: u64,
    pub wasted_ns: u64,
    pub total_ns: u64,
    /// The longest chain of serial fetch rounds before the body finished (entries - 1 under
    /// pull and kind order; goal tasks fetch their facts in parallel rounds).
    pub serial_rounds: u32,
}

impl Restarts {
    /// Wasted work over total work, by steps.
    pub fn ratio(&self) -> f64 {
        if self.total_steps == 0 {
            return 0.0;
        }
        self.wasted_steps as f64 / self.total_steps as f64
    }
}

/// The ratio at or below which re-entry from the top stays the design.
pub const R_THRESHOLD: f64 = 0.10;

/// Simulates a cold body under `order` from the log of one run that had every answer.
///
/// A run asks the log's questions in order; it stops at the first one not yet fetched and
/// costs the work up to that ask. The questions that run asked (the hints) and the one it
/// missed are fetched before the next run. Each restart repeats the work before its stop.
pub fn restarts(log: &AskLog<'_>, order: Order, alloc: Alloc<'_>) -> Restarts {
    let mut have: Vec<Question, Alloc<'_>> = Vec::new_in(alloc);
    let missable = |a: &Ask| match order {
        Order::Pull => true,
        Order::Kind | Order::KindWithGoals => a.kind == RecordKind::Body,
    };
    let mut r = Restarts { total_steps: u64::from(log.end_step), total_ns: log.end_ns, ..Restarts::default() };
    // Bounded: each run that stops fetches at least the question it stopped on, which no
    // later run misses, so there are at most `asks.len() + 1` runs.
    loop {
        r.entries += 1;
        let stop = log.asks.iter().position(|a| missable(a) && !have.contains(&a.question));
        let Some(i) = stop else { return r };
        let a = log.asks[i];
        r.wasted_steps += u64::from(a.step);
        r.wasted_ns += a.at_ns;
        r.serial_rounds += 1;
        for b in &log.asks[..=i] {
            if !have.contains(&b.question) {
                have.push(b.question);
            }
        }
        if order == Order::KindWithGoals && a.ground_goal {
            // Every ground goal's miss is fetched by its own task in this round: the body
            // waits once for all of them.
            for b in &log.asks[i..] {
                if b.ground_goal && missable(b) && !have.contains(&b.question) {
                    have.push(b.question);
                }
            }
        }
    }
}

/// The histogram of entries per body: `counts[n]` bodies ran `n + 1` times (the last bucket
/// takes the rest).
pub fn entries_histogram<const N: usize>(all: &[Restarts]) -> [u32; N] {
    let mut counts = [0u32; N];
    for r in all {
        let n = (r.entries.saturating_sub(1) as usize).min(N - 1);
        counts[n] += 1;
    }
    counts
}
