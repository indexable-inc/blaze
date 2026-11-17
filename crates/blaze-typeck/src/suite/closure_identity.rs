//! A closure's identity across bodies (typeck.md, "A closure's identity").
//!
//! The Reference, "Closure types": "A closure expression produces a closure value with a
//! unique, anonymous type that cannot be written out"; "Capture modes": a closure captures a
//! place by the least mode its uses need (shared reference, unique/mutable reference, move),
//! and "Closures that are `move` capture every place by value" ("by move or, for `Copy` types,
//! by copy"); the traits it implements follow from what its body does with the captured
//! values ("`FnOnce`: the closure can be called once"; "`FnMut`: ... mutates"; "`Fn`: ... only
//! shared references"), not from the capture mode. "Generic closures": a closure in a generic
//! item is generic over the item's parameters, so `gen::<u8>`'s and `gen::<u16>`'s are two
//! types.
//!
//! A closure from another body is `PNode::Closure { owner, ordinal, args }`; its kind and
//! signature come from the fixture's `Fx::closures`, as the world answers
//! `Question::Closure(owner, ordinal)` from the owner's `Checked::closures`.

use std::vec::Vec;

use super::check_body;
use crate::body::{Expr, IntSuffix};
use crate::columns::CaptureKind;
use crate::fixture::{Bb, Fx};
use crate::ids::{Abi, FnPtrHeader, ItemKey, LangItem, List, Mutbl, Safety, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::{PNode, PTy};
use crate::ty::ClosureKind;

struct Items {
    /// `fn make() -> {closure#0 of make}`: its body is `|y: u8| y`.
    make: ItemKey,
    /// `fn gen<T>(t: T) -> {closure#0 of gen<T>}`: its body is `move |y: u8| t`.
    gen_: ItemKey,
    /// `fn main(x: u8) -> u8`
    main: ItemKey,
    /// `fn kinds(x: u8, s: S, t: S) -> u8`
    kinds: ItemKey,
    /// `fn own(x: u8) -> u8`: a caller whose own closure is selected at its call.
    own: ItemKey,
    /// `impl<F: Fn() -> u8> Next for Map<F> { type Out = u8; }`
    next_map: ItemKey,
    /// `fn if_let(o: Option<u32>)`: a closure whose `if let` binds what its block uses.
    if_let: ItemKey,
}

/// `with_facts`: whether the world answers `Question::Closure` (the change's key part).
fn world(with_facts: bool) -> (Fx, Items) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let copy = fx.tr("Copy", 1, Vec::new(), &[]);
    fx.lang(LangItem::Copy, copy);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let rust_call = FnPtrHeader { abi: Abi::RustCall, safety: Safety::Safe, c_variadic: false };
    let fn_once = fx.tr("FnOnce", 2, Vec::new(), &["Output"]);
    fx.lang(LangItem::FnOnce, fn_once);
    let out = fx.alias(fn_once, "Output", &[p0, p1]);
    let call_once = fx.trait_fn(fn_once, "call_once", true, &[p0, p1], out);
    fx.set_header(call_once, rust_call);
    let sup = fx.bound(fn_once, &[p0, p1]);
    let fn_mut = fx.tr("FnMut", 2, std::vec![sup], &[]);
    fx.lang(LangItem::FnMut, fn_mut);
    let m_self = fx.r(Mutbl::Mut, p0);
    let call_mut = fx.trait_fn(fn_mut, "call_mut", true, &[m_self, p1], out);
    fx.set_header(call_mut, rust_call);
    let sup = fx.bound(fn_mut, &[p0, p1]);
    let fn_ = fx.tr("Fn", 2, std::vec![sup], &[]);
    fx.lang(LangItem::Fn, fn_);
    let r_self = fx.r(Mutbl::Not, p0);
    let call = fx.trait_fn(fn_, "call", true, &[r_self, p1], out);
    fx.set_header(call, rust_call);

    let u8_ = fx.ty(PNode::Uint(UintTy::U8));
    // impl Copy for u8 (core/src/marker.rs `marker_impls!`); `S` below is not Copy.
    fx.imp("impl Copy for u8", 0, u8_, Some((copy, &[])), &[], Vec::new());
    let io = fx.list(&[u8_, u8_]);
    let sig_u8 = fx.ty(PNode::FnPtr(io, FnPtrHeader::RUST));
    let io = fx.list(&[u8_, p0]);
    let sig_t = fx.ty(PNode::FnPtr(io, FnPtrHeader::RUST));

    // The fixture names keys in order: a probe key tells the next fn's key, so a fn's own
    // output can name a closure of its own body.
    let probe = fx.opaque_key("probe");
    let make_k = ItemKey(probe.0 + 1);
    let c_make = fx.ty(PNode::Closure { owner: make_k, ordinal: 0, args: List::EMPTY });
    let make = fx.func("make", 0, &[], c_make, Vec::new());
    assert_eq!(make, make_k);
    let probe = fx.opaque_key("probe");
    let gen_k = ItemKey(probe.0 + 1);
    let args = fx.list(&[p0]);
    let c_gen = fx.ty(PNode::Closure { owner: gen_k, ordinal: 0, args });
    let gen_ = fx.func("gen", 1, &[p0], c_gen, Vec::new());
    assert_eq!(gen_, gen_k);
    if with_facts {
        fx.closures.push((make, 0, ClosureKind::Fn, sig_u8));
        fx.closures.push((gen_, 0, ClosureKind::FnOnce, sig_t));
    }

    // fn apply<F: Fn(u8) -> u8>(f: F, x: u8) -> u8
    let a = fx.tuple(&[u8_]);
    let b = fx.bound(fn_, &[p0, a]);
    let ret = fx.proj(fn_once, &[p0, a], "Output", u8_);
    fx.func("apply", 1, &[p0, u8_], u8_, std::vec![b, ret]);
    // struct Map<F> { f: F }   fn wrap<F>(f: F) -> Map<F>
    // impl<F: Fn(u8) -> u8> Map<F> { fn next(&self, x: u8) -> u8 { (self.f)(x) } }
    let map_k = fx.strukt("Map", 1, &[("f", p0)]);
    let map_f = fx.adt(map_k, &[p0]);
    fx.func("wrap", 1, &[p0], map_f, Vec::new());
    let b = fx.bound(fn_, &[p0, a]);
    let ret = fx.proj(fn_once, &[p0, a], "Output", u8_);
    let imp = fx.imp("impl Map", 1, map_f, None, &[], std::vec![b, ret]);
    let r_map = fx.r(Mutbl::Not, map_f);
    fx.impl_fn_own(imp, "next", true, 0, &[r_map, u8_], u8_, Vec::new());

    // struct S;  fn touch(r: &mut u8) -> u8;  fn consume(s: S) -> u8;  fn peek(s: &S) -> u8;
    let s_k = fx.strukt("S", 0, &[]);
    let s = fx.adt(s_k, &[]);
    let mut_u8 = fx.r(Mutbl::Mut, u8_);
    fx.func("touch", 0, &[mut_u8], u8_, Vec::new());
    fx.func("consume", 0, &[s], u8_, Vec::new());
    let r_s = fx.r(Mutbl::Not, s);
    fx.func("peek", 0, &[r_s], u8_, Vec::new());
    let main = fx.func("main", 0, &[u8_], u8_, Vec::new());
    let kinds = fx.func("kinds", 0, &[u8_, s, s], u8_, Vec::new());

    // trait Next { type Out; }   impl<F: Fn() -> u8> Next for Map<F> { type Out = u8; }
    // fn run<T: Next>(t: T) -> <T as Next>::Out   fn own(x: u8) -> u8
    let next = fx.tr("Next", 1, Vec::new(), &["Out"]);
    let unit = fx.tuple(&[]);
    let b = fx.bound(fn_, &[p0, unit]);
    let ret = fx.proj(fn_once, &[p0, unit], "Output", u8_);
    let next_map = fx.imp("impl Next for Map", 1, map_f, Some((next, &[])), &[("Out", u8_)], std::vec![b, ret]);
    let out_t = fx.alias(next, "Out", &[p0]);
    let b = fx.bound(next, &[p0]);
    fx.func("run", 1, &[p0], out_t, std::vec![b]);
    let own = fx.func("own", 0, &[u8_], u8_, Vec::new());

    // enum Option<T> { None, Some(T) }   fn if_let(o: Option<u32>)
    let (option, _) = fx.enum_("Option", 1, &[("None", &[]), ("Some", &[p0])]);
    fx.lang_def(LangItem::OptionNone, crate::world::Def::Ctor { adt: option, variant: 0, kind: crate::world::CtorKind::Unit });
    fx.lang_def(LangItem::OptionSome, crate::world::Def::Ctor { adt: option, variant: 1, kind: crate::world::CtorKind::Tuple });
    let u32_ = fx.u32();
    let opt_u32 = fx.adt(option, &[u32_]);
    let unit = fx.unit();
    let if_let = fx.func("if_let", 0, &[opt_u32], unit, Vec::new());
    fx.finish();
    (fx, Items { make, gen_, main, kinds, own, next_map, if_let })
}

