//! Variance (piece B13; NEVER COMPILED when written, 2026-11-18): a value related to another of
//! its type position by position at each position's variance (variance.rs; the Reference,
//! "Subtyping and Variance"). Each row of the table with its positive (rustc accepts), its
//! negative (rustc rejects: "lifetime may not live long enough", here `UnprovenOutlives`), and
//! THE CONTROL (rule 23): `Plant::NoVariance` (every position below the top reference related
//! both ways, the form before this piece) finds the positives again, as before.
//!
//! Each body is `fn f<..>(x: T) -> U { x }` of the fixture world, its lifetimes read from its
//! text (tests.rs `Wide`), type checked by the real checker and lowered (`findings_with`, the
//! shared helper of tests.rs, which takes the process heap: listed in the lane's report).
//!
//! NOT HERE: the table's fn-pointer row (`fn(&'short T)` to `fn(&'long T)`): a fn pointer's
//! lifetimes have no position in the lowering (lib.rs `npos`), so neither direction relates
//! anything there yet (variance.rs, "WHAT IS NOT HERE").

use blaze_typeck::fixture::Bb;
use blaze_typeck::{ExprId, ItemKey, Mutbl, Name, PTy};

use crate::callee_value_tests::no_clauses;
use crate::tests::{Sigs, Wide, findings_with};
use crate::{AdtFacts, CalleeFacts, Facts, Plant, SigLife, Variance};

/// The fixtures' facts with one ADT `W<'w>` (`Wide::life`) whose variance in `'w` is `var`
/// (`None`: not known).
struct Var {
    wide: Wide,
    var: Option<&'static [Variance]>,
}

impl Facts for Var {
    fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
        self.wide.adt(k)
    }
    fn callee(&self, k: ItemKey) -> Option<CalleeFacts> {
        self.wide.callee(k)
    }
    fn sig(&self, item: ItemKey) -> Option<SigLife<'_>> {
        self.wide.sig(item)
    }
    fn closure_bodies_checked(&self) -> bool {
        self.wide.closure_bodies_checked()
    }
    fn adt_variances(&self, adt: ItemKey) -> Option<&[Variance]> {
        if Some(adt) == self.wide.life { self.var } else { None }
    }
    fn field(&self, adt: ItemKey, variant: u32, name: Name) -> Option<u32> {
        self.wide.field(adt, variant, name)
    }
    fn field_count(&self, adt: ItemKey, variant: u32) -> Option<u32> {
        self.wide.field_count(adt, variant)
    }
    fn param_copy(&self, index: u32) -> bool {
        self.wide.param_copy(index)
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

/// The findings of `fn f..(x: T) -> T { x }` written as `text`, its erased type `shape`, `W`'s
/// variance `var`, under `plant`: `Ok(lines)` or the refusal's words.
fn case(text: &str, shape: Shape, var: Option<&'static [Variance]>, plant: Plant) -> Result<std::vec::Vec<std::string::String>, crate::Refusal> {
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
    let mut b = Bb::new();
    let px = b.param("x");
    let lx: ExprId = b.local(px);
    let root = b.block(&[], Some(lx));
    let mut facts = Var { wide: Wide { sigs: Sigs::new(), life: Some(wk) }, var };
    facts.wide.add(&w.fx, f, text, &[t], t);
    findings_with(&w.fx, &b, f, root, &facts, plant)
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
    let ok = "fn f<'a, 'b: 'a>(x: &'a &'b S) -> &'a &'a S { x }";
    let got = case(ok, Shape::RefRef, None, Plant::None);
    std::println!("shared referent: {got:?}");
    assert!(clean(&got), "{got:?}");
    let got = case("fn f<'a, 'b: 'a>(x: &'a &'a S) -> &'a &'b S { x }", Shape::RefRef, None, Plant::None);
    std::println!("shared referent reversed: {got:?}");
    assert!(found(&got), "{got:?}");
    let got = case(ok, Shape::RefRef, None, Plant::NoVariance);
    std::println!("shared referent, planted NoVariance: {got:?}");
    assert!(found(&got), "{got:?}");
}

/// `&'a mut T` is invariant in `T`: `&mut &'long S` to `&mut &'short S` is the classic
/// unsoundness (a `&'short S` could be stored where `'long` is assumed) and MUST be found; the
/// same type to itself is clean.
/// Prints: `mut referent shortened: Ok(["borrow UnprovenOutlives ..` and `mut referent same: Ok([])`.
#[test]
fn a_mutable_referent_is_invariant() {
    let got = case("fn f<'a, 'b: 'a>(x: &'a mut &'b S) -> &'a mut &'a S { x }", Shape::MutRef, None, Plant::None);
    std::println!("mut referent shortened: {got:?}");
    assert!(found(&got), "{got:?}");
    let got = case("fn f<'a, 'b: 'a>(x: &'a mut &'b S) -> &'a mut &'b S { x }", Shape::MutRef, None, Plant::None);
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
    let ok = "fn f<'a, 'b: 'a>(x: (&'b S, &'b S)) -> (&'a S, &'a S) { x }";
    let got = case(ok, Shape::Pair, None, Plant::None);
    std::println!("tuple: {got:?}");
    assert!(clean(&got), "{got:?}");
    let got = case("fn f<'a, 'b: 'a>(x: (&'a S, &'a S)) -> (&'b S, &'a S) { x }", Shape::Pair, None, Plant::None);
    std::println!("tuple reversed: {got:?}");
    assert!(found(&got), "{got:?}");
    let got = case(ok, Shape::Pair, None, Plant::NoVariance);
    std::println!("tuple, planted NoVariance: {got:?}");
    assert!(found(&got), "{got:?}");
}

/// An ADT is variant in a parameter as its fields are (`Facts::adt_variances`): `W<'w>`
/// covariant (`struct W<'w>(&'w S)`) takes `W<'long>` as `W<'short>`, and finds the reverse;
/// invariant (`struct W<'w>(Cell<&'w S>)`), or not known, finds the shortening.
/// Prints: `adt co: Ok([])`, `adt co reversed: Ok(["borrow UnprovenOutlives ..`, `adt in:
/// Ok(["borrow UnprovenOutlives ..`, `adt unknown: Ok(["borrow UnprovenOutlives ..` and
/// `adt co, planted NoVariance: Ok(["borrow UnprovenOutlives ..`.
#[test]
fn an_adt_is_variant_as_its_fields_are() {
    let ok = "fn f<'a, 'b: 'a>(x: W<'b>) -> W<'a> { x }";
    let got = case(ok, Shape::W, Some(&CO), Plant::None);
    std::println!("adt co: {got:?}");
    assert!(clean(&got), "{got:?}");
    let got = case("fn f<'a, 'b: 'a>(x: W<'a>) -> W<'b> { x }", Shape::W, Some(&CO), Plant::None);
    std::println!("adt co reversed: {got:?}");
    assert!(found(&got), "{got:?}");
    let got = case(ok, Shape::W, Some(&IN), Plant::None);
    std::println!("adt in: {got:?}");
    assert!(found(&got), "{got:?}");
    let got = case(ok, Shape::W, None, Plant::None);
    std::println!("adt unknown: {got:?}");
    assert!(found(&got), "{got:?}");
    let got = case(ok, Shape::W, Some(&CO), Plant::NoVariance);
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
