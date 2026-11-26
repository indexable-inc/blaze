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
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false, c_variadic: false })
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

/// Guarded or-pattern with by-value non-`Copy` bindings (`sim.rs::complete` shape:
/// `Effect::Clone { from, .. } | Effect::Link { from, .. } if !self.files.contains_key(&from)`):
/// - Positive: `match e { E::A(x) | E::B(x) if p(&x) => h(x), _ => {} }` lowers cleanly with zero
///   findings (previously refused with `Form::OrPatternBinding`).
/// - Negative 1 (body use-after-move): `.. => { h(x); g(&x); }` catches `move UseOfMoved` (E0382).
/// - Negative 2 (guard moves binding): `E::A(x) | E::B(x) if take(x) => {}` catches
///   `move MoveOut(Deref)` (E0507).
#[test]
fn guarded_or_pattern_by_value_binding_lowers_and_checks_moves() {
    let w = world();
    // Positive + body use-after-move negative.
    for after in [false, true] {
        let mut b = Bb::new();
        let e = b.param("e");
        let (xa, xb) = (b.bind("x"), b.bind("x"));
        let a = b.variant_pat(&["E", "A"], &[xa]);
        let bb = b.variant_pat(&["E", "B"], &[xb]);
        let or = b.or_pat(&[a, bb]);
        let guard = call_ref(&mut b, "p", Mutbl::Not, xa);
        let moved = call1(&mut b, "h", xa);
        let body = if after {
            let read = call_ref(&mut b, "g", Mutbl::Not, xa);
            two(&mut b, moved, read)
        } else {
            moved
        };
        let wild = b.wild();
        let other = b.block(&[], None);
        let root = match_block(&mut b, e, &[(or, Some(guard), body), (wild, None, other)]);
        let got = findings(&w.fx, &b, w.either, root, &w.facts, &w.facts.decls, Plant::None);
        std::println!("guarded or binding {}: {got:?}", if after { "used after move" } else { "moved once" });
        let got = got.expect("lowered");
        assert_eq!(got.iter().any(|l| l.starts_with("move ")), after, "{got:?}");
        if !after {
            assert!(got.is_empty(), "{got:?}");
        }
    }
    // Negative 2: guard tries to move its by-value binding (must be E0507 MoveOut(Deref)).
    let mut b = Bb::new();
    let e = b.param("e");
    let (xa, xb) = (b.bind("x"), b.bind("x"));
    let a = b.variant_pat(&["E", "A"], &[xa]);
    let bb = b.variant_pat(&["E", "B"], &[xb]);
    let or = b.or_pat(&[a, bb]);
    let guard = call1(&mut b, "take", xa);
    let empty = b.block(&[], None);
    let wild = b.wild();
    let other = b.block(&[], None);
    let root = match_block(&mut b, e, &[(or, Some(guard), empty), (wild, None, other)]);
    let got = findings(&w.fx, &b, w.either, root, &w.facts, &w.facts.decls, Plant::None);
    std::println!("guarded or binding moved in guard: {got:?}");
    assert!(got.as_ref().is_ok_and(|g| g.iter().any(|l| l.starts_with("move MoveOut(Deref)"))), "E0507: {got:?}");
}

