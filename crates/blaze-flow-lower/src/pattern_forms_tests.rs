//! Guards that move, or-pattern bindings and slice patterns (pattern_forms.rs; lane B8, NEVER
//! COMPILED when written, 2026-11-18). Each body is a small fn of the fixture world,
//! type-checked by the real checker, lowered and checked by blaze-flow (region_fixture.rs
//! `findings`, the heads laid in the checked record's own table).
//! Or-patterns and slice patterns are built with `Bb::or_pat` and `Bb::slice_pat` (blaze-typeck
//! fixture/pats.rs, lines the lane's report asks for).
//!
//! Every fn's head is the region record the head lowering files for it (D148, made by hand
//! here from the head written in [`World`]'s doc): each has at most one lifetime, elided, so
//! `Param(0)`, late-bound.

use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ExprId, ItemKey, LangItem, Mutbl, Name, PNode, PTy, PatId};

use crate::callee_value_tests::{call1, call_ref, no_clauses};
use crate::region_fixture::{Decls, FnHead, findings};
use crate::{AdtFacts, CalleeFacts, Facts, Plant, Region};

/// The fixture's facts: plain ADTs (tests.rs `Plain`: not `Copy`, no lifetime, needing drop),
/// two enums of two variants each (`Option<T> { None, Some(T) }` and `E { A(S), B(S) }`) with
/// their variants' field counts, and every fn's region head ([`Decls`]: no ADT of the fixture
/// declares a lifetime).
struct Pats {
    enums: [(ItemKey, [u32; 2]); 2],
    decls: Decls,
}

impl Pats {
    /// `item`'s region head over `fn(io[..last]) -> io[last]`: one elided, late-bound lifetime
    /// when `slots` names it, none otherwise.
    fn add(&mut self, _fx: &mut Fx, item: ItemKey, io: &[PTy], slots: &[Region]) {
        let Some((&output, inputs)) = io.split_last() else { panic!("{item:?}: a head has an output") };
        let count = u32::from(!slots.is_empty());
        let late: &[u32] = if count == 1 { &[0] } else { &[] };
        self.decls.func(item, FnHead { lifetimes: count, late, outlives: &[], inputs, output, slots });
    }
}

impl Facts for Pats {
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
        let is_enum = self.enums.iter().any(|(e, _)| *e == k);
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: true, lifetimes: 0, union: false, is_enum, is_box: false })
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
    }
    fn closure_bodies_checked(&self) -> bool {
        true
    }
    fn field(&self, _: ItemKey, _: u32, _: Name) -> Option<u32> {
        None
    }
    fn field_count(&self, adt: ItemKey, variant: u32) -> Option<u32> {
        let (_, counts) = self.enums.iter().find(|(e, _)| *e == adt)?;
        counts.get(variant as usize).copied()
    }
    fn param_copy(&self, _: u32) -> bool {
        false
    }
}

/// `struct S;` (not `Copy`), `Option<T>`, `E { A(S), B(S) }`, `fn g(_: &S)`, `fn m(_: &mut S)`,
/// `fn h(_: S)`, `fn n(_: &mut [S])`, `fn p(_: &S) -> bool`, `fn take(_: S) -> bool`, and the
/// owners: `opt(o: Option<S>)`, `either(e: E)`, `pair(a: &mut [S; 2])`, `slice(v: &mut [S])`.
struct World {
    fx: Fx,
    facts: Pats,
    opt: ItemKey,
    either: ItemKey,
    pair: ItemKey,
    slice: ItemKey,
}

