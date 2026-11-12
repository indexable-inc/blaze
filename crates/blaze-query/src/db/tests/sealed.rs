//! Seal groups' deterministic simulation ([`crate::db::seal`]): the changed set's flow
//! programs (`changed`), with their inputs given to groups that depend on each other, run
//! through writes, pulls, seal passes and asks on one thread. The engine code that runs is the
//! real one: the run-end placing of a row in its group, the pass, the write's index from a
//! field to its group, the group's check and the walk.
//!
//! The oracle is the refinement: a database with seal groups and one with none answer every
//! pull alike, and as a from-scratch evaluation does, after every step. The controls plant one
//! bug each ([`Plant`]) in a directed program that passes as the engine is.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    super::{
        Changed, ChangedSetWork, SealGroup, SealKind, SealState,
        frontier::Plant,
        plumbing::{changed_since, evict_values, input_seal_group},
    },
    changed::{FlowNode, FlowOp, evaluate, flowed, sum},
    *,
};

/// The flow as a query of this module's own.
struct Crated;

impl Spec for Crated {
    type Key = Node;
    type Value = u64;
    const QUERY: &'static str = "Crated";
    const EVICT: bool = true;

    fn cache() -> &'static Ingredient {
        ingredient!(function::<Crated>)
    }

    fn compute(db: &TestDb, key: Node) -> u64 {
        flowed::<Crated>(db, key)
    }
}

/// A database with its flow, its inputs and the model's copy of them.
struct Shop {
    db: TestDb,
    flow: Vec<FlowNode>,
    inputs: Vec<u64>,
    cells: Vec<Cell>,
}

impl Shop {
    fn new(plant: Plant) -> Shop {
        let mut db = TestDb::new();
        db.storage_mut().plant_in_changed_set(plant);
        Shop { db, flow: Vec::new(), inputs: Vec::new(), cells: Vec::new() }
    }

    fn group(&mut self, name: &str, deps: &[SealGroup]) -> SealGroup {
        self.db.storage_mut().seal_group(name, SealKind::Library, deps)
    }

    /// A new input holding `value`, an input of `group` if one is given.
    fn input(&mut self, value: u64, group: Option<SealGroup>) -> usize {
        let cell = Cell::new(&self.db, value, 0);
        if let Some(group) = group {
            input_seal_group::<Cells>(self.db.storage_mut(), cell.0, group);
        }
        self.db.cells.lock().push(cell.0);
        self.cells.push(cell);
        self.inputs.push(value);
        self.inputs.len() - 1
    }

    fn row(&mut self, node: FlowNode) -> usize {
        self.db.flow.lock().push(node.clone());
        self.flow.push(node);
        self.flow.len() - 1
    }

    fn write(&mut self, input: usize, value: u64) {
        self.inputs[input] = value;
        self.cells[input].set_a(&mut self.db, value);
    }

    /// Pulls `rows` and holds each to a run from scratch.
    fn pull(&self, rows: &[usize]) -> Result<(), String> {
        let model = evaluate(&self.flow, &self.inputs, self.db.world.load(Ordering::Relaxed));
        for &at in rows {
            let got = *fetch::<Crated>(&self.db, node(&self.db, at));
            if got != model[at] {
                return Err(format!("row {at} is {got}, from scratch {}", model[at]));
            }
        }
        Ok(())
    }

    fn pull_all(&self) -> Result<(), String> {
        self.pull(&(0..self.flow.len()).collect::<Vec<_>>())
    }

    /// A build's end as a host does it: fold, settle, then the seal pass.
    fn seal(&mut self) -> crate::db::SealPass {
        let storage = self.db.storage_mut();
        storage.pack_reverse();
        storage.settle_changed_set();
        storage.seal_quiet_groups()
    }

