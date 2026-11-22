//! The telemetry's proofs (D178): the distribution's brackets, the longest chain against brute
//! force, the whole path from hooks to encoded bytes, and the sampler's core. Each runs as
//! controls: the shipped code passes, and each planted bug is caught by the invariant named
//! for it.

use core::fmt;
use std::{format, vec::Vec as StdVec};

use blaze_meter::{Call, Tally};
use blaze_own::Vec;
use blaze_sim::{Broken, Config, Tape, controls};

use super::{
    Brief, Cell, CellOpts, ChainPlant, Dist, DistPlant, Exit, Level, Partial, Report, ReportOpts, SpanPath, Target, TaskRec,
    TelemetryError, TelemetryPlant, WaitRec, bucket, dist::Bracket, longest_chain,
    sampler::{
        NO_CALLER, OUTSIDE, Period, Regs, Ring, RingView, SamplerPlant, SamplerRow, Symbolize, Stack, Stop, fold, take, walk,
    },
};

// ---- The distribution -----------------------------------------------------------------------

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DistBroken {
    /// A recorded value's bucket does not bracket it.
    Bucket { value: u64 },
    /// A percentile's bracket does not hold the nearest-rank value.
    Percentile { permille: u32, want: u64 },
}

impl fmt::Display for DistBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DistBroken::Bucket { value } => write!(f, "{value} was recorded in a bucket that does not hold it"),
            DistBroken::Percentile { permille, want } => write!(f, "the {permille}‰ bracket does not hold {want}"),
        }
    }
}

blaze_sim::plants! {
    impl DistPlant breaks DistBroken {
        None,
        BucketOffByOne => DistBroken::Bucket { .. },
        PercentileBucketBelow => DistBroken::Percentile { .. },
    }
}

/// A value of a tape-drawn magnitude: every bucket is reached.
fn value(tape: &Tape) -> u64 {
    let bits = tape.below(64, "magnitude");
    if bits == 0 { tape.below(2, "0 or 1") } else { (1u64 << bits) | tape.below(1u64 << bits, "below the top bit") }
}

fn dist_brackets(tape: &Tape, plant: DistPlant) -> Result<(), Broken<DistBroken>> {
    let n = 1 + tape.index(40, "values");
    let mut values: StdVec<u64> = StdVec::new();
    let mut dist = Dist::ZERO;
    for _ in 0..n {
        let v = value(tape);
        let mut one = Dist::ZERO;
        one.record_with(v, plant);
        let at = one.buckets().iter().position(|c| *c == 1).ok_or(Broken::Harness(format!("{v} recorded nowhere")))?;
        if !Bracket::of(at).holds(v) {
            return Err(Broken::Oracle(DistBroken::Bucket { value: v }));
        }
        dist.plus(&one);
        values.push(v);
    }
    values.sort_unstable();
    for permille in [1u32, 250, 500, 900, 990, 1000] {
        let rank = (values.len() as u128 * u128::from(permille)).div_ceil(1000).max(1) as usize;
        let want = values[rank - 1];
        let got = dist.at_permille(permille, plant).ok_or(Broken::Harness(format!("no {permille}‰ of {n} values")))?;
        if !got.holds(want) {
            return Err(Broken::Oracle(DistBroken::Percentile { permille, want }));
        }
    }
    if bucket(0) != 0 || bucket(1) != 0 || bucket(u64::MAX) != 63 {
        return Err(Broken::Harness("the bucket's ends moved".into()));
    }
    Ok(())
}

#[test]
fn a_distribution_brackets_every_value_and_percentile() {
    controls(Config::cases(500), dist_brackets);
}

// ---- The longest chain ----------------------------------------------------------------------

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ChainBroken {
    /// The chain's total is not the brute-force longest.
    Total { got: u64, want: u64 },
}

impl fmt::Display for ChainBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChainBroken::Total { got, want } => write!(f, "the chain's total is {got} ns, the longest path is {want} ns"),
        }
    }
}

blaze_sim::plants! {
    impl ChainPlant breaks ChainBroken {
        None,
        FirstWaitOnly => ChainBroken::Total { .. },
        OwnDropped => ChainBroken::Total { .. },
    }
}

