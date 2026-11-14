//! The memo under deterministic simulation (rules 43, 64): worker tasks on their own threads,
//! one running at a time, the next picked by a seeded draw at every flight and at points inside
//! the queries' bodies; a second asker of a running key blocks until it ends, and a wait that
//! would close a cycle across tasks gives way (the scheduler's rule), exactly as `Scheduled`
//! does on the pool. Between rounds the driver edits crates and makes the next snapshot.
//!
//! The invariant: in every snapshot, under any interleaving,
//! - each key of an acyclic query runs to its end at most once (single flight; nothing reruns
//!   what a snapshot holds), and each key of a cyclic one is settled once: a fixpoint member
//!   runs only on its head's task (the head keeps its flight until it settles), so the memo
//!   never meets a second entry for it, which it refuses with "ran twice";
//! - every answer, by every asker, is the recursive evaluator's for that snapshot's inputs,
//!   cycles included (`ring` answers its `cycle_result` fallback at every member, `reach` its
//!   least fixpoint);
//! - an arena whose input changed is never carried: `Memo::next` drops every changed crate
//!   and, exactly, its transitive dependents;
//! - a recorded read list is exactly the facts the evaluator reads, whether the result was run
//!   or hit;
//! - a hit makes no check (decision 18): `Counts::hit_checks` is zero, and the only checks are
//!   one per arena per `Memo::next`.
//!
//! Each `the_oracle_catches_*` test plants one bug (`Plant`) and shows the oracle refuses some
//! seed's run; a failing seed is printed and replays alone.

mod common;

use std::{
    cell::Cell,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    time::Duration,
};

use blaze_core::lock::{Lock, LockGuard, Signal};
use blaze_memo::{
    Cancelled, Config, Counts, Database, Flight, FlightKey, Flown, Gid, Hold, Kept, Memo, Plant,
    Reads, record,
    take_counts,
};
use blaze_rng::SplitMix64;

use common::{CRATES, DEPTH, Db, LEAVES, Model, RING, Runs, World, new_runs, twice};

const NONE: usize = usize::MAX;
const WORKERS: usize = 4;
const ASKS: usize = 10;
const SNAPSHOTS: usize = 4;
/// How long a worker waits for its turn before the run is called stuck.
const STUCK: Duration = Duration::from_secs(60);
/// How many times one ask gives way before the run is called stuck.
const GIVE_WAYS: u32 = 256;

thread_local! {
    static ME: Cell<usize> = const { Cell::new(NONE) };
}

struct State {
    running: usize,
    runnable: Vec<bool>,
    blocked_on: Vec<Option<FlightKey>>,
    held: Vec<(FlightKey, usize)>,
    rng: SplitMix64,
    stuck: Option<String>,
}

/// The baton: one worker runs at a time; who runs next is the seeded draw's.
struct Sim {
    state: Lock<State>,
    turn: Signal,
}

impl Sim {
    fn new(seed: u64) -> Sim {
        Sim {
            state: Lock::new(
                "blaze-memo sim state",
                State {
                    running: NONE,
                    runnable: vec![true; WORKERS],
                    blocked_on: vec![None; WORKERS],
                    held: Vec::new(),
                    rng: SplitMix64::new(seed),
                    stuck: None,
                },
            ),
            turn: Signal::new("blaze-memo sim turn"),
        }
    }

    /// Hands the baton to a runnable worker (maybe this one).
    fn pass(&self, state: &mut LockGuard<'_, State>) {
        let candidates: Vec<usize> = (0..WORKERS).filter(|w| state.runnable[*w]).collect();
        if candidates.is_empty() {
            let blocked = state.blocked_on.iter().any(Option::is_some);
            if blocked && state.stuck.is_none() {
                state.stuck = Some(format!("deadlock: every live worker waits ({:?})", state.held));
            }
            state.running = NONE;
        } else {
            let at = state.rng.index(candidates.len());
            state.running = candidates[at];
        }
        self.turn.notify_all();
    }

