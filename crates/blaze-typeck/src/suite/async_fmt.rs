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
}

fn world() -> (Fx, Items) {
    let mut world = Fx::new();
    let fx = &mut world;
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let display = fx.tr("Display", 1, Vec::new(), &[]);
    fx.lang(LangItem::FmtDisplay, display);
    let debug = fx.tr("Debug", 1, Vec::new(), &[]);
    fx.lang(LangItem::FmtDebug, debug);
    let arguments = fx.strukt("Arguments", 0, &[]);
    fx.lang(LangItem::FormatArguments, arguments);
    let (u8_, usize_) = (fx.u8(), fx.usize());
    fx.imp("impl Display for u8", 0, u8_, Some((display, &[])), &[], Vec::new());

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

    let arguments_t = fx.adt(arguments, &[]);
    let wait = fx.func("wait", 0, &[ready_t], u8_, Vec::new());
    let show = fx.func("show", 0, &[u8_, usize_], arguments_t, Vec::new());
    fx.finish();
    (world, Items { wait, show })
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
