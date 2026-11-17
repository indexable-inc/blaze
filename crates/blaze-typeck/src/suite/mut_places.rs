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
//! ```

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
    let (u8_, usize_, unit) = (fx.u8(), fx.usize(), fx.unit());

    let bytes_k = fx.strukt("Bytes", 0, &[]);
    let bytes = fx.adt(bytes_k, &[]);
    let bytes_impl = fx.imp("impl Bytes", 0, bytes, None, &[], Vec::new());
    let m_bytes = fx.r(Mutbl::Mut, bytes);
    fx.impl_fn(bytes_impl, "push", true, &[m_bytes, u8_], unit);
    fx.imp("impl Index<usize> for Bytes", 0, bytes, Some((index, &[usize_])), &[("Output", u8_)], Vec::new());

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
