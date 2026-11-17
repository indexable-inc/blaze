//! A mutable place through an overloaded dereference calls `DerefMut::deref_mut`
//! (rules/mut_places.rs).
//!
//! The rule. The Reference, "Operator expressions", "The dereference operator": "On non-pointer
//! types `*x` is equivalent to `*std::ops::Deref::deref(&x)` in an immutable place expression
//! context and `*std::ops::DerefMut::deref_mut(&mut x)` in a mutable place expression context."
//! "Place expressions and value expressions", "Mutability": a place "assigned to, mutably
//! borrowed, implicitly mutably borrowed, or bound to a pattern containing `ref mut`" is a
//! mutable place. The pinned library, core/src/ops/deref.rs ("Mutable deref coercion"): "If `T`
//! implements `DerefMut<Target = U>`, and `v` is a value of type `T`, then: In mutable contexts,
//! `*v` (where `T` is neither a reference nor a raw pointer) is equivalent to
//! `*DerefMut::deref_mut(&mut v)`", and `DerefMut` is `pub trait DerefMut: Deref`, so a type with
//! only `Deref` (alloc's `Rc`) is never dereferenced mutably.
//!
//! The toy world (`Bytes` stands for `Vec<u8>`: alloc/src/vec/mod.rs `pub fn push(&mut self,
//! value: T)` and `impl Index<I> for Vec<T, A>`):
//! ```text
//! #[lang = "sized"] trait Sized {}
//! #[lang = "deref"] trait Deref { type Target; }  #[lang = "deref_mut"] trait DerefMut: Deref {}
//! #[lang = "index"] trait Index<Idx> { type Output; }
//! struct Bytes;  impl Bytes { fn push(&mut self, x: u8); }  impl Index<usize> for Bytes { type Output = u8; }
//! struct B(Bytes);  impl Deref for B { type Target = Bytes; }  impl DerefMut for B {}
//! struct R(Bytes);  impl Deref for R { type Target = Bytes; }   // no DerefMut, as `Rc`
//! fn a_method(b: &mut B) { b.push(1u8); }
//! fn b_explicit(b: &mut B) { (**b).push(1u8); }
//! fn c_index(b: &mut B) { b[0usize] = 2u8; }
//! fn d_deref_only(r: &mut R) { r.push(1u8); }
//! #[lang = "index_mut"] trait IndexMut<Idx>: Index<Idx> {}  impl IndexMut<usize> for Bytes {}
//! struct Fixed;  impl Index<usize> for Fixed { type Output = u8; }        // no IndexMut
//! struct Raw;  impl Index<usize> for Raw { type Output = *mut u8; }       // no IndexMut
//! fn e_shapes(v: &mut Bytes);  fn f_fixed(f: &mut Fixed);  fn g_raw(r: &Raw);
//! struct Grid;  impl Index<usize> for Grid { type Output = Bytes; }  impl IndexMut<usize> for Grid {}
//! struct Pair { f: u8 }  struct Pairs;  impl Index<usize> for Pairs { type Output = Pair; }  // + IndexMut
//! fn h_nested(g: &mut Grid, p: &mut Pairs);
//! ```
//! An overloaded index follows the same rule ("Array and slice indexing expressions").

use std::vec::Vec;

use super::{adjusts, check_body};
use crate::body::{Expr, IntSuffix, UnOp};
use crate::columns::Checked;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, LangItem, Mutbl, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;

