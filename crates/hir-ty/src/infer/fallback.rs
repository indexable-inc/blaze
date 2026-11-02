//! Fallback of infer vars to `!` and `i32`/`f64`.

use blaze_ids::bits::Bits;
use rustc_type_ir::{
    TyVid,
    inherent::{IntoKind, Ty as _},
};
use tracing::debug;

use crate::{
    infer::{
        InferenceContext,
        columns::{FrameMap, frame_map},
    },
    next_solver::{Ty, TyKind},
};

impl<'db, 'f> InferenceContext<'db, 'f> {
    pub(super) fn type_inference_fallback(&mut self) {
        debug!(
            "type-inference-fallback start obligations: {:#?}",
            self.table.fulfillment_cx.pending_obligations()
        );

        // All type checking constraints were added, try to fallback unsolved variables.
        self.table.select_obligations_where_possible();

        debug!(
            "type-inference-fallback post selection obligations: {:#?}",
            self.table.fulfillment_cx.pending_obligations()
        );

        let fallback_occurred = self.fallback_types();

        if !fallback_occurred {
            return;
        }

        // We now see if we can make progress. This might cause us to
        // unify inference variables for opaque types, since we may
        // have unified some other type variables during the first
        // phase of fallback. This means that we only replace
        // inference variables with their underlying opaque types as a
        // last resort.
        //
        // In code like this:
        //
        // ```rust
        // type MyType = impl Copy;
        // fn produce() -> MyType { true }
        // fn bad_produce() -> MyType { panic!() }
        // ```
        //
        // we want to unify the opaque inference variable in `bad_produce`
        // with the diverging fallback for `panic!` (e.g. `()` or `!`).
        // This will produce a nice error message about conflicting concrete
        // types for `MyType`.
        //
        // If we had tried to fallback the opaque inference variable to `MyType`,
        // we will generate a confusing type-check error that does not explicitly
        // refer to opaque types.
        self.table.select_obligations_where_possible();
    }

    fn fallback_types(&mut self) -> bool {
        // Check if we have any unresolved variables. If not, no need for fallback.
        let (unresolved_ty, unresolved_int, unresolved_float) =
            self.table.infer_ctxt.unresolved_root_variables();

        if unresolved_ty.is_empty() && unresolved_int.is_empty() && unresolved_float.is_empty() {
            return false;
        }

        let interner = self.interner();
        let mut unresolved_variables = Vec::with_capacity_in(
            unresolved_ty.len() + unresolved_int.len() + unresolved_float.len(),
            self.frame,
        );
        unresolved_variables.extend(unresolved_ty.into_iter().map(|vid| Ty::new_var(interner, vid)));
        unresolved_variables
            .extend(unresolved_int.into_iter().map(|vid| Ty::new_int_var(interner, vid)));
        unresolved_variables
            .extend(unresolved_float.into_iter().map(|vid| Ty::new_float_var(interner, vid)));

        let diverging_fallback = self.calculate_diverging_fallback(&unresolved_variables);

        // We do fallback in two passes, to try to generate
        // better error messages.
        // The first time, we do *not* replace opaque types.
        let mut fallback_occurred = false;
        for ty in unresolved_variables {
            debug!("unsolved_variable = {:?}", ty);
            fallback_occurred |= self.fallback_if_possible(ty, &diverging_fallback);
        }

        fallback_occurred
    }

    // Tries to apply a fallback to `ty` if it is an unsolved variable.
    //
    // - Unconstrained ints are replaced with `i32`.
    //
    // - Unconstrained floats are replaced with `f64`.
    //
    // - Non-numerics that `calculate_diverging_fallback` found diverging are
    //   replaced with `!`.
    //
    // Fallback becomes very dubious if we have encountered
    // type-checking errors. In that case, fallback to Error.
    //
    // Sets `FnCtxt::fallback_has_occurred` if fallback is performed
    // during this call.
    fn fallback_if_possible(
        &mut self,
        ty: Ty<'db>,
        diverging_fallback: &FrameMap<'_, Ty<'db>, Ty<'db>>,
    ) -> bool {
        // Careful: we do NOT shallow-resolve `ty`. We know that `ty`
        // is an unsolved variable, and we determine its fallback
        // based solely on how it was created, not what other type
        // variables it may have been unified with since then.
        //
        // The reason this matters is that other attempts at fallback
        // may (in principle) conflict with this fallback, and we wish
        // to generate a type error in that case. (However, this
        // actually isn't true right now, because we're only using the
        // builtin fallback rules. This would be true if we were using
        // user-supplied fallbacks. But it's still useful to write the
        // code to detect bugs.)
        //
        // (Note though that if we have a general type variable `?T`
        // that is then unified with an integer type variable `?I`
        // that ultimately never gets resolved to a special integral
        // type, `?T` is not considered unsolved, but `?I` is. The
        // same is true for float variables.)
        let fallback = match ty.kind() {
            TyKind::Infer(rustc_type_ir::IntVar(_)) => self.types.types.i32,
            TyKind::Infer(rustc_type_ir::FloatVar(_)) => self.types.types.f64,
            _ => match diverging_fallback.get(&ty) {
                Some(&fallback_ty) => fallback_ty,
                None => return false,
            },
        };
        debug!("fallback_if_possible(ty={:?}): defaulting to `{:?}`", ty, fallback);

        _ = self.demand_eqtype_fixme_no_diag(ty, fallback);
        true
    }

    /// "Diverging" type variables are variables created when we coerce a `!` type into an
    /// unbound type variable `?X`. One that is never constrained falls back to `!`, in every
    /// edition, as rustc's `calculate_diverging_fallback` does at the pinned nightly (its `()`
    /// fallback before 2024, and `#![feature(never_type_fallback)]`'s context-dependent one, are
    /// gone): `Some(return)` is an `Option<!>`, and `|| panic!()` returns `!`.
    fn calculate_diverging_fallback(
        &self,
        unresolved_variables: &[Ty<'db>],
    ) -> FrameMap<'f, Ty<'db>, Ty<'db>> {
        debug!("calculate_diverging_fallback({:?})", unresolved_variables);

        // The root vids of the type variables that (a) are the target of a coercion from a `!`
        // type and (b) have not yet been solved.
        let interner = self.interner();
        let mut diverging_roots = Bits::<_, u32>::from_words(Vec::<u64, _>::new_in(self.frame));
        for var in self.table.diverging_type_vars().iter() {
            let ty = self.shallow_resolve(Ty::new_var(interner, TyVid::from_u32(var)));
            if let Some(vid) = ty.ty_vid() {
                diverging_roots.insert_growing(self.table.infer_ctxt.root_var(vid).as_u32());
            }
        }
        debug!("calculate_diverging_fallback: diverging_roots={:?}", diverging_roots);

        let mut diverging_fallback = frame_map(self.frame);
        for vid in unresolved_variables.iter().filter_map(|ty| ty.ty_vid()) {
            if diverging_roots.contains(self.table.infer_ctxt.root_var(vid).as_u32()) {
                debug!("fallback to !: {:?}", vid);
                diverging_fallback
                    .insert(Ty::new_var(self.interner(), vid), self.types.types.never);
            }
        }
        diverging_fallback
    }
}
