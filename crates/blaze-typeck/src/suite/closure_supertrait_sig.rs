//! Closure signature deduction through supertraits of pending trait goals and trait-object
//! principals (`rules/closure.rs`: `infer_closure_hint`, `elaborate_fn_supertraits`,
//! `object_closure_sig`).
//!
//! When a closure's expected type is an inference variable `?F` with a pending bound
//! `?F: SubTrait` (or a trait object `dyn SubTrait`) where `SubTrait` has a transitive `Fn*`
//! supertrait (possibly generic, possibly with a `<Self as FnOnce<Args>>::Output = R` projection),
//! the closure takes its expected inputs, output, and kind from the elaborated supertrait.
//!
//! No test here invokes a compiler; each expectation follows the Rust Reference ("Closure
//! expressions", "Supertraits", "Unsized coercions").

use std::vec::Vec;

use super::check_body;
use crate::body::IntSuffix;
use crate::columns::{Checked, SigFrom, SupertraitOrigin};
use crate::diag::DiagCode;
use crate::error::{CheckError, Limit};
use crate::fixture::{Bb, Fx};
use crate::ids::{Abi, FnPtrHeader, ItemKey, LangItem, Mutbl, Safety, UintTy};
use crate::infer::{InferCtx, InferPlant};
use crate::machine::Plant;
use crate::portable::PNode;
use crate::world::Needs;
use crate::{Outcome, check_with};

struct World {
    readiness_ok: ItemKey,
    readiness_bad_ret: ItemKey,
    readiness_bad_arity: ItemKey,
    multihop_param: ItemKey,
    generic_mapper: ItemKey,
    dyn_subtrait: ItemKey,
    cyclic_ok: ItemKey,
    mixed_direct_and_super_out: ItemKey,
    has_out_trait: ItemKey,
    readiness_trait: ItemKey,
}

