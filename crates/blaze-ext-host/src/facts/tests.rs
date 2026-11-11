//! The fixpoint against a reference, with the key's safeguards planted out.
//!
//! The world is a small call graph, recursion and all, whose items each have a seed. Two
//! facts are solved over it: `reach` (a set of bits: the item's seed, turned by the
//! extension's own amount, joined with the `reach` of everything it calls) and `width` (the
//! most bits any `reach` has along its calls, which requires `reach`). A plan edits seeds,
//! rewires calls, switches between two extensions and solves, all against one cache, as a
//! daemon does across edits.

use blaze_ids::ContentId;
use hegel::{TestCase, generators as gs};

use super::*;

const REACH: usize = 0;
const WIDTH: usize = 1;

fn decls() -> Vec<Decl> {
    vec![
        Decl {
            name: "reach".to_owned(),
            requires: Vec::new(),
            bottom: 0u16.to_le_bytes().to_vec(),
        },
        Decl { name: "width".to_owned(), requires: vec![REACH], bottom: vec![0] },
    ]
}

#[derive(Clone, Debug)]
struct World {
    seeds: Vec<u16>,
    calls: Vec<Vec<u32>>,
    /// Which extension solves: how far it turns a seed.
    turn: u32,
}

impl World {
    fn nodes(&self) -> Vec<Node> {
        (0..self.seeds.len())
            .map(|at| {
                let mut identity = self.seeds[at].to_le_bytes().to_vec();
                for callee in &self.calls[at] {
                    identity.extend_from_slice(&callee.to_le_bytes());
                }
                Node {
                    name: format!("f{at}"),
                    which: ContentId::of("test fact item", &(at as u32).to_le_bytes()),
                    identity: ContentId::of("test fact item code", &identity),
                    callees: self.calls[at].clone(),
                }
            })
            .collect()
    }

    fn extension(&self) -> ContentId {
        ContentId::of("test fact extension", &self.turn.to_le_bytes())
    }

    fn own(&self, at: usize) -> u16 {
        self.seeds[at].rotate_left(self.turn)
    }

    /// The reference model: every item from its bottom, whole sweeps until one changes
    /// nothing. No components, no order, no cache: the least fixpoint by its definition.
    fn reference(&self) -> (Vec<u16>, Vec<u8>) {
        let n = self.seeds.len();
        let mut reach = vec![0u16; n];
        loop {
            let mut changed = false;
            for at in 0..n {
                let value =
                    self.calls[at].iter().fold(self.own(at), |acc, c| acc | reach[*c as usize]);
                changed |= value != reach[at];
                reach[at] = value;
            }
            if !changed {
                break;
            }
        }
        let mut width = vec![0u8; n];
        loop {
            let mut changed = false;
            for at in 0..n {
                let own = reach[at].count_ones() as u8;
                let value = self.calls[at].iter().fold(own, |acc, c| acc.max(width[*c as usize]));
                changed |= value != width[at];
                width[at] = value;
            }
            if !changed {
                break;
            }
        }
        (reach, width)
    }
}

/// The extension's functions, as a [`Runner`]: each reads only through its view.
struct Model<'w> {
    world: &'w World,
}

impl Runner for Model<'_> {
    type Error = String;

    fn run(&mut self, view: &View<'_>) -> Result<Vec<u8>, String> {
        let (fact, node) = view.of();
        let read = |fact: usize, of: u32| match view.get(fact, of) {
            Slot::Value(bytes) => Ok(bytes.to_vec()),
            other => Err(format!("fact {fact} of f{of}, asked by f{node}: {other:?}")),
        };
        let calls = &self.world.calls[node as usize];
        match fact {
            REACH => {
                let mut value = self.world.own(node as usize);
                for &callee in calls {
                    let bytes = read(REACH, callee)?;
                    value |= u16::from_le_bytes([bytes[0], bytes[1]]);
                }
                Ok(value.to_le_bytes().to_vec())
            }
            _ => {
                let own = read(REACH, node)?;
                let mut value = u16::from_le_bytes([own[0], own[1]]).count_ones() as u8;
                for &callee in calls {
                    value = value.max(read(WIDTH, callee)?[0]);
                }
                Ok(vec![value])
            }
        }
    }
}