/// A task's value by brute force: its own time and the largest of its waits (a wait on a task
/// is worth that task's value).
fn brute(task: u64, tasks: &[TaskRec], waits: &[WaitRec]) -> u64 {
    let own = tasks.iter().find(|t| t.task == task).map_or(0, |t| t.own_ns);
    let best = waits
        .iter()
        .filter(|w| w.task == task)
        .map(|w| match w.target {
            Target::Task(id) if tasks.iter().any(|t| t.task == id) => brute(id, tasks, waits),
            _ => w.ns(),
        })
        .max()
        .unwrap_or(0);
    own.saturating_add(best)
}

fn chain_longest(tape: &Tape, plant: ChainPlant) -> Result<(), Broken<ChainBroken>> {
    let n = 1 + tape.index(8, "tasks");
    let mut tasks: StdVec<TaskRec> = StdVec::new();
    let mut waits: StdVec<WaitRec> = StdVec::new();
    for at in 0..n as u64 {
        let own = tape.below(1_000, "own ns");
        tasks.push(TaskRec { task: at, name: "task", start: at * 10_000, end: at * 10_000 + 5_000, own_ns: own });
        for k in 0..tape.below(4, "waits") {
            let start = at * 10_000 + k * 100;
            let ns = tape.below(2_000, "wait ns");
            // Only on later tasks: the waits make no circle.
            let later = (at + 1..n as u64).count() as u64;
            let target = if later > 0 && tape.coin("on a task") {
                Target::Task(at + 1 + tape.below(later, "which"))
            } else {
                Target::Thing { id: None }
            };
            waits.push(WaitRec { task: at, on: "w", target, start, end: start + ns });
        }
    }
    let want = (0..n as u64).map(|t| brute(t, &tasks, &waits)).max().unwrap_or(0);
    let alloc = blaze_own::testing::heap().shared();
    let got = longest_chain(&tasks, &waits, Partial::default(), alloc, plant)
        .map_err(|e| Broken::Harness(format!("the chain: {e}")))?
        .map_or(0, |c| c.total_ns);
    if got != want {
        return Err(Broken::Oracle(ChainBroken::Total { got, want }));
    }
    Ok(())
}

#[test]
fn the_longest_chain_is_the_brute_force_longest_path() {
    controls(Config::cases(500), chain_longest);
}

#[test]
fn a_chain_in_a_circle_is_refused() {
    let tasks = [
        TaskRec { task: 1, name: "a", start: 0, end: 10, own_ns: 1 },
        TaskRec { task: 2, name: "b", start: 0, end: 10, own_ns: 1 },
    ];
    let waits = [
        WaitRec { task: 1, on: "w", target: Target::Task(2), start: 0, end: 5 },
        WaitRec { task: 2, on: "w", target: Target::Task(1), start: 0, end: 5 },
    ];
    let alloc = blaze_own::testing::heap().shared();
    let got = longest_chain(&tasks, &waits, Partial::default(), alloc, ChainPlant::None);
    assert!(matches!(got, Err(TelemetryError::ChainCycle { .. })), "{got:?}");
}

// ---- The whole path: hooks, cells, merge, encoding ------------------------------------------

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RunBroken {
    /// A span was left open after every span closed.
    SpanOpen { open: u64 },
    /// The report's waits are not the waits the run made.
    WaitLost { got: (u64, u64), want: (u64, u64) },
    /// A cell merged a second time was taken.
    MergedTwice,
    /// An effect row is not the truth.
    Effect { call: &'static str },
    /// The span rows' counts are not the spans closed.
    Spans { got: u64, want: u64 },
    /// Two merge orders encoded different bytes.
    Bytes { level: &'static str },
    /// A worker's accounted time is not its lifetime.
    Pool { worker: u32 },
}

impl fmt::Display for RunBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunBroken::SpanOpen { open } => write!(f, "{open} spans open at exit"),
            RunBroken::WaitLost { got, want } => write!(f, "waits (count, ns) {got:?}, the run made {want:?}"),
            RunBroken::MergedTwice => f.write_str("a cell merged twice was taken"),
            RunBroken::Effect { call } => write!(f, "the {call} row is not what the run did"),
            RunBroken::Spans { got, want } => write!(f, "span rows count {got}, the run closed {want}"),
            RunBroken::Bytes { level } => write!(f, "at {level} two merge orders encoded different bytes"),
            RunBroken::Pool { worker } => write!(f, "worker {worker}'s accounted time is not its lifetime"),
        }
    }
}

