//! The prover's cost is bounded by design (`reach`'s module doc, "The cost, by design").
//!
//! The invariant: the work a proof does is at most the bound of its body
//! (`reach::work_bound` of its statements and terminators), and the same on every run. Held
//! here over drawn bodies (chains of guards, sums and guarded indexing), over a planted
//! pathological chain of the kind that hung the walk (wide comparisons whose facts chain, and
//! sums of them), and against a control that skips the budget check, which the oracle
//! catches. The near-linear claim is held per step: doubling the chain raises its work per
//! step by at most a quarter.

use blaze_sim::{Config, Tape};
use hir_def::DefWithBodyId;
use hir_expand::fixture;

use super::{fixture::crate_named, fns::fn_named};
use crate::reach::{self, PLANT_UNBOUNDED_WORK, Reach, WORK_BOUND_FOR_TEST};

const MINICORE: &str = "//- minicore: sized, copy, clone, drop, option, result, fn, panic, index, slice, builtin_impls, reach\n";

/// The proof of `probe` in `source`, or what breaks the invariant: work past the bound, a
/// bound that is not its body's, or two runs of one proof that differ.
fn work_checked(source: &str) -> Result<Reach, String> {
    let db = fixture::db(source).into_db();
    let krate = crate_named(&db, "kernel");
    blaze_query::db::attach(&db, || {
        let def = DefWithBodyId::FunctionId(fn_named(&db, krate, "probe"));
        // `explain` is not kept, so each is a proof of its own.
        let first = reach::explain(&db, def, &[]);
        let second = reach::explain(&db, def, &[]);
        if first.work > first.bound {
            // alloc: a test's message
            return Err(format!(
                "the proof did {} units of work, past its bound of {}\n{source}",
                first.work, first.bound
            ));
        }
        let analysed = first.steps > 0;
        if analysed && WORK_BOUND_FOR_TEST.get().is_none() && first.bound != reach::work_bound(first.steps) {
            // alloc: a test's message
            return Err(format!(
                "the bound {} is not that of {} steps\n{source}",
                first.bound, first.steps
            ));
        }
        if first != second {
            // alloc: a test's message
            return Err(format!(
                "two runs of one proof differ: {} and {} units\n{source}",
                first.work, second.work
            ));
        }
        Ok(first)
    })
}

/// Below `n`, at least 1.
fn below(tape: &Tape, n: u32, label: &'static str) -> u32 {
    tape.int_in(0..=n - 1, label)
}

fn pick<'a>(tape: &Tape, of: &[&'a str], label: &'static str) -> &'a str {
    *tape.one_of(of, label)
}

/// One statement of a drawn body, over `v`, `a`, `b` and `c`.
fn statement(tape: &Tape) -> String {
    let (x, y) = (pick(tape, &["a", "b", "c"], "x"), pick(tape, &["a", "b", "c"], "y"));
    let k = below(tape, 9, "k");
    // alloc: a test's text
    match below(tape, 5, "statement") {
        0 => format!("if {x} < {y} {{ return {k}; }}"),
        1 => format!("if {x} < v.len() {{ return v[{x}] as usize; }}"),
        2 => format!("let {x} = {x} / 2 + {k};"),
        3 => format!("let {x} = {x} % 8 + {y} % 8;"),
        _ => format!("if {y} <= {x} && {x} < v.len() {{ return v[{x} - {y}] as usize; }}"),
    }
}

fn drawn(tape: &Tape) -> String {
    let count = 1 + below(tape, 12, "statements");
    let body: Vec<String> = (0..count).map(|_| statement(tape)).collect();
    let checks = if below(tape, 2, "overflow checks") == 0 { " cfg:overflow_checks" } else { "" };
    // alloc: a test's fixture
    format!(
        "{MINICORE}//- /lib.rs crate:kernel{checks}\n\
         pub fn probe(v: &[u32], a: usize, b: usize, c: usize) -> usize {{\n    {}\n    0\n}}\n",
        body.join("\n    ")
    )
}

/// The invariant, over drawn bodies: the work is within the body's bound and the same twice.
#[test]
fn a_proofs_work_is_within_its_bound_and_the_same_each_run() {
    blaze_sim::check(Config::cases(100), |tape| {
        let source = drawn(tape);
        if let Err(what) = work_checked(&source) {
            panic!("{what}");
        }
    })
}

