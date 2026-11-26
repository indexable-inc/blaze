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
use blaze_ty::pat_ty::ConstPatMember;

use super::check_body;
use crate::diag::DiagCode;
use crate::error::{CheckError, Limit, Malformed};
use crate::fixture::{Bb, Fx};
use crate::ids::{ItemKey, Mutbl, UintTy};
use crate::infer::{InferCtx, InferPlant};
use crate::machine::Plant;
use crate::portable::{
    MAX_PATTERN_SET_MEMBERS, PNode, PortableTys, canonical_const_set, canonical_ranges,
};
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

// --- Finite const value-set pattern types (`T is in CONST_SLICE` / `SemVer is in VERSIONS`) ---

/// Canonical 128-bit structural digest for `SemVer { major: u16, minor: u16, patch: u16 }`,
/// using the exact same `ConstPatMember::digest128` definition as `blaze-ty::pat_ty`.
fn semver_digest(major: u16, minor: u16, patch: u16) -> u128 {
    let mut buf = [0u8; 6];
    buf[0..2].copy_from_slice(&major.to_le_bytes());
    buf[2..4].copy_from_slice(&minor.to_le_bytes());
    buf[4..6].copy_from_slice(&patch.to_le_bytes());
    ConstPatMember::from_bytes(&buf, "SemVer").digest128()
}

/// Canonical sorting, deduplication, empty refusal, and `MAX_PATTERN_SET_MEMBERS` cap (rule 18).
#[test]
fn const_set_canonical_sorting_dedup_and_bounds() {
    let v1_1_1 = semver_digest(1, 1, 1);
    let v1_2_2 = semver_digest(1, 2, 2);
    let v3_4_5 = semver_digest(3, 4, 5);

    let (a, na) = canonical_const_set(&[v3_4_5, v1_1_1, v1_2_2, v1_1_1]).expect("valid set");
    let (b, nb) = canonical_const_set(&[v1_2_2, v3_4_5, v3_4_5, v1_1_1]).expect("valid set");
    assert_eq!(na, 3);
    assert_eq!(nb, 3);
    assert_eq!(&a[..na], &b[..nb]);
    assert!(a[0] < a[1] && a[1] < a[2], "digests must be strictly ascending");

    // Empty set is refused.
    assert_eq!(canonical_const_set(&[]).unwrap_err(), CheckError::Malformed(Malformed::EmptyPatternSet));

    // Exactly `MAX_PATTERN_SET_MEMBERS` succeeds; one more hits `Limit::PatternSetMembers`.
    let at_cap: Vec<u128> = (1..=MAX_PATTERN_SET_MEMBERS as u128).collect();
    let (capped, nc) = canonical_const_set(&at_cap).expect("at cap succeeds");
    assert_eq!(nc, MAX_PATTERN_SET_MEMBERS);
    assert_eq!(capped[0], 1);
    assert_eq!(capped[nc - 1], MAX_PATTERN_SET_MEMBERS as u128);

    let over_cap: Vec<u128> = (1..=(MAX_PATTERN_SET_MEMBERS + 1) as u128).collect();
    assert_eq!(
        canonical_const_set(&over_cap).unwrap_err(),
        CheckError::Limit(Limit::PatternSetMembers),
    );
}