#[derive(Clone, Debug)]
enum Op {
    Edit { node: usize, seed: u16 },
    Rewire { node: usize, calls: Vec<u32> },
    Switch { turn: u32 },
    Solve,
}

#[derive(Clone, Debug)]
struct Plan {
    world: World,
    ops: Vec<Op>,
}

/// Where a plan's choices come from: Hegel, or a seed for the controls.
trait Draws {
    fn below(&mut self, n: u64) -> u64;
}

struct Drawn<'t>(&'t TestCase);

impl Draws for Drawn<'_> {
    fn below(&mut self, n: u64) -> u64 {
        self.0.draw(gs::integers::<u64>().min_value(0).max_value(n - 1))
    }
}

/// SplitMix64: the controls' draws, the same on every run.
struct Seeded(u64);

impl Draws for Seeded {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        (z ^ (z >> 31)) % n
    }
}

fn calls_of(draws: &mut impl Draws, n: usize) -> Vec<u32> {
    let mut calls = Vec::new();
    for _ in 0..draws.below(4) {
        // Any item, itself and earlier ones included: recursion is the case under test.
        let callee = draws.below(n as u64) as u32;
        if !calls.contains(&callee) {
            calls.push(callee);
        }
    }
    calls
}

fn plan(draws: &mut impl Draws) -> Plan {
    let n = 1 + draws.below(10) as usize;
    let seeds = (0..n).map(|_| 1u16 << draws.below(16)).collect();
    let calls = (0..n).map(|_| calls_of(draws, n)).collect();
    let mut ops = vec![Op::Solve];
    for _ in 0..draws.below(12) {
        let node = draws.below(n as u64) as usize;
        ops.push(match draws.below(4) {
            0 => Op::Edit { node, seed: 1u16 << draws.below(16) },
            1 => Op::Rewire { node, calls: calls_of(draws, n) },
            2 => Op::Switch { turn: draws.below(2) as u32 * 3 },
            _ => Op::Solve,
        });
        ops.push(Op::Solve);
    }
    Plan { world: World { seeds, calls, turn: 0 }, ops }
}

/// Runs `plan` against one cache and checks the oracle after every solve.
fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let decls = decls();
    let mut world = plan.world.clone();
    let mut cache = Cache::default();
    for (step, op) in plan.ops.iter().enumerate() {
        match op {
            Op::Edit { node, seed } => world.seeds[*node] = *seed,
            Op::Rewire { node, calls } => world.calls[*node] = calls.clone(),
            Op::Switch { turn } => world.turn = *turn,
            Op::Solve => {
                let nodes = world.nodes();
                let mut model = Model { world: &world };
                let solved =
                    solve(world.extension(), &decls, &nodes, &mut cache, &mut model, plant);
                if let Some((fact, node, why)) = solved.failed.first() {
                    return Err(format!("step {step}: fact {fact} of f{node} failed: {why}"));
                }
                if let Some(it) = solved.unsettled.first() {
                    return Err(format!("step {step}: {}", it.said("test", &decls, &nodes)));
                }
                let (reach, width) = world.reference();
                for at in 0..nodes.len() {
                    let got = solved.table.value(REACH, at as u32);
                    if got != Some(&reach[at].to_le_bytes()[..]) {
                        return Err(format!(
                            "step {step}: `reach` of f{at} is {got:?}, and the reference has {:?}",
                            reach[at].to_le_bytes()
                        ));
                    }
                    let got = solved.table.value(WIDTH, at as u32);
                    if got != Some(&[width[at]][..]) {
                        return Err(format!(
                            "step {step}: `width` of f{at} is {got:?}, and the reference has {}",
                            width[at]
                        ));
                    }
                }
                // Build twice: a solve with nothing kept gives the same bytes as this one.
                let mut cold = Cache::default();
                let again = solve(world.extension(), &decls, &nodes, &mut cold, &mut model, plant);
                if again.table != solved.table {
                    return Err(format!("step {step}: a cold solve and a warm one differ"));
                }
                // Nothing changed since: a second warm solve computes nothing.
                let idle = solve(world.extension(), &decls, &nodes, &mut cache, &mut model, plant);
                if !idle.computed.is_empty() {
                    return Err(format!(
                        "step {step}: with nothing changed, {} facts were computed again",
                        idle.computed.len()
                    ));
                }
            }
        }
    }
    Ok(())
}

