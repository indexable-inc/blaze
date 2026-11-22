//! A subpattern that peels references under an ADT field (`Form::PeelUnderAdtField` before
//! piece RB7; NEVER COMPILED when written, 2026-11-17). The Reference, "Binding modes": "If a
//! binding pattern does not explicitly have `ref`, `ref mut`, or `mut`, then it uses the default
//! binding mode": each reference a non-reference pattern meets is dereferenced, `&` making the
//! mode `ref`, `&mut` making a move mode `ref mut` (and leaving `ref` as it is). So a binding
//! below the peel is a reborrow `&(*field).sub`, and a `mut x` there (edition 2021: it resets
//! the mode to move) moves out of the deref: E0507.
//!
//! Each body is a small fn of the fixture world, type-checked by the real checker, lowered and
//! checked by blaze-flow. The field's declared type comes from [`Facts::field_lead`]; the
//! control of the first test removes it (rule 23) and the refusal comes back.
//!
//! The tuple variants stand for the brief's braced ones (`Cmd::Hello(who)` for
//! `Cmd::Hello { who }`): [`Bb`] builds no braced patterns, and both reach the same
//! `bind_fields` (they differ only in how a field's index is found, `Facts::field`).

use std::vec;
use std::vec::Vec;

use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ExprId, ItemKey, Mutbl, Name, PNode, PTy, PatId};

use crate::region_fixture::{Decls, FnHead, findings};
use crate::{AdtFacts, CalleeFacts, Facts, FieldLead, Form, Plant, Region};

/// The world's ADTs as the census reads them: each enum's variants with each field's lead;
/// every other ADT a plain struct with no `Copy` and no `Drop`. `leads` false: no field's
/// declared type is known (the control).
struct Peel {
    enums: Vec<(ItemKey, Vec<Vec<FieldLead>>)>,
    leads: bool,
}

impl Peel {
    fn variant(&self, adt: ItemKey, variant: u32) -> Option<&Vec<FieldLead>> {
        self.enums.iter().find(|(k, _)| *k == adt)?.1.get(variant as usize)
    }
}

impl Facts for Peel {
    // Every other fact not known here, written out: `Facts` has no default answer.
    fn regions(&self) -> &dyn crate::RegionFacts {
        &crate::NoRegions
    }
    fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
        None
    }
    fn adt_variances(&self, _: blaze_typeck::ItemKey) -> Option<&[crate::Variance]> {
        None
    }
    fn implied_bounds(&self) -> Option<(&[(u32, crate::Region)], &[(crate::Region, crate::Region)])> {
        None
    }
    fn constant_initializer(&self) -> bool {
        false
    }
    fn fn_binders(&self) -> bool {
        true
    }
    fn closure_bodies_checked(&self) -> bool {
        false
    }
    fn closure_requirements(&self, _: blaze_typeck::ExprId) -> &[crate::Requirement] {
        &[]
    }
    fn async_bodies_checked(&self) -> bool {
        false
    }
    fn is_future(&self, _: blaze_typeck::PTy) -> Option<bool> {
        None
    }
    fn shape(&self, _: blaze_typeck::ItemKey) -> Option<crate::Shape<'_>> {
        None
    }
    fn object_trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<bool> {
        None
    }
    fn call_bounds(&self, _: blaze_typeck::ItemKey) -> Option<crate::CallBounds<'_>> {
        None
    }
    fn trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<u32> {
        None
    }
    fn alias_lifetimes(&self, _: blaze_typeck::ItemKey, _: blaze_typeck::Name) -> Option<u32> {
        None
    }
    fn ties(&self, _: blaze_typeck::ItemKey) -> Option<crate::Ties<'_>> {
        None
    }
    fn field_regions(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<&[crate::FieldRegion]> {
        None
    }
    fn copy_args(&self, _: blaze_typeck::ItemKey) -> Option<&[bool]> {
        None
    }
    fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
        let is_enum = self.enums.iter().any(|(e, _)| *e == k);
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: true, lifetimes: 0, union: false, is_enum, is_box: false })
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
    }
    fn field(&self, _: ItemKey, _: u32, _: Name) -> Option<u32> {
        None
    }
    fn field_count(&self, adt: ItemKey, variant: u32) -> Option<u32> {
        self.variant(adt, variant).map(|v| v.len() as u32)
    }
    fn field_param(&self, adt: ItemKey, variant: u32, field: u32) -> Option<u32> {
        match self.field_lead(adt, variant, field)? {
            FieldLead::Param(p) => Some(p),
            _ => None,
        }
    }
    fn field_lead(&self, adt: ItemKey, variant: u32, field: u32) -> Option<FieldLead> {
        if !self.leads {
            return None;
        }
        self.variant(adt, variant)?.get(field as usize).copied()
    }
    fn param_copy(&self, _: u32) -> bool {
        false
    }
}

