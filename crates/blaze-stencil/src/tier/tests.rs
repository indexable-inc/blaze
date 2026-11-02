use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use blaze_ids::rng::SplitMix64 as Rng;
use cranelift_codegen::{
    cursor::{Cursor as _, FuncCursor},
    ir::{self, InstBuilder as _, condcodes::IntCC, types::I64},
    isa::CallConv,
    settings,
};
use parking_lot::Mutex;

use super::*;

#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

fn policy() -> Policy {
    Policy { warm: 100, hot: 1000, back_edge_weight: 2, highest: Tier::Llvm }
}

fn counts(calls: u64, back_edges: u64) -> Counts {
    Counts { calls, back_edges }
}

fn memory(pairs: usize) -> Vec<AtomicU64> {
    (0..pairs * 2).map(|_| AtomicU64::new(0)).collect()
}

/// What tier-0 or Cranelift code does on a call or a back edge: a plain read-modify-write.
fn bump(memory: &[AtomicU64], cells: u64, addend: i64, by: u64) {
    let word = &memory[((cells + addend as u64) / 8) as usize];
    word.store(word.load(Ordering::Relaxed).wrapping_add(by), Ordering::Relaxed);
}

#[test]
fn defaults_are_the_argued_thresholds() {
    let policy = Policy::default();
    assert_eq!((policy.warm, policy.hot, policy.back_edge_weight), (4096, 65_536, 1));
    assert_eq!(policy.highest, Tier::Llvm);
    assert!(policy.threshold(Tier::Stencil) < policy.threshold(Tier::Cranelift));
    assert!(policy.threshold(Tier::Cranelift) < policy.threshold(Tier::Llvm));
}

#[test]
fn weighted_counts_saturate() {
    assert_eq!(counts(3, 4).weighted(2), 11);
    assert_eq!(counts(u64::MAX, 1).weighted(1), u64::MAX);
    assert_eq!(counts(1, u64::MAX).weighted(3), u64::MAX);
}

#[test]
fn tiers_count_except_the_last() {
    assert!(Tier::Stencil.counts() && Tier::Cranelift.counts() && !Tier::Llvm.counts());
    assert_eq!(Tier::Stencil.backend(), Backend::Stencil);
    assert_eq!(Tier::Cranelift.backend(), Backend::Cranelift { speed: false });
    assert_eq!(Tier::Llvm.backend(), Backend::Llvm { opt: 3 });
}

#[test]
fn nothing_moves_below_warm() {
    let mut tiering = Tiering::new(1, policy());
    let mut out = Vec::new();
    for calls in 0..100 {
        tiering.observe(0, counts(calls, 0), &mut out);
    }
    tiering.observe(0, counts(1, 49), &mut out);
    assert!(out.is_empty());
    assert_eq!(tiering.tier(0), Tier::Stencil);
}

#[test]
fn warm_then_hot_one_recompile_each() {
    let mut tiering = Tiering::new(2, policy());
    let mut out = Vec::new();
    tiering.observe(0, counts(0, 50), &mut out);
    assert_eq!(out, [Promote { function: 0, to: Tier::Cranelift }]);
    out.clear();
    tiering.observe(0, counts(5000, 0), &mut out);
    assert!(out.is_empty(), "a second ask while one is in flight: {out:?}");
    tiering.landed(0, Tier::Cranelift);
    assert_eq!(tiering.tier(0), Tier::Cranelift);
    tiering.observe(0, counts(999, 0), &mut out);
    assert!(out.is_empty());
    tiering.observe(0, counts(1000, 0), &mut out);
    assert_eq!(out, [Promote { function: 0, to: Tier::Llvm }]);
    out.clear();
    tiering.landed(0, Tier::Llvm);
    tiering.observe(0, counts(u64::MAX, u64::MAX), &mut out);
    assert!(out.is_empty());
    assert_eq!(tiering.tiers(), [Tier::Llvm, Tier::Stencil]);
}

#[test]
fn past_hot_at_tier_zero_goes_straight_to_llvm() {
    let mut tiering = Tiering::new(1, policy());
    let mut out = Vec::new();
    tiering.observe(0, counts(1000, 0), &mut out);
    assert_eq!(out, [Promote { function: 0, to: Tier::Llvm }]);
}

