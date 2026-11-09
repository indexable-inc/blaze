use std::time::Duration;

use blaze_ids::rng::SplitMix64 as Rng;

use super::*;

fn key(n: u8) -> InstanceKey {
    InstanceKey(ContentId([n; 32]))
}

fn shape(n: u8) -> ContentId {
    ContentId([n ^ 0x80; 32])
}

fn named(it: &mut String, owner: &str) {
    it.push_str("a test's function, which ");
    it.push_str(owner);
    it.push_str(" compiles");
}

fn owner() -> Arc<str> {
    Arc::from("a test's build")
}

const LONG: Duration = Duration::from_secs(60);

#[test]
fn the_first_to_ask_runs_and_the_rest_are_told() {
    let flights = Flights::new();
    let ran = flights.ask(key(1), &owner(), || {
        // Another build asks while this run is under way: told it is theirs, never run.
        let other = std::thread::scope(|scope| {
            scope.spawn(|| flights.ask(key(1), &owner(), || panic!("ran twice"))).join()
        });
        (Some(shape(1)), other.unwrap())
    });
    assert_eq!(ran, Asked::Ran(Asked::<()>::Theirs));
    assert_eq!(
        flights.ask(key(1), &owner(), || panic!("ran a landed function")),
        Asked::<()>::Landed(shape(1))
    );
    assert_eq!(flights.wait(key(1), named), Some(shape(1)));
    assert_eq!(flights.landed(), 1);
}

#[test]
fn a_run_that_lands_nothing_lets_the_function_go() {
    let flights = Flights::new();
    assert_eq!(flights.ask(key(1), &owner(), || (None, 7)), Asked::Ran(7));
    assert_eq!(flights.wait(key(1), named), None, "nothing under way and nothing landed");
    assert_eq!(
        flights.ask(key(1), &owner(), || (Some(shape(1)), 8)),
        Asked::Ran(8),
        "the next to ask runs it"
    );
    assert_eq!(flights.landed(), 1);
}

#[test]
fn a_landing_outlives_no_template_the_memo_drops() {
    let mut flights = Flights::new();
    flights.ask(key(1), &owner(), || (Some(shape(1)), ()));
    flights.ask(key(2), &owner(), || (Some(shape(2)), ()));
    flights.retain(|it| *it == shape(2));
    assert_eq!(flights.ask(key(1), &owner(), || (None, ())), Asked::Ran(()));
    assert_eq!(flights.ask(key(2), &owner(), || (None, ())), Asked::Landed(shape(2)));
}

fn summary(n: u8) -> Summary {
    Summary {
        key: key(n),
        size: u32::from(n),
        hint: crate::summary::Hint::None,
        calls: Vec::new(),
        addressed: Vec::new(),
        params: 0,
        unused: Vec::new(),
        attrs: crate::summary::Attrs::default(),
        body: crate::summary::Attrs::default(),
        external: true,
    }
}

/// A second summary under one flight key is one when equal, and refused when other: the
/// kept one stays (rule 21).
#[test]
fn another_summary_under_a_kept_key_is_refused() {
    let flights = Flights::new();
    assert!(flights.keep_summary(key(1), summary(1)));
    assert!(flights.keep_summary(key(1), summary(1)), "an equal summary is one");
    assert_eq!(flights.keep_summaries(vec![(key(1), summary(2)), (key(2), summary(2))]), [key(1)]);
    assert_eq!(flights.summaries(&[key(1), key(2)]), [Some(summary(1)), Some(summary(2))]);
}

/// A summary kept and landed is every later asker's, and goes with its landing.
#[test]
fn a_landed_summary_is_taken_and_collected_with_its_landing() {
    let mut flights = Flights::new();
    for n in [1, 2] {
        let ran = flights.ask(key(n), &owner(), || {
            assert!(flights.keep_summary(key(n), summary(n)));
            (Some(key(n).0), ())
        });
        assert_eq!(ran, Asked::Ran(()));
    }
    assert_eq!(
        flights.ask(key(1), &owner(), || panic!("ran a landed summary")),
        Asked::<()>::Landed(key(1).0)
    );
    assert_eq!(flights.summaries(&[key(1), key(3)]), [Some(summary(1)), None]);
    flights.retain(|it| *it == key(2).0);
    assert_eq!(flights.summaries(&[key(1), key(2)]), [None, Some(summary(2))]);
}