/// Guarded or-pattern fallthrough & guard-mutation soundness:
/// - Positive: if the guard fails, the scrutinee was only borrowed (not moved), so a following
///   arm `E::A(y) | E::B(y) => h(y)` can still move out of it cleanly.
/// - Negative 1: using `y` after move in the following arm still catches `move UseOfMoved`.
/// - Negative 2: mutably borrowing the scrutinee `e` inside the guard while `x` and `FakeBorrow`
///   are active catches `borrow Invalidated`.
#[test]
fn guarded_or_pattern_failed_guard_preserves_scrutinee_and_blocks_mutation() {
    let w = world();
    for after_in_second in [false, true] {
        let mut b = Bb::new();
        let e = b.param("e");
        let (xa, xb) = (b.bind("x"), b.bind("x"));
        let a1 = b.variant_pat(&["E", "A"], &[xa]);
        let b1 = b.variant_pat(&["E", "B"], &[xb]);
        let or1 = b.or_pat(&[a1, b1]);
        let guard = call_ref(&mut b, "p", Mutbl::Not, xa);
        let body1 = call1(&mut b, "h", xa);

        let (ya, yb) = (b.bind("y"), b.bind("y"));
        let a2 = b.variant_pat(&["E", "A"], &[ya]);
        let b2 = b.variant_pat(&["E", "B"], &[yb]);
        let or2 = b.or_pat(&[a2, b2]);
        let moved2 = call1(&mut b, "h", ya);
        let body2 = if after_in_second {
            let read2 = call_ref(&mut b, "g", Mutbl::Not, ya);
            two(&mut b, moved2, read2)
        } else {
            moved2
        };

        let root = match_block(&mut b, e, &[(or1, Some(guard), body1), (or2, None, body2)]);
        let got = findings(&w.fx, &b, w.either, root, &w.facts, &w.facts.decls, Plant::None);
        std::println!("guarded or fallthrough {}: {got:?}", if after_in_second { "use after move" } else { "clean" });
        let got = got.expect("lowered");
        assert_eq!(got.iter().any(|l| l.starts_with("move ")), after_in_second, "{got:?}");
        if !after_in_second {
            assert!(got.is_empty(), "{got:?}");
        }
    }
    // Negative 2: guard takes `&mut e` while `x` is borrowed for the guard (conflicting borrow).
    let mut b = Bb::new();
    let e = b.param("e");
    let (xa, xb) = (b.bind("x"), b.bind("x"));
    let a = b.variant_pat(&["E", "A"], &[xa]);
    let bb = b.variant_pat(&["E", "B"], &[xb]);
    let or = b.or_pat(&[a, bb]);
    let le = b.local(e);
    let mut_e = b.borrow(Mutbl::Mut, le);
    let pr = b.bind("_r");
    let st_mut = b.let_(pr, None, mut_e);
    let check = call_ref(&mut b, "p", Mutbl::Not, xa);
    let guard = b.block(&[st_mut], Some(check));
    let body = call1(&mut b, "h", xa);
    let wild = b.wild();
    let other = b.block(&[], None);
    let root = match_block(&mut b, e, &[(or, Some(guard), body), (wild, None, other)]);
    let got = findings(&w.fx, &b, w.either, root, &w.facts, &w.facts.decls, Plant::None);
    std::println!("guarded or mutates scrutinee in guard: {got:?}");
    assert!(got.as_ref().is_ok_and(|g| g.iter().any(|l| l.starts_with("borrow Invalidated"))), "guard mutates scrutinee: {got:?}");
}

