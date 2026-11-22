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

/// The question kinds the tally counts apart, in [`Question`]'s declaration order.
pub const QUESTION_KINDS: [&str; 25] = [
    "FnSig",
    "Predicates",
    "Adt",
    "ImplsOf",
    "ImplHeader",
    "Inherent",
    "AssocItem",
    "TraitsInScope",
    "Path",
    "Lang",
    "LangPath",
    "Const",
    "Opaque",
    "Attrs",
    "TraitDef",
    "AssocParent",
    "ConstHolds",
    "ConstParams",
    "Closure",
    "AssocGenerics",
    "Coroutine",
    "TypeAlias",
    "FieldDefaults",
    "MemberVisible",
    "MemberUnstable",
];

/// The index of `q`'s kind in [`QUESTION_KINDS`].
pub const fn question_kind(q: Question) -> usize {
    match q {
        Question::FnSig(_) => 0,
        Question::Predicates(_) => 1,
        Question::Adt(_) => 2,
        Question::ImplsOf(..) => 3,
        Question::ImplHeader(_) => 4,
        Question::Inherent(..) => 5,
        Question::AssocItem(..) => 6,
        Question::TraitsInScope(_) => 7,
        Question::Path(..) => 8,
        Question::Lang(_) => 9,
        Question::LangPath(_) => 10,
        Question::Const(_) => 11,
        Question::Opaque(_) => 12,
        Question::Attrs(_) => 13,
        Question::TraitDef(_) => 14,
        Question::AssocParent(_) => 15,
        Question::ConstHolds(..) => 16,
        Question::ConstParams(_) => 17,
        Question::Closure(..) => 18,
        Question::AssocGenerics(..) => 19,
        Question::Coroutine(..) => 20,
        Question::TypeAlias(_) => 21,
        Question::FieldDefaults(_) => 22,
        Question::MemberVisible(..) => 23,
        Question::MemberUnstable(..) => 24,
    }
}

/// The ask log's summary form: the same seam (`Reads::record`) counts what a run asked and
/// what the answers cost instead of listing every ask, so it can stay on for a whole crate.
/// Summed over every check it is passed to (a caller diffs two copies for one body). Times are
/// read from the caller's clock (rule 75); none of this is an input to any answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    /// Checks run (a body's rounds: each `Outcome::Need` is followed by another check).
    pub checks: u64,
    /// Questions asked of the world, by [`QUESTION_KINDS`].
    pub asks: [u64; QUESTION_KINDS.len()],
    /// Questions the world had no answer for.
    pub missing: u64,
    /// Record tables converted into the solver's facts (one per answer with a table).
    pub conversions: u64,
    /// Table entries converted into solver terms.
    pub entries_converted: u64,
    /// Table entries the answers' own roots reach (each counted once per conversion).
    pub entries_reached: u64,
    /// Table entries offered with the answers (the tables' lengths, summed per conversion).
    pub entries_offered: u64,
    /// Time inside the world's answers.
    pub world_ns: u64,
    /// Time hashing answers into the read list.
    pub hash_ns: u64,
    /// Time converting answers' tables into solver facts.
    pub convert_ns: u64,
    /// Time inside the checks, all of the above included.
    pub check_ns: u64,
    /// Fills of a round's needs that could not be made and were dropped as speculative (the
    /// caller's count: front2 check.rs `make_round`).
    pub dropped: u64,
    /// The solver's work, by its counters (blaze-solve `Stats`, added by the caller from the
    /// context after each check) and the time inside its calls (`Solve`).
    pub solve: Solve,
    /// The holes the second and later runs of a body met, by question kind (`QUESTION_KINDS`):
    /// what makes a body take three rounds or more (a record that names a record). The
    /// caller's count, from each round's `Needs::missing`.
    pub late_holes: [u64; QUESTION_KINDS.len()],
}

