//! Piece B7: calls related through projections (generic_call.rs, "OPEN PROJECTIONS" and "A
//! PART WITH NO POSITION"). Each test gives the heads as the head records file them (D148: a
//! fn's lifetime parameters and its signature column, made by hand from the head the test
//! declares), each head's generic shape read with the same counts, and tie rows as
//! blaze-ladder borrows_ties.rs files them; lowers a body the real type checker checked, and
//! checks it with blaze-flow. NEVER COMPILED when written (2026-11-18, lane B7; ported to
//! region records 2026-11-19).
//!
//! Numbering (regions.rs): `Param(i)` is the container's lifetimes, then the item's own in
//! declaration order, then one per elided input; slots in the type's pre-order. A projection
//! `T::Error` of `trait De<'de>` has one slot, the trait's `'de` as the bound writes it.
//!
//! Sources: the Reference, "Implementations", "Generic implementations" (E0207: an impl's
//! lifetime in an associated type is in its self type or its trait); "Subtyping and Variance"
//! (a trait's parameters are invariant); RFC 1214 (a projection outlives `'x` when its
//! components do); RFC 2094 (a loan in a region the body does not choose is live at every
//! point of it: its place dying is `borrow Invalidated`; a relation the signature does not
//! state is `borrow UnprovenOutlives`).
//!
//! The region heads are declared with the shared fixture (region_fixture.rs [`Decls`], laid in
//! the checked record's own table, the same counts as [`Counts`]) and the lowering runs through
//! region_fixture.rs `findings`; every allocation of the shapes goes to one named buffer per
//! test ([`Inline`]).

use std::vec;

use blaze_own::Inline;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::regions::{Arity, Count};
use blaze_typeck::{ExprId, ItemKey, Mutbl, Name, PNode, PTy, PatId};

use crate::region_fixture::{Decls, FnHead, findings};
use crate::{AdtFacts, CallBounds, CalleeFacts, Facts, Plant, Region, Shape, ShapeBuf, Tie, Ties, read_shape};

const BYTES: usize = 1 << 18;
type Buf<'x> = &'x Inline<BYTES>;

/// Every head's one type parameter has a trait bound and no type-outlives bound.
const TRAIT_BOUNDED: &[u32] = &[0];

/// The counts the columns and shapes are laid by: no ADT declares a lifetime; every trait
/// asked declares one, and no associated type is generic, so a projection has one slot.
struct Counts;

impl Arity for Counts {
    fn adt_lifetimes(&self, _: ItemKey) -> Count {
        Count::Known(0)
    }
    fn opaque_captures(&self, _: ItemKey) -> Count {
        Count::NotKnown
    }
    fn alias_lifetimes(&self, _: ItemKey, _: Name) -> Count {
        Count::Known(1)
    }
    fn trait_lifetimes(&self, _: ItemKey) -> Count {
        Count::Known(1)
    }
}

/// The heads: their region heads ([`Decls`]), shapes and tie rows; which write lifetimes and
/// which take `&self`/`&mut self`; the one ADT with a field (`W { k }`).
struct Heads<'x> {
    buf: Buf<'x>,
    decls: Decls,
    shapes: vec::Vec<(ItemKey, ShapeBuf<Buf<'x>>)>,
    ties: vec::Vec<(ItemKey, vec::Vec<Tie>, vec::Vec<Region>)>,
    writes: vec::Vec<ItemKey>,
    self_ref: vec::Vec<ItemKey>,
    with_field: Option<ItemKey>,
}