    fn wait_turn(&self, state: &mut LockGuard<'_, State>, me: usize) {
        let timed_out = self.turn.wait_while_for(
            state,
            |it| it.running != me && it.stuck.is_none(),
            STUCK,
        );
        assert!(!timed_out, "worker {me} waited {STUCK:?} for its turn");
        if let Some(why) = &state.stuck {
            panic!("{why}");
        }
    }

    fn yield_now(&self) {
        let me = ME.with(Cell::get);
        let mut state = self.state.lock();
        self.pass(&mut state);
        self.wait_turn(&mut state, me);
    }

    /// Every worker runnable again, nothing held, the draws from `seed`: a snapshot's round.
    fn reset(&self, seed: u64) {
        let mut state = self.state.lock();
        assert!(state.held.is_empty(), "a key held past its round: {:?}", state.held);
        state.running = NONE;
        state.runnable = vec![true; WORKERS];
        state.blocked_on = vec![None; WORKERS];
        state.rng = SplitMix64::new(seed);
        state.stuck = None;
    }

    fn start(&self) {
        let mut state = self.state.lock();
        self.pass(&mut state);
    }

    fn finish(&self, me: usize) {
        let mut state = self.state.lock();
        state.runnable[me] = false;
        self.pass(&mut state);
    }
}

/// Single flight on the baton, the scheduler's rules.
struct SimFlight(Arc<Sim>);

impl Flight for SimFlight {
    fn single<'f>(
        &'f self,
        key: FlightKey,
        _: &dyn Fn(&mut String),
        run: &mut dyn FnMut() -> Hold,
    ) -> Flown<'f> {
        let sim = &*self.0;
        sim.yield_now();
        let me = ME.with(Cell::get);
        {
            let mut state = sim.state.lock();
            let holder = state.held.iter().find(|(k, _)| *k == key).map(|(_, w)| *w);
            match holder {
                Some(w) if w == me => return Flown::Mine,
                Some(w) => {
                    // Would waiting close a cycle of waits across tasks?
                    let mut at = w;
                    loop {
                        if at == me {
                            return Flown::Cancelled;
                        }
                        let Some(next) = state.blocked_on[at] else { break };
                        at = state
                            .held
                            .iter()
                            .find(|(k, _)| *k == next)
                            .map(|(_, w)| *w)
                            .expect("a blocked worker waits for a held key");
                    }
                    state.blocked_on[me] = Some(key);
                    state.runnable[me] = false;
                    sim.pass(&mut state);
                    sim.wait_turn(&mut state, me);
                    return Flown::Waited;
                }
                None => state.held.push((key, me)),
            }
        }
        struct Release<'a>(&'a Sim, FlightKey);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                let mut state = self.0.state.lock();
                let at = state.held.iter().position(|(k, _)| *k == self.1).expect("held");
                state.held.swap_remove(at);
                for w in 0..WORKERS {
                    if state.blocked_on[w] == Some(self.1) {
                        state.blocked_on[w] = None;
                        state.runnable[w] = true;
                    }
                }
            }
        }
        let release = Release(sim, key);
        let flown = match run() {
            Hold::End => {
                drop(release);
                Flown::Ran
            }
            // Held by this worker until the member's head settles (or unwinds).
            Hold::Keep => Flown::Kept(Kept::new(release)),
        };
        sim.yield_now();
        flown
    }
}

struct SimDb<'a> {
    memo: &'a Memo,
    sim: &'a Sim,
    runs: &'a Runs,
}

impl Database for SimDb<'_> {
    fn memo(&self) -> &Memo {
        self.memo
    }
}

impl Db for SimDb<'_> {
    fn pause(&self) {
        self.sim.yield_now();
    }

    fn ran(&self, kind: u8, id: Gid, extra: u32) {
        *self.runs.lock().entry((kind, id, extra)).or_insert(0) += 1;
    }
}

#[derive(Clone, Copy, Debug)]
enum Ask {
    Value(u32, usize),
    Total(u32, u32),
    Ring(u32, u32),
    Reach(u32, u32),
    Record(u32, u32),
}

