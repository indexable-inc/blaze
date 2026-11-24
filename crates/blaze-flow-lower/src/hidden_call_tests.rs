//! Piece sig-assoc-per-call (SOUNDNESS): a call of a head with a hidden input, `fn take<T:
//! Tr>(x: T::Out)`, is related per call (hidden_call.rs). Each test declares the heads' region
//! facts by hand (region_fixture.rs, D148: the column and lifetimes the head lowering lays),
//! lowers a body checked by the real type checker, and checks it with blaze-flow.
//!
//! Sources: the Reference, "Implementations", "Generic implementations" (an impl's lifetime
//! appears in its self type or trait: E0207, so with `T = Foo`, no lifetime, every lifetime of
//! `<Foo as Tr>::Out` is `'static`); "Associated Items", "Associated types" (`impl Tr for Foo {
//! type Out = &'static S; }` defines `<Foo as Tr>::Out` as `&'static S`; the fixture's erased
//! `&S`, its text's `'static` being what E0207 leaves); "Lifetime bounds" (`'static` "outlives all
//! other lifetimes"); "Temporary scopes" / RFC 2094: a local's borrow ends when the local dies,
//! so a loan that must outlive `'static` is live past it (E0597, `borrow Invalidated`).

use std::string::String;
use std::vec;

use blaze_typeck::fixture::{Bb, Fx, rung_7c};
use blaze_typeck::{ExprId, ItemKey, List, Mutbl, Name, PNode, PTy};

use crate::region_fixture::{self, Decls, FnHead, P0, P1, P2};
use crate::{AdtFacts, CallBounds, CalleeFacts, Facts, Form, Plant, Refusal, Region, Stats};

/// `take`'s one type parameter has a trait bound (`T: Tr`) and no type-outlives bound.
const TRAIT_BOUNDED: &[u32] = &[0];

/// `fn take<T: Tr>(x: T::Out)`'s column: `T::Out` has no lifetime of its own (`Tr` declares
/// none, `Out` is no GAT), so no slot.
const TAKE_SLOTS: &[Region] = &[];

/// The heads' region facts, each declared by hand; `take`'s bounds as its head says.
struct Heads {
    decls: Decls,
    take: ItemKey,
}

impl Heads {
    fn add(&mut self, item: ItemKey, lifetimes: u32, late: &[u32], inputs: &[PTy], output: PTy, slots: &[Region]) {
        self.decls.func(item, FnHead { lifetimes, late, outlives: &[], inputs, output, slots });
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
    fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
        // `needs_drop` as tests.rs's `Plain` (whose fn-pointer `'static` test finds E0597): the
        // parameter's drop at the body's end is the access the live `'static` loan conflicts
        // with. (With `needs_drop: false` the run at 21:24 found nothing though the demand was
        // made: the parameter's `StorageDead` alone did not conflict; reported as its own gap.)
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: true, lifetimes: 0, union: false, is_enum: false, is_box: false })
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

/// The findings of one body, one line each, and the lowering's counts; or the refusal. The
/// heads are laid in the body's record (region_fixture.rs `run`).
fn run(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId, facts: &Heads, plant: Plant) -> Result<(vec::Vec<String>, Stats), Refusal> {
    region_fixture::run(fx, &b.view(owner, root), facts, &facts.decls, plant).map(|r| (r.lines, r.stats))
}

/// The world: `S`, `Foo`, `trait Tr { type Out; }`, `impl Tr for Foo { type Out = &'static S;
/// }`, `fn take<T: Tr>(x: T::Out) {}`, and the owners `bad(s: S)`, `good(s: &'static S)`,
/// `pass<U: Tr>(x: U::Out)`, `bound<U: Tr<Out = &S>>(s: &S)`.
struct World {
    fx: Fx,
    /// `Foo`'s key: a turbofish's types are written in the BODY's table (`Bb::ty`), not the
    /// world's (blaze-typeck suite/const_enum.rs makes its argument with `b.variant_value`).
    foo: ItemKey,
    take: ItemKey,
    bad: ItemKey,
    good: ItemKey,
    pass: ItemKey,
    bound: ItemKey,
    heads: Heads,
}

