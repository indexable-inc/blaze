//! [`SourceAnalyzer`]'s node-taking methods over views: each finds its ids with the source map's
//! `*_view` lookups (`AstPtr::of(view)`, no handle made) and then answers as the handle method
//! does, from the same inference result and resolver.

use std::{cell::OnceCell, iter::once};

use base_db::Db;
use either::Either;
use hir_def::{
    AdtId, AssocItemId, CallableDefId, DefWithBodyId, ExpressionStoreOwnerId, FieldId,
    GenericDefId, LoweringMode, ModuleDefId, VariantId,
    expr_store::{
        Body, ExpressionStoreSourceMap, HygieneId,
        lower::ImplTraitLowering,
        path::Path,
        scope::{ExprScopes, ScopeId},
    },
    hir::{BindingId, Expr, ExprId, ExprOrPatId, Pat},
    resolver::{Resolver, ValueNs, resolver_for_scope},
    type_ref::TypeRefId,
};
use hir_expand::{
    HirFileId, InFile,
    mod_path::{ModPath, PathKind},
    name::AsName,
};
use hir_ty::{
    Adjustment, InferenceResult, LifetimeElisionKind, LifetimeLoweringMode, TyLoweringContext,
    method_resolution::CandidateId,
    next_solver::{
        AliasTy, DbInterner, ErrorGuaranteed, GenericArgs, Region, Ty, TyKind, TypingMode,
        infer::DbInternerInferExt,
    },
    traits::structurally_normalize_ty,
};
use itertools::Itertools;
use rustc_type_ir::{
    AliasTyKind,
    inherent::{IntoKind, Ty as _},
};
use stdx::never;
use syntax::{
    ast as tag,
    view::{self as ast, AstNode, Node},
};

use super::{
    BodyOrSig, SourceAnalyzer, resolve_hir_path_, resolve_hir_path_as_attr_macro,
    resolve_hir_path_qualifier, resolve_hir_value_path,
};
use crate::{
    AssocItem, BuiltinAttr, Callable, DeriveHelper, EnumVariant, Field, Function,
    GenericSubstitution, Local, ModuleDef, SemanticsImpl, ToolModule, TupleField, Type,
    TypeOwnerId,
    db::HirDatabase,
    semantics::{PathResolution, PathResolutionPerNs},
};

