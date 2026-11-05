//! Type checking `for const` and `base.[field]` (`#![feature(blaze_comptime)]`,
//! docs/v2/plan/design/comptime.md, "Type checking once"): a loop's body is checked once, its
//! binder `F` a placeholder of a universe of its own, with the environment extended by what
//! the loop's domain says of `F` (`F: core::reflect::Field<B>`, its host effect, and the field
//! binders of the environment over `B` instantiated with `F`). Monomorphization unrolls the
//! loop later; nothing here depends on which fields `B` has.

use hir_def::{
    AdtId,
    expr_store::ComptimeForm,
    hir::{ExprId, LabelId, PatId},
};
use rustc_type_ir::{
    BoundVar, TypeVisitableExt,
    inherent::{GenericArgs as _, IntoKind, Ty as _},
    reflect::ReflectedDomain,
};

use super::{
    BreakableKind, ComptimeLoop, Expectation, InferenceContext, InferenceDiagnostic,
    expr::ExprIsRead, pat::PatOrigin,
};
use crate::next_solver::{
    Clauses, GenericArgs, IsRigid, ParamEnv, PlaceholderType, TraitRef, Ty, TyKind,
    infer::traits::{Obligation, ObligationCause},
};

/// What a `for const` loop's iterable gave its binder.
enum Entered<'db> {
    /// The loop is checked: `F`, the placeholder.
    Binder(Ty<'db>),
    /// A typed error is reported (a mismatch, a type to be known, a form not checked yet) or
    /// the iterable is an error already: the body is inferred with `F` the error type.
    Error,
}

