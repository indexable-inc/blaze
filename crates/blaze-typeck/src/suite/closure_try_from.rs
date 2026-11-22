//! `?` in a closure whose return type comes only from the callee's `FnOnce(..) -> Result<(),
//! Error>` bound, the error converted by a blanket `From` (eyre's `impl<E: StdError + Send +
//! Sync + 'static> From<E> for Report`, error.rs:490): the reduction of tempo's
//! crates/zones/node/tests/it/utils.rs:2658 (`.extend_rpc_modules(move |ctx| { ..
//! module.register_method(..)?; .. ctx.modules.replace_configured(module)?; Ok(()) })`, reth's
//! `F: FnOnce(RpcContext<..>) -> eyre::Result<()> + Send + 'static`), which blaze reported as
//! "found `Error`".
//!
//! ```text
//! (rung_7c's world: Sized, Infallible, ControlFlow, Try, FromResidual, From, Result, `impl<T>
//!  From<T> for T`, `impl<T, E, F: From<E>> FromResidual<Result<Infallible, E>> for Result<T, F>`,
//!  struct Io, struct Error, struct Other, `impl Io { fn other(&self) -> Result<u8, Other> }`)
//! #[lang = "fn_once"] trait FnOnce<Args> { type Output; }   FnMut, Fn as core declares them
//! trait StdError {}   struct RegErr;   impl StdError for RegErr {}
//! impl<E: StdError> From<E> for Error {}                        // eyre's blanket
//! impl Io { fn reg(&self) -> Result<u8, RegErr>; }
//! trait Id { type Out; }   impl Id for Io { type Out = Io; }  // reth's `CB::Components`
//! fn ext<F: FnOnce(<Io as Id>::Out) -> Result<(), Error>>(f: F) -> u8;
//! fn hook() -> u8 { ext(|io| { io.reg()?; Ok(()) }) }
//! fn hook_bad() -> u8 { ext(|io| { io.other()?; Ok(()) }) }  // `Other` is no `StdError`
//! fn coerce<F: FnOnce(<Io as Id>::Out, &mut u8) -> &u8>(f: F) -> u8;
//! fn hook_coerce() -> u8 { coerce(|_io, m| m) }               // `&mut u8` coerces to `&u8`
//! fn coerce_bad<F: FnOnce(<Io as Id>::Out, &mut u16) -> &u8>(f: F) -> u8;
//! fn hook_coerce_bad() -> u8 { coerce_bad(|_io, m| m) }       // `&mut u16` does not
//! ```
//!
//! THE CAUSE FOUND: the closure's expected output comes from the pending
//! `<F as FnOnce<Args>>::Output == R` (infer/mod.rs `closure_hint`), which was taken only when
//! its `Args` was the SAME list as the `F: FnOnce<Args>` goal's. Each clause of a bound is
//! lowered on its own, and lowering replaces an alias by a fresh variable and a projection
//! (machine.rs `lower`), so with an alias in `Args` (reth's `RpcContext<'_, NodeAdapter<T,
//! CB::Components>, AO::EthApi>`) the two lists differ and the output was dropped: the closure's
//! return type was then made by its body alone. Now the projection is taken when its `Args` CAN
//! equal the inputs (a rolled-back unification).
//!
//! The rule: rustc_hir_typeck closure.rs `deduce_closure_signature` takes the pending
//! `F: FnOnce<(Io,)>` and its `Output` projection as the closure's signature BEFORE the body is
//! checked, so the body's `return FromResidual::from_residual(r)` (the `?` desugaring) is
//! checked against `Result<(), Error>`: `Result<(), Error>: FromResidual<Result<Infallible,
//! RegErr>>` needs `Error: From<RegErr>`, met by the blanket (the identity impl does not unify).
//! `hook` checks clean; `hook_bad` is E0277 (`Error: From<Other>`: neither impl holds).
//! Were the output not deduced, the `From` goal would have a variable for its `Self` and must
//! stay pending until the call's bound fixes it, never be judged on its own.

use std::vec::Vec;

use super::check_body;
use crate::diag::DiagCode;
use crate::fixture::{Bb, Fx, rung_7c};
use crate::ids::{Abi, FnPtrHeader, LangItem, Mutbl, Safety};
use crate::infer::InferPlant;
use crate::machine::Plant;

