//! The database on a driven pool ([`blaze_graph::pool::Pool::driven`]), the pool a target with
//! no threads gets. The scenarios are shared with the seed's probe, which runs them built for
//! `wasm64-blaze-wasip1`. The scheduler's wait graph is the process's and the scenarios' queries
//! keep statics, so each test holds [`ONE`] and the binary is their own.
#![cfg(not(blaze_check))]
#![allow(clippy::disallowed_types, reason = "a test's own bookkeeping, off every hot path")]

use blaze_core::lock::Lock;

#[path = "driven/scenarios.rs"]
mod scenarios;

/// One test at a time: the wait graph and the queries' statics are the whole process's.
static ONE: Lock<()> = Lock::new("query.test.driven.one", ());

fn run(name: &str) {
    let _one = ONE.lock();
    let (_, scenario) =
        scenarios::ON_THE_SEED.iter().find(|(it, _)| *it == name).expect("a scenario of that name");
    scenario();
}

#[test]
fn a_query_waiting_on_the_one_worker_frees_it_for_other_jobs() {
    run("a_query_waiting_on_the_one_worker_frees_it_for_other_jobs");
}

#[test]
fn a_wait_that_would_close_a_cycle_across_views_is_cancelled() {
    run("a_wait_that_would_close_a_cycle_across_views_is_cancelled");
}

#[test]
fn a_query_asked_again_at_once_after_giving_way_ends() {
    run("a_query_asked_again_at_once_after_giving_way_ends");
}

#[test]
fn every_seed_scenario_has_a_test() {
    let source = include_str!("driven.rs");
    for (name, _) in scenarios::ON_THE_SEED {
        assert!(source.contains(&format!("fn {name}()")), "scenario {name} has no #[test] here");
    }
}
