//! The evaluator's `target_intrinsic` (THREADS.md D1): the four aarch64 barriers and hints run
//! as no effect; any other name is refused by name.
//!
//! `every_barrier_and_hint_runs_as_no_effect` prints "4 target intrinsics ran as no effect";
//! `another_target_intrinsic_is_refused` prints "refused: instruction 1 (`target_intrinsic`):
//! the evaluator does not model a target intrinsic other than ...".

use std::alloc::Global;

use crate::builder::{Param, Variadic};
use crate::eval::{EvalError, Outcome, Val, run_in};
use crate::func::{BlockKind, Conv, Func};
use crate::{Builder, Full, Op, Ty};

use super::NO_EFFECT;

/// `f() = { <name>(imm); 7 }`.
fn calling(name: &str, imm: u128) -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let void = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32)], &[]);
    let id = b.intrinsic(name, void);
    let imm = b.iconst(Ty::I32, imm);
    b.target_intrinsic(id, &[imm]);
    let seven = b.iconst(Ty::I64, 7);
    b.ret(&[seven]);
    b.finish_checked().expect("a hand-built function builds")
}

/// Without the evaluator's arm every one of these is `EvalError::Unsupported` ("this
/// operation") and the test fails.
#[test]
fn every_barrier_and_hint_runs_as_no_effect() {
    for (name, imm) in NO_EFFECT.iter().zip([15u128, 11, 15, 1]) {
        let name = core::str::from_utf8(name).expect("ASCII names");
        let func = calling(name, imm);
        let outcome = run_in(func.view(), &[], 100, Global).expect("a barrier or hint is modeled");
        assert!(matches!(&outcome, Outcome::Returned(values) if values[..] == [Val::Int(7)]), "{name}: {outcome:?}");
    }
    println!("{} target intrinsics ran as no effect", NO_EFFECT.len());
}

/// Planted control: a name outside the set (an x86 hint, a NEON table lookup) is refused,
/// never run as no effect. If the arm accepted every name this test would fail.
#[test]
fn another_target_intrinsic_is_refused() {
    for name in ["llvm.x86.sse2.pause", "llvm.aarch64.neon.tbl1"] {
        let func = calling(name, 0);
        let error = run_in(func.view(), &[], 100, Global).expect_err("not a modeled target intrinsic");
        assert!(matches!(error, EvalError::Unsupported { op: Op::TargetIntrinsic, .. }), "{name}: {error}");
        println!("refused: {error}");
    }
}
