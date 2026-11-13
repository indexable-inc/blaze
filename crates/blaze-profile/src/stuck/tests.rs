//! The watchdog under a simulation on an injected clock: tasks that run, sleep to a deadline,
//! wait for a signal, take locks others hold and join each other, with planted faults (a lost
//! wake, a deadline that passes with no wake, a two-lock cycle, a three-task join cycle) and a
//! long CPU-bound task that must never be flagged. The model knows which tasks can never run
//! again; the oracle holds the watchdog's reports to it.

use hegel::{TestCase, generators as gs};
use std::alloc::Global;

use super::*;

const S: u64 = 1_000_000_000;
const LOOK: u64 = LOOK_EVERY.as_nanos() as u64;
const HORIZON: u64 = 1500 * S;

/// What a simulated task does before it runs.
#[derive(Clone, Copy, Debug)]
enum Blocks {
    /// Runs at once.
    Not,
    /// Sleeps to `deadline`, woken at `wakes` (never, if a planted fault).
    Sleep { deadline: u64, wakes: Option<u64> },
    /// Waits on a signal with no deadline, woken at `wakes` (never: a lost wake).
    Quiet { wakes: Option<u64> },
    /// Waits for a lock the task at the index holds until it is done.
    Lock(usize),
    /// Joins the task at the index.
    Join(usize),
}

#[derive(Clone, Copy, Debug)]
struct Sim {
    start: u64,
    blocks: Blocks,
    /// How long it runs once unblocked, burning CPU in one span.
    work: u64,
}

/// What a control forces into every scenario, so its planted bug shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Force {
    Nothing,
    LostWake,
    LongCpu,
    WaiterBehindLostWake,
}

fn secs(tc: &TestCase, lo: u64, hi: u64) -> u64 {
    S * tc.draw(gs::integers::<u64>().min_value(lo).max_value(hi))
}

fn looks(tc: &TestCase, hi: u64) -> u64 {
    LOOK * tc.draw(gs::integers::<u64>().max_value(hi))
}

fn scenario(tc: &TestCase, force: Force) -> Vec<Sim, Global> {
    let mut sims = Vec::new_in(Global);
    // Plants first, so honest waiters may land behind them.
    if force == Force::LostWake || tc.draw(gs::booleans()) {
        sims.push(Sim { start: looks(tc, 20), blocks: Blocks::Quiet { wakes: None }, work: S });
    }
    if tc.draw(gs::booleans()) {
        let start = looks(tc, 20);
        let deadline = start + looks(tc, 60);
        sims.push(Sim { start, blocks: Blocks::Sleep { deadline, wakes: None }, work: S });
    }
    if tc.draw(gs::booleans()) {
        let start = looks(tc, 20);
        let a = sims.len();
        sims.push(Sim { start, blocks: Blocks::Lock(a + 1), work: S });
        sims.push(Sim { start, blocks: Blocks::Lock(a), work: S });
    }
    if tc.draw(gs::booleans()) {
        let start = looks(tc, 20);
        let a = sims.len();
        sims.push(Sim { start, blocks: Blocks::Join(a + 1), work: S });
        sims.push(Sim { start, blocks: Blocks::Join(a + 2), work: S });
        sims.push(Sim { start, blocks: Blocks::Join(a), work: S });
    }
    if force == Force::LongCpu || tc.draw(gs::booleans()) {
        sims.push(Sim { start: looks(tc, 20), blocks: Blocks::Not, work: 1400 * S });
    }
    if force == Force::WaiterBehindLostWake {
        let start = looks(tc, 20);
        let h = sims.len();
        sims.push(Sim { start, blocks: Blocks::Quiet { wakes: None }, work: S });
        sims.push(Sim { start: start + looks(tc, 4), blocks: Blocks::Lock(h), work: S });
    }
    let honest = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
    for _ in 0..honest {
        let mut start = looks(tc, 20);
        let blocks = match tc.draw(gs::integers::<u8>().max_value(4)) {
            1 => {
                let deadline = start + looks(tc, 60);
                let wakes = start + secs(tc, 0, (deadline - start) / S);
                Blocks::Sleep { deadline, wakes: Some(wakes) }
            }
            // A signal with no deadline wakes within its bound, less a look.
            2 => Blocks::Quiet { wakes: Some(start + secs(tc, 0, 110)) },
            3 | 4 if !sims.is_empty() => {
                let h = tc.draw(gs::integers::<usize>().max_value(sims.len() - 1));
                // The holder holds from its start: a waiter comes no earlier.
                start = start.max(sims[h].start);
                if tc.draw(gs::booleans()) { Blocks::Lock(h) } else { Blocks::Join(h) }
            }
            _ => Blocks::Not,
        };
        let work = secs(tc, 0, if tc.draw(gs::booleans()) { 2000 } else { 30 });
        sims.push(Sim { start, blocks, work });
    }
    sims
}

