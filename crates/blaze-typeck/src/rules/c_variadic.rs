//! Calls of a C-variadic fn (`extern "C" { fn printf(format: *const c_char, ...) -> c_int; }`):
//! at least the declared arguments, each checked against its parameter as any call's; every
//! argument past them is checked with no expectation (its own type), then by C's default
//! argument promotions.
//!
//! The rules, quoted:
//! - The Rust Reference, "External blocks", [items.extern.variadic.syntax]: "Functions within
//!   external blocks may be made variadic by specifying ... as the last parameter." and "Passing
//!   an unexpected number of arguments or arguments of unexpected type to a variadic function
//!   may lead to undefined behavior." So fewer arguments than declared is E0061, more is the
//!   variadic part.
//! - The error index, E0617: "Attempted to pass an invalid type of variable into a variadic
//!   function." ... "Certain Rust types must be cast before passing them to a variadic function,
//!   because of arcane ABI rules dictated by the C standard." Its example: `printf(.., 0f32)` is
//!   "error: cannot pass an `f32` to variadic function, cast to `c_double`".
//! - The pinned library, library/core/src/ffi/va_list.rs (`VaArgSafe`): "When C passes variable
//!   arguments, signed integers smaller than [`c_int`] are promoted to [`c_int`], unsigned
//!   integers smaller than [`c_uint`] are promoted to [`c_uint`], and [`c_float`] is promoted to
//!   [`c_double`]." Rust makes no such promotion implicitly: the narrow type is refused, the
//!   program writes the cast.
//!
//! So refused (E0617, [`Promote`] naming the cast): `f32` (to `c_double`); `i8`, `i16` and
//! `bool` (to `c_int`); `u8`, `u16` (to `c_uint`); a fn item (to its fn pointer type: a fn item
//! is zero-sized and has no C form). Everything else passes here, an integer or float literal's
//! variable included (it falls back to `i32` or `f64`, both promoted types already).

use crate::diag::{About, At, DiagCode};
use crate::error::Stop;
use crate::ids::{ExprId, FloatTy, IntTy, ItemKey, UintTy};
use crate::machine::Machine;
use crate::ty::{Ty, TyList, TyView, TypeCtx};
use crate::world::World;
use blaze_own::Vec;

/// The cast a refused variable argument needs (E0617's "cast to ..."), by C's default
/// argument promotions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Promote {
    /// `f32` to `c_double`.
    ToCDouble,
    /// `i8`, `i16`, `bool` to `c_int`.
    ToCInt,
    /// `u8`, `u16` to `c_uint`.
    ToCUint,
    /// A fn item to its fn pointer type.
    ToFnPtr,
}

/// Whether a variable argument of (resolved) type `t` needs a cast first, and which: C's
/// default argument promotions as Rust refuses them (module doc).
pub(crate) fn promotion_refusal(t: TyView) -> Option<Promote> {
    match t {
        TyView::Float(FloatTy::F32) => Some(Promote::ToCDouble),
        TyView::Int(IntTy::I8 | IntTy::I16) | TyView::Bool => Some(Promote::ToCInt),
        TyView::Uint(UintTy::U8 | UintTy::U16) => Some(Promote::ToCUint),
        TyView::FnDef(..) => Some(Promote::ToFnPtr),
        _ => None,
    }
}

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// The formal inputs of a call of fn item `k` with `n` arguments whose declared inputs are
    /// `inputs`: for a C-variadic `k` called with more, `inputs` then one fresh variable per
    /// extra argument (each extra argument is checked with no expectation and coerced to its
    /// own type, so nothing about it is inferred from the callee); otherwise `inputs` as they
    /// are (fewer than declared stays E0061 in `start_args`).
    pub(crate) fn c_variadic_inputs(&mut self, k: ItemKey, inputs: TyList, n: usize) -> Result<TyList, Stop> {
        let variadic = match self.reads.fn_sig(k)? {
            Some(sig) => sig.c_variadic,
            None => false,
        };
        let declared = self.ty.list_len(inputs) as usize;
        if !variadic || n <= declared {
            return Ok(inputs);
        }
        let mut all: Vec<Ty, _> = Vec::with_capacity_in(n, self.alloc);
        for t in self.list(inputs) {
            all.push(t);
        }
        for _ in declared..n {
            let t = self.fresh()?;
            all.push(t);
        }
        self.mk_list(&all)
    }

    /// After every argument of call `e` is checked: if its callee is a C-variadic fn item,
    /// each argument past the declared ones is held to C's default argument promotions
    /// ([`promotion_refusal`]); a refused one is E0617 at that argument, its type the found
    /// type. Nothing else changes: the argument keeps its type (no implicit promotion).
    pub(crate) fn check_c_variadic_args(&mut self, e: ExprId, args: &[ExprId]) -> Result<(), Stop> {
        let Some(&(_, k, _)) = self.callees.iter().rev().find(|c| c.0 == e) else {
            return Ok(());
        };
        let declared = match self.reads.fn_sig(k)? {
            Some(sig) if sig.c_variadic => sig.inputs.len(),
            _ => return Ok(()),
        };
        for &arg in args.iter().skip(declared) {
            let t = self.expr_ty[arg.ix()];
            let t = self.try_structurally_resolve(t)?;
            let view = self.view(t);
            if promotion_refusal(view).is_some() {
                self.diag(DiagCode::VariadicArg, At::Expr(arg), About::found(t));
            }
        }
        Ok(())
    }
}
