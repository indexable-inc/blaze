//! The body suite: bodies that SELECT impls, checked by the real context (`InferCtx`: blaze-ty's
//! arena and blaze-solve), each with the expected type of every expression and pattern, or the
//! expected diagnostic, written out with the rule it follows. Nothing runs rustc for an answer
//! (rule 7): each expectation is derived from the language's rules as the pinned compiler's
//! method probe (rustc_hir_typeck method/probe.rs, confirm.rs) and next solver
//! (vendor/rustc_next_trait_solver) implement them, and says which function.
//!
//! The world is one program (`world()`, its source in the comment there); each case is one fn
//! of it whose body is built node by node.
//!
//! Planted bugs, each caught by a named control (testing skill, `blaze_sim::control` style):
//! - `Plant::AutoderefShort` (autoderef stops one step early): `control_autoderef_short`;
//! - `Plant::IgnoreWhereClauses` (the owner's where-clauses never reach the param env):
//!   `control_where_clause_ignored`;
//! - `InferPlant::FirstImpl` (the solver sees only the first impl of a bucket, so it takes that
//!   one instead of selecting): `control_first_impl_taken`;
//! - `InferPlant::AmbiguityByOrder` (an obligation still ambiguous at the end is settled by the
//!   first impl whose header unifies): `control_ambiguity_by_order`.

use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_own::Alloc;

use crate::columns::{AdjustKind, Checked};
use crate::diag::DiagCode;
use crate::body::IntSuffix;
use crate::ids::{ExprId, ItemKey, LangItem, Mutbl, Name, PatId, UintTy};
use crate::infer::{InferCtx, InferPlant};
use crate::machine::Plant;
use crate::portable::PNode;
use crate::world::{Needs, PClause};
use crate::{Outcome, check_with};
use crate::fixture::{Bb, Fx, show};

mod anon_params;
mod assoc_bound_supers;
mod assoc_item_bounds;
mod async_closures;
mod async_fmt;
mod await_impl;
mod checked_record;
mod closure_identity;
mod closures;
mod const_args;
mod const_enum;
mod const_path_args;
mod const_terms;
mod coroutine;
mod coroutine_id;
mod deferred_place;
mod discriminant;
mod dyn_objects;
mod expect_inputs;
mod expect_fudge;
mod fn_ptr;
mod fn_item_lub;
mod fn_lub_earlier;
mod fn_lub_gates;
mod fn_traits;
mod gat;
mod holes;
mod impl_const_params;
mod instance_holes;
mod infer_const_arg;
mod legacy_const;
mod lower_forms;
mod method_object_step;
mod handler_blanket;
mod closure_try_from;
mod mut_places;
mod neg_unsigned;
mod op_callees;
mod op_fast_path;
mod op_literal_vars;
mod obligation_range;
mod opaque;
mod qpath_defaults;
mod opaque_supertraits;
mod opaque_nested;
mod opaque_hidden_closure;
/// A self-recursive boxed future revealed one layer at an instance (`TypingMode::PostAnalysis`).
mod opaque_post_analysis_recursive;
/// E0425 names the segment that failed (rules/lit_path.rs `path_miss`).
mod path_miss;
mod pat_editions;
/// An impl parameter named only by a where-clause projection (reth's `DebugNodeLauncher`).
mod impl_param_by_projection;
/// The body's own signature's projections as rows at its root (design B).
mod sig_rows;
/// A callee's trait bound as a row at the call whatever its trait names, and a selected
/// impl's where-clauses as further rows there.
mod bound_rows;
mod pattern_types;
mod pin_unsize;
mod pin_unsize_interior;
mod projection_unsize;
mod struct_tail_unsize;
mod self_impl_args;
mod self_paths;
mod self_ty_paths;
mod type_alias_paths;
mod trait_relative_value;
mod inherent_impl_clauses;
mod unsized_arg_hint;
mod deref_open_target;
mod op_peeled_ref;
mod panic_hook_closure;
mod why_maybe;
mod suggest;
mod where_const;
mod where_projection;
mod env_shadow_projection;
mod promote;
mod impl_for_object;
mod blanket_beside_own_impl;
mod coerce_target_obligations;
mod env_supertrait_assoc_bound;
mod env_where_projection_rhs;
mod assoc_ty_variant;
mod self_fn_impl_args;
mod specialize;
mod dyn_closure_sig;
mod diag_node;
mod undecided_origin;
mod fn_ptr_fn_traits;

/// The keys of the world's items the cases name.
struct Items {
    deref: ItemKey,
    foo: ItemKey,
    a: ItemKey,
    b: ItemKey,
    of: ItemKey,
    apply: ItemKey,
    kind_fn: ItemKey,
    kind_for_a: ItemKey,
    kind_blanket: ItemKey,
    kind_a_fn: ItemKey,
    kind_blanket_fn: ItemKey,
    owners: Vec<ItemKey>,
}

