//! Fallback and the end sequence.
//!
//! Follows rustc_hir_typeck/src/lib.rs (`typeck_with_inspect`'s order after the walk) and
//! fallback.rs (`type_inference_fallback`), checked against Relax's port of them @8d1a764
//! (types/src/typeck.rs, the end of the body's check: "diverging fallback (rustc fallback.rs)"
//! and "integer/float literal fallback happens once, at the end").
//!
//! After the walk, in that order: select where possible; the diverging fallback (a variable a
//! `!` value flowed into and nothing else decided is `!`, in every edition: the pinned rustc
//! 1.100 made it so, where earlier ones gave `()` before edition 2024); then every integer
//! variable left is `i32` and every float `f64`; select; the deferred casts; select; closure
//! analysis; select all (ambiguity is E0282 when there is no other error); then writeback.
//!
//! Fallback does not look at whether the body has errors: as in Relax's port, a variable left
//! unresolved after it becomes the error type at writeback, which reports E0282 only when the
//! body has no other error.

use crate::error::Stop;
use crate::machine::Machine;
use crate::ty::{TyView, TypeCtx, Var, VarKind};
use crate::world::World;

impl<W: World, C: TypeCtx> Machine<'_, '_, '_, '_, W, C> {
    /// `type_inference_fallback`. Returns whether anything fell back.
    pub(crate) fn fallback(&mut self) -> Result<bool, Stop> {
        self.select()?;
        let mut any = false;
        for i in 0..self.diverging.len() {
            let t = self.ty.shallow_resolve(self.diverging[i]);
            if matches!(self.ty.view(t), TyView::Infer(Var { kind: VarKind::Ty, .. })) {
                self.unify(t, self.common.never)?;
                any = true;
            }
        }
        for i in 0..self.numeric.len() {
            let t = self.ty.shallow_resolve(self.numeric[i]);
            let to = match self.ty.view(t) {
                TyView::Infer(Var { kind: VarKind::Int, .. }) => self.common.i32_,
                TyView::Infer(Var { kind: VarKind::Float, .. }) => self.common.f64_,
                _ => continue,
            };
            self.unify(t, to)?;
            any = true;
        }
        if any {
            self.select()?;
        }
        Ok(any)
    }

    /// Everything after the walk up to writeback, in rustc's order.
    pub(crate) fn end_sequence(&mut self) -> Result<(), Stop> {
        self.fallback()?;
        self.check_casts()?;
        self.check_asms()?;
        self.select()?;
        self.analyze_closures()?;
        // An async block's captures (rules/async_fn.rs): rows of the same table.
        self.analyze_coroutine_captures()?;
        self.analyze_futures()?;
        // Constant promotion (rules/promote.rs): after every type the rule reads is fixed.
        self.analyze_promotions()?;
        // A mutable place through an overloaded deref calls `DerefMut` (rules/mut_places.rs);
        // before `select_all`, so the `DerefMut` goals it registers are proven with the rest.
        self.mut_places()?;
        // Each `.await`'s callee: the `IntoFuture` impl the body's solver selects
        // (rules/await_impl.rs); before `select_all`, which reports an ambiguous operand.
        self.await_impls()?;
        self.select_all()
    }
}