fn world() -> (Fx, World) {
    let mut world = Fx::new();
    let fx = &mut world;
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let unsize = fx.tr("Unsize", 2, Vec::new(), &[]);
    fx.lang(LangItem::Unsize, unsize);
    let coerce = fx.tr("CoerceUnsized", 2, Vec::new(), &[]);
    fx.lang(LangItem::CoerceUnsized, coerce);
    let send = fx.tr("Send", 1, Vec::new(), &[]);
    fx.auto(send);
    let sync = fx.tr("Sync", 1, Vec::new(), &[]);
    fx.auto(sync);
    let (p0, p1, p2) = (fx.param(0), fx.param(1), fx.param(2));
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

    let (u8_, u32_, usize_, unit) = (
        fx.ty(PNode::Uint(UintTy::U8)),
        fx.ty(PNode::Uint(UintTy::U32)),
        fx.ty(PNode::Uint(UintTy::Usize)),
        fx.unit(),
    );

    // `struct Box<T: ?Sized>; impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<Box<U>> for Box<T>`.
    let box_k = fx.strukt("Box", 1, &[]);
    let (box_t, box_u) = (fx.adt(box_k, &[p0]), fx.adt(box_k, &[p1]));
    let t_unsize_u = fx.bound(unsize, &[p0, p1]);
    let cu = fx.imp("impl CoerceUnsized<Box<U>> for Box<T>", 2, box_t, Some((coerce, &[box_u])), &[], std::vec![t_unsize_u]);
    fx.unsized_param(cu, 0);
    fx.unsized_param(cu, 1);
    fx.func("box_new", 1, &[p0], box_t, Vec::new());

    // `trait DynVal {}  struct Concrete(u32);  impl DynVal for Concrete {}`
    let dyn_val = fx.tr("DynVal", 1, Vec::new(), &[]);
    let concrete = fx.strukt("Concrete", 0, &[("0", u32_)]);
    let concrete_ty = fx.adt(concrete, &[]);
    fx.imp("impl DynVal for Concrete", 0, concrete_ty, Some((dyn_val, &[])), &[], Vec::new());
    fx.func("make_concrete", 0, &[u32_], concrete_ty, Vec::new());

    let empty_l = fx.t.intern_list(&[]).expect("list");
    let ex_dyn_val = fx.ty(PNode::ExTrait(dyn_val, empty_l));
    let dyn_val_ty = fx.t.dynamic(&[ex_dyn_val]).expect("dyn");
    let box_dyn_val = fx.adt(box_k, &[dyn_val_ty]);

    // `trait ReadinessCheck: Fn() -> Box<dyn DynVal> + Send + Sync {}`
    // `impl<F: Fn() -> Box<dyn DynVal> + Send + Sync> ReadinessCheck for F {}`
    let empty_tup = fx.tuple(&[]);
    let rc_fn = fx.bound(fn_, &[p0, empty_tup]);
    let rc_out = fx.proj(fn_once, &[p0, empty_tup], "Output", box_dyn_val);
    let rc_send = fx.bound(send, &[p0]);
    let rc_sync = fx.bound(sync, &[p0]);
    let readiness = fx.tr("ReadinessCheck", 1, std::vec![rc_fn, rc_out, rc_send, rc_sync], &[]);
    fx.imp(
        "impl<F> ReadinessCheck for F",
        1,
        p0,
        Some((readiness, &[])),
        &[],
        std::vec![rc_fn, rc_out, rc_send, rc_sync],
    );
    let b_rc = fx.bound(readiness, &[p0]);
    fx.func("spawn_ready", 1, &[p0], unit, std::vec![b_rc]);

    // `struct Text; impl Text { fn len(&self) -> usize; }`
    let text_k = fx.strukt("Text", 0, &[]);
    let text_ty = fx.adt(text_k, &[]);
    let r_text = fx.r(Mutbl::Not, text_ty);
    let text_imp = fx.imp("impl Text", 0, text_ty, None, &[], Vec::new());
    fx.impl_fn(text_imp, "len", true, &[r_text], usize_);

    // Multi-hop generic supertraits:
    // `trait Inner<T>: Fn(T) -> usize {}`
    // `trait Outer<T>: Inner<T> {}`
    let p1_tup = fx.tuple(&[p1]);
    let inner_fn = fx.bound(fn_, &[p0, p1_tup]);
    let inner_out = fx.proj(fn_once, &[p0, p1_tup], "Output", usize_);
    let inner = fx.tr("Inner", 2, std::vec![inner_fn, inner_out], &[]);
    fx.imp("impl<F, T> Inner<T> for F", 2, p0, Some((inner, &[p1])), &[], std::vec![inner_fn, inner_out]);
    let outer_sup = fx.bound(inner, &[p0, p1]);
    let outer = fx.tr("Outer", 2, std::vec![outer_sup], &[]);
    fx.imp("impl<F, T> Outer<T> for F", 2, p0, Some((outer, &[p1])), &[], std::vec![outer_sup]);
    let b_outer_text = fx.bound(outer, &[p0, text_ty]);
    fx.func("take_outer_text", 1, &[p0], unit, std::vec![b_outer_text]);

    // Generic mapper with both arg and return substituted:
    // `trait Mapper<A, B>: Fn(A) -> B {}`
    let map_fn = fx.bound(fn_, &[p0, p1_tup]);
    let map_out = fx.proj(fn_once, &[p0, p1_tup], "Output", p2);
    let mapper = fx.tr("Mapper", 3, std::vec![map_fn, map_out], &[]);
    fx.imp("impl<F, A, B> Mapper<A, B> for F", 3, p0, Some((mapper, &[p1, p2])), &[], std::vec![map_fn, map_out]);
    let b_mapper = fx.bound(mapper, &[p0, u32_, box_dyn_val]);
    fx.func("take_mapper", 1, &[p0], unit, std::vec![b_mapper]);

    // `trait SubTrait: Fn(u32) -> Box<dyn DynVal> {}` + `fn take_dyn_sub(f: Box<dyn SubTrait>)`
    let u32_tup = fx.tuple(&[u32_]);
    let sub_fn = fx.bound(fn_, &[p0, u32_tup]);
    let sub_out = fx.proj(fn_once, &[p0, u32_tup], "Output", box_dyn_val);
    let sub_trait = fx.tr("SubTrait", 1, std::vec![sub_fn, sub_out], &[]);
    fx.imp("impl<F> SubTrait for F", 1, p0, Some((sub_trait, &[])), &[], std::vec![sub_fn, sub_out]);
    let ex_sub = fx.ty(PNode::ExTrait(sub_trait, empty_l));
    let dyn_sub = fx.t.dynamic(&[ex_sub]).expect("dyn");
    let box_dyn_sub = fx.adt(box_k, &[dyn_sub]);
    fx.func("take_dyn_sub", 0, &[box_dyn_sub], unit, Vec::new());

    // Cyclic supertraits must terminate without looping:
    // `trait CycA: CycB {}  trait CycB: CycA {}`
    let cyc_a = fx.tr("CycA", 1, Vec::new(), &[]);
    let b_cyc_a = fx.bound(cyc_a, &[p0]);
    let cyc_b = fx.tr("CycB", 1, std::vec![b_cyc_a], &[]);
    let b_cyc_b = fx.bound(cyc_b, &[p0]);
    fx.set_supers(cyc_a, std::vec![b_cyc_b]);
    fx.imp("impl<F> CycA for F", 1, p0, Some((cyc_a, &[])), &[], Vec::new());
    fx.imp("impl<F> CycB for F", 1, p0, Some((cyc_b, &[])), &[], Vec::new());
    let u8_tup = fx.tuple(&[u8_]);
    let b_fn_u8 = fx.bound(fn_, &[p0, u8_tup]);
    let b_fn_u8_out = fx.proj(fn_once, &[p0, u8_tup], "Output", u8_);
    fx.func("take_cyclic_and_fn", 1, &[p0], unit, std::vec![b_cyc_a, b_fn_u8, b_fn_u8_out]);
    fx.func("take_cyclic_only", 1, &[p0], unit, std::vec![b_cyc_a]);

    let readiness_ok = fx.func("readiness_ok", 0, &[], unit, Vec::new());
    let readiness_bad_ret = fx.func("readiness_bad_ret", 0, &[], unit, Vec::new());
    let readiness_bad_arity = fx.func("readiness_bad_arity", 0, &[], unit, Vec::new());
    let multihop_param = fx.func("multihop_param", 0, &[], unit, Vec::new());
    let generic_mapper = fx.func("generic_mapper", 0, &[], unit, Vec::new());
    let dyn_subtrait = fx.func("dyn_subtrait", 0, &[], unit, Vec::new());
    let cyclic_ok = fx.func("cyclic_ok", 0, &[], unit, Vec::new());

    // Mixed direct `Fn<()>` input bound + separate supertrait `HasOut` supplying `Output = Box<dyn DynVal>`:
    let has_out_trait = fx.tr("HasOut", 1, std::vec![rc_out], &[]);
    fx.imp("impl<F> HasOut for F", 1, p0, Some((has_out_trait, &[])), &[], std::vec![rc_out]);
    let b_has_out = fx.bound(has_out_trait, &[p0]);
    fx.func("take_mixed_out", 1, &[p0], unit, std::vec![rc_fn, b_has_out]);
    let mixed_direct_and_super_out = fx.func("mixed_direct_and_super_out", 0, &[], unit, Vec::new());

    fx.finish();
    (
        world,
        World {
            readiness_ok,
            readiness_bad_ret,
            readiness_bad_arity,
            multihop_param,
            generic_mapper,
            dyn_subtrait,
            cyclic_ok,
            mixed_direct_and_super_out,
            has_out_trait,
            readiness_trait: readiness,
        },
    )
}