fn world() -> World {
    let mut fx = Fx::new();
    let copy = fx.tr("Copy", 1, no_clauses(), &[]);
    fx.lang(LangItem::Copy, copy);
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let rms = fx.r(Mutbl::Mut, st);
    let unit = fx.unit();
    let boolean = fx.ty(PNode::Bool);
    let p0 = fx.param(0);
    let (option, _) = fx.enum_("Option", 1, &[("None", &[]), ("Some", &[p0])]);
    let (e, _) = fx.enum_("E", 0, &[("A", &[st]), ("B", &[st])]);
    let opt_s = fx.adt(option, &[st]);
    let et = fx.adt(e, &[]);
    let slice_s = fx.ty(PNode::Slice(st));
    let r_slice = fx.r(Mutbl::Mut, slice_s);
    let arr = fx.array(st, 2);
    let r_arr = fx.r(Mutbl::Mut, arr);
    let g = fx.func("g", 0, &[rs], unit, no_clauses());
    let m = fx.func("m", 0, &[rms], unit, no_clauses());
    let h = fx.func("h", 0, &[st], unit, no_clauses());
    let n = fx.func("n", 0, &[r_slice], unit, no_clauses());
    let p = fx.func("p", 0, &[rs], boolean, no_clauses());
    let take = fx.func("take", 0, &[st], boolean, no_clauses());
    let opt = fx.func("opt", 0, &[opt_s], unit, no_clauses());
    let either = fx.func("either", 0, &[et], unit, no_clauses());
    let pair = fx.func("pair", 0, &[r_arr], unit, no_clauses());
    let slice = fx.func("slice", 0, &[r_slice], unit, no_clauses());
    fx.finish();
    // Each reference input its one elided lifetime, `Param(0)`; no output borrows.
    let p0 = Region::Param(0);
    let mut facts = Pats { enums: [(option, [0, 1]), (e, [1, 1])], decls: Decls::new() };
    facts.add(&mut fx, g, &[rs, unit], &[p0]);
    facts.add(&mut fx, m, &[rms, unit], &[p0]);
    facts.add(&mut fx, h, &[st, unit], &[]);
    facts.add(&mut fx, n, &[r_slice, unit], &[p0]);
    facts.add(&mut fx, p, &[rs, boolean], &[p0]);
    facts.add(&mut fx, take, &[st, boolean], &[]);
    facts.add(&mut fx, opt, &[opt_s, unit], &[]);
    facts.add(&mut fx, either, &[et, unit], &[]);
    facts.add(&mut fx, pair, &[r_arr, unit], &[p0]);
    facts.add(&mut fx, slice, &[r_slice, unit], &[p0]);
    World { fx, facts, opt, either, pair, slice }
}

/// `{ match <scrutinee> { arms } }` as a block's tail.
fn match_block(b: &mut Bb, scrutinee: PatId, arms: &[(PatId, Option<ExprId>, ExprId)]) -> ExprId {
    let s = b.local(scrutinee);
    let m = b.match_guarded(s, arms);
    b.block(&[], Some(m))
}

/// A block `{ a; b; }`.
fn two(b: &mut Bb, x: ExprId, y: ExprId) -> ExprId {
    let (s1, s2) = (b.semi(x), b.semi(y));
    b.block(&[s1, s2], None)
}

/// The Reference, "Match guards": a by-value binding is borrowed shared for the guard and moved
/// only once the guard holds, so a guard that moves its binding is an error (rustc E0507:
/// cannot move out of `x` in a pattern guard, behind a shared reference). `fn opt(o:
/// Option<S>) { match o { Some(x) if take(x) => {} _ => {} } }`: a `move` finding (out of the
/// guard's reference: `move MoveOut(Deref)`). CONTROL (rule 23): `Plant::GuardBindsByValue`
/// binds `x` by value before the guard (as an arm with no guard is): the guard's move is then
/// out of an owned binding, so the `MoveOut(Deref)` finding is GONE. The plant keeps the arm's
/// own by-value bind after the guard, which finds the value the guard moved (`move UseOfMoved`
/// at the pattern `x`): an artifact of the plant binding twice, not of the real lowering, which
/// binds once (Plant::None finds only the E0507). Before this piece the arm was refused
/// (`GuardMovesBinding`).
/// Prints: `guard moves: ["move MoveOut(Deref) ..` and `guard moves, planted
/// GuardBindsByValue: ["move UseOfMoved ..`.
#[test]
fn a_guard_that_moves_its_binding_is_found() {
    let w = world();
    let mut b = Bb::new();
    let o = b.param("o");
    let x = b.bind("x");
    let some = b.variant_pat(&["Option", "Some"], &[x]);
    let guard = call1(&mut b, "take", x);
    let empty = b.block(&[], None);
    let wild = b.wild();
    let other = b.block(&[], None);
    let root = match_block(&mut b, o, &[(some, Some(guard), empty), (wild, None, other)]);
    let got = findings(&w.fx, &b, w.opt, root, &w.facts, &w.facts.decls, Plant::None);
    std::println!("guard moves: {got:?}");
    assert!(got.as_ref().is_ok_and(|g| g.iter().any(|l| l.starts_with("move MoveOut(Deref)"))), "E0507: {got:?}");
    let planted = findings(&w.fx, &b, w.opt, root, &w.facts, &w.facts.decls, Plant::GuardBindsByValue);
    std::println!("guard moves, planted GuardBindsByValue: {planted:?}");
    assert!(planted.as_ref().is_ok_and(|p| !p.iter().any(|l| l.starts_with("move MoveOut(Deref)"))), "the control: the E0507 is gone: {planted:?}");
}