fn world() -> World {
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let unit = fx.unit();
    // A trait's generics count `Self` (blaze-build tests/operators.rs: `tr("Neg", 1, ..)` with
    // `alias(tr, "Output", &[p0])`).
    let tr = fx.tr("Tr", 1, vec![], &["Out"]);
    let foo = fx.strukt("Foo", 0, &[]);
    let foot = fx.adt(foo, &[]); // the world's `Foo`, the impl's self type
    let imp = fx.imp("impl Tr for Foo", 0, foot, Some((tr, &[])), &[("Out", rs)], vec![]);
    let p0 = fx.param(0);
    let out = fx.alias(tr, "Out", &[p0]);
    let tb = fx.bound(tr, &[p0]);
    let take = fx.func("take", 1, &[out], unit, vec![tb]);
    let bad = fx.func("bad", 0, &[st], unit, vec![]);
    let good = fx.func("good", 0, &[rs], unit, vec![]);
    let pb = fx.bound(tr, &[p0]);
    let pass = fx.func("pass", 1, &[out], unit, vec![pb]);
    let bb = fx.bound(tr, &[p0]);
    let bp = fx.proj(tr, &[p0], "Out", rs);
    let bound = fx.func("bound", 1, &[rs], unit, vec![bb, bp]);
    fx.finish();
    let mut heads = Heads { decls: Decls::new(), take };
    heads.decls.trait_(tr, 0);
    heads.decls.alias(tr, "Out", 0);
    heads.decls.adt(foo, 0);
    heads.decls.adt(s, 0);
    heads.decls.imp(imp, crate::region_fixture::ImplHead { lifetimes: 0, self_ty: foot, self_slots: &[], trait_ref: Some((tr, &[], &[])), outlives: &[] });
    heads.decls.impl_assoc(imp, "Out", rs, &[Region::Static]);
    // `fn take<T: Tr>(x: T::Out)`: no lifetime.
    heads.add(take, 0, &[], &[out], unit, TAKE_SLOTS);
    // `fn bad(s: S)`: no lifetime.
    heads.add(bad, 0, &[], &[st], unit, &[]);
    // `fn good(s: &'static S)`: `[Static]`.
    heads.add(good, 0, &[], &[rs], unit, &[Region::Static]);
    // `fn pass<U: Tr>(x: U::Out)`: no lifetime.
    heads.add(pass, 0, &[], &[out], unit, &[]);
    // `fn bound<U: Tr<Out = &S>>(s: &S)`: `s`'s elided lifetime `P0`, late-bound (the clause's
    // own column is not declared: the call is refused before any clause is read).
    heads.add(bound, 1, &[0], &[rs], unit, &[P0]);
    World { fx, foo, take, bad, good, pass, bound, heads }
}

/// `take::<T>(arg)` as the body's one statement, `T` the node `t` in the body's table, `arg`
/// the parameter `s` borrowed (`borrow`) or as it is.
fn call_take(b: &mut Bb, t: PNode, borrow: bool) -> ExprId {
    let s = b.param("s");
    let t = b.ty(t);
    let callee = b.path_args("take", &[t]);
    let ls = b.local(s);
    let arg = if borrow { b.borrow(Mutbl::Not, ls) } else { ls };
    let call = b.call(callee, &[arg]);
    let st1 = b.semi(call);
    b.block(&[st1], None)
}

