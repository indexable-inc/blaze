//! Path expression resolution.

use hir_def::{
    AdtId, AssocItemId, GenericDefId, ItemContainerId, Lookup,
    expr_store::path::{Path, PathSegment},
    hir::{ExprOrPatId, ExprOrPatIdPacked},
    item_tree::FieldsShape,
    resolver::{ResolveValueResult, TypeNs, ValueNs},
    signatures::{ConstSignature, FunctionSignature, StructSignature},
};
use hir_expand::name::Name;
use rustc_type_ir::inherent::{SliceLike, Ty as _};

use crate::{
    db::HirDatabase,
    ExplicitDropMethodUseKind, InferenceDiagnostic, Span, StructPathDef, StructPathFound,
    ValueTyDefId,
    infer::{
        InferenceTyLoweringVarsCtx, diagnostics::InferenceTyLoweringContext as TyLoweringContext,
    },
    lower::{GenericPredicates, LifetimeElisionKind, LifetimeLoweringMode},
    method_resolution::{self, CandidateId, MethodError},
    next_solver::{
        GenericArg, GenericArgs, TraitRef, Ty, Unnormalized,
        infer::traits::{Obligation, ObligationCause, ObligationCauseCode},
    },
};

use super::InferenceContext;

impl<'db, 'f> InferenceContext<'db, 'f> {
    pub(super) fn infer_path(
        &mut self,
        path: Path<'_>,
        id: ExprOrPatIdPacked,
    ) -> Option<(ValueNs, Ty<'db>)> {
        let (value, self_subst) = self.resolve_value_path_inner(path, id, false)?;
        self.write_value_resolution(id, value);

        if let ValueNs::FunctionId(f) = value
            && self.lang_items.Drop_drop.is_some_and(|drop_fn| drop_fn == f)
        {
            self.push_diagnostic(InferenceDiagnostic::ExplicitDropMethodUse {
                kind: ExplicitDropMethodUseKind::Path(id),
            });
        }

        let (value_def, generic_def, substs) =
            match self.resolve_value_path(path, id, value, self_subst)? {
                ValuePathResolution::GenericDef(value_def, generic_def, substs) => {
                    (value_def, generic_def, substs)
                }
                ValuePathResolution::NonGeneric(ty) => return Some((value, ty)),
            };
        let args = self.insert_type_vars(substs);

        self.add_required_obligations_for_value_path(id, generic_def, args);

        let ty = self.db.value_ty(value_def)?.instantiate(self.interner(), args).skip_norm_wip();
        let ty = self.process_remote_user_written_ty(ty);
        Some((value, ty))
    }

    fn resolve_value_path(
        &mut self,
        path: Path<'_>,
        id: ExprOrPatIdPacked,
        value: ValueNs,
        self_subst: Option<GenericArgs<'db>>,
    ) -> Option<ValuePathResolution<'db>> {
        let value_def: ValueTyDefId = match value {
            ValueNs::FunctionId(it) => it.into(),
            ValueNs::ConstId(it) => it.into(),
            ValueNs::StaticId(it) => it.into(),
            ValueNs::StructId(it) => {
                self.write_variant_resolution(id, it.into());

                it.into()
            }
            ValueNs::EnumVariantId(it) => {
                self.write_variant_resolution(id, it.into());

                it.into()
            }
            ValueNs::LocalBinding(pat) => {
                return match self.result.type_of_binding.get(pat) {
                    Some(ty) => Some(ValuePathResolution::NonGeneric(ty.as_ref())),
                    None => unreachable!("no type for local variable {pat:?}"),
                };
            }
            ValueNs::ImplSelf(impl_id) => {
                // rustc's `check_pat_path` (a pattern takes a unit struct only, E0533) and
                // `instantiate_value_path` (a constructor).
                let ty = self.db.impl_self_ty(impl_id).instantiate_identity().skip_norm_wip();
                let ctor = match ty.as_adt() {
                    Some((AdtId::StructId(struct_id), substs)) => {
                        Some((struct_id, substs, StructSignature::of(self.db, struct_id).shape))
                    }
                    _ => None,
                };
                return match (ctor, id.unpack()) {
                    (Some((struct_id, substs, FieldsShape::Unit)), _)
                    | (Some((struct_id, substs, FieldsShape::Tuple)), ExprOrPatId::ExprId(_)) => {
                        self.write_variant_resolution(id, struct_id.into());
                        Some(ValuePathResolution::GenericDef(
                            struct_id.into(),
                            struct_id.into(),
                            substs,
                        ))
                    }
                    (_, ExprOrPatId::PatId(_)) => {
                        let found = StructPathFound::TypeRelative(StructPathDef::SelfCtor);
                        self.push_diagnostic(InferenceDiagnostic::NotAStructPath {
                            node: id,
                            found,
                            ty: None,
                        });
                        None
                    }
                    (_, ExprOrPatId::ExprId(expr)) => {
                        if !ty.references_non_lt_error() {
                            self.push_diagnostic(InferenceDiagnostic::SelfCtorNotTupleOrUnit {
                                expr,
                                self_ty: ty.store(),
                            });
                        }
                        None
                    }
                };
            }
            ValueNs::GenericParam(it) => {
                return Some(ValuePathResolution::NonGeneric(self.db.const_param_ty(it)));
            }
        };

