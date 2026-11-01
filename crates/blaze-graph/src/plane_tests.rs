//! The plane's tests (docs/v2/plan/lanes/core/plane.md): what each rule does alone, the plane as the machine
//! with one request, and random requests under a seeded generator, checked after every step
//! and step by step against [`reference`], the plane as it was before its state moved to
//! columns.

#![forbid(unsafe_code)]

mod memory;
mod reference;

use std::sync::Arc;

use blaze_hash::{HashMap, HashSet};
use blaze_ids::rng::SplitMix64;
use blaze_protocol::events::{Event, NodeKind, NodeState, Order, Record};
use parking_lot::Mutex;

use crate::{
    machine::{Config, Error, Input, Machine, NodeId, Outcome, Output, Pool, Spec},
    plane::{
        Budget, Counts, Key, Memo, NodeRef, Nodes, Plane, PlaneConfig, PlaneInput, PlaneOutput,
        RequestId, RequestStats,
    },
    trace::{Trace, render},
};

fn key(k: u8) -> Key {
    Key([k; 32])
}

fn name(id: u32) -> String {
    format!("<{id}>")
}

fn node(id: u32, deps: &[u32], pool: Pool, cost: u64, key: Option<Key>, memory: u64) -> Spec {
    Spec {
        id: NodeId(id),
        kind: NodeKind::Codegen,
        name: name(id),
        deps: deps.iter().map(|&it| NodeId(it)).collect(),
        pool,
        cost,
        key,
        memory,
    }
}

fn work(id: u32, deps: &[u32], cost: u64) -> Spec {
    node(id, deps, Pool::Work, cost, None, 0)
}

fn keyed(id: u32, deps: &[u32], cost: u64, k: u8) -> Spec {
    node(id, deps, Pool::Work, cost, Some(key(k)), 0)
}

fn config(io: usize, work: usize) -> PlaneConfig {
    PlaneConfig { io, work, budget: None, starve_after: 0 }
}

fn budget(bytes: u64) -> Option<Budget> {
    Some(Budget { bytes, from: "the test's budget".to_owned() })
}

fn open(plane: &mut Plane<impl Memo>, request: u32, weight: u32, keep_going: bool) {
    open_as(plane, request, weight, keep_going, false);
}

fn open_as(plane: &mut Plane<impl Memo>, request: u32, weight: u32, keep_going: bool, idle: bool) {
    let request = RequestId(request);
    let input = PlaneInput::Open { at_ns: 0, request, weight, keep_going, idle };
    assert!(plane.step(input).unwrap().is_empty());
}

fn add(plane: &mut Plane<impl Memo>, request: u32, nodes: Vec<Spec>) -> Vec<PlaneOutput> {
    plane.step(PlaneInput::Add { at_ns: 0, request: RequestId(request), nodes }).unwrap()
}

fn close(plane: &mut Plane<impl Memo>, request: u32) -> Vec<PlaneOutput> {
    plane.step(PlaneInput::Close { at_ns: 0, request: RequestId(request) }).unwrap()
}

fn done(id: u32, outcome: Outcome, added: Vec<Spec>) -> PlaneInput {
    PlaneInput::Done { at_ns: 0, id: NodeId(id), outcome, added, group: None, peak: 0 }
}

fn built(plane: &mut Plane<impl Memo>, id: u32) -> Vec<PlaneOutput> {
    plane.step(done(id, Outcome::Built, Vec::new())).unwrap()
}

fn runs(outputs: &[PlaneOutput]) -> Vec<u32> {
    outputs
        .iter()
        .filter_map(|it| match it {
            PlaneOutput::Run { id, .. } => Some(id.0),
            _ => None,
        })
        .collect()
}

fn finished(outputs: &[PlaneOutput]) -> Vec<(u32, bool)> {
    outputs
        .iter()
        .filter_map(|it| match it {
            PlaneOutput::Finished { request, ok } => Some((request.0, *ok)),
            _ => None,
        })
        .collect()
}

fn moves(outputs: &[PlaneOutput]) -> Vec<(u32, NodeState)> {
    outputs
        .iter()
        .filter_map(|it| match it {
            PlaneOutput::Said { record: Record { event: Event::Moved { id, state }, .. }, .. } => {
                Some((*id, state.clone()))
            }
            _ => None,
        })
        .collect()
}

/// The plane does no IO and reads no clock: what it knows comes in its inputs.
#[test]
fn the_plane_does_no_io() {
    let source = include_str!("plane.rs");
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
        assert!(!source.contains(word), "plane.rs names `{word}`");
    }
}

/// The plane's outputs as the machine says them.
fn as_machine(outputs: Vec<PlaneOutput>) -> Vec<Output> {
    outputs
        .into_iter()
        .map(|it| match it {
            PlaneOutput::Said { request, record } => {
                assert_eq!(request, RequestId(0));
                Output::Said(record)
            }
            PlaneOutput::Run { id, pool, .. } => Output::Run { id, pool },
            PlaneOutput::Finished { request, ok } => {
                assert_eq!(request, RequestId(0));
                Output::Finished { ok }
            }
            PlaneOutput::Yield { id } => panic!("{id} asked to give way: no request is idle"),
        })
        .collect()
}

/// The machine's input as the plane's, of request 0.
fn as_plane(input: Input) -> PlaneInput {
    let request = RequestId(0);
    match input {
        Input::Add { at_ns, nodes } => PlaneInput::Add { at_ns, request, nodes },
        Input::Close { at_ns } => PlaneInput::Close { at_ns, request },
        Input::Done { at_ns, id, outcome, added } => {
            PlaneInput::Done { at_ns, id, outcome, added, group: None, peak: 0 }
        }
    }
}

fn one_request(config: Config) -> Plane {
    let mut plane = Plane::new(PlaneConfig {
        io: config.io,
        work: config.work,
        budget: None,
        starve_after: 0,
    })
    .unwrap();
    let input = PlaneInput::Open {
        at_ns: 0,
        request: RequestId(0),
        weight: 1,
        keep_going: config.keep_going,
        idle: false,
    };
    assert!(plane.step(input).unwrap().is_empty());
    plane
}

/// With one request, no keys and no budget, the plane says what the machine says, step by
/// step: the traces of `traces/` and random builds with failures and nodes found as they go.
#[test]
fn one_request_without_keys_or_budget_says_what_the_machine_says() {
    let trace = Trace::read(include_str!("../traces/cold-small.jsonl")).unwrap();
    let mut plane = one_request(trace.config);
    let mut outputs = Vec::new();
    for input in &trace.inputs {
        outputs.extend(as_machine(plane.step(as_plane(input.clone())).unwrap()));
    }
    assert_eq!(render(&outputs).trim_end(), include_str!("../traces/cold-small.out").trim_end());

    for seed in 0..300u64 {
        let mut random = SplitMix64::new(seed);
        let config = Config {
            io: 1 + random.below(3) as usize,
            work: 1 + random.below(4) as usize,
            keep_going: random.one_in(2),
        };
        let mut machine = Machine::new(config).unwrap();
        let mut plane = one_request(config);
        let mut next = 0u32;
        let mut ids: Vec<u32> = Vec::new();
        let mut make = |random: &mut SplitMix64, ids: &mut Vec<u32>, count: u64| {
            let mut nodes = Vec::new();
            for _ in 0..count {
                let mut deps: Vec<u32> = (0..random.below(4))
                    .filter(|_| !ids.is_empty())
                    .map(|_| ids[random.below(ids.len() as u64) as usize])
                    .collect();
                deps.sort_unstable();
                deps.dedup();
                let pool = if random.one_in(3) { Pool::Io } else { Pool::Work };
                nodes.push(node(next, &deps, pool, random.below(10), None, 0));
                ids.push(next);
                next += 1;
            }
            nodes
        };
        let mut running: Vec<NodeId> = Vec::new();
        let mut same = |input: Input, running: &mut Vec<NodeId>| -> bool {
            let want = machine.step(input.clone()).unwrap();
            let got = as_machine(plane.step(as_plane(input)).unwrap());
            assert_eq!(render(&got), render(&want), "seed {seed}");
            assert_eq!(got, want, "seed {seed}");
            for output in &want {
                if let Output::Run { id, .. } = output {
                    running.push(*id);
                }
            }
            want.iter().any(|it| matches!(it, Output::Finished { .. }))
        };
        let count = 1 + random.below(30);
        let first = make(&mut random, &mut ids, count);
        let mut over = same(Input::Add { at_ns: 0, nodes: first }, &mut running);
        over |= same(Input::Close { at_ns: 0 }, &mut running);
        let mut at = 0;
        while !over {
            assert!(!running.is_empty(), "seed {seed}: nothing runs and the build is not over");
            let id = running.swap_remove(random.below(running.len() as u64) as usize);
            at += 1 + random.below(5);
            let outcome = match random.below(12) {
                0 => Outcome::Failed { why: "no".to_owned() },
                1 => Outcome::Cached { remote: false },
                _ => Outcome::Built,
            };
            let count = if random.one_in(4) { 1 + random.below(3) } else { 0 };
            let added = make(&mut random, &mut ids, count);
            over = same(Input::Done { at_ns: at, id, outcome, added }, &mut running);
        }
    }
}

/// What the query database has, as the plane's [`crate::plane::Memo`] sees it: the test puts a
/// task's value in it before its `Done`, as the work does.
type Db = Arc<Mutex<HashMap<Key, Outcome>>>;

/// A test's [`Db`] as the plane's memo.
struct DbMemo(Db);

impl Memo for DbMemo {
    fn settled(&self, key: Key) -> Option<Outcome> {
        self.0.lock().get(&key).cloned()
    }
}

fn with_db(config: PlaneConfig) -> (Plane<DbMemo>, Db) {
    let db = Db::default();
    (Plane::with_memo(config, DbMemo(Arc::clone(&db))).unwrap(), db)
}

#[test]
fn a_key_the_database_has_runs_nothing() {
    let (mut plane, db) = with_db(config(1, 2));
    open(&mut plane, 1, 1, false);
    assert_eq!(runs(&add(&mut plane, 1, vec![keyed(1, &[], 1, 7)])), [1]);
    close(&mut plane, 1);
    db.lock().insert(key(7), Outcome::Built);
    assert_eq!(finished(&built(&mut plane, 1)), [(1, true)]);
    open(&mut plane, 2, 1, false);
    let out = add(&mut plane, 2, vec![keyed(2, &[], 1, 7), work(3, &[2], 1)]);
    assert_eq!(runs(&out), [3]);
    assert_eq!(plane.state(NodeId(2)), Some(&NodeState::Cached { remote: false }));
    assert_eq!(plane.counts().remembered, 1);
    // A failure the database keeps fails the node, and what waits for it is skipped.
    db.lock().insert(key(8), Outcome::Failed { why: "it failed before".to_owned() });
    open(&mut plane, 3, 1, true);
    let out = add(&mut plane, 3, vec![keyed(4, &[], 1, 8), work(5, &[4], 1)]);
    assert!(runs(&out).is_empty());
    assert_eq!(
        moves(&out),
        [
            (5, NodeState::Waiting),
            (4, NodeState::Failed { why: "it failed before".to_owned() }),
            (5, NodeState::Skipped { because: 4 }),
        ]
    );
    // Gone from the database, the key runs again.
    db.lock().remove(&key(7));
    open(&mut plane, 4, 1, false);
    assert_eq!(runs(&add(&mut plane, 4, vec![keyed(6, &[], 1, 7)])), [6]);
}

