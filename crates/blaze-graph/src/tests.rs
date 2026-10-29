use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

use blaze_protocol::events::{Event, NodeKind, NodeState, Record};
use expect_test::{expect, expect_file};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    exec::{Did, drive},
    machine::{Config, Error, Input, Machine, NodeId, Outcome, Output, Pool, Spec},
    trace::{Trace, render},
};

const ONE: Config = Config { io: 1, work: 1, keep_going: false };

fn spec(id: u32, kind: NodeKind, deps: &[u32], pool: Pool, cost: u64) -> Spec {
    Spec {
        id: NodeId(id),
        kind,
        name: format!("n{id}"),
        deps: deps.iter().map(|&it| NodeId(it)).collect(),
        pool,
        cost,
    }
}

fn work(id: u32, deps: &[u32], cost: u64) -> Spec {
    spec(id, NodeKind::Names, deps, Pool::Work, cost)
}

fn done(at_ns: u64, id: u32, outcome: Outcome) -> Input {
    Input::Done { at_ns, id: NodeId(id), outcome, added: Vec::new() }
}

fn runs(outputs: &[Output]) -> Vec<u32> {
    outputs
        .iter()
        .filter_map(|it| match it {
            Output::Run { id, .. } => Some(id.0),
            _ => None,
        })
        .collect()
}

/// The machine does no IO and reads no clock: what it knows comes in its inputs.
#[test]
fn the_machine_does_no_io() {
    let source = include_str!("machine.rs");
    for word in [
        "std::fs",
        "std::net",
        "std::process",
        "std::thread",
        "std::time",
        "std::env",
        "Instant",
        "SystemTime",
        "tokio",
        "crossbeam",
        "File::",
    ] {
        assert!(!source.contains(word), "machine.rs names `{word}`");
    }
}

