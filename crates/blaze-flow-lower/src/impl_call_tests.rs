//! Piece impl-per-call: a call of `fn take<T: Tr>(x: T::Out)` whose type argument carries a
//! lifetime (`T = Wrap<'_>`) is read through the impl the TYPE CHECKER selected at the call
//! (`Checked::selected`), never refused when that impl's text decides it (impl_call.rs).
//!
//! Sources: the Reference, "Associated Items", "Associated types" (the selected impl's `type Out
//! = U;` defines `<T as Tr>::Out` at the call); "Implementations", "Generic implementations"
//! (E0207: the impl's `'a` appears in its self type `Wrap<'a>`, so it is the call's `T`'s);
//! "Lifetime bounds" (`'static` "outlives all other lifetimes"); RFC 2094: a local's borrow ends
//! when the local dies, so a loan that must outlive `'static` is live past it (E0597, `borrow
//! Invalidated`).

use std::format;
use std::string::String;
use std::vec;

use blaze_flow::record::check;
use blaze_own::Alloc;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ExprId, InferCtx, ItemKey, List, Mutbl, Name, Needs, Outcome, PNode, PTy, Selected};

use crate::{AdtFacts, CallBounds, CalleeFacts, Facts, Form, ImplSource, Plant, Refusal, SigBuf, SigLife, lower_planted, read_signature};

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// `take`'s one type parameter has a trait bound (`T: Tr`) and no type-outlives bound.
const TRAIT_BOUNDED: &[u32] = &[0];

const WRAP_IMPL: &str = "impl<'a> Tr for Wrap<'a> { type Out = &'a S; }";
const WRAP_ASSOC: &[(&str, &str)] = &[("Out", "type Out = &'a S;")];
const KEEP_IMPL: &str = "impl<'a> Tr for Keep<'a> { type Out = &'static S; }";
const KEEP_ASSOC: &[(&str, &str)] = &[("Out", "type Out = &'static S;")];

