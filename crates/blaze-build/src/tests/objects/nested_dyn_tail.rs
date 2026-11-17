//! A trait object at the end of two nested structs, `Nest<dyn Get>`, built from the objects
//! fixture world, emitted and run natively: its tail reached through both levels and its
//! `size_of_val` / `align_of_val`.
//!
//! The rule. The Reference, "Dynamically Sized Types": "Structs may contain a DST as the last
//! field; this makes the struct itself a DST", and such a field may itself be such a struct;
//! "Type layout": each field is placed at the next multiple of its alignment, and a trait
//! object's alignment is its value's (the vtable's word 2), so `align_of_val` of the inner
//! struct is the larger of its static alignment and the vtable's: the inner struct's offset and
//! the trait object's offset in it are each rounded at run time. rustc's
//! `size_and_align_of_dst` recurses the same way: per level `size = round_up(offset + tail_size,
//! max(static_align, tail_align))`.
//!
//! The world adds, beside objects.rs's:
//! ```text
//! struct Tl<T: ?Sized>(u8, T);   struct Nest<T: ?Sized>(u8, Tl<T>);
//! trait Get { fn get(&self) -> u64; }   impl Get for u128 { fn get(&self) -> u64 { *self as u64 } }
//! fn transmute<T, U>(t: T) -> U;   // the intrinsic
//! ```
//! `Nest<dyn Get>`'s static rows: `Tl` at 1 in `Nest` (`Tl`'s static alignment is 1) and the
//! `dyn` at 1 in `Tl`. With a `u128`'s vtable (size 16, alignment 16): `Tl` at `round_up(1, 16)
//! = 16`, the `dyn` at `16 + round_up(1, 16) = 32`; `size_of_val` is `Tl`'s `round_up(16 + 16,
//! 16) = 32`, then `Nest`'s `round_up(16 + 32, 16) = 48`; `align_of_val` 16.

use std::alloc::Global;
use std::vec::Vec as StdVec;

use blaze_own::Alloc;
use blaze_typeck::fixture::Bb;
use blaze_typeck::{CtorKind, Def, Expr, ExprId, ItemKey, Mutbl, PNode, PortableTys, UintTy, UnOp};

use super::super::{Tables, run_native_vt};
use super::{World, build, dyn_ty, world};
use crate::Intrinsic;
use crate::vtable::ObjectMethod;

/// The added items: `Nest`, the trait `Get`, its method and its impl fn for `u128`.
struct Added {
    nest: ItemKey,
    get_tr: ItemKey,
    get: ItemKey,
    get_u128: ItemKey,
}

fn world_nested_dyn() -> (World, Added) {
    let mut w = world();
    let fx = &mut w.fx;
    let (p0, p1) = (fx.param(0), fx.param(1));
    let u8_ = fx.u8();
    let u128_ = fx.ty(PNode::Uint(UintTy::U128));
    let tuple = |adt| Def::Ctor { adt, variant: 0, kind: CtorKind::Tuple };
    let tl = fx.tuple_strukt("Tl", 1, &[u8_, p0]);
    fx.use_path("Tl", tuple(tl));
    let tl_p0 = fx.adt(tl, &[p0]);
    let nest = fx.tuple_strukt("Nest", 1, &[u8_, tl_p0]);
    fx.use_path("Nest", tuple(nest));
    let get_tr = fx.tr("Get", 1, Vec::new(), &[]);
    let r_self = fx.r(Mutbl::Not, p0);
    let u64_ = w.u64_;
    let get = fx.trait_fn(get_tr, "get", true, &[r_self], u64_);
    let i = fx.imp("get_u128", 0, u128_, Some((get_tr, &[])), &[], Vec::new());
    let r_u128 = fx.r(Mutbl::Not, u128_);
    let get_u128 = fx.impl_fn(i, "get", true, &[r_u128], u64_);
    let transmute = fx.func("transmute", 2, &[p0], p1, Vec::new());
    fx.finish();
    w.intrinsics.push((transmute, Intrinsic::Table(crate::intrinsics::Which::Transmute)));
    w.traits.push((get_tr, StdVec::new(), std::vec![ObjectMethod { item: get, dispatch: true }]));
    (w, Added { nest, get_tr, get, get_u128 })
}

/// `base.index`.
fn tfield(b: &mut Bb, base: ExprId, index: u32) -> ExprId {
    let at = b.int(None);
    b.exprs[at.ix()] = Expr::TupleField { base, index };
    at
}

/// `operand as <t>`.
fn cast(b: &mut Bb, operand: ExprId, t: PNode) -> ExprId {
    let ty = b.ty(t);
    let at = b.int(None);
    b.exprs[at.ix()] = Expr::Cast { operand, ty };
    at
}

/// `lhs <op> rhs`.
fn bin(b: &mut Bb, op: blaze_typeck::BinOp, lhs: ExprId, rhs: ExprId) -> ExprId {
    let at = b.int(None);
    b.exprs[at.ix()] = Expr::Binary { op, lhs, rhs };
    at
}

