//! Variance (piece B13; NEVER COMPILED when written, 2026-11-18): a value related to another of
//! its type position by position at each position's variance (variance.rs; the Reference,
//! "Subtyping and Variance"). Each row of the table with its positive (rustc accepts), its
//! negative (rustc rejects: "lifetime may not live long enough", here `UnprovenOutlives`), and
//! THE CONTROL (rule 23): `Plant::NoVariance` (every position below the top reference related
//! both ways, the form before this piece) finds the positives again, as before.
//!
//! Each body is `fn f<'a, 'b: 'a>(x: T) -> U { x }` of the fixture world, its head the region
//! record the head lowering files for it (D148, made by hand here: `'a` is `Param(0)`, `'b`
//! `Param(1)`, both early-bound as both are in the clause `'b: 'a`, which the record holds as
//! `(Param(1), Param(0))`), declared with the shared fixture (region_fixture.rs [`Decls`], laid
//! in the checked record's own table), type checked by the real checker and lowered
//! (region_fixture.rs `findings`).
//!
//! NOT HERE: the table's fn-pointer row (`fn(&'short T)` to `fn(&'long T)`): a fn pointer's
//! lifetimes have no position in the lowering (lib.rs `npos`), so neither direction relates
//! anything there yet (variance.rs, "WHAT IS NOT HERE").

use blaze_typeck::fixture::Bb;
use blaze_typeck::{ExprId, ItemKey, Mutbl, Name, PTy};

use crate::callee_value_tests::no_clauses;
use crate::region_fixture::{Decls, FnHead, findings};
use crate::{AdtFacts, CalleeFacts, Facts, Plant, Region, Variance};

/// The fixtures' facts: one ADT `W<'w>` (`life`) whose variance in `'w` is `var` (`None`: not
/// known), every other ADT with no lifetime (the heads' [`Decls::adt`] says the same).
struct Var {
    life: ItemKey,
    var: Option<&'static [Variance]>,
}