#[test]
fn a_failed_tier_is_not_asked_again() {
    let mut tiering = Tiering::new(1, policy());
    let mut out = Vec::new();
    tiering.observe(0, counts(100, 0), &mut out);
    tiering.failed(0, Tier::Cranelift);
    assert!(tiering.refused(0, Tier::Cranelift) && !tiering.in_flight(0));
    out.clear();
    tiering.observe(0, counts(999, 0), &mut out);
    assert!(out.is_empty());
    tiering.observe(0, counts(1000, 0), &mut out);
    assert_eq!(out, [Promote { function: 0, to: Tier::Llvm }]);
    out.clear();
    tiering.failed(0, Tier::Llvm);
    tiering.observe(0, counts(u64::MAX, 0), &mut out);
    assert!(out.is_empty());
    assert_eq!(tiering.tier(0), Tier::Stencil);
}

#[test]
fn the_highest_tier_caps() {
    let mut tiering = Tiering::new(1, Policy { highest: Tier::Cranelift, ..policy() });
    let mut out = Vec::new();
    tiering.observe(0, counts(u64::MAX, 0), &mut out);
    assert_eq!(out, [Promote { function: 0, to: Tier::Cranelift }]);
    out.clear();
    tiering.landed(0, Tier::Cranelift);
    tiering.observe(0, counts(u64::MAX, 0), &mut out);
    assert!(out.is_empty());
    let mut off = Tiering::new(1, Policy { highest: Tier::Stencil, ..policy() });
    off.observe(0, counts(u64::MAX, 0), &mut out);
    assert!(out.is_empty());
    assert_eq!(off.remembered(0, Tier::Llvm), None);
}

#[test]
fn remembered_tiers_start_at_once() {
    let mut tiering = Tiering::new(2, policy());
    assert_eq!(tiering.remembered(0, Tier::Llvm), Some(Promote { function: 0, to: Tier::Llvm }));
    assert_eq!(tiering.remembered(0, Tier::Llvm), None, "already in flight");
    assert_eq!(tiering.remembered(1, Tier::Stencil), None, "not above its tier");
    let mut out = Vec::new();
    tiering.observe(0, counts(u64::MAX, 0), &mut out);
    assert!(out.is_empty());
    tiering.failed(0, Tier::Llvm);
    assert_eq!(tiering.remembered(0, Tier::Llvm), None, "refused in this run");
}

#[test]
fn landing_lower_never_lowers() {
    let mut tiering = Tiering::new(1, policy());
    tiering.landed(0, Tier::Llvm);
    tiering.landed(0, Tier::Cranelift);
    assert_eq!(tiering.tier(0), Tier::Llvm);
}

#[test]
fn racy_decreases_add_nothing_to_the_rate() {
    let mut tiering = Tiering::new(1, Policy { warm: u64::MAX, hot: u64::MAX, ..policy() });
    let mut out = Vec::new();
    tiering.observe(0, counts(40, 0), &mut out);
    let rate = tiering.rate(0);
    tiering.observe(0, counts(39, 0), &mut out);
    assert!(tiering.rate(0) < rate);
    tiering.observe(0, counts(41, 0), &mut out);
    let after = tiering.rate(0);
    assert!(after <= rate, "a lost add counted twice: {rate} then {after}");
}

#[test]
fn hottest_first_is_by_rate_then_index() {
    let mut tiering = Tiering::new(4, Policy { warm: 1, ..policy() });
    let mut out = Vec::new();
    for (function, calls) in [(0, 10), (1, 500), (2, 10), (3, 70)] {
        tiering.observe(function, counts(calls, 0), &mut out);
    }
    tiering.hottest_first(&mut out);
    let order: Vec<u32> = out.iter().map(|it| it.function).collect();
    assert_eq!(order, [1, 3, 0, 2]);
}

/// The state machine over random operations on many functions, against what the module
/// promises: tiers only rise, a promotion is past its threshold and above the function's
/// tier, at most one is in flight per function, a refused tier is never asked again, and the
/// same inputs give the same outputs.
#[test]
fn random_snapshots_keep_every_promise() {
    for seed in 0..64 {
        let first = run_random(seed);
        assert_eq!(first, run_random(seed), "seed {seed} is not deterministic");
    }
}

