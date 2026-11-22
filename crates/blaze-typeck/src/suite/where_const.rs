//! `where const { COND }` in impl selection (extensions.md, `where const`): the clause is a
//! nested goal of its impl, decided by the world's evaluation of the condition at the impl's
//! ground arguments. `false` is an unmet bound whose finding names the condition; over a
//! generic parameter the condition is not decided, which also fails, never holds.
//!
//! ```text
//! trait Flat {}
//! impl<T> Flat for T where const { small::<T>() } {}   // small: true for u8, false for u32
//! fn need<T: Flat>(t: T);
//! fn main<U>(u: U) { need(5u8); need(5u32); need(u); }
//! ```

use std::vec::Vec;

use super::{Want, check_body, check_body_by, differences, expect};
use crate::body::IntSuffix;
use crate::diag::{Because, DiagCode};
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem, PatId, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;
use crate::world::ConstHolds;

/// `small::<T>()`: true for `u8`, false for every other ground type.
fn small(args: &[PNode]) -> ConstHolds {
    match args {
        [PNode::Uint(UintTy::U8)] => ConstHolds::True,
        _ => ConstHolds::False,
    }
}

/// The world, the owner `main<U>(u: U)`, and the condition.
fn world() -> (Fx, ItemKey, ItemKey) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let flat = fx.tr("Flat", 1, Vec::new(), &[]);
    let p0 = fx.param(0);
    let cond = fx.condition("small::<T>()", small);
    let holds = fx.holds(cond, &[p0]);
    fx.imp("impl<T> Flat for T where const { small::<T>() }", 1, p0, Some((flat, &[])), &[], std::vec![holds]);
    let unit = fx.unit();
    let bound = fx.bound(flat, &[p0]);
    fx.func("need", 1, &[p0], unit, std::vec![bound]);
    let main = fx.func("main", 1, &[p0], unit, Vec::new());
    fx.finish();
    (fx, main, cond)
}

/// `need(5u8); need(5u32); need(u)`: expressions 0..=2, 3..=5 and 6..=8 are the callee, the
/// argument and the call.
fn body() -> (Bb, ExprId) {
    let mut b = Bb::new();
    let u = b.param("u");
    let mut stmts = Vec::new();
    for arg in [Some(UintTy::U8), Some(UintTy::U32), None] {
        let callee = b.path(&["need"]);
        let a = match arg {
            Some(t) => b.int(Some(IntSuffix::Uint(t))),
            None => b.local(u),
        };
        let call = b.call(callee, &[a]);
        stmts.push(b.semi(call));
    }
    let root = b.block(&stmts, None);
    (b, root)
}

const WANT: Want<'static> = Want {
    exprs: &[
        (ExprId(0), "fn need<u8>"),
        (ExprId(1), "u8"),
        (ExprId(2), "()"),
        (ExprId(3), "fn need<u32>"),
        (ExprId(4), "u32"),
        (ExprId(5), "()"),
        (ExprId(6), "fn need<P0>"),
        (ExprId(7), "P0"),
        (ExprId(8), "()"),
        (ExprId(9), "()"),
    ],
    pats: &[(PatId(0), "P0")],
    // `u8` meets the condition; `u32` does not; `U` is not decided. Each at the argument.
    diags: &[(DiagCode::Unsatisfied, ExprId(4)), (DiagCode::Unsatisfied, ExprId(7))],
};

#[test]
fn a_false_condition_is_an_unmet_bound_naming_it() {
    let (fx, main, cond) = world();
    let (b, root) = body();
    let c = check_body(&fx, &b, main, root, Plant::None, InferPlant::None);
    expect(&fx, &b, &c, &WANT);
    let mut because: Vec<Because> = c.diags.iter().filter_map(|d| d.because).collect();
    because.sort_by_key(|b| matches!(b, Because::Generic(_)));
    assert_eq!(because, [Because::False(cond), Because::Generic(cond)], "{:?}", c.diags);
}

/// D161 loop 3: a `where const` condition is answered where the solver meets it (infer
/// `fill_const_holds` through the pull), so no goal is asked again from its root for it
/// (`Solve::refills` 0). The control removes the cause: the stop strategy, which fills
/// nothing mid-step, asks goals again for the same body (rule 23).
#[test]
fn a_condition_is_pulled_and_no_goal_is_asked_again() {
    let (fx, main, _) = world();
    let (b, root) = body();
    let (pulled, rounds) = check_body_by(&fx, &b, main, root, Plant::None, InferPlant::None, false);
    let (stopped, again) = check_body_by(&fx, &b, main, root, Plant::None, InferPlant::None, true);
    assert_eq!(rounds, 0, "the pull asked goals again for a `where const` condition");
    assert!(again > 0, "the stop asked no goal again: the comparison compares nothing");
    assert!(pulled == stopped, "the pull and the stop answer the condition differently");
}

/// The control: the clause dropped where the impl's where-clauses are read, the impl holds for
/// every type and `need(5u32)` and `need(u)` check with no finding.
#[test]
fn control_where_const_dropped() {
    let (fx, main, _) = world();
    let (b, root) = body();
    let c = check_body(&fx, &b, main, root, Plant::None, InferPlant::DropConstClauses);
    assert!(!differences(&fx, &b, &c, &WANT).is_empty(), "the dropped condition was not caught: {:?}", c.diags);
}
