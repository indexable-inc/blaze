//! Which units of a build stand: a unit nothing can have changed under is one task of the
//! plane, its record looked up and replayed in it (`Front::stand`), not a node for each of its
//! phases. No IO, no clock: what the last build of a plan left and what the next one asks.
//!
//! On the Mac 2026-11-12 (six dependencies, a body edit) the other units' 187 phase tasks were
//! all hits, held 2.3 ms of work and took 5.3 to 7.3 ms of the daemon's 47.5 ms: a phase a node,
//! each waiting on the one before it and on its dependencies' last through a loop turn and a
//! worker's wake. A unit that stands is one node.
//!
//! A unit stands when all of these hold:
//! - it is sealed: a digest pins its package's sources (`serve::sealed::pin`, what the planner
//!   writes on its crates as `CrateData.sealed`), so no edit of the checkout reaches them;
//! - the last build of the same plan ended ok with the unit's record found or stored: the plan
//!   is its units' `Arc` itself (`Jobs::planned` hands an equal plan the same one), compared
//!   by pointer, never a digest of every unit's key made each build;
//! - the build's config is the last one's, its environment included: a unit's key reads
//!   variables, flags and features no unit's sources say;
//! - every unit it depends on stands: its key holds its dependencies' (a sealed library over an
//!   edited path crate is keyed anew), so a sealed unit over one that does not stand is posted
//!   with its phases.
//!
//! So a standing unit's key is the one its record was found or stored under last build, and
//! a record not found under it left the store between the builds (collected, its layer removed)
//! or was keyed by what none of the above pins (a dependency's build script that read the disk
//! anew): its task fails naming the unit (rule 3), never posts its phases quietly, and the
//! build that fails leaves nothing standing, so the next one posts every phase.

use std::sync::Arc;

use blaze_units::query::{BuildConfig, Units};

/// What a checkout's last build of a plan that ended ok left: the plan, its config, and by
/// unit whether the unit's record was found or stored (its verdict was done). Kept beside the
/// plan's other state by the units' identity (`LastPlan::stood`) and taken by the build that
/// reads it, so a build that does not end ok leaves none.
pub(super) struct Stood {
    pub(super) units: Arc<Units>,
    pub(super) config: Arc<BuildConfig>,
    pub(super) kept: Vec<bool>,
}

impl Stood {
    /// Whether it says anything of a build of `units` under `config`: they are its plan (the
    /// same `Arc`) and its config, environment and all (equal, or the same `Arc`). What a build
    /// asks before it asks which units are sealed.
    pub(super) fn applies(&self, units: &Arc<Units>, config: &Arc<BuildConfig>) -> bool {
        let plan = planted(Plant::IgnoresPlan) || Arc::ptr_eq(&self.units, units);
        let config = planted(Plant::IgnoresConfig)
            || Arc::ptr_eq(&self.config, config)
            || *self.config == **config;
        plan && config
    }
}

/// A bug planted here, for a test's control: each lets a unit stand that must not, and the
/// oracle (the build's records against a fresh daemon's) says so. Never planted outside a test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::serve) enum Plant {
    None,
    /// A unit stands under a plan other than the one that kept it.
    IgnoresPlan,
    /// A unit stands under a config whose environment moved.
    IgnoresConfig,
    /// A sealed unit stands whatever its dependencies do.
    DepsBlind,
}

#[cfg(test)]
thread_local! {
    pub(in crate::serve) static PLANT: std::cell::Cell<Plant> =
        const { std::cell::Cell::new(Plant::None) };
}

#[cfg(test)]
fn planted(plant: Plant) -> bool {
    PLANT.get() == plant
}

#[cfg(not(test))]
fn planted(plant: Plant) -> bool {
    let _ = plant;
    false
}

/// Where a unit is in the walk of [`hops`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Walk {
    Unseen,
    /// Its dependencies are being walked.
    Open,
    Stands,
    Posts,
}

/// By unit of `units`, whether it stands ([`self`]): one node, `Phase::Names` alone, whose
/// task stands it. `sealed` is by unit too. `None` when `stood` is of another plan or config,
/// or says nothing of these units: then every unit posts its phases.
pub(super) fn hops(
    stood: &Stood,
    units: &Arc<Units>,
    config: &Arc<BuildConfig>,
    sealed: &[bool],
) -> Option<Vec<bool>> {
    if !stood.applies(units, config) {
        return None;
    }
    let graph = &units.graph.units;
    if stood.kept.len() != graph.len() || sealed.len() != graph.len() {
        return None;
    }
    // Units come in no order of their dependencies: a walk, each unit settled once its
    // dependencies are. A unit graph has no cycle; a unit met again while it is open (or a
    // dependency past the graph) does not stand.
    let mut walk = vec![Walk::Unseen; graph.len()]; // alloc: once per build that a stood plan applies to
    let mut stack: Vec<(usize, usize)> = Vec::new(); // alloc: once per build, as deep as the graph
    for root in 0..graph.len() {
        if walk[root] != Walk::Unseen {
            continue;
        }
        walk[root] = Walk::Open;
        stack.push((root, 0));
        while let Some(top) = stack.last_mut() {
            let (at, next) = *top;
            if let Some(dep) = graph[at].dependencies.get(next) {
                top.1 += 1;
                let dep = dep.index as usize;
                if walk.get(dep) == Some(&Walk::Unseen) {
                    walk[dep] = Walk::Open;
                    stack.push((dep, 0));
                }
                continue;
            }
            stack.pop();
            let below = planted(Plant::DepsBlind)
                || (graph[at].dependencies.iter())
                    .all(|dep| walk.get(dep.index as usize) == Some(&Walk::Stands));
            walk[at] = match stood.kept[at] && sealed[at] && below {
                true => Walk::Stands,
                false => Walk::Posts,
            };
        }
    }
    Some(walk.into_iter().map(|it| it == Walk::Stands).collect()) // alloc: once per build, a flag a unit
}