blaze_sim::plants! {
    impl TelemetryPlant breaks RunBroken {
        None,
        SpanLeftOpen => RunBroken::SpanOpen { .. },
        WaitUnrecorded => RunBroken::WaitLost { .. },
        MergedTwice => RunBroken::MergedTwice,
    }
}

/// Three names by four ids: twelve wait keys, past blaze-meter's eight rows, so a worker's
/// table evicts and the report's gathered table must still be one for every merge order.
const WAIT_ON: [&str; 3] = ["lock", "record", "effect"];
const SPAN_NAMES: [&str; 3] = ["parse", "check", "emit"];

#[derive(Default)]
struct Truth {
    waits: (u64, u64),
    effects: [(u64, u64, u64); blaze_meter::CALLS],
    closed: u64,
}

fn run(tape: &Tape, plant: TelemetryPlant) -> Result<(), Broken<RunBroken>> {
    let workers = 1 + tape.below(4, "workers") as u32;
    let level = [Level::Totals, Level::Spans, Level::Detail][tape.index(3, "level")];
    let alloc = blaze_own::testing::heap().shared();
    let harness = |e: TelemetryError| Broken::Harness(format!("{e}"));
    let mut truth = Truth::default();
    let mut cells: StdVec<(Tally, Cell<_>)> = StdVec::new();
    let mut task = 0u64;
    for worker in 0..workers {
        let mut tally = Tally::ZERO;
        let mut cell = Cell::new(CellOpts { worker, level }, alloc).map_err(harness)?;
        let mut clock = 0u64;
        cell.pool().started(clock);
        for _ in 0..tape.below(24, "events") {
            let ns = 1 + tape.below(5_000, "ns");
            match tape.below(4, "event") {
                0 => {
                    let outer = cell.open(&SpanPath::ROOT, SPAN_NAMES[tape.index(3, "span")], clock, Brief::ZERO);
                    if tape.coin("nested") {
                        let inner = cell.open(outer.path(), SPAN_NAMES[tape.index(3, "inner")], clock, Brief::ZERO);
                        cell.close(inner, clock + ns / 2, Brief::ZERO, plant).map_err(harness)?;
                        truth.closed += 1;
                    }
                    cell.close(outer, clock + ns, Brief::ZERO, plant).map_err(harness)?;
                    truth.closed += 1;
                }
                1 => {
                    let on = WAIT_ON[tape.index(3, "on")];
                    let rec = WaitRec { task, on, target: Target::Thing { id: Some(tape.below(4, "id")) }, start: clock, end: clock + ns };
                    cell.wait_ended(&mut tally, rec, plant);
                    truth.waits.0 += 1;
                    truth.waits.1 += ns;
                }
                2 => {
                    let call = Call::ALL[tape.index(Call::ALL.len(), "call")];
                    let bytes = tape.below(1 << 20, "bytes");
                    cell.effect(call, bytes, ns);
                    let row = &mut truth.effects[call as usize];
                    *row = (row.0 + 1, row.1 + bytes, row.2 + ns);
                }
                _ => {
                    cell.task_ended(TaskRec { task, name: "task", start: clock, end: clock + ns, own_ns: ns }, false);
                    task += 1;
                }
            }
            cell.pool().worked(ns);
            clock += ns;
            let parked = tape.below(100, "parked");
            cell.pool().parked(parked);
            clock += parked;
        }
        cell.pool().stopped(clock);
        cells.push((tally, cell));
    }
    let opts = ReportOpts { level, most_workers: workers };
    let mut encoded: StdVec<StdVec<u8>> = StdVec::new();
    let mut first: Option<Report<_>> = None;
    for reversed in [false, true] {
        let mut report = Report::new(opts, alloc).map_err(harness)?;
        let order: StdVec<usize> = if reversed { (0..cells.len()).rev().collect() } else { (0..cells.len()).collect() };
        for at in order {
            let (tally, cell) = &cells[at];
            report.merge_planted(tally, cell, plant).map_err(harness)?;
        }
        report.finish(Exit::Returned).map_err(harness)?;
        let mut bytes = Vec::new_in(alloc);
        report.encode_into(&mut bytes).map_err(harness)?;
        encoded.push(bytes.iter().copied().collect());
        if first.is_none() {
            first = Some(report);
        }
    }
    let Some(mut report) = first else { return Err(Broken::Harness("no report".into())) };
    if report.open_spans() != 0 {
        return Err(Broken::Oracle(RunBroken::SpanOpen { open: report.open_spans() }));
    }
    if report.waited().total() != truth.waits {
        return Err(Broken::Oracle(RunBroken::WaitLost { got: report.waited().total(), want: truth.waits }));
    }
    for (at, call) in Call::ALL.iter().enumerate() {
        let row = &report.effects()[at];
        if (row.count, row.bytes, row.ns) != truth.effects[*call as usize] {
            return Err(Broken::Oracle(RunBroken::Effect { call: call.name() }));
        }
    }
    if level.keeps(Level::Spans) {
        let got: u64 = report.spans().iter().map(|r| r.count).sum::<u64>() + report.other_span().count;
        if got != truth.closed {
            return Err(Broken::Oracle(RunBroken::Spans { got, want: truth.closed }));
        }
    }
    for row in report.workers() {
        if row.pool.lifetime() != Some(row.pool.accounted()) {
            return Err(Broken::Oracle(RunBroken::Pool { worker: row.worker }));
        }
    }
    if encoded[0] != encoded[1] {
        return Err(Broken::Oracle(RunBroken::Bytes { level: level.name() }));
    }
    // Every run tries to merge worker 0 again: refused, unless planted.
    let (tally, cell) = &cells[0];
    match report.merge_planted(tally, cell, plant) {
        Err(TelemetryError::MergedTwice { worker: 0 }) => Ok(()),
        Err(other) => Err(Broken::Harness(format!("the second merge: {other}"))),
        Ok(()) => Err(Broken::Oracle(RunBroken::MergedTwice)),
    }
}