/// `stats` with `ns` of run time summed under [`NodeKind::Codegen`], the kind of every node here.
fn codegen(mut stats: RequestStats, ns: u64) -> RequestStats {
    stats.steps_ns[NodeKind::Codegen as usize] = ns;
    stats
}

/// Each request counts what it ran, what the database settled and what it joined, its
/// longest chain of measured run times and its run time by kind, a cached or joined node's own
/// time being 0.
#[test]
fn a_request_says_what_it_ran_and_its_measured_critical_path() {
    let (mut plane, db) = with_db(config(1, 2));
    let at = |plane: &mut Plane<DbMemo>, input: PlaneInput| plane.step(input).unwrap();
    let add_at = |plane: &mut Plane<DbMemo>, at_ns: u64, request: u32, nodes: Vec<Spec>| {
        plane.step(PlaneInput::Add { at_ns, request: RequestId(request), nodes }).unwrap()
    };
    let done_at = |id: u32, at_ns: u64| PlaneInput::Done {
        at_ns,
        id: NodeId(id),
        outcome: Outcome::Built,
        added: Vec::new(),
        group: None,
        peak: 0,
    };
    // 1 then 2, and 3 beside them: 1 runs 0..10, 2 runs 10..15, 3 runs 0..12.
    open(&mut plane, 1, 1, false);
    let out = add(&mut plane, 1, vec![work(1, &[], 1), work(2, &[1], 1), work(3, &[], 1)]);
    assert_eq!(runs(&out), [1, 3]);
    close(&mut plane, 1);
    assert_eq!(runs(&at(&mut plane, done_at(1, 10))), [2]);
    at(&mut plane, done_at(3, 12));
    assert_eq!(finished(&at(&mut plane, done_at(2, 15))), [(1, true)]);
    let stats = plane.stats(RequestId(1)).unwrap();
    assert_eq!(stats, codegen(RequestStats { ran: 3, cached: 0, joined: 0, critical_path_ns: 15, ..Default::default() }, 27));

    // Request 3's node of key 7 joins request 2's, which runs 20..30; each then runs one
    // more: request 2's 30..38, request 3's 30..33.
    open(&mut plane, 2, 1, false);
    open(&mut plane, 3, 1, false);
    assert_eq!(runs(&add_at(&mut plane, 20, 2, vec![keyed(4, &[], 1, 7), work(5, &[4], 1)])), [4]);
    let out = add_at(&mut plane, 20, 3, vec![keyed(6, &[], 1, 7), work(7, &[6], 1)]);
    assert!(runs(&out).is_empty());
    close(&mut plane, 2);
    close(&mut plane, 3);
    db.lock().insert(key(7), Outcome::Built);
    // Request 3 has started nothing yet, so its turn comes first.
    assert_eq!(runs(&at(&mut plane, done_at(4, 30))), [7, 5]);
    assert_eq!(finished(&at(&mut plane, done_at(7, 33))), [(3, true)]);
    assert_eq!(finished(&at(&mut plane, done_at(5, 38))), [(2, true)]);
    let two = plane.stats(RequestId(2)).unwrap();
    assert_eq!(two, codegen(RequestStats { ran: 2, cached: 0, joined: 0, critical_path_ns: 18, ..Default::default() }, 18));
    let three = plane.stats(RequestId(3)).unwrap();
    assert_eq!(three, codegen(RequestStats { ran: 1, cached: 0, joined: 1, critical_path_ns: 3, ..Default::default() }, 3));

    // Now the database has key 7: request 4's node of it is cached, and only 9 runs, 40..47.
    open(&mut plane, 4, 1, false);
    assert_eq!(runs(&add_at(&mut plane, 40, 4, vec![keyed(8, &[], 1, 7), work(9, &[8], 1)])), [9]);
    close(&mut plane, 4);
    assert_eq!(finished(&at(&mut plane, done_at(9, 47))), [(4, true)]);
    let four = plane.stats(RequestId(4)).unwrap();
    assert_eq!(four, codegen(RequestStats { ran: 1, cached: 1, joined: 0, critical_path_ns: 7, ..Default::default() }, 7));

    assert_eq!(plane.stats(RequestId(99)), None);
    plane.release(RequestId(4)).unwrap();
    assert_eq!(plane.stats(RequestId(4)), None);
}

/// A run is critical when nothing ready or running of its request has a longer path to go;
/// a running node that rises as nodes are added after it counts at its new priority.
#[test]
fn a_run_says_whether_it_is_on_the_critical_path() {
    let critical = |outputs: &[PlaneOutput]| -> Vec<(u32, bool)> {
        outputs
            .iter()
            .filter_map(|it| match it {
                PlaneOutput::Run { id, critical, .. } => Some((id.0, *critical)),
                _ => None,
            })
            .collect()
    };
    let mut plane = Plane::new(config(1, 2)).unwrap();
    open(&mut plane, 1, 1, false);
    // 1 (then 2) has 10 to go, 3 has 1.
    let out = add(&mut plane, 1, vec![work(1, &[], 5), work(2, &[1], 5), work(3, &[], 1)]);
    assert_eq!(critical(&out), [(1, true), (3, false)]);
    // 4 after running 3 raises 3 to 101: the most of the request is now running 3.
    assert!(runs(&add(&mut plane, 1, vec![work(4, &[3], 100)])).is_empty());
    close(&mut plane, 1);
    assert_eq!(critical(&built(&mut plane, 1)), [(2, false)]);
    assert_eq!(critical(&built(&mut plane, 3)), [(4, true)]);
}

/// Nodes as specs, the same nodes from `first` on.
fn as_nodes(first: u32, specs: &[Spec]) -> Nodes {
    let names = specs.iter().map(|it| it.name.len()).sum();
    let deps = specs.iter().map(|it| it.deps.len()).sum();
    let mut nodes = Nodes::with_capacity(NodeId(first), specs.len(), names, deps);
    for spec in specs {
        let id = nodes.push(NodeRef {
            kind: spec.kind,
            name: &spec.name,
            deps: &spec.deps,
            pool: spec.pool,
            cost: spec.cost,
            key: spec.key,
            memory: spec.memory,
        });
        assert_eq!(id, spec.id);
    }
    nodes
}

/// `outputs` apart from what they say of nodes, and what they say of nodes as an
/// [`Event::Node`] a node, in the order those would come: each [`Event::Nodes`] read as its
/// nodes in [`Order::places`]. A malformed record fails the test.
fn nodes_apart(outputs: &[PlaneOutput]) -> (Vec<PlaneOutput>, Vec<PlaneOutput>) {
    let (mut rest, mut nodes) = (Vec::new(), Vec::new());
    let mut order = Order::default();
    for output in outputs {
        let PlaneOutput::Said { request, record } = output else {
            rest.push(output.clone());
            continue;
        };
        match &record.event {
            Event::Node { .. } => nodes.push(output.clone()),
            Event::Nodes { .. } => {
                let batch = record.event.nodes(&mut order).unwrap().unwrap();
                for &place in order.places() {
                    let event = batch.node(place as usize).event();
                    let record = Record { at_ns: record.at_ns, event };
                    nodes.push(PlaneOutput::Said { request: *request, record });
                }
            }
            _ => rest.push(output.clone()),
        }
    }
    (rest, nodes)
}

/// Batches as columns say what the same batches as specs say, refusals too: deps out of
/// order, cycles, unknown deps, ids given before, single flight and two requests. A batch of
/// columns is said in one [`Event::Nodes`], first, which read gives the [`Event::Node`]s the
/// specs say, in the order they say them; all else is the same, in the same order.
#[test]
fn nodes_as_columns_say_what_nodes_as_specs_say() {
    let kinds = [NodeKind::Codegen, NodeKind::Std, NodeKind::Link];
    let mut batches = 0;
    for seed in 0..300 {
        let mut random = SplitMix64::new(seed);
        let config = config(1 + random.below(2) as usize, 1 + random.below(3) as usize);
        let mut specs = Plane::new(config.clone()).unwrap();
        let mut columns = Plane::new(config).unwrap();
        let mut both = |input: PlaneInput, columns_input: PlaneInput| {
            let batch = matches!(columns_input, PlaneInput::AddNodes { .. });
            let said = specs.step(input);
            let columns_said = columns.step(columns_input);
            match (&said, &columns_said) {
                (Ok(each), Ok(together)) => {
                    assert_eq!(nodes_apart(each), nodes_apart(together), "seed {seed}");
                    if batch {
                        batches += 1;
                        let first = together.first().map(|it| match it {
                            PlaneOutput::Said { record, .. } => &record.event,
                            _ => panic!("seed {seed}: {it:?} before the batch's record"),
                        });
                        assert!(matches!(first, Some(Event::Nodes { .. })), "seed {seed}");
                        assert!(each.iter().all(|it| !matches!(it, PlaneOutput::Said {
                            record: Record { event: Event::Nodes { .. }, .. },
                            ..
                        })));
                    } else {
                        assert_eq!(each, together, "seed {seed}");
                    }
                }
                _ => assert_eq!(said, columns_said, "seed {seed}"),
            }
            said
        };
        for request in 1..=2 {
            let id = RequestId(request);
            let open = PlaneInput::Open {
                at_ns: 0,
                request: id,
                weight: request,
                keep_going: true,
                idle: false,
            };
            both(open.clone(), open).unwrap();
        }
        let mut next = 1u32;
        let mut known: [Vec<u32>; 2] = [Vec::new(), Vec::new()];
        let mut running: Vec<u32> = Vec::new();
        for step in 0..40u64 {
            if running.is_empty() || random.one_in(2) {
                let r = random.below(2) as usize;
                // Now and then a run that starts on an id given before.
                let first = if next > 1 && random.one_in(8) {
                    1 + random.below(u64::from(next - 1)) as u32
                } else {
                    next
                };
                let count = 1 + random.below(6) as u32;
                let batch: Vec<Spec> = (first..first + count)
                    .map(|id| {
                        let mut deps = Vec::new();
                        for _ in 0..random.below(3) {
                            let dep = match random.below(10) {
                                0 => 1_000_000,
                                1..=4 if !known[r].is_empty() => {
                                    known[r][random.below(known[r].len() as u64) as usize]
                                }
                                _ => first + random.below(u64::from(count)) as u32,
                            };
                            if dep != id && !deps.contains(&dep) {
                                deps.push(dep);
                            }
                        }
                        let key = random.one_in(3).then(|| key(random.below(4) as u8));
                        let pool = if random.one_in(4) { Pool::Io } else { Pool::Work };
                        let mut spec = node(id, &deps, pool, random.below(50), key, 0);
                        spec.kind = kinds[random.below(3) as usize];
                        spec.name = format!("{}é{id}", "x".repeat(random.below(4) as usize));
                        spec
                    })
                    .collect();
                let request = RequestId(r as u32 + 1);
                let nodes = as_nodes(first, &batch);
                let said = both(
                    PlaneInput::Add { at_ns: step, request, nodes: batch },
                    PlaneInput::AddNodes { at_ns: step, request, nodes },
                );
                if let Ok(out) = said {
                    known[r].extend(first..first + count);
                    next = next.max(first + count);
                    running.extend(runs(&out));
                }
            } else {
                let id = running.swap_remove(random.below(running.len() as u64) as usize);
                let outcome = if random.one_in(5) {
                    Outcome::Failed { why: "no".to_owned() }
                } else {
                    Outcome::Built
                };
                let out =
                    both(done(id, outcome.clone(), Vec::new()), done(id, outcome, Vec::new()))
                        .unwrap();
                running.extend(runs(&out));
            }
        }
    }
    assert!(batches > 500, "{batches} batches taken");
}