/// The invariant: after any edits, rewirings and switches of extension, against whatever an
/// earlier solve kept, every fact of every item equals the least fixpoint of its function
/// over the call graph as it stands; a solve with nothing kept gives the same bytes; and a
/// solve after no change computes nothing.
#[hegel::test]
fn facts_equal_the_reference_fixpoint_after_any_edits(tc: TestCase) {
    let plan = plan(&mut Drawn(&tc));
    if let Err(broke) = run(&plan, Plant::None) {
        panic!("{broke}\nplan: {plan:?}");
    }
}

const CONTROL_SEEDS: u64 = 200;

/// A plant the oracle must catch on one of the seeded plans; the same plans pass unplanted.
fn control(plant: Plant) {
    for seed in 0..CONTROL_SEEDS {
        let plan = plan(&mut Seeded(seed));
        if let Err(broke) = run(&plan, Plant::None) {
            panic!("seed {seed}, no plant: {broke}");
        }
        if let Err(why) = run(&plan, plant) {
            eprintln!("{plant:?} caught at seed {seed}: {why}");
            return;
        }
    }
    panic!(
        "the oracle caught {plant:?} at none of seeds 0..{CONTROL_SEEDS}: it, or the draws, are too weak"
    );
}

#[test]
fn the_oracle_catches_a_fact_not_recomputed_when_a_callee_s_fact_changed() {
    control(Plant::CalleeFactsOutOfKey);
}

#[test]
fn the_oracle_catches_a_cache_hit_across_two_extensions_code_identities() {
    control(Plant::ExtensionOutOfKey);
}

fn chain() -> World {
    // f0 calls f1 calls f2; f3 stands alone.
    World { seeds: vec![1, 2, 4, 8], calls: vec![vec![1], vec![2], vec![], vec![]], turn: 0 }
}

#[test]
fn an_edit_computes_again_only_what_reads_it_and_says_why() {
    let decls = decls();
    let mut world = chain();
    let mut cache = Cache::default();
    let nodes = world.nodes();
    let first = solve(
        world.extension(),
        &decls,
        &nodes,
        &mut cache,
        &mut Model { world: &world },
        Plant::None,
    );
    assert_eq!(first.computed.len(), 8, "two facts of four items, each first seen");
    assert!(first.computed.iter().all(|it| it.reason == Reason::FirstSeen));

    world.seeds[2] = 16;
    let nodes = world.nodes();
    let second = solve(
        world.extension(),
        &decls,
        &nodes,
        &mut cache,
        &mut Model { world: &world },
        Plant::None,
    );
    let said: Vec<String> =
        second.computed.iter().map(|it| it.said("demo", &decls, &nodes)).collect();
    assert!(second.computed.iter().all(|it| it.node != 3), "f3 reads nothing of f2: {said:?}");
    assert_eq!(
        said[0],
        "fact `reach` of `f2` (extension `demo`) was computed again: the code of `f2` changed"
    );
    assert_eq!(
        said[1],
        "fact `reach` of `f1` (extension `demo`) was computed again: the fact `reach` of `f2`, which `f1` calls, changed"
    );
    // f2 and what calls it, f1 and f0, for both facts: six of the eight; f3's are found.
    assert_eq!(second.computed.len(), 6, "{said:?}");

    world.turn = 3;
    let third = solve(
        world.extension(),
        &decls,
        &nodes,
        &mut cache,
        &mut Model { world: &world },
        Plant::None,
    );
    assert_eq!(third.computed[0].reason, Reason::Changed(Part::Extension));
}