/// SOUNDNESS. `fn bad(s: S) { take::<Foo>(&s); }` with `impl Tr for Foo { type Out = &'static
/// S; }`: the parameter is `&'static S` at this call, and the borrow of `s` ends when `s` dies:
/// E0597 (`borrow Invalidated`). The call's type argument `Foo` carries no lifetime, so by
/// E0207 every lifetime at the hidden position is `'static` (hidden_call.rs, "known at the
/// call"): related by the signature expanded (`calls_hidden_static` 1, `calls_by_signature` 1).
/// `fn good(s: &'static S) { take::<Foo>(s); }` is right: clean.
/// CONTROL (rule 23): `Plant::HiddenCoarse`, the coarse path as it was before this piece,
/// ACCEPTS `bad` (no finding): the missed error.
/// Prints `a_static_assoc_type_is_demanded_of_the_argument bad: ["borrow Invalidated ..`, `..
/// good: []`, `.. coarse (control) bad: []`.
#[test]
fn a_static_assoc_type_is_demanded_of_the_argument() {
    let w = world();
    let mut b = Bb::new();
    let root = call_take(&mut b, PNode::Adt(w.foo, List::EMPTY), true);
    let (lines, stats) = run(&w.fx, &b, w.bad, root, &w.heads, Plant::None).expect("lowered");
    std::println!("a_static_assoc_type_is_demanded_of_the_argument bad: {lines:?}; {stats:?}");
    assert_eq!(stats.calls_by_signature, 1, "{stats:?}; {lines:?}");
    assert!(lines.iter().any(|l| l.starts_with("borrow Invalidated")), "E0597: {lines:?}");
    let mut b = Bb::new();
    let root = call_take(&mut b, PNode::Adt(w.foo, List::EMPTY), false);
    let (lines, stats) = run(&w.fx, &b, w.good, root, &w.heads, Plant::None).expect("lowered");
    std::println!("a_static_assoc_type_is_demanded_of_the_argument good: {lines:?}");
    assert_eq!(lines, vec::Vec::<String>::new(), "a `'static` borrow is passed: clean");
    assert_eq!(stats.calls_by_signature, 1, "{stats:?}");
    let _ = w.take;
}

/// The GENERIC CALLER. `fn pass<U: Tr>(x: U::Out) { take::<U>(x); }`: the argument's type is the
/// parameter's projection itself, so its lifetimes are the parameter's: nothing is demanded,
/// clean (never a false `'static` demand). `fn bound<U: Tr<Out = &S>>(s: &S) { take::<U>(s); }`:
/// the impl is not known at the call and the argument carries an origin at the hidden position:
/// REFUSED by name (`Form::CallHiddenInput { input: 0 }`), never related coarsely (which would
/// accept a `U` whose impl writes `'static`... and never demanded `'static`, which would be a
/// false error here).
/// Prints `a_generic_caller_is_conservative pass: Ok([])`, `.. bound: Err(Refusal { form:
/// CallHiddenInput { .., input: 0 }, .. })`.
#[test]
fn a_generic_caller_is_conservative() {
    let w = world();
    let mut b = Bb::new();
    let x = b.param("x");
    let u = b.ty(PNode::Param(0));
    let callee = b.path_args("take", &[u]);
    let lx = b.local(x);
    let call = b.call(callee, &[lx]);
    let st1 = b.semi(call);
    let root = b.block(&[st1], None);
    let got = run(&w.fx, &b, w.pass, root, &w.heads, Plant::None).map(|(l, _)| l);
    std::println!("a_generic_caller_is_conservative pass: {got:?}");
    assert_eq!(got, Ok(vec::Vec::new()), "the same projection passed on: clean");
    let mut b = Bb::new();
    let root = call_take(&mut b, PNode::Param(0), false);
    let got = run(&w.fx, &b, w.bound, root, &w.heads, Plant::None).map(|(l, _)| l);
    std::println!("a_generic_caller_is_conservative bound: {got:?}");
    assert!(matches!(got, Err(Refusal { form: Form::OneRule { .. }, .. })), "{got:?}");
}

/// Facts of rung 7c's world (blaze-typeck fixture.rs `rung_7c`, the hello world on `core`'s own
/// lang items): its enums are enums of one field per variant; no type has drop glue or a
/// lifetime; no head writes a lifetime; `Try`'s and `FromResidual`'s fns bound `Self` by their
/// trait.
struct Rung {
    decls: Decls,
    enums: [ItemKey; 4],
    trait_fns: [ItemKey; 2],
}

impl Facts for Rung {
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
        let is_enum = self.enums.contains(&k);
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes: 0, union: false, is_enum, is_box: false })
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
    }
    fn call_bounds(&self, item: ItemKey) -> Option<CallBounds<'_>> {
        let trait_bounded: &[u32] = if self.trait_fns.contains(&item) { TRAIT_BOUNDED } else { &[] };
        Some(CallBounds { outlives: &[], unread_below: 0, trait_bounded })
    }
    fn field(&self, _: ItemKey, _: u32, _: Name) -> Option<u32> {
        None
    }
    fn field_count(&self, adt: ItemKey, _: u32) -> Option<u32> {
        Some(u32::from(self.enums.contains(&adt)))
    }
    fn param_copy(&self, _: u32) -> bool {
        false
    }
}