impl<'db> SourceAnalyzer<'db> {
    /// The analyzer of `def`'s body at a view, with its inference when `infer`, with no offset.
    pub(crate) fn new_for_body_view(
        db: &'db Db,
        def: DefWithBodyId,
        node @ InFile { file_id, .. }: InFile<Node<'db>>,
        infer: bool,
    ) -> SourceAnalyzer<'db> {
        let (body, source_map) = Body::with_source_map(db, def);
        let scopes = ExprScopes::of(db, def);
        let (scope, _expr) = scope_for_view(db, scopes, source_map, node).unzip();
        let resolver = resolver_for_scope(db, def, scope);
        let infer = infer.then(|| InferenceResult::of(db, def));
        SourceAnalyzer {
            resolver,
            body_or_sig: Some(BodyOrSig::Body { def, body, source_map, infer }),
            file_id,
            type_owner: def.generic_def(db).into(),
            infer_body: Some(def.into()),
            maps: None,
        }
    }

    /// The analyzer of `def`'s signature at a view, with no offset.
    pub(crate) fn new_generic_def_view(
        db: &'db Db,
        sema: &SemanticsImpl<'db>,
        def: GenericDefId,
        node @ InFile { file_id, .. }: InFile<Node<'db>>,
        infer: bool,
    ) -> SourceAnalyzer<'db> {
        let (generics, store, source_map) =
            hir_def::hir::generics::GenericParams::with_source_map(db, def);
        let scopes = ExprScopes::of(db, def);
        let (scope, expr) = scope_for_view(db, scopes, source_map, node).unzip();
        let resolver = resolver_for_scope(db, def, scope);
        let infer_body = expr.and_then(|expr| {
            sema.infer_body_for_expr_or_pat(
                ExpressionStoreOwnerId::Signature(def),
                store,
                expr.into(),
            )
        });
        let infer = if infer && let Some(infer_body) = infer_body {
            Some(InferenceResult::of(db, infer_body))
        } else {
            None
        };
        SourceAnalyzer {
            resolver,
            body_or_sig: Some(BodyOrSig::Sig { def, store, source_map, generics, infer }),
            file_id,
            type_owner: def.into(),
            infer_body,
            maps: None,
        }
    }

    /// The analyzer of `def`'s fields at a view, with no offset.
    pub(crate) fn new_variant_body_view(
        db: &'db Db,
        sema: &SemanticsImpl<'db>,
        def: VariantId,
        node @ InFile { file_id, .. }: InFile<Node<'db>>,
        infer: bool,
    ) -> SourceAnalyzer<'db> {
        let (fields, source_map) = def.fields_with_source_map(db);
        let scopes = ExprScopes::of(db, def);
        let (scope, expr) = scope_for_view(db, scopes, source_map, node).unzip();
        let resolver = resolver_for_scope(db, def, scope);
        let infer_body = expr.and_then(|expr| {
            sema.infer_body_for_expr_or_pat(
                ExpressionStoreOwnerId::VariantFields(def),
                &fields.store,
                expr.into(),
            )
        });
        let infer = if infer && let Some(infer_body) = infer_body {
            Some(InferenceResult::of(db, infer_body))
        } else {
            None
        };
        SourceAnalyzer {
            resolver,
            body_or_sig: Some(BodyOrSig::VariantFields {
                def,
                store: &fields.store,
                source_map,
                infer,
            }),
            file_id,
            type_owner: GenericDefId::from(def.adt_id(db)).into(),
            infer_body,
            maps: None,
        }
    }

    /// The analyzer of `resolver`'s scope for a node of `file_id`.
    pub(crate) fn new_for_resolver_view(
        resolver: Resolver<'db>,
        file_id: HirFileId,
    ) -> SourceAnalyzer<'db> {
        SourceAnalyzer {
            type_owner: resolver
                .generic_def()
                .map(Into::into)
                .unwrap_or_else(|| TypeOwnerId::NoParams(resolver.krate())),
            resolver,
            body_or_sig: None,
            file_id,
            infer_body: None,
            maps: None,
        }
    }

    pub(crate) fn expr_id_view(&self, expr: ast::Expr<'_>) -> Option<ExprOrPatId> {
        self.store_sm()?.node_expr_view(InFile::new(self.file_id, expr))
    }

    fn pat_id_view(&self, pat: ast::Pat<'_>) -> Option<ExprOrPatId> {
        self.store_sm()?.node_pat_view(InFile::new(self.file_id, pat))
    }

    fn type_id_view(&self, ty: ast::Type<'_>) -> Option<TypeRefId> {
        self.store_sm()?.node_type_view(InFile::new(self.file_id, ty))
    }

    pub(crate) fn expr_adjustments_view(&self, expr: ast::Expr<'_>) -> Option<&'db [Adjustment]> {
        let expr_id = self.expr_id_view(expr)?.as_expr()?;
        self.infer()?.expr_adjustment(expr_id)
    }

    pub(crate) fn type_of_type_view(&self, db: &'db Db, ty: ast::Type<'_>) -> Option<Type<'db>> {
        let type_ref = self.type_id_view(ty)?;
        let generic_def = self.generic_def(db)?;
        let store = self.store()?;
        // A static's or const's own type elides to `'static` as its lowering does; a type in
        // a body is inferred.
        let elision = match self.body_or_sig {
            Some(BodyOrSig::Sig { def: GenericDefId::StaticId(_), .. }) => {
                LifetimeElisionKind::Elided(Region::new_static(DbInterner::new_no_crate(db)))
            }
            Some(BodyOrSig::Sig { def: GenericDefId::ConstId(it), .. }) => {
                LifetimeElisionKind::for_const(DbInterner::new_no_crate(db), it.loc(db).container)
            }
            _ => LifetimeElisionKind::Infer,
        };
        let generics = OnceCell::new();
        let ty = blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
            TyLoweringContext::new(
                db,
                &self.resolver,
                store,
                self.resolver.expression_store_owner().unwrap_or_else(|| generic_def.into()),
                generic_def,
                &generics,
                elision,
                LifetimeLoweringMode::LateParam,
                frame,
            )
            .with_interning_mode(LoweringMode::Ide)
            .with_answered_infer_vars(self.infer())
            .lower_ty(type_ref)
        });
        Some(self.ty(db, ty))
    }

    pub(crate) fn type_of_expr_view(
        &self,
        db: &'db Db,
        expr: ast::Expr<'_>,
    ) -> Option<(Type<'db>, Option<Type<'db>>)> {
        let expr_id = self.expr_id_view(expr)?;
        let infer = self.infer()?;
        let coerced = expr_id
            .as_expr()
            .and_then(|expr_id| infer.expr_adjustment(expr_id))
            .and_then(|adjusts| adjusts.last().map(|adjust| adjust.target.as_ref()));
        let ty = infer.expr_or_pat_ty(expr_id);
        let mk_ty = |ty: Ty<'db>| self.ty(db, ty);
        Some((mk_ty(ty), coerced.map(mk_ty)))
    }

    pub(crate) fn type_of_pat_view(
        &self,
        db: &'db Db,
        pat: ast::Pat<'_>,
    ) -> Option<(Type<'db>, Option<Type<'db>>)> {
        let expr_or_pat_id = self.pat_id_view(pat)?;
        let infer = self.infer()?;
        let coerced = match expr_or_pat_id {
            ExprOrPatId::ExprId(idx) => infer
                .expr_adjustment(idx)
                .and_then(|adjusts| adjusts.last())
                .map(|adjust| adjust.target.as_ref()),
            ExprOrPatId::PatId(idx) => infer
                .pat_adjustment(idx)
                .and_then(|adjusts| adjusts.last())
                .map(|adjust| adjust.source.as_ref()),
        };
        let ty = infer.expr_or_pat_ty(expr_or_pat_id);
        let mk_ty = |ty: Ty<'db>| self.ty(db, ty);
        Some((mk_ty(ty), coerced.map(mk_ty)))
    }

    /// [`Self::type_of_pat_view`]'s type as writeback named it when it found it undecided,
    /// for rustc's "consider giving `x` an explicit type": each variable made for a const
    /// parameter, or for a type parameter outside a macro expansion, is that parameter, and
    /// every other variable an error, so it has the shape of `type_of_pat_view`'s type.
    pub(crate) fn named_type_of_pat_view(
        &self,
        db: &'db Db,
        pat: ast::Pat<'_>,
    ) -> Option<Type<'db>> {
        let ExprOrPatId::PatId(pat) = self.pat_id_view(pat)? else { return None };
        let named = self.infer()?.named_pat_type(pat)?;
        let source_map = self.store_sm()?;
        let named = named.named(DbInterner::new_no_crate(db), |span, param| match param {
            hir_def::GenericParamId::TypeParamId(_) => {
                crate::AnyDiagnostic::span_syntax(span, source_map)
                    .is_none_or(|span| !span.file_id.is_macro())
            }
            _ => true,
        });
        Some(self.ty(db, named.ty()?))
    }

    pub(crate) fn type_of_binding_in_pat_view(
        &self,
        db: &'db Db,
        pat: ast::IdentPat<'_>,
    ) -> Option<Type<'db>> {
        let binding_id = self.binding_id_of_pat_view(pat)?;
        let ty = self.infer()?.binding_ty(binding_id);
        Some(self.ty(db, ty))
    }

    fn binding_id_of_pat_view(&self, pat: ast::IdentPat<'_>) -> Option<BindingId> {
        let pat_id = self.pat_id_view(ast::Pat::IdentPat(pat))?;
        if let Pat::Bind { id, .. } = self.store()?.pat(pat_id.as_pat()?) { Some(id) } else { None }
    }

    pub(crate) fn resolve_method_call_as_callable_view(
        &self,
        db: &'db Db,
        call: ast::MethodCallExpr<'_>,
    ) -> Option<Callable<'db>> {
        let expr_id = self.expr_id_view(ast::Expr::MethodCallExpr(call))?.as_expr()?;
        let (func, args) = self.infer()?.method_resolution(expr_id)?;
        let interner = DbInterner::new_no_crate(db);
        let ty = db.value_ty(func.into())?.instantiate(interner, args).skip_norm_wip();
        let ty = self.ty(db, ty);
        let mut res = ty.as_callable(db)?;
        res.is_bound_method = true;
        Some(res)
    }

    pub(crate) fn resolve_method_call_view(
        &self,
        db: &'db Db,
        call: ast::MethodCallExpr<'_>,
    ) -> Option<Function> {
        let expr_id = self.expr_id_view(ast::Expr::MethodCallExpr(call))?.as_expr()?;
        let (f_in_trait, substs) = self.infer()?.method_resolution(expr_id)?;
        Some(self.resolve_impl_method_or_trait_def(db, f_in_trait, substs))
    }

    pub(crate) fn resolve_expr_as_callable_view(
        &self,
        db: &'db Db,
        call: ast::Expr<'_>,
    ) -> Option<Callable<'db>> {
        let (orig, adjusted) = self.type_of_expr_view(db, call)?;
        adjusted.unwrap_or(orig).as_callable(db)
    }

    pub(crate) fn resolve_field_view(
        &self,
        field: ast::FieldExpr<'_>,
    ) -> Option<Either<Field, TupleField<'db>>> {
        let def = self.infer_body?;
        let expr_id = self.expr_id_view(ast::Expr::FieldExpr(field))?.as_expr()?;
        self.infer()?.field_resolution(expr_id).map(|it| {
            it.map_either(Into::into, |f| TupleField { owner: def, tuple: f.tuple, index: f.index })
        })
    }

    pub(crate) fn resolve_record_field_view(
        &self,
        db: &'db Db,
        field: ast::RecordExprField<'_>,
    ) -> Option<(Field, Option<Local<'db>>, Type<'db>, GenericSubstitution<'db>)> {
        let record_expr =
            ast::RecordExpr::cast(field.syntax().parent().and_then(|p| p.parent())?)?;
        let expr_id = self
            .store_sm()?
            .node_expr_view(InFile::new(self.file_id, ast::Expr::RecordExpr(record_expr)))?;
        let interner = DbInterner::new_no_crate(db);

        let ast_name = field.field_name()?;
        let local_name = ast_name.as_name();
        let local = if field.name_ref().is_some() {
            None
        } else {
            // Shorthand syntax, resolve to the local
            // alloc: an interned `Name`'s count and a one-segment path, as the handle version.
            let mod_path = ModPath::from_segments(PathKind::Plain, once(local_name.clone()));
            let path = Path::from_known_path_with_no_generic(&mod_path);
            match self.resolver.resolve_path_in_value_ns_fully(
                db,
                path,
                name_hygiene_view(db, InFile::new(self.file_id, ast_name.syntax())),
            ) {
                Some(ValueNs::LocalBinding(binding_id)) => Some(Local {
                    binding_id,
                    parent: self.owner()?,
                    parent_infer: self.infer_body?,
                }),
                _ => None,
            }
        };
        let (adt, subst) = self.infer()?.type_of_expr_or_pat(expr_id)?.as_adt()?;
        let variant = self.infer()?.variant_resolution_for_expr_or_pat(expr_id)?;
        let variant_data = variant.fields(db);
        let field = FieldId { parent: variant, local_id: variant_data.field(&local_name)? };
        let field_ty = (*db.field_types(variant).get(field.local_id)?)
            .ty()
            .instantiate(interner, subst)
            .skip_norm_wip();
        Some((
            field.into(),
            local,
            self.ty(db, field_ty),
            GenericSubstitution::new(adt.into(), subst, self.type_owner),
        ))
    }

    pub(crate) fn resolve_record_pat_field_view(
        &self,
        db: &'db Db,
        field: ast::RecordPatField<'_>,
    ) -> Option<(Field, Type<'db>, GenericSubstitution<'db>)> {
        let interner = DbInterner::new_no_crate(db);
        let field_name = field.field_name()?.as_name();
        let record_pat = ast::RecordPat::cast(field.syntax().parent().and_then(|p| p.parent())?)?;
        let pat_id = self.pat_id_view(ast::Pat::RecordPat(record_pat))?;
        let variant = self.infer()?.variant_resolution_for_pat(pat_id.as_pat()?)?;
        let variant_data = variant.fields(db);
        let field = FieldId { parent: variant, local_id: variant_data.field(&field_name)? };
        let (adt, subst) = self.infer()?.pat_ty(pat_id.as_pat()?).as_adt()?;
        let field_ty = (*db.field_types(variant).get(field.local_id)?)
            .ty()
            .instantiate(interner, subst)
            .skip_norm_wip();
        Some((
            field.into(),
            self.ty(db, field_ty),
            GenericSubstitution::new(adt.into(), subst, self.type_owner),
        ))
    }

    pub(crate) fn resolve_tuple_struct_pat_fields_view(
        &self,
        db: &'db Db,
        tuple_struct_pat: ast::TupleStructPat<'_>,
    ) -> Option<Vec<(Field, Type<'db>)>> {
        let interner = DbInterner::new_no_crate(db);
        let pat_id = self.pat_id_view(ast::Pat::TupleStructPat(tuple_struct_pat))?;
        let variant_id = self.infer()?.variant_resolution_for_pat(pat_id.as_pat()?)?;
        let (_adt, substs) = self.infer()?.pat_ty(pat_id.as_pat()?).as_adt()?;
        Some(
            db.field_types(variant_id)
                .iter()
                .map(|(local_id, field)| {
                    let def = Field { parent: variant_id.into(), id: local_id };
                    let ty = field.ty().instantiate(interner, substs).skip_norm_wip();
                    (def, self.ty(db, ty))
                })
                .collect(), // alloc: the pattern's fields, what the handle version gives
        )
    }

    pub(crate) fn resolve_bind_pat_to_const_view(
        &self,
        db: &'db Db,
        pat: ast::IdentPat<'_>,
    ) -> Option<ModuleDef> {
        let expr_or_pat_id = self.pat_id_view(ast::Pat::IdentPat(pat))?;
        let store = self.store()?;
        let path = match expr_or_pat_id {
            ExprOrPatId::ExprId(idx) => match store.expr(idx) {
                Expr::Path(path) => path,
                _ => return None,
            },
            ExprOrPatId::PatId(idx) => match store.pat(idx) {
                Pat::Path(path) => path,
                _ => return None,
            },
        };
        let store_owner = self.resolver.expression_store_owner();
        let (res, _) = resolve_hir_value_path(
            db,
            &self.resolver,
            store_owner,
            self.infer_body,
            path,
            HygieneId::ROOT,
        )?;
        match res {
            PathResolution::Def(def) => Some(def),
            _ => None,
        }
    }

    pub(crate) fn resolve_offset_of_field_view(
        &self,
        db: &'db Db,
        name_ref: ast::NameRef<'_>,
    ) -> Option<(Either<EnumVariant, Field>, GenericSubstitution<'db>)> {
        let offset_of_expr = ast::OffsetOfExpr::cast(name_ref.syntax().parent()?)?;
        let container = offset_of_expr.ty()?;
        let container = self.type_of_type_view(db, container)?;

        let env = self.trait_environment(db);
        let interner = DbInterner::new_with(db, env.krate);
        let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);

        let mut container = Either::Right(container.ty.skip_binder());
        for field_name in offset_of_expr.fields() {
            if let Either::Right(container) = &mut container {
                *container = structurally_normalize_ty(&infcx, *container, env.param_env);
            }
            let handle_variants =
                |variant: VariantId, subst: GenericArgs<'db>, container: &mut _| {
                    let fields = variant.fields(db);
                    let field = fields.field(&field_name.as_name())?;
                    let field_types = db.field_types(variant);
                    *container = Either::Right(
                        field_types[field].ty().instantiate(interner, subst).skip_norm_wip(),
                    );
                    let generic_def = match variant {
                        VariantId::EnumVariantId(it) => it.loc(db).parent.into(),
                        VariantId::StructId(it) => it.into(),
                        VariantId::UnionId(it) => it.into(),
                    };
                    Some((
                        Either::Right(Field { parent: variant.into(), id: field }),
                        generic_def,
                        subst,
                    ))
                };
            let temp_ty = Ty::new_error(interner, ErrorGuaranteed);
            let (field_def, generic_def, subst) =
                match std::mem::replace(&mut container, Either::Right(temp_ty)) {
                    Either::Left((variant_id, subst)) => {
                        handle_variants(VariantId::from(variant_id), subst, &mut container)?
                    }
                    Either::Right(container_ty) => match container_ty.kind() {
                        TyKind::Adt(adt_def, subst) => match adt_def.def_id() {
                            AdtId::StructId(id) => {
                                handle_variants(id.into(), subst, &mut container)?
                            }
                            AdtId::UnionId(id) => {
                                handle_variants(id.into(), subst, &mut container)?
                            }
                            AdtId::EnumId(id) => {
                                let variants = id.enum_variants(db);
                                let variant = variants.variant(&field_name.as_name())?;
                                container = Either::Left((variant, subst));
                                (Either::Left(EnumVariant { id: variant }), id.into(), subst)
                            }
                        },
                        _ => return None,
                    },
                };

            if field_name.syntax().text_range() == name_ref.syntax().text_range() {
                return Some((
                    field_def,
                    GenericSubstitution::new(generic_def, subst, self.type_owner),
                ));
            }
        }
        never!("the `NameRef` is a child of the `OffsetOfExpr`, we should've visited it");
        None
    }

    /// What `path` resolves to, with the generic arguments it was given.
    pub(crate) fn resolve_path_view(
        &self,
        db: &'db Db,
        path: ast::Path<'_>,
    ) -> Option<(PathResolution<'db>, Option<GenericSubstitution<'db>>)> {
        let parent = path.syntax().parent();

        let mut prefer_value_ns = parent.and_then(ast::PathExpr::cast).is_some();
        let resolved = (|| {
            let infer = self.infer()?;
            if let Some(path_expr) = parent.and_then(ast::PathExpr::cast) {
                let expr_id = self.expr_id_view(ast::Expr::PathExpr(path_expr))?;
                if let Some((assoc, subs)) = infer.assoc_resolutions_for_expr_or_pat(expr_id) {
                    let (assoc, subst) = match assoc {
                        CandidateId::FunctionId(f_in_trait) => {
                            match infer.type_of_expr_or_pat(expr_id) {
                                None => {
                                    let subst = GenericSubstitution::new(
                                        f_in_trait.into(),
                                        subs,
                                        self.type_owner,
                                    );
                                    (AssocItem::Function(f_in_trait.into()), Some(subst))
                                }
                                Some(func_ty) => {
                                    if let TyKind::FnDef(_fn_def, subs) = func_ty.kind() {
                                        let (fn_, subst) = self
                                            .resolve_impl_method_or_trait_def_with_subst(
                                                db,
                                                f_in_trait,
                                                subs.no_bound_vars().unwrap(),
                                            );
                                        let subst = GenericSubstitution::new_from_fn(
                                            fn_,
                                            subst,
                                            self.type_owner,
                                        );
                                        (AssocItem::Function(fn_), subst)
                                    } else {
                                        let subst = GenericSubstitution::new(
                                            f_in_trait.into(),
                                            subs,
                                            self.type_owner,
                                        );
                                        (AssocItem::Function(f_in_trait.into()), Some(subst))
                                    }
                                }
                            }
                        }
                        CandidateId::ConstId(const_id) => {
                            let (konst, subst) =
                                self.resolve_impl_const_or_trait_def_with_subst(db, const_id, subs);
                            let subst =
                                GenericSubstitution::new(konst.into(), subst, self.type_owner);
                            (AssocItem::Const(konst.into()), Some(subst))
                        }
                    };

                    return Some((PathResolution::Def(assoc.into()), subst));
                }
                if let Some(VariantId::EnumVariantId(variant)) =
                    infer.variant_resolution_for_expr_or_pat(expr_id)
                {
                    return Some((
                        PathResolution::Def(ModuleDef::EnumVariant(variant.into())),
                        None,
                    ));
                }
                prefer_value_ns = true;
            } else if let Some(path_pat) = parent.and_then(ast::PathPat::cast) {
                let expr_or_pat_id = self.pat_id_view(ast::Pat::PathPat(path_pat))?;
                if let Some((assoc, subs)) = infer.assoc_resolutions_for_expr_or_pat(expr_or_pat_id)
                {
                    let (assoc, subst) = match assoc {
                        CandidateId::ConstId(const_id) => {
                            let (konst, subst) =
                                self.resolve_impl_const_or_trait_def_with_subst(db, const_id, subs);
                            let subst =
                                GenericSubstitution::new(konst.into(), subst, self.type_owner);
                            (AssocItemId::from(konst), subst)
                        }
                        CandidateId::FunctionId(function_id) => (
                            function_id.into(),
                            GenericSubstitution::new(function_id.into(), subs, self.type_owner),
                        ),
                    };
                    return Some((PathResolution::Def(AssocItem::from(assoc).into()), Some(subst)));
                }
                if let Some(VariantId::EnumVariantId(variant)) =
                    infer.variant_resolution_for_expr_or_pat(expr_or_pat_id)
                {
                    return Some((
                        PathResolution::Def(ModuleDef::EnumVariant(variant.into())),
                        None,
                    ));
                }
            } else if let Some(rec_lit) = parent.and_then(ast::RecordExpr::cast) {
                let expr_id = self.expr_id_view(ast::Expr::RecordExpr(rec_lit))?;
                if let Some(VariantId::EnumVariantId(variant)) =
                    infer.variant_resolution_for_expr_or_pat(expr_id)
                {
                    return Some((
                        PathResolution::Def(ModuleDef::EnumVariant(variant.into())),
                        None,
                    ));
                }
            } else {
                let record_pat = parent.and_then(ast::RecordPat::cast).map(ast::Pat::RecordPat);
                let tuple_struct_pat =
                    || parent.and_then(ast::TupleStructPat::cast).map(ast::Pat::TupleStructPat);
                if let Some(pat) = record_pat.or_else(tuple_struct_pat) {
                    let pat_id = self.pat_id_view(pat)?;
                    let variant_res_for_pat = infer.variant_resolution_for_pat(pat_id.as_pat()?);
                    if let Some(VariantId::EnumVariantId(variant)) = variant_res_for_pat {
                        return Some((
                            PathResolution::Def(ModuleDef::EnumVariant(variant.into())),
                            None,
                        ));
                    }
                }
            }
            None
        })();
        if resolved.is_some() {
            return resolved;
        }

        let mut collector = self.path_collector(db);
        let hir_path = collector.lower_path_view(path, &mut ImplTraitLowering::Error)?;
        let parent_hir_path = path
            .parent_path()
            .and_then(|p| collector.lower_path_view(p, &mut ImplTraitLowering::Error));
        // The store is read only to lower the path's type anchor.
        let lowered = if collector.store.path_at(hir_path).type_anchor().is_some() {
            Either::Left(collector.store.finish().0)
        } else {
            Either::Right(collector.store)
        };
        let (hir_path, parent_hir_path, store) = match &lowered {
            Either::Left(store) => {
                (store.path_at(hir_path), parent_hir_path.map(|it| store.path_at(it)), Some(store))
            }
            Either::Right(builder) => {
                (builder.path_at(hir_path), parent_hir_path.map(|it| builder.path_at(it)), None)
            }
        };

        // Case where path is a qualifier of a use tree, e.g. foo::bar::{Baz, Qux} where we are
        // trying to resolve foo::bar.
        if let Some(use_tree) = parent.and_then(ast::UseTree::cast)
            && use_tree.coloncolon_token().is_some()
        {
            return resolve_hir_path_qualifier(db, &self.resolver, hir_path, store)
                .map(|it| (it, None));
        }

        let meta_path = path
            .syntax()
            .ancestors()
            .take_while(|it| {
                let kind = it.kind();
                ast::Path::can_cast(kind) || ast::Meta::can_cast(kind)
            })
            .last()
            .and_then(ast::Meta::cast);

        let tool_module = || {
            path.first_segment()
                .and_then(|it| it.name_ref())
                .and_then(|name_ref| {
                    ToolModule::by_name(db, self.resolver.krate().into(), name_ref.text())
                        .map(PathResolution::ToolModule)
                })
                .map(|it| (it, None))
        };

        // Case where path is a qualifier of another path, e.g. foo::bar::Baz where we are
        // trying to resolve foo::bar.
        if let Some(parent_hir_path) = parent_hir_path {
            return match resolve_hir_path_qualifier(db, &self.resolver, hir_path, store) {
                None if meta_path.is_some() => tool_module(),
                // The type name conflicts with a module in scope (`use std::str;` then
                // `str::from_utf8()`): the module when it has the next segment.
                Some(it) if matches!(it, PathResolution::Def(ModuleDef::BuiltinType(_))) => {
                    if let Some(mod_path) = hir_path.mod_path()
                        && let Some(ModuleDefId::ModuleId(id)) =
                            self.resolver.resolve_module_path_in_items(db, mod_path).take_types()
                    {
                        let parent_hir_name = parent_hir_path.segments().get(1).map(|it| it.name);
                        let module = crate::Module { id };
                        if module
                            .scope(db, None)
                            .into_iter()
                            .any(|(name, _)| Some(&name) == parent_hir_name)
                        {
                            return Some((PathResolution::Def(ModuleDef::Module(module)), None));
                        };
                    }
                    Some((it, None))
                }
                res => res.map(|it| (it, None)),
            };
        } else if let Some(meta_path) = meta_path {
            // The last segment of a path in an attribute: inert and builtin attributes and tools
            // first, then attribute macros.
            if let Some(name_ref) = path.as_single_name_ref() {
                let builtin = BuiltinAttr::builtin(name_ref.text());
                if builtin.is_some() {
                    return builtin.map(|it| (PathResolution::BuiltinAttr(it), None));
                }

                if let Some(attr) = meta_path.parent_attr() {
                    let adt =
                        attr.syntax().ancestors().find_map(ast::Item::cast).and_then(
                            |it| match it {
                                ast::Item::Struct(it) => Some(ast::Adt::Struct(it)),
                                ast::Item::Enum(it) => Some(ast::Adt::Enum(it)),
                                ast::Item::Union(it) => Some(ast::Adt::Union(it)),
                                _ => None,
                            },
                        );
                    if let Some(adt) = adt {
                        let ast_id = self.file_id.ast_id_map(db).ast_id_of_view::<tag::Adt>(adt);
                        if let Some(helpers) = self
                            .resolver
                            .def_map()
                            .derive_helpers_in_scope(db, InFile::new(self.file_id, ast_id))
                        {
                            let name_ref = name_ref.as_name();
                            for (macro_id, mut helpers) in
                                helpers.iter().chunk_by(|(_, macro_id, ..)| macro_id).into_iter()
                            {
                                if let Some(idx) = helpers.position(|(name, ..)| *name == name_ref)
                                {
                                    return Some((
                                        PathResolution::DeriveHelper(DeriveHelper {
                                            derive: *macro_id,
                                            idx: idx as u32,
                                        }),
                                        None,
                                    ));
                                }
                            }
                        }
                    }
                }
            }
            return match resolve_hir_path_as_attr_macro(db, &self.resolver, hir_path) {
                Some(m) => Some((PathResolution::Def(ModuleDef::Macro(m)), None)),
                // A path that starts with a tool module is labelled the tool itself.
                None => tool_module(),
            };
        }
        if parent.is_some_and(|it| ast::Visibility::can_cast(it.kind())) {
            // Only modules can be inside visibilities, and those have no generics.
            resolve_hir_path_qualifier(db, &self.resolver, hir_path, store).map(|it| (it, None))
        } else {
            let res = resolve_hir_path_(
                db,
                &self.resolver,
                self.infer_body,
                hir_path,
                prefer_value_ns,
                name_hygiene_view(db, InFile::new(self.file_id, path.syntax())),
                store,
                false,
            )
            .any()?;
            let subst = (|| {
                let parent = parent?;
                let ty = if let Some(expr) = ast::Expr::cast(parent) {
                    let expr_id = self.expr_id_view(expr)?;
                    self.infer()?.type_of_expr_or_pat(expr_id)?
                } else {
                    let pat = ast::Pat::cast(parent)?;
                    let pat_id = self.pat_id_view(pat)?;
                    self.infer()?.expr_or_pat_ty(pat_id)
                };
                let (subst, expected_resolution) = match ty.kind() {
                    TyKind::Adt(adt_def, subst) => {
                        let adt_id = adt_def.def_id();
                        (
                            GenericSubstitution::new(adt_id.into(), subst, self.type_owner),
                            PathResolution::Def(ModuleDef::Adt(adt_id.into())),
                        )
                    }
                    TyKind::Alias(
                        _,
                        AliasTy { kind: AliasTyKind::Projection { def_id }, args, .. },
                    ) => {
                        let assoc_id = def_id.0;
                        (
                            GenericSubstitution::new(assoc_id.into(), args, self.type_owner),
                            PathResolution::Def(ModuleDef::TypeAlias(assoc_id.into())),
                        )
                    }
                    TyKind::FnDef(fn_id, subst) => {
                        let generic_def_id = match fn_id.0 {
                            CallableDefId::StructId(id) => id.into(),
                            CallableDefId::FunctionId(id) => id.into(),
                            CallableDefId::EnumVariantId(_) => return None,
                        };
                        let subst = subst.no_bound_vars().unwrap();
                        (
                            GenericSubstitution::new(generic_def_id, subst, self.type_owner),
                            PathResolution::Def(ModuleDefId::from(fn_id.0).into()),
                        )
                    }
                    _ => return None,
                };
                if res != expected_resolution {
                    return None;
                }
                Some(subst)
            })();
            Some((res, subst))
        }
    }

    pub(crate) fn resolve_hir_path_per_ns_view(
        &self,
        db: &'db Db,
        path: ast::Path<'_>,
    ) -> Option<PathResolutionPerNs<'db>> {
        let mut collector = self.path_collector(db);
        let hir_path = collector.lower_path_view(path, &mut ImplTraitLowering::Error)?;
        // The store is read only to lower the path's type anchor.
        let lowered = if collector.store.path_at(hir_path).type_anchor().is_some() {
            Either::Left(collector.store.finish().0)
        } else {
            Either::Right(collector.store)
        };
        let (hir_path, store) = match &lowered {
            Either::Left(store) => (store.path_at(hir_path), Some(store)),
            Either::Right(builder) => (builder.path_at(hir_path), None),
        };
        Some(resolve_hir_path_(
            db,
            &self.resolver,
            self.infer_body,
            hir_path,
            false,
            name_hygiene_view(db, InFile::new(self.file_id, path.syntax())),
            store,
            true,
        ))
    }

    pub(crate) fn resolve_variant_view(&self, record_lit: ast::RecordExpr<'_>) -> Option<VariantId> {
        let infer = self.infer()?;
        let expr_id = self.expr_id_view(ast::Expr::RecordExpr(record_lit))?;
        infer.variant_resolution_for_expr_or_pat(expr_id)
    }
}

