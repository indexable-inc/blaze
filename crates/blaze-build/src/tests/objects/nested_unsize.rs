//! Unsizing through a pointer nested in structs (walk/unsize_nested.rs), built from the
//! objects fixture world plus three user structs, emitted and run natively.
//!
//! The rule. The Rust Reference, "Type coercions", "Unsized coercions": "`Foo<..., T, ...>` to
//! `Foo<..., U, ...>`, when: `Foo` is a struct. `T` implements `Unsize<U>`. The last field of
//! `Foo` has a type involving `T`. If that field has type `Bar<T>`, then `Bar<T>` implements
//! `Unsize<Bar<U>>`. T is not part of the type of any other fields." And core's
//! `CoerceUnsized` (library/core/src/ops/unsize.rs, nightly-2026-10-27): "Such an impl can only
//! be written if `Foo<T>` has only a single non-phantomdata field involving `T`. If the type of
//! that field is `Bar<T>`, an implementation of `CoerceUnsized<Bar<U>> for Bar<T>` must exist.
//! The coercion will work by coercing the `Bar<T>` field into `Bar<U>` and filling in the rest
//! of the fields from `Foo<T>` to create a `Foo<U>`."
//!
//! The world adds, beside objects.rs's `Box`, `Ob`, `A`, `B` and `mk_a`/`mk_b`:
//! ```text
//! struct Wrap<T: ?Sized>(u32, Box<T>);
//! impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<Wrap<U>> for Wrap<T> {}
//! struct Outer<T>(u8, T);
//! impl<T: CoerceUnsized<U>, U> CoerceUnsized<Outer<U>> for Outer<T> {}
//! struct Two<T: ?Sized>(Box<T>, Box<T>);
//! impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<Two<U>> for Two<T> {}  // not a legal
//!     // impl (two fields involve `T`); the fixture's checker takes it, so the build is asked
//! ```
//! Without the fix (walk.rs `unsize_ends` before walk/unsize_nested.rs) each build of (a) and
//! (b) fails: "a `CoerceUnsized` struct with a field beside its pointer that is not
//! zero-sized" for `Wrap` (16 bytes before, 24 after), and for `Outer<Wrap<_>>` the same; (c)
//! is refused there too, but by that message, not by the rule's.

use std::alloc::Global;
use std::string::String;
use std::vec::Vec as StdVec;

use blaze_own::Alloc;
use blaze_typeck::fixture::Bb;
use blaze_typeck::{BinOp, CtorKind, Def, Expr, ExprId, InferCtx, ItemKey, Needs, Outcome, PNode, PTy, PortableTys, UintTy};

use super::super::{Tables, run_native_vt};
use super::{FxAdts, World, build, dyn_ty, ground_of, logged, world, LOG};
use crate::layout::{LayoutCx, Layouts};
use crate::{Built, Subst};

/// The three user structs' keys.
struct Nested {
    wrap: ItemKey,
    outer: ItemKey,
    two: ItemKey,
}