    /// The rows the changed set names since `since`, by their place in the flow, and the
    /// walk's work.
    fn ask(&self, since: crate::db::View) -> (BTreeSet<usize>, ChangedSetWork) {
        let Changed::Keys(keys) = changed_since::<Crated>(&self.db, since) else {
            panic!("the changed set is unknown")
        };
        let named = keys
            .iter()
            .map(|key| {
                (0..self.flow.len()).find(|&at| node(&self.db, at) == *key).expect("a row of the flow")
            })
            .collect();
        (named, self.db.storage().changed_set_work())
    }

    fn state(&self, group: SealGroup) -> SealState {
        self.db.storage().seal_group_state(group)
    }
}

/// What [`a_body_edit_under_dependents`] saw.
#[derive(Debug, PartialEq, Eq)]
struct BodyEdit {
    /// Rows the edit's walk asked, rows in its reach, rows it set apart.
    walk: (u64, u64, u64),
    /// Groups it checked and seals it broke.
    groups: (u64, u64),
    /// Rows its changed set named.
    named: usize,
    /// The library's, the edited crate's and the dependent's state after the walk.
    states: [SealState; 3],
    /// Rows behind seals after the walk.
    behind: u64,
}

/// A library, an application above it and a dependent crate of `dependents` rows above that:
/// the application has a body row (its input and the library's top) and an interface row (its
/// input modulo 3); each dependent row reads the interface and the dependent's own input, and
/// one top row reads them all. Everything is built and sealed. Then the application's input
/// is written so that its interface stays (a body edit), the host asks for the changed set,
/// and pulls the body, the interface, a dependent row and the dependent's top.
fn a_body_edit_under_dependents(dependents: usize, plant: Plant) -> BodyEdit {
    let mut shop = Shop::new(plant);
    let lib = shop.group("lib", &[]);
    let app = shop.group("app", &[lib]);
    let dep = shop.group("dep", &[app]);
    let (of_lib, of_app, of_dep) =
        (shop.input(1, Some(lib)), shop.input(10, Some(app)), shop.input(100, Some(dep)));
    let top = shop.row(sum(&[Dep::Input(of_lib)]));
    let body = shop.row(sum(&[Dep::Input(of_app), Dep::Query(top)]));
    let interface = shop.row(FlowNode { op: FlowOp::Mod3, deps: vec![Dep::Input(of_app)] });
    let first = shop.flow.len();
    for _ in 0..dependents {
        shop.row(sum(&[Dep::Query(interface), Dep::Input(of_dep)]));
    }
    let above = shop.row(sum(&(first..first + dependents).map(Dep::Query).collect::<Vec<_>>()));
    shop.pull_all().unwrap();
    let pass = shop.seal();
    assert_eq!((pass.sealed, pass.rows, pass.excluded), (3, shop.flow.len() as u64, 0), "{pass:?}");
    let since = shop.db.storage().view();
    // 10 and 13 are both 1 modulo 3: the interface stays.
    shop.write(of_app, 13);
    let (named, work) = shop.ask(since);
    shop.pull(&[body, interface, first, above]).unwrap();
    let census = shop.db.storage().seal_census();
    shop.pull_all().unwrap();
    if plant == Plant::None {
        assert_eq!(named, BTreeSet::from([body]), "the edit reaches the body");
    }
    BodyEdit {
        walk: (work.visited, work.reach, work.marked),
        groups: (work.groups_checked, work.groups_broken),
        named: named.len(),
        states: [shop.state(lib), shop.state(app), shop.state(dep)],
        behind: census.rows_behind_seals(),
    }
}

/// The invariant: an edit in one group visits no row of another sealed group. A body edit of
/// the application opens the application's group and no other; the walk asks the application's
/// two rows and its reach holds those two; the dependent group is checked once (by the one row
/// it reads from outside, whose stamp did not move) and stays sealed, and the library is not
/// looked at. The same at 100 dependent rows and at 2,000: only the rows behind seals grow.
#[test]
fn an_edit_under_sealed_dependents_stays_in_its_group_at_two_sizes() {
    let small = a_body_edit_under_dependents(100, Plant::None);
    let large = a_body_edit_under_dependents(2000, Plant::None);
    let states = [SealState::Sealed, SealState::Open, SealState::Sealed];
    assert_eq!(
        small,
        BodyEdit { walk: (2, 2, 0), groups: (1, 0), named: 1, states, behind: 102 },
    );
    assert_eq!(large, BodyEdit { behind: 2002, ..small }, "the edit's work grew with its dependents");
}

