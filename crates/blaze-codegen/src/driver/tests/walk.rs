//! The walk as a worklist (docs/v2/plan/design/scheduler.md, "Codegen: a worklist"): its
//! describes run on all cores with no round between a function and what it reaches, and what
//! the walk gives is the program's alone, the same on any pool and any schedule: the functions
//! and statics the level walk it replaced gave, in that walk's order.

use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use blaze_graph::{pool::Pool, spin::SpinWait, topology::Topology};
use blaze_ids::rng::SplitMix64 as Rng;

use super::*;

const FUNCTIONS: u32 = 1500;
const STATICS: u32 = 40;

/// A program of [`FUNCTIONS`] functions and [`STATICS`] statics from `seed`: each function
/// calls a few others (any of them: cycles, shared callees, stretches of one call after
/// another) and some name a static, whose value points to functions. Much of it is reached
/// from the roots by several paths of several lengths, and some of it by none.
fn generated(seed: u64) -> Program {
    let mut rng = Rng::new(seed);
    let mut program = Program::default();
    let any = |rng: &mut Rng| 1 + rng.below(u64::from(FUNCTIONS)) as u32;
    for id in 1..=FUNCTIONS {
        let calls: Vec<u32> = match rng.below(3) {
            0 => vec![id % FUNCTIONS + 1],
            _ => (0..rng.below(5)).map(|_| any(&mut rng)).collect(),
        };
        let function = program.function(id, (id % 251) as u8, &calls);
        if rng.below(8) == 0 {
            function.statics = vec![1 + rng.below(u64::from(STATICS)) as u32];
        }
    }
    for id in 1..=STATICS {
        program.statics.insert(id, (id as u8, None));
        let points_to: Vec<u32> = (0..rng.below(3)).map(|_| any(&mut rng)).collect();
        program.static_calls.insert(id, points_to);
    }
    program
}

const ROOTS: [u32; 4] = [1, 2, 2, 700];
const STATIC_ROOTS: [u32; 1] = [3];

/// The reference model: the walk by levels that the worklist replaced. The roots are the
/// first level (functions, then statics); each level is described in its order, its
/// functions before its statics, and what a describe refers to that no level held yet is
/// appended to the next, in its references' order. The functions and the statics, each in
/// the order described.
fn level_walk(program: &Program) -> (Vec<u32>, Vec<u32>) {
    let (mut seen, mut seen_statics) = (HashSet::new(), HashSet::new());
    let mut functions: Vec<u32> = ROOTS.into_iter().filter(|it| seen.insert(*it)).collect();
    let mut statics: Vec<u32> =
        STATIC_ROOTS.into_iter().filter(|it| seen_statics.insert(*it)).collect();
    let (mut order, mut static_order) = (Vec::new(), Vec::new());
    while !(functions.is_empty() && statics.is_empty()) {
        let (mut next, mut next_statics) = (Vec::new(), Vec::new());
        for &id in &functions {
            order.push(id);
            let function = &program.functions[&id];
            next.extend(function.calls.iter().copied().filter(|it| seen.insert(*it)));
            next_statics
                .extend(function.statics.iter().copied().filter(|it| seen_statics.insert(*it)));
        }
        for &id in &statics {
            static_order.push(id);
            let points_to = program.static_calls.get(&id).into_iter().flatten();
            next.extend(points_to.copied().filter(|it| seen.insert(*it)));
        }
        (functions, statics) = (next, next_statics);
    }
    (order, static_order)
}

/// What a walk gave, by name.
#[derive(PartialEq, Eq, Debug)]
struct Walked {
    /// The functions in the order the walk gives a lazy build.
    order: Vec<String>,
    /// The functions of the program, in its order: by key.
    program: Vec<String>,
    /// The statics of the program, in its order: by item and instance.
    statics: Vec<String>,
    /// The functions the front end was told of, sorted: each once.
    told: Vec<u32>,
}

