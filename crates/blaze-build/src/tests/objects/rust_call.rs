//! A `extern "rust-call"` method of a trait object called with its tuple written whole, built
//! from the objects fixture world plus one trait, emitted and run natively.
//!
//! The rule. core's library/core/src/ops/function.rs (nightly-2026-10-27) writes `impl<A:
//! Tuple, F: ?Sized> FnMut<A> for &mut F where F: FnMut<A> { extern "rust-call" fn
//! call_mut(&mut self, args: A) -> F::Output { (**self).call_mut(args) } }`: at `F = dyn
//! FnMut(..)` the inner call goes through the vtable with the tuple `args` as one value. The
//! convention of "rust-call" (codegen.md, "Closures and the Fn traits") passes the last tuple's
//! fields as separate parameters, so the slot's fn, built as any "rust-call" fn, takes them
//! apart: the caller must spread the tuple as a direct call does (walk.rs `spread_tuple`).
//!
//! The world adds, beside objects.rs's:
//! ```text
//! trait Call2 { extern "rust-call" fn c(&self, t: (u64, u64)) -> u64; }
//! impl Call2 for u64 { extern "rust-call" fn c(&self, t: (u64, u64)) -> u64 { *self * 100 + t.0 * 10 + t.1 } }
//! ```

use std::alloc::Global;
use std::vec::Vec as StdVec;

use blaze_own::Alloc;
use blaze_typeck::fixture::Bb;
use blaze_typeck::{Abi as TAbi, BinOp, Expr, ExprId, FnPtrHeader, ItemKey, LangItem, Mutbl, PNode, PortableTys, Safety, UnOp};

use super::super::{Tables, run_native_vt};
use super::{World, build, dyn_ty, ground_of, world};
use crate::vtable::ObjectMethod;

/// objects.rs's world with `Call2` and its impl for `u64`: (the world, the trait, the impl fn).
fn world_call2() -> (World, ItemKey, ItemKey, ItemKey) {
    let mut w = world();
    let fx = &mut w.fx;
    let rust_call = FnPtrHeader { abi: TAbi::RustCall, safety: Safety::Safe, c_variadic: false };
    // core's `#[lang = "tuple_trait"] trait Tuple {}`: a "rust-call" fn's last parameter must
    // implement it (the checker's own impls cover every tuple type).
    let tuple = fx.tr("Tuple", 1, Vec::new(), &[]);
    fx.lang(LangItem::Tuple, tuple);
    let p0 = fx.param(0);
    let r_self = fx.r(Mutbl::Not, p0);
    let u64_ = w.u64_;
    let pair = fx.tuple(&[u64_, u64_]);
    let call2 = fx.tr("Call2", 1, Vec::new(), &[]);
    let c = fx.trait_fn(call2, "c", true, &[r_self, pair], u64_);
    fx.set_header(c, rust_call);
    let i = fx.imp("call2_u64", 0, u64_, Some((call2, &[])), &[], Vec::new());
    let r_u64 = fx.r(Mutbl::Not, u64_);
    let f = fx.impl_fn(i, "c", true, &[r_u64, pair], u64_);
    fx.set_header(f, rust_call);
    fx.finish();
    w.traits.push((call2, StdVec::new(), std::vec![ObjectMethod { item: c, dispatch: true }]));
    w.rust_call.extend([c, f]);
    (w, call2, c, f)
}

/// `lhs <op> rhs`.
fn bin(b: &mut Bb, op: BinOp, lhs: ExprId, rhs: ExprId) -> ExprId {
    let at = b.int(None);
    b.exprs[at.ix()] = Expr::Binary { op, lhs, rhs };
    at
}

/// `base.index`.
fn tfield(b: &mut Bb, base: ExprId, index: u32) -> ExprId {
    let at = b.int(None);
    b.exprs[at.ix()] = Expr::TupleField { base, index };
    at
}

/// `let x: u64 = 5; let r: &dyn Call2 = &x; r.c((2, 3))`: the slot's fn gets `self` (the data
/// pointer, `*self` = 5) and the tuple's two fields: 500 + 20 + 3 = 523.
/// Prints `test tests::objects::rust_call::a_rust_call_method_of_an_object_takes_its_tuple_spread ... ok`.
/// Fails without the spread in walk.rs `dyn_call`: the build of `main` is refused ("a
/// \"rust-call\" method of a trait object called with its tuple written whole"); a call that
/// passed the tuple by address would give the callee a pointer for `t.0` and junk for `t.1`.
#[test]
fn a_rust_call_method_of_an_object_takes_its_tuple_spread() {
    let (w, call2, c, f) = world_call2();
    let mut ground = PortableTys::new_in(Alloc::global());

    let mut ib = Bb::new();
    let s = ib.param("self");
    let t = ib.param("t");
    let se = ib.local(s);
    let star = ib.int(None);
    ib.exprs[star.ix()] = Expr::Unary { op: UnOp::Deref, operand: se };
    let hundred = ib.int(None);
    let te = ib.local(t);
    let t0 = tfield(&mut ib, te, 0);
    let ten = ib.int(None);
    let te = ib.local(t);
    let t1 = tfield(&mut ib, te, 1);
    let a = bin(&mut ib, BinOp::Mul, star, hundred);
    let m = bin(&mut ib, BinOp::Mul, t0, ten);
    let sum = bin(&mut ib, BinOp::Add, a, m);
    let sum = bin(&mut ib, BinOp::Add, sum, t1);
    let iroot = ib.block(&[], Some(sum));
    let impl_built = build(&w, &ib, f, iroot, &[(hundred, 100), (ten, 10)], &mut ground);

    let mut b = Bb::new();
    let x = b.bind("x");
    let five = b.int(None);
    let u64_ty = b.ty(PNode::Uint(blaze_typeck::UintTy::U64));
    let s0 = b.let_(x, Some(u64_ty), five);
    let r = b.bind("r");
    let r_ty = dyn_ty(&mut b, call2, |b, d| b.ty(PNode::Ref(Mutbl::Not, d)));
    let xe = b.local(x);
    let borrow = b.borrow(Mutbl::Not, xe);
    let s1 = b.let_(r, Some(r_ty), borrow);
    let (two, three) = (b.int(None), b.int(None));
    let pair = b.tuple(&[two, three]);
    let re = b.local(r);
    let call = b.method(re, "c", &[pair]);
    let root = b.block(&[s0, s1], Some(call));
    let main = build(&w, &b, w.main, root, &[(five, 5), (two, 2), (three, 3)], &mut ground);

    let gu64 = ground_of(&w.fx, &mut ground, w.u64_);
    let impls = [(c, gu64, f)];
    let sizes = [(gu64, 8, 8, None)];
    let facts = |t| w.object_trait(t);
    let mut tables = Tables { facts: &facts, impls: &impls, sizes: &sizes, ground: &mut ground, placed: StdVec::new_in(Global) };
    let fns: [(ItemKey, Option<blaze_typeck::List>, &crate::Built<Global>); 2] = [(f, None, &impl_built), (w.main, None, &main)];
    assert_eq!(run_native_vt(&[], &fns, Some(&mut tables)), 523);
}
