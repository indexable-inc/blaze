//! Piece B7: calls related through projections (generic_call.rs, "OPEN PROJECTIONS" and "A
//! PART WITH NO POSITION"). Each test reads the heads from text as the census does (lifetime
//! part, generic shape, tie rows as blaze-ladder borrows_ties.rs files them), lowers a body the
//! real type checker checked, and checks it with blaze-flow. NEVER COMPILED when written
//! (2026-11-18, lane B7).
//!
//! Sources: the Reference, "Implementations", "Generic implementations" (E0207: an impl's
//! lifetime in an associated type is in its self type or its trait); "Subtyping and Variance"
//! (a trait's parameters are invariant); RFC 1214 (a projection outlives `'x` when its
//! components do); RFC 2094 (a loan in a region the body does not choose is live at every
//! point of it: its place dying is `borrow Invalidated`; a relation the signature does not
//! state is `borrow UnprovenOutlives`).
//!
//! Every allocation of the heads goes to one named buffer per test ([`Inline`]).

use std::vec;

use blaze_own::Inline;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ExprId, ItemKey, Mutbl, Name, PTy, PatId};

use crate::sig::trait_header;
use crate::tests::findings_with;
use crate::{AdtFacts, AssocRead, CallBounds, CalleeFacts, Facts, Plant, Region, Shape, ShapeBuf, SigBuf, SigLife, Tie, Ties, read_shape, read_signature};

const BYTES: usize = 1 << 18;
type Buf<'x> = &'x Inline<BYTES>;

/// Every head's one type parameter has a trait bound and no type-outlives bound.
const TRAIT_BOUNDED: &[u32] = &[0];