/// The walk of `program` on a pool of `workers`.
fn walked(program: &Program, workers: usize) -> Walked {
    let pool = Pool::new(&Topology::flat(workers), false).unwrap();
    program.reached.lock().clear();
    pool.install(|| {
        let policy = policy(OptLevel::O0, Mode::Output);
        let (machine, memo) = (Machine::default(), Memo::new());
        let build = Build::new(&machine, program, &policy, &memo);
        let roots: Vec<(u32, Origin)> = ROOTS.iter().map(|it| (*it, member())).collect();
        let (mut order, mut errors) = (Vec::new(), Vec::new());
        let (nodes, statics, _, walk, _) =
            build.walk(&roots, &STATIC_ROOTS, &[], Some(&mut order), &mut errors);
        assert!(errors.is_empty(), "the walk of a program with no error: {errors:?}");
        assert_eq!(walk.reached, nodes.len());
        assert!(nodes.keys().is_sorted(), "the program's functions are in key order");
        assert!(statics.keys().is_sorted(), "the program's statics are in their order");
        let mut told: Vec<u32> = program.reached.lock().iter().flatten().copied().collect();
        told.sort_unstable();
        Walked {
            order: order.iter().map(|key| nodes[key].described.name.clone()).collect(),
            program: nodes.values().map(|it| it.described.name.clone()).collect(),
            statics: statics.values().map(|it| it.described.name.clone()).collect(),
            told,
        }
    })
}

fn named(functions: &[u32]) -> Vec<String> {
    functions.iter().map(|id| format!("f{id}")).collect()
}

fn sorted<T: Ord>(mut items: Vec<T>) -> Vec<T> {
    items.sort_unstable();
    items
}

/// The refinement: the worklist's walk gives the level walk's program ([`level_walk`]): the
/// same functions in the same order, the same statics, each function told to the front end
/// once; and gives it again, byte for byte, on a pool of one worker, of two and of eight,
/// however their describes fall.
#[test]
fn the_worklists_walk_is_the_level_walks_program_in_its_order() {
    for seed in 0..6u64 {
        let program = generated(seed);
        let (order, static_order) = level_walk(&program);
        assert!(order.len() > 200, "seed {seed}: the roots reach {} functions", order.len());
        let first = walked(&program, 1);
        assert_eq!(first.order, named(&order), "seed {seed}: the order of the level walk");
        assert_eq!(sorted(first.program.clone()), sorted(named(&order)), "seed {seed}");
        let statics: Vec<String> = static_order.iter().map(|id| format!("app::S{id}")).collect();
        assert_eq!(sorted(first.statics.clone()), sorted(statics), "seed {seed}");
        assert_eq!(first.told, sorted(order), "seed {seed}: each function told once");
        for workers in [2, 8, 8, 8] {
            assert_eq!(walked(&program, workers), first, "seed {seed}, {workers} workers");
        }
    }
}

/// The planted bug: a walk whose order is the order its describes happened to run in, here
/// depth first, as one job that takes what it just reached next would give. The oracle of
/// [`the_worklists_walk_is_the_level_walks_program_in_its_order`] compares orders, and this
/// one is another.
#[test]
fn the_oracle_catches_a_walk_in_the_order_its_describes_ran() {
    let program = generated(0);
    let (mut seen, mut order) = (HashSet::new(), Vec::new());
    let mut stack: Vec<u32> = ROOTS.into_iter().rev().collect();
    while let Some(id) = stack.pop() {
        if seen.insert(id) {
            order.push(id);
            stack.extend(program.functions[&id].calls.iter().rev());
        }
    }
    let (level, _) = level_walk(&program);
    // The statics' functions aside, the same set; another order.
    assert!(order.iter().all(|it| level.contains(it)));
    assert_ne!(named(&order), walked(&program, 1).order[..order.len()]);
}

/// No round stands between a describe and what it reached: `main` (1) calls 2 and 3, and 2
/// calls 4. The describe of 3 waits until 4's describe has begun, which a walk by levels never
/// gets to (4 is of the level after 3's): here 4 is described while 3, reached a step before
/// it, is still being described and long before the walk ends.
#[test]
fn a_function_is_described_while_one_reached_before_it_still_is() {
    const LONG: Duration = Duration::from_secs(60);
    let four = Arc::new(AtomicBool::new(false));
    let waited = Arc::new(AtomicBool::new(false));
    let mut program = Program::default();
    program.function(1, 0x11, &[2, 3]);
    program.function(2, 0x22, &[4]);
    program.function(3, 0x33, &[]);
    program.function(4, 0x44, &[]);
    program.watch = Some(Box::new({
        let (four, waited) = (Arc::clone(&four), Arc::clone(&waited));
        move |id: u32| {
            if id == 4 {
                four.store(true, Ordering::Release);
            }
            if id == 3 {
                SpinWait::within("the describe of f4, begun while f3 is described", LONG, || {
                    four.load(Ordering::Acquire)
                });
                waited.store(true, Ordering::Release);
            }
        }
    }));
    let pool = Pool::new(&Topology::flat(4), false).unwrap();
    let reached = pool.install(|| {
        let policy = policy(OptLevel::O0, Mode::Output);
        let (machine, memo) = (Machine::default(), Memo::new());
        let build = Build::new(&machine, &program, &policy, &memo);
        let mut errors = Vec::new();
        let (nodes, ..) = build.walk(&[(1, member())], &[], &[], None, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        nodes.len()
    });
    assert_eq!(reached, 4);
    assert!(waited.load(Ordering::Acquire), "f3's describe saw f4's before it ended");
}

/// A subscriber that keeps the last `done` and `known` recorded on any span: what a walk's
/// span carries, as the profile's subscriber reads it.
#[derive(Default)]
struct Counts {
    done: std::sync::atomic::AtomicU64,
    known: std::sync::atomic::AtomicU64,
    /// How many records named either field.
    records: std::sync::atomic::AtomicU64,
}

struct CountsOf<'a>(&'a Counts);

