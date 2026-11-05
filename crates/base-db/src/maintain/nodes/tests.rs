use super::*;

/// A transcript of a pass on the shape that hurt (core under everything, a wide layer beside a
/// deep chain): each line a task's ask and what the plan must decide. Replayed with no clock.
///
/// ```text
///   0 core (cost 40) <- 1 alloc (2) <- 2 std (3) <- 3, 4, 5 leaves (1 each)
///                                  <- 6 chain a (5) <- 7 chain b (5)
///   8 lone (1), depending on nothing
/// ```
#[test]
fn a_pass_starts_the_longest_chain_first_and_begins_unstarted_deps_on_the_askers_stack() {
    let costs = [40, 2, 3, 1, 1, 1, 5, 5, 1];
    let deps: [&[u32]; 9] = [&[], &[0], &[1], &[2], &[2], &[2], &[1], &[6], &[]];
    let mut plan = Plan::new(&costs, |n| deps[n as usize].iter().copied()).unwrap();
    // Bottom levels: core carries the chain a-b (40 + 2 + 5 + 5), not the wide leaves.
    assert_eq!(plan.bottom_level(0), 52);
    assert_eq!(plan.bottom_level(1), 12);
    assert_eq!(plan.bottom_level(6), 10);
    assert_eq!(plan.bottom_level(2), 4);
    // Highest first, ties by index; every node after its dependencies.
    assert_eq!(plan.order(), &[0, 1, 6, 7, 2, 3, 4, 5, 8]);
    for (at, &node) in plan.order().iter().enumerate() {
        for dep in plan.deps(node) {
            assert!(
                plan.order()[..at].contains(dep),
                "{node} is ordered after its dependency {dep}"
            );
        }
    }

    // Worker A takes core; worker B, stealing ahead, asks for std before alloc began: alloc is
    // begun with it, to run first on B's stack (core is running, not waiting, so not with it).
    assert_eq!(plan.begin(0), Begin::Run { first: SmallVec::new() });
    assert_eq!(plan.begin(2), Begin::Run { first: SmallVec::from_slice(&[1]) });
    assert_eq!(plan.state(1), State::Running, "alloc began with std");
    // Worker C reaches alloc in the order: B has it.
    assert_eq!(plan.begin(1), Begin::Taken);
    assert_eq!(plan.end(1), Ok(()));
    assert_eq!(plan.end(2), Ok(()));
    assert_eq!(plan.end(2), Err(PlanError::NotRunning { node: 2, state: State::Done }));
    assert_eq!(
        plan.outcome(),
        Err(PlanError::Unfinished { node: 0, state: State::Running, left: 7 }),
        "an unfinished pass names its first node not done"
    );
    assert_eq!(plan.begin(0), Begin::Taken, "core runs on worker A");
    assert_eq!(plan.end(0), Ok(()));
    for node in [6, 7, 3, 4, 5, 8] {
        assert!(matches!(plan.begin(node), Begin::Run { .. }));
        assert_eq!(plan.end(node), Ok(()));
    }
    assert_eq!(plan.outcome(), Ok(()));
}

#[test]
fn a_cycle_or_a_missing_dep_is_refused_naming_the_node() {
    let cycle: [&[u32]; 3] = [&[2], &[0], &[1]];
    let refused = Plan::new(&[1, 1, 1], |n| cycle[n as usize].iter().copied()).err();
    assert!(matches!(refused, Some(PlanError::Cycle { .. })), "{refused:?}");
    let missing = Plan::new(&[1, 1], |n| if n == 1 { vec![5] } else { vec![] }).err();
    assert_eq!(missing, Some(PlanError::NoSuchDep { node: 1, dep: 5 }));
}

#[test]
fn costs_learn_each_keys_runs_and_predict_a_millisecond_for_one_never_seen() {
    let mut costs = Costs::default();
    assert_eq!(costs.predict(7), UNSEEN_NS);
    costs.observe(7, 400);
    assert_eq!(costs.predict(7), 400);
    costs.observe(7, 800);
    assert_eq!(costs.predict(7), 500, "a quarter of the way to the latest");
    assert_eq!(costs.predict(8), UNSEEN_NS);
}