/// A batch of columns is said in one record before anything else: the columns as given,
/// through the wire's codec unchanged, read as the records the same nodes as specs say, in
/// their order, though given out of order; the same record malformed is refused.
#[test]
fn a_batch_of_columns_is_one_record() {
    let specs = vec![
        node(3, &[5], Pool::Work, 1, None, 0),
        node(4, &[], Pool::Io, 2, Some(key(1)), 0),
        node(5, &[4, 1], Pool::Work, 3, None, 0),
    ];
    let mut each = Plane::new(config(1, 2)).unwrap();
    let mut together = Plane::new(config(1, 2)).unwrap();
    for plane in [&mut each, &mut together] {
        open(plane, 1, 1, true);
        assert_eq!(runs(&add(plane, 1, vec![work(1, &[], 1)])), [1]);
    }
    let request = RequestId(1);
    let nodes = as_nodes(3, &specs);
    let said = each.step(PlaneInput::Add { at_ns: 2, request, nodes: specs }).unwrap();
    let out = together.step(PlaneInput::AddNodes { at_ns: 2, request, nodes }).unwrap();
    let PlaneOutput::Said { request: of, record } = &out[0] else { panic!("{out:?}") };
    assert_eq!(*of, request);
    let names = format!("{}{}{}", name(3), name(4), name(5));
    let (three, four) = (name(3).len() as u32, name(4).len() as u32);
    let want = Event::Nodes {
        first: 3,
        kinds: vec![NodeKind::Codegen; 3],
        name_at: vec![0, three, three + four, names.len() as u32],
        names,
        deps: vec![5, 4, 1],
        deps_at: vec![0, 1, 1, 3],
    };
    assert_eq!(*record, Record { at_ns: 2, event: want });
    let bytes = blaze_protocol::cbor::to_vec(record).unwrap();
    blaze_protocol::cbor::check(&bytes).unwrap();
    assert_eq!(blaze_protocol::cbor::from_slice::<Record>(&bytes).unwrap(), *record);
    // Read, its nodes are those the specs said, 4 then 5 then 3, and nothing else differs.
    assert_eq!(nodes_apart(&out), nodes_apart(&said));
    let mut order = Order::default();
    record.event.nodes(&mut order).unwrap().unwrap();
    assert_eq!(order.places(), [1, 2, 0]);
    let said_ids: Vec<u32> = said
        .iter()
        .filter_map(|it| match it {
            PlaneOutput::Said { record: Record { event: Event::Node { id, .. }, .. }, .. } => {
                Some(*id)
            }
            _ => None,
        })
        .collect();
    assert_eq!(said_ids, [4, 5, 3]);
    let mut broken = record.event.clone();
    let Event::Nodes { deps_at, .. } = &mut broken else { unreachable!("an Event::Nodes") };
    deps_at[2] = 4;
    let why = broken.nodes(&mut order).unwrap().unwrap_err().to_string();
    assert!(why.contains("deps_at goes back at node 2"), "{why}");
    assert!(order.places().is_empty());
}

/// A trace holds nodes as columns and gives back the same; columns that disagree are refused.
#[test]
fn nodes_read_from_a_trace_are_checked() {
    let nodes = as_nodes(7, &[work(7, &[], 1), work(8, &[7], 2), keyed(9, &[7, 8], 3, 1)]);
    let json = serde_json::to_value(&nodes).unwrap();
    assert_eq!(serde_json::from_value::<Nodes>(json.clone()).unwrap(), nodes);
    fn broken(json: &serde_json::Value, edit: impl FnOnce(&mut serde_json::Value)) -> String {
        let mut it = json.clone();
        edit(&mut it);
        serde_json::from_value::<Nodes>(it).unwrap_err().to_string()
    }
    let refused = broken(&json, |it| it["costs"].as_array_mut().unwrap().truncate(2));
    assert!(refused.contains("3 nodes but 2 costs"), "{refused}");
    let refused = broken(&json, |it| it["deps_at"][1] = 3.into());
    assert!(refused.contains("deps_at goes back"), "{refused}");
    let refused = broken(&json, |it| it["name_at"].as_array_mut().unwrap().pop().map(drop).unwrap());
    assert!(refused.contains("name_at of 3 offsets"), "{refused}");
    let refused = broken(&json, |it| it["first"] = u32::MAX.into());
    assert!(refused.contains("past u32"), "{refused}");
}

/// The plane keeps no results of its own: without a database a key done before runs again.
#[test]
fn without_a_database_a_key_done_before_runs_again() {
    let mut plane = Plane::new(config(1, 2)).unwrap();
    open(&mut plane, 1, 1, false);
    assert_eq!(runs(&add(&mut plane, 1, vec![keyed(1, &[], 1, 7)])), [1]);
    close(&mut plane, 1);
    assert_eq!(finished(&built(&mut plane, 1)), [(1, true)]);
    open(&mut plane, 2, 1, false);
    assert_eq!(runs(&add(&mut plane, 2, vec![keyed(2, &[], 1, 7)])), [2]);
    assert_eq!(plane.counts().remembered, 0);
}

/// A released request gives its room back once it finishes: its nodes and its id are no longer
/// known, and the id can be opened again.
#[test]
fn a_released_request_is_forgotten_once_it_finishes() {
    let mut plane = Plane::new(config(1, 1)).unwrap();
    open(&mut plane, 1, 1, false);
    assert_eq!(runs(&add(&mut plane, 1, vec![work(1, &[], 1), work(2, &[1], 1)])), [1]);
    close(&mut plane, 1);
    plane.release(RequestId(1)).unwrap();
    // Not yet finished: it is still known.
    assert_eq!(plane.state(NodeId(1)), Some(&NodeState::Running));
    assert_eq!(runs(&built(&mut plane, 1)), [2]);
    assert_eq!(finished(&built(&mut plane, 2)), [(1, true)]);
    assert_eq!(plane.state(NodeId(1)), None);
    assert_eq!(plane.state(NodeId(2)), None);
    let close_again = PlaneInput::Close { at_ns: 0, request: RequestId(1) };
    assert_eq!(plane.step(close_again), Err(Error::UnknownRequest(1)));
    // Its id and its nodes' ids are free again; the plane reuses the room.
    open(&mut plane, 1, 1, false);
    assert_eq!(runs(&add(&mut plane, 1, vec![work(2, &[], 1), work(1, &[2], 1)])), [2]);
    close(&mut plane, 1);
    assert_eq!(runs(&built(&mut plane, 2)), [1]);
    assert_eq!(finished(&built(&mut plane, 1)), [(1, true)]);
    assert_eq!(plane.release(RequestId(9)), Err(Error::UnknownRequest(9)));
}

#[test]
fn one_key_runs_once_for_two_requests() {
    let mut plane = Plane::new(config(1, 4)).unwrap();
    open(&mut plane, 1, 1, false);
    open(&mut plane, 2, 1, false);
    assert_eq!(runs(&add(&mut plane, 1, vec![keyed(1, &[], 1, 3)])), [1]);
    let out = add(&mut plane, 2, vec![keyed(2, &[], 1, 3), work(3, &[2], 1)]);
    assert_eq!(runs(&out), Vec::<u32>::new());
    assert_eq!(moves(&out), [(2, NodeState::Running), (3, NodeState::Waiting)]);
    close(&mut plane, 1);
    close(&mut plane, 2);
    // The joined node is not the task's: only its runner is said done.
    assert_eq!(plane.step(done(2, Outcome::Built, Vec::new())), Err(Error::NotRunning(NodeId(2))));
    let out = built(&mut plane, 1);
    assert_eq!(runs(&out), [3]);
    assert_eq!(finished(&out), [(1, true)]);
    assert_eq!(plane.state(NodeId(2)), Some(&NodeState::Built));
    assert_eq!(finished(&built(&mut plane, 3)), [(2, true)]);
    assert_eq!((plane.counts().ran, plane.counts().joined), (2, 1));
}

/// A task not yet started is ready in each request that waits for it, at that request's
/// priority: the first request whose turn it is starts it.
#[test]
fn a_joined_node_of_another_request_can_start_the_task() {
    let mut plane = Plane::new(config(1, 1)).unwrap();
    open(&mut plane, 1, 1, true);
    let out = add(&mut plane, 1, vec![work(1, &[], 100), work(2, &[], 50), keyed(3, &[], 1, 9)]);
    assert_eq!(runs(&out), [1]);
    open(&mut plane, 2, 1, true);
    assert_eq!(runs(&add(&mut plane, 2, vec![keyed(4, &[], 1, 9)])), Vec::<u32>::new());
    // Request 1 started a task, request 2 none: request 2's turn, and its one task is the key's.
    let out = built(&mut plane, 1);
    assert_eq!(runs(&out), [4]);
    assert_eq!(plane.state(NodeId(3)), Some(&NodeState::Running));
    let out = built(&mut plane, 4);
    assert_eq!(runs(&out), [2]);
    assert_eq!(plane.state(NodeId(3)), Some(&NodeState::Built));
}

#[test]
fn a_cancelled_request_skips_what_has_not_started_and_lets_what_runs_finish() {
    let mut plane = Plane::new(config(1, 1)).unwrap();
    open(&mut plane, 1, 1, true);
    let out = add(&mut plane, 1, vec![work(1, &[], 10), work(2, &[1], 1), work(3, &[], 1)]);
    assert_eq!(runs(&out), [1]);
    let out = plane.cancel(5, RequestId(1)).unwrap();
    assert!(matches!(
        &out[0],
        PlaneOutput::Said { record: Record { event: Event::Said { text, .. }, .. }, .. }
            if text == "request 1 is cancelled"
    ));
    assert_eq!(
        moves(&out),
        [(2, NodeState::Skipped { because: 1 }), (3, NodeState::Skipped { because: 1 })]
    );
    assert!(finished(&out).is_empty());
    assert!(plane.cancel(6, RequestId(1)).unwrap().is_empty());
    assert_eq!(
        plane.step(PlaneInput::Add { at_ns: 6, request: RequestId(1), nodes: vec![work(9, &[], 1)] }),
        Err(Error::Closed)
    );
    assert_eq!(plane.step(PlaneInput::Close { at_ns: 6, request: RequestId(1) }), Err(Error::Closed));
    // What the running task finds is added skipped; the request then finishes, not ok.
    let out = plane.step(done(1, Outcome::Built, vec![work(4, &[1], 1)])).unwrap();
    assert_eq!(
        moves(&out),
        [(1, NodeState::Built), (4, NodeState::Skipped { because: 1 })]
    );
    assert_eq!(runs(&out), Vec::<u32>::new());
    assert_eq!(finished(&out), [(1, false)]);
    assert_eq!(plane.cancel(7, RequestId(1)), Err(Error::Over));
    assert_eq!(plane.cancel(7, RequestId(8)), Err(Error::UnknownRequest(8)));
}

