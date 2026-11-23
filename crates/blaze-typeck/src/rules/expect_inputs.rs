//! A call's expected type, carried to its arguments.
//!
//! The Reference, "Type coercions", "Coercion sites": "Arguments for function calls: The value
//! being coerced is the actual parameter, and it is coerced to the type of the formal
//! parameter." What the formal parameter's type IS, when the callee is generic, can depend on
//! the call's own expected type:
//!
//! ```text
//! impl<P: Clone, N> Inst<&P, N> {
//!     fn with_cloned_provider(self) -> Inst<P, N> {
//!         Inst { provider: ::core::clone::Clone::clone(&self.provider), .. }
//!     }
//! }
//! ```
//!
//! `Clone::clone` is `fn(&Self) -> Self` (library/core/src/clone.rs). The field's type `P` is
//! the call's expected type, so `Self = P` and the formal parameter is `&P`; the argument
//! `&self.provider` is `&&P` and coerces to it ("Coercion types": "`&T` or `&mut T` to `&U` if
//! `T` implements `Deref<Target = U>`"). Typed with no word from the expected type, the
//! argument alone gives `Self = &P`, and the call's `&P` is then refused against `P`: tempo's
//! 33 "diag E0308" at `with_cloned_provider` (alloy's `sol!`).
//!
//! This is rustc's `expected_inputs_for_expected_output` (rustc_hir_typeck
//! fn_ctxt/checks.rs), the piece rules/call.rs named as omitted. In a probe
//! (`Machine::probe`: the machine's one snapshot, always rolled back) the callee's instantiated
//! output is unified with the expected type and every formal input is resolved; nothing of the
//! probe is kept but those resolved types. The output is NOT bound to the expected type: the
//! call's result may still coerce to it (`let b: Box<dyn Tr> = Box::new(v)` must leave `T = V`).
//! A resolved input is then the type its argument must coerce to (`check_argument_types`:
//! `coerced_ty = expectation.only_has_type().unwrap_or(formal_input_ty)`, then the formal is
//! made equal to it), so the formal is unified with it before the argument is typed, and the
//! argument is checked against the formal as the coercion site it already was
//! (`Machine::next_arg`, `Then::Coerce`).
//!
//! A resolved input that is `str`, a slice or a trait object binds no formal
//! (`Expectation::rvalue_hint`: `ExpectRvalueLikeUnsized` has no `only_has_type`): `Box::new(v)`
//! expected as `Box<dyn Tr>` resolves its input to `dyn Tr`, which no argument has. It is the
//! argument's hint instead (`Machine::unsized_arg_hints`, taken by `next_arg`), as in rustc,
//! where the argument is checked with that expectation and coerced to the formal: a closure
//! takes its signature from it (`Box::new(|tx| tx.clone())` expected as
//! `Box<dyn FnMut(&T)>`: rules/closure.rs `deduce_sig`).
//!
//! A resolved input says nothing when it holds an inference variable THE PROBE MADE. rustc
//! replaces those by fresh ones (`fudge_inference_if_ok`); the table here has no such
//! operation, so such an input is not taken. A variable made BEFORE the probe (the callee's
//! own generic arguments) is the same variable after the rollback, so an input holding only
//! those is taken (`TypeCtx::list_has_infer_since`): `vec![a, b]` is
//! `box_assume_init_into_vec_unsafe::<T, N>(write_box_via_move(Box::new_uninit(), [a, b]))`
//! (the pinned library/alloc/src/macros.rs), whose input `Box<MaybeUninit<[P; ?N]>>` under an
//! expected `Vec<P>` still holds its own `?N`, and only through it does `P` reach the array's
//! elements as their coercion target (rung s6-async-04; suite/expect_fudge.rs). Said plainly:
//! this takes fewer hints than rustc does (an input holding a variable the probe made; an ADT
//! whose LAST FIELD is unsized is also not looked through: `struct_tail` is not asked).

use blaze_own::{Alloc, Vec};