/// Diverging call (`-> !`, `PNode::Never`) terminates its path (`StmtKind::Unreachable`):
/// moving a local before a diverging call in an `if` branch does not leave it moved after the
/// `if` (world/threads.rs `resume_unwind` / `self.run.refuse`), while a non-diverging call still
/// rejects the post-`if` use and a borrow passed to the diverging call still rejects a prior move.
#[test]
fn diverging_call_terminates_path_and_keeps_soundness() {
    use blaze_typeck::Expr;
    let mut fx = Fx::new();
    let copy = fx.tr("Copy", 1, no_clauses(), &[]);
    fx.lang(LangItem::Copy, copy);
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let unit = fx.unit();
    let never = fx.never();
    let boolean = fx.ty(PNode::Bool);
    let consume = fx.func("consume", 0, &[st], unit, no_clauses());
    let diverge = fx.func("diverge", 0, &[], never, no_clauses());
    let diverge_ref = fx.func("diverge_ref", 0, &[rs], never, no_clauses());
    let unit_fn = fx.func("unit_fn", 0, &[], unit, no_clauses());
    let ok_fn = fx.func("ok_fn", 0, &[boolean, st], unit, no_clauses());
    let bad_after_unit = fx.func("bad_after_unit", 0, &[boolean, st], unit, no_clauses());
    let bad_borrow_before_diverge = fx.func("bad_borrow_before_diverge", 0, &[st], unit, no_clauses());
    fx.finish();

    let mut facts = Pats { enums: [(s, [0, 0]), (s, [0, 0])], decls: Decls::new() };
    facts.add(&mut fx, consume, &[st, unit], &[]);
    facts.add(&mut fx, diverge, &[never], &[]);
    facts.add(&mut fx, diverge_ref, &[rs, never], &[Region::Param(0)]);
    facts.add(&mut fx, unit_fn, &[unit], &[]);
    facts.add(&mut fx, ok_fn, &[boolean, st, unit], &[]);
    facts.add(&mut fx, bad_after_unit, &[boolean, st, unit], &[]);
    facts.add(&mut fx, bad_borrow_before_diverge, &[st, unit], &[]);

    let build_if = |call_name: &str| {
        let mut b = Bb::new();
        let pc = b.param("c");
        let px = b.param("x");
        let lc = b.local(pc);
        let mv_e = call1(&mut b, "consume", px);
        let mv = b.semi(mv_e);
        let p_fn = b.path(&[call_name]);
        let c_e = b.call(p_fn, &[]);
        let st_call = b.semi(c_e);
        let then = b.block(&[mv, st_call], None);
        let if_e = b.bool();
        b.exprs[if_e.ix()] = Expr::If { cond: lc, then, else_: None };
        let s1 = b.semi(if_e);
        let c2_e = call1(&mut b, "consume", px);
        let s2 = b.semi(c2_e);
        let root = b.block(&[s1, s2], None);
        (b, root)
    };

    // Positive: `if c { consume(x); diverge(); } consume(x);` is sound.
    let (b_ok, root_ok) = build_if("diverge");
    let got_ok = findings(&fx, &b_ok, ok_fn, root_ok, &facts, &facts.decls, Plant::None);
    assert!(got_ok.as_ref().is_ok_and(|g| g.is_empty()), "diverging call must terminate branch: {got_ok:?}");

    // Negative control 1: non-diverging `unit_fn()` falls through -> `UseOfMoved`.
    let (b_bad, root_bad) = build_if("unit_fn");
    let got_bad = findings(&fx, &b_bad, bad_after_unit, root_bad, &facts, &facts.decls, Plant::None);
    assert!(
        got_bad.as_ref().is_ok_and(|g| g.iter().any(|l| l.starts_with("move UseOfMoved"))),
        "CONTROL: non-diverging call must still report UseOfMoved: {got_bad:?}",
    );

    // Negative control 2: `let r = &x; consume(x); diverge_ref(r);` still catches `MoveWhileBorrowed`.
    let mut b_mwb = Bb::new();
    let px = b_mwb.param("x");
    let lx = b_mwb.local(px);
    let bor = b_mwb.borrow(Mutbl::Not, lx);
    let pr = b_mwb.bind("r");
    let s1 = b_mwb.let_(pr, None, bor);
    let mv_e = call1(&mut b_mwb, "consume", px);
    let s2 = b_mwb.semi(mv_e);
    let lr = b_mwb.local(pr);
    let p_dr = b_mwb.path(&["diverge_ref"]);
    let dr_e = b_mwb.call(p_dr, &[lr]);
    let s3 = b_mwb.semi(dr_e);
    let root_mwb = b_mwb.block(&[s1, s2, s3], None);
    let got_mwb = findings(&fx, &b_mwb, bad_borrow_before_diverge, root_mwb, &facts, &facts.decls, Plant::None);
    assert!(
        got_mwb.as_ref().is_ok_and(|g| g.iter().any(|l| l.starts_with("borrow Invalidated"))),
        "CONTROL: diverging call reading borrow must still reject prior move: {got_mwb:?}",
    );
}