fn closure_of(c: &crate::Checked<'_>, t: PTy) -> Option<(ItemKey, u32, Vec<PTy>)> {
    match c.types.view().node(t).ok()? {
        PNode::Closure { owner, ordinal, args } => Some((owner, ordinal, c.types.view().list(args).ok()?.to_vec())),
        _ => None,
    }
}

/// ```text
/// fn make() -> impl Fn(u8) -> u8 { |y| y }          // closure #0 of `make`
/// fn main(x: u8) -> u8 { apply(make(), x) }         // through `F: Fn(u8) -> u8`
/// fn main(x: u8) -> u8 { make()(x) }                // a direct call
/// ```
/// Two bodies: the caller meets `make`'s closure only by its identity and asks its facts.
#[test]
fn a_closure_returned_from_another_body_is_called_through_its_bound() {
    let (fx, it) = world(true);
    for direct in [false, true] {
        let mut b = Bb::new();
        let x = b.param("x");
        let mk = b.path(&["make"]);
        let made = b.call(mk, &[]);
        let lx = b.local(x);
        let tail = if direct {
            b.call(made, &[lx])
        } else {
            let ap = b.path(&["apply"]);
            b.call(ap, &[made, lx])
        };
        let root = b.block(&[], Some(tail));
        let c = check_body(&fx, &b, it.main, root, Plant::None, InferPlant::None);
        let d: Vec<_> = c.diags.iter().map(|d| (d.code, d.at)).collect();
        assert_eq!(d, Vec::new(), "direct {direct}");
        assert_eq!(closure_of(&c, c.expr_ty[made.ix()]), Some((it.make, 0, Vec::new())), "direct {direct}");
    }
}