/// ```text
/// fn main() -> u64 {
///     let k: u128 = 0;
///     let d: &dyn Get = &k;                           // a `u128`'s vtable
///     let pair: (usize, usize) = transmute(d);
///     let buf: (u128, u128, u128, u128) = (10, 20, 30, 40);
///     let base: usize = transmute(&buf);
///     let raw: (usize, usize) = (base, pair.1);
///     let o: &Nest<dyn Get> = transmute(raw);         // `buf`'s address, the vtable
///     o.1.1.get() * 10000 + size_of_val(o) as u64 * 100 + align_of_val(o) as u64
/// }                                                   // 30 * 10000 + 48 * 100 + 16 = 304_816
/// ```
/// The `dyn` is at byte 32 of `buf`, its third `u128` (30), by the module doc's two roundings.
/// Prints `test tests::objects::nested_dyn_tail::a_dyn_tail_two_structs_deep_is_rounded_at_each_level ... ok`.
/// Fails without layout.rs `dyn_chain`: the layout of `Nest<dyn Get>` is refused ("the nested
/// tail's offset needs two run-time roundings"); a version rounding only the innermost level
/// reads byte 17 (not a `u128` of `buf`) and answers a size of 33 rounded to 48 or less.
#[test]
fn a_dyn_tail_two_structs_deep_is_rounded_at_each_level() {
    let (w, a) = world_nested_dyn();
    let mut ground = PortableTys::new_in(Alloc::global());

    let mut ib = Bb::new();
    let s = ib.param("self");
    let se = ib.local(s);
    let star = ib.int(None);
    ib.exprs[star.ix()] = Expr::Unary { op: UnOp::Deref, operand: se };
    let body = cast(&mut ib, star, PNode::Uint(UintTy::U64));
    let iroot = ib.block(&[], Some(body));
    let impl_built = build(&w, &ib, a.get_u128, iroot, &[], &mut ground);

    let mut b = Bb::new();
    let usize_ = b.ty(PNode::Uint(UintTy::Usize));
    let u128_ = b.ty(PNode::Uint(UintTy::U128));
    let pair_ty = b.t.intern_list(&[usize_, usize_]).expect("list");
    let pair_ty = b.ty(PNode::Tuple(pair_ty));
    let buf_ty = b.t.intern_list(&[u128_, u128_, u128_, u128_]).expect("list");
    let buf_ty = b.ty(PNode::Tuple(buf_ty));
    let nest_dyn = dyn_ty(&mut b, a.get_tr, |b, d| {
        let l = b.t.intern_list(&[d]).expect("list");
        let n = b.ty(PNode::Adt(a.nest, l));
        b.ty(PNode::Ref(Mutbl::Not, n))
    });
    let d_ty = dyn_ty(&mut b, a.get_tr, |b, d| b.ty(PNode::Ref(Mutbl::Not, d)));
    let mut lits = StdVec::new();
    let mut stmts = StdVec::new();

    let k = b.bind("k");
    let zero = b.int(None);
    lits.push((zero, 0u128));
    stmts.push(b.let_(k, Some(u128_), zero));
    let d = b.bind("d");
    let ke = b.local(k);
    let kr = b.borrow(Mutbl::Not, ke);
    stmts.push(b.let_(d, Some(d_ty), kr));
    let pair = b.bind("pair");
    let de = b.local(d);
    let tm = b.path(&["transmute"]);
    let pv = b.call(tm, &[de]);
    stmts.push(b.let_(pair, Some(pair_ty), pv));
    let buf = b.bind("buf");
    let elems: StdVec<ExprId> = [10u128, 20, 30, 40]
        .iter()
        .map(|&v| {
            let e = b.int(None);
            lits.push((e, v));
            e
        })
        .collect();
    let bv = b.tuple(&elems);
    stmts.push(b.let_(buf, Some(buf_ty), bv));
    let base = b.bind("base");
    let be = b.local(buf);
    let br = b.borrow(Mutbl::Not, be);
    let tm = b.path(&["transmute"]);
    let basev = b.call(tm, &[br]);
    stmts.push(b.let_(base, Some(usize_), basev));
    let raw = b.bind("raw");
    let (base_e, pe) = (b.local(base), b.local(pair));
    let vt = tfield(&mut b, pe, 1);
    let rawv = b.tuple(&[base_e, vt]);
    stmts.push(b.let_(raw, Some(pair_ty), rawv));
    let o = b.bind("o");
    let re = b.local(raw);
    let tm = b.path(&["transmute"]);
    let ov = b.call(tm, &[re]);
    stmts.push(b.let_(o, Some(nest_dyn), ov));

    let oe = b.local(o);
    let tl = tfield(&mut b, oe, 1);
    let tail = tfield(&mut b, tl, 1);
    let got = b.method(tail, "get", &[]);
    let k1 = b.int(None);
    lits.push((k1, 10000));
    let hi = bin(&mut b, blaze_typeck::BinOp::Mul, got, k1);
    let oe = b.local(o);
    let size = b.path(&["size_of_val"]);
    let size = b.call(size, &[oe]);
    let size = cast(&mut b, size, PNode::Uint(UintTy::U64));
    let k2 = b.int(None);
    lits.push((k2, 100));
    let mid = bin(&mut b, blaze_typeck::BinOp::Mul, size, k2);
    let oe = b.local(o);
    let align = b.path(&["align_of_val"]);
    let align = b.call(align, &[oe]);
    let align = cast(&mut b, align, PNode::Uint(UintTy::U64));
    let sum = bin(&mut b, blaze_typeck::BinOp::Add, hi, mid);
    let sum = bin(&mut b, blaze_typeck::BinOp::Add, sum, align);
    let root = b.block(&stmts, Some(sum));
    let main = build(&w, &b, w.main, root, &lits, &mut ground);

    let gu128 = ground.intern(PNode::Uint(UintTy::U128)).expect("the ground table is full");
    let impls = [(a.get, gu128, a.get_u128)];
    let sizes = [(gu128, 16, 16, None)];
    let facts = |t| w.object_trait(t);
    let mut tables = Tables { facts: &facts, impls: &impls, sizes: &sizes, ground: &mut ground, placed: StdVec::new_in(Global) };
    let fns: [(ItemKey, Option<blaze_typeck::List>, &crate::Built<Global>); 2] = [(a.get_u128, None, &impl_built), (w.main, None, &main)];
    assert_eq!(run_native_vt(&[], &fns, Some(&mut tables)), 304_816);
}