/// What the trait solver did in the checks counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Solve {
    /// Calls into the solver (one goal evaluated, one selection over the pending goals, one
    /// environment built), and the time inside them, fact fills excluded.
    pub calls: u64,
    pub ns: u64,
    /// Solver calls repeated after a fact was filled (`InferCtx::need_rounds`).
    pub refills: u64,
    /// The solver's own counters (blaze-solve `Stats`: goals, frames, cache hits, imports,
    /// canonical forms, ...), summed over the contexts counted.
    pub stats: blaze_solve::eval::Stats,
    /// Calls into the fact cache (`FactCache::fill`), and those that read the world because
    /// the fact was not kept yet.
    pub fills: u64,
    pub fill_misses: u64,
    /// The misses by the solver's question kind (blaze-solve `Question::kind`).
    pub misses_by_kind: [u64; blaze_solve::facts::QUESTION_KIND_NAMES.len()],
    /// The read list's entries recorded (one per answer asked, repeats included) and kept
    /// after its sort and dedup (`Reads::finish`).
    pub entries_recorded: u64,
    pub entries_kept: u64,
}

impl Solve {
    /// Adds `o` into `self`.
    pub fn add(&mut self, o: &Solve) {
        self.calls += o.calls;
        self.ns += o.ns;
        self.refills += o.refills;
        self.stats.add(&o.stats);
        self.fills += o.fills;
        self.fill_misses += o.fill_misses;
        for (a, b) in self.misses_by_kind.iter_mut().zip(o.misses_by_kind.iter()) {
            *a += *b;
        }
        self.entries_recorded += o.entries_recorded;
        self.entries_kept += o.entries_kept;
    }

    /// `self - e`: what happened between two readings.
    pub fn since(&self, e: &Solve) -> Solve {
        Solve {
            calls: self.calls - e.calls,
            ns: self.ns - e.ns,
            refills: self.refills - e.refills,
            stats: self.stats.since(&e.stats),
            fills: self.fills - e.fills,
            fill_misses: self.fill_misses - e.fill_misses,
            misses_by_kind: core::array::from_fn(|i| self.misses_by_kind[i] - e.misses_by_kind[i]),
            entries_recorded: self.entries_recorded - e.entries_recorded,
            entries_kept: self.entries_kept - e.entries_kept,
        }
    }
}

impl Tally {
    /// Adds `o` into `self`.
    pub fn add(&mut self, o: &Tally) {
        self.checks += o.checks;
        for (a, b) in self.asks.iter_mut().zip(o.asks.iter()) {
            *a += *b;
        }
        self.missing += o.missing;
        self.conversions += o.conversions;
        self.entries_converted += o.entries_converted;
        self.entries_reached += o.entries_reached;
        self.entries_offered += o.entries_offered;
        self.world_ns += o.world_ns;
        self.hash_ns += o.hash_ns;
        self.convert_ns += o.convert_ns;
        self.check_ns += o.check_ns;
        self.dropped += o.dropped;
        self.solve.add(&o.solve);
        for (a, b) in self.late_holes.iter_mut().zip(o.late_holes.iter()) {
            *a += *b;
        }
    }

    /// `self - earlier`: what happened between two readings of one tally.
    pub fn since(&self, earlier: &Tally) -> Tally {
        let mut asks = [0u64; QUESTION_KINDS.len()];
        for (i, a) in asks.iter_mut().enumerate() {
            *a = self.asks[i] - earlier.asks[i];
        }
        Tally {
            checks: self.checks - earlier.checks,
            asks,
            missing: self.missing - earlier.missing,
            conversions: self.conversions - earlier.conversions,
            entries_converted: self.entries_converted - earlier.entries_converted,
            entries_reached: self.entries_reached - earlier.entries_reached,
            entries_offered: self.entries_offered - earlier.entries_offered,
            world_ns: self.world_ns - earlier.world_ns,
            hash_ns: self.hash_ns - earlier.hash_ns,
            convert_ns: self.convert_ns - earlier.convert_ns,
            check_ns: self.check_ns - earlier.check_ns,
            dropped: self.dropped - earlier.dropped,
            solve: self.solve.since(&earlier.solve),
            late_holes: core::array::from_fn(|i| self.late_holes[i] - earlier.late_holes[i]),
        }
    }
}

/// A tally being kept, with the clock it reads.
pub(crate) struct Tallying<'w> {
    pub tally: &'w mut Tally,
    pub clock: &'w dyn Clock,
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
/// costs the work up to that ask. A real run goes on past a missing answer (world.rs `Hole`)
/// and fetches every hole in one round, so under `Pull` this is an upper bound on its entries. The questions that run asked (the hints) and the one it
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
