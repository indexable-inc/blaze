//! Calls through the `Fn*` traits. The rule, from the pinned library's declarations
//! (library/core/src/ops/function.rs: `#[lang = "fn_once"] trait FnOnce<Args: Tuple> { type
//! Output; extern "rust-call" fn call_once(self, args: Args) -> Self::Output; }`, `FnMut::
//! call_mut(&mut self, args: Args)`, `Fn::call(&self, args: Args)`) and the unstable
//! `unboxed_closures` / `fn_traits` features: a "rust-call" fn's last parameter is a tuple, taken
//! as one value at the type level and spread only by the call's ABI. So `f(a)` on a callee that
//! is no fn item, fn pointer or closure is `Fn::call(&f, (a,))`, else `FnMut::call_mut(&mut f,
//! (a,))`, else `FnOnce::call_once(f, (a,))` (rustc's `try_overloaded_call_traits` order), its
//! type `<F as FnOnce<(A,)>>::Output`. `Args` that is no tuple is E0059. A fn item is `Fn*`
//! only when safe and not "rust-call" (`FnSig::is_fn_trait_compatible`).
//!
//! ```text
//! #[lang = "fn_once"] trait FnOnce<Args> { type Output; extern "rust-call" fn call_once(self, args: Args) -> Self::Output; }
//! #[lang = "fn_mut"] trait FnMut<Args>: FnOnce<Args> { extern "rust-call" fn call_mut(&mut self, args: Args) -> Self::Output; }
//! #[lang = "fn"] trait Fn<Args>: FnMut<Args> { extern "rust-call" fn call(&self, args: Args) -> Self::Output; }
//! fn by_fn<F: Fn(u8) -> u16>(f: F) -> u16;
//! fn by_once<F: FnOnce(u8) -> u16>(f: F) -> u16;
//! fn by_mut<F: FnMut(u8) -> u16>(f: &mut F) -> u16;
//! fn not_tuple<F: Fn<u8, Output = u16>>(f: F);
//! fn apply<F: Fn(u8) -> u16>(f: F) -> u16;
//! fn k(x: u8) -> u16;   unsafe fn u(x: u8) -> u16;   extern "rust-call" fn rc(x: (u8,)) -> u16;
//! fn main();
//! ```

use std::vec::Vec;

use super::{Want, adjusts, check_body, expect};
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{Abi, ExprId, FnPtrHeader, ItemKey, LangItem, Mutbl, Name, PatId, Safety, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

struct Items {
    by_fn: ItemKey,
    by_once: ItemKey,
    by_mut: ItemKey,
    not_tuple: ItemKey,
    main: ItemKey,
    fn_: ItemKey,
    fn_once: ItemKey,
    h: ItemKey,
}

fn world() -> (Fx, Items) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
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

    let (u8_, u16_) = (fx.ty(PNode::Uint(UintTy::U8)), fx.ty(PNode::Uint(UintTy::U16)));
    let unit = fx.unit();
    let args = fx.tuple(&[u8_]);
    let ret = fx.proj(fn_once, &[p0, args], "Output", u16_);

    let b = fx.bound(fn_, &[p0, args]);
    let by_fn = fx.func("by_fn", 1, &[p0], u16_, std::vec![b, ret]);
    let b = fx.bound(fn_once, &[p0, args]);
    let by_once = fx.func("by_once", 1, &[p0], u16_, std::vec![b, ret]);
    let b = fx.bound(fn_mut, &[p0, args]);
    let m_p0 = fx.r(Mutbl::Mut, p0);
    let by_mut = fx.func("by_mut", 1, &[m_p0], u16_, std::vec![b, ret]);
    let b = fx.bound(fn_, &[p0, u8_]);
    let ret_u8 = fx.proj(fn_once, &[p0, u8_], "Output", u16_);
    let not_tuple = fx.func("not_tuple", 1, &[p0], unit, std::vec![b, ret_u8]);
    let b = fx.bound(fn_, &[p0, args]);
    fx.func("apply", 1, &[p0], u16_, std::vec![b, ret]);

    fx.func("k", 0, &[u8_], u16_, Vec::new());
    let u = fx.func("u", 0, &[u8_], u16_, Vec::new());
    fx.set_header(u, FnPtrHeader { abi: Abi::Rust, safety: Safety::Unsafe, c_variadic: false });
    let rc = fx.func("rc", 0, &[args], u16_, Vec::new());
    fx.set_header(rc, rust_call);
    let main = fx.func("main", 0, &[], unit, Vec::new());
    let h = fx.func("h", 0, &[], u16_, Vec::new());
    let unsize = fx.tr("Unsize", 2, Vec::new(), &[]);
    fx.lang(LangItem::Unsize, unsize);
    fx.finish();
    (fx, Items { by_fn, by_once, by_mut, not_tuple, main, fn_, fn_once, h })
}