impl tracing::field::Visit for CountsOf<'_> {
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        let slot = match field.name() {
            "done" => &self.0.done,
            "known" => &self.0.known,
            _ => return,
        };
        slot.store(value, Ordering::Release);
        self.0.records.fetch_add(1, Ordering::Release);
    }

    // dyn: tracing's visitor takes any other value by its `Debug`
    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

struct Recorder(Arc<Counts>);

impl tracing::Subscriber for Recorder {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, attributes: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        attributes.record(&mut CountsOf(&self.0));
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        values.record(&mut CountsOf(&self.0));
    }

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, _: &tracing::Event<'_>) {}

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// What the span over a walk of `program` on `workers` carried last: (`done`, `known`), how
/// many records named them, and how many functions and statics the walk gave.
fn carried(program: &Program, workers: usize, bug: Option<Planted>) -> (u64, u64, u64, usize) {
    let counts = Arc::new(Counts::default());
    let pool = Pool::new(&Topology::flat(workers), false).unwrap();
    let items = pool.install(|| {
        tracing::subscriber::with_default(Recorder(Arc::clone(&counts)), || {
            let policy = policy(OptLevel::O0, Mode::Output);
            let (machine, memo) = (Machine::default(), Memo::new());
            let build = Build::new(&machine, program, &policy, &memo);
            let build = match bug {
                Some(bug) => build.planting(bug),
                None => build,
            };
            let roots: Vec<(u32, Origin)> = ROOTS.iter().map(|it| (*it, member())).collect();
            let mut errors = Vec::new();
            let span = tracing::info_span!(
                target: "blaze",
                "codegen walk",
                done = tracing::field::Empty,
                known = tracing::field::Empty,
            );
            let (nodes, statics, ..) =
                span.in_scope(|| build.walk(&roots, &STATIC_ROOTS, &[], None, &mut errors));
            assert!(errors.is_empty(), "{errors:?}");
            nodes.len() + statics.len()
        })
    });
    (
        counts.done.load(Ordering::Acquire),
        counts.known.load(Ordering::Acquire),
        counts.records.load(Ordering::Acquire),
        items,
    )
}

/// The invariant: the walk's span carries how far the walk is: as it ends, every item sent
/// was visited (`done == known`), that count is the walk's functions and statics, and it is
/// the same on one worker and on eight.
#[test]
fn the_walks_span_carries_done_and_known() {
    let program = generated(3);
    let (done, known, records, items) = carried(&program, 1, None);
    assert!(records >= 2, "the span was told its counts: {records} records");
    assert_eq!(done, known, "every item sent was visited");
    // One key a function here (`generated` gives each its own code), so the walk's nodes
    // are its entries.
    assert_eq!(done as usize, items, "the count is the walk's functions and statics");
    let (done_8, known_8, ..) = carried(&program, 8, None);
    assert_eq!((done_8, known_8), (done, known), "the same counts on eight workers");
}

/// The planted bug: only the roots are counted as known. The oracle of
/// [`the_walks_span_carries_done_and_known`] compares `done` with `known`, and here more
/// visits ended than items were said to be known.
#[test]
fn the_oracle_catches_a_walk_that_counts_only_its_roots() {
    let program = generated(3);
    let (done, known, ..) = carried(&program, 1, Some(Planted::UncountedSends));
    assert!(done > known, "done {done}, known {known}");
}