#[cfg(test)]
mod tests {
    use blaze_units::{
        query::{Command, Invocation, Mount, Origin},
        schema::{Mode, UnitGraph},
    };
    use blaze_sim::{Config, Tape};

    use super::*;
    use crate::serve::tests::{of_graph, unit};

    const APP: &str = "path+file:///w#app@0.1.0";

    fn config(made: &str) -> BuildConfig {
        BuildConfig {
            invocation: Invocation {
                command: Command::Build,
                args: Vec::new(),
                root: "/w".to_owned(),
                cwd: "/w".to_owned(),
                env: vec![("MADE".to_owned(), made.to_owned())],
                files: Vec::new(),
            },
            mounts: vec![Mount { at: "/w".to_owned(), origin: Origin::View, under: String::new() }],
            host: "h".to_owned(),
            library: "/rustc/x/library".to_owned(),
            std: "/std".to_owned(),
            builds: "/builds".to_owned(),
            sources: "/sources".to_owned(),
            blaze: String::new(),
        }
    }

    /// A plan of `deps.len()` units, unit `at` on `deps[at]` (each below it).
    fn plan(deps: &[Vec<usize>]) -> Units {
        let units = deps.iter().map(|deps| unit(APP, "lib", Mode::Build, deps)).collect();
        of_graph("/w", String::new(), UnitGraph { version: 1, units, roots: Vec::new() })
    }

    /// One case: the last build's plan and config and what it kept, the units sealed, and
    /// whether the build asks the same plan (the same `Arc`, else an equal one of its own) and
    /// the same config (else its environment moved).
    #[derive(Debug)]
    struct Case {
        deps: Vec<Vec<usize>>,
        kept: Vec<bool>,
        sealed: Vec<bool>,
        same_plan: bool,
        same_config: bool,
    }

    /// The reference model: unit `at` stands when the plan and the config are the last
    /// build's, it was kept, it is sealed and each unit it depends on stands. Its dependencies
    /// are below it, so one pass in order settles each.
    fn model(case: &Case) -> Option<Vec<bool>> {
        if !case.same_plan || !case.same_config {
            return None;
        }
        let mut stands = Vec::new();
        for (at, deps) in case.deps.iter().enumerate() {
            let below = deps.iter().all(|dep| stands[*dep]);
            stands.push(case.kept[at] && case.sealed[at] && below);
        }
        Some(stands)
    }

    fn check(case: &Case) -> Result<(), String> {
        let last = Arc::new(plan(&case.deps));
        let asked = match case.same_plan {
            true => last.clone(), // clone: a refcount, the same plan
            false => Arc::new(plan(&case.deps)),
        };
        let was = Arc::new(config("1"));
        let now = match case.same_config {
            // An equal config of its own: equal is enough, the same `Arc` is not asked.
            true => Arc::new(config("1")),
            false => Arc::new(config("2")),
        };
        let stood = Stood { units: last, config: was, kept: case.kept.clone() }; // clone: a test case's flags
        let got = hops(&stood, &asked, &now, &case.sealed);
        let want = model(case);
        match got == want {
            true => Ok(()),
            false => Err(format!("{case:?}: stands {got:?}, the model says {want:?}")),
        }
    }

    fn case(tape: &Tape) -> Case {
        let count = tape.int_in(1..=9usize, "count");
        let mut deps = Vec::new();
        for at in 0..count {
            let mut of = Vec::new();
            for below in 0..at {
                if tape.bool("depends") {
                    of.push(below);
                }
            }
            deps.push(of);
        }
        Case {
            deps,
            kept: tape.vec(count..=count, "kept", |tape| tape.bool("kept")),
            sealed: tape.vec(count..=count, "sealed", |tape| tape.bool("sealed")),
            same_plan: tape.bool("same plan"),
            same_config: tape.bool("same config"),
        }
    }

    /// The invariant: a unit stands only when the last build was of this very plan and an
    /// equal config, kept the unit, the unit is sealed and every unit below it stands; any
    /// other plan or config stands nothing. `BLAZE_SIM_SEEDS=<n>` runs it longer.
    #[test]
    fn a_unit_stands_only_under_the_plan_and_config_that_kept_it_over_units_that_stand() {
        blaze_sim::check(Config::cases(256), |tape| check(&case(tape)));
    }

    /// The controls: each planted bug stands a unit the model posts, on a case that shows it.
    #[test]
    fn the_model_catches_each_planted_bug() {
        let every = |same_plan, same_config| Case {
            // Unit 1, sealed and kept, over unit 0, which was not kept.
            deps: vec![Vec::new(), vec![0]],
            kept: vec![false, true],
            sealed: vec![true, true],
            same_plan,
            same_config,
        };
        for (plant, case) in [
            (Plant::IgnoresPlan, every(false, true)),
            (Plant::IgnoresConfig, every(true, false)),
            (Plant::DepsBlind, every(true, true)),
        ] {
            assert!(check(&case).is_ok(), "{case:?} holds unplanted");
            PLANT.set(plant);
            let caught = check(&case);
            PLANT.set(Plant::None);
            assert!(caught.is_err(), "{plant:?} was not caught");
        }
    }
}