/// When each task stops waiting: `None` if never (a fault, a cycle, or behind either).
fn resolves(sims: &[Sim]) -> Vec<Option<u64>, Global> {
    type Memo = [Option<Option<u64>>];
    fn go(sims: &[Sim], i: usize, memo: &mut Memo, on: &mut [bool]) -> Option<u64> {
        if let Some(it) = memo[i] {
            return it;
        }
        if on[i] {
            return None; // a cycle
        }
        on[i] = true;
        let sim = sims[i];
        let it = match sim.blocks {
            Blocks::Not => Some(sim.start),
            Blocks::Sleep { wakes, .. } | Blocks::Quiet { wakes } => wakes,
            Blocks::Lock(h) | Blocks::Join(h) => {
                go(sims, h, memo, on).map(|r| sim.start.max(r + sims[h].work))
            }
        };
        on[i] = false;
        memo[i] = Some(it);
        it
    }
    let mut memo = Vec::new_in(Global);
    memo.resize(sims.len(), None);
    let mut on = Vec::new_in(Global);
    on.resize(sims.len(), false);
    let mut out = Vec::new_in(Global);
    for i in 0..sims.len() {
        out.push(go(sims, i, &mut memo, &mut on));
    }
    out
}

fn holder(sim: &Sim) -> Option<usize> {
    match sim.blocks {
        Blocks::Lock(h) | Blocks::Join(h) => Some(h),
        _ => None,
    }
}

/// The tasks on the cycle through `i`, if `i` is on one.
fn cycle_through(sims: &[Sim], i: usize) -> Option<Vec<usize, Global>> {
    let mut at = i;
    let mut path = Vec::new_in(Global);
    for _ in 0..=sims.len() {
        path.push(at);
        at = holder(&sims[at])?;
        if at == i {
            return Some(path);
        }
    }
    None
}

/// When the watchdog must have said task `i` (one that never runs again), by the bounds of
/// [`super`]: its own bound if it ends its chain, its cycle's, or its chain end's and its own
/// [`BEHIND_AFTER`].
fn due(sims: &[Sim], i: usize) -> u64 {
    fn end_due(sims: &[Sim], i: usize, steps: usize) -> u64 {
        if let Some(cycle) = cycle_through(sims, i) {
            let last = cycle.iter().map(|&c| sims[c].start).max().unwrap();
            return last + CYCLE_AFTER.as_nanos() as u64;
        }
        let sim = sims[i];
        match sim.blocks {
            Blocks::Quiet { .. } => sim.start + STUCK_AFTER.as_nanos() as u64,
            Blocks::Sleep { deadline, .. } => deadline + DEADLINE_SLACK.as_nanos() as u64,
            Blocks::Lock(h) | Blocks::Join(h) if steps > 0 => end_due(sims, h, steps - 1),
            _ => unreachable!("task {i} never runs again but blocks on nothing"),
        }
    }
    let at = end_due(sims, i, sims.len());
    if cycle_through(sims, i).is_none() && holder(&sims[i]).is_some() {
        at.max(sims[i].start + BEHIND_AFTER.as_nanos() as u64)
    } else {
        at
    }
}

