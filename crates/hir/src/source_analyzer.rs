//! Lookup hir elements using positions in the source code. This is a lossy
//! transformation: in general, a single source might correspond to several
//! modules, functions, etc, due to macros, cfgs and `#[path=]` attributes on
//! modules.
//!
//! So, this modules should not be used during hir construction, it exists
//! purely for "IDE needs".
mod view;

use std::cell::OnceCell;

use base_db::Db;
use either::Either;
use hir_def::{
    AdtId, ConstId, DefWithBodyId, ExpressionStoreOwnerId, FunctionId, GenericDefId, HasModule,
    LoweringMode, ModuleDefId, StructId, VariantId,
    expr_store::{
        Body, BodySourceMap, ExpressionStore, ExpressionStoreSourceMap, HygieneId,
        lower::ExprCollector, path::Path,
    },
    hir::generics::GenericParams,
    nameres::MacroSubNs,
    resolver::{ResolveValueResult, Resolver, TypeNs, ValueNs},
    visibility::Visibility,
};
use hir_expand::HirFileId;
use hir_ty::{
    InferBodyId, InferenceResult, LifetimeElisionKind, LifetimeLoweringMode, ParamEnvAndCrate,
    TyLoweringContext,
    method_resolution,
    next_solver::{
        DbInterner, EarlyBinder, GenericArgs, ParamEnv, Ty, TypingMode, infer::DbInternerInferExt,
    },
};
use rustc_type_ir::inherent::Ty as _;
use span::{AstIdMap, RealSpanMap};

use crate::{
    Adt, AnyFunctionId, BuiltinType, Const, EnumVariant, Function, Local, Macro, ModuleDef,
    Static, Struct, Trait, Type, TypeAlias, TypeOwnerId,
    db::HirDatabase,
    semantics::{PathResolution, PathResolutionPerNs},
};

/// `SourceAnalyzer` is a convenience wrapper which exposes HIR API in terms of
/// original source files. It should not be used inside the HIR itself.
#[derive(Debug)]
pub(crate) struct SourceAnalyzer<'db> {
    pub(crate) file_id: HirFileId,
    pub(crate) resolver: Resolver<'db>,
    pub(crate) body_or_sig: Option<BodyOrSig<'db>>,
    pub(crate) type_owner: TypeOwnerId,
    pub(crate) infer_body: Option<InferBodyId<'db>>,
    /// The span and ast id maps of the tree `file_id`'s nodes are read from, when it is not the
    /// file's own (a body's relative tree, [`crate::BodyAnalysis`]): a path lowered again here
    /// reads them, not the file's whole maps.
    pub(crate) maps: Option<(&'db RealSpanMap, &'db AstIdMap)>,
}

#[derive(Debug)]
pub(crate) enum BodyOrSig<'db> {
    Body {
        def: DefWithBodyId,
        body: &'db Body,
        source_map: &'db BodySourceMap,
        infer: Option<&'db InferenceResult<'db>>,
    },
    VariantFields {
        def: VariantId,
        store: &'db ExpressionStore,
        source_map: &'db ExpressionStoreSourceMap,
        infer: Option<&'db InferenceResult<'db>>,
    },
    Sig {
        def: GenericDefId,
        store: &'db ExpressionStore,
        source_map: &'db ExpressionStoreSourceMap,
        infer: Option<&'db InferenceResult<'db>>,
        #[expect(dead_code)]
        generics: &'db GenericParams,
    },
}