/// objects.rs's world with `Wrap`, `Outer` and `Two` and their impls (the module doc).
fn world_nested() -> (World, Nested) {
    let mut w = world();
    let box_k = w.boxes.box_k;
    let fx = &mut w.fx;
    let unsize = fx.key_of("Unsize");
    let coerce = fx.key_of("CoerceUnsized");
    let (p0, p1) = (fx.param(0), fx.param(1));
    let (u32_, u8_) = (fx.u32(), fx.u8());
    let box_p0 = fx.adt(box_k, &[p0]);
    let tuple = |adt| Def::Ctor { adt, variant: 0, kind: CtorKind::Tuple };

    let wrap = fx.tuple_strukt("Wrap", 1, &[u32_, box_p0]);
    fx.use_path("Wrap", tuple(wrap));
    let (wt, wu) = (fx.adt(wrap, &[p0]), fx.adt(wrap, &[p1]));
    let t_unsize_u = fx.bound(unsize, &[p0, p1]);
    let i = fx.imp("cu_wrap", 2, wt, Some((coerce, &[wu])), &[], std::vec![t_unsize_u]);
    fx.unsized_param(i, 0);
    fx.unsized_param(i, 1);

    let outer = fx.tuple_strukt("Outer", 1, &[u8_, p0]);
    fx.use_path("Outer", tuple(outer));
    let (ot, ou) = (fx.adt(outer, &[p0]), fx.adt(outer, &[p1]));
    let t_cu_u = fx.bound(coerce, &[p0, p1]);
    fx.imp("cu_outer", 2, ot, Some((coerce, &[ou])), &[], std::vec![t_cu_u]);

    let two = fx.tuple_strukt("Two", 1, &[box_p0, box_p0]);
    fx.use_path("Two", tuple(two));
    let (tt, tu) = (fx.adt(two, &[p0]), fx.adt(two, &[p1]));
    let t_unsize_u = fx.bound(unsize, &[p0, p1]);
    let i = fx.imp("cu_two", 2, tt, Some((coerce, &[tu])), &[], std::vec![t_unsize_u]);
    fx.unsized_param(i, 0);
    fx.unsized_param(i, 1);

    fx.finish();
    (w, Nested { wrap, outer, two })
}

/// `base.index`.
fn tfield(b: &mut Bb, base: ExprId, index: u32) -> ExprId {
    let at = b.int(None);
    b.exprs[at.ix()] = Expr::TupleField { base, index };
    at
}

/// `operand as u64`.
fn as_u64(b: &mut Bb, operand: ExprId) -> ExprId {
    let ty = b.ty(PNode::Uint(UintTy::U64));
    let at = b.int(None);
    b.exprs[at.ix()] = Expr::Cast { operand, ty };
    at
}

/// `lhs + rhs`.
fn add(b: &mut Bb, lhs: ExprId, rhs: ExprId) -> ExprId {
    let at = b.int(None);
    b.exprs[at.ix()] = Expr::Binary { op: BinOp::Add, lhs, rhs };
    at
}

/// `<maker>()`.
fn make(b: &mut Bb, maker: &str) -> ExprId {
    let f = b.path(&[maker]);
    b.call(f, &[])
}

/// `<ctor>(args..)`.
fn ctor(b: &mut Bb, name: &str, args: &[ExprId]) -> ExprId {
    let f = b.path(&[name]);
    b.call(f, args)
}

/// `adt<dyn Ob>` in the body's table.
fn adt_dyn(w: &World, b: &mut Bb, adt: ItemKey) -> PTy {
    dyn_ty(b, w.boxes.ob, |b, d| {
        let l = b.t.intern_list(&[d]).expect("list");
        b.ty(PNode::Adt(adt, l))
    })
}

/// `adt<inner>` of the ground table.
fn g_adt(g: &mut PortableTys<'static>, adt: ItemKey, inner: PTy) -> PTy {
    let l = g.intern_list(&[inner]).expect("the ground table is full");
    g.intern(PNode::Adt(adt, l)).expect("the ground table is full")
}

/// Checks body `b` of `owner` and builds it: the build's refusal as its text.
fn try_build(w: &World, b: &Bb, owner: ItemKey, root: ExprId, lits: &[(ExprId, u128)], ground: &mut PortableTys<'static>) -> Result<Built<Global>, String> {
    let view = b.view(owner, root);
    let mut ty = InferCtx::new_in(Alloc::global());
    let checked = match blaze_typeck::check(&view, &w.fx, &mut ty, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        Ok(Outcome::Need(n)) => panic!("the check of {owner:?} needed {:?}", n.questions),
        Err(error) => panic!("the check of {owner:?} failed: {error:?}"),
    };
    assert!(checked.diags.is_empty(), "diagnostics: {:?}", checked.diags);
    let sig = w.fx.sig_of(owner).expect("the owner's signature");
    let mut subst = Subst { args: &[], ground, scratch: Alloc::global() };
    crate::build(&view, &checked, &sig, &mut subst, &FxAdts { w, lits }, Global).map_err(|e| std::format!("{e}"))
}

