//! Piece RB8: a callee's trait lifetimes are the caller's at a call (trait_ties.rs). Each test
//! reads the heads from text as the census does, builds each fn's tie rows as blaze-ladder's
//! borrows_ties.rs files them (a trait method's implicit `Self: Trait<'params>` by the trait's
//! header, a written bound by `SigBuf::trait_bounds`), lowers a body checked by the real type
//! checker, and checks it with blaze-flow.
//!
//! Sources: the Reference, "Subtyping and Variance" (a trait's parameters are invariant);
//! "Traits" (a trait's items are generic over `Self: Trait<'params>`); "Trait and lifetime
//! bounds" (`for<'a>` quantifies the bound: it holds for every `'a`); "Lifetime elision";
//! RFC 2094 (a loan in a region the body does not choose is live at every point of it, so its
//! place dying is E0597: `borrow Invalidated`; a relation the signature does not state is
//! "lifetime may not live long enough": `borrow UnprovenOutlives`).
//!
//! Every allocation goes to one named buffer per test ([`Inline`]): no ambient heap is named.

use std::vec;

use blaze_own::Inline;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ExprId, ItemKey, Mutbl, Name, PTy};

use crate::sig::trait_header;
use crate::tests::findings_with;
use crate::{AdtFacts, CallBounds, CalleeFacts, Facts, Form, Plant, Refusal, Region, SigBuf, SigLife, TextBound, Tie, TieMiss, Ties, read_signature};

/// One test's buffer: every signature read and its scratch.
const BYTES: usize = 1 << 18;
type Buf<'x> = &'x Inline<BYTES>;

/// The callee's one parameter, `Self`, has a trait bound and no type-outlives bound.
const TRAIT_BOUNDED: &[u32] = &[0];

/// The heads' lifetime parts and tie rows, each read from text.
struct Heads<'x> {
    buf: Buf<'x>,
    sigs: vec::Vec<(ItemKey, SigBuf<Buf<'x>>)>,
    ties: vec::Vec<(ItemKey, vec::Vec<Tie>, vec::Vec<Region>)>,
}

impl<'x> Heads<'x> {
    fn new(buf: Buf<'x>) -> Heads<'x> {
        Heads { buf, sigs: vec::Vec::new(), ties: vec::Vec::new() }
    }

    fn read(&self, fx: &Fx, text: &str, inputs: &[PTy], output: PTy) -> SigBuf<Buf<'x>> {
        let mut out = SigBuf::new_in(self.buf);
        if let Err(why) = read_signature(text, fx.t.view(), inputs, output, &|_| Some(0), &mut out, self.buf) {
            panic!("`{text}` was not read: {why:?}");
        }
        out
    }

    /// A trait method: its signature, and its implicit `Self: Trait<'params>` row (param 0), each
    /// trait lifetime the region the method's text names it with (borrows_ties.rs).
    fn method(&mut self, fx: &Fx, item: ItemKey, trait_: ItemKey, trait_text: &str, text: &str, inputs: &[PTy], output: PTy) {
        let sig = self.read(fx, text, inputs, output);
        let mut names = blaze_own::Vec::new_in(self.buf);
        trait_header(trait_text, &mut names, self.buf).expect("a trait header");
        let mut regions = vec::Vec::new();
        for (i, n) in names.iter().enumerate() {
            regions.push(sig.names.iter().find(|(m, _)| m == n).map_or(Region::Named((sig.names.len() + i) as u32), |&(_, r)| r));
        }
        let rows = vec![Tie { param: 0, trait_, lo: 0, hi: regions.len() as u32, read: true }];
        self.ties.push((item, rows, regions));
        self.sigs.push((item, sig));
    }

    /// A fn: its signature, and a row per written trait bound, its bounded parameter found by
    /// name in `params` and its trait by name in `traits` (borrows_ties.rs resolves both through
    /// the head). A bound its `for<..>` quantifies is no row.
    fn func(&mut self, fx: &Fx, item: ItemKey, params: &[(&str, u32)], traits: &[(&str, ItemKey)], text: &str, inputs: &[PTy], output: PTy) {
        let sig = self.read(fx, text, inputs, output);
        let (mut rows, mut regions) = (vec::Vec::new(), vec::Vec::new());
        for tb in sig.trait_bounds.iter().filter(|tb| !tb.binder) {
            let param = params.iter().find(|(n, _)| Name::of(n) == tb.param).map(|&(_, i)| i).expect("a parameter of the fixture");
            let trait_ = traits.iter().find(|(n, _)| Name::of(n) == tb.trait_).map(|&(_, k)| k).expect("a trait of the fixture");
            let lo = regions.len() as u32;
            regions.extend(sig.trait_bound_regions[tb.lo as usize..tb.hi as usize].iter().copied());
            rows.push(Tie { param, trait_, lo, hi: regions.len() as u32, read: !tb.unread });
        }
        self.ties.push((item, rows, regions));
        self.sigs.push((item, sig));
    }
}