/// Structural unification for finite const value-set pattern types: same base + canonical set
/// unifies regardless of input order/duplicates; different sets, different bases, the base type
/// itself, and range patterns over the same `u128` bits do NOT unify.
#[test]
fn const_set_pattern_types_unify_by_base_and_canonical_set() {
    let mut cx = InferCtx::new_in(Alloc::global());
    let empty = cx.mk_list(&[]).expect("interns");
    let semver = cx.mk(TyView::Adt(ItemKey(0x5345_4d56_4552_0001), empty)).expect("interns");
    let other_ver = cx.mk(TyView::Adt(ItemKey(0x5345_4d56_4552_0002), empty)).expect("interns");
    let str_ty = cx.mk(TyView::Str).expect("interns");
    let str_ref = cx.mk(TyView::Ref(Mutbl::Not, str_ty)).expect("interns");

    let v1_1_1 = semver_digest(1, 1, 1);
    let v1_2_2 = semver_digest(1, 2, 2);
    let v3_4_5 = semver_digest(3, 4, 5);

    let all_a = cx.mk_pattern_const_set(semver, &[v3_4_5, v1_1_1, v1_2_2, v1_1_1]).expect("interns");
    let all_b = cx.mk_pattern_const_set(semver, &[v1_2_2, v3_4_5, v3_4_5, v1_1_1]).expect("interns");
    let old = cx.mk_pattern_const_set(semver, &[v1_1_1, v1_2_2]).expect("interns");
    let other_all = cx.mk_pattern_const_set(other_ver, &[v1_1_1, v1_2_2, v3_4_5]).expect("interns");

    assert_eq!(cx.unify(all_a, all_b), Ok(true), "canonical order + dedup unify identically");
    assert_eq!(cx.unify(old, all_a), Ok(false), "different value sets do not unify");
    assert_eq!(cx.unify(all_a, other_all), Ok(false), "different base types do not unify");
    assert_eq!(cx.unify(all_a, semver), Ok(false), "const-set pattern type is not its base");

    // Non-scalar/non-ADT reference base (`&str is in ["1.1.1", "1.2.2"]`).
    let s1 = ConstPatMember::from_str("1.1.1").digest128();
    let s2 = ConstPatMember::from_str("1.2.2").digest128();
    let str_set_a = cx.mk_pattern_const_set(str_ref, &[s2, s1, s2]).expect("interns");
    let str_set_b = cx.mk_pattern_const_set(str_ref, &[s1, s2]).expect("interns");
    assert_eq!(cx.unify(str_set_a, str_set_b), Ok(true));
    assert_eq!(cx.unify(str_set_a, str_ref), Ok(false));

    // A range pattern `u128 is lo..=hi` and a 2-member const-set `u128 is in [lo, hi]` with the
    // exact same two `u128` values must never collide.
    let (lo, hi) = if v1_1_1 < v1_2_2 { (v1_1_1, v1_2_2) } else { (v1_2_2, v1_1_1) };
    let u128_ty = cx.mk(TyView::Uint(UintTy::U128)).expect("interns");
    let range_pat = pat(&mut cx, TyView::Uint(UintTy::U128), &[(lo, hi)], 128);
    let set_pat = cx.mk_pattern_const_set(u128_ty, &[lo, hi]).expect("interns");
    assert_eq!(cx.unify(range_pat, set_pat), Ok(false), "range pattern must not collide with const-set");
}