/// objects.rs's `run_body` with the drop glue of the three structs at `A` and at `dyn Ob`
/// (and `Outer<Wrap<_>>` at both) beside its own.
fn run_nested(w: &World, n: &Nested, body: impl FnOnce(&mut Bb) -> (ExprId, StdVec<(ExprId, u128)>)) -> u64 {
    let mut ground = PortableTys::new_in(Alloc::global());
    let bx = &w.boxes;
    let full = "the ground table is full";
    let conc: StdVec<PTy> = bx.concrete.iter().map(|&(_, c)| ground_of(&w.fx, &mut ground, c)).collect();
    let ex = ground.intern(PNode::ExTrait(bx.ob, blaze_typeck::List::EMPTY)).expect(full);
    let dyn_ob = ground.dynamic(&[ex]).expect(full);
    let mut glue_tys: StdVec<PTy> = conc.clone();
    for &t in conc.iter().chain([dyn_ob].iter()) {
        glue_tys.push(g_adt(&mut ground, bx.box_k, t));
    }
    glue_tys.push(dyn_ob);
    for inner in [conc[0], dyn_ob] {
        let wrap = g_adt(&mut ground, n.wrap, inner);
        glue_tys.push(wrap);
        glue_tys.push(g_adt(&mut ground, n.outer, wrap));
        glue_tys.push(g_adt(&mut ground, n.two, inner));
    }
    let mut gb = Bb::new();
    gb.param("p");
    let groot = gb.block(&[], None);
    let view = gb.view(bx.dip, groot);
    let mut ty = InferCtx::new_in(Alloc::global());
    let checked = match blaze_typeck::check(&view, &w.fx, &mut ty, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        other => panic!("the check of `drop_in_place` did not finish: {:?}", other.err()),
    };
    let sig = w.fx.sig_of(bx.dip).expect("`drop_in_place`'s signature");
    let mut glues = StdVec::new();
    for &t in glue_tys.iter() {
        let args = ground.intern_list(&[t]).expect(full);
        let mut subst = Subst { args: &[t], ground: &mut ground, scratch: Alloc::global() };
        let built = crate::build_drop_glue(&view, &checked, &sig, &mut subst, &FxAdts { w, lits: &[] }, Global)
            .unwrap_or_else(|e| panic!("the glue at {t:?} failed: {e}"));
        super::super::emits(&built, &[]);
        glues.push((args, built));
    }
    let mut impl_built = StdVec::new();
    for &(_, _, f, v) in w.impls.iter() {
        let mut b = Bb::new();
        b.param("self");
        let e = b.int(None);
        let root = b.block(&[], Some(e));
        impl_built.push((f, build(w, &b, f, root, &[(e, v)], &mut ground)));
    }
    let mut b = Bb::new();
    let (root, lits) = body(&mut b);
    let main = try_build(w, &b, w.main, root, &lits, &mut ground).unwrap_or_else(|e| panic!("the build of `main` failed: {e}"));
    super::super::emits(&main, &[]);
    let mut fns: StdVec<(ItemKey, Option<blaze_typeck::List>, &Built<Global>)> = glues.iter().map(|(a, g)| (bx.dip, Some(*a), g)).collect();
    fns.extend(impl_built.iter().map(|(f, b)| (*f, None, b)));
    fns.push((w.main, None, &main));
    let impls: StdVec<(ItemKey, PTy, ItemKey)> = w.impls.iter().map(|&(m, c, f, _)| (m, ground_of(&w.fx, &mut ground, c), f)).collect();
    let (gu64, gu8) = (ground_of(&w.fx, &mut ground, w.u64_), ground_of(&w.fx, &mut ground, w.u8_));
    let d = Some(bx.dip);
    let sizes = [(gu64, 8, 8, None), (gu8, 1, 1, None), (conc[0], 8, 8, d), (conc[1], 8, 8, d), (conc[2], 0, 1, d)];
    let facts = |t| w.object_trait(t);
    let mut tables = Tables { facts: &facts, impls: &impls, sizes: &sizes, ground: &mut ground, placed: StdVec::new_in(Global) };
    LOG.with(|l| l.borrow_mut().clear());
    run_native_vt(&bx.host, &fns, Some(&mut tables))
}