/// The control of [`an_edit_under_sealed_dependents_stays_in_its_group_at_two_sizes`]: a walk
/// that takes a row behind a seal as any reader has every dependent row in its reach, and the
/// oracle says so at both sizes.
#[test]
fn the_oracle_catches_a_walk_that_enters_a_sealed_group() {
    let small = a_body_edit_under_dependents(100, Plant::WalkEntersSealedGroups);
    let large = a_body_edit_under_dependents(2000, Plant::WalkEntersSealedGroups);
    assert!(small.walk.1 >= 100, "{small:?}");
    assert!(large.walk.1 >= 2000, "{large:?}");
}

/// A library and three crates above it that do not depend on each other, three rows each
/// (one reads the library's top), built and sealed; the first crate's input is written.
/// Returns the shop, the groups (library first), the first crate's rows and the view before
/// the write.
fn three_crates_one_written(plant: Plant) -> (Shop, Vec<SealGroup>, Vec<usize>, crate::db::View) {
    let mut shop = Shop::new(plant);
    let lib = shop.group("lib", &[]);
    let of_lib = shop.input(1, Some(lib));
    let top = shop.row(sum(&[Dep::Input(of_lib)]));
    let mut groups = vec![lib];
    let (mut edited, mut written) = (Vec::new(), 0);
    for (at, name) in ["a", "b", "c"].into_iter().enumerate() {
        let group = shop.group(name, &[lib]);
        let input = shop.input(10 + at as u64, Some(group));
        let rows = [
            shop.row(sum(&[Dep::Input(input)])),
            shop.row(sum(&[Dep::Input(input), Dep::Query(top)])),
        ];
        let last = shop.row(sum(&[Dep::Query(rows[0]), Dep::Query(rows[1])]));
        if at == 0 {
            edited = vec![rows[0], rows[1], last];
            written = input;
        }
        groups.push(group);
    }
    shop.pull_all().unwrap();
    let pass = shop.seal();
    assert_eq!((pass.sealed, pass.rows), (4, 10), "{pass:?}");
    let since = shop.db.storage().view();
    shop.write(written, 50);
    (shop, groups, edited, since)
}

/// The invariant: a write to an input of a sealed group opens that group and only it, found
/// from the field (no group is checked, none other opened), and every row answers as a run
/// from scratch. The census says so: three of four groups sealed, seven rows behind seals.
#[test]
fn a_write_to_a_sealed_groups_input_opens_that_group_and_no_other() {
    let (shop, groups, edited, since) = three_crates_one_written(Plant::None);
    assert_eq!(shop.db.storage().seal_opens(), 1);
    let states: Vec<SealState> = groups.iter().map(|&it| shop.state(it)).collect();
    assert_eq!(
        states,
        [SealState::Sealed, SealState::Open, SealState::Sealed, SealState::Sealed],
        "before any walk: the write itself opened the one group"
    );
    let (named, work) = shop.ask(since);
    assert_eq!(named, edited.iter().copied().collect::<BTreeSet<_>>());
    assert_eq!((work.visited, work.groups_checked, work.groups_broken), (3, 0, 0), "{work:?}");
    shop.pull_all().unwrap();
    let census = shop.db.storage().seal_census();
    assert_eq!((census.sealed_groups(), census.rows_behind_seals(), census.opens), (3, 7, 1));
    assert_eq!((census.grouped_rows(), census.ungrouped), (10, 0));
    let said = census.to_string();
    assert!(said.starts_with("seal groups: 3 of 4 sealed"), "{said}");
}