fn run_random(seed: u64) -> Vec<(u32, Promote)> {
    const FUNCTIONS: u32 = 16;
    let mut rng = Rng::new(seed);
    let policy = Policy {
        warm: 1 + rng.below(200),
        hot: 1 + rng.below(4000),
        back_edge_weight: rng.below(4),
        highest: Tier::ALL[rng.below(3) as usize],
    };
    let mut tiering = Tiering::new(FUNCTIONS, policy);
    let mut cells = vec![counts(0, 0); FUNCTIONS as usize];
    let mut flying: Vec<Promote> = Vec::new();
    let mut log = Vec::new();
    let mut out = Vec::new();
    for step in 0..2000u32 {
        let before = tiering.tiers().to_vec();
        out.clear();
        match rng.below(10) {
            0..=5 => {
                let function = rng.below(u64::from(FUNCTIONS)) as u32;
                let cell = &mut cells[function as usize];
                if tiering.tier(function).counts() {
                    cell.calls += rng.below(50);
                    cell.back_edges += rng.below(50);
                }
                let seen = *cell;
                let was_flying = tiering.in_flight(function);
                tiering.observe(function, seen, &mut out);
                for promote in &out {
                    assert!(!was_flying, "seed {seed} step {step}: a second recompile of {function}");
                    assert!(promote.to > before[function as usize]);
                    assert!(promote.to <= policy.highest);
                    assert!(seen.weighted(policy.back_edge_weight) >= policy.threshold(promote.to));
                    assert!(!tiering.refused(function, promote.to));
                }
            }
            6 | 7 if !flying.is_empty() => {
                let promote = flying.swap_remove(rng.below(flying.len() as u64) as usize);
                tiering.landed(promote.function, promote.to);
                cells[promote.function as usize] = counts(0, 0);
                assert_eq!(tiering.tier(promote.function), promote.to);
            }
            8 if !flying.is_empty() => {
                let promote = flying.swap_remove(rng.below(flying.len() as u64) as usize);
                tiering.failed(promote.function, promote.to);
                assert!(tiering.refused(promote.function, promote.to));
            }
            _ => {
                let function = rng.below(u64::from(FUNCTIONS)) as u32;
                let tier = Tier::ALL[rng.below(3) as usize];
                let was_flying = tiering.in_flight(function);
                if let Some(promote) = tiering.remembered(function, tier) {
                    assert!(!was_flying && promote.to > before[function as usize]);
                    out.push(promote);
                }
            }
        }
        for (now, then) in tiering.tiers().iter().zip(&before) {
            assert!(now >= then, "seed {seed} step {step}: a tier went down");
        }
        for promote in &out {
            assert!(tiering.in_flight(promote.function));
            flying.push(*promote);
            log.push((step, *promote));
        }
        let mut flying_functions: Vec<u32> = flying.iter().map(|it| it.function).collect();
        flying_functions.sort_unstable();
        let before_dedup = flying_functions.len();
        flying_functions.dedup();
        assert_eq!(flying_functions.len(), before_dedup, "two recompiles of one function in flight");
    }
    log
}

/// A snapshot of a million functions, below warm and then past it, allocates nothing but the
/// promotions it pushes, and none at all into an `out` with room.
#[test]
fn observing_a_million_functions_allocates_nothing() {
    const FUNCTIONS: u32 = 1 << 20;
    let mut tiering = Tiering::new(FUNCTIONS, policy());
    let mut out = Vec::with_capacity(FUNCTIONS as usize);
    let ((), cold) = blaze_alloc_count::measure(|| {
        for function in 0..FUNCTIONS {
            tiering.observe(function, counts(u64::from(function % 50), 0), &mut out);
        }
    });
    assert_eq!(cold, blaze_alloc_count::Used::default());
    assert!(out.is_empty());
    let ((), warm) = blaze_alloc_count::measure(|| {
        for function in 0..FUNCTIONS {
            tiering.observe(function, counts(100, 0), &mut out);
        }
    });
    assert_eq!(warm, blaze_alloc_count::Used::default());
    assert_eq!(out.len(), FUNCTIONS as usize);
    assert!(out.iter().all(|it| it.to == Tier::Cranelift));
    let ((), sorted) = blaze_alloc_count::measure(|| tiering.hottest_first(&mut out));
    assert_eq!(sorted, blaze_alloc_count::Used::default());
}

/// After its first round a driver reuses its buffers: a round reads, decides and sorts with no
/// allocation.
#[test]
fn a_warm_round_allocates_nothing() {
    const FUNCTIONS: u32 = 1 << 12;
    let memory = memory(FUNCTIONS as usize);
    let mut driver = Driver::new(FUNCTIONS, policy());
    for function in 0..FUNCTIONS {
        driver.bind(function, Some(u64::from(function * CELLS))).unwrap();
    }
    for function in 0..FUNCTIONS {
        bump(&memory, u64::from(function * CELLS), CALLS, 100);
    }
    assert_eq!(driver.round(memory.as_slice()).unwrap().len(), FUNCTIONS as usize);
    for function in 0..FUNCTIONS {
        bump(&memory, u64::from(function * CELLS), CALLS, 1);
    }
    let (promotes, used) = blaze_alloc_count::measure(|| driver.round(memory.as_slice()).unwrap().len());
    assert_eq!(promotes, 0, "every function is in flight");
    assert_eq!(used, blaze_alloc_count::Used::default());
}