/// A node of another request that joined a task the cancelled request runs stays on it.
#[test]
fn a_cancel_leaves_another_request_on_the_task_it_ran() {
    let mut plane = Plane::new(config(1, 2)).unwrap();
    open(&mut plane, 1, 1, true);
    open(&mut plane, 2, 1, true);
    assert_eq!(runs(&add(&mut plane, 1, vec![keyed(1, &[], 1, 4)])), [1]);
    add(&mut plane, 2, vec![keyed(2, &[], 1, 4)]);
    close(&mut plane, 2);
    let out = plane.cancel(1, RequestId(1)).unwrap();
    assert!(moves(&out).is_empty());
    assert_eq!(plane.state(NodeId(2)), Some(&NodeState::Running));
    let out = built(&mut plane, 1);
    assert_eq!(moves(&out), [(1, NodeState::Built), (2, NodeState::Built)]);
    assert_eq!(finished(&out), [(1, false), (2, true)]);
}

/// A node of the cancelled request that joined another's running task leaves it at once; the
/// task runs on for the other.
#[test]
fn a_cancelled_request_does_not_wait_for_another_requests_task() {
    let mut plane = Plane::new(config(1, 2)).unwrap();
    open(&mut plane, 1, 1, true);
    open(&mut plane, 2, 1, true);
    assert_eq!(runs(&add(&mut plane, 1, vec![keyed(1, &[], 1, 4)])), [1]);
    add(&mut plane, 2, vec![keyed(2, &[], 1, 4), work(3, &[2], 1)]);
    let out = plane.cancel(1, RequestId(2)).unwrap();
    assert_eq!(
        moves(&out),
        [(2, NodeState::Skipped { because: 2 }), (3, NodeState::Skipped { because: 2 })]
    );
    assert_eq!(finished(&out), [(2, false)]);
    close(&mut plane, 1);
    let out = built(&mut plane, 1);
    assert_eq!(moves(&out), [(1, NodeState::Built)]);
    assert_eq!(finished(&out), [(1, true)]);
}

#[test]
fn a_task_bigger_than_the_budget_fails_naming_itself() {
    let mut plane = Plane::new(PlaneConfig { budget: budget(10), ..config(1, 2) }).unwrap();
    open(&mut plane, 1, 1, true);
    let out = add(
        &mut plane,
        1,
        vec![node(1, &[], Pool::Work, 1, None, 11), node(2, &[], Pool::Work, 1, None, 10)],
    );
    assert_eq!(runs(&out), [2]);
    let Some(NodeState::Failed { why }) = plane.state(NodeId(1)) else {
        panic!("{:?}", plane.state(NodeId(1)))
    };
    assert!(why.starts_with("<1> needs"), "{why}");
    assert!(why.contains("more than blaze may use here"), "{why}");
    assert!(why.contains("the test's budget"), "{why}");
    // One that fits the budget but not what the process holds, with nothing running, fails too.
    built(&mut plane, 2);
    plane.step(PlaneInput::Held { at_ns: 0, bytes: 8 }).unwrap();
    add(&mut plane, 1, vec![node(3, &[], Pool::Work, 1, None, 3)]);
    let Some(NodeState::Failed { why }) = plane.state(NodeId(3)) else {
        panic!("{:?}", plane.state(NodeId(3)))
    };
    assert!(why.starts_with("<3> needs"), "{why}");
    assert!(why.contains("with nothing of blaze's running"), "{why}");
}

/// A task passed over for memory `starve_after` times holds back every task with memory, of
/// any pool, until it fits, and then starts before the smaller ones behind it.
#[test]
fn a_task_passed_over_starts_before_smaller_ones_behind_it() {
    let mut plane =
        Plane::new(PlaneConfig { io: 2, work: 4, budget: budget(10), starve_after: 2 }).unwrap();
    open(&mut plane, 1, 1, true);
    let mut nodes = vec![
        node(1, &[], Pool::Work, 1000, None, 5),
        node(2, &[], Pool::Work, 100, None, 8),
    ];
    nodes.extend((3..9).map(|id| node(id, &[], Pool::Work, 10 - u64::from(id), None, 2)));
    let out = add(&mut plane, 1, nodes);
    assert_eq!(runs(&out), [1, 3, 4]);
    assert_eq!(plane.holding(), Some(NodeId(2)));
    assert_eq!(plane.granted(), 9);
    // Held back too: another pool's task with memory; not one with none.
    let out = add(
        &mut plane,
        1,
        vec![node(9, &[], Pool::Io, 1, None, 1), node(10, &[], Pool::Io, 1, None, 0)],
    );
    assert_eq!(runs(&out), [10]);
    assert_eq!(runs(&built(&mut plane, 3)), Vec::<u32>::new());
    assert_eq!(runs(&built(&mut plane, 1)), [2]);
    assert_eq!(plane.holding(), None);
    assert_eq!(plane.granted(), 10);
    let out = built(&mut plane, 2);
    assert_eq!(runs(&out), [9, 5, 6, 7]);
}

/// A task predicted the whole budget (a program never measured) runs alone: it waits while any
/// task granted memory runs, starts once none does though the process holds memory besides,
/// and while it runs only tasks with no memory start.
#[test]
fn a_task_predicted_the_whole_budget_runs_alone() {
    let mut plane =
        Plane::new(PlaneConfig { io: 1, work: 4, budget: budget(10), starve_after: 100 }).unwrap();
    open(&mut plane, 1, 1, true);
    plane.step(PlaneInput::Held { at_ns: 0, bytes: 4 }).unwrap();
    assert_eq!(runs(&add(&mut plane, 1, vec![node(1, &[], Pool::Work, 1000, None, 3)])), [1]);
    let out = add(
        &mut plane,
        1,
        vec![
            node(2, &[], Pool::Work, 100, None, 10),
            node(3, &[], Pool::Work, 10, None, 2),
            node(4, &[], Pool::Work, 1, None, 0),
        ],
    );
    assert_eq!(runs(&out), [3, 4], "the whole budget does not fit beside what runs");
    assert_eq!(runs(&built(&mut plane, 1)), Vec::<u32>::new());
    assert_eq!(runs(&built(&mut plane, 3)), [2], "nothing granted memory runs: it starts");
    assert_eq!(plane.granted(), 10);
    let out = add(&mut plane, 1, vec![node(5, &[], Pool::Work, 1, None, 1), node(6, &[], Pool::Work, 1, None, 0)]);
    assert_eq!(runs(&out), [6], "beside it, only what holds no memory");
    assert_eq!(runs(&built(&mut plane, 2)), [5]);
}

/// With one thread and every request ready, each request's starts keep to its weight.
#[test]
fn one_thread_is_shared_by_weight() {
    let mut plane = Plane::new(config(1, 1)).unwrap();
    open(&mut plane, 1, 1, true);
    open(&mut plane, 2, 3, true);
    let mut running = runs(&add(&mut plane, 1, (0..40).map(|id| work(id, &[], 1)).collect()));
    running.extend(runs(&add(&mut plane, 2, (100..140).map(|id| work(id, &[], 1)).collect())));
    let mut order = String::new();
    for _ in 0..16 {
        let id = running.pop().unwrap();
        order.push(if id < 100 { 'a' } else { 'b' });
        running.extend(runs(&built(&mut plane, id)));
    }
    // Request 2 comes in at the plane's virtual time, a task behind request 1, and ties go to
    // the heavier.
    assert_eq!(order, "abbbbabbbabbbabb");
}

/// Requests always ready, on any number of threads, finishing in any order: no request's
/// starts run ahead of its share by more than one task (stride scheduling's bound: for any
/// two, `(started_i - 1) / weight_i <= started_j / weight_j`).
#[test]
fn fair_share_keeps_each_request_to_its_weight_within_one_task() {
    for seed in 0..200u64 {
        let mut random = SplitMix64::new(seed);
        let threads = 1 + random.below(6) as usize;
        let mut plane = Plane::new(config(1, threads)).unwrap();
        // A request that takes every thread first, so the others all come in at once.
        open(&mut plane, 0, 1, true);
        let first: Vec<Spec> = (0..threads as u32).map(|id| work(id, &[], 1)).collect();
        let mut running: Vec<u32> = runs(&add(&mut plane, 0, first));
        assert_eq!(running.len(), threads);
        let requests = 2 + random.below(4) as usize;
        let weights: Vec<u64> = (0..requests).map(|_| 1 + random.below(6)).collect();
        const TASKS: u32 = 300;
        for (at, &weight) in weights.iter().enumerate() {
            let r = at as u32 + 1;
            open(&mut plane, r, weight as u32, true);
            let nodes = (0..TASKS).map(|it| work(r * 1000 + it, &[], random.below(5))).collect();
            assert_eq!(runs(&add(&mut plane, r, nodes)), Vec::<u32>::new());
        }
        let mut started = vec![0u64; requests];
        'steps: loop {
            let id = running.swap_remove(random.below(running.len() as u64) as usize);
            for ran in runs(&built(&mut plane, id)) {
                running.push(ran);
                let at = (ran / 1000) as usize - 1;
                started[at] += 1;
                if started.iter().any(|&it| it + 1 >= u64::from(TASKS)) {
                    break 'steps;
                }
                for i in 0..requests {
                    for j in 0..requests {
                        assert!(
                            started[i].saturating_sub(1) * weights[j] <= started[j] * weights[i],
                            "seed {seed}: started {started:?} for weights {weights:?}"
                        );
                    }
                }
            }
        }
    }
}

/// A request of the simulation, as its driver sees it.
struct Planned {
    weight: u32,
    keep_going: bool,
    idle: bool,
    opened: bool,
    closed: bool,
    batches: u32,
}

/// What the simulation knows of the plane, from what it said, to check it after every step.
#[derive(Default)]
struct Model {
    seed: u64,
    budget: Option<u64>,
    specs: HashMap<u32, Spec>,
    request_of: HashMap<u32, u32>,
    nodes_of: HashMap<u32, Vec<u32>>,
    dependents: HashMap<u32, Vec<u32>>,
    states: HashMap<u32, NodeState>,
    /// The tasks running, by the node they run as.
    running: Vec<u32>,
    running_keys: HashSet<Key>,
    granted: u64,
    /// What the database has: keys whose task ended well, some that failed, less those
    /// dropped since.
    db: Db,
    finished: HashMap<u32, bool>,
    cancelled: HashSet<u32>,
    /// The requests idle now.
    idle: HashSet<u32>,
    /// The running tasks asked to give way, by the node they run as.
    yielding: HashSet<u32>,
    /// How many `Run`s each request was said, less those that gave way.
    ran: HashMap<u32, u64>,
    outputs: Vec<PlaneOutput>,
}