/// Fn `name`'s head in rung 7c's world, its declared types the world's (`Fx::sig_of`).
fn rung_head(fx: &Fx, decls: &mut Decls, name: &str, lifetimes: u32, late: &[u32], slots: &[Region]) {
    let item = fx.key_of(name);
    let Some(sig) = fx.sig_of(item) else { panic!("{name} is no fn") };
    decls.func(item, FnHead { lifetimes, late, outlives: &[], inputs: sig.inputs, output: sig.output, slots });
}

/// THE HELLO WORLD'S SHAPE (the regression a real run met, wave-1119 round 4). Rung 7c's
/// `fn main(cx: &Cx, io: Io) -> Result<(), Error> { io.need_out()?.line(cx, "Hello, world!")?;
/// Ok(()) }`, with `trait Try { type Output; type Residual; fn branch(self) ->
/// ControlFlow<Self::Residual, Self::Output>; }`, `trait FromResidual<R> { fn
/// from_residual(residual: R) -> Self; }` and `impl Out { fn line(&self, cx: &Cx, text: &str)
/// -> Result<(), OutError>; }`. Each `?` calls `Try::branch(e)`, whose output holds TWO
/// projection subtrees and no slot (`Try` and its associated types have no lifetime): laid as
/// two hidden positions, each of none of the column's slots (lib.rs `lay_positions`). The type
/// arguments (`Result<Out, Missing>`, `Result<(), OutError>`) carry no origin, so each call is
/// known (hidden_call.rs) and its destination (`ControlFlow<Result<Infallible, _>, _>`) holds
/// no position: the whole call by the signature (`calls_hidden_static` 2). The body is checked
/// whole: no finding, no refusal.
/// CONTROL (rule 23): `Plant::HiddenSubtreesApart`, two subtrees refused as in the wave's L3
/// head: the first `?` is refused `CallHiddenInput { input: u32::MAX, why: NotLaid }`, the
/// note the real run printed.
/// Prints `the_question_mark_is_checked_whole: Ok(..)`, `.. (control): Err(Refusal { form:
/// CallHiddenInput { .., input: 4294967295, why: NotLaid }, .. })`.
#[test]
fn the_question_mark_is_checked_whole() {
    let (fx, s) = rung_7c();
    let try_tr = fx.key_of("Try");
    let from_residual_tr = fx.key_of("FromResidual");
    let mut decls = Decls::new();
    decls.trait_(try_tr, 0);
    decls.trait_(from_residual_tr, 0);
    decls.alias(try_tr, "Output", 0);
    decls.alias(try_tr, "Residual", 0);
    // `fn main(cx: &Cx, io: Io)`: `cx`'s elided lifetime, late-bound.
    rung_head(&fx, &mut decls, "main", 1, &[0], &[P0]);
    // `fn need_out(&self) -> Result<Out, Missing>`: `self`'s elided lifetime.
    rung_head(&fx, &mut decls, "need_out", 1, &[0], &[P0]);
    // `fn line(&self, cx: &Cx, text: &str) -> Result<(), OutError>`: three elided.
    rung_head(&fx, &mut decls, "line", 3, &[0, 1, 2], &[P0, P1, P2]);
    // `fn branch(self) -> ControlFlow<Self::Residual, Self::Output>`: no lifetime, no slot.
    rung_head(&fx, &mut decls, "branch", 0, &[], &[]);
    // `fn from_residual(residual: R) -> Self`: none.
    rung_head(&fx, &mut decls, "from_residual", 0, &[], &[]);
    let enums = [s.result, s.control_flow, s.option, fx.key_of("Infallible")];
    let trait_fns = [fx.key_of("branch"), fx.key_of("from_residual")];
    let facts = Rung { decls, enums, trait_fns };
    let body = |b: &mut Bb| {
        let cx = b.param("cx");
        let io = b.param("io");
        let e0 = b.local(io);
        let e1 = b.method(e0, "need_out", &[]);
        let e9 = b.try_op(e1);
        let e10 = b.local(cx);
        let e11 = b.str_();
        let e12 = b.method(e9, "line", &[e10, e11]);
        let e20 = b.try_op(e12);
        let s0 = b.semi(e20);
        let e21 = b.path(&["Ok"]);
        let e22 = b.tuple(&[]);
        let e23 = b.call(e21, &[e22]);
        b.block(&[s0], Some(e23))
    };
    let mut b = Bb::new();
    let root = body(&mut b);
    let got = region_fixture::run(&fx, &b.view(s.main, root), &facts, &facts.decls, Plant::None);
    std::println!("the_question_mark_is_checked_whole: {:?}", got.as_ref().map(|r| (&r.lines, r.stats)));
    let run = got.expect("the hello world is lowered whole");
    assert_eq!(run.lines, vec::Vec::<String>::new(), "clean: {}", run.flow);
    assert!(run.stats.calls_by_signature >= 2, "{:?}", run.stats);
}

