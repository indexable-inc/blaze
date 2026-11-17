//! The memo under deterministic simulation (rules 43, 64): worker tasks on their own threads,
//! one running at a time, the next picked by a draw from the case's tape at every flight and at
//! points inside the queries' bodies (drawn only by the worker holding the baton, or by the
//! driver while no worker runs, so the tape's choices come in one order); a second asker of a running key blocks until it ends, and a wait that
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
//!   one per arena per `Memo::next`;
//! - a key held by one task is never taken for another task's own: of two tasks asking one key,
//!   one runs it, the other gets that run's value, and neither is told it is in a cycle
//!   (`two_tasks_ask_one_key`).
//!
//! Each `the_oracle_catches_*` test plants one bug (`Plant`) and shows the oracle refuses some
//! case's run; a failing case's tape is printed and replays alone.

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
use blaze_sim::{Broke, Tape};

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
    /// How many times this worker paused inside a query body.
    static PAUSES: Cell<u32> = const { Cell::new(0) };
}

/// A panic to inject in an ordinary run: worker `worker` panics at its `pause`-th pause. The
/// control that a worker's panic is the run's verdict, never a hang or a give-way bound.
#[derive(Clone, Copy, Debug)]
struct PanicAt {
    worker: usize,
    pause: u32,
}

const INJECTED: &str = "an injected panic in a query body";

struct State {
    running: usize,
    runnable: Vec<bool>,
    blocked_on: Vec<Option<FlightKey>>,
    held: Vec<(FlightKey, usize)>,
    /// The case's tape: who runs next is drawn from it, under this lock, by the baton's holder
    /// or by the driver while no worker runs.
    tape: Tape,
    stuck: Option<String>,
    /// The first panic of a worker this round: the round's verdict.
    panicked: Option<String>,
}

/// The baton: one worker runs at a time; who runs next is the tape's draw.
struct Sim {
    state: Lock<State>,
    turn: Signal,
}

impl Sim {
    fn new(tape: &Tape) -> Sim {
        Sim {
            state: Lock::new(
                "blaze-memo sim state",
                State {
                    running: NONE,
                    runnable: vec![true; WORKERS],
                    blocked_on: vec![None; WORKERS],
                    held: Vec::new(),
                    tape: tape.clone(),
                    stuck: None,
                    panicked: None,
                },
            ),
            turn: Signal::new("blaze-memo sim turn"),
        }
    }