impl Model {
    fn register(&mut self, request: u32, nodes: &[Spec]) {
        for spec in nodes {
            self.request_of.insert(spec.id.0, request);
            self.nodes_of.entry(request).or_default().push(spec.id.0);
            for dep in &spec.deps {
                self.dependents.entry(dep.0).or_default().push(spec.id.0);
            }
            self.specs.insert(spec.id.0, spec.clone());
        }
    }

    /// The longest path of expected cost from `id` to the end of its request.
    fn priority(&self, id: u32, memo: &mut HashMap<u32, u64>) -> u64 {
        if let Some(&it) = memo.get(&id) {
            return it;
        }
        let after = self
            .dependents
            .get(&id)
            .map(|it| it.iter().map(|&it| self.priority(it, memo)).max().unwrap_or(0))
            .unwrap_or(0);
        let priority = self.specs[&id].cost.saturating_add(after);
        memo.insert(id, priority);
        priority
    }

    fn done_well(&self, id: u32) -> bool {
        matches!(self.states.get(&id), Some(NodeState::Built | NodeState::Cached { .. }))
    }

    /// Takes the outputs of one step, checking each; `holding` is the node the plane said held
    /// back the others before it.
    fn take(&mut self, plane: &Plane<DbMemo>, outputs: Vec<PlaneOutput>, holding: Option<NodeId>) {
        let seed = self.seed;
        let mut first_with_memory = true;
        let mut held_settled = false;
        for output in &outputs {
            match output {
                PlaneOutput::Said { request, record } => match &record.event {
                    Event::Node { id, .. } => {
                        assert_eq!(self.request_of[id], request.0, "seed {seed}: #{id}");
                    }
                    Event::Nodes { .. } => {
                        let mut order = Order::default();
                        let batch = record.event.nodes(&mut order).unwrap();
                        let batch = batch.unwrap_or_else(|why| panic!("seed {seed}: {why}"));
                        for node in batch.iter() {
                            let id = node.id;
                            assert_eq!(self.request_of[&id], request.0, "seed {seed}: #{id}");
                            assert!(!self.states.contains_key(&id), "seed {seed}: #{id} moved first");
                            let spec = &self.specs[&id];
                            assert_eq!((node.kind, node.name), (spec.kind, spec.name.as_str()));
                            let deps = spec.deps.iter().map(|it| it.0);
                            assert!(node.deps.iter().copied().eq(deps), "seed {seed}: #{id}");
                        }
                    }
                    Event::Moved { id, state } => {
                        assert_eq!(self.request_of[id], request.0, "seed {seed}: #{id}");
                        if let Some(was) = self.states.get(id) {
                            assert!(!was.settled(), "seed {seed}: #{id} moved after it settled");
                        }
                        if matches!(state, NodeState::Running | NodeState::Ready) {
                            for dep in &self.specs[id].deps {
                                assert!(
                                    self.done_well(dep.0),
                                    "seed {seed}: #{id} {state:?} before #{dep} is done"
                                );
                            }
                        }
                        if let NodeState::Failed { why } = state
                            && why != "boom"
                        {
                            let spec = &self.specs[id];
                            assert!(why.starts_with(&spec.name), "seed {seed}: {why}");
                            let budget = self.budget.expect("a failure of memory has a budget");
                            if why.contains("more than blaze may use here") {
                                assert!(spec.memory > budget, "seed {seed}: {why}");
                            } else {
                                assert!(why.contains("with nothing of blaze's running"));
                                assert!(self.running.is_empty(), "seed {seed}: {why}");
                            }
                        }
                        if state.settled() && holding == Some(NodeId(*id)) {
                            held_settled = true;
                        }
                        self.states.insert(*id, state.clone());
                    }
                    Event::Critical { path } => {
                        for pair in path.windows(2) {
                            assert!(
                                self.specs[&pair[1]].deps.contains(&NodeId(pair[0])),
                                "seed {seed}: critical {path:?}"
                            );
                        }
                    }
                    Event::Said { .. } => {
                        assert!(self.cancelled.contains(&request.0), "seed {seed}");
                    }
                    other => panic!("seed {seed}: the plane said {other:?}"),
                },
                PlaneOutput::Run { id, pool, critical, .. } => {
                    let spec = &self.specs[&id.0];
                    assert_eq!(*pool, spec.pool, "seed {seed}");
                    assert_eq!(self.states[&id.0], NodeState::Running, "seed {seed}: #{id}");
                    // Critical exactly when nothing ready or running of its request has a
                    // longer path to go, counted from the whole graph.
                    let mut memo = HashMap::default();
                    let mine = self.priority(id.0, &mut memo);
                    let most = self.nodes_of[&self.request_of[&id.0]]
                        .iter()
                        .filter(|it| {
                            matches!(self.states.get(it), Some(NodeState::Ready | NodeState::Running))
                        })
                        .map(|it| self.priority(*it, &mut memo))
                        .max()
                        .unwrap_or(0);
                    assert_eq!(*critical, mine >= most, "seed {seed}: #{id} at {mine}, most {most}");
                    for dep in &spec.deps {
                        assert!(self.done_well(dep.0), "seed {seed}: #{id} runs before #{dep}");
                    }
                    if let Some(key) = spec.key {
                        assert!(self.running_keys.insert(key), "seed {seed}: {key:?} runs twice");
                        assert!(
                            !self.db.lock().contains_key(&key),
                            "seed {seed}: #{id} runs a key the database has"
                        );
                    }
                    self.granted += spec.memory;
                    if let Some(budget) = self.budget {
                        assert!(self.granted <= budget, "seed {seed}: {} granted", self.granted);
                    }
                    if spec.memory > 0 && first_with_memory {
                        first_with_memory = false;
                        if let Some(held) = holding
                            && !held_settled
                        {
                            let held = &self.specs[&held.0];
                            let same = held.id == spec.id
                                || (held.key.is_some() && held.key == spec.key);
                            assert!(same, "seed {seed}: #{id} starts while {} is held", held.id);
                        }
                    }
                    if plane.config().budget.is_none() {
                        let mut memo = HashMap::default();
                        let request = self.request_of[&id.0];
                        let mine = self.priority(id.0, &mut memo);
                        for other in &self.nodes_of[&request] {
                            if self.states.get(other) == Some(&NodeState::Ready)
                                && self.specs[other].pool == spec.pool
                            {
                                let theirs = self.priority(*other, &mut memo);
                                assert!(
                                    theirs <= mine,
                                    "seed {seed}: #{id} at {mine} before #{other} at {theirs}"
                                );
                            }
                        }
                    }
                    // With no budget, an idle request's task starts only while no request not
                    // idle has a task ready in its pool.
                    if self.budget.is_none() && self.idle.contains(&self.request_of[&id.0]) {
                        for (other, state) in &self.states {
                            let theirs = self.request_of[other];
                            assert!(
                                *state != NodeState::Ready
                                    || self.idle.contains(&theirs)
                                    || self.specs[other].pool != spec.pool,
                                "seed {seed}: idle #{id} starts while #{other} is ready"
                            );
                        }
                    }
                    self.running.push(id.0);
                    *self.ran.entry(self.request_of[&id.0]).or_default() += 1;
                }
                PlaneOutput::Yield { id } => {
                    assert!(self.running.contains(&id.0), "seed {seed}: #{id} yields, not running");
                    let r = self.request_of[&id.0];
                    assert!(self.idle.contains(&r), "seed {seed}: #{id} of request {r} yields");
                    assert!(self.yielding.insert(id.0), "seed {seed}: #{id} asked twice");
                }
                PlaneOutput::Finished { request, ok } => {
                    let r = request.0;
                    assert!(self.finished.insert(r, *ok).is_none(), "seed {seed}: twice");
                    let nodes = self.nodes_of.get(&r).map_or(&[][..], Vec::as_slice);
                    assert!(nodes.iter().all(|it| self.states[it].settled()), "seed {seed}");
                    let failed = nodes
                        .iter()
                        .any(|it| matches!(self.states[it], NodeState::Failed { .. }));
                    assert_eq!(*ok, !failed && !self.cancelled.contains(&r), "seed {seed}");
                    // Released, it is gone once finished; kept, it says what it ran.
                    if let Some(stats) = plane.stats(*request) {
                        let ran = self.ran.get(&r).copied().unwrap_or(0);
                        assert_eq!(stats.ran, ran, "seed {seed}: request {r}");
                        let settled = stats.ran + stats.cached + stats.joined;
                        assert!(settled <= nodes.len() as u64, "seed {seed}: {stats:?}");
                    }
                }
            }
        }
        assert_eq!(plane.granted(), self.granted, "seed {seed}");
        // Nothing waits for a thread while nothing runs.
        if self.running.is_empty() {
            for (id, state) in &self.states {
                assert_ne!(*state, NodeState::Ready, "seed {seed}: #{id} ready, nothing runs");
            }
        }
        if let Some(held) = plane.holding() {
            assert!(self.budget.is_some(), "seed {seed}");
            assert_eq!(self.states[&held.0], NodeState::Ready, "seed {seed}");
        }
        self.outputs.extend(outputs);
    }

    /// A running task is done: what the model keeps of it, and what its work puts in the
    /// database before the plane is told: the value, and a failure too if `keep_failure`.
    fn finish(&mut self, id: u32, outcome: &Outcome, keep_failure: bool) -> Option<(Key, Outcome)> {
        self.running.retain(|&it| it != id);
        self.yielding.remove(&id);
        let spec = &self.specs[&id];
        self.granted -= spec.memory;
        let key = spec.key?;
        self.running_keys.remove(&key);
        (keep_failure || !matches!(outcome, Outcome::Failed { .. })).then(|| (key, outcome.clone()))
    }

    /// A running task gave way: it holds nothing, runs no key, and was no start.
    fn yielded(&mut self, id: u32) {
        self.running.retain(|&it| it != id);
        self.yielding.remove(&id);
        let spec = &self.specs[&id];
        self.granted -= spec.memory;
        if let Some(key) = spec.key {
            self.running_keys.remove(&key);
        }
        let r = self.request_of[&id];
        *self.ran.get_mut(&r).expect("a start of its request") -= 1;
    }
}

/// What a simulation does to its plane and database, in order, so the reference can be put
/// through the same.
#[derive(Debug, Clone)]
enum Act {
    Step(PlaneInput),
    Cancel(u64, RequestId),
    Release(RequestId),
    /// The work puts a value in the database.
    Put(Key, Outcome),
    /// The database drops a value.
    Drop(Key),
}

/// What a plane says of itself after an act: of requests `0..` and nodes `0..` as many as
/// were named so far.
#[derive(Debug, PartialEq)]
struct Seen {
    holding: Option<NodeId>,
    granted: u64,
    counts: Counts,
    stats: Vec<Option<RequestStats>>,
    states: Vec<Option<NodeState>>,
}