/// The heads: lifetime parts, shapes and tie rows read from text; which write lifetimes and
/// which take `&self`/`&mut self`; the one ADT with a field (`W { k }`).
struct Heads<'x> {
    buf: Buf<'x>,
    sigs: vec::Vec<(ItemKey, SigBuf<Buf<'x>>)>,
    shapes: vec::Vec<(ItemKey, ShapeBuf<Buf<'x>>)>,
    ties: vec::Vec<(ItemKey, vec::Vec<Tie>, vec::Vec<Region>)>,
    writes: vec::Vec<ItemKey>,
    self_ref: vec::Vec<ItemKey>,
    with_field: Option<ItemKey>,
}

impl<'x> Heads<'x> {
    fn new(buf: Buf<'x>) -> Heads<'x> {
        Heads { buf, sigs: vec::Vec::new(), shapes: vec::Vec::new(), ties: vec::Vec::new(), writes: vec::Vec::new(), self_ref: vec::Vec::new(), with_field: None }
    }

    /// The lifetime part and the shape of `item`'s head (every trait asked declares a
    /// lifetime; no associated type is generic).
    fn read(&mut self, fx: &Fx, item: ItemKey, text: &str, inputs: &[PTy], output: PTy) -> SigBuf<Buf<'x>> {
        let mut sig = SigBuf::new_in(self.buf);
        if let Err(why) = read_signature(text, fx.t.view(), inputs, output, &|_| Some(0), &mut sig, self.buf) {
            panic!("`{text}` was not read: {why:?}");
        }
        let mut shape = ShapeBuf::new_in(self.buf);
        let assoc = |_: ItemKey, _: Name| Some(AssocRead { trait_lifetimes: true, generic: false });
        if let Err(why) = read_shape(fx.t.view(), inputs, output, &|_| Some(0), &assoc, &mut shape, self.buf) {
            panic!("`{text}` has no shape: {why:?}");
        }
        self.shapes.push((item, shape));
        sig
    }

    /// A trait method and its implicit `Self: Trait<'params>` row (borrows_ties.rs).
    fn method(&mut self, fx: &Fx, item: ItemKey, trait_: ItemKey, trait_text: &str, text: &str, inputs: &[PTy], output: PTy) {
        let sig = self.read(fx, item, text, inputs, output);
        let mut names = blaze_own::Vec::new_in(self.buf);
        trait_header(trait_text, &mut names, self.buf).expect("a trait header");
        let mut regions = vec::Vec::new();
        for (i, n) in names.iter().enumerate() {
            regions.push(sig.names.iter().find(|(m, _)| m == n).map_or(Region::Named((sig.names.len() + i) as u32), |&(_, r)| r));
        }
        self.ties.push((item, vec![Tie { param: 0, trait_, lo: 0, hi: regions.len() as u32, read: true }], regions));
        self.sigs.push((item, sig));
    }

    /// A fn and a row per written trait bound, its parameter `Param(0)` and its trait `trait_`.
    fn func(&mut self, fx: &Fx, item: ItemKey, trait_: Option<ItemKey>, text: &str, inputs: &[PTy], output: PTy) {
        let sig = self.read(fx, item, text, inputs, output);
        let (mut rows, mut regions) = (vec::Vec::new(), vec::Vec::new());
        for tb in sig.trait_bounds.iter().filter(|tb| !tb.binder) {
            let lo = regions.len() as u32;
            regions.extend(sig.trait_bound_regions[tb.lo as usize..tb.hi as usize].iter().copied());
            rows.push(Tie { param: 0, trait_: trait_.expect("a trait of the fixture"), lo, hi: regions.len() as u32, read: !tb.unread });
        }
        self.ties.push((item, rows, regions));
        self.sigs.push((item, sig));
    }
}

impl Facts for Heads<'_> {
    fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: true, lifetimes: 0, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, item: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: self.writes.contains(&item), self_ref: self.self_ref.contains(&item) })
    }
    fn sig(&self, item: ItemKey) -> Option<SigLife<'_>> {
        self.sigs.iter().find(|(k, _)| *k == item).map(|(_, s)| s.life())
    }
    fn shape(&self, item: ItemKey) -> Option<Shape<'_>> {
        self.shapes.iter().find(|(k, _)| *k == item).map(|(_, s)| s.shape())
    }
    fn call_bounds(&self, _: ItemKey) -> Option<CallBounds<'_>> {
        Some(CallBounds { outlives: &[], unread_below: 0, trait_bounded: TRAIT_BOUNDED })
    }
    fn ties(&self, item: ItemKey) -> Option<Ties<'_>> {
        self.ties.iter().find(|(k, _, _)| *k == item).map(|(_, rows, regions)| Ties { rows, regions })
    }
    fn field(&self, adt: ItemKey, _: u32, name: Name) -> Option<u32> {
        (Some(adt) == self.with_field && name == Name::of("k")).then_some(0)
    }
    fn field_count(&self, adt: ItemKey, _: u32) -> Option<u32> {
        (Some(adt) == self.with_field).then_some(1)
    }
    fn param_copy(&self, _: u32) -> bool {
        false
    }
}

fn clean() -> Result<vec::Vec<std::string::String>, crate::Refusal> {
    Ok(vec::Vec::new())
}

fn any(got: &Result<vec::Vec<std::string::String>, crate::Refusal>, prefix: &str) -> bool {
    got.as_ref().is_ok_and(|l| l.iter().any(|l| l.starts_with(prefix)))
}