/// Borrowing a field of an immutable `static` item (`&PROFILE.rings`) lives for `'static` without
/// materializing a stack temporary, whereas borrowing a field of a non-promoted `const` item with
/// drop glue still materializes a temporary and is rejected when returned as `'static`.
#[test]
fn static_field_borrow_is_static_while_const_temp_is_rejected() {
    use blaze_typeck::{Def, Expr};
    let mut fx = Fx::new();
    let copy = fx.tr("Copy", 1, no_clauses(), &[]);
    fx.lang(LangItem::Copy, copy);
    let inner = fx.strukt("Inner", 0, &[]);
    let inner_t = fx.adt(inner, &[]);
    let holder = fx.strukt("Holder", 0, &[("inner", inner_t)]);
    let holder_t = fx.adt(holder, &[]);
    let r_inner = fx.r(Mutbl::Not, inner_t);
    let stat = fx.konst("_PROFILE_KEY", holder_t);
    fx.use_path("PROFILE", Def::Static(stat, Mutbl::Not));
    let _konst = fx.konst("CONST_H", holder_t);
    let good = fx.func("good", 0, &[], r_inner, no_clauses());
    let bad = fx.func("bad", 0, &[], r_inner, no_clauses());
    fx.finish();

    struct HolderFacts {
        decls: Decls,
        holder: ItemKey,
    }
    impl Facts for HolderFacts {
        fn regions(&self) -> &dyn crate::RegionFacts { &crate::NoRegions }
        fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> { None }
        fn adt_variances(&self, _: ItemKey) -> Option<&[crate::Variance]> { None }
        fn implied_bounds(&self) -> Option<(&[(u32, Region)], &[(Region, Region)])> { None }
        fn constant_initializer(&self) -> bool { false }
        fn fn_binders(&self) -> bool { true }
        fn closure_requirements(&self, _: ExprId) -> &[crate::Requirement] { &[] }
        fn async_bodies_checked(&self) -> bool { false }
        fn is_future(&self, _: PTy) -> Option<bool> { None }
        fn shape(&self, _: ItemKey) -> Option<crate::Shape<'_>> { None }
        fn object_trait_lifetimes(&self, _: ItemKey) -> Option<bool> { None }
        fn call_bounds(&self, _: ItemKey) -> Option<crate::CallBounds<'_>> { None }
        fn trait_lifetimes(&self, _: ItemKey) -> Option<u32> { None }
        fn alias_lifetimes(&self, _: ItemKey, _: Name) -> Option<u32> { None }
        fn ties(&self, _: ItemKey) -> Option<crate::Ties<'_>> { None }
        fn field_param(&self, _: ItemKey, _: u32, _: u32) -> Option<u32> { None }
        fn field_lead(&self, _: ItemKey, _: u32, _: u32) -> Option<crate::FieldLead> { None }
        fn field_regions(&self, _: ItemKey, _: u32, _: u32) -> Option<&[crate::FieldRegion]> { None }
        fn copy_args(&self, _: ItemKey) -> Option<&[bool]> { None }
        fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
            Some(AdtFacts { copy: false, drop_impl: false, needs_drop: true, lifetimes: 0, union: false, is_enum: false, is_box: false })
        }
        fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
            Some(CalleeFacts { writes_lifetimes: false, self_ref: false, c_variadic: false })
        }
        fn closure_bodies_checked(&self) -> bool { true }
        fn field(&self, adt: ItemKey, variant: u32, name: Name) -> Option<u32> {
            (adt == self.holder && variant == 0 && name == Name::of("inner")).then_some(0)
        }
        fn field_count(&self, adt: ItemKey, variant: u32) -> Option<u32> {
            (adt == self.holder && variant == 0).then_some(1)
        }
        fn param_copy(&self, _: u32) -> bool { false }
    }

    let mut decls = Decls::new();
    decls.func(good, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[], output: r_inner, slots: &[Region::Static] });
    decls.func(bad, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[], output: r_inner, slots: &[Region::Static] });
    let facts = HolderFacts { decls, holder };

    let build = |name: &str| {
        let mut b = Bb::new();
        let base = b.path(&[name]);
        let fld = b.bool();
        b.exprs[fld.ix()] = Expr::Field { base, name: Name::of("inner") };
        let r = b.borrow(Mutbl::Not, fld);
        let root = b.block(&[], Some(r));
        (b, root)
    };

    // Positive: `&PROFILE.inner` is a `'static` borrow of the immutable `static`.
    let (bg, root_g) = build("PROFILE");
    let got_g = findings(&fx, &bg, good, root_g, &facts, &facts.decls, Plant::None);
    assert!(got_g.as_ref().is_ok_and(|g| g.is_empty()), "&PROFILE.inner must be 'static: {got_g:?}");

    // Negative control: `&CONST_H.inner` (a non-`Copy` `const` item) borrows a local temporary.
    let (bb, root_b) = build("CONST_H");
    let got_b = findings(&fx, &bb, bad, root_b, &facts, &facts.decls, Plant::None);
    assert!(
        got_b.as_ref().is_ok_and(|g| !g.is_empty()),
        "CONTROL: &CONST_H.inner on a dropping const must NOT return 'static: {got_b:?}",
    );
}

