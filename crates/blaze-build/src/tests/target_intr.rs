//! Target intrinsics through the walk (walk/target_intr.rs; INTRINSICS.md P7b): a call of a
//! declaration front2 tables as `Intrinsic::Target`, built, verified, emitted by the baseline
//! tier (tests.rs `build`'s `emits`) and run by blaze-lir's evaluator.
//!
//! The declarations are the pinned library's (stdarch `aarch64/neon/generated.rs`):
//! `vrndns_f32`'s `#[link_name = "llvm.roundeven.f32"] fn _vrndns_f32(a: f32) -> f32`,
//! `vrndn_f64`'s `"llvm.roundeven.v1f64"` and `vfmaq_f64`'s `"llvm.fma.v2f64"` take the same
//! path at a vector shape; `core::hint::spin_loop` reaches `llvm.aarch64.isb` (`fn(i32)`).
//! LLVM's LangRef: `llvm.roundeven` "rounds ... to the nearest integer ... rounding halfway
//! cases to even"; `llvm.fma` "fused multiply-add ... without intermediate rounding".
//!
//! `roundeven_and_fma_through_the_walk` prints
//! "roundeven(2.5) = 2, roundeven(3.5) = 4, fma(3, 3, 3) = 12; isb #15 built".

use super::*;
use crate::intrinsics::target::Target;
use crate::intrinsics::target::float::{FloatCall, FloatFn, Shape};

const F64: PNode = PNode::Float(blaze_typeck::FloatTy::F64);

/// `fn main(x: f64) -> f64 { g(x) }` (or `g(x, x, x)` for a three-operand `g`), `g` the
/// target intrinsic `t` at key 6: built, emitted and run at `arg`.
fn run_f64(t: Target, operands: usize, arg: f64) -> f64 {
    let mut w = TestWorld::new();
    w.func(1, "main", &[F64], F64);
    let inputs = [F64; 3];
    w.func(6, "g", &inputs[..operands], F64);
    w.intrinsics.push((ItemKey(6), Intrinsic::Target(t)));
    let mut b = BodyBuf::new();
    let x = b.bind("x");
    b.params.push(x);
    let mut args = Vec::new_in(Global);
    for _ in 0..operands {
        args.push(b.local(x));
    }
    let root = b.call("g", &args);
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    let vals = run(&built, &[Val::Int(u128::from(arg.to_bits()))]).unwrap_or_else(|e| panic!("{e}"));
    match vals.as_slice() {
        [Val::Int(bits)] => f64::from_bits(u64::try_from(*bits).expect("an f64's bits")),
        other => panic!("returned {other:?}"),
    }
}

/// roundeven(2.5) is 2 and roundeven(3.5) is 4 (halfway to even, not away from zero: a walk
/// that built `fround` gives 3 and fails); fma(3, 3, 3) is 12. And `isb #15` builds in a
/// body (`fn main() -> u32 { isb(15); 7 }`) as one `target_intrinsic` named
/// `llvm.aarch64.isb`. Without walk/target_intr.rs's float arm the first call is refused
/// ("a generic float function built as a barrier").
#[test]
fn roundeven_and_fma_through_the_walk() {
    let roundeven = Target::Float(FloatCall { func: FloatFn::Roundeven, shape: Shape::F64 });
    let fma = Target::Float(FloatCall { func: FloatFn::Fma, shape: Shape::F64 });
    let (a, b, c) = (run_f64(roundeven, 1, 2.5), run_f64(roundeven, 1, 3.5), run_f64(fma, 3, 3.0));
    assert_eq!((a, b, c), (2.0, 4.0, 12.0));

    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    w.func(6, "isb", &[I32], UNIT);
    w.intrinsics.push((ItemKey(6), Intrinsic::Target(Target::Aarch64Isb)));
    let mut body = BodyBuf::new();
    let fifteen = body.int(15, None);
    let call = body.call("isb", &[fifteen]);
    let seven = body.int(7, SU32);
    let root = body.block(&[Stmt::Expr { expr: call, semi: true }], Some(seven));
    let built = build(&w, &body, root).unwrap_or_else(|e| panic!("{e}"));
    let view = built.func.view();
    let calls: Vec<usize> = (0..view.ops.len()).filter(|&i| view.op(i) == blaze_lir::Op::TargetIntrinsic).collect();
    assert_eq!(calls.len(), 1, "{}", blaze_lir::print(view));
    assert_eq!(view.intrinsic_name(view.operands(calls[0]).intrinsic()), b"llvm.aarch64.isb");
    assert_eq!(one(&built, &[]), 7);
    println!("roundeven(2.5) = {a}, roundeven(3.5) = {b}, fma(3, 3, 3) = {c}; isb #15 built");
}