/// A node begins only once every node under it, not just its direct dependencies, has begun:
/// method resolution reads impls across the whole closure. With `core <- alloc <- std <- app`,
/// a worker that reaches `app` first begins all three under it, deps first; one that then
/// asks for `std` finds it taken; and a dependent of a running node (whose closure is running)
/// begins nothing more.
///
/// ```text
///   0 core <- 1 alloc <- 2 std <- 3 app
///          <- 4 serde (core only)  <- 5 bin (app, serde)
///   6 lone
/// ```
#[test]
fn a_node_begins_its_whole_unbegun_dependency_closure_deps_first() {
    let deps: [&[u32]; 7] = [&[], &[0], &[1], &[2], &[0], &[3, 4], &[]];
    let mut plan = Plan::new(&[1; 7], |n| deps[n as usize].iter().copied()).unwrap();
    // `app`'s direct dependency is `std` alone; `alloc` and `core` are begun with it too.
    assert_eq!(plan.begin(3), Begin::Run { first: SmallVec::from_slice(&[0, 1, 2]) });
    for node in [0, 1, 2, 3] {
        assert_eq!(plan.state(node), State::Running, "node {node} began with app");
    }
    assert_eq!(plan.begin(2), Begin::Taken, "std began with app");
    // `bin` needs `app` (running, its closure with it) and `serde` (waiting): only `serde`.
    assert_eq!(plan.begin(5), Begin::Run { first: SmallVec::from_slice(&[4]) });
    assert_eq!(plan.begin(6), Begin::Run { first: SmallVec::new() });
    for node in [0, 1, 2, 3, 4, 5, 6] {
        assert_eq!(plan.end(node), Ok(()));
    }
    assert_eq!(plan.outcome(), Ok(()));

    // A diamond: `top` reaches `core` through both sides, and begins it once.
    let diamond: [&[u32]; 4] = [&[], &[0], &[0], &[1, 2]];
    let mut plan = Plan::new(&[1; 4], |n| diamond[n as usize].iter().copied()).unwrap();
    assert_eq!(plan.begin(3), Begin::Run { first: SmallVec::from_slice(&[0, 1, 2]) });
}

/// A node whose task gave way before it changed anything goes back to waiting with the nodes
/// begun with it that had not run, so the loop's rerun of it begins it, and them, anew; one
/// not running can't be given back.
#[test]
fn a_node_given_back_is_begun_anew_by_its_rerun() {
    let deps: [&[u32]; 3] = [&[], &[0], &[1]];
    let mut plan = Plan::new(&[1; 3], |n| deps[n as usize].iter().copied()).unwrap();
    assert_eq!(plan.begin(2), Begin::Run { first: SmallVec::from_slice(&[0, 1]) });
    // `core` ran; `alloc` gave way, so it and `std` (not run) go back.
    assert_eq!(plan.end(0), Ok(()));
    assert_eq!(plan.give_back(1), Ok(()));
    assert_eq!(plan.give_back(2), Ok(()));
    assert_eq!(plan.give_back(0), Err(PlanError::NotRunning { node: 0, state: State::Done }));
    assert_eq!(
        plan.outcome(),
        Err(PlanError::Unfinished { node: 1, state: State::Waiting, left: 2 }),
        "a node given back and never rerun is named"
    );
    // The rerun of `std` begins `alloc` again under it; `core` is done.
    assert_eq!(plan.begin(2), Begin::Run { first: SmallVec::from_slice(&[1]) });
    assert_eq!(plan.end(1), Ok(()));
    assert_eq!(plan.end(2), Ok(()));
    assert_eq!(plan.outcome(), Ok(()));
}

#[test]
fn costs_keep_only_the_keys_of_the_last_pass() {
    let mut costs = Costs::default();
    for key in [1, 2, 3] {
        costs.observe(key, 10);
    }
    costs.keep_only(&[2, 3]);
    assert_eq!(costs.len(), 2);
    assert_eq!(costs.predict(1), UNSEEN_NS, "a key no longer in the graph is forgotten");
    assert_eq!(costs.predict(2), 10);
}