impl<'db> SourceAnalyzer<'db> {
    /// A collector to lower a path of this analyzer's tree again, over its [`Self::maps`].
    fn path_collector(&self, db: &'db Db) -> ExprCollector<'db> {
        let module = self.resolver.module();
        ExprCollector::with_maps(db, module, self.file_id, LoweringMode::Ide, self.maps)
    }

    fn owner(&self) -> Option<ExpressionStoreOwnerId> {
        self.body_or_sig.as_ref().map(|it| match *it {
            BodyOrSig::VariantFields { def, .. } => def.into(),
            BodyOrSig::Sig { def, .. } => def.into(),
            BodyOrSig::Body { def, .. } => def.into(),
        })
    }

    fn infer(&self) -> Option<&'db InferenceResult<'db>> {
        self.body_or_sig.as_ref().and_then(|it| match *it {
            BodyOrSig::VariantFields { infer, .. }
            | BodyOrSig::Sig { infer, .. }
            | BodyOrSig::Body { infer, .. } => infer,
        })
    }

    pub(crate) fn ty(&self, db: &'db Db, ty: Ty<'db>) -> Type<'db> {
        Type { owner: self.type_owner, ty: EarlyBinder::bind(DbInterner::new_no_crate(db), ty) }
    }

    pub(crate) fn store(&self) -> Option<&'db ExpressionStore> {
        self.body_or_sig.as_ref().map(|it| match *it {
            BodyOrSig::Sig { store, .. } => store,
            BodyOrSig::VariantFields { store, .. } => store,
            BodyOrSig::Body { body, .. } => &body.store,
        })
    }

    pub(crate) fn store_sm(&self) -> Option<&'db ExpressionStoreSourceMap> {
        self.body_or_sig.as_ref().map(|it| match *it {
            BodyOrSig::Sig { source_map, .. } => source_map,
            BodyOrSig::VariantFields { source_map, .. } => source_map,
            BodyOrSig::Body { source_map, .. } => &source_map.store,
        })
    }

    fn param_and<'a>(&self, param_env: ParamEnv<'a>) -> ParamEnvAndCrate<'a> {
        ParamEnvAndCrate { param_env, krate: self.resolver.krate() }
    }

    /// The item whose generics the analyzed store's types are written in. Not the resolver's
    /// innermost generic scope: a static's resolver has none, yet its types lower in it.
    pub(crate) fn generic_def(&self, db: &'db Db) -> Option<GenericDefId> {
        self.body_or_sig.as_ref().map(|it| match *it {
            BodyOrSig::Body { def, .. } => def.generic_def(db),
            BodyOrSig::VariantFields { def, .. } => match def {
                VariantId::EnumVariantId(def) => def.loc(db).parent.into(),
                VariantId::StructId(def) => def.into(),
                VariantId::UnionId(def) => def.into(),
            },
            BodyOrSig::Sig { def, .. } => def,
        })
    }

    fn trait_environment(&self, db: &'db Db) -> ParamEnvAndCrate<'db> {
        self.param_and(self.generic_def(db).map_or_else(ParamEnv::empty, |def| db.trait_environment(def)))
    }

    fn resolve_impl_method_or_trait_def(
        &self,
        db: &'db Db,
        func: FunctionId,
        substs: GenericArgs<'db>,
    ) -> Function {
        self.resolve_impl_method_or_trait_def_with_subst(db, func, substs).0
    }

    fn resolve_impl_method_or_trait_def_with_subst(
        &self,
        db: &'db Db,
        func: FunctionId,
        substs: GenericArgs<'db>,
    ) -> (Function, GenericArgs<'db>) {
        let owner = match self.generic_def(db) {
            Some(it) => it,
            None => return (func.into(), substs),
        };
        let env = self.param_and(db.trait_environment(owner));
        let (func, args) = db.lookup_impl_method(env, func, substs);
        match func {
            Either::Left(func) => (func.into(), args),
            Either::Right((impl_, method)) => {
                (Function { id: AnyFunctionId::BuiltinDeriveImplMethod { method, impl_ } }, args)
            }
        }
    }

    fn resolve_impl_const_or_trait_def_with_subst(
        &self,
        db: &'db Db,
        const_id: ConstId,
        subs: GenericArgs<'db>,
    ) -> (ConstId, GenericArgs<'db>) {
        let owner = match self.generic_def(db) {
            Some(it) => it,
            None => return (const_id, subs),
        };
        let env = self.param_and(db.trait_environment(owner));
        let interner = DbInterner::new_with(db, env.krate);
        let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);
        method_resolution::lookup_impl_const(&infcx, env.param_env, const_id, subs)
    }

}