/// A waiter waits for the one function it needs, and wakes as that one lands, while the
/// build that compiles it is still compiling others; a run that lands nothing wakes its
/// waiter with nothing, to ask again.
#[test]
fn a_waiter_wakes_for_its_function_alone() {
    let flights = &Flights::new();
    let (asked, told) = crossbeam_channel::unbounded();
    let (landed, heard) = crossbeam_channel::unbounded();
    std::thread::scope(|scope| {
        let compiler = scope.spawn(move || {
            flights.ask(key(1), &owner(), || {
                told.recv_timeout(LONG).expect("the waiter asks");
                (Some(shape(1)), ())
            });
            // The owner's next function, under way until the waiter has its landing.
            flights.ask(key(2), &owner(), || {
                heard.recv_timeout(LONG).expect("the waiter wakes while the owner still compiles");
                (None, ())
            });
        });
        let waiter = scope.spawn(move || {
            blaze_graph::spin::SpinWait::within("the owner to run", LONG, || {
                flights.runs.running() == 1
            });
            assert_eq!(
                flights.ask(key(1), &owner(), || panic!("ran a function under way")),
                Asked::<()>::Theirs
            );
            asked.send(()).unwrap();
            let got = flights.wait(key(1), named);
            landed.send(()).unwrap();
            assert_eq!(got, Some(shape(1)));
            blaze_graph::spin::SpinWait::within("the owner's second run to end", LONG, || {
                flights.runs.running() == 0
            });
            assert_eq!(flights.wait(key(2), named), None, "let go of, unlanded");
        });
        waiter.join().unwrap();
        compiler.join().unwrap();
    });
}

/// On a pool of one worker, a build waits for a function another build compiles while that
/// build, a job parked inside the compile, is woken: the landing can run only on the worker
/// the wait is on, so the wait ends only if it parks its job rather than its thread (with the
/// thread held, as by a lock's condition, it never ends).
#[test]
fn a_build_waiting_for_another_builds_function_leaves_its_thread_to_that_compile() {
    let flights = std::sync::Arc::new(Flights::new());
    let pool =
        blaze_graph::pool::Pool::new(&blaze_graph::topology::Topology::flat(1), false).unwrap();
    let go = blaze_graph::sched::Event::new();
    let on = go.waiter();
    let (running_tx, running) = crossbeam_channel::unbounded::<()>();
    let (owned_tx, owned) = crossbeam_channel::unbounded::<Result<Asked<()>, String>>();
    {
        let flights = std::sync::Arc::clone(&flights);
        pool.submit(
            None,
            move || {
                flights.ask(key(1), &owner(), || {
                    running_tx.send(()).unwrap();
                    on.wait().expect("an event's wait closes no cycle");
                    (Some(shape(1)), ())
                })
            },
            move |it| _ = owned_tx.send(it.map_err(|_| "the owner's build panicked".to_owned())),
        );
    }
    running.recv_timeout(LONG).expect("the owner runs its function");
    let (asking_tx, asking) = crossbeam_channel::unbounded::<()>();
    let (waited_tx, waited) = crossbeam_channel::unbounded();
    {
        let flights = std::sync::Arc::clone(&flights);
        pool.submit(
            None,
            move || {
                assert_eq!(
                    flights.ask(key(1), &owner(), || panic!("ran a function under way")),
                    Asked::<()>::Theirs
                );
                asking_tx.send(()).unwrap();
                flights.wait(key(1), named)
            },
            move |it| _ = waited_tx.send(it.map_err(|_| "the waiting build panicked".to_owned())),
        );
    }
    asking.recv_timeout(LONG).expect("the waiter runs once the owner parks");
    go.fire();
    let waited = waited
        .recv_timeout(LONG)
        .expect("the waiter parks its job, so the owner lands on the one worker");
    assert_eq!(waited, Ok(Some(shape(1))));
    assert_eq!(owned.recv_timeout(LONG).expect("the owner's build ends"), Ok(Asked::Ran(())));
}

