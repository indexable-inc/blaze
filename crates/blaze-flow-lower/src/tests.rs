//! Fixtures: bodies built with the type checker's fixture world, checked by the real type
//! checker, lowered here and checked by blaze-flow. Each expectation is stated from a written
//! rule (quoted per test); each catch has a control with a planted bug that hides it, and every
//! test prints the line it caught.

use std::format;
use std::string::String;
use std::vec;

use blaze_flow::record::check;
use blaze_own::Alloc;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{Checked, ExprId, FnPtrHeader, InferCtx, ItemKey, Mutbl, Name, Needs, Outcome, PNode, PTy};

use crate::region_fixture::{self, BOUND1, Decls, FnHead, P0, P1, P2};
use crate::{AdtFacts, CalleeFacts, Facts, Form, Plant, Refusal, Region, UnreadWhy};

/// Every ADT of the fixtures is a plain struct with no `Copy` impl, no lifetimes, needing drop.
struct Plain;

impl Facts for Plain {
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
    fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: true, lifetimes: 0, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
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

/// Facts whose heads' region facts are declared by hand ([`Decls`], laid in the checked
/// record's table by region_fixture.rs `run`): what [`findings_laid`] lowers under.
pub(crate) trait Declared: Facts {
    fn decls(&self) -> &Decls;
}

/// The fixtures' facts with the heads' region facts DECLARED (D148: each fn's column and
/// lifetime parameters by the numbering rule, region_fixture.rs's module doc), never read from
/// a text. `callees_known` false: no callee's `CalleeFacts` is known.
pub(crate) struct Sigs {
    pub(crate) decls: Decls,
    pub(crate) callees_known: bool,
}

impl Sigs {
    pub(crate) fn new() -> Sigs {
        Sigs { decls: Decls::new(), callees_known: true }
    }

    /// `item`'s head: its lifetimes and its column over `fn(inputs..) -> output`.
    pub(crate) fn add(&mut self, item: ItemKey, head: FnHead<'_>) {
        self.decls.func(item, head);
    }
}

impl Declared for Sigs {
    fn decls(&self) -> &Decls {
        &self.decls
    }
}

impl Facts for Sigs {
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
        Plain.adt(k)
    }
    fn callee(&self, k: ItemKey) -> Option<CalleeFacts> {
        if self.callees_known { Plain.callee(k) } else { None }
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

/// The findings of one body under `facts` and `plant`, one line each; or the refusal.
pub(crate) fn findings_with<F: Facts>(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId, facts: &F, plant: Plant) -> Result<vec::Vec<String>, Refusal> {
    let c = checked(fx, b, owner, root);
    let view = b.view(owner, root);
    // Through the call-rule oracle (oracle.rs): both rules, compared per call, production's flow.
    let lowered = crate::oracle::lower_checked(&view, &c, facts, plant, blaze_own::testing::heap().alloc())?;
    let rec = check(&lowered.flow, Alloc::global()).expect("bounded");
    let mut out = vec::Vec::new();
    for m in rec.moves.iter() {
        out.push(format!("move {:?} at {:?} span {}..{}", m.kind, m.at, m.span.lo, m.span.hi));
    }
    for f in rec.borrows.iter() {
        out.push(format!("borrow {f:?}"));
    }
    Ok(out)
}

/// The findings of one body under `facts` (their declared heads laid in the body's record,
/// region_fixture.rs `run`) and `plant`, one line each; or the refusal.
pub(crate) fn findings_laid<F: Declared>(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId, facts: &F, plant: Plant) -> Result<vec::Vec<String>, Refusal> {
    region_fixture::findings(fx, b, owner, root, facts, facts.decls(), plant)
}

fn checked<'b>(fx: &Fx, b: &'b Bb, owner: ItemKey, root: ExprId) -> Checked<'static> {
    let mut cx = InferCtx::new_in(Alloc::global());
    match blaze_typeck::check(&b.view(owner, root), fx, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => {
            assert!(c.diags.is_empty(), "the fixture type-checks clean: {:?}", c.diags);
            c
        }
        Ok(Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("{e:?}"),
    }
}

/// The findings of one body under `plant` and `Plain` with `heads` laid, one line each, and the
/// flow's text.
fn findings(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId, heads: &Decls, plant: Plant) -> (vec::Vec<String>, String) {
    match region_fixture::run(fx, &b.view(owner, root), &Plain, heads, plant) {
        Ok(r) => (r.lines, r.flow),
        Err(r) => panic!("refused: {r:?}"),
    }
}

/// `struct S; fn g(_: &S); fn h(_: S); fn f(s: S)`, the world finished, and the three heads.
fn world() -> (Fx, ItemKey, ItemKey, Decls) {
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let unit = fx.unit();
    let g = fx.func("g", 0, &[rs], unit, vec![]);
    let h = fx.func("h", 0, &[st], unit, vec![]);
    let f = fx.func("f", 0, &[st], unit, vec![]);
    fx.finish();
    let mut heads = Decls::new();
    // `fn g(_: &S)`: one elided input lifetime, `'0`, late-bound.
    heads.func(g, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[rs], output: unit, slots: &[P0] });
    // `fn h(_: S)` and `fn f(s: S)`: no lifetime.
    heads.func(h, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[st], output: unit, slots: &[] });
    heads.func(f, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[st], output: unit, slots: &[] });
    (fx, s, f, heads)
}

/// The Reference, "Moved and copied types": "After moving out of a place expression that
/// evaluates to a local variable, the location is deinitialized and cannot be read from again
/// until it is reinitialized." `fn f(s: S) { let a = s; let b = s; }`: the second read is a use of
/// a moved value. Control: lowering every read as a copy (`Plant::AllCopy`) hides it.
#[test]
fn moved_twice() {
    let (fx, _s, f, heads) = world();
    let mut b = Bb::new();
    let s = b.param("s");
    let (a, bb) = (b.bind("a"), b.bind("b"));
    let r1 = b.local(s);
    let st1 = b.let_(a, None, r1);
    let r2 = b.local(s);
    let st2 = b.let_(bb, None, r2);
    let root = b.block(&[st1, st2], None);
    let (got, text) = findings(&fx, &b, f, root, &heads, Plant::None);
    std::println!("moved_twice: {got:?}");
    assert!(got.iter().any(|l| l.contains("UseOfMoved")), "the second move is caught:\n{text}");
    let (planted, _) = findings(&fx, &b, f, root, &heads, Plant::AllCopy);
    std::println!("moved_twice, planted AllCopy: {planted:?}");
    assert!(planted.is_empty(), "the control hides it");
}

/// `fn f(s: S) { let a = s; h(a); }`: one move each: no finding.
#[test]
fn moved_once() {
    let (fx, _s, f, heads) = world();
    let mut b = Bb::new();
    let s = b.param("s");
    let a = b.bind("a");
    let r1 = b.local(s);
    let st1 = b.let_(a, None, r1);
    let h = b.path(&["h"]);
    let ra = b.local(a);
    let call = b.call(h, &[ra]);
    let st2 = b.semi(call);
    let root = b.block(&[st1, st2], None);
    let (got, text) = findings(&fx, &b, f, root, &heads, Plant::None);
    std::println!("moved_once: {got:?}");
    assert!(got.is_empty(), "{got:?}\n{text}");
}

