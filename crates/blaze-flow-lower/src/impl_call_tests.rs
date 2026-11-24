//! Piece impl-per-call: a call of `fn take<T: Tr>(x: T::Out)` whose type argument carries a
//! lifetime (`T = Wrap<'_>`) is read through the impl the TYPE CHECKER selected at the call
//! (`Checked::selected`), never refused when that impl's head record decides it (impl_call.rs;
//! the impls' regions declared by hand here, region_fixture.rs, D148).
//!
//! Sources: the Reference, "Associated Items", "Associated types" (the selected impl's `type Out
//! = U;` defines `<T as Tr>::Out` at the call); "Implementations", "Generic implementations"
//! (E0207: the impl's `'a` appears in its self type `Wrap<'a>`, so it is the call's `T`'s);
//! "Lifetime bounds" (`'static` "outlives all other lifetimes"); RFC 2094: a local's borrow ends
//! when the local dies, so a loan that must outlive `'static` is live past it (E0597, `borrow
//! Invalidated`).

use std::string::String;
use std::vec;

use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ExprId, ItemKey, List, Mutbl, Name, PNode, Selected};

use crate::region_fixture::{self, Decls, FnHead, ImplHead, P0};
use crate::{AdtFacts, CallBounds, CalleeFacts, Facts, Form, Plant, Refusal, Region};

/// `take`'s one type parameter has a trait bound (`T: Tr`) and no type-outlives bound.
const TRAIT_BOUNDED: &[u32] = &[0];

const WRAP_IMPL: &str = "impl<'a> Tr for Wrap<'a> { type Out = &'a S; }";
const KEEP_IMPL: &str = "impl<'a> Tr for Keep<'a> { type Out = &'static S; }";

/// The heads' region facts, declared by hand (region_fixture.rs, D148); the ADTs that hold a
/// lifetime.
struct Heads {
    decls: Decls,
    with_lifetime: vec::Vec<ItemKey>,
    take: ItemKey,
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
        let lifetimes = u32::from(self.with_lifetime.contains(&k));
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
    }
    fn call_bounds(&self, item: ItemKey) -> Option<CallBounds<'_>> {
        (item == self.take).then_some(CallBounds { outlives: &[], unread_below: 0, trait_bounded: TRAIT_BOUNDED })
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

/// The world: `S`, `Wrap<'a>`, `Keep<'a>`, `trait Tr { type Out; }`, `impl<'a> Tr for Wrap<'a> {
/// type Out = &'a S; }`, `impl<'a> Tr for Keep<'a> { type Out = &'static S; }`, `fn take<T:
/// Tr>(x: T::Out) {}`, and the owner `bad(s: S)`. Portable types hold no lifetimes: `Wrap` and
/// `Keep` hold one by their `AdtFacts`. `wrap` and `keep` are the structs' items: a body names
/// them as a type in ITS OWN table (`call_take`), never by a `PTy` of the world's.
struct World {
    fx: Fx,
    wrap: ItemKey,
    keep: ItemKey,
    wrap_impl: ItemKey,
    bad: ItemKey,
    heads: Heads,
}

fn world() -> World {
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let unit = fx.unit();
    // One parameter, `Self` (fixture.rs `tr`: the count includes `Self`); with 0 the trait goal
    // `<T as Tr>` has no arguments and the solver refuses it as malformed.
    let tr = fx.tr("Tr", 1, vec![], &["Out"]);
    let w = fx.strukt("Wrap", 0, &[]);
    let wrap = fx.adt(w, &[]);
    let k = fx.strukt("Keep", 0, &[]);
    let keep = fx.adt(k, &[]);
    let wrap_impl = fx.imp(WRAP_IMPL, 0, wrap, Some((tr, &[])), &[("Out", rs)], vec![]);
    let keep_impl = fx.imp(KEEP_IMPL, 0, keep, Some((tr, &[])), &[("Out", rs)], vec![]);
    let p0 = fx.param(0);
    let out = fx.alias(tr, "Out", &[p0]);
    let tb = fx.bound(tr, &[p0]);
    let take = fx.func("take", 1, &[out], unit, vec![tb]);
    let bad = fx.func("bad", 0, &[st], unit, vec![]);
    fx.finish();
    let mut decls = Decls::new();
    // `Wrap<'a>` and `Keep<'a>` declare one lifetime each (as `Facts::adt` says); `Tr` none,
    // and `<_ as Tr>::Out` none (no GAT).
    decls.adt(w, 1);
    decls.adt(k, 1);
    decls.trait_(tr, 0);
    decls.alias(tr, "Out", 0);
    // `fn take<T: Tr>(x: T::Out)`: no lifetime, no slot.
    decls.func(take, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[out], output: unit, slots: &[] });
    // `fn bad(s: S)`: no lifetime.
    decls.func(bad, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[st], output: unit, slots: &[] });
    // WRAP_IMPL, `impl<'a> Tr for Wrap<'a> { type Out = &'a S; }`: `'a` is `P0`; self type
    // `Wrap<'a>` `[P0]`; `Tr` holds no lifetime `[]`; `Out`'s value `&'a S` `[P0]`.
    decls.imp(wrap_impl, ImplHead { lifetimes: 1, outlives: &[], self_ty: wrap, self_slots: &[P0], trait_ref: Some((tr, &[], &[])) });
    decls.impl_assoc(wrap_impl, "Out", rs, &[P0]);
    // KEEP_IMPL, `impl<'a> Tr for Keep<'a> { type Out = &'static S; }`: the same header;
    // `Out`'s value `&'static S` `[Static]`.
    decls.imp(keep_impl, ImplHead { lifetimes: 1, outlives: &[], self_ty: keep, self_slots: &[P0], trait_ref: Some((tr, &[], &[])) });
    decls.impl_assoc(keep_impl, "Out", rs, &[Region::Static]);
    let heads = Heads { decls, with_lifetime: vec![w, k], take };
    World { fx, wrap: w, keep: k, wrap_impl, bad, heads }
}