fn draw(rng: &mut SplitMix64) -> Ask {
    let c = 1 + rng.below(u64::from(CRATES)) as u32;
    match rng.below(5) {
        0 => Ask::Value(c, rng.index(LEAVES)),
        1 => Ask::Total(c, rng.below(u64::from(DEPTH) + 1) as u32),
        2 => Ask::Ring(c, rng.below(u64::from(RING)) as u32),
        3 => Ask::Reach(c, rng.below(LEAVES as u64) as u32),
        _ => Ask::Record(c, rng.below(u64::from(DEPTH) + 1) as u32),
    }
}

fn ask(db: &SimDb<'_>, world: &World, model: &Model<'_>, ask: Ask) -> Result<(), String> {
    let got_want = match ask {
        Ask::Value(c, l) => {
            let leaf = world.krate(c).leaves(db)[l];
            (common::value(db, leaf), model.value(c, l))
        }
        Ask::Total(c, d) => (common::total(db, world.krate(c), d), model.total(c, d)),
        Ask::Ring(c, at) => (common::ring(db, world.krate(c), at), model.ring(at)),
        Ask::Reach(c, n) => (common::reach(db, world.krate(c), n), model.reach(c, n)),
        Ask::Record(c, d) => {
            let (got, reads) = record(db.memo, || common::total(db, world.krate(c), d))
                .map_err(|it| it.to_string())?;
            let want = model.total_facts(c, d);
            if reads.as_slice() != want.as_slice() {
                return Err(format!(
                    "{ask:?}: the recorder listed {} facts, the evaluator reads {}",
                    reads.len(),
                    want.len()
                ));
            }
            (got, model.total(c, d))
        }
    };
    if got_want.0 != got_want.1 {
        return Err(format!("{ask:?} answered {}, the evaluator {}", got_want.0, got_want.1));
    }
    Ok(())
}

fn worker(me: usize, sim: &Sim, db: &SimDb<'_>, world: &World, model: &Model<'_>, asks: &[Ask]) -> Result<Counts, String> {
    ME.with(|it| it.set(me));
    let _ = take_counts();
    {
        let mut state = sim.state.lock();
        sim.wait_turn(&mut state, me);
    }
    for it in asks {
        let mut gave_way = 0;
        loop {
            match Cancelled::catch(|| ask(db, world, model, *it)) {
                Ok(result) => {
                    result?;
                    break;
                }
                Err(Cancelled::GaveWay) => {
                    gave_way += 1;
                    if gave_way >= GIVE_WAYS {
                        return Err(format!("{it:?} gave way {GIVE_WAYS} times"));
                    }
                    sim.yield_now();
                }
            }
        }
    }
    Ok(take_counts())
}