/// SERDE (i), `&'de str` kept past `'de`, with an open projection in the output. `trait
/// De<'de> { type Error; fn take(self) -> (&'de S, Self::Error); }`. `fn good<'de, D:
/// De<'de>>(d: D) -> (&'de S, D::Error) { d.take() }` is right: the call's `'de` is the
/// caller's (the tie), and `D::Error` is the caller's opaque type, whose inputs `'de` and
/// `D` outlive the caller's own `D::Error` placeholder (`own_alias_known`). `fn bad<'de, 'x, D:
/// De<'de>>(d: D) -> (&'x S, D::Error)` is wrong (rustc: "lifetime may not live long enough"):
/// the `&'de S` is kept as `&'x S`. The one constraint the negative tests: the trait's `'de`,
/// an input of the open projection, is the caller's placeholder.
/// `bad`'s three findings (origins numbered as made: `'x` 0, the return's `D::Error`
/// placeholder `ρ` 1, then `ρ_D`, the caller's `'de`): `'x: 'de` and `'de: 'x` (the kept
/// `&'de S` against the return's `&'x S`, both ways because the lowering relates a position
/// under the top of a returned value invariantly), and `'x: ρ` (`'x` reaches the call's `'de`
/// through that same relation, and `'de` is an input of `ρ`): the same mismatch seen through
/// the projection, true of `bad` whether or not the tie holds.
/// CONTROLS (rule 23): `Plant::TiesFresh` (a fresh `'de`) drops the two findings between `'x`
/// and the caller's `'de` and keeps only `'x: ρ`; `Plant::NoKnown` takes the known `'de: ρ`
/// away and `good` is no longer clean.
/// Prints `open_projection good: Ok([])`, `.. bad: Ok([three "borrow UnprovenOutlives .."])`,
/// `.. bad, fresh (control): Ok([the `'x: ρ` one])`, `.. good, no known (control): <not
/// Ok([])>`.
#[test]
fn a_kept_de_through_an_open_projection() {
    let inline = Inline::<BYTES>::new();
    let mut fx = Fx::new();
    // A tuple's elements but the last are `Sized` (rustc wf.rs `TupleElem`): the solver's
    // well-formedness of `(&'de S, D::Error)` asks the `Sized` lang item, which the world must
    // declare (before any `Fx::func`, whose implied `Sized` clauses read it).
    let sized = fx.tr("Sized", 1, vec![], &[]);
    fx.lang(blaze_typeck::LangItem::Sized, sized);
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let de = fx.tr("De", 1, vec![], &["Error"]);
    let p0 = fx.param(0);
    let err = fx.alias(de, "Error", &[p0]);
    let out = fx.tuple(&[rs, err]);
    let take = fx.trait_fn(de, "take", true, &[p0], out);
    fx.use_trait(de);
    let gb = fx.bound(de, &[p0]);
    let good = fx.func("good", 1, &[p0], out, vec![gb]);
    let bb = fx.bound(de, &[p0]);
    let bad = fx.func("bad", 1, &[p0], out, vec![bb]);
    fx.finish();
    let mut heads = Heads::new(&inline);
    let header = "pub trait De<'de> { type Error; fn take(self) -> (&'de S, Self::Error); }";
    heads.method(&fx, take, de, header, "fn take(self) -> (&'de S, Self::Error);", &[p0], out);
    heads.writes.push(take);
    heads.func(&fx, good, Some(de), "fn good<'de, D: De<'de>>(d: D) -> (&'de S, D::Error) { d.take() }", &[p0], out);
    heads.func(&fx, bad, Some(de), "fn bad<'de, 'x, D: De<'de>>(d: D) -> (&'x S, D::Error) { d.take() }", &[p0], out);
    let mut b = Bb::new();
    let pd = b.param("d");
    let ld = b.local(pd);
    let call = b.method(ld, "take", &[]);
    let root = b.block(&[], Some(call));
    let got = findings_with(&fx, &b, good, root, &heads, Plant::None);
    std::println!("open_projection good: {got:?}");
    assert_eq!(got, clean(), "the caller's `'de` and its own `D::Error`: clean");
    let got = findings_with(&fx, &b, bad, root, &heads, Plant::None);
    std::println!("open_projection bad: {got:?}");
    assert!(any(&got, "borrow UnprovenOutlives"), "`'de: 'x` is not stated: {got:?}");
    let tied = got;
    let got = findings_with(&fx, &b, bad, root, &heads, Plant::TiesFresh);
    std::println!("open_projection bad, fresh (control): {got:?}");
    // The control removes the tie's findings only: each one it keeps is one of `bad`'s, and
    // at least one of `bad`'s (those between `'x` and the caller's `'de`) is gone.
    let (Ok(tied), Ok(fresh)) = (&tied, &got) else { panic!("both lowered: {tied:?} {got:?}") };
    assert!(fresh.iter().all(|f| tied.contains(f)), "the control adds no finding: {fresh:?} vs {tied:?}");
    assert!(fresh.len() < tied.len(), "the control: a fresh `'de` misses the tie's errors: {fresh:?} vs {tied:?}");
    let got = findings_with(&fx, &b, good, root, &heads, Plant::NoKnown);
    std::println!("open_projection good, no known (control): {got:?}");
    assert_ne!(got, clean(), "the control: without `'de: ρ` the return is not proven");
}