/// The program every case is a fn of:
///
/// ```text
/// #[lang = "sized"] trait Sized {}
/// #[lang = "deref"] trait Deref { type Target; }
/// #[lang = "fn_once"] trait FnOnce<Args> { type Output; }
/// #[lang = "fn_mut"] trait FnMut<Args>: FnOnce<Args> {}
/// #[lang = "fn"] trait Fn<Args>: FnMut<Args> {}
/// struct Foo { n: u32 }  struct Wrap { inner: Foo }  struct A {}  struct B {}
/// impl Foo { fn get(&self) -> u32; fn bump(&mut self) -> u32; fn name(&self) -> u8; }
/// impl Deref for Wrap { type Target = Foo; }
/// trait Named { fn name(&self) -> u16; }          impl Named for Foo {}
/// trait Size { fn size(&self) -> usize; }         impl Size for Foo {}
/// fn of<T: Size>(t: &T) -> usize;
/// trait Make { type Out; fn make(&self) -> Self::Out; }  impl Make for A { type Out = Foo; }
/// trait Mix<R> { type Out; fn mix(&self, r: R) -> Self::Out; }
/// impl Mix<u8> for A { type Out = u16; }  impl Mix<bool> for A { type Out = char; }
/// trait Kind { type K; fn kind(&self) -> Self::K; }
/// impl Kind for A { type K = u16; }  impl<T: Size> Kind for T { type K = u8; }
/// trait Conv<T> { fn conv(&self) -> T; }  impl Conv<u8> for A {}  impl Conv<u16> for A {}
/// trait Base { fn base(&self) -> u32; }  trait Sub: Base {}
/// fn apply<F: Fn(u8) -> u16>(f: F) -> u16;
/// use {Named, Size, Make, Mix, Kind, Conv};   // in scope; Base and Sub are not
/// ```
fn world() -> (Fx, Items) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let deref = fx.tr("Deref", 1, Vec::new(), &["Target"]);
    fx.lang(LangItem::Deref, deref);
    let fn_once = fx.tr("FnOnce", 2, Vec::new(), &["Output"]);
    fx.lang(LangItem::FnOnce, fn_once);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let sup = fx.bound(fn_once, &[p0, p1]);
    let fn_mut = fx.tr("FnMut", 2, std::vec![sup], &[]);
    fx.lang(LangItem::FnMut, fn_mut);
    let sup = fx.bound(fn_mut, &[p0, p1]);
    let fn_ = fx.tr("Fn", 2, std::vec![sup], &[]);
    fx.lang(LangItem::Fn, fn_);

    let (u8_, u16_, u32_, usize_) = (fx.u8(), fx.u16(), fx.u32(), fx.usize());
    let bool_ = fx.ty(PNode::Bool);
    let char_ = fx.ty(PNode::Char);
    let foo = fx.strukt("Foo", 0, &[("n", u32_)]);
    let foo_t = fx.adt(foo, &[]);
    let wrap = fx.strukt("Wrap", 0, &[("inner", foo_t)]);
    let wrap_t = fx.adt(wrap, &[]);
    let a = fx.strukt("A", 0, &[]);
    let a_t = fx.adt(a, &[]);
    let b = fx.strukt("B", 0, &[]);
    let b_t = fx.adt(b, &[]);
    let r_self = fx.r(Mutbl::Not, p0);

    let foo_impl = fx.imp("impl Foo", 0, foo_t, None, &[], Vec::new());
    let r_foo = fx.r(Mutbl::Not, foo_t);
    let m_foo = fx.r(Mutbl::Mut, foo_t);
    fx.impl_fn(foo_impl, "get", true, &[r_foo], u32_);
    fx.impl_fn(foo_impl, "bump", true, &[m_foo], u32_);
    fx.impl_fn(foo_impl, "name", true, &[r_foo], u8_);
    fx.imp("impl Deref for Wrap", 0, wrap_t, Some((deref, &[])), &[("Target", foo_t)], Vec::new());

    let named = fx.tr("Named", 1, Vec::new(), &[]);
    fx.trait_fn(named, "name", true, &[r_self], u16_);
    fx.imp("impl Named for Foo", 0, foo_t, Some((named, &[])), &[], Vec::new());
    let size = fx.tr("Size", 1, Vec::new(), &[]);
    fx.trait_fn(size, "size", true, &[r_self], usize_);
    fx.imp("impl Size for Foo", 0, foo_t, Some((size, &[])), &[], Vec::new());
    let size_t = fx.bound(size, &[p0]);
    let of = fx.func("of", 1, &[r_self], usize_, std::vec![size_t]);

    let make = fx.tr("Make", 1, Vec::new(), &["Out"]);
    let out = fx.alias(make, "Out", &[p0]);
    fx.trait_fn(make, "make", true, &[r_self], out);
    fx.imp("impl Make for A", 0, a_t, Some((make, &[])), &[("Out", foo_t)], Vec::new());

    let mix = fx.tr("Mix", 2, Vec::new(), &["Out"]);
    let out = fx.alias(mix, "Out", &[p0, p1]);
    fx.trait_fn(mix, "mix", true, &[r_self, p1], out);
    fx.imp("impl Mix<u8> for A", 0, a_t, Some((mix, &[u8_])), &[("Out", u16_)], Vec::new());
    fx.imp("impl Mix<bool> for A", 0, a_t, Some((mix, &[bool_])), &[("Out", char_)], Vec::new());

    let kind = fx.tr("Kind", 1, Vec::new(), &["K"]);
    let k = fx.alias(kind, "K", &[p0]);
    let kind_fn = fx.trait_fn(kind, "kind", true, &[r_self], k);
    let kind_for_a = fx.imp("impl Kind for A", 0, a_t, Some((kind, &[])), &[("K", u16_)], Vec::new());
    let r_a = fx.r(Mutbl::Not, a_t);
    let kind_a_fn = fx.impl_fn(kind_for_a, "kind", true, &[r_a], u16_);
    let kind_blanket = fx.imp("impl<T: Size> Kind for T", 1, p0, Some((kind, &[])), &[("K", u8_)], std::vec![size_t]);
    let kind_blanket_fn = fx.impl_fn(kind_blanket, "kind", true, &[r_self], u8_);

    let conv = fx.tr("Conv", 2, Vec::new(), &[]);
    fx.trait_fn(conv, "conv", true, &[r_self], p1);
    fx.imp("impl Conv<u8> for A", 0, a_t, Some((conv, &[u8_])), &[], Vec::new());
    fx.imp("impl Conv<u16> for A", 0, a_t, Some((conv, &[u16_])), &[], Vec::new());

    let base = fx.tr("Base", 1, Vec::new(), &[]);
    fx.trait_fn(base, "base", true, &[r_self], u32_);
    let sup = fx.bound(base, &[p0]);
    let sub = fx.tr("Sub", 1, std::vec![sup], &[]);

    let args = fx.tuple(&[u8_]);
    let f_fn = fx.bound(fn_, &[p0, args]);
    let f_out = fx.proj(fn_once, &[p0, args], "Output", u16_);
    let apply = fx.func("apply", 1, &[p0], u16_, std::vec![f_fn, f_out]);
    // `fn made<T>(t: T) -> <T as Make>::Out`: no bound, so the projection is proved at the call.
    let make_out = fx.alias(make, "Out", &[p0]);
    fx.func("made", 1, &[p0], make_out, Vec::new());
    // `impl u32 { const MAX: u32; fn pow(self, e: u32) -> u32; }`, as core writes it, and the
    // path `u32` resolving to the built-in type.
    let impl_u32 = fx.imp("impl u32", 0, u32_, None, &[], Vec::new());
    fx.impl_const(impl_u32, "MAX", u32_);
    fx.impl_fn(impl_u32, "pow", true, &[u32_, u32_], u32_);
    fx.use_path("u32", crate::world::Def::Prim(crate::world::Prim::Uint(UintTy::U32)));

    for t in [named, size, make, mix, kind, conv] {
        fx.use_trait(t);
    }

    // The cases' owners, in case order (see each case).
    let unit = fx.unit();
    let u64_ = fx.ty(PNode::Uint(UintTy::U64));
    let rr_foo = fx.r(Mutbl::Not, r_foo);
    let r_wrap = fx.r(Mutbl::Not, wrap_t);
    let sub_t = fx.bound(sub, &[p0]);
    let owners = std::vec![
        fx.func("c_autoderef", 0, &[rr_foo, m_foo, r_wrap], u32_, Vec::new()),
        fx.func("c_inherent_first", 0, &[foo_t], usize_, Vec::new()),
        fx.func("c_where_clause", 1, &[r_self], usize_, std::vec![size_t]),
        fx.func("c_projection", 0, &[a_t], u32_, Vec::new()),
        fx.func("c_narrowed", 0, &[a_t], char_, Vec::new()),
        fx.func("c_blanket", 0, &[a_t, foo_t], u8_, Vec::new()),
        fx.func("c_ambiguous_then_fixed", 0, &[a_t], u16_, Vec::new()),
        fx.func("c_ambiguous", 0, &[a_t], unit, Vec::new()),
        fx.func("c_missing_impl", 0, &[b_t], usize_, Vec::new()),
        fx.func("c_supertrait", 1, &[r_self], u32_, std::vec![sub_t]),
        fx.func("c_fallback", 0, &[], u64_, Vec::new()),
        fx.func("c_closure", 0, &[], u16_, Vec::new()),
    ];
    fx.finish();
    let items = Items { deref, foo, a, b, of, apply, kind_fn, kind_for_a, kind_blanket, kind_a_fn, kind_blanket_fn, owners };
    (fx, items)
}