fn world() -> Fx {
    let mut world = Fx::new();
    // Built through `&mut Fx`, as pin_unsize.rs's world is.
    let fx = &mut world;
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let deref = fx.tr("Deref", 1, Vec::new(), &["Target"]);
    fx.lang(LangItem::Deref, deref);
    let p0 = fx.param(0);
    let sup = fx.bound(deref, &[p0]);
    let deref_mut = fx.tr("DerefMut", 1, std::vec![sup], &[]);
    fx.lang(LangItem::DerefMut, deref_mut);
    let index = fx.tr("Index", 2, Vec::new(), &["Output"]);
    fx.lang(LangItem::Index, index);
    let p1 = fx.param(1);
    let index_sup = fx.bound(index, &[p0, p1]);
    let index_mut = fx.tr("IndexMut", 2, std::vec![index_sup], &[]);
    fx.lang(LangItem::IndexMut, index_mut);
    let (u8_, usize_, unit) = (fx.u8(), fx.usize(), fx.unit());

    let bytes_k = fx.strukt("Bytes", 0, &[]);
    let bytes = fx.adt(bytes_k, &[]);
    let bytes_impl = fx.imp("impl Bytes", 0, bytes, None, &[], Vec::new());
    let m_bytes = fx.r(Mutbl::Mut, bytes);
    fx.impl_fn(bytes_impl, "push", true, &[m_bytes, u8_], unit);
    fx.imp("impl Index<usize> for Bytes", 0, bytes, Some((index, &[usize_])), &[("Output", u8_)], Vec::new());
    fx.imp("impl IndexMut<usize> for Bytes", 0, bytes, Some((index_mut, &[usize_])), &[], Vec::new());
    // `Fixed`: `Index` only (a `HashMap`'s shape, whose `m[k] = v` is E0596).
    let fixed_k = fx.strukt("Fixed", 0, &[]);
    let fixed = fx.adt(fixed_k, &[]);
    fx.imp("impl Index<usize> for Fixed", 0, fixed, Some((index, &[usize_])), &[("Output", u8_)], Vec::new());
    // `Raw`: `Index` only, its element a raw pointer written through.
    let raw_k = fx.strukt("Raw", 0, &[]);
    let raw = fx.adt(raw_k, &[]);
    let p_u8 = fx.ty(crate::portable::PNode::Ptr(Mutbl::Mut, u8_));
    fx.imp("impl Index<usize> for Raw", 0, raw, Some((index, &[usize_])), &[("Output", p_u8)], Vec::new());
    let (m_fixed, r_raw) = (fx.r(Mutbl::Mut, fixed), fx.r(Mutbl::Not, raw));
    fx.func("e_shapes", 0, &[m_bytes], unit, Vec::new());
    fx.func("f_fixed", 0, &[m_fixed], unit, Vec::new());
    fx.func("g_raw", 0, &[r_raw], unit, Vec::new());
    // `Grid`: rows of `Bytes`; `Pairs`: elements with a field.
    let grid_k = fx.strukt("Grid", 0, &[]);
    let grid = fx.adt(grid_k, &[]);
    fx.imp("impl Index<usize> for Grid", 0, grid, Some((index, &[usize_])), &[("Output", bytes)], Vec::new());
    fx.imp("impl IndexMut<usize> for Grid", 0, grid, Some((index_mut, &[usize_])), &[], Vec::new());
    let pair_k = fx.strukt("Pair", 0, &[("f", u8_)]);
    let pair = fx.adt(pair_k, &[]);
    let pairs_k = fx.strukt("Pairs", 0, &[]);
    let pairs = fx.adt(pairs_k, &[]);
    fx.imp("impl Index<usize> for Pairs", 0, pairs, Some((index, &[usize_])), &[("Output", pair)], Vec::new());
    fx.imp("impl IndexMut<usize> for Pairs", 0, pairs, Some((index_mut, &[usize_])), &[], Vec::new());
    let (m_grid, m_pairs) = (fx.r(Mutbl::Mut, grid), fx.r(Mutbl::Mut, pairs));
    fx.func("h_nested", 0, &[m_grid, m_pairs], unit, Vec::new());

    let b_k = fx.tuple_strukt("B", 0, &[bytes]);
    let b = fx.adt(b_k, &[]);
    fx.imp("impl Deref for B", 0, b, Some((deref, &[])), &[("Target", bytes)], Vec::new());
    fx.imp("impl DerefMut for B", 0, b, Some((deref_mut, &[])), &[], Vec::new());
    let r_k = fx.tuple_strukt("R", 0, &[bytes]);
    let r = fx.adt(r_k, &[]);
    fx.imp("impl Deref for R", 0, r, Some((deref, &[])), &[("Target", bytes)], Vec::new());

    let (m_b, m_r) = (fx.r(Mutbl::Mut, b), fx.r(Mutbl::Mut, r));
    fx.func("a_method", 0, &[m_b], unit, Vec::new());
    fx.func("b_explicit", 0, &[m_b], unit, Vec::new());
    fx.func("c_index", 0, &[m_b], unit, Vec::new());
    fx.func("d_deref_only", 0, &[m_r], unit, Vec::new());
    fx.finish();
    world
}