/// The control of [`a_write_to_a_sealed_groups_input_opens_that_group_and_no_other`]: a seal
/// that survives a write to its group's input answers the rows' old values, and the oracle
/// (a run from scratch) says so.
#[test]
fn the_oracle_catches_a_seal_that_survives_a_write() {
    let (shop, _, edited, _) = three_crates_one_written(Plant::SealSurvivesAWrite);
    let why = shop.pull(&edited).expect_err("the oracle missed a seal that survived a write");
    assert!(why.contains("from scratch"), "{why}");
}

/// An application with two interface rows, each of an input of its own (the first modulo 3,
/// the second as it is); a dependent that reads the first, a dependent that reads the second
/// (one row of it modulo 3), and a crate above that one that reads its modulo row. All sealed.
/// Then a write that keeps the first interface, and, after another pass, a write that changes
/// the second. Returns what the oracle said of the last pulls, the states of the two
/// dependents and the crate above after each write's walk, and the seals each walk broke.
fn two_interface_edits(plant: Plant) -> (Result<(), String>, [[SealState; 3]; 2], [u64; 2]) {
    let mut shop = Shop::new(plant);
    let app = shop.group("app", &[]);
    let first = shop.group("reads the first", &[app]);
    let second = shop.group("reads the second", &[app]);
    let above = shop.group("above the second", &[second]);
    let (one, two) = (shop.input(10, Some(app)), shop.input(3, Some(app)));
    let (in_first, in_second, in_above) =
        (shop.input(1, Some(first)), shop.input(2, Some(second)), shop.input(4, Some(above)));
    let interface_one = shop.row(FlowNode { op: FlowOp::Mod3, deps: vec![Dep::Input(one)] });
    let interface_two = shop.row(sum(&[Dep::Input(two)]));
    shop.row(sum(&[Dep::Query(interface_one), Dep::Input(in_first)]));
    let whole = shop.row(sum(&[Dep::Query(interface_two), Dep::Input(in_second)]));
    // 3 + 2 and 6 + 2 are both 2 modulo 3; its own input makes it a row of the dependent.
    let modulo = shop.row(FlowNode {
        op: FlowOp::Mod3,
        deps: vec![Dep::Query(interface_two), Dep::Input(in_second)],
    });
    shop.row(sum(&[Dep::Query(modulo), Dep::Input(in_above)]));
    shop.pull_all().unwrap();
    assert_eq!(shop.seal().sealed, 4);
    let states = |shop: &Shop| [shop.state(first), shop.state(second), shop.state(above)];
    // The first interface stays (10 and 13 are 1 modulo 3).
    let since = shop.db.storage().view();
    shop.write(one, 13);
    let (_, kept) = shop.ask(since);
    let after_kept = states(&shop);
    shop.pull_all().unwrap();
    shop.seal();
    // The second interface moves from 3 to 6: the row that reads it whole changes, the row
    // that reads it modulo 3 does not.
    let since = shop.db.storage().view();
    shop.write(two, 6);
    let (named, changed) = shop.ask(since);
    let after_changed = states(&shop);
    let said = shop.pull_all();
    if plant == Plant::None {
        assert!(named.contains(&whole) && named.contains(&interface_two), "{named:?}");
    }
    (said, [after_kept, after_changed], [kept.groups_broken, changed.groups_broken])
}

/// The invariant: a write in a dependency that keeps what a dependent reads leaves the
/// dependent sealed, and one that changes it breaks the seal of exactly the dependents that
/// read the changed part: the dependent of the other interface row stays sealed, and so does
/// the crate above the broken one, whose own import (a row that did not change) holds.
#[test]
fn an_interface_change_breaks_the_seals_of_the_dependents_that_read_it() {
    let (said, states, broken) = two_interface_edits(Plant::None);
    assert_eq!(said, Ok(()));
    let sealed = SealState::Sealed;
    assert_eq!(states[0], [sealed; 3], "a write that keeps the interface");
    assert_eq!(states[1], [sealed, SealState::Broken, sealed], "a write that changes one part of it");
    assert_eq!(broken, [0, 1]);
}

