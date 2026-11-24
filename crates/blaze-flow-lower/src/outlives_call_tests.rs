//! Piece L4 (SOUNDNESS): a call's type-outlives bound and an opaque output's captured lifetime
//! are read from the head records' region facts (D148, `RegionFacts`), never from a text. Each
//! test lowers a body checked by the real type checker, with the callee's column, shape and
//! bounds as the head records give them, and checks it with blaze-flow.
//!
//! Sources: the Reference, "Trait and lifetime bounds" (`T: 'static`: "T outlives 'static",
//! so a `T = &'x S` must have `'x: 'static`); "Lifetime elision" / "impl Trait", "Capturing"
//! (edition 2024: a return-position `impl Trait` captures every lifetime in scope, so `fn h(x:
//! &S) -> impl Sized` returns a value whose type holds `x`'s lifetime); RFC 2094: a local's
//! borrow ends when the local dies, so a loan that must outlive `'static` is live past it
//! (E0597, `borrow Invalidated`).

use std::string::String;
use std::vec;

use blaze_own::Alloc;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::regions::{Arity, Count};
use blaze_typeck::{ExprId, ItemKey, List, Mutbl, Name, PClause, PNode, PTy};

use crate::region_fixture::{Decls, FnHead};
use crate::{AdtFacts, CallBounds, CalleeFacts, Facts, OutlivesType, Plant, Refusal, Region, ShapeBuf, Shape, Stats, read_shape};

fn alloc() -> Alloc<'static> {
    Alloc::global()
}

/// The counts the one region counter reads: every ADT none, the opaque `captures`.
struct Counts {
    opaque: ItemKey,
    captures: u32,
}

impl Arity for Counts {
    fn adt_lifetimes(&self, _: ItemKey) -> Count {
        Count::Known(0)
    }
    fn opaque_captures(&self, o: ItemKey) -> Count {
        if o == self.opaque { Count::Known(self.captures) } else { Count::NotKnown }
    }
    fn alias_lifetimes(&self, _: ItemKey, _: Name) -> Count {
        Count::NotKnown
    }
    fn trait_lifetimes(&self, _: ItemKey) -> Count {
        Count::NotKnown
    }
}

