//! `ub_checks` in constants, and an intrinsic the table does not hold, each evaluated through
//! two roots that share one unit's compiled code (the fixture keeps `Codes` across
//! evaluations, as the driver's `Consts` does).
//!
//! The pinned core, library/core/src/intrinsics/mod.rs, on `pub const fn ub_checks() -> bool`:
//! "# Consteval: In consteval, this function currently returns `true`. This is because the
//! value of the `ub_checks` configuration can differ across crates, but we need this function
//! to always return the same value in consteval in order to avoid unsoundness."
//! core's `Layout::from_size_align_unchecked` (a `const unsafe fn`, library/core/src/alloc/
//! layout.rs) reaches it through `assert_unsafe_precondition!` (library/core/src/ub_checks.rs:
//! `pub use intrinsics::ub_checks as check_library_ub;`), so every constant that builds a
//! `Layout` (`SizedTypeProperties::LAYOUT`) calls it.

use blaze_typeck::PNode;

use super::{eval_int, refusal, world};
use crate::{Intrinsic, Refusal};

/// `ub_checks()` is `true` in a constant, the same in every root: `const fn f` takes the
/// `true` arm in both `C1` and `C2`, and `D` reads the intrinsic directly.
/// Prints `test tests::ub_checks::ub_checks_is_true_in_every_constant ... ok`.
#[test]
fn ub_checks_is_true_in_every_constant() {
    let mut w = world();
    let b = w.fx.ty(PNode::Bool);
    w.intrinsic("ub_checks", 0, &[], b, Intrinsic::UbChecks);
    w.const_fn("f", &[], "u8", "{ if ub_checks() { 1 } else { 2 } }");
    w.konst("C1", "u8", "f()");
    w.konst("C2", "u8", "f()");
    w.konst("D", "bool", "ub_checks()");
    w.build();
    assert_eq!(eval_int(&mut w, "C1"), Ok(1), "the first root compiles `f` and runs it");
    assert_eq!(eval_int(&mut w, "C2"), Ok(1), "the second root runs `f`'s kept code");
    assert_eq!(eval_int(&mut w, "D"), Ok(1), "`ub_checks()` itself is `true`");
}

/// `overflow_checks()` is `true` in a constant, whatever the build's overflow setting: the
/// pinned core's doc says "In consteval, this function currently returns `true`", as for
/// `ub_checks`. imbl's `ITER_STACK_CAPACITY = HASH_WIDTH.div_ceil(HASH_SHIFT) + 1` reaches it
/// through core's `div_ceil` (uint_macros.rs: `if intrinsics::overflow_checks() { .. }`); `f`
/// is that form, the checked arm taken. Without the table row and the evaluator's arm the
/// constants are refused as `UnknownIntrinsic`.
/// Prints `test tests::ub_checks::overflow_checks_is_true_in_every_constant ... ok`.
#[test]
fn overflow_checks_is_true_in_every_constant() {
    let mut w = world();
    let b = w.fx.ty(PNode::Bool);
    w.intrinsic("overflow_checks", 0, &[], b, Intrinsic::OverflowChecks);
    w.const_fn("f", &[], "usize", "{ if overflow_checks() { 7 } else { 9 } }");
    w.konst("CAP1", "usize", "f() + 1");
    w.konst("CAP2", "usize", "f() + 1");
    w.konst("O", "bool", "overflow_checks()");
    w.build();
    assert_eq!(eval_int(&mut w, "CAP1"), Ok(8), "the first root compiles `f` and runs it");
    assert_eq!(eval_int(&mut w, "CAP2"), Ok(8), "the second root runs `f`'s kept code");
    assert_eq!(eval_int(&mut w, "O"), Ok(1), "`overflow_checks()` itself is `true`");
}

/// A `#[rustc_intrinsic]` fn the table does not hold is refused as that, by item, in every
/// root: the refusal is kept in `g`'s compiled code and the second root, which reuses it,
/// names the same reason (never "not a `const fn`", and never the "kept refusal of a kind
/// compiling does not make" a missing `attach` arm gives).
/// Prints `test tests::ub_checks::an_unknown_intrinsic_is_named_in_every_root ... ok`.
#[test]
fn an_unknown_intrinsic_is_named_in_every_root() {
    let mut w = world();
    let b = w.fx.ty(PNode::Bool);
    w.unknown_intrinsic("an_intrinsic_not_in_the_table", 0, &[], b);
    w.const_fn("g", &[], "u8", "{ if an_intrinsic_not_in_the_table() { 1 } else { 2 } }");
    w.konst("E1", "u8", "g()");
    w.konst("E2", "u8", "g()");
    w.build();
    let item = w.key("an_intrinsic_not_in_the_table");
    let want = std::format!("{:?}", Refusal::UnknownIntrinsic { item });
    assert_eq!(refusal(&mut w, "E1"), want, "the first root");
    assert_eq!(refusal(&mut w, "E2"), want, "the second root, from the kept code");
}
