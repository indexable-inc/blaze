//! Calls through values (lane B8; NEVER COMPILED when written, 2026-11-18): a fn pointer whose
//! result is a borrow, a closure called while a loan of it is held (`FnMut`), an `FnOnce`
//! called twice, and a closure of this body whose result holds its capture (callee_value.rs).
//!
//! Each body is a small fn of the fixture world, type-checked by the real checker, lowered and
//! checked by blaze-flow (`findings_with`, which takes the process heap: the shared helper of
//! tests.rs, listed in the lane's report). The world's lists of clauses are the fixture's own
//! `std` vectors (`Fx::func` takes one).

use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ExprId, FnPtrHeader, ItemKey, LangItem, Mutbl, Name, PClause, PNode, PTy, PatId};

use crate::tests::{Sigs, Wide, findings_with};
use crate::{AdtFacts, CalleeFacts, Facts, Form, Plant, Refusal, Requirement, SigLife};

/// The fixtures' facts (tests.rs [`Wide`]: closures lowered, signatures read from text) with the
/// owner's text ([`Facts::own_text`]) and one closure's requirements
/// ([`Facts::closure_requirements`]).
pub(crate) struct Lane {
    pub(crate) wide: Wide,
    /// The owner's text; empty: not known.
    pub(crate) own: &'static str,
    /// The closure expression `reqs` are for.
    pub(crate) closure: Option<ExprId>,
    pub(crate) reqs: &'static [Requirement],
}

impl Lane {
    pub(crate) fn new() -> Lane {
        Lane { wide: Wide { sigs: Sigs::new(), life: None }, own: "", closure: None, reqs: &[] }
    }
}

impl Facts for Lane {
    fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
        self.wide.adt(k)
    }
    fn callee(&self, k: ItemKey) -> Option<CalleeFacts> {
        self.wide.callee(k)
    }
    fn sig(&self, item: ItemKey) -> Option<SigLife<'_>> {
        self.wide.sig(item)
    }
    fn own_text(&self) -> Option<&str> {
        if self.own.is_empty() { None } else { Some(self.own) }
    }
    fn closure_bodies_checked(&self) -> bool {
        true
    }
    fn closure_requirements(&self, closure: ExprId) -> &[Requirement] {
        if self.closure == Some(closure) { self.reqs } else { &[] }
    }
    fn field(&self, adt: ItemKey, variant: u32, name: Name) -> Option<u32> {
        self.wide.field(adt, variant, name)
    }
    fn field_count(&self, adt: ItemKey, variant: u32) -> Option<u32> {
        self.wide.field_count(adt, variant)
    }
    fn param_copy(&self, index: u32) -> bool {
        self.wide.param_copy(index)
    }
}

/// No clauses: the fixture's `func` takes its list as a `std` vector.
pub(crate) fn no_clauses() -> std::vec::Vec<PClause> {
    std::vec::Vec::new()
}

/// `struct S;` with `Copy` known (so `&S` is `Copy` and a `move` of `S` is a move), and
/// `fn g(_: &S)`, `fn m(_: &mut S)`, `fn h(_: S)`.
pub(crate) struct World {
    pub(crate) fx: Fx,
    pub(crate) st: PTy,
    pub(crate) rs: PTy,
    pub(crate) unit: PTy,
}

/// The world, with `owners(fx, st, rs, unit)` declaring the owners before it is finished.
pub(crate) fn world_with(owners: impl FnOnce(&mut Fx, PTy, PTy, PTy)) -> World {
    let mut fx = Fx::new();
    let copy = fx.tr("Copy", 1, no_clauses(), &[]);
    fx.lang(LangItem::Copy, copy);
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let rms = fx.r(Mutbl::Mut, st);
    let unit = fx.unit();
    // core's `impl<T: ?Sized> Copy for &T {}` (core/src/marker.rs): the solver holds no builtin
    // `Copy` for a shared reference (blaze-solve builtin.rs `step_copy_clone`, as rustc's
    // `instantiate_constituent_tys_for_copy_clone_trait`: "impls in core"), so without it a
    // closure's by-value use of `a: &S` was a move and the capture by value, not by
    // reference (B12, the fix of closure_requirements_tests' `Malformed`: the requirement's
    // `Capture(3)` past a two-position closure value). `T: Sized` is implied by the fixture;
    // every `T` here is sized.
    let p0 = fx.param(0);
    let ref_t = fx.r(Mutbl::Not, p0);
    let _ = fx.imp("impl<T: ?Sized> Copy for &T {}", 1, ref_t, Some((copy, &[])), &[], no_clauses());
    let _ = fx.func("g", 0, &[rs], unit, no_clauses());
    let _ = fx.func("m", 0, &[rms], unit, no_clauses());
    let _ = fx.func("h", 0, &[st], unit, no_clauses());
    owners(&mut fx, st, rs, unit);
    fx.finish();
    World { fx, st, rs, unit }
}

