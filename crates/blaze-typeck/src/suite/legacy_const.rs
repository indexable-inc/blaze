//! `#[rustc_legacy_const_generics]` (rules/legacy_const.rs): a const generic passed as a value
//! argument at the position the attribute lists.
//!
//! ```text
//! #[rustc_legacy_const_generics(1)]
//! fn vget<const IMM: i32>(v: u64) -> u64;
//! fn g() { vget(5u64, 3); }        // the call `vget::<3>(5u64)`: clean
//! fn h() { vget::<_>(5u64, 3); }   // generic arguments written: checked as written, E0061
//! ```
//!
//! The rule is rustc's (rustc_ast_lowering `lower_legacy_const_generics`, applied only to a
//! plain path callee whose last segment writes no generic arguments). The case that made this:
//! hashbrown's `Group::match_full`, `neon::vget_lane_u64(neon::vreinterpret_u64_u8(cmp), 0)`,
//! E0061 "1 wanted, 2 written" in tempo's build census of tree 4dd8a4485e. No test here runs a
//! compiler for its answer: each expectation is the rule above.

use std::vec::Vec;

use super::check_body;
use crate::body::IntSuffix;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx, show};
use crate::ids::{IntTy, ItemKey, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

/// `vget` above and the owner `fn g()`.
fn world() -> (Fx, ItemKey) {
    let mut fx = Fx::new();
    let unit = fx.unit();
    let u64_ = fx.ty(PNode::Uint(UintTy::U64));
    let i32_ = fx.ty(PNode::Int(IntTy::I32));
    let vget = fx.func("vget", 1, &[u64_], u64_, Vec::new());
    fx.const_param(vget, 0, i32_);
    fx.legacy_const_generics(vget, &[1]);
    let g = fx.func("g", 0, &[], unit, Vec::new());
    fx.finish();
    (fx, g)
}

/// `vget(5u64, 3)`: the argument at position 1 is the const generic `IMM`, typed `i32`; the
/// call is clean and its callee is `fn vget<3>`. Without the rule it is E0061 (one input, two
/// arguments) and the callee's `IMM` is never fixed.
///
/// Prints: `test suite::legacy_const::a_listed_argument_is_the_const_generic ... ok`
#[test]
fn a_listed_argument_is_the_const_generic() {
    let (fx, g) = world();
    let mut b = Bb::new();
    let callee = b.path(&["vget"]);
    let v = b.int(Some(IntSuffix::Uint(UintTy::U64)));
    let three = b.int_valued(None, 3);
    let call = b.call(callee, &[v, three]);
    let s = b.semi(call);
    let root = b.block(&[s], None);
    let c = check_body(&fx, &b, g, root, Plant::None, InferPlant::None);
    let codes: Vec<DiagCode> = c.diags.iter().map(|d| d.code).collect();
    assert!(codes.is_empty(), "unexpected diagnostics {codes:?}");
    let types = c.types.view();
    // `show` writes a const value with its type (fixture.rs: `3i32`, only `usize` bare), so
    // this names both the value and that it is the `i32` of `IMM`.
    assert_eq!(show(&fx, types, c.expr_ty[callee.0 as usize]), "fn vget<3i32>");
    assert_eq!(show(&fx, types, c.expr_ty[three.0 as usize]), "i32");
}

/// `vget::<_>(5u64, 3)`: the callee writes its generic arguments, so the call is checked as
/// written (rustc applies the attribute only to a path with none): E0061.
///
/// Prints: `test suite::legacy_const::written_generic_arguments_turn_the_rule_off ... ok`
#[test]
fn written_generic_arguments_turn_the_rule_off() {
    let (fx, g) = world();
    let mut b = Bb::new();
    let hole = b.ty(PNode::Infer);
    let callee = b.path_args("vget", &[hole]);
    let v = b.int(Some(IntSuffix::Uint(UintTy::U64)));
    let three = b.int_valued(None, 3);
    let call = b.call(callee, &[v, three]);
    let s = b.semi(call);
    let root = b.block(&[s], None);
    let c = check_body(&fx, &b, g, root, Plant::None, InferPlant::None);
    let codes: Vec<DiagCode> = c.diags.iter().map(|d| d.code).collect();
    assert!(codes.contains(&DiagCode::ArgCount), "want E0061, got {codes:?}");
}