/// Polonius: "It is an error to invalidate a loan that is live at a given point."
/// `fn f(s: S) { let r = &s; h(s); g(r); }`: moving `s` writes it deeply while the loan, through
/// `r`, is live until `g(r)`. Control: an assignment that relates no origins (`Plant::NoRelate`)
/// leaves the loan in the borrow's temporary, dead before the move: no finding.
#[test]
fn move_while_borrowed() {
    let (fx, _s, f, heads) = world();
    let mut b = Bb::new();
    let s = b.param("s");
    let r = b.bind("r");
    let ls = b.local(s);
    let bor = b.borrow(Mutbl::Not, ls);
    let st1 = b.let_(r, None, bor);
    let h = b.path(&["h"]);
    let ls2 = b.local(s);
    let c1 = b.call(h, &[ls2]);
    let st2 = b.semi(c1);
    let g = b.path(&["g"]);
    let lr = b.local(r);
    let c2 = b.call(g, &[lr]);
    let st3 = b.semi(c2);
    let root = b.block(&[st1, st2, st3], None);
    let (got, text) = findings(&fx, &b, f, root, &heads, Plant::None);
    std::println!("move_while_borrowed: {got:?}");
    assert!(got.iter().any(|l| l.starts_with("borrow Invalidated")), "{got:?}\n{text}");
    let (planted, _) = findings(&fx, &b, f, root, &heads, Plant::NoRelate);
    std::println!("move_while_borrowed, planted NoRelate: {planted:?}");
    assert!(!planted.iter().any(|l| l.starts_with("borrow")), "the control hides it");
}

/// The same with the use first: `let r = &s; g(r); h(s);`: the loan is dead at the move (RFC
/// 2094: "a variable is live if the current value that it holds may be used later").
#[test]
fn move_after_last_use() {
    let (fx, _s, f, heads) = world();
    let mut b = Bb::new();
    let s = b.param("s");
    let r = b.bind("r");
    let ls = b.local(s);
    let bor = b.borrow(Mutbl::Not, ls);
    let st1 = b.let_(r, None, bor);
    let g = b.path(&["g"]);
    let lr = b.local(r);
    let c2 = b.call(g, &[lr]);
    let st2 = b.semi(c2);
    let h = b.path(&["h"]);
    let ls2 = b.local(s);
    let c1 = b.call(h, &[ls2]);
    let st3 = b.semi(c1);
    let root = b.block(&[st1, st2, st3], None);
    let (got, text) = findings(&fx, &b, f, root, &heads, Plant::None);
    std::println!("move_after_last_use: {got:?}");
    assert!(got.is_empty(), "{got:?}\n{text}");
}

// ---------------------------------------------------------------- the signature's lifetime part

/// `struct S;` and the types `S` and `&S`, before any fn is added.
fn sig_base() -> (Fx, PTy, PTy) {
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    (fx, st, rs)
}

/// `fn keep<'a, 'b>(x: &'a S, y: &'b S) -> &'a S { x }` is right. Its nearest wrong program,
/// `fn pick<'a, 'b>(x: &'a S, y: &'b S) -> &'a S { y }`, needs `'b: 'a`, which the signature
/// does not state (rustc: "lifetime may not live long enough"): `UnprovenOutlives`.
#[test]
fn a_returned_reference_must_be_the_signatures() {
    let (mut fx, _st, rs) = sig_base();
    let keep = fx.func("keep", 0, &[rs, rs], rs, vec![]);
    let pick = fx.func("pick", 0, &[rs, rs], rs, vec![]);
    fx.finish();
    let mut sigs = Sigs::new();
    // `fn keep<'a, 'b>(x: &'a S, y: &'b S) -> &'a S` and `pick`, the same head: `'a` is `P0`,
    // `'b` `P1`, both late-bound (in the inputs, in no bound); column `[x, y, output]`.
    for item in [keep, pick] {
        sigs.add(item, FnHead { lifetimes: 2, late: &[0, 1], outlives: &[], inputs: &[rs, rs], output: rs, slots: &[P0, P1, P0] });
    }
    for (owner, tail_is_y, wrong) in [(keep, false, false), (pick, true, true)] {
        let mut b = Bb::new();
        let x = b.param("x");
        let y = b.param("y");
        let tail = b.local(if tail_is_y { y } else { x });
        let root = b.block(&[], Some(tail));
        let got = findings_laid(&fx, &b, owner, root, &sigs, Plant::None).expect("lowered");
        std::println!("a_returned_reference_must_be_the_signatures, wrong {wrong}: {got:?}");
        assert_eq!(got.iter().any(|l| l.starts_with("borrow UnprovenOutlives")), wrong, "{got:?}");
        assert_eq!(got.is_empty(), !wrong, "{got:?}");
    }
}

/// `fn f(x: &S) -> &S { x }` (elision: the one input's lifetime) and `fn g<'a, 'b: 'a>(x: &'b S)
/// -> &'a S { x }` (the declared bound; the Reference, "Lifetime bounds") are right. Control:
/// with the signature's bounds not given as known (`Plant::NoKnown`), `g` is refused.
#[test]
fn elision_and_a_declared_bound_are_known() {
    let (mut fx, _st, rs) = sig_base();
    let f = fx.func("f", 0, &[rs], rs, vec![]);
    let g = fx.func("g", 0, &[rs], rs, vec![]);
    fx.finish();
    let mut sigs = Sigs::new();
    // `fn f(x: &S) -> &S`: one elided input lifetime `P0` (late-bound), the output's by elision.
    sigs.add(f, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[rs], output: rs, slots: &[P0, P0] });
    // `fn g<'a, 'b: 'a>(x: &'b S) -> &'a S`: `'a` is `P0`, `'b` `P1` (declaration order), both
    // early-bound (the bound names them); `'b: 'a` declared.
    sigs.add(g, FnHead { lifetimes: 2, late: &[], outlives: &[(P1, P0)], inputs: &[rs], output: rs, slots: &[P1, P0] });
    for owner in [f, g] {
        let mut b = Bb::new();
        let x = b.param("x");
        let tail = b.local(x);
        let root = b.block(&[], Some(tail));
        let got = findings_laid(&fx, &b, owner, root, &sigs, Plant::None).expect("lowered");
        std::println!("elision_and_a_declared_bound_are_known: {got:?}");
        assert!(got.is_empty(), "{got:?}");
        if owner == g {
            let planted = findings_laid(&fx, &b, owner, root, &sigs, Plant::NoKnown).expect("lowered");
            std::println!("elision_and_a_declared_bound_are_known, planted NoKnown: {planted:?}");
            assert!(planted.iter().any(|l| l.starts_with("borrow UnprovenOutlives")), "the control refuses g: {planted:?}");
        }
    }
}

/// `fn dangle<'a>(s: S) -> &'a S { &s }` is wrong, the classic (rustc's E0515): the loan of `s`
/// is in `'a`, live at every point, when `s` is dropped at the body's end: `Invalidated`.
#[test]
fn a_reference_to_a_local_is_not_returned() {
    let (mut fx, st, rs) = sig_base();
    let dangle = fx.func("dangle", 0, &[st], rs, vec![]);
    fx.finish();
    let mut sigs = Sigs::new();
    // `fn dangle<'a>(s: S) -> &'a S`: `'a` is `P0`, early-bound (not in the inputs).
    sigs.add(dangle, FnHead { lifetimes: 1, late: &[], outlives: &[], inputs: &[st], output: rs, slots: &[P0] });
    let mut b = Bb::new();
    let s = b.param("s");
    let ls = b.local(s);
    let bor = b.borrow(Mutbl::Not, ls);
    let root = b.block(&[], Some(bor));
    let got = findings_laid(&fx, &b, dangle, root, &sigs, Plant::None).expect("lowered");
    std::println!("a_reference_to_a_local_is_not_returned: {got:?}");
    assert!(got.iter().any(|l| l.starts_with("borrow Invalidated")), "{got:?}");
}