/// `let y = Wrap(900, mk_a()); let x: Wrap<dyn Ob> = y; x.1.take() + x.0 as u64`.
///
/// The coercion moves the `u32` from its offset in `Wrap<A>` to its offset in `Wrap<dyn Ob>`
/// (asserted to differ: the builder's own layouts, `Box` first by alignment: 8 before, 16
/// after) and makes the `Box`'s two words: `take` is A's (21) through the new table, the tag is
/// intact (900): 921. The callee owns the box: A's `Drop` (1), then the free of its 8 bytes
/// (108); `main` drops nothing more (the `u32` left in `x` has no drop).
/// Prints `test tests::objects::nested_unsize::a_wrapped_box_unsizes_keeping_its_other_field ... ok`.
/// Fails without walk/unsize_nested.rs: the build of `main` is refused ("a `CoerceUnsized`
/// struct with a field beside its pointer that is not zero-sized"); a fix that copied the tag
/// at its source offset reads a vtable word's low half as the tag (not 921).
#[test]
fn a_wrapped_box_unsizes_keeping_its_other_field() {
    let (w, n) = world_nested();
    {
        let mut ground = PortableTys::new_in(Alloc::global());
        let a = ground_of(&w.fx, &mut ground, w.boxes.concrete[0].1);
        let ex = ground.intern(PNode::ExTrait(w.boxes.ob, blaze_typeck::List::EMPTY)).expect("full");
        let dyn_ob = ground.dynamic(&[ex]).expect("full");
        let (wa, wd) = (g_adt(&mut ground, n.wrap, a), g_adt(&mut ground, n.wrap, dyn_ob));
        let mut ls = Layouts::new_in(Global);
        let facts = FxAdts { w: &w, lits: &[] };
        let mut cx = LayoutCx { ground: &mut ground, facts: &facts, scratch: Alloc::global() };
        let la = ls.of(wa, &mut cx).expect("Wrap<A>'s layout");
        let ld = ls.of(wd, &mut cx).expect("Wrap<dyn Ob>'s layout");
        let (oa, od) = (ls.field_offset(la, 0), ls.field_offset(ld, 0));
        assert_ne!(oa, od, "the tag moves once the box is wide: a copy at the same offset would be wrong");
    }
    let got = run_nested(&w, &n, |b| {
        let y = b.bind("y");
        let tag = b.int(None);
        let boxed = make(b, "mk_a");
        let made = ctor(b, "Wrap", &[tag, boxed]);
        let s0 = b.let_(y, None, made);
        let x = b.bind("x");
        let xt = adt_dyn(&w, b, n.wrap);
        let ye = b.local(y);
        let s1 = b.let_(x, Some(xt), ye);
        let xe = b.local(x);
        let field1 = tfield(b, xe, 1);
        let took = b.method(field1, "take", &[]);
        let xe = b.local(x);
        let field0 = tfield(b, xe, 0);
        let wide = as_u64(b, field0);
        let sum = add(b, took, wide);
        (b.block(&[s0, s1], Some(sum)), std::vec![(tag, 900u128)])
    });
    assert_eq!(got, 921);
    assert_eq!(logged(), [1, 108]);
}