impl Seen {
    fn of(plane: &Plane<DbMemo>, requests: u32, nodes: u32) -> Seen {
        Seen {
            holding: plane.holding(),
            granted: plane.granted(),
            counts: plane.counts(),
            stats: (0..requests).map(|r| plane.stats(RequestId(r))).collect(),
            states: (0..nodes).map(|id| plane.state(NodeId(id)).cloned()).collect(),
        }
    }

    fn of_reference(plane: &reference::Plane<impl Memo>, requests: u32, nodes: u32) -> Seen {
        Seen {
            holding: plane.holding(),
            granted: plane.granted(),
            counts: plane.counts(),
            stats: (0..requests).map(|r| plane.stats(RequestId(r))).collect(),
            states: (0..nodes).map(|id| plane.state(NodeId(id)).cloned()).collect(),
        }
    }
}

/// A simulation's plane, driven through one buffer of outputs as a long-lived caller would,
/// and what was done to it.
struct Driver {
    plane: Plane<DbMemo>,
    db: Db,
    buffer: Vec<PlaneOutput>,
    requests: u32,
    acts: Vec<(Act, Vec<PlaneOutput>, Seen)>,
}

impl Driver {
    /// Does `act`, `nodes` ids being named so far: what the plane said.
    fn act(&mut self, act: Act, nodes: u32) -> Vec<PlaneOutput> {
        assert!(self.buffer.is_empty());
        match &act {
            Act::Step(input) => self.plane.step_into(input.clone(), &mut self.buffer).unwrap(),
            Act::Cancel(at_ns, request) => {
                self.plane.cancel_into(*at_ns, *request, &mut self.buffer).unwrap()
            }
            Act::Release(request) => self.plane.release(*request).unwrap(),
            Act::Put(key, outcome) => _ = self.db.lock().insert(*key, outcome.clone()),
            Act::Drop(key) => _ = self.db.lock().remove(key),
        }
        let outputs: Vec<PlaneOutput> = std::mem::take(&mut self.buffer);
        let seen = Seen::of(&self.plane, self.requests, nodes);
        self.acts.push((act, outputs.clone(), seen));
        outputs
    }
}

/// What one random run of many requests did.
struct Simulated {
    config: PlaneConfig,
    outputs: Vec<PlaneOutput>,
    steps: u64,
    /// Each act, what the plane said to it, and what it said of itself after.
    acts: Vec<(Act, Vec<PlaneOutput>, Seen)>,
}

/// Fisher-Yates, by the seeded generator.
fn shuffle<T>(random: &mut SplitMix64, items: &mut [T]) {
    for at in (1..items.len()).rev() {
        items.swap(at, random.below(at as u64 + 1) as usize);
    }
}

/// One random run of many requests, checked after every step. With `release`, each request
/// is released as it is opened; with `idle`, some are opened idle, some of those promoted,
/// and a task asked to give way mostly does. Without, the run is that of before idle
/// requests were.
fn simulate(seed: u64, release: bool, idle: bool) -> Simulated {
    let mut random = SplitMix64::new(seed);
    let budget = (!random.one_in(3)).then(|| 100 + random.below(900));
    let starve_after = random.below(5) as u32;
    let config = PlaneConfig {
        io: 1 + random.below(3) as usize,
        work: 1 + random.below(4) as usize,
        budget: budget.and_then(self::budget),
        starve_after,
    };
    let (plane, db) = with_db(config.clone());
    let keys = 1 + random.below(12) as u8;
    let memory = |random: &mut SplitMix64| match budget {
        None => random.below(1000),
        Some(budget) if random.one_in(15) => budget + 1 + random.below(budget),
        Some(budget) if random.one_in(4) => 0,
        Some(budget) => random.below(budget / 2 + 1),
    };
    let by_key: Vec<(u64, Pool)> = (0..keys)
        .map(|_| (memory(&mut random), if random.one_in(3) { Pool::Io } else { Pool::Work }))
        .collect();
    let mut model = Model { seed, budget, db: Arc::clone(&db), ..Model::default() };
    let mut planned: Vec<Planned> = (0..1 + random.below(5))
        .map(|_| Planned {
            weight: 1 + random.below(4) as u32,
            keep_going: !random.one_in(3),
            idle: idle && random.one_in(3),
            opened: false,
            closed: false,
            batches: 1 + random.below(4) as u32,
        })
        .collect();
    let mut driver =
        Driver { plane, db, buffer: Vec::new(), requests: planned.len() as u32, acts: Vec::new() };
    let mut next_id = 0u32;
    // Each node after its deps, a third of the batches then shuffled: the plane puts them in
    // order.
    let new_nodes =
        |random: &mut SplitMix64, model: &Model, next_id: &mut u32, request: u32, count: u64| {
            let mut nodes: Vec<Spec> = Vec::new();
            let mut known: Vec<u32> = model.nodes_of.get(&request).cloned().unwrap_or_default();
            for _ in 0..count {
                let mut deps: Vec<u32> = (0..random.below(4))
                    .filter(|_| !known.is_empty())
                    .map(|_| known[random.below(known.len() as u64) as usize])
                    .collect();
                deps.sort_unstable();
                deps.dedup();
                let id = *next_id;
                *next_id += 1;
                let (key, memory, pool) = if random.one_in(2) {
                    let k = random.below(u64::from(keys)) as u8;
                    (Some(key(k)), by_key[usize::from(k)].0, by_key[usize::from(k)].1)
                } else {
                    let pool = if random.one_in(3) { Pool::Io } else { Pool::Work };
                    (None, memory(random), pool)
                };
                nodes.push(node(id, &deps, pool, random.below(10), key, memory));
                known.push(id);
            }
            if random.one_in(3) {
                shuffle(random, &mut nodes);
            }
            nodes
        };
    let mut at = 0u64;
    let mut steps = 0u64;
    loop {
        let all_over = planned.iter().enumerate().all(|(r, it)| {
            it.opened && model.finished.contains_key(&(r as u32))
        });
        if all_over {
            break;
        }
        steps += 1;
        assert!(steps < 100_000, "seed {seed}: no end");
        at += random.below(3);
        let holding = driver.plane.holding();
        let roll = random.below(100);
        let live: Vec<u32> = (0..planned.len() as u32)
            .filter(|r| planned[*r as usize].opened && !model.finished.contains_key(r))
            .collect();
        let outputs = if roll < 3 && !live.is_empty() {
            let r = live[random.below(live.len() as u64) as usize];
            model.cancelled.insert(r);
            planned[r as usize].closed = true;
            driver.act(Act::Cancel(at, RequestId(r)), next_id)
        } else if roll < 8 && budget.is_some() {
            let budget = budget.unwrap_or(0);
            let bytes = if random.one_in(10) { random.below(budget * 2) } else { random.below(budget / 3) };
            driver.act(Act::Step(PlaneInput::Held { at_ns: at, bytes }), next_id)
        } else if roll < 10 && !model.db.lock().is_empty() {
            let dropped = {
                let db = model.db.lock();
                let mut kept: Vec<Key> = db.keys().copied().collect();
                kept.sort_unstable();
                kept[random.below(kept.len() as u64) as usize]
            };
            driver.act(Act::Drop(dropped), next_id)
        } else if idle
            && (45..48).contains(&roll)
            && live.iter().any(|r| model.idle.contains(r))
        {
            let idle: Vec<u32> = live.iter().copied().filter(|r| model.idle.contains(r)).collect();
            let r = idle[random.below(idle.len() as u64) as usize];
            model.idle.remove(&r);
            driver.act(Act::Step(PlaneInput::Promote { at_ns: at, request: RequestId(r) }), next_id)
        } else if roll < 45 || model.running.is_empty() {
            let unopened = planned.iter().position(|it| !it.opened);
            let unclosed: Vec<usize> =
                (0..planned.len()).filter(|&r| planned[r].opened && !planned[r].closed).collect();
            if let Some(r) = unopened
                && (unclosed.is_empty() || random.one_in(2))
            {
                let it = &mut planned[r];
                it.opened = true;
                let request = RequestId(r as u32);
                let open = PlaneInput::Open {
                    at_ns: at,
                    request,
                    weight: it.weight,
                    keep_going: it.keep_going,
                    idle: it.idle,
                };
                if it.idle {
                    model.idle.insert(r as u32);
                }
                let outputs = driver.act(Act::Step(open), next_id);
                if release {
                    assert!(driver.act(Act::Release(request), next_id).is_empty());
                }
                outputs
            } else if !unclosed.is_empty() {
                let r = unclosed[random.below(unclosed.len() as u64) as usize];
                let request = RequestId(r as u32);
                if planned[r].batches > 0 {
                    planned[r].batches -= 1;
                    let count = 1 + random.below(8);
                    let nodes = new_nodes(&mut random, &model, &mut next_id, r as u32, count);
                    model.register(r as u32, &nodes);
                    // A batch that is a run of ids, half the time given as columns.
                    let run = nodes.windows(2).all(|it| it[1].id.0 == it[0].id.0 + 1);
                    let input = if run && random.one_in(2) {
                        let nodes = as_nodes(nodes[0].id.0, &nodes);
                        PlaneInput::AddNodes { at_ns: at, request, nodes }
                    } else {
                        PlaneInput::Add { at_ns: at, request, nodes }
                    };
                    driver.act(Act::Step(input), next_id)
                } else {
                    planned[r].closed = true;
                    driver.act(Act::Step(PlaneInput::Close { at_ns: at, request }), next_id)
                }
            } else {
                assert!(
                    !model.running.is_empty(),
                    "seed {seed}: nothing runs, every request is closed, and not all finished"
                );
                continue;
            }
        } else {
            let id = model.running[random.below(model.running.len() as u64) as usize];
            if model.yielding.contains(&id) && !random.one_in(4) {
                model.yielded(id);
                driver.act(Act::Step(PlaneInput::Yielded { at_ns: at, id: NodeId(id) }), next_id)
            } else {
                let outcome = match random.below(10) {
                    0 => Outcome::Failed { why: "boom".to_owned() },
                    1 => Outcome::Cached { remote: random.one_in(2) },
                    _ => Outcome::Built,
                };
                let r = model.request_of[&id];
                let added = if random.one_in(4) {
                    let count = 1 + random.below(3);
                    new_nodes(&mut random, &model, &mut next_id, r, count)
                } else {
                    Vec::new()
                };
                model.register(r, &added);
                let keep_failure = random.one_in(2);
                if let Some((key, outcome)) = model.finish(id, &outcome, keep_failure) {
                    assert!(driver.act(Act::Put(key, outcome), next_id).is_empty());
                }
                let group = (!random.one_in(3)).then(|| random.below(4) as u32);
                let input = PlaneInput::Done {
                    at_ns: at,
                    id: NodeId(id),
                    outcome,
                    added,
                    group,
                    peak: random.below(1 << 20),
                };
                driver.act(Act::Step(input), next_id)
            }
        };
        model.take(&driver.plane, outputs, holding);
    }
    assert!(model.running.is_empty(), "seed {seed}");
    for (id, state) in &model.states {
        assert!(state.settled(), "seed {seed}: #{id} {state:?} after every request finished");
    }
    assert_eq!(model.states.len(), model.specs.len(), "seed {seed}");
    for id in model.specs.keys() {
        assert_eq!(driver.plane.state(NodeId(*id)).is_some(), !release, "seed {seed}: #{id}");
    }
    Simulated { config, outputs: std::mem::take(&mut model.outputs), steps, acts: driver.acts }
}

