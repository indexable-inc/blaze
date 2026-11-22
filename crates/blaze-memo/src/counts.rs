//! Counters per thread, never shared: a worker's are its own cache line, taken when its task
//! ends ([`take_counts`]) and merged by whoever reports them. And the process's count of query
//! work ended ([`completed`]), with its hook ([`on_each_run`]): what a stall watchdog on
//! another thread reads.

use std::{
    cell::Cell,
    sync::{
        OnceLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

/// How many stripes [`completed`]'s count has: threads past it share.
const STRIPES: usize = 64;

/// One stripe of [`completed`]'s count, on a cache line of its own.
#[repr(align(128))]
struct Stripe(AtomicU64);

static COMPLETED: [Stripe; STRIPES] = [const { Stripe(AtomicU64::new(0)) }; STRIPES];
static NEXT_STRIPE: AtomicUsize = AtomicUsize::new(0);

/// What is told of each query run, once its function returned ([`on_each_run`]).
#[expect(
    clippy::disallowed_types,
    reason = "the process's one run hook, set once by the daemon that owns the stall watchdog"
)]
static ON_RUN: OnceLock<fn()> = OnceLock::new();

thread_local! {
    static STRIPE: Cell<usize> = const { Cell::new(usize::MAX) };
}

/// The query work this process's memos ended, on every thread: each run of a query's function
/// that returned (each round of a fixpoint) and each entry served from an image. It grows
/// while query work moves and only then (a hit is no work; a worker parked on a wait ends
/// none), so a watchdog fails a build where tasks run and it stays still.
pub fn completed() -> u64 {
    COMPLETED.iter().map(|it| it.0.load(Ordering::Relaxed)).sum()
}

/// Has `hook` called on the running thread after each query's function returns (never on a
/// hit or a served entry): the daemon's stall watchdog counts it as work moving. Once per
/// process.
///
/// # Errors
///
/// A hook was set already: `hook` is handed back.
pub fn on_each_run(hook: fn()) -> Result<(), fn()> {
    ON_RUN.set(hook)
}

/// One more of [`completed`]'s count, on this thread's stripe. Never inlined, as every read of
/// a thread-local in this crate is (`task::innermost_ptr` says why).
#[inline(never)]
pub(crate) fn completed_one() {
    let at = STRIPE.with(|it| {
        if it.get() == usize::MAX {
            it.set(NEXT_STRIPE.fetch_add(1, Ordering::Relaxed) % STRIPES);
        }
        it.get()
    });
    COMPLETED[at].0.fetch_add(1, Ordering::Relaxed);
}

/// A query's function returned: [`completed_one`] and the [`on_each_run`] hook.
pub(crate) fn ran_one() {
    completed_one();
    if let Some(hook) = ON_RUN.get() {
        hook();
    }
}

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
    /// Image records found under their key whose read list was checked: one per (item,
    /// snapshot) at most, the read list's one check (`crate::image`).
    pub image_checks: u64,
    /// Of those, the ones whose read list held: entries served from the image, not run.
    pub image_served: u64,
    /// Of those, the ones whose read list had a moved digest: the query ran.
    pub image_redone: u64,
    /// Records of imaged runs queued for the driver to file.
    pub image_filed: u64,
    /// Imaged runs recomputed after a read digest moved whose serialized output bytes matched the
    /// previous record (early-cutoff backdating: redundant re-filing skipped).
    pub image_backdated: u64,
    /// Runs of imaged queries not filed because their list held an untracked read.
    pub image_untracked: u64,
    /// Values of an `owns` query's fixpoint kept as a round's provisional value: the initial
    /// value, each head round's value and each member round's entry, each placed once in its
    /// arena and dropped once with it. At most `MAX_ROUNDS` per head and member of a cycle.
    pub owned_rounds: u64,
    /// Rows an ask appended to its asker's read list ([`crate::Reads::Kept`]): one per entry
    /// asked, hit or run, whatever that entry read; a recorder's or a fixpoint member's round's
    /// own direct rows passed to its outer run. Never a callee's whole flat list.
    pub rows_replayed: u64,
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
        self.image_checks += other.image_checks;
        self.image_served += other.image_served;
        self.image_redone += other.image_redone;
        self.image_filed += other.image_filed;
        self.image_backdated += other.image_backdated;
        self.image_untracked += other.image_untracked;
        self.owned_rounds += other.owned_rounds;
        self.rows_replayed += other.rows_replayed;
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
            image_checks: 0,
            image_served: 0,
            image_redone: 0,
            image_filed: 0,
            image_backdated: 0,
            image_untracked: 0,
            owned_rounds: 0,
            rows_replayed: 0,
        })
    };
}

/// This thread's counts, reset. Never inlined ([`completed_one`]).
#[inline(never)]
pub fn take_counts() -> Counts {
    COUNTS.with(|it| it.replace(Counts::default()))
}

/// Never inlined ([`completed_one`]): a count before a run's function and one after it must
/// each reach the worker they run on.
#[inline(never)]
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

pub(crate) fn image_check() {
    bump(|it| it.image_checks += 1);
}

pub(crate) fn image_served() {
    bump(|it| it.image_served += 1);
}

pub(crate) fn image_redo() {
    bump(|it| it.image_redone += 1);
}

pub(crate) fn image_filed() {
    bump(|it| it.image_filed += 1);
}

pub(crate) fn image_backdated() {
    bump(|it| it.image_backdated += 1);
}

pub(crate) fn image_untracked() {
    bump(|it| it.image_untracked += 1);
}

pub(crate) fn owned_round() {
    bump(|it| it.owned_rounds += 1);
}

pub(crate) fn rows_replayed(rows: usize) {
    let rows = u64::try_from(rows).expect("a count of rows fits in 64 bits");
    bump(|it| it.rows_replayed += rows);
}
