//! Piece RB8: a callee's trait lifetimes are the caller's at a call (trait_ties.rs). Each test
//! gives the heads' regions as the head records file them (D148: a fn's lifetime parameters and
//! its signature column, made by hand from the head the test declares), builds each fn's tie
//! rows as blaze-ladder's borrows_ties.rs files them (a trait method's implicit `Self:
//! Trait<'params>` with the trait's lifetimes as the method's own `Param`s, a written bound's
//! lifetime arguments as the fn's), lowers a body checked by the real type checker, and checks
//! it with blaze-flow.
//!
//! Numbering (regions.rs): `Param(i)` is the container's lifetimes, then the item's own in
//! declaration order, then one per elided input. A lifetime named in a bound is early-bound;
//! an elided one late-bound.
//!
//! Sources: the Reference, "Subtyping and Variance" (a trait's parameters are invariant);
//! "Traits" (a trait's items are generic over `Self: Trait<'params>`); "Trait and lifetime
//! bounds" (`for<'a>` quantifies the bound: it holds for every `'a`); "Lifetime elision";
//! RFC 2094 (a loan in a region the body does not choose is live at every point of it, so its
//! place dying is E0597: `borrow Invalidated`; a relation the signature does not state is
//! "lifetime may not live long enough": `borrow UnprovenOutlives`).
//!
//! The region heads are declared with the shared fixture (region_fixture.rs [`Decls`], laid in
//! the checked record's own table: the toys' one ADT, `S`, declares no lifetime, and no column
//! holds an opaque or alias) and the lowering runs through region_fixture.rs `findings`.

use std::vec;

use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ExprId, ItemKey, Mutbl, Name, PNode, PTy};

use crate::region_fixture::{Decls, FnHead, findings};
use crate::{AdtFacts, CallBounds, CalleeFacts, Facts, Form, Plant, Refusal, Region, Tie, TieMiss, Ties};

/// The callee's one parameter, `Self`, has a trait bound and no type-outlives bound.
const TRAIT_BOUNDED: &[u32] = &[0];

/// The heads' region heads ([`Decls`]) and tie rows.
struct Heads {
    decls: Decls,
    ties: vec::Vec<(ItemKey, vec::Vec<Tie>, vec::Vec<Region>)>,
}

impl Heads {
    fn new() -> Heads {
        Heads { decls: Decls::new(), ties: vec::Vec::new() }
    }

    /// `item`'s region head: `count` lifetime parameters, those in `late` late-bound, its
    /// column over `fn(io[..last]) -> io[last]` `slots`.
    fn add(&mut self, _fx: &mut Fx, item: ItemKey, count: u32, late: &[u32], io: &[PTy], slots: &[Region]) {
        let Some((&output, inputs)) = io.split_last() else { panic!("{item:?}: a head has an output") };
        self.decls.func(item, FnHead { lifetimes: count, late, outlives: &[], inputs, output, slots });
    }

    /// A trait method: its head, and its implicit `Self: Trait<'params>` in both of the heads'
    /// forms: the tie row (param 0, the old model) and the clause column 0 (`ExTrait(trait,
    /// [Self])` over `Param(0..trait_lifetimes)`, as blaze-ladder heads_regions.rs
    /// `trait_inherited` lays it: the one model). The trait's lifetimes are the method's own
    /// `Param(0..trait_lifetimes)` (the container's come first in the numbering).
    #[expect(clippy::too_many_arguments, reason = "one head of the fixture, every part named")]
    fn method(&mut self, fx: &mut Fx, item: ItemKey, trait_: ItemKey, trait_lifetimes: u32, count: u32, late: &[u32], io: &[PTy], slots: &[Region]) {
        self.add(fx, item, count, late, io, slots);
        let regions: vec::Vec<Region> = (0..trait_lifetimes).map(Region::Param).collect();
        self.decls.trait_(trait_, trait_lifetimes);
        let clause = bound_ty(fx, 0, trait_);
        self.decls.clause(item, 0, 0, clause, &regions);
        let rows = vec![Tie { param: 0, trait_, lo: 0, hi: trait_lifetimes, read: true }];
        self.ties.push((item, rows, regions));
    }