/// One case's expectations: the type of every expression and every binding, as printed by
/// `fixture::show`, and the diagnostics by code and expression.
struct Want<'s> {
    exprs: &'s [(ExprId, &'s str)],
    pats: &'s [(PatId, &'s str)],
    diags: &'s [(DiagCode, ExprId)],
}

/// Checks the body with each solver fact pulled where it is met (the context's way), and again
/// with each stopped for and the goal asked again (`InferCtx::stop_for_facts`, rule 78): the
/// two must give one record, read list included, so every case of the suite compares them.
fn check_body(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId, plant: Plant, ip: InferPlant) -> Checked<'static> {
    let (pulled, rounds) = check_body_by(fx, b, owner, root, plant, ip, false);
    let (stopped, again) = check_body_by(fx, b, owner, root, plant, ip, true);
    assert!(pulled == stopped, "`{}`: the pull and the stop differ:\n{pulled:?}\n{stopped:?}", fx.name_of(owner));
    assert!(rounds <= again, "`{}`: the pull asked {rounds} goals again, the stop {again}", fx.name_of(owner));
    // Every clean body the suite checks round-trips through its record (D163 R1, rule 26).
    checked_record::round_trips(fx.name_of(owner), &pulled, &stopped);
    pulled
}

/// The comparison compares something: the stop asks goals again for facts, the pull asks none
/// (every question is pulled, `ConstHolds` too: where_const.rs holds that one).
#[test]
fn pull_asks_no_goal_again() {
    let (fx, it) = world();
    let (b, root, owner) = case_where_clause(&it);
    let (_, pulled) = check_body_by(&fx, &b, owner, root, Plant::None, InferPlant::None, false);
    let (_, stopped) = check_body_by(&fx, &b, owner, root, Plant::None, InferPlant::None, true);
    assert_eq!(pulled, 0, "the pull asked goals again");
    assert!(stopped > 0, "the stop asked no goal again: the suite's comparison compares nothing");
}

/// One check of the body; `stop`: the stop strategy. Also how many goals were asked again
/// after a fact was filled.
pub(super) fn check_body_by(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId, plant: Plant, ip: InferPlant, stop: bool) -> (Checked<'static>, u64) {
    let mut cx = InferCtx::new_in(Alloc::global());
    cx.plant = ip;
    cx.stop_for_facts = stop;
    cx.facts.first_impl_only = ip == InferPlant::FirstImpl;
    cx.facts.drop_const_clauses = ip == InferPlant::DropConstClauses;
    match check_with(&b.view(owner, root), fx, &mut cx, &Needs::new_in(Alloc::global()), plant, Alloc::global()) {
        Ok(Outcome::Done(c)) => (c, cx.need_rounds),
        Ok(Outcome::Need(n)) => panic!("`{}`: the world missed {:?}", fx.name_of(owner), n.questions),
        Err(e) => panic!("`{}`: {e:?}", fx.name_of(owner)),
    }
}

/// Every difference between the record and the expectations, one line each; empty when equal.
fn differences(fx: &Fx, b: &Bb, c: &Checked<'_>, want: &Want<'_>) -> Vec<String> {
    let mut out = Vec::new();
    let types = c.types.view();
    for i in 0..b.exprs.len() {
        let e = ExprId(i as u32);
        let got = show(fx, types, c.expr_ty[i]);
        match want.exprs.iter().find(|(x, _)| *x == e) {
            Some((_, w)) if *w == got => {}
            Some((_, w)) => out.push(format!("expr {i} ({:?}): want {w}, got {got}", b.exprs[i])),
            None => out.push(format!("expr {i} ({:?}): no expectation written (got {got})", b.exprs[i])),
        }
    }
    for &(p, w) in want.pats {
        let got = show(fx, types, c.local_ty[p.0 as usize]);
        if got != w {
            out.push(format!("binding {}: want {w}, got {got}", p.0));
        }
    }
    let mut got: Vec<(DiagCode, u32)> = c.diags.iter().map(|d| (d.code, d.at.start)).collect();
    let mut wanted: Vec<(DiagCode, u32)> = want.diags.iter().map(|&(d, e)| (d, e.0)).collect();
    got.sort();
    wanted.sort();
    if got != wanted {
        out.push(format!("diagnostics: want {wanted:?}, got {got:?}"));
    }
    out
}