    /// Hands the baton to a runnable worker (maybe this one). Called only by the baton's
    /// holder, or by the driver while no worker runs, or once the run is stopped; a stopped run
    /// draws nothing, since the workers that wake to stop call this in the kernel's order.
    fn pass(&self, state: &mut LockGuard<'_, State>) {
        let candidates: Vec<usize> = (0..WORKERS).filter(|w| state.runnable[*w]).collect();
        if state.stuck.is_some() {
            state.running = NONE;
        } else if candidates.is_empty() {
            let blocked = state.blocked_on.iter().any(Option::is_some);
            if blocked {
                state.stuck = Some(format!("deadlock: every live worker waits ({:?})", state.held));
            }
            state.running = NONE;
        } else {
            let at = state.tape.index(candidates.len(), "next thread");
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

    /// Every worker runnable again, nothing held: a snapshot's round, its schedule drawn on from
    /// the same tape.
    fn reset(&self) {
        let mut state = self.state.lock();
        assert!(state.held.is_empty(), "a key held past its round: {:?}", state.held);
        state.running = NONE;
        state.runnable = vec![true; WORKERS];
        state.blocked_on = vec![None; WORKERS];
        state.stuck = None;
        state.panicked = None;
    }

    fn start(&self) {
        let mut state = self.state.lock();
        self.pass(&mut state);
    }

    /// A worker panicked with `why`: the simulation ends with it as the verdict (the first
    /// panic stands, and outranks any hang or give-way bound another worker reports, before or
    /// after). Every worker waiting for its turn wakes and stops.
    fn panicked(&self, why: String) {
        let mut state = self.state.lock();
        if state.panicked.is_none() {
            state.panicked = Some(why.clone());
        }
        if state.stuck.is_none() {
            state.stuck = Some(format!("stopped: {why}"));
        }
        self.turn.notify_all();
    }

    /// The first worker's panic of this round, if any.
    fn panic_verdict(&self) -> Option<String> {
        self.state.lock().panicked.clone()
    }

    fn finish(&self, me: usize) {
        let mut state = self.state.lock();
        state.runnable[me] = false;
        self.pass(&mut state);
    }
}

/// Whose key the simulation's flight says a held key is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Whose {
    /// Its holder's: `Mine` to the holder alone, a wait (or a give-way) to every other task.
    Holder,
    /// The planted bug, `Alone` as it was: any key held is the asker's own (`Mine`), whoever
    /// holds it.
    AnyHeldIsMine,
}

/// Single flight on the baton, the scheduler's rules.
struct SimFlight {
    sim: Arc<Sim>,
    whose: Whose,
}

impl Flight for SimFlight {
    fn single<'f>(
        &'f self,
        key: FlightKey,
        _: &dyn Fn(&mut dyn std::fmt::Write),
        run: &mut dyn FnMut() -> Hold,
    ) -> Flown<'f> {
        let sim = &*self.sim;
        sim.yield_now();
        let me = ME.with(Cell::get);
        {
            let mut state = sim.state.lock();
            let holder = state.held.iter().find(|(k, _)| *k == key).map(|(_, w)| *w);
            match holder {
                Some(w) if w == me => return Flown::Mine,
                Some(_) if self.whose == Whose::AnyHeldIsMine => return Flown::Mine,
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
    panic_at: Option<PanicAt>,
}

impl Database for SimDb<'_> {
    fn memo(&self) -> &Memo {
        self.memo
    }
}

impl Db for SimDb<'_> {
    fn pause(&self) {
        let pauses = PAUSES.with(|it| {
            it.set(it.get() + 1);
            it.get()
        });
        if let Some(at) = self.panic_at
            && at.worker == ME.with(Cell::get)
            && at.pause == pauses
        {
            panic!("{INJECTED}");
        }
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

fn draw(tape: &Tape) -> Ask {
    let c = 1 + tape.below(u64::from(CRATES), "ask crate") as u32;
    match tape.below(5, "ask") {
        0 => Ask::Value(c, tape.index(LEAVES, "leaf")),
        1 => Ask::Total(c, tape.below(u64::from(DEPTH) + 1, "total depth") as u32),
        2 => Ask::Ring(c, tape.below(u64::from(RING), "ring member") as u32),
        3 => Ask::Reach(c, tape.below(LEAVES as u64, "reach from") as u32),
        _ => Ask::Record(c, tape.below(u64::from(DEPTH) + 1, "record depth") as u32),
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
            let (got, rows) = record(db.memo, || common::total(db, world.krate(c), d))
                .map_err(|it| it.to_string())?;
            let reads = common::facts_of(&rows);
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
                    // A peer's panic can leave this ask nothing to wait for but a run that
                    // never comes: that panic is the verdict, not this ask's bound.
                    if let Some(why) = sim.panic_verdict() {
                        return Err(format!("stopped: {why}"));
                    }
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

/// One round on snapshot `current`: worker `me` asks `asks[me]` in order, all of them under the
/// baton, from a fresh schedule drawn on from the case's tape. The workers' counts summed, or
/// the round's verdict: a worker's panic, whatever the others reported after it (a give-way
/// bound, a stop, a hang), else the first worker's error.
fn round(
    sim: &Sim,
    current: &Memo,
    world: &World,
    model: &Model<'_>,
    runs: &Runs,
    asks: &[Vec<Ask>],
    panic_at: Option<PanicAt>,
) -> Result<Counts, String> {
    let results: Vec<Result<Counts, String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..WORKERS)
            .map(|me| {
                let asks = &asks[me];
                scope.spawn(move || {
                    let db = SimDb { memo: current, sim, runs, panic_at };
                    PAUSES.with(|it| it.set(0));
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        worker(me, sim, &db, world, model, asks)
                    }))
                    .unwrap_or_else(|payload| {
                        let why = payload
                            .downcast_ref::<String>()
                            .cloned()
                            .or_else(|| payload.downcast_ref::<&str>().map(|it| (*it).to_owned()))
                            .unwrap_or_else(|| "a panic with no message".to_owned());
                        // Set while this worker still holds the baton: no peer runs between
                        // the panic and its verdict.
                        sim.panicked(format!("worker {me} panicked: {why}"));
                        Err(why)
                    });
                    sim.finish(me);
                    result
                })
            })
            .collect();
        sim.start();
        handles.into_iter().map(|it| it.join().expect("a worker's panic is caught")).collect()
    });
    if let Some(why) = sim.panic_verdict() {
        return Err(why);
    }
    let mut counts = Counts::default();
    for result in results {
        counts.add(&result?);
    }
    Ok(counts)
}