/// The control of [`an_interface_change_breaks_the_seals_of_the_dependents_that_read_it`]:
/// when a write opens its own group and no group above it is checked, the dependent that read
/// the changed interface answers its old value, and the oracle says so.
#[test]
fn the_oracle_catches_dependents_that_are_never_checked() {
    let (said, _, _) = two_interface_edits(Plant::SealedDependentsNeverSuspect);
    let why = said.expect_err("the oracle missed a dependent left sealed over a changed interface");
    assert!(why.contains("from scratch"), "{why}");
}

/// The invariant: a set of the value an input holds unseals nothing, whichever groups read
/// the input: one map of every crate's expanders, of no group, read by two sealed crates,
/// given again as it is, opens no view and leaves both sealed and unchecked. Given another
/// value, both are checked and both seals break, and every row answers as from scratch.
#[test]
fn an_equal_set_of_an_input_every_crate_reads_unseals_nothing() {
    let mut shop = Shop::new(Plant::None);
    let (a, b) = (shop.group("a", &[]), shop.group("b", &[]));
    let expanders = shop.input(7, None);
    let (of_a, of_b) = (shop.input(1, Some(a)), shop.input(2, Some(b)));
    shop.row(sum(&[Dep::Input(of_a), Dep::Input(expanders)]));
    shop.row(sum(&[Dep::Input(of_b), Dep::Input(expanders)]));
    shop.pull_all().unwrap();
    assert_eq!(shop.seal().sealed, 2);
    let since = shop.db.storage().view();
    shop.write(expanders, 7);
    assert_eq!(shop.db.storage().view(), since, "an equal set opens no view");
    assert_eq!([shop.state(a), shop.state(b)], [SealState::Sealed; 2]);
    assert_eq!(shop.db.storage().seal_opens(), 0);
    shop.write(expanders, 8);
    assert_eq!([shop.state(a), shop.state(b)], [SealState::Suspect; 2]);
    let (named, work) = shop.ask(since);
    assert_eq!((named.len(), work.groups_checked, work.groups_broken), (2, 2, 2), "{work:?}");
    shop.pull_all().unwrap();
}

/// The invariant: a plan that adds a crate unseals nothing. A library and an application are
/// sealed; a new group is made with an input and a row over the library's top, and the crate
/// list (an input of no group, read by one row of no group) is written. No group is opened,
/// checked or broken, and every row answers as from scratch.
#[test]
fn a_plan_that_adds_a_crate_unseals_nothing() {
    let mut shop = Shop::new(Plant::None);
    let lib = shop.group("lib", &[]);
    let app = shop.group("app", &[lib]);
    let (of_lib, of_app, list) = (shop.input(1, Some(lib)), shop.input(10, Some(app)), shop.input(2, None));
    let top = shop.row(sum(&[Dep::Input(of_lib)]));
    shop.row(sum(&[Dep::Input(of_app), Dep::Query(top)]));
    let listed = shop.row(sum(&[Dep::Input(list)]));
    shop.pull_all().unwrap();
    assert_eq!(shop.seal().sealed, 2);
    let since = shop.db.storage().view();
    let added = shop.group("added", &[lib]);
    let of_added = shop.input(5, Some(added));
    let new = shop.row(sum(&[Dep::Input(of_added), Dep::Query(top)]));
    shop.write(list, 3);
    let (named, work) = shop.ask(since);
    assert_eq!(named, BTreeSet::from([listed]));
    assert_eq!((work.groups_checked, work.groups_broken), (0, 0), "{work:?}");
    shop.pull(&[listed, new]).unwrap();
    assert_eq!([shop.state(lib), shop.state(app)], [SealState::Sealed; 2]);
    assert_eq!(shop.db.storage().seal_opens(), 0);
    shop.pull_all().unwrap();
    // The next pass seals the added crate beside the others.
    assert_eq!(shop.seal().sealed, 1);
    assert_eq!(shop.state(added), SealState::Sealed);
}