/// Census class "`( self , other )`" (a derived `eq`): `fn eq2(a: &S, b: &S) { let t = (a, b); }`
/// is right: the tuple's type is `(&'1 S, &'2 S)`, and nothing relates `'1` to `'2`. Control:
/// the older lowering, which made every position of a tuple's operands equal
/// (`Plant::TupleUnified`), reports `UnprovenOutlives` between the two parameters' lifetimes.
#[test]
fn a_tuple_relates_each_element_to_its_own_field() {
    let (mut fx, _st, rs) = sig_base();
    let unit = fx.unit();
    let eq2 = fx.func("eq2", 0, &[rs, rs], unit, vec![]);
    fx.finish();
    let mut sigs = Sigs::new();
    // `fn eq2(a: &S, b: &S)`: two elided input lifetimes, `P0` and `P1`, both late-bound.
    sigs.add(eq2, FnHead { lifetimes: 2, late: &[0, 1], outlives: &[], inputs: &[rs, rs], output: unit, slots: &[P0, P1] });
    let mut b = Bb::new();
    let (pa, pb) = (b.param("a"), b.param("b"));
    let t = b.bind("t");
    let (la, lb) = (b.local(pa), b.local(pb));
    let tup = b.tuple(&[la, lb]);
    let st1 = b.let_(t, None, tup);
    let root = b.block(&[st1], None);
    let got = findings_laid(&fx, &b, eq2, root, &sigs, Plant::None).expect("lowered");
    std::println!("a_tuple_relates_each_element_to_its_own_field: {got:?}");
    assert!(got.is_empty(), "{got:?}");
    let planted = findings_laid(&fx, &b, eq2, root, &sigs, Plant::TupleUnified).expect("lowered");
    std::println!("a_tuple_relates_each_element_to_its_own_field, planted TupleUnified: {planted:?}");
    assert!(planted.iter().any(|l| l.starts_with("borrow UnprovenOutlives")), "{planted:?}");
}

/// The nearest wrong program to the tuple fixture: `fn swap<'a, 'b>(a: &'a S, b: &'b S) ->
/// (&'b S, &'a S) { (a, b) }` puts `'a` where the signature says `'b`: still refused.
#[test]
fn a_tuple_of_the_wrong_lifetimes_is_refused() {
    let (mut fx, _st, rs) = sig_base();
    let out = fx.tuple(&[rs, rs]);
    let swap = fx.func("swap", 0, &[rs, rs], out, vec![]);
    fx.finish();
    let mut sigs = Sigs::new();
    // `fn swap<'a, 'b>(a: &'a S, b: &'b S) -> (&'b S, &'a S)`: `'a` is `P0`, `'b` `P1`, both
    // late-bound; the output tuple's slots in element order.
    sigs.add(swap, FnHead { lifetimes: 2, late: &[0, 1], outlives: &[], inputs: &[rs, rs], output: out, slots: &[P0, P1, P1, P0] });
    let mut b = Bb::new();
    let (pa, pb) = (b.param("a"), b.param("b"));
    let (la, lb) = (b.local(pa), b.local(pb));
    let tup = b.tuple(&[la, lb]);
    let root = b.block(&[], Some(tup));
    let got = findings_laid(&fx, &b, swap, root, &sigs, Plant::None).expect("lowered");
    std::println!("a_tuple_of_the_wrong_lifetimes_is_refused: {got:?}");
    assert!(got.iter().any(|l| l.starts_with("borrow UnprovenOutlives")), "{got:?}");
}

/// A call is related through its callee's lifetime part, instantiated fresh at the call. With
/// `fn first<'a, 'b>(x: &'a S, y: &'b S) -> &'a S`: `fn ok<'a, 'b>(x: &'a S, y: &'b S) -> &'a S
/// { first(x, y) }` is right, and `fn bad<'a, 'b>(x: &'a S, y: &'b S) -> &'b S { first(x, y) }`
/// is wrong (the result lives for `'a`, returned as `'b`). Without the callee's lifetime part
/// the call is not lowered at all: refused by name (`RegionsUnread { item: first, NoHead }`,
/// D148; before the record it was `ElisionAmbiguous`, two input lifetimes).
#[test]
fn a_call_is_related_through_its_callees_signature() {
    let (mut fx, _st, rs) = sig_base();
    let first = fx.func("first", 0, &[rs, rs], rs, vec![]);
    let ok = fx.func("ok", 0, &[rs, rs], rs, vec![]);
    let bad = fx.func("bad", 0, &[rs, rs], rs, vec![]);
    fx.finish();
    // `fn first<'a, 'b>(x: &'a S, y: &'b S) -> &'a S` and `fn ok<'a, 'b>(..) -> &'a S`: `'a` is
    // `P0`, `'b` `P1`, both late-bound.
    let two_one = FnHead { lifetimes: 2, late: &[0, 1], outlives: &[], inputs: &[rs, rs], output: rs, slots: &[P0, P1, P0] };
    // `fn bad<'a, 'b>(x: &'a S, y: &'b S) -> &'b S`.
    let two_two = FnHead { slots: &[P0, P1, P1], ..two_one };
    let mut sigs = Sigs::new();
    sigs.add(first, two_one);
    sigs.add(ok, two_one);
    sigs.add(bad, two_two);
    // The control's facts: the owner's head alone, the callee's not laid.
    let mut no_callee = Sigs::new();
    no_callee.add(ok, two_one);
    for (owner, wrong) in [(ok, false), (bad, true)] {
        let mut b = Bb::new();
        let (px, py) = (b.param("x"), b.param("y"));
        let callee = b.path(&["first"]);
        let (lx, ly) = (b.local(px), b.local(py));
        let call = b.call(callee, &[lx, ly]);
        let root = b.block(&[], Some(call));
        let got = findings_laid(&fx, &b, owner, root, &sigs, Plant::None).expect("lowered");
        std::println!("a_call_is_related_through_its_callees_signature, wrong {wrong}: {got:?}");
        assert_eq!(got.iter().any(|l| l.starts_with("borrow UnprovenOutlives")), wrong, "{got:?}");
        assert_eq!(got.is_empty(), !wrong, "{got:?}");
        if !wrong {
            let without = findings_laid(&fx, &b, owner, root, &no_callee, Plant::None);
            std::println!("a_call_is_related_through_its_callees_signature, no lifetime parts: {without:?}");
            assert!(
                matches!(without, Err(Refusal { form: Form::RegionsUnread { item, why: UnreadWhy::NoHead }, .. }) if item == first),
                "{without:?}"
            );
        }
    }
}