use crate::error::Stop;
use crate::ids::ExprId;
use crate::machine::{Expectation, Machine};
use crate::ty::{Ty, TyList, TyView, TypeCtx};
use crate::world::World;

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// Binds each formal input of a callee (`inputs`, `output`: its instantiated signature) to
    /// the type the call's expected type gives it, before any argument is typed. Nothing is
    /// bound when the call has no expected type, when `output` cannot be the expected type
    /// (the mismatch is reported once by whoever gave the expectation), or for an input the
    /// expected type does not decide.
    pub(crate) fn expected_inputs(
        &mut self,
        expect: Expectation,
        inputs: TyList,
        output: Ty,
        args: &[ExprId],
    ) -> Result<(), Stop> {
        let expected = match expect {
            Expectation::HasType(t) => t,
            // A coroutine body carries a sized destination output as a non-reporting rvalue
            // hint. Use that hint to infer generic inputs while leaving the output equality to
            // the enclosing coercion's projection obligation. Ordinary unsized rvalue hints are
            // for the argument only and do not determine a callee's output.
            Expectation::RvalueLikeUnsized(t) => {
                let t = self.ty.shallow_resolve(t);
                if matches!(self.view(t), TyView::Str | TyView::Slice(_) | TyView::Dynamic(_)) {
                    return Ok(());
                }
                t
            }
            Expectation::None | Expectation::Castable(_) => return Ok(()),
        };
        let n = self.ty.list_len(inputs);
        if n == 0 || self.is_ty_var(expected) || self.is_err(expected) || self.is_err(output) {
            return Ok(());
        }
        // Per input: `HasType(t)` binds the formal to `t`; `RvalueLikeUnsized(t)` is the
        // argument's hint only (`rvalue_hint`: `str`, a slice, a trait object).
        let mut hints: Vec<Option<Expectation>, Alloc<'a>> = Vec::with_capacity_in(n as usize, self.alloc);
        let outer_ev = self.active_inferred_err_var();
        let fits = self.probe(|m| -> Result<bool, Stop> {
            // The variables that outlive the probe: those that exist before it.
            let mark = m.ty.var_mark();
            if !m.unify(output, expected)? {
                return Ok(false);
            }
            for i in 0..n {
                let formal = m.ty.list_get(inputs, i);
                let resolved = m.ty.resolve(formal)?;
                if let Some(ev) = outer_ev
                    && m.ty.shallow_resolve(ev) == resolved
                {
                    hints.push(Some(Expectation::HasType(ev)));
                    continue;
                }
                let one = m.mk_list(&[resolved])?;
                let says_nothing = m.ty.list_has_infer_since(one, mark) || matches!(m.ty.view(resolved), TyView::Error);
                hints.push(if says_nothing { None } else { Some(m.rvalue_hint(resolved)?) });
            }
            Ok(true)
        })?;
        if !fits {
            return Ok(());
        }
        for (i, hint) in hints.iter().enumerate() {
            match *hint {
                Some(Expectation::HasType(t)) => {
                    let formal = self.ty.list_get(inputs, i as u32);
                    // `t` is `formal` under the probe's bindings, so the two unify; `false`
                    // binds nothing (`TypeCtx::unify`), and the argument is then checked
                    // against the formal as written, where any mismatch is reported.
                    self.unify(formal, t)?;
                }
                Some(Expectation::RvalueLikeUnsized(t)) => {
                    // An argument past the formals (E0061, reported by `start_args`) has none.
                    let Some(&arg) = args.get(i) else { continue };
                    match self.unsized_arg_hints.iter_mut().find(|(e, _)| *e == arg) {
                        Some(row) => row.1 = t,
                        None => self.unsized_arg_hints.push((arg, t)),
                    }
                }
                Some(Expectation::None | Expectation::Castable(_)) | None => {}
            }
        }
        Ok(())
    }

    /// The unsized type the call's expected type gave argument `arg` as its hint
    /// ([`Machine::expected_inputs`]), taken once by `next_arg`.
    pub(crate) fn take_unsized_arg_hint(&mut self, arg: ExprId) -> Option<Ty> {
        let at = self.unsized_arg_hints.iter().position(|&(e, _)| e == arg)?;
        Some(self.unsized_arg_hints.remove_shift(at).1)
    }
}