/// Random requests under many seeds, checked after every step: nothing runs before its deps;
/// no key runs twice at once; a key the database has never runs; granted memory never exceeds
/// the budget; a held task starts before any other with memory; a failure for memory names its
/// node; the critical path goes first within a request, as priorities kept as nodes are added
/// match those counted again from the whole graph; every request ends, ok exactly when nothing
/// of it failed and it was not cancelled; with no budget an idle request's task starts only
/// while no other has one ready in its pool, and only an idle request's running task is asked
/// to give way, once.
#[test]
fn random_requests_keep_the_rules() {
    let mut steps = 0;
    let mut critical = [0u64; 2];
    let mut yields = 0;
    for seed in 0..600u64 {
        for idle in [false, true] {
            let run = simulate(seed, false, idle);
            steps += run.steps;
            for output in &run.outputs {
                match output {
                    PlaneOutput::Run { critical: it, .. } => critical[usize::from(*it)] += 1,
                    PlaneOutput::Yield { .. } => {
                        assert!(idle, "seed {seed}: a yield with no idle request");
                        yields += 1;
                    }
                    _ => {}
                }
            }
        }
    }
    assert!(steps > 10_000, "{steps} steps");
    assert!(critical.iter().all(|&it| it > 100), "runs off and on the critical path: {critical:?}");
    assert!(yields > 20, "{yields} yields");
}

#[test]
fn the_same_seed_gives_the_same_outputs() {
    for seed in 0..60u64 {
        let a = simulate(seed, false, true);
        let b = simulate(seed, false, true);
        assert_eq!(a.outputs, b.outputs, "seed {seed} after {} steps", a.steps);
    }
}

/// Room given back and taken again changes nothing the plane says: each finished request's
/// room is reused by the next, and the outputs are those of a plane that keeps everything.
#[test]
fn releasing_requests_changes_no_output() {
    for seed in 0..300u64 {
        for idle in [false, true] {
            let kept = simulate(seed, false, idle);
            let released = simulate(seed, true, idle);
            assert_eq!(kept.outputs, released.outputs, "seed {seed} after {} steps", kept.steps);
        }
    }
}

/// Puts the reference through what `simulated` did to the plane, and checks that it says the
/// same to each act and of itself after it.
fn replay(seed: u64, simulated: &Simulated) {
    let db = Db::default();
    let mut model = reference::Plane::with_memo(simulated.config.clone(), DbMemo(Arc::clone(&db))).unwrap();
    assert_eq!(model.config(), &simulated.config);
    let requests = simulated.acts.first().map_or(0, |(_, _, seen)| seen.stats.len() as u32);
    for (at, (act, said, seen)) in simulated.acts.iter().enumerate() {
        let outputs = match act {
            Act::Step(input) => model.step(input.clone()).unwrap(),
            Act::Cancel(at_ns, request) => model.cancel(*at_ns, *request).unwrap(),
            Act::Release(request) => {
                model.release(*request).unwrap();
                Vec::new()
            }
            Act::Put(key, outcome) => {
                db.lock().insert(*key, outcome.clone());
                Vec::new()
            }
            Act::Drop(key) => {
                db.lock().remove(key);
                Vec::new()
            }
        };
        assert_eq!(&outputs, said, "seed {seed}: act {at}, {act:?}");
        let theirs = Seen::of_reference(&model, requests, seen.states.len() as u32);
        assert_eq!(&theirs, seen, "seed {seed}: act {at}, {act:?}");
    }
}

/// The plane in columns says what the plane before them said, act by act over the random
/// runs, kept or released: the same outputs, and after each act the same states, stats,
/// counts, granted memory and held task. So the heaps keep the ready sets' order, the edge
/// lists the dependents' order, the member lists the members' order, and a reused slot or
/// buffer changes nothing.
#[test]
fn the_plane_says_what_the_reference_says() {
    let (mut acts, mut yielded) = (0, 0);
    for seed in 0..300u64 {
        for (release, idle) in [(false, false), (true, false), (false, true), (true, true)] {
            let simulated = simulate(seed, release, idle);
            acts += simulated.acts.len();
            yielded += simulated
                .acts
                .iter()
                .filter(|(act, ..)| matches!(act, Act::Step(PlaneInput::Yielded { .. })))
                .count();
            replay(seed, &simulated);
        }
    }
    assert!(acts > 20_000, "{acts} acts");
    assert!(yielded > 20, "{yielded} tasks gave way");
}

/// Batches given in any order, keyed or not, with deps on nodes before them or of the same
/// batch, and some refused (an id added twice, a dep not added, a cycle, a node waiting for
/// itself): the plane takes or refuses each as the reference does, with the same outputs, and
/// one refused changes nothing.
#[test]
fn batches_in_any_order_and_refused_ones_match_the_reference() {
    let batch = |random: &mut SplitMix64, known: &[u32], next: &mut u32| {
        let count = 1 + random.below(8) as u32;
        let fresh: Vec<u32> = (*next..*next + count).collect();
        *next += count;
        let mut nodes = Vec::new();
        for (at, &id) in fresh.iter().enumerate() {
            let mut deps = Vec::new();
            for _ in 0..random.below(4) {
                let (roll, coin) = (random.below(40), random.one_in(2));
                let dep = if roll == 0 {
                    1_000_000
                } else if roll <= 2 {
                    fresh[random.below(u64::from(count)) as usize]
                } else if !known.is_empty() && coin {
                    known[random.below(known.len() as u64) as usize]
                } else if at > 0 {
                    fresh[random.below(at as u64) as usize]
                } else {
                    continue;
                };
                deps.push(dep);
            }
            let id = if !known.is_empty() && random.one_in(50) {
                known[random.below(known.len() as u64) as usize]
            } else {
                id
            };
            let key = random.one_in(3).then(|| self::key(random.below(4) as u8));
            let pool = if random.one_in(3) { Pool::Io } else { Pool::Work };
            nodes.push(node(id, &deps, pool, random.below(10), key, 0));
        }
        shuffle(random, &mut nodes);
        (nodes, fresh)
    };
    let mut refused = 0;
    for seed in 0..400u64 {
        let mut random = SplitMix64::new(seed);
        let config = PlaneConfig {
            io: 1 + random.below(2) as usize,
            work: 1 + random.below(3) as usize,
            budget: None,
            starve_after: 0,
        };
        let mut plane = Plane::new(config.clone()).unwrap();
        let mut model = reference::Plane::new(config).unwrap();
        let mut both = |plane: &mut Plane, model: &mut reference::Plane, input: PlaneInput| {
            let got = plane.step(input.clone());
            let want = model.step(input);
            assert_eq!(got, want, "seed {seed}");
            refused += usize::from(got.is_err());
            got
        };
        let request = RequestId(0);
        let keep_going = random.one_in(2);
        let open = PlaneInput::Open { at_ns: 0, request, weight: 1, keep_going, idle: false };
        both(&mut plane, &mut model, open).unwrap();
        let (mut known, mut running, mut next, mut at) = (Vec::new(), Vec::new(), 0u32, 0u64);
        for _ in 0..12 {
            let (nodes, fresh) = batch(&mut random, &known, &mut next);
            let add = PlaneInput::Add { at_ns: at, request, nodes };
            if let Ok(out) = both(&mut plane, &mut model, add) {
                known.extend(fresh);
                running.extend(runs(&out));
            }
            for _ in 0..random.below(3) {
                if running.is_empty() {
                    break;
                }
                let id = running.swap_remove(random.below(running.len() as u64) as usize);
                at += 1 + random.below(3);
                let outcome = if random.one_in(8) {
                    Outcome::Failed { why: "no".to_owned() }
                } else {
                    Outcome::Built
                };
                let (added, fresh) = if random.one_in(3) {
                    batch(&mut random, &known, &mut next)
                } else {
                    (Vec::new(), Vec::new())
                };
                let input =
                    PlaneInput::Done { at_ns: at, id: NodeId(id), outcome, added, group: None, peak: 0 };
                match both(&mut plane, &mut model, input) {
                    Ok(out) => {
                        known.extend(fresh);
                        running.extend(runs(&out));
                    }
                    Err(_) => running.push(id),
                }
            }
        }
        both(&mut plane, &mut model, PlaneInput::Close { at_ns: at, request }).unwrap();
        while let Some(id) = running.pop() {
            at += 1;
            let out = both(&mut plane, &mut model, done(id, Outcome::Built, Vec::new())).unwrap();
            running.extend(runs(&out));
        }
        for id in 0..next {
            assert_eq!(plane.state(NodeId(id)), model.state(NodeId(id)), "seed {seed}: #{id}");
        }
        assert_eq!(plane.counts(), model.counts(), "seed {seed}");
    }
    assert!(refused > 100, "{refused} refused");
}

/// A long-lived plane reuses its rooms: requests opened, run and released one after another
/// leave it holding no more slots than the largest of them needed, and say what a fresh plane
/// says to each.
#[test]
fn a_long_lived_plane_says_to_each_request_what_a_fresh_one_says() {
    let mut long = Plane::new(config(2, 3)).unwrap();
    let mut buffer = Vec::new();
    let mut at = 0;
    for round in 0..50u32 {
        let mut random = SplitMix64::new(u64::from(round));
        let count = 1 + random.below(40) as u32;
        let nodes: Vec<Spec> = (0..count)
            .map(|id| {
                let deps: Vec<u32> = (0..id).filter(|_| random.one_in(4)).collect();
                let key = random.one_in(2).then(|| key(random.below(6) as u8));
                node(id, &deps, Pool::Work, random.below(10), key, 0)
            })
            .collect();
        let mut fresh = Plane::new(config(2, 3)).unwrap();
        let request = RequestId(round % 3);
        let mut both = |input: PlaneInput, fresh: &mut Plane| {
            let want = fresh.step(input.clone()).unwrap();
            long.step_into(input, &mut buffer).unwrap();
            let got: Vec<PlaneOutput> = std::mem::take(&mut buffer);
            // A request's id and its nodes' ids are the same each round; only the plane's
            // order of adding and its clock move on, and the clock never goes back, so the
            // fresh plane is given the same times.
            assert_eq!(got, want, "round {round}");
            got
        };
        let open = PlaneInput::Open { at_ns: at, request, weight: 1, keep_going: true, idle: false };
        both(open, &mut fresh);
        let mut running = runs(&both(PlaneInput::Add { at_ns: at, request, nodes }, &mut fresh));
        both(PlaneInput::Close { at_ns: at, request }, &mut fresh);
        let mut over = false;
        while let Some(id) = running.pop() {
            at += 1;
            let input = PlaneInput::Done {
                at_ns: at,
                id: NodeId(id),
                outcome: Outcome::Built,
                added: Vec::new(),
                group: None,
                peak: 0,
            };
            let out = both(input, &mut fresh);
            over |= !finished(&out).is_empty();
            running.extend(runs(&out));
        }
        assert!(over, "round {round}");
        long.release(request).unwrap();
        assert_eq!(long.stats(request), None);
    }
    let (nodes, tasks, requests) = long.slots();
    assert!(nodes <= 40, "{nodes} node slots");
    assert!(tasks <= 40, "{tasks} task slots");
    assert_eq!(requests, 1);
}