/// One build's loop over its functions as the driver runs it: each asked for, each another
/// build runs waited for once its own ran, each let go of asked for again.
fn build_of(
    flights: &Flights,
    keys: &[InstanceKey],
    compile: impl Fn(InstanceKey) -> Option<ContentId>,
) -> Vec<Decision> {
    let mut plan = Plan::new(keys.len());
    let mut seen = Vec::new();
    loop {
        let decisions = plan.take_decisions();
        if decisions.is_empty() {
            break;
        }
        for decision in decisions {
            seen.push(decision);
            match decision {
                Decision::Ask(at) => {
                    let found = match flights.ask(keys[at], &owner(), || (compile(keys[at]), ())) {
                        Asked::Ran(()) => Found::Ran,
                        Asked::Theirs => Found::Theirs,
                        Asked::Landed(shape) => Found::Landed(shape),
                    };
                    plan.asked(at, found);
                }
                Decision::Wait(at) => plan.waited(at, flights.wait(keys[at], named)),
                Decision::Take(..) | Decision::GiveUp { .. } => {}
            }
        }
    }
    assert!(plan.done());
    seen
}

/// Builds that each run a function the other then reaches: each runs or waits for one
/// function at a time and holds none while it waits, so both end, every function compiled
/// once between them.
#[test]
fn builds_reaching_each_others_functions_both_end_and_compile_each_once() {
    let flights = Flights::new();
    let compiled = blaze_core::lock::Lock::new("test.codegen.flight.compiled", Vec::new());
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        for order in [[1u8, 2, 3], [3, 2, 1]] {
            let (flights, compiled, barrier) = (&flights, &compiled, &barrier);
            scope.spawn(move || {
                let keys: Vec<InstanceKey> = order.iter().map(|it| key(*it)).collect();
                barrier.wait();
                build_of(flights, &keys, |key| {
                    compiled.lock().push(key);
                    blaze_graph::spin::busy(Duration::from_millis(5));
                    Some(shape(key.0.0[0]))
                });
            });
        }
    });
    let mut compiled = compiled.into_inner();
    compiled.sort();
    assert_eq!(compiled, [key(1), key(2), key(3)]);
    assert_eq!(flights.landed(), 3);
}

/// What the driver tells a plan, replayed: the decisions it hands back for each.
enum Told {
    Asked(usize, Found),
    Waited(usize, Option<ContentId>),
}

/// A build of three functions, replayed from a transcript of what its asks and waits found:
/// one it runs, one landed, and one another build runs, lets go of, and runs again.
#[test]
fn a_plan_replays_a_transcript_of_asks_and_waits() {
    use Decision::*;
    let transcript: &[(&[Told], &[Decision])] = &[
        (&[], &[Ask(0), Ask(1), Ask(2)]),
        (
            &[
                Told::Asked(0, Found::Ran),
                Told::Asked(1, Found::Theirs),
                Told::Asked(2, Found::Landed(shape(2))),
            ],
            &[Wait(1), Take(2, shape(2))],
        ),
        (&[Told::Waited(1, None)], &[Ask(1)]),
        (&[Told::Asked(1, Found::Theirs)], &[Wait(1)]),
        (&[Told::Waited(1, Some(shape(1)))], &[Take(1, shape(1))]),
        (&[], &[]),
    ];
    let mut plan = Plan::new(3);
    for (step, (told, decided)) in transcript.iter().enumerate() {
        for it in *told {
            match *it {
                Told::Asked(at, found) => plan.asked(at, found),
                Told::Waited(at, landing) => plan.waited(at, landing),
            }
        }
        assert_eq!(plan.take_decisions(), *decided, "step {step}");
    }
    assert!(plan.done());
}