/// By-value `@` binding (`whole @ (copy_field, _)`) lowers its subpattern before moving `whole`:
/// copying a `Copy` field before moving `whole` is accepted, while moving a non-`Copy` field inside
/// the subpattern still rejects moving `whole` as `UseMoved`.
#[test]
fn at_pattern_reads_copy_subpattern_before_moving_whole_and_rejects_non_copy() {
    let mut fx = Fx::new();
    let copy = fx.tr("Copy", 1, no_clauses(), &[]);
    fx.lang(LangItem::Copy, copy);
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let boolean = fx.ty(PNode::Bool);
    let unit = fx.unit();
    let pair_copy = fx.tuple(&[boolean, st]);
    let pair_move = fx.tuple(&[st, st]);
    let use_bool = fx.func("use_bool", 0, &[boolean], unit, no_clauses());
    let consume_s = fx.func("consume_s", 0, &[st], unit, no_clauses());
    let consume_pc = fx.func("consume_pc", 0, &[pair_copy], unit, no_clauses());
    let consume_pm = fx.func("consume_pm", 0, &[pair_move], unit, no_clauses());
    let ok_fn = fx.func("ok_fn", 0, &[pair_copy], unit, no_clauses());
    let bad_fn = fx.func("bad_fn", 0, &[pair_move], unit, no_clauses());
    fx.finish();

    let mut facts = Pats { enums: [(s, [0, 0]), (s, [0, 0])], decls: Decls::new() };
    facts.add(&mut fx, use_bool, &[boolean, unit], &[]);
    facts.add(&mut fx, consume_s, &[st, unit], &[]);
    facts.add(&mut fx, consume_pc, &[pair_copy, unit], &[]);
    facts.add(&mut fx, consume_pm, &[pair_move, unit], &[]);
    facts.add(&mut fx, ok_fn, &[pair_copy, unit], &[]);
    facts.add(&mut fx, bad_fn, &[pair_move, unit], &[]);

    // Positive: `let whole @ (flag, _) = p; use_bool(flag); consume_pc(whole);`
    let mut b_ok = Bb::new();
    let pp = b_ok.param("p");
    let flag = b_ok.bind("flag");
    let wild = b_ok.wild();
    let sub = b_ok.tuple_pat(&[flag, wild]);
    let whole = b_ok.bind_sub("whole", sub);
    let lp = b_ok.local(pp);
    let s1 = b_ok.let_(whole, None, lp);
    let u_e = call1(&mut b_ok, "use_bool", flag);
    let s2 = b_ok.semi(u_e);
    let c_e = call1(&mut b_ok, "consume_pc", whole);
    let s3 = b_ok.semi(c_e);
    let root_ok = b_ok.block(&[s1, s2, s3], None);
    let got_ok = findings(&fx, &b_ok, ok_fn, root_ok, &facts, &facts.decls, Plant::None);
    assert!(got_ok.as_ref().is_ok_and(|g| g.is_empty()), "@ with Copy field must succeed: {got_ok:?}");

    // Negative control: `let whole @ (first, _) = p; consume_s(first); consume_pm(whole);`
    let mut b_bad = Bb::new();
    let pp = b_bad.param("p");
    let first = b_bad.bind("first");
    let wild = b_bad.wild();
    let sub = b_bad.tuple_pat(&[first, wild]);
    let whole = b_bad.bind_sub("whole", sub);
    let lp = b_bad.local(pp);
    let s1 = b_bad.let_(whole, None, lp);
    let c1_e = call1(&mut b_bad, "consume_s", first);
    let s2 = b_bad.semi(c1_e);
    let c2_e = call1(&mut b_bad, "consume_pm", whole);
    let s3 = b_bad.semi(c2_e);
    let root_bad = b_bad.block(&[s1, s2, s3], None);
    let got_bad = findings(&fx, &b_bad, bad_fn, root_bad, &facts, &facts.decls, Plant::None);
    assert!(
        got_bad.as_ref().is_ok_and(|g| g.iter().any(|l| l.starts_with("move UseOfMoved"))),
        "CONTROL: @ with non-Copy subpattern must reject moving whole: {got_bad:?}",
    );
}