/// The heads' lifetime parts, each read from its text; the ADTs that hold a lifetime; the
/// impls' texts.
struct Heads {
    sigs: vec::Vec<(ItemKey, SigBuf<Alloc<'static>>)>,
    with_lifetime: vec::Vec<ItemKey>,
    impls: vec::Vec<(ItemKey, &'static str, &'static [(&'static str, &'static str)])>,
    take: ItemKey,
}

impl Heads {
    fn add(&mut self, fx: &Fx, item: ItemKey, text: &str, inputs: &[PTy], output: PTy) {
        let lifes = self.with_lifetime.clone();
        let adt = move |k: ItemKey| Some(u32::from(lifes.contains(&k)));
        let mut buf = SigBuf::new_in(alloc());
        if let Err(why) = read_signature(text, fx.t.view(), inputs, output, &adt, &mut buf, alloc()) {
            panic!("`{text}` was not read: {why:?}");
        }
        self.sigs.push((item, buf));
    }
}

impl Facts for Heads {
    fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
        let lifetimes = u32::from(self.with_lifetime.contains(&k));
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes, union: false, is_enum: false, is_box: false })
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
    fn impl_source(&self, imp: ItemKey) -> Option<ImplSource<'_>> {
        self.impls.iter().find(|r| r.0 == imp).map(|&(_, text, assoc)| ImplSource { text, assoc })
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
    let impls = vec![(wrap_impl, WRAP_IMPL, WRAP_ASSOC), (keep_impl, KEEP_IMPL, KEEP_ASSOC)];
    let mut heads = Heads { sigs: vec::Vec::new(), with_lifetime: vec![w, k], impls, take };
    heads.add(&fx, take, "fn take<T: Tr>(x: T::Out) {}", &[out], unit);
    heads.add(&fx, bad, "fn bad(s: S) { take::<Wrap<'_>>(&s); }", &[st], unit);
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
/// control); lowered and checked: the findings, one line each, or the refusal. Also the row
/// the checker kept for the call.
fn run(w: &World, b: &Bb, root: ExprId, drop_rows: bool) -> (Result<vec::Vec<String>, Refusal>, vec::Vec<Selected>) {
    let a = alloc();
    let view = b.view(w.bad, root);
    let mut cx = InferCtx::new_in(a);
    let mut c = match blaze_typeck::check(&view, &w.fx, &mut cx, &Needs::new_in(a), a) {
        Ok(Outcome::Done(c)) => {
            assert!(c.diags.is_empty(), "the fixture type-checks clean: {:?}", c.diags);
            c
        }
        Ok(Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("{e:?}"),
    };
    let rows: vec::Vec<Selected> = c.selected.iter().map(|r| r.selected).collect();
    if drop_rows {
        c.selected.clear();
    }
    let got = lower_planted(&view, &c, &w.heads, Plant::None, a).map(|lowered| {
        let rec = check(&lowered.flow, a).expect("bounded");
        let mut out = vec::Vec::new();
        for m in rec.moves.iter() {
            out.push(format!("move {:?} at {:?}", m.kind, m.at));
        }
        for f in rec.borrows.iter() {
            out.push(format!("borrow {f:?}"));
        }
        out
    });
    (got, rows)
}

/// `fn bad(s: S) { take::<Wrap<'_>>(&s); }` with `impl<'a> Tr for Wrap<'a> { type Out = &'a S;
/// }`: the parameter is `&'x S` for the call's own `'x` (the type argument's), which nothing
/// else constrains: CLEAN, and related, not refused. The checker kept ONE row for the call,
/// the impl for `Wrap`. CONTROL (rule 23): the same record with its `selected` rows emptied is
/// REFUSED by name, `CallHiddenInput { input: 0 }` (the state before this piece).
/// Prints `an_impl_lifetime_relates_exactly selected: [Impl { .. }]`, `.. got: Ok([])`,
/// `.. without rows (control): Err(Refusal { form: CallHiddenInput { .., input: 0 }, .. })`.
#[test]
fn an_impl_lifetime_relates_exactly() {
    let w = world();
    let mut b = Bb::new();
    let root = call_take(&mut b, w.wrap);
    let (got, rows) = run(&w, &b, root, false);
    std::println!("an_impl_lifetime_relates_exactly selected: {rows:?}");
    assert!(matches!(rows[..], [Selected::Impl { imp, .. }] if imp == w.wrap_impl), "the checker's one row: {rows:?}");
    std::println!("an_impl_lifetime_relates_exactly got: {got:?}");
    assert_eq!(got, Ok(vec::Vec::new()), "`type Out = &'a S` demands nothing of `&s`: clean");
    let mut b = Bb::new();
    let root = call_take(&mut b, w.wrap);
    let (got, _) = run(&w, &b, root, true);
    std::println!("an_impl_lifetime_relates_exactly without rows (control): {got:?}");
    assert!(matches!(got, Err(Refusal { form: Form::CallHiddenInput { input: 0, .. }, .. })), "{got:?}");
}

/// `fn bad(s: S) { take::<Keep<'_>>(&s); }` with `impl<'a> Tr for Keep<'a> { type Out =
/// &'static S; }`: the parameter is `&'static S`, and `s` dies at the end of `bad`: E0597
/// (`borrow Invalidated`), found through the selected impl's text.
/// Prints `a_static_impl_type_still_errors: Ok(["borrow Invalidated ..`.
#[test]
fn a_static_impl_type_still_errors() {
    let w = world();
    let mut b = Bb::new();
    let root = call_take(&mut b, w.keep);
    let (got, rows) = run(&w, &b, root, false);
    std::println!("a_static_impl_type_still_errors: {got:?}");
    assert!(matches!(rows[..], [Selected::Impl { .. }]), "the checker's one row: {rows:?}");
    let lines = got.expect("related through the impl, not refused");
    assert!(lines.iter().any(|l| l.starts_with("borrow Invalidated")), "E0597: {lines:?}");
}