impl<'db> InferenceContext<'db, '_> {
    /// `'label: for const pat in iterable body`, of type `()`: `pat` is of the binder's type,
    /// `body` is checked once, and `break`/`continue` target the loop as a `for`'s (no
    /// `break value`). The body may run any number of times, zero too, so what follows is as
    /// reachable as the iterable leaves it.
    pub(super) fn infer_comptime_for(
        &mut self,
        tgt_expr: ExprId,
        pat: PatId,
        iterable: ExprId,
        body: ExprId,
        label: Option<LabelId>,
    ) -> Ty<'db> {
        let iterable_ty = self.infer_expr_no_expect(iterable, ExprIsRead::Yes);
        let entered = self.enter_comptime_loop(tgt_expr, iterable, iterable_ty);
        let binder = match entered {
            Entered::Binder(binder) => binder,
            Entered::Error => self.types.types.error,
        };
        self.infer_top_pat(pat, binder, PatOrigin::MatchArm);
        let diverges = self.diverges;
        let unit = self.types.types.unit;
        self.with_breakable_ctx(BreakableKind::Loop, None, label, |this| {
            this.infer_expr_suptype_coerce_never(
                body,
                &Expectation::HasType(unit),
                ExprIsRead::Yes,
            );
        });
        self.diverges = diverges;
        unit
    }

    /// The binder of the loop `tgt_expr` over `iterable` (of type `iterable_ty`), with the
    /// table's environment extended by what the loop assumes of it, recorded in the result's
    /// `comptime_loops`. The environment stays extended for the rest of the body: no type
    /// outside the loop names its placeholder, so the assumptions say nothing there, and the
    /// checks inference defers past the loop (casts, closure captures) still see them.
    fn enter_comptime_loop(
        &mut self,
        tgt_expr: ExprId,
        iterable: ExprId,
        iterable_ty: Ty<'db>,
    ) -> Entered<'db> {
        let lang = self.lang_items;
        let (Some(fields), Some(variants), Some(field_trait), Some(variant_trait)) =
            (lang.ReflectFields, lang.ReflectVariants, lang.ReflectField, lang.ReflectVariant)
        else {
            // A `core` without `core::reflect`: no domain to check the loop against.
            return self.comptime_not_yet_supported(tgt_expr, ComptimeForm::ForConst);
        };
        let interner = self.interner();
        let domain = self.structurally_resolve_type(iterable.into(), iterable_ty);
        let (kind, container) = match domain.kind() {
            TyKind::Error(_) => return Entered::Error,
            TyKind::Adt(def, args) if def.def_id() == AdtId::StructId(fields) => {
                let filter = self.structurally_resolve_type(iterable.into(), args.type_at(1));
                match filter.kind() {
                    TyKind::Tuple(tys) if tys.is_empty() => {}
                    TyKind::Error(_) => return Entered::Error,
                    // `Fields<T, OnlyUnmarked<M>>`: filters are not checked yet.
                    _ => return self.comptime_not_yet_supported(tgt_expr, ComptimeForm::ForConst),
                }
                (ReflectedDomain::Fields, args.type_at(0))
            }
            TyKind::Adt(def, args) if def.def_id() == AdtId::StructId(variants) => {
                (ReflectedDomain::Variants, args.type_at(0))
            }
            _ => {
                // A loop over anything else is a mismatch with the fields domain.
                let error = self.types.types.error;
                let unit = self.types.types.unit;
                let expected = Ty::new_adt(
                    interner,
                    AdtId::StructId(fields),
                    GenericArgs::new_from_slice(&[error.into(), unit.into()]),
                );
                self.emit_type_mismatch(iterable.into(), expected, domain);
                return Entered::Error;
            }
        };
        let container = self.resolve_vars_if_possible(container);
        if container.references_error() {
            return Entered::Error;
        }
        // The environment holds no inference variable: `fields_of::<_>()` is refused as an
        // unknown type, as rustc refuses a `for` over a type it must know.
        if container.has_non_region_infer() {
            self.type_must_be_known_at_this_point(iterable.into(), container);
            return Entered::Error;
        }
        let universe = self.table.infer_ctxt.create_next_universe();
        let binder =
            Ty::new_placeholder(interner, PlaceholderType::new_anon(universe, BoundVar::ZERO));
        let env = self.table.param_env;
        let assumptions = crate::comptime::member_assumptions(
            interner,
            env,
            binder,
            kind,
            container,
            match kind {
                ReflectedDomain::Fields => field_trait,
                ReflectedDomain::Variants => variant_trait,
            },
        );
        self.table.param_env =
            ParamEnv::new(Clauses::new_from_iter(env.clauses().iter().chain(assumptions.iter())));
        self.result.comptime_loops.insert(
            tgt_expr,
            ComptimeLoop {
                binder: binder.store(),
                domain: domain.store(),
                kind,
                container: container.store(),
                assumptions: assumptions.store(),
            },
        );
        Entered::Binder(binder)
    }

    /// `base.[field]`: the place of `base`, auto-deref'd to the first type `B` with
    /// `F: core::reflect::Field<B>` (`F` the type of `field`), of type
    /// `<F as core::field::Field>::Type`. The goal `F: Field<B>` is registered, so a `field`
    /// that names no field of the base is E0277 at the expression.
    pub(super) fn infer_reflected_field(
        &mut self,
        tgt_expr: ExprId,
        base: ExprId,
        field: ExprId,
    ) -> Ty<'db> {
        // As a field projection, the base is not read.
        let base_ty = self.infer_expr_inner(base, &Expectation::none(), ExprIsRead::No);
        let field_ty = self.infer_expr_no_expect(field, ExprIsRead::Yes);
        let lang = self.lang_items;
        let (Some(reflect_field), Some(field_type)) = (lang.ReflectField, lang.FieldType) else {
            self.comptime_not_yet_supported(tgt_expr, ComptimeForm::ReflectedField);
            return self.types.types.error;
        };
        let base_ty = self.structurally_resolve_type(base.into(), base_ty);
        let field_ty = self.resolve_vars_if_possible(field_ty);
        if base_ty.references_error() || field_ty.references_error() {
            return self.types.types.error;
        }
        let interner = self.interner();
        let frame = self.frame;
        let cause = ObligationCause::new(tgt_expr);
        let membership = |container: Ty<'db>| {
            TraitRef::new(interner, reflect_field.into(), [field_ty, container])
        };
        let mut autoderef = self.table.autoderef_with_tracking(base_ty, base.into());
        let found = autoderef.by_ref().find(|&(derefed, _)| {
            let obligation =
                Obligation::new(interner, cause, self.table.param_env, membership(derefed));
            self.table.infer_ctxt.predicate_may_hold(&obligation)
        });
        // No step of the base has the field: the goal on the base as written fails, E0277.
        let container = match found {
            Some((container, _)) => {
                let adjustments =
                    self.table.register_infer_ok(autoderef.adjust_steps_as_infer_ok(frame));
                self.write_expr_adj(base, &adjustments);
                container
            }
            None => {
                drop(autoderef);
                base_ty
            }
        };
        let env = self.table.param_env;
        self.table.register_predicate(Obligation::new(interner, cause, env, membership(container)));
        self.table.try_structurally_resolve_type(
            tgt_expr.into(),
            Ty::new_projection(interner, IsRigid::No, field_type.into(), [field_ty]),
        )
    }

    fn comptime_not_yet_supported(&mut self, expr: ExprId, form: ComptimeForm) -> Entered<'db> {
        self.push_diagnostic(InferenceDiagnostic::ComptimeNotYetSupported { expr, form });
        Entered::Error
    }
}