fn seen_at(sims: &[Sim], resolves: &[Option<u64>], now: u64) -> Vec<TaskSeen, Global> {
    let mut seen = Vec::new_in(Global);
    for (i, sim) in sims.iter().enumerate() {
        let done = resolves[i].map(|r| r + sim.work);
        if now < sim.start || done.is_some_and(|d| now >= d) {
            continue;
        }
        // A blocked thread accrues a microsecond a look in the kernel; never a slice.
        let base = (i as u64 + 1) << 40;
        let noise = now / LOOK * 1_000;
        let (span, cpu, waits) = match resolves[i] {
            Some(r) if now >= r => (2 * i as u64 + 2, base + noise + (now - r), WaitsOn::Nothing),
            _ => {
                let waits = match sim.blocks {
                    Blocks::Sleep { deadline, .. } => {
                        WaitsOn::Named { what: "sleep", deadline_ns: Some(deadline) }
                    }
                    Blocks::Quiet { .. } => WaitsOn::Named { what: "signal", deadline_ns: None },
                    Blocks::Lock(h) => {
                        WaitsOn::Lock { name: "lock", holder: Some(Task::Thread(h as u64)) }
                    }
                    Blocks::Join(h) => WaitsOn::Join { task: Task::Thread(h as u64) },
                    Blocks::Not => unreachable!("runs from its start"),
                };
                (2 * i as u64 + 1, base + noise, waits)
            }
        };
        seen.push(TaskSeen {
            task: Task::Thread(i as u64),
            build: 1,
            span,
            span_since_ns: sim.start,
            cpu_ns: Some(cpu),
            waits,
            bound_ns: None,
        });
    }
    seen
}

/// Runs the scenario under the watchdog with `flaw` planted and checks the oracle.
fn simulate(tc: &TestCase, flaw: Flaw, force: Force) -> Result<(), String> {
    let sims = scenario(tc, force);
    let resolves = resolves(&sims);
    let mut watch = Watch::new();
    watch.flaw = flaw;
    let mut said: Vec<(u64, Stuck), Global> = Vec::new_in(Global);
    let mut now = 0;
    while now <= HORIZON {
        let seen = seen_at(&sims, &resolves, now);
        for stuck in watch.look(now, &seen) {
            said.push((now, stuck));
        }
        now += LOOK;
    }
    oracle(&sims, &resolves, &said)
}

fn index(task: Task) -> usize {
    match task {
        Task::Thread(i) => i as usize,
        other => panic!("the simulation has only threads, not {other:?}"),
    }
}

/// The invariant: every task that can never run again is reported within its bound, with the
/// edge that explains it; no task that later makes progress by itself is ever reported; every
/// reported cycle is a real cycle of the model's waits; and a task is said once.
fn oracle(sims: &[Sim], resolves: &[Option<u64>], said: &[(u64, Stuck)]) -> Result<(), String> {
    let mut told: Vec<Option<(u64, Stuck)>, Global> = Vec::new_in(Global);
    told.resize(sims.len(), None);
    for (at, stuck) in said {
        let cycle = stuck.cycle.iter().map(|it| it.task);
        for task in std::iter::once(stuck.task).chain(cycle) {
            let i = index(task);
            if resolves[i].is_some() {
                return Err(format!("task {i} was said stuck at {at} but runs again: {stuck:?}"));
            }
            if let Some(was) = told[i].replace((*at, stuck.clone()))
                && was.0 != *at
            {
                return Err(format!("task {i} said twice: at {} and {at}", was.0));
            }
        }
        if stuck.why == Why::Cycle {
            let edges = &stuck.cycle;
            if edges.is_empty() || edges[0].task != stuck.task {
                return Err(format!("a cycle that does not start at its task: {stuck:?}"));
            }
            for (k, edge) in edges.iter().enumerate() {
                let next = edges[(k + 1) % edges.len()];
                if edge.held_by != next.task {
                    return Err(format!("cycle edge {k} does not lead on: {stuck:?}"));
                }
                if holder(&sims[index(edge.task)]) != Some(index(edge.held_by)) {
                    return Err(format!("cycle edge {k} is not a wait of the model: {stuck:?}"));
                }
            }
        }
    }
    for i in 0..sims.len() {
        if resolves[i].is_some() {
            continue;
        }
        let due = due(sims, i);
        if due + LOOK > HORIZON {
            continue;
        }
        let Some((at, stuck)) = &told[i] else {
            return Err(format!("task {i} never runs again ({:?}) and was never said", sims[i]));
        };
        if *at > due + LOOK {
            return Err(format!("task {i} said at {at}, past its bound {due} and a look"));
        }
        if let Some(h) = holder(&sims[i]) {
            let edge = Task::Thread(h as u64);
            let me = Task::Thread(i as u64);
            let named = stuck.held_by == Some(edge)
                || stuck.cycle.iter().any(|it| it.task == me && it.held_by == edge);
            if !named {
                return Err(format!("task {i} waits on {h}, said without that edge: {stuck:?}"));
            }
        }
    }
    Ok(())
}

