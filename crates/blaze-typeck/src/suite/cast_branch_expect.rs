//! Cast expectations (`Expectation::Castable(to)`) do not flow into the arms of `if`, `match`,
//! or unary `-`/`!`: `let d = if x > 8u8 { 8 } else { x } as u64;` checks the `if`'s branches
//! with `Expectation::None` (`adjust_for_branches`), so `{ 8 }` infers `u8` from `else { x }`
//! instead of `u64` from the outer cast (`blaze-lint/src/lib.rs:1207, 1224, 1233`).

use std::vec::Vec;

use super::check_body;
use crate::body::{BinOp, Expr, IntSuffix, UnOp};
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx, show};
use crate::ids::UintTy;
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

/// `fn f(x: u8) -> u64 { let d = if x > 8u8 { 8 } else { x } as u64; d }`:
/// the cast target `u64` does not leak into `{ 8 }`, so both arms unify at `u8` and cast to `u64`.
#[test]
fn if_branch_under_cast_does_not_take_cast_target() {
    let mut fx = Fx::new();
    let u8_ = fx.u8();
    let u64_ = fx.ty(PNode::Uint(UintTy::U64));
    let owner = fx.func("f", 0, &[u8_], u64_, Vec::new());
    fx.finish();

    let mut b = Bb::new();
    let x = b.param("x");
    let x_lhs = b.local(x);
    let eight_u8 = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let cond = b.e(Expr::Binary { op: BinOp::Gt, lhs: x_lhs, rhs: eight_u8 });
    let eight = b.int(None);
    let then_blk = b.block(&[], Some(eight));
    let x_else = b.local(x);
    let else_blk = b.block(&[], Some(x_else));
    let if_e = b.e(Expr::If { cond, then: then_blk, else_: Some(else_blk) });
    let u64_ty = b.ty(PNode::Uint(UintTy::U64));
    let cast_e = b.e(Expr::Cast { operand: if_e, ty: u64_ty });
    let d = b.bind("d");
    let stmt = b.let_(d, None, cast_e);
    let d_use = b.local(d);
    let root = b.block(&[stmt], Some(d_use));

    let c = check_body(&fx, &b, owner, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    assert_eq!(show(&fx, c.types.view(), c.expr_ty[if_e.ix()]), "u8");
    assert_eq!(show(&fx, c.types.view(), c.expr_ty[cast_e.ix()]), "u64");
}

/// `fn g(flag: bool, x: u8) -> u64 { match flag { _ => 8, _ => x } as u64 }` and
/// `fn neg_cast() -> u64 { (-1) as u64 }`: neither `match` arms nor unary `-` take `Castable(u64)`.
#[test]
fn match_and_unary_under_cast_do_not_take_cast_target() {
    let mut fx = Fx::new();
    let bool_ = fx.ty(PNode::Bool);
    let u8_ = fx.u8();
    let u64_ = fx.ty(PNode::Uint(UintTy::U64));
    let g = fx.func("g", 0, &[bool_, u8_], u64_, Vec::new());
    let neg_cast = fx.func("neg_cast", 0, &[], u64_, Vec::new());
    fx.finish();

    let mut b = Bb::new();
    let flag = b.param("flag");
    let x = b.param("x");
    let scrut = b.local(flag);
    let w0 = b.wild();
    let w1 = b.wild();
    let eight = b.int(None);
    let x_use = b.local(x);
    let match_e = b.match_(scrut, &[(w0, eight), (w1, x_use)]);
    let u64_ty = b.ty(PNode::Uint(UintTy::U64));
    let cast_e = b.e(Expr::Cast { operand: match_e, ty: u64_ty });
    let root = b.block(&[], Some(cast_e));
    let c = check_body(&fx, &b, g, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "match under cast: {:?}", c.diags);
    assert_eq!(show(&fx, c.types.view(), c.expr_ty[match_e.ix()]), "u8");

    let mut b2 = Bb::new();
    let one = b2.int(None);
    let neg = b2.e(Expr::Unary { op: UnOp::Neg, operand: one });
    let u64_ty2 = b2.ty(PNode::Uint(UintTy::U64));
    let cast2 = b2.e(Expr::Cast { operand: neg, ty: u64_ty2 });
    let root2 = b2.block(&[], Some(cast2));
    let c2 = check_body(&fx, &b2, neg_cast, root2, Plant::None, InferPlant::None);
    assert!(c2.diags.is_empty(), "(-1) as u64: {:?}", c2.diags);
    assert_eq!(show(&fx, c2.types.view(), c2.expr_ty[neg.ix()]), "i32");
    assert_eq!(show(&fx, c2.types.view(), c2.expr_ty[cast2.ix()]), "u64");
}

/// Control: genuine incompatible arms without a cast (`if x > 8u8 { 8 } else { x }` returning `u64`
/// with no cast, and `if x > 8u8 { 8u32 } else { x } as u64`) still fail with `IncompatibleArms`.
#[test]
fn control_incompatible_arms_still_fail() {
    let mut fx = Fx::new();
    let u8_ = fx.u8();
    let u64_ = fx.ty(PNode::Uint(UintTy::U64));
    let no_cast = fx.func("no_cast", 0, &[u8_], u64_, Vec::new());
    let bad_cast = fx.func("bad_cast", 0, &[u8_], u64_, Vec::new());
    fx.finish();

    // Without cast, the return type `u64` is `Expectation::HasType(u64)`, so `{ 8 }` is `u64`
    // and `else { x }` (`u8`) is E0308 `IncompatibleArms`.
    let mut b = Bb::new();
    let x = b.param("x");
    let x_lhs = b.local(x);
    let eight_u8 = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let cond = b.e(Expr::Binary { op: BinOp::Gt, lhs: x_lhs, rhs: eight_u8 });
    let eight = b.int(None);
    let then_blk = b.block(&[], Some(eight));
    let x_else = b.local(x);
    let else_blk = b.block(&[], Some(x_else));
    let if_e = b.e(Expr::If { cond, then: then_blk, else_: Some(else_blk) });
    let root = b.block(&[], Some(if_e));
    let c = check_body(&fx, &b, no_cast, root, Plant::None, InferPlant::None);
    assert!(
        c.diags.iter().any(|d| matches!(d.code, DiagCode::Mismatch | DiagCode::IncompatibleArms)),
        "expected Mismatch or IncompatibleArms without cast, got: {:?}",
        c.diags
    );

    // Even with a cast, suffixed `{ 8u32 }` vs `{ x: u8 }` is still E0308 `IncompatibleArms`.
    let mut b2 = Bb::new();
    let x2 = b2.param("x");
    let x2_lhs = b2.local(x2);
    let eight_u8_2 = b2.int(Some(IntSuffix::Uint(UintTy::U8)));
    let cond2 = b2.e(Expr::Binary { op: BinOp::Gt, lhs: x2_lhs, rhs: eight_u8_2 });
    let eight_u32 = b2.int(Some(IntSuffix::Uint(UintTy::U32)));
    let then_blk2 = b2.block(&[], Some(eight_u32));
    let x2_else = b2.local(x2);
    let else_blk2 = b2.block(&[], Some(x2_else));
    let if_e2 = b2.e(Expr::If { cond: cond2, then: then_blk2, else_: Some(else_blk2) });
    let u64_ty2 = b2.ty(PNode::Uint(UintTy::U64));
    let cast_e2 = b2.e(Expr::Cast { operand: if_e2, ty: u64_ty2 });
    let root2 = b2.block(&[], Some(cast_e2));
    let c2 = check_body(&fx, &b2, bad_cast, root2, Plant::None, InferPlant::None);
    assert!(
        c2.diags.iter().any(|d| d.code == DiagCode::IncompatibleArms),
        "expected IncompatibleArms for u32 vs u8 under cast, got: {:?}",
        c2.diags
    );
}