/// Subset checks (`(SemVer is in OLD) <: (SemVer is in ALL)`) work in both `PTys` and `InferCtx`.
#[test]
fn const_set_subset_checks_in_portable_and_infer() {
    let v1_1_1 = semver_digest(1, 1, 1);
    let v1_2_2 = semver_digest(1, 2, 2);
    let v3_4_5 = semver_digest(3, 4, 5);
    let v9_9_9 = semver_digest(9, 9, 9);

    // 1. Portable table (`PortableTys::pattern_const_set` + `PTys::const_set_is_subset`).
    let mut pt = PortableTys::new_in(Alloc::global());
    let empty_list = pt.intern_list(&[]).expect("interns");
    let semver_p = pt.intern(PNode::Adt(ItemKey(0x5345_4d56_4552_0001), empty_list)).expect("interns");
    let other_p = pt.intern(PNode::Adt(ItemKey(0x5345_4d56_4552_0002), empty_list)).expect("interns");
    let u32_p = pt.intern(PNode::Uint(UintTy::U32)).expect("interns");

    let old_p = pt.pattern_const_set(semver_p, &[v1_2_2, v1_1_1]).expect("interns");
    let all_p = pt.pattern_const_set(semver_p, &[v3_4_5, v1_1_1, v1_2_2, v1_1_1]).expect("interns");
    let all_p_dup = pt.pattern_const_set(semver_p, &[v1_1_1, v1_2_2, v3_4_5]).expect("interns");
    let outside_p = pt.pattern_const_set(semver_p, &[v1_1_1, v9_9_9]).expect("interns");
    let other_old_p = pt.pattern_const_set(other_p, &[v1_1_1, v1_2_2]).expect("interns");
    let range_p = pt.pattern(u32_p, u32_p, 32, &[(1, 10)]).expect("interns");

    assert_eq!(all_p, all_p_dup, "hash-consed portable table interns equal sets to one PTy");
    let view = pt.view();
    assert!(view.const_set_is_subset(old_p, all_p), "OLD <: ALL");
    assert!(view.const_set_is_subset(all_p, all_p_dup), "ALL <: ALL");
    assert!(!view.const_set_is_subset(all_p, old_p), "!(ALL <: OLD)");
    assert!(!view.const_set_is_subset(outside_p, all_p), "set with unknown member is not subset");
    assert!(!view.const_set_is_subset(other_old_p, all_p), "different base is not subset");
    assert!(!view.const_set_is_subset(semver_p, all_p), "base is not a const-set subset");
    assert!(!view.const_set_is_subset(old_p, semver_p), "const-set is not subset of bare base");
    assert!(!view.const_set_is_subset(range_p, range_p), "range pattern is not a const-set");

    // 2. Inference context (`InferCtx::mk_pattern_const_set` + `InferCtx::const_set_is_subset`).
    let mut cx = InferCtx::new_in(Alloc::global());
    let empty = cx.mk_list(&[]).expect("interns");
    let semver = cx.mk(TyView::Adt(ItemKey(0x5345_4d56_4552_0001), empty)).expect("interns");
    let other = cx.mk(TyView::Adt(ItemKey(0x5345_4d56_4552_0002), empty)).expect("interns");

    let old = cx.mk_pattern_const_set(semver, &[v1_2_2, v1_1_1]).expect("interns");
    let all = cx.mk_pattern_const_set(semver, &[v3_4_5, v1_1_1, v1_2_2]).expect("interns");
    let outside = cx.mk_pattern_const_set(semver, &[v1_1_1, v9_9_9]).expect("interns");
    let other_old = cx.mk_pattern_const_set(other, &[v1_1_1, v1_2_2]).expect("interns");
    let range = pat(&mut cx, TyView::Uint(UintTy::U32), &[(1, 10)], 32);

    assert!(cx.const_set_is_subset(old, all), "OLD <: ALL in InferCtx");
    assert!(cx.const_set_is_subset(all, all), "ALL <: ALL in InferCtx");
    assert!(!cx.const_set_is_subset(all, old), "!(ALL <: OLD) in InferCtx");
    assert!(!cx.const_set_is_subset(outside, all), "!(OUTSIDE <: ALL) in InferCtx");
    assert!(!cx.const_set_is_subset(other_old, all), "different base rejected in InferCtx");
    assert!(!cx.const_set_is_subset(semver, all), "bare base rejected in InferCtx");
    assert!(!cx.const_set_is_subset(range, range), "range pattern rejected in InferCtx");
}

/// Fixture with `struct SemVer { major: u16, minor: u16, patch: u16 }`, `const V1_1_1: SemVer`,
/// and `fn main() { ... }`.
fn semver_world() -> (Fx, ItemKey, ItemKey) {
    let mut world = Fx::new();
    let fx = &mut world;
    let u16_ = fx.u16();
    let semver_key = fx.strukt("SemVer", 0, &[("major", u16_), ("minor", u16_), ("patch", u16_)]);
    let semver_ty = fx.adt(semver_key, &[]);
    let _v1_1_1 = fx.konst("V1_1_1", semver_ty);
    let unit = fx.unit();
    let main = fx.func("main", 0, &[], unit, Vec::new());
    fx.finish();
    (world, semver_key, main)
}

/// `fn main() { let a: (SemVer is in [V1_1_1, V1_2_2, V3_4_5]) = V1_1_1; }`:
/// base `SemVer` where `SemVer is in VERSIONS` is expected.
fn semver_base_for_const_set(ip: InferPlant) -> crate::columns::Checked<'static> {
    let (fx, semver_key, main) = semver_world();
    let mut b = Bb::new();
    let a = b.bind("a");
    let empty = b.t.intern_list(&[]).expect("interns");
    let semver_ty = b.ty(PNode::Adt(semver_key, empty));
    let versions = b
        .t
        .pattern_const_set(
            semver_ty,
            &[semver_digest(1, 1, 1), semver_digest(1, 2, 2), semver_digest(3, 4, 5)],
        )
        .expect("interns");
    let init = b.path(&["V1_1_1"]);
    let s0 = b.let_(a, Some(versions), init);
    let root = b.block(&[s0], None);
    check_body(&fx, &b, main, root, Plant::None, ip)
}

