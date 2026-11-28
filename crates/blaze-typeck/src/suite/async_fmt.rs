//! `format_args!` and `.await` as blaze-lower gives them (`Expr::FormatArgs`, `Expr::Await`,
//! `BodyView::coroutine`), with their controls.
//!
//! ```text
//! #[lang = "sized"] trait Sized {}
//! trait Display {}            // FmtDisplay
//! trait Debug {}              // FmtDebug
//! struct Arguments;           // format_arguments
//! impl Display for u8 {}
//! trait IntoFuture { type Output; type IntoFuture; fn into_future(self) -> Self::IntoFuture; }
//! struct Ready;
//! impl IntoFuture for Ready { type Output = u8; type IntoFuture = Ready; .. }
//! ```

use std::vec::Vec;

use super::{Want, adjusts, callee, check_body, expect};
use crate::body::FmtTrait;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem, PatId};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;
use crate::world::Def;

struct Items {
    /// `async fn wait(r: Ready) -> u8` (its signature's output written as `u8` here: the
    /// body's check reads `CoroutineKind::Async`'s output, never the signature's).
    wait: ItemKey,
    /// `fn show(x: u8, w: usize) -> Arguments`.
    show: ItemKey,
    /// `fn count_derefs(x: u8, r: &usize, rr: &&usize, mr: &mut usize, d: MyDeref) -> Arguments`.
    count_derefs: ItemKey,
    /// `fn count_bad(x: u8, i: i32, ri: &i32, s: &str) -> Arguments`.
    count_bad: ItemKey,
    /// `fn dual_use(d: MyDeref, b: BothDerefDisplay) -> Arguments`.
    dual_use: ItemKey,
}

fn world() -> (Fx, Items) {
    let mut world = Fx::new();
    let fx = &mut world;
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let deref = fx.tr("Deref", 1, Vec::new(), &["Target"]);
    fx.lang(LangItem::Deref, deref);
    let display = fx.tr("Display", 1, Vec::new(), &[]);
    fx.lang(LangItem::FmtDisplay, display);
    let debug = fx.tr("Debug", 1, Vec::new(), &[]);
    fx.lang(LangItem::FmtDebug, debug);
    let arguments = fx.strukt("Arguments", 0, &[]);
    fx.lang(LangItem::FormatArguments, arguments);
    let (u8_, usize_) = (fx.u8(), fx.usize());
    let i32_ = fx.ty(PNode::Int(crate::ids::IntTy::I32));
    let str_ = fx.ty(PNode::Str);
    fx.imp("impl Display for u8", 0, u8_, Some((display, &[])), &[], Vec::new());
    fx.imp("impl Display for usize", 0, usize_, Some((display, &[])), &[], Vec::new());

    let my_deref = fx.strukt("MyDeref", 0, &[]);
    let my_deref_t = fx.adt(my_deref, &[]);
    fx.imp("impl Deref for MyDeref", 0, my_deref_t, Some((deref, &[])), &[("Target", usize_)], Vec::new());

    let both = fx.strukt("BothDerefDisplay", 0, &[]);
    let both_t = fx.adt(both, &[]);
    fx.imp("impl Deref for BothDerefDisplay", 0, both_t, Some((deref, &[])), &[("Target", usize_)], Vec::new());
    fx.imp("impl Display for BothDerefDisplay", 0, both_t, Some((display, &[])), &[], Vec::new());

    // core's `IntoFuture` (core/src/future/into_future.rs) declares both members, and `.await`
    // projects both (rules/async_fmt.rs `expr_await`: the held future, then the output).
    let into_future = fx.tr("IntoFuture", 1, Vec::new(), &["Output", "IntoFuture"]);
    fx.lang(LangItem::IntoFuture, into_future);
    let p0 = fx.param(0);
    let held_p0 = fx.alias(into_future, "IntoFuture", &[p0]);
    let f = fx.trait_fn(into_future, "into_future", true, &[p0], held_p0);
    fx.lang_def(LangItem::IntoFutureIntoFuture, Def::Fn(f));
    let ready = fx.strukt("Ready", 0, &[]);
    let ready_t = fx.adt(ready, &[]);
    fx.imp("impl IntoFuture for Ready", 0, ready_t, Some((into_future, &[])), &[("Output", u8_), ("IntoFuture", ready_t)], Vec::new());

    let ref_usize = fx.r(crate::ids::Mutbl::Not, usize_);
    let ref_ref_usize = fx.r(crate::ids::Mutbl::Not, ref_usize);
    let mut_usize = fx.r(crate::ids::Mutbl::Mut, usize_);
    let ref_i32 = fx.r(crate::ids::Mutbl::Not, i32_);
    let ref_str = fx.r(crate::ids::Mutbl::Not, str_);
    let arguments_t = fx.adt(arguments, &[]);
    let wait = fx.func("wait", 0, &[ready_t], u8_, Vec::new());
    let show = fx.func("show", 0, &[u8_, usize_], arguments_t, Vec::new());
    let count_derefs = fx.func("count_derefs", 0, &[u8_, ref_usize, ref_ref_usize, mut_usize, my_deref_t], arguments_t, Vec::new());
    let count_bad = fx.func("count_bad", 0, &[u8_, i32_, ref_i32, ref_str], arguments_t, Vec::new());
    let dual_use = fx.func("dual_use", 0, &[my_deref_t, both_t], arguments_t, Vec::new());
    fx.finish();
    (world, Items { wait, show, count_derefs, count_bad, dual_use })
}