/// `let x = a.next(); a.next(); <tail>` over `fn <name><'de, A: Seq<'de>>(a: &mut A) ->
/// A::Error`, `moves` times `eat(a);` before the tail `x`.
fn next_twice(b: &mut Bb, moves: u32) -> ExprId {
    let pa = b.param("a");
    let la = b.local(pa);
    let first = b.method(la, "next", &[]);
    let px = b.bind("x");
    let mut stmts = vec![b.let_(px, None, first)];
    let la = b.local(pa);
    let second = b.method(la, "next", &[]);
    stmts.push(b.semi(second));
    for _ in 0..moves {
        let f = b.path(&["eat"]);
        let la = b.local(pa);
        let call = b.call(f, &[la]);
        stmts.push(b.semi(call));
    }
    let lx = b.local(px);
    b.block(&stmts, Some(lx))
}

/// SERDE (ii), `next_element` twice, then the access moved twice. `trait Seq<'de> { type
/// Error; fn next(&mut self) -> Self::Error; }`: the result is `A::Error`, whose inputs are
/// `A` and `'de`, never the `&mut self` loan. `visit`: `let x = a.next(); a.next(); x` is
/// right (rustc accepts). `twice`: the same, then `eat(a); eat(a);` is E0382 (`a: &mut A` is
/// not `Copy`), and only that: no conflict of the two `next`s.
/// CONTROLS (rule 23): `Plant::HiddenCoarse` (a hidden output through the coarse path, as
/// before this piece) makes `visit` unclean (the hub puts the first `&mut` loan into `x`);
/// `Plant::AllCopy` takes `twice`'s E0382 away.
/// Prints `next_twice visit: Ok([])`, `.. visit, coarse (control): <not Ok([])>`, `.. twice:
/// Ok(["move ..])`, `.. twice, all copy (control): Ok([])`.
#[test]
fn next_element_twice_holds_no_loan() {
    let inline = Inline::<BYTES>::new();
    let mut fx = Fx::new();
    let seq = fx.tr("Seq", 1, vec![], &["Error"]);
    let p0 = fx.param(0);
    let rm = fx.r(Mutbl::Mut, p0);
    let err = fx.alias(seq, "Error", &[p0]);
    let unit = fx.unit();
    let next = fx.trait_fn(seq, "next", true, &[rm], err);
    let eat = fx.func("eat", 1, &[p0], unit, vec![]);
    fx.use_trait(seq);
    let vb = fx.bound(seq, &[p0]);
    let visit = fx.func("visit", 1, &[rm], err, vec![vb]);
    let tb = fx.bound(seq, &[p0]);
    let twice = fx.func("twice", 1, &[rm], err, vec![tb]);
    fx.finish();
    let mut heads = Heads::new(&inline);
    let header = "pub trait Seq<'de> { type Error; fn next(&mut self) -> Self::Error; }";
    heads.method(&fx, next, seq, header, "fn next(&mut self) -> Self::Error;", &[rm], err);
    heads.self_ref.push(next);
    heads.func(&fx, eat, None, "fn eat<T>(t: T) {}", &[p0], unit);
    let text = "fn visit<'de, A: Seq<'de>>(a: &mut A) -> A::Error { let x = a.next(); a.next(); x }";
    heads.func(&fx, visit, Some(seq), text, &[rm], err);
    let text = "fn twice<'de, A: Seq<'de>>(a: &mut A) -> A::Error { let x = a.next(); a.next(); eat(a); eat(a); x }";
    heads.func(&fx, twice, Some(seq), text, &[rm], err);
    let mut b = Bb::new();
    let root = next_twice(&mut b, 0);
    let got = findings_with(&fx, &b, visit, root, &heads, Plant::None);
    std::println!("next_twice visit: {got:?}");
    assert_eq!(got, clean(), "the first result holds no `&mut` loan: clean");
    let got = findings_with(&fx, &b, visit, root, &heads, Plant::HiddenCoarse);
    std::println!("next_twice visit, coarse (control): {got:?}");
    assert_ne!(got, clean(), "the control: the coarse path is not clean");
    let mut b = Bb::new();
    let root = next_twice(&mut b, 2);
    let got = findings_with(&fx, &b, twice, root, &heads, Plant::None);
    std::println!("next_twice twice: {got:?}");
    assert!(any(&got, "move"), "E0382: {got:?}");
    assert!(!any(&got, "borrow"), "only the move: {got:?}");
    let got = findings_with(&fx, &b, twice, root, &heads, Plant::AllCopy);
    std::println!("next_twice twice, all copy (control): {got:?}");
    assert!(!any(&got, "move"), "the control: no move, no E0382: {got:?}");
}