fn codes(c: &Checked<'_>) -> Vec<DiagCode> {
    c.diags.iter().map(|d| d.code).collect()
}

/// `spawn_ready(|| box_new(make_concrete(1)))`: supertrait `Fn() -> Box<dyn DynVal>` gives the
/// closure its return type so `Box<Concrete>` unsizes to `Box<dyn DynVal>`.
#[test]
fn supertrait_return_coerces_boxed_concrete_to_dyn() {
    let (fx, w) = world();
    let mut b = Bb::new();
    let one = b.int(Some(IntSuffix::Uint(UintTy::U32)));
    let mk = b.path(&["make_concrete"]);
    let concrete = b.call(mk, &[one]);
    let bn = b.path(&["box_new"]);
    let boxed = b.call(bn, &[concrete]);
    let cl = b.closure(&[], boxed);
    let spawn = b.path(&["spawn_ready"]);
    let call = b.call(spawn, &[cl]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, w.readiness_ok, root, Plant::None, InferPlant::None);
    assert!(codes(&c).is_empty(), "expected clean check, got {:?}", c.diags);
    assert!(
        matches!(
            c.closures.column::<4>().first().copied(),
            Some(SigFrom::Supertrait {
                origin: SupertraitOrigin::Clause { nth: 0, .. },
                trait_,
                nth: 0,
                output: Some((out_t, 1)),
            }) if trait_ == w.readiness_trait && out_t == w.readiness_trait
        ),
        "expected SigFrom::Supertrait with Clause origin, got {:?}",
        c.closures.column::<4>()
    );
}

