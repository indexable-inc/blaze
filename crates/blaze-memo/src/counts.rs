//! Counters per thread, never shared: a worker's are its own cache line, taken when its task
//! ends ([`take_counts`]) and merged by whoever reports them.

use std::cell::Cell;

/// What one thread's fetches did since its counts were last taken.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counts {
    pub hits: u64,
    pub misses: u64,
    pub runs: u64,
    pub waits: u64,
    pub cycles: u64,
    pub spills: u64,
    /// Validity checks: one per arena per [`crate::Memo::next`] (whether it is carried), the
    /// one place the memo decides an entry may be reused. Nothing else checks.
    pub checks: u64,
    /// Checks made on a hit: zero by construction (a hit is a lookup in an arena the snapshot
    /// holds, and holding it is the proof it was settled); the simulation's oracle asserts it.
    pub hit_checks: u64,
}

impl Counts {
    pub fn add(&mut self, other: &Counts) {
        self.hits += other.hits;
        self.misses += other.misses;
        self.runs += other.runs;
        self.waits += other.waits;
        self.cycles += other.cycles;
        self.spills += other.spills;
        self.checks += other.checks;
        self.hit_checks += other.hit_checks;
    }
}

thread_local! {
    static COUNTS: Cell<Counts> = const {
        Cell::new(Counts {
            hits: 0,
            misses: 0,
            runs: 0,
            waits: 0,
            cycles: 0,
            spills: 0,
            checks: 0,
            hit_checks: 0,
        })
    };
}

/// This thread's counts, reset.
pub fn take_counts() -> Counts {
    COUNTS.with(|it| it.replace(Counts::default()))
}

fn bump(field: impl FnOnce(&mut Counts)) {
    COUNTS.with(|it| {
        let mut counts = it.get();
        field(&mut counts);
        it.set(counts);
    });
}

pub(crate) fn hit() {
    bump(|it| it.hits += 1);
}

pub(crate) fn miss() {
    bump(|it| it.misses += 1);
}

pub(crate) fn run() {
    bump(|it| it.runs += 1);
}

pub(crate) fn wait() {
    bump(|it| it.waits += 1);
}

pub(crate) fn cycle() {
    bump(|it| it.cycles += 1);
}

pub(crate) fn spill() {
    bump(|it| it.spills += 1);
}

pub(crate) fn check() {
    bump(|it| it.checks += 1);
}

pub(crate) fn hit_check() {
    bump(|it| it.hit_checks += 1);
}