#[inline]
pub(crate) fn resolve_hir_path<'db>(
    db: &'db Db,
    resolver: &Resolver<'db>,
    infer_body: Option<InferBodyId<'db>>,
    path: Path<'_>,
    hygiene: HygieneId,
    store: Option<&ExpressionStore>,
) -> Option<PathResolution<'db>> {
    resolve_hir_path_(db, resolver, infer_body, path, false, hygiene, store, false).any()
}

#[inline]
pub(crate) fn resolve_hir_path_as_attr_macro(
    db: &Db,
    resolver: &Resolver<'_>,
    path: Path<'_>,
) -> Option<Macro> {
    resolver
        .resolve_path_as_macro(db, path.mod_path()?, Some(MacroSubNs::Attr))
        .map(|it| it.def)
        .map(Into::into)
}

fn resolve_hir_path_<'db>(
    db: &'db Db,
    resolver: &Resolver<'db>,
    infer_body: Option<InferBodyId<'db>>,
    path: Path<'_>,
    prefer_value_ns: bool,
    hygiene: HygieneId,
    store: Option<&ExpressionStore>,
    resolve_per_ns: bool,
) -> PathResolutionPerNs<'db> {
    let types = || {
        let (ty, unresolved, ty_is_visible) = match path.type_anchor() {
            Some(type_ref) => resolver.generic_def().and_then(|def| {
                let store = store?;
                let generics = OnceCell::new();
                let (_, res) = blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
                    TyLoweringContext::new(
                        db,
                        resolver,
                        store,
                        resolver.expression_store_owner().unwrap_or_else(|| def.into()),
                        def,
                        &generics,
                        LifetimeElisionKind::Infer,
                        LifetimeLoweringMode::LateParam,
                        frame,
                    )
                    .with_interning_mode(LoweringMode::Ide)
                    .lower_ty_ext(type_ref)
                });
                res.map(|ty_ns| (ty_ns, path.segments().first(), Visibility::Public))
            }),
            None => {
                let (ty, remaining_idx, _, _, vis) =
                    resolver.resolve_path_in_type_ns_with_prefix_info(db, path)?;
                match remaining_idx {
                    Some(remaining_idx) => {
                        if remaining_idx + 1 == path.segments().len() {
                            Some((ty, path.segments().last(), vis))
                        } else {
                            None
                        }
                    }
                    None => Some((ty, None, vis)),
                }
            }
        }?;

        // If we are in a TypeNs for a Trait, and we have an unresolved name, try to resolve it as a type
        // within the trait's associated types.
        if let (Some(unresolved), &TypeNs::TraitId(trait_id)) = (&unresolved, &ty)
            && let Some(type_alias_id) =
                trait_id.trait_items(db).associated_type_by_name(unresolved.name)
        {
            return Some((
                PathResolution::Def(ModuleDefId::from(type_alias_id).into()),
                ty_is_visible,
            ));
        }

        let res = match ty {
            TypeNs::SelfType(it) => PathResolution::SelfType(it.into()),
            TypeNs::GenericParam(id) => PathResolution::TypeParam(id.into()),
            TypeNs::AdtSelfType(it) | TypeNs::AdtId(it) => {
                PathResolution::Def(Adt::from(it).into())
            }
            TypeNs::EnumVariantId(it) => PathResolution::Def(EnumVariant::from(it).into()),
            TypeNs::TypeAliasId(it) => PathResolution::Def(TypeAlias::from(it).into()),
            TypeNs::BuiltinType(it) => PathResolution::Def(BuiltinType::from(it).into()),
            TypeNs::TraitId(it) => PathResolution::Def(Trait::from(it).into()),
            TypeNs::ModuleId(it) => PathResolution::Def(ModuleDef::Module(it.into())),
        };
        match unresolved {
            Some(unresolved) => resolver
                .generic_def()
                .and_then(|def| {
                    hir_ty::associated_type_shorthand_candidates(
                        db,
                        def,
                        res.in_type_ns()?,
                        |name, _| name == unresolved.name,
                    )
                })
                .map(TypeAlias::from)
                .map(Into::into)
                .map(|def| (PathResolution::Def(def), ty_is_visible)),
            None => Some((res, ty_is_visible)),
        }
    };

    let body_owner = resolver.expression_store_owner();
    let values = || resolve_hir_value_path(db, resolver, body_owner, infer_body, path, hygiene);

    let items = || {
        resolver
            .resolve_module_path_in_items(db, path.mod_path()?)
            .take_types_full()
            .map(|it| (PathResolution::Def(it.def.into()), it.vis))
    };

    let macros = || {
        resolver
            .resolve_path_as_macro(db, path.mod_path()?, None)
            .map(|res| (PathResolution::Def(ModuleDef::Macro(res.def.into())), res.vis))
    };

    let mut types_ns: Option<Option<_>> = None;
    let mut values_ns: Option<Option<_>> = None;

    let mut types_is_visible: Option<bool> = None;
    let mut values_is_visible: Option<bool> = None;

    if !resolve_per_ns {
        if prefer_value_ns {
            values_ns = Some(values().inspect(|(_, vis)| {
                values_is_visible = Some(resolver.is_visible(db, *vis));
            }));

            if let Some(Some((res, _))) = values_ns
                && values_is_visible.unwrap_or_default()
            {
                return PathResolutionPerNs::new(None, Some(res), None);
            }
        } else {
            types_ns = Some(types().or_else(items).inspect(|(_, vis)| {
                types_is_visible = Some(resolver.is_visible(db, *vis));
            }));

            if let Some(Some((res, _))) = types_ns
                && types_is_visible.unwrap_or_default()
            {
                return PathResolutionPerNs::new(Some(res), None, None);
            }
        }
    }

    let mut macros_is_visible = false;

    let mut types = types_ns.unwrap_or_else(|| types().or_else(items)).map(|(res, vis)| {
        types_is_visible = Some(types_is_visible.unwrap_or_else(|| resolver.is_visible(db, vis)));
        res
    });
    let mut values = values_ns.unwrap_or_else(values).map(|(res, vis)| {
        values_is_visible = Some(values_is_visible.unwrap_or_else(|| resolver.is_visible(db, vis)));
        res
    });
    let mut macros = macros().map(|(res, vis)| {
        macros_is_visible = resolver.is_visible(db, vis);
        res
    });

    let types_is_visible = types_is_visible.unwrap_or_default();
    let values_is_visible = values_is_visible.unwrap_or_default();

    // If there is a visible resolution and an invisible one, we only want to include the visible one. But if all are
    // invisible, we want to include them all.
    if types_is_visible || values_is_visible || macros_is_visible {
        if !types_is_visible {
            types = None;
        }
        if !values_is_visible {
            values = None;
        }
        if !macros_is_visible {
            macros = None;
        }
    }

    PathResolutionPerNs { type_ns: types, value_ns: values, macro_ns: macros }
}