impl<'x> Heads<'x> {
    /// `traits`: the traits of the world, each declaring one lifetime and one associated type
    /// `assoc` with no lifetime of its own (as [`Counts`] says of every trait).
    fn new(buf: Buf<'x>, traits: &[(ItemKey, &str)]) -> Heads<'x> {
        let mut decls = Decls::new();
        for &(t, assoc) in traits {
            decls.trait_(t, 1);
            decls.alias(t, assoc, 1);
        }
        Heads { buf, decls, shapes: vec::Vec::new(), ties: vec::Vec::new(), writes: vec::Vec::new(), self_ref: vec::Vec::new(), with_field: None }
    }

    /// `item`'s region head (`count` lifetimes, those in `late` late-bound, its column over
    /// `fn(inputs..) -> output` `slots`) and its shape by [`Counts`].
    #[expect(clippy::too_many_arguments, reason = "one head of the fixture, every part named")]
    fn add(&mut self, fx: &mut Fx, item: ItemKey, count: u32, late: &[u32], inputs: &[PTy], output: PTy, slots: &[Region]) {
        self.decls.func(item, FnHead { lifetimes: count, late, outlives: &[], inputs, output, slots });
        let mut shape = ShapeBuf::new_in(self.buf);
        if let Err(why) = read_shape(fx.t.view(), inputs, output, &Counts, &mut shape, self.buf) {
            panic!("{item:?} has no shape: {why:?}");
        }
        self.shapes.push((item, shape));
    }

    /// A trait method of a trait with one lifetime: its head (the trait's `'de` its own
    /// `Param(0)`, the container's first) and its implicit `Self: Trait<'de>` as a row
    /// (borrows_ties.rs, the old model) and as clause column 0 (heads_regions.rs
    /// `trait_inherited`, the one model).
    #[expect(clippy::too_many_arguments, reason = "one head of the fixture, every part named")]
    fn method(&mut self, fx: &mut Fx, item: ItemKey, trait_: ItemKey, count: u32, late: &[u32], inputs: &[PTy], output: PTy, slots: &[Region]) {
        self.add(fx, item, count, late, inputs, output, slots);
        self.ties.push((item, vec![Tie { param: 0, trait_, lo: 0, hi: 1, read: true }], vec![Region::Param(0)]));
        self.bound_clause(fx, item, trait_);
    }

    /// A fn and, with `bound`, its one written bound `T: Trait<'de>` over its `Param(0)` type,
    /// `'de` its own `Param(0)`: a row and clause column 0.
    #[expect(clippy::too_many_arguments, reason = "one head of the fixture, every part named")]
    fn func(&mut self, fx: &mut Fx, item: ItemKey, bound: Option<ItemKey>, count: u32, late: &[u32], inputs: &[PTy], output: PTy, slots: &[Region]) {
        self.add(fx, item, count, late, inputs, output, slots);
        let (rows, regions) = match bound {
            Some(trait_) => (vec![Tie { param: 0, trait_, lo: 0, hi: 1, read: true }], vec![Region::Param(0)]),
            None => (vec::Vec::new(), vec::Vec::new()),
        };
        self.ties.push((item, rows, regions));
        if let Some(trait_) = bound {
            self.bound_clause(fx, item, trait_);
        }
    }

    /// `item`'s clause 0, `Param(0): trait_<'de>` (`ExTrait(trait_, [Param(0)])` over
    /// `[Param(0)]`).
    fn bound_clause(&mut self, fx: &mut Fx, item: ItemKey, trait_: ItemKey) {
        let subject = fx.param(0);
        let list = fx.list(&[subject]);
        let clause = fx.ty(PNode::ExTrait(trait_, list));
        self.decls.trait_(trait_, 1);
        self.decls.clause(item, 0, 0, clause, &[Region::Param(0)]);
    }
}

impl Facts for Heads<'_> {
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
    fn object_trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<bool> {
        None
    }
    fn trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<u32> {
        None
    }
    fn alias_lifetimes(&self, _: blaze_typeck::ItemKey, _: blaze_typeck::Name) -> Option<u32> {
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
    fn callee(&self, item: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: self.writes.contains(&item), self_ref: self.self_ref.contains(&item) })
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
    // `fn take(self) -> (&'de S, Self::Error)`: the trait's `'de` `Param(0)`; the output's
    // `&'de S` and `Self::Error`'s `'de`: `[Param(0), Param(0)]`.
    // `fn good<'de, D: De<'de>>(d: D) -> (&'de S, D::Error)`: `[Param(0), Param(0)]`.
    // `fn bad<'de, 'x, D: De<'de>>(d: D) -> (&'x S, D::Error)`: `'x` `Param(1)`, early (in no
    // input): `[Param(1), Param(0)]`.
    let (p_de, p_x) = (Region::Param(0), Region::Param(1));
    let mut heads = Heads::new(&inline, &[(de, "Error")]);
    heads.method(&mut fx, take, de, 1, &[], &[p0], out, &[p_de, p_de]);
    heads.writes.push(take);
    heads.func(&mut fx, good, Some(de), 1, &[], &[p0], out, &[p_de, p_de]);
    heads.func(&mut fx, bad, Some(de), 2, &[], &[p0], out, &[p_x, p_de]);
    let mut b = Bb::new();
    let pd = b.param("d");
    let ld = b.local(pd);
    let call = b.method(ld, "take", &[]);
    let root = b.block(&[], Some(call));
    let got = findings(&fx, &b, good, root, &heads, &heads.decls, Plant::None);
    std::println!("open_projection good: {got:?}");
    assert_eq!(got, clean(), "the caller's `'de` and its own `D::Error`: clean");
    let got = findings(&fx, &b, bad, root, &heads, &heads.decls, Plant::None);
    std::println!("open_projection bad: {got:?}");
    assert!(any(&got, "borrow UnprovenOutlives"), "`'de: 'x` is not stated: {got:?}");
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
    // `fn next(&mut self) -> Self::Error`: the trait's `'de` `Param(0)`, the elided `&mut
    // self` `Param(1)` (late): `[Param(1)]` then `[Param(0)]`. `fn eat<T>(t: T)`: none.
    // `fn visit<'de, A: Seq<'de>>(a: &mut A) -> A::Error` (and `twice`): `'de` `Param(0)`
    // (early, in the bound), the elided input `Param(1)` (late): the same column as `next`.
    let (p_de, p_self) = (Region::Param(0), Region::Param(1));
    let mut heads = Heads::new(&inline, &[(seq, "Error")]);
    heads.method(&mut fx, next, seq, 2, &[1], &[rm], err, &[p_self, p_de]);
    heads.self_ref.push(next);
    heads.func(&mut fx, eat, None, 0, &[], &[p0], unit, &[]);
    heads.func(&mut fx, visit, Some(seq), 2, &[1], &[rm], err, &[p_self, p_de]);
    heads.func(&mut fx, twice, Some(seq), 2, &[1], &[rm], err, &[p_self, p_de]);
    let mut b = Bb::new();
    let root = next_twice(&mut b, 0);
    let got = findings(&fx, &b, visit, root, &heads, &heads.decls, Plant::None);
    std::println!("next_twice visit: {got:?}");
    assert_eq!(got, clean(), "the first result holds no `&mut` loan: clean");
    let mut b = Bb::new();
    let root = next_twice(&mut b, 2);
    let got = findings(&fx, &b, twice, root, &heads, &heads.decls, Plant::None);
    std::println!("next_twice twice: {got:?}");
    assert!(any(&got, "move"), "E0382: {got:?}");
    assert!(!any(&got, "borrow"), "only the move: {got:?}");
    let got = findings(&fx, &b, twice, root, &heads, &heads.decls, Plant::AllCopy);
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
    let mut heads = Heads::new(&inline, &[]);
    heads.with_field = Some(w);
    // `fn keep(k: K, s: &'static S)` and `fn good(w: W, s: &'static S)`: `['static]`;
    // `fn bad(w: W, s: S)`: none.
    heads.func(&mut fx, keep, None, 0, &[], &[kt, rs], unit, &[Region::Static]);
    heads.writes.push(keep);
    heads.func(&mut fx, good, None, 0, &[], &[wt, rs], unit, &[Region::Static]);
    heads.func(&mut fx, bad, None, 0, &[], &[wt, st], unit, &[]);
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
        let got = findings(&fx, &b, owner, root, &heads, &heads.decls, Plant::None);
        if borrow {
            std::println!("zero_part bad: {got:?}");
            assert!(any(&got, "borrow Invalidated"), "E0597: {got:?}");
        } else {
            std::println!("zero_part good: {got:?}");
            assert_eq!(got, clean(), "a field with no position relates nothing: clean");
        }
    }
}