/// The quiet rule ([`SealKind::Local`]): a local crate is sealed by the pass that ends the
/// second build in a row with no write to its inputs, and a write starts the count again.
#[test]
fn a_local_group_is_sealed_after_two_quiet_builds() {
    let mut shop = Shop::new(Plant::None);
    let local = shop.db.storage_mut().seal_group("local", SealKind::Local, &[]);
    let input = shop.input(1, Some(local));
    shop.row(sum(&[Dep::Input(input)]));
    shop.pull_all().unwrap();
    assert_eq!((shop.seal().waiting, shop.state(local)), (1, SealState::Open), "one quiet build");
    shop.write(input, 2);
    shop.pull_all().unwrap();
    assert_eq!((shop.seal().waiting, shop.state(local)), (1, SealState::Open), "a build that wrote");
    assert_eq!((shop.seal().waiting, shop.state(local)), (1, SealState::Open), "one quiet build");
    assert_eq!((shop.seal().sealed, shop.state(local)), (1, SealState::Sealed), "the second");
    shop.write(input, 3);
    assert_eq!(shop.state(local), SealState::Open);
    shop.pull_all().unwrap();
}

/// Two crates that do not depend on each other and a third over the first; the first has an
/// input and a row of it, the third a row over that row. All sealed. The input is given to the
/// second crate's group (a file that moves to another source root), everything is sealed
/// again, and the input is written. Returns what the oracle says of every row then, and the
/// groups opened by the move.
fn an_input_moved_then_written(plant: Plant) -> (Result<(), String>, u64) {
    let mut shop = Shop::new(plant);
    let (from, to) = (shop.group("from", &[]), shop.group("to", &[]));
    let over = shop.group("over", &[from]);
    let (moved, of_to, of_over) =
        (shop.input(1, Some(from)), shop.input(2, Some(to)), shop.input(3, Some(over)));
    let row = shop.row(sum(&[Dep::Input(moved)]));
    shop.row(sum(&[Dep::Input(of_to)]));
    shop.row(sum(&[Dep::Query(row), Dep::Input(of_over)]));
    shop.pull_all().unwrap();
    assert_eq!(shop.seal().sealed, 3);
    input_seal_group::<Cells>(shop.db.storage_mut(), shop.cells[moved].0, to);
    let opened = shop.db.storage().seal_opens();
    shop.pull_all().unwrap();
    shop.seal();
    shop.write(moved, 9);
    (shop.pull_all(), opened)
}

/// The invariant: an input that moves to another group is removed from the one and added to
/// the other. The move opens the group it leaves and the group above that read it through a
/// row (two groups, and not the group it joins); after everything is sealed again, a write of
/// the input, which now opens the group it joined, still reaches the rows that read it before
/// the move, and every row answers as from scratch.
#[test]
fn an_input_that_moves_groups_is_removed_from_one_and_added_to_the_other() {
    assert_eq!(an_input_moved_then_written(Plant::None), (Ok(()), 2));
}

/// The control of [`an_input_that_moves_groups_is_removed_from_one_and_added_to_the_other`]:
/// an input only given its new group leaves its old readers behind a seal no write of it
/// reaches, and the oracle sees their old values.
#[test]
fn the_oracle_catches_an_input_regrouped_with_no_removal() {
    let (said, opened) = an_input_moved_then_written(Plant::InputMovesWithoutRemoval);
    assert_eq!(opened, 0);
    let why = said.expect_err("the oracle missed rows left behind the old group's seal");
    assert!(why.contains("from scratch"), "{why}");
}

#[derive(Clone, Debug)]
enum Step {
    Write(usize, u64),
    /// Changes the world an untracked read sees; a view opens with nothing written.
    World(u64),
    Evict,
    Pull(Vec<usize>),
    /// A build's end: fold, settle and, in the database that has groups, the seal pass.
    Seal,
    /// Asks for the changed set since the last ask, then for every row made.
    Ask,
}

#[derive(Clone, Debug)]
struct Program {
    /// Each group's dependencies, among the groups before it.
    groups: Vec<Vec<usize>>,
    /// Each input's group, if it has one.
    inputs: Vec<Option<usize>>,
    flow: Vec<FlowNode>,
    steps: Vec<Step>,
}