fn expect(fx: &Fx, b: &Bb, c: &Checked<'_>, want: &Want<'_>) {
    let d = differences(fx, b, c, want);
    assert!(d.is_empty(), "\n{}", d.join("\n"));
}

/// The adjustments recorded on `e`, printed: `*` builtin deref, `*Deref` overloaded, `&`/`&mut`
/// autoref (with `2φ` when two-phase), `unsize`.
fn adjusts(fx: &Fx, c: &Checked<'_>, e: ExprId) -> String {
    let (es, ks, ts) = (c.adjusts.column::<0>(), c.adjusts.column::<1>(), c.adjusts.column::<2>());
    let mut parts = Vec::new();
    for i in 0..es.len() {
        if es[i] != e {
            continue;
        }
        let k = match ks[i] {
            AdjustKind::Deref { overloaded: None } => String::from("*"),
            AdjustKind::Deref { overloaded: Some(Mutbl::Not) } => String::from("*Deref"),
            AdjustKind::Deref { overloaded: Some(Mutbl::Mut) } => String::from("*DerefMut"),
            AdjustKind::Borrow { mutbl: Mutbl::Not, .. } => String::from("&"),
            AdjustKind::Borrow { mutbl: Mutbl::Mut, two_phase, .. } => {
                String::from(if two_phase { "&mut 2φ" } else { "&mut" })
            }
            AdjustKind::Unsize => String::from("unsize"),
            other => format!("{other:?}"),
        };
        parts.push(format!("{k} -> {}", show(fx, c.types.view(), ts[i])));
    }
    parts.join(", ")
}

/// The callee recorded for `e`, as `name<args>`.
fn callee(fx: &Fx, c: &Checked<'_>, e: ExprId) -> String {
    let (es, items, args) = (c.callees.column::<0>(), c.callees.column::<1>(), c.callees.column::<2>());
    match es.iter().position(|&x| x == e) {
        Some(i) => {
            let a: Vec<String> = c.types.view().list(args[i]).expect("a callee's arguments in the body's table").iter().map(|&t| show(fx, c.types.view(), t)).collect();
            format!("{}<{}>", fx.name_of(items[i]), a.join(", "))
        }
        None => String::from("none"),
    }
}

// The cases. Each builds `fn <owner>` of `world()`; ids are in build order.

/// ```text
/// fn c_autoderef(r: &&Foo, m: &mut Foo, w: &Wrap) -> u32 { r.get(); m.bump(); w.get() }
/// ```
/// probe.rs `method_autoderef_steps`, `pick_all_method` (by value, then `&`, then `&mut` at each
/// step), `pick_by_value_method` (a by-value pick at a reference step reborrows): `r.get()` is
/// picked by value at step `&Foo` (the inherent `get(&self)`, assembled from step `Foo`) and
/// adjusted `*, *, &`; `m.bump()` by value at step `&mut Foo`, a two-phase `&mut` reborrow;
/// `w.get()` at step `Foo`, reached by the overloaded `Deref` (`<Wrap as Deref>::Target = Foo`),
/// by autoref.
fn case_autoderef(it: &Items) -> (Bb, ExprId, ItemKey) {
    let mut b = Bb::new();
    let (r, m, w) = (b.param("r"), b.param("m"), b.param("w"));
    let e0 = b.local(r);
    let e1 = b.method(e0, "get", &[]);
    let e2 = b.local(m);
    let e3 = b.method(e2, "bump", &[]);
    let e4 = b.local(w);
    let e5 = b.method(e4, "get", &[]);
    let (s0, s1) = (b.semi(e1), b.semi(e3));
    let root = b.block(&[s0, s1], Some(e5));
    (b, root, it.owners[0])
}

const WANT_AUTODEREF: Want<'static> = Want {
    exprs: &[
        (ExprId(0), "&&Foo"),
        (ExprId(1), "u32"),
        (ExprId(2), "&mut Foo"),
        (ExprId(3), "u32"),
        (ExprId(4), "&Wrap"),
        (ExprId(5), "u32"),
        (ExprId(6), "u32"),
    ],
    pats: &[(PatId(0), "&&Foo"), (PatId(1), "&mut Foo"), (PatId(2), "&Wrap")],
    diags: &[],
};

#[test]
fn autoderef_through_ref_mut_and_deref() {
    let (fx, it) = world();
    let (b, root, owner) = case_autoderef(&it);
    let c = check_body(&fx, &b, owner, root, Plant::None, InferPlant::None);
    expect(&fx, &b, &c, &WANT_AUTODEREF);
    assert_eq!(adjusts(&fx, &c, ExprId(0)), "* -> &Foo, * -> Foo, & -> &Foo");
    assert_eq!(adjusts(&fx, &c, ExprId(2)), "* -> Foo, &mut 2φ -> &mut Foo");
    assert_eq!(adjusts(&fx, &c, ExprId(4)), "* -> Wrap, *Deref -> Foo, & -> &Foo");
    assert_eq!(callee(&fx, &c, ExprId(5)), "get<>");
    let _ = it.deref;
}

/// The control: autoderef that stops one step early never reaches `Foo` behind `Wrap`, so
/// `w.get()` is E0599 (and its type the error type).
#[test]
fn control_autoderef_short() {
    let (fx, it) = world();
    let (b, root, owner) = case_autoderef(&it);
    let c = check_body(&fx, &b, owner, root, Plant::AutoderefShort, InferPlant::None);
    assert!(!differences(&fx, &b, &c, &WANT_AUTODEREF).is_empty(), "the planted short autoderef was not caught");
}