fn resolve_hir_value_path<'db>(
    db: &'db Db,
    resolver: &Resolver<'db>,
    store_owner: Option<ExpressionStoreOwnerId>,
    infer_body: Option<InferBodyId<'db>>,
    path: Path<'_>,
    hygiene: HygieneId,
) -> Option<(PathResolution<'db>, Visibility)> {
    resolver.resolve_path_in_value_ns_with_prefix_info(db, path, hygiene).and_then(
        |(val, _, vis)| {
            let ResolveValueResult::ValueNs(val) = val else { return None };
            let res = match val {
                ValueNs::LocalBinding(binding_id) => {
                    let var = Local { parent: store_owner?, parent_infer: infer_body?, binding_id };
                    PathResolution::Local(var)
                }
                ValueNs::FunctionId(it) => PathResolution::Def(Function::from(it).into()),
                ValueNs::ConstId(it) => PathResolution::Def(Const::from(it).into()),
                ValueNs::StaticId(it) => PathResolution::Def(Static::from(it).into()),
                ValueNs::StructId(it) => PathResolution::Def(Struct::from(it).into()),
                ValueNs::EnumVariantId(it) => PathResolution::Def(EnumVariant::from(it).into()),
                ValueNs::ImplSelf(impl_id) => PathResolution::SelfType(impl_id.into()),
                ValueNs::GenericParam(id) => PathResolution::ConstParam(id.into()),
            };
            Some((res, vis))
        },
    )
}