fn run(tape: &Tape, plant: Plant) -> Result<(), String> {
    run_with(tape, plant, None)
}

/// Two tasks ask one key (the case's crate's deepest `total`, which reads every leaf and its
/// dependencies' totals, so the ask spans many flights and pauses); the other workers ask
/// nothing. The oracle: both answers are the evaluator's, the key and everything under it ran
/// to its end once, and neither task was told it is in a cycle (on `Mine` with no run of the
/// key on its stack, `reenter` panics, and a worker's panic is the verdict). Under the plant
/// (`Whose::AnyHeldIsMine`, `Alone` as it was) the second asker is told the first one's run is
/// its own.
fn two_tasks_ask_one_key(tape: &Tape, whose: Whose) -> Result<(), String> {
    let values: Vec<Vec<u64>> = (0..=CRATES)
        .map(|_| (0..LEAVES).map(|_| tape.below(8, "leaf value")).collect())
        .collect();
    let mut world = World::new(values);
    let sim = Arc::new(Sim::new(tape));
    let config = Config {
        arenas: CRATES + 1,
        reads: Reads::Kept,
        flight: Arc::new(SimFlight { sim: Arc::clone(&sim), whose }),
        cap_bytes: u64::MAX,
        plant: Plant::None,
    };
    sim.reset();
    let current = world.first(config);
    let model = Model { values: &world.values };
    let runs = new_runs();
    let c = 1 + tape.below(u64::from(CRATES), "asked crate") as u32;
    let one = Ask::Total(c, DEPTH);
    let mut asks: Vec<Vec<Ask>> = (0..WORKERS).map(|_| Vec::new()).collect();
    asks[0].push(one);
    asks[1].push(one);
    round(&sim, &current, &world, &model, &runs, &asks, None)?;
    let again = twice(&runs);
    if !again.is_empty() {
        return Err(format!("{one:?} by two tasks ran twice: {again:?}"));
    }
    let gid = blaze_memo::Id::gid(world.krate(c));
    let ran = runs.lock().get(&(common::TOTAL, gid, DEPTH)).copied().unwrap_or(0);
    if ran != 1 {
        return Err(format!("{one:?} by two tasks ran {ran} times, not once"));
    }
    Ok(())
}

/// The invariant of [`two_tasks_ask_one_key`], under every interleaving the cases draw.
#[test]
fn two_tasks_ask_one_key_and_it_runs_once() {
    blaze_sim::check(blaze_sim::Config::cases(64), |tape| two_tasks_ask_one_key(tape, Whose::Holder));
}

/// The control: a flight that says any held key is the asker's own (`Alone` before it named
/// its holder) tells the second asker it is in a cycle with the first one's run.
#[test]
fn the_oracle_catches_a_held_key_taken_for_the_askers_own() {
    blaze_sim::control(
        blaze_sim::Config::cases(64),
        Whose::Holder,
        Whose::AnyHeldIsMine,
        "the flight says this task runs",
        |tape, whose| two_tasks_ask_one_key(tape, whose).map_err(Broke::Oracle),
    );
}

