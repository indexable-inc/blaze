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
use crate::vtable::{MethodDispatch, ObjectMethod};

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
    w.traits.push((call2, StdVec::new(), std::vec![ObjectMethod { item: c, dispatch: MethodDispatch::Direct }]));
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

/// `Box<dyn Once2>::call_once` (`(*self).call_once(args)`):
/// the vtable holds a `build_vtable_shim` taking `*mut Self` that moves concrete `A { x: 1 }`
/// out by value into `A::call_once`, while `box_call_once` clears the `[Deref]` drop bit on
/// `self` so `Box::drop` frees the box WITHOUT calling `dyn_glue` on the already-consumed `A`.
///
/// Verifies both native return value (`1 * 100 + 2 * 10 + 3 = 123`) AND exact drop log:
/// - called: `[1, 108]` (`A(1)` dropped once inside `call_once`, then 8-byte `Box` freed once)
/// - dropped uncalled: `[1, 108]` (`A(1)` dropped once via `dyn_glue`, then 8-byte `Box` freed once).
#[test]
#[cfg(target_arch = "aarch64")]
fn box_dyn_fnonce_by_value_dispatch_drops_once_and_frees_box() {
    use blaze_typeck::{InferCtx, List, Needs, Outcome};

    let alloc = blaze_own::testing::heap().alloc();
    let mut w = world();
    let fx = &mut w.fx;
    let rust_call = FnPtrHeader { abi: TAbi::RustCall, safety: Safety::Safe, c_variadic: false };
    let tuple_tr = fx.tr("Tuple", 1, Default::default(), &[]);
    fx.lang(LangItem::Tuple, tuple_tr);
    let u64_ = w.u64_;
    let pair = fx.tuple(&[u64_, u64_]);
    let p0 = fx.param(0);
    let a_ty = w.boxes.concrete[0].1;
    let bx_adt = w.boxes.box_k;

    // `trait Once2 { extern "rust-call" fn call_once(self, t: (u64, u64)) -> u64; }`
    let once2 = fx.tr("Once2", 1, Default::default(), &[]);
    let call_once_tr = fx.trait_fn(once2, "call_once", true, &[p0, pair], u64_);
    fx.set_header(call_once_tr, rust_call);

    // `impl Once2 for A`: takes `self: A` by value, returns `self.x * 100 + t.0 * 10 + t.1`, drops `self` once.
    let imp_a = fx.imp("once2_a", 0, a_ty, Some((once2, &[])), &[], Default::default());
    let call_once_a = fx.impl_fn(imp_a, "call_once", true, &[a_ty, pair], u64_);
    fx.set_header(call_once_a, rust_call);

    // `fn box_call_once(self: Box<dyn Once2>, args: (u64, u64)) -> u64 { (*self).call_once(args) }`
    let ex_once2 = fx.ty(PNode::ExTrait(once2, List::EMPTY));
    let dyn_once2 = fx.t.dynamic(&[ex_once2]).expect("dyn Once2");
    let box_dyn = fx.adt(bx_adt, &[dyn_once2]);
    let box_call_once = fx.func("box_call_once", 0, &[box_dyn, pair], u64_, Default::default());
    fx.set_header(box_call_once, rust_call);

    // `fn main_drop() -> u64 { { let b: Box<dyn Once2> = mk_a(); } 99 }`
    let main_drop = fx.func("main_drop", 0, &[], u64_, Default::default());

    fx.finish();
    w.traits.push((once2, Default::default(), [ObjectMethod { item: call_once_tr, dispatch: MethodDispatch::ByValueShim }].into()));
    w.rust_call.extend([call_once_tr, call_once_a, box_call_once]);

    let mut ground = PortableTys::new_in(alloc);

    // Build `impl Once2 for A { extern "rust-call" fn call_once(self, t: (u64, u64)) -> u64 { self.x * 100 + t.0 * 10 + t.1 } }`.
    let mut ib = Bb::new();
    let s = ib.param("self");
    let t = ib.param("t");
    let se = ib.local(s);
    let sx = ib.int(None);
    ib.exprs[sx.ix()] = Expr::Field { base: se, name: blaze_typeck::Name::of("x") };
    let hundred = ib.int(None);
    let te0 = ib.local(t);
    let t0 = tfield(&mut ib, te0, 0);
    let ten = ib.int(None);
    let te1 = ib.local(t);
    let t1 = tfield(&mut ib, te1, 1);
    let a = bin(&mut ib, BinOp::Mul, sx, hundred);
    let m = bin(&mut ib, BinOp::Mul, t0, ten);
    let sum = bin(&mut ib, BinOp::Add, a, m);
    let sum = bin(&mut ib, BinOp::Add, sum, t1);
    let iroot = ib.block(&[], Some(sum));
    let impl_built = build(&w, &ib, call_once_a, iroot, &[(hundred, 100), (ten, 10)], &mut ground);

    // Build `VTableShim` for `call_once_a` via `crate::build_vtable_shim`.
    let shim_sig = w.fx.sig_of(call_once_a).expect("call_once_a sig");
    let shim_built = {
        let mut subst = crate::Subst { args: &[], ground: &mut ground, scratch: alloc };
        let adts = super::FxAdts { w: &w, lits: &[] };
        let target = crate::Instance { item: call_once_a, args: List::EMPTY };
        let built = crate::build_vtable_shim(target, Some(&shim_sig), &mut subst, &adts, Default::default()).expect("build_vtable_shim");
        blaze_lir::verify(built.func.view()).expect("vtable shim LIR verifies");
        built
    };

    // Build `box_call_once(self: Box<dyn Once2>, args: (u64, u64)) -> u64`: `(*self).call_once(args)`.
    let mut bcb = Bb::new();
    let p_self = bcb.param("self");
    let p_args = bcb.param("args");
    let self_e = bcb.local(p_self);
    let star_self = bcb.int(None);
    bcb.exprs[star_self.ix()] = Expr::Unary { op: UnOp::Deref, operand: self_e };
    let args_e = bcb.local(p_args);
    let call_e = bcb.method(star_self, "call_once", &[args_e]);
    let bc_root = bcb.block(&[], Some(call_e));
    let box_call_once_built = build(&w, &bcb, box_call_once, bc_root, &[], &mut ground);

    let box_dyn_ty = |b: &mut Bb| {
        dyn_ty(b, once2, |b, d| {
            let l = b.t.intern_list(&[d]).expect("list");
            b.ty(PNode::Adt(bx_adt, l))
        })
    };

    // Build `main()`: `let b: Box<dyn Once2> = mk_a(); box_call_once(b, (2, 3))`.
    let mut mb = Bb::new();
    let bx_bind = mb.bind("b");
    let mk_fn = mb.path(&["mk_a"]);
    let mk_call = mb.call(mk_fn, &[]);
    let ty_m = box_dyn_ty(&mut mb);
    let s0 = mb.let_(bx_bind, Some(ty_m), mk_call);
    let be = mb.local(bx_bind);
    let (two, three) = (mb.int(None), mb.int(None));
    let pair_e = mb.tuple(&[two, three]);
    let bc_fn = mb.path(&["box_call_once"]);
    let call_bc = mb.call(bc_fn, &[be, pair_e]);
    let mroot = mb.block(&[s0], Some(call_bc));
    let main_built = build(&w, &mb, w.main, mroot, &[(two, 2), (three, 3)], &mut ground);

    // Build `main_drop()`: `{ let b: Box<dyn Once2> = mk_a(); } 99`.
    let mut db = Bb::new();
    let bx_bind2 = db.bind("b");
    let mk_fn2 = db.path(&["mk_a"]);
    let mk_call2 = db.call(mk_fn2, &[]);
    let ty_d = box_dyn_ty(&mut db);
    let s_drop = db.let_(bx_bind2, Some(ty_d), mk_call2);
    let inner_blk = db.block(&[s_drop], None);
    let s_blk = db.semi(inner_blk);
    let ninetynine = db.int(None);
    let droot = db.block(&[s_blk], Some(ninetynine));
    let main_drop_built = build(&w, &db, main_drop, droot, &[(ninetynine, 99)], &mut ground);

    // Build drop glues for `A` and `Box<dyn Once2>`.
    let a_ground = ground.copy_from(w.fx.t.view(), a_ty, None).expect("copy A");
    let box_dyn_ground = ground.copy_from(w.fx.t.view(), box_dyn, None).expect("copy Box<dyn Once2>");
    let mut gb = Bb::new();
    gb.param("p");
    let groot = gb.block(&[], None);
    let gview = gb.view(w.boxes.dip, groot);
    let mut ty_cx = InferCtx::new_in(alloc);
    let gchecked = match blaze_typeck::check(&gview, &w.fx, &mut ty_cx, &Needs::new_in(alloc), alloc) {
        Ok(Outcome::Done(c)) => c,
        other => panic!("the check of `drop_in_place` did not finish: {:?}", other.err()),
    };
    let gsig = w.fx.sig_of(w.boxes.dip).expect("`drop_in_place`'s signature");
    let mut make_glue = |t| {
        let args = ground.intern_list(&[t]).expect("intern");
        let mut subst = crate::Subst { args: &[t], ground: &mut ground, scratch: alloc };
        let built = crate::build_drop_glue(&gview, &gchecked, &gsig, &mut subst, &super::FxAdts { w: &w, lits: &[] }, Default::default())
            .unwrap_or_else(|e| panic!("glue at {t:?}: {e}"));
        (args, built)
    };
    let (glue_a_args, glue_a) = make_glue(a_ground);
    let (glue_box_dyn_args, glue_box_dyn) = make_glue(box_dyn_ground);

    let shim_item = ItemKey(0xCAFE_0001);
    let impls = [(call_once_tr, a_ground, shim_item)];
    let sizes = [(a_ground, 8, 8, Some(w.boxes.dip))];
    let facts = |t| w.object_trait(t);

    // 1. Calling `Box<dyn Once2>`: returns 123, drops `A(1)` ONCE inside `call_once`, then frees 8-byte `Box` ONCE.
    super::LOG.with(|l| l.borrow_mut().clear());
    let mut tables = Tables { facts: &facts, impls: &impls, sizes: &sizes, ground: &mut ground, placed: Default::default() };
    let fns = [
        (w.boxes.dip, Some(glue_a_args), &glue_a),
        (w.boxes.dip, Some(glue_box_dyn_args), &glue_box_dyn),
        (call_once_a, None, &impl_built),
        (shim_item, None, &shim_built),
        (box_call_once, None, &box_call_once_built),
        (w.main, None, &main_built),
    ];
    let got = run_native_vt(&w.boxes.host, &fns, Some(&mut tables));
    assert_eq!(got, 123, "Box<dyn Once2>::call_once return value");
    assert_eq!(super::logged(), [1, 108], "A(1) dropped once inside call_once, then Box freed once (no double-drop, no leak)");

    // 2. Dropping `Box<dyn Once2>` uncalled: returns 99, drops `A(1)` ONCE via dyn_glue, then frees 8-byte `Box` ONCE.
    super::LOG.with(|l| l.borrow_mut().clear());
    tables.placed.clear();
    let fns2 = [
        (w.boxes.dip, Some(glue_a_args), &glue_a),
        (w.boxes.dip, Some(glue_box_dyn_args), &glue_box_dyn),
        (call_once_a, None, &impl_built),
        (shim_item, None, &shim_built),
        (main_drop, None, &main_drop_built),
    ];
    let got2 = run_native_vt(&w.boxes.host, &fns2, Some(&mut tables));
    assert_eq!(got2, 99);
    assert_eq!(super::logged(), [1, 108], "uncalled Box<dyn Once2> drops A(1) once via dyn_glue then frees Box once");
}