/// A guard that only reads its binding, and a body that moves it after: `fn opt(o: Option<S>)
/// { match o { Some(x) if p(&x) => h(x), _ => {} } }` is right (rustc accepts it): no finding.
/// Its negative, the same arm with `h(x); g(&x)` as the body, uses `x` after the body moved it:
/// rustc E0382, here a `move` finding.
/// Prints: `guard reads: []` and `guard reads, body uses after move: ["move ..`.
#[test]
fn a_guard_that_reads_its_binding_then_the_body_moves_it() {
    let w = world();
    for after in [false, true] {
        let mut b = Bb::new();
        let o = b.param("o");
        let x = b.bind("x");
        let some = b.variant_pat(&["Option", "Some"], &[x]);
        let guard = call_ref(&mut b, "p", Mutbl::Not, x);
        let moved = call1(&mut b, "h", x);
        let body = if after {
            let read = call_ref(&mut b, "g", Mutbl::Not, x);
            two(&mut b, moved, read)
        } else {
            moved
        };
        let wild = b.wild();
        let other = b.block(&[], None);
        let root = match_block(&mut b, o, &[(some, Some(guard), body), (wild, None, other)]);
        let got = findings(&w.fx, &b, w.opt, root, &w.facts, &w.facts.decls, Plant::None);
        std::println!("guard reads{}: {got:?}", if after { ", body uses after move" } else { "" });
        let got = got.expect("lowered");
        assert_eq!(got.iter().any(|l| l.starts_with("move ")), after, "{got:?}");
        if !after {
            assert!(got.is_empty(), "{got:?}");
        }
    }
}

/// An or-pattern's name is ONE variable whichever alternative bound it (the Reference,
/// "Or-patterns"): `fn either(e: E) { match e { E::A(x) | E::B(x) => h(x) } }` is right; its
/// negative `.. => { h(x); g(&x); } }` uses `x` after moving it: rustc E0382, here a `move`
/// finding. Before this piece both were refused (`OrPatternBinding`).
/// Prints: `or binding moved once: []` and `or binding used after move: ["move ..`.
#[test]
fn an_or_pattern_binding_moved_then_used() {
    let w = world();
    for after in [false, true] {
        let mut b = Bb::new();
        let e = b.param("e");
        let (xa, xb) = (b.bind("x"), b.bind("x"));
        let a = b.variant_pat(&["E", "A"], &[xa]);
        let bb = b.variant_pat(&["E", "B"], &[xb]);
        let or = b.or_pat(&[a, bb]);
        let moved = call1(&mut b, "h", xa);
        let body = if after {
            let read = call_ref(&mut b, "g", Mutbl::Not, xa);
            two(&mut b, moved, read)
        } else {
            moved
        };
        let root = match_block(&mut b, e, &[(or, None, body)]);
        let got = findings(&w.fx, &b, w.either, root, &w.facts, &w.facts.decls, Plant::None);
        std::println!("or binding {}: {got:?}", if after { "used after move" } else { "moved once" });
        let got = got.expect("lowered");
        assert_eq!(got.iter().any(|l| l.starts_with("move ")), after, "{got:?}");
        if !after {
            assert!(got.is_empty(), "{got:?}");
        }
    }
}

