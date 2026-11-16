//! `Self` in an impl of a type that is no struct, enum or union, and value paths through an
//! associated type (rules/self_path.rs). tempo census 13: 1,043 of 1,052 "diag E0425" bodies,
//! nearly all in tempo_precompiles (`Self::from_word(..)` and `Self::BYTES` in `impl<T:
//! Packable> Storable for T`, `Self::Handler::new(..)` in `impl StorableType for [T; N]`,
//! `Self::Error::out_of_gas()` in a trait's provided method, serde's `D::Error::custom(..)`).
//!
//! The Reference, "Paths", "`Self`" (`paths.qualifiers.type-self.impl`): "In an
//! implementation, it refers to the type being implemented." "Trait and lifetime bounds"
//! (`bound.satisfaction`), given `Ty: Trait`: "In the body of a generic function, methods from
//! `Trait` can be called on `Ty` values. Likewise associated constants on the `Trait` can be
//! used. Associated types from `Trait` can be used." and "`trait A { type B: Copy; }` is
//! equivalent to `trait A where Self::B: Copy { type B; }`". "Associated Items", "Associated
//! Types": "Associated types are type aliases associated with another type."
//! The refusals are the compiler's error index's: E0220 "The associated type used was not
//! defined in the trait.", E0221 "An attempt was made to retrieve an associated type, but the
//! type was ambiguous." (two bounds declare it), E0223 (the same sentence for a prefix that is
//! no parameter and no `Self`: "we cannot simply use `Struct::X`"), E0599 for an associated
//! function no candidate has.
//!
//! The world answers `Self` with `Def::SelfTy` consuming one segment, as blaze-ladder
//! front2/world_paths.rs does for every self type that is not an ADT; the real-front-end form
//! is blaze-ladder tests/front2_self_ty.rs.
//!
//! Each clean test fails without `Def::SelfTy`'s arm and the two-segment step of
//! `Machine::value_path` (rules/lit_path.rs): the path is then E0425 at expression 0.

use std::format;
use std::string::String;
use std::vec::Vec;

use super::{Want, check_body, differences};
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem, Mutbl, Name};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;
use crate::world::Def;

/// The impl the body `h` is a member of.
#[derive(Clone, Copy, Debug)]
enum Imp {
    /// `impl<T: Tr> Other for T`: `Self` is the parameter.
    Blanket,
    /// `impl Tr for [u8; 4]`.
    Array,
    /// `impl Tr for u32`.
    Prim,
    /// `impl<A, B> Tr for (A, B)`.
    Tuple,
    /// `impl<T> Tr for &T`.
    Ref,
}

/// ```text
/// trait Mk { fn g() -> u16; }      impl Mk for u16 {}             // `Mk` is in scope
/// trait Tr { type Out: Mk; const K: u8; fn f() -> u8; fn h(); }   // `Tr` is NOT in scope
/// trait Other { type Out2: Mk; fn h(); }                          // nor is `Other`
/// impl<T: Tr> Other for T { type Out2 = u16; fn h() { .. } }      // Imp::Blanket
/// impl Tr for [u8; 4]     { type Out = u16;  fn h() { .. } }      // Imp::Array
/// impl Tr for u32 { .. }   impl<A, B> Tr for (A, B) { .. }   impl<T> Tr for &T { .. }
/// ```
///
/// The world, the body's owner `h` and the name of the impl's associated type. In the blanket
/// impl `f` and `K` come from the bound `T: Tr` (`Self` IS `T`); in the others from the impl's
/// own trait, which needs no import.
fn world(which: Imp) -> (Fx, ItemKey, &'static str) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (u8_, u16_, unit) = (fx.u8(), fx.u16(), fx.unit());
    let (p0, p1) = (fx.param(0), fx.param(1));
    fx.use_path("Self", Def::SelfTy);
    let mk = fx.tr("Mk", 1, Vec::new(), &[]);
    fx.trait_fn(mk, "g", false, &[], u16_);
    fx.imp("impl Mk for u16", 0, u16_, Some((mk, &[])), &[], Vec::new());
    fx.use_trait(mk);
    let tr = fx.tr("Tr", 1, Vec::new(), &["Out"]);
    let out = fx.alias(tr, "Out", &[p0]);
    let (out_mk, out_sized) = (fx.bound(mk, &[out]), fx.bound(sized, &[out]));
    fx.set_assoc_bounds(tr, "Out", std::vec![out_mk, out_sized]);
    fx.trait_const(tr, "K", u8_);
    fx.trait_fn(tr, "f", false, &[], u8_);
    let (imp, assoc) = match which {
        Imp::Blanket => {
            let other = fx.tr("Other", 1, Vec::new(), &["Out2"]);
            let t_tr = fx.bound(tr, &[p0]);
            (fx.imp("impl<T: Tr> Other for T", 1, p0, Some((other, &[])), &[("Out2", u16_)], std::vec![t_tr]), "Out2")
        }
        Imp::Array => {
            let a = fx.array(u8_, 4);
            (fx.imp("impl Tr for [u8; 4]", 0, a, Some((tr, &[])), &[("Out", u16_)], Vec::new()), "Out")
        }
        Imp::Prim => {
            let u = fx.u32();
            (fx.imp("impl Tr for u32", 0, u, Some((tr, &[])), &[("Out", u16_)], Vec::new()), "Out")
        }
        Imp::Tuple => {
            let t = fx.tuple(&[p0, p1]);
            (fx.imp("impl<A, B> Tr for (A, B)", 2, t, Some((tr, &[])), &[("Out", u16_)], Vec::new()), "Out")
        }
        Imp::Ref => {
            let r = fx.r(Mutbl::Not, p0);
            (fx.imp("impl<T> Tr for &T", 1, r, Some((tr, &[])), &[("Out", u16_)], Vec::new()), "Out")
        }
    };
    let h = fx.impl_fn(imp, "h", false, &[], unit);
    fx.finish();
    (fx, h, assoc)
}