/// Resolves a path where we know it is a qualifier of another path.
///
/// For example, if we have:
/// ```
/// mod my {
///     pub mod foo {
///         struct Bar;
///     }
///
///     pub fn foo() {}
/// }
/// ```
/// then we know that `foo` in `my::foo::Bar` refers to the module, not the function.
fn resolve_hir_path_qualifier<'db>(
    db: &'db Db,
    resolver: &Resolver<'db>,
    path: Path<'_>,
    store: Option<&ExpressionStore>,
) -> Option<PathResolution<'db>> {
    (|| {
        let (ty, unresolved) = match path.type_anchor() {
            Some(type_ref) => resolver.generic_def().and_then(|def| {
                let store = store?;
                let generics = OnceCell::new();
                let (_, res) = blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
                    TyLoweringContext::new(
                        db,
                        resolver,
                        store,
                        resolver.expression_store_owner().unwrap_or_else(|| def.into()),
                        def,
                        &generics,
                        LifetimeElisionKind::Infer,
                        LifetimeLoweringMode::LateParam,
                        frame,
                    )
                    .with_interning_mode(LoweringMode::Ide)
                    .lower_ty_ext(type_ref)
                });
                res.map(|ty_ns| (ty_ns, path.segments().first()))
            }),
            None => {
                let (ty, remaining_idx, _) = resolver.resolve_path_in_type_ns(db, path)?;
                match remaining_idx {
                    Some(remaining_idx) => {
                        if remaining_idx + 1 == path.segments().len() {
                            Some((ty, path.segments().last()))
                        } else {
                            None
                        }
                    }
                    None => Some((ty, None)),
                }
            }
        }?;

        // If we are in a TypeNs for a Trait, and we have an unresolved name, try to resolve it as a type
        // within the trait's associated types.
        if let (Some(unresolved), &TypeNs::TraitId(trait_id)) = (&unresolved, &ty)
            && let Some(type_alias_id) =
                trait_id.trait_items(db).associated_type_by_name(unresolved.name)
        {
            return Some(PathResolution::Def(ModuleDefId::from(type_alias_id).into()));
        }

        let res = match ty {
            TypeNs::SelfType(it) => PathResolution::SelfType(it.into()),
            TypeNs::GenericParam(id) => PathResolution::TypeParam(id.into()),
            TypeNs::AdtSelfType(it) | TypeNs::AdtId(it) => {
                PathResolution::Def(Adt::from(it).into())
            }
            TypeNs::EnumVariantId(it) => PathResolution::Def(EnumVariant::from(it).into()),
            TypeNs::TypeAliasId(it) => PathResolution::Def(TypeAlias::from(it).into()),
            TypeNs::BuiltinType(it) => PathResolution::Def(BuiltinType::from(it).into()),
            TypeNs::TraitId(it) => PathResolution::Def(Trait::from(it).into()),
            TypeNs::ModuleId(it) => PathResolution::Def(ModuleDef::Module(it.into())),
        };
        match unresolved {
            Some(unresolved) => resolver
                .generic_def()
                .and_then(|def| {
                    hir_ty::associated_type_shorthand_candidates(
                        db,
                        def,
                        res.in_type_ns()?,
                        |name, _| name == unresolved.name,
                    )
                })
                .map(TypeAlias::from)
                .map(Into::into)
                .map(PathResolution::Def),
            None => Some(res),
        }
    })()
    .or_else(|| {
        resolver
            .resolve_module_path_in_items(db, path.mod_path()?)
            .take_types()
            .map(|it| PathResolution::Def(it.into()))
    })
}
