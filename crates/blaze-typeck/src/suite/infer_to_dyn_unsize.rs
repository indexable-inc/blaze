//! Unsizing coercion of an unresolved type inference variable to a trait object (`&?E -> &dyn Tr`,
//! `&mut ?E -> &dyn Tr`, `&mut ?E -> &mut dyn Tr`, `*mut ?E -> *const dyn Tr`, `Box<?E> -> Box<dyn Tr>`,
//! `Pin<Box<?E>> -> Pin<Box<dyn Tr>>`).
//!
//! Real-world pattern (`s.parse().map_err(|e| bad_because(s, &e))` returning `Result<u32, ()>`):
//!
//! ```text
//! trait Display {}
//! trait Error: Display {}
//! trait MyTrait {}
//! trait FromStr { type Err; }
//! struct ParseIntError;
//! impl Display for ParseIntError {}
//! impl Error for ParseIntError {}
//! impl FromStr for u32 { type Err = ParseIntError; }
//! struct Str;
//! impl Str { fn parse<F: FromStr>(&self) -> Result<F, F::Err>; }
//! impl<T, E> Result<T, E> {
//!     fn map_err<O, F: FnOnce(E) -> O>(self, op: F) -> Result<T, O>;
//! }
//! ```
//!
//! Inside the closure `|e| takes_dyn_display(&e)`, `F` is not yet unified with `u32`, so `e` has
//! type `?E` (an unresolved `TyView::Infer(VarKind::Ty)` linked to `<?F as FromStr>::Err`), and
//! `&e` has type `&?E`. `coerce_unsized` must register `?E: Unsize<dyn Display>` (and
//! `Box<?E>: CoerceUnsized<Box<dyn Display>>` for smart pointers) instead of returning `Ok(None)`
//! and falling through to unification, which prematurely forced `?E = dyn Display` and then failed
//! with E0271 (`<u32 as FromStr>::Err == dyn Display`) + E0277 (`dyn Display: Sized`).
//!
//! The rules relied on (the Reference, "Type coercions", "Unsized Coercions"):
//! - "`T` to `dyn U`, when `T` implements `U + Sized`, and `U` is dyn compatible";
//! - "TyCtor(`T`) to TyCtor(`U`), where TyCtor(`T`) is one of `&T`, `&mut T`, `*const T`,
//!   `*mut T`, `Box<T>`, and where `U` can be obtained from `T` by unsized coercion".

use std::vec::Vec;