/// `struct S; enum Option<T> { None, Some(T) } enum E { V(S), Q } enum Wrapper { W(E) }`,
/// `fn g(_: &S)`, `fn h(_: S)`, `fn p(_: &S) -> bool`, `fn take(_: Wrapper)`, and one owner per
/// case.
struct World {
    fx: Fx,
    facts: Peel,
    /// Every fn's region head, laid in the checked record's own table.
    decls: Decls,
    /// `fn f(o: Option<&E>)`.
    opt_ref: ItemKey,
    /// `fn f(t: (&u8, &E))`.
    pair: ItemKey,
    /// `fn f(w: &Wrapper)`.
    wrap_ref: ItemKey,
    /// `fn f(o: &Option<&mut E>)`.
    mut_under_shared: ItemKey,
    /// `fn f(o: Option<&Option<&E>>)`.
    double: ItemKey,
    /// `fn f(w: Wrapper)`.
    wrap: ItemKey,
}

fn world() -> World {
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let p0 = fx.param(0);
    let (option, _) = fx.enum_("Option", 1, &[("None", &[]), ("Some", &[p0])]);
    let (e, _) = fx.enum_("E", 0, &[("V", &[st]), ("Q", &[])]);
    let et = fx.adt(e, &[]);
    let (wrapper, _) = fx.enum_("Wrapper", 0, &[("W", &[et])]);
    let wt = fx.adt(wrapper, &[]);
    let unit = fx.unit();
    let boolean = fx.ty(PNode::Bool);
    let rs = fx.r(Mutbl::Not, st);
    let g = fx.func("g", 0, &[rs], unit, vec![]);
    let h = fx.func("h", 0, &[st], unit, vec![]);
    let p = fx.func("p", 0, &[rs], boolean, vec![]);
    let take = fx.func("take", 0, &[wt], unit, vec![]);
    let re = fx.r(Mutbl::Not, et);
    let opt_re = fx.adt(option, &[re]);
    let opt_ref = fx.func("opt_ref", 0, &[opt_re], unit, vec![]);
    let u8_ = fx.u8();
    let ru8 = fx.r(Mutbl::Not, u8_);
    let pair_ty = fx.tuple(&[ru8, re]);
    let pair = fx.func("pair", 0, &[pair_ty], unit, vec![]);
    let rw = fx.r(Mutbl::Not, wt);
    let wrap_ref = fx.func("wrap_ref", 0, &[rw], unit, vec![]);
    let rme = fx.r(Mutbl::Mut, et);
    let opt_rme = fx.adt(option, &[rme]);
    let r_opt_rme = fx.r(Mutbl::Not, opt_rme);
    let mut_under_shared = fx.func("mut_under_shared", 0, &[r_opt_rme], unit, vec![]);
    let r_opt_re = fx.r(Mutbl::Not, opt_re);
    let opt_r_opt_re = fx.adt(option, &[r_opt_re]);
    let double = fx.func("double", 0, &[opt_r_opt_re], unit, vec![]);
    let wrap = fx.func("wrap", 0, &[wt], unit, vec![]);
    fx.finish();
    // Every head as declared above: each reference its own elided, late-bound lifetime,
    // numbered in order of appearance; no output borrows; no ADT declares a lifetime.
    let (p0, p1) = (Region::Param(0), Region::Param(1));
    let mut decls = Decls::new();
    let mut head = |item: ItemKey, inputs: &[PTy], output: PTy, slots: &[Region]| {
        let lifetimes = slots.len() as u32;
        let late: vec::Vec<u32> = (0..lifetimes).collect();
        decls.func(item, FnHead { lifetimes, late: &late, outlives: &[], inputs, output, slots });
    };
    // `fn g(_: &S)`, `fn h(_: S)`, `fn p(_: &S) -> bool`, `fn take(_: Wrapper)`.
    head(g, &[rs], unit, &[p0]);
    head(h, &[st], unit, &[]);
    head(p, &[rs], boolean, &[p0]);
    head(take, &[wt], unit, &[]);
    // `fn opt_ref(o: Option<&E>)`, `fn pair(t: (&u8, &E))`, `fn wrap_ref(w: &Wrapper)`.
    head(opt_ref, &[opt_re], unit, &[p0]);
    head(pair, &[pair_ty], unit, &[p0, p1]);
    head(wrap_ref, &[rw], unit, &[p0]);
    // `fn mut_under_shared(o: &Option<&mut E>)`, `fn double(o: Option<&Option<&E>>)`,
    // `fn wrap(w: Wrapper)`.
    head(mut_under_shared, &[r_opt_rme], unit, &[p0, p1]);
    head(double, &[opt_r_opt_re], unit, &[p0, p1]);
    head(wrap, &[wt], unit, &[]);
    let facts = Peel {
        enums: vec![
            (option, vec![vec![], vec![FieldLead::Param(0)]]),
            (e, vec![vec![FieldLead::Plain], vec![]]),
            (wrapper, vec![vec![FieldLead::Plain]]),
        ],
        leads: true,
    };
    World { fx, facts, decls, opt_ref, pair, wrap_ref, mut_under_shared, double, wrap }
}