#[test]
fn a_run_reports_exact_sums_and_equal_bytes_at_every_level() {
    controls(Config::cases(400), run);
}

/// Two workers whose waits name fourteen keys (past blaze-meter's eight), one with keys tied in
/// time: merged in both orders, the reports encode equal bytes at every level (rule 25). The
/// merge-order form keeps a different set on these same tables (blaze-meter's
/// `gathering_waits_is_a_function_of_the_multiset_not_the_order` is its control).
#[test]
fn two_merge_orders_of_more_than_eight_wait_keys_encode_equal_bytes() {
    const NAMES: [&str; 12] = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l"];
    let alloc = blaze_own::testing::heap().shared();
    for level in [Level::Totals, Level::Spans, Level::Detail] {
        let mut workers: StdVec<(Tally, Cell<_>)> = StdVec::new();
        for worker in 0..2u32 {
            let mut tally = Tally::ZERO;
            let mut cell = Cell::new(CellOpts { worker, level }, alloc).expect("a cell");
            cell.pool().started(0);
            let waits: StdVec<(&'static str, Option<u64>, u64)> = if worker == 0 {
                NAMES[..8].iter().enumerate().map(|(at, name)| (*name, None, 100 + at as u64)).collect()
            } else {
                let mut waits: StdVec<(&'static str, Option<u64>, u64)> =
                    NAMES[8..].iter().enumerate().map(|(at, name)| (*name, None, 1000 + at as u64)).collect();
                waits.push(("y", Some(1), 105));
                waits.push(("z", Some(1), 105));
                waits
            };
            let mut clock = 0u64;
            for (on, id, ns) in waits {
                let rec = WaitRec { task: 0, on, target: Target::Thing { id }, start: clock, end: clock + ns };
                cell.wait_ended(&mut tally, rec, TelemetryPlant::None);
                clock += ns;
            }
            cell.pool().worked(clock);
            cell.pool().stopped(clock);
            workers.push((tally, cell));
        }
        let mut encoded: StdVec<StdVec<u8>> = StdVec::new();
        for order in [[0usize, 1], [1, 0]] {
            let mut report = Report::new(ReportOpts { level, most_workers: 2 }, alloc).expect("a report");
            for at in order {
                let (tally, cell) = &workers[at];
                report.merge(tally, cell).expect("one merge a worker");
            }
            report.finish(Exit::Returned).expect("a finish");
            let kept: StdVec<&str> = report.waited().rows.iter().flatten().map(|row| row.on).collect();
            assert_eq!(kept, ["l", "k", "j", "i", "h", "g", "f", "y"], "{}", level.name());
            let mut bytes = Vec::new_in(alloc);
            report.encode_into(&mut bytes).expect("an encoding");
            encoded.push(bytes.iter().copied().collect());
        }
        assert_eq!(encoded[0], encoded[1], "{}", level.name());
    }
}

// ---- The hooks: spans across tasks, the current task, Totals' cost ---------------------------

/// A snapshot whose first field is `n`: what a hook's `Cell::snapshot` would read.
fn used(n: u64) -> Brief {
    let mut brief = Brief::ZERO;
    brief.v[0] = n;
    brief
}

/// Decision 1 of the module doc, "Spans across tasks": a span is its opener's value's life
/// (one close, its wall time the opener's open to close), and the use of a task spawned under
/// it is charged to its path by the worker that ran it: a task nested in the opener's stretch
/// on the same worker is not counted twice, and one on another worker adds to the sum. Rows are
/// inclusive: the task's use reaches every ancestor's row too.
#[test]
fn a_parallel_spans_use_is_the_sum_over_workers_and_its_wall_is_the_openers() {
    let alloc = blaze_own::testing::heap().shared();
    let level = Level::Spans;
    let mut opener = Cell::new(CellOpts { worker: 0, level }, alloc).expect("a cell");
    let mut other = Cell::new(CellOpts { worker: 1, level }, alloc).expect("a cell");
    let (mut opener_tally, other_tally) = (Tally::ZERO, Tally::ZERO);
    let (outer_slot, outer) = opener.span_open(&SpanPath::ROOT, "check", 0, used(0)).expect("a slot");
    let (slot, a) = opener.span_open(&outer, "bodies", 0, used(0)).expect("a slot");
    // A task under `check/bodies` run inline in the opener's stretch: 0..10 uses 7.
    let inline = opener.task_open("body", &a, 2, used(1));
    assert_ne!(opener.current_task(), 0, "a task begun is the worker's current task");
    let task = opener.current_task();
    opener.wait_ended(&mut opener_tally, WaitRec { task, on: "lock", target: Target::Thing { id: None }, start: 3, end: 5 }, TelemetryPlant::None);
    opener.task_close(inline, 10, used(8), false);
    assert_eq!(opener.current_task(), 0, "the task ended: none is current");
    // Two tasks of the same span on worker 1: uses 100 and 50, neither in an opener's stretch.
    for (start, end, n) in [(1, 20, 100), (20, 25, 50)] {
        let begun = other.task_open("body", &a, start, used(0));
        other.task_close(begun, end, used(n), false);
    }
    opener.span_close(slot, 30, used(20));
    opener.span_close(outer_slot, 31, used(21));
    assert_eq!(opener.unrecorded(), 0);
    assert_eq!(opener.open_spans(), 0, "every span opened was closed by its token");
    for cell in [&mut opener, &mut other] {
        cell.stop(40);
    }
    let mut report = Report::new(ReportOpts { level, most_workers: 2 }, alloc).expect("a report");
    report.merge(&opener_tally, &opener).expect("a merge");
    report.merge(&other_tally, &other).expect("a merge");
    report.finish(Exit::Returned).expect("a finish");
    let row = |names: &[&str]| *report.spans().iter().find(|row| row.path.names() == names).expect("the path's row");
    let bodies = row(&["check", "bodies"]);
    assert_eq!(bodies.count, 1, "one close: the opener's");
    assert_eq!(bodies.wall_ns, 30, "the wall is open to close on the opener");
    assert_eq!(bodies.sum.v[0], 20 + 100 + 50, "the opener's 20 (its inline task's 7 once) and worker 1's 150");
    let check = row(&["check"]);
    assert_eq!(check.count, 1);
    assert_eq!(check.sum.v[0], 21 + 100 + 50, "the parent's row holds the tasks' use too");
    // The inline task's record: its own time is its wall less its wait.
    let rec = opener.parts().4.tasks().iter().find(|rec| rec.task == task).copied().expect("the inline task's record");
    assert_eq!((rec.end - rec.start, rec.own_ns), (8, 6));
}

/// Decision 4: at `Totals` a cell reserves nothing, and no hook allocates. The control: the
/// same hooks at `Spans` allocate (so the counter sees the cell's allocator).
#[test]
fn a_cell_at_totals_allocates_nothing() {
    for level in [Level::Totals, Level::Spans] {
        let counting = blaze_own::testing::Counting::new(blaze_own::testing::TestHeap);
        {
            let mut cell = Cell::new(CellOpts { worker: 0, level }, &counting).expect("a cell");
            let mut tally = Tally::ZERO;
            let opened = cell.span_open(&SpanPath::ROOT, "check", 0, Brief::ZERO);
            let begun = cell.task_open("body", &SpanPath::ROOT, 1, Brief::ZERO);
            cell.wait_ended(&mut tally, WaitRec { task: cell.current_task(), on: "lock", target: Target::Thing { id: None }, start: 1, end: 2 }, TelemetryPlant::None);
            cell.effect(Call::Read, 0, 5);
            cell.task_close(begun, 3, Brief::ZERO, false);
            if let Some((slot, _)) = opened {
                cell.span_close(slot, 4, Brief::ZERO);
            }
            cell.stop(5);
            assert_eq!(tally.waits().rows.iter().flatten().count(), 1, "the wait reached the tally at {}", level.name());
        }
        match level {
            Level::Totals => assert_eq!(counting.allocations(), 0, "a Totals cell allocated"),
            Level::Spans | Level::Detail => assert!(counting.allocations() > 0, "the control: a Spans cell reserves its tables"),
        }
        counting.check().expect("every block freed");
    }
}

// ---- The sampler's core ---------------------------------------------------------------------

/// Eight functions at `0x1000 * (k + 1)`, 4 KiB each.
struct Fake;

const FUNC_NAMES: [&str; 8] = ["f0", "f1", "f2", "f3", "f4", "f5", "f6", "f7"];

impl Symbolize for Fake {
    fn find(&self, address: u64) -> Option<(u32, &str)> {
        let k = (address / 0x1000).checked_sub(1)?;
        FUNC_NAMES.get(k as usize).map(|name| (k as u32, *name))
    }

    fn name(&self, index: u32) -> Option<&str> {
        FUNC_NAMES.get(index as usize).copied()
    }
}

fn func(k: u64) -> u64 {
    0x1000 * (k + 1) + 0x10
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SampleBroken {
    /// The ring's on-CPU or parked count is not the truth.
    Counts { seen: u64, on_cpu: u64, parked: u64, want_parked: u64 },
    /// A kept sample is not the one at its place on the stride.
    Kept { k: usize },
    /// The walk read outside the stack.
    ReadPast { reads: u64 },
    /// The fold's ticks of a function are not the brute-force count.
    Fold { key: u32, got: u64, want: u64 },
}

impl fmt::Display for SampleBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SampleBroken::Counts { seen, on_cpu, parked, want_parked } => {
                write!(f, "the ring saw {seen} on-CPU and {parked} parked ticks, the run had {on_cpu} and {want_parked}")
            }
            SampleBroken::Kept { k } => write!(f, "kept sample {k} is not the tick at its stride"),
            SampleBroken::ReadPast { reads } => write!(f, "the walk read outside the stack {reads} times"),
            SampleBroken::Fold { key, got, want } => write!(f, "function {key}: the fold says {got} ticks, the samples {want}"),
        }
    }
}