/// A call whose output is an OPAQUE type and whose arguments carry no lifetime: `async fn
/// add_slowly(a: u32, b: u32) -> u32` called as `let f = mk(1u8);` (its future the opaque `mk`
/// returns). The Reference, "Async functions": the call "returns a future that holds its
/// arguments", and "Impl trait": a return-position `impl Trait` captures the in-scope generic
/// parameters (edition 2024); with no input lifetime there is none to capture, so the output's
/// one origin position is related to nothing, and the body is lowered with no finding. Before
/// this, the opaque's position took the elision path, which needs exactly one input lifetime:
/// refused `ElisionAmbiguous` (the rung x0-async-01's `compute`). Control: the same call with an
/// output of a REFERENCE (`fn mk_ref(a: u8) -> &S`, an elided output lifetime with no input
/// lifetime, E0106) stays refused: its head is refused (D148: `RegionsUnread { item: mk_ref,
/// HeadRefused }`, before the record `ElisionAmbiguous`): the rule is the opaque's, not elision's.
///
/// Prints: `an opaque output with no input lifetime: lowered, findings []; a reference output: HeadRefused`
#[test]
fn an_opaque_output_with_no_input_lifetime_is_related_to_nothing() {
    let (mut fx, _st, rs) = sig_base();
    let (unit, u8_) = (fx.unit(), fx.u8());
    let sized = fx.tr("Sized", 1, vec::Vec::new(), &[]);
    fx.lang(blaze_typeck::LangItem::Sized, sized);
    let opaque_key = fx.opaque_key("mk::{opaque#0}");
    let none = fx.list(&[]);
    let opaque = fx.ty(PNode::Opaque(opaque_key, none));
    let opaque_sized = fx.bound(sized, &[opaque]);
    fx.set_opaque_bounds(opaque_key, vec![opaque_sized]);
    let mk = fx.func("mk", 0, &[u8_], opaque, vec![]);
    let mk_ref = fx.func("mk_ref", 0, &[u8_], rs, vec![]);
    let owner = fx.func("owner", 0, &[], unit, vec![]);
    fx.finish();
    let mut sigs = Sigs::new();
    // `fn mk(a: u8) -> impl Sized` (the opaque captures nothing: no lifetime in scope): no slot.
    sigs.decls.opaque(opaque_key, 0);
    sigs.add(mk, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[u8_], output: opaque, slots: &[] });
    // `fn owner()`: no lifetime.
    sigs.add(owner, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[], output: unit, slots: &[] });
    // `fn mk_ref(a: u8) -> &S`: E0106, the head refused.
    sigs.decls.refuse(mk_ref);
    let body = |callee: &str| {
        let mut b = Bb::new();
        let e0 = b.path(&[callee]);
        let e1 = b.int(Some(blaze_typeck::IntSuffix::Uint(blaze_typeck::UintTy::U8)));
        let e2 = b.call(e0, &[e1]);
        let f = b.bind("f");
        let s0 = b.let_(f, None, e2);
        let root = b.block(&[s0], None);
        (b, root)
    };
    let (b, root) = body("mk");
    let got = findings_laid(&fx, &b, owner, root, &sigs, Plant::None).unwrap_or_else(|r| panic!("an opaque output with no input lifetime is lowered: {r:?}"));
    assert!(got.is_empty(), "{got:?}");
    let (b, root) = body("mk_ref");
    let control = findings_laid(&fx, &b, owner, root, &sigs, Plant::None);
    assert!(
        matches!(control, Err(Refusal { form: Form::RegionsUnread { item, why: UnreadWhy::HeadRefused }, .. }) if item == mk_ref),
        "control: {control:?}"
    );
    std::println!("an opaque output with no input lifetime: lowered, findings {got:?}; a reference output: HeadRefused");
}

/// A call whose callee's head is not known: `fn two(a: &S, b: &S) { k(a, b); }` is right: each
/// argument is a reference held by value, covariant in its own lifetime, so the call cannot
/// relate `a`'s lifetime to `b`'s (RFC 2094, "Subtyping"; the design is the source). Control:
/// the older form, the first position related both ways (`Plant::HubBothWays`), reports
/// `UnprovenOutlives` between them.
#[test]
fn an_unknown_callee_takes_each_reference_one_way() {
    let (mut fx, _st, rs) = sig_base();
    let unit = fx.unit();
    let k = fx.func("k", 0, &[rs, rs], unit, vec![]);
    let two = fx.func("two", 0, &[rs, rs], unit, vec![]);
    fx.finish();
    let mut sigs = Sigs::new();
    sigs.callees_known = false;
    // `fn two(a: &S, b: &S)`: two elided input lifetimes `P0`, `P1`, late-bound; `k`'s head is
    // not laid.
    sigs.add(two, FnHead { lifetimes: 2, late: &[0, 1], outlives: &[], inputs: &[rs, rs], output: unit, slots: &[P0, P1] });
    let mut b = Bb::new();
    let (pa, pb) = (b.param("a"), b.param("b"));
    let callee = b.path(&["k"]);
    let (la, lb) = (b.local(pa), b.local(pb));
    let call = b.call(callee, &[la, lb]);
    let st1 = b.semi(call);
    let root = b.block(&[st1], None);
    // SUPERSEDED by the soundness rule below (`an_unknown_callee_is_refused_by_name`): a call
    // of unknown head with a reference argument is no longer related at all, it is refused
    // (D148: by the callee's unread regions, `RegionsUnread`, where it was `CalleeUnknown`).
    let got = findings_laid(&fx, &b, two, root, &sigs, Plant::None);
    std::println!("an_unknown_callee_takes_each_reference_one_way: {got:?}");
    assert!(matches!(got, Err(Refusal { form: Form::RegionsUnread { item, why: UnreadWhy::NoHead }, .. }) if item == k), "{got:?}");
}

/// The nearest wrong program to the unknown-callee fixture: `fn leak<'a>(s: S) -> &'a S {
/// k1(&s) }` with `k1: fn(&S) -> &S` of unknown head: the result may be the argument, so the
/// loan of `s` still flows through the call into `'a`, and `s` dies at the end: `Invalidated`.
#[test]
fn an_unknown_callee_still_carries_its_argument_to_its_result() {
    let (mut fx, st, rs) = sig_base();
    let k1 = fx.func("k1", 0, &[rs], rs, vec![]);
    let leak = fx.func("leak", 0, &[st], rs, vec![]);
    fx.finish();
    let mut sigs = Sigs::new();
    sigs.callees_known = false;
    // `fn leak<'a>(s: S) -> &'a S`: `'a` is `P0`, early-bound (not in the inputs).
    sigs.add(leak, FnHead { lifetimes: 1, late: &[], outlives: &[], inputs: &[st], output: rs, slots: &[P0] });
    let mut b = Bb::new();
    let s = b.param("s");
    let callee = b.path(&["k1"]);
    let ls = b.local(s);
    let bor = b.borrow(Mutbl::Not, ls);
    let call = b.call(callee, &[bor]);
    let root = b.block(&[], Some(call));
    let got = findings_laid(&fx, &b, leak, root, &sigs, Plant::None);
    std::println!("an_unknown_callee_still_carries_its_argument_to_its_result: {got:?}");
    // Never accepted: refused by name since the soundness rule (before it: `Invalidated`; D148:
    // by the callee's unread regions).
    assert!(matches!(got, Err(Refusal { form: Form::RegionsUnread { item, why: UnreadWhy::NoHead }, .. }) if item == k1), "{got:?}");
}

/// SOUNDNESS: `fn f(s: S) { spawn(&s); }` with `spawn` of unknown head. `spawn` may be `fn
/// spawn(x: &'static S)` (the Reference, "Lifetime bounds": `'static` "outlives all other
/// lifetimes"; a local's borrow does not), and one origin for the call cannot say so: the body
/// is REFUSED by name (D148: `Form::RegionsUnread { item: spawn, NoHead }`, "the callee's head
/// regions are not read"; before the record `CalleeUnknown`), never accepted. A call of unknown
/// head in which nothing carries a lifetime (`h(s)`, `s: S`) has nothing a signature could
/// demand, and is still lowered.
/// Prints: `an_unknown_callee_is_refused_by_name: Err(Refusal { form: RegionsUnread { .. }, .. })`.
#[test]
fn an_unknown_callee_is_refused_by_name() {
    let (mut fx, st, rs) = sig_base();
    let unit = fx.unit();
    let spawn = fx.func("spawn", 0, &[rs], unit, vec![]);
    let _h = fx.func("h", 0, &[st], unit, vec![]);
    let f = fx.func("f", 0, &[st], unit, vec![]);
    fx.finish();
    let mut sigs = Sigs::new();
    sigs.callees_known = false;
    // `fn f(s: S)`: no lifetime; neither `spawn`'s nor `h`'s head is laid.
    sigs.add(f, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[st], output: unit, slots: &[] });
    let mut b = Bb::new();
    let s = b.param("s");
    let callee = b.path(&["spawn"]);
    let ls = b.local(s);
    let bor = b.borrow(Mutbl::Not, ls);
    let call = b.call(callee, &[bor]);
    let st1 = b.semi(call);
    let root = b.block(&[st1], None);
    let got = findings_laid(&fx, &b, f, root, &sigs, Plant::None);
    std::println!("an_unknown_callee_is_refused_by_name: {got:?}");
    assert!(matches!(got, Err(Refusal { form: Form::RegionsUnread { item, why: UnreadWhy::NoHead }, .. }) if item == spawn), "{got:?}");
    // `fn f(s: S) { h(s); }`: no lifetime anywhere in the call.
    let mut b = Bb::new();
    let s = b.param("s");
    let callee = b.path(&["h"]);
    let ls = b.local(s);
    let call = b.call(callee, &[ls]);
    let st1 = b.semi(call);
    let root = b.block(&[st1], None);
    let got = findings_laid(&fx, &b, f, root, &sigs, Plant::None);
    std::println!("an_unknown_callee_is_refused_by_name, no lifetime in the call: {got:?}");
    assert_eq!(got, Ok(vec![]), "{got:?}");
}