/// `base.name`: the fixture has no field-access builder, so it takes the slot of a read of `p`
/// (as field_regions_tests.rs `field` does).
fn field(b: &mut Bb, p: PatId, base: ExprId, name: &str) -> ExprId {
    let at = b.local(p);
    b.exprs[at.ix()] = blaze_typeck::Expr::Field { base, name: Name::of(name) };
    at
}

/// THE CENSUS'S `Unaligned(Positions { want: 0, have: 0, exact: false })` (bstd
/// `self.why.says()`, core `self.0.wrapping_shl(n)`): an operand read from a field (an inexact
/// place) whose value has no position. `fn keep(k: K, s: &'static S)`; `fn good(w: W, s:
/// &'static S) { keep(w.k, s) }` is right; `fn bad(w: W, s: S) { keep(w.k, &s) }` is E0597
/// (`s` does not live for ever): `borrow Invalidated`. Before this piece both were refused
/// (`CallWritesLifetimes { lacks: Unaligned(..) }`). The one constraint the negative tests: the
/// second operand outlives the callee's `'static`.
/// CONTROL (rule 23): `Plant::StaticIgnored` (`'static` fresh per call) ACCEPTS `bad`.
/// Prints `zero_part good: Ok([])`, `.. bad: Ok(["borrow Invalidated ..])`, `.. bad, static
/// ignored (control): Ok([])`.
#[test]
fn an_operand_with_no_position_relates_nothing() {
    let inline = Inline::<BYTES>::new();
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let k = fx.strukt("K", 0, &[]);
    let kt = fx.adt(k, &[]);
    let w = fx.strukt("W", 0, &[("k", kt)]);
    let wt = fx.adt(w, &[]);
    let unit = fx.unit();
    let keep = fx.func("keep", 0, &[kt, rs], unit, vec![]);
    let good = fx.func("good", 0, &[wt, rs], unit, vec![]);
    let bad = fx.func("bad", 0, &[wt, st], unit, vec![]);
    fx.finish();
    let mut heads = Heads::new(&inline);
    heads.with_field = Some(w);
    heads.func(&fx, keep, None, "fn keep(k: K, s: &'static S) {}", &[kt, rs], unit);
    heads.writes.push(keep);
    heads.func(&fx, good, None, "fn good(w: W, s: &'static S) { keep(w.k, s) }", &[wt, rs], unit);
    heads.func(&fx, bad, None, "fn bad(w: W, s: S) { keep(w.k, &s) }", &[wt, st], unit);
    for (owner, borrow) in [(good, false), (bad, true)] {
        let mut b = Bb::new();
        let (pw, ps) = (b.param("w"), b.param("s"));
        let base = b.local(pw);
        let wk = field(&mut b, pw, base, "k");
        let ls = b.local(ps);
        let arg = if borrow { b.borrow(Mutbl::Not, ls) } else { ls };
        let f = b.path(&["keep"]);
        let call = b.call(f, &[wk, arg]);
        let stmt = b.semi(call);
        let root = b.block(&[stmt], None);
        let got = findings_with(&fx, &b, owner, root, &heads, Plant::None);
        if borrow {
            std::println!("zero_part bad: {got:?}");
            assert!(any(&got, "borrow Invalidated"), "E0597: {got:?}");
            let got = findings_with(&fx, &b, owner, root, &heads, Plant::StaticIgnored);
            std::println!("zero_part bad, static ignored (control): {got:?}");
            assert_eq!(got, clean(), "the control: a fresh `'static` misses E0597");
        } else {
            std::println!("zero_part good: {got:?}");
            assert_eq!(got, clean(), "a field with no position relates nothing: clean");
        }
    }
}