/// Every difference but the expressions a test writes no expectation for.
fn written(lines: Vec<String>) -> Vec<String> {
    lines.into_iter().filter(|l| !l.contains("no expectation written")).collect()
}

/// The body `{ segs(); }` (`call`) or `{ segs; }` of `owner`: its differences from "the value
/// is a `ty`, and there is no diagnostic".
fn clean(fx: &Fx, owner: ItemKey, segs: &[&str], call: bool, ty: &str) -> Vec<String> {
    let mut b = Bb::new();
    let p = b.path(segs);
    let v = if call { b.call(p, &[]) } else { p };
    let s = b.semi(v);
    let root = b.block(&[s], None);
    let c = check_body(fx, &b, owner, root, Plant::None, InferPlant::None);
    let d = written(differences(fx, &b, &c, &Want { exprs: &[(v, ty)], pats: &[], diags: &[] }));
    d.into_iter().map(|l| format!("`{}`: {l}", segs.join("::"))).collect()
}

/// The body `{ segs(); }` of `owner`: its differences from "one diagnostic `code` at the path,
/// naming `name`".
fn refused(fx: &Fx, owner: ItemKey, segs: &[&str], code: DiagCode, name: &str) -> Vec<String> {
    let mut b = Bb::new();
    let p = b.path(segs);
    let v = b.call(p, &[]);
    let s = b.semi(v);
    let root = b.block(&[s], None);
    let c = check_body(fx, &b, owner, root, Plant::None, InferPlant::None);
    let mut d = written(differences(fx, &b, &c, &Want { exprs: &[], pats: &[], diags: &[(code, ExprId(0))] }));
    if !c.diags.iter().any(|x| x.code == code && x.name == Some(Name::of(name))) {
        let got: Vec<String> = c.diags.iter().map(|x| format!("{:?} naming {:?}", x.code, x.name)).collect();
        d.push(format!("the diagnostic names `{name}`: got [{}]", got.join(", ")));
    }
    d.into_iter().map(|l| format!("`{}`: {l}", segs.join("::"))).collect()
}

/// `Self::f()`, `Self::K` and `Self::Out::g()` in the impl `which`: each clean, at its type.
fn self_paths_in(which: Imp) {
    let (fx, h, assoc) = world(which);
    let mut d = clean(&fx, h, &["Self", "f"], true, "u8");
    d.extend(clean(&fx, h, &["Self", "K"], false, "u8"));
    d.extend(clean(&fx, h, &["Self", assoc, "g"], true, "u16"));
    assert!(d.is_empty(), "{which:?}\n{}", d.join("\n"));
}

/// tempo's `impl<T: Packable> Storable for T { .. Self::from_word(word) .. Self::BYTES .. }`:
/// `Self` is `T`, so `f` and `K` are found through `T`'s bound; `Self::Out2` is the impl's own
/// trait's associated type.
#[test]
fn self_in_a_blanket_impl_is_the_parameter() {
    self_paths_in(Imp::Blanket);
}

/// tempo's `impl StorableType for [T; N] { .. Self::Handler::new(..) .. }`.
#[test]
fn self_in_an_impl_for_an_array() {
    self_paths_in(Imp::Array);
}

#[test]
fn self_in_an_impl_for_a_primitive() {
    self_paths_in(Imp::Prim);
}

#[test]
fn self_in_an_impl_for_a_tuple() {
    self_paths_in(Imp::Tuple);
}

#[test]
fn self_in_an_impl_for_a_reference() {
    self_paths_in(Imp::Ref);
}