/// SOUNDNESS: a call through a fn pointer takes its lifetimes from the pointer's own type (the
/// Reference, "Higher-ranked trait bounds": such bounds "specify a bound that is true for all
/// lifetimes"; "Lifetime elision" holds in "function item, function pointer, and closure trait
/// signatures", so `fn(&S)` is `for<'a> fn(&'a S)`: fresh at each call). `fn ok(f: fn(&S), s:
/// S) { f(&s); }` is right. `fn bad(f: fn(&'static S), s: S) { f(&s); }` is wrong: `'static`
/// "outlives all other lifetimes" ("Lifetime bounds") and the borrow of `s` ends with `s`: the
/// loan reaches the `'static` placeholder and is live when `s` dies: `Invalidated`. Control:
/// the pointer's `'static` instantiated like any other region (`Plant::StaticIgnored`) accepts
/// `bad`. Without the body's own head regions the pointer's lifetimes are unknown: refused by
/// name (D148: `RegionsUnread { item: owner, NoHead }`, where the text reader's absence was
/// `CalleeValueUnknown`).
/// Prints: `a_fn_pointers_static_is_demanded_of_its_argument bad: ["borrow Invalidated ..`.
#[test]
fn a_fn_pointers_static_is_demanded_of_its_argument() {
    let (mut fx, st, rs) = sig_base();
    let unit = fx.unit();
    let l = fx.list(&[rs, unit]);
    let fp = fx.ty(PNode::FnPtr(l, FnPtrHeader::RUST));
    let ok = fx.func("ok", 0, &[fp, st], unit, vec![]);
    let bad = fx.func("bad", 0, &[fp, st], unit, vec![]);
    fx.finish();
    // `fn ok(f: fn(&S), s: S)`: no lifetime of its own; the pointer's elided input is the
    // pointer's binder's (binder 1: the signature's root is 0): `[BOUND1]`.
    // `fn bad(f: fn(&'static S), s: S)`: `[Static]`.
    for (owner, text, slot, wrong) in [(ok, "fn ok(f: fn(&S), s: S) { f(&s); }", BOUND1, false), (bad, "fn bad(f: fn(&'static S), s: S) { f(&s); }", Region::Static, true)] {
        let mut sigs = Sigs::new();
        sigs.add(owner, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[fp, st], output: unit, slots: &[slot] });
        let mut b = Bb::new();
        let (pf, ps) = (b.param("f"), b.param("s"));
        let (lf, ls) = (b.local(pf), b.local(ps));
        let bor = b.borrow(Mutbl::Not, ls);
        let call = b.call(lf, &[bor]);
        let st1 = b.semi(call);
        let root = b.block(&[st1], None);
        let got = findings_laid(&fx, &b, owner, root, &sigs, Plant::None).expect("lowered");
        std::println!("a_fn_pointers_static_is_demanded_of_its_argument {}: {got:?}", if wrong { "bad" } else { "ok" });
        assert_eq!(got.iter().any(|l| l.starts_with("borrow Invalidated")), wrong, "{text}: {got:?}");
        if wrong {
            let planted = findings_laid(&fx, &b, owner, root, &sigs, Plant::StaticIgnored).expect("lowered");
            std::println!("a_fn_pointers_static_is_demanded_of_its_argument, planted StaticIgnored: {planted:?}");
            assert!(planted.is_empty(), "{planted:?}");
        } else {
            assert!(got.is_empty(), "{text}: {got:?}");
        }
        let unread = findings_laid(&fx, &b, owner, root, &Sigs::new(), Plant::None);
        assert!(matches!(unread, Err(Refusal { form: Form::RegionsUnread { item, why: UnreadWhy::NoHead }, .. }) if item == owner), "{unread:?}");
    }
}

/// A fn pointer that returns a reference: `fn pass(f: fn(&S) -> &S, s: &S) -> &S { f(s) }` is
/// right (elision in the pointer's type: its output takes its one input's lifetime, so the
/// result is `s`'s). `fn leak<'a>(f: fn(&S) -> &S, s: S) -> &'a S { f(&s) }` is wrong: the
/// result carries the loan of `s` into `'a`: `Invalidated`.
/// Prints: `a_fn_pointers_result_carries_its_argument leak: ["borrow Invalidated ..`.
#[test]
fn a_fn_pointers_result_carries_its_argument() {
    let (mut fx, st, rs) = sig_base();
    let l = fx.list(&[rs, rs]);
    let fp = fx.ty(PNode::FnPtr(l, FnPtrHeader::RUST));
    let pass = fx.func("pass", 0, &[fp, rs], rs, vec![]);
    let leak = fx.func("leak", 0, &[fp, st], rs, vec![]);
    fx.finish();
    // `fn pass(f: fn(&S) -> &S, s: &S) -> &S`: the pointer's input and output are its binder's
    // (`BOUND1`, elision in the pointer's type); `s`'s elided lifetime `P0` (late-bound) is the
    // output's: `[BOUND1, BOUND1, P0, P0]`.
    let pass_slots: &[Region] = &[BOUND1, BOUND1, P0, P0];
    // `fn leak<'a>(f: fn(&S) -> &S, s: S) -> &'a S`: `'a` is `P0`, early-bound (in no input):
    // `[BOUND1, BOUND1, P0]`.
    let leak_slots: &[Region] = &[BOUND1, BOUND1, P0];
    for (owner, text, wrong) in [(pass, "fn pass(f: fn(&S) -> &S, s: &S) -> &S { f(s) }", false), (leak, "fn leak<'a>(f: fn(&S) -> &S, s: S) -> &'a S { f(&s) }", true)] {
        let mut sigs = Sigs::new();
        let (late, slots): (&[u32], &[Region]) = if wrong { (&[], leak_slots) } else { (&[0], pass_slots) };
        sigs.add(owner, FnHead { lifetimes: 1, late, outlives: &[], inputs: &[fp, if wrong { st } else { rs }], output: rs, slots });
        let mut b = Bb::new();
        let (pf, ps) = (b.param("f"), b.param("s"));
        let (lf, ls) = (b.local(pf), b.local(ps));
        let arg = if wrong { b.borrow(Mutbl::Not, ls) } else { ls };
        let call = b.call(lf, &[arg]);
        let root = b.block(&[], Some(call));
        let got = findings_laid(&fx, &b, owner, root, &sigs, Plant::None).expect("lowered");
        std::println!("a_fn_pointers_result_carries_its_argument {}: {got:?}", if wrong { "leak" } else { "pass" });
        if wrong {
            assert!(got.iter().any(|l| l.starts_with("borrow Invalidated")), "{text}: {got:?}");
        } else {
            assert!(got.is_empty(), "{text}: {got:?}");
        }
    }
}

/// [`Sigs`] with one ADT that declares ONE lifetime (`life`), and closures lowered. The ADT's
/// count is declared to the column layer too ([`Wide::add`] declares it with each head), so a
/// head's column and `Facts::adt`'s `lifetimes` agree.
pub(crate) struct Wide {
    pub(crate) sigs: Sigs,
    pub(crate) life: Option<ItemKey>,
}

