//! Fallback and the end sequence.
//!
//! Follows rustc_hir_typeck/src/lib.rs (`typeck_with_inspect`'s order after the walk) and
//! fallback.rs (`type_inference_fallback`, `fallback_types`, `fallback_if_possible`,
//! `calculate_diverging_fallback`); Relax maps them in typeck.rs (`fallback`, `finish`).
//!
//! After the walk, in rustc's order: select where possible; fallback (an integer variable left
//! is `i32`, a float `f64`, a diverging variable `()` before edition 2024 and `!` from it; when
//! the body already has errors, every variable left is the error type instead), and select
//! again if anything fell back; the deferred casts; select; closure analysis; select all
//! (ambiguity is E0282 when there is no other error); then writeback.
//!
//! First cut, said where it differs: the diverging fallback is by edition only; rustc before
//! 2024 also keeps `()` only where the variable is not constrained by a "spontaneous" `!`
//! coercion path (`calculate_diverging_fallback`'s unsafe-infer-vars and the lint around it).

use core::alloc::Allocator;

use crate::error::Stop;
use crate::ids::Edition;
use crate::machine::Machine;
use crate::ty::{TyView, TypeCtx, Var, VarKind};
use crate::world::World;

impl<W: World, C: TypeCtx, A: Allocator + Clone> Machine<'_, '_, '_, W, C, A> {
    /// `type_inference_fallback`. Returns whether anything fell back.
    pub(crate) fn fallback(&mut self) -> Result<bool, Stop> {
        self.select()?;
        let tainted = self.errors != 0;
        let mut any = false;
        for i in 0..self.numeric.len() {
            let v = self.numeric[i];
            let t = self.ty.shallow_resolve(v);
            let to = match self.ty.view(t) {
                _ if tainted && matches!(self.ty.view(t), TyView::Infer(_)) => self.common.err,
                TyView::Infer(Var { kind: VarKind::Int, .. }) => self.common.i32_,
                TyView::Infer(Var { kind: VarKind::Float, .. }) => self.common.f64_,
                _ => continue,
            };
            self.unify(t, to)?;
            any = true;
        }
        let diverging_to = if self.body.edition >= Edition::E2024 { self.common.never } else { self.common.unit };
        for i in 0..self.diverging.len() {
            let v = self.diverging[i];
            let t = self.ty.shallow_resolve(v);
            if !matches!(self.ty.view(t), TyView::Infer(Var { kind: VarKind::Ty, .. })) {
                continue;
            }
            let to = if tainted { self.common.err } else { diverging_to };
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
        self.select()?;
        self.analyze_closures()?;
        self.select_all()
    }
}