impl Facts for Var {
    // Every other fact not known here, written out: `Facts` has no default answer.
    fn regions(&self) -> &dyn crate::RegionFacts {
        &crate::NoRegions
    }
    fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
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
    fn field_param(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<u32> {
        None
    }
    fn field_lead(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<crate::FieldLead> {
        None
    }
    fn field_regions(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<&[crate::FieldRegion]> {
        None
    }
    fn copy_args(&self, _: blaze_typeck::ItemKey) -> Option<&[bool]> {
        None
    }
    fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
        // As tests.rs's `Plain`, `W` with its one lifetime.
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: true, lifetimes: u32::from(k == self.life), union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
    }
    fn closure_bodies_checked(&self) -> bool {
        true
    }
    fn adt_variances(&self, adt: ItemKey) -> Option<&[Variance]> {
        if adt == self.life { self.var } else { None }
    }
    fn field(&self, _: ItemKey, _: u32, _: Name) -> Option<u32> {
        None
    }
    fn field_count(&self, _: ItemKey, _: u32) -> Option<u32> {
        None
    }
    fn param_copy(&self, _: u32) -> bool {
        false
    }
}

/// The types a case is written over.
#[derive(Clone, Copy)]
enum Shape {
    /// `&&S`.
    RefRef,
    /// `&mut &S`.
    MutRef,
    /// `(&S, &S)`.
    Pair,
    /// `W<'_>`, the one-lifetime ADT.
    W,
}

/// `'a`, `'b` of every owner.
const A: Region = Region::Param(0);
const B: Region = Region::Param(1);

/// The findings of `fn f<'a, 'b: 'a>(x: T) -> U { x }` (`written`, printed) whose erased type
/// is `shape` and whose column is `input` then `output`, `W`'s variance `var`, under `plant`:
/// `Ok(lines)` or the refusal's words.
fn case(written: &str, input: &[Region], output: &[Region], shape: Shape, var: Option<&'static [Variance]>, plant: Plant) -> Result<std::vec::Vec<std::string::String>, crate::Refusal> {
    let mut owner: Option<(ItemKey, ItemKey, PTy)> = None;
    let w = crate::callee_value_tests::world_with(|fx, _st, rs, _unit| {
        let wk = fx.strukt("W", 0, &[]);
        let t = match shape {
            Shape::RefRef => fx.r(Mutbl::Not, rs),
            Shape::MutRef => fx.r(Mutbl::Mut, rs),
            Shape::Pair => fx.tuple(&[rs, rs]),
            Shape::W => fx.adt(wk, &[]),
        };
        owner = Some((fx.func("f", 0, &[t], t, no_clauses()), wk, t));
    });
    let (f, wk, t) = owner.expect("declared");
    // `fn f<'a, 'b: 'a>(x: t) -> t`: two early lifetimes (both in the clause `'b: 'a`), the
    // column `input` then `output` over `fn(t) -> t`.
    let mut slots = std::vec::Vec::new();
    slots.extend_from_slice(input);
    slots.extend_from_slice(output);
    let mut decls = Decls::new();
    decls.adt(wk, 1);
    decls.func(f, FnHead { lifetimes: 2, late: &[], outlives: &[(B, A)], inputs: &[t], output: t, slots: &slots });
    let mut b = Bb::new();
    let px = b.param("x");
    let lx: ExprId = b.local(px);
    let root = b.block(&[], Some(lx));
    let facts = Var { life: wk, var };
    std::println!("case `{written}`: {input:?} -> {output:?}");
    findings(&w.fx, &b, f, root, &facts, &decls, plant)
}

fn found(got: &Result<std::vec::Vec<std::string::String>, crate::Refusal>) -> bool {
    got.as_ref().is_ok_and(|l| l.iter().any(|l| l.starts_with("borrow UnprovenOutlives")))
}

fn clean(got: &Result<std::vec::Vec<std::string::String>, crate::Refusal>) -> bool {
    got.as_ref().is_ok_and(|l| l.is_empty())
}

const CO: [Variance; 1] = [Variance::Co];
const IN: [Variance; 1] = [Variance::In];

/// `&'a T` is covariant in `T`: `&'a &'long S` is a `&'a &'short S`; the reverse is found.
/// The control finds the positive (`'a: 'b`, the referent's region tied both ways).
/// Prints: `shared referent: Ok([])`, `shared referent reversed: Ok(["borrow UnprovenOutlives ..`
/// and `shared referent, planted NoVariance: Ok(["borrow UnprovenOutlives ..`.
#[test]
fn a_shared_referent_is_covariant() {
    let ok = ("fn f<'a, 'b: 'a>(x: &'a &'b S) -> &'a &'a S { x }", &[A, B][..], &[A, A][..]);
    let got = case(ok.0, ok.1, ok.2, Shape::RefRef, None, Plant::None);
    std::println!("shared referent: {got:?}");
    assert!(clean(&got), "{got:?}");
    let got = case("fn f<'a, 'b: 'a>(x: &'a &'a S) -> &'a &'b S { x }", &[A, A], &[A, B], Shape::RefRef, None, Plant::None);
    std::println!("shared referent reversed: {got:?}");
    assert!(found(&got), "{got:?}");
    let got = case(ok.0, ok.1, ok.2, Shape::RefRef, None, Plant::NoVariance);
    std::println!("shared referent, planted NoVariance: {got:?}");
    assert!(found(&got), "{got:?}");
}

/// `&'a mut T` is invariant in `T`: `&mut &'long S` to `&mut &'short S` is the classic
/// unsoundness (a `&'short S` could be stored where `'long` is assumed) and MUST be found; the
/// same type to itself is clean.
/// Prints: `mut referent shortened: Ok(["borrow UnprovenOutlives ..` and `mut referent same: Ok([])`.
#[test]
fn a_mutable_referent_is_invariant() {
    let got = case("fn f<'a, 'b: 'a>(x: &'a mut &'b S) -> &'a mut &'a S { x }", &[A, B], &[A, A], Shape::MutRef, None, Plant::None);
    std::println!("mut referent shortened: {got:?}");
    assert!(found(&got), "{got:?}");
    let got = case("fn f<'a, 'b: 'a>(x: &'a mut &'b S) -> &'a mut &'b S { x }", &[A, B], &[A, B], Shape::MutRef, None, Plant::None);
    std::println!("mut referent same: {got:?}");
    assert!(clean(&got), "{got:?}");
}

/// A tuple is covariant in its elements (B7's finding: `(&'x S, D::Error)` returned from a
/// callee's `(&'de S, ..)` with `'de: 'x` was a false `'x: 'de`). The reverse is found; the
/// control finds the positive.
/// Prints: `tuple: Ok([])`, `tuple reversed: Ok(["borrow UnprovenOutlives ..` and
/// `tuple, planted NoVariance: Ok(["borrow UnprovenOutlives ..`.
#[test]
fn a_tuple_is_covariant() {
    let ok = ("fn f<'a, 'b: 'a>(x: (&'b S, &'b S)) -> (&'a S, &'a S) { x }", &[B, B][..], &[A, A][..]);
    let got = case(ok.0, ok.1, ok.2, Shape::Pair, None, Plant::None);
    std::println!("tuple: {got:?}");
    assert!(clean(&got), "{got:?}");
    let got = case("fn f<'a, 'b: 'a>(x: (&'a S, &'a S)) -> (&'b S, &'a S) { x }", &[A, A], &[B, A], Shape::Pair, None, Plant::None);
    std::println!("tuple reversed: {got:?}");
    assert!(found(&got), "{got:?}");
    let got = case(ok.0, ok.1, ok.2, Shape::Pair, None, Plant::NoVariance);
    std::println!("tuple, planted NoVariance: {got:?}");
    assert!(found(&got), "{got:?}");
}

/// An ADT is variant in a parameter as its fields are (`Facts::adt_variances`): `W<'w>`
/// covariant (`struct W<'w>(&'w S)`) takes `W<'long>` as `W<'short>`, and finds the reverse;
/// invariant (`struct W<'w>(Cell<&'w S>)`) finds the shortening; not known, it is refused by
/// name (`VarianceUnknown`).
/// Prints: `adt co: Ok([])`, `adt co reversed: Ok(["borrow UnprovenOutlives ..`, `adt in:
/// Ok(["borrow UnprovenOutlives ..`, `adt unknown: Err(.. VarianceUnknown ..)` and
/// `adt co, planted NoVariance: Ok(["borrow UnprovenOutlives ..`.
#[test]
fn an_adt_is_variant_as_its_fields_are() {
    let ok = ("fn f<'a, 'b: 'a>(x: W<'b>) -> W<'a> { x }", &[B][..], &[A][..]);
    let got = case(ok.0, ok.1, ok.2, Shape::W, Some(&CO), Plant::None);
    std::println!("adt co: {got:?}");
    assert!(clean(&got), "{got:?}");
    let got = case("fn f<'a, 'b: 'a>(x: W<'a>) -> W<'b> { x }", &[A], &[B], Shape::W, Some(&CO), Plant::None);
    std::println!("adt co reversed: {got:?}");
    assert!(found(&got), "{got:?}");
    let got = case(ok.0, ok.1, ok.2, Shape::W, Some(&IN), Plant::None);
    std::println!("adt in: {got:?}");
    assert!(found(&got), "{got:?}");
    // NOT KNOWN: related invariantly (sound: more relations refuse more), which is stricter
    // than Rust's whenever the true variance is not `In`, so noted (variance.rs, the coarse
    // rule): its finding is the relation's artifact and the body is refused by name, never
    // reported and never clean. The one test of the unknown case: every other fixture states
    // its ADTs' variances.
    let got = case(ok.0, ok.1, ok.2, Shape::W, None, Plant::None);
    std::println!("adt unknown: {got:?}");
    assert!(
        matches!(got, Err(crate::Refusal { form: crate::Form::Unified { why: crate::Unified::VarianceUnknown { .. } }, .. })),
        "{got:?}"
    );
    let got = case(ok.0, ok.1, ok.2, Shape::W, Some(&CO), Plant::NoVariance);
    std::println!("adt co, planted NoVariance: {got:?}");
    assert!(found(&got), "{got:?}");
}

/// The composition rule (variance.rs `Variance::then`), exhaustively.
#[test]
fn variances_compose_as_the_table_says() {
    use Variance::{Co, Contra, In};
    for (a, b, want) in [(Co, Co, Co), (Co, Contra, Contra), (Contra, Co, Contra), (Contra, Contra, Co), (In, Co, In), (Co, In, In), (Contra, In, In), (In, Contra, In), (In, In, In)] {
        assert_eq!(a.then(b), want, "{a:?} then {b:?}");
    }
}
