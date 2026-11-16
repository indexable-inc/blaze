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

use crate::{
    AdtFacts, CalleeFacts, CallableRefusal, Facts, Form, Plant, Refusal, Region, SigBuf, SigLife, SigRefusal, lower_planted, read_callable_param,
    read_signature,
};

/// Every ADT of the fixtures is a plain struct with no `Copy` impl, no lifetimes, needing drop.
struct Plain;

impl Facts for Plain {
    fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: true, lifetimes: false, union: false, is_enum: false, is_box: false })
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

/// The fixtures' facts with lifetime parts: each fn's read from the text given for it by
/// [`read_signature`], as the census does. `callees_known` false: no callee's head is known
/// (the lowering relates such a call as one of unknown signature).
struct Sigs {
    sigs: vec::Vec<(ItemKey, SigBuf<Alloc<'static>>)>,
    callees_known: bool,
    /// The text of the body being lowered (`Facts::own_text`); empty: not known.
    own: String,
}

impl Sigs {
    fn new() -> Sigs {
        Sigs { sigs: vec::Vec::new(), callees_known: true, own: String::new() }
    }

    /// Reads `text` as the signature of `item`, whose erased types are `inputs` and `output`.
    fn add(&mut self, fx: &Fx, item: ItemKey, text: &str, inputs: &[PTy], output: PTy) {
        let mut buf = SigBuf::new_in(Alloc::global());
        match read_signature(text, fx.t.view(), inputs, output, &|_| Some(false), &mut buf, Alloc::global()) {
            Ok(()) => self.sigs.push((item, buf)),
            Err(why) => panic!("`{text}` was not read: {why:?}"),
        }
    }
}

impl Facts for Sigs {
    fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
        Plain.adt(k)
    }
    fn callee(&self, k: ItemKey) -> Option<CalleeFacts> {
        if self.callees_known { Plain.callee(k) } else { None }
    }
    fn sig(&self, item: ItemKey) -> Option<SigLife<'_>> {
        self.sigs.iter().find(|(k, _)| *k == item).map(|(_, s)| s.life())
    }
    fn own_text(&self) -> Option<&str> {
        if self.own.is_empty() { None } else { Some(&self.own) }
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
fn findings_with<F: Facts>(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId, facts: &F, plant: Plant) -> Result<vec::Vec<String>, Refusal> {
    let c = checked(fx, b, owner, root);
    let view = b.view(owner, root);
    let lowered = lower_planted(&view, &c, facts, plant, Alloc::global())?;
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

/// The findings of one body under `plant`, one line each, and the flow's text.
fn findings(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId, plant: Plant) -> (vec::Vec<String>, String) {
    let c = checked(fx, b, owner, root);
    let view = b.view(owner, root);
    let lowered = match lower_planted(&view, &c, &Plain, plant, Alloc::global()) {
        Ok(l) => l,
        Err(r) => panic!("refused: {r:?}"),
    };
    let mut text = String::new();
    lowered.flow.print(&mut text).expect("print");
    let rec = check(&lowered.flow, Alloc::global()).expect("bounded");
    let mut out = vec::Vec::new();
    for m in rec.moves.iter() {
        out.push(format!("move {:?} at {:?} span {}..{}", m.kind, m.at, m.span.lo, m.span.hi));
    }
    for f in rec.borrows.iter() {
        out.push(format!("borrow {f:?}"));
    }
    (out, text)
}

/// `struct S; fn g(_: &S); fn f(s: S)`, and the fixture world finished by `body`.
fn world() -> (Fx, ItemKey, ItemKey) {
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let unit = fx.unit();
    let _g = fx.func("g", 0, &[rs], unit, vec![]);
    let _h = fx.func("h", 0, &[st], unit, vec![]);
    let f = fx.func("f", 0, &[st], unit, vec![]);
    fx.finish();
    (fx, s, f)
}

/// The Reference, "Moved and copied types": "After moving out of a place expression that
/// evaluates to a local variable, the location is deinitialized and cannot be read from again
/// until it is reinitialized." `fn f(s: S) { let a = s; let b = s; }`: the second read is a use of
/// a moved value. Control: lowering every read as a copy (`Plant::AllCopy`) hides it.
#[test]
fn moved_twice() {
    let (fx, _s, f) = world();
    let mut b = Bb::new();
    let s = b.param("s");
    let (a, bb) = (b.bind("a"), b.bind("b"));
    let r1 = b.local(s);
    let st1 = b.let_(a, None, r1);
    let r2 = b.local(s);
    let st2 = b.let_(bb, None, r2);
    let root = b.block(&[st1, st2], None);
    let (got, text) = findings(&fx, &b, f, root, Plant::None);
    std::println!("moved_twice: {got:?}");
    assert!(got.iter().any(|l| l.contains("UseOfMoved")), "the second move is caught:\n{text}");
    let (planted, _) = findings(&fx, &b, f, root, Plant::AllCopy);
    std::println!("moved_twice, planted AllCopy: {planted:?}");
    assert!(planted.is_empty(), "the control hides it");
}

/// `fn f(s: S) { let a = s; h(a); }`: one move each: no finding.
#[test]
fn moved_once() {
    let (fx, _s, f) = world();
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
    let (got, text) = findings(&fx, &b, f, root, Plant::None);
    std::println!("moved_once: {got:?}");
    assert!(got.is_empty(), "{got:?}\n{text}");
}

/// Polonius: "It is an error to invalidate a loan that is live at a given point."
/// `fn f(s: S) { let r = &s; h(s); g(r); }`: moving `s` writes it deeply while the loan, through
/// `r`, is live until `g(r)`. Control: an assignment that relates no origins (`Plant::NoRelate`)
/// leaves the loan in the borrow's temporary, dead before the move: no finding.
#[test]
fn move_while_borrowed() {
    let (fx, _s, f) = world();
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
    let (got, text) = findings(&fx, &b, f, root, Plant::None);
    std::println!("move_while_borrowed: {got:?}");
    assert!(got.iter().any(|l| l.starts_with("borrow Invalidated")), "{got:?}\n{text}");
    let (planted, _) = findings(&fx, &b, f, root, Plant::NoRelate);
    std::println!("move_while_borrowed, planted NoRelate: {planted:?}");
    assert!(!planted.iter().any(|l| l.starts_with("borrow")), "the control hides it");
}

/// The same with the use first: `let r = &s; g(r); h(s);`: the loan is dead at the move (RFC
/// 2094: "a variable is live if the current value that it holds may be used later").
#[test]
fn move_after_last_use() {
    let (fx, _s, f) = world();
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
    let (got, text) = findings(&fx, &b, f, root, Plant::None);
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

fn read(fx: &Fx, text: &str, inputs: &[PTy], output: PTy) -> Result<(vec::Vec<Region>, vec::Vec<u32>, vec::Vec<(Region, Region)>), SigRefusal> {
    let mut buf = SigBuf::new_in(Alloc::global());
    read_signature(text, fx.t.view(), inputs, output, &|_| Some(false), &mut buf, Alloc::global())?;
    Ok((buf.regions.iter().copied().collect(), buf.ends.iter().copied().collect(), buf.bounds.iter().copied().collect()))
}

/// The reader lines a signature's written lifetimes up with the erased types' positions, by
/// the Reference's "Lifetime elision" rules (quoted in `sig.rs`), and refuses what it cannot.
#[test]
fn signature_text_is_read_by_the_references_rules() {
    let (mut fx, st, rs) = sig_base();
    let u8t = fx.u8();
    let pair = fx.tuple(&[u8t, u8t]);
    let rpair = fx.r(Mutbl::Not, pair);
    let ru8 = fx.r(Mutbl::Not, u8t);
    let rrs = fx.r(Mutbl::Not, rs);
    let rms = fx.r(Mutbl::Mut, st);
    use Region::{Anon, Named, Static};
    // A named lifetime is one region wherever it is written.
    let got = read(&fx, "fn first<'a>(x: &'a (u8, u8)) -> &'a u8 { &x.0 }", &[rpair], ru8);
    assert_eq!(got, Ok((vec![Named(0), Named(0)], vec![1], vec![])));
    // "Each elided lifetime in the parameters becomes a distinct lifetime parameter"; "If there
    // is exactly one lifetime used in the parameters (elided or not), that lifetime is assigned
    // to all elided output lifetimes."
    let got = read(&fx, "fn f(x: &S) -> &S { x }", &[rs], rs);
    assert_eq!(got, Ok((vec![Anon(0), Anon(0)], vec![1], vec![])));
    // "If the receiver has type `&Self` or `&mut Self`, then the lifetime of that reference to
    // `Self` is assigned to all elided output lifetime parameters."
    let got = read(&fx, "pub fn name(&mut self, other: &S) -> &S { other }", &[rms, rs], rs);
    assert_eq!(got, Ok((vec![Anon(0), Anon(1), Anon(0)], vec![1, 2], vec![])));
    // A declared bound, `'b: 'a` (numbered by first appearance: `'b` is 0).
    let got = read(&fx, "fn g<'a, 'b: 'a>(x: &'b S) -> &'a S { x }", &[rs], rs);
    assert_eq!(got, Ok((vec![Named(0), Named(1)], vec![1], vec![(Named(0), Named(1))])));
    // The same in a where clause.
    let got = read(&fx, "fn g<'a, 'b>(x: &'b S) -> &'a S where 'b: 'a { x }", &[rs], rs);
    assert_eq!(got, Ok((vec![Named(0), Named(1)], vec![1], vec![(Named(0), Named(1))])));
    // A nested reference: two positions, outer first.
    let got = read(&fx, "fn inner<'a, 'b>(x: &'a &'b S) -> &'a S { *x }", &[rrs], rs);
    assert_eq!(got, Ok((vec![Named(0), Named(1), Named(0)], vec![2], vec![])));
    // `'static` is the one lifetime used in the parameters.
    let got = read(&fx, "fn s(x: &'static S) -> &S { x }", &[rs], rs);
    assert_eq!(got, Ok((vec![Static, Static], vec![1], vec![])));
    // Two input lifetimes and no receiver: the elided output is an error in Rust; refused.
    let got = read(&fx, "fn h(x: &S, y: &S) -> &S { x }", &[rs, rs], rs);
    assert!(matches!(got, Err(SigRefusal::ElisionUndecided { .. })), "{got:?}");
    // A written type that is not the erased type's form (an alias): refused, never guessed.
    let got = read(&fx, "fn a(x: Alias) -> u8 { 0 }", &[rs], u8t);
    assert!(matches!(got, Err(SigRefusal::Unaligned { .. })), "{got:?}");
    // The parameters of the text are not the head's.
    let got = read(&fx, "fn a(x: &S) -> u8 { 0 }", &[rs, rs], u8t);
    assert_eq!(got, Err(SigRefusal::ParamCount { text: 1, head: 2 }));
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
    sigs.add(&fx, keep, "fn keep<'a, 'b>(x: &'a S, y: &'b S) -> &'a S { x }", &[rs, rs], rs);
    sigs.add(&fx, pick, "fn pick<'a, 'b>(x: &'a S, y: &'b S) -> &'a S { y }", &[rs, rs], rs);
    for (owner, tail_is_y, wrong) in [(keep, false, false), (pick, true, true)] {
        let mut b = Bb::new();
        let x = b.param("x");
        let y = b.param("y");
        let tail = b.local(if tail_is_y { y } else { x });
        let root = b.block(&[], Some(tail));
        let got = findings_with(&fx, &b, owner, root, &sigs, Plant::None).expect("lowered");
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
    sigs.add(&fx, f, "fn f(x: &S) -> &S { x }", &[rs], rs);
    sigs.add(&fx, g, "fn g<'a, 'b: 'a>(x: &'b S) -> &'a S { x }", &[rs], rs);
    for owner in [f, g] {
        let mut b = Bb::new();
        let x = b.param("x");
        let tail = b.local(x);
        let root = b.block(&[], Some(tail));
        let got = findings_with(&fx, &b, owner, root, &sigs, Plant::None).expect("lowered");
        std::println!("elision_and_a_declared_bound_are_known: {got:?}");
        assert!(got.is_empty(), "{got:?}");
        if owner == g {
            let planted = findings_with(&fx, &b, owner, root, &sigs, Plant::NoKnown).expect("lowered");
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
    sigs.add(&fx, dangle, "fn dangle<'a>(s: S) -> &'a S { &s }", &[st], rs);
    let mut b = Bb::new();
    let s = b.param("s");
    let ls = b.local(s);
    let bor = b.borrow(Mutbl::Not, ls);
    let root = b.block(&[], Some(bor));
    let got = findings_with(&fx, &b, dangle, root, &sigs, Plant::None).expect("lowered");
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
    sigs.add(&fx, eq2, "fn eq2(a: &S, b: &S) { let t = (a, b); }", &[rs, rs], unit);
    let mut b = Bb::new();
    let (pa, pb) = (b.param("a"), b.param("b"));
    let t = b.bind("t");
    let (la, lb) = (b.local(pa), b.local(pb));
    let tup = b.tuple(&[la, lb]);
    let st1 = b.let_(t, None, tup);
    let root = b.block(&[st1], None);
    let got = findings_with(&fx, &b, eq2, root, &sigs, Plant::None).expect("lowered");
    std::println!("a_tuple_relates_each_element_to_its_own_field: {got:?}");
    assert!(got.is_empty(), "{got:?}");
    let planted = findings_with(&fx, &b, eq2, root, &sigs, Plant::TupleUnified).expect("lowered");
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
    sigs.add(&fx, swap, "fn swap<'a, 'b>(a: &'a S, b: &'b S) -> (&'b S, &'a S) { (a, b) }", &[rs, rs], out);
    let mut b = Bb::new();
    let (pa, pb) = (b.param("a"), b.param("b"));
    let (la, lb) = (b.local(pa), b.local(pb));
    let tup = b.tuple(&[la, lb]);
    let root = b.block(&[], Some(tup));
    let got = findings_with(&fx, &b, swap, root, &sigs, Plant::None).expect("lowered");
    std::println!("a_tuple_of_the_wrong_lifetimes_is_refused: {got:?}");
    assert!(got.iter().any(|l| l.starts_with("borrow UnprovenOutlives")), "{got:?}");
}

/// A call is related through its callee's lifetime part, instantiated fresh at the call. With
/// `fn first<'a, 'b>(x: &'a S, y: &'b S) -> &'a S`: `fn ok<'a, 'b>(x: &'a S, y: &'b S) -> &'a S
/// { first(x, y) }` is right, and `fn bad<'a, 'b>(x: &'a S, y: &'b S) -> &'b S { first(x, y) }`
/// is wrong (the result lives for `'a`, returned as `'b`). Without the lifetime part the call is
/// not lowered at all (`ElisionAmbiguous`: two input lifetimes), which is the census's refusal.
#[test]
fn a_call_is_related_through_its_callees_signature() {
    let (mut fx, _st, rs) = sig_base();
    let first = fx.func("first", 0, &[rs, rs], rs, vec![]);
    let ok = fx.func("ok", 0, &[rs, rs], rs, vec![]);
    let bad = fx.func("bad", 0, &[rs, rs], rs, vec![]);
    fx.finish();
    let mut sigs = Sigs::new();
    sigs.add(&fx, first, "fn first<'a, 'b>(x: &'a S, y: &'b S) -> &'a S { x }", &[rs, rs], rs);
    sigs.add(&fx, ok, "fn ok<'a, 'b>(x: &'a S, y: &'b S) -> &'a S { first(x, y) }", &[rs, rs], rs);
    sigs.add(&fx, bad, "fn bad<'a, 'b>(x: &'a S, y: &'b S) -> &'b S { first(x, y) }", &[rs, rs], rs);
    for (owner, wrong) in [(ok, false), (bad, true)] {
        let mut b = Bb::new();
        let (px, py) = (b.param("x"), b.param("y"));
        let callee = b.path(&["first"]);
        let (lx, ly) = (b.local(px), b.local(py));
        let call = b.call(callee, &[lx, ly]);
        let root = b.block(&[], Some(call));
        let got = findings_with(&fx, &b, owner, root, &sigs, Plant::None).expect("lowered");
        std::println!("a_call_is_related_through_its_callees_signature, wrong {wrong}: {got:?}");
        assert_eq!(got.iter().any(|l| l.starts_with("borrow UnprovenOutlives")), wrong, "{got:?}");
        assert_eq!(got.is_empty(), !wrong, "{got:?}");
        if !wrong {
            let without = findings_with(&fx, &b, owner, root, &Plain, Plant::None);
            std::println!("a_call_is_related_through_its_callees_signature, no lifetime parts: {without:?}");
            assert!(matches!(without, Err(Refusal { form: Form::ElisionAmbiguous, .. })), "{without:?}");
        }
    }
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
    let _k = fx.func("k", 0, &[rs, rs], unit, vec![]);
    let two = fx.func("two", 0, &[rs, rs], unit, vec![]);
    fx.finish();
    let mut sigs = Sigs::new();
    sigs.callees_known = false;
    sigs.add(&fx, two, "fn two(a: &S, b: &S) { k(a, b); }", &[rs, rs], unit);
    let mut b = Bb::new();
    let (pa, pb) = (b.param("a"), b.param("b"));
    let callee = b.path(&["k"]);
    let (la, lb) = (b.local(pa), b.local(pb));
    let call = b.call(callee, &[la, lb]);
    let st1 = b.semi(call);
    let root = b.block(&[st1], None);
    // SUPERSEDED by the soundness rule below (`an_unknown_callee_is_refused_by_name`): a call
    // of unknown head with a reference argument is no longer related at all, it is refused.
    let got = findings_with(&fx, &b, two, root, &sigs, Plant::None);
    std::println!("an_unknown_callee_takes_each_reference_one_way: {got:?}");
    assert!(matches!(got, Err(Refusal { form: Form::CalleeUnknown, .. })), "{got:?}");
}

/// The nearest wrong program to the unknown-callee fixture: `fn leak<'a>(s: S) -> &'a S {
/// k1(&s) }` with `k1: fn(&S) -> &S` of unknown head: the result may be the argument, so the
/// loan of `s` still flows through the call into `'a`, and `s` dies at the end: `Invalidated`.
#[test]
fn an_unknown_callee_still_carries_its_argument_to_its_result() {
    let (mut fx, st, rs) = sig_base();
    let _k1 = fx.func("k1", 0, &[rs], rs, vec![]);
    let leak = fx.func("leak", 0, &[st], rs, vec![]);
    fx.finish();
    let mut sigs = Sigs::new();
    sigs.callees_known = false;
    sigs.add(&fx, leak, "fn leak<'a>(s: S) -> &'a S { k1(&s) }", &[st], rs);
    let mut b = Bb::new();
    let s = b.param("s");
    let callee = b.path(&["k1"]);
    let ls = b.local(s);
    let bor = b.borrow(Mutbl::Not, ls);
    let call = b.call(callee, &[bor]);
    let root = b.block(&[], Some(call));
    let got = findings_with(&fx, &b, leak, root, &sigs, Plant::None);
    std::println!("an_unknown_callee_still_carries_its_argument_to_its_result: {got:?}");
    // Never accepted: refused by name since the soundness rule (before it: `Invalidated`).
    assert!(matches!(got, Err(Refusal { form: Form::CalleeUnknown, .. })), "{got:?}");
}

/// SOUNDNESS: `fn f(s: S) { spawn(&s); }` with `spawn` of unknown head. `spawn` may be `fn
/// spawn(x: &'static S)` (the Reference, "Lifetime bounds": `'static` "outlives all other
/// lifetimes"; a local's borrow does not), and one origin for the call cannot say so: the body
/// is REFUSED by name (`Form::CalleeUnknown`: "the callee's signature is not known"), never
/// accepted. A call of unknown head in which nothing carries a lifetime (`h(s)`, `s: S`) has
/// nothing a signature could demand, and is still lowered.
/// Prints: `an_unknown_callee_is_refused_by_name: Err(Refusal { form: CalleeUnknown, .. })`.
#[test]
fn an_unknown_callee_is_refused_by_name() {
    let (mut fx, st, rs) = sig_base();
    let unit = fx.unit();
    let _spawn = fx.func("spawn", 0, &[rs], unit, vec![]);
    let _h = fx.func("h", 0, &[st], unit, vec![]);
    let f = fx.func("f", 0, &[st], unit, vec![]);
    fx.finish();
    let mut sigs = Sigs::new();
    sigs.callees_known = false;
    sigs.add(&fx, f, "fn f(s: S) { spawn(&s); }", &[st], unit);
    let mut b = Bb::new();
    let s = b.param("s");
    let callee = b.path(&["spawn"]);
    let ls = b.local(s);
    let bor = b.borrow(Mutbl::Not, ls);
    let call = b.call(callee, &[bor]);
    let st1 = b.semi(call);
    let root = b.block(&[st1], None);
    let got = findings_with(&fx, &b, f, root, &sigs, Plant::None);
    std::println!("an_unknown_callee_is_refused_by_name: {got:?}");
    assert!(matches!(got, Err(Refusal { form: Form::CalleeUnknown, .. })), "{got:?}");
    // `fn f(s: S) { h(s); }`: no lifetime anywhere in the call.
    let mut b = Bb::new();
    let s = b.param("s");
    let callee = b.path(&["h"]);
    let ls = b.local(s);
    let call = b.call(callee, &[ls]);
    let st1 = b.semi(call);
    let root = b.block(&[st1], None);
    let got = findings_with(&fx, &b, f, root, &sigs, Plant::None);
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
/// `bad`. Without the body's text the pointer's lifetimes are unknown: refused by name.
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
    for (owner, text, wrong) in [(ok, "fn ok(f: fn(&S), s: S) { f(&s); }", false), (bad, "fn bad(f: fn(&'static S), s: S) { f(&s); }", true)] {
        let mut sigs = Sigs::new();
        sigs.add(&fx, owner, text, &[fp, st], unit);
        sigs.own = String::from(text);
        let mut b = Bb::new();
        let (pf, ps) = (b.param("f"), b.param("s"));
        let (lf, ls) = (b.local(pf), b.local(ps));
        let bor = b.borrow(Mutbl::Not, ls);
        let call = b.call(lf, &[bor]);
        let st1 = b.semi(call);
        let root = b.block(&[st1], None);
        let got = findings_with(&fx, &b, owner, root, &sigs, Plant::None).expect("lowered");
        std::println!("a_fn_pointers_static_is_demanded_of_its_argument {}: {got:?}", if wrong { "bad" } else { "ok" });
        assert_eq!(got.iter().any(|l| l.starts_with("borrow Invalidated")), wrong, "{text}: {got:?}");
        if wrong {
            let planted = findings_with(&fx, &b, owner, root, &sigs, Plant::StaticIgnored).expect("lowered");
            std::println!("a_fn_pointers_static_is_demanded_of_its_argument, planted StaticIgnored: {planted:?}");
            assert!(planted.is_empty(), "{planted:?}");
        } else {
            assert!(got.is_empty(), "{text}: {got:?}");
        }
        sigs.own = String::new();
        let unread = findings_with(&fx, &b, owner, root, &sigs, Plant::None);
        assert!(matches!(unread, Err(Refusal { form: Form::CalleeValueUnknown, .. })), "{unread:?}");
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
    for (owner, text, wrong) in [(pass, "fn pass(f: fn(&S) -> &S, s: &S) -> &S { f(s) }", false), (leak, "fn leak<'a>(f: fn(&S) -> &S, s: S) -> &'a S { f(&s) }", true)] {
        let mut sigs = Sigs::new();
        sigs.add(&fx, owner, text, &[fp, if wrong { st } else { rs }], rs);
        sigs.own = String::from(text);
        let mut b = Bb::new();
        let (pf, ps) = (b.param("f"), b.param("s"));
        let (lf, ls) = (b.local(pf), b.local(ps));
        let arg = if wrong { b.borrow(Mutbl::Not, ls) } else { ls };
        let call = b.call(lf, &[arg]);
        let root = b.block(&[], Some(call));
        let got = findings_with(&fx, &b, owner, root, &sigs, Plant::None).expect("lowered");
        std::println!("a_fn_pointers_result_carries_its_argument {}: {got:?}", if wrong { "leak" } else { "pass" });
        if wrong {
            assert!(got.iter().any(|l| l.starts_with("borrow Invalidated")), "{text}: {got:?}");
        } else {
            assert!(got.is_empty(), "{text}: {got:?}");
        }
    }
}

/// The reader of a callable parameter's type (`read_callable_param`), over text and erased
/// types alone: a fn pointer, a `dyn Fn`, an `impl Fn`, a generic `F: Fn(..)` bounded in the
/// generics or the where clause; a written `for<'a>` lifetime is the binder's (`Named`), an
/// elided one its own (`Anon`), `'static` is `Static`; the enclosing fn's lifetime, an alias
/// and two `Fn` bounds are refused by name.
/// Prints: `callable_parameter_types_are_read: 8 forms`.
#[test]
fn callable_parameter_types_are_read() {
    let (mut fx, _st, rs) = sig_base();
    let unit = fx.unit();
    fx.finish();
    let read = |text: &str, inputs: &[PTy], output: PTy| -> Result<(vec::Vec<Region>, vec::Vec<u32>), CallableRefusal> {
        let mut buf = SigBuf::new_in(Alloc::global());
        read_callable_param(text, fx.t.view(), 0, inputs, output, &|_| Some(false), &mut buf, Alloc::global())?;
        Ok((buf.regions.iter().copied().collect(), buf.ends.iter().copied().collect()))
    };
    let got = read("fn a(f: fn(&'static S), s: S) { f(&s); }", &[rs], unit);
    assert_eq!(got, Ok((vec![Region::Static], vec![1])));
    let got = read("fn b(f: fn(&S) -> &S, s: &S) -> &S { f(s) }", &[rs], rs);
    assert_eq!(got, Ok((vec![Region::Anon(0), Region::Anon(0)], vec![1])));
    let got = read("fn c<F: for<'a> Fn(&'a S) -> &'a S>(f: F, s: &S) -> &S { f(s) }", &[rs], rs);
    assert_eq!(got, Ok((vec![Region::Named(0), Region::Named(0)], vec![1])));
    let got = read("fn d<F>(f: F, s: S) where F: Fn(&'static S) + Send { f(&s); }", &[rs], unit);
    assert_eq!(got, Ok((vec![Region::Static], vec![1])));
    let got = read("fn e(f: &mut dyn FnMut(&S, &S), s: &S) { f(s, s); }", &[rs, rs], unit);
    assert_eq!(got, Ok((vec![Region::Anon(0), Region::Anon(1)], vec![1, 2])));
    let got = read("fn g<'a>(f: impl Fn(&'a S), s: &'a S) { f(s); }", &[rs], unit);
    assert!(matches!(got, Err(CallableRefusal::OuterLifetime { .. })), "{got:?}");
    let got = read("fn h(f: Callback, s: &S) { f(s); }", &[rs], unit);
    assert!(matches!(got, Err(CallableRefusal::NotCallable { .. })), "{got:?}");
    let got = read("fn i<F: Fn(&S) + Fn(&'static S)>(f: F, s: &S) { f(s); }", &[rs], unit);
    assert!(matches!(got, Err(CallableRefusal::NotCallable { .. })), "{got:?}");
    std::println!("callable_parameter_types_are_read: 8 forms");
}

/// [`Sigs`] with one ADT that declares lifetimes (`life`), and closures lowered.
struct Wide {
    sigs: Sigs,
    life: Option<ItemKey>,
}

impl Wide {
    fn add(&mut self, fx: &Fx, item: ItemKey, text: &str, inputs: &[PTy], output: PTy) {
        let life = self.life;
        let mut buf = SigBuf::new_in(Alloc::global());
        match read_signature(text, fx.t.view(), inputs, output, &|k| Some(Some(k) == life), &mut buf, Alloc::global()) {
            Ok(()) => self.sigs.sigs.push((item, buf)),
            Err(why) => panic!("`{text}` was not read: {why:?}"),
        }
    }
}

impl Facts for Wide {
    fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
        Plain.adt(k).map(|f| AdtFacts { lifetimes: Some(k) == self.life, ..f })
    }
    fn callee(&self, k: ItemKey) -> Option<CalleeFacts> {
        self.sigs.callee(k)
    }
    fn sig(&self, item: ItemKey) -> Option<SigLife<'_>> {
        self.sigs.sig(item)
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

/// The findings of one body, one line each (or the refusal), and its flow's text.
fn findings_and_flow<F: Facts>(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId, facts: &F, plant: Plant) -> (Result<vec::Vec<String>, Refusal>, String) {
    let c = checked(fx, b, owner, root);
    let view = b.view(owner, root);
    let lowered = match lower_planted(&view, &c, facts, plant, Alloc::global()) {
        Ok(l) => l,
        Err(r) => return (Err(r), String::new()),
    };
    let mut text = String::new();
    lowered.flow.print(&mut text).expect("print");
    let rec = check(&lowered.flow, Alloc::global()).expect("bounded");
    let mut out = vec::Vec::new();
    for m in rec.moves.iter() {
        out.push(format!("move {:?} at {:?} span {}..{}", m.kind, m.at, m.span.lo, m.span.hi));
    }
    for f in rec.borrows.iter() {
        out.push(format!("borrow {f:?}"));
    }
    (Ok(out), text)
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
    let own = "fn fmt(b: B, s: &S) { b.field(s).field(s).finish(); }";
    let mut b = Bb::new();
    let (pb, ps) = (b.param("b"), b.param("s"));
    let (lb, s1, s2) = (b.local(pb), b.local(ps), b.local(ps));
    let c1 = b.method(lb, "field", &[s1]);
    let c2 = b.method(c1, "field", &[s2]);
    let c3 = b.method(c2, "finish", &[]);
    let st1 = b.semi(c3);
    let root = b.block(&[st1], None);
    let mut known = Wide { sigs: Sigs::new(), life: Some(bk) };
    known.add(&fx, fmt, own, &[bt, rs], unit);
    known.add(&fx, field, "fn field(&mut self, v: &S) -> &mut Self { self }", &[rmb, rs], rmb);
    known.add(&fx, finish, "fn finish(&mut self) {}", &[rmb], unit);
    let (got, flow) = findings_and_flow(&fx, &b, fmt, root, &known, Plant::None);
    std::println!("a_builder_chain_keeps_each_loan_in_its_result: {got:?}");
    assert_eq!(got, Ok(vec![]), "the flow:\n{flow}");
    // The heads unknown: refused by name; with the old relation put back: the census's finding.
    let mut unknown = Wide { sigs: Sigs::new(), life: Some(bk) };
    unknown.sigs.callees_known = false;
    unknown.add(&fx, fmt, own, &[bt, rs], unit);
    let (got, _) = findings_and_flow(&fx, &b, fmt, root, &unknown, Plant::None);
    assert!(matches!(got, Err(Refusal { form: Form::CalleeUnknown, .. })), "{got:?}");
    let (planted, flow) = findings_and_flow(&fx, &b, fmt, root, &unknown, Plant::UnknownUnified);
    std::println!("a_builder_chain_keeps_each_loan_in_its_result, planted UnknownUnified: {planted:?}");
    assert!(planted.as_ref().is_ok_and(|p| p.iter().any(|l| l.starts_with("borrow Invalidated"))), "{planted:?}; the flow:\n{flow}");
}

/// `struct S; fn g(_: &S); fn h(_: S); fn m(_: &mut S); fn f(s: S)` for the closure fixtures.
fn closure_world() -> (Fx, ItemKey) {
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
    let _g = fx.func("g", 0, &[rs], unit, vec![]);
    let _h = fx.func("h", 0, &[st], unit, vec![]);
    let _m = fx.func("m", 0, &[rms], unit, vec![]);
    let f = fx.func("f", 0, &[st], unit, vec![]);
    fx.finish();
    (fx, f)
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
    let (fx, f) = closure_world();
    let facts = Wide { sigs: Sigs::new(), life: None };
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
    let (fx, f) = closure_world();
    let facts = Wide { sigs: Sigs::new(), life: None };
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
    let (fx, f) = closure_world();
    let facts = Wide { sigs: Sigs::new(), life: None };
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
    let (fx, f) = closure_world();
    let facts = Wide { sigs: Sigs::new(), life: None };
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