/// The same caller with the world NOT answering `Question::Closure`: the closure's signature
/// and kind are not part of its type, so nothing can call it; the check stops naming it.
#[test]
#[should_panic(expected = "BodyTypeInRecord(Closure(")]
fn without_the_owners_facts_a_foreign_closure_cannot_be_called() {
    let (fx, it) = world(false);
    let mut b = Bb::new();
    let x = b.param("x");
    let mk = b.path(&["make"]);
    let made = b.call(mk, &[]);
    let lx = b.local(x);
    let tail = b.call(made, &[lx]);
    let root = b.block(&[], Some(tail));
    check_body(&fx, &b, it.main, root, Plant::None, InferPlant::None);
}

/// ```text
/// struct Map<F> { f: F }  fn wrap<F>(f: F) -> Map<F>
/// impl<F: Fn(u8) -> u8> Map<F> { fn next(&self, x: u8) -> u8 { (self.f)(x) } }
/// fn main(x: u8) -> u8 { let m = wrap(make()); m.next(x) }
/// ```
/// The iterator-adaptor shape (`iter.map(|x| ..)`): `make`'s closure stored in a generic
/// struct, the impl's `F: Fn(u8) -> u8` proved at `F` = that closure by its facts. Three
/// bodies: `make` (the owner), `main` (this check), `next` (generic over `F`; its call
/// `(self.f)(x)` runs at the instance whose `F` is `{closure#0 of make}`).
#[test]
fn a_closure_stored_in_an_adaptor_meets_the_impls_bound() {
    let (fx, it) = world(true);
    let mut b = Bb::new();
    let x = b.param("x");
    let m = b.bind("m");
    let wrap = b.path(&["wrap"]);
    let mk = b.path(&["make"]);
    let made = b.call(mk, &[]);
    let wrapped = b.call(wrap, &[made]);
    let s0 = b.let_(m, None, wrapped);
    let lm = b.local(m);
    let lx = b.local(x);
    let next = b.method(lm, "next", &[lx]);
    let root = b.block(&[s0], Some(next));
    let c = check_body(&fx, &b, it.main, root, Plant::None, InferPlant::None);
    let d: Vec<_> = c.diags.iter().map(|d| (d.code, d.at)).collect();
    assert_eq!(d, Vec::new());
    let PNode::Adt(_, l) = c.types.view().node(c.expr_ty[wrapped.ix()]).unwrap() else { panic!("Map<..>") };
    let f = c.types.view().list(l).unwrap()[0];
    assert_eq!(closure_of(&c, f), Some((it.make, 0, Vec::new())));
}

