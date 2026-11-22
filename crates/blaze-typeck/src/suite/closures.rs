//! Closures, fn items and constructors passed where a generic callee's bound says
//! `F: FnOnce(Args) -> R` (typeck.md, "Closures against `Fn` bounds"). The pinned library's
//! declarations (library/core/src/ops/function.rs):
//!
//! ```text
//! #[lang = "fn_once"] trait FnOnce<Args: Tuple> { #[lang = "fn_once_output"] type Output; extern "rust-call" fn call_once(self, args: Args) -> Self::Output; }
//! #[lang = "fn_mut"] trait FnMut<Args: Tuple>: FnOnce<Args> { extern "rust-call" fn call_mut(&mut self, args: Args) -> Self::Output; }
//! #[lang = "fn"] trait Fn<Args: Tuple>: FnMut<Args> { extern "rust-call" fn call(&self, args: Args) -> Self::Output; }
//! fn then<T, F: FnOnce() -> T>(f: F) -> T;              // `bool::then`, `ok_or_else`
//! fn map_err<E, O, F: FnOnce(E) -> O>(e: E, f: F) -> O; // `Result::map_err`, `map`
//! fn apply<F: Fn(u8) -> u16>(f: F) -> u16;
//! fn k() -> u16;   fn widen(x: u8) -> u16;
//! struct S(u8);    // its constructor is `fn(u8) -> S`
//! ```
//!
//! The Reference, "Closure types": a closure's type implements `FnOnce`, and `FnMut` / `Fn` by
//! how it uses its captures; "Call expressions": a call of a non-fn-item callee goes through
//! the `Fn*` traits. Where the Reference is silent (the closure's signature from the callee's
//! pending bound) blaze's rule is typeck.md's.

use std::vec::Vec;