#[test]
fn cells_are_bound_densely() {
    let mut rng = Rng::new(7);
    let mut counters = Counters::new(64);
    let mut model: Vec<Option<u64>> = vec![None; 64];
    for _ in 0..5000 {
        let function = rng.below(64) as u32;
        let cells = (rng.below(3) != 0).then(|| rng.below(1 << 20) * 8);
        counters.bind(function, cells).unwrap();
        model[function as usize] = cells;
        assert_eq!(counters.bound(), model.iter().flatten().count());
    }
    for (function, cells) in model.iter().enumerate() {
        assert_eq!(counters.cells(function as u32), *cells);
    }
    assert!(counters.bind(64, Some(0)).is_err());
    assert!(counters.bind(0, Some(4)).unwrap_err().contains("8-aligned"));
    assert!(counters.bind(0, Some(u64::MAX - 7)).is_err());
}

#[test]
fn a_snapshot_reads_both_cells_of_each_bound_function() {
    let memory = memory(4);
    let mut counters = Counters::new(3);
    counters.bind(0, Some(16)).unwrap();
    counters.bind(2, Some(48)).unwrap();
    bump(&memory, 16, CALLS, 5);
    bump(&memory, 16, BACK_EDGES, 7);
    bump(&memory, 48, CALLS, 1);
    bump(&memory, 0, CALLS, 99);
    let mut out = Vec::new();
    counters.snapshot(memory.as_slice(), &mut out).unwrap();
    out.sort_unstable_by_key(|it| it.0);
    assert_eq!(out, [(0, counts(5, 7)), (2, counts(1, 0))]);
    assert_eq!(counters.read(1, memory.as_slice()), Ok(None));
    assert_eq!(counters.read(0, memory.as_slice()), Ok(Some(counts(5, 7))));
    counters.bind(2, Some(64)).unwrap();
    let error = counters.snapshot(memory.as_slice(), &mut Vec::new()).unwrap_err();
    assert!(error.contains("function 2") && error.contains("0x40"), "{error}");
}

#[test]
fn memory_loads_refuse_what_is_not_a_cell() {
    let memory = memory(1);
    assert!(memory.as_slice().load(4).unwrap_err().contains("aligned"));
    assert!(memory.as_slice().load(16).unwrap_err().contains("past"));
    assert_eq!(memory.as_slice().load(8), Ok(0));
}

/// One writer and a reader at once: every snapshot is a count the writer made, never more,
/// and they only rise; the last is exact.
#[test]
fn one_writer_counts_are_read_exactly() {
    const ADDS: u64 = 200_000;
    let memory: Arc<[AtomicU64]> = (0..2).map(|_| AtomicU64::new(0)).collect();
    let mut counters = Counters::new(1);
    counters.bind(0, Some(0)).unwrap();
    let writer = {
        let memory = Arc::clone(&memory);
        std::thread::spawn(move || {
            for _ in 0..ADDS {
                bump(&memory, 0, CALLS, 1);
                bump(&memory, 0, BACK_EDGES, 1);
            }
        })
    };
    let mut last = counts(0, 0);
    while !writer.is_finished() {
        let now = counters.read(0, &memory).unwrap().unwrap();
        assert!(now.calls >= last.calls && now.back_edges >= last.back_edges);
        assert!(now.calls <= ADDS && now.back_edges <= ADDS);
        last = now;
    }
    writer.join().unwrap();
    assert_eq!(counters.read(0, &memory).unwrap(), Some(counts(ADDS, ADDS)));
}