impl Facts for Heads<'_> {
    fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
        // As tests.rs's `Plain`: the parameter's drop at the body's end is the access a loan
        // live at every point conflicts with.
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: true, lifetimes: 0, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
    }
    fn sig(&self, item: ItemKey) -> Option<SigLife<'_>> {
        self.sigs.iter().find(|(k, _)| *k == item).map(|(_, s)| s.life())
    }
    fn call_bounds(&self, _: ItemKey) -> Option<CallBounds<'_>> {
        Some(CallBounds { outlives: &[], unread_below: 0, trait_bounded: TRAIT_BOUNDED })
    }
    fn ties(&self, item: ItemKey) -> Option<Ties<'_>> {
        self.ties.iter().find(|(k, _, _)| *k == item).map(|(_, rows, regions)| Ties { rows, regions })
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

const CONV: &str = "trait Conv<'a, 'b> { fn conv(self, x: &'a S) -> &'b S; }";

/// `recv.name(arg)` as the body's tail (`tail`) or its one statement, `arg` the parameter
/// `arg` borrowed (`borrow`) or as it is.
fn method_body(b: &mut Bb, recv: &str, name: &str, arg: &str, borrow: bool, tail: bool) -> ExprId {
    let (pr, pa) = (b.param(recv), b.param(arg));
    let (lr, la) = (b.local(pr), b.local(pa));
    let a = if borrow { b.borrow(Mutbl::Not, la) } else { la };
    let call = b.method(lr, name, &[a]);
    if tail {
        b.block(&[], Some(call))
    } else {
        let st = b.semi(call);
        b.block(&[st], None)
    }
}

/// MINIATURE (v), the tie of a generic caller of a generic callee. With `trait Conv<'a, 'b> {
/// fn conv(self, x: &'a S) -> &'b S; }`, `fn good<'a, 'b, T: Conv<'a, 'b>>(t: T, x: &'a S) ->
/// &'b S { t.conv(x) }` is right: the trait's `'a` is the caller's `'a`, its `'b` the caller's
/// `'b`. `fn swapped<'a, 'b, T: Conv<'b, 'a>>(..)` with the same body is wrong twice over (rustc:
/// "lifetime may not live long enough"): `x: &'a S` passed where the bound makes the trait's
/// `'a` the caller's `'b`, needing `'a: 'b`, which nothing states: `UnprovenOutlives`. The one
/// constraint tested: callee `Named(0)` (trait `'a`) equals the caller's placeholder of the
/// bound's first argument.
/// CONTROL (rule 23): `Plant::TiesFresh` (each trait lifetime fresh per call, as before this
/// piece) ACCEPTS `swapped`: the missed error.
/// Prints `a_trait_lifetime_is_the_callers good: Ok([])`, `.. swapped: Ok(["borrow
/// UnprovenOutlives ..])`, `.. swapped, fresh (control): Ok([])`.
#[test]
fn a_trait_lifetime_is_the_callers() {
    let inline = Inline::<BYTES>::new();
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let conv = fx.tr("Conv", 1, vec![], &[]);
    let p0 = fx.param(0);
    let conv_fn = fx.trait_fn(conv, "conv", true, &[p0, rs], rs);
    fx.use_trait(conv);
    let gb = fx.bound(conv, &[p0]);
    let good = fx.func("good", 1, &[p0, rs], rs, vec![gb]);
    let sb = fx.bound(conv, &[p0]);
    let swapped = fx.func("swapped", 1, &[p0, rs], rs, vec![sb]);
    fx.finish();
    let mut heads = Heads::new(&inline);
    heads.method(&fx, conv_fn, conv, CONV, "fn conv(self, x: &'a S) -> &'b S;", &[p0, rs], rs);
    let (ps, ts) = (&[("T", 0)][..], &[("Conv", conv)][..]);
    heads.func(&fx, good, ps, ts, "fn good<'a, 'b, T: Conv<'a, 'b>>(t: T, x: &'a S) -> &'b S { t.conv(x) }", &[p0, rs], rs);
    heads.func(&fx, swapped, ps, ts, "fn swapped<'a, 'b, T: Conv<'b, 'a>>(t: T, x: &'a S) -> &'b S { t.conv(x) }", &[p0, rs], rs);
    let mut b = Bb::new();
    let root = method_body(&mut b, "t", "conv", "x", false, true);
    let got = findings_with(&fx, &b, good, root, &heads, Plant::None);
    std::println!("a_trait_lifetime_is_the_callers good: {got:?}");
    assert_eq!(got, Ok(vec::Vec::new()), "the bound's lifetimes in order: clean");
    let got = findings_with(&fx, &b, swapped, root, &heads, Plant::None);
    std::println!("a_trait_lifetime_is_the_callers swapped: {got:?}");
    assert!(got.as_ref().is_ok_and(|l| l.iter().any(|l| l.starts_with("borrow UnprovenOutlives"))), "{got:?}");
    let got = findings_with(&fx, &b, swapped, root, &heads, Plant::TiesFresh);
    std::println!("a_trait_lifetime_is_the_callers swapped, fresh (control): {got:?}");
    assert_eq!(got, Ok(vec::Vec::new()), "the control: fresh trait lifetimes miss the error");
}

