//! x0-atomic-01: the atomic intrinsics with their orderings as CONST GENERIC ARGUMENTS of an
//! enum type (no_core; success 42, else the failed check's number). The declarations are the
//! pinned library's (library/core/src/intrinsics/mod.rs):
//!
//!   pub const unsafe fn atomic_load<T: Copy, const ORD: AtomicOrdering, const VOLATILE: bool>(
//!   pub const unsafe fn atomic_store<T: Copy, const ORD: AtomicOrdering, const VOLATILE: bool>(
//!   pub const unsafe fn atomic_xadd<T: Copy, U: Copy, const ORD: AtomicOrdering>(
//!   pub const unsafe fn atomic_cxchg<T: Copy, const ORD_SUCC: AtomicOrdering, const ORD_FAIL: AtomicOrdering>(
//!
//! "Loads the current value of the pointer." "Stores the value at the specified memory
//! location." `atomic_xadd`: "Adds to the current value, returning the previous value."
//! `atomic_cxchg`: "Stores a value if the current value is the same as the `old` value, with
//! the returned `T` being the previous value and the returned `bool` being whether the exchange
//! was successful." And `AtomicOrdering`: "These values must match the compiler's
//! `AtomicOrdering`": Relaxed = 0, Release = 1, Acquire = 2, AcqRel = 3, SeqCst = 4.
//!
//! This program declares `AtomicOrdering` with core's DISCRIMINANTS but its variants in the
//! REVERSE order, so a variant's index is not its discriminant: the ordering is read by its
//! value. A builder that passes the variant INDEX reads `Release` (index 3) as `AcqRel`, and
//! refuses check 1's store ("an atomic store with an acquire ordering"): the rung fails.
//!
//! 1. store 40 (Release), then `atomic_xadd` 2 (Relaxed) returns the old 40.
//! 2. a load (Acquire) gives 42; a load (SeqCst, VOLATILE) gives 42.
//! 3. `atomic_cxchg` (AcqRel / Acquire) from 42 to 7 returns (42, true); a load (Relaxed) gives 7.
//! 4. `atomic_cxchg` (SeqCst / Relaxed) expecting 42 when 7 is held returns (7, false), stores
//!    nothing.
//! The run prints nothing: `front2/x0-atomic-01-orderings: PASS (status 42, stdout byte-equal)`.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures, adt_const_params)]
#![no_core]
#![no_main]
#![allow(internal_features, incomplete_features)]

mod prelude;
use prelude::*;

/// core::marker::StructuralPartialEq (marker.rs).
#[lang = "structural_peq"]
pub trait StructuralPartialEq {}

/// core::cmp::Eq: `PartialEq` with reflexivity (cmp.rs).
pub trait Eq: PartialEq {}

/// core::marker::ConstParamTy_ (marker.rs): "`{Self}` can't be used as a const parameter type"
/// unless implemented; core derives it with `#[derive(ConstParamTy)]`.
#[lang = "const_param_ty"]
pub trait ConstParamTy_: StructuralPartialEq + Eq {}

/// core::intrinsics::AtomicOrdering's discriminants, its variants in reverse order (above).
pub enum AtomicOrdering {
    SeqCst = 4,
    AcqRel = 3,
    Acquire = 2,
    Release = 1,
    Relaxed = 0,
}

impl Clone for AtomicOrdering {
    fn clone(&self) -> Self {
        *self
    }
}
impl Copy for AtomicOrdering {}
impl PartialEq for AtomicOrdering {
    fn eq(&self, other: &AtomicOrdering) -> bool {
        *self as u8 == *other as u8
    }
}
impl StructuralPartialEq for AtomicOrdering {}
impl Eq for AtomicOrdering {}
impl ConstParamTy_ for AtomicOrdering {}

#[rustc_intrinsic]
#[rustc_nounwind]
pub const unsafe fn atomic_load<T: Copy, const ORD: AtomicOrdering, const VOLATILE: bool>(src: *const T) -> T;

#[rustc_intrinsic]
#[rustc_nounwind]
pub const unsafe fn atomic_store<T: Copy, const ORD: AtomicOrdering, const VOLATILE: bool>(dst: *mut T, val: T);

#[rustc_intrinsic]
#[rustc_nounwind]
pub const unsafe fn atomic_xadd<T: Copy, U: Copy, const ORD: AtomicOrdering>(dst: *mut T, src: U) -> T;

#[rustc_intrinsic]
#[rustc_nounwind]
pub const unsafe fn atomic_cxchg<T: Copy, const ORD_SUCC: AtomicOrdering, const ORD_FAIL: AtomicOrdering>(
    dst: *mut T,
    old: T,
    src: T,
) -> (T, bool);

fn check(p: *mut u32) -> i32 {
    unsafe {
        atomic_store::<u32, { AtomicOrdering::Release }, false>(p, 40);
        if atomic_xadd::<u32, u32, { AtomicOrdering::Relaxed }>(p, 2) != 40 {
            return 1;
        }
        if atomic_load::<u32, { AtomicOrdering::Acquire }, false>(p) != 42 {
            return 2;
        }
        if atomic_load::<u32, { AtomicOrdering::SeqCst }, true>(p) != 42 {
            return 2;
        }
        let (old, done) = atomic_cxchg::<u32, { AtomicOrdering::AcqRel }, { AtomicOrdering::Acquire }>(p, 42, 7);
        if old != 42 || !done || atomic_load::<u32, { AtomicOrdering::Relaxed }, false>(p) != 7 {
            return 3;
        }
        let (old, done) = atomic_cxchg::<u32, { AtomicOrdering::SeqCst }, { AtomicOrdering::Relaxed }>(p, 42, 9);
        if old != 7 || done || atomic_load::<u32, { AtomicOrdering::Relaxed }, false>(p) != 7 {
            return 4;
        }
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut cell: u32 = 0;
    let failed = check(&mut cell);
    if failed == 0 { 42 } else { failed }
}