/// `f(1)` in the body of `owner`, whose one parameter is `f`.
fn call_f(b: &mut Bb) -> ExprId {
    let p = b.param("f");
    let e0 = b.local(p);
    let e1 = b.int(None);
    let e2 = b.call(e0, &[e1]);
    b.block(&[], Some(e2))
}

/// ```text
/// fn by_fn<F: Fn(u8) -> u16>(f: F) -> u16 { f(1) }
/// fn by_once<F: FnOnce(u8) -> u16>(f: F) -> u16 { f(1) }
/// fn by_mut<F: FnMut(u8) -> u16>(f: &mut F) -> u16 { f(1) }
/// ```
/// Each call is the trait's method: the literal is checked against the tuple's one field
/// (`u8`) and the call's type is the bound's `Output` (`u16`). `Fn` borrows the callee, `FnOnce`
/// takes it by value, and `&mut F` (no `Fn*` impl of its own here) is dereferenced to `F` and
/// reborrowed mutably for `FnMut`.
#[test]
fn a_bounded_parameter_is_called_through_its_trait() {
    let (fx, it) = world();
    for (owner, callee_ty, adj) in [
        (it.by_fn, "P0", "& -> &P0"),
        (it.by_once, "P0", ""),
        (it.by_mut, "&mut P0", "* -> P0, &mut -> &mut P0"),
    ] {
        let mut b = Bb::new();
        let root = call_f(&mut b);
        let c = check_body(&fx, &b, owner, root, Plant::None, InferPlant::None);
        expect(
            &fx,
            &b,
            &c,
            &Want {
                exprs: &[(ExprId(0), callee_ty), (ExprId(1), "u8"), (ExprId(2), "u16"), (ExprId(3), "u16")],
                pats: &[(PatId(0), callee_ty)],
                diags: &[],
            },
        );
        assert_eq!(adjusts(&fx, &c, ExprId(0)), adj, "the callee's adjustments in {owner:?}");
    }
}

/// ```text
/// fn by_fn<F: Fn(u8) -> u16>(f: F) -> u16 { f(1, 2) }
/// ```
/// The Reference, "Call expressions" (expr.call.desugar): a call of a non-function type is
/// `Fn::call(&f, (1, 2))`; `Args` is `(u8,)` (library/core/src/ops/function.rs, `extern
/// "rust-call" fn call(&self, args: Args)`), so two arguments are E0057 ("this function takes
/// 1 argument but 2 arguments were supplied" for a closure or `Fn*` callee), not E0061.
#[test]
fn wrong_count_through_fn_is_e0057() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let p = b.param("f");
    let e0 = b.local(p);
    let e1 = b.int(None);
    let e2 = b.int(None);
    let e3 = b.call(e0, &[e1, e2]);
    let root = b.block(&[], Some(e3));
    let c = check_body(&fx, &b, it.by_fn, root, Plant::None, InferPlant::None);
    let codes: Vec<&str> = c.diags.iter().map(|d| d.code.code()).collect();
    assert_eq!(codes, ["E0057"], "{:?}", c.diags);
}