/// `name(local)`.
fn call1(b: &mut Bb, name: &str, arg: PatId) -> ExprId {
    let f = b.path(&[name]);
    let a = b.local(arg);
    b.call(f, &[a])
}

/// `{ match <param> { pat => g(y), _ => {} } }` over the owner's one parameter.
fn match_g(b: &mut Bb, scrutinee: PatId, pat: PatId, y: PatId) -> ExprId {
    let s = b.local(scrutinee);
    let body = call1(b, "g", y);
    let w = b.wild();
    let none = b.block(&[], None);
    let m = b.match_(s, &[(pat, body), (w, none)]);
    b.block(&[], Some(m))
}

fn some(b: &mut Bb, inner: PatId) -> PatId {
    b.variant_pat(&["Option", "Some"], &[inner])
}

fn ev(b: &mut Bb, inner: PatId) -> PatId {
    b.variant_pat(&["E", "V"], &[inner])
}

/// The findings of `root` under `facts`, printed; a refusal fails the test by name.
fn lowered(w: &World, b: &Bb, owner: ItemKey, root: ExprId, facts: &Peel, plant: Plant, case: &str) -> Vec<std::string::String> {
    match findings(&w.fx, b, owner, root, facts, &w.decls, plant) {
        Ok(got) => {
            std::println!("{case}: {got:?}");
            got
        }
        Err(r) => panic!("{case}: refused {r:?}"),
    }
}