/// `{ b.push(1u8); }`, or with `explicit` `{ (**b).push(1u8); }`: the expression whose
/// adjustments the case reads (`b`, or the inner `*b`), and the check.
fn push_case(fx: &Fx, owner: &str, explicit: bool) -> (ExprId, Checked<'static>) {
    let mut bb = Bb::new();
    let p = bb.param("b");
    let local = bb.local(p);
    // `(**b)`: the inner `*` is the builtin one of `&mut B`; the outer, overloaded, is
    // recorded on the inner (op.rs: the check puts an explicit overloaded `*x` on `x`).
    let (receiver, read) = if explicit {
        let inner = bb.e(Expr::Unary { op: UnOp::Deref, operand: local });
        let outer = bb.e(Expr::Unary { op: UnOp::Deref, operand: inner });
        (bb.e(Expr::Paren(outer)), inner)
    } else {
        (local, local)
    };
    let one = bb.int(Some(IntSuffix::Uint(UintTy::U8)));
    let call = bb.method(receiver, "push", &[one]);
    let s = bb.semi(call);
    let root = bb.block(&[s], None);
    let c = check_body(fx, &bb, fx.key_of(owner), root, Plant::None, InferPlant::None);
    (read, c)
}

/// `{ b[0usize] = 2u8; }`: the indexed base's id and the check.
fn index_case(fx: &Fx) -> (ExprId, Checked<'static>) {
    let mut bb = Bb::new();
    let p = bb.param("b");
    let base = bb.local(p);
    let zero = bb.int(Some(IntSuffix::Uint(UintTy::Usize)));
    let place = bb.e(Expr::Index { base, index: zero });
    let two = bb.int(Some(IntSuffix::Uint(UintTy::U8)));
    let assign = bb.e(Expr::Assign { lhs: place, rhs: two });
    let s = bb.semi(assign);
    let root = bb.block(&[s], None);
    let c = check_body(fx, &bb, fx.key_of("c_index"), root, Plant::None, InferPlant::None);
    (base, c)
}

/// A method receiver's `&mut` autoref (`b.push(1)`), an explicit `*` under one (`(**b).push(1)`)
/// and the left of an assignment through an index (`b[0] = 2`) are mutable places, so the
/// overloaded dereference of `B` each goes through is recorded `DerefMut::deref_mut`; through
/// `R`, which implements only `Deref`, it stays `Deref::deref` (the borrow check's E0596, not the
/// type check's) and the check records no finding. Without the fix (rules/mut_places.rs) the
/// first three record `*Deref`; a fix that upgrades without asking `R: DerefMut` fails the
/// fourth.
/// Should print: "test suite::mut_places::a_mutable_place_through_an_overloaded_deref_calls_deref_mut ... ok".
#[test]
fn a_mutable_place_through_an_overloaded_deref_calls_deref_mut() {
    let fx = world();
    let (e, c) = push_case(&fx, "a_method", false);
    assert!(c.diags.is_empty(), "a_method: {:?}", c.diags);
    assert_eq!(adjusts(&fx, &c, e), "* -> B, *DerefMut -> Bytes, &mut 2φ -> &mut Bytes", "a_method");

    let (e, c) = push_case(&fx, "b_explicit", true);
    assert!(c.diags.is_empty(), "b_explicit: {:?}", c.diags);
    assert_eq!(adjusts(&fx, &c, e), "*DerefMut -> Bytes", "b_explicit");

    let (e, c) = index_case(&fx);
    assert!(c.diags.is_empty(), "c_index: {:?}", c.diags);
    assert_eq!(adjusts(&fx, &c, e), "* -> B, *DerefMut -> Bytes", "c_index");

    let (e, c) = push_case(&fx, "d_deref_only", false);
    assert!(c.diags.is_empty(), "d_deref_only: {:?}", c.diags);
    assert_eq!(adjusts(&fx, &c, e), "* -> R, *Deref -> Bytes, &mut 2φ -> &mut Bytes", "d_deref_only");
}

/// `v[i]` on `v: &mut Bytes` (one statement each, `v` a fresh use) and the index expression.
fn at(bb: &mut Bb, v: crate::ids::PatId) -> ExprId {
    let base = bb.local(v);
    let i = bb.int(Some(IntSuffix::Uint(UintTy::Usize)));
    bb.e(Expr::Index { base, index: i })
}