/// ```text
/// async fn wait(r: Ready) -> u8 { r.await }
/// ```
/// `Ready: IntoFuture` by the impl; the `.await` is `<Ready as IntoFuture>::Output`, normalized
/// to `u8`; the callee row names `into_future::<Ready>`.
#[test]
fn await_in_an_async_fn_is_into_future_output() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let r = b.param("r");
    let u8_ = b.ty(PNode::Uint(crate::ids::UintTy::U8));
    b.async_fn(u8_);
    let e0 = b.local(r);
    let e1 = b.await_(e0);
    let root = b.block(&[], Some(e1));
    let c = check_body(&fx, &b, it.wait, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[(ExprId(0), "Ready"), (ExprId(1), "u8"), (ExprId(2), "u8")],
            pats: &[(PatId(0), "Ready")],
            diags: &[],
        },
    );
    assert_eq!(callee(&fx, &c, ExprId(1)), "into_future<Ready>");
}

/// The control: the same body in a fn that is not async is E0728, and nothing is asked of
/// `IntoFuture`.
#[test]
fn await_outside_async_is_e0728() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let r = b.param("r");
    let e0 = b.local(r);
    let e1 = b.await_(e0);
    let root = b.block(&[], Some(e1));
    let c = check_body(&fx, &b, it.wait, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[(ExprId(0), "Ready"), (ExprId(1), "{error}"), (ExprId(2), "{error}")],
            pats: &[(PatId(0), "Ready")],
            diags: &[(DiagCode::AwaitOutsideAsync, ExprId(1))],
        },
    );
    assert_eq!(callee(&fx, &c, ExprId(1)), "none");
}

/// ```text
/// fn show(x: u8, w: usize) -> Arguments { format_args!("{x} {x:w$} {:.*}", 3, x) }
/// ```
/// Uses: `x` Display twice, `w` a count, the literal `3` a count (so `usize`, not `i32`).
/// Each argument is borrowed once.
#[test]
fn format_args_proves_each_use_and_counts_are_usize() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let x = b.param("x");
    let w = b.param("w");
    let e0 = b.local(x);
    let e1 = b.local(w);
    let e2 = b.int(None);
    let e3 = b.format_args(
        &[e0, e1, e2],
        &[(0, FmtTrait::Display), (0, FmtTrait::Display), (1, FmtTrait::Count), (2, FmtTrait::Count)],
    );
    let root = b.block(&[], Some(e3));
    let c = check_body(&fx, &b, it.show, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[(ExprId(0), "u8"), (ExprId(1), "usize"), (ExprId(2), "usize"), (ExprId(3), "Arguments"), (ExprId(4), "Arguments")],
            pats: &[(PatId(0), "u8"), (PatId(1), "usize")],
            diags: &[],
        },
    );
    assert_eq!(adjusts(&fx, &c, ExprId(0)), "& -> &u8");
    assert_eq!(adjusts(&fx, &c, ExprId(1)), "& -> &usize");
    assert_eq!(adjusts(&fx, &c, ExprId(2)), "& -> &usize");
}

