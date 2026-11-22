//! A coercion's target is resolved with the pending obligations first (rustc_hir_typeck
//! `demand_coerce_diag` calls `resolve_vars_with_obligations` on the expected type before
//! `coerce`; `check_argument_types` does the same for each formal input). The tempo build
//! census's "can't compare" rows (2026-11-18): `self.vtable as *const Vtable ==
//! &PROMOTABLE_EVEN_VTABLE` (bytes), `current == thread_data` (parking_lot), `(**self)
//! .partial_cmp(other)` with `other: &ArrayVec<T, CAP>` (arrayvec), `Ptr::Target::lt(self,
//! other)` (core's `Pin`): in each the goal's one candidate (the only impl for `*const T`, for
//! `[T]`, the where-clause `Ptr::Target: PartialOrd<Q::Target>`) fixes the right side's
//! variable BEFORE the argument coerces to it; without the selection the variable took the
//! argument's own type and the goal failed (E0277).
//!
//! The same shape in miniature, through a call's where-clause:
//!
//! ```text
//! trait Tr<B> {}
//! impl Tr<*const u8> for *const u8 {}
//! fn need<A: Tr<B>, B>(a: A, b: B) -> u32;
//! fn need2<A: Tr<B>, B>(b: B, a: A) -> u32;
//! fn f(p: *const u8, r: &u8) -> u32 { need(p, r) }     // OK in rustc: `&u8` to `*const u8`
//! fn f2(p: *const u8, r: &u8) -> u32 { need2(r, p) }   // E0277 in rustc: `B` is fixed first
//! fn f3(p: *const u8, r: &u16) -> u32 { need(p, r) }   // E0308 in rustc: `&u16` is no `*const u8`
//! ```

use std::vec::Vec;

use super::check_body;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ItemKey, Mutbl};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

/// The world of the module doc: `[f, f2, f3]`.
fn world() -> (Fx, [ItemKey; 3]) {
    let mut fx = Fx::new();
    let tr = fx.tr("Tr", 2, Vec::new(), &[]);
    let u8_ = fx.u8();
    let u16_ = fx.u16();
    let u32_ = fx.u32();
    let ptr_u8 = fx.ty(PNode::Ptr(Mutbl::Not, u8_));
    fx.imp("impl Tr<*const u8> for *const u8", 0, ptr_u8, Some((tr, &[ptr_u8])), &[], Vec::new());
    let (p0, p1) = (fx.param(0), fx.param(1));
    let a_tr_b = fx.bound(tr, &[p0, p1]);
    fx.func("need", 2, &[p0, p1], u32_, std::vec![a_tr_b]);
    let a_tr_b = fx.bound(tr, &[p0, p1]);
    fx.func("need2", 2, &[p1, p0], u32_, std::vec![a_tr_b]);
    let r_u8 = fx.r(Mutbl::Not, u8_);
    let r_u16 = fx.r(Mutbl::Not, u16_);
    let f = fx.func("f", 0, &[ptr_u8, r_u8], u32_, Vec::new());
    let f2 = fx.func("f2", 0, &[ptr_u8, r_u8], u32_, Vec::new());
    let f3 = fx.func("f3", 0, &[ptr_u8, r_u16], u32_, Vec::new());
    fx.finish();
    (fx, [f, f2, f3])
}

/// `callee(p, r)` (or `callee(r, p)` when `swapped`) as `owner`'s body, checked: the codes.
/// Expression ids: the callee 0, the first argument 1, the second 2, the call 3, the block 4.
fn codes(owner_ix: usize, callee: &str, swapped: bool) -> Vec<DiagCode> {
    let (fx, owners) = world();
    let mut b = Bb::new();
    let p = b.param("p");
    let r = b.param("r");
    let c = b.path(&[callee]);
    let (first, second) = if swapped { (r, p) } else { (p, r) };
    let e1 = b.local(first);
    let e2 = b.local(second);
    let call = b.call(c, &[e1, e2]);
    let root = b.block(&[], Some(call));
    let checked = check_body(&fx, &b, owners[owner_ix], root, Plant::None, InferPlant::None);
    let got: Vec<DiagCode> = checked.diags.iter().map(|d| d.code).collect();
    std::println!("{callee} in {}: {got:?}", fx.name_of(owners[owner_ix]));
    got
}

/// `need(p, r)`: `*const u8: Tr<?B>` has one impl, so `?B = *const u8` before `r: &u8` is
/// coerced to it (`&u8` to `*const u8`, the Reference's "Coercion types": "`&T` or `&mut T`
/// to `*const T`"). Prints "need in f: []".
#[test]
fn a_targets_variable_fixed_by_its_one_candidate_takes_the_coercion() {
    assert_eq!(codes(0, "need", false), Vec::<DiagCode>::new());
}

/// The other order: `need2(r, p)` checks `r` first, when `?A: Tr<?B>` decides nothing (its
/// self type is a variable), so `?B = &u8`, and `*const u8: Tr<&u8>` fails: E0277, as in
/// rustc. Prints "need2 in f2: [Unsatisfied]". The selection is no license to reorder.
#[test]
fn the_other_order_leaves_the_variable_to_the_argument_and_fails() {
    assert_eq!(codes(1, "need2", true), std::vec![DiagCode::Unsatisfied]);
}

/// The negative beside the fix: `?B = *const u8` as above, and `&u16` does not coerce to
/// `*const u8`: E0308 at the argument, as in rustc (the fix did not just accept more).
/// Prints "need in f3: [Mismatch]".
#[test]
fn a_source_that_does_not_coerce_to_the_fixed_target_is_e0308() {
    assert_eq!(codes(2, "need", false), std::vec![DiagCode::Mismatch]);
}