/// A function every other build lets go of is given up after [`ASKS`] asks, an error the
/// driver names it by, never asked for without end (rule 18).
#[test]
fn a_function_let_go_of_without_end_is_given_up() {
    let mut plan = Plan::new(1);
    for asks in 1..=ASKS {
        assert_eq!(plan.take_decisions(), [Decision::Ask(0)]);
        plan.asked(0, Found::Theirs);
        assert_eq!(plan.take_decisions(), [Decision::Wait(0)]);
        plan.waited(0, None);
        if asks == ASKS {
            assert_eq!(plan.take_decisions(), [Decision::GiveUp { at: 0, asks }]);
        }
    }
    assert!(plan.done());
}

/// Builds of one set of functions, each its own plan, stepped in a random order against a
/// model of the flights (a run takes steps, and some compiles fail): every build ends with
/// each function done, and no function is under way twice at once or lands twice.
#[test]
fn plans_of_builds_interleaved_at_random_end_with_one_landing_each() {
    const KEYS: usize = 12;
    const BUILDS: usize = 5;
    for seed in 0..200u64 {
        let mut rng = Rng::new(seed);
        // Of each key: whether its compile fails (in every build: a compile error), its
        // landing, and the build whose run of it is under way.
        let fails: Vec<bool> = (0..KEYS).map(|_| rng.below(5) == 0).collect();
        let mut landed: Vec<Option<ContentId>> = vec![None; KEYS];
        let mut running: Vec<Option<usize>> = vec![None; KEYS];
        let mut plans: Vec<Plan> = (0..BUILDS).map(|_| Plan::new(KEYS)).collect();
        // Of each build: its decisions to go, and the run or wait it is in.
        let mut queues: Vec<std::collections::VecDeque<Decision>> =
            vec![Default::default(); BUILDS];
        let mut busy: Vec<Option<Decision>> = vec![None; BUILDS];
        let mut runs = vec![0usize; KEYS];
        for step in 0.. {
            assert!(step < 100_000, "seed {seed}: the builds did not end");
            for (plan, queue) in plans.iter_mut().zip(&mut queues) {
                queue.extend(plan.take_decisions());
            }
            let live: Vec<usize> =
                (0..BUILDS).filter(|it| busy[*it].is_some() || !queues[*it].is_empty()).collect();
            if live.is_empty() {
                break;
            }
            let build = live[rng.below(live.len() as u64) as usize];
            match busy[build].take() {
                // A run ends: lands, or lets go.
                Some(Decision::Ask(at)) => {
                    assert_eq!(running[at], Some(build));
                    running[at] = None;
                    if !fails[at] {
                        assert_eq!(landed[at], None, "seed {seed}: key {at} landed twice");
                        landed[at] = Some(shape(at as u8));
                    }
                    plans[build].asked(at, Found::Ran);
                }
                // A wait ends once nothing runs the key.
                Some(Decision::Wait(at)) if running[at].is_none() => {
                    plans[build].waited(at, landed[at])
                }
                Some(waiting) => busy[build] = Some(waiting),
                None => match queues[build].pop_front().expect("a live build has a decision") {
                    Decision::Ask(at) => match (landed[at], running[at]) {
                        (Some(shape), _) => plans[build].asked(at, Found::Landed(shape)),
                        (None, Some(other)) => {
                            assert_ne!(
                                other, build,
                                "seed {seed}: a build asked for a key it runs"
                            );
                            plans[build].asked(at, Found::Theirs);
                        }
                        (None, None) => {
                            running[at] = Some(build);
                            runs[at] += 1;
                            busy[build] = Some(Decision::Ask(at));
                        }
                    },
                    wait @ Decision::Wait(_) => busy[build] = Some(wait),
                    Decision::Take(at, shape) => assert_eq!(landed[at], Some(shape)),
                    Decision::GiveUp { at, .. } => panic!("seed {seed}: key {at} given up"),
                },
            }
        }
        assert!(plans.iter().all(Plan::done), "seed {seed}");
        for at in 0..KEYS {
            if fails[at] {
                assert!(
                    runs[at] >= 1 && runs[at] <= BUILDS,
                    "seed {seed}: key {at} failing ran {} times",
                    runs[at]
                );
            } else {
                assert_eq!(runs[at], 1, "seed {seed}: key {at} compiled {} times", runs[at]);
            }
        }
    }
}
