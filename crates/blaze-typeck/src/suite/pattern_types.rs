//! Pattern types (typeck.md, "Pattern types"): `T is PAT`, the pinned library's
//! `pattern_type!` (core/src/pat.rs, a `#[rustc_builtin_macro(pattern_type)]`; RFC-less, tracked
//! by issue 123646 `pattern_type_range_trait` and the `pattern_types` feature). The rules, from
//! the pinned library's own use:
//! - core goes between `T` and `T is PAT` only through `transmute` (num/niche_types.rs `new`,
//!   `new_unchecked`, `as_inner`; ptr/non_null.rs `as_ptr`, `new_unchecked`): no coercion, field
//!   or method links them, so a pattern type where its base is expected is a type error, here
//!   `DiagCode::PatternNotBase` (E0308) naming both;
//! - two pattern types are one type when their bases are and their sets of values are: the node
//!   holds canonical ranges, so `u8 is 1..` and `u8 is 1..=255` are equal, `i8 is ..0 | 1..` is
//!   the one wrapping range `1 ..= 0xff` (its bits), and different sets are different types.
//!
//! Planted bug: `InferPlant::PatIsBase` (a pattern type unifies as its base), caught by
//! `control_a_pattern_type_is_its_base`.

use std::vec::Vec;

use blaze_own::Alloc;

use super::check_body;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ItemKey, UintTy};
use crate::infer::{InferCtx, InferPlant};
use crate::machine::Plant;
use crate::portable::{PNode, canonical_ranges};
use crate::ty::{Ty, TyView, TypeCtx};

/// `base is start ..= end, ...` in an inference context, its ranges given as written.
fn pat(cx: &mut InferCtx<'_>, base: TyView, ranges: &[(u128, u128)], width: u32) -> Ty {
    let base = cx.mk(base).expect("a scalar type interns");
    let (runs, n) = canonical_ranges(width, ranges).expect("ranges within the bound");
    let mut ends = Vec::new();
    for (start, end) in runs[..n].iter().copied() {
        ends.push(cx.mk(TyView::ConstValue { ty: base, bits: start }).expect("interns"));
        ends.push(cx.mk(TyView::ConstValue { ty: base, bits: end }).expect("interns"));
    }
    let list = cx.mk_list(&ends).expect("interns");
    cx.mk(TyView::Pat(base, list)).expect("a pattern type interns")
}

/// One set of values has one form: an or that touches merges, a run to the maximum and one from
/// zero become one wrapping range, and a range given wrapped is the same as its two halves.
#[test]
fn ranges_are_canonical() {
    // `i8 is ..0 | 1..` (core's NonZeroI8Inner) in bits: 0x80..=0xff and 0x01..=0x7f.
    let (r, n) = canonical_ranges(8, &[(0x80, 0xff), (1, 0x7f)]).expect("in bounds");
    assert_eq!(&r[..n], &[(1, 0xff)]);
    // `u8 is ..=5 | 250..` is the wrapping `250 ..= 5`, as is the range written wrapped.
    let (r, n) = canonical_ranges(8, &[(0, 5), (250, 255)]).expect("in bounds");
    assert_eq!(&r[..n], &[(250, 5)]);
    let (w, m) = canonical_ranges(8, &[(250, 5)]).expect("in bounds");
    assert_eq!(&w[..m], &r[..n]);
    // `i32 is ..-1 | 0..` (I32NotAllOnes): every value but -1.
    let (r, n) = canonical_ranges(32, &[(0x8000_0000, 0xffff_fffe), (0, 0x7fff_ffff)]).expect("in bounds");
    assert_eq!(&r[..n], &[(0, 0xffff_fffe)]);
    // Disjoint runs stay apart, sorted.
    let (r, n) = canonical_ranges(16, &[(40, 50), (1, 3)]).expect("in bounds");
    assert_eq!(&r[..n], &[(1, 3), (40, 50)]);
}

/// Equality is structural: same base and same canonical ranges are one type; other ranges, or
/// the base itself, are not.
#[test]
fn pattern_types_are_equal_by_base_and_ranges() {
    let mut cx = InferCtx::new_in(Alloc::global());
    let u32_ = TyView::Uint(UintTy::U32);
    let a = pat(&mut cx, u32_, &[(1, 0xffff_ffff)], 32);
    let b = pat(&mut cx, u32_, &[(1, 10), (11, 0xffff_ffff)], 32);
    let c = pat(&mut cx, u32_, &[(0, 999_999_999)], 32);
    let d = pat(&mut cx, TyView::Uint(UintTy::U64), &[(1, u128::from(u64::MAX))], 64);
    let base = cx.mk(u32_).expect("interns");
    assert_eq!(cx.unify(a, b), Ok(true), "`u32 is 1..` written two ways is one type");
    assert_eq!(cx.unify(a, c), Ok(false), "other ranges are another type");
    assert_eq!(cx.unify(a, d), Ok(false), "another base is another type");
    assert_eq!(cx.unify(a, base), Ok(false), "a pattern type is not its base");
}

fn world() -> (Fx, ItemKey) {
    let mut world = Fx::new();
    let fx = &mut world;
    let unit = fx.unit();
    let main = fx.func("main", 0, &[], unit, Vec::new());
    fx.finish();
    (world, main)
}

/// `fn main() { let a: (u32 is 1..) = 0u32; }`: the base where the pattern type is expected.
fn base_for_pattern(ip: InferPlant) -> crate::columns::Checked<'static> {
    let (fx, main) = world();
    let mut b = Bb::new();
    let a = b.bind("a");
    let u32_ = b.ty(PNode::Uint(UintTy::U32));
    let nonzero = b.t.pattern(u32_, u32_, 32, &[(1, 0xffff_ffff)]).expect("interns");
    let e0 = b.int(Some(crate::body::IntSuffix::Uint(UintTy::U32)));
    let s0 = b.let_(a, Some(nonzero), e0);
    let root = b.block(&[s0], None);
    check_body(&fx, &b, main, root, Plant::None, ip)
}

/// A pattern type where its base is given is E0308 as `PatternNotBase`.
#[test]
fn a_base_where_a_pattern_type_is_expected_is_the_typed_error() {
    let c = base_for_pattern(InferPlant::None);
    assert!(c.diags.iter().any(|d| d.code == DiagCode::PatternNotBase), "{:?}", c.diags);
}

/// The planted bug lets `u32` stand for `u32 is 1..`: the wrong program checks clean.
#[test]
fn control_a_pattern_type_is_its_base() {
    let c = base_for_pattern(InferPlant::PatIsBase);
    assert!(!c.diags.iter().any(|d| d.code == DiagCode::PatternNotBase), "the plant was not reached: {:?}", c.diags);
}
