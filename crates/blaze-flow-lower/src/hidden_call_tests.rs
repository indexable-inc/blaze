//! Piece sig-assoc-per-call (SOUNDNESS): a call of a head with a hidden input, `fn take<T:
//! Tr>(x: T::Out)`, is related per call (hidden_call.rs). Each test reads the heads from text as
//! the census does, lowers a body checked by the real type checker, and checks it with
//! blaze-flow.
//!
//! Sources: the Reference, "Implementations", "Generic implementations" (an impl's lifetime
//! appears in its self type or trait: E0207, so with `T = Foo`, no lifetime, every lifetime of
//! `<Foo as Tr>::Out` is `'static`); "Associated Items", "Associated types" (`impl Tr for Foo {
//! type Out = &'static S; }` defines `<Foo as Tr>::Out` as `&'static S`; the fixture's erased
//! `&S`, its text's `'static` being what E0207 leaves); "Lifetime bounds" (`'static` "outlives all
//! other lifetimes"); "Temporary scopes" / RFC 2094: a local's borrow ends when the local dies,
//! so a loan that must outlive `'static` is live past it (E0597, `borrow Invalidated`).

use std::format;
use std::string::String;
use std::vec;

use blaze_flow::record::check;
use blaze_own::Alloc;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ExprId, InferCtx, ItemKey, List, Mutbl, Name, Needs, Outcome, PNode, PTy};

use crate::{AdtFacts, CallBounds, CalleeFacts, Facts, Form, Plant, Refusal, SigBuf, SigLife, Stats, lower_planted, read_signature};

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// `take`'s one type parameter has a trait bound (`T: Tr`) and no type-outlives bound.
const TRAIT_BOUNDED: &[u32] = &[0];

/// The heads' lifetime parts, each read from its text; `take`'s bounds as its head says.
struct Heads {
    sigs: vec::Vec<(ItemKey, SigBuf<Alloc<'static>>)>,
    take: ItemKey,
}

impl Heads {
    fn add(&mut self, fx: &Fx, item: ItemKey, text: &str, inputs: &[PTy], output: PTy) {
        let adt = |_: ItemKey| Some(0);
        let mut buf = SigBuf::new_in(alloc());
        if let Err(why) = read_signature(text, fx.t.view(), inputs, output, &adt, &mut buf, alloc()) {
            panic!("`{text}` was not read: {why:?}");
        }
        self.sigs.push((item, buf));
    }
}

impl Facts for Heads {
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
    fn sig(&self, item: ItemKey) -> Option<SigLife<'_>> {
        self.sigs.iter().find(|(k, _)| *k == item).map(|(_, s)| s.life())
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

/// The findings of one body, one line each, and the lowering's counts; or the refusal.
fn run(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId, facts: &Heads, plant: Plant) -> Result<(vec::Vec<String>, Stats), Refusal> {
    let a = alloc();
    let view = b.view(owner, root);
    let mut cx = InferCtx::new_in(a);
    let c = match blaze_typeck::check(&view, fx, &mut cx, &Needs::new_in(a), a) {
        Ok(Outcome::Done(c)) => {
            assert!(c.diags.is_empty(), "the fixture type-checks clean: {:?}", c.diags);
            c
        }
        Ok(Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("{e:?}"),
    };
    let lowered = lower_planted(&view, &c, facts, plant, a)?;
    let rec = check(&lowered.flow, a).expect("bounded");
    let mut out = vec::Vec::new();
    for m in rec.moves.iter() {
        out.push(format!("move {:?} at {:?}", m.kind, m.at));
    }
    for f in rec.borrows.iter() {
        out.push(format!("borrow {f:?}"));
    }
    Ok((out, lowered.stats))
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
    let _imp = fx.imp("impl Tr for Foo", 0, foot, Some((tr, &[])), &[("Out", rs)], vec![]);
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
    let mut heads = Heads { sigs: vec::Vec::new(), take };
    heads.add(&fx, take, "fn take<T: Tr>(x: T::Out) {}", &[out], unit);
    heads.add(&fx, bad, "fn bad(s: S) { take::<Foo>(&s); }", &[st], unit);
    heads.add(&fx, good, "fn good(s: &'static S) { take::<Foo>(s); }", &[rs], unit);
    heads.add(&fx, pass, "fn pass<U: Tr>(x: U::Out) { take::<U>(x); }", &[out], unit);
    heads.add(&fx, bound, "fn bound<U: Tr<Out = &S>>(s: &S) { take::<U>(s); }", &[rs], unit);
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
    let take_read = w.heads.sig(w.take).map(|l| (l.regions.to_vec(), l.ends.to_vec()));
    std::println!("a_static_assoc_type_is_demanded_of_the_argument bad: {lines:?}; take read {take_read:?}; {stats:?}");
    // The step first (which path the call took), then its effect.
    assert_eq!((stats.calls_hidden_static, stats.calls_by_signature), (1, 1), "take read {take_read:?}: {stats:?}; {lines:?}");
    assert!(lines.iter().any(|l| l.starts_with("borrow Invalidated")), "E0597: {lines:?}");
    let mut b = Bb::new();
    let root = call_take(&mut b, PNode::Adt(w.foo, List::EMPTY), false);
    let (lines, stats) = run(&w.fx, &b, w.good, root, &w.heads, Plant::None).expect("lowered");
    std::println!("a_static_assoc_type_is_demanded_of_the_argument good: {lines:?}");
    assert_eq!(lines, vec::Vec::<String>::new(), "a `'static` borrow is passed: clean");
    assert_eq!(stats.calls_hidden_static, 1, "{stats:?}");
    let mut b = Bb::new();
    let root = call_take(&mut b, PNode::Adt(w.foo, List::EMPTY), true);
    let (lines, _) = run(&w.fx, &b, w.bad, root, &w.heads, Plant::HiddenCoarse).expect("lowered");
    std::println!("a_static_assoc_type_is_demanded_of_the_argument coarse (control) bad: {lines:?}");
    assert_eq!(lines, vec::Vec::<String>::new(), "the control: the coarse path misses E0597");
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
    assert!(matches!(got, Err(Refusal { form: Form::CallHiddenInput { input: 0, .. }, .. })), "{got:?}");
}
