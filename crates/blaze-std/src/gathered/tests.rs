//! [`Gathered`]'s proofs: a simulation whose oracle requires one result across worker counts
//! and interleavings, with its planted control.
//!
//! A case draws a list of `(key, value)` items, then places them twice: each placement draws a
//! worker count, a worker for each item and the order the items are pushed (an interleaving of
//! the tasks that found them), pushes each into its worker's cell through `with`, and gathers
//! with `gather_sorted` (and, for a count, `gather`). The oracle: both placements' merged
//! sequences equal the items sorted, and both counts equal the number of items. Control:
//! [`GatherPlant::WorkerOrder`] (the items handed in the order the cells were visited), which
//! the search must find.
//!
//! The other control, a gather while a worker holds a cell, is a type fact: `gather` takes
//! `&mut self` and a task's `with` borrows `&self`. The `compile_fail` example on
//! [`Gathered::gather`] is that control; blaze-std's `doctest = false` keeps it from running
//! (named in the lane's report).

use std::{format, vec::Vec as StdVec};

use core::fmt;

use blaze_own::Alloc;
use blaze_sim::{Broken, Config, Tape, controls};

use super::{GatherPlant, Gathered};
use crate::workers::{Limits, SlotOpts, WorkerId, Workers, WorkersOpts, WorkersSetup};

const MOST_ITEMS: usize = 24;
const MOST_WORKERS: u32 = 4;

/// A cell: the items its worker found, in the order it found them.
#[derive(Clone, Copy, Debug)]
struct Bag {
    items: [(u8, u16); MOST_ITEMS],
    len: usize,
}

/// The invariants the simulation checks.
#[derive(Clone, PartialEq, Eq, Debug)]
enum GatherBroken {
    /// A placement's merged sequence is not the items in `(key, value)` order.
    Order { workers: u32 },
    /// A placement's count is not the number of items.
    Count { workers: u32, got: u64, want: u64 },
}

impl fmt::Display for GatherBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GatherBroken::Order { workers } => write!(f, "on {workers} workers the merge saw the items in another order"),
            GatherBroken::Count { workers, got, want } => write!(f, "on {workers} workers the count gathered {got}, not {want}"),
        }
    }
}

blaze_sim::plants! {
    /// The bugs planted in the gather, each with the invariant that must catch it.
    impl GatherPlant breaks GatherBroken {
        /// The gather as it is.
        None,
        /// The sorted gather skips its sort: worker order.
        WorkerOrder => GatherBroken::Order { .. },
    }
}

/// One placement of `items` on a tape-drawn number of workers, gathered.
fn place(tape: &Tape, items: &[(u8, u16)], plant: GatherPlant) -> Result<(u32, StdVec<(u8, u16)>, u64), Broken<GatherBroken>> {
    let workers_count = 1 + u32::try_from(tape.below(u64::from(MOST_WORKERS), "workers")).unwrap_or(0);
    let setup = WorkersSetup { limits: Limits { most_workers: workers_count, callers: 1, most_slots: 4 }, seed: None, models: 0 };
    let refused = |error: crate::workers::WorkersError| Broken::Harness(format!("the stand-in's workers: {error}"));
    let mut opts = WorkersOpts::new(setup).map_err(refused)?;
    let alloc = blaze_own::testing::heap().shared();
    let mut bags = Gathered::<Bag>::planted(
        &mut opts,
        SlotOpts { name: "test.gathered.bags", alloc, make: |_: WorkerId, _alloc: Alloc<'_>| Bag { items: [(0, 0); MOST_ITEMS], len: 0 } },
        plant,
    );
    let mut counts = Gathered::<u64>::register(&mut opts, SlotOpts { name: "test.gathered.count", alloc, make: |_: WorkerId, _alloc: Alloc<'_>| 0u64 });
    let workers: Workers = Workers::new(opts).map_err(refused)?;
    // The interleaving: the order the items are found, drawn.
    // alloc: the test's order.
    let mut left: StdVec<usize> = (0..items.len()).collect();
    while !left.is_empty() {
        let item = left.remove(tape.index(left.len(), "next item found"));
        let on = workers.pool_worker(u32::try_from(tape.below(u64::from(workers_count), "its worker")).unwrap_or(0));
        workers.enter(on, |w| {
            bags.with(w, |bag| {
                bag.items[bag.len] = items[item];
                bag.len += 1;
            });
            counts.with(w, |count| *count += 1);
        });
    }
    // alloc: the merge's result.
    let mut merged = StdVec::new();
    bags.gather_sorted(
        &workers,
        alloc,
        |bag: &mut Bag, out: &mut dyn FnMut(u8, u16)| {
            for &(key, value) in &bag.items[..bag.len] {
                out(key, value);
            }
            bag.len = 0;
        },
        |key, value| merged.push((key, value)),
    )
    .map_err(|error| Broken::Harness(format!("the gather: {error}")))?;
    let mut total = 0u64;
    counts.gather(&workers, |count| total += *count);
    Ok((workers_count, merged, total))
}

fn gathered(tape: &Tape, plant: GatherPlant) -> Result<(), Broken<GatherBroken>> {
    let count = 1 + tape.index(MOST_ITEMS, "items");
    // alloc: the test's items.
    let items: StdVec<(u8, u16)> = (0..count)
        .map(|_| (u8::try_from(tape.below(4, "key")).unwrap_or(0), u16::try_from(tape.below(1_000, "value")).unwrap_or(0)))
        .collect();
    let mut want = items.clone(); // clone: the oracle's reference
    want.sort_unstable();
    for _ in 0..2 {
        let (workers, merged, total) = place(tape, &items, plant)?;
        tape.note(format_args!("on {workers} workers: {merged:?}"));
        if merged != want {
            return Err(Broken::Oracle(GatherBroken::Order { workers }));
        }
        let want_count = u64::try_from(count).unwrap_or(u64::MAX);
        if total != want_count {
            return Err(Broken::Oracle(GatherBroken::Count { workers, got: total, want: want_count }));
        }
    }
    Ok(())
}

#[test]
fn a_gather_is_one_result_across_worker_counts_and_interleavings() {
    controls(Config::cases(500), gathered);
}