/// clap's `Cmd` match: `fn f(o: Option<&E>) { match o { Some(E::V(who)) => g(who), _ => {} } }`.
/// `Some`'s field is `T = &E`; `E::V(..)` peels it (mode `ref`), so `who: &S` is
/// `&(*(o as Some).0 as V).0`: no finding. Control (rule 23): with no field's declared type
/// known, the peel's type cannot be rebuilt and the lowering refuses `PeelUnderAdtField` again.
#[test]
fn a_variant_under_option_of_a_reference_peels() {
    let w = world();
    let mut b = Bb::new();
    let o = b.param("o");
    let who = b.bind("who");
    let inner = ev(&mut b, who);
    let pat = some(&mut b, inner);
    let root = match_g(&mut b, o, pat, who);
    let got = lowered(&w, &b, w.opt_ref, root, &w.facts, Plant::None, "option of a reference");
    assert!(got.is_empty(), "{got:?}");
    let blind = Peel { enums: w.facts.enums.clone(), leads: false };
    let control = findings(&w.fx, &b, w.opt_ref, root, &blind, &w.decls, Plant::None);
    std::println!("option of a reference, no leads: {control:?}");
    assert!(matches!(control, Err(ref r) if r.form == Form::PeelUnderAdtField), "{control:?}");
}

/// The derive's `(a, Self::V(x))` against `(&u8, &Self)`: `fn f(t: (&u8, &E)) { match t { (a,
/// E::V(x)) => g(x), _ => {} } }`. The tuple's second element peels `&E`: no finding.
#[test]
fn a_variant_in_a_tuple_of_references_peels() {
    let w = world();
    let mut b = Bb::new();
    let t = b.param("t");
    let a = b.bind("a");
    let x = b.bind("x");
    let inner = ev(&mut b, x);
    let pat = b.tuple_pat(&[a, inner]);
    let root = match_g(&mut b, t, pat, x);
    let got = lowered(&w, &b, w.pair, root, &w.facts, Plant::None, "tuple of references");
    assert!(got.is_empty(), "{got:?}");
}

/// `Wrapper { inner: Variant(y) }` as `Wrapper::W(E::V(y))` against `&Wrapper`: the field's
/// declared type `E` holds no reference, so its subpattern peels nothing below the top's `&`
/// (`FieldLead::Plain`); `y: &S` through the top's peel. No finding.
#[test]
fn a_variant_inside_a_wrapper_field() {
    let w = world();
    let mut b = Bb::new();
    let r = b.param("r");
    let y = b.bind("y");
    let inner = ev(&mut b, y);
    let pat = b.variant_pat(&["Wrapper", "W"], &[inner]);
    let root = match_g(&mut b, r, pat, y);
    let got = lowered(&w, &b, w.wrap_ref, root, &w.facts, Plant::None, "wrapper field");
    assert!(got.is_empty(), "{got:?}");
}

/// `&mut` under a shared peel: `fn f(o: &Option<&mut E>) { match o { Some(E::V(y)) => g(y), _
/// => {} } }`. The top `&` makes the mode `ref`; the field's `&mut E` leaves it `ref` (the
/// Reference: "`ref` stays"), so `y: &S`, a shared reborrow through `&mut`. No finding.
#[test]
fn a_mutable_reference_under_a_shared_peel_binds_shared() {
    let w = world();
    let mut b = Bb::new();
    let o = b.param("o");
    let y = b.bind("y");
    let inner = ev(&mut b, y);
    let pat = some(&mut b, inner);
    let root = match_g(&mut b, o, pat, y);
    let got = lowered(&w, &b, w.mut_under_shared, root, &w.facts, Plant::None, "mut under shared");
    assert!(got.is_empty(), "{got:?}");
}

/// A nested double peel: `fn f(o: Option<&Option<&E>>) { match o { Some(Some(E::V(y))) =>
/// g(y), _ => {} } }`: each `Some`'s field is a `T` that is a reference, each peeled in turn.
#[test]
fn a_nested_double_peel() {
    let w = world();
    let mut b = Bb::new();
    let o = b.param("o");
    let y = b.bind("y");
    let v = ev(&mut b, y);
    let inner = some(&mut b, v);
    let pat = some(&mut b, inner);
    let root = match_g(&mut b, o, pat, y);
    let got = lowered(&w, &b, w.double, root, &w.facts, Plant::None, "double peel");
    assert!(got.is_empty(), "{got:?}");
}