/// A fact whose value rises on every round of a cycle: never a fixpoint.
struct Rising;

impl Runner for Rising {
    type Error = String;

    fn run(&mut self, view: &View<'_>) -> Result<Vec<u8>, String> {
        let (fact, node) = view.of();
        let Slot::Value(bytes) = view.get(fact, node) else { return Err("not a cycle".to_owned()) };
        let count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        Ok((count + 1).to_le_bytes().to_vec())
    }
}

#[test]
fn a_cycle_that_never_settles_is_refused_at_its_bound_naming_it() {
    let decls = vec![Decl { name: "depth".to_owned(), requires: Vec::new(), bottom: vec![0; 4] }];
    let world = World { seeds: vec![1], calls: vec![vec![0]], turn: 0 };
    let nodes = world.nodes();
    let solved =
        solve(world.extension(), &decls, &nodes, &mut Cache::default(), &mut Rising, Plant::None);
    let [unsettled] = &solved.unsettled[..] else { panic!("one cycle unsettled") };
    assert_eq!(unsettled.rounds, ROUNDS_MIN + ROUNDS_PER_MEMBER);
    let said = unsettled.said("demo", &decls, &nodes);
    assert!(said.contains("fact `depth` did not reach a fixpoint"), "{said}");
    assert!(said.contains("through `f0`"), "{said}");
    assert_eq!(solved.table.value(0, 0), None, "a value still rising is not the fact");
}

/// Fails on one item, and reads its callees elsewhere.
struct FailsOn(u32);

impl Runner for FailsOn {
    type Error = String;

    fn run(&mut self, view: &View<'_>) -> Result<Vec<u8>, String> {
        let (fact, node) = view.of();
        if node == self.0 {
            return Err("planted".to_owned());
        }
        match view.get(fact, node + 1) {
            Slot::Failed => Err("its callee's fact failed".to_owned()),
            _ => Ok(vec![1]),
        }
    }
}

#[test]
fn a_failed_fact_fails_who_asks_for_it_and_no_one_else() {
    let decls = vec![Decl { name: "ok".to_owned(), requires: Vec::new(), bottom: vec![0] }];
    let world = chain();
    let nodes = world.nodes();
    let solved = solve(
        world.extension(),
        &decls,
        &nodes,
        &mut Cache::default(),
        &mut FailsOn(2),
        Plant::None,
    );
    let failed: Vec<u32> = solved.failed.iter().map(|it| it.1).collect();
    assert_eq!(failed, [2, 1, 0], "f2 fails, then f1 and f0, which ask for it");
    assert_eq!(solved.table.value(0, 3), Some(&[1][..]), "f3 asks for nothing of f2");
}

#[test]
fn a_view_refuses_a_fact_not_required_and_an_item_not_called() {
    let decls = decls();
    let world = chain();
    let nodes = world.nodes();
    struct Probe;
    impl Runner for Probe {
        type Error = String;
        fn run(&mut self, view: &View<'_>) -> Result<Vec<u8>, String> {
            let (fact, node) = view.of();
            if fact == REACH && node == 0 {
                assert_eq!(
                    view.get(WIDTH, 1),
                    Slot::Undeclared,
                    "`reach` does not require `width`"
                );
                assert_eq!(view.get(REACH, 2), Slot::Undeclared, "f0 does not call f2");
                assert_eq!(view.get(REACH, 0), Slot::Undeclared, "f0 does not call itself");
                assert!(matches!(view.get(REACH, 1), Slot::Value(_)));
            }
            Ok(vec![0, 0])
        }
    }
    let solved =
        solve(world.extension(), &decls, &nodes, &mut Cache::default(), &mut Probe, Plant::None);
    assert!(solved.failed.is_empty());
}
