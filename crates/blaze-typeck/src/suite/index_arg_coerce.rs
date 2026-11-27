//! Overloaded `Index` argument coercion (rules/field.rs `expr_index`, `try_coerced_index`):
//! follows rustc's `try_overloaded_place_op` + `demand_coerce`.
//!
//! When no autoderef step of `base` directly satisfies builtin indexing or `Index<idx>`,
//! `expr_index` probes `step.ty: Index<?Idx>`: if a unique impl or where-clause constrains
//! `?Idx` to a concrete or partially-known shape (not a bare unconstrained `Infer`), `index`
//! is coerced to it (recording deref / reborrow / unsize adjustments on `index`) and `Index`
//! is verified at the coerced type.
//!
//! Real-world trigger: `meta.keys().filter(|k| meta[k] == 1)` on `BTreeMap<(u64, u64), u32>`,
//! where `k: &&(u64, u64)`. `(u64, u64): Borrow<?Q>` uniquely determines `?Q = (u64, u64)`,
//! constraining `?Idx = &(u64, u64)`, and `k` deref-coerces `&&(u64, u64) -> &(u64, u64)`.

use std::vec::Vec;

use super::{adjusts, check_body};
use crate::body::{Expr, IntSuffix};
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx, show};
use crate::ids::{ExprId, ItemKey, LangItem, Mutbl, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

struct World {
    fx: Fx,
    f_map_rr: ItemKey,
    f_ref_map_rr: ItemKey,
    f_map_box: ItemKey,
    f_map_mut_ref: ItemKey,
    f_strmap_string: ItemKey,
    f_strmap_rr_str: ItemKey,
    f_slicemap_arr: ItemKey,
    f_mutmap_assign: ItemKey,
    f_generic_param: ItemKey,
    f_deref_priority: ItemKey,
    f_neg_vec_ref_usize: ItemKey,
    f_neg_arr_ref_usize: ItemKey,
    f_neg_map_wrong_key: ItemKey,
    f_neg_string_map_wrong_key: ItemKey,
}

fn world() -> World {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let deref = fx.tr("Deref", 1, Vec::new(), &["Target"]);
    fx.lang(LangItem::Deref, deref);
    let index = fx.tr("Index", 2, Vec::new(), &["Output"]);
    fx.lang(LangItem::Index, index);
    let (p0, p1, p2) = (fx.param(0), fx.param(1), fx.param(2));
    let index_sup = fx.bound(index, &[p0, p1]);
    let index_mut = fx.tr("IndexMut", 2, std::vec![index_sup], &[]);
    fx.lang(LangItem::IndexMut, index_mut);

    let borrow = fx.tr("Borrow", 2, Vec::new(), &[]);
    // `impl<T: ?Sized> Borrow<T> for T`
    fx.imp("impl<T> Borrow<T> for T", 1, p0, Some((borrow, &[p0])), &[], Vec::new());

    let u8_ = fx.u8();
    let u16_ = fx.u16();
    let u32_ = fx.u32();
    let u64_ = fx.ty(PNode::Uint(UintTy::U64));
    let usize_ = fx.usize();
    let unit = fx.unit();
    let str_ = fx.ty(PNode::Str);
    let r_str = fx.r(Mutbl::Not, str_);
    let rr_str = fx.r(Mutbl::Not, r_str);

    // `String`: `Deref<Target = str>`, `Borrow<str>` (plus blanket `Borrow<String>`).
    let string_k = fx.strukt("String", 0, &[]);
    let string_t = fx.adt(string_k, &[]);
    let r_string = fx.r(Mutbl::Not, string_t);
    fx.imp("impl Deref for String", 0, string_t, Some((deref, &[])), &[("Target", str_)], Vec::new());
    fx.imp("impl Borrow<str> for String", 0, string_t, Some((borrow, &[str_])), &[], Vec::new());

    // `Box<T>`: `Deref<Target = T>`.
    let box_k = fx.strukt("Box", 1, &[]);
    let box_p0 = fx.adt(box_k, &[p0]);
    fx.imp("impl<T> Deref for Box<T>", 1, box_p0, Some((deref, &[])), &[("Target", p0)], Vec::new());

    // `Map<K, V>`: `impl<K: Borrow<Q>, V, Q> Index<&Q> for Map<K, V> { type Output = V; }`
    let map_k = fx.strukt("Map", 2, &[]);
    let map_kv = fx.adt(map_k, &[p0, p1]);
    let r_q = fx.r(Mutbl::Not, p2);
    let k_borrow_q = fx.bound(borrow, &[p0, p2]);
    fx.imp(
        "impl<K: Borrow<Q>, V, Q> Index<&Q> for Map<K, V>",
        3,
        map_kv,
        Some((index, &[r_q])),
        &[("Output", p1)],
        std::vec![k_borrow_q],
    );

    // `MutMap<K, V>`: both `Index<&Q>` and `IndexMut<&Q>` where `K: Borrow<Q>`.
    let mutmap_k = fx.strukt("MutMap", 2, &[]);
    let mutmap_kv = fx.adt(mutmap_k, &[p0, p1]);
    let k_borrow_q = fx.bound(borrow, &[p0, p2]);
    fx.imp(
        "impl<K: Borrow<Q>, V, Q> Index<&Q> for MutMap<K, V>",
        3,
        mutmap_kv,
        Some((index, &[r_q])),
        &[("Output", p1)],
        std::vec![k_borrow_q],
    );
    let k_borrow_q = fx.bound(borrow, &[p0, p2]);
    fx.imp(
        "impl<K: Borrow<Q>, V, Q> IndexMut<&Q> for MutMap<K, V>",
        3,
        mutmap_kv,
        Some((index_mut, &[r_q])),
        &[],
        std::vec![k_borrow_q],
    );

    // `StrMap`: `impl Index<&str> for StrMap { type Output = u32; }`.
    let strmap_k = fx.strukt("StrMap", 0, &[]);
    let strmap_t = fx.adt(strmap_k, &[]);
    fx.imp("impl Index<&str> for StrMap", 0, strmap_t, Some((index, &[r_str])), &[("Output", u32_)], Vec::new());

    // `SliceMap`: `impl Index<&[u8]> for SliceMap { type Output = u32; }`.
    let slice_u8 = fx.ty(PNode::Slice(u8_));
    let r_slice_u8 = fx.r(Mutbl::Not, slice_u8);
    let slicemap_k = fx.strukt("SliceMap", 0, &[]);
    let slicemap_t = fx.adt(slicemap_k, &[]);
    fx.imp("impl Index<&[u8]> for SliceMap", 0, slicemap_t, Some((index, &[r_slice_u8])), &[("Output", u32_)], Vec::new());

    // `Outer` -> `Deref<Target = Inner>`, `Outer: Index<&str, Output = u8>`, `Inner: Index<&String, Output = u16>`.
    let inner_k = fx.strukt("Inner", 0, &[]);
    let inner_t = fx.adt(inner_k, &[]);
    let outer_k = fx.strukt("Outer", 0, &[]);
    let outer_t = fx.adt(outer_k, &[]);
    fx.imp("impl Deref for Outer", 0, outer_t, Some((deref, &[])), &[("Target", inner_t)], Vec::new());
    fx.imp("impl Index<&str> for Outer", 0, outer_t, Some((index, &[r_str])), &[("Output", u8_)], Vec::new());
    fx.imp("impl Index<&String> for Inner", 0, inner_t, Some((index, &[r_string])), &[("Output", u16_)], Vec::new());

    // `Vec<T>` and `[T]` with multiple `Index` impls (`usize` and `Range`) via `SliceIndex`.
    let range_k = fx.strukt("Range", 0, &[]);
    let range_t = fx.adt(range_k, &[]);
    let slice_index = fx.tr("SliceIndex", 2, Vec::new(), &["Output"]);
    let slice_p0 = fx.ty(PNode::Slice(p0));
    fx.imp("impl<T> SliceIndex<[T]> for usize", 1, usize_, Some((slice_index, &[slice_p0])), &[("Output", p0)], Vec::new());
    fx.imp("impl<T> SliceIndex<[T]> for Range", 1, range_t, Some((slice_index, &[slice_p0])), &[("Output", slice_p0)], Vec::new());
    let vec_k = fx.strukt("Vec", 1, &[]);
    let vec_p0 = fx.adt(vec_k, &[p0]);
    let i_si_slice = fx.bound(slice_index, &[p1, slice_p0]);
    let si_out = fx.alias(slice_index, "Output", &[p1, slice_p0]);
    fx.imp(
        "impl<T, I: SliceIndex<[T]>> Index<I> for Vec<T>",
        2,
        vec_p0,
        Some((index, &[p1])),
        &[("Output", si_out)],
        std::vec![i_si_slice],
    );
    let i_si_slice = fx.bound(slice_index, &[p1, slice_p0]);
    let si_out = fx.alias(slice_index, "Output", &[p1, slice_p0]);
    fx.imp(
        "impl<T, I: SliceIndex<[T]>> Index<I> for [T]",
        2,
        slice_p0,
        Some((index, &[p1])),
        &[("Output", si_out)],
        std::vec![i_si_slice],
    );

    // Concrete types for test functions.
    let key_pair = fx.tuple(&[u64_, u64_]);
    let r_key = fx.r(Mutbl::Not, key_pair);
    let rr_key = fx.r(Mutbl::Not, r_key);
    let mut_key = fx.r(Mutbl::Mut, key_pair);
    let box_key = fx.adt(box_k, &[key_pair]);
    let r_box_key = fx.r(Mutbl::Not, box_key);
    let map_pair_u32 = fx.adt(map_k, &[key_pair, u32_]);
    let r_map_pair_u32 = fx.r(Mutbl::Not, map_pair_u32);
    let map_string_u32 = fx.adt(map_k, &[string_t, u32_]);
    let mutmap_pair_u32 = fx.adt(mutmap_k, &[key_pair, u32_]);
    let m_mutmap = fx.r(Mutbl::Mut, mutmap_pair_u32);
    let len4 = fx.ty(PNode::ConstValue { ty: usize_, bits: 4 });
    let arr_u8_4 = fx.ty(PNode::Array(u8_, len4));
    let r_arr_u8_4 = fx.r(Mutbl::Not, arr_u8_4);
    let arr_u32_4 = fx.ty(PNode::Array(u32_, len4));
    let vec_u32 = fx.adt(vec_k, &[u32_]);
    let r_usize = fx.r(Mutbl::Not, usize_);
    let r_u32 = fx.r(Mutbl::Not, u32_);
    let rr_u32 = fx.r(Mutbl::Not, r_u32);
    let r_p0 = fx.r(Mutbl::Not, p0);

    let f_map_rr = fx.func("f_map_rr", 0, &[map_pair_u32, rr_key], u32_, Vec::new());
    let f_ref_map_rr = fx.func("f_ref_map_rr", 0, &[r_map_pair_u32, rr_key], u32_, Vec::new());
    let f_map_box = fx.func("f_map_box", 0, &[map_pair_u32, r_box_key], u32_, Vec::new());
    let f_map_mut_ref = fx.func("f_map_mut_ref", 0, &[map_pair_u32, mut_key], u32_, Vec::new());
    let f_strmap_string = fx.func("f_strmap_string", 0, &[strmap_t, r_string], u32_, Vec::new());
    let f_strmap_rr_str = fx.func("f_strmap_rr_str", 0, &[strmap_t, rr_str], u32_, Vec::new());
    let f_slicemap_arr = fx.func("f_slicemap_arr", 0, &[slicemap_t, r_arr_u8_4], u32_, Vec::new());
    let f_mutmap_assign = fx.func("f_mutmap_assign", 0, &[m_mutmap, rr_key], unit, Vec::new());
    let m_idx_bound = fx.bound(index, &[p0, r_u32]);
    let m_idx_out = fx.proj(index, &[p0, r_u32], "Output", u8_);
    let f_generic_param = fx.func("f_generic_param", 1, &[r_p0, rr_u32], u8_, std::vec![m_idx_bound, m_idx_out]);
    let f_deref_priority = fx.func("f_deref_priority", 0, &[outer_t, r_string], u16_, Vec::new());
    let f_neg_vec_ref_usize = fx.func("f_neg_vec_ref_usize", 0, &[vec_u32, r_usize], u32_, Vec::new());
    let f_neg_arr_ref_usize = fx.func("f_neg_arr_ref_usize", 0, &[arr_u32_4, r_usize], u32_, Vec::new());
    let f_neg_map_wrong_key = fx.func("f_neg_map_wrong_key", 0, &[map_pair_u32, r_u32], u32_, Vec::new());
    let f_neg_string_map_wrong_key = fx.func("f_neg_string_map_wrong_key", 0, &[map_string_u32, r_u32], u32_, Vec::new());

    fx.finish();
    World {
        fx,
        f_map_rr,
        f_ref_map_rr,
        f_map_box,
        f_map_mut_ref,
        f_strmap_string,
        f_strmap_rr_str,
        f_slicemap_arr,
        f_mutmap_assign,
        f_generic_param,
        f_deref_priority,
        f_neg_vec_ref_usize,
        f_neg_arr_ref_usize,
        f_neg_map_wrong_key,
        f_neg_string_map_wrong_key,
    }
}

/// Checks `{ m[k] }`, returning `(base_expr, index_expr, index_op_expr, checked)`.
fn check_index(fx: &Fx, owner: ItemKey) -> (ExprId, ExprId, ExprId, crate::columns::Checked<'static>) {
    let mut b = Bb::new();
    let m = b.param("m");
    let k = b.param("k");
    let lm = b.local(m);
    let lk = b.local(k);
    let indexed = b.e(Expr::Index { base: lm, index: lk });
    let root = b.block(&[], Some(indexed));
    let c = check_body(fx, &b, owner, root, Plant::None, InferPlant::None);
    (lm, lk, indexed, c)
}

/// `Map<(u64, u64), u32>[k]` with `k: &&(u64, u64)`, `&Map<(u64, u64), u32>[k]`,
/// `Map<(u64, u64), u32>[&Box<(u64, u64)>]`, and `Map<(u64, u64), u32>[&mut (u64, u64)]`.
#[test]
fn btreemap_like_borrow_index_coerces_argument() {
    let w = world();

    // `meta[k]` where `meta: Map<(u64, u64), u32>`, `k: &&(u64, u64)`
    let (lm, lk, indexed, c) = check_index(&w.fx, w.f_map_rr);
    assert!(c.diags.is_empty(), "f_map_rr: {:?}", c.diags);
    assert_eq!(show(&w.fx, c.types.view(), c.expr_ty[indexed.ix()]), "u32");
    assert_eq!(adjusts(&w.fx, &c, lm), "");
    assert_eq!(adjusts(&w.fx, &c, lk), "* -> &(u64, u64), * -> (u64, u64), & -> &(u64, u64)");

    // `meta_ref[k]` where `meta_ref: &Map<(u64, u64), u32>`, `k: &&(u64, u64)`
    let (lm, lk, indexed, c) = check_index(&w.fx, w.f_ref_map_rr);
    assert!(c.diags.is_empty(), "f_ref_map_rr: {:?}", c.diags);
    assert_eq!(show(&w.fx, c.types.view(), c.expr_ty[indexed.ix()]), "u32");
    assert_eq!(adjusts(&w.fx, &c, lm), "* -> Map<(u64, u64), u32>");
    assert_eq!(adjusts(&w.fx, &c, lk), "* -> &(u64, u64), * -> (u64, u64), & -> &(u64, u64)");

    // `meta[&Box<(u64, u64)>]`
    let (_, lk, indexed, c) = check_index(&w.fx, w.f_map_box);
    assert!(c.diags.is_empty(), "f_map_box: {:?}", c.diags);
    assert_eq!(show(&w.fx, c.types.view(), c.expr_ty[indexed.ix()]), "u32");
    assert_eq!(adjusts(&w.fx, &c, lk), "* -> Box<(u64, u64)>, *Deref -> (u64, u64), & -> &(u64, u64)");

    // `meta[&mut (u64, u64)]`
    let (_, lk, indexed, c) = check_index(&w.fx, w.f_map_mut_ref);
    assert!(c.diags.is_empty(), "f_map_mut_ref: {:?}", c.diags);
    assert_eq!(show(&w.fx, c.types.view(), c.expr_ty[indexed.ix()]), "u32");
    assert_eq!(adjusts(&w.fx, &c, lk), "* -> (u64, u64), & -> &(u64, u64)");
}

/// `StrMap[&String]`, `StrMap[&&str]`, `SliceMap[&[u8; 4]]`, and generic `M: Index<&u32>`.
#[test]
fn deref_unsize_and_param_env_index_coercions() {
    let w = world();

    // `StrMap[&String]` -> `&str` via `String: Deref<Target = str>`
    let (_, lk, indexed, c) = check_index(&w.fx, w.f_strmap_string);
    assert!(c.diags.is_empty(), "f_strmap_string: {:?}", c.diags);
    assert_eq!(show(&w.fx, c.types.view(), c.expr_ty[indexed.ix()]), "u32");
    assert_eq!(adjusts(&w.fx, &c, lk), "* -> String, *Deref -> str, & -> &str");

    // `StrMap[&&str]` -> `&str`
    let (_, lk, indexed, c) = check_index(&w.fx, w.f_strmap_rr_str);
    assert!(c.diags.is_empty(), "f_strmap_rr_str: {:?}", c.diags);
    assert_eq!(show(&w.fx, c.types.view(), c.expr_ty[indexed.ix()]), "u32");
    assert_eq!(adjusts(&w.fx, &c, lk), "* -> &str, * -> str, & -> &str");

    // `SliceMap[&[u8; 4]]` -> `&[u8]` (unsizing coercion)
    let (_, lk, indexed, c) = check_index(&w.fx, w.f_slicemap_arr);
    assert!(c.diags.is_empty(), "f_slicemap_arr: {:?}", c.diags);
    assert_eq!(show(&w.fx, c.types.view(), c.expr_ty[indexed.ix()]), "u32");
    assert_eq!(adjusts(&w.fx, &c, lk), "* -> [u8; 4], & -> &[u8; 4], unsize -> &[u8]");

    // Generic `fn<M: Index<&u32, Output = u8>>(m: &M, k: &&u32) -> u8 { m[k] }`
    let (lm, lk, indexed, c) = check_index(&w.fx, w.f_generic_param);
    assert!(c.diags.is_empty(), "f_generic_param: {:?}", c.diags);
    assert_eq!(show(&w.fx, c.types.view(), c.expr_ty[indexed.ix()]), "u8");
    assert_eq!(adjusts(&w.fx, &c, lm), "* -> P0");
    assert_eq!(adjusts(&w.fx, &c, lk), "* -> &u32, * -> u32, & -> &u32");
}

/// Mutable indexing `mm[k] = 7u32` where `k: &&(u64, u64)` coerces to `&(u64, u64)` selects
/// `IndexMut` at the coerced index type.
#[test]
fn mutable_index_uses_coerced_index_type_for_index_mut() {
    let w = world();
    let mut b = Bb::new();
    let m = b.param("m");
    let k = b.param("k");
    let lm = b.local(m);
    let lk = b.local(k);
    let place = b.e(Expr::Index { base: lm, index: lk });
    let val = b.int(Some(IntSuffix::Uint(UintTy::U32)));
    let assign = b.e(Expr::Assign { lhs: place, rhs: val });
    let s = b.semi(assign);
    let root = b.block(&[s], None);
    let c = check_body(&w.fx, &b, w.f_mutmap_assign, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "f_mutmap_assign: {:?}", c.diags);
    assert!(c.index_is_mut(place), "`mm[k] = 7u32` selects `IndexMut<&(u64, u64)>`");
    assert_eq!(adjusts(&w.fx, &c, lm), "* -> MutMap<(u64, u64), u32>");
    assert_eq!(adjusts(&w.fx, &c, lk), "* -> &(u64, u64), * -> (u64, u64), & -> &(u64, u64)");
}

/// Direct `Index<idx>` across receiver autoderef steps takes priority over index-argument coercion,
/// so `Deref` receiver indexing never regresses.
#[test]
fn direct_receiver_autoderef_precedes_index_coercion() {
    let w = world();
    let (lm, lk, indexed, c) = check_index(&w.fx, w.f_deref_priority);
    assert!(c.diags.is_empty(), "f_deref_priority: {:?}", c.diags);
    assert_eq!(show(&w.fx, c.types.view(), c.expr_ty[indexed.ix()]), "u16");
    assert_eq!(adjusts(&w.fx, &c, lm), "*Deref -> Inner");
    assert_eq!(adjusts(&w.fx, &c, lk), "");
}

/// Negative controls:
/// - `vec[&0usize]` and `arr[&0usize]` MUST fail (multiple competing `Index` impls and no `&usize -> usize` coercion).
/// - `map[&wrong]` on both unique-`Borrow` (`Map<(u64, u64), u32>`) and multi-`Borrow` (`Map<String, u32>`) MUST fail.
#[test]
fn negative_controls_reject_invalid_indexing() {
    let w = world();

    for (name, owner) in [
        ("vec[&usize]", w.f_neg_vec_ref_usize),
        ("arr[&usize]", w.f_neg_arr_ref_usize),
        ("Map<(u64, u64), u32>[&u32]", w.f_neg_map_wrong_key),
        ("Map<String, u32>[&u32]", w.f_neg_string_map_wrong_key),
    ] {
        let (_, _, indexed, c) = check_index(&w.fx, owner);
        let codes: Vec<_> = c.diags.iter().map(|d| (d.code, d.at.start)).collect();
        assert_eq!(codes, [(DiagCode::CantIndex, indexed.0)], "CONTROL {name} must fail with E0608");
    }
}