blaze_sim::plants! {
    impl SamplerPlant breaks SampleBroken {
        None,
        ParkedSampled => SampleBroken::Counts { .. },
        WalkUnbounded => SampleBroken::ReadPast { .. },
    }
}

/// A fake stack: words from `BASE`, the readable part `[BASE, TOP)`, and past it a poisoned
/// tail whose every read is counted.
const BASE: u64 = 0x10_0000;
const WORDS: usize = 64;
const TOP: u64 = BASE + 8 * 48;

fn sampler_core(tape: &Tape, plant: SamplerPlant) -> Result<(), Broken<SampleBroken>> {
    let mut ring = std::boxed::Box::new(Ring::EMPTY);
    let ticks = tape.below(3_000, "ticks");
    let (mut on_cpu, mut parked) = (0u64, 0u64);
    // The truth of each on-CPU tick: its tick, leaf and nearest caller.
    let mut truth: StdVec<(u64, u32, u32)> = StdVec::new();
    let reads_past = core::cell::Cell::new(0u64);
    for tick in 0..ticks {
        let is_parked = tape.one_in(4, "parked");
        // A chain of `depth` records at 16-byte steps from BASE; its last record's next is 0,
        // or (a corrupt one) past the stack's top.
        let depth = tape.below(4, "depth") as usize;
        let mut words = [0u64; WORDS];
        let leaf = tape.below(9, "leaf");
        let mut callers = [0u64; 3];
        for (at, caller) in callers.iter_mut().enumerate().take(depth) {
            *caller = tape.below(8, "caller");
            words[2 * at] = if at + 1 < depth { BASE + 16 * (at as u64 + 1) } else { 0 };
            words[2 * at + 1] = func(*caller);
        }
        if depth > 0 && tape.one_in(3, "corrupt") {
            words[2 * (depth - 1)] = TOP + 16;
            words[(TOP + 16 - BASE) as usize / 8 + 1] = func(0);
        }
        let read = |at: u64| -> u64 {
            if !(BASE..TOP).contains(&at) {
                reads_past.set(reads_past.get() + 1);
            }
            at.checked_sub(BASE).and_then(|off| words.get(off as usize / 8)).copied().unwrap_or(0)
        };
        let pc = if leaf == 8 { 0x9_0000 } else { func(leaf) };
        let fp = if depth > 0 { BASE } else { 0 };
        let span = 0;
        take(&mut ring, is_parked, Regs { pc, fp, sp: BASE }, TOP, span, tick, read, plant.sys());
        if is_parked {
            parked += 1;
        } else {
            on_cpu += 1;
            let leaf_key = if leaf == 8 { OUTSIDE } else { leaf as u32 };
            let caller_key = if depth > 0 { callers[0] as u32 } else { NO_CALLER };
            truth.push((tick, leaf_key, caller_key));
        }
    }
    if reads_past.get() > 0 {
        return Err(Broken::Oracle(SampleBroken::ReadPast { reads: reads_past.get() }));
    }
    if ring.seen() != on_cpu || ring.parked() != parked {
        return Err(Broken::Oracle(SampleBroken::Counts { seen: ring.seen(), on_cpu, parked: ring.parked(), want_parked: parked }));
    }
    let stride = ring.stride() as usize;
    for (k, sample) in ring.samples().iter().enumerate() {
        if truth.get(k * stride).map(|t| t.0) != Some(sample.tick) {
            return Err(Broken::Oracle(SampleBroken::Kept { k }));
        }
    }
    // The fold against a brute-force count of the kept samples, each weighed by the stride.
    let alloc = blaze_own::testing::heap().shared();
    let view = RingView::of(&ring);
    let profile = fold(core::iter::once(view), &Fake, &[], SamplerRow::new(Period::DEFAULT), alloc)
        .map_err(|e| Broken::Harness(format!("the fold: {e}")))?;
    let mut want: StdVec<(u32, u64)> = StdVec::new();
    for k in 0..ring.samples().len() {
        let leaf = truth[k * stride].1;
        match want.iter_mut().find(|(key, _)| *key == leaf) {
            Some(row) => row.1 += stride as u64,
            None => want.push((leaf, stride as u64)),
        }
    }
    for (key, ticks) in &want {
        let got = profile.funcs.iter().flatten().find(|row| row.key == *key).map_or(0, |row| row.ticks);
        if got != *ticks {
            return Err(Broken::Oracle(SampleBroken::Fold { key: *key, got, want: *ticks }));
        }
    }
    if profile.on_cpu != on_cpu || profile.parked != parked {
        return Err(Broken::Harness("the profile's counts are not the ring's".into()));
    }
    Ok(())
}

#[test]
fn the_sampler_keeps_a_uniform_ring_and_folds_exact_counts() {
    controls(Config::cases(300), sampler_core);
}

#[test]
fn the_walk_stops_at_a_corrupt_record_a_cycle_and_the_stack_end() {
    let stack = Stack { low: BASE, high: BASE + 64 };
    let words = [BASE + 16, func(1), BASE + 16, func(2), BASE + 48, func(3), 0, 0];
    let read = |at: u64| words[((at - BASE) / 8) as usize];
    // A cycle: the second record points at itself.
    let walked = walk(BASE, stack, read, SamplerPlant::None.sys());
    assert_eq!((walked.depth, walked.stop), (2, Stop::NotAbove));
    // A corrupt (unaligned) frame pointer.
    let walked = walk(BASE + 4, stack, read, SamplerPlant::None.sys());
    assert_eq!((walked.depth, walked.stop), (0, Stop::Unaligned));
    // A record straddling the end is refused before it is read.
    let walked = walk(BASE + 56, stack, |_| panic!("read past the stack"), SamplerPlant::None.sys());
    assert_eq!((walked.depth, walked.stop), (0, Stop::Bounds));
}