use super::{Want, check_body, expect};
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{Abi, ExprId, FnPtrHeader, ItemKey, LangItem, Mutbl, PatId, Safety, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

struct Items {
    /// `fn main() -> u16`
    main_u16: ItemKey,
    /// `fn main(x: u8) -> u16`
    main_x: ItemKey,
    /// `fn main_m(g: Flag) -> Opt<u16>`
    main_m: ItemKey,
    /// `fn main_o(x: u8) -> Opt<u8>`
    main_o: ItemKey,
    /// `fn main_p(x: u8, f: fn(u8) -> u16) -> u16`
    main_p: ItemKey,
}

fn world() -> (Fx, Items) {
    world_with(true)
}

/// The world, with core's `impl Copy for u8` or (`u8_copy` false) without it, as in a
/// `no_core` crate that declares `Copy` and no impl (ladder rung front2/f5-closures before
/// its fix).
fn world_with(u8_copy: bool) -> (Fx, Items) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    // `Copy` and core's `impl Copy for u8`: a capture used by value is a copy, not a move.
    let copy = fx.tr("Copy", 1, Vec::new(), &[]);
    fx.lang(LangItem::Copy, copy);
    let u8_c = fx.ty(PNode::Uint(UintTy::U8));
    if u8_copy {
        fx.imp("impl Copy for u8", 0, u8_c, Some((copy, &[])), &[], Vec::new());
    }
    let (p0, p1, p2) = (fx.param(0), fx.param(1), fx.param(2));
    let rust_call = FnPtrHeader { abi: Abi::RustCall, safety: Safety::Safe, c_variadic: false };

    let fn_once = fx.tr("FnOnce", 2, Vec::new(), &["Output"]);
    fx.lang(LangItem::FnOnce, fn_once);
    // No answer for `fn_once_output` itself: the checker names it from `FnOnce` and `Output`
    // (`LangItem::assoc_type`). Before, it took the world's answer, which the real world gives
    // as the `type Output` item's own key (no `assoc_def`), and every closure's output was E0277.
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

    let (u8_, u16_) = (fx.ty(PNode::Uint(UintTy::U8)), fx.ty(PNode::Uint(UintTy::U16)));
    let none = fx.tuple(&[]);
    // fn then<T, F: FnOnce() -> T>(f: F) -> T
    let b = fx.bound(fn_once, &[p1, none]);
    let ret = fx.proj(fn_once, &[p1, none], "Output", p0);
    fx.func("then", 2, &[p1], p0, std::vec![b, ret]);
    // fn map_err<E, O, F: FnOnce(E) -> O>(e: E, f: F) -> O
    let e_args = fx.tuple(&[p0]);
    let b = fx.bound(fn_once, &[p2, e_args]);
    let ret = fx.proj(fn_once, &[p2, e_args], "Output", p1);
    fx.func("map_err", 3, &[p0, p2], p1, std::vec![b, ret]);
    // fn apply<F: Fn(u8) -> u16>(f: F) -> u16
    let args = fx.tuple(&[u8_]);
    let b = fx.bound(fn_, &[p0, args]);
    let ret = fx.proj(fn_once, &[p0, args], "Output", u16_);
    fx.func("apply", 1, &[p0], u16_, std::vec![b, ret]);
    // fn apply_mut<F: FnMut(u8) -> u16>(f: F) -> u16
    let b = fx.bound(fn_mut, &[p0, args]);
    let ret = fx.proj(fn_once, &[p0, args], "Output", u16_);
    fx.func("apply_mut", 1, &[p0], u16_, std::vec![b, ret]);
    // The library's impls for references (core/src/ops/function.rs `mod impls`), `A` = P0,
    // `F` = P1:
    // impl<A: Tuple, F: ?Sized> Fn<A> for &F where F: Fn<A>
    // impl<A: Tuple, F: ?Sized> FnMut<A> for &F where F: Fn<A>
    // impl<A: Tuple, F: ?Sized> FnOnce<A> for &F where F: Fn<A> { type Output = F::Output; }
    // impl<A: Tuple, F: ?Sized> FnMut<A> for &mut F where F: FnMut<A>
    // impl<A: Tuple, F: ?Sized> FnOnce<A> for &mut F where F: FnMut<A> { type Output = F::Output; }
    let f_out = fx.alias(fn_once, "Output", &[p1, p0]);
    for (m, by) in [(Mutbl::Not, fn_), (Mutbl::Mut, fn_mut)] {
        let self_ty = fx.r(m, p1);
        let traits: &[ItemKey] = if m == Mutbl::Not { &[fn_, fn_mut, fn_once] } else { &[fn_mut, fn_once] };
        for &t in traits {
            let w = fx.bound(by, &[p1, p0]);
            let assoc: &[(&str, _)] = if t == fn_once { &[("Output", f_out)] } else { &[] };
            fx.imp("impl Fn* for &F", 2, self_ty, Some((t, &[p0])), assoc, std::vec![w]);
        }
    }

    fx.func("k", 0, &[], u16_, Vec::new());
    fx.func("widen", 0, &[u8_], u16_, Vec::new());
    let main_u16 = fx.func("main_u16", 0, &[], u16_, Vec::new());
    let main_x = fx.func("main_x", 0, &[u8_], u16_, Vec::new());
    // struct Flag;  enum Opt<T> { Some(T), None }
    // impl Flag { fn then<T, F: FnOnce() -> T>(self, f: F) -> Opt<T>;
    //             fn map_err<O, F: FnOnce(u8) -> O>(self, f: F) -> Opt<O>; }
    let flag_k = fx.strukt("Flag", 0, &[]);
    let flag = fx.adt(flag_k, &[]);
    let (opt_k, _) = fx.enum_("Opt", 1, &[("Some", &[p0]), ("None", &[])]);
    let opt_t = fx.adt(opt_k, &[p0]);
    let imp = fx.imp("impl Flag", 0, flag, None, &[], Vec::new());
    let b = fx.bound(fn_once, &[p1, none]);
    let ret = fx.proj(fn_once, &[p1, none], "Output", p0);
    fx.impl_fn_own(imp, "then", true, 2, &[flag, p1], opt_t, std::vec![b, ret]);
    let b = fx.bound(fn_once, &[p1, args]);
    let ret = fx.proj(fn_once, &[p1, args], "Output", p0);
    fx.impl_fn_own(imp, "map_err", true, 2, &[flag, p1], opt_t, std::vec![b, ret]);
    let opt_u16 = fx.adt(opt_k, &[u16_]);
    let main_m = fx.func("main_m", 0, &[flag], opt_u16, Vec::new());
    let opt_u8 = fx.adt(opt_k, &[u8_]);
    let main_o = fx.func("main_o", 0, &[u8_], opt_u8, Vec::new());
    let io = fx.list(&[u8_, u16_]);
    let fn_u8_u16 = fx.ty(PNode::FnPtr(io, FnPtrHeader::RUST));
    let main_p = fx.func("main_p", 0, &[u8_, fn_u8_u16], u16_, Vec::new());
    fx.finish();
    (fx, Items { main_u16, main_x, main_m, main_o, main_p })
}

