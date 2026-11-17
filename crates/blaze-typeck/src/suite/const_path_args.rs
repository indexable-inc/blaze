//! A checked body's constant path records its generic arguments (`Checked::const_args`), so the
//! builder evaluates the constant at the instance's arguments: front2 const_qpath.rs's OWED
//! "the checker's record must hold the path's arguments" (walk list-AH class 12: core's
//! `size_of`/`align_of`, whose bodies read `<T as SizedTypeProperties>::SIZE`, left out as "the
//! trait's constant .. is read with no `Self`").
//!
//! ```text
//! trait P { const S: usize; }
//! impl P for u8 { const S: usize = 1; }
//! const K: usize = 3;
//! fn f<T: P>() -> usize { T::S }        // row [P0]: the trait's constant at `Self = T`
//! fn g() -> usize { <u8>::S }           // row [u8]
//! fn h() -> usize { K }                 // no row: a free constant has no generics
//! ```
//!
//! The Reference, "Associated Items", "Associated Constants": "An associated constant
//! definition defines a constant associated with a type"; its "Using default values" example: an
//! impl that writes no definition has the trait's default. So the value of `<T as P>::S` depends
//! on `T`: the path's `[Self, trait args..]` are part of what it names. The first two tests fail
//! if `Machine::record_const_args` (rules/lit_path.rs) records nothing.

use super::check_body;
use crate::columns::Checked;
use crate::fixture::{Bb, Fx, show};
use crate::ids::{ExprId, ItemKey, LangItem, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;
use crate::world::Def;

/// The world above: `[f, g, h]`.
fn world() -> (Fx, [ItemKey; 3]) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Default::default(), &[]);
    fx.lang(LangItem::Sized, sized);
    let u8_ = fx.u8();
    let usize_ = fx.ty(PNode::Uint(UintTy::Usize));
    let t = fx.param(0);
    fx.use_path("T", Def::TyParam(0));
    let p = fx.tr("P", 1, Default::default(), &[]);
    fx.trait_const(p, "S", usize_);
    let p_u8 = fx.imp("impl P for u8", 0, u8_, Some((p, &[])), &[], Default::default());
    fx.impl_const(p_u8, "S", usize_);
    fx.use_trait(p);
    fx.konst("K", usize_);
    let c_p = fx.bound(p, &[t]);
    let f = fx.func("f", 1, &[], usize_, std::vec![c_p]);
    let g = fx.func("g", 0, &[], usize_, Default::default());
    let h = fx.func("h", 0, &[], usize_, Default::default());
    fx.finish();
    (fx, [f, g, h])
}

/// Whether the row at `e` holds exactly `want`, each shown (`P0`: the fn's first type parameter).
fn holds(fx: &Fx, c: &Checked<'_>, e: ExprId, want: &[&str]) -> bool {
    let Some(l) = c.const_args_of(e) else { return false };
    let got = c.types.view().list(l).expect("the callee's arguments in the body's table");
    got.len() == want.len() && got.iter().zip(want.iter()).all(|(&t, w)| show(fx, c.types.view(), t) == *w)
}

/// "a trait constant's path records `[Self, ..]` (front2 const_qpath.rs OWED)": `T::S` in
/// `f<T: P>` holds `[P0]`.
///
/// Prints: `test suite::const_path_args::a_trait_constant_on_a_parameter_records_self ... ok`
#[test]
fn a_trait_constant_on_a_parameter_records_self() {
    let (fx, [f, _, _]) = world();
    let mut b = Bb::new();
    let e = b.path(&["T", "S"]);
    let root = b.block(&[], Some(e));
    let c = check_body(&fx, &b, f, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    assert!(holds(&fx, &c, e, &["P0"]), "{:?}", c.const_args);
}

/// `<u8>::S` in `g` holds `[u8]`: the ground `Self` the builder selects `impl P for u8` by.
///
/// Prints: `test suite::const_path_args::a_trait_constant_on_a_ground_type_records_it ... ok`
#[test]
fn a_trait_constant_on_a_ground_type_records_it() {
    let (fx, [_, g, _]) = world();
    let mut b = Bb::new();
    let u = b.ty(PNode::Uint(UintTy::U8));
    let e = b.qpath(u, &["S"]);
    let root = b.block(&[], Some(e));
    let c = check_body(&fx, &b, g, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    assert!(holds(&fx, &c, e, &["u8"]), "{:?}", c.const_args);
}

/// A constant PATTERN records its arguments too (`Checked::pat_const_args`): in `g`,
/// `match K { <u8>::S => .., x => .. }` holds `[u8]` at the pattern, and nothing at the
/// expression rows. The builder reads the constant's value at them (blaze-build
/// walk/matches/forms.rs `const_test`: tempo's `match y_parity.0 { U256::ZERO => .. }`, an
/// inherent impl's constant over the impl's generics). Fails if `record_const_args` records
/// expressions only (the old rule: "a pattern's constant has none").
///
/// Prints: `test suite::const_path_args::a_constant_pattern_records_its_arguments ... ok`
#[test]
fn a_constant_pattern_records_its_arguments() {
    let (fx, [_, g, _]) = world();
    let mut b = Bb::new();
    let u = b.ty(PNode::Uint(UintTy::U8));
    let k = b.path(&["K"]);
    let p = b.qpath_pat(u, &["S"]);
    let x = b.bind("x");
    let one = b.path(&["K"]);
    let other = b.local(x);
    let m = b.match_(k, &[(p, one), (x, other)]);
    let root = b.block(&[], Some(m));
    let c = check_body(&fx, &b, g, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    let l = c.pat_const_args_of(p).unwrap_or_else(|| panic!("no row at the pattern: {:?}", c.pat_defs));
    let got = c.types.view().list(l).expect("the pattern's arguments in the body's table");
    assert_eq!(got.len(), 1);
    assert_eq!(show(&fx, c.types.view(), got[0]), "u8");
    assert!(c.const_args.column::<0>().is_empty(), "{:?}", c.const_args);
}

/// A free constant `K` has no generics: no row, and the path is still `Def::Const` in `expr_defs`.
///
/// Prints: `test suite::const_path_args::a_free_constant_has_no_row ... ok`
#[test]
fn a_free_constant_has_no_row() {
    let (fx, [_, _, h]) = world();
    let mut b = Bb::new();
    let e = b.path(&["K"]);
    let root = b.block(&[], Some(e));
    let c = check_body(&fx, &b, h, root, Plant::None, InferPlant::None);
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    assert_eq!(c.const_args_of(e), None);
    assert!(c.expr_defs.column::<1>().iter().any(|d| matches!(d, Def::Const(_))), "{:?}", c.expr_defs);
}