/// A CONSTANT AT A HIDDEN INPUT. `fn host() { take::<Bar>("x"); }` with `impl Tr for Bar {
/// type Out = &'static str; }`: the literal holds no origin, but `call_laid` counts its
/// positions by its type (one), so the hidden input is expanded to one `'static` region and the
/// call goes by the signature, clean (`calls_hidden_static` 1). CONTROL (rule 23):
/// `Plant::HiddenConstantZero`, the constant expanded to none as in the wave's L3 head: the
/// call is refused `CallHiddenInput { input: 0, why: OperandUnaligned { laid: 0, call: 1 } }`.
/// Prints `a_constant_at_a_hidden_input_is_counted: Ok([])`, `.. (control): Err(..)`.
#[test]
fn a_constant_at_a_hidden_input_is_counted() {
    let mut fx = Fx::new();
    let str_ = fx.ty(PNode::Str);
    let rstr = fx.r(Mutbl::Not, str_);
    let unit = fx.unit();
    let tr = fx.tr("Tr", 1, vec![], &["Out"]);
    let bar = fx.strukt("Bar", 0, &[]);
    let bart = fx.adt(bar, &[]);
    let imp = fx.imp("impl Tr for Bar", 0, bart, Some((tr, &[])), &[("Out", rstr)], vec![]);
    let p0 = fx.param(0);
    let out = fx.alias(tr, "Out", &[p0]);
    let tb = fx.bound(tr, &[p0]);
    let take = fx.func("take", 1, &[out], unit, vec![tb]);
    let host = fx.func("host", 0, &[], unit, vec![]);
    fx.finish();
    let mut heads = Heads { decls: Decls::new(), take };
    heads.decls.trait_(tr, 0);
    heads.decls.alias(tr, "Out", 0);
    heads.decls.adt(bar, 0);
    heads.decls.imp(imp, crate::region_fixture::ImplHead { lifetimes: 0, self_ty: bart, self_slots: &[], trait_ref: Some((tr, &[], &[])), outlives: &[] });
    heads.decls.impl_assoc(imp, "Out", rstr, &[Region::Static]);
    heads.add(take, 0, &[], &[out], unit, TAKE_SLOTS);
    heads.add(host, 0, &[], &[], unit, &[]);
    let body = |b: &mut Bb| {
        let t = b.ty(PNode::Adt(bar, List::EMPTY));
        let callee = b.path_args("take", &[t]);
        let lit = b.str_();
        let call = b.call(callee, &[lit]);
        let st = b.semi(call);
        b.block(&[st], None)
    };
    let mut b = Bb::new();
    let root = body(&mut b);
    let got = run(&fx, &b, host, root, &heads, Plant::None);
    std::println!("a_constant_at_a_hidden_input_is_counted: {got:?}");
    let (lines, stats) = got.expect("lowered");
    assert_eq!(lines, vec::Vec::<String>::new(), "clean");
    assert_eq!(stats.calls_by_signature, 1, "{stats:?}");
}