impl Wide {
    /// `item`'s head (the numbering rule: region_fixture.rs's module doc).
    pub(crate) fn add(&mut self, item: ItemKey, head: FnHead<'_>) {
        // A second declaration of the same count is harmless: the first is the one read.
        if let Some(life) = self.life {
            self.sigs.decls.adt(life, 1);
        }
        self.sigs.add(item, head);
    }
}

impl Declared for Wide {
    fn decls(&self) -> &Decls {
        &self.sigs.decls
    }
}

impl Facts for Wide {
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
        Plain.adt(k).map(|f| AdtFacts { lifetimes: u32::from(Some(k) == self.life), ..f })
    }
    /// The ADT that declares a lifetime is `struct B<'x>(&'x S);`: covariant in `'x` (the
    /// Reference, "Subtyping and Variance"); every other fixture ADT has no parameter.
    fn adt_variances(&self, k: ItemKey) -> Option<&[crate::Variance]> {
        Some(region_fixture::all_co(u32::from(Some(k) == self.life)))
    }
    fn callee(&self, k: ItemKey) -> Option<CalleeFacts> {
        self.sigs.callee(k)
    }
    fn closure_bodies_checked(&self) -> bool {
        true
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

/// The findings of one body, one line each (or the refusal), and its flow's text; `facts`'
/// declared heads laid in its record (region_fixture.rs `run`).
fn findings_and_flow<F: Declared>(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId, facts: &F, plant: Plant) -> (Result<vec::Vec<String>, Refusal>, String) {
    match region_fixture::run(fx, &b.view(owner, root), facts, facts.decls(), plant) {
        Ok(r) => (Ok(r.lines), r.flow),
        Err(r) => (Err(r), String::new()),
    }
}

/// Census 1's class "a live loan invalidated in a derived body" (131): what `#[derive(Debug)]`
/// writes, `b.field(..).field(..).finish()` with `b: DebugStruct<'_, '_>`, here `struct B`
/// (declaring a lifetime) with `fn field(&mut self, v: &S) -> &mut Self` and `fn finish(&mut
/// self)`: a chain of two-phase borrows and reborrows of the receiver. The program is right:
/// `fn fmt(b: B, s: &S) { b.field(s).field(s).finish(); }`: each call's loan of `b` flows to
/// that call's result alone (elision: the output's reference takes the receiver's lifetime;
/// `Self`'s own lifetime is the same on both sides and takes nothing from it: `&'r mut Self`
/// states `Self: 'r`, never the reverse: the Reference, "Lifetime bounds").
/// THE CAUSE, tested by putting it back (`Plant::UnknownUnified`, heads unknown): a call of
/// unknown head made the receiver's loan equal to every other position through one origin, so
/// the loan of `b` reached `b`'s own lifetime position, live as long as `b` is: `Invalidated`
/// where `b` dies. Without the plant the same body is refused by name, not accepted.
/// Prints: `a_builder_chain_keeps_each_loan_in_its_result: []` and, planted, a line holding
/// `borrow Invalidated`; a failure prints the flow.
#[test]
fn a_builder_chain_keeps_each_loan_in_its_result() {
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let bk = fx.strukt("B", 0, &[]);
    let bt = fx.adt(bk, &[]);
    let rmb = fx.r(Mutbl::Mut, bt);
    let unit = fx.unit();
    let imp = fx.imp("impl B", 0, bt, None, &[], vec![]);
    let field = fx.impl_fn(imp, "field", true, &[rmb, rs], rmb);
    let finish = fx.impl_fn(imp, "finish", true, &[rmb], unit);
    let fmt = fx.func("fmt", 0, &[bt, rs], unit, vec![]);
    fx.finish();
    // `fn fmt(b: B<'_>, s: &S)`: two elided input lifetimes, `B`'s `P0` and `s`'s `P1`, both
    // late-bound.
    let fmt_head = FnHead { lifetimes: 2, late: &[0, 1], outlives: &[], inputs: &[bt, rs], output: unit, slots: &[P0, P1] };
    // `impl<'x> B<'x> { fn field(&mut self, v: &S) -> &mut Self }`: the container's `'x` is
    // `P0` (early), then the elided `&mut self` `P1` and `v`'s `P2` (late); the output's
    // reference takes the receiver's `P1`, `Self` is `B<'x>`: `[P1, P0, P2, P1, P0]`.
    let field_head = FnHead { lifetimes: 3, late: &[1, 2], outlives: &[], inputs: &[rmb, rs], output: rmb, slots: &[P1, P0, P2, P1, P0] };
    // `fn finish(&mut self)` in the same impl: `'x` `P0`, the receiver's `P1` (late): `[P1, P0]`.
    let finish_head = FnHead { lifetimes: 2, late: &[1], outlives: &[], inputs: &[rmb], output: unit, slots: &[P1, P0] };
    let mut b = Bb::new();
    let (pb, ps) = (b.param("b"), b.param("s"));
    let (lb, s1, s2) = (b.local(pb), b.local(ps), b.local(ps));
    let c1 = b.method(lb, "field", &[s1]);
    let c2 = b.method(c1, "field", &[s2]);
    let c3 = b.method(c2, "finish", &[]);
    let st1 = b.semi(c3);
    let root = b.block(&[st1], None);
    let mut known = Wide { sigs: Sigs::new(), life: Some(bk) };
    known.add(fmt, fmt_head);
    known.add(field, field_head);
    known.add(finish, finish_head);
    let (got, flow) = findings_and_flow(&fx, &b, fmt, root, &known, Plant::None);
    std::println!("a_builder_chain_keeps_each_loan_in_its_result: {got:?}");
    assert_eq!(got, Ok(vec![]), "the flow:\n{flow}");
    // The heads unknown: refused by name (D148: the first callee's regions unread, where it was
    // `CalleeUnknown`); with the old relation put back: the census's finding.
    let mut unknown = Wide { sigs: Sigs::new(), life: Some(bk) };
    unknown.sigs.callees_known = false;
    unknown.add(fmt, fmt_head);
    let (got, _) = findings_and_flow(&fx, &b, fmt, root, &unknown, Plant::None);
    assert!(matches!(got, Err(Refusal { form: Form::RegionsUnread { item, why: UnreadWhy::NoHead }, .. }) if item == field), "{got:?}");
}

/// `struct S; fn g(_: &S); fn h(_: S); fn m(_: &mut S); fn f(s: S)` for the closure fixtures,
/// and the facts with their heads.
fn closure_world() -> (Fx, ItemKey, Wide) {
    let mut fx = Fx::new();
    // `Copy`, with no impl for `S`: a `move` capture asks whether the captured type is `Copy`
    // ("Capture modes": "by move or, for `Copy` types, by copy").
    let copy = fx.tr("Copy", 1, vec![], &[]);
    fx.lang(blaze_typeck::LangItem::Copy, copy);
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let rms = fx.r(Mutbl::Mut, st);
    let unit = fx.unit();
    let g = fx.func("g", 0, &[rs], unit, vec![]);
    let h = fx.func("h", 0, &[st], unit, vec![]);
    let m = fx.func("m", 0, &[rms], unit, vec![]);
    let f = fx.func("f", 0, &[st], unit, vec![]);
    fx.finish();
    let mut facts = Wide { sigs: Sigs::new(), life: None };
    // `fn g(_: &S)` and `fn m(_: &mut S)`: one elided input lifetime `P0`, late-bound.
    facts.add(g, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[rs], output: unit, slots: &[P0] });
    facts.add(m, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[rms], output: unit, slots: &[P0] });
    // `fn h(_: S)` and `fn f(s: S)`: no lifetime.
    facts.add(h, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[st], output: unit, slots: &[] });
    facts.add(f, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[st], output: unit, slots: &[] });
    (fx, f, facts)
}