/// The control: `{x:?}` with no `impl Debug for u8` is E0277 on the argument.
#[test]
fn format_args_without_the_impl_is_unsatisfied() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let x = b.param("x");
    b.param("w");
    let e0 = b.local(x);
    let e1 = b.format_args(&[e0], &[(0, FmtTrait::Debug)]);
    let root = b.block(&[], Some(e1));
    let c = check_body(&fx, &b, it.show, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[(ExprId(0), "u8"), (ExprId(1), "Arguments"), (ExprId(2), "Arguments")],
            pats: &[(PatId(0), "u8"), (PatId(1), "usize")],
            diags: &[(DiagCode::Unsatisfied, ExprId(0))],
        },
    );
}

/// ```text
/// fn count_derefs(x: u8, r: &usize, rr: &&usize, mr: &mut usize, d: MyDeref) -> Arguments {
///     let w = 4;
///     let rw = &4;
///     format_args!("{x:r$}{x:rr$}{x:mr$}{x:d$}{x:w$}{x:rw$}")
/// }
/// ```
/// Width/precision (`FmtTrait::Count`) deref-coerces `&arg` to `&usize`, matching
/// `Argument::from_usize(&arg)`: `&usize`, `&&usize`, `&mut usize`, `Deref<Target = usize>`,
/// unsuffixed `w = 4`, and `&4` all succeed with the exact deref + shared borrow adjustments.
#[test]
fn format_args_count_deref_coerces_references_and_infers_ints() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let x = b.param("x");
    let r = b.param("r");
    let rr = b.param("rr");
    let mr = b.param("mr");
    let d = b.param("d");
    let w = b.bind("w");
    let w_init = b.int(None);
    let rw = b.bind("rw");
    let rw_lit = b.int(None);
    let rw_init = b.borrow(crate::ids::Mutbl::Not, rw_lit);
    let s0 = b.let_(w, None, w_init);
    let s1 = b.let_(rw, None, rw_init);
    let e_x = b.local(x);
    let e_r = b.local(r);
    let e_rr = b.local(rr);
    let e_mr = b.local(mr);
    let e_d = b.local(d);
    let e_w = b.local(w);
    let e_rw = b.local(rw);
    let fmt = b.format_args(
        &[e_x, e_r, e_rr, e_mr, e_d, e_w, e_rw],
        &[
            (1, FmtTrait::Count),
            (0, FmtTrait::Display),
            (2, FmtTrait::Count),
            (0, FmtTrait::Display),
            (3, FmtTrait::Count),
            (0, FmtTrait::Display),
            (4, FmtTrait::Count),
            (0, FmtTrait::Display),
            (5, FmtTrait::Count),
            (0, FmtTrait::Display),
            (6, FmtTrait::Count),
            (0, FmtTrait::Display),
        ],
    );
    let root = b.block(&[s0, s1], Some(fmt));
    let c = check_body(&fx, &b, it.count_derefs, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[
                (w_init, "usize"),
                (rw_lit, "usize"),
                (rw_init, "&usize"),
                (e_x, "u8"),
                (e_r, "&usize"),
                (e_rr, "&&usize"),
                (e_mr, "&mut usize"),
                (e_d, "MyDeref"),
                (e_w, "usize"),
                (e_rw, "&usize"),
                (fmt, "Arguments"),
                (root, "Arguments"),
            ],
            pats: &[
                (x, "u8"),
                (r, "&usize"),
                (rr, "&&usize"),
                (mr, "&mut usize"),
                (d, "MyDeref"),
                (w, "usize"),
                (rw, "&usize"),
            ],
            diags: &[],
        },
    );
    assert_eq!(adjusts(&fx, &c, e_x), "& -> &u8");
    assert_eq!(adjusts(&fx, &c, e_r), "* -> usize, & -> &usize");
    assert_eq!(adjusts(&fx, &c, e_rr), "* -> &usize, * -> usize, & -> &usize");
    assert_eq!(adjusts(&fx, &c, e_mr), "* -> usize, & -> &usize");
    assert_eq!(adjusts(&fx, &c, e_d), "*Deref -> usize, & -> &usize");
    assert_eq!(adjusts(&fx, &c, e_w), "& -> &usize");
    assert_eq!(adjusts(&fx, &c, e_rw), "* -> usize, & -> &usize");
}

