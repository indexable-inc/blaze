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
use blaze_typeck::{Checked, ExprId, InferCtx, ItemKey, Mutbl, Name, Needs, Outcome, PTy};

use crate::{AdtFacts, CalleeFacts, Facts, Form, Plant, Refusal, Region, SigBuf, SigLife, SigRefusal, lower_planted, read_signature};

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
    let got = findings_with(&fx, &b, leak, root, &sigs, Plant::None).expect("lowered");
    std::println!("an_unknown_callee_still_carries_its_argument_to_its_result: {got:?}");
    assert!(got.iter().any(|l| l.starts_with("borrow Invalidated")), "{got:?}");
}
