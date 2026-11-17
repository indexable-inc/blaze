//! THE COST OF ONE BODY (rule 16; engine.md "Floors": about 1 ms of CPU a body): where a body
//! task's time and bytes go, by part, measured with the caller's gauge (rule 75: the clock and
//! the byte counter are passed in, never read from the ambient world). A measurement only: no
//! answer, read list or byte of a result depends on it, and with no [`Cost`] nothing is read.
//!
//! Each stretch of time is filed under the INNERMOST part entered (exclusive time): heads made
//! while lowering are `Heads`, not `Lower`; the impl lookups a check round's fills ask are
//! `Impls`, not `Rounds`. Time outside every part (between them) is filed nowhere, so the
//! caller's own clock around the task shows what is not yet named.

use core::cell::Cell;

use super::super::heads::Want;

/// The caller's clock and allocation counter.
pub trait Gauge {
    /// Nanoseconds of a monotonic clock.
    fn now_ns(&self) -> u64;
    /// Bytes allocated so far (a counter that only grows); 0 when none is at hand.
    fn bytes(&self) -> u64;
}

/// Where a body task's time goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Part {
    /// Finding the body and building its world (`Index::from_source`, `ProjectWorld::over`).
    Setup,
    /// Lowering the body (its heads excluded).
    Lower,
    /// Heads made (`Store::ensure`), each entry one want asked.
    Heads,
    /// The type check rounds (the checker and its solver; heads and impl lookups excluded).
    Rounds,
    /// Impl questions filed (`admit_one`), each entry one question.
    Impls,
    /// The move and borrow check and the stalled goals' decision.
    Borrows,
    /// Making the read list: facts, the recorder's list, cells and their digests, the bytes.
    Reads,
}

/// Every part, in [`Part`]'s order.
pub const PARTS: [Part; 7] = [Part::Setup, Part::Lower, Part::Heads, Part::Rounds, Part::Impls, Part::Borrows, Part::Reads];

/// How deep parts nest at most; an entry past it is counted in [`Spent::too_deep`] and its time
/// stays with the part below it.
const MOST_DEPTH: usize = 32;

impl Part {
    pub fn name(self) -> &'static str {
        match self {
            Part::Setup => "setup",
            Part::Lower => "lower",
            Part::Heads => "heads",
            Part::Rounds => "rounds",
            Part::Impls => "impls",
            Part::Borrows => "borrows",
            Part::Reads => "reads",
        }
    }
}

/// What was spent so far, per part (indexed by `Part as usize`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Spent {
    pub ns: [u64; PARTS.len()],
    pub bytes: [u64; PARTS.len()],
    pub entries: [u64; PARTS.len()],
    /// The solver's counters, added after each check round (blaze-typeck `Solve`; its `ns` is
    /// not measured on this path).
    pub solve: blaze_typeck::Solve,
    pub rounds: u64,
    pub too_deep: u64,
    /// Head steps taken; those that stopped at a need (their work done again after it), and
    /// the time in those (inclusive).
    pub steps: u64,
    pub stopped: u64,
    pub stopped_ns: u64,
}

impl Spent {
    /// What was spent after `before`.
    pub fn since(&self, before: &Spent) -> Spent {
        let mut out = Spent::default();
        for i in 0..PARTS.len() {
            out.ns[i] = self.ns[i] - before.ns[i];
            out.bytes[i] = self.bytes[i] - before.bytes[i];
            out.entries[i] = self.entries[i] - before.entries[i];
        }
        out.solve = self.solve.since(&before.solve);
        out.rounds = self.rounds - before.rounds;
        out.too_deep = self.too_deep - before.too_deep;
        out.steps = self.steps - before.steps;
        out.stopped = self.stopped - before.stopped;
        out.stopped_ns = self.stopped_ns - before.stopped_ns;
        out
    }

    /// `other` added in.
    pub fn add(&mut self, other: &Spent) {
        for i in 0..PARTS.len() {
            self.ns[i] += other.ns[i];
            self.bytes[i] += other.bytes[i];
            self.entries[i] += other.entries[i];
        }
        self.solve.add(&other.solve);
        self.rounds += other.rounds;
        self.too_deep += other.too_deep;
        self.steps += other.steps;
        self.stopped += other.stopped;
        self.stopped_ns += other.stopped_ns;
    }
}

/// The meter one worker's body tasks file their parts in (not `Sync`: one a worker).
pub struct Cost<'g> {
    gauge: &'g dyn Gauge,
    stack: Cell<[Part; MOST_DEPTH]>,
    depth: Cell<usize>,
    /// The gauge's reading at the last switch: (ns, bytes).
    at: Cell<(u64, u64)>,
    spent: Cell<Spent>,
    slowest: Cell<[Option<(u64, Want)>; SLOWEST]>,
}

