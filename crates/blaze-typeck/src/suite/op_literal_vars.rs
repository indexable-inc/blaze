//! An operator whose operand is an integer- or float-literal VARIABLE (`a + 1`, `1 << n`,
//! `a == 0`, `1.0 + x`) is the built-in operator when both operand types are of one built-in
//! class (rules/builtin_ops.rs `classify`, called from rules/op.rs `binop`): the check unifies
//! the operands (not for a shift), states the result, records no callee and asks the trait
//! solver nothing. A sampled profile of `blaze run` on hello world (2026-11-16 16:36) had
//! `Machine::binop` -> `binop_lookup` at 24% of the main thread, all of it
//! `InferCtx::evaluate`, after the fast path for two KNOWN scalars had landed.
//!
//! The rule, from the Reference:
//! - "Operator expressions", [expr.arith-logic.behavior]: "This table summarizes the behavior of
//!   arithmetic and logical binary operators on primitive types and which traits are used to
//!   overload these operators for other types." (`+` .. `%` on Integer and Floating Point; `&`,
//!   `|`, `^` on Integer and bool; `<<`, `>>` on Integer.)
//! - [expr.cmp]: "Comparison operators are also defined both for primitive types and many types
//!   in the standard library."
//! - [expr.compound-assign]: "As with normal assignment expressions, compound assignment
//!   expressions always produce the unit value."
//! - "Literal expressions", integer literals: "If the token has no suffix, the expression's type
//!   is determined by type inference:" .. "If the program context under-constrains the type, it
//!   defaults to the signed 32-bit integer i32."; floating-point literals: "If the program
//!   context under-constrains the type, it defaults to f64."
//! - Shifts, from the pinned library (core/src/ops/bit.rs `shl_impl_all!`: `impl Shl<$f> for $t
//!   { type Output = $t; .. }` for every pair of integer types): the right operand's type is
//!   its own, the value has the left's type.
//!
//! The world:
//! ```text
//! #[lang = "sized"] trait Sized {}
//! #[lang = "add"] trait Add<Rhs> { type Output; fn add(self, rhs: Rhs) -> Self::Output; }
//! impl Add<i32> for i32 { type Output = i32; }   impl Add<&i32> for i32 { type Output = i32; }
//! impl Add<u8> for u8 { type Output = u8; }      impl Add<f32> for f32 { type Output = f32; }
//! #[lang = "add_assign"] trait AddAssign<Rhs> { fn add_assign(&mut self, rhs: Rhs); }
//! impl AddAssign<u8> for u8 {}
//! #[lang = "shl"] trait Shl<Rhs> { type Output; fn shl(self, rhs: Rhs) -> Self::Output; }
//! impl Shl<u8> for i32 { type Output = i32; }
//! #[lang = "bitand"] trait BitAnd<Rhs> { type Output; fn bitand(self, rhs: Rhs) -> Self::Output; }
//! impl BitAnd<i32> for i32 { type Output = i32; }  impl BitAnd<bool> for bool { type Output = bool; }
//! #[lang = "eq"] trait PartialEq<Rhs> { fn eq(&self, other: &Rhs) -> bool; }
//! impl PartialEq<u8> for u8 {}
//! fn f(a: u8, x: f32, i: i32, r: &i32) { <body> }
//! ```

use std::vec::Vec;

use blaze_own::Alloc;

use crate::body::{BinOp, Expr, IntSuffix};
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, FloatTy, IntTy, LangItem, Mutbl, PatId, StmtId, UintTy};
use crate::infer::InferCtx;
use crate::portable::{PNode, PTy};
use crate::world::Needs;
use crate::{Clock, Outcome, Tally, check_counted};

/// A clock that counts its reads (the tally's times are not what these tests read).
struct Ticks(core::cell::Cell<u64>);

impl Clock for Ticks {
    fn now_ns(&self) -> u64 {
        let t = self.0.get();
        self.0.set(t + 1);
        t
    }
}

/// What one check of `f` gave: the solver calls the tally counted, the callee rows, the
/// diagnostics' codes and the final type of each probed expression.
struct Run {
    calls: u64,
    rows: usize,
    codes: Vec<DiagCode>,
    probes: Vec<PNode>,
}