/// A guard that uses the bound reference: `match o { Some(E::V(y)) if p(y) => g(y), _ => {} }`.
/// `y` is bound by reference, so the guard moves nothing (the guard's refusal is for a
/// by-value binding of a non-`Copy` type). No finding.
#[test]
fn a_guard_reads_the_peeled_binding() {
    let w = world();
    let mut b = Bb::new();
    let o = b.param("o");
    let y = b.bind("y");
    let inner = ev(&mut b, y);
    let pat = some(&mut b, inner);
    let s = b.local(o);
    let guard = call1(&mut b, "p", y);
    let body = call1(&mut b, "g", y);
    let wild = b.wild();
    let none = b.block(&[], None);
    let m = b.match_guarded(s, &[(pat, Some(guard), body), (wild, None, none)]);
    let root = b.block(&[], Some(m));
    let got = lowered(&w, &b, w.opt_ref, root, &w.facts, Plant::None, "guard");
    assert!(got.is_empty(), "{got:?}");
}

/// E0507, "cannot move out of a shared reference": `match o { Some(E::V(mut y)) => h(y), _ =>
/// {} }` in edition 2021, where `mut` resets the default mode to move, so `y: S` moves out of
/// `*(o as Some).0`. Must be an ERROR (blaze-flow `MoveOut(Deref)`). Control (rule 23): every
/// read a copy (`Plant::AllCopy`) hides it.
#[test]
fn a_by_value_binding_under_the_peel_is_e0507() {
    let w = world();
    let mut b = Bb::new();
    let o = b.param("o");
    let y = b.bind_mut("y");
    let inner = ev(&mut b, y);
    let pat = some(&mut b, inner);
    let s = b.local(o);
    let body = call1(&mut b, "h", y);
    let wild = b.wild();
    let none = b.block(&[], None);
    let m = b.match_(s, &[(pat, body), (wild, none)]);
    let root = b.block(&[], Some(m));
    let got = lowered(&w, &b, w.opt_ref, root, &w.facts, Plant::None, "E0507");
    assert!(got.iter().any(|l| l.contains("MoveOut(Deref)")), "{got:?}");
    let planted = lowered(&w, &b, w.opt_ref, root, &w.facts, Plant::AllCopy, "E0507, planted AllCopy");
    assert!(!planted.iter().any(|l| l.contains("MoveOut")), "the control hides it: {planted:?}");
}

/// The peeled binding's loan is live where it is used: `fn f(w: Wrapper) { let r = &w; match r
/// { Wrapper::W(E::V(y)) => { take(w); g(y) } _ => {} } }` moves `w` while `y`, a reborrow
/// through `*r`, is still to be used: the loan of `w` is invalidated (E0505). Control (rule
/// 23): an assignment that relates no origins (`Plant::NoRelate`) leaves the loan in `&w`'s
/// temporary, so nothing reaches `y`.
#[test]
fn a_peeled_binding_keeps_its_loan_live() {
    let w = world();
    let mut b = Bb::new();
    let wp = b.param("w");
    let r = b.bind("r");
    let lw = b.local(wp);
    let bor = b.borrow(Mutbl::Not, lw);
    let st1 = b.let_(r, None, bor);
    let y = b.bind("y");
    let inner = ev(&mut b, y);
    let pat = b.variant_pat(&["Wrapper", "W"], &[inner]);
    let take = call1(&mut b, "take", wp);
    let take = b.semi(take);
    let use_y = call1(&mut b, "g", y);
    let arm = b.block(&[take], Some(use_y));
    let lr = b.local(r);
    let wild = b.wild();
    let none = b.block(&[], None);
    let m = b.match_(lr, &[(pat, arm), (wild, none)]);
    let root = b.block(&[st1], Some(m));
    let got = lowered(&w, &b, w.wrap, root, &w.facts, Plant::None, "loan through the peel");
    assert!(got.iter().any(|l| l.starts_with("borrow Invalidated")), "{got:?}");
    let planted = lowered(&w, &b, w.wrap, root, &w.facts, Plant::NoRelate, "loan through the peel, planted NoRelate");
    assert!(!planted.iter().any(|l| l.starts_with("borrow")), "the control hides it: {planted:?}");
}
