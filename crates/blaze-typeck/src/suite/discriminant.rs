//! `<T as DiscriminantKind>::Discriminant`: the trait has no impl in the library; the language
//! gives it for every type (library/core/src/marker.rs: "This trait is automatically
//! implemented for every type"). Every derived `PartialEq::eq` on an enum reads it through
//! `core::intrinsics::discriminant_value`. The Reference, "Enumerations", "Discriminants":
//! "Under the default representation, the discriminant is interpreted as an `isize` value";
//! "Type Layout", "Primitive representations": `#[repr(u8)]` makes it a `u8`. A struct's is
//! `u8` (`Ty::discriminant_ty`), a type parameter's stays a projection.
//!
//! ```text
//! #[lang = "discriminant_kind"] trait DiscriminantKind { #[lang = "discriminant_type"] type Discriminant; }
//! fn discriminant_value<T>(v: &T) -> <T as DiscriminantKind>::Discriminant;
//! enum E { A, B }            const E0: E;
//! #[repr(u8)] enum R { A }   const R0: R;
//! struct S;                  const S0: S;
//! fn h() { let d: WANT = discriminant_value(&X0); }
//! ```

use std::vec::Vec;

use super::check_body;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{IntTy, ItemKey, LangItem, Mutbl, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;
use crate::world::{DiscrRepr, TypeAttrs};

fn world() -> (Fx, ItemKey) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let dk = fx.tr("DiscriminantKind", 1, Vec::new(), &["Discriminant"]);
    fx.lang(LangItem::DiscriminantKind, dk);
    let p0 = fx.param(0);
    let out = fx.alias(dk, "Discriminant", &[p0]);
    let r_p0 = fx.r(Mutbl::Not, p0);
    fx.func("discriminant_value", 1, &[r_p0], out, Vec::new());
    let (e, _) = fx.enum_("E", 0, &[("A", &[]), ("B", &[])]);
    let (r, _) = fx.enum_("R", 0, &[("A", &[])]);
    fx.set_attrs(r, TypeAttrs(0).with_discr(DiscrRepr::Uint(UintTy::U8)));
    let s = fx.strukt("S", 0, &[]);
    for (k, name) in [(e, "E0"), (r, "R0"), (s, "S0")] {
        let t = fx.adt(k, &[]);
        fx.konst(name, t);
    }
    let unit = fx.unit();
    let h = fx.func("h", 0, &[], unit, Vec::new());
    fx.finish();
    (fx, h)
}

/// `{ let d: want = discriminant_value(&value); }`: the codes of its diagnostics.
fn codes(value: &str, want: PNode) -> Vec<DiagCode> {
    let (fx, h) = world();
    let mut b = Bb::new();
    let want = b.ty(want);
    let d = b.bind("d");
    let f = b.path(&["discriminant_value"]);
    let v = b.path(&[value]);
    let rv = b.borrow(Mutbl::Not, v);
    let call = b.call(f, &[rv]);
    let s = b.let_(d, Some(want), call);
    let root = b.block(&[s], None);
    let c = check_body(&fx, &b, h, root, Plant::None, InferPlant::None);
    c.diags.iter().map(|d| d.code).collect()
}

/// A derived `eq` on an enum with the default representation: its discriminant is an `isize`.
#[test]
fn an_enum_s_discriminant_is_isize_by_default() {
    assert_eq!(codes("E0", PNode::Int(IntTy::Isize)), Vec::new());
    assert_eq!(codes("E0", PNode::Uint(UintTy::U8)), std::vec![DiagCode::Mismatch]);
}

/// `#[repr(u8)]`: the discriminant is that integer.
#[test]
fn a_repr_u8_enum_s_discriminant_is_u8() {
    assert_eq!(codes("R0", PNode::Uint(UintTy::U8)), Vec::new());
    assert_eq!(codes("R0", PNode::Int(IntTy::Isize)), std::vec![DiagCode::Mismatch]);
}

/// Any type that is not an enum: `u8`.
#[test]
fn a_struct_s_discriminant_is_u8() {
    assert_eq!(codes("S0", PNode::Uint(UintTy::U8)), Vec::new());
}