/// [`super::scope_for`] from a view: the scope of the innermost expression of the store above
/// `node`, climbing out of macro expansions, stopping at the enclosing item.
fn scope_for_view<'db>(
    db: &'db Db,
    scopes: &ExprScopes,
    source_map: &ExpressionStoreSourceMap,
    node: InFile<Node<'db>>,
) -> Option<(ScopeId, ExprId)> {
    node.ancestors_with_macros_view(db)
        .take_while(|it| {
            let kind = it.value.kind();
            !ast::Item::can_cast(kind)
                || ast::MacroCall::can_cast(kind)
                || ast::Use::can_cast(kind)
                || ast::AsmExpr::can_cast(kind)
        })
        .filter_map(|it| it.map(ast::Expr::cast).transpose())
        .filter_map(|it| source_map.node_expr_view(it)?.as_expr())
        .find_map(|expr| scopes.scope_for(expr).map(|scope| (scope, expr)))
}

/// [`super::name_hygiene`] of a view.
pub(crate) fn name_hygiene_view(db: &Db, name: InFile<Node<'_>>) -> HygieneId {
    let Some(macro_file) = name.file_id.macro_file() else {
        return HygieneId::ROOT;
    };
    let span_map = macro_file.expansion_span_map(db);
    let ctx = span_map.span_at(name.value.text_range().start()).ctx;
    HygieneId::new(ctx.opaque_and_semiopaque(db))
}