/// ```text
/// fn main_m(g: Flag) -> Opt<u16> { g.then(|| k()) }
/// fn main_m(g: Flag) -> Opt<u16> { g.map_err(|e| widen(e)) }
/// ```
/// The same closures as arguments of METHOD calls (`self.io.offered(..).then(|| AudioIn {..})`
/// at bstd audio.rs:131; `.map_err(|error| BlobsError {..})` at blobs.rs:176), whose output is
/// also expected from the body's return type.
#[test]
fn a_closure_against_a_methods_bound() {
    let (fx, it) = world();
    for (m, n) in [("then", 0), ("map_err", 1)] {
        let mut b = Bb::new();
        let g = b.param("g");
        let e0 = b.local(g);
        let params: Vec<PatId> = (0..n).map(|_| b.bind("e")).collect();
        let body = if n == 0 {
            let k = b.path(&["k"]);
            b.call(k, &[])
        } else {
            let w = b.path(&["widen"]);
            let a = b.local(params[0]);
            b.call(w, &[a])
        };
        let cl = b.closure(&params, body);
        let e = b.method(e0, m, &[cl]);
        let root = b.block(&[], Some(e));
        let c = check_body(&fx, &b, it.main_m, root, Plant::None, InferPlant::None);
        let d: Vec<_> = c.diags.iter().map(|d| (d.code, d.at)).collect();
        assert_eq!(d, Vec::new(), "g.{m}(closure)");
    }
}

/// ```text
/// fn main_u16() -> u16 { then(|| k()) }
/// ```
/// A zero-argument closure against `F: FnOnce() -> T` with `T` the callee's own parameter
/// (`bool::then`, `Option::ok_or_else`): `T` is decided by the closure's body.
#[test]
fn a_closure_decides_the_callees_output_parameter() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let e0 = b.path(&["then"]);
    let e1 = b.path(&["k"]);
    let e2 = b.call(e1, &[]);
    let e3 = b.closure(&[], e2);
    let e4 = b.call(e0, &[e3]);
    let root = b.block(&[], Some(e4));
    let c = check_body(&fx, &b, it.main_u16, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[
                (ExprId(0), "fn then<u16, closure#0 of main_u16<>>"),
                (ExprId(1), "fn k"),
                (ExprId(2), "u16"),
                (ExprId(3), "closure#0 of main_u16<>"),
                (ExprId(4), "u16"),
                (ExprId(5), "u16"),
            ],
            pats: &[],
            diags: &[],
        },
    );
}

/// ```text
/// fn main_x(x: u8) -> u16 { map_err(x, |e| widen(e)) }
/// ```
/// One argument against `F: FnOnce(E) -> O`: `E` is known from the first argument, the
/// closure's parameter type only from the bound, and `O` (the callee's parameter) from the body.
#[test]
fn a_closures_parameter_comes_from_the_bound_and_its_output_decides_a_parameter() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let x = b.param("x");
    let e0 = b.path(&["map_err"]);
    let e1 = b.local(x);
    let e = b.bind("e");
    let e2 = b.path(&["widen"]);
    let e3 = b.local(e);
    let e4 = b.call(e2, &[e3]);
    let e5 = b.closure(&[e], e4);
    let e6 = b.call(e0, &[e1, e5]);
    let root = b.block(&[], Some(e6));
    let c = check_body(&fx, &b, it.main_x, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[
                (ExprId(0), "fn map_err<u8, u16, closure#0 of main_x<>>"),
                (ExprId(1), "u8"),
                (ExprId(2), "fn widen"),
                (ExprId(3), "u8"),
                (ExprId(4), "u16"),
                (ExprId(5), "closure#0 of main_x<>"),
                (ExprId(6), "u16"),
                (ExprId(7), "u16"),
            ],
            pats: &[(PatId(0), "u8"), (PatId(1), "u8")],
            diags: &[],
        },
    );
}

/// ```text
/// fn main_x(x: u8) -> u16 { then(|| widen(x)) }
/// ```
/// A closure that CAPTURES (`x`, by shared reference: a `Fn` closure) against `F: FnOnce() ->
/// T` (audio.rs:131 captures `self`, blobs.rs:176 captures `id`).
#[test]
fn a_capturing_closure_decides_the_callees_output_parameter() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let x = b.param("x");
    let e0 = b.path(&["then"]);
    let e1 = b.path(&["widen"]);
    let e2 = b.local(x);
    let e3 = b.call(e1, &[e2]);
    let e4 = b.closure(&[], e3);
    let e5 = b.call(e0, &[e4]);
    let root = b.block(&[], Some(e5));
    let c = check_body(&fx, &b, it.main_x, root, Plant::None, InferPlant::None);
    let d: Vec<_> = c.diags.iter().map(|d| (d.code, d.at)).collect();
    assert_eq!(d, Vec::new(), "then(|| widen(x))");
}

