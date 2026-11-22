//! `Solver::why_no` (why_no.rs): the chain from an unmet goal to the goal under it that answers
//! No by itself, each step said by what it is of the step above (a tuple's element, an ADT's
//! field, a raw pointer's pointee), and why the leaf has no solution. Each chain has its twin:
//! the same shape with the cause removed (rule 23) answers [`Leaf::NotNo`] with the root alone,
//! so the chain is the cause's, not the shape's.
//!
//! The Reference, "Send and Sync" / "Auto traits": an auto trait holds for a tuple, an ADT, a
//! raw pointer (here: core's `!Send for *const T` is not in this world, see auto.solve) when
//! it holds for each constituent; `impl<T> !Send for Rc<T>` (the prelude) refuses `Rc<u8>`.

use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_own::Alloc;

use crate::assembly::{Clause, Goal};
use crate::eval::Solver;
use crate::facts::{Read, Stopping};
use crate::term::TypingMode;
use crate::testing::parse::{CheckA, DeclA, parse_file};
use crate::testing::run::PRELUDE;
use crate::testing::tys::TestTys;
use crate::testing::world::{Arena, Scope, World, def_of};
use crate::why_no::{Leaf, Reached};

/// The chain of each goal of `text` (one case per goal), in order: each step's `Reached` and
/// the leaf.
fn chains(text: &str) -> Result<Vec<(Vec<Reached>, Leaf)>, String> {
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
    let mut out = Vec::new();
    for case in &f.cases {
        for g in &case.goals {
            let CheckA::Goal(p) = &g.check else { return Err(format!("line {}: not a goal", g.line)) };
            let mut tys = TestTys::new();
            let mut vars = std::collections::HashMap::new();
            let mut solver = Solver::new_in(alloc);
            let sc = Scope { params: &case.params, bound: &[] };
            let mut env_cl: Vec<Clause> = Vec::new();
            let mut cl: Vec<Clause> = Vec::new();
            {
                let mut a = Arena { tys: &mut tys, vars: &mut vars };
                let sized: Vec<bool> = case.params.iter().map(|_| false).collect();
                world.names.case_sized(&mut a, case.line, &sized, &mut env_cl).map_err(|e| format!("the env: {e}"))?;
                world.names.pred(&mut a, sc, g.line, p, &mut cl).map_err(|e| format!("line {}: {e}", g.line))?;
            }
            let mut reads: blaze_own::Vec<Read, Alloc<'static>> = blaze_own::Vec::new_in(alloc);
            let env = solver.env(&mut tys, &world, &env_cl, &mut reads).map_err(|s| format!("the env: {s:?}"))?;
            let [c]: [Clause; 1] = cl.try_into().map_err(|_| format!("line {}: a goal is one predicate", g.line))?;
            let why = solver
                .why_no(&mut tys, &mut Stopping(&world), TypingMode::Analysis, Goal { env, pred: c.pred, bound: c.bound }, &mut reads)
                .map_err(|s| format!("line {}: why_no stopped: {s:?}", g.line))?;
            out.push((why.steps.iter().map(|s| s.reached).collect(), why.leaf));
        }
    }
    Ok(out)
}

fn chain(text: &str) -> (Vec<Reached>, Leaf) {
    let mut all = chains(text).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(all.len(), 1, "one goal: {text}");
    all.remove(0)
}

fn assert_holds(text: &str) {
    let (steps, leaf) = chain(text);
    assert_eq!(leaf, Leaf::NotNo, "TWIN holds: {text}");
    assert_eq!(steps, [Reached::Root], "TWIN: the root alone: {text}");
}

/// `(u8, Rc<u8>): Send`: element 1 of the tuple, refused by the prelude's negative impl.
/// TWIN `(u8, u8): Send` holds.
#[test]
fn a_tuple_element_under_a_negative_impl() {
    let (steps, leaf) = chain("case c = why_no.rs\ngoal (u8, Rc<u8>): Send => no\n");
    assert_eq!(steps, [Reached::Root, Reached::TupleElement { index: 1 }], "the tuple's element 1");
    assert!(matches!(leaf, Leaf::NegativeImpl { .. }), "refused by `impl<T> !Send for Rc<T>`: {leaf:?}");
    assert_holds("case c = why_no.rs\ngoal (u8, u8): Send => yes\n");
}

/// `S: Send` for `struct S(u8, Rc<u8>)`: field 1 of `S`, then the negative impl.
/// TWIN `struct S(u8, u16)` holds.
#[test]
fn an_adt_field_chain() {
    let (steps, leaf) = chain("case c = why_no.rs\nstruct S(u8, Rc<u8>)\ngoal S: Send => no\n");
    assert_eq!(steps, [Reached::Root, Reached::Field { adt: def_of("adt", "S"), field: 1 }], "field 1 of `S`");
    assert!(matches!(leaf, Leaf::NegativeImpl { .. }), "refused by `impl<T> !Send for Rc<T>`: {leaf:?}");
    assert_holds("case c = why_no.rs\nstruct S(u8, u16)\ngoal S: Send => yes\n");
}

/// `*const Rc<u8>: Send`: the raw pointer's pointee (the structural rule; auto.solve
/// `auto_raw_ptr_pointee`), then the negative impl. TWIN `*const u8: Send` holds here.
#[test]
fn a_raw_pointers_pointee() {
    let (steps, leaf) = chain("case c = why_no.rs\ngoal *const Rc<u8>: Send => no\n");
    assert_eq!(steps, [Reached::Root, Reached::PtrTarget], "the pointee");
    assert!(matches!(leaf, Leaf::NegativeImpl { .. }), "refused by `impl<T> !Send for Rc<T>`: {leaf:?}");
    assert_holds("case c = why_no.rs\ngoal *const u8: Send => yes\n");
}

/// A trait with no impl: the root is the leaf, with no candidate at all. TWIN: an impl for
/// `u8` holds.
#[test]
fn no_impl_is_the_roots_own_no() {
    let (steps, leaf) = chain("case c = why_no.rs\ntrait Tr\ngoal u8: Tr => no\n");
    assert_eq!(steps, [Reached::Root], "the root alone");
    assert_eq!(leaf, Leaf::NoImpl, "no candidate");
    assert_holds("case c = why_no.rs\ntrait Tr\nimpl Tr for u8\ngoal u8: Tr => yes\n");
}