/// The mutable place expression contexts of an overloaded index select `IndexMut::index_mut`
/// (`Checked::index_mut`), the others `Index::index` (the Reference, "Array and slice indexing
/// expressions": "*std::ops::IndexMut::index_mut(&mut a, b) in a mutable place expression
/// context"):
/// ```text
/// fn e_shapes(v: &mut Bytes) {
///     v[0] = 2u8;             // assigned to: IndexMut
///     v[0] += 1u8;            // compound-assigned: IndexMut
///     let r = &mut v[0];      // mutably borrowed: IndexMut
///     let x = v[0];           // read: Index
///     let s = &v[0];          // shared borrow: Index
///     b[0usize] = 2u8;        // (c_index) through `B: DerefMut`: IndexMut on `Bytes`
/// }
/// ```
/// With the selection removed (rules/mut_places.rs `mut_indexes`), `index_mut` is empty and the
/// first three fail; a selection that ignored the context lists the last two.
/// Should print: "test suite::mut_places::a_mutable_place_through_an_overloaded_index_calls_index_mut ... ok".
#[test]
fn a_mutable_place_through_an_overloaded_index_calls_index_mut() {
    let fx = world();
    let mut bb = Bb::new();
    let v = bb.param("v");
    let a = at(&mut bb, v);
    let two = bb.int(Some(IntSuffix::Uint(UintTy::U8)));
    let assign = bb.e(Expr::Assign { lhs: a, rhs: two });
    let s1 = bb.semi(assign);
    let b = at(&mut bb, v);
    let one = bb.int(Some(IntSuffix::Uint(UintTy::U8)));
    let add = bb.e(Expr::AssignOp { op: crate::body::BinOp::Add, lhs: b, rhs: one });
    let s2 = bb.semi(add);
    let c_ = at(&mut bb, v);
    let r_init = bb.borrow(Mutbl::Mut, c_);
    let r = bb.bind("r");
    let s3 = bb.let_(r, None, r_init);
    let d = at(&mut bb, v);
    let x = bb.bind("x");
    let s4 = bb.let_(x, None, d);
    let e_ = at(&mut bb, v);
    let s_init = bb.borrow(Mutbl::Not, e_);
    let s = bb.bind("s");
    let s5 = bb.let_(s, None, s_init);
    let root = bb.block(&[s1, s2, s3, s4, s5], None);
    let c = check_body(&fx, &bb, fx.key_of("e_shapes"), root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "e_shapes: {:?}", c.diags);
    let mut want = std::vec![a, b, c_];
    want.sort();
    let got: Vec<ExprId> = c.index_mut.iter().copied().collect();
    assert_eq!(got, want, "assigned, compound-assigned, `&mut`: IndexMut; read, `&`: Index");
    for (e, m) in [(a, true), (b, true), (c_, true), (d, false), (e_, false)] {
        assert_eq!(c.index_is_mut(e), m, "{e:?}");
    }

    let mut bb = Bb::new();
    let p = bb.param("b");
    let base = bb.local(p);
    let zero = bb.int(Some(IntSuffix::Uint(UintTy::Usize)));
    let place = bb.e(Expr::Index { base, index: zero });
    let two = bb.int(Some(IntSuffix::Uint(UintTy::U8)));
    let assign = bb.e(Expr::Assign { lhs: place, rhs: two });
    let s = bb.semi(assign);
    let root = bb.block(&[s], None);
    let c = check_body(&fx, &bb, fx.key_of("c_index"), root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "c_index: {:?}", c.diags);
    assert!(c.index_is_mut(place), "`b[0] = 2` through `B: DerefMut` calls `Bytes`'s `index_mut`");
}