/// The diagnostics of `fn main_u16() -> u16 { then(F) }` and of `fn main_x(x: u8) -> u16 {
/// map_err(x, G) }` with `F`, `G` a path to a fn item.
fn pass_item(zero: &str, one: &str) -> (Vec<DiagCode>, Vec<DiagCode>) {
    let (fx, it) = world();
    let mut b = Bb::new();
    let e0 = b.path(&["then"]);
    let e1 = b.path(&[zero]);
    let e2 = b.call(e0, &[e1]);
    let root = b.block(&[], Some(e2));
    let c0 = check_body(&fx, &b, it.main_u16, root, Plant::None, InferPlant::None);
    let mut b = Bb::new();
    let x = b.param("x");
    let e0 = b.path(&["map_err"]);
    let e1 = b.local(x);
    let e2 = b.path(&[one]);
    let e3 = b.call(e0, &[e1, e2]);
    let root = b.block(&[], Some(e3));
    let c1 = check_body(&fx, &b, it.main_x, root, Plant::None, InferPlant::None);
    (c0.diags.iter().map(|d| d.code).collect(), c1.diags.iter().map(|d| d.code).collect())
}

/// ```text
/// fn main_u16() -> u16 { then(k) }
/// fn main_x(x: u8) -> u16 { map_err(x, widen) }
/// ```
/// A fn item where `F: FnOnce(..) -> T` is expected: its built-in `Fn*` impl, `Output` its
/// signature's output, decides `T`.
#[test]
fn a_fn_item_decides_the_callees_output_parameter() {
    assert_eq!(pass_item("k", "widen"), (Vec::new(), Vec::new()));
}

/// Each case's findings as (code, range).
fn findings(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId) -> Vec<(DiagCode, crate::ids::Range)> {
    let c = check_body(fx, b, owner, root, Plant::None, InferPlant::None);
    c.diags.iter().map(|d| (d.code, d.at)).collect()
}

/// ```text
/// fn main_o(x: u8) -> Opt<u8> { map_err(x, Opt::Some) }
/// fn main_p(x: u8, f: fn(u8) -> u16) -> u16 { map_err(x, f) }
/// ```
/// A variant's constructor (`opt.map(Some)`) and a fn pointer where `F: FnOnce(E) -> O` is
/// expected: the built-in `Fn*` impls, `Output` the signature's output, decide `O`.
#[test]
fn a_constructor_and_a_fn_pointer_decide_the_callees_output_parameter() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let x = b.param("x");
    let e0 = b.path(&["map_err"]);
    let e1 = b.local(x);
    let e2 = b.path(&["Opt", "Some"]);
    let e3 = b.call(e0, &[e1, e2]);
    let root = b.block(&[], Some(e3));
    assert_eq!(findings(&fx, &b, it.main_o, root), Vec::new(), "map_err(x, Opt::Some)");
    let mut b = Bb::new();
    let x = b.param("x");
    let f = b.param("f");
    let e0 = b.path(&["map_err"]);
    let e1 = b.local(x);
    let e2 = b.local(f);
    let e3 = b.call(e0, &[e1, e2]);
    let root = b.block(&[], Some(e3));
    assert_eq!(findings(&fx, &b, it.main_p, root), Vec::new(), "map_err(x, f)");
}

/// ```text
/// fn main_u16() -> u16 { let g = (|| |y| widen(y))(); apply(g) }
/// ```
/// A closure that returns a closure: the inner one's parameter is decided by its body, its
/// kind (`Fn`: no captures) after; then it meets `F: Fn(u8) -> u16`.
#[test]
fn a_closure_returned_by_a_closure_meets_a_bound() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let y = b.bind("y");
    let w = b.path(&["widen"]);
    let ly = b.local(y);
    let call_w = b.call(w, &[ly]);
    let inner = b.closure(&[y], call_w);
    let outer = b.closure(&[], inner);
    let called = b.call(outer, &[]);
    let g = b.bind("g");
    let s0 = b.let_(g, None, called);
    let a = b.path(&["apply"]);
    let lg = b.local(g);
    let e = b.call(a, &[lg]);
    let root = b.block(&[s0], Some(e));
    assert_eq!(findings(&fx, &b, it.main_u16, root), Vec::new());
}