/// Two levels: `let y = Outer(7, Wrap(900, mk_a())); let x: Outer<Wrap<dyn Ob>> = y;
/// x.1.0 as u64 + x.0 as u64`: both other fields intact (907); at `main`'s end `x` is
/// dropped, its box through the table the coercion made: A's `Drop` (1) and the free (108).
/// Needs the checker's pin-unsize-1 (wiring/blaze-typeck/pin-unsize-1.md): the target's
/// argument `Wrap<dyn Ob>` reaches the `dyn` only through an ADT argument.
/// Prints `test tests::objects::nested_unsize::an_outer_struct_unsizes_through_two_levels ... ok`.
/// Fails without walk/unsize_nested.rs: `Outer`'s one differing argument is `Wrap<A>` /
/// `Wrap<dyn Ob>`, two ADTs, and `unsize_from` refused them ("an unsizing to other than a
/// slice or a trait object") after the size check refused `Outer` (16 before, 24 after).
#[test]
fn an_outer_struct_unsizes_through_two_levels() {
    let (w, n) = world_nested();
    let got = run_nested(&w, &n, |b| {
        let y = b.bind("y");
        let (small, tag) = (b.int(None), b.int(None));
        let boxed = make(b, "mk_a");
        let inner = ctor(b, "Wrap", &[tag, boxed]);
        let made = ctor(b, "Outer", &[small, inner]);
        let s0 = b.let_(y, None, made);
        let x = b.bind("x");
        let wd = adt_dyn(&w, b, n.wrap);
        let l = b.t.intern_list(&[wd]).expect("list");
        let xt = b.ty(PNode::Adt(n.outer, l));
        let ye = b.local(y);
        let s1 = b.let_(x, Some(xt), ye);
        let xe = b.local(x);
        let f1 = tfield(b, xe, 1);
        let f10 = tfield(b, f1, 0);
        let lhs = as_u64(b, f10);
        let xe = b.local(x);
        let f0 = tfield(b, xe, 0);
        let rhs = as_u64(b, f0);
        let sum = add(b, lhs, rhs);
        (b.block(&[s0, s1], Some(sum)), std::vec![(small, 7u128), (tag, 900u128)])
    });
    assert_eq!(got, 907);
    assert_eq!(logged(), [1, 108]);
}

/// A struct where two fields involve `T`: `let y = Two(mk_a(), mk_a()); let x: Two<dyn Ob> = y;`
/// is refused by the rule's name (the Reference: "T is not part of the type of any other
/// fields"; core: "only a single non-phantomdata field involving `T`"), never built by
/// coercing one box and copying the other's thin word.
/// Prints `test tests::objects::nested_unsize::two_differing_fields_are_refused_by_name ... ok`.
/// Fails without walk/unsize_nested.rs: the refusal is the old size check's text, not
/// "more than one field's type differs".
#[test]
fn two_differing_fields_are_refused_by_name() {
    let (w, n) = world_nested();
    let mut ground = PortableTys::new_in(Alloc::global());
    let mut b = Bb::new();
    let y = b.bind("y");
    // Both boxes of A: `Two(mk_a(), mk_b())` is a mismatch (`T` cannot be both A and B).
    let (ba, bb) = (make(&mut b, "mk_a"), make(&mut b, "mk_a"));
    let made = ctor(&mut b, "Two", &[ba, bb]);
    let s0 = b.let_(y, None, made);
    let x = b.bind("x");
    let xt = adt_dyn(&w, &mut b, n.two);
    let ye = b.local(y);
    let s1 = b.let_(x, Some(xt), ye);
    let five = b.int(None);
    let root = b.block(&[s0, s1], Some(five));
    let refused = match try_build(&w, &b, w.main, root, &[(five, 5)], &mut ground) {
        Ok(_) => panic!("a coercion of two fields involving `T` was built"),
        Err(text) => text,
    };
    assert!(refused.contains("more than one field's type differs"), "{refused}");
    assert!(refused.contains("Unsized coercions"), "{refused}");
}