/// Distinct elements of a slice pattern are distinct places (the Reference, "Slice patterns";
/// rustc's `ConstantIndex` projections, disjoint by offset): `fn pair(a: &mut [S; 2]) { let
/// [x, y] = a; m(x); m(y); }` binds `x: &mut S`, `y: &mut S` (default binding mode `ref mut`),
/// two mutable loans that do not overlap: rustc accepts it, no finding. CONTROL (rule 23):
/// `Plant::SliceElementsOverlap` projects each element as any index, and the two loans
/// conflict (`Invalidated`, the false E0499 the brief names).
/// Prints: `slice pair: []` and `slice pair, planted SliceElementsOverlap: ["borrow Invalidated ..`.
#[test]
fn two_elements_of_a_mutable_array_are_disjoint() {
    let w = world();
    let mut b = Bb::new();
    let a = b.param("a");
    let (x, y) = (b.bind("x"), b.bind("y"));
    let pat = b.slice_pat(&[x, y], None);
    let la = b.local(a);
    let st1 = b.let_(pat, None, la);
    let (mx, my) = (call1(&mut b, "m", x), call1(&mut b, "m", y));
    let (st2, st3) = (b.semi(mx), b.semi(my));
    let root = b.block(&[st1, st2, st3], None);
    let got = findings(&w.fx, &b, w.pair, root, &w.facts, &w.facts.decls, Plant::None);
    std::println!("slice pair: {got:?}");
    assert!(got.as_ref().is_ok_and(|g| g.is_empty()), "{got:?}");
    let planted = findings(&w.fx, &b, w.pair, root, &w.facts, &w.facts.decls, Plant::SliceElementsOverlap);
    std::println!("slice pair, planted SliceElementsOverlap: {planted:?}");
    assert!(planted.as_ref().is_ok_and(|p| p.iter().any(|l| l.starts_with("borrow Invalidated"))), "{planted:?}");
}

/// An element and the whole slice overlap (the slice is a prefix of the element's place):
/// `fn slice(v: &mut [S]) { match v { [x, ..] => { n(v); m(x); } _ => {} } }` reborrows the
/// whole of `*v` while `x: &mut S` (its first element) is live: rustc E0499, here
/// `Invalidated`. Its positive, `.. => { m(x); n(v); } ..`, ends `x` first: no finding. And
/// `rest @ ..` beside a first element: `[x, rest @ ..] => { n(rest); m(x); }`, disjoint
/// (`Subslice { from: 1, .. }` against `ConstantIndex { offset: 0, .. }`): no finding.
/// Prints: `slice and element, whole first: ["borrow Invalidated ..`, `slice and element,
/// element first: []` and `slice first and rest: []`.
#[test]
fn an_element_and_the_whole_slice_overlap() {
    let w = world();
    for whole_first in [false, true] {
        let mut b = Bb::new();
        let v = b.param("v");
        let x = b.bind("x");
        let dots = b.wild();
        let pat = b.slice_pat(&[x, dots], Some(1));
        let whole = call1(&mut b, "n", v);
        let elem = call1(&mut b, "m", x);
        let body = if whole_first { two(&mut b, whole, elem) } else { two(&mut b, elem, whole) };
        let wild = b.wild();
        let other = b.block(&[], None);
        let root = match_block(&mut b, v, &[(pat, None, body), (wild, None, other)]);
        let got = findings(&w.fx, &b, w.slice, root, &w.facts, &w.facts.decls, Plant::None);
        std::println!("slice and element, {}: {got:?}", if whole_first { "whole first" } else { "element first" });
        let got = got.expect("lowered");
        assert_eq!(got.iter().any(|l| l.starts_with("borrow Invalidated")), whole_first, "{got:?}");
        if !whole_first {
            assert!(got.is_empty(), "{got:?}");
        }
    }
    let mut b = Bb::new();
    let v = b.param("v");
    let x = b.bind("x");
    let dots = b.wild();
    let rest = b.bind_sub("rest", dots);
    let pat = b.slice_pat(&[x, rest], Some(1));
    // `rest` used while `x` is live: the two loans are held at once.
    let tail = call1(&mut b, "n", rest);
    let elem = call1(&mut b, "m", x);
    let body = two(&mut b, tail, elem);
    let wild = b.wild();
    let other = b.block(&[], None);
    let root = match_block(&mut b, v, &[(pat, None, body), (wild, None, other)]);
    let got = findings(&w.fx, &b, w.slice, root, &w.facts, &w.facts.decls, Plant::None);
    std::println!("slice first and rest: {got:?}");
    assert!(got.as_ref().is_ok_and(|g| g.is_empty()), "{got:?}");
}