/// rung_7c's world with the items of the module doc added.
fn world() -> Fx {
    let (mut world, s) = rung_7c();
    // Through `&mut Fx`, as rung_7c builds (`fx.adt` on `Fx` would pick `World::adt`).
    let fx = &mut world;
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

    let (from_tr, error_k, io_k, io_impl) = (fx.key_of("From"), fx.key_of("Error"), fx.key_of("Io"), fx.key_of("impl Io"));
    let error = fx.adt(error_k, &[]);
    let io = fx.adt(io_k, &[]);
    let std_error = fx.tr("StdError", 1, Vec::new(), &[]);
    let reg_err_k = fx.strukt("RegErr", 0, &[]);
    let reg_err = fx.adt(reg_err_k, &[]);
    fx.imp("impl StdError for RegErr", 0, reg_err, Some((std_error, &[])), &[], Vec::new());
    let e_std = fx.bound(std_error, &[p0]);
    fx.imp("impl<E: StdError> From<E> for Error", 1, error, Some((from_tr, &[p0])), &[], std::vec![e_std]);
    let u8_ = fx.u8();
    let r_io = fx.r(Mutbl::Not, io);
    let res_u8_reg = fx.adt(s.result, &[u8_, reg_err]);
    fx.impl_fn(io_impl, "reg", true, &[r_io], res_u8_reg);

    // trait Id { type Out; }  impl Id for Io { type Out = Io; }: reth's `CB::Components` in
    // the bound's arguments, an alias each clause lowers to its own variable.
    let id = fx.tr("Id", 1, Vec::new(), &["Out"]);
    fx.imp("impl Id for Io", 0, io, Some((id, &[])), &[("Out", io)], Vec::new());
    let io_out = fx.alias(id, "Out", &[io]);

    // fn ext<F: FnOnce(<Io as Id>::Out) -> Result<(), Error>>(f: F) -> u8
    let unit = fx.unit();
    let res_unit_error = fx.adt(s.result, &[unit, error]);
    let args = fx.tuple(&[io_out]);
    let f_once = fx.bound(fn_once, &[p0, args]);
    let f_out = fx.proj(fn_once, &[p0, args], "Output", res_unit_error);
    fx.func("ext", 1, &[p0], u8_, std::vec![f_once, f_out]);
    fx.func("hook", 0, &[], u8_, Vec::new());
    fx.func("hook_bad", 0, &[], u8_, Vec::new());

    // fn coerce<F: FnOnce(<Io as Id>::Out, &mut u8) -> &u8>(f: F) -> u8, and the same with
    // `&mut u16` for the twin.
    let u16_ = fx.u16();
    for (name, int) in [("coerce", u8_), ("coerce_bad", u16_)] {
        let m_int = fx.r(Mutbl::Mut, int);
        let r_u8 = fx.r(Mutbl::Not, u8_);
        let args = fx.tuple(&[io_out, m_int]);
        let f_once = fx.bound(fn_once, &[p0, args]);
        let f_out = fx.proj(fn_once, &[p0, args], "Output", r_u8);
        fx.func(name, 1, &[p0], u8_, std::vec![f_once, f_out]);
        fx.func(&std::format!("hook_{name}"), 0, &[], u8_, Vec::new());
    }
    fx.finish();
    world
}

/// `fn <owner>() -> u8 { <callee>(|_io, m| m) }`: the diagnostics' codes.
fn coerce(fx: &Fx, owner: &str, callee: &str) -> Vec<DiagCode> {
    let mut b = Bb::new();
    let f = b.path(&[callee]);
    let io = b.bind("_io");
    let m = b.bind("m");
    let m_e = b.local(m);
    let closure = b.closure(&[io, m], m_e);
    let outer = b.call(f, &[closure]);
    let root = b.block(&[], Some(outer));
    let c = check_body(fx, &b, fx.key_of(owner), root, Plant::None, InferPlant::None);
    c.diags.iter().map(|d| d.code).collect()
}

/// The closure's return type is the bound's `&u8` BEFORE its body is checked, though the
/// bound's `Args` holds an alias: the body `m: &mut u8` coerces to it (the Reference,
/// "Coercion sites": a closure body is a function body). Before the fix (rules/closure.rs
/// `deduce_sig`, infer/mod.rs `closure_hint` comparing the trait goal's and the projection's
/// `Args` as lists) the output was not taken, the body made it `&mut u8`, and
/// `<closure as FnOnce<..>>::Output == &u8` failed: a false mismatch.
#[test]
fn a_closures_output_is_taken_from_a_bound_whose_args_hold_an_alias() {
    let fx = world();
    assert_eq!(coerce(&fx, "hook_coerce", "coerce"), Vec::<DiagCode>::new());
}

/// Negative twin: `m: &mut u16` does not coerce to `&u8`: one E0308 at the body, the output
/// being the bound's `&u8` (so the `Output` projection itself holds).
#[test]
fn a_closure_body_that_cannot_coerce_to_the_bounds_output_is_refused() {
    let fx = world();
    assert_eq!(coerce(&fx, "hook_coerce_bad", "coerce_bad"), std::vec![DiagCode::Mismatch]);
}

/// `fn <owner>() -> u8 { ext(|io| { io.<method>()?; Ok(()) }) }`: the diagnostics' codes.
fn hook(fx: &Fx, owner: &str, method: &str) -> Vec<DiagCode> {
    let mut b = Bb::new();
    let ext = b.path(&["ext"]);
    let io = b.bind("io");
    let io_e = b.local(io);
    let call = b.method(io_e, method, &[]);
    let tried = b.try_op(call);
    let s0 = b.semi(tried);
    let ok = b.path(&["Ok"]);
    let unit = b.tuple(&[]);
    let ok_unit = b.call(ok, &[unit]);
    let body = b.block(&[s0], Some(ok_unit));
    let closure = b.closure(&[io], body);
    let outer = b.call(ext, &[closure]);
    let root = b.block(&[], Some(outer));
    let c = check_body(fx, &b, fx.key_of(owner), root, Plant::None, InferPlant::None);
    c.diags.iter().map(|d| d.code).collect()
}

/// `?` on `Result<u8, RegErr>` in a closure returning the bound's `Result<(), Error>`: the
/// blanket `From<E> for Error` converts. Clean.
#[test]
fn a_closures_question_mark_converts_into_the_bounds_error_type() {
    let fx = world();
    assert_eq!(hook(&fx, "hook", "reg"), Vec::<DiagCode>::new());
}

/// Negative twin: `?` on `Result<u8, Other>`: `Error: From<Other>` has no impl (`Other` is no
/// `StdError`, and the identity impl does not unify): E0277.
#[test]
fn a_closures_question_mark_with_no_from_is_unmet() {
    let fx = world();
    assert_eq!(hook(&fx, "hook_bad", "other"), std::vec![DiagCode::Unsatisfied]);
}