/// MINIATURE (i), serde's shape: `trait Visitor<'de> { fn take(self, s: &'de S); }`. `fn
/// keep<'de, V: Visitor<'de>>(v: V, s: &'de S) { v.take(s); }` is right. `fn local<'de, V:
/// Visitor<'de>>(v: V, s: S) { v.take(&s); }` is wrong (E0597, "`s` does not live long
/// enough"): the trait's `'de` is the caller's, a region the body does not choose, so the loan
/// of `s` is live at every point of the body and `s` dies under it: `Invalidated`. The one
/// constraint tested: callee `Named(0)` (trait `'de`) equals the caller's placeholder of `'de`,
/// a lifetime the caller writes only in its bound.
/// CONTROL (rule 23): `Plant::TiesFresh` ACCEPTS `local`.
/// Prints `a_visitors_lifetime_is_the_callers keep: Ok([])`, `.. local: Ok(["borrow Invalidated
/// ..])`, `.. local, fresh (control): Ok([])`.
#[test]
fn a_visitors_lifetime_is_the_callers() {
    let inline = Inline::<BYTES>::new();
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let unit = fx.unit();
    let vis = fx.tr("Visitor", 1, vec![], &[]);
    let p0 = fx.param(0);
    let take = fx.trait_fn(vis, "take", true, &[p0, rs], unit);
    fx.use_trait(vis);
    let kb = fx.bound(vis, &[p0]);
    let keep = fx.func("keep", 1, &[p0, rs], unit, vec![kb]);
    let lb = fx.bound(vis, &[p0]);
    let local = fx.func("local", 1, &[p0, st], unit, vec![lb]);
    fx.finish();
    let mut heads = Heads::new(&inline);
    heads.method(&fx, take, vis, "pub trait Visitor<'de>: Sized { fn take(self, s: &'de S); }", "fn take(self, s: &'de S);", &[p0, rs], unit);
    let (ps, ts) = (&[("V", 0)][..], &[("Visitor", vis)][..]);
    heads.func(&fx, keep, ps, ts, "fn keep<'de, V: Visitor<'de>>(v: V, s: &'de S) { v.take(s); }", &[p0, rs], unit);
    heads.func(&fx, local, ps, ts, "fn local<'de, V: Visitor<'de>>(v: V, s: S) { v.take(&s); }", &[p0, st], unit);
    let mut b = Bb::new();
    let root = method_body(&mut b, "v", "take", "s", false, false);
    let got = findings_with(&fx, &b, keep, root, &heads, Plant::None);
    std::println!("a_visitors_lifetime_is_the_callers keep: {got:?}");
    assert_eq!(got, Ok(vec::Vec::new()), "a `&'de S` passed on: clean");
    let mut b = Bb::new();
    let root = method_body(&mut b, "v", "take", "s", true, false);
    let got = findings_with(&fx, &b, local, root, &heads, Plant::None);
    std::println!("a_visitors_lifetime_is_the_callers local: {got:?}");
    assert!(got.as_ref().is_ok_and(|l| l.iter().any(|l| l.starts_with("borrow Invalidated"))), "E0597: {got:?}");
    let got = findings_with(&fx, &b, local, root, &heads, Plant::TiesFresh);
    std::println!("a_visitors_lifetime_is_the_callers local, fresh (control): {got:?}");
    assert_eq!(got, Ok(vec::Vec::new()), "the control: a fresh `'de` misses E0597");
}

