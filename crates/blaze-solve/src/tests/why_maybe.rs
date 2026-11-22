//! `Solver::why_maybe` (why_maybe.rs): the chain from an ambiguous goal to the goal under it
//! that is ambiguous by itself, one test per leaf kind. Each has its twin (rule 23): the same
//! shape with the cause removed holds, and answers [`Leaf::NotMaybe`] with the root alone, so
//! the leaf is the cause's, not the shape's.
//!
//! The Reference, "Trait and lifetime bounds" / "Implementations": an impl holds where its
//! where-clauses hold; a goal whose type is not known yet, or that two where-clauses answer
//! differently, cannot be decided (rustc's E0282 / E0283); a recursion past the limit is
//! E0275.

use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_own::Alloc;

use crate::assembly::{Clause, Goal, Source};
use crate::eval::Solver;
use crate::facts::{Read, Stopping};
use crate::term::TypingMode;
use crate::testing::parse::{CheckA, DeclA, parse_file};
use crate::testing::run::PRELUDE;
use crate::testing::tys::TestTys;
use crate::testing::world::{Arena, Scope, World, def_of};
use crate::why_maybe::{Leaf, LimitHit, Unopened};
use crate::why_no::Reached;

/// The chain of the one goal of `text`: each step's `Reached`, and the leaf.
fn chain(text: &str) -> (Vec<Reached>, Leaf) {
    let r = (|| -> Result<(Vec<Reached>, Leaf), String> {
        let prelude = parse_file(PRELUDE).map_err(|e| format!("the prelude: {e}"))?;
        let f = parse_file(text).map_err(|e| format!("the fixture: {e}"))?;
        let mut decls: Vec<(u32, &DeclA)> = Vec::new();
        decls.extend(prelude.decls.iter().map(|(l, d)| (*l, d)));
        decls.extend(f.decls.iter().map(|(l, d)| (*l, d)));
        for c in &f.cases {
            decls.extend(c.decls.iter().map(|(l, d)| (*l, d)));
        }
        let world = World::build(&decls).map_err(|e| format!("the world: {e}"))?;
        let alloc = blaze_own::testing::heap().alloc();
        let [case] = &f.cases[..] else { return Err(format!("one case: {text}")) };
        let [g] = &case.goals[..] else { return Err(format!("one goal: {text}")) };
        let CheckA::Goal(p) = &g.check else { return Err(format!("line {}: not a goal", g.line)) };
        let mut tys = TestTys::new();
        let mut vars = std::collections::HashMap::new();
        let mut solver = Solver::new_in(alloc);
        let sc = Scope { params: &case.params, bound: &[] };
        let mut env_cl: Vec<Clause> = Vec::new();
        let mut cl: Vec<Clause> = Vec::new();
        {
            let mut a = Arena { tys: &mut tys, vars: &mut vars };
            // As testing/run.rs `run_case`: the case's where-clauses, then each parameter's
            // implicit `Sized`.
            for e in &case.env {
                world.names.pred(&mut a, sc, case.line, e, &mut env_cl).map_err(|e| format!("the env: {e}"))?;
            }
            let sized: Vec<bool> = case.params.iter().map(|_| false).collect();
            world.names.case_sized(&mut a, case.line, &sized, &mut env_cl).map_err(|e| format!("the env: {e}"))?;
            world.names.pred(&mut a, sc, g.line, p, &mut cl).map_err(|e| format!("line {}: {e}", g.line))?;
        }
        let mut reads: blaze_own::Vec<Read, Alloc<'static>> = blaze_own::Vec::new_in(alloc);
        let env = solver.env(&mut tys, &world, &env_cl, &mut reads).map_err(|s| format!("the env: {s:?}"))?;
        let [c]: [Clause; 1] = cl.try_into().map_err(|_| format!("line {}: a goal is one predicate", g.line))?;
        let why = solver
            .why_maybe(&mut tys, &mut Stopping(&world), TypingMode::Analysis, Goal { env, pred: c.pred, bound: c.bound }, &mut reads)
            .map_err(|s| format!("line {}: why_maybe stopped: {s:?}", g.line))?;
        Ok((why.steps.iter().map(|s| s.reached).collect(), why.leaf))
    })();
    r.unwrap_or_else(|e| panic!("{e}"))
}