/// ```text
/// fn not_tuple<F: Fn<u8, Output = u16>>(f: F) { f(1) }
/// ```
/// `Args` is `u8`, no tuple: E0059 at the callee ("cannot use call notation; the first type
/// parameter for the function trait is neither a tuple nor unit"), and the call is an error.
#[test]
fn args_that_are_no_tuple_are_e0059() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let root = call_f(&mut b);
    let c = check_body(&fx, &b, it.not_tuple, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[(ExprId(0), "P0"), (ExprId(1), "i32"), (ExprId(2), "{error}"), (ExprId(3), "{error}")],
            pats: &[(PatId(0), "P0")],
            diags: &[(DiagCode::CallArgsNotTuple, ExprId(0))],
        },
    );
}

/// ```text
/// fn h() -> u16 { let f: &dyn Fn(u8) -> u16 = &k; f(1) }
/// ```
/// The Reference, "Unsized Coercions": "`T` to `dyn U`, when `T` implements `U + Sized`, and `U`
/// is dyn compatible" (`&k` coerces to the object: the fn item is `Fn<(u8,)>`); "Trait object
/// types": the object implements its base trait and its supertraits, so `f(1)` is
/// `Fn::call(&*f, (1,))` (expr.call.desugar) and its type the object's own binding of
/// `<dyn Fn(u8) -> u16 as FnOnce<(u8,)>>::Output`, `u16`; the literal is checked against `u8`.
#[test]
fn a_dyn_fn_is_called_through_its_object() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let (u8_, u16_) = (b.ty(PNode::Uint(UintTy::U8)), b.ty(PNode::Uint(UintTy::U16)));
    let fields = b.t.intern_list(&[u8_]).expect("list");
    let args = b.ty(PNode::Tuple(fields));
    let args_l = b.t.intern_list(&[args]).expect("list");
    let principal = b.ty(PNode::ExTrait(it.fn_, args_l));
    let output = b.ty(PNode::ExProj { trait_: it.fn_once, name: Name::of("Output"), args: args_l, term: u16_ });
    let object = b.t.dynamic(&[principal, output]).expect("dyn");
    let r_object = b.ty(PNode::Ref(Mutbl::Not, object));
    let f = b.bind("f");
    let e0 = b.path(&["k"]);
    let e1 = b.borrow(Mutbl::Not, e0);
    let s = b.let_(f, Some(r_object), e1);
    let e2 = b.local(f);
    let e3 = b.int(None);
    let e4 = b.call(e2, &[e3]);
    let root = b.block(&[s], Some(e4));
    let c = check_body(&fx, &b, it.h, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[
                (ExprId(0), "fn k"),
                (ExprId(1), "&fn k"),
                (ExprId(2), "&dyn Fn<(u8,)> + FnOnce<(u8,)>::4b0c5b46233fa2c6 = u16"),
                (ExprId(3), "u8"),
                (ExprId(4), "u16"),
                (ExprId(5), "u16"),
            ],
            pats: &[],
            diags: &[],
        },
    );
    let unsize = adjusts(&fx, &c, ExprId(1));
    assert!(unsize.ends_with("unsize -> &dyn Fn<(u8,)> + FnOnce<(u8,)>::4b0c5b46233fa2c6 = u16"), "`&k`'s adjustments: {unsize}");
}

/// ```text
/// fn main() { apply(k); apply(u); apply(rc); }
/// ```
/// A safe Rust fn item is `Fn<(u8,)>` with `Output = u16`; an `unsafe fn` and an
/// `extern "rust-call" fn` are not (`FnSig::is_fn_trait_compatible`): E0277 at each.
#[test]
fn only_safe_rust_fn_items_are_fn() {
    let (fx, it) = world();
    for (name, refused) in [("k", 0), ("u", 1), ("rc", 1)] {
        let mut b = Bb::new();
        let a = b.path(&["apply"]);
        let f = b.path(&[name]);
        let call = b.call(a, &[f]);
        let s = b.semi(call);
        let root = b.block(&[s], None);
        let c = check_body(&fx, &b, it.main, root, Plant::None, InferPlant::None);
        let unsatisfied = c.diags.iter().filter(|d| d.code == DiagCode::Unsatisfied).count();
        // The `Fn` bound and its `Output` projection each fail for a refused item (two E0277
        // rows at one range; rustc reports the bound only).
        assert_eq!((unsatisfied > 0, c.diags.len() == unsatisfied), (refused > 0, true), "apply({name}): {:?}", c.diags);
    }
}