/// ```text
/// fn c_inherent_first(f: Foo) -> usize { let x = f.name(); let y = f.size(); y }
/// ```
/// probe.rs `pick_method`: at each autoref level the inherent candidates are tried before the
/// extension candidates (traits in scope), so `f.name()` is the inherent `name -> u8`, not
/// `Named::name -> u16`, though both apply at `&Foo`; `f.size()` has no inherent candidate and
/// is `Size::size` from the trait in scope.
fn case_inherent_first(it: &Items) -> (Bb, ExprId, ItemKey) {
    let mut b = Bb::new();
    let f = b.param("f");
    let e0 = b.local(f);
    let e1 = b.method(e0, "name", &[]);
    let x = b.bind("x");
    let s0 = b.let_(x, None, e1);
    let e2 = b.local(f);
    let e3 = b.method(e2, "size", &[]);
    let y = b.bind("y");
    let s1 = b.let_(y, None, e3);
    let e4 = b.local(y);
    let root = b.block(&[s0, s1], Some(e4));
    (b, root, it.owners[1])
}

#[test]
fn inherent_before_trait_in_scope() {
    let (fx, it) = world();
    let (b, root, owner) = case_inherent_first(&it);
    let c = check_body(&fx, &b, owner, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[
                (ExprId(0), "Foo"),
                (ExprId(1), "u8"),
                (ExprId(2), "Foo"),
                (ExprId(3), "usize"),
                (ExprId(4), "usize"),
                (ExprId(5), "usize"),
            ],
            pats: &[(PatId(0), "Foo"), (PatId(1), "u8"), (PatId(2), "usize")],
            diags: &[],
        },
    );
    assert_eq!(callee(&fx, &c, ExprId(1)), "name<>", "the inherent `name`");
    assert_eq!(callee(&fx, &c, ExprId(3)), "size<Foo>", "`Size::size` with `Self = Foo`");
    assert_eq!(adjusts(&fx, &c, ExprId(0)), "& -> &Foo");
}

/// ```text
/// fn c_where_clause<T: Size>(t: &T) -> usize { of(t) }
/// ```
/// `of::<T>` registers `T: Size` (and `T: Sized`); the next solver proves both from the param env
/// (trait_goals.rs `assemble_param_env_candidates`, `ParamEnv` candidate), no impl applying to a
/// parameter.
fn case_where_clause(it: &Items) -> (Bb, ExprId, ItemKey) {
    let mut b = Bb::new();
    let t = b.param("t");
    let e0 = b.path(&["of"]);
    let e1 = b.local(t);
    let e2 = b.call(e0, &[e1]);
    let root = b.block(&[], Some(e2));
    (b, root, it.owners[2])
}

const WANT_WHERE_CLAUSE: Want<'static> = Want {
    exprs: &[(ExprId(0), "fn of<P0>"), (ExprId(1), "&P0"), (ExprId(2), "usize"), (ExprId(3), "usize")],
    pats: &[(PatId(0), "&P0")],
    diags: &[],
};

#[test]
fn bound_proved_through_where_clause() {
    let (fx, it) = world();
    let (b, root, owner) = case_where_clause(&it);
    let c = check_body(&fx, &b, owner, root, Plant::None, InferPlant::None);
    expect(&fx, &b, &c, &WANT_WHERE_CLAUSE);
    let _ = it.of;
}

/// The control: with the where-clauses kept out of the param env, `T: Size` has no candidate
/// and is E0277.
#[test]
fn control_where_clause_ignored() {
    let (fx, it) = world();
    let (b, root, owner) = case_where_clause(&it);
    let c = check_body(&fx, &b, owner, root, Plant::IgnoreWhereClauses, InferPlant::None);
    assert!(!differences(&fx, &b, &c, &WANT_WHERE_CLAUSE).is_empty(), "the ignored where-clause was not caught");
}

/// ```text
/// fn c_projection(a: A) -> u32 { a.make().get() }
/// ```
/// `Make::make` returns `<A as Make>::Out`, normalized by `impl Make for A` to `Foo`
/// (normalizes_to.rs `consider_impl_candidate`); the receiver of `.get()` is resolved
/// structurally (probe.rs `probe_op` on `structurally_resolve_type`), so the projection is
/// used as `Foo` and `get` is `Foo`'s inherent method.
#[test]
fn projection_normalized_then_used() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let a = b.param("a");
    let e0 = b.local(a);
    let e1 = b.method(e0, "make", &[]);
    let e2 = b.method(e1, "get", &[]);
    let root = b.block(&[], Some(e2));
    let c = check_body(&fx, &b, it.owners[3], root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[(ExprId(0), "A"), (ExprId(1), "Foo"), (ExprId(2), "u32"), (ExprId(3), "u32")],
            pats: &[(PatId(0), "A")],
            diags: &[],
        },
    );
    assert_eq!(callee(&fx, &c, ExprId(1)), "make<A>");
    let _ = it.a;
}

/// ```text
/// fn c_narrowed(a: A) -> char { let x = a.mix(1u8); a.mix(true) }
/// ```
/// `a.mix(r)` registers `A: Mix<?R>`; the argument fixes `?R` (u8, then bool), and of the two
/// impls in `A`'s bucket only the one whose trait arguments unify applies
/// (trait_goals.rs `consider_impl_candidate`), so `Out` normalizes to `u16`, then `char`.
fn case_narrowed(it: &Items) -> (Bb, ExprId, ItemKey) {
    let mut b = Bb::new();
    let a = b.param("a");
    let e0 = b.local(a);
    let e1 = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let e2 = b.method(e0, "mix", &[e1]);
    let x = b.bind("x");
    let s0 = b.let_(x, None, e2);
    let e3 = b.local(a);
    let e4 = b.bool();
    let e5 = b.method(e3, "mix", &[e4]);
    let root = b.block(&[s0], Some(e5));
    (b, root, it.owners[4])
}

const WANT_NARROWED: Want<'static> = Want {
    exprs: &[
        (ExprId(0), "A"),
        (ExprId(1), "u8"),
        (ExprId(2), "u16"),
        (ExprId(3), "A"),
        (ExprId(4), "bool"),
        (ExprId(5), "char"),
        (ExprId(6), "char"),
    ],
    pats: &[(PatId(0), "A"), (PatId(1), "u16")],
    diags: &[],
};