fn assert_decided(text: &str) {
    let (steps, leaf) = chain(text);
    assert_eq!(leaf, Leaf::NotMaybe, "TWIN is decided: {text}");
    assert_eq!(steps, [Reached::Root], "TWIN: the root alone: {text}");
}

/// `?0: Tr`: the self type is a variable (forced ambiguity), term 0. TWIN `u8: Tr` holds.
#[test]
fn an_unresolved_self_type() {
    let (steps, leaf) = chain("case c = why_maybe.rs\ntrait Tr\nimpl Tr for u8\ngoal ?0: Tr => maybe\n");
    assert_eq!(steps, [Reached::Root], "the root alone");
    assert!(matches!(leaf, Leaf::Unresolved { term: 0, .. }), "`Self` is not known: {leaf:?}");
    assert_decided("case c = why_maybe.rs\ntrait Tr\nimpl Tr for u8\ngoal u8: Tr => yes\n");
}

/// `Vec<?0>: Debug`: the prelude's `impl<T: Debug> Debug for Vec<T>`, then its first
/// where-clause still undecided (`?0: Sized`, the implied one, comes first in the test world's
/// order), undecided by its variable. TWIN `Vec<u8>: Debug` holds.
#[test]
fn an_unresolved_variable_under_an_impl_clause() {
    let (steps, leaf) = chain("case c = why_maybe.rs\ngoal Vec<?0>: Debug => maybe\n");
    assert_eq!(steps.len(), 2, "the root, then the impl's where-clause: {steps:?}");
    assert!(matches!(steps[1], Reached::ImplClause { .. }), "an impl's where-clause: {steps:?}");
    assert!(matches!(leaf, Leaf::Unresolved { term: 0, .. }), "`?0` is not known: {leaf:?}");
    assert_decided("case c = why_maybe.rs\ngoal Vec<u8>: Debug => yes\n");
}

/// `<?0 as Iterator>::Item == u32`: the projection's self type is a variable; the alias is
/// term 0. TWIN `<Vec<u8> as Iterator>::Item == u8` holds.
#[test]
fn an_unresolved_projection_self_type() {
    let (steps, leaf) = chain("case c = why_maybe.rs\ngoal <?0 as Iterator>::Item == u32 => maybe\n");
    assert_eq!(steps.first(), Some(&Reached::Root), "{steps:?}");
    assert!(matches!(leaf, Leaf::Unresolved { term: 0, .. }), "the alias names `?0`: {leaf:?}");
    assert_decided("case c = why_maybe.rs\ngoal <Vec<u8> as Iterator>::Item == u8 => yes\n");
}

/// `T: Foo<?0>` under `T: Foo<u8>` and `T: Foo<u16>`: both where-clauses hold with different
/// answers for `?0` (core.solve `where_bounds_overlap_ambiguous`). TWIN `T: Foo<u16>`.
#[test]
fn two_where_clauses_may_hold() {
    let (steps, leaf) = chain("case c = why_maybe.rs\ntrait Foo<X>\nparams T\nenv T: Foo<u8>; T: Foo<u16>\ngoal T: Foo<?0> => maybe\n");
    assert_eq!(steps, [Reached::Root], "the root alone");
    match leaf {
        Leaf::Candidates { first, second, count } => {
            assert_eq!(count, 2, "two where-clauses");
            assert!(matches!(first.source, Source::ParamEnv { .. }) && matches!(second.source, Source::ParamEnv { .. }), "{first:?} {second:?}");
        }
        other => panic!("two candidates may hold: {other:?}"),
    }
    assert_decided("case c = why_maybe.rs\ntrait Foo<X>\nparams T\nenv T: Foo<u8>; T: Foo<u16>\ngoal T: Foo<u16> => yes\n");
}