/// Base `SemVer` where `SemVer is in VERSIONS` is expected produces `DiagCode::PatternNotBase`
/// (E0308).
#[test]
fn semver_base_where_const_set_pattern_is_expected_is_pattern_not_base() {
    let c = semver_base_for_const_set(InferPlant::None);
    assert!(c.diags.iter().any(|d| d.code == DiagCode::PatternNotBase), "{:?}", c.diags);
}

/// `SemVer is in VERSIONS` where bare `SemVer` is expected also produces `DiagCode::PatternNotBase`
/// (E0308).
#[test]
fn semver_const_set_pattern_where_base_is_expected_is_pattern_not_base() {
    let mut world = Fx::new();
    let fx = &mut world;
    let u16_ = fx.u16();
    let semver_key = fx.strukt("SemVer", 0, &[("major", u16_), ("minor", u16_), ("patch", u16_)]);
    let semver_ty = fx.adt(semver_key, &[]);
    let versions = fx
        .t
        .pattern_const_set(
            semver_ty,
            &[semver_digest(1, 1, 1), semver_digest(1, 2, 2), semver_digest(3, 4, 5)],
        )
        .expect("interns");
    let unit = fx.unit();
    let widen = fx.func("widen", 0, &[versions], unit, Vec::new());
    fx.finish();

    let mut b = Bb::new();
    let v = b.param("v");
    let out = b.bind("out");
    let empty = b.t.intern_list(&[]).expect("interns");
    let semver_b = b.ty(PNode::Adt(semver_key, empty));
    let lv = b.local(v);
    let s0 = b.let_(out, Some(semver_b), lv);
    let root = b.block(&[s0], None);
    let c = check_body(&world, &b, widen, root, Plant::None, InferPlant::None);
    assert!(c.diags.iter().any(|d| d.code == DiagCode::PatternNotBase), "{:?}", c.diags);
}

/// Two `SemVer is in ...` annotations with the same members in different orders and with
/// duplicates check clean in a full body check.
#[test]
fn semver_const_set_canonical_order_checks_clean() {
    let mut world = Fx::new();
    let fx = &mut world;
    let u16_ = fx.u16();
    let semver_key = fx.strukt("SemVer", 0, &[("major", u16_), ("minor", u16_), ("patch", u16_)]);
    let semver_ty = fx.adt(semver_key, &[]);
    let v1_1_1 = semver_digest(1, 1, 1);
    let v1_2_2 = semver_digest(1, 2, 2);
    let v3_4_5 = semver_digest(3, 4, 5);
    let param_pat = fx
        .t
        .pattern_const_set(semver_ty, &[v3_4_5, v1_1_1, v1_2_2, v1_1_1])
        .expect("interns");
    let unit = fx.unit();
    let id = fx.func("id", 0, &[param_pat], unit, Vec::new());
    fx.finish();

    let mut b = Bb::new();
    let v = b.param("v");
    let out = b.bind("out");
    let empty = b.t.intern_list(&[]).expect("interns");
    let semver_b = b.ty(PNode::Adt(semver_key, empty));
    let local_pat = b
        .t
        .pattern_const_set(semver_b, &[v1_2_2, v3_4_5, v3_4_5, v1_1_1])
        .expect("interns");
    let lv = b.local(v);
    let s0 = b.let_(out, Some(local_pat), lv);
    let root = b.block(&[s0], None);
    let c = check_body(&world, &b, id, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "expected clean check, got {:?}", c.diags);
}

/// Planted bug control: `InferPlant::PatIsBase` lets bare `SemVer` stand for
/// `SemVer is in [V1_1_1, V1_2_2, V3_4_5]`, so the wrong program checks clean.
#[test]
fn control_const_set_pattern_type_is_its_base() {
    let c = semver_base_for_const_set(InferPlant::PatIsBase);
    assert!(
        !c.diags.iter().any(|d| d.code == DiagCode::PatternNotBase),
        "the plant was not reached: {:?}",
        c.diags,
    );
}