/// The heads: each fn's region head ([`Decls`], laid in the checked record's own table,
/// region_fixture.rs `lower`), shape and call bounds.
struct Heads {
    decls: Decls,
    shapes: vec::Vec<(ItemKey, ShapeBuf<Alloc<'static>>)>,
    bounds: vec::Vec<(ItemKey, vec::Vec<(OutlivesType, Region)>)>,
}

impl Heads {
    /// Fn `item` with `lifetimes` early-bound lifetime parameters, its column `slots` laid over
    /// `fn(inputs..) -> output`, its shape read from the world's `tys`.
    #[expect(clippy::too_many_arguments, reason = "one head's parts")]
    fn add(&mut self, fx: &Fx, arity: &Counts, item: ItemKey, lifetimes: u32, inputs: &[PTy], output: PTy, slots: &[Region]) {
        self.decls.func(item, FnHead { lifetimes, late: &[], outlives: &[], inputs, output, slots });
        let mut shape = ShapeBuf::new_in(alloc());
        if let Err(why) = read_shape(fx.t.view(), inputs, output, arity, &mut shape, alloc()) {
            panic!("the shape of {item:?} was not read: {why:?}");
        }
        self.shapes.push((item, shape));
        // Every head's bounds are filed, none for a head that declares none, as blaze-ladder
        // front2/call_generics.rs files a row per fn. A head with no row is not one the
        // generic signature can relate: `h` without one went the hidden path's coarse
        // output (hidden_call.rs `call_through_hidden`, `calls_hidden_static`), which puts
        // the input's loan into the output whatever the opaque captures.
        self.bounds.push((item, vec::Vec::new()));
    }
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
    fn object_trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<bool> {
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
        // `needs_drop` as hidden_call_tests.rs's: the parameter's drop at the body's end is the
        // access a live `'static` loan conflicts with.
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: true, lifetimes: 0, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
    }
    fn shape(&self, item: ItemKey) -> Option<Shape<'_>> {
        self.shapes.iter().find(|(k, _)| *k == item).map(|(_, s)| s.shape())
    }
    fn call_bounds(&self, item: ItemKey) -> Option<CallBounds<'_>> {
        let (_, b) = self.bounds.iter().find(|(k, _)| *k == item)?;
        Some(CallBounds { outlives: b, unread_below: 0, trait_bounded: &[] })
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

/// The findings of one body, one line each, and the lowering's counts; or the refusal. The
/// heads are laid into the checked record's own table (region_fixture.rs `run`), never into a
/// table of their own the lowering would read the record's `PTy`s in.
fn run(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId, facts: &Heads) -> Result<(vec::Vec<String>, Stats), Refusal> {
    let r = crate::region_fixture::run(fx, &b.view(owner, root), facts, &facts.decls, Plant::None)?;
    Ok((r.lines, r.stats))
}

/// The world: `S`; `fn g<T: 'static>(t: T)`; `fn h(x: &S) -> impl Sized` (the opaque `o`
/// capturing `x`'s lifetime when `captures` is 1); the owners `bad(s: S)`, `good(s: &'static
/// S)`, `within(s: S)`, `escape(s: S)`. The `T: 'static` row is filed only when `static_row`.
struct World {
    fx: Fx,
    s: ItemKey,
    o: ItemKey,
    bad: ItemKey,
    good: ItemKey,
    within: ItemKey,
    escape: ItemKey,
    heads: Heads,
}

fn world(static_row: bool, captures: u32) -> World {
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let unit = fx.unit();
    let p0 = fx.param(0);
    let g_clauses = if static_row { vec![PClause::TypeOutlives { ty: p0, region: Region::Static }] } else { vec![] };
    let g = fx.func("g", 1, &[p0], unit, g_clauses);
    let o = fx.opaque_key("h::{opaque#0}");
    fx.set_opaque_bounds(o, vec![]);
    let ot = fx.ty(PNode::Opaque(o, List::EMPTY));
    let h = fx.func("h", 0, &[rs], ot, vec![]);
    let bad = fx.func("bad", 0, &[st], unit, vec![]);
    let good = fx.func("good", 0, &[rs], unit, vec![]);
    let within = fx.func("within", 0, &[st], unit, vec![]);
    let escape = fx.func("escape", 0, &[st], unit, vec![]);
    fx.finish();
    let arity = Counts { opaque: o, captures };
    let mut decls = Decls::new();
    decls.opaque(o, captures);
    let mut heads = Heads { decls, shapes: vec::Vec::new(), bounds: vec::Vec::new() };
    heads.add(&fx, &arity, g, 0, &[p0], unit, &[]);
    if static_row {
        heads.decls.clause(g, 0, 0, p0, &[]);
        heads.decls.type_outlives(g, 0, Region::Static);
        if let Some((_, b)) = heads.bounds.iter_mut().find(|(k, _)| *k == g) {
            b.push((OutlivesType::Param(0), Region::Static));
        }
    }
    let h_slots: &[Region] = if captures == 1 { &[Region::Param(0), Region::Param(0)] } else { &[Region::Param(0)] };
    heads.add(&fx, &arity, h, 1, &[rs], ot, h_slots);
    heads.add(&fx, &arity, bad, 0, &[st], unit, &[]);
    heads.add(&fx, &arity, good, 0, &[rs], unit, &[Region::Static]);
    heads.add(&fx, &arity, within, 0, &[st], unit, &[]);
    heads.add(&fx, &arity, escape, 0, &[st], unit, &[]);
    World { fx, s, o, bad, good, within, escape, heads }
}

/// SOUNDNESS. `fn bad(s: S) { g::<&S>(&s); }` with `fn g<T: 'static>(t: T)`: `T = &'x S` must
/// have `'x: 'static`, and the borrow of `s` ends when `s` dies: E0597. `fn good(s: &'static S)
/// { g::<&S>(s); }` is clean.
/// CONTROL (rule 23): with the `T: 'static` row not filed, `bad` is ACCEPTED (no finding): the
/// row read from the region facts is the cause.
#[test]
fn a_static_bound_refuses_a_local_borrow() {
    for (static_row, owner, borrow, want_error) in [(true, "bad", true, true), (true, "good", false, false), (false, "bad", true, false)] {
        let w = world(static_row, 1);
        let mut b = Bb::new();
        let s = b.param("s");
        let st = b.ty(PNode::Adt(w.s, List::EMPTY));
        let rs = b.ty(PNode::Ref(Mutbl::Not, st));
        let callee = b.path_args("g", &[rs]);
        let ls = b.local(s);
        let arg = if borrow { b.borrow(Mutbl::Not, ls) } else { ls };
        let call = b.call(callee, &[arg]);
        let st1 = b.semi(call);
        let root = b.block(&[st1], None);
        let item = if owner == "bad" { w.bad } else { w.good };
        let (lines, stats) = run(&w.fx, &b, item, root, &w.heads).expect("lowered");
        std::println!("a_static_bound_refuses_a_local_borrow row {static_row} {owner}: {lines:?}; {stats:?}");
        assert_eq!(lines.iter().any(|l| l.starts_with("borrow Invalidated")), want_error, "row {static_row} {owner}: {lines:?}");
    }
}

/// An opaque output. `fn within(s: S) { let _y = h(&s); }`: the opaque holds `s`'s borrow and
/// dies before `s`: clean. `fn escape(s: S) { g::<impl Sized>(h(&s)); }`: the opaque holds the
/// borrow and must outlive `'static`: E0597.
/// CONTROL (rule 23): with the opaque capturing nothing (its column without the captured
/// slot), `escape` is ACCEPTED: the capture read from the facts is the cause.
#[test]
fn an_opaque_carries_its_captured_lifetime() {
    for (captures, owner, want_error) in [(1, "within", false), (1, "escape", true), (0, "escape", false)] {
        let w = world(true, captures);
        let mut b = Bb::new();
        let s = b.param("s");
        let ls = b.local(s);
        let arg = b.borrow(Mutbl::Not, ls);
        let callee = b.path_args("h", &[]);
        let made = b.call(callee, &[arg]);
        let (item, st1) = if owner == "within" {
            let y = b.bind("_y");
            (w.within, b.let_(y, None, made))
        } else {
            let ot = b.ty(PNode::Opaque(w.o, List::EMPTY));
            let g = b.path_args("g", &[ot]);
            let call = b.call(g, &[made]);
            (w.escape, b.semi(call))
        };
        let root = b.block(&[st1], None);
        let (lines, stats) = run(&w.fx, &b, item, root, &w.heads).expect("lowered");
        std::println!("an_opaque_carries_its_captured_lifetime captures {captures} {owner}: {lines:?}; {stats:?}");
        // The step first: `h`'s call (and `g`'s in `escape`) went by the generic signature,
        // never the hidden path's coarse output, which relates the input to the output
        // whatever the captures and so cannot show the capture is the cause.
        let calls = if owner == "within" { 1 } else { 2 };
        assert_eq!((stats.calls_by_signature, stats.calls_hidden_static), (calls, 0), "captures {captures} {owner}: {stats:?}");
        assert_eq!(lines.iter().any(|l| l.starts_with("borrow Invalidated")), want_error, "captures {captures} {owner}: {lines:?}");
    }
}

/// An opaque output's ITEM BOUNDS decide what its value keeps live (rustc_borrowck liveness,
/// rust-lang/rust#116733; lib.rs `opaque_live`, generic_call.rs `Proj::live`): the world `S`,
/// `fn take(s: S)`, `fn k<T>(t: T)`, and `fn h2<'a, 'b>(a: &'a S, b: &'b S) -> impl Sized +
/// BOUND` (the opaque capturing both, Rust 2024), the body
/// `fn owner(a: S, b: S) { let o = h2(&a, &b); take(MOVED); k::<impl Sized>(o); }`.
///
/// - `+ 'static`: `o` keeps nothing live; moving `a` (or `b`) while `o` lives: CLEAN, as rustc
///   accepts (front2_precise_capture_borrows.rs `uses_keep`).
/// - `+ 'a`: only `a`'s borrow is live through `o`: moving `b`: CLEAN (`uses_tag`); TWIN moving
///   `a`: a finding (E0505, `uses_tag_self`'s shape).
/// - TWIN no bound: every capture is live: moving `b`: a finding (`uses_hold`).
///
/// CONTROL (rule 23): the no-bound twin is the same body and head without the opaque's
/// `Outlives::Type`: the bound read from the opaque's head is the cause.
#[test]
fn an_opaques_bound_decides_what_its_value_keeps_live() {
    // (bound, move `a` (else `b`), a finding wanted)
    let cases = [(Some(Region::Static), true, false), (Some(Region::Param(0)), false, false), (Some(Region::Param(0)), true, true), (None, false, true)];
    let mut wrong = vec::Vec::new();
    for (bound, move_a, want) in cases {
        let mut fx = Fx::new();
        let s = fx.strukt("S", 0, &[]);
        let st = fx.adt(s, &[]);
        let rs = fx.r(Mutbl::Not, st);
        let unit = fx.unit();
        let p0 = fx.param(0);
        let take = fx.func("take", 0, &[st], unit, vec![]);
        let k = fx.func("k", 1, &[p0], unit, vec![]);
        let o = fx.opaque_key("h2::{opaque#0}");
        fx.set_opaque_bounds(o, vec![]);
        let ot = fx.ty(PNode::Opaque(o, List::EMPTY));
        let h2 = fx.func("h2", 0, &[rs, rs], ot, vec![]);
        let owner = fx.func("owner", 0, &[st, st], unit, vec![]);
        fx.finish();
        let arity = Counts { opaque: o, captures: 2 };
        let mut decls = Decls::new();
        decls.opaque(o, 2);
        if let Some(r) = bound {
            decls.opaque_outlives(o, 2, 0, r);
        }
        let mut heads = Heads { decls, shapes: vec::Vec::new(), bounds: vec::Vec::new() };
        heads.add(&fx, &arity, take, 0, &[st], unit, &[]);
        heads.add(&fx, &arity, k, 0, &[p0], unit, &[]);
        // `&'0 S`, `&'1 S`, the opaque's captures `['0, '1]`.
        heads.add(&fx, &arity, h2, 2, &[rs, rs], ot, &[Region::Param(0), Region::Param(1), Region::Param(0), Region::Param(1)]);
        heads.add(&fx, &arity, owner, 0, &[st, st], unit, &[]);
        let mut b = Bb::new();
        let pa = b.param("a");
        let pb = b.param("b");
        let la = b.local(pa);
        let ra = b.borrow(Mutbl::Not, la);
        let lb = b.local(pb);
        let rb = b.borrow(Mutbl::Not, lb);
        let h2_ = b.path_args("h2", &[]);
        let made = b.call(h2_, &[ra, rb]);
        let y = b.bind("o");
        let s0 = b.let_(y, None, made);
        let take_ = b.path_args("take", &[]);
        let moved = b.local(if move_a { pa } else { pb });
        let tc = b.call(take_, &[moved]);
        let s1 = b.semi(tc);
        let oty = b.ty(PNode::Opaque(o, List::EMPTY));
        let k_ = b.path_args("k", &[oty]);
        let ly = b.local(y);
        let kc = b.call(k_, &[ly]);
        let s2 = b.semi(kc);
        let root = b.block(&[s0, s1, s2], None);
        let (lines, stats) = run(&fx, &b, owner, root, &heads).expect("lowered");
        std::println!("an_opaques_bound_decides_what_its_value_keeps_live bound {bound:?} move_a {move_a}: {lines:?}; {stats:?}");
        // `h2`'s call went by the generic signature (the path the bound is read on), never
        // the hidden path's coarse output.
        if stats.calls_hidden_static != 0 || lines.is_empty() == want {
            wrong.push(std::format!("bound {bound:?} move_a {move_a}: want a finding {want}: {lines:?}; {stats:?}"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:?}");
}