fn run_with(tape: &Tape, plant: Plant, panic_at: Option<PanicAt>) -> Result<(), String> {
    let values: Vec<Vec<u64>> = (0..=CRATES)
        .map(|_| (0..LEAVES).map(|_| tape.below(8, "leaf value")).collect())
        .collect();
    let mut world = World::new(values);
    let mut memo: Option<Memo> = None;
    // One flight for the session, as the pool's is: a snapshot made by `Memo::next` keeps its
    // predecessor's config.
    let sim = Arc::new(Sim::new(tape));
    let config = Config {
        arenas: CRATES + 1,
        reads: Reads::Kept,
        flight: Arc::new(SimFlight { sim: Arc::clone(&sim), whose: Whose::Holder }),
        cap_bytes: u64::MAX,
        plant,
    };
    for snapshot in 0..SNAPSHOTS {
        sim.reset();
        let mut checks = take_counts().checks;
        let current = match memo.take() {
            None => world.first(config.clone()),
            Some(old) => {
                // Edit one or two crates, then make the next snapshot from the old one.
                let mut changed: Vec<u32> = (0..1 + tape.below(2, "crates edited"))
                    .map(|_| 1 + tape.below(u64::from(CRATES), "edited crate") as u32)
                    .collect();
                changed.sort_unstable();
                changed.dedup();
                for c in &changed {
                    let l = tape.index(LEAVES, "edited leaf");
                    world.values[*c as usize][l] += 1 + tape.below(4, "edit step");
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
            (0..WORKERS).map(|_| (0..ASKS).map(|_| draw(tape)).collect()).collect();
        let counts = round(&sim, &current, &world, &model, &runs, &asks, panic_at)
            .map_err(|why| format!("snapshot {snapshot}: {why}"))?;
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

/// The invariant at the top of this file, over many cases.
#[test]
fn every_interleaving_answers_as_the_evaluator() {
    blaze_sim::check(blaze_sim::Config::cases(64), |tape| run(tape, Plant::None));
}

/// Without `plant` every case passes; with it some case fails, saying `says`.
fn caught(plant: Plant, says: &str) {
    blaze_sim::control(blaze_sim::Config::cases(16), Plant::None, plant, says, |tape, plant| {
        run(tape, plant).map_err(Broke::Oracle)
    });
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
/// while its head iterates; the memo refuses the second entry, naming it, and that worker's
/// panic is the verdict.
#[test]
fn the_oracle_catches_a_member_run_twice() {
    caught(Plant::MemberLetGo, "ran twice in one snapshot");
}

/// Without a plant, a panic in a query body on one worker fails the run with its own message,
/// never as a peer that gave way until its bound or a hang: the propagation the planted
/// controls rely on holds for real failures too.
#[test]
fn a_worker_panic_is_the_verdict() {
    let mut seen = 0;
    blaze_sim::check(blaze_sim::Config::cases(16), |tape| -> Result<(), String> {
        let pause = tape.int_in(1u32..=3, "panic at pause");
        for worker in 0..WORKERS {
            let at = PanicAt { worker, pause };
            match run_with(tape, Plant::None, Some(at)) {
                Ok(()) => {}
                Err(why) => {
                    if !why.contains(&format!("worker {worker} panicked: {INJECTED}")) {
                        return Err(format!("{at:?}: the verdict is not the injected panic: {why}"));
                    }
                    seen += 1;
                }
            }
        }
        Ok(())
    });
    assert!(seen > 0, "no injected panic was reached");
}

/// How a reader of a table kept across `Memo::next` treats each id it finds there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reader {
    /// Through `Memo::live` first: a dead id is a typed answer, never a lookup.
    Live,
    /// The planted bug: looks every id up in its snapshot, as `claim_keeps` did before
    /// 2026-11-15 (the keep of a later snapshot panicked on a trace of a dropped arena).
    Blind,
}

struct PlainDb<'a> {
    memo: &'a Memo,
}

impl Database for PlainDb<'_> {
    fn memo(&self) -> &Memo {
        self.memo
    }
}

impl Db for PlainDb<'_> {
    fn pause(&self) {}

    fn ran(&self, _: u8, _: Gid, _: u32) {}
}

/// A snapshot still answering: the memo, each crate's leaf values and ids as it was made.
struct Answering {
    memo: Memo,
    values: Vec<Vec<u64>>,
    /// Crates `1..=CRATES`, crate `c` at `c - 1`.
    crates: Vec<common::Crate>,
}

impl Answering {
    fn of(memo: Memo, world: &World) -> Answering {
        let crates = (1..=CRATES).map(|c| world.krate(c)).collect();
        Answering { memo, values: world.values.clone(), crates }
    }

    fn crate_of(&self, c: u32) -> common::Crate {
        self.crates[c as usize - 1]
    }
}

/// The daemon's shape in miniature: a table of ids (the item store's traces) filled by runs of
/// every snapshot still answering, while commits replace arenas and older snapshots go on
/// answering beside the newest; readers of any of them claim from the table between commits.
/// The order of runs, commits, claims and the snapshot each uses is drawn from the tape.
///
/// The invariant: a reader looks up only ids its snapshot holds at their generation, and each
/// such lookup answers that snapshot's value; an id it refuses names an arena its snapshot
/// holds no making of that generation of; a refused id of a superseded making leaves the table
/// (a superseded making's id is held by no later snapshot), and no id a reader's snapshot
/// holds is ever refused.
fn carried_reader(tape: &Tape, reader: Reader) -> Result<(), String> {
    let values: Vec<Vec<u64>> =
        (0..=CRATES).map(|_| (0..LEAVES).map(|_| tape.below(8, "leaf value")).collect()).collect();
    let mut world = World::new(values);
    let config = Config {
        arenas: CRATES + 1,
        reads: Reads::Kept,
        flight: Arc::new(blaze_memo::Alone::new()),
        cap_bytes: u64::MAX,
        plant: Plant::None,
    };
    let first = world.first(config);
    let mut answering: Vec<Answering> = vec![Answering::of(first, &world)];
    // The table: each id with its crate and leaf, as a run put it there.
    let mut table: Vec<(common::Leaf, u32, usize)> = Vec::new();
    for step in 0..24 {
        let at = tape.index(answering.len(), "snapshot");
        match tape.below(4, "op") {
            // A commit: edit a crate, make the next snapshot from the newest; the oldest
            // stops answering past three.
            0 => {
                let c = 1 + tape.below(u64::from(CRATES), "edited crate") as u32;
                let l = tape.index(LEAVES, "edited leaf");
                world.values[c as usize][l] += 1 + tape.below(4, "edit step");
                let newest = &answering.last().expect("one answers").memo;
                let (next, _) = world.next(newest, &[c]);
                answering.push(Answering::of(next, &world));
                if answering.len() > 3 {
                    answering.remove(0);
                }
            }
            // A run in snapshot `at`: its leaf's id goes into the table.
            1 | 2 => {
                let it = &answering[at];
                let db = PlainDb { memo: &it.memo };
                let c = 1 + tape.below(u64::from(CRATES), "run crate") as u32;
                let l = tape.index(LEAVES, "run leaf");
                let leaf = it.crate_of(c).leaves(&db)[l];
                let got = common::value(&db, leaf);
                if got != it.values[c as usize][l] {
                    return Err(format!("step {step}: a run answered {got}, not {}", it.values[c as usize][l]));
                }
                table.push((leaf, c, l));
            }
            // A claim by a reader of snapshot `at`.
            _ => {
                let it = &answering[at];
                let db = PlainDb { memo: &it.memo };
                let mut left = Vec::with_capacity(table.len());
                for &(leaf, c, l) in &table {
                    let held = blaze_memo::Id::gid(it.crate_of(c)).arena();
                    let named = blaze_memo::Id::gid(leaf).arena();
                    let id = match reader {
                        Reader::Live => match it.memo.live(leaf) {
                            Ok(live) => {
                                if named != held {
                                    return Err(format!(
                                        "step {step}: a live proof of {named:?}, which the snapshot holds as {held:?}"
                                    ));
                                }
                                live.get()
                            }
                            Err(dead) => {
                                if named == held {
                                    return Err(format!("step {step}: refused {named:?}, which the snapshot holds"));
                                }
                                if dead.named != named || dead.held != Some(held) {
                                    return Err(format!(
                                        "step {step}: refused {named:?} as {dead:?}, but the snapshot holds {held:?}"
                                    ));
                                }
                                if !dead.superseded() {
                                    left.push((leaf, c, l));
                                }
                                continue;
                            }
                        },
                        Reader::Blind => leaf,
                    };
                    let looked = catch_unwind(AssertUnwindSafe(|| common::value(&db, id)));
                    match looked {
                        Ok(got) if got == it.values[c as usize][l] => left.push((leaf, c, l)),
                        Ok(got) => {
                            return Err(format!(
                                "step {step}: {id:?} answered {got}, not its snapshot's {}",
                                it.values[c as usize][l]
                            ));
                        }
                        Err(payload) => {
                            let why = payload
                                .downcast_ref::<String>()
                                .cloned()
                                .or_else(|| payload.downcast_ref::<&str>().map(|it| (*it).to_owned()))
                                .unwrap_or_default();
                            return Err(format!("step {step}: a reader looked up an id its snapshot does not hold: {why}"));
                        }
                    }
                }
                table = left;
            }
        }
    }
    Ok(())
}

#[test]
fn a_reader_of_a_carried_table_looks_up_only_live_ids() {
    blaze_sim::check(blaze_sim::Config::cases(64), |tape| carried_reader(tape, Reader::Live));
}

/// The control: a reader that looks every id up ([`Reader::Blind`]) meets an id of an arena a
/// commit replaced, and the oracle names it.
#[test]
fn the_oracle_catches_a_reader_that_looks_up_a_dead_id() {
    blaze_sim::control(
        blaze_sim::Config::cases(64),
        Reader::Live,
        Reader::Blind,
        "a reader looked up an id its snapshot does not hold",
        |tape, reader| carried_reader(tape, reader).map_err(Broke::Oracle),
    );
}