fn program(tc: &TestCase) -> Program {
    let int = |low: usize, high: usize| tc.draw(gs::integers::<usize>().min_value(low).max_value(high));
    let groups: Vec<Vec<usize>> = (0..int(1, 4))
        .map(|at| (0..at).filter(|_| tc.draw(gs::booleans())).collect())
        .collect();
    // One input in five is of no group.
    let inputs: Vec<Option<usize>> =
        (0..int(1, 6)).map(|_| (int(0, 4) != 0).then(|| int(0, groups.len() - 1))).collect();
    let nodes = int(1, 12);
    let mut flow = Vec::with_capacity(nodes);
    for at in 0..nodes {
        let op = [FlowOp::Sum, FlowOp::Sum, FlowOp::Mod3, FlowOp::Max, FlowOp::Pick, FlowOp::World]
            [int(0, 5)];
        let deps = (0..int(1, 4))
            .map(|_| {
                if at > 0 && tc.draw(gs::booleans()) {
                    Dep::Query(int(0, at - 1))
                } else {
                    Dep::Input(int(0, inputs.len() - 1))
                }
            })
            .collect();
        flow.push(FlowNode { op, deps });
    }
    let mut steps = Vec::new();
    for _ in 0..int(1, 20) {
        steps.push(match int(0, 11) {
            0..=3 => Step::Write(int(0, inputs.len() - 1), tc.draw(gs::integers::<u64>().max_value(6))),
            4 => Step::World(tc.draw(gs::integers::<u64>().max_value(3))),
            5 => Step::Evict,
            6 | 7 => Step::Pull((0..int(1, 3)).map(|_| int(0, nodes - 1)).collect()),
            8 | 9 => Step::Seal,
            _ => Step::Ask,
        });
    }
    steps.push(Step::Ask);
    Program { groups, inputs, flow, steps }
}

/// One of the simulation's two databases: `sealed` gives it the program's groups.
fn made(program: &Program, sealed: bool, plant: Plant) -> Shop {
    let mut shop = Shop::new(plant);
    let mut groups: Vec<SealGroup> = Vec::new();
    if sealed {
        for (at, deps) in program.groups.iter().enumerate() {
            let deps: Vec<SealGroup> = deps.iter().map(|&dep| groups[dep]).collect();
            groups.push(shop.group(&format!("group {at}"), &deps));
        }
    }
    for (at, group) in program.inputs.iter().enumerate() {
        shop.input(at as u64, group.filter(|_| sealed).map(|group| groups[group]));
    }
    for node in &program.flow {
        shop.row(node.clone());
    }
    shop
}