/// `fn main_x(x: u8) -> u16 { let f = |y| widen(y); TAIL }` for each tail: `f(x)` (a stored
/// closure called later: its signature from its body), `apply(&f)` and `apply_mut(&mut f)`
/// (through the library's `impl<A, F: Fn<A>> Fn<A> for &F` and `impl<A, F: FnMut<A>> FnMut<A>
/// for &mut F`: impls of the world, not built-ins), and `{ let r = &f; r(x) }`.
#[test]
fn a_stored_closure_is_called_directly_and_through_references() {
    let (fx, it) = world();
    for case in 0..4 {
        let mut b = Bb::new();
        let x = b.param("x");
        let y = b.bind("y");
        let w = b.path(&["widen"]);
        let ly = b.local(y);
        let call_w = b.call(w, &[ly]);
        let cl = b.closure(&[y], call_w);
        let f = b.bind("f");
        let s0 = b.let_(f, None, cl);
        let mut stmts = std::vec![s0];
        let tail = match case {
            0 => {
                let lf = b.local(f);
                let lx = b.local(x);
                b.call(lf, &[lx])
            }
            1 | 2 => {
                let (name, m) = if case == 1 { ("apply", Mutbl::Not) } else { ("apply_mut", Mutbl::Mut) };
                let a = b.path(&[name]);
                let lf = b.local(f);
                let r = b.borrow(m, lf);
                b.call(a, &[r])
            }
            _ => {
                let lf = b.local(f);
                let r = b.borrow(Mutbl::Not, lf);
                let rp = b.bind("r");
                stmts.push(b.let_(rp, None, r));
                let lr = b.local(rp);
                let lx = b.local(x);
                b.call(lr, &[lx])
            }
        };
        let root = b.block(&stmts, Some(tail));
        assert_eq!(findings(&fx, &b, it.main_x, root), Vec::new(), "case {case}");
    }
}

/// `fn main_x(x: u8) -> u16 { apply(BODY) }` (ladder rung front2/f5-closures: `apply(|x| x *
/// k, 21)` with `apply<F: Fn(u8) -> u8>`), the world with or without `impl Copy for u8`.
fn apply_case(u8_copy: bool, captures: bool) -> Vec<DiagCode> {
    let (fx, it) = world_with(u8_copy);
    let mut b = Bb::new();
    let x = b.param("x");
    let y = b.bind("y");
    let w = b.path(&["widen"]);
    // `|y| widen(x)` uses the capture `x` by value; `|y| widen(y)` captures nothing.
    let arg = if captures { b.local(x) } else { b.local(y) };
    let body = b.call(w, &[arg]);
    let cl = b.closure(&[y], body);
    let a = b.path(&["apply"]);
    let e = b.call(a, &[cl]);
    let root = b.block(&[], Some(e));
    findings(&fx, &b, it.main_x, root).into_iter().map(|(code, _)| code).collect()
}

/// A closure that uses a captured `u8` by value meets `F: Fn(u8) -> u16` (local `Fn*` lang
/// items) when `u8: Copy` holds: the use is a copy, so the capture is a shared borrow (the
/// Reference, "Closure types", capture modes) and the closure is `Fn`. Without the impl (a
/// `no_core` crate declaring `Copy` and no impl: a primitive is `Copy` only by core's impl,
/// rustc select/mod.rs `copy_clone_conditions`) the use moves `x`, the closure is only
/// `FnOnce` (rustc upvar.rs: a by-value capture), and the bound is unmet (E0277; rustc gives
/// E0525/E0507 for the same program). Control: the closure that captures nothing meets the
/// bound in both worlds, so the refusal is the capture's.
#[test]
fn a_closure_using_a_capture_by_value_is_fn_only_when_the_capture_is_copy() {
    assert_eq!(apply_case(true, true), Vec::new(), "with `impl Copy for u8`: `|y| widen(x)` is Fn");
    assert_eq!(apply_case(false, true), std::vec![DiagCode::Unsatisfied], "without it: `|y| widen(x)` is FnOnce");
    assert_eq!(apply_case(false, false), Vec::new(), "control: `|y| widen(y)` captures nothing");
    assert_eq!(apply_case(true, false), Vec::new(), "control with the impl");
}
