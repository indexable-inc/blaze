use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

use blaze_graph::{
    Key,
    pool::Pool,
    sched::{Event, EventWait},
    topology::Topology,
    wait::{self, WaitOn},
};
use blaze_core::lock::Lock;

use super::{NoTurn, Turns};

/// How long a scenario may take: it ends in milliseconds unless it deadlocks (bounded-progress).
const DEADLINE: Duration = Duration::from_secs(20);

/// The session every call of a scenario is of.
fn session() -> Key {
    let mut key = [0u8; 32];
    key[..24].copy_from_slice(b"a test's macro session 1");
    Key(key)
}

fn named(it: &mut String) {
    it.push_str("the proc-macro session of `stateful` for `calls`");
}

/// The calls of one session on a pool, and what each did, in order, for a failure to say where
/// each stopped.
struct Scenario {
    turns: Turns,
    log: Lock<Vec<String>>,
    /// Whether a call holds the turn now: a second one given it at once is a failure.
    in_turn: AtomicBool,
    /// How many of the other calls have come to ask for the turn.
    arrived: AtomicUsize,
    others: usize,
    /// The holder's expansion, which the last of the other calls to arrive ends.
    expansion: Lock<Option<Event>>,
}

type Ended = mpsc::Sender<Result<&'static str, String>>;

impl Scenario {
    fn note(&self, what: impl Into<String>) {
        self.log.lock().push(what.into());
    }