/// Runs `program` in a database with its seal groups (`plant` planted there) and in one with
/// none, and checks both against the model after every step. `Err` says what the oracle saw.
fn simulate(program: &Program, plant: Plant) -> Result<(), String> {
    let mut shops = [made(program, true, plant), made(program, false, Plant::None)];
    let rows = program.flow.len();
    let ids: BTreeMap<u32, usize> =
        (0..rows).map(|at| (node(&shops[0].db, at).0.as_u32(), at)).collect();
    let made_rows = |shop: &Shop| -> BTreeSet<usize> {
        shop.db.log.lock().iter().filter(|it| it.0 == "Crated").map(|it| ids[&it.1]).collect()
    };
    // The sealed database's last ask: its view, the model then, the rows made by then.
    let mut asked = (shops[0].db.storage().view(), evaluate(&program.flow, &shops[0].inputs, 0), BTreeSet::new());
    for (at, step) in program.steps.iter().enumerate() {
        for (which, shop) in shops.iter_mut().enumerate() {
            let said = |why: String| format!("step {at}, {}: {why}", ["sealed", "unsealed"][which]);
            match step {
                Step::Write(input, value) => shop.write(*input, *value),
                Step::World(value) => {
                    shop.db.world.store(*value, Ordering::Relaxed);
                    shop.db.storage_mut().synthetic_write(Durability::LOW);
                }
                Step::Evict => {
                    evict_values::<Crated>(&mut shop.db);
                }
                Step::Pull(pulled) => shop.pull(pulled).map_err(said)?,
                Step::Seal => {
                    if which == 0 {
                        shop.seal();
                    } else {
                        shop.db.storage_mut().pack_reverse();
                        shop.db.storage_mut().settle_changed_set();
                    }
                }
                Step::Ask => {
                    let since = if which == 0 { asked.0 } else { shop.db.storage().view() };
                    let (named, work) = shop.ask(since);
                    let rows_made = made_rows(shop);
                    if work.visited > rows_made.len() as u64 {
                        return Err(said(format!("the walk visited {} rows of {} made", work.visited, rows_made.len())));
                    }
                    shop.pull(&rows_made.iter().copied().collect::<Vec<_>>()).map_err(said)?;
                    if which == 0 {
                        // Exact: every row made by the last ask whose value differs is named.
                        let now = evaluate(&program.flow, &shop.inputs, shop.db.world.load(Ordering::Relaxed));
                        for &row in &asked.2 {
                            if now[row] != asked.1[row] && !named.contains(&row) {
                                return Err(said(format!(
                                    "row {row} was {} at the last ask and is {}, and the changed \
                                     set does not name it (it names {named:?})",
                                    asked.1[row], now[row]
                                )));
                            }
                        }
                        asked = (shop.db.storage().view(), now, made_rows(shop));
                    }
                }
            }
        }
        // The refinement: both databases answer every row either has made alike. (Each was
        // held to the model where it was pulled; this holds them to each other.)
        if matches!(step, Step::Ask) {
            let (sealed, plain) = (&shops[0], &shops[1]);
            for row in made_rows(sealed).intersection(&made_rows(plain)) {
                let (a, b) = (
                    *fetch::<Crated>(&sealed.db, node(&sealed.db, *row)),
                    *fetch::<Crated>(&plain.db, node(&plain.db, *row)),
                );
                if a != b {
                    return Err(format!("step {at}: row {row} is {a} behind seals and {b} without"));
                }
            }
        }
    }
    Ok(())
}

/// The refinement: a database with seal groups and one with none answer every query alike,
/// and as a run from scratch does, under random writes, world changes, evictions, pulls, seal
/// passes and asks, over random group graphs with inputs of every group and of none, and rows
/// that read across groups, so some are of no group; and the sealed database's changed set
/// names every row whose value differs since its last ask.
#[hegel::test]
fn a_sealed_database_answers_as_an_unsealed_one_under_random_writes(tc: TestCase) {
    let program = program(&tc);
    if let Err(why) = simulate(&program, Plant::None) {
        panic!("{why}\n{program:#?}");
    }
}

/// A directed program for the simulation's controls: a library and a crate over it, both
/// sealed between a pull and a write of the library's input, which changes the crate's rows
/// too (row 1 reads row 0 and the crate's input, row 2 reads row 1).
fn directed() -> Program {
    Program {
        groups: vec![vec![], vec![0]],
        inputs: vec![Some(0), Some(1)],
        flow: vec![sum(&[Dep::Input(0)]), sum(&[Dep::Input(1), Dep::Query(0)]), sum(&[Dep::Query(1)])],
        steps: vec![
            Step::Pull(vec![0, 1, 2]),
            Step::Ask,
            Step::Seal,
            Step::Write(0, 9),
            Step::Pull(vec![2]),
            Step::Ask,
        ],
    }
}

/// The simulation's own controls: its directed program passes as the engine is; with a seal
/// that survives a write, and with dependents never checked (the write is of the library's
/// input, so the crate above is a dependent), the oracle sees a stale answer.
#[test]
fn the_simulations_oracle_catches_each_planted_seal_bug() {
    let program = directed();
    assert_eq!(simulate(&program, Plant::None), Ok(()), "the engine as it is must pass");
    for plant in [Plant::SealSurvivesAWrite, Plant::SealedDependentsNeverSuspect] {
        let why = simulate(&program, plant).expect_err("the oracle missed the planted bug");
        assert!(why.contains("from scratch"), "{plant:?}: {why}");
    }
}