use super::{adjusts, check_body};
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{Abi, ExprId, FnPtrHeader, LangItem, List, Mutbl, Safety, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

fn world() -> Fx {
    let mut world = Fx::new();
    let fx = &mut world;

    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let unsize = fx.tr("Unsize", 2, Vec::new(), &[]);
    fx.lang(LangItem::Unsize, unsize);
    let coerce = fx.tr("CoerceUnsized", 2, Vec::new(), &[]);
    fx.lang(LangItem::CoerceUnsized, coerce);

    let (p0, p1, p2, p3) = (fx.param(0), fx.param(1), fx.param(2), fx.param(3));
    let unit = fx.unit();
    let u32_ = fx.ty(PNode::Uint(UintTy::U32));

    // FnOnce / FnMut / Fn
    let rust_call = FnPtrHeader { abi: Abi::RustCall, safety: Safety::Safe, c_variadic: false };
    let fn_once = fx.tr("FnOnce", 2, Vec::new(), &["Output"]);
    fx.lang(LangItem::FnOnce, fn_once);
    let out = fx.alias(fn_once, "Output", &[p0, p1]);
    let call_once = fx.trait_fn(fn_once, "call_once", true, &[p0, p1], out);
    fx.set_header(call_once, rust_call);
    let sup_once = fx.bound(fn_once, &[p0, p1]);
    let fn_mut = fx.tr("FnMut", 2, std::vec![sup_once], &[]);
    fx.lang(LangItem::FnMut, fn_mut);
    let m_self = fx.r(Mutbl::Mut, p0);
    let call_mut = fx.trait_fn(fn_mut, "call_mut", true, &[m_self, p1], out);
    fx.set_header(call_mut, rust_call);
    let sup_mut = fx.bound(fn_mut, &[p0, p1]);
    let fn_ = fx.tr("Fn", 2, std::vec![sup_mut], &[]);
    fx.lang(LangItem::Fn, fn_);
    let r_self = fx.r(Mutbl::Not, p0);
    let call = fx.trait_fn(fn_, "call", true, &[r_self, p1], out);
    fx.set_header(call, rust_call);

    // Traits: Display, Error: Display, MyTrait (not implemented by ParseIntError)
    let display = fx.tr("Display", 1, Vec::new(), &[]);
    let display_self = fx.bound(display, &[p0]);
    let error = fx.tr("Error", 1, std::vec![display_self], &[]);
    let my_trait = fx.tr("MyTrait", 1, Vec::new(), &[]);

    // FromStr + ParseIntError + impl FromStr for u32
    let from_str = fx.tr("FromStr", 1, Vec::new(), &["Err"]);
    let pie_k = fx.strukt("ParseIntError", 0, &[]);
    let pie = fx.adt(pie_k, &[]);
    fx.imp("impl Display for ParseIntError", 0, pie, Some((display, &[])), &[], Vec::new());
    fx.imp("impl Error for ParseIntError", 0, pie, Some((error, &[])), &[], Vec::new());
    fx.imp("impl FromStr for u32", 0, u32_, Some((from_str, &[])), &[("Err", pie)], Vec::new());

    // Box<T: ?Sized> and Pin<P> with CoerceUnsized impls
    let box_k = fx.strukt("Box", 1, &[]);
    let (box_t, box_u) = (fx.adt(box_k, &[p0]), fx.adt(box_k, &[p1]));
    let t_unsize_u = fx.bound(unsize, &[p0, p1]);
    let box_cu = fx.imp("impl CoerceUnsized<Box<U>> for Box<T>", 2, box_t, Some((coerce, &[box_u])), &[], std::vec![t_unsize_u]);
    fx.unsized_param(box_cu, 0);
    fx.unsized_param(box_cu, 1);

    let pin_k = fx.strukt("Pin", 1, &[]);
    let (pin_p, pin_u) = (fx.adt(pin_k, &[p0]), fx.adt(pin_k, &[p1]));
    let p_cu_u = fx.bound(coerce, &[p0, p1]);
    fx.imp("impl CoerceUnsized<Pin<U>> for Pin<P>", 2, pin_p, Some((coerce, &[pin_u])), &[], std::vec![p_cu_u]);

    // Wide<M, P> with a binary-tree marker `M` exceeding `has_unresolved_to_dyn`'s 256-pair walk cap
    // before reaching the trailing `P = Box<?E> -> Box<dyn Display>` argument.
    let pair_k = fx.strukt("Pair", 2, &[]);
    let mut deep_marker = u32_;
    for _ in 0..8 {
        deep_marker = fx.adt(pair_k, &[deep_marker, deep_marker]);
    }
    let wide_k = fx.strukt("Wide", 2, &[]);
    let (wide_mp, wide_mu) = (fx.adt(wide_k, &[p0, p1]), fx.adt(wide_k, &[p0, p2]));
    let p1_cu_p2 = fx.bound(coerce, &[p1, p2]);
    fx.imp("impl CoerceUnsized<Wide<M, U>> for Wide<M, P>", 3, wide_mp, Some((coerce, &[wide_mu])), &[], std::vec![p1_cu_p2]);
    let wide_tree_box_p0 = fx.adt(wide_k, &[deep_marker, box_t]);
    fx.func("wide_box", 1, &[p0], wide_tree_box_p0, Vec::new());

    let pin_box_t = fx.adt(pin_k, &[box_t]);
    fx.func("box_new", 1, &[p0], box_t, Vec::new());
    fx.func("box_pin", 1, &[p0], pin_box_t, Vec::new());
    let ptr_mut_p0 = fx.ty(PNode::Ptr(Mutbl::Mut, p0));
    fx.func("as_raw_mut", 1, &[m_self], ptr_mut_p0, Vec::new());

    // Reflexive helpers:
    // fn make_ref_fn<T: ?Sized, F: FnOnce(&T) -> ()>(f: F) -> F
    // fn make_box_fn<T: ?Sized, F: FnOnce(Box<T>) -> ()>(f: F) -> F
    let args_ref_t = fx.tuple(&[r_self]);
    let f1_ref = fx.bound(fn_once, &[p1, args_ref_t]);
    let f1_ref_out = fx.proj(fn_once, &[p1, args_ref_t], "Output", unit);
    let mrf = fx.func("make_ref_fn", 2, &[p1], p1, std::vec![f1_ref, f1_ref_out]);
    fx.unsized_fn_param(mrf, 0);

    let args_box_t = fx.tuple(&[box_t]);
    let f1_box = fx.bound(fn_once, &[p1, args_box_t]);
    let f1_box_out = fx.proj(fn_once, &[p1, args_box_t], "Output", unit);
    let mbf = fx.func("make_box_fn", 2, &[p1], p1, std::vec![f1_box, f1_box_out]);
    fx.unsized_fn_param(mbf, 0);

    // Result<T, E> with map_err<O, F: FnOnce(E) -> O>(self, op: F) -> Result<T, O>
    let (res_k, _) = fx.enum_("Result", 2, &[("Ok", &[p0]), ("Err", &[p1])]);
    let res_t_e = fx.adt(res_k, &[p0, p1]);
    let res_t_o = fx.adt(res_k, &[p0, p2]);
    let res_imp = fx.imp("impl<T, E> Result<T, E>", 2, res_t_e, None, &[], Vec::new());
    let args_e = fx.tuple(&[p1]);
    let f_once_e = fx.bound(fn_once, &[p3, args_e]);
    let f_out_o = fx.proj(fn_once, &[p3, args_e], "Output", p2);
    fx.impl_fn_own(res_imp, "map_err", true, 2, &[res_t_e, p3], res_t_o, std::vec![f_once_e, f_out_o]);

    // Str with parse<F: FromStr>(&self) -> Result<F, <F as FromStr>::Err>
    let str_k = fx.strukt("Str", 0, &[]);
    let str_ty = fx.adt(str_k, &[]);
    let r_str = fx.r(Mutbl::Not, str_ty);
    let str_imp = fx.imp("impl Str", 0, str_ty, None, &[], Vec::new());
    let f_from_str = fx.bound(from_str, &[p0]);
    let f_err = fx.alias(from_str, "Err", &[p0]);
    let res_f_err = fx.adt(res_k, &[p0, f_err]);
    fx.impl_fn_own(str_imp, "parse", true, 1, &[r_str], res_f_err, std::vec![f_from_str]);

    // Dyn target types
    let dyn_display = {
        let ex = fx.ty(PNode::ExTrait(display, List::EMPTY));
        fx.t.dynamic(&[ex]).expect("dyn Display")
    };
    let dyn_error = {
        let ex = fx.ty(PNode::ExTrait(error, List::EMPTY));
        fx.t.dynamic(&[ex]).expect("dyn Error")
    };
    let dyn_my_trait = {
        let ex = fx.ty(PNode::ExTrait(my_trait, List::EMPTY));
        fx.t.dynamic(&[ex]).expect("dyn MyTrait")
    };

    let r_dyn_display = fx.r(Mutbl::Not, dyn_display);
    let m_dyn_display = fx.r(Mutbl::Mut, dyn_display);
    let ptr_const_dyn_display = fx.ty(PNode::Ptr(Mutbl::Not, dyn_display));
    let r_dyn_error = fx.r(Mutbl::Not, dyn_error);
    let r_dyn_my_trait = fx.r(Mutbl::Not, dyn_my_trait);
    let box_dyn_display = fx.adt(box_k, &[dyn_display]);
    let pin_box_dyn_display = fx.adt(pin_k, &[box_dyn_display]);
    let wide_box_dyn_display = fx.adt(wide_k, &[deep_marker, box_dyn_display]);
    let box_dyn_my_trait = fx.adt(box_k, &[dyn_my_trait]);
    let wide_box_dyn_my_trait = fx.adt(wide_k, &[deep_marker, box_dyn_my_trait]);

    fx.func("takes_dyn_display", 0, &[r_dyn_display], unit, Vec::new());
    fx.func("takes_mut_dyn_display", 0, &[m_dyn_display], unit, Vec::new());
    fx.func("takes_ptr_const_dyn_display", 0, &[ptr_const_dyn_display], unit, Vec::new());
    fx.func("takes_dyn_error", 0, &[r_dyn_error], unit, Vec::new());
    fx.func("takes_dyn_my_trait", 0, &[r_dyn_my_trait], unit, Vec::new());
    fx.func("takes_box_dyn_display", 0, &[box_dyn_display], unit, Vec::new());
    fx.func("takes_pin_box_dyn_display", 0, &[pin_box_dyn_display], unit, Vec::new());
    fx.func("takes_wide_box_dyn_display", 0, &[wide_box_dyn_display], unit, Vec::new());
    fx.func("takes_box_dyn_my_trait", 0, &[box_dyn_my_trait], unit, Vec::new());
    fx.func("takes_wide_box_dyn_my_trait", 0, &[wide_box_dyn_my_trait], unit, Vec::new());

    let res_u32_unit = fx.adt(res_k, &[u32_, unit]);
    for name in [
        "parse_ref_display",
        "parse_ref_error",
        "parse_mut_to_shared_display",
        "parse_mut_to_mut_display",
        "parse_ref_to_const_ptr_display",
        "parse_mut_ptr_to_const_ptr_display",
        "parse_box_display",
        "parse_pin_box_display",
        "parse_wide_box_display",
        "parse_ref_bad_trait",
        "parse_box_bad_trait",
        "parse_wide_box_bad_trait",
    ] {
        fx.func(name, 0, &[r_str], res_u32_unit, Vec::new());
    }
    fx.func("reflexive_ref_dyn", 0, &[r_dyn_display], unit, Vec::new());
    fx.func("reflexive_box_dyn", 0, &[box_dyn_display], unit, Vec::new());

    fx.finish();
    world
}

/// Builds `s.parse().map_err(|e| <callee>(<make_arg(e)>))` and checks `owner`.
fn check_parse_map_err(
    fx: &Fx,
    owner: &str,
    callee_name: &str,
    make_arg: impl FnOnce(&mut Bb, crate::ids::PatId) -> ExprId,
) -> (crate::columns::Checked<'static>, ExprId) {
    let mut b = Bb::new();
    let s = b.param("s");
    let s_use = b.local(s);
    let parsed = b.method(s_use, "parse", &[]);
    let e_pat = b.bind("e");
    let arg = make_arg(&mut b, e_pat);
    let callee = b.path(&[callee_name]);
    let call = b.call(callee, &[arg]);
    let clo = b.closure(&[e_pat], call);
    let mapped = b.method(parsed, "map_err", &[clo]);
    let root = b.block(&[], Some(mapped));
    (check_body(fx, &b, fx.key_of(owner), root, Plant::None, InferPlant::None), arg)
}

#[test]
fn parse_map_err_ref_unsizes_to_dyn_display_and_error() {
    let fx = world();

    let (c, arg) = check_parse_map_err(&fx, "parse_ref_display", "takes_dyn_display", |b, e| {
        let e_use = b.local(e);
        b.borrow(Mutbl::Not, e_use)
    });
    assert!(c.diags.is_empty(), "Display: {:?}", c.diags);
    let got = adjusts(&fx, &c, arg);
    assert!(got.ends_with("unsize -> &dyn Display"), "Display adjustments `{got}`");

    let (c, arg) = check_parse_map_err(&fx, "parse_ref_error", "takes_dyn_error", |b, e| {
        let e_use = b.local(e);
        b.borrow(Mutbl::Not, e_use)
    });
    assert!(c.diags.is_empty(), "Error: {:?}", c.diags);
    let got = adjusts(&fx, &c, arg);
    assert!(got.ends_with("unsize -> &dyn Error"), "Error adjustments `{got}`");
}

#[test]
fn parse_map_err_mut_ref_and_raw_ptr_unsize_to_dyn() {
    let fx = world();

    // &mut ?E -> &dyn Display
    let (c, arg) = check_parse_map_err(&fx, "parse_mut_to_shared_display", "takes_dyn_display", |b, e| {
        let e_use = b.local(e);
        b.borrow(Mutbl::Mut, e_use)
    });
    assert!(c.diags.is_empty(), "&mut -> &dyn: {:?}", c.diags);
    let got = adjusts(&fx, &c, arg);
    assert!(got.ends_with("unsize -> &dyn Display"), "&mut -> &dyn adjustments `{got}`");

    // &mut ?E -> &mut dyn Display
    let (c, arg) = check_parse_map_err(&fx, "parse_mut_to_mut_display", "takes_mut_dyn_display", |b, e| {
        let e_use = b.local(e);
        b.borrow(Mutbl::Mut, e_use)
    });
    assert!(c.diags.is_empty(), "&mut -> &mut dyn: {:?}", c.diags);
    let got = adjusts(&fx, &c, arg);
    assert!(got.ends_with("unsize -> &mut dyn Display"), "&mut -> &mut dyn adjustments `{got}`");

    // &?E -> *const dyn Display
    let (c, arg) = check_parse_map_err(&fx, "parse_ref_to_const_ptr_display", "takes_ptr_const_dyn_display", |b, e| {
        let e_use = b.local(e);
        b.borrow(Mutbl::Not, e_use)
    });
    assert!(c.diags.is_empty(), "& -> *const dyn: {:?}", c.diags);
    let got = adjusts(&fx, &c, arg);
    assert!(got.ends_with("unsize -> *const dyn Display"), "& -> *const dyn adjustments `{got}`");

    // *mut ?E -> *const dyn Display
    let (c, arg) = check_parse_map_err(&fx, "parse_mut_ptr_to_const_ptr_display", "takes_ptr_const_dyn_display", |b, e| {
        let e_use = b.local(e);
        let r = b.borrow(Mutbl::Mut, e_use);
        let as_raw = b.path(&["as_raw_mut"]);
        b.call(as_raw, &[r])
    });
    assert!(c.diags.is_empty(), "*mut -> *const dyn: {:?}", c.diags);
    let got = adjusts(&fx, &c, arg);
    assert!(got.ends_with("unsize -> *const dyn Display"), "*mut -> *const dyn adjustments `{got}`");
}

#[test]
fn parse_map_err_box_and_pin_box_unsize_to_dyn() {
    let fx = world();

    let (c, arg) = check_parse_map_err(&fx, "parse_box_display", "takes_box_dyn_display", |b, e| {
        let e_use = b.local(e);
        let bn = b.path(&["box_new"]);
        b.call(bn, &[e_use])
    });
    assert!(c.diags.is_empty(), "Box<?E> -> Box<dyn Display>: {:?}", c.diags);
    let got = adjusts(&fx, &c, arg);
    assert_eq!(got, "unsize -> Box<dyn Display>");

    let (c, arg) = check_parse_map_err(&fx, "parse_pin_box_display", "takes_pin_box_dyn_display", |b, e| {
        let e_use = b.local(e);
        let bp = b.path(&["box_pin"]);
        b.call(bp, &[e_use])
    });
    assert!(c.diags.is_empty(), "Pin<Box<?E>> -> Pin<Box<dyn Display>>: {:?}", c.diags);
    let got = adjusts(&fx, &c, arg);
    assert_eq!(got, "unsize -> Pin<Box<dyn Display>>");
}

#[test]
fn reflexive_infer_resolving_to_dyn_itself_checks_clean() {
    let fx = world();

    // `let f = make_ref_fn(|x| takes_dyn_display(x)); f(d)` where `d: &dyn Display`:
    // Closure parameter `x` is deduced as `&?E` while `?E` is still unresolved, so
    // `takes_dyn_display(x)` defers `?E: Unsize<dyn Display>` and records `Unsize`;
    // calling `f(d)` on the next statement then resolves `?E = dyn Display`.
    {
        let mut b = Bb::new();
        let d = b.param("d");
        let x = b.bind("x");
        let x_use = b.local(x);
        let takes = b.path(&["takes_dyn_display"]);
        let body = b.call(takes, &[x_use]);
        let clo = b.closure(&[x], body);
        let helper = b.path(&["make_ref_fn"]);
        let made = b.call(helper, &[clo]);
        let f_pat = b.bind("f");
        let s1 = b.let_(f_pat, None, made);
        let f_use = b.local(f_pat);
        let d_use = b.local(d);
        let call = b.call(f_use, &[d_use]);
        let root = b.block(&[s1], Some(call));
        let c = check_body(&fx, &b, fx.key_of("reflexive_ref_dyn"), root, Plant::None, InferPlant::None);
        assert!(c.diags.is_empty(), "reflexive &?E -> &dyn Display: {:?}", c.diags);
        let got = adjusts(&fx, &c, x_use);
        assert!(got.ends_with("unsize -> &dyn Display"), "reflexive ref adjustments `{got}`");
    }

    // `let f = make_box_fn(|bx| takes_box_dyn_display(bx)); f(bd)` where `bd: Box<dyn Display>`:
    // Closure parameter `bx` is deduced as `Box<?E>`, deferring `Box<?E>: CoerceUnsized<Box<dyn Display>>`,
    // and calling `f(bd)` then resolves `?E = dyn Display`.
    {
        let mut b = Bb::new();
        let bd = b.param("bd");
        let bx = b.bind("bx");
        let bx_use = b.local(bx);
        let takes = b.path(&["takes_box_dyn_display"]);
        let body = b.call(takes, &[bx_use]);
        let clo = b.closure(&[bx], body);
        let helper = b.path(&["make_box_fn"]);
        let made = b.call(helper, &[clo]);
        let f_pat = b.bind("f");
        let s1 = b.let_(f_pat, None, made);
        let f_use = b.local(f_pat);
        let bd_use = b.local(bd);
        let call = b.call(f_use, &[bd_use]);
        let root = b.block(&[s1], Some(call));
        let c = check_body(&fx, &b, fx.key_of("reflexive_box_dyn"), root, Plant::None, InferPlant::None);
        assert!(c.diags.is_empty(), "reflexive Box<?E> -> Box<dyn Display>: {:?}", c.diags);
        let got = adjusts(&fx, &c, bx_use);
        assert_eq!(got, "unsize -> Box<dyn Display>");
    }
}

/// Negative control (rule 23): when `FromStr::Err` (`ParseIntError`) does NOT implement `MyTrait`,
/// `s.parse().map_err(|e| takes_dyn_my_trait(&e))` and `takes_box_dyn_my_trait(box_new(e))` MUST
/// fail with E0277 (`Unsatisfied`), never silently pass.
#[test]
fn control_missing_trait_impl_fails_with_e0277() {
    let fx = world();

    let (c, _) = check_parse_map_err(&fx, "parse_ref_bad_trait", "takes_dyn_my_trait", |b, e| {
        let e_use = b.local(e);
        b.borrow(Mutbl::Not, e_use)
    });
    assert!(
        c.diags.iter().any(|d| d.code == DiagCode::Unsatisfied),
        "CONTROL: &ParseIntError -> &dyn MyTrait must fail with E0277, got {:?}",
        c.diags
    );

    let (c, _) = check_parse_map_err(&fx, "parse_box_bad_trait", "takes_box_dyn_my_trait", |b, e| {
        let e_use = b.local(e);
        let bn = b.path(&["box_new"]);
        b.call(bn, &[e_use])
    });
    assert!(
        c.diags.iter().any(|d| d.code == DiagCode::Unsatisfied),
        "CONTROL: Box<ParseIntError> -> Box<dyn MyTrait> must fail with E0277, got {:?}",
        c.diags
    );
}

/// Boundary test for `has_unresolved_to_dyn`'s `MAX_PAIRS` (256) walk cap:
/// `Wide<DeepMarker, Box<?E>> -> Wide<DeepMarker, Box<dyn Display>>` has 511 matching pairs in
/// its leading `DeepMarker` argument before reaching `(?E, dyn Display)`. Exhausting the walk cap
/// must return `true` conservatively so `coerce_unsized_adt` still defers `CoerceUnsized` instead
/// of falling through to `probe(unify)` and prematurely equating `?E = dyn Display`.
#[test]
fn walk_cap_exhaustion_is_conservative_for_unresolved_to_dyn() {
    let fx = world();

    let (c, arg) = check_parse_map_err(&fx, "parse_wide_box_display", "takes_wide_box_dyn_display", |b, e| {
        let e_use = b.local(e);
        let wb = b.path(&["wide_box"]);
        b.call(wb, &[e_use])
    });
    assert!(c.diags.is_empty(), "Wide<Tree, Box<?E>> -> Wide<Tree, Box<dyn Display>>: {:?}", c.diags);
    let got = adjusts(&fx, &c, arg);
    assert!(got.starts_with("unsize -> Wide<"), "expected unsize adjustment, got `{got}`");

    // Control: missing trait impl still fails with E0277 past the walk cap.
    let (c_bad, _) = check_parse_map_err(&fx, "parse_wide_box_bad_trait", "takes_wide_box_dyn_my_trait", |b, e| {
        let e_use = b.local(e);
        let wb = b.path(&["wide_box"]);
        b.call(wb, &[e_use])
    });
    assert!(
        c_bad.diags.iter().any(|d| d.code == DiagCode::Unsatisfied),
        "CONTROL: Wide<Tree, Box<ParseIntError>> -> Wide<Tree, Box<dyn MyTrait>> must fail with E0277, got {:?}",
        c_bad.diags
    );
}