/// ```text
/// fn own(x: u8) -> u8 { run(wrap(|| x)) }   // run<T: Next>(t: T) -> <T as Next>::Out
/// ```
/// core's `char_count_general_case` shape (`s.iter().filter(|&&b| ..).count()`): a callee's
/// signature names a projection whose arguments hold a closure of the body being checked. Its
/// writeback selects the impl at that call (`Checked::selected`) with the closure's facts from
/// this check's own closure rows: the world holds no record of `own` yet, and asking it was the
/// stop `Malformed(BodyTypeInRecord(Closure(own, 0)))`. The impl's `F: Fn() -> u8` holds only
/// with the closure's real kind and signature, so a source that dropped them selects nothing.
/// Should print: "test suite::closure_identity::a_call_selects_its_impl_over_the_bodys_own_closure ... ok".
#[test]
fn a_call_selects_its_impl_over_the_bodys_own_closure() {
    let (fx, it) = world(true);
    let mut b = Bb::new();
    let x = b.param("x");
    let lx = b.local(x);
    let cl = b.closure(&[], lx);
    let wrap = b.path(&["wrap"]);
    let wrapped = b.call(wrap, &[cl]);
    let run = b.path(&["run"]);
    let ran = b.call(run, &[wrapped]);
    let root = b.block(&[], Some(ran));
    let c = check_body(&fx, &b, it.own, root, Plant::None, InferPlant::None);
    let d: Vec<_> = c.diags.iter().map(|d| (d.code, d.at)).collect();
    assert_eq!(d, Vec::new());
    assert_eq!(closure_of(&c, c.expr_ty[cl.ix()]), Some((it.own, 0, Vec::new())));
    let rows = c.selected_at(ran);
    assert_eq!(rows.len(), 1, "one projection in `run`'s signature: {rows:?}");
    assert!(
        matches!(rows[0].selected, crate::selected::Selected::Impl { imp, .. } if imp == it.next_map),
        "`<Map<{{closure#0 of own}}> as Next>::Out` is `impl Next for Map`'s: {rows:?}"
    );
}

/// ```text
/// fn gen<T>(t: T) -> impl FnOnce(u8) -> T { move |y| t }
/// fn main(x: u8) -> u8 { let a = gen(x); let b = gen(1u16); a(x) }
/// ```
/// One closure expression at two instances: two types, `{closure#0 of gen<u8>}` and
/// `{closure#0 of gen<u16>}`, and the call's output is `gen::<u8>`'s `T`.
#[test]
fn a_closure_of_a_generic_fn_at_two_instances_is_two_types() {
    let (fx, it) = world(true);
    let mut b = Bb::new();
    let x = b.param("x");
    let (pa, pb) = (b.bind("a"), b.bind("b"));
    let g = b.path(&["gen"]);
    let lx = b.local(x);
    let ga = b.call(g, &[lx]);
    let s0 = b.let_(pa, None, ga);
    let g = b.path(&["gen"]);
    let one = b.int(Some(IntSuffix::Uint(UintTy::U16)));
    let gb = b.call(g, &[one]);
    let s1 = b.let_(pb, None, gb);
    let la = b.local(pa);
    let lx = b.local(x);
    let tail = b.call(la, &[lx]);
    let root = b.block(&[s0, s1], Some(tail));
    let c = check_body(&fx, &b, it.main, root, Plant::None, InferPlant::None);
    let d: Vec<_> = c.diags.iter().map(|d| (d.code, d.at)).collect();
    assert_eq!(d, Vec::new());
    let (ta, tb) = (c.expr_ty[ga.ix()], c.expr_ty[gb.ix()]);
    assert_ne!(ta, tb, "two instances, two types");
    let u8_ = c.types.view().nodes().position(|n| *n == PNode::Uint(UintTy::U8)).map(|i| PTy(i as u32));
    let u16_ = c.types.view().nodes().position(|n| *n == PNode::Uint(UintTy::U16)).map(|i| PTy(i as u32));
    assert_eq!(closure_of(&c, ta), Some((it.gen_, 0, u8_.into_iter().collect())));
    assert_eq!(closure_of(&c, tb), Some((it.gen_, 0, u16_.into_iter().collect())));
}