/// `take::<adt>(&s)` as the body's one statement. The type argument is interned in the
/// body's table: a world `PTy` there indexes another table (`Malformed::TypeOrder`).
fn call_take(b: &mut Bb, adt: ItemKey) -> ExprId {
    let s = b.param("s");
    let t = b.ty(PNode::Adt(adt, List::EMPTY));
    let callee = b.path_args("take", &[t]);
    let ls = b.local(s);
    let arg = b.borrow(Mutbl::Not, ls);
    let call = b.call(callee, &[arg]);
    let st1 = b.semi(call);
    b.block(&[st1], None)
}

/// The body checked by the real type checker; its `selected` rows emptied when `drop_rows` (the
/// control); lowered and checked: the findings, one line each, or the refusal. Also the rows
/// the checker kept for the call, each with its associated type (`None`: a bound row).
fn run(w: &World, b: &Bb, root: ExprId, drop_rows: bool) -> (Result<vec::Vec<String>, Refusal>, vec::Vec<(Option<Name>, Selected)>) {
    let view = b.view(w.bad, root);
    let mut c = region_fixture::checked(&w.fx, &view);
    let rows: vec::Vec<(Option<Name>, Selected)> = c.selected.iter().map(|r| (r.name, r.selected)).collect();
    if drop_rows {
        c.selected.clear();
    }
    let got = region_fixture::lower(&w.fx, &view, &mut c, &w.heads, &w.heads.decls, Plant::None).map(|r| r.lines);
    (got, rows)
}

/// `fn bad(s: S) { take::<Wrap<'_>>(&s); }` with `impl<'a> Tr for Wrap<'a> { type Out = &'a S;
/// }`: the parameter is `&'x S` for the call's own `'x` (the type argument's), which nothing
/// else constrains: CLEAN, and related, not refused. The checker kept ONE row for the call,
/// the projection `<Wrap as Tr>::Out`'s, the impl for `Wrap`; it is the bound `Wrap: Tr`'s row
/// too. A projection is proven through its trait reference's source (rustc's next solver:
/// `NormalizesTo` merges by the trait goal's `TraitGoalProvenVia`, assembly/mod.rs
/// `assemble_and_merge_candidates`), so the call's bound of the same trait at the same
/// arguments has no row of its own (rules/call.rs `record_lifetime_impls` skips it; rule 77),
/// and the borrow check reads a bound from any row of its trait and self type (trait_ties.rs
/// `tie_by_impl`). CONTROL (rule 23): the same record with its `selected` rows emptied is
/// REFUSED by name, `CallHiddenInput { input: 0 }` (the state before this piece).
/// Prints `an_impl_lifetime_relates_exactly selected: [(Some(..), Impl { .. })]`, `.. got: Ok([])`,
/// `.. without rows (control): Err(Refusal { form: CallHiddenInput { .., input: 0 }, .. })`.
#[test]
fn an_impl_lifetime_relates_exactly() {
    let w = world();
    let mut b = Bb::new();
    let root = call_take(&mut b, w.wrap);
    let (got, rows) = run(&w, &b, root, false);
    std::println!("an_impl_lifetime_relates_exactly selected: {rows:?}");
    assert!(
        matches!(rows[..], [(Some(name), Selected::Impl { imp, .. })] if name == Name::of("Out") && imp == w.wrap_impl),
        "the checker's one row: the projection `<Wrap as Tr>::Out`'s (the bound `Wrap: Tr`'s too), the impl for `Wrap`: {rows:?}"
    );
    std::println!("an_impl_lifetime_relates_exactly got: {got:?}");
    assert_eq!(got, Ok(vec::Vec::new()), "`type Out = &'a S` demands nothing of `&s`: clean");
    let mut b = Bb::new();
    let root = call_take(&mut b, w.wrap);
    let (got, _) = run(&w, &b, root, true);
    std::println!("an_impl_lifetime_relates_exactly without rows (control): {got:?}");
    assert!(matches!(got, Err(Refusal { form: Form::OneRule { .. }, .. })), "{got:?}");
}

/// `fn bad(s: S) { take::<Keep<'_>>(&s); }` with `impl<'a> Tr for Keep<'a> { type Out =
/// &'static S; }`: the parameter is `&'static S`, and `s` dies at the end of `bad`: E0597
/// (`borrow Invalidated`), found through the selected impl's `type Out` column.
/// Prints `a_static_impl_type_still_errors: Ok(["borrow Invalidated ..`.
#[test]
fn a_static_impl_type_still_errors() {
    let w = world();
    let mut b = Bb::new();
    let root = call_take(&mut b, w.keep);
    let (got, rows) = run(&w, &b, root, false);
    std::println!("a_static_impl_type_still_errors: {got:?}");
    assert!(
        matches!(rows[..], [(Some(name), Selected::Impl { .. })] if name == Name::of("Out")),
        "the checker's one row: the projection `<Keep as Tr>::Out`'s (the bound `Keep: Tr`'s too): {rows:?}"
    );
    let lines = got.expect("related through the impl, not refused");
    assert!(lines.iter().any(|l| l.starts_with("borrow Invalidated")), "E0597: {lines:?}");
}