fn run(seed: u64, plant: Plant) -> Result<(), String> {
    let mut rng = SplitMix64::new(seed);
    let values: Vec<Vec<u64>> = (0..=CRATES)
        .map(|_| (0..LEAVES).map(|_| rng.below(8)).collect())
        .collect();
    let mut world = World::new(values);
    let mut memo: Option<Memo> = None;
    // One flight for the session, as the pool's is: a snapshot made by `Memo::next` keeps its
    // predecessor's config.
    let sim = Arc::new(Sim::new(0));
    let config = Config {
        arenas: CRATES + 1,
        reads: Reads::Kept,
        flight: Arc::new(SimFlight(Arc::clone(&sim))),
        cap_bytes: u64::MAX,
        plant,
    };
    for snapshot in 0..SNAPSHOTS {
        sim.reset(rng.next_u64());
        let mut checks = take_counts().checks;
        let current = match memo.take() {
            None => world.first(config.clone()),
            Some(old) => {
                // Edit one or two crates, then make the next snapshot from the old one.
                let mut changed: Vec<u32> = (0..1 + rng.below(2))
                    .map(|_| 1 + rng.below(u64::from(CRATES)) as u32)
                    .collect();
                changed.sort_unstable();
                changed.dedup();
                for c in &changed {
                    let l = rng.index(LEAVES);
                    world.values[*c as usize][l] += 1 + rng.below(4);
                }
                let (next, dropped) = world.next(&old, &changed);
                checks += take_counts().checks;
                if checks != u64::from(CRATES) + 1 {
                    return Err(format!(
                        "snapshot {snapshot}: {checks} checks to make it, not one per arena"
                    ));
                }
                let want = Model::closure(&changed);
                if dropped != want {
                    return Err(format!(
                        "snapshot {snapshot}: carried an arena whose input changed, or dropped one \
                         that did not: changed {changed:?} dropped {dropped:?}, the closure is {want:?}"
                    ));
                }
                drop(old);
                next
            }
        };
        let model = Model { values: &world.values };
        let runs = new_runs();
        let asks: Vec<Vec<Ask>> =
            (0..WORKERS).map(|_| (0..ASKS).map(|_| draw(&mut rng)).collect()).collect();
        let results: Vec<Result<Counts, String>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..WORKERS)
                .map(|me| {
                    let (sim, world, model, runs, current, asks) =
                        (&*sim, &world, &model, &runs, &current, &asks[me]);
                    scope.spawn(move || {
                        let db = SimDb { memo: current, sim, runs };
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            worker(me, sim, &db, world, model, asks)
                        }))
                        .unwrap_or_else(|payload| {
                            Err(payload
                                .downcast_ref::<String>()
                                .cloned()
                                .or_else(|| payload.downcast_ref::<&str>().map(|it| (*it).to_owned()))
                                .unwrap_or_else(|| "a panic".to_owned()))
                        });
                        sim.finish(me);
                        result
                    })
                })
                .collect();
            sim.start();
            handles.into_iter().map(|it| it.join().expect("a worker's panic is caught")).collect()
        });
        let mut counts = Counts::default();
        for result in results {
            counts.add(&result.map_err(|why| format!("snapshot {snapshot}: {why}"))?);
        }
        let again = twice(&runs);
        if !again.is_empty() {
            return Err(format!("snapshot {snapshot}: ran twice: {again:?}"));
        }
        if counts.hit_checks != 0 {
            return Err(format!("snapshot {snapshot}: {} checks on hits", counts.hit_checks));
        }
        memo = Some(current);
    }
    Ok(())
}

/// The invariant at the top of this file, over many seeds.
#[test]
fn every_interleaving_answers_as_the_evaluator() {
    for seed in 0..64 {
        if let Err(why) = run(seed, Plant::None) {
            panic!("seed {seed}: {why}");
        }
    }
}

fn caught(plant: Plant, says: &str) {
    let mut caught = false;
    for seed in 0..16 {
        run(seed, Plant::None).unwrap_or_else(|why| panic!("seed {seed} without a plant: {why}"));
        if let Err(why) = run(seed, plant) {
            assert!(why.contains(says), "{plant:?}, seed {seed}: {why}");
            caught = true;
        }
    }
    assert!(caught, "the oracle missed {plant:?}");
}

#[test]
fn the_oracle_catches_a_carried_changed_arena() {
    caught(Plant::CarryChanged, "carried an arena whose input changed");
}

#[test]
fn the_oracle_catches_a_carried_dependent() {
    caught(Plant::IgnoreBorrows, "carried an arena whose input changed");
}

#[test]
fn the_oracle_catches_a_hit_that_does_not_replay() {
    caught(Plant::HitSkipsReplay, "the recorder listed");
}

#[test]
fn the_oracle_catches_a_check_on_a_hit() {
    caught(Plant::HitChecks, "checks on hits");
}

/// A fixpoint member whose flight is let go when its run returns is run again by another task
/// while its head iterates; the memo refuses the second entry, naming it.
#[test]
fn the_oracle_catches_a_member_run_twice() {
    caught(Plant::MemberLetGo, "ran twice");
}