/// `name(local)`.
pub(crate) fn call1(b: &mut Bb, name: &str, arg: PatId) -> ExprId {
    let f = b.path(&[name]);
    let a = b.local(arg);
    b.call(f, &[a])
}

/// `name(&local)` or `name(&mut local)`.
pub(crate) fn call_ref(b: &mut Bb, name: &str, m: Mutbl, arg: PatId) -> ExprId {
    let f = b.path(&[name]);
    let a = b.local(arg);
    let r = b.borrow(m, a);
    b.call(f, &[r])
}

/// A fn pointer returning a borrow (the Reference, "Lifetime elision": in a fn pointer type the
/// output takes the one input's lifetime, so `p(&s)` is a loan of `s` carried in the result):
/// `fn f(p: fn(&S) -> &S, s: S) { let r = p(&s); g(r); m(&mut s); }` is right (the result is
/// dead when `s` is borrowed mutably). Its negative, `.. let r = p(&s); m(&mut s); g(r); }`,
/// mutates `s` while the result holds its loan: rustc E0502, here `Invalidated`. Without the
/// owner's text the pointer's lifetimes are not known: refused by name (`CalleeValueUnknown`),
/// never accepted.
/// Prints: `fn pointer result late: ["borrow Invalidated ..` and `fn pointer result early: []`.
#[test]
fn a_fn_pointer_result_is_a_loan_used_after_mutation() {
    let mut owner = None;
    let w = world_with(|fx, st, rs, unit| {
        let l = fx.list(&[rs, rs]);
        let fp = fx.ty(PNode::FnPtr(l, FnPtrHeader::RUST));
        owner = Some((fx.func("f", 0, &[fp, st], unit, no_clauses()), fp));
    });
    let (f, fp) = owner.expect("declared");
    let text = "fn f(p: fn(&S) -> &S, s: S) { let r = p(&s); m(&mut s); g(r); }";
    for late in [false, true] {
        let mut b = Bb::new();
        let (pp, ps) = (b.param("p"), b.param("s"));
        let lp = b.local(pp);
        let ls = b.local(ps);
        let bor = b.borrow(Mutbl::Not, ls);
        let call = b.call(lp, &[bor]);
        let pr = b.bind("r");
        let st1 = b.let_(pr, None, call);
        let use_r = call1(&mut b, "g", pr);
        let st_use = b.semi(use_r);
        let mutate = call_ref(&mut b, "m", Mutbl::Mut, ps);
        let st_mut = b.semi(mutate);
        let root = if late { b.block(&[st1, st_mut, st_use], None) } else { b.block(&[st1, st_use, st_mut], None) };
        let mut facts = Lane::new();
        facts.wide.add(&w.fx, f, text, &[fp, w.st], w.unit);
        facts.own = text;
        let got = findings_with(&w.fx, &b, f, root, &facts, Plant::None);
        std::println!("fn pointer result {}: {got:?}", if late { "late" } else { "early" });
        let got = got.expect("lowered");
        if late {
            assert!(got.iter().any(|l| l.starts_with("borrow Invalidated")), "{got:?}");
        } else {
            assert!(got.is_empty(), "{got:?}");
        }
        facts.own = "";
        let unread = findings_with(&w.fx, &b, f, root, &facts, Plant::None);
        assert!(matches!(unread, Err(Refusal { form: Form::CalleeValueUnknown, .. })), "{unread:?}");
    }
}

/// An `FnMut` closure called while a shared loan of it is held (the Reference, "Closure
/// types", "Call traits and coercions": `FnMut::call_mut` takes `&mut self`): `fn f(s: S) {
/// let mut c = || m(&mut s); c(); c(); }` is right (each call's mutable borrow ends with it).
/// Its negative, `.. let r = &c; c(); let d = r; }`, calls `c` while `r` borrows it: rustc
/// E0502, here `Invalidated`.
/// Prints: `fn mut while borrowed: ["borrow Invalidated ..` and `fn mut twice: []`.
#[test]
fn an_fn_mut_called_while_borrowed() {
    let mut owner = None;
    let w = world_with(|fx, st, _rs, unit| owner = Some(fx.func("f", 0, &[st], unit, no_clauses())));
    let f = owner.expect("declared");
    for borrowed in [false, true] {
        let mut b = Bb::new();
        let s = b.param("s");
        let inner = call_ref(&mut b, "m", Mutbl::Mut, s);
        let cl = b.closure(&[], inner);
        let pc = b.bind_mut("c");
        let st1 = b.let_(pc, None, cl);
        let root = if borrowed {
            let lc = b.local(pc);
            let r = b.borrow(Mutbl::Not, lc);
            let pr = b.bind("r");
            let st2 = b.let_(pr, None, r);
            let lc2 = b.local(pc);
            let call = b.call(lc2, &[]);
            let st3 = b.semi(call);
            let lr = b.local(pr);
            let pd = b.bind("d");
            let st4 = b.let_(pd, None, lr);
            b.block(&[st1, st2, st3, st4], None)
        } else {
            let (l1, l2) = (b.local(pc), b.local(pc));
            let (c1, c2) = (b.call(l1, &[]), b.call(l2, &[]));
            let (st2, st3) = (b.semi(c1), b.semi(c2));
            b.block(&[st1, st2, st3], None)
        };
        let got = findings_with(&w.fx, &b, f, root, &Lane::new(), Plant::None);
        std::println!("fn mut {}: {got:?}", if borrowed { "while borrowed" } else { "twice" });
        let got = got.expect("lowered");
        if borrowed {
            assert!(got.iter().any(|l| l.starts_with("borrow Invalidated")), "{got:?}");
        } else {
            assert!(got.is_empty(), "{got:?}");
        }
    }
}