/// Synthetic bindings with identical names outside `Pat::Or` (as desugared destructuring
/// assignments `(a, b) = expr` emit) are distinct locals, never aliased as or-pattern bindings.
#[test]
fn duplicate_binding_names_outside_or_pattern_are_distinct_locals() {
    let mut fx = Fx::new();
    let copy = fx.tr("Copy", 1, no_clauses(), &[]);
    fx.lang(LangItem::Copy, copy);
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let boolean = fx.ty(PNode::Bool);
    let unit = fx.unit();
    let diff_tup = fx.tuple(&[st, boolean]);
    let same_tup = fx.tuple(&[st, st]);
    let consume_s = fx.func("consume_s", 0, &[st], unit, no_clauses());
    let use_bool = fx.func("use_bool", 0, &[boolean], unit, no_clauses());
    let diff_fn = fx.func("diff_fn", 0, &[diff_tup], unit, no_clauses());
    let same_fn = fx.func("same_fn", 0, &[same_tup], unit, no_clauses());
    fx.finish();

    let mut facts = Pats { enums: [(s, [0, 0]), (s, [0, 0])], decls: Decls::new() };
    facts.add(&mut fx, consume_s, &[st, unit], &[]);
    facts.add(&mut fx, use_bool, &[boolean, unit], &[]);
    facts.add(&mut fx, diff_fn, &[diff_tup, unit], &[]);
    facts.add(&mut fx, same_fn, &[same_tup, unit], &[]);

    // Different types with identical synthetic name `"__d"` (no `Pat::Or`): must NOT hit `OrPatternBinding`.
    let mut b1 = Bb::new();
    let pp = b1.param("p");
    let d0 = b1.bind("__d");
    let d1 = b1.bind("__d");
    let tup = b1.tuple_pat(&[d0, d1]);
    let lp = b1.local(pp);
    let s1 = b1.let_(tup, None, lp);
    let e2 = call1(&mut b1, "consume_s", d0);
    let s2 = b1.semi(e2);
    let e3 = call1(&mut b1, "use_bool", d1);
    let s3 = b1.semi(e3);
    let root1 = b1.block(&[s1, s2, s3], None);
    let got1 = findings(&fx, &b1, diff_fn, root1, &facts, &facts.decls, Plant::None);
    assert!(got1.as_ref().is_ok_and(|g| g.is_empty()), "different-type synthetic tuple bindings must succeed: {got1:?}");

    // Same non-Copy type with identical synthetic name `"__d"` (no `Pat::Or`): must be two distinct locals!
    let mut b2 = Bb::new();
    let pp = b2.param("p");
    let d0 = b2.bind("__d");
    let d1 = b2.bind("__d");
    let tup = b2.tuple_pat(&[d0, d1]);
    let lp = b2.local(pp);
    let s1 = b2.let_(tup, None, lp);
    let e2 = call1(&mut b2, "consume_s", d0);
    let s2 = b2.semi(e2);
    let e3 = call1(&mut b2, "consume_s", d1);
    let s3 = b2.semi(e3);
    let root2 = b2.block(&[s1, s2, s3], None);
    let got2 = findings(&fx, &b2, same_fn, root2, &facts, &facts.decls, Plant::None);
    assert!(got2.as_ref().is_ok_and(|g| g.is_empty()), "same-type synthetic tuple bindings must be distinct locals: {got2:?}");
}