        let generic_def = value_def.to_generic_def_id(self.db);
        if let GenericDefId::StaticId(_) = generic_def {
            // `Static` is the kind of item that can never be generic currently. We can just skip the binders to get its type.
            let ty = self.db.value_ty(value_def)?.skip_binder();
            let ty = self.process_remote_user_written_ty(ty);
            return Some(ValuePathResolution::NonGeneric(ty));
        };

        let substs = if self_subst.is_some()
            && matches!(value_def, ValueTyDefId::EnumVariantId(_))
        {
            // This is something like `TypeAlias::<Args>::EnumVariant`. Do not call `substs_from_path()`,
            // as it'll try to re-lower the previous segment assuming it refers to the enum, but it refers
            // to the type alias and they may have different generics. The variant's own arguments
            // are E0109 (`resolve_value_path_inner`).
            self.types.empty.generic_args
        } else {
            self.with_ty_lowering(|ctx| {
                let mut path_ctx = ctx.at_path(path, id);
                let last_segment = path.segments().len().checked_sub(1);
                if let Some(last_segment) = last_segment {
                    path_ctx.set_current_segment(last_segment)
                }
                path_ctx.substs_from_path(value_def, true, false, id.into())
            })
        };

        let parent_substs_len = self_subst.map_or(0, |it| it.len());
        let substs = GenericArgs::fill_rest(
            self.interner(),
            generic_def.into(),
            self_subst.iter().flat_map(|it| it.iter()).chain(substs.iter().skip(parent_substs_len)),
            |_, id, _| GenericArg::error_from_id(self.interner(), id),
        );