    /// A fn: its head, and per written trait bound with lifetime arguments `(param, trait,
    /// arguments)` (the arguments in the trait's order as the fn's own regions) its tie row and
    /// its clause column (clause `i`, `ExTrait(trait, [Param(param)])` over the arguments).
    #[expect(clippy::too_many_arguments, reason = "one head of the fixture, every part named")]
    fn func(&mut self, fx: &mut Fx, item: ItemKey, count: u32, late: &[u32], bounds: &[(u32, ItemKey, &[Region])], io: &[PTy], slots: &[Region]) {
        self.add(fx, item, count, late, io, slots);
        let (mut rows, mut regions) = (vec::Vec::new(), vec::Vec::new());
        for (nth, &(param, trait_, args)) in bounds.iter().enumerate() {
            let lo = regions.len() as u32;
            regions.extend(args.iter().copied());
            rows.push(Tie { param, trait_, lo, hi: regions.len() as u32, read: true });
            self.decls.trait_(trait_, args.len() as u32);
            let clause = bound_ty(fx, param, trait_);
            self.decls.clause(item, nth as u32, 0, clause, args);
        }
        self.ties.push((item, rows, regions));
    }
}

/// `Param(param): trait_<..>` as a clause column's type (subject-first, the world's table).
fn bound_ty(fx: &mut Fx, param: u32, trait_: ItemKey) -> PTy {
    let subject = fx.param(param);
    let args = fx.list(&[subject]);
    fx.ty(PNode::ExTrait(trait_, args))
}