fn yields(outputs: &[PlaneOutput]) -> Vec<u32> {
    outputs
        .iter()
        .filter_map(|it| match it {
            PlaneOutput::Yield { id } => Some(id.0),
            _ => None,
        })
        .collect()
}

/// An idle request starts nothing while a request not idle has a task ready, however far
/// behind its virtual time and heavy its weight; on threads no other request can use, it
/// starts.
#[test]
fn an_idle_request_runs_only_on_what_no_other_can_use() {
    let mut plane = Plane::new(config(1, 1)).unwrap();
    open(&mut plane, 1, 1, true);
    open_as(&mut plane, 2, 16, true, true);
    assert_eq!(runs(&add(&mut plane, 1, (1..4).map(|id| work(id, &[], 1)).collect())), [1]);
    assert!(runs(&add(&mut plane, 2, (10..13).map(|id| work(id, &[], 1)).collect())).is_empty());
    assert_eq!(runs(&built(&mut plane, 1)), [2]);
    assert_eq!(runs(&built(&mut plane, 2)), [3]);
    assert_eq!(runs(&built(&mut plane, 3)), [10]);
    assert_eq!(plane.state(NodeId(11)), Some(&NodeState::Ready));

    let mut plane = Plane::new(config(1, 3)).unwrap();
    open(&mut plane, 1, 1, true);
    open_as(&mut plane, 2, 1, true, true);
    assert_eq!(runs(&add(&mut plane, 1, vec![work(1, &[], 1)])), [1]);
    let out = add(&mut plane, 2, (10..13).map(|id| work(id, &[], 1)).collect());
    assert_eq!(runs(&out), [10, 11]);

    // With a budget, it starts while the task not idle that is ready does not fit.
    let mut plane = Plane::new(PlaneConfig { budget: budget(10), ..config(1, 2) }).unwrap();
    open(&mut plane, 1, 1, true);
    open_as(&mut plane, 2, 1, true, true);
    let ahead = vec![node(1, &[], Pool::Work, 1, None, 6), node(2, &[], Pool::Work, 1, None, 6)];
    assert_eq!(runs(&add(&mut plane, 1, ahead)), [1]);
    let out = add(&mut plane, 2, vec![node(10, &[], Pool::Work, 1, None, 3)]);
    assert_eq!(runs(&out), [10]);
    assert!(yields(&out).is_empty(), "giving way would not let 2 fit");
    assert_eq!(runs(&built(&mut plane, 1)), [2]);
}

/// A promoted request takes its share from the plane's virtual time on, as a request that
/// comes back with work ready does; a second promote, and one of a request finished or not
/// known, do nothing.
#[test]
fn a_promoted_request_takes_its_share() {
    let mut plane = Plane::new(config(1, 1)).unwrap();
    open(&mut plane, 1, 1, true);
    open_as(&mut plane, 2, 1, true, true);
    let mut running = runs(&add(&mut plane, 1, (0..20).map(|id| work(id, &[], 1)).collect()));
    running.extend(runs(&add(&mut plane, 2, (100..120).map(|id| work(id, &[], 1)).collect())));
    let mut order = String::new();
    let mut step = |plane: &mut Plane, running: &mut Vec<u32>| {
        let id = running.pop().unwrap();
        order.push(if id < 100 { 'a' } else { 'b' });
        running.extend(runs(&built(plane, id)));
    };
    for _ in 0..3 {
        step(&mut plane, &mut running);
    }
    let promote = PlaneInput::Promote { at_ns: 0, request: RequestId(2) };
    assert!(plane.step(promote.clone()).unwrap().is_empty(), "the thread is taken");
    for _ in 0..6 {
        step(&mut plane, &mut running);
    }
    assert_eq!(order, "aaaababab");
    assert!(plane.step(promote).unwrap().is_empty());
    let unknown = PlaneInput::Promote { at_ns: 0, request: RequestId(99) };
    assert!(plane.step(unknown).unwrap().is_empty());
    open_as(&mut plane, 3, 1, true, true);
    assert_eq!(finished(&close(&mut plane, 3)), [(3, true)]);
    let over = PlaneInput::Promote { at_ns: 0, request: RequestId(3) };
    assert!(plane.step(over).unwrap().is_empty());
}

/// A task not idle that waits only for the thread or the memory of running idle tasks has
/// the newest of them asked to give way, once, and as few as it takes; given way, the idle
/// task's node is ready again and the thread is the other's. One that ends before it gives
/// way frees what it held as ever.
#[test]
fn a_yield_frees_the_thread_for_the_foreground() {
    let mut plane = Plane::new(config(1, 1)).unwrap();
    open_as(&mut plane, 2, 1, true, true);
    assert_eq!(runs(&add(&mut plane, 2, vec![work(10, &[], 1), work(11, &[], 1)])), [10]);
    open(&mut plane, 1, 1, true);
    let out = add(&mut plane, 1, vec![work(1, &[], 1)]);
    assert!(runs(&out).is_empty());
    assert_eq!(yields(&out), [10]);
    let held = plane.step(PlaneInput::Held { at_ns: 0, bytes: 0 }).unwrap();
    assert!(yields(&held).is_empty(), "asked once");
    let not_running = PlaneInput::Yielded { at_ns: 1, id: NodeId(11) };
    assert_eq!(plane.step(not_running), Err(Error::NotRunning(NodeId(11))));
    let out = plane.step(PlaneInput::Yielded { at_ns: 1, id: NodeId(10) }).unwrap();
    assert_eq!(moves(&out)[0], (10, NodeState::Ready));
    assert_eq!(runs(&out), [1]);
    assert_eq!(plane.stats(RequestId(2)).unwrap().ran, 0);
    assert_eq!(runs(&built(&mut plane, 1)), [10]);

    let mut plane = Plane::new(PlaneConfig { budget: budget(10), ..config(1, 2) }).unwrap();
    open_as(&mut plane, 2, 1, true, true);
    let idle = vec![node(10, &[], Pool::Work, 1, None, 4), node(11, &[], Pool::Work, 1, None, 4)];
    assert_eq!(runs(&add(&mut plane, 2, idle)), [10, 11]);
    open(&mut plane, 1, 1, true);
    let out = add(&mut plane, 1, vec![node(1, &[], Pool::Work, 1, None, 5)]);
    assert_eq!(yields(&out), [11], "one frees the thread and enough memory");
    let out = built(&mut plane, 11);
    assert_eq!(runs(&out), [1]);
    assert_eq!(plane.granted(), 9);
}

/// A node of a request not idle that joins an idle request's task not started makes it ready
/// at its own priority, in its own request, which starts it; one that joins an idle task
/// running keeps it from giving way.
#[test]
fn a_foreground_node_joining_an_idle_task_runs_it_as_its_own() {
    let mut plane = Plane::new(config(1, 1)).unwrap();
    open(&mut plane, 1, 1, true);
    open_as(&mut plane, 2, 1, true, true);
    assert_eq!(runs(&add(&mut plane, 1, vec![work(1, &[], 1)])), [1]);
    assert!(runs(&add(&mut plane, 2, vec![keyed(10, &[], 1, 5)])).is_empty());
    let out = add(&mut plane, 1, vec![keyed(2, &[], 1, 5)]);
    assert_eq!(moves(&out), [(2, NodeState::Ready)]);
    assert_eq!(runs(&built(&mut plane, 1)), [2]);
    assert_eq!(plane.state(NodeId(10)), Some(&NodeState::Running));
    assert_eq!(plane.stats(RequestId(1)).unwrap().ran, 2);

    let mut plane = Plane::new(config(1, 1)).unwrap();
    open_as(&mut plane, 2, 1, true, true);
    open(&mut plane, 1, 1, true);
    assert_eq!(runs(&add(&mut plane, 2, vec![keyed(10, &[], 1, 5)])), [10]);
    let out = add(&mut plane, 1, vec![keyed(2, &[], 1, 5), work(3, &[], 1)]);
    assert!(yields(&out).is_empty(), "the running task is wanted");
    assert_eq!(plane.state(NodeId(2)), Some(&NodeState::Running));
}

/// What a plane says of request 0: its records, its runs and its end.
fn foreground(outputs: &[PlaneOutput]) -> Vec<PlaneOutput> {
    outputs
        .iter()
        .filter(|it| match it {
            PlaneOutput::Said { request, .. } | PlaneOutput::Finished { request, .. } => {
                *request == RequestId(0)
            }
            PlaneOutput::Run { id, .. } => id.0 < BUSY,
            PlaneOutput::Yield { .. } => false,
        })
        .cloned()
        .collect()
}

/// The first id of the busy idle request's nodes.
const BUSY: u32 = 1 << 30;

/// Beside an idle request with work for every thread, the build of `traces/cold-small` says
/// what it says alone, at the same times, so finishes in the same virtual time: an idle task
/// in its way gives way as soon as asked.
#[test]
fn a_busy_idle_request_does_not_delay_the_foreground() {
    let trace = Trace::read(include_str!("../traces/cold-small.jsonl")).unwrap();
    let mut alone = one_request(trace.config);
    let mut beside = one_request(trace.config);
    open_as(&mut beside, 1, 4, true, true);
    let busy = (0..64u32)
        .map(|at| node(BUSY + at, &[], if at % 3 == 0 { Pool::Io } else { Pool::Work }, 1, None, 0))
        .collect();
    let first = add(&mut beside, 1, busy);
    assert_eq!(runs(&first).len(), trace.config.io + trace.config.work, "every thread is busy");
    let (mut mine, mut theirs, mut gave_way) = (Vec::new(), Vec::new(), 0);
    for input in &trace.inputs {
        let input = as_plane(input.clone());
        let at_ns = input.at_ns();
        mine.extend(alone.step(input.clone()).unwrap());
        let mut out = beside.step(input).unwrap();
        let mut asked: Vec<u32> = Vec::new();
        loop {
            asked.extend(yields(&out));
            theirs.extend(out);
            let Some(id) = asked.pop() else { break };
            gave_way += 1;
            out = beside.step(PlaneInput::Yielded { at_ns, id: NodeId(id) }).unwrap();
        }
    }
    assert_eq!(finished(&mine), [(0, true)]);
    assert_eq!(finished(&foreground(&theirs)), [(0, true)]);
    assert!(gave_way > 0, "the idle request was in the way");
    // The same records at the same times; within one time, a task may start in the step that
    // freed its thread or in the one after, as its thread was the foreground's or given way.
    let said = |outputs: &[PlaneOutput]| {
        let mut said: Vec<String> = foreground(outputs)
            .iter()
            .map(|it| match it {
                PlaneOutput::Said { record, .. } => format!("{} {:?}", record.at_ns, record.event),
                other => format!("{other:?}"),
            })
            .collect();
        said.sort();
        said
    };
    assert_eq!(said(&theirs), said(&mine));
}
