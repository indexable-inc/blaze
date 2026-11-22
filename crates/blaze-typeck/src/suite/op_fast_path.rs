//! The builtin operator's fast path (rules/builtin_ops.rs `classify`, from rules/op.rs `binop`): when both operand
//! types are known primitive scalars and the pair is one the operator is builtin on, the check
//! records no callee, registers no obligation and asks the trait solver nothing. A sampler
//! profile of hello world under `blaze run` had 18% of the run under `rules::op` ->
//! `InferCtx::may_hold` -> `Solver::evaluate`: every `i32 + i32` asked the solver.
//!
//! Why the fast path cannot change an answer, by the Reference:
//! - [expr.arith-logic.behavior]: "This table summarizes the behavior of arithmetic and logical
//!   binary operators on primitive types and which traits are used to overload these operators
//!   for other types." On primitive integer types `+` IS the integer addition; [expr.cmp]
//!   likewise for `==` on primitive types. The check's final state already takes the builtin
//!   operator for every such pair, whatever the trait said.
//! - [items.impl.trait.orphan-rule.general]: "At least one of the types `T0..=Tn` must be a local
//!   type" unless the trait is local: an `impl Add<i32> for i32` can only be written by the crate
//!   that declares `Add`, and the pinned library's (core/src/ops/arith.rs, `add_impl!`: `fn
//!   add(self, other: $t) -> $t { self + other }`) is the builtin operator itself.
//! - Both types are known (no inference variable, no projection, no type parameter): nothing is
//!   left for an obligation to infer.

use std::vec::Vec;

use blaze_own::Alloc;

use crate::body::{BinOp, Expr, UnOp};
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, IntTy, LangItem, Mutbl, PatId};
use crate::infer::InferCtx;
use crate::portable::{PNode, PTy};
use crate::world::Needs;
use crate::{Clock, Outcome, Tally, check_counted};

/// A clock that counts its reads (the tally's times are not what this test reads).
struct Ticks(core::cell::Cell<u64>);

impl Clock for Ticks {
    fn now_ns(&self) -> u64 {
        let t = self.0.get();
        self.0.set(t + 1);
        t
    }
}

/// ```text
/// #[lang = "sized"] trait Sized {}
/// #[lang = "eq"] trait PartialEq<Rhs> { fn eq(&self, other: &Rhs) -> bool; }
/// impl PartialEq<i32> for i32 {}
/// #[lang = "add"] trait Add<Rhs> { type Output; fn add(self, rhs: Rhs) -> Self::Output; }
/// impl Add<i32> for i32 { type Output = i32; }
/// impl Add<&i32> for i32 { type Output = i32; }
/// fn f(a: <a>, b: <b>) -> bool { <body(a, b)> }
/// ```
/// checked once with a tally: the solver calls it counted, and the callee rows it left.
fn solver_calls(b_is_ref: bool, body: impl Fn(&mut Bb, PatId, PatId) -> ExprId) -> (u64, usize) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let i32_: PTy = fx.ty(PNode::Int(IntTy::I32));
    let r_i32 = fx.r(Mutbl::Not, i32_);
    let bool_ = fx.ty(PNode::Bool);
    let (rp0, rp1) = (fx.r(Mutbl::Not, p0), fx.r(Mutbl::Not, p1));
    let partial_eq = fx.tr("PartialEq", 2, Vec::new(), &[]);
    fx.lang(LangItem::PartialEq, partial_eq);
    fx.trait_fn(partial_eq, "eq", true, &[rp0, rp1], bool_);
    fx.imp("eq for i32", 0, i32_, Some((partial_eq, &[i32_])), &[], Vec::new());
    let add = fx.tr("Add", 2, Vec::new(), &["Output"]);
    fx.lang(LangItem::Add, add);
    let out = fx.alias(add, "Output", &[p0, p1]);
    fx.trait_fn(add, "add", true, &[p0, p1], out);
    fx.imp("add for i32", 0, i32_, Some((add, &[i32_])), &[("Output", i32_)], Vec::new());
    fx.imp("add &i32 for i32", 0, i32_, Some((add, &[r_i32])), &[("Output", i32_)], Vec::new());
    let b_ty = if b_is_ref { r_i32 } else { i32_ };
    let owner = fx.func("f", 0, &[i32_, b_ty], bool_, Vec::new());
    fx.finish();
    let mut bb = Bb::new();
    let (pa, pb) = (bb.param("a"), bb.param("b"));
    let tail = body(&mut bb, pa, pb);
    let root = bb.block(&[], Some(tail));
    let mut cx = InferCtx::new_in(Alloc::global());
    let mut tally = Tally::default();
    let clock = Ticks(core::cell::Cell::new(0));
    let out = check_counted(&bb.view(owner, root), &fx, &mut cx, &Needs::new_in(Alloc::global()), &mut tally, &clock, Alloc::global());
    let c = match out {
        Ok(Outcome::Done(c)) => c,
        Ok(Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("{e:?}"),
    };
    assert!(c.diags.is_empty(), "diagnostics: {:?}", c.diags);
    (tally.solve.calls, c.callees.len())
}

/// `fn f(a: i32, b: i32) -> bool { a + b + -a == b + a }`: every operator is builtin on known
/// `i32` operands, so the operators ask the solver NOTHING (no call beyond those of the same
/// function with the body `{ true }`) and leave no callee row. The control, `fn f(a: i32, b:
/// &i32) -> bool { a + b + -a == a }`, asks it (`i32: Add<&i32>` is a trait's method): the
/// counter counts. Without the fast path this test prints "solver calls for a body of builtin
/// operators, beyond a body of none" with `left` above zero.
#[test]
fn builtin_operators_on_known_scalars_ask_the_solver_nothing() {
    let (calls, rows) = solver_calls(false, |bb, a, b| {
        let (x, y) = (bb.local(a), bb.local(b));
        let sum = bb.e(Expr::Binary { op: BinOp::Add, lhs: x, rhs: y });
        let operand = bb.local(a);
        let neg = bb.e(Expr::Unary { op: UnOp::Neg, operand });
        let left = bb.e(Expr::Binary { op: BinOp::Add, lhs: sum, rhs: neg });
        let (y, x) = (bb.local(b), bb.local(a));
        let right = bb.e(Expr::Binary { op: BinOp::Add, lhs: y, rhs: x });
        bb.e(Expr::Binary { op: BinOp::Eq, lhs: left, rhs: right })
    });
    // The same function with no operator (`{ true }`): the solver calls the body makes for
    // other reasons (its signature's and locals' obligations), which the operators must not add to.
    let (baseline, _) = solver_calls(false, |bb, _, _| bb.bool());
    assert_eq!(calls - baseline, 0, "solver calls for a body of builtin operators, beyond a body of none");
    assert_eq!(rows, 0, "a builtin operator left a callee row");

    let (calls, rows) = solver_calls(true, |bb, a, b| {
        let (x, y) = (bb.local(a), bb.local(b));
        let sum = bb.e(Expr::Binary { op: BinOp::Add, lhs: x, rhs: y });
        let operand = bb.local(a);
        let neg = bb.e(Expr::Unary { op: UnOp::Neg, operand });
        let left = bb.e(Expr::Binary { op: BinOp::Add, lhs: sum, rhs: neg });
        let x = bb.local(a);
        bb.e(Expr::Binary { op: BinOp::Eq, lhs: left, rhs: x })
    });
    assert!(calls > baseline, "the control (`i32 + &i32`) asked the solver nothing: the counter does not count");
    assert_eq!(rows, 1, "the control's `i32 + &i32` is `Add::add`, one row");
}