/// Racing writers lose adds, as the plain read-modify-write allows, but never make one up.
#[test]
fn racing_writers_lose_counts_but_never_add_any() {
    const WRITERS: u64 = 4;
    const ADDS: u64 = 50_000;
    let memory: Arc<[AtomicU64]> = (0..2).map(|_| AtomicU64::new(0)).collect();
    let writers: Vec<_> = (0..WRITERS)
        .map(|_| {
            let memory = Arc::clone(&memory);
            std::thread::spawn(move || {
                for _ in 0..ADDS {
                    bump(&memory, 0, CALLS, 1);
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    let calls = memory.load(CALLS as u64).unwrap();
    assert!(calls > 0 && calls <= WRITERS * ADDS, "{calls}");
}

#[test]
fn the_driver_refuses_cells_that_do_not_match_the_tier() {
    let mut driver = Driver::new(1, policy());
    let error = driver.landed(Promote { function: 0, to: Tier::Cranelift }, None).unwrap_err();
    assert!(error.contains("no counter cells"), "{error}");
    let error = driver.landed(Promote { function: 0, to: Tier::Llvm }, Some(0)).unwrap_err();
    assert!(error.contains("counts nothing"), "{error}");
    assert_eq!(driver.tiering().tier(0), Tier::Stencil);
}

/// A daemon in simulation: tier-0 code placed with cells, a program that runs, rounds of the
/// driver, and a recompiler that places each new version with fresh cells and publishes it
/// in a slot table, as blaze-lazy's placer and blaze-link's slots do.
struct Daemon {
    memory: Vec<AtomicU64>,
    /// The next free cell pair.
    next: u64,
    /// By function: the tier of the code its slot runs, and that code's cells.
    slots: Vec<(Tier, Option<u64>)>,
    recompiles: Vec<Promote>,
    /// Functions whose Cranelift compile fails.
    broken: Vec<u32>,
}

impl Daemon {
    fn new(functions: u32, pairs: usize) -> (Daemon, Driver) {
        let mut daemon = Daemon {
            memory: memory(pairs),
            next: 0,
            slots: Vec::new(),
            recompiles: Vec::new(),
            broken: Vec::new(),
        };
        let mut driver = Driver::new(functions, policy());
        for function in 0..functions {
            let cells = daemon.cells();
            daemon.slots.push((Tier::Stencil, Some(cells)));
            driver.bind(function, Some(cells)).unwrap();
        }
        (daemon, driver)
    }

    fn cells(&mut self) -> u64 {
        let cells = self.next * u64::from(CELLS);
        self.next += 1;
        cells
    }

    /// `function` runs once with `loops` back edges, in whatever code its slot has.
    fn call(&self, function: u32, loops: u64) {
        if let (_, Some(cells)) = self.slots[function as usize] {
            bump(&self.memory, cells, CALLS, 1);
            bump(&self.memory, cells, BACK_EDGES, loops);
        }
    }

    fn recompile(&mut self, promote: Promote) -> Result<Option<u64>, String> {
        self.recompiles.push(promote);
        if promote.to == Tier::Cranelift && self.broken.contains(&promote.function) {
            return Err(format!("function {} has no Cranelift code", promote.function));
        }
        let cells = promote.to.counts().then(|| self.cells());
        self.slots[promote.function as usize] = (promote.to, cells);
        Ok(cells)
    }

    fn round(&mut self, driver: &mut Driver) {
        let promotes = driver.round(self.memory.as_slice()).unwrap().to_vec();
        for promote in promotes {
            match self.recompile(promote) {
                Ok(cells) => driver.landed(promote, cells).unwrap(),
                Err(_) => driver.failed(promote),
            }
        }
    }
}

#[test]
fn the_round_trip_in_simulation() {
    const FUNCTIONS: u32 = 8;
    let (mut daemon, mut driver) = Daemon::new(FUNCTIONS, 64);
    daemon.broken.push(5);
    let mut rng = Rng::new(42);
    for _ in 0..400 {
        for _ in 0..200 {
            // Function 0 loops, 1 and 5 are called often, 2 seldom (about 600 calls in all:
            // past warm, and far from hot in its Cranelift code), the rest never.
            match rng.below(16) {
                0..=5 => daemon.call(0, 8),
                6..=9 => daemon.call(1, 0),
                10..=13 => daemon.call(5, 0),
                14 if rng.below(8) == 0 => daemon.call(2, 0),
                _ => {}
            }
        }
        daemon.round(&mut driver);
        for function in 0..FUNCTIONS {
            assert_eq!(daemon.slots[function as usize].0, driver.tiering().tier(function));
            assert!(!driver.tiering().in_flight(function));
        }
    }
    let tiers = driver.tiering().tiers();
    assert_eq!(tiers[0], Tier::Llvm);
    assert_eq!(tiers[1], Tier::Llvm);
    assert_eq!(tiers[2], Tier::Cranelift);
    assert!(tiers[3..5].iter().chain(&tiers[6..]).all(|it| *it == Tier::Stencil));
    // Cranelift failed for 5, which went on to LLVM from tier 0's counts.
    assert_eq!(tiers[5], Tier::Llvm);
    assert!(driver.tiering().refused(5, Tier::Cranelift));
    let mut asked = daemon.recompiles.clone();
    let all = asked.len();
    asked.sort_unstable_by_key(|it| (it.function, it.to));
    asked.dedup();
    assert_eq!(asked.len(), all, "a promotion recompiled twice: {:?}", daemon.recompiles);
    assert!(asked.iter().all(|it| it.to != Tier::Stencil));
    // LLVM code counts nothing, so its functions are read no more.
    assert_eq!(driver.counters().bound(), tiers.iter().filter(|it| it.counts()).count());
}

/// A thread-safe [`Daemon`] for the [`Runner`]: cells in shared words, the slot table behind
/// a lock.
struct Threaded {
    memory: Arc<[AtomicU64]>,
    next: AtomicU64,
    slots: Mutex<Vec<(Tier, Option<u64>)>>,
    recompiles: Mutex<Vec<Promote>>,
}

impl Recompile for Arc<Threaded> {
    fn recompile(&self, function: u32, to: Tier) -> Result<Option<u64>, String> {
        self.recompiles.lock().push(Promote { function, to });
        if function == 3 {
            panic!("function 3's backend fell over");
        }
        let cells = to.counts().then(|| self.next.fetch_add(1, Ordering::Relaxed) * u64::from(CELLS));
        self.slots.lock()[function as usize] = (to, cells);
        Ok(cells)
    }
}

#[test]
fn the_runner_tiers_a_running_program() {
    const FUNCTIONS: u32 = 4;
    let daemon = Arc::new(Threaded {
        memory: (0..128).map(|_| AtomicU64::new(0)).collect(),
        next: AtomicU64::new(u64::from(FUNCTIONS)),
        slots: Mutex::new((0..FUNCTIONS).map(|it| (Tier::Stencil, Some(u64::from(it * CELLS)))).collect()),
        recompiles: Mutex::new(Vec::new()),
    });
    let config = Config { period: Duration::from_millis(1), workers: 2 };
    let runner = Runner::start(FUNCTIONS, policy(), config, Arc::clone(&daemon.memory), Arc::clone(&daemon)).unwrap();
    for function in 0..FUNCTIONS {
        runner.bind(function, Some(u64::from(function * CELLS))).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while runner.tier(0) != Tier::Llvm && Instant::now() < deadline {
        for function in [0, 3] {
            let slot = daemon.slots.lock()[function as usize];
            if let (_, Some(cells)) = slot {
                bump(&daemon.memory, cells, CALLS, 1);
                bump(&daemon.memory, cells, BACK_EDGES, 3);
            }
        }
        std::thread::yield_now();
    }
    let closed = runner.close();
    assert_eq!(closed.stopped, None);
    assert_eq!(closed.tiers[0], Tier::Llvm);
    assert_eq!(closed.tiers[1..3], [Tier::Stencil, Tier::Stencil]);
    assert_eq!(closed.tiers[3], Tier::Stencil);
    assert!(
        closed.failures.iter().all(|it| it.function == 3 && it.why.contains("fell over")),
        "{:?}",
        closed.failures
    );
    assert!(!closed.failures.is_empty());
    // Function 0 to Cranelift then LLVM, or straight to LLVM if one period ran it past hot.
    assert!((1..=2).contains(&closed.landed), "{closed:?}");
    let recompiles = daemon.recompiles.lock().clone();
    let zero: Vec<Tier> = recompiles.iter().filter(|it| it.function == 0).map(|it| it.to).collect();
    assert!(zero.is_sorted() && zero.last() == Some(&Tier::Llvm), "{zero:?}");
    let mut unique = recompiles.clone();
    unique.sort_unstable_by_key(|it| (it.function, it.to));
    unique.dedup();
    assert_eq!(unique.len(), recompiles.len(), "{recompiles:?}");
    assert_eq!(closed.promotions as usize, recompiles.len());
}

#[test]
fn cells_that_cannot_be_read_stop_the_tiering_and_say_so() {
    let memory: Arc<[AtomicU64]> = (0..2).map(|_| AtomicU64::new(0)).collect();
    let daemon = Arc::new(Threaded {
        memory: Arc::clone(&memory),
        next: AtomicU64::new(1),
        slots: Mutex::new(vec![(Tier::Stencil, None)]),
        recompiles: Mutex::new(Vec::new()),
    });
    let config = Config { period: Duration::from_millis(1), workers: 1 };
    let runner = Runner::start(1, policy(), config, memory, daemon).unwrap();
    runner.bind(0, Some(4096)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while runner.stopped().is_none() && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(runner.tier(0), Tier::Stencil);
    let closed = runner.close();
    let stopped = closed.stopped.expect("the read error is reported");
    assert!(stopped.contains("function 0") && stopped.contains("0x1000"), "{stopped}");
}

/// Cells whose every load panics.
struct Panicking;

impl Cells for Panicking {
    fn load(&self, _: u64) -> Result<u64, String> {
        panic!("the cells' memory went away")
    }
}

#[test]
fn a_panic_reading_the_cells_stops_the_tiering_and_says_so() {
    let daemon = Arc::new(Threaded {
        memory: (0..2).map(|_| AtomicU64::new(0)).collect(),
        next: AtomicU64::new(1),
        slots: Mutex::new(vec![(Tier::Stencil, None)]),
        recompiles: Mutex::new(Vec::new()),
    });
    let config = Config { period: Duration::from_millis(1), workers: 1 };
    let runner = Runner::start(1, policy(), config, Panicking, daemon).unwrap();
    runner.bind(0, Some(0)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while runner.stopped().is_none() && Instant::now() < deadline {
        std::thread::yield_now();
    }
    let closed = runner.close();
    let stopped = closed.stopped.expect("the panic is reported");
    assert!(stopped.contains("panicked") && stopped.contains("went away"), "{stopped}");
}

#[test]
fn a_zero_period_is_refused() {
    let memory: Arc<[AtomicU64]> = (0..2).map(|_| AtomicU64::new(0)).collect();
    let daemon = Arc::new(Threaded {
        memory: Arc::clone(&memory),
        next: AtomicU64::new(0),
        slots: Mutex::new(Vec::new()),
        recompiles: Mutex::new(Vec::new()),
    });
    let config = Config { period: Duration::ZERO, workers: 1 };
    assert!(Runner::start(1, policy(), config, memory, daemon).is_err());
}

/// `(i64) -> i64` with a counted loop: entry, a header that tests `i < n`, the body `end`
/// gives a terminator to, and an exit.
fn looped(end: impl FnOnce(&mut FuncCursor<'_>, [ir::Block; 4], [ir::Value; 4])) -> ir::Function {
    let mut sig = ir::Signature::new(CallConv::SystemV);
    sig.params.push(ir::AbiParam::new(I64));
    sig.returns.push(ir::AbiParam::new(I64));
    let mut func = ir::Function::with_name_signature(ir::UserFuncName::testcase("looped"), sig);
    let blocks = [(); 4].map(|()| func.dfg.make_block());
    let [entry, header, body, exit] = blocks;
    for block in blocks {
        func.layout.append_block(block);
    }
    let n = func.dfg.append_block_param(entry, I64);
    let i = func.dfg.append_block_param(header, I64);
    let acc = func.dfg.append_block_param(header, I64);
    let result = func.dfg.append_block_param(exit, I64);
    let mut cursor = FuncCursor::new(&mut func);
    cursor.goto_bottom(entry);
    let zero = cursor.ins().iconst(I64, 0);
    cursor.ins().jump(header, &[zero.into(), zero.into()]);
    cursor.goto_bottom(header);
    let more = cursor.ins().icmp(IntCC::SignedLessThan, i, n);
    cursor.ins().brif(more, body, &[], exit, &[acc.into()]);
    cursor.goto_bottom(body);
    end(&mut cursor, blocks, [n, i, acc, result]);
    cursor.goto_bottom(exit);
    cursor.ins().return_(&[result]);
    func
}

fn verify(func: &ir::Function) {
    let flags = settings::Flags::new(settings::builder());
    if let Err(errors) = cranelift_codegen::verify_function(func, &flags) {
        panic!("{}\n{func}", cranelift_codegen::print_errors::pretty_verifier_error(func, None, errors));
    }
}

/// The offsets of the counter cells each store of `block` adds to.
fn counted(func: &ir::Function, block: ir::Block) -> Vec<i32> {
    let mut out = Vec::new();
    for inst in func.layout.block_insts(block) {
        if let ir::InstructionData::Store { args: [_, at], offset, .. } = func.dfg.insts[inst] {
            let def = func.dfg.value_def(at).inst().unwrap();
            let ir::InstructionData::UnaryGlobalValue { global_value, .. } = func.dfg.insts[def] else {
                panic!("a store to something not the cells");
            };
            let ir::GlobalValueData::Symbol { name: ir::ExternalName::User(name), colocated: true, .. } =
                func.global_values[global_value]
            else {
                panic!("cells that are not a colocated user name");
            };
            let name = &func.params.user_named_funcs()[name];
            assert_eq!((name.namespace, name.index), (COUNTER_NAMESPACE, 0));
            out.push(i32::from(offset));
        }
    }
    out
}

fn all_counted(func: &ir::Function) -> Vec<i32> {
    func.layout.blocks().flat_map(|block| counted(func, block)).collect()
}

#[test]
fn a_jump_back_counts_before_it() {
    let mut func = looped(|cursor, [_, header, ..], [_, i, acc, _]| {
        let next = cursor.ins().iadd_imm(i, 1);
        let sum = cursor.ins().iadd(acc, i);
        cursor.ins().jump(header, &[next.into(), sum.into()]);
    });
    let body = func.layout.blocks().nth(2).unwrap();
    instrument(&mut func).unwrap();
    verify(&func);
    let entry = func.layout.entry_block().unwrap();
    assert_eq!(counted(&func, entry), [CALLS as i32]);
    assert_eq!(counted(&func, body), [BACK_EDGES as i32]);
    assert_eq!(all_counted(&func).len(), 2);
    let first = func.layout.first_inst(entry).unwrap();
    assert_eq!(func.dfg.insts[first].opcode(), ir::Opcode::SymbolValue, "the count comes first");
    assert_eq!(func.layout.blocks().count(), 4, "a jump's edge needs no block of its own");
}

#[test]
fn a_two_way_branch_back_counts_on_its_own_edges() {
    let mut func = looped(|cursor, [_, header, ..], [_, i, acc, _]| {
        let next = cursor.ins().iadd_imm(i, 1);
        let sum = cursor.ins().iadd(acc, i);
        let odd = cursor.ins().band_imm(i, 1);
        cursor.ins().brif(odd, header, &[next.into(), sum.into()], header, &[next.into(), acc.into()]);
    });
    let blocks: Vec<ir::Block> = func.layout.blocks().collect();
    let (header, body) = (blocks[1], blocks[2]);
    let expected: Vec<Vec<ir::BlockArg>> = {
        let last = func.layout.last_inst(body).unwrap();
        let dests = func.dfg.insts[last].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
        dests.iter().map(|it| it.args(&func.dfg.value_lists).collect()).collect()
    };
    instrument(&mut func).unwrap();
    verify(&func);
    assert!(counted(&func, body).is_empty(), "the branch itself counts nothing");
    let last = func.layout.last_inst(body).unwrap();
    let dests: Vec<ir::BlockCall> =
        func.dfg.insts[last].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables).to_vec();
    let after: Vec<ir::Block> = func.layout.blocks().skip(3).take(2).collect();
    for (k, dest) in dests.iter().enumerate() {
        let edge = dest.block(&func.dfg.value_lists);
        assert_eq!(dest.len(&func.dfg.value_lists), 0);
        assert!(after.contains(&edge), "the edge's block is placed after the branch's");
        assert_eq!(counted(&func, edge), [BACK_EDGES as i32]);
        let jump = func.layout.last_inst(edge).unwrap();
        let ir::InstructionData::Jump { destination, .. } = func.dfg.insts[jump] else {
            panic!("an edge block that does not jump on");
        };
        assert_eq!(destination.block(&func.dfg.value_lists), header);
        let args: Vec<ir::BlockArg> = destination.args(&func.dfg.value_lists).collect();
        assert_eq!(args, expected[k]);
    }
    // The header's exit is forward: not counted, not moved.
    let test = func.layout.last_inst(header).unwrap();
    let forward = func.dfg.insts[test].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
    assert_eq!(forward[1].block(&func.dfg.value_lists), blocks[3]);
    assert_eq!(all_counted(&func).len(), 3);
}

#[test]
fn a_jump_table_back_counts_only_its_back_entries() {
    let mut func = looped(|cursor, [_, header, _, exit], [_, i, acc, _]| {
        let next = cursor.ins().iadd_imm(i, 1);
        let pool = &mut cursor.func.dfg;
        let back = pool.block_call(header, &[ir::BlockArg::Value(next), ir::BlockArg::Value(acc)]);
        let out = pool.block_call(exit, &[ir::BlockArg::Value(acc)]);
        let table = cursor.func.create_jump_table(ir::JumpTableData::new(out, &[back, out, back]));
        let index = cursor.ins().band_imm(i, 3);
        let index = cursor.ins().ireduce(ir::types::I32, index);
        cursor.ins().br_table(index, table);
    });
    instrument(&mut func).unwrap();
    verify(&func);
    assert_eq!(all_counted(&func).iter().filter(|it| **it == BACK_EDGES as i32).count(), 2);
    assert_eq!(all_counted(&func).iter().filter(|it| **it == CALLS as i32).count(), 1);
}

#[test]
fn straight_code_counts_its_call_only() {
    let mut sig = ir::Signature::new(CallConv::SystemV);
    sig.returns.push(ir::AbiParam::new(I64));
    let mut func = ir::Function::with_name_signature(ir::UserFuncName::testcase("straight"), sig);
    let entry = func.dfg.make_block();
    func.layout.append_block(entry);
    let mut cursor = FuncCursor::new(&mut func);
    cursor.goto_bottom(entry);
    let one = cursor.ins().iconst(I64, 1);
    cursor.ins().return_(&[one]);
    instrument(&mut func).unwrap();
    verify(&func);
    assert_eq!(all_counted(&func), [CALLS as i32]);
}

#[test]
fn a_function_with_no_blocks_is_an_error() {
    let mut func = ir::Function::new();
    assert!(instrument(&mut func).unwrap_err().what.contains("no blocks"));
}