    /// `who` takes the session's turn, runs `during` holding it, and gives it back.
    fn call(&self, who: &'static str, during: impl FnOnce()) -> Result<&'static str, String> {
        self.note(format!("{who} asks for the session's turn"));
        let turn =
            self.turns.take(session(), &mut named).map_err(|it| it.said("the test's session"))?;
        if self.in_turn.swap(true, Ordering::SeqCst) {
            return Err(format!("{who} was given the session's turn while another call held it"));
        }
        self.note(format!("{who} holds the session's turn"));
        during();
        self.in_turn.store(false, Ordering::SeqCst);
        // Noted before the turn ends, so the log has each holder's give-back before the next
        // holder's hold.
        self.note(format!("{who} gives the session's turn back"));
        drop(turn);
        Ok(who)
    }

    /// Another call of the session, on the pool: the last to arrive first ends the holder's
    /// expansion, so the holder is ready to go on just as every worker runs another call.
    fn other(self: &Arc<Scenario>, pool: &Pool, who: &'static str, ended: Ended) {
        let this = Arc::clone(self);
        pool.submit(
            None,
            move || {
                if this.arrived.fetch_add(1, Ordering::SeqCst) + 1 == this.others {
                    this.note(format!("{who} ends the holder's expansion"));
                    drop(this.expansion.lock().take());
                }
                this.call(who, || {})
            },
            move |it| _ = ended.send(it.unwrap_or_else(|_| Err(format!("{who} panicked")))),
        );
    }
}

/// On a pool of `workers` workers: the holder takes the session's turn and waits for its
/// expansion (an [`Event`], as `wasm::thread::run` waits for its expansion thread), switched
/// out. Meanwhile `workers` other calls of the session ask for the turn, the last of them to
/// arrive first ending the holder's expansion: then every worker runs another call, so the
/// holder is ready with no worker of its own. Every call gets the turn, one at a time, the
/// holder's ending first.
fn calls_of_one_session_wait_their_turn_switched_out(workers: usize) {
    let pool = Arc::new(Pool::new(&Topology::flat(workers), false).unwrap());
    let expansion = Event::new();
    let done: EventWait = expansion.waiter();
    let scenario = Arc::new(Scenario {
        turns: Turns::default(),
        log: Lock::new("macro.turn.test.log", Vec::new()),
        in_turn: AtomicBool::new(false),
        arrived: AtomicUsize::new(0),
        others: workers,
        expansion: Lock::new("macro.turn.test.expansion", Some(expansion)),
    });
    let (ended, endings) = mpsc::channel();
    {
        let (this, on, others, ended) =
            (Arc::clone(&scenario), Arc::clone(&pool), ended.clone(), ended.clone());
        pool.submit(
            None,
            move || {
                this.call("the holder", || {
                    for who in ["the first other call", "the second other call"]
                        .into_iter()
                        .take(this.others)
                    {
                        this.other(&on, who, others.clone());
                    }
                    this.note("the holder waits for its expansion, switched out");
                    let waited = wait::wait(
                        WaitOn::Channel("the test's expansion of the holder"),
                        None,
                        || done.wait(),
                    );
                    this.note(format!("the holder's expansion ended: {waited:?}"));
                })
            },
            move |it| _ = ended.send(it.unwrap_or_else(|_| Err("the holder panicked".to_owned()))),
        );
    }
    drop(ended);

    let mut ended = Vec::new();
    for _ in 0..=workers {
        let Ok(it) = endings.recv_timeout(DEADLINE) else {
            let said = scenario.log.lock().join("\n  ");
            // Its workers are blocked for good: stopping the pool would wait on them forever.
            std::mem::forget(pool);
            panic!(
                "on a pool of {workers} workers, {} of {} calls of one session ended within {DEADLINE:?}: \
                 the holder of the session's turn is ready to go on and every worker waits for that \
                 turn, holding its thread. What happened, in order:\n  {said}",
                ended.len(),
                workers + 1,
            );
        };
        let said = || scenario.log.lock().join("\n  ");
        ended
            .push(it.unwrap_or_else(|why| panic!("{why}; what happened, in order:\n  {}", said())));
    }
    // One call at a time held the turn, the holder first: each hold is followed by the same
    // call's give-back before the next call's hold.
    let log = scenario.log.lock();
    let turns: Vec<&str> = log
        .iter()
        .filter_map(|it| {
            it.strip_suffix(" holds the session's turn")
                .or_else(|| it.strip_suffix(" gives the session's turn back"))
        })
        .collect();
    let said = log.join("\n  ");
    assert_eq!(
        turns.len(),
        2 * (workers + 1),
        "every call held the turn once; what happened, in order:\n  {said}"
    );
    assert_eq!(
        turns[0], "the holder",
        "the holder's turn came first; what happened, in order:\n  {said}"
    );
    for pair in turns.chunks(2) {
        assert_eq!(
            pair[0], pair[1],
            "a call held the turn while another had it; what happened, in order:\n  {said}"
        );
    }
    drop(log);
    Arc::try_unwrap(pool).ok().expect("every job of the pool ended").stop(DEADLINE).unwrap();
}

/// A session's turn is a scheduler hold, never a lock held across a wait (rules 12 and 27):
/// the one worker asks for the turn while its holder is switched out, and the holder still
/// gets the worker back.
#[test]
fn a_call_waiting_for_a_sessions_turn_leaves_the_one_worker_to_its_holder() {
    calls_of_one_session_wait_their_turn_switched_out(1);
}

/// As above with every worker of two asking for the turn (round 2 of selfbuild4 had all 18
/// workers so, and the daemon stopped).
#[test]
fn calls_waiting_for_a_sessions_turn_leave_every_worker_to_its_holder() {
    calls_of_one_session_wait_their_turn_switched_out(2);
}

/// A call of a session asked for inside the run of another on the same job would wait on
/// itself: it is refused, naming why, never a wait.
#[test]
fn a_turn_asked_for_by_the_job_holding_it_is_refused() {
    let turns = Turns::default();
    let held = turns.take(session(), &mut named).unwrap();
    assert_eq!(turns.take(session(), &mut named).err(), Some(NoTurn::HeldByThisJob));
    let said = NoTurn::HeldByThisJob.said("the proc-macro session of `stateful` for `calls`");
    assert!(said.contains("`stateful`") && said.contains("same job"), "{said}");
    drop(held);
    drop(turns.take(session(), &mut named).unwrap());
}