/// `u8: Foo` under `impl<T> Foo for T where Vec<T>: Foo`: each step asks of a larger type
/// until the depth limit (core.solve `growing_goal_overflows`). TWIN: the impl with no
/// where-clause holds.
#[test]
fn the_depth_limit() {
    let (steps, leaf) = chain("case c = why_maybe.rs\ntrait Foo\nimpl<T> Foo for T where Vec<T>: Foo\ngoal u8: Foo => overflow\n");
    assert!(steps.len() > 2, "the growing chain: {}", steps.len());
    // Clause 1: `Vec<T>: Foo`, after the implied `T: Sized`, which holds. At the deepest
    // frame the limit is met by whichever of its goals is entered first there: clause 0,
    // `Vec<..Vec<u8>..>: Sized`, which holds by Rust's rules but cannot be entered at the
    // limit (eval.rs `maybe_cause` follows the first goal still pending, the one that
    // overflowed). So the last step may be clause 0; every step above it is clause 1.
    let (last, above) = steps[1..].split_last().expect("more than two steps");
    assert!(above.iter().all(|s| matches!(s, Reached::ImplClause { clause: 1, .. })), "each step above the limit the impl's where-clause `Vec<T>: Foo`: {steps:?}");
    assert!(matches!(last, Reached::ImplClause { clause: 0 | 1, .. }), "the step at the limit is one of the impl's two clauses: {steps:?}");
    assert!(matches!(leaf, Leaf::Limit(LimitHit::Depth { .. })), "the recursion limit: {leaf:?}");
    assert_decided("case c = why_maybe.rs\ntrait Foo\nimpl<T> Foo for T\ngoal u8: Foo => yes\n");
}

/// `S: Foo` under `impl Foo for S where S: Foo`: an inductive cycle the search answers
/// ambiguous (core.solve `misc_trait_cycle_overflows`). TWIN: no where-clause, it holds.
#[test]
fn an_inductive_cycle() {
    let (steps, leaf) = chain("case c = why_maybe.rs\ntrait Foo\nstruct S\nimpl Foo for S where S: Foo\ngoal S: Foo => overflow\n");
    assert_eq!(steps, [Reached::Root], "the cycle closes on the root");
    assert_eq!(leaf, Leaf::Cycle, "a cycle");
    assert_decided("case c = why_maybe.rs\ntrait Foo\nstruct S\nimpl Foo for S\ngoal S: Foo => yes\n");
}

/// `Op: Send` for an opaque type whose hidden type this world does not give: the auto-trait
/// rule cannot look into it. TWIN: an opaque whose hidden type is `u32` holds.
#[test]
fn an_opaque_with_no_hidden_type() {
    let (steps, leaf) = chain("case c = why_maybe.rs\nopaque Op: Clone\ngoal opaque#Op: Send => maybe\n");
    assert_eq!(steps.first(), Some(&Reached::Root), "{steps:?}");
    assert_eq!(leaf, Leaf::Unopened(Unopened::Hidden { opaque: def_of("opaque", "Op") }), "the hidden type is the missing record");
    assert_decided("case c = why_maybe.rs\nopaque Oh: Clone = u32\ngoal opaque#Oh: Send => yes\n");
}

/// A goal that fails is not ambiguous: no chain. (`why_no` walks it.)
#[test]
fn a_failing_goal_is_not_maybe() {
    let (steps, leaf) = chain("case c = why_maybe.rs\ntrait Tr\ngoal u8: Tr => no\n");
    assert_eq!(leaf, Leaf::NotMaybe, "a No is no ambiguity");
    assert_eq!(steps, [Reached::Root]);
}