/// Mixed direct `Fn<()>` input clause + supertrait `HasOut` output projection: preserves
/// `output_trait = Some(HasOut)` and `output = Some(0)` on `SigFrom::Clause` (and round-trips).
#[test]
fn mixed_direct_fn_input_and_supertrait_output_preserves_output_trait_provenance() {
    let (fx, w) = world();
    let mut b = Bb::new();
    let one = b.int(Some(IntSuffix::Uint(UintTy::U32)));
    let mk = b.path(&["make_concrete"]);
    let concrete = b.call(mk, &[one]);
    let bn = b.path(&["box_new"]);
    let boxed = b.call(bn, &[concrete]);
    let cl = b.closure(&[], boxed);
    let take = b.path(&["take_mixed_out"]);
    let call = b.call(take, &[cl]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, w.mixed_direct_and_super_out, root, Plant::None, InferPlant::None);
    assert!(codes(&c).is_empty(), "expected clean check, got {:?}", c.diags);
    assert!(
        matches!(
            c.closures.column::<4>().first().copied(),
            Some(SigFrom::Clause {
                nth: 0,
                output: Some(0),
                output_trait: Some(ot),
                ..
            }) if ot == w.has_out_trait
        ),
        "expected SigFrom::Clause with output_trait = Some(HasOut), got {:?}",
        c.closures.column::<4>()
    );
}

/// Negative control: `spawn_ready(|| 1u32)` fails with E0308 against the elaborated return type.
#[test]
fn supertrait_return_wrong_type_is_mismatch() {
    let (fx, w) = world();
    let mut b = Bb::new();
    let bad = b.int(Some(IntSuffix::Uint(UintTy::U32)));
    let cl = b.closure(&[], bad);
    let spawn = b.path(&["spawn_ready"]);
    let call = b.call(spawn, &[cl]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, w.readiness_bad_ret, root, Plant::None, InferPlant::None);
    let got = codes(&c);
    assert!(got.contains(&DiagCode::Mismatch), "expected E0308 Mismatch, got {got:?}");
}

/// Negative control: `spawn_ready(|_, _| box_new(make_concrete(1)))` fails with a trait/arity error.
#[test]
fn supertrait_wrong_closure_arity_is_rejected() {
    let (fx, w) = world();
    let mut b = Bb::new();
    let p0 = b.bind("a");
    let p1 = b.bind("b");
    let one = b.int(Some(IntSuffix::Uint(UintTy::U32)));
    let mk = b.path(&["make_concrete"]);
    let concrete = b.call(mk, &[one]);
    let bn = b.path(&["box_new"]);
    let boxed = b.call(bn, &[concrete]);
    let cl = b.closure(&[p0, p1], boxed);
    let spawn = b.path(&["spawn_ready"]);
    let call = b.call(spawn, &[cl]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, w.readiness_bad_arity, root, Plant::None, InferPlant::None);
    let got = codes(&c);
    assert!(!got.is_empty(), "expected error for wrong closure arity, got clean");
}

/// `take_outer_text(|s| s.len())`: multi-hop generic `Outer<Text>: Inner<Text>: Fn(Text) -> usize`
/// deduces unannotated `s: Text` so `s.len()` resolves.
#[test]
fn multihop_generic_supertrait_deduces_closure_param() {
    let (fx, w) = world();
    let mut b = Bb::new();
    let s = b.bind("s");
    let sl = b.local(s);
    let len = b.method(sl, "len", &[]);
    let cl = b.closure(&[s], len);
    let take = b.path(&["take_outer_text"]);
    let call = b.call(take, &[cl]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, w.multihop_param, root, Plant::None, InferPlant::None);
    assert!(codes(&c).is_empty(), "expected clean check, got {:?}", c.diags);
}

/// `take_mapper(|x| box_new(make_concrete(x)))`: generic `Mapper<u32, Box<dyn DynVal>>`
/// substitutes both `A := u32` and `B := Box<dyn DynVal>`.
#[test]
fn generic_mapper_supertrait_substitutes_input_and_output() {
    let (fx, w) = world();
    let mut b = Bb::new();
    let x = b.bind("x");
    let xl = b.local(x);
    let mk = b.path(&["make_concrete"]);
    let concrete = b.call(mk, &[xl]);
    let bn = b.path(&["box_new"]);
    let boxed = b.call(bn, &[concrete]);
    let cl = b.closure(&[x], boxed);
    let take = b.path(&["take_mapper"]);
    let call = b.call(take, &[cl]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, w.generic_mapper, root, Plant::None, InferPlant::None);
    assert!(codes(&c).is_empty(), "expected clean check, got {:?}", c.diags);
}