/// An `FnOnce` closure called twice (the Reference, "Closure types": a closure that moves out
/// of a capture implements only `FnOnce`, whose `call_once` takes `self` by value): `fn f(s:
/// S) { let c = move || h(s); c(); }` is right; `.. c(); c(); }` uses `c` after its move:
/// rustc E0382, here a `move` finding.
/// Prints: `fn once twice: ["move ..` and `fn once once: []`.
#[test]
fn an_fn_once_called_twice() {
    let mut owner = None;
    let w = world_with(|fx, st, _rs, unit| owner = Some(fx.func("f", 0, &[st], unit, no_clauses())));
    let f = owner.expect("declared");
    for twice in [false, true] {
        let mut b = Bb::new();
        let s = b.param("s");
        let inner = call1(&mut b, "h", s);
        let cl = b.move_closure(&[], inner);
        let pc = b.bind("c");
        let st1 = b.let_(pc, None, cl);
        let l1 = b.local(pc);
        let c1 = b.call(l1, &[]);
        let st2 = b.semi(c1);
        let root = if twice {
            let l2 = b.local(pc);
            let c2 = b.call(l2, &[]);
            let st3 = b.semi(c2);
            b.block(&[st1, st2, st3], None)
        } else {
            b.block(&[st1, st2], None)
        };
        let got = findings_with(&w.fx, &b, f, root, &Lane::new(), Plant::None);
        std::println!("fn once {}: {got:?}", if twice { "twice" } else { "once" });
        let got = got.expect("lowered");
        assert_eq!(got.iter().any(|l| l.starts_with("move ")), twice, "{got:?}");
        if !twice {
            assert!(got.is_empty(), "{got:?}");
        }
    }
}

/// A closure of this body whose result is a borrow of its capture (callee_value.rs): `fn f(s:
/// S) { let c = || &s; let r = c(); g(r); m(&mut s); }` is right. Its negative, `.. let r =
/// c(); m(&mut s); g(r); }`, mutates `s` while `r` holds the capture's loan: rustc E0502, here
/// `Invalidated`. CONTROL (rule 23): `Plant::ClosureCallUnrelated` relates nothing into the
/// result, and the negative is accepted. Before this piece the call was refused
/// (`CalleeValueUnknown`: the callee is no body parameter).
/// Prints: `closure result late: ["borrow Invalidated ..`, `closure result early: []` and
/// `closure result late, planted ClosureCallUnrelated: []`.
#[test]
fn a_closure_result_holds_its_capture() {
    let mut owner = None;
    let w = world_with(|fx, st, _rs, unit| owner = Some(fx.func("f", 0, &[st], unit, no_clauses())));
    let f = owner.expect("declared");
    for late in [false, true] {
        let mut b = Bb::new();
        let s = b.param("s");
        let ls = b.local(s);
        let inner = b.borrow(Mutbl::Not, ls);
        let cl = b.closure(&[], inner);
        let pc = b.bind("c");
        let st1 = b.let_(pc, None, cl);
        let lc = b.local(pc);
        let call = b.call(lc, &[]);
        let pr = b.bind("r");
        let st2 = b.let_(pr, None, call);
        let use_r = call1(&mut b, "g", pr);
        let st_use = b.semi(use_r);
        let mutate = call_ref(&mut b, "m", Mutbl::Mut, s);
        let st_mut = b.semi(mutate);
        let root = if late { b.block(&[st1, st2, st_mut, st_use], None) } else { b.block(&[st1, st2, st_use, st_mut], None) };
        let got = findings_with(&w.fx, &b, f, root, &Lane::new(), Plant::None);
        std::println!("closure result {}: {got:?}", if late { "late" } else { "early" });
        let got = got.expect("lowered");
        if late {
            assert!(got.iter().any(|l| l.starts_with("borrow Invalidated")), "{got:?}");
            let planted = findings_with(&w.fx, &b, f, root, &Lane::new(), Plant::ClosureCallUnrelated);
            std::println!("closure result late, planted ClosureCallUnrelated: {planted:?}");
            assert!(planted.as_ref().is_ok_and(|p| p.is_empty()), "{planted:?}");
        } else {
            assert!(got.is_empty(), "{got:?}");
        }
    }
}