#[test]
fn two_impls_narrowed_by_argument() {
    let (fx, it) = world();
    let (b, root, owner) = case_narrowed(&it);
    let c = check_body(&fx, &b, owner, root, Plant::None, InferPlant::None);
    expect(&fx, &b, &c, &WANT_NARROWED);
    assert_eq!(callee(&fx, &c, ExprId(5)), "mix<A, bool>");
}

/// The control: a solver that sees only the first impl of `A`'s bucket (`Mix<u8>`) takes it
/// for `a.mix(true)` instead of selecting `Mix<bool>`.
#[test]
fn control_first_impl_taken() {
    let (fx, it) = world();
    let (b, root, owner) = case_narrowed(&it);
    let c = check_body(&fx, &b, owner, root, Plant::None, InferPlant::FirstImpl);
    assert!(!differences(&fx, &b, &c, &WANT_NARROWED).is_empty(), "taking the first impl was not caught");
}

/// ```text
/// fn c_blanket(a: A, f: Foo) -> u8 { let x = a.kind(); f.kind() }
/// ```
/// `A`'s bucket holds `impl Kind for A` and the blanket `impl<T: Size> Kind for T`; the
/// blanket's where-clause `A: Size` fails (no impl), so only the concrete impl applies and
/// `K = u16`. `Foo` has no concrete impl; the blanket applies through `impl Size for Foo` and
/// `K = u8` (trait_goals.rs `consider_impl_candidate`, its nested where-clause goals). For
/// monomorphization `resolve_instance` picks each impl's own `kind`.
#[test]
fn blanket_impl_against_concrete() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let (a, f) = (b.param("a"), b.param("f"));
    let e0 = b.local(a);
    let e1 = b.method(e0, "kind", &[]);
    let x = b.bind("x");
    let s0 = b.let_(x, None, e1);
    let e2 = b.local(f);
    let e3 = b.method(e2, "kind", &[]);
    let root = b.block(&[s0], Some(e3));
    let c = check_body(&fx, &b, it.owners[5], root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[(ExprId(0), "A"), (ExprId(1), "u16"), (ExprId(2), "Foo"), (ExprId(3), "u8"), (ExprId(4), "u8")],
            pats: &[(PatId(0), "A"), (PatId(1), "Foo"), (PatId(2), "u16")],
            diags: &[],
        },
    );
    let mut ground = crate::portable::PortableTys::new_in(Alloc::global());
    for (self_, want_fn, want_impl) in [(it.a, it.kind_a_fn, it.kind_for_a), (it.foo, it.kind_blanket_fn, it.kind_blanket)] {
        let t = ground.intern(PNode::Adt(self_, crate::ids::List::EMPTY)).expect("intern");
        let args = ground.intern_list(&[t]).expect("intern");
        let r = crate::resolve_instance(&fx, it.kind_fn, args, &mut ground, Alloc::global())
            .unwrap_or_else(|e| panic!("{}: {e}", fx.name_of(self_)));
        assert_eq!(r.item, want_fn, "`{}`: the `kind` of {}", fx.name_of(self_), fx.name_of(want_impl));
    }
}

/// ```text
/// fn c_ambiguous_then_fixed(a: A) -> u16 { let x = a.conv(); let y: u16 = x; y }
/// ```
/// `A: Conv<?T>` matches both impls: the next solver answers ambiguity and the obligation stays
/// pending (fulfill.rs; never selected by order). `let y: u16 = x` unifies `?T = u16`; the
/// obligation is then selected and holds (`impl Conv<u16> for A`).
#[test]
fn ambiguous_until_a_later_statement() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let a = b.param("a");
    let e0 = b.local(a);
    let e1 = b.method(e0, "conv", &[]);
    let x = b.bind("x");
    let s0 = b.let_(x, None, e1);
    let e2 = b.local(x);
    let y = b.bind("y");
    let u16_ = b.ty(PNode::Uint(UintTy::U16));
    let s1 = b.let_(y, Some(u16_), e2);
    let e3 = b.local(y);
    let root = b.block(&[s0, s1], Some(e3));
    let c = check_body(&fx, &b, it.owners[6], root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[(ExprId(0), "A"), (ExprId(1), "u16"), (ExprId(2), "u16"), (ExprId(3), "u16"), (ExprId(4), "u16")],
            pats: &[(PatId(0), "A"), (PatId(1), "u16"), (PatId(2), "u16")],
            diags: &[],
        },
    );
    assert_eq!(callee(&fx, &c, ExprId(1)), "conv<A, u16>");
}

/// ```text
/// fn c_ambiguous(a: A) { let x = a.conv(); }
/// ```
/// Nothing fixes `?T`: at the end `A: Conv<?T>` is still ambiguous and is reported as "type
/// annotations needed" (rustc's `select_all_or_error`, then `maybe_report_ambiguity`: a trait
/// goal whose self type is known is E0283 "cannot satisfy", a `Sized` goal or an unknown self
/// type E0282). Writeback gives the unresolved `?T` the error type.
fn case_ambiguous(it: &Items) -> (Bb, ExprId, ItemKey) {
    let mut b = Bb::new();
    let a = b.param("a");
    let e0 = b.local(a);
    let e1 = b.method(e0, "conv", &[]);
    let x = b.bind("x");
    let s0 = b.let_(x, None, e1);
    let root = b.block(&[s0], None);
    (b, root, it.owners[7])
}

const WANT_AMBIGUOUS: Want<'static> = Want {
    exprs: &[(ExprId(0), "A"), (ExprId(1), "{error}"), (ExprId(2), "()")],
    pats: &[(PatId(0), "A"), (PatId(1), "{error}")],
    diags: &[(DiagCode::CannotSatisfy, ExprId(1))],
};

#[test]
fn ambiguity_reported() {
    let (fx, it) = world();
    let (b, root, owner) = case_ambiguous(&it);
    let c = check_body(&fx, &b, owner, root, Plant::None, InferPlant::None);
    expect(&fx, &b, &c, &WANT_AMBIGUOUS);
}