/// Settling operands with exact lifetime positions (`settle_operand`): copying a `&S` field before
/// a later operand moves its owner (`(h.r, consume(h))`) succeeds, while moving a lifetime-bearing
/// non-`Copy` value before a later borrow of it (`(h, borrow(&h))`) is rejected as `UseMoved`.
#[test]
fn settle_operand_with_lifetimes_copies_before_move_and_rejects_move_before_borrow() {
    use blaze_typeck::Expr;
    let mut fx = Fx::new();
    let copy = fx.tr("Copy", 1, no_clauses(), &[]);
    fx.lang(LangItem::Copy, copy);
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let pair = fx.tuple(&[rs, st]);
    let r_pair = fx.r(Mutbl::Not, pair);
    let unit = fx.unit();
    let consume_pair = fx.func("consume_pair", 0, &[pair], unit, no_clauses());
    let borrow_pair = fx.func("borrow_pair", 0, &[r_pair], unit, no_clauses());
    let ok_fn = fx.func("ok_fn", 0, &[pair], unit, no_clauses());
    let bad_fn = fx.func("bad_fn", 0, &[pair], unit, no_clauses());
    fx.finish();

    let mut facts = Pats { enums: [(s, [0, 0]), (s, [0, 0])], decls: Decls::new() };
    facts.add(&mut fx, consume_pair, &[pair, unit], &[Region::Param(0)]);
    facts.add(&mut fx, borrow_pair, &[r_pair, unit], &[Region::Param(0), Region::Param(0)]);
    facts.add(&mut fx, ok_fn, &[pair, unit], &[Region::Param(0)]);
    facts.add(&mut fx, bad_fn, &[pair, unit], &[Region::Param(0)]);

    // Positive: `let _ = (p.0, consume_pair(p));` (`p.0: &S` is `Copy` with 1 exact lifetime slot).
    let mut b_ok = Bb::new();
    let pp = b_ok.param("p");
    let lp0 = b_ok.local(pp);
    let f0 = b_ok.bool();
    b_ok.exprs[f0.ix()] = Expr::TupleField { base: lp0, index: 0 };
    let mv = call1(&mut b_ok, "consume_pair", pp);
    let tup = b_ok.tuple(&[f0, mv]);
    let st_ok = b_ok.semi(tup);
    let root_ok = b_ok.block(&[st_ok], None);
    let got_ok = findings(&fx, &b_ok, ok_fn, root_ok, &facts, &facts.decls, Plant::None);
    assert!(got_ok.as_ref().is_ok_and(|g| g.is_empty()), "(p.0, consume(p)) must succeed: {got_ok:?}");

    // Negative control: `let _ = (p, borrow_pair(&p));` moves `p` BEFORE `&p` -> `UseMoved`.
    let mut b_bad = Bb::new();
    let pp = b_bad.param("p");
    let lp1 = b_bad.local(pp);
    let bor = call_ref(&mut b_bad, "borrow_pair", Mutbl::Not, pp);
    let tup = b_bad.tuple(&[lp1, bor]);
    let st_bad = b_bad.semi(tup);
    let root_bad = b_bad.block(&[st_bad], None);
    let got_bad = findings(&fx, &b_bad, bad_fn, root_bad, &facts, &facts.decls, Plant::None);
    assert!(
        got_bad.as_ref().is_ok_and(|g| g.iter().any(|l| l.starts_with("move UseOfMoved"))),
        "CONTROL: (p, borrow(&p)) with lifetimes must reject UseOfMoved: {got_bad:?}",
    );
}