/// A caller that writes no bound for the parameter the callee's tie names (its bound is its
/// impl's header's, or a supertrait's): the tied region is not known, so the call is REFUSED by
/// name (`CallTieUnplaced { why: NoOwnBound }`), never related with a fresh one. Control: a
/// callee whose tied region its signature never writes relates nothing and is lowered.
/// Prints `a_tie_with_no_answer_is_refused: Err(Refusal { form: CallTieUnplaced { .., param: 0,
/// why: NoOwnBound }, .. })`, `.. unwritten: Ok([])`.
#[test]
fn a_tie_with_no_answer_is_refused() {
    let inline = Inline::<BYTES>::new();
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let unit = fx.unit();
    let vis = fx.tr("Visitor", 1, vec![], &[]);
    let p0 = fx.param(0);
    let take = fx.trait_fn(vis, "take", true, &[p0, rs], unit);
    let skip = fx.trait_fn(vis, "skip", true, &[p0, rs], unit);
    fx.use_trait(vis);
    let ob = fx.bound(vis, &[p0]);
    let owner = fx.func("owner", 1, &[p0, rs], unit, vec![ob]);
    fx.finish();
    let header = "trait Visitor<'de> { fn take(self, s: &'de S); fn skip(self, s: &S); }";
    let mut heads = Heads::new(&inline);
    heads.method(&fx, take, vis, header, "fn take(self, s: &'de S);", &[p0, rs], unit);
    heads.method(&fx, skip, vis, header, "fn skip(self, s: &S);", &[p0, rs], unit);
    // The owner's text writes no `V: Visitor<'de>`: its rows are known and hold none for `V`.
    heads.func(&fx, owner, &[("V", 0)], &[("Visitor", vis)], "fn owner<V>(v: V, s: &S) { v.take(s); }", &[p0, rs], unit);
    let mut b = Bb::new();
    let root = method_body(&mut b, "v", "take", "s", false, false);
    let got = findings_with(&fx, &b, owner, root, &heads, Plant::None);
    std::println!("a_tie_with_no_answer_is_refused: {got:?}");
    assert!(matches!(got, Err(Refusal { form: Form::CallTieUnplaced { param: 0, why: TieMiss::NoOwnBound, .. }, .. })), "{got:?}");
    let mut b = Bb::new();
    let root = method_body(&mut b, "v", "skip", "s", false, false);
    let got = findings_with(&fx, &b, owner, root, &heads, Plant::None);
    std::println!("a_tie_with_no_answer_is_refused unwritten: {got:?}");
    assert_eq!(got, Ok(vec::Vec::new()), "`'de` is not in `skip`'s signature: nothing to tie");
}

/// The reader's half (sig/outlives.rs): a written bound's lifetime arguments in the trait's
/// order, as regions of the fn's own numbering (`'a` first written in `x: &'a S` is `Named(0)`);
/// a bound its `for<..>` quantifies is a `binder` row with no regions; a bound with no lifetime
/// argument is no row; and a trait's header gives its lifetime parameters in order.
#[test]
fn trait_bounds_and_headers_are_read() {
    let inline = Inline::<BYTES>::new();
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let p0 = fx.param(0);
    let p1 = fx.param(1);
    let unit = fx.unit();
    fx.finish();
    let heads = Heads::new(&inline);
    let sig = heads.read(&fx, "fn g<'a, 'b, T: Conv<'b, 'a>>(t: T, x: &'a S) -> &'b S { t.conv(x) }", &[p0, rs], rs);
    let rows: vec::Vec<TextBound> = sig.trait_bounds.iter().copied().collect();
    let want = TextBound { param: Name::of("T"), trait_: Name::of("Conv"), lo: 0, hi: 2, unread: false, binder: false };
    assert_eq!(rows, vec![want]);
    assert_eq!(&sig.trait_bound_regions[..], &[Region::Named(1), Region::Named(0)][..]);
    let sig = heads.read(&fx, "fn d<T, U>(t: T, u: U) where T: for<'de> Deserialize<'de> + Clone, U: Seq<'static> {}", &[p0, p1], unit);
    let rows: vec::Vec<(Name, bool, u32)> = sig.trait_bounds.iter().map(|b| (b.trait_, b.binder, b.hi - b.lo)).collect();
    assert_eq!(rows, vec![(Name::of("Deserialize"), true, 0), (Name::of("Seq"), false, 1)]);
    assert_eq!(&sig.trait_bound_regions[..], &[Region::Static][..]);
    let mut names = blaze_own::Vec::new_in(&inline);
    let got = trait_header("pub trait Conv<'a, 'b, T: ?Sized>: Sized where T: 'a { fn conv(self); }", &mut names, &inline);
    assert_eq!(got, Ok(Name::of("Conv")));
    let lifetimes: vec::Vec<Name> = names.iter().copied().collect();
    let mut want = blaze_own::Vec::new_in(&inline);
    let _ = trait_header("trait W<'a, 'b> {}", &mut want, &inline);
    assert_eq!(lifetimes.len(), 2, "{lifetimes:?}");
    assert_eq!(&lifetimes[..], &want[..], "the same lifetimes, by name, in order");
    names.clear();
    assert_eq!(trait_header("trait Plain<T> {}", &mut names, &inline), Ok(Name::of("Plain")));
    assert!(names.is_empty(), "no lifetime parameter");
    std::println!("trait_bounds_and_headers_are_read: {rows:?}");
}