/// An overloaded index whose type implements `Index` but not `IndexMut`, in a mutable place, is
/// E0596 at the index (rustc's borrow check: "cannot assign to data in an index of `Fixed`",
/// "trait `IndexMut` is required to modify indexed content"): `fn f_fixed(f: &mut Fixed) {
/// f[0] = 2u8; }`. Below a raw pointer's dereference the index's place is only read: `fn
/// g_raw(r: &Raw) { *r[0] = 2u8; }` (`Raw: Index<usize, Output = *mut u8>` only) checks clean
/// and stays `Index::index`, as in rustc. A check that refused every mutable chain fails the
/// second; one that kept `Index` silently fails the first.
/// Should print: "test suite::mut_places::a_mutable_place_through_an_index_without_index_mut_is_e0596 ... ok".
#[test]
fn a_mutable_place_through_an_index_without_index_mut_is_e0596() {
    let fx = world();
    let mut bb = Bb::new();
    let f = bb.param("f");
    let place = at(&mut bb, f);
    let two = bb.int(Some(IntSuffix::Uint(UintTy::U8)));
    let assign = bb.e(Expr::Assign { lhs: place, rhs: two });
    let s = bb.semi(assign);
    let root = bb.block(&[s], None);
    let c = check_body(&fx, &bb, fx.key_of("f_fixed"), root, Plant::None, InferPlant::None);
    let codes: Vec<_> = c.diags.iter().map(|d| (d.code, d.code.code())).collect();
    assert_eq!(codes, [(crate::diag::DiagCode::IndexNotMut, "E0596")], "f_fixed");
    assert!(c.index_mut.is_empty());

    let mut bb = Bb::new();
    let r = bb.param("r");
    let place = at(&mut bb, r);
    let star = bb.e(Expr::Unary { op: UnOp::Deref, operand: place });
    let two = bb.int(Some(IntSuffix::Uint(UintTy::U8)));
    let assign = bb.e(Expr::Assign { lhs: star, rhs: two });
    let s = bb.semi(assign);
    let root = bb.block(&[s], None);
    let c = check_body(&fx, &bb, fx.key_of("g_raw"), root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "g_raw: {:?}", c.diags);
    assert!(!c.index_is_mut(place), "a write through the raw pointer `r[0]` holds needs no `IndexMut`");
}

/// Nested places select `IndexMut` at every overloaded index on the chain:
/// ```text
/// fn h_nested(g: &mut Grid, p: &mut Pairs) {
///     g[0][1] = 2u8;      // both indexes: `Grid`'s and `Bytes`'s
///     p[0].f = 3u8;       // a field of the element
///     g[1].push(4u8);     // a `&mut self` method: its receiver's `&mut` autoref
///     let n = g[2][3];    // read: neither
/// }
/// ```
/// Without the chain walk through indexes and fields only the outermost place (or nothing) is
/// listed; a selection by syntax alone lists the read.
/// Should print: "test suite::mut_places::nested_places_and_a_mut_self_receiver_select_index_mut ... ok".
#[test]
fn nested_places_and_a_mut_self_receiver_select_index_mut() {
    let fx = world();
    let mut bb = Bb::new();
    let (g, p) = (bb.param("g"), bb.param("p"));
    let row = at(&mut bb, g);
    let one = bb.int(Some(IntSuffix::Uint(UintTy::Usize)));
    let cell = bb.e(Expr::Index { base: row, index: one });
    let two = bb.int(Some(IntSuffix::Uint(UintTy::U8)));
    let set = bb.e(Expr::Assign { lhs: cell, rhs: two });
    let s1 = bb.semi(set);
    let elem = at(&mut bb, p);
    let f = bb.e(Expr::Field { base: elem, name: crate::ids::Name::of("f") });
    let three = bb.int(Some(IntSuffix::Uint(UintTy::U8)));
    let set_f = bb.e(Expr::Assign { lhs: f, rhs: three });
    let s2 = bb.semi(set_f);
    let recv = at(&mut bb, g);
    let four = bb.int(Some(IntSuffix::Uint(UintTy::U8)));
    let call = bb.method(recv, "push", &[four]);
    let s3 = bb.semi(call);
    let row_r = at(&mut bb, g);
    let j = bb.int(Some(IntSuffix::Uint(UintTy::Usize)));
    let cell_r = bb.e(Expr::Index { base: row_r, index: j });
    let n = bb.bind("n");
    let s4 = bb.let_(n, None, cell_r);
    let root = bb.block(&[s1, s2, s3, s4], None);
    let c = check_body(&fx, &bb, fx.key_of("h_nested"), root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "h_nested: {:?}", c.diags);
    let mut want = std::vec![row, cell, elem, recv];
    want.sort();
    let got: Vec<ExprId> = c.index_mut.iter().copied().collect();
    assert_eq!(got, want, "`g[0][1] =`: both; `p[0].f =`: `p[0]`; `g[1].push()`: `g[1]`; a read: none");
    assert!(!c.index_is_mut(row_r) && !c.index_is_mut(cell_r));
}