/// The E0283 finding names the goal left undecided (`A: Conv<?T>`), as an E0277 names its
/// unmet goal: a person reads which bound had more than one answer, not "cannot satisfy".
#[test]
fn an_ambiguity_names_its_goal() {
    let (fx, it) = world();
    let (b, root, owner) = case_ambiguous(&it);
    let c = check_body(&fx, &b, owner, root, Plant::None, InferPlant::None);
    let named: Vec<String> = c
        .diags
        .iter()
        .map(|d| match d.because {
            Some(crate::diag::Because::Unmet { goal: crate::diag::UnmetGoal::Trait { trait_, .. }, leaf: None }) => format!("{}", fx.name_of(trait_)),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(named, ["Conv"], "the E0283's goal");
}

/// The control: settling the leftover ambiguity by the first impl that unifies makes `x: u8`
/// and drops the error.
#[test]
fn control_ambiguity_by_order() {
    let (fx, it) = world();
    let (b, root, owner) = case_ambiguous(&it);
    let c = check_body(&fx, &b, owner, root, Plant::None, InferPlant::AmbiguityByOrder);
    assert!(!differences(&fx, &b, &c, &WANT_AMBIGUOUS).is_empty(), "an ambiguity resolved by order was not caught");
}

/// ```text
/// fn c_missing_impl(b: B) -> usize { of(&b) }
/// ```
/// `B: Size` has no impl and no bound: E0277, at `&b`, the argument passed for the input that
/// names the clause's parameter (rules/obligation_site.rs; rustc's `adjust_fulfillment_errors_
/// for_expr_obligation` points there too). The call's type is still the signature's output, `usize`.
#[test]
fn missing_impl_reported() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let p = b.param("b");
    let e0 = b.path(&["of"]);
    let e1 = b.local(p);
    let e2 = b.borrow(Mutbl::Not, e1);
    let e3 = b.call(e0, &[e2]);
    let root = b.block(&[], Some(e3));
    let c = check_body(&fx, &b, it.owners[8], root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[(ExprId(0), "fn of<B>"), (ExprId(1), "B"), (ExprId(2), "&B"), (ExprId(3), "usize"), (ExprId(4), "usize")],
            pats: &[(PatId(0), "B")],
            diags: &[(DiagCode::Unsatisfied, ExprId(2))],
        },
    );
    let _ = it.b;
}

/// ```text
/// fn made<T>(t: T) -> <T as Make>::Out;
/// fn c_missing_impl(b: B) -> usize { made(b) }
/// ```
/// The Reference, "Paths", "Qualified paths": `<B as Make>::Out` names the associated type of
/// `B`'s implementation of `Make`, and there is none ("Trait and lifetime bounds": the bound
/// `B: Make` is required for the path to be well formed). The projection is in the callee's
/// signature, instantiated by the call: its failure is reported at the call `made(b)`, never at
/// an empty range.
#[test]
fn projection_in_a_callee_signature_fails_at_the_call() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let p = b.param("b");
    let e0 = b.path(&["made"]);
    let e1 = b.local(p);
    let e2 = b.call(e0, &[e1]);
    let root = b.block(&[], Some(e2));
    let c = check_body(&fx, &b, it.owners[8], root, Plant::None, InferPlant::None);
    // A fixture expression's range starts at its id and is not empty.
    let codes: Vec<_> = c.diags.iter().map(|d| (d.code, d.at.start, d.at.end > d.at.start)).collect();
    assert_eq!(codes, std::vec![(DiagCode::Unsatisfied, e2.0, true)]);
}

/// The Reference, "Paths" ("Paths in expressions"; "Qualified paths"): `u32::MAX` is a
/// type-relative path whose first segment, `u32`, names a built-in type ("Primitive types")
/// and whose last is an associated item of that type: an inherent impl's (core's
/// `impl u32 { pub const MAX: Self = ..; }`) first, then the traits in scope. `<u32>::MAX` is
/// the same lookup with the type written as a qualified path's anchor.
fn prim_case(build: impl Fn(&mut Bb) -> ExprId) -> (Fx, Bb, Checked<'static>) {
    let (fx, it) = world();
    let mut b = Bb::new();
    let _ = b.param("a");
    let e = build(&mut b);
    let root = b.block(&[], Some(e));
    let c = check_body(&fx, &b, it.owners[3], root, Plant::None, InferPlant::None);
    (fx, b, c)
}

/// `fn c_projection(a: A) -> u32 { u32::MAX }`: the constant has type `u32`.
#[test]
fn path_from_a_primitive_type_names_its_inherent_const() {
    let (fx, b, c) = prim_case(|b| b.path(&["u32", "MAX"]));
    expect(&fx, &b, &c, &Want { exprs: &[(ExprId(0), "u32"), (ExprId(1), "u32")], pats: &[(PatId(0), "A")], diags: &[] });
}

/// `fn c_projection(a: A) -> u32 { u32::pow(2, 3) }`: the method as a function of two `u32`.
#[test]
fn path_from_a_primitive_type_calls_its_inherent_method() {
    let (fx, b, c) = prim_case(|b| {
        let f = b.path(&["u32", "pow"]);
        let x = b.int(None);
        let y = b.int(None);
        b.call(f, &[x, y])
    });
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[(ExprId(0), "fn pow"), (ExprId(1), "u32"), (ExprId(2), "u32"), (ExprId(3), "u32"), (ExprId(4), "u32")],
            pats: &[(PatId(0), "A")],
            diags: &[],
        },
    );
}

/// `fn c_projection(a: A) -> u32 { <u32>::MAX }`.
#[test]
fn qualified_path_on_a_primitive_type_names_its_const() {
    let (fx, b, c) = prim_case(|b| {
        let u = b.ty(PNode::Uint(UintTy::U32));
        b.qpath(u, &["MAX"])
    });
    expect(&fx, &b, &c, &Want { exprs: &[(ExprId(0), "u32"), (ExprId(1), "u32")], pats: &[(PatId(0), "A")], diags: &[] });
}

/// `fn c_projection(a: A) -> u32 { u32::MIN }`: no inherent item and no trait in scope has
/// `MIN`: the error names the type and the name.
#[test]
fn unknown_item_on_a_primitive_type_names_both() {
    let (fx, _b, c) = prim_case(|b| b.path(&["u32", "MIN"]));
    let got: Vec<_> = c
        .diags
        .iter()
        .map(|d| (d.code, d.name.map(|n| n == Name::of("MIN")), d.found.map(|t| show(&fx, c.types.view(), t))))
        .collect();
    assert_eq!(got, std::vec![(DiagCode::NoMethod, Some(true), Some(String::from("u32")))]);
}