        Some(ValuePathResolution::GenericDef(value_def, generic_def, substs))
    }

    pub(super) fn resolve_value_path_inner(
        &mut self,
        path: Path<'_>,
        id: ExprOrPatIdPacked,
        no_diagnostics: bool,
    ) -> Option<(ValueNs, Option<GenericArgs<'db>>)> {
        // Don't use `self.make_ty()` here as we need `orig_ns`.
        let mut vars_ctx = InferenceTyLoweringVarsCtx {
            table: &mut self.table,
            type_of_type_placeholder: &mut self.result.type_of_type_placeholder,
        };
        let mut ctx = TyLoweringContext::new(
            self.db,
            &self.resolver,
            self.store,
            &self.diagnostics,
            self.store_owner,
            self.generic_def,
            &self.generics,
            LifetimeElisionKind::Infer,
            self.allow_using_generic_params,
            &mut vars_ctx,
            &self.defined_anon_consts,
            LifetimeLoweringMode::LateParam,
            self.frame,
        );
        let mut path_ctx = if no_diagnostics {
            ctx.at_path_forget_diagnostics(path)
        } else {
            ctx.at_path(path, id)
        };
        let (value, self_subst) = if let Some(type_ref) = path.type_anchor() {
            let last = path.segments().last()?;

            let (ty, orig_ns) = path_ctx.ty_ctx().lower_ty_ext(type_ref);
            let ty = path_ctx.expect_table().process_user_written_ty(ty);

            path_ctx.ignore_last_segment();
            let (ty, _) = path_ctx.lower_ty_relative_path(ty, orig_ns, true, id.into());
            drop_ctx(ctx, no_diagnostics);
            let ty = self.table.process_user_written_ty(ty);
            self.resolve_ty_assoc_item(ty, last.name, id, false)
                .map(|(it, substs)| (it, Some(substs)))?
        } else {
            let hygiene = self.store.expr_or_pat_path_hygiene(id.unpack());
            // FIXME: report error, unresolved first path segment
            let value_or_partial = path_ctx.resolve_path_in_value_ns(hygiene)?;

            match value_or_partial {
                ResolveValueResult::ValueNs(it) => {
                    drop_ctx(ctx, no_diagnostics);

                    // A trait's function or constant reached by a plain path (`use Trait::f;`,
                    // `import_trait_associated_functions`) is rustc's `<_ as Trait>::f`; an
                    // impl's item is only reached so through a lang item path.
                    let is_lang_item = matches!(path, Path::LangItem(..));
                    let def_and_container = match it {
                        ValueNs::ConstId(it) => Some((it.into(), it.loc(self.db).container)),
                        ValueNs::FunctionId(it) => Some((it.into(), it.loc(self.db).container)),
                        _ => None,
                    };
                    let def_and_container =
                        def_and_container.and_then(|(def, container)| match container {
                            ItemContainerId::ImplId(it) if is_lang_item => Some((def, it.into())),
                            ItemContainerId::TraitId(it) => Some((def, it.into())),
                            ItemContainerId::ImplId(_)
                            | ItemContainerId::ExternBlockId(_)
                            | ItemContainerId::ModuleId(_) => None,
                        });
                    let args = def_and_container.map(|(def, container)| {
                        let args = self.infcx().fresh_args_for_item(id.into(), container);
                        self.write_assoc_resolution(id, def, args);
                        args
                    });

                    (it, args)
                }
                ResolveValueResult::Partial(def, remaining_index) => {
                    // `path_ctx` holds the rest of `self`.
                    let segments = u32::try_from(remaining_index).expect("2^32 path segments");
                    self.result.qualifier_resolutions.insert(id, (def, segments));
                    // there may be more intermediate segments between the resolved one and
                    // the end. Only the last segment needs to be resolved to a value; from
                    // the segments before that, we need to get either a type or a trait ref.

                    let remaining_segments = path.segments().skip(remaining_index);
                    let is_before_last = remaining_segments.len() == 1;
                    let last_segment = remaining_segments
                        .last()
                        .expect("there should be at least one segment here");

                    let (resolution, substs) = match (def, is_before_last) {
                        (TypeNs::TraitId(trait_), true) => {
                            let self_ty = path_ctx.expect_table().next_ty_var(id.into());
                            let trait_ref = path_ctx.lower_trait_ref_from_resolved_path(
                                trait_,
                                self_ty,
                                true,
                                id.into(),
                            );
                            drop_ctx(ctx, no_diagnostics);
                            self.resolve_trait_assoc_item(trait_ref, last_segment, id)
                        }
                        (def, _) => {
                            // Either we already have a type (e.g. `Vec::new`), or we have a
                            // trait but it's not the last segment, so the next segment
                            // should resolve to an associated type of that trait (e.g. `<T
                            // as Iterator>::Item::default`)
                            path_ctx.ignore_last_segment();
                            let (ty, _) = path_ctx.lower_partly_resolved_path(def, true, id.into());
                            drop_ctx(ctx, no_diagnostics);
                            if ty.is_ty_error() {
                                return None;
                            }

                            let ty = self.process_user_written_ty(ty);

                            let desugared = matches!(path, Path::LangItem(..));
                            self.resolve_ty_assoc_item(ty, last_segment.name, id, desugared)
                        }
                    }?;
                    (resolution, Some(substs))
                }
            }
        };
        // Only a variant found on a type has its enum's arguments as `self_subst`.
        if !no_diagnostics
            && self_subst.is_some()
            && let ValueNs::EnumVariantId(_) = value
        {
            let last = path.segments().len() - 1;
            Self::forbid_type_relative_variant_args(&self.diagnostics, id, path, last);
        }
        return Some((value, self_subst));

        #[inline]
        fn drop_ctx(mut ctx: TyLoweringContext<'_, '_, '_>, no_diagnostics: bool) {
            if no_diagnostics {
                ctx.forget_diagnostics();
            }
        }
    }

    pub(super) fn add_required_obligations_for_value_path(
        &mut self,
        node: ExprOrPatIdPacked,
        def: GenericDefId,
        subst: GenericArgs<'db>,
    ) {
        let interner = self.interner();
        let predicates = GenericPredicates::query_all(self.db, def);
        let param_env = self.table.param_env;
        // rustc's `add_required_obligations_with_code`: each bound's cause names it by its place
        // in the item's predicates, its parents' first.
        self.table.register_predicates(
            predicates
                .iter_instantiated(interner, subst.as_slice())
                .map(Unnormalized::skip_norm_wip)
                .enumerate()
                .map(|(bound_index, clause)| {
                    let code = ObligationCauseCode::WhereClauseInExpr {
                        item: def,
                        bound_index: u32::try_from(bound_index).expect("2^32 bounds"),
                        arg_index: None,
                    };
                    Obligation {
                        cause: ObligationCause::with_code(node, code),
                        param_env,
                        predicate: clause.as_predicate(),
                        recursion_depth: 0,
                    }
                }),
        );
    }

    fn resolve_trait_assoc_item(
        &mut self,
        trait_ref: TraitRef<'db>,
        segment: PathSegment<'_>,
        id: ExprOrPatIdPacked,
    ) -> Option<(ValueNs, GenericArgs<'db>)> {
        let trait_ = trait_ref.def_id.0;
        let item =
            trait_.trait_items(self.db).items.iter().map(|(_name, id)| *id).find_map(|item| {
                match item {
                    AssocItemId::FunctionId(func) => {
                        if segment.name == &FunctionSignature::of(self.db, func).name {
                            Some(CandidateId::FunctionId(func))
                        } else {
                            None
                        }
                    }

                    AssocItemId::ConstId(konst) => {
                        if ConstSignature::of(self.db, konst).name.as_ref() == Some(segment.name) {
                            Some(CandidateId::ConstId(konst))
                        } else {
                            None
                        }
                    }
                    AssocItemId::TypeAliasId(_) => None,
                }
            })?;
        let def = match item {
            CandidateId::FunctionId(f) => ValueNs::FunctionId(f),
            CandidateId::ConstId(c) => ValueNs::ConstId(c),
        };

        self.write_assoc_resolution(id, item, trait_ref.args);
        Some((def, trait_ref.args))
    }

    fn resolve_ty_assoc_item(
        &mut self,
        ty: Ty<'db>,
        name: &Name,
        id: ExprOrPatIdPacked,
        desugared: bool,
    ) -> Option<(ValueNs, GenericArgs<'db>)> {
        if ty.is_ty_error() {
            return None;
        }
        let ty = self.table.resolve_projections_within(ty);

        if let Some(result) = self.resolve_enum_variant_on_ty(ty, name, id) {
            return Some(result);
        }

        let res = self.with_method_resolution(Span::Dummy, Span::Dummy, |ctx| {
            // clone: an interned name (a refcount); the probe takes it by value
            let (mode, name) = (method_resolution::Mode::Path, name.clone());
            if desugared {
                ctx.probe_for_desugared_name(mode, name, ty)
            } else {
                ctx.probe_for_name(mode, name, ty)
            }
        });
        let (item, visible) = match res {
            Ok(res) => {
                self.write_unstable_candidates(id, &res);
                self.record_trait_pick(id, &res);
                self.lint_fully_qualified_call_from_2018(id, name, ty, &res);
                (res.item, true)
            }
            Err(error) => match error {
                MethodError::PrivateMatch(candidate_id) => (candidate_id.item, false),
                _ => {
                    // rustc's `report_method_error`: an error in the type was reported already.
                    if !ty.references_non_lt_error() {
                        self.push_diagnostic(InferenceDiagnostic::UnresolvedAssocItem {
                            id,
                            ty: ty.store(),
                            // clone: an error's name, a refcount.
                            name: name.clone(),
                        });
                    }
                    return None;
                }
            },
        };

        let (def, container) = match item {
            CandidateId::FunctionId(f) => (ValueNs::FunctionId(f), f.lookup(self.db).container),
            CandidateId::ConstId(c) => (ValueNs::ConstId(c), c.lookup(self.db).container),
        };
        let substs = match container {
            ItemContainerId::ImplId(impl_id) => {
                let impl_substs = self.table.fresh_args_for_item(id.into(), impl_id.into());
                let impl_self_ty = self
                    .db
                    .impl_self_ty(impl_id)
                    .instantiate(self.interner(), impl_substs)
                    .skip_norm_wip();
                _ = self.demand_eqtype(id, impl_self_ty, ty);
                impl_substs
            }
            ItemContainerId::TraitId(trait_) => {
                // we're picking this method
                GenericArgs::fill_rest(
                    self.interner(),
                    trait_.into(),
                    [ty.into()],
                    |_, param, _| self.table.var_for_def(param, id.into()),
                )
            }
            ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_) => {
                unreachable!("assoc item contained in module/extern block")
            }
        };

        self.write_assoc_resolution(id, item, substs);
        if !visible {
            let item = match item {
                CandidateId::FunctionId(it) => it.into(),
                CandidateId::ConstId(it) => it.into(),
            };
            self.push_diagnostic(InferenceDiagnostic::PrivateAssocItem { id, item });
        }
        Some((def, substs))
    }

    fn resolve_enum_variant_on_ty(
        &mut self,
        ty: Ty<'db>,
        name: &Name,
        id: ExprOrPatIdPacked,
    ) -> Option<(ValueNs, GenericArgs<'db>)> {
        let ty = self.table.try_structurally_resolve_type(id.into(), ty);
        let (enum_id, subst) = match ty.as_adt() {
            Some((AdtId::EnumId(e), subst)) => (e, subst),
            _ => return None,
        };
        let enum_data = enum_id.enum_variants(self.db);
        let variant = enum_data.variant(name)?;
        self.write_variant_resolution(id, variant.into());
        Some((ValueNs::EnumVariantId(variant), subst))
    }
}

#[derive(Debug)]
enum ValuePathResolution<'db> {
    // It's awkward to wrap a single ID in two enums, but we need both and this saves fallible
    // conversion between them + `unwrap()`.
    GenericDef(ValueTyDefId, GenericDefId, GenericArgs<'db>),
    NonGeneric(Ty<'db>),
}
