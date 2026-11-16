//! Generic associated types: a projection carries the associated type's own arguments after
//! the trait's, and normalizing it through an impl substitutes both.
//!
//! The Reference, "Associated Items", "Associated Types": "Associated types may include generic
//! parameters and where clauses; these are often referred to as generic associated types, or
//! GATs." and, of an impl: "`type Assoc<Params> = Type where WhereBounds;`". The Reference,
//! "Paths", "Qualified paths": "Fully qualified paths allow for disambiguating the path for
//! trait implementations and for specifying canonical paths": `<S as Tr>::Out<u16>` names
//! `Out` of the impl of `Tr` for `S`, at `T = u16`.
//!
//! ```text
//! trait Tr { type Out<T>; }
//! struct S;
//! impl Tr for S { type Out<T> = (T, u8); }
//! fn f() { let x: <S as Tr>::Out<u16> = (1, 2); }        // x: (u16, u8); 1: u16, 2: u8
//! fn g() { let y: <S as Tr>::Out<u16, u8> = (1, 2); }    // E0107: 1 declared, 2 written
//! fn h() { let z: <S as Tr>::Out = (1, 2); }             // E0107: 1 declared, 0 written
//! ```
//!
//! The portable forms (`world::AssocGenerics`): the projection is `Alias { Tr, Out, [S, u16] }`
//! (the trait's arguments, `Self` first, then the type's own); the impl's value is `(Param(0),
//! u8)`, over the impl's generics (none) then `T`. The world answers `Question::AssocGenerics(Tr,
//! Out)` with `parent: 1, own: 1` (`Fx::set_assoc_generics`), which is what the facts file as
//! the associated type's `own_generics` and what the count is checked against.

use super::{Want, check_body, differences};
use crate::diag::{Because, DiagCode};
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem, List, Name, UintTy};
use crate::infer::{InferPlant, assoc_def};
use crate::machine::Plant;
use crate::portable::{PNode, PTy};

/// The world above: the trait, `S`, and the three fns.
fn world() -> (Fx, ItemKey, ItemKey, [ItemKey; 3]) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Default::default(), &[]);
    fx.lang(LangItem::Sized, sized);
    let tr = fx.tr("Tr", 1, Default::default(), &["Out"]);
    fx.set_assoc_generics(tr, "Out", 1);
    let s = fx.strukt("S", 0, &[]);
    let s_t = fx.adt(s, &[]);
    // `type Out<T> = (T, u8);`: `T` is the parameter after the impl's own (it has none).
    let t = fx.param(0);
    let u8_ = fx.u8();
    let value = fx.tuple(&[t, u8_]);
    fx.imp("impl Tr for S", 0, s_t, Some((tr, &[])), &[("Out", value)], Default::default());
    let unit = fx.unit();
    let f = fx.func("f", 0, &[], unit, Default::default());
    let g = fx.func("g", 0, &[], unit, Default::default());
    let h = fx.func("h", 0, &[], unit, Default::default());
    fx.finish();
    (fx, tr, s, [f, g, h])
}

/// `<S as Tr>::Out<own..>` in the body's table.
fn out_of(b: &mut Bb, tr: ItemKey, s: ItemKey, own: &[PTy]) -> PTy {
    let s_t = b.ty(PNode::Adt(s, List::EMPTY));
    let mut args = [s_t; 3];
    let n = 1 + own.len();
    args[1..n].copy_from_slice(own);
    let args = b.t.intern_list(&args[..n]).expect("intern");
    b.ty(PNode::Alias { trait_: tr, name: Name::of("Out"), args })
}

/// `let x: <ty> = (1, 2);` as a body: the binding, and the two literals' expressions.
fn body(b: &mut Bb, ty: PTy) -> (crate::ids::PatId, ExprId) {
    let x = b.bind("x");
    let one = b.int(None);
    let two = b.int(None);
    let init = b.tuple(&[one, two]);
    let let_x = b.let_(x, Some(ty), init);
    (x, b.block(&[let_x], None))
}

/// `<S as Tr>::Out<u16>` is `(u16, u8)`: the impl's `(T, u8)` at the projection's own
/// argument, so the unsuffixed literals are a `u16` and a `u8`. Without the own argument
/// reaching the impl's value (the trait's arguments alone): the check stops with
/// `Malformed(Param { index: 0, args: 0 })` instead of a record.
///
/// Expected line: `test suite::gat::a_projection_carries_the_associated_types_own_argument ... ok`.
#[test]
fn a_projection_carries_the_associated_types_own_argument() {
    let (fx, tr, s, [f, _, _]) = world();
    let mut b = Bb::new();
    let u16_ = b.ty(PNode::Uint(UintTy::U16));
    let ty = out_of(&mut b, tr, s, &[u16_]);
    let (x, root) = body(&mut b, ty);
    let c = check_body(&fx, &b, f, root, Plant::None, InferPlant::None);
    let want = Want { exprs: &[(ExprId(0), "u16"), (ExprId(1), "u8"), (ExprId(2), "(u16, u8)"), (ExprId(3), "()")], pats: &[(x, "(u16, u8)")], diags: &[] };
    let d = differences(&fx, &b, &c, &want);
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// Two arguments, or none, where `type Out<T>` declares one: E0107 naming the associated
/// type with the declared and written counts of its OWN arguments, once per written type.
/// Without the count check: no E0107 (the extra argument is dropped, the missing one stops
/// the check as malformed).
///
/// Expected line: `test suite::gat::a_wrong_count_of_own_arguments_is_e0107 ... ok`.
#[test]
fn a_wrong_count_of_own_arguments_is_e0107() {
    let (fx, tr, s, [_, g, h]) = world();
    let out = ItemKey(assoc_def(tr, Name::of("Out")));
    for (owner, got) in [(g, 2u32), (h, 0u32)] {
        let mut b = Bb::new();
        let u16_ = b.ty(PNode::Uint(UintTy::U16));
        let u8_ = b.ty(PNode::Uint(UintTy::U8));
        let own = [u16_, u8_];
        let ty = out_of(&mut b, tr, s, &own[..got as usize]);
        let (_, root) = body(&mut b, ty);
        let c = check_body(&fx, &b, owner, root, Plant::None, InferPlant::None);
        let mut counts = c.diags.iter().filter(|d| d.code == DiagCode::GenericArgCount).map(|d| d.because);
        assert_eq!(counts.next(), Some(Some(Because::ArgCount { item: out, expected: 1, got })), "Out with {got} written: {:?}", c.diags);
        assert_eq!(counts.next(), None, "Out with {got} written: {:?}", c.diags);
    }
}