/// The Reference, "Closure types", "Capture modes": "The compiler prefers to capture a
/// closed-over variable by immutable borrow, followed by unique immutable borrow, by mutable
/// borrow, and finally by move." The capture is a borrow at the closure's creation, live as
/// long as the closure value is. `fn f(s: S) { let c = || g(&s); g(&s); let d = c; }`: a read
/// under a shared capture: right. `fn f(s: S) { let c = || g(&s); let w = &mut s; let d = c;
/// }`: a mutable borrow of `s` while the capture is live (the closure is used after):
/// `Invalidated`. Control: the captures not lowered (`Plant::NoCaptures`) accepts the second.
/// Prints: `a_shared_capture_is_a_live_borrow written: ["borrow Invalidated ..`.
#[test]
fn a_shared_capture_is_a_live_borrow() {
    let (fx, f, facts) = closure_world();
    for written in [false, true] {
        let mut b = Bb::new();
        let s = b.param("s");
        let (callee, ls) = (b.path(&["g"]), b.local(s));
        let bor = b.borrow(Mutbl::Not, ls);
        let inner = b.call(callee, &[bor]);
        let cl = b.closure(&[], inner);
        let pc = b.bind("c");
        let st1 = b.let_(pc, None, cl);
        let ls2 = b.local(s);
        let st2 = if written {
            let w = b.borrow(Mutbl::Mut, ls2);
            let pw = b.bind("w");
            b.let_(pw, None, w)
        } else {
            let callee = b.path(&["g"]);
            let r = b.borrow(Mutbl::Not, ls2);
            let call = b.call(callee, &[r]);
            b.semi(call)
        };
        let lc = b.local(pc);
        let pd = b.bind("d");
        let st3 = b.let_(pd, None, lc);
        let root = b.block(&[st1, st2, st3], None);
        let (got, flow) = findings_and_flow(&fx, &b, f, root, &facts, Plant::None);
        std::println!("a_shared_capture_is_a_live_borrow {}: {got:?}", if written { "written" } else { "read" });
        let got = got.expect("lowered");
        if written {
            assert!(got.iter().any(|l| l.starts_with("borrow Invalidated")), "{got:?}; the flow:\n{flow}");
            let (planted, _) = findings_and_flow(&fx, &b, f, root, &facts, Plant::NoCaptures);
            std::println!("a_shared_capture_is_a_live_borrow, planted NoCaptures: {planted:?}");
            assert_eq!(planted, Ok(vec![]));
        } else {
            assert!(got.is_empty(), "{got:?}; the flow:\n{flow}");
        }
    }
    // Without a caller that checks closure bodies, the body is refused, not accepted.
    let mut b = Bb::new();
    let s = b.param("s");
    let (callee, ls) = (b.path(&["g"]), b.local(s));
    let bor = b.borrow(Mutbl::Not, ls);
    let inner = b.call(callee, &[bor]);
    let cl = b.closure(&[], inner);
    let st1 = b.semi(cl);
    let root = b.block(&[st1], None);
    let got = findings_with(&fx, &b, f, root, &Plain, Plant::None);
    assert!(matches!(got, Err(Refusal { form: Form::Closure, .. })), "{got:?}");
}

/// A capture by mutable borrow ("Capture modes": the body's `&mut s` needs it) made after
/// another borrow has ended: `fn f(s: S) { let r = &s; g(r); let c = || m(&mut s); let d = c;
/// }` is right: `r` is dead when the closure is made. Its nearest wrong program uses `r`
/// after: `.. let c = || m(&mut s); g(r); let d = c; }`: the capture's mutable borrow
/// invalidates the loan `r` holds: `Invalidated`.
/// Prints: `a_mutable_capture_after_a_borrow_ends late use: ["borrow Invalidated ..`.
#[test]
fn a_mutable_capture_after_a_borrow_ends() {
    let (fx, f, facts) = closure_world();
    for late in [false, true] {
        let mut b = Bb::new();
        let s = b.param("s");
        let ls = b.local(s);
        let r0 = b.borrow(Mutbl::Not, ls);
        let pr = b.bind("r");
        let st1 = b.let_(pr, None, r0);
        let use_r = |b: &mut Bb| {
            let (callee, lr) = (b.path(&["g"]), b.local(pr));
            let call = b.call(callee, &[lr]);
            b.semi(call)
        };
        let st2 = use_r(&mut b);
        let (callee, ls2) = (b.path(&["m"]), b.local(s));
        let bm = b.borrow(Mutbl::Mut, ls2);
        let inner = b.call(callee, &[bm]);
        let cl = b.closure(&[], inner);
        let pc = b.bind("c");
        let st3 = b.let_(pc, None, cl);
        // Built only when it is in the block: the type checker refuses an unreached expression.
        let st_late = if late { Some(use_r(&mut b)) } else { None };
        let lc = b.local(pc);
        let pd = b.bind("d");
        let st4 = b.let_(pd, None, lc);
        let root = match st_late {
            Some(x) => b.block(&[st1, st2, st3, x, st4], None),
            None => b.block(&[st1, st2, st3, st4], None),
        };
        let (got, flow) = findings_and_flow(&fx, &b, f, root, &facts, Plant::None);
        std::println!("a_mutable_capture_after_a_borrow_ends {}: {got:?}", if late { "late use" } else { "ended" });
        let got = got.expect("lowered");
        assert_eq!(got.iter().any(|l| l.starts_with("borrow Invalidated")), late, "{got:?}; the flow:\n{flow}");
        if !late {
            assert!(got.is_empty(), "{got:?}; the flow:\n{flow}");
        }
    }
}

/// "Capture modes": "If the `move` keyword is used, then all captures are by move or, for
/// `Copy` types, by copy". `fn f(s: S) { let c = move || h(s); g(&s); }`: `s` was moved into
/// the closure where it was made; "Moved and copied types": it "cannot be read from again until
/// it is reinitialized": a use of a moved value. Control: `Plant::NoCaptures` hides it.
/// Prints: `a_move_closure_moves_its_captures: ["move ..`.
#[test]
fn a_move_closure_moves_its_captures() {
    let (fx, f, facts) = closure_world();
    let mut b = Bb::new();
    let s = b.param("s");
    let (callee, ls) = (b.path(&["h"]), b.local(s));
    let inner = b.call(callee, &[ls]);
    let cl = b.move_closure(&[], inner);
    let pc = b.bind("c");
    let st1 = b.let_(pc, None, cl);
    let (callee, ls2) = (b.path(&["g"]), b.local(s));
    let bor = b.borrow(Mutbl::Not, ls2);
    let call = b.call(callee, &[bor]);
    let st2 = b.semi(call);
    let root = b.block(&[st1, st2], None);
    let (got, flow) = findings_and_flow(&fx, &b, f, root, &facts, Plant::None);
    std::println!("a_move_closure_moves_its_captures: {got:?}");
    assert!(got.as_ref().is_ok_and(|g| g.iter().any(|l| l.starts_with("move "))), "{got:?}; the flow:\n{flow}");
    let (planted, _) = findings_and_flow(&fx, &b, f, root, &facts, Plant::NoCaptures);
    std::println!("a_move_closure_moves_its_captures, planted NoCaptures: {planted:?}");
    assert_eq!(planted, Ok(vec![]));
}

