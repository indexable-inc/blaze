//! A const argument of an enum type (Reference, "Generic parameters", "Const generics": "A
//! const argument must be ... a block expression"; the pinned library goes past the integer,
//! `bool` and `char` types under `adt_const_params`: core/src/intrinsics/mod.rs `pub unsafe fn
//! atomic_xadd<T: Copy, U: Copy, const ORD: AtomicOrdering>(dst: *mut T, src: U) -> T;`,
//! called as `atomic_xadd::<T, U, { AtomicOrdering::SeqCst }>`). The value is its variant's
//! index under the enum's key (`PortableTys::variant_value`); it must be of the parameter's
//! declared type.
//!
//! ```text
//! enum Ord { Relaxed, SeqCst }   enum Other { A }
//! fn f<const ORD: Ord>();
//! fn main() { f::<{ Ord::SeqCst }>(); f::<{ Other::A }>(); }
//! ```

use std::vec::Vec;

use super::check_body;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey};
use crate::infer::InferPlant;
use crate::machine::Plant;

fn world() -> (Fx, ItemKey, ItemKey, ItemKey) {
    let mut fx = Fx::new();
    let (ord, _) = fx.enum_("Ord", 0, &[("Relaxed", &[]), ("SeqCst", &[])]);
    let (other, _) = fx.enum_("Other", 0, &[("A", &[])]);
    let unit = fx.unit();
    let ord_ty = fx.adt(ord, &[]);
    let f = fx.func("f", 1, &[], unit, Vec::new());
    fx.const_param(f, 0, ord_ty);
    let main = fx.func("main", 0, &[], unit, Vec::new());
    fx.finish();
    (fx, main, ord, other)
}

/// `f::<{ <adt>::<variant> }>()` as `main`'s body: the callee is expression 0.
fn call(adt: ItemKey, variant: u32) -> (Bb, ExprId) {
    let mut b = Bb::new();
    let v = b.variant_value(adt, variant);
    let callee = b.path_args("f", &[v]);
    let e = b.call(callee, &[]);
    let root = b.block(&[], Some(e));
    (b, root)
}

#[test]
fn an_enum_const_argument_of_the_parameters_type_checks() {
    let (fx, main, ord, _) = world();
    let (b, root) = call(ord, 1);
    let c = check_body(&fx, &b, main, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "{:?}", c.diags);
}

#[test]
fn an_enum_const_argument_of_another_enum_is_e0308() {
    let (fx, main, _, other) = world();
    let (b, root) = call(other, 0);
    let c = check_body(&fx, &b, main, root, Plant::None, InferPlant::None);
    let codes: Vec<DiagCode> = c.diags.iter().map(|d| d.code).collect();
    assert_eq!(codes, [DiagCode::Mismatch], "{:?}", c.diags);
}