/// ```text
/// fn c_supertrait<T: Sub>(t: &T) -> u32 { t.base() }
/// ```
/// `Base` is not in scope; `base` is found because the param env's `T: Sub` is elaborated to
/// its supertrait `T: Base` and a parameter's bound traits are inherent candidates (probe.rs
/// `assemble_inherent_candidates_from_param`, over the elaborated env); `T: Base` is then
/// proved from the elaborated param env.
#[test]
fn supertrait_method_on_a_parameter() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let t = b.param("t");
    let e0 = b.local(t);
    let e1 = b.method(e0, "base", &[]);
    let root = b.block(&[], Some(e1));
    let c = check_body(&fx, &b, it.owners[9], root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want { exprs: &[(ExprId(0), "&P0"), (ExprId(1), "u32"), (ExprId(2), "u32")], pats: &[(PatId(0), "&P0")], diags: &[] },
    );
    assert_eq!(callee(&fx, &c, ExprId(1)), "base<P0>");
}

/// ```text
/// fn c_fallback() -> u64 { let a = 1; let b = 2.0; let c = 3; let d: u64 = c; d }
/// ```
/// Integer and float variables nothing constrains fall back to `i32` and `f64`
/// (fallback.rs `type_inference_fallback`); `c` is fixed to `u64` by the annotated `let`.
#[test]
fn literal_fallback() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let e0 = b.int(None);
    let pa = b.bind("a");
    let s0 = b.let_(pa, None, e0);
    let e1 = b.float(None);
    let pb = b.bind("b");
    let s1 = b.let_(pb, None, e1);
    let e2 = b.int(None);
    let pc = b.bind("c");
    let s2 = b.let_(pc, None, e2);
    let e3 = b.local(pc);
    let pd = b.bind("d");
    let u64_ = b.ty(PNode::Uint(UintTy::U64));
    let s3 = b.let_(pd, Some(u64_), e3);
    let e4 = b.local(pd);
    let root = b.block(&[s0, s1, s2, s3], Some(e4));
    let c = check_body(&fx, &b, it.owners[10], root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[(ExprId(0), "i32"), (ExprId(1), "f64"), (ExprId(2), "u64"), (ExprId(3), "u64"), (ExprId(4), "u64"), (ExprId(5), "u64")],
            pats: &[(PatId(0), "i32"), (PatId(1), "f64"), (PatId(2), "u64"), (PatId(3), "u64")],
            diags: &[],
        },
    );
}

/// ```text
/// fn c_closure() -> u16 { apply(|x| 5) }
/// ```
/// The call checks the closure argument last, after selecting (rustc's two argument passes); its
/// expected signature comes from the pending `?F: Fn<(u8,)>` and `<?F as FnOnce<(u8,)>>::Output
/// == u16` (closure.rs `deduce_closure_signature`, `deduce_sig_from_projection`), so `x: u8` and
/// the literal `5` is checked against `u16`. The closure's own type is printed by its expression.
#[test]
fn closure_passed_to_a_generic_bound() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let e0 = b.path(&["apply"]);
    let x = b.bind("x");
    let e1 = b.int(None);
    let e2 = b.closure(&[x], e1);
    let e3 = b.call(e0, &[e2]);
    let root = b.block(&[], Some(e3));
    let c = check_body(&fx, &b, it.owners[11], root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[
                (ExprId(0), "fn apply<closure#0 of c_closure<>>"),
                (ExprId(1), "u16"),
                (ExprId(2), "closure#0 of c_closure<>"),
                (ExprId(3), "u16"),
                (ExprId(4), "u16"),
            ],
            pats: &[(PatId(0), "u8")],
            diags: &[],
        },
    );
    let _ = it.apply;
}

/// The solver's own case files (blaze-solve `tests/cases/*.solve`) run with their terms in this
/// crate's `Terms` (blaze-ty's arena, the `Tys` `InferCtx` hands the solver) instead of the
/// solver's test arena; the facts stay the case world's (`Facts` answered from the case text: the
/// `FactCache` path needs a typeck `World`, which the cases do not describe). Every goal that
/// passes on the test arena must pass here, and the counts are printed.
#[test]
fn solver_cases_over_terms() {
    use blaze_solve::testing::run::run_all_with;
    use blaze_solve::testing::tys::TestTys;
    let all = |_: &str| true;
    // The runner catches a case's panic and reports it; the default hook would print each.
    let hook = std::panic::take_hook();
    std::panic::set_hook(std::boxed::Box::new(|_| {}));
    let base = run_all_with(&all, &TestTys::new);
    let real = run_all_with(&all, &|| crate::infer::Terms::new_in(Alloc::global()));
    std::panic::set_hook(hook);
    let base = base.unwrap_or_else(|e| panic!("the case files: {e:?}"));
    let real = real.unwrap_or_else(|e| panic!("the case files: {e:?}"));
    let only_real: Vec<&String> = real.failures.iter().filter(|f| !base.failures.contains(f)).collect();
    // One line per kind of failure (the text after the case's name), with its count.
    let mut kinds: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    for f in &only_real {
        let why = f.split("): ").nth(1).unwrap_or(f);
        *kinds.entry(why.chars().take(140).collect()).or_default() += 1;
    }
    let summary = format!(
        "solver cases: {} cases, {} goals; test arena {} passed; Terms {} passed; {} fail only over Terms",
        real.cases,
        real.goals,
        base.passed,
        real.passed,
        only_real.len()
    );
    std::eprintln!("{summary}");
    assert!(only_real.is_empty(), "{summary}\n{kinds:#?}\nfirst: {:#?}", only_real.iter().take(3).collect::<Vec<_>>());
}

/// Every control's plant is off in the cases above: each case passes with `Plant::None`, and
/// the planted run of the same body differs. The clause type is used by the world's builder.
#[allow(dead_code)]
fn _uses(_: PClause) {}
