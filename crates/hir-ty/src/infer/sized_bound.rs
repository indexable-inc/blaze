//! What rustc's `lookup_method` (`compiler/rustc_hir_typeck/src/method/mod.rs` at 16d1ffac)
//! gathers for `MethodError::IllegalSizedBound`, a method with a `Self: Sized` bound called on
//! a trait object: whether the receiver of the other mutability picks another method
//! (`needs_mut`), and the traits a probe over the traits of every crate picks from instead.

use base_db::all_crates;
use hir_def::{
    FunctionId, GenericDefId, ItemContainerId, Lookup, TraitId, hir::ExprId,
    signatures::FunctionSignature,
};
use hir_expand::name::Name;
use rustc_type_ir::inherent::{IntoKind as _, Ty as _};

use crate::{
    InferenceDiagnostic,
    db::HirDatabase,
    infer::InferenceContext,
    method_resolution::{
        CandidateId, CandidateSource, MethodError, MethodResolutionContext, Mode, Pick,
        TraitSet, TraitsInScope, in_crate_trait_value_items,
    },
    next_solver::{AnyImplId, Ty, TyKind},
};

/// rustc's `Pick::differs_from`.
fn differs_from(pick: &Pick<'_>, other: &Pick<'_>) -> bool {
    pick.self_ty != other.self_ty || pick.item != other.item
}

impl<'db, 'f> InferenceContext<'db, 'f> {
    /// Pushes `MethodCallIllegalSizedBound` for the call of `method`, picked as `pick` for a
    /// receiver of type `self_ty`; `bound` is whose own clauses hold the `Self: Sized` bound.
    pub(crate) fn push_illegal_sized_bound(
        &mut self,
        pick: &Pick<'db>,
        self_ty: Ty<'db>,
        call_expr: ExprId,
        receiver: ExprId,
        method: FunctionId,
        bound: Option<GenericDefId>,
    ) {
        // clone: the error path's name, for the probes.
        let name = FunctionSignature::of(self.db, method).name.clone();
        let mut needs_mut = false;
        if let TyKind::Ref(region, t_type, mutability) = self_ty.kind() {
            let trait_type = Ty::new_ref(self.interner(), region, t_type, mutability.invert());
            // clone: an interned name (a refcount), on the error path.
            if let Ok(new_pick) = self.lookup_probe(call_expr, receiver, name.clone(), trait_type)
                && differs_from(pick, &new_pick)
            {
                let new_mutability = match new_pick.self_ty.kind() {
                    TyKind::Ref(_, _, it) => Some(it),
                    _ => None,
                };
                needs_mut = new_mutability != Some(mutability);
            }
        }

        let candidates = match self.probe_all_traits(call_expr, receiver, name, self_ty) {
            // A pick of another inherent item would have been the first probe's too, so a pick
            // that differs is a trait's item.
            Ok(new_pick) if differs_from(pick, &new_pick) => {
                let container = match new_pick.item {
                    CandidateId::FunctionId(it) => it.lookup(self.db).container,
                    CandidateId::ConstId(it) => it.lookup(self.db).container,
                };
                match container {
                    ItemContainerId::TraitId(it) => Box::from([it]),
                    _ => Box::default(),
                }
            }
            Err(MethodError::Ambiguity(sources)) => sources
                .iter()
                .filter_map(|source| match *source {
                    CandidateSource::Impl(AnyImplId::ImplId(it)) => {
                        self.db.impl_trait(it).map(|it| it.skip_binder().def_id.0)
                    }
                    CandidateSource::Impl(AnyImplId::BuiltinDeriveImplId(it)) => {
                        it.loc(self.db).trait_.get_id(self.lang_items)
                    }
                    CandidateSource::Trait(_) => None,
                })
                .collect(), // alloc: the diagnostic's own list, on the error path
            _ => Box::default(),
        };

        self.push_diagnostic(InferenceDiagnostic::MethodCallIllegalSizedBound {
            call_expr,
            method,
            bound,
            needs_mut,
            candidates,
        });
    }

    /// rustc's probe with `ProbeScope::AllTraits`: the traits in scope and every trait of every
    /// crate declared in a module that has a value item named `name`.
    fn probe_all_traits(
        &self,
        call_expr: ExprId,
        receiver: ExprId,
        name: Name,
        self_ty: Ty<'db>,
    ) -> Result<Pick<'db>, MethodError<'db>> {
        let db = self.db;
        let in_scope = TraitsInScope::of_resolver(db, &self.resolver);
        // alloc: the error path's set of every trait with an item of the name, interned once.
        let mut traits: Vec<TraitId> = in_scope.all(db).collect();
        for &krate in all_crates(db).iter() {
            // clone: an interned name (a refcount), the query's key.
            let items = in_crate_trait_value_items(db, krate, name.clone());
            traits.extend(items.iter().map(|&(it, _)| it));
        }
        let ctx = MethodResolutionContext {
            infcx: &self.table.infer_ctxt,
            resolver: &self.resolver,
            param_env: self.table.param_env,
            traits_in_scope: TraitsInScope::Set(TraitSet::new(&mut traits)),
            edition: self.edition,
            features: self.features,
            call_span: call_expr.into(),
            receiver_span: receiver.into(),
            frame: self.frame,
            // A second probe of a call the first already reported for.
            errors: None,
        };
        ctx.probe_for_name(Mode::MethodCall, name, self_ty)
    }
}