/// The parameters of `f`: `a: u8`, `x: f32`, `i: i32`, `r: &i32`.
#[derive(Clone, Copy)]
struct Params {
    a: PatId,
    x: PatId,
    i: PatId,
    r: PatId,
}

/// Checks `fn f(a: u8, x: f32, i: i32, r: &i32) { <stmts> }` in the module doc's world; `body`
/// returns the statements and the expressions whose types are read back.
fn run(body: impl Fn(&mut Bb, Params) -> (Vec<StmtId>, Vec<ExprId>)) -> Run {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let i32_: PTy = fx.ty(PNode::Int(IntTy::I32));
    let u8_ = fx.u8();
    let f32_ = fx.ty(PNode::Float(FloatTy::F32));
    let bool_ = fx.ty(PNode::Bool);
    let unit = fx.unit();
    let r_i32 = fx.r(Mutbl::Not, i32_);
    let (rp0, rp1) = (fx.r(Mutbl::Not, p0), fx.r(Mutbl::Not, p1));
    let mp0 = fx.r(Mutbl::Mut, p0);

    let add = fx.tr("Add", 2, Vec::new(), &["Output"]);
    fx.lang(LangItem::Add, add);
    let add_out = fx.alias(add, "Output", &[p0, p1]);
    fx.trait_fn(add, "add", true, &[p0, p1], add_out);
    fx.imp("add i32", 0, i32_, Some((add, &[i32_])), &[("Output", i32_)], Vec::new());
    fx.imp("add &i32 for i32", 0, i32_, Some((add, &[r_i32])), &[("Output", i32_)], Vec::new());
    fx.imp("add u8", 0, u8_, Some((add, &[u8_])), &[("Output", u8_)], Vec::new());
    fx.imp("add f32", 0, f32_, Some((add, &[f32_])), &[("Output", f32_)], Vec::new());

    let add_assign = fx.tr("AddAssign", 2, Vec::new(), &[]);
    fx.lang(LangItem::AddAssign, add_assign);
    fx.trait_fn(add_assign, "add_assign", true, &[mp0, p1], unit);
    fx.imp("add_assign u8", 0, u8_, Some((add_assign, &[u8_])), &[], Vec::new());

    let shl = fx.tr("Shl", 2, Vec::new(), &["Output"]);
    fx.lang(LangItem::Shl, shl);
    let shl_out = fx.alias(shl, "Output", &[p0, p1]);
    fx.trait_fn(shl, "shl", true, &[p0, p1], shl_out);
    fx.imp("shl u8 for i32", 0, i32_, Some((shl, &[u8_])), &[("Output", i32_)], Vec::new());

    let bitand = fx.tr("BitAnd", 2, Vec::new(), &["Output"]);
    fx.lang(LangItem::BitAnd, bitand);
    let bitand_out = fx.alias(bitand, "Output", &[p0, p1]);
    fx.trait_fn(bitand, "bitand", true, &[p0, p1], bitand_out);
    fx.imp("bitand i32", 0, i32_, Some((bitand, &[i32_])), &[("Output", i32_)], Vec::new());
    fx.imp("bitand bool", 0, bool_, Some((bitand, &[bool_])), &[("Output", bool_)], Vec::new());

    let partial_eq = fx.tr("PartialEq", 2, Vec::new(), &[]);
    fx.lang(LangItem::PartialEq, partial_eq);
    fx.trait_fn(partial_eq, "eq", true, &[rp0, rp1], bool_);
    fx.imp("eq u8", 0, u8_, Some((partial_eq, &[u8_])), &[], Vec::new());

    let owner = fx.func("f", 0, &[u8_, f32_, i32_, r_i32], unit, Vec::new());
    fx.finish();

    let mut bb = Bb::new();
    let params = Params { a: bb.param("a"), x: bb.param("x"), i: bb.param("i"), r: bb.param("r") };
    let (stmts, probes) = body(&mut bb, params);
    let root = bb.block(&stmts, None);
    let mut cx = InferCtx::new_in(Alloc::global());
    let mut tally = Tally::default();
    let clock = Ticks(core::cell::Cell::new(0));
    let out = check_counted(&bb.view(owner, root), &fx, &mut cx, &Needs::new_in(Alloc::global()), &mut tally, &clock, Alloc::global());
    let c = match out {
        Ok(Outcome::Done(c)) => c,
        Ok(Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("{e:?}"),
    };
    let codes = c.diags.iter().map(|d| d.code).collect();
    let probes = probes.iter().map(|p| c.types.view().node(c.expr_ty[p.ix()]).expect("a probed type")).collect();
    Run { calls: tally.solve.calls, rows: c.callees.len(), codes, probes }
}

/// `let name = lhs op rhs;`: the statement and the operator expression.
fn let_op(bb: &mut Bb, name: &str, op: BinOp, lhs: ExprId, rhs: ExprId) -> (StmtId, ExprId) {
    let e = bb.e(Expr::Binary { op, lhs, rhs });
    let p = bb.bind(name);
    (bb.let_(p, None, e), e)
}

/// ```text
/// let b = a + 1;      // b: u8, the literal u8          (arithmetic, unified)
/// let s = 1 << 2u8;   // s: i32 (the left's default), the right stays u8   (shift, not unified)
/// let y = 1.0 + x;    // y: f32, the literal f32        (float arithmetic)
/// let c = a == 0;     // c: bool, the literal u8        (comparison, unified)
/// let d = 1 & 2;      // d: i32 (both default)          (bit operator on two variables)
/// let t = true & true;// t: bool                        (bit operator on bool)
/// a += 1;             // the literal u8                 (compound assignment)
/// ```
/// Every operator is built in, so the body asks the solver NOTHING beyond the same body with
/// the operators taken out (each `let` binding an operand of the operator's result type, the
/// other operands as statements), and leaves no callee row. With the solver still asked for an
/// integer- or float-variable operand (the code before this change) the test prints
/// "solver calls for operators on literal variables, beyond a body of none" with `left` above
/// zero.
#[test]
fn operators_on_literal_variables_are_builtin_and_ask_the_solver_nothing() {
    let with_ops = run(|bb, p| {
        let mut stmts = Vec::new();
        let mut probes = Vec::new();
        // let b = a + 1;
        let (a, one) = (bb.local(p.a), bb.int(None));
        let (s, e) = let_op(bb, "b", BinOp::Add, a, one);
        stmts.push(s);
        probes.extend([e, one]);
        // let s = 1 << 2u8;
        let (one, two) = (bb.int(None), bb.int(Some(IntSuffix::Uint(UintTy::U8))));
        let (s, e) = let_op(bb, "s", BinOp::Shl, one, two);
        stmts.push(s);
        probes.extend([e, two]);
        // let y = 1.0 + x;
        let (lit, x) = (bb.float(None), bb.local(p.x));
        let (s, e) = let_op(bb, "y", BinOp::Add, lit, x);
        stmts.push(s);
        probes.extend([e, lit]);
        // let c = a == 0;
        let (a, zero) = (bb.local(p.a), bb.int(None));
        let (s, e) = let_op(bb, "c", BinOp::Eq, a, zero);
        stmts.push(s);
        probes.extend([e, zero]);
        // let d = 1 & 2;
        let (one, two) = (bb.int(None), bb.int(None));
        let (s, e) = let_op(bb, "d", BinOp::BitAnd, one, two);
        stmts.push(s);
        probes.push(e);
        // let t = true & true;
        let (l, r) = (bb.bool(), bb.bool());
        let (s, e) = let_op(bb, "t", BinOp::BitAnd, l, r);
        stmts.push(s);
        probes.push(e);
        // a += 1;
        let (a, one) = (bb.local(p.a), bb.int(None));
        let e = bb.e(Expr::AssignOp { op: BinOp::Add, lhs: a, rhs: one });
        stmts.push(bb.semi(e));
        probes.push(one);
        (stmts, probes)
    });
    let none = run(|bb, p| {
        let mut stmts = Vec::new();
        let mut keep = |bb: &mut Bb, name: &str, init: ExprId, rest: &[ExprId]| {
            let pat = bb.bind(name);
            stmts.push(bb.let_(pat, None, init));
            for &r in rest {
                stmts.push(bb.semi(r));
            }
        };
        let (a, one) = (bb.local(p.a), bb.int(None));
        keep(bb, "b", a, &[one]);
        let (one, two) = (bb.int(None), bb.int(Some(IntSuffix::Uint(UintTy::U8))));
        keep(bb, "s", one, &[two]);
        let (x, lit) = (bb.local(p.x), bb.float(None));
        keep(bb, "y", x, &[lit]);
        let (t, a, zero) = (bb.bool(), bb.local(p.a), bb.int(None));
        keep(bb, "c", t, &[a, zero]);
        let (one, two) = (bb.int(None), bb.int(None));
        keep(bb, "d", one, &[two]);
        let (l, r) = (bb.bool(), bb.bool());
        keep(bb, "t", l, &[r]);
        let (a, one) = (bb.local(p.a), bb.int(None));
        stmts.push(bb.semi(a));
        stmts.push(bb.semi(one));
        (stmts, Vec::new())
    });
    assert!(with_ops.codes.is_empty(), "diagnostics: {:?}", with_ops.codes);
    assert!(none.codes.is_empty(), "the operator-free body's diagnostics: {:?}", none.codes);
    let (u8_, i32_, f32_) = (PNode::Uint(UintTy::U8), PNode::Int(IntTy::I32), PNode::Float(FloatTy::F32));
    let want = [u8_, u8_, i32_, u8_, f32_, f32_, PNode::Bool, u8_, i32_, PNode::Bool, u8_];
    assert_eq!(with_ops.probes.as_slice(), want.as_slice(), "the inferred types: a+1, 1, 1<<2u8, 2u8, 1.0+x, 1.0, a==0, 0, 1&2, true&true, the 1 of a+=1");
    assert_eq!(with_ops.calls - none.calls, 0, "solver calls for operators on literal variables, beyond a body of none");
    assert_eq!(with_ops.rows, 0, "a built-in operator left a callee row");
}

/// CONTROL: `let z = i + r;` with `r: &i32` is no built-in operator: the solver is asked
/// (`i32: Add<&i32>`, the module doc's impl), the operator is `Add::add` (one callee row) and
/// `z` is `i32`. If the counter did not count, this prints "the reference operand asked the
/// solver nothing: the counter does not count".
#[test]
fn a_reference_operand_still_asks_the_solver() {
    let with_op = run(|bb, p| {
        let (i, r) = (bb.local(p.i), bb.local(p.r));
        let (s, e) = let_op(bb, "z", BinOp::Add, i, r);
        (std::vec![s], std::vec![e])
    });
    let none = run(|bb, p| {
        let (i, r) = (bb.local(p.i), bb.local(p.r));
        let pat = bb.bind("z");
        (std::vec![bb.let_(pat, None, i), bb.semi(r)], Vec::new())
    });
    assert!(with_op.codes.is_empty(), "diagnostics: {:?}", with_op.codes);
    assert!(with_op.calls > none.calls, "the reference operand asked the solver nothing: the counter does not count");
    assert_eq!(with_op.rows, 1, "`i32 + &i32` is `Add::add`, one row");
    assert_eq!(with_op.probes.as_slice(), [PNode::Int(IntTy::I32)].as_slice(), "z: i32");
}

/// REFUSED: `let w = 1 + 1.0;` (an integer variable and a float variable) is no built-in pair
/// and goes to the solver with the SAME question as before the change (`{integer}:
/// Add<?R>`, asked before the right operand is known; rules/op.rs `binop` asks it with a fresh
/// variable when it defers): it may hold, the obligation `{integer}: Add<{float}>` is
/// registered and fails, and the finding is E0277 (`DiagCode::Unsatisfied`), never E0369
/// (`DiagCode::BinOp`). A deferral that asked with the resolved right operand prints "1 +
/// 1.0: the finding changed" with `[BinOp]`.
#[test]
fn an_integer_and_a_float_operand_are_refused_as_before() {
    let got = run(|bb, _| {
        let (one, lit) = (bb.int(None), bb.float(None));
        let (s, _) = let_op(bb, "w", BinOp::Add, one, lit);
        (std::vec![s], Vec::new())
    });
    assert!(got.codes.contains(&DiagCode::Unsatisfied) && !got.codes.contains(&DiagCode::BinOp), "1 + 1.0: the finding changed: {:?}", got.codes);
}