/// `n` strict comparisons of `u64`s that chain (`x0 < x1 < .. < xn`, each an early return
/// when it fails), then `n` sums of neighbours' quarters with overflow checks: every check
/// asks of a state holding the whole chain, the shape of the wide arithmetic that hung.
fn chain(n: usize) -> String {
    let params: Vec<String> = (0..=n).map(|i| format!("x{i}: u64")).collect(); // alloc: a test's text
    let mut body = String::new(); // alloc: a test's text
    for i in 0..n {
        body.push_str(&format!("    if x{i} >= x{} {{ return 0; }}\n", i + 1)); // alloc: as above
    }
    for i in 0..n {
        body.push_str(&format!("    let s{i} = x{i} / 4 + x{} / 4;\n", i + 1)); // alloc: as above
    }
    let total: Vec<String> = (0..n).map(|i| format!("s{i}")).collect(); // alloc: as above
    // alloc: a test's fixture
    format!(
        "{MINICORE}//- /lib.rs crate:kernel cfg:overflow_checks\n\
         pub fn probe({}) -> u64 {{\n{body}    {}\n}}\n",
        params.join(", "),
        total.join(" ^ ")
    )
}

/// The planted pathology: 30 chained comparisons and 30 sums end, proved or refused, within
/// their bound.
#[test]
fn a_chain_of_wide_comparisons_and_sums_ends_within_its_bound() {
    let reach = work_checked(&chain(30)).unwrap_or_else(|what| panic!("{what}"));
    assert!(reach.work <= reach.bound, "{reach:?}");
    assert!(reach.steps > 60, "the chain lowers to its statements: {reach:?}");
}

/// The control: with the budget check skipped, a proof run with a small bound goes past it,
/// and the oracle refuses it; with the check, the same proof stops at its bound, refused,
/// and says so.
#[test]
fn the_work_oracle_catches_a_proof_run_past_its_bound() {
    let source = chain(8);
    WORK_BOUND_FOR_TEST.set(Some(64));
    let bounded = work_checked(&source);
    PLANT_UNBOUNDED_WORK.set(true);
    let caught = work_checked(&source);
    PLANT_UNBOUNDED_WORK.set(false);
    WORK_BOUND_FOR_TEST.set(None);
    let bounded = bounded.unwrap_or_else(|what| panic!("{what}"));
    assert_eq!(bounded.work, 64, "{bounded:?}");
    let why = bounded.refusal().expect("a proof out of work is refused");
    assert!(why.contains("work bound") && why.contains("walking block"), "{why}");
    let Err(what) = caught else { panic!("the planted unbounded proof was not caught: {caught:?}") };
    assert!(what.contains("past its bound"), "{what}");
}

/// Near-linear: the work per statement is bounded as the body grows. A chain twice as long
/// does at most `K = 5/4` times the work per step of the shorter, measured against the
/// steps (what the bound is of), not `n`. `K = 1` is linear; the quarter is what the design
/// leaves: a write's path in a state's slot tree grows by a level per sixteenfold locals, the
/// first write after a copy moves an index's list of chunks (`f / 64` pointers), liveness is a
/// word per 64 locals per edge, and the shape of the body changes a little with its length.
/// A cost linear in the state per step (an edge copying its slots or facts, a join scanning
/// every slot) makes the ratio about 2 and fails.
#[test]
fn work_grows_near_linearly_with_the_body() {
    let short = work_checked(&chain(12)).unwrap_or_else(|what| panic!("{what}"));
    let long = work_checked(&chain(24)).unwrap_or_else(|what| panic!("{what}"));
    // alloc: a test's message
    let said = format!(
        "(steps, work, bound): chain(12) ({}, {}, {}), chain(24) ({}, {}, {})",
        short.steps, short.work, short.bound, long.steps, long.work, long.bound
    );
    assert!(short.stopped.is_none() && long.stopped.is_none(), "{said}\n{short:?}\n{long:?}");
    // work(24) / steps(24) <= 5/4 * work(12) / steps(12), in integers.
    assert!(4 * long.work * short.steps <= 5 * short.work * long.steps, "{said}");
}