/// ```text
/// fn kinds(x: u8, s: S, t: S) -> u8 {
///     let r = || x;                 // #0: reads a Copy value: Fn, x by ref
///     let m = || touch(&mut x);     // #1: a mutable use: FnMut, x by mutable ref
///     let o = || consume(s);        // #2: moves a non-Copy value out: FnOnce, s by value
///     let v = move || x;            // #3: `move`: x by value (a copy), still Fn
///     let w = move || peek(&t);     // #4: `move`: t by value, only borrowed inside: Fn
///     x
/// }
/// ```
/// The owner's own record: `Checked::closures` by ordinal with each kind, and the captures
/// in order with their modes. Each closure's type names the owner and its ordinal.
#[test]
fn the_kinds_and_captures_are_recorded_by_ordinal() {
    let (fx, it) = world(true);
    let mut b = Bb::new();
    let (x, s, t) = (b.param("x"), b.param("s"), b.param("t"));
    let mut stmts = Vec::new();
    let mut cls = Vec::new();
    for case in 0..5 {
        let body = match case {
            0 | 3 => b.local(x),
            1 => {
                let f = b.path(&["touch"]);
                let lx = b.local(x);
                let r = b.borrow(Mutbl::Mut, lx);
                b.call(f, &[r])
            }
            2 => {
                let f = b.path(&["consume"]);
                let ls = b.local(s);
                b.call(f, &[ls])
            }
            _ => {
                let f = b.path(&["peek"]);
                let lt = b.local(t);
                let r = b.borrow(Mutbl::Not, lt);
                b.call(f, &[r])
            }
        };
        let cl = if case >= 3 { b.move_closure(&[], body) } else { b.closure(&[], body) };
        cls.push(cl);
        let p = b.bind("c");
        stmts.push(b.let_(p, None, cl));
    }
    let lx = b.local(x);
    let root = b.block(&stmts, Some(lx));
    let c = check_body(&fx, &b, it.kinds, root, Plant::None, InferPlant::None);
    let d: Vec<_> = c.diags.iter().map(|d| (d.code, d.at)).collect();
    assert_eq!(d, Vec::new());
    let rows: Vec<_> = (0..c.closures.len()).map(|i| (c.closures.column::<0>()[i], c.closures.column::<1>()[i])).collect();
    let want_kinds = [ClosureKind::Fn, ClosureKind::FnMut, ClosureKind::FnOnce, ClosureKind::Fn, ClosureKind::Fn];
    let want: Vec<_> = cls.iter().copied().zip(want_kinds).collect();
    assert_eq!(rows, want, "closures by ordinal: (expression, kind)");
    for (i, &cl) in cls.iter().enumerate() {
        assert_eq!(closure_of(&c, c.expr_ty[cl.ix()]), Some((it.kinds, i as u32, Vec::new())), "closure #{i}'s type");
    }
    let caps: Vec<_> = (0..c.captures.len())
        .map(|i| (c.captures.column::<0>()[i], c.captures.column::<1>()[i], c.captures.column::<2>()[i]))
        .collect();
    let want = std::vec![
        (cls[0], x, CaptureKind::ByRef),
        (cls[1], x, CaptureKind::ByMutRef),
        (cls[2], s, CaptureKind::ByValue),
        (cls[3], x, CaptureKind::ByValue),
        (cls[4], t, CaptureKind::ByValue),
    ];
    assert_eq!(caps, want, "captures: (closure, binding, mode)");
}

/// ```text
/// fn if_let(o: Option<u32>) { let c = move || { if let Some(s) = o { s } else { 0 } }; }
/// ```
/// std's `panic_handler` shape: the `if let`'s binding `s` is bound inside the closure, so the
/// only capture is `o` (by value: `move`). A walk that visits `then` before `cond`'s `let`
/// also records `(c, s, ..)`, and the builder then stops on "a binding captured before its
/// binding". Should print:
/// "test suite::closure_identity::an_if_let_binding_in_a_closure_is_not_a_capture ... ok".
#[test]
fn an_if_let_binding_in_a_closure_is_not_a_capture() {
    let (fx, it) = world(true);
    let mut b = Bb::new();
    let o = b.param("o");
    let s = b.bind("s");
    let some = b.lang_variant_pat(LangItem::OptionSome, &[s]);
    let lo = b.local(o);
    let cond = b.e(Expr::Let { pat: some, init: lo });
    let ls = b.local(s);
    let then = b.block(&[], Some(ls));
    let zero = b.int(None);
    let else_ = b.block(&[], Some(zero));
    let iff = b.e(Expr::If { cond, then, else_: Some(else_) });
    let body = b.block(&[], Some(iff));
    let cl = b.move_closure(&[], body);
    let p = b.bind("c");
    let s0 = b.let_(p, None, cl);
    let root = b.block(&[s0], None);
    let c = check_body(&fx, &b, it.if_let, root, Plant::None, InferPlant::None);
    let d: Vec<_> = c.diags.iter().map(|d| (d.code, d.at)).collect();
    assert_eq!(d, Vec::new());
    let caps: Vec<_> = (0..c.captures.len())
        .map(|i| (c.captures.column::<0>()[i], c.captures.column::<1>()[i], c.captures.column::<2>()[i]))
        .collect();
    assert_eq!(caps, std::vec![(cl, o, CaptureKind::ByValue)], "captures: (closure, binding, mode)");
}
