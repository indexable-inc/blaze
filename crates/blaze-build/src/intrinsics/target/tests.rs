//! The target intrinsics' set and their build (THREADS.md D1). The names are the pinned
//! library's: stdarch `arm_shared/barrier/mod.rs` `link_name = "llvm.aarch64.dmb"`,
//! `"llvm.aarch64.dsb"`, `"llvm.aarch64.isb"`, and `arm_shared/hints.rs`
//! `link_name = "llvm.aarch64.hint"`, each `fn(_: i32)`.
//!
//! `an_unknown_llvm_name_is_refused_by_name` prints "refused: `llvm.aarch64.neon.tbl1` is not a
//! target intrinsic blaze knows"; `a_barrier_builds_and_the_evaluator_gives_it_no_effect`
//! prints "isb #15 built, evaluated: Returned([Int(7)])".

use std::alloc::Global;

use blaze_lir::builder::{Param, Variadic};
use blaze_lir::eval::{Outcome, Val, run_in};
use blaze_lir::func::{BlockKind, Conv};
use blaze_lir::{Builder, Full, Op, Ty};

use super::{ALL, Target, build, named};
use crate::BuildError;

/// "`<name>` is not a target intrinsic blaze knows" (D1): an `llvm.*` name outside the set is
/// refused, never taken as a C function. Without `named`'s exact match this name would
/// either be accepted or refused with another text.
#[test]
fn an_unknown_llvm_name_is_refused_by_name() {
    let error = named("llvm.aarch64.neon.tbl1").expect_err("not a target intrinsic blaze knows");
    let text = error.to_string();
    assert_eq!(text, "`llvm.aarch64.neon.tbl1` is not a target intrinsic blaze knows");
    println!("refused: {text}");
    // A name longer than what the error keeps is cut, and says so.
    let long = "llvm.aarch64.".to_string() + &"x".repeat(100);
    let cut = named(&long).expect_err("not a target intrinsic").to_string();
    assert!(cut.ends_with("...` is not a target intrinsic blaze knows"), "{cut}");
}

/// Every member is found by its own link name and no other.
#[test]
fn every_target_is_named_by_its_link_name() {
    for t in ALL {
        assert_eq!(named(t.link_name()), Ok(t));
    }
    // The match is exact: a prefix or a different case is not the name.
    assert!(named("llvm.aarch64.is").is_err());
    assert!(named("LLVM.aarch64.isb").is_err());
}

/// `f() = { isb #15; 7 }`: the function holds one `target_intrinsic` naming
/// `llvm.aarch64.isb` with the constant 15, and blaze-lir's reference evaluator runs it with no
/// effect (a barrier has no meaning in its single-threaded machine), returning 7. Without the
/// evaluator's arm the run is `EvalError::Unsupported` and the test fails.
#[test]
fn a_barrier_builds_and_the_evaluator_gives_it_no_effect() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let imm = b.iconst(Ty::I32, 15);
    build(&mut b, Target::Aarch64Isb, imm).expect("a constant immediate builds");
    // Declared once per function: a second call names the same row.
    build(&mut b, Target::Aarch64Isb, imm).expect("a constant immediate builds");
    let seven = b.iconst(Ty::I64, 7);
    b.ret(&[seven]);
    let func = b.finish_checked().expect("a hand-built function builds");
    let view = func.view();
    assert_eq!(view.intrinsics.len(), 1, "one declaration of `llvm.aarch64.isb`");
    let calls: Vec<usize> = (0..view.ops.len()).filter(|&i| view.op(i) == Op::TargetIntrinsic).collect();
    assert_eq!(calls.len(), 2);
    let mut o = view.operands(calls[0]);
    assert_eq!(view.intrinsic_name(o.intrinsic()), b"llvm.aarch64.isb");
    let args = o.values();
    assert_eq!(args.len(), 1);
    assert_eq!(view.op(args.get(0).index()), Op::Iconst);
    let outcome = run_in(view, &[], 100, Global).expect("the evaluator models a target intrinsic");
    assert!(matches!(&outcome, Outcome::Returned(values) if values[..] == [Val::Int(7)]), "{outcome:?}");
    println!("isb #15 built, evaluated: {outcome:?}");
}

/// An immediate that is not a constant is refused by name at build time: every backend
/// encodes it in the instruction.
#[test]
fn an_immediate_that_is_not_a_constant_is_refused() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32)], &[]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let p = b.param(Ty::I32);
    let error = build(&mut b, Target::Aarch64Dmb, p).expect_err("a parameter is not a constant");
    assert!(
        matches!(error, BuildError::Target { target: Target::Aarch64Dmb, expr: None, why: "an immediate that is not a constant" }),
        "{error}"
    );
    let wide = b.iconst(Ty::I64, 15);
    let error = build(&mut b, Target::Aarch64Dmb, wide).expect_err("an `i64` is not the signature's `i32`");
    assert!(matches!(error, BuildError::Target { why: "an immediate that is not an `i32`", .. }), "{error}");
}