/// `Self::missing()` stays an error naming `missing`: E0599 (no candidate has it), as for any
/// type; and bare `Self` is no value there (E0423: only a tuple or unit struct's `Self` "also
/// refers to the constructor in the value namespace").
#[test]
fn a_missing_item_through_self_is_refused_by_name() {
    let (fx, h, _) = world(Imp::Array);
    let mut d = refused(&fx, h, &["Self", "missing"], DiagCode::NoMethod, "missing");
    d.extend(refused(&fx, h, &["Self"], DiagCode::NotAValue, "Self"));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// ```text
/// trait Mk { const C: u16; fn g() -> u16; }                      // in scope
/// trait De { type Error: Mk; }   trait De2 { type Error; }   trait Plain { type Error; }
/// struct S {}
/// fn one<D: De>()         { D::Error::g(); <D as De>::Error::C; }    // serde's `D::Error::custom(..)`
/// fn two<D: De + De2>()   { D::Error::g(); }                         // E0221
/// fn plain<D: Plain>()    { D::Error::g(); }                         // E0599
/// ```
///
/// The world and the owners `[one, two, plain]`.
fn generic_world() -> (Fx, [ItemKey; 3]) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (u16_, unit) = (fx.u16(), fx.unit());
    let p0 = fx.param(0);
    fx.use_path("D", Def::TyParam(0));
    fx.strukt("S", 0, &[]);
    let mk = fx.tr("Mk", 1, Vec::new(), &[]);
    fx.trait_fn(mk, "g", false, &[], u16_);
    fx.trait_const(mk, "C", u16_);
    fx.use_trait(mk);
    let de = fx.tr("De", 1, Vec::new(), &["Error"]);
    let err = fx.alias(de, "Error", &[p0]);
    let (err_mk, err_sized) = (fx.bound(mk, &[err]), fx.bound(sized, &[err]));
    fx.set_assoc_bounds(de, "Error", std::vec![err_mk, err_sized]);
    let de2 = fx.tr("De2", 1, Vec::new(), &["Error"]);
    let plain_tr = fx.tr("Plain", 1, Vec::new(), &["Error"]);
    let d_de = fx.bound(de, &[p0]);
    let one = fx.func("one", 1, &[], unit, std::vec![d_de]);
    let (d_de, d_de2) = (fx.bound(de, &[p0]), fx.bound(de2, &[p0]));
    let two = fx.func("two", 1, &[], unit, std::vec![d_de, d_de2]);
    let d_plain = fx.bound(plain_tr, &[p0]);
    let plain = fx.func("plain", 1, &[], unit, std::vec![d_plain]);
    fx.finish();
    (fx, [one, two, plain])
}

/// `D::Error::g()` with `D: De` and `type Error: Mk`: `g` of `<D as De>::Error` through the
/// declaration's bound.
#[test]
fn a_function_through_a_parameters_associated_type() {
    let (fx, [one, _, _]) = generic_world();
    let d = clean(&fx, one, &["D", "Error", "g"], true, "u16");
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// `<D as De>::Error::C`: the qualified form of the same path (the Reference, "Qualified
/// paths"), a constant of the associated type. It was clean before this rule
/// (`Machine::anchored_value`) and is here so the two forms are stated side by side.
#[test]
fn a_constant_through_a_qualified_associated_type() {
    let (fx, [one, _, _]) = generic_world();
    let mut b = Bb::new();
    let d_ty = b.ty(PNode::Param(0));
    let p = b.qpath_as(d_ty, "De", &["Error", "C"]);
    let s = b.semi(p);
    let root = b.block(&[s], None);
    let c = check_body(&fx, &b, one, root, Plant::None, InferPlant::None);
    let d = written(differences(&fx, &b, &c, &Want { exprs: &[(p, "u16")], pats: &[], diags: &[] }));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// `D::Error::nope()`: `Error` is found and no bound of it has `nope`: E0599 naming `nope`.
/// `D::Error::g()` with `D: Plain`, whose `Error` declares no bound: E0599 naming `g` (`Mk` is
/// in scope and does not hold for it).
#[test]
fn an_item_no_bound_of_the_associated_type_has_is_refused() {
    let (fx, [one, _, plain]) = generic_world();
    let mut d = refused(&fx, one, &["D", "Error", "nope"], DiagCode::NoMethod, "nope");
    d.extend(refused(&fx, plain, &["D", "Error", "g"], DiagCode::NoMethod, "g"));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// `D::Error` with `D: De + De2`, both declaring `Error`: E0221 naming `Error`, whatever
/// follows it.
#[test]
fn an_associated_type_two_bounds_declare_is_ambiguous() {
    let (fx, [_, two, _]) = generic_world();
    let d = refused(&fx, two, &["D", "Error", "g"], DiagCode::AmbiguousAssocTyInBounds, "Error");
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// `D::Nope::g()`: no bound of `D` declares `Nope`: E0220 naming `Nope`. `S::Error::g()` for
/// a struct `S`: a type that is no parameter and no `Self` has no bounds to look in: E0223
/// naming `Error`.
#[test]
fn an_associated_type_no_bound_declares_is_refused() {
    let (fx, [one, _, _]) = generic_world();
    let mut d = refused(&fx, one, &["D", "Nope", "g"], DiagCode::NoAssocTy, "Nope");
    d.extend(refused(&fx, one, &["S", "Error", "g"], DiagCode::AmbiguousAssocTy, "Error"));
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}