/// `take_dyn_sub(box_new(|x| box_new(make_concrete(x))))`: `dyn SubTrait` elaborates its
/// supertrait `Fn(u32) -> Box<dyn DynVal>` in `object_closure_sig`.
#[test]
fn dyn_subtrait_elaborates_fn_supertrait_for_closure() {
    let (fx, w) = world();
    let mut b = Bb::new();
    let x = b.bind("x");
    let xl = b.local(x);
    let mk = b.path(&["make_concrete"]);
    let concrete = b.call(mk, &[xl]);
    let bn1 = b.path(&["box_new"]);
    let inner_box = b.call(bn1, &[concrete]);
    let cl = b.closure(&[x], inner_box);
    let bn2 = b.path(&["box_new"]);
    let outer_box = b.call(bn2, &[cl]);
    let take = b.path(&["take_dyn_sub"]);
    let call = b.call(take, &[outer_box]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, w.dyn_subtrait, root, Plant::None, InferPlant::None);
    assert!(codes(&c).is_empty(), "expected clean check, got {:?}", c.diags);
}

/// Cyclic supertraits (`CycA: CycB`, `CycB: CycA`) terminate cleanly without looping.
#[test]
fn cyclic_supertraits_terminate_cleanly() {
    let (fx, w) = world();
    let mut b = Bb::new();
    let x = b.bind("x");
    let xl = b.local(x);
    let cl = b.closure(&[x], xl);
    let take = b.path(&["take_cyclic_only"]);
    let call = b.call(take, &[cl]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, w.cyclic_ok, root, Plant::None, InferPlant::None);
    // Unannotated param `x` has no `Fn` supertrait in the cycle, so E0282 is expected, never a hang.
    assert!(!codes(&c).is_empty(), "expected E0282 for unannotated closure param with no Fn supertrait");
}

/// A supertrait chain exceeding `MAX_SUPERTRAITS` (64) returns `CheckError::Limit(Limit::Supertraits)`
/// rather than silently dropping callable bounds.
#[test]
fn supertrait_overflow_returns_limit_supertraits() {
    let mut world = Fx::new();
    let fx = &mut world;
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
    let sup = fx.bound(fn_mut, &[p0, p1]);
    let fn_ = fx.tr("Fn", 2, std::vec![sup], &[]);
    fx.lang(LangItem::Fn, fn_);

    let (u32_, unit) = (fx.ty(PNode::Uint(UintTy::U32)), fx.unit());
    let empty_tup = fx.tuple(&[]);
    let fn_bound = fx.bound(fn_, &[p0, empty_tup]);
    let fn_out = fx.proj(fn_once, &[p0, empty_tup], "Output", u32_);
    // Build a chain of 66 traits: T65 -> T64 -> ... -> T0 -> Fn() -> u32.
    let mut prev = fx.tr("Chain0", 1, std::vec![fn_bound, fn_out], &[]);
    for i in 1..66u32 {
        let name = std::format!("Chain{i}");
        let b = fx.bound(prev, &[p0]);
        prev = fx.tr(&name, 1, std::vec![b], &[]);
    }
    let top_bound = fx.bound(prev, &[p0]);
    fx.func("take_deep", 1, &[p0], unit, std::vec![top_bound]);
    let caller = fx.func("caller", 0, &[], unit, Vec::new());
    fx.finish();

    let mut b = Bb::new();
    let val = b.int(Some(IntSuffix::Uint(UintTy::U32)));
    let cl = b.closure(&[], val);
    let take = b.path(&["take_deep"]);
    let call = b.call(take, &[cl]);
    let root = b.block(&[], Some(call));

    let mut ty = InferCtx::new_in(blaze_own::Alloc::global());
    let res = check_with(
        &b.view(caller, root),
        &world,
        &mut ty,
        &Needs::new_in(blaze_own::Alloc::global()),
        Plant::None,
        blaze_own::Alloc::global(),
    );
    match res {
        Err(CheckError::Limit(Limit::Supertraits)) => {}
        Err(other) => panic!("expected CheckError::Limit(Limit::Supertraits), got Err({other:?})"),
        Ok(Outcome::Done(_)) => panic!("expected CheckError::Limit(Limit::Supertraits), got Ok(Done)"),
        Ok(Outcome::Need(n)) => panic!("expected CheckError::Limit(Limit::Supertraits), got Ok(Need({:?}))", n.questions),
    }
}
