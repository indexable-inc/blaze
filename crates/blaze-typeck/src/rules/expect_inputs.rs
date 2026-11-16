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
//! Two kinds of resolved input say nothing, as in rustc:
//! - one that is `str`, a slice or a trait object (`Expectation::rvalue_hint`:
//!   `ExpectRvalueLikeUnsized` has no `only_has_type`): `Box::new(v)` expected as
//!   `Box<dyn Tr>` resolves its input to `dyn Tr`, which no argument has;
//! - one that still holds an inference variable. rustc replaces the variables the probe made
//!   by fresh ones (`fudge_inference_if_ok`); the table here has no such operation, and a type
//!   holding none is the same type after the rollback, so only those are taken. Said plainly:
//!   this takes fewer hints than rustc does (an ADT whose LAST FIELD is unsized is also not
//!   looked through: `struct_tail` is not asked).

use blaze_own::{Alloc, Vec};

use crate::error::Stop;
use crate::machine::{Expectation, Machine};
use crate::ty::{Ty, TyList, TyView, TypeCtx};
use crate::world::World;

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// Binds each formal input of a callee (`inputs`, `output`: its instantiated signature) to
    /// the type the call's expected type gives it, before any argument is typed. Nothing is
    /// bound when the call has no expected type, when `output` cannot be the expected type
    /// (the mismatch is reported once, at the call, by whoever gave the expectation), or for
    /// an input the expected type does not decide.
    pub(crate) fn expected_inputs(&mut self, expect: Expectation, inputs: TyList, output: Ty) -> Result<(), Stop> {
        let Some(expected) = expect.only_has_type() else { return Ok(()) };
        let n = self.ty.list_len(inputs);
        if n == 0 || self.is_ty_var(expected) || self.is_err(expected) || self.is_err(output) {
            return Ok(());
        }
        let mut hints: Vec<Option<Ty>, Alloc<'a>> = Vec::with_capacity_in(n as usize, self.alloc);
        let fits = self.probe(|m| -> Result<bool, Stop> {
            if !m.unify(output, expected)? {
                return Ok(false);
            }
            for i in 0..n {
                let formal = m.ty.list_get(inputs, i);
                let resolved = m.ty.resolve(formal)?;
                let one = m.mk_list(&[resolved])?;
                let says_nothing = m.ty.list_has_infer(one)
                    || matches!(m.ty.view(resolved), TyView::Str | TyView::Slice(_) | TyView::Dynamic(_) | TyView::Error);
                hints.push(if says_nothing { None } else { Some(resolved) });
            }
            Ok(true)
        })?;
        if !fits {
            return Ok(());
        }
        for (i, hint) in hints.iter().enumerate() {
            let Some(hint) = *hint else { continue };
            let formal = self.ty.list_get(inputs, i as u32);
            // `hint` is `formal` under the probe's bindings, so the two unify; `false` binds
            // nothing (`TypeCtx::unify`), and the argument is then checked against the formal
            // as written, where any mismatch is reported.
            self.unify(formal, hint)?;
        }
        Ok(())
    }
}