/// Negative controls for `FmtTrait::Count`: `i32`, `&i32`, and `&str` do not deref-coerce to
/// `&usize` and each must produce E0308 (`DiagCode::Mismatch`) on the count argument.
#[test]
fn format_args_count_rejects_non_usize_types_with_mismatch() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let x = b.param("x");
    let i = b.param("i");
    let ri = b.param("ri");
    let s = b.param("s");
    let e_x = b.local(x);
    let e_i = b.local(i);
    let e_ri = b.local(ri);
    let e_s = b.local(s);
    let fmt = b.format_args(
        &[e_x, e_i, e_ri, e_s],
        &[
            (1, FmtTrait::Count),
            (0, FmtTrait::Display),
            (2, FmtTrait::Count),
            (0, FmtTrait::Display),
            (3, FmtTrait::Count),
            (0, FmtTrait::Display),
        ],
    );
    let root = b.block(&[], Some(fmt));
    let c = check_body(&fx, &b, it.count_bad, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[(e_x, "u8"), (e_i, "i32"), (e_ri, "&i32"), (e_s, "&str"), (fmt, "Arguments"), (root, "Arguments")],
            pats: &[(x, "u8"), (i, "i32"), (ri, "&i32"), (s, "&str")],
            diags: &[
                (DiagCode::Mismatch, e_i),
                (DiagCode::Mismatch, e_ri),
                (DiagCode::Mismatch, e_s),
            ],
        },
    );
}

/// An argument used BOTH as a formatted value (`Display`) and as a count (`FmtTrait::Count`) must
/// check `Display` on its original type (not on `usize`) and preserve its initial `&T` borrow
/// before the count deref chain:
/// - `BothDerefDisplay` (`Deref<Target = usize>` + `Display`) succeeds with
///   `& -> &BothDerefDisplay, * -> BothDerefDisplay, *Deref -> usize, & -> &usize`.
/// - `MyDeref` (`Deref<Target = usize>`, NO `Display`) fails with `DiagCode::Unsatisfied` (E0277),
///   even though `usize: Display` holds.
#[test]
fn format_args_dual_use_checks_display_on_original_type_not_usize() {
    let (fx, it) = world();
    let mut b = Bb::new();
    let d = b.param("d");
    let both = b.param("b");
    let e_d = b.local(d);
    let e_both = b.local(both);
    let fmt = b.format_args(
        &[e_d, e_both],
        &[
            (0, FmtTrait::Count),
            (0, FmtTrait::Display),
            (1, FmtTrait::Count),
            (1, FmtTrait::Display),
        ],
    );
    let root = b.block(&[], Some(fmt));
    let c = check_body(&fx, &b, it.dual_use, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want {
            exprs: &[(e_d, "MyDeref"), (e_both, "BothDerefDisplay"), (fmt, "Arguments"), (root, "Arguments")],
            pats: &[(d, "MyDeref"), (both, "BothDerefDisplay")],
            diags: &[(DiagCode::Unsatisfied, e_d)],
        },
    );
    assert_eq!(adjusts(&fx, &c, e_both), "& -> &BothDerefDisplay, * -> BothDerefDisplay, *Deref -> usize, & -> &usize");
}