#[test]
fn a_diamond_runs_each_node_after_what_it_waits_for() {
    let mut machine = Machine::new(Config { io: 2, work: 2, keep_going: false }).unwrap();
    let mut all = machine
        .step(Input::Add {
            at_ns: 0,
            nodes: vec![
                spec(1, NodeKind::Manifest, &[], Pool::Io, 1),
                work(2, &[1], 5),
                work(3, &[1], 1),
                spec(4, NodeKind::Link, &[2, 3], Pool::Io, 1),
            ],
        })
        .unwrap();
    all.extend(machine.step(Input::Close { at_ns: 1 }).unwrap());
    all.extend(machine.step(done(10, 1, Outcome::Built)).unwrap());
    all.extend(machine.step(done(20, 3, Outcome::Cached { remote: true })).unwrap());
    all.extend(machine.step(done(30, 2, Outcome::Built)).unwrap());
    all.extend(machine.step(done(40, 4, Outcome::Built)).unwrap());
    expect![[r#"
        0 node #1 Manifest n1 deps []
        0 #1 Ready
        0 node #2 Names n2 deps [1]
        0 #2 Waiting
        0 node #3 Names n3 deps [1]
        0 #3 Waiting
        0 node #4 Link n4 deps [2, 3]
        0 #4 Waiting
        0 #1 Running
          run #1 on Io
        10 #1 Built
        10 #2 Ready
        10 #3 Ready
        10 #2 Running
          run #2 on Work
        10 #3 Running
          run #3 on Work
        20 #3 Cached { remote: true }
        30 #2 Built
        30 #4 Ready
        30 #4 Running
          run #4 on Io
        40 #4 Built
        40 critical [1, 2, 4]
          finished ok=true
    "#]]
    .assert_eq(&render(&all));
    assert!(machine.is_over());
}

/// A node may come before what it waits for in one input: it is added after it.
#[test]
fn an_input_is_added_in_the_order_of_what_waits() {
    let mut machine = Machine::new(ONE).unwrap();
    let out = machine
        .step(Input::Add { at_ns: 0, nodes: vec![work(2, &[1], 1), work(1, &[], 1)] })
        .unwrap();
    expect![[r#"
                0 node #1 Names n1 deps []
                0 #1 Ready
                0 node #2 Names n2 deps [1]
                0 #2 Waiting
                0 #1 Running
                  run #1 on Work
        "#]]
    .assert_eq(&render(&out));
}

/// Of the nodes that are ready, the one the most waits behind runs first.
#[test]
fn the_longest_path_runs_first() {
    let mut machine = Machine::new(ONE).unwrap();
    let out = machine
        .step(Input::Add {
            at_ns: 0,
            nodes: vec![work(1, &[], 1), work(2, &[], 1), work(3, &[2], 10), work(4, &[1], 2)],
        })
        .unwrap();
    assert_eq!(runs(&out), [2]);
    let out = machine.step(done(1, 2, Outcome::Built)).unwrap();
    assert_eq!(runs(&out), [3]);
    let out = machine.step(done(2, 3, Outcome::Built)).unwrap();
    assert_eq!(runs(&out), [1]);
}

/// A node added later makes what it waits for more urgent.
#[test]
fn a_node_added_later_moves_what_it_waits_for_ahead() {
    let mut machine = Machine::new(ONE).unwrap();
    let out = machine
        .step(Input::Add {
            at_ns: 0,
            nodes: vec![work(1, &[], 1), work(2, &[], 1), work(3, &[], 1)],
        })
        .unwrap();
    assert_eq!(runs(&out), [1]);
    let out = machine
        .step(Input::Done {
            at_ns: 1,
            id: NodeId(1),
            outcome: Outcome::Built,
            added: vec![work(4, &[3], 50)],
        })
        .unwrap();
    assert_eq!(runs(&out), [3]);
}

#[test]
fn a_failure_skips_what_waits_for_it_and_stops_the_build() {
    let mut machine = Machine::new(Config { io: 1, work: 2, keep_going: false }).unwrap();
    let mut all = machine
        .step(Input::Add {
            at_ns: 0,
            nodes: vec![work(1, &[], 1), work(2, &[], 1), work(3, &[1], 1), work(4, &[], 1)],
        })
        .unwrap();
    all.extend(machine.step(Input::Close { at_ns: 0 }).unwrap());
    all.extend(machine.step(done(5, 1, Outcome::Failed { why: "no".to_owned() })).unwrap());
    all.extend(machine.step(done(6, 2, Outcome::Built)).unwrap());
    expect![[r#"
                0 node #1 Names n1 deps []
                0 #1 Ready
                0 node #2 Names n2 deps []
                0 #2 Ready
                0 node #3 Names n3 deps [1]
                0 #3 Waiting
                0 node #4 Names n4 deps []
                0 #4 Ready
                0 #1 Running
                  run #1 on Work
                0 #2 Running
                  run #2 on Work
                5 #1 Failed { why: "no" }
                5 #3 Skipped { because: 1 }
                5 #4 Skipped { because: 1 }
                6 #2 Built
                6 critical [2]
                  finished ok=false
        "#]]
    .assert_eq(&render(&all));
}

#[test]
fn keep_going_runs_what_does_not_wait_for_the_failure() {
    let mut machine = Machine::new(Config { io: 1, work: 1, keep_going: true }).unwrap();
    machine
        .step(Input::Add {
            at_ns: 0,
            nodes: vec![work(1, &[], 2), work(2, &[1], 1), work(3, &[], 1)],
        })
        .unwrap();
    machine.step(Input::Close { at_ns: 0 }).unwrap();
    let out = machine.step(done(1, 1, Outcome::Failed { why: "no".to_owned() })).unwrap();
    assert_eq!(runs(&out), [3]);
    assert_eq!(machine.state(NodeId(2)), Some(&NodeState::Skipped { because: 1 }));
    let out = machine.step(done(2, 3, Outcome::Built)).unwrap();
    assert_eq!(out.last(), Some(&Output::Finished { ok: false }));
    // A node found after the failure that waits for a skipped one is skipped for the failure.
    let mut machine = Machine::new(Config { io: 1, work: 1, keep_going: true }).unwrap();
    machine
        .step(Input::Add {
            at_ns: 0,
            nodes: vec![work(1, &[], 1), work(2, &[1], 1), work(3, &[], 0)],
        })
        .unwrap();
    machine.step(done(1, 1, Outcome::Failed { why: "no".to_owned() })).unwrap();
    machine
        .step(Input::Done {
            at_ns: 2,
            id: NodeId(3),
            outcome: Outcome::Built,
            added: vec![work(4, &[2], 1)],
        })
        .unwrap();
    assert_eq!(machine.state(NodeId(4)), Some(&NodeState::Skipped { because: 1 }));
}

/// What another thread or process says is checked, and an input refused changes nothing.
#[test]
fn inputs_it_cannot_take_are_refused_and_change_nothing() {
    assert_eq!(
        Machine::new(Config { io: 0, work: 1, keep_going: false }).err(),
        Some(Error::NoThreads(Pool::Io))
    );
    let mut machine = Machine::new(ONE).unwrap();
    machine.step(Input::Add { at_ns: 0, nodes: vec![work(1, &[], 1), work(2, &[1], 1)] }).unwrap();
    let refused = [
        (Input::Add { at_ns: 1, nodes: vec![work(1, &[], 1)] }, Error::Twice(NodeId(1))),
        (
            Input::Add { at_ns: 1, nodes: vec![work(3, &[], 1), work(3, &[], 1)] },
            Error::Twice(NodeId(3)),
        ),
        (
            Input::Add { at_ns: 1, nodes: vec![work(3, &[9], 1)] },
            Error::UnknownDep { node: NodeId(3), dep: NodeId(9) },
        ),
        (
            Input::Add {
                at_ns: 1,
                nodes: vec![work(3, &[5], 1), work(4, &[3], 1), work(5, &[4], 1), work(6, &[], 1)],
            },
            Error::Cycle(vec![NodeId(3), NodeId(4), NodeId(5)]),
        ),
        (done(1, 2, Outcome::Built), Error::NotRunning(NodeId(2))),
        (done(1, 9, Outcome::Built), Error::NotRunning(NodeId(9))),
        (
            Input::Done {
                at_ns: 1,
                id: NodeId(1),
                outcome: Outcome::Built,
                added: vec![work(2, &[], 1)],
            },
            Error::Twice(NodeId(2)),
        ),
    ];
    for (input, error) in refused {
        assert_eq!(machine.step(input).err(), Some(error));
    }
    assert_eq!(machine.state(NodeId(1)), Some(&NodeState::Running));
    assert_eq!(machine.state(NodeId(3)), None);
    assert_eq!(machine.state(NodeId(6)), None);
    machine.step(Input::Close { at_ns: 1 }).unwrap();
    assert_eq!(machine.step(Input::Close { at_ns: 1 }).err(), Some(Error::Closed));
    assert_eq!(machine.step(Input::Add { at_ns: 1, nodes: vec![] }).err(), Some(Error::Closed));
    machine.step(done(2, 1, Outcome::Built)).unwrap();
    machine.step(done(3, 2, Outcome::Built)).unwrap();
    assert_eq!(machine.step(done(4, 2, Outcome::Built)).err(), Some(Error::Over));
}

/// A small generator, so the simulation needs no dependency and every seed is the same run.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) % n.max(1)
    }
}

/// Random graphs finished in a random order: no node runs before what it waits for is done,
/// no pool runs more than its threads, every node settles, and the critical path is a chain
/// of nodes that each waited for the one before.
#[test]
fn random_builds_keep_the_rules() {
    for seed in 0..300u64 {
        let mut random = Lcg(seed);
        let nodes = 1 + random.below(40) as u32;
        let config = Config {
            io: 1 + random.below(3) as usize,
            work: 1 + random.below(4) as usize,
            keep_going: random.below(2) == 0,
        };
        let mut specs = Vec::new();
        for id in 0..nodes {
            let deps: FxHashSet<u32> = (0..random.below(4))
                .filter(|_| id > 0)
                .map(|_| random.below(id.into()) as u32)
                .collect();
            let mut deps: Vec<u32> = deps.into_iter().collect();
            deps.sort();
            let pool = if random.below(3) == 0 { Pool::Io } else { Pool::Work };
            specs.push(spec(id, NodeKind::Names, &deps, pool, random.below(10)));
        }
        let deps_of: FxHashMap<u32, Vec<u32>> =
            specs.iter().map(|it| (it.id.0, it.deps.iter().map(|it| it.0).collect())).collect();
        let pool_of: FxHashMap<u32, Pool> = specs.iter().map(|it| (it.id.0, it.pool)).collect();
        let mut machine = Machine::new(config).unwrap();
        let mut running: Vec<u32> = Vec::new();
        let mut finished = None;
        let mut states: FxHashMap<u32, NodeState> = FxHashMap::default();
        let mut critical = Vec::new();
        let take = |outputs: Vec<Output>,
                    running: &mut Vec<u32>,
                    states: &mut FxHashMap<u32, NodeState>,
                    finished: &mut Option<bool>,
                    critical: &mut Vec<u32>| {
            for output in outputs {
                match output {
                    Output::Run { id, .. } => {
                        for dep in &deps_of[&id.0] {
                            assert!(
                                matches!(states[dep], NodeState::Built | NodeState::Cached { .. }),
                                "seed {seed}: #{} runs before #{dep} is done",
                                id.0
                            );
                        }
                        running.push(id.0);
                    }
                    Output::Said(Record { event: Event::Moved { id, state }, .. }) => {
                        if let Some(was) = states.get(&id) {
                            assert!(!was.settled(), "seed {seed}: #{id} moved after it settled");
                        }
                        states.insert(id, state);
                    }
                    Output::Said(Record { event: Event::Critical { path }, .. }) => {
                        *critical = path
                    }
                    Output::Said(_) => {}
                    Output::Finished { ok } => *finished = Some(ok),
                }
            }
            for pool in [Pool::Io, Pool::Work] {
                let limit = if pool == Pool::Io { config.io } else { config.work };
                let busy = running.iter().filter(|it| pool_of[*it] == pool).count();
                assert!(busy <= limit, "seed {seed}: {busy} of {pool:?} at once");
            }
        };
        let mut at = 0;
        for input in [Input::Add { at_ns: at, nodes: specs }, Input::Close { at_ns: at }] {
            let outputs = machine.step(input).unwrap();
            take(outputs, &mut running, &mut states, &mut finished, &mut critical);
        }
        while finished.is_none() {
            assert!(!running.is_empty(), "seed {seed}: nothing runs and the build is not over");
            let id = running.swap_remove(random.below(running.len() as u64) as usize);
            at += 1 + random.below(5);
            let outcome = match random.below(12) {
                0 => Outcome::Failed { why: "no".to_owned() },
                1 => Outcome::Cached { remote: false },
                _ => Outcome::Built,
            };
            let outputs = machine.step(done(at, id, outcome)).unwrap();
            take(outputs, &mut running, &mut states, &mut finished, &mut critical);
        }
        assert!(running.is_empty(), "seed {seed}");
        assert_eq!(states.len(), nodes as usize, "seed {seed}");
        assert!(states.values().all(NodeState::settled), "seed {seed}");
        let failed = states.values().any(|it| matches!(it, NodeState::Failed { .. }));
        assert_eq!(finished, Some(!failed), "seed {seed}");
        assert!(!critical.is_empty(), "seed {seed}");
        for pair in critical.windows(2) {
            assert!(deps_of[&pair[1]].contains(&pair[0]), "seed {seed}: critical {critical:?}");
        }
    }
}

#[test]
fn the_executor_runs_every_ready_node_at_once() {
    // Four nodes that each wait until all four run: they finish only if they run together.
    let started = AtomicUsize::new(0);
    let nodes: Vec<Spec> = (0..4).map(|id| work(id, &[], 1)).collect();
    let mut machine = Machine::new(Config { io: 1, work: 4, keep_going: false }).unwrap();
    let mut records = Vec::new();
    let ok = drive(
        &mut machine,
        nodes,
        &|_| {
            started.fetch_add(1, Ordering::SeqCst);
            while started.load(Ordering::SeqCst) < 4 {
                std::thread::yield_now();
            }
            Did::built()
        },
        &mut |record| records.push(record.clone()),
        None,
    )
    .unwrap();
    assert!(ok);
    let built = records
        .iter()
        .filter(|it| matches!(it.event, Event::Moved { state: NodeState::Built, .. }))
        .count();
    assert_eq!(built, 4);
}

#[test]
fn the_executor_adds_what_work_finds_and_fails_on_a_panic() {
    let ran = Mutex::new(Vec::new());
    let mut machine = Machine::new(Config { io: 2, work: 2, keep_going: true }).unwrap();
    let mut trace = Vec::new();
    let mut records = Vec::new();
    let drove = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drive(
            &mut machine,
            vec![spec(0, NodeKind::Manifest, &[], Pool::Io, 1)],
            &|id| {
                ran.lock().unwrap().push(id.0);
                match id.0 {
                    0 => Did {
                        outcome: Outcome::Built,
                        added: vec![work(1, &[0], 1), work(2, &[0], 1), work(3, &[1, 2], 1)],
                    },
                    2 => panic!("no def map"),
                    _ => Did::built(),
                }
            },
            &mut |record| records.push(record.clone()),
            Some(&mut trace),
        )
    }));
    // The panic goes on once the build is over, and the build says the node failed.
    let panic = drove.err().expect("the work's panic reaches the caller");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"no def map"));
    assert!(matches!(records.last().map(|it| &it.event), Some(Event::Critical { .. })));
    let mut ran = ran.into_inner().unwrap();
    ran.sort();
    assert_eq!(ran, [0, 1, 2]);
    assert_eq!(
        machine.state(NodeId(2)),
        Some(&NodeState::Failed { why: "panicked: no def map".to_owned() })
    );
    assert_eq!(machine.state(NodeId(3)), Some(&NodeState::Skipped { because: 2 }));
    // The trace replays to what the build said.
    let trace = Trace { config: machine.config(), inputs: trace };
    let replayed: Vec<Record> = Trace::read(&trace.write())
        .unwrap()
        .replay()
        .unwrap()
        .into_iter()
        .filter_map(|it| match it {
            Output::Said(record) => Some(record),
            _ => None,
        })
        .collect();
    assert_eq!(replayed, records);
}

/// Each trace of `traces/` gives the outputs its `.out` file says.
#[test]
fn traces_replay_as_they_were() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("traces");
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|it| it.unwrap().path())
        .filter(|it| it.extension().is_some_and(|it| it == "jsonl"))
        .collect();
    names.sort();
    assert!(!names.is_empty());
    for path in names {
        let trace = Trace::read(&std::fs::read_to_string(&path).unwrap())
            .unwrap_or_else(|why| panic!("{}: {why}", path.display()));
        let outputs = trace.replay().unwrap_or_else(|why| panic!("{}: {why}", path.display()));
        expect_file![path.with_extension("out")].assert_eq(&render(&outputs));
    }
}