/// How many of the costliest wants a meter keeps (by inclusive time) until taken.
pub const SLOWEST: usize = 4;

/// A part entered: leaves it when dropped.
pub struct Entered<'c, 'g> {
    cost: &'c Cost<'g>,
    counted: bool,
    /// The head asked and when (ns), for [`Cost::take_slowest`].
    want: Option<(Want, u64)>,
}

impl Drop for Entered<'_, '_> {
    fn drop(&mut self) {
        if self.counted {
            self.cost.switch();
            self.cost.depth.set(self.cost.depth.get() - 1);
        }
        if let Some((want, start)) = self.want {
            let took = self.cost.gauge.now_ns().saturating_sub(start);
            let mut slowest = self.cost.slowest.get();
            if let Some(at) = slowest.iter().position(|s| s.is_none_or(|(ns, _)| ns < took)) {
                slowest.copy_within(at..SLOWEST - 1, at + 1);
                slowest[at] = Some((took, want));
                self.cost.slowest.set(slowest);
            }
        }
    }
}

impl<'g> Cost<'g> {
    pub fn new(gauge: &'g dyn Gauge) -> Cost<'g> {
        Cost { gauge, stack: Cell::new([Part::Setup; MOST_DEPTH]), depth: Cell::new(0), at: Cell::new((0, 0)), spent: Cell::new(Spent::default()), slowest: Cell::new([None; SLOWEST]) }
    }

    /// [`Part::Heads`] entered for `want`: its inclusive time is kept among the slowest.
    pub fn enter_want<'c>(cost: Option<&'c Cost<'g>>, want: Want) -> Option<Entered<'c, 'g>> {
        let mut entered = Cost::enter(cost, Part::Heads)?;
        entered.want = Some((want, entered.cost.gauge.now_ns()));
        Some(entered)
    }

    /// When a head's step starts (0 with no meter).
    pub fn step_start(cost: Option<&Cost<'g>>) -> u64 {
        cost.map_or(0, |c| c.gauge.now_ns())
    }

    /// A head's step ended; `stopped`: it stopped at a need, so its work is done again.
    pub fn step_end(cost: Option<&Cost<'g>>, start: u64, stopped: bool) {
        let Some(cost) = cost else { return };
        let mut spent = cost.spent.get();
        spent.steps += 1;
        if stopped {
            spent.stopped += 1;
            spent.stopped_ns += cost.gauge.now_ns().saturating_sub(start);
        }
        cost.spent.set(spent);
    }

    /// The costliest wants since the last take (inclusive ns), costliest first.
    pub fn take_slowest(&self) -> [Option<(u64, Want)>; SLOWEST] {
        self.slowest.replace([None; SLOWEST])
    }

    /// `part` entered on `cost`, when there is one.
    pub fn enter<'c>(cost: Option<&'c Cost<'g>>, part: Part) -> Option<Entered<'c, 'g>> {
        let cost = cost?;
        let depth = cost.depth.get();
        let mut spent = cost.spent.get();
        if depth == MOST_DEPTH {
            spent.too_deep += 1;
            cost.spent.set(spent);
            return Some(Entered { cost, counted: false, want: None });
        }
        cost.switch();
        let mut spent = cost.spent.get();
        spent.entries[part as usize] += 1;
        cost.spent.set(spent);
        let mut stack = cost.stack.get();
        stack[depth] = part;
        cost.stack.set(stack);
        cost.depth.set(depth + 1);
        Some(Entered { cost, counted: true, want: None })
    }

    /// One check round's solver counters added.
    pub fn round(&self, solve: &blaze_typeck::Solve) {
        let mut spent = self.spent.get();
        spent.solve.add(solve);
        spent.rounds += 1;
        self.spent.set(spent);
    }

    /// What was spent so far.
    pub fn spent(&self) -> Spent {
        self.spent.get()
    }

    /// The stretch since the last switch filed under the innermost part.
    fn switch(&self) {
        let now = (self.gauge.now_ns(), self.gauge.bytes());
        let (t, b) = self.at.replace(now);
        let depth = self.depth.get();
        if depth > 0 {
            let top = self.stack.get()[depth - 1] as usize;
            let mut spent = self.spent.get();
            spent.ns[top] += now.0.saturating_sub(t);
            spent.bytes[top] += now.1.saturating_sub(b);
            self.spent.set(spent);
        }
    }
}