/// A call of a closure is a use of the closure value ("Closure types", "Call traits and
/// coercions": a closure that does not move out of its captures implements `FnMut`, and one
/// that does not mutate them `Fn`): `fn f(s: S) { let c = || g(&s); let w = &mut s; c(); }`:
/// the call keeps the shared capture live past the mutable borrow: `Invalidated`.
/// Prints: `a_closure_call_is_a_use_of_the_closure: ["borrow Invalidated ..`.
#[test]
fn a_closure_call_is_a_use_of_the_closure() {
    let (fx, f, facts) = closure_world();
    let mut b = Bb::new();
    let s = b.param("s");
    let (callee, ls) = (b.path(&["g"]), b.local(s));
    let bor = b.borrow(Mutbl::Not, ls);
    let inner = b.call(callee, &[bor]);
    let cl = b.closure(&[], inner);
    let pc = b.bind("c");
    let st1 = b.let_(pc, None, cl);
    let ls2 = b.local(s);
    let w = b.borrow(Mutbl::Mut, ls2);
    let pw = b.bind("w");
    let st2 = b.let_(pw, None, w);
    let lc = b.local(pc);
    let call = b.call(lc, &[]);
    let st3 = b.semi(call);
    let root = b.block(&[st1, st2, st3], None);
    let (got, flow) = findings_and_flow(&fx, &b, f, root, &facts, Plant::None);
    std::println!("a_closure_call_is_a_use_of_the_closure: {got:?}");
    assert!(got.as_ref().is_ok_and(|g| g.iter().any(|l| l.starts_with("borrow Invalidated"))), "{got:?}; the flow:\n{flow}");
}

// ---------------------------------------------------------------- struct update expressions

/// Every ADT of the fixtures `Copy`, needing no drop.
struct CopyAll;

impl Facts for CopyAll {
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
    fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
        Some(AdtFacts { copy: true, drop_impl: false, needs_drop: false, lifetimes: 0, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, k: ItemKey) -> Option<CalleeFacts> {
        Plain.callee(k)
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

/// `struct S; struct P { a: S, c: S }; fn mk() -> P; fn h(_: P); fn g(_: S); fn f(p: P, s: S)`.
fn update_world() -> (Fx, ItemKey) {
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let p = fx.strukt("P", 0, &[("a", st), ("c", st)]);
    let pt = fx.adt(p, &[]);
    let unit = fx.unit();
    let _mk = fx.func("mk", 0, &[], pt, vec![]);
    let _h = fx.func("h", 0, &[pt], unit, vec![]);
    let _g = fx.func("g", 0, &[st], unit, vec![]);
    let f = fx.func("f", 0, &[pt, st], unit, vec![]);
    fx.finish();
    (fx, f)
}

/// The written field of every update below: `a: <expr>`, `field_inits[0..1]` of the view.
const WRITES_A: blaze_typeck::List = blaze_typeck::List { start: 0, len: 1 };

/// `P { a: <field_inits[0]>, ..base }`: the fixture has no struct-expression builder, so the
/// expression takes the slot (and range) of a path expression `P` made for it; its one written
/// field is the view's `field_inits[0]` ([`update_findings`]).
fn update(b: &mut Bb, base: ExprId) -> ExprId {
    let at = b.path(&["P"]);
    let blaze_typeck::Expr::Path(path) = b.exprs[at.ix()] else { panic!("`Bb::path` made other than a path expression") };
    b.exprs[at.ix()] = blaze_typeck::Expr::Struct { path, fields: WRITES_A, base: blaze_typeck::StructBase::Expr(base) };
    at
}

/// `base.name`: the fixture has no field-access builder, so it takes the slot of a read of `p`.
fn field(b: &mut Bb, p: blaze_typeck::PatId, base: ExprId, name: &str) -> ExprId {
    let at = b.local(p);
    b.exprs[at.ix()] = blaze_typeck::Expr::Field { base, name: Name::of(name) };
    at
}

/// The findings of one body whose view's `field_inits` are `inits`, checked by the real type
/// checker (so `Checked::fru_fields` is its own), under `facts`; or the refusal.
fn update_findings<F: Facts>(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId, inits: &[blaze_typeck::FieldInit], facts: &F) -> Result<vec::Vec<String>, Refusal> {
    let mut view = b.view(owner, root);
    view.field_inits = inits;
    let mut cx = InferCtx::new_in(Alloc::global());
    let c = match blaze_typeck::check(&view, fx, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        Ok(Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("{e:?}"),
    };
    assert!(c.diags.is_empty(), "the fixture type-checks clean: {:?}", c.diags);
    let lowered = crate::oracle::lower_checked(&view, &c, facts, Plant::None, blaze_own::testing::heap().alloc())?;
    let rec = check(&lowered.flow, Alloc::global()).expect("bounded");
    let mut out = vec::Vec::new();
    for m in rec.moves.iter() {
        out.push(format!("move {:?} at {:?} span {}..{}", m.kind, m.at, m.span.lo, m.span.hi));
    }
    for f in rec.borrows.iter() {
        out.push(format!("borrow {f:?}"));
    }
    Ok(out)
}

/// One body of `fn f(p: P, s: S)`: `let q = P { a: s, ..<base> }; <after>(<arg>);`, where
/// `base` is `p` or `mk()` and `arg` is `q`, `p`, `p.a` or `p.c` as `use_` says.
fn update_body(fx: &Fx, f: ItemKey, temporary: bool, after: &str, use_: Option<&str>, whole: bool, facts: &impl Facts) -> Result<vec::Vec<String>, Refusal> {
    let mut b = Bb::new();
    let p = b.param("p");
    let s = b.param("s");
    let q = b.bind("q");
    let ls = b.local(s);
    let base = if temporary {
        let mk = b.path(&["mk"]);
        b.call(mk, &[])
    } else {
        b.local(p)
    };
    let up = update(&mut b, base);
    let st1 = b.let_(q, None, up);
    let callee = b.path(&[after]);
    let arg = match (use_, whole) {
        (Some(name), _) => {
            let lp = b.local(p);
            field(&mut b, p, lp, name)
        }
        (None, true) => b.local(p),
        (None, false) => b.local(q),
    };
    let c = b.call(callee, &[arg]);
    let st2 = b.semi(c);
    let root = b.block(&[st1, st2], None);
    let inits = [blaze_typeck::FieldInit { name: Name::of("a"), expr: ls }];
    update_findings(fx, &b, f, root, &inits, facts)
}

/// The Reference, "Functional update syntax": the update "moves or copies the remaining fields
/// from the base expression"; "Moved and copied types": a value whose type implements `Copy`
/// is copied, and a move out of a field of a local moves only that field. Four bodies of
/// `fn f(p: P, s: S)` with `struct P { a: S, c: S }`, each `let q = P { a: s, ..base }; ..`:
/// 1. base `mk()`, then `h(q)`: the temporary's `c` moves into `q`: no finding.
/// 2. base `p`, every type `Copy`, then `h(p)`: `p.c` is copied, `p` still usable: no finding.
/// 3. base `p`, then `g(p.a)`: `a` was written, so the update took only `p.c`; `p.a` is still
///    there to move: no finding.
/// 4. base `p`, then `g(p.c)`: the update moved `p.c` out: a use of a moved value.
/// A version that moves the base whole finds `UseOfMoved` in 2 (under `Copy` it would be a
/// copy, so in 3); one that copies it whole, or takes no field, accepts 4. Prints
/// "struct_update_bases: temporary Ok([]), copy Ok([]), written Ok([]), taken Ok([\"move UseOfMoved at ..\"])"
/// (the finding's place and span in place of `..`).
#[test]
fn struct_update_bases() {
    let (fx, f) = update_world();
    let temporary = update_body(&fx, f, true, "h", None, false, &Plain);
    assert!(temporary.as_ref().is_ok_and(|g| g.is_empty()), "{temporary:?}");
    let copy = update_body(&fx, f, false, "h", None, true, &CopyAll);
    assert!(copy.as_ref().is_ok_and(|g| g.is_empty()), "{copy:?}");
    let written = update_body(&fx, f, false, "g", Some("a"), false, &Plain);
    assert!(written.as_ref().is_ok_and(|g| g.is_empty()), "{written:?}");
    let taken = update_body(&fx, f, false, "g", Some("c"), false, &Plain);
    std::println!("struct_update_bases: temporary {temporary:?}, copy {copy:?}, written {written:?}, taken {taken:?}");
    assert!(taken.as_ref().is_ok_and(|g| g.iter().any(|l| l.contains("UseOfMoved"))), "{taken:?}");
}