/// The invariant: under any mix of running, sleeping, signalled, locking and joining tasks with
/// any of the planted faults, every task that can never run again is reported within its bound
/// with the edge that explains it, no task that later makes progress by itself is reported, and
/// every reported cycle is a real one.
#[hegel::test(test_cases = 300)]
fn the_watchdog_says_exactly_the_tasks_that_never_run_again(tc: TestCase) {
    if let Err(why) = simulate(&tc, Flaw::None, Force::Nothing) {
        panic!("{why}");
    }
}

/// The control: today's guard, which counts being inside a job's code as progress, never says a
/// lost wake.
#[hegel::test(test_cases = 50)]
fn the_oracle_catches_a_watchdog_that_counts_job_code_as_progress(tc: TestCase) {
    assert!(simulate(&tc, Flaw::JobCodeIsProgress, Force::LostWake).is_err());
}

/// The control: a watchdog that flags by elapsed time alone fails a long CPU-bound task.
#[hegel::test(test_cases = 50)]
fn the_oracle_catches_a_watchdog_that_flags_by_elapsed_time(tc: TestCase) {
    assert!(simulate(&tc, Flaw::ElapsedOnly, Force::LongCpu).is_err());
}

/// The control: a watchdog that drops the holder says a lock waiter without the edge that
/// explains it.
#[hegel::test(test_cases = 50)]
fn the_oracle_catches_a_watchdog_that_drops_the_holder(tc: TestCase) {
    assert!(simulate(&tc, Flaw::DropHolder, Force::WaiterBehindLostWake).is_err());
}

/// A test's process judged by its own CPU with its own quiet bound: still for the bound, it is
/// stuck; burning CPU, never.
#[test]
fn a_quiet_test_is_stuck_at_its_own_bound_and_a_busy_one_never() {
    let quiet = 30 * S;
    let child = |pid, cpu| TaskSeen {
        task: Task::Child(pid),
        build: 7,
        span: 0,
        span_since_ns: 0,
        cpu_ns: Some(cpu),
        waits: WaitsOn::Nothing,
        bound_ns: Some(quiet),
    };
    let mut watch = Watch::new();
    let mut said = Vec::new_in(Global);
    let mut now = 0;
    while now <= 120 * S {
        for stuck in watch.look(now, &[child(1, 5), child(2, now)]) {
            said.push((now, stuck.task));
        }
        now += LOOK;
    }
    assert_eq!(said[..], [(quiet, Task::Child(1))]);
}

/// A look that finds nothing allocates nothing once the scratch has grown: the returned vector
/// has no capacity.
#[test]
fn a_look_that_finds_nothing_returns_an_unallocated_vector() {
    let seen = [TaskSeen {
        task: Task::Thread(1),
        build: 1,
        span: 1,
        span_since_ns: 0,
        cpu_ns: Some(0),
        waits: WaitsOn::Nothing,
        bound_ns: None,
    }];
    let mut watch = Watch::new();
    assert_eq!(watch.look(0, &seen).capacity(), 0);
    assert_eq!(watch.look(LOOK, &seen).capacity(), 0);
}