impl Facts for Heads {
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
        // As tests.rs's `Plain`: the parameter's drop at the body's end is the access a loan
        // live at every point conflicts with.
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: true, lifetimes: 0, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
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
/// constraint tested: callee `Param(0)` (trait `'a`) equals the caller's placeholder of the
/// bound's first argument.
///
/// Heads: `conv` has the trait's `'a`, `'b` as `Param(0)`, `Param(1)` (early: the container's),
/// column `[Param(0)]` for `x`, `[Param(1)]` for the output. `good` and `swapped` declare `'a`,
/// `'b` (`Param(0)`, `Param(1)`, early: named in the bound), column the same; `good`'s tie
/// `[Param(0), Param(1)]`, `swapped`'s `[Param(1), Param(0)]`.
/// CONTROL (rule 23): `Plant::TiesFresh` (each trait lifetime fresh per call, as before this
/// piece) ACCEPTS `swapped`: the missed error.
/// Prints `a_trait_lifetime_is_the_callers good: Ok([])`, `.. swapped: Ok(["borrow
/// UnprovenOutlives ..])`, `.. swapped, fresh (control): Ok([])`.
#[test]
fn a_trait_lifetime_is_the_callers() {
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
    let (a, b_) = (Region::Param(0), Region::Param(1));
    let mut heads = Heads::new();
    heads.method(&mut fx, conv_fn, conv, 2, 2, &[], &[p0, rs, rs], &[a, b_]);
    heads.func(&mut fx, good, 2, &[], &[(0, conv, &[a, b_])], &[p0, rs, rs], &[a, b_]);
    heads.func(&mut fx, swapped, 2, &[], &[(0, conv, &[b_, a])], &[p0, rs, rs], &[a, b_]);
    let mut b = Bb::new();
    let root = method_body(&mut b, "t", "conv", "x", false, true);
    let got = findings(&fx, &b, good, root, &heads, &heads.decls, Plant::None);
    std::println!("a_trait_lifetime_is_the_callers good: {got:?}");
    assert_eq!(got, Ok(vec::Vec::new()), "the bound's lifetimes in order: clean");
    let got = findings(&fx, &b, swapped, root, &heads, &heads.decls, Plant::None);
    std::println!("a_trait_lifetime_is_the_callers swapped: {got:?}");
    assert!(got.as_ref().is_ok_and(|l| l.iter().any(|l| l.starts_with("borrow UnprovenOutlives"))), "{got:?}");
}

/// MINIATURE (i), serde's shape: `trait Visitor<'de> { fn take(self, s: &'de S); }`. `fn
/// keep<'de, V: Visitor<'de>>(v: V, s: &'de S) { v.take(s); }` is right. `fn local<'de, V:
/// Visitor<'de>>(v: V, s: S) { v.take(&s); }` is wrong (E0597, "`s` does not live long
/// enough"): the trait's `'de` is the caller's, a region the body does not choose, so the loan
/// of `s` is live at every point of the body and `s` dies under it: `Invalidated`. The one
/// constraint tested: callee `Param(0)` (trait `'de`) equals the caller's placeholder of `'de`,
/// a lifetime the caller writes only in its bound.
///
/// Heads: `take` `[Param(0)]` (the trait's `'de`); `keep` `[Param(0)]`, its tie `[Param(0)]`;
/// `local` declares `'de` (early, in the bound), column `[]` (`s: S`), its tie `[Param(0)]`.
/// CONTROL (rule 23): `Plant::TiesFresh` ACCEPTS `local`.
/// Prints `a_visitors_lifetime_is_the_callers keep: Ok([])`, `.. local: Ok(["borrow Invalidated
/// ..])`, `.. local, fresh (control): Ok([])`.
#[test]
fn a_visitors_lifetime_is_the_callers() {
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
    let de = Region::Param(0);
    let mut heads = Heads::new();
    heads.method(&mut fx, take, vis, 1, 1, &[], &[p0, rs, unit], &[de]);
    heads.func(&mut fx, keep, 1, &[], &[(0, vis, &[de])], &[p0, rs, unit], &[de]);
    heads.func(&mut fx, local, 1, &[], &[(0, vis, &[de])], &[p0, st, unit], &[]);
    let mut b = Bb::new();
    let root = method_body(&mut b, "v", "take", "s", false, false);
    let got = findings(&fx, &b, keep, root, &heads, &heads.decls, Plant::None);
    std::println!("a_visitors_lifetime_is_the_callers keep: {got:?}");
    assert_eq!(got, Ok(vec::Vec::new()), "a `&'de S` passed on: clean");
    let mut b = Bb::new();
    let root = method_body(&mut b, "v", "take", "s", true, false);
    let got = findings(&fx, &b, local, root, &heads, &heads.decls, Plant::None);
    std::println!("a_visitors_lifetime_is_the_callers local: {got:?}");
    assert!(got.as_ref().is_ok_and(|l| l.iter().any(|l| l.starts_with("borrow Invalidated"))), "E0597: {got:?}");
}

/// A caller that writes no bound for the parameter the callee's tie names (its bound is its
/// impl's header's, or a supertrait's): the tied region is not known, so the call is REFUSED by
/// name (`CallTieUnplaced { why: NoOwnBound }`), never related with a fresh one. Control: a
/// callee whose tied region its signature never writes relates nothing and is lowered.
///
/// Heads: `trait Visitor<'de> { fn take(self, s: &'de S); fn skip(self, s: &S); }`: `take`
/// `[Param(0)]`; `skip` has the trait's `'de` (`Param(0)`) and its elided input (`Param(1)`,
/// late), column `[Param(1)]`; both tie `[Param(0)]`. `fn owner<V>(v: V, s: &S)`: one elided
/// lifetime (`Param(0)`, late), column `[Param(0)]`, its rows known and none for `V`.
/// Prints `a_tie_with_no_answer_is_refused: Err(Refusal { form: CallTieUnplaced { .., param: 0,
/// why: NoOwnBound }, .. })`, `.. unwritten: Ok([])`.
#[test]
fn a_tie_with_no_answer_is_refused() {
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
    let mut heads = Heads::new();
    heads.method(&mut fx, take, vis, 1, 1, &[], &[p0, rs, unit], &[Region::Param(0)]);
    heads.method(&mut fx, skip, vis, 1, 2, &[1], &[p0, rs, unit], &[Region::Param(1)]);
    // The owner's head writes no `V: Visitor<'de>`: its rows are known and hold none for `V`.
    heads.func(&mut fx, owner, 1, &[0], &[], &[p0, rs, unit], &[Region::Param(0)]);
    let mut b = Bb::new();
    let root = method_body(&mut b, "v", "take", "s", false, false);
    let got = findings(&fx, &b, owner, root, &heads, &heads.decls, Plant::None);
    std::println!("a_tie_with_no_answer_is_refused: {got:?}");
    assert!(matches!(got, Err(Refusal { form: Form::OneRule { why: crate::OneRuleWhy::Unselected { why: crate::Unselected::NoBound, .. }, .. }, .. })), "{got:?}");
    let mut b = Bb::new();
    let root = method_body(&mut b, "v", "skip", "s", false, false);
    let got = findings(&fx, &b, owner, root, &heads, &heads.decls, Plant::None);
    std::println!("a_tie_with_no_answer_is_refused unwritten: {got:?}");
    assert!(matches!(got, Err(Refusal { form: Form::OneRule { why: crate::OneRuleWhy::Unselected { why: crate::Unselected::NoBound, .. }, .. }, .. })), "{got:?}");
}
