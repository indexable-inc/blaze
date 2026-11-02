//! Transforms `ast::Expr` into an equivalent `hir_def::expr::Expr`
//! representation.

mod asm;
mod format_args;
mod generics;
mod path;

use std::{
    cell::{OnceCell, RefCell},
    mem,
    sync::Arc,
};

use base_db::{Db, FxIndexSet};
use blaze_hash::HashMap;
use cfg::CfgOptions;
use either::Either;
use hir_expand::{
    HirFileId, InFile, MacroDefId,
    builtin::MODULE_PATH_PLACEHOLDER,
    mod_path::{ModPath, PathKind},
    name::{AsName, Name},
    span_map::SpanMap,
};
use intern::{Symbol, sym};
use itertools::Itertools;
use rustc_abi::ExternAbi;
use smallvec::SmallVec;
use span::{AstIdMap, RealSpanMap, SyntaxContext};
use syntax::{
    AstPtr, FlatTree, SyntaxNodePtr, ast as tag,
    view::{
        self as ast, ArrayExprKind, AstChildren, AstNode, BlockExpr, ForBinder, HasArgList,
        HasAttrs, HasGenericArgs, HasGenericParams, HasLoopBody, HasName, HasTypeBounds, IsString,
        RangeItem, SlicePatComponents, Typed,
    },
};
use thin_vec::ThinVec;
use tt::TextRange;

use crate::{
    AdtId, BlockId, BlockIdLt, ConstId, DefWithBodyId, FunctionId, GenericDefId, HasModule, ImplId,
    ItemContainerId, LoweringMode, MacroId, ModuleDefId, ModuleId, TraitId, TypeAliasId,
    UnresolvedMacro,
    attrs::AttrFlags,
    expr_store::{
        Body, BodySourceMap, ExprPtr, ExprRoot, ExpressionStore, ExpressionStoreBuilder,
        ExpressionStoreDiagnostics, ExpressionStoreSourceMap, HygieneId, LabelPtr, LifetimePtr,
        ListMark, Lists, MacroCallPtr, MissingBodyItemKind, PatPtr, SelfParamPtr, StoreVisitor,
        TypePtr,
        body::Param,
        expander::Expander,
        path::{
            BindingRow, GenericArg, GenericArgs, GenericArgsParentheses, Names, Path, PathIdx, slot,
        },
    },
    hir::{
        Array, Binding, BindingAnnotation, BindingId, BindingProblems, CaptureBy, ClosureKind,
        CoroutineKind, CoroutineSource, Expr, ExprId, Item, Label, LabelId, Literal, LoopSource,
        MatchArm, Movability, OffsetOf, Pat, PatId, RecordFieldPat, RecordLitField, RecordSpread,
        Statement, Statements, Unsafe, UnsafeBinderCastKind,
        generics::{GenericParams, LifetimeParamData},
    },
    item_scope::BuiltinShadowMode,
    lang_item::{LangItemTarget, LangItems},
    nameres::{DefMap, LocalDefMap, MacroSubNs, block_def_map},
    resolver::{HasResolver, macro_def_site},
    signatures::TypeAliasSignature,
    type_ref::{
        ArrayType, ConstRef, FnType, LifetimeRef, LifetimeRefId, Mutability, PathId, Rawness,
        RefType, TraitBoundModifier, TraitRef, TypeBound, TypeRef, TypeRefId,
    },
    unstable_features::UnstableFeatures,
};

pub use self::generics::ImplTraitLowering;
pub use self::path::hir_segment_to_ast_segment_view;

/// Which of a store's two lowerings this is: a body's ([`lower_body`]) or a signature's (the
/// `lower_*` entry points).
#[derive(Clone, Copy)]
pub(crate) enum BodyLowering<'a> {
    /// The store alone, as [`Body::of`] and each `*Signature::of` keep it: no source map, and
    /// its blocks are made here.
    Body,
    /// The store again, with its source map. A `BlockId` is a tracked struct of the query that
    /// makes it, so the blocks are not made again but taken from the first lowering's
    /// `block_scopes`, in the order they are made: the two lowerings agree on every id.
    SourceMap { blocks: &'a [BlockId] },
    /// The store with its source map, its blocks made here: one lowering for both, where
    /// no first lowering kept blocks to give (a signature its crate's image serves, whose
    /// `with_source_map` is the reference lowering from syntax; `lower_generic_params`).
    Whole,
}

pub(super) fn lower_body(
    db: &Db,
    owner: DefWithBodyId,
    syntax_node: SyntaxNodePtr,
    current_file_id: HirFileId,
    module: ModuleId,
    parameters: Option<ast::ParamList<'_>>,
    body: Option<ast::Expr<'_>>,
    is_async_fn: bool,
    is_gen_fn: bool,
    maps: Option<(&RealSpanMap, &AstIdMap)>,
    lowering: BodyLowering<'_>,
) -> (Body, BodySourceMap) {
    // We cannot leave the root span map empty and let any identifier from it be treated as root,
    // because when inside nested macros `SyntaxContextId`s from the outer macro will be interleaved
    // with the inner macro, and that will cause confusion because they won't be the same as `ROOT`
    // even though they should be the same. Also, when the body comes from multiple expansions, their
    // hygiene is different.

    let mut self_param = None;
    let mut source_map_self_param = None;
    // alloc: the body's parameter list, once per body.
    let mut params = vec![];
    let mut collector =
        ExprCollector::with_maps(db, module, current_file_id, LoweringMode::Analysis, maps);
    collector.set_lowering(lowering);
    // A block's item tree is lowered from the body's syntax only when its id is of that
    // syntax's map: lowered from the whole file, the body's blocks have the file's ids.
    collector.block_owner = maps.is_some().then_some(owner);

    let skip_body = AttrFlags::query(
        db,
        match owner {
            DefWithBodyId::FunctionId(it) => it.into(),
            DefWithBodyId::StaticId(it) => it.into(),
            DefWithBodyId::ConstId(it) => it.into(),
            DefWithBodyId::VariantId(it) => it.into(),
        },
    )
    .contains(AttrFlags::RUST_ANALYZER_SKIP);
    // If #[rust_analyzer::skip] annotated, only construct enough information for the signature
    // and skip the body.
    if skip_body {
        collector.with_expr_root(|collector| {
            if let Some(param_list) = parameters {
                if let Some(self_param_syn) =
                    param_list.self_param().filter(|&self_param| collector.check_cfg(self_param))
                {
                    let is_mutable = self_param_syn.mut_token().is_some()
                        && self_param_syn.amp_token().is_none();
                    let hygiene = self_param_syn
                        .name()
                        .map(|name| collector.hygiene_id_for(name.syntax().text_range()))
                        .unwrap_or(HygieneId::ROOT);
                    let binding_id: la_arena::Idx<Binding> = collector.alloc_binding(
                        Name::new_symbol_root(sym::self_),
                        BindingAnnotation::new(is_mutable, false),
                        hygiene,
                    );
                    self_param = Some(Param::new(binding_id));
                    source_map_self_param =
                        Some(collector.expander.in_file(SelfParamPtr::of(self_param_syn)));
                }
                let count = param_list.params().filter(|&it| collector.check_cfg(it)).count();
                // alloc: the parameter list, once per skipped body.
                params = (0..count).map(|_| Param::new(collector.missing_pat())).collect();
            };

            collector.missing_expr()
        });
        return collector.finish_body(params, self_param, source_map_self_param);
    }
    validate_required_body(db, owner, current_file_id, syntax_node, body.is_some(), &mut collector);
    collector.with_expr_root(|collector| {
        if let DefWithBodyId::FunctionId(func) = owner
            && let Some(param_list) = parameters
        {
            if let Some(self_param_syn) =
                param_list.self_param().filter(|&it| collector.check_cfg(it))
            {
                let is_mutable =
                    self_param_syn.mut_token().is_some() && self_param_syn.amp_token().is_none();
                let hygiene = self_param_syn
                    .name()
                    .map(|name| collector.hygiene_id_for(name.syntax().text_range()))
                    .unwrap_or(HygieneId::ROOT);
                let binding_id: la_arena::Idx<Binding> = collector.alloc_binding(
                    Name::new_symbol_root(sym::self_),
                    BindingAnnotation::new(is_mutable, false),
                    hygiene,
                );
                self_param = Some(Param::new(binding_id));
                source_map_self_param =
                    Some(collector.expander.in_file(SelfParamPtr::of(self_param_syn)));
            }

            let params_are_bare_idents = match func.loc(db).container {
                ItemContainerId::ExternBlockId(_) => true,
                ItemContainerId::TraitId(_) => body.is_none(),
                ItemContainerId::ModuleId(_) | ItemContainerId::ImplId(_) => false,
            };

            for param in param_list.params() {
                if collector.check_cfg(param) {
                    let param_pat = match param.pat() {
                        Some(pat) => {
                            if params_are_bare_idents {
                                collector.collect_param_as_ident(pat)
                            } else {
                                collector.collect_pat_top(Some(pat))
                            }
                        }
                        // An anonymous parameter (2015 `fn f(String) {}`) binds nothing, as
                        // rustc's `PatKind::Missing` is a wildcard to its THIR.
                        None if param.ty().is_some() => collector.store.alloc_pat(Pat::Wild),
                        None => collector.missing_pat(),
                    };
                    params.push(Param::new(param_pat));
                }
            }
        }

        collector.collect(
            &mut self_param,
            &mut params,
            body,
            if is_async_fn {
                Awaitable::Yes
            } else {
                match owner {
                    DefWithBodyId::FunctionId(..) => Awaitable::No("non-async function"),
                    DefWithBodyId::StaticId(..) => Awaitable::No("static"),
                    DefWithBodyId::ConstId(..) => Awaitable::No("constant"),
                    DefWithBodyId::VariantId(..) => Awaitable::No("enum variant"),
                }
            },
            is_async_fn,
            is_gen_fn,
        )
    });

    collector.finish_body(params, self_param, source_map_self_param)
}

fn validate_required_body(
    db: &Db,
    owner: DefWithBodyId,
    current_file_id: HirFileId,
    syntax_node: SyntaxNodePtr,
    has_body: bool,
    collector: &mut ExprCollector<'_>,
) {
    if has_body {
        return;
    }
    let diagnostic_kind = match owner {
        // FIXME: add diagnostic for missing body
        // rustc says: if body.is_none() && !is_intrinsic && !self.is_sdylib_interface
        DefWithBodyId::FunctionId(_function_id) => None,
        DefWithBodyId::StaticId(id) => match id.loc(db).container {
            ItemContainerId::ModuleId(_) => Some(MissingBodyItemKind::Static),
            ItemContainerId::ExternBlockId(_)
            | ItemContainerId::ImplId(_)
            | ItemContainerId::TraitId(_) => None,
        },
        DefWithBodyId::ConstId(id) => match id.loc(db).container {
            ItemContainerId::ModuleId(_) => Some(MissingBodyItemKind::Const),
            ItemContainerId::ImplId(_) => Some(MissingBodyItemKind::AssocConst),
            ItemContainerId::ExternBlockId(_) | ItemContainerId::TraitId(_) => None,
        },
        DefWithBodyId::VariantId(_) => None,
    };
    if let Some(kind) = diagnostic_kind {
        let node = InFile::new(current_file_id, syntax_node);
        collector.store.diagnostics.push(ExpressionStoreDiagnostics::MissingBody { node, kind });
    }
}

pub(crate) fn lower_type_ref(
    db: &Db,
    module: ModuleId,
    type_ref: InFile<Option<ast::Type<'_>>>,
    maps: Option<(&RealSpanMap, &AstIdMap)>,
    lowering: BodyLowering<'_>,
) -> (ExpressionStore, ExpressionStoreSourceMap, TypeRefId) {
    let mut expr_collector =
        ExprCollector::with_maps(db, module, type_ref.file_id, LoweringMode::Analysis, maps);
    expr_collector.set_lowering(lowering);
    let type_ref =
        expr_collector.lower_type_ref_opt(type_ref.value, &mut ImplTraitLowering::Opaque);
    let (store, source_map) = expr_collector.finish_store();
    (store, source_map, type_ref)
}

/// [`lower_generic_params_view`] of handles, for the callers outside the crate not yet on views:
/// the nodes are found as views under their own tree, so both give the same store.
pub fn lower_generic_params(
    db: &Db,
    module: ModuleId,
    def: GenericDefId,
    file_id: HirFileId,
    param_list: Option<tag::GenericParamList>,
    where_clause: Option<tag::WhereClause>,
    mode: LoweringMode,
) -> (ExpressionStore, GenericParams, ExpressionStoreSourceMap) {
    let top = param_list
        .as_ref()
        .map(top_of)
        .or_else(|| where_clause.as_ref().map(top_of));
    let param_list = param_list.zip(top.as_ref()).map(|(it, top)| view_of(&it, top));
    let where_clause = where_clause.zip(top.as_ref()).map(|(it, top)| view_of(&it, top));
    lower_generic_params_view(db, module, def, file_id, param_list, where_clause, mode)
}

pub fn lower_generic_params_view(
    db: &Db,
    module: ModuleId,
    def: GenericDefId,
    file_id: HirFileId,
    param_list: Option<ast::GenericParamList<'_>>,
    where_clause: Option<ast::WhereClause<'_>>,
    mode: LoweringMode,
) -> (ExpressionStore, GenericParams, ExpressionStoreSourceMap) {
    lower_generic_params_within(
        db,
        module,
        def,
        file_id,
        param_list,
        where_clause,
        mode,
        None,
        BodyLowering::Whole,
    )
}

/// [`lower_generic_params_view`] of a part of `file_id` lowered as a tree of its own, with `maps`
/// that part's maps ([`ExprCollector::with_maps`]).
pub(crate) fn lower_generic_params_within(
    db: &Db,
    module: ModuleId,
    def: GenericDefId,
    file_id: HirFileId,
    param_list: Option<ast::GenericParamList<'_>>,
    where_clause: Option<ast::WhereClause<'_>>,
    mode: LoweringMode,
    maps: Option<(&RealSpanMap, &AstIdMap)>,
    lowering: BodyLowering<'_>,
) -> (ExpressionStore, GenericParams, ExpressionStoreSourceMap) {
    let mut expr_collector = ExprCollector::with_maps(db, module, file_id, mode, maps);
    expr_collector.set_lowering(lowering);
    let mut collector = generics::GenericParamsCollector::new(def);
    collector.lower(&mut expr_collector, param_list, where_clause);
    let params = collector.finish();
    let (store, source_map) = expr_collector.finish_store();
    (store, params, source_map)
}

pub(crate) fn lower_impl(
    db: &Db,
    module: ModuleId,
    impl_syntax: InFile<ast::Impl<'_>>,
    impl_id: ImplId,
    maps: Option<(&RealSpanMap, &AstIdMap)>,
    lowering: BodyLowering<'_>,
) -> (ExpressionStore, ExpressionStoreSourceMap, TypeRefId, Option<TraitRef>, GenericParams) {
    let mut expr_collector =
        ExprCollector::with_maps(db, module, impl_syntax.file_id, LoweringMode::Analysis, maps);
    expr_collector.set_lowering(lowering);
    expr_collector.impl_header_lifetimes = Some(0);
    let self_ty =
        expr_collector.lower_type_ref_opt_disallow_impl_trait(impl_syntax.value.self_ty());
    let trait_ = impl_syntax.value.trait_().and_then(|it| match it {
        ast::Type::PathType(path_type) => {
            let path = expr_collector
                .lower_path_type(path_type, &mut ImplTraitLowering::Opaque)?;
            Some(TraitRef { path: expr_collector.alloc_path(path, TypePtr::of(it)) })
        }
        _ => None,
    });
    let fresh = expr_collector.impl_header_lifetimes.take().unwrap_or(0);
    let mut collector = generics::GenericParamsCollector::new(impl_id.into());
    collector.lower(
        &mut expr_collector,
        impl_syntax.value.generic_param_list(),
        impl_syntax.value.where_clause(),
    );
    collector.push_fresh_lifetimes(fresh);
    let params = collector.finish();
    let (store, source_map) = expr_collector.finish_store();
    (store, source_map, self_ty, trait_, params)
}

pub(crate) fn lower_trait(
    db: &Db,
    module: ModuleId,
    trait_syntax: InFile<ast::Trait<'_>>,
    trait_id: TraitId,
    maps: Option<(&RealSpanMap, &AstIdMap)>,
    lowering: BodyLowering<'_>,
) -> (ExpressionStore, ExpressionStoreSourceMap, GenericParams) {
    let mut expr_collector =
        ExprCollector::with_maps(db, module, trait_syntax.file_id, LoweringMode::Analysis, maps);
    expr_collector.set_lowering(lowering);
    let mut collector = generics::GenericParamsCollector::with_self_param(
        &mut expr_collector,
        trait_id.into(),
        trait_syntax.value.type_bound_list(),
    );
    collector.lower(
        &mut expr_collector,
        trait_syntax.value.generic_param_list(),
        trait_syntax.value.where_clause(),
    );
    let params = collector.finish();
    let (store, source_map) = expr_collector.finish_store();
    (store, source_map, params)
}

pub(crate) fn lower_type_alias(
    db: &Db,
    container: ItemContainerId,
    alias: InFile<ast::TypeAlias<'_>>,
    type_alias_id: TypeAliasId,
    maps: Option<(&RealSpanMap, &AstIdMap)>,
    lowering: BodyLowering<'_>,
) -> (ExpressionStore, ExpressionStoreSourceMap, GenericParams, Box<[TypeBound]>, Option<TypeRefId>)
{
    let mut expr_collector = ExprCollector::with_maps(
        db,
        container.module(db),
        alias.file_id,
        LoweringMode::Analysis,
        maps,
    );
    expr_collector.set_lowering(lowering);
    let bounds_mark = expr_collector
        .type_bounds_from_ast(alias.value.type_bound_list(), &mut ImplTraitLowering::Opaque);
    // alloc: the alias's bounds, one exact allocation per type alias, kept by its signature.
    let bounds: Box<[TypeBound]> = expr_collector.store.drain_bounds(bounds_mark).collect();
    let mut collector = generics::GenericParamsCollector::new(type_alias_id.into());
    collector.lower(
        &mut expr_collector,
        alias.value.generic_param_list(),
        alias.value.where_clause(),
    );
    let params = collector.finish();
    let type_ref = alias
        .value
        .ty()
        .map(|ty| expr_collector.lower_type_ref(ty, &mut ImplTraitLowering::Opaque));
    if alias.value.ty().is_none() {
        let diagnostic_kind = match container {
            ItemContainerId::ModuleId(_) => Some(MissingBodyItemKind::TypeAlias),
            ItemContainerId::ImplId(_) => Some(MissingBodyItemKind::AssocType),
            ItemContainerId::ExternBlockId(_) => None,
            ItemContainerId::TraitId(_) => None,
        };
        if let Some(kind) = diagnostic_kind {
            let node = InFile::new(alias.file_id, ast::ptr(alias.value.syntax()));
            expr_collector
                .store
                .diagnostics
                .push(ExpressionStoreDiagnostics::MissingBody { node, kind });
        }
    };
    let (store, source_map) = expr_collector.finish_store();
    (store, source_map, params, bounds, type_ref)
}

pub(crate) fn lower_function(
    db: &Db,
    module: ModuleId,
    fn_: InFile<ast::Fn<'_>>,
    function_id: FunctionId,
    maps: Option<(&RealSpanMap, &AstIdMap)>,
    lowering: BodyLowering<'_>,
) -> (
    ExpressionStore,
    ExpressionStoreSourceMap,
    GenericParams,
    Box<[TypeRefId]>,
    Option<TypeRefId>,
    bool,
    bool,
    Option<u8>,
) {
    let mut expr_collector =
        ExprCollector::with_maps(db, module, fn_.file_id, LoweringMode::Analysis, maps);
    expr_collector.set_lowering(lowering);
    let mut collector = generics::GenericParamsCollector::new(function_id.into());
    collector.lower(&mut expr_collector, fn_.value.generic_param_list(), fn_.value.where_clause());
    let params_mark = expr_collector.store.list_mark();
    let mut has_self_param = false;
    let mut has_variadic = false;
    let is_splat = |attr: ast::Attr<'_>| attr.simple_name().is_some_and(|it| it == "rustc_splat");
    let mut splat_at: Option<usize> = None;
    collector.collect_impl_trait(&mut expr_collector, |collector, impl_trait_lower_fn| {
        collector.with_lifetime_bound_scope(LifetimeBoundScope::Argument, |collector| {
            if let Some(param_list) = fn_.value.param_list() {
                if let Some(param) = param_list.self_param() {
                    let enabled = collector.check_cfg(param);
                    if enabled {
                        has_self_param = true;
                        if ast::HasAttrs::attrs(&param).any(is_splat) {
                            splat_at = Some(0);
                        }
                        let self_param_ty = match param.ty() {
                            Some(ty) => collector.lower_type_ref(ty, impl_trait_lower_fn),
                            None => {
                                let self_type = collector
                                    .store
                                    .push_name_path(Name::new_symbol_root(sym::Self_));
                                let self_type = collector.alloc_type_ref_path_desugared(self_type);
                                let lifetime = param
                                    .lifetime()
                                    .map(|lifetime| collector.lower_lifetime_ref(lifetime));
                                match param.kind() {
                                    ast::SelfParamKind::Owned => self_type,
                                    ast::SelfParamKind::Ref => collector.alloc_type_ref_desugared(
                                        TypeRef::Reference(&RefType {
                                            ty: self_type,
                                            lifetime,
                                            mutability: Mutability::Shared,
                                        }),
                                    ),
                                    ast::SelfParamKind::MutRef => collector
                                        .alloc_type_ref_desugared(TypeRef::Reference(&RefType {
                                            ty: self_type,
                                            lifetime,
                                            mutability: Mutability::Mut,
                                        })),
                                }
                            }
                        };
                        collector.store.push_list_type(self_param_ty);
                    }
                }
                // Every parameter's cfg is checked before any type is lowered.
                let p = param_list
                    .params()
                    .filter(|&param| collector.check_cfg(param))
                    .filter(|param| {
                        let is_variadic = param.dotdotdot_token().is_some();
                        has_variadic |= is_variadic;
                        !is_variadic
                    })
                    .enumerate()
                    .map(|(index, param)| {
                        if splat_at.is_none() && ast::HasAttrs::attrs(&param).any(is_splat) {
                            splat_at = Some(index + usize::from(has_self_param));
                        }
                        param.ty()
                    })
                    .collect::<SmallVec<[_; 8]>>();
                for p in p {
                    let ty = collector.lower_type_ref_opt(p, impl_trait_lower_fn);
                    collector.store.push_list_type(ty);
                }
            }
        })
    });
    let params: Box<[TypeRefId]> = expr_collector.store.types_since(params_mark).into();
    expr_collector.store.truncate_to(params_mark);
    let return_type = fn_.value.ret_type().map(|ret_type| {
        expr_collector.with_lifetime_bound_scope(LifetimeBoundScope::Return, |this| {
            this.lower_type_ref_opt(ret_type.ty(), &mut ImplTraitLowering::Opaque)
        })
    });
    collector.update_to_late_bound_lifetimes(&expr_collector.named_lifetime_store);
    let generics = collector.finish();

    let return_type = if fn_.value.async_token().is_some() || fn_.value.gen_token().is_some() {
        let (path, assoc_name) =
            match (fn_.value.async_token().is_some(), fn_.value.gen_token().is_some()) {
                (true, true) => {
                    (hir_expand::mod_path::path![core::async_iter::AsyncIterator], sym::Item)
                }
                (true, false) => (hir_expand::mod_path::path![core::future::Future], sym::Output),
                (false, true) => (hir_expand::mod_path::path![core::iter::Iterator], sym::Item),
                (false, false) => unreachable!(),
            };
        let type_ref =
            return_type.unwrap_or_else(|| expr_collector.alloc_type_ref_desugared(TypeRef::UNIT));
        let paths = expr_collector.store.paths_mut();
        let binding = paths.binding(Name::new_symbol_root(assoc_name), None, Some(type_ref), []);
        let args = paths.push_generic_args([], false, [binding], GenericArgsParentheses::No);
        let slots = std::iter::repeat_n(0, path.segments().len() - 1).chain([slot(Some(args))]);
        let path = paths.push_normal(path, None, slots);
        let path =
            PathId::from_type_ref_unchecked(expr_collector.alloc_type_ref_path_desugared(path));
        let ty_bound = TypeBound::Path(path, TraitBoundModifier::None);
        Some(expr_collector.alloc_type_ref_desugared(TypeRef::ImplTrait(&[ty_bound])))
    } else {
        return_type
    };
    let (store, source_map) = expr_collector.finish_store();
    (
        store,
        source_map,
        generics,
        params,
        return_type,
        has_self_param,
        has_variadic,
        if has_variadic {
            None
        } else {
            splat_at.and_then(|index| u8::try_from(index).ok()).filter(|&index| index < u8::MAX)
        },
    )
}

pub struct ExprCollector<'db> {
    db: &'db Db,
    cfg_options: &'db CfgOptions,
    expander: Expander<'db>,
    def_map: &'db DefMap,
    local_def_map: &'db LocalDefMap,
    module: ModuleId,
    lowering_mode: LoweringMode,
    lang_items: OnceCell<&'db LangItems>,
    pub store: ExpressionStoreBuilder,
    pub named_lifetime_store: NamedLifetimeStore,

    // state stuff
    // Prevent nested impl traits like `impl Foo<impl Bar>`.
    outer_impl_trait: bool,

    is_lowering_coroutine: bool,

    for_type_binder: Option<ThinVec<Name>>,

    /// While an impl's header is lowered, how many lifetimes it has left out so far (`&T`,
    /// `'_`): each is a fresh early-bound lifetime parameter of the impl, as rustc's resolver
    /// `fresh_lifetime` makes it, referred to by its name ([`LifetimeParamData::fresh_name`])
    /// as a declared one is. `None` elsewhere, and inside a fn pointer type or `Fn(..)` sugar,
    /// whose binder owns the lifetimes left out in it.
    impl_header_lifetimes: Option<u32>,

    /// Legacy (`macro_rules!`) macros can have multiple definitions and shadow each other,
    /// and we need to find the current definition. So we track the number of definitions we saw.
    current_block_legacy_macro_defs_count: HashMap<Name, usize>,

    current_try_block: Option<TryBlock>,

    awaitable_context: Option<Awaitable>,
    krate: base_db::Crate,

    name_generator_index: usize,

    stacks: Stacks,
    /// The blocks of the body's first lowering not yet taken ([`BodyLowering::SourceMap`]).
    given_blocks: Option<std::slice::Iter<'db, BlockId>>,
    /// The body being lowered, the owner of each block made ([`BlockIdLt::owner`]); `None`
    /// for a signature's store.
    block_owner: Option<DefWithBodyId>,
}

#[derive(Clone, Debug)]
struct LabelRib {
    kind: RibKind,
}

impl LabelRib {
    fn new(kind: RibKind) -> Self {
        LabelRib { kind }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RibKind {
    Normal(Name, LabelId, HygieneId),
    Closure,
    Constant,
    MacroDef(MacroDefId),
}

impl RibKind {
    /// This rib forbids referring to labels defined in upwards ribs.
    fn is_label_barrier(&self) -> bool {
        match self {
            RibKind::Normal(..) | RibKind::MacroDef(_) => false,
            RibKind::Closure | RibKind::Constant => true,
        }
    }
}

#[derive(PartialEq, Eq, Debug, Copy, Clone)]
enum Awaitable {
    Yes,
    No(&'static str),
}

enum TryBlock {
    // `try { ... }`
    Homogeneous { label: LabelId },
    // `try bikeshed Ty { ... }`
    Heterogeneous { label: LabelId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatBoundCtx {
    Product,
    Or,
}

/// The names a pattern binds, kept like rustc's resolver keeps them: a stack of product and
/// or-pattern contexts, each with the bindings made in it.
#[derive(Debug)]
struct BindingList {
    /// Inline for a pattern with at most one or-pattern level open.
    ctxs: SmallVec<[(PatBoundCtx, HashMap<(Name, HygieneId), BindingId>); 2]>,
    is_used: HashMap<BindingId, bool>,
    reject_new: bool,
}

impl Default for BindingList {
    fn default() -> Self {
        BindingList {
            ctxs: SmallVec::from_iter([(PatBoundCtx::Product, HashMap::default())]),
            is_used: HashMap::default(),
            reject_new: false,
        }
    }
}

impl BindingList {
    fn find(
        &mut self,
        ec: &mut ExprCollector<'_>,
        name: Name,
        hygiene: HygieneId,
        mode: BindingAnnotation,
    ) -> BindingId {
        let key = (name, hygiene);
        // `(a, a)` is an error, and the second `a` is a binding of its own.
        let bound_in_product = self
            .ctxs
            .iter()
            .any(|(ctx, map)| *ctx == PatBoundCtx::Product && map.contains_key(&key));
        // The alternatives of `V1(a) | V2(a)` bind the same `a`.
        let bound_in_or = self.ctxs.iter().find_map(|(ctx, map)| match ctx {
            PatBoundCtx::Or => map.get(&key).copied(),
            PatBoundCtx::Product => None,
        });
        let id = bound_in_or.unwrap_or_else(|| ec.alloc_binding(key.0, mode, key.1));
        if ec.store.bindings[id].mode != mode {
            ec.store.bindings[id].problems = Some(BindingProblems::BoundInconsistently);
        }
        self.check_is_used(ec, id);
        if bound_in_product {
            ec.store.bindings[id].problems = Some(BindingProblems::BoundMoreThanOnce);
        }
        self.ctxs.last_mut().expect("a pattern has a binding context").1.insert(key, id);
        id
    }

    fn push(&mut self, ctx: PatBoundCtx) {
        self.ctxs.push((ctx, HashMap::default()));
    }

    /// Ends the innermost context, moving its bindings to the one around it.
    fn pop(&mut self) {
        let (_, bound) = self.ctxs.pop().expect("a pattern has a binding context");
        self.ctxs.last_mut().expect("a pattern has a binding context").1.extend(bound);
    }

    fn check_is_used(&mut self, ec: &mut ExprCollector<'_>, id: BindingId) {
        match self.is_used.get(&id) {
            None => {
                if self.reject_new {
                    ec.store.bindings[id].problems = Some(BindingProblems::NotBoundAcrossAll);
                }
            }
            Some(true) => {
                ec.store.bindings[id].problems = Some(BindingProblems::BoundMoreThanOnce);
            }
            Some(false) => {}
        }
        self.is_used.insert(id, true);
    }
}

/// A collector's stacks, kept per thread between collectors as the builder's scratch is, so a
/// collector made to lower one path (an IDE lookup) allocates nothing once they are warm.
#[derive(Default)]
struct Stacks {
    label_ribs: Vec<LabelRib>,
    unowned_bindings: Vec<BindingId>,
    // The lists of a path, its generic arguments and a `FnType` while they are built, like the
    // builder's node lists: a nested one pushes and pops above, and the finished list is moved
    // into the store's path columns (a `FnType`'s out in one exact allocation).
    path_segments: Vec<Name>,
    /// The slot word of each segment ([`slot`]).
    path_generic_args: Vec<u32>,
    generic_args: Vec<GenericArg>,
    assoc_bindings: Vec<BindingRow>,
    fn_params: Vec<(Option<Name>, TypeRefId)>,
    /// A coroutine closure's parameters while its body is desugared.
    closure_params: Vec<Param<PatId>>,
}

/// The stack sets a thread keeps, and the elements above which a set is dropped instead of
/// kept, so that one huge body doesn't pin its memory.
const STACK_SETS: usize = 4;
const STACK_ELEMENTS: usize = 1 << 16;

thread_local! {
    /// This thread's spare stacks: a collector can run a query that lowers another store on
    /// the same thread, which takes the next set (or a fresh one).
    // alloc: const-empty, once per thread.
    static STACKS: RefCell<Vec<Stacks>> = const { RefCell::new(Vec::new()) };
}

impl Stacks {
    fn take() -> Stacks {
        STACKS.try_with(|pool| pool.borrow_mut().pop()).ok().flatten().unwrap_or_default()
    }

    fn capacity(&self) -> usize {
        self.label_ribs.capacity()
            + self.unowned_bindings.capacity()
            + self.path_segments.capacity()
            + self.path_generic_args.capacity()
            + self.generic_args.capacity()
            + self.assoc_bindings.capacity()
            + self.fn_params.capacity()
            + self.closure_params.capacity()
    }

    fn clear(&mut self) {
        let Stacks {
            label_ribs,
            unowned_bindings,
            path_segments,
            path_generic_args,
            generic_args,
            assoc_bindings,
            fn_params,
            closure_params,
        } = self;
        label_ribs.clear();
        unowned_bindings.clear();
        path_segments.clear();
        path_generic_args.clear();
        generic_args.clear();
        assoc_bindings.clear();
        fn_params.clear();
        closure_params.clear();
    }
}

impl Drop for Stacks {
    /// Empties the set and keeps it for the next collector on this thread, unless it never grew,
    /// is over [`STACK_ELEMENTS`], the pool is full or the thread is exiting. The set is moved
    /// into the pool only from inside the borrow and nothing is dropped there, so this never
    /// runs again under it.
    fn drop(&mut self) {
        let held = self.capacity();
        if held == 0 || held > STACK_ELEMENTS {
            return;
        }
        self.clear();
        let _exiting = STACKS.try_with(|pool| {
            let mut pool = pool.borrow_mut();
            if pool.len() < STACK_SETS {
                pool.push(mem::take(self));
            }
        });
    }
}

#[derive(Debug, Default)]
pub struct NamedLifetimeStore {
    lifetime_bound_scope: Option<LifetimeBoundScope>,
    lifetimes_in_where_clause: FxIndexSet<Name>,
    lifetimes_constrained_by_input: FxIndexSet<Name>,
    lifetimes_in_output: FxIndexSet<Name>,
}

#[derive(Debug)]
enum LifetimeBoundScope {
    Argument,
    Return,
    WhereClause,
    ImplTrait { is_argument_scope: bool },
}

impl NamedLifetimeStore {
    /// Adds in the lifetime one of three lists fields if
    /// `lifetime_bound_context` is `Some` and based on enum variant.
    pub(crate) fn push_named_lifetime(&mut self, lifetime: Name) {
        match self.lifetime_bound_scope {
            Some(LifetimeBoundScope::Argument) => {
                self.lifetimes_constrained_by_input.insert(lifetime);
            }
            Some(LifetimeBoundScope::Return) => {
                self.lifetimes_in_output.insert(lifetime);
            }
            Some(LifetimeBoundScope::WhereClause) => {
                self.lifetimes_in_where_clause.insert(lifetime);
            }
            Some(LifetimeBoundScope::ImplTrait { is_argument_scope }) => {
                if is_argument_scope {
                    self.lifetimes_in_where_clause.insert(lifetime);
                } else {
                    self.lifetimes_in_output.insert(lifetime);
                }
            }
            None => (),
        };
    }
}

impl<'db> ExprCollector<'db> {
    pub fn new(
        db: &'db Db,
        module: ModuleId,
        current_file_id: HirFileId,
        lowering_mode: LoweringMode,
    ) -> ExprCollector<'db> {
        Self::with_maps(db, module, current_file_id, lowering_mode, None)
    }

    /// With `maps`, what is collected is a part of `current_file_id` lowered as a tree of its
    /// own, and these are the maps of that part ([`Expander::within`]).
    pub fn with_maps(
        db: &'db Db,
        module: ModuleId,
        current_file_id: HirFileId,
        lowering_mode: LoweringMode,
        maps: Option<(&'db RealSpanMap, &'db AstIdMap)>,
    ) -> ExprCollector<'db> {
        let (def_map, local_def_map) = module.local_def_map(db);
        let expander = match maps {
            Some((span_map, ast_id_map)) => {
                Expander::within(db, current_file_id, def_map, span_map, ast_id_map)
            }
            None => Expander::new(db, current_file_id, def_map),
        };
        let krate = module.krate(db);
        let mut result = ExprCollector {
            db,
            cfg_options: krate.cfg_options(db),
            module,
            lowering_mode,
            def_map,
            local_def_map,
            lang_items: OnceCell::new(),
            store: ExpressionStoreBuilder::new(),
            expander,
            current_try_block: None,
            is_lowering_coroutine: false,
            awaitable_context: None,
            current_block_legacy_macro_defs_count: HashMap::default(),
            outer_impl_trait: false,
            krate,
            name_generator_index: 0,
            named_lifetime_store: NamedLifetimeStore::default(),
            for_type_binder: None,
            impl_header_lifetimes: None,
            stacks: Stacks::take(),
            given_blocks: None,
            block_owner: None,
        };
        result.store.inference_roots = Some(SmallVec::new());
        result
    }

    /// Lowers as `lowering` says: without the source map for [`BodyLowering::Body`], with it
    /// and the first lowering's blocks for [`BodyLowering::SourceMap`], with it and blocks
    /// made here for [`BodyLowering::Whole`] (what `with_maps` sets up).
    pub(crate) fn set_lowering(&mut self, lowering: BodyLowering<'db>) {
        match lowering {
            BodyLowering::Body => self.store.skip_source_map(),
            BodyLowering::SourceMap { blocks } => self.given_blocks = Some(blocks.iter()),
            BodyLowering::Whole => {}
        }
    }

    /// The store and its source map, checking that a lowering given blocks made all of them.
    pub(crate) fn finish_store(self) -> (ExpressionStore, ExpressionStoreSourceMap) {
        if let Some(rest) = &self.given_blocks {
            assert_eq!(rest.len(), 0, "the store made fewer blocks than its first lowering");
        }
        self.store.finish()
    }

    /// The lowered body, and its source map when it was recorded.
    fn finish_body(
        self,
        params: Vec<Param<PatId>>,
        self_param: Option<Param<BindingId>>,
        self_param_source: Option<InFile<SelfParamPtr>>,
    ) -> (Body, BodySourceMap) {
        if let Some(rest) = &self.given_blocks {
            assert_eq!(rest.len(), 0, "the body made fewer blocks than its first lowering");
        }
        let record = self.store.record;
        let (store, store_map) = self.store.finish();
        (
            Body { store, params: params.into_boxed_slice(), self_param },
            BodySourceMap { self_param: self_param_source.filter(|_| record), store: store_map },
        )
    }

    fn generate_new_name(&mut self) -> Name {
        let index = self.name_generator_index;
        self.name_generator_index += 1;
        Name::generate_new_name(index)
    }

    #[inline]
    pub(crate) fn lang_items(&self) -> &'db LangItems {
        self.lang_items.get_or_init(|| crate::lang_item::lang_items(self.db, self.def_map.krate()))
    }

    #[inline]
    pub(crate) fn span_map(&self) -> SpanMap<'_> {
        self.expander.span_map()
    }

    pub(in crate::expr_store) fn lower_lifetime_ref(
        &mut self,
        lifetime: ast::Lifetime<'_>,
    ) -> LifetimeRefId {
        // FIXME: Keyword check?
        let lifetime_ref = match lifetime.text() {
            "" | "'" => LifetimeRef::Error,
            "'static" => LifetimeRef::Static,
            "'_" => self.fresh_impl_header_lifetime().unwrap_or(LifetimeRef::Placeholder),
            text => LifetimeRef::Named(Name::new_lifetime(text)),
        };
        self.alloc_lifetime_ref(lifetime_ref, LifetimePtr::of(lifetime))
    }

    /// The next fresh lifetime parameter of the impl whose header is being lowered
    /// ([`Self::impl_header_lifetimes`]), `None` outside one.
    fn fresh_impl_header_lifetime(&mut self) -> Option<LifetimeRef> {
        let fresh = self.impl_header_lifetimes.as_mut()?;
        let name = LifetimeParamData::fresh_name(*fresh);
        *fresh += 1;
        Some(LifetimeRef::Named(name))
    }

    /// `f` with no fresh impl-header lifetimes: inside a fn pointer type or `Fn(..)` sugar, the
    /// lifetimes left out are its binder's.
    fn without_impl_header_lifetimes<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let header = self.impl_header_lifetimes.take();
        let result = f(self);
        self.impl_header_lifetimes = header;
        result
    }

    pub(in crate::expr_store) fn lower_lifetime_ref_opt(
        &mut self,
        lifetime: Option<ast::Lifetime<'_>>,
    ) -> LifetimeRefId {
        match lifetime {
            Some(lifetime) => self.lower_lifetime_ref(lifetime),
            None => self.alloc_lifetime_ref_desugared(LifetimeRef::Placeholder),
        }
    }

    /// Converts an `ast::TypeRef` to a `hir::TypeRef`.
    pub(in crate::expr_store) fn lower_type_ref(
        &mut self,
        node: ast::Type<'_>,
        impl_trait_lower_fn: &mut ImplTraitLowering<'_>,
    ) -> TypeRefId {
        let ptr = TypePtr::of(node);
        match node {
            ast::Type::ParenType(inner) => self.lower_type_ref_opt(inner.ty(), impl_trait_lower_fn),
            ast::Type::TupleType(inner) => {
                let mark = self.store.list_mark();
                for field in inner.fields() {
                    let ty = self.lower_type_ref(field, impl_trait_lower_fn);
                    self.store.push_list_type(ty);
                }
                let id = self.with_lists(mark, |this, lists| {
                    this.store.alloc_type_ref(TypeRef::Tuple(lists.types_since(mark)))
                });
                self.map_type_ref(id, ptr, false)
            }
            ast::Type::NeverType(..) => self.alloc_type_ref(TypeRef::Never, ptr),
            ast::Type::PathType(inner) => {
                match inner.path().and_then(|it| self.lower_path_view(it, impl_trait_lower_fn)) {
                    Some(path) => self.alloc_type_ref_path(path, ptr),
                    None => self.alloc_type_ref(TypeRef::Error, ptr),
                }
            }
            ast::Type::PtrType(inner) => {
                let inner_ty = self.lower_type_ref_opt(inner.ty(), impl_trait_lower_fn);
                let mutability = Mutability::from_mutable(inner.mut_token().is_some());
                self.alloc_type_ref(TypeRef::RawPtr(inner_ty, mutability), ptr)
            }
            ast::Type::ArrayType(inner) => {
                let len = self.lower_const_arg_opt(inner.const_arg());
                let ty = self.lower_type_ref_opt(inner.ty(), impl_trait_lower_fn);
                self.alloc_type_ref(TypeRef::Array(&ArrayType { ty, len }), ptr)
            }
            ast::Type::SliceType(inner) => {
                let inner_ty = self.lower_type_ref_opt(inner.ty(), impl_trait_lower_fn);
                self.alloc_type_ref(TypeRef::Slice(inner_ty), ptr)
            }
            ast::Type::RefType(inner) => {
                let lifetime = match inner.lifetime() {
                    Some(lt) => Some(self.lower_lifetime_ref(lt)),
                    None => self
                        .fresh_impl_header_lifetime()
                        .map(|it| self.alloc_lifetime_ref_desugared(it)),
                };
                let inner_ty = self.lower_type_ref_opt(inner.ty(), impl_trait_lower_fn);
                let mutability = Mutability::from_mutable(inner.mut_token().is_some());
                self.alloc_type_ref(
                    TypeRef::Reference(&RefType { ty: inner_ty, lifetime, mutability }),
                    ptr,
                )
            }
            ast::Type::InferType(_inner) => self.alloc_type_ref(TypeRef::Placeholder, ptr),
            ast::Type::FnPtrType(inner) => {
                // Taken before the parameters are lowered: a function pointer among them
                // is not what the `for` is written on.
                let binder = self.for_type_binder.take().map(|b| b.into());
                // The lifetimes left out in it are its own binder's.
                let impl_header_lifetimes = self.impl_header_lifetimes.take();
                let ret_ty = inner
                    .ret_type()
                    .and_then(|rt| rt.ty())
                    .map(|it| self.lower_type_ref(it, impl_trait_lower_fn))
                    .unwrap_or_else(|| self.alloc_type_ref_desugared(TypeRef::UNIT));
                let mut is_varargs = false;
                let params_base = self.stacks.fn_params.len();
                if let Some(pl) = inner.param_list() {
                    if let Some(param) = pl.params().last() {
                        is_varargs = param.dotdotdot_token().is_some();
                    }

                    for it in pl.params().filter(|it| it.dotdotdot_token().is_none()) {
                        let type_ref = self.lower_type_ref_opt(it.ty(), impl_trait_lower_fn);
                        let name = match it.pat() {
                            Some(ast::Pat::IdentPat(it)) => Some(
                                it.name().map(|nr| nr.as_name()).unwrap_or_else(Name::missing),
                            ),
                            _ => None,
                        };
                        self.stacks.fn_params.push((name, type_ref));
                    }
                }
                fn lower_abi(abi: ast::Abi<'_>) -> ExternAbi {
                    abi.abi_string()
                        .and_then(|abi| abi.text_without_quotes().parse().ok())
                        .unwrap_or(ExternAbi::FALLBACK)
                }

                let abi = inner.abi().map(lower_abi).unwrap_or(ExternAbi::Rust);
                self.stacks.fn_params.push((None, ret_ty));

                let fn_ = FnType {
                    is_varargs,
                    is_unsafe: inner.unsafe_token().is_some(),
                    abi,
                    // alloc: the fn type's own stored list, one exact allocation per `fn` type.
                    params: self.stacks.fn_params.drain(params_base..).collect(),
                    binder,
                };
                self.impl_header_lifetimes = impl_header_lifetimes;
                let id = self.store.alloc_fn_type(fn_);
                self.map_type_ref(id, ptr, false)
            }
            // for types are close enough for our purposes to the inner type for now...
            ast::Type::ForType(inner) => {
                let binder = self.lower_for_binder_opt(inner.for_binder());
                let old_for_binder = self.for_type_binder.replace(binder);
                let ty = self.lower_type_ref_opt(inner.ty(), impl_trait_lower_fn);
                self.for_type_binder = old_for_binder;
                ty
            }
            ast::Type::ImplTraitType(inner) => {
                if self.outer_impl_trait {
                    // Disallow nested impl traits
                    self.alloc_type_ref(TypeRef::Error, ptr)
                } else {
                    self.with_outer_impl_trait_scope(true, |this| {
                        let is_argument_scope = this.is_argument_lt_bound_scope();
                        let type_bounds = this.with_lifetime_bound_scope(
                            LifetimeBoundScope::ImplTrait { is_argument_scope },
                            |this| {
                                this.type_bounds_from_ast(
                                    inner.type_bound_list(),
                                    impl_trait_lower_fn,
                                )
                            },
                        );
                        impl_trait_lower_fn.lower(this, ptr, type_bounds)
                    })
                }
            }
            ast::Type::DynTraitType(inner) => {
                let mark = self.type_bounds_from_ast(inner.type_bound_list(), impl_trait_lower_fn);
                let id = self.with_lists(mark, |this, lists| {
                    this.store.alloc_type_ref(TypeRef::DynTrait(lists.bounds_since(mark)))
                });
                self.map_type_ref(id, ptr, false)
            }
            ast::Type::PatternType(inner) => {
                let ty = self.lower_type_ref_opt(inner.ty(), impl_trait_lower_fn);
                let pat = self.collect_ty_pat_opt(inner.pat());
                self.alloc_type_ref(TypeRef::PatternType(ty, pat), ptr)
            }
            ast::Type::UnsafeBinderType(inner) => {
                let lifetimes = match inner.generic_param_list() {
                    Some(params) => self.store.paths_mut().push_names(
                        params
                            .lifetime_params()
                            .flat_map(|lp| lp.lifetime().map(|lt| Name::new_lifetime(lt.text()))),
                    ),
                    None => Names::EMPTY,
                };
                let ty = self.lower_type_ref_opt(inner.ty(), impl_trait_lower_fn);
                self.alloc_type_ref(TypeRef::UnsafeBinder { lifetimes, ty }, ptr)
            }
            ast::Type::MacroType(mt) => match mt.macro_call() {
                Some(mcall) => {
                    let macro_ptr = AstPtr::<tag::MacroCall>::of(mcall);
                    let src = self.expander.in_file(ptr);
                    let id = self.collect_macro_call(mcall, macro_ptr, true, |this, expansion| {
                        this.lower_type_ref_opt(expansion, impl_trait_lower_fn)
                    });
                    if self.store.record {
                        self.store.types_map.push((src, id));
                    }
                    id
                }
                None => self.alloc_type_ref(TypeRef::Error, ptr),
            },
        }
    }

    pub(crate) fn lower_type_ref_disallow_impl_trait(&mut self, node: ast::Type<'_>) -> TypeRefId {
        self.lower_type_ref(node, &mut ImplTraitLowering::Error)
    }

    pub(crate) fn lower_type_ref_opt(
        &mut self,
        node: Option<ast::Type<'_>>,
        impl_trait_lower_fn: &mut ImplTraitLowering<'_>,
    ) -> TypeRefId {
        match node {
            Some(node) => self.lower_type_ref(node, impl_trait_lower_fn),
            None => self.alloc_error_type(),
        }
    }

    pub(crate) fn lower_type_ref_opt_disallow_impl_trait(
        &mut self,
        node: Option<ast::Type<'_>>,
    ) -> TypeRefId {
        self.lower_type_ref_opt(node, &mut ImplTraitLowering::Error)
    }

    fn alloc_type_ref(&mut self, type_ref: TypeRef<'_>, node: TypePtr) -> TypeRefId {
        let plain = matches!(type_ref, TypeRef::Path(path) if is_plain_path(path));
        let id = self.store.alloc_type_ref(type_ref);
        self.map_type_ref(id, node, plain)
    }

    fn alloc_type_ref_path(&mut self, path: PathIdx, node: TypePtr) -> TypeRefId {
        let plain = is_plain_path(self.store.path_at(path));
        let id = self.store.alloc_path_type(path);
        self.map_type_ref(id, node, plain)
    }

    /// The source maps (and, for a plain path, the hygiene) of a type ref just allocated.
    fn map_type_ref(&mut self, id: TypeRefId, node: TypePtr, plain: bool) -> TypeRefId {
        if plain {
            let hygiene = self.hygiene_id_for(node.text_range());
            if !hygiene.is_root() {
                self.store.type_hygiene.push((id, hygiene));
            }
        }
        if self.store.record {
            let ptr = self.expander.in_file(node);
            self.store.types_map_back.insert(id, ptr);
            self.store.types_map.push((ptr, id));
        }
        id
    }

    fn alloc_lifetime_ref(
        &mut self,
        lifetime_ref: LifetimeRef,
        node: LifetimePtr,
    ) -> LifetimeRefId {
        if let LifetimeRef::Named(name) = &lifetime_ref {
            self.named_lifetime_store.push_named_lifetime(*name);
        }

        let id = self.store.lifetimes.alloc(lifetime_ref);
        if self.store.record {
            let ptr = self.expander.in_file(node);
            self.store.lifetime_map_back.insert(id, ptr);
            self.store.lifetime_map.push((ptr, id));
        }
        id
    }

    fn alloc_type_ref_desugared(&mut self, type_ref: TypeRef<'_>) -> TypeRefId {
        self.store.alloc_type_ref(type_ref)
    }

    fn alloc_type_ref_path_desugared(&mut self, path: PathIdx) -> TypeRefId {
        self.store.alloc_path_type(path)
    }

    fn alloc_lifetime_ref_desugared(&mut self, lifetime_ref: LifetimeRef) -> LifetimeRefId {
        self.store.lifetimes.alloc(lifetime_ref)
    }

    fn alloc_error_type(&mut self) -> TypeRefId {
        self.store.alloc_type_ref(TypeRef::Error)
    }

    /// [`Self::lower_path_view`] of a handle, for the callers outside the crate not yet on views:
    /// the path is found as a view under its own tree, so both give the same path.
    pub fn lower_path(
        &mut self,
        path: tag::Path,
        impl_trait_lower_fn: &mut ImplTraitLowering<'_>,
    ) -> Option<PathIdx> {
        let top = top_of(&path);
        self.lower_path_view(view_of(&path, &top), impl_trait_lower_fn)
    }

    pub fn lower_path_view(
        &mut self,
        path: ast::Path<'_>,
        impl_trait_lower_fn: &mut ImplTraitLowering<'_>,
    ) -> Option<PathIdx> {
        super::lower::path::lower_path(self, path, impl_trait_lower_fn)
    }

    fn with_outer_impl_trait_scope<R>(
        &mut self,
        impl_trait: bool,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let old = mem::replace(&mut self.outer_impl_trait, impl_trait);
        let result = f(self);
        self.outer_impl_trait = old;
        result
    }

    fn alloc_path(&mut self, path: PathIdx, node: TypePtr) -> PathId {
        PathId::from_type_ref_unchecked(self.alloc_type_ref_path(path, node))
    }

    /// Lowers a desugared coroutine body after moving all of the arguments
    /// into the body. This is to make sure that the future actually owns the
    /// arguments that are passed to the function, and to ensure things like
    /// drop order are stable.
    fn lower_coroutine_body_with_moved_arguments(
        &mut self,
        self_param: &mut Option<Param<BindingId>>,
        params: &mut [Param<PatId>],
        body: ExprId,
        kind: CoroutineKind,
        coroutine_source: CoroutineSource,
    ) -> ExprId {
        // Async function parameters are lowered into the closure body so that they are
        // captured and so that the drop order matches the equivalent non-async functions.
        //
        // from:
        //
        //     async fn foo(<pattern>: <ty>, <pattern>: <ty>, <pattern>: <ty>) {
        //         <body>
        //     }
        //
        // into:
        //
        //     fn foo(__arg0: <ty>, __arg1: <ty>, __arg2: <ty>) {
        //       async move {
        //         let __arg2 = __arg2;
        //         let <pattern> = __arg2;
        //         let __arg1 = __arg1;
        //         let <pattern> = __arg1;
        //         let __arg0 = __arg0;
        //         let <pattern> = __arg0;
        //         drop-temps { <body> } // see comments later in fn for details
        //       }
        //     }
        //
        // If `<pattern>` is a simple ident, then it is lowered to a single
        // `let <pattern> = <pattern>;` statement as an optimization.

        let statements = self.store.list_mark();

        if let Some(self_param) = self_param {
            let Binding { name, mode, hygiene, .. } = self.store.bindings[self_param.formal];
            let child_binding_id = self.alloc_binding(name, mode, hygiene);
            let child_pat_id =
                self.alloc_pat_desugared(Pat::Bind { id: child_binding_id, subpat: None });
            self.add_definition_to_binding(child_binding_id, child_pat_id);
            let expr = self.store.alloc_name_path_expr(name);
            if !hygiene.is_root() {
                self.store.ident_hygiene.insert(expr.into(), hygiene);
            }
            self.store.push_list_statement(Statement::Let {
                pat: child_pat_id,
                type_ref: None,
                initializer: Some(expr),
                else_branch: None,
                is_super: false,
            });
            self_param.user_written = child_binding_id;
        }

        for param in params {
            let (name, hygiene, is_simple_parameter) = match self.store.pat(param.formal) {
                // Check if this is a binding pattern, if so, we can optimize and avoid adding a
                // `let <pat> = __argN;` statement. In this case, we do not rename the parameter.
                Pat::Bind { id, subpat: None, .. }
                    if matches!(
                        self.store.bindings[id].mode,
                        BindingAnnotation::Unannotated | BindingAnnotation::Mutable
                    ) =>
                {
                    let binding = &self.store.bindings[id];
                    (binding.name, binding.hygiene, true)
                }
                Pat::Bind { id, .. } => {
                    // If this is a `ref` binding, we can't leave it as is but we can at least reuse the name, for better display.
                    let binding = &self.store.bindings[id];
                    (binding.name, binding.hygiene, false)
                }
                _ => (self.generate_new_name(), HygieneId::ROOT, false),
            };
            let pat_syntax = self.store.pat_map_back.get(param.formal).copied();
            if !is_simple_parameter {
                // It needs to be mutable so the inner patterns can borrow it mutably (`ref mut`).
                let child_binding_id =
                    self.alloc_binding(name, BindingAnnotation::Mutable, hygiene);
                let child_pat_id =
                    self.alloc_pat_desugared(Pat::Bind { id: child_binding_id, subpat: None });
                self.add_definition_to_binding(child_binding_id, child_pat_id);
                if let Some(pat_syntax) = pat_syntax {
                    self.store.pat_map_back.insert(child_pat_id, pat_syntax);
                }
                let expr = self.store.alloc_name_path_expr(name);
                if !hygiene.is_root() {
                    self.store.ident_hygiene.insert(expr.into(), hygiene);
                }
                self.store.push_list_statement(Statement::Let {
                    pat: child_pat_id,
                    type_ref: None,
                    initializer: Some(expr),
                    else_branch: None,
                    is_super: false,
                });
            }
            let expr = self.store.alloc_name_path_expr(name);
            if !hygiene.is_root() {
                self.store.ident_hygiene.insert(expr.into(), hygiene);
            }
            self.store.push_list_statement(Statement::Let {
                pat: param.formal,
                type_ref: None,
                initializer: Some(expr),
                else_branch: None,
                is_super: false,
            });

            let parent_binding_id = self.alloc_binding(name, BindingAnnotation::Mutable, hygiene);
            let parent_pat_id =
                self.alloc_pat_desugared(Pat::Bind { id: parent_binding_id, subpat: None });
            self.add_definition_to_binding(parent_binding_id, parent_pat_id);
            if let Some(pat_syntax) = pat_syntax {
                self.store.pat_map_back.insert(parent_pat_id, pat_syntax);
            }
            *param = Param { formal: parent_pat_id, user_written: param.formal };
        }

        self.with_lists(statements, |this, lists| {
            let coroutine = this.desugared_coroutine_expr(
                kind,
                coroutine_source,
                // The default capture mode here is by-ref. Later on during upvar analysis,
                // we will force the captured arguments to by-move, but for async closures,
                // we want to make sure that we avoid unnecessarily moving captures, or else
                // all async closures would default to `FnOnce` as their calling mode.
                CaptureBy::Ref,
                None,
                lists.statements_since(statements),
                Some(body),
            );
            // It's important that this comes last, see the lowering of async closures for why.
            this.alloc_expr_desugared(coroutine)
        })
    }

    /// Allocates the coroutine's block and returns the closure around it, for the caller to
    /// allocate.
    fn desugared_coroutine_expr(
        &mut self,
        kind: CoroutineKind,
        source: CoroutineSource,
        capture_by: CaptureBy,
        id: Option<BlockId>,
        statements: Statements<'_>,
        tail: Option<ExprId>,
    ) -> Expr<'static> {
        let block = self.alloc_expr_desugared(Expr::Block {
            label: None,
            id,
            statements,
            tail,
            unsafe_: Unsafe::No,
        });
        Expr::Closure {
            args: &[],
            arg_types: &[],
            ret_type: None,
            body: block,
            closure_kind: ClosureKind::Coroutine { kind, source },
            capture_by,
        }
    }

    fn collect(
        &mut self,
        self_param: &mut Option<Param<BindingId>>,
        params: &mut [Param<PatId>],
        expr: Option<ast::Expr<'_>>,
        awaitable: Awaitable,
        is_async_fn: bool,
        is_gen_fn: bool,
    ) -> ExprId {
        self.awaitable_context.replace(awaitable);
        self.with_label_rib(RibKind::Closure, |this| {
            let body = this.collect_expr_opt(expr);
            if is_async_fn || is_gen_fn {
                let kind = match (is_async_fn, is_gen_fn) {
                    (true, true) => CoroutineKind::AsyncGen,
                    (true, false) => CoroutineKind::Async,
                    (false, true) => CoroutineKind::Gen,
                    (false, false) => unreachable!(),
                };
                let coroutine = this.lower_coroutine_body_with_moved_arguments(
                    self_param,
                    params,
                    body,
                    kind,
                    CoroutineSource::Fn,
                );
                // *All* locals belong to the inner coroutine...
                this.associate_unowned_bindings_with(0, coroutine);
                // ...except the formal parameters, which are *not* necessarily what was passed as parameters into `collect()`,
                // since `lower_coroutine_body_with_moved_arguments()` might have changed them.
                // They are owned by the top-level function, so should not be present in `bindings_owner`.
                for param in params.iter() {
                    let Pat::Bind { id, .. } = this.store.pat(param.formal) else {
                        unreachable!(
                            "`lower_coroutine_body_with_moved_arguments()` makes each formal \
                            parameter of a coroutine closure a simple binding"
                        );
                    };
                    this.store.binding_owners.remove(&id);
                }
                if let Some(param) = self_param {
                    this.store.binding_owners.remove(&param.formal);
                }
                coroutine
            } else {
                body
            }
        })
    }

    fn type_bounds_from_ast(
        &mut self,
        type_bounds_opt: Option<ast::TypeBoundList<'_>>,
        impl_trait_lower_fn: &mut ImplTraitLowering<'_>,
    ) -> ListMark {
        let mark = self.store.list_mark();
        for bound in type_bounds_opt.iter().flat_map(|it| it.bounds()) {
            let bound = self.lower_type_bound(bound, impl_trait_lower_fn);
            self.store.push_list_bound(bound);
        }
        mark
    }

    fn lower_path_type(
        &mut self,
        path_type: ast::PathType<'_>,
        impl_trait_lower_fn: &mut ImplTraitLowering<'_>,
    ) -> Option<PathIdx> {
        let path = self.lower_path_view(path_type.path()?, impl_trait_lower_fn)?;
        Some(path)
    }

    fn lower_for_binder_opt(&mut self, binder: Option<ForBinder<'_>>) -> ThinVec<Name> {
        binder.map(|b| self.lower_for_binder(b)).unwrap_or_default()
    }

    fn lower_for_binder(&mut self, binder: ForBinder<'_>) -> ThinVec<Name> {
        match binder.generic_param_list() {
            Some(gpl) => gpl
                .lifetime_params()
                .flat_map(|lp| lp.lifetime().map(|lt| Name::new_lifetime(lt.text())))
                // alloc: a `for<..>` type's binder, once per one written, kept by `FnType.binder`.
                .collect(),
            None => ThinVec::default(),
        }
    }

    fn lower_const_arg_opt(&mut self, arg: Option<ast::ConstArg<'_>>) -> ConstRef {
        ConstRef {
            expr: self.with_fresh_binding_expr_root(|this| {
                this.collect_expr_opt(arg.and_then(|arg| arg.expr()))
            }),
        }
    }

    pub fn lower_const_arg(&mut self, arg: ast::ConstArg<'_>) -> ConstRef {
        ConstRef {
            expr: self.with_fresh_binding_expr_root(|this| this.collect_expr_opt(arg.expr())),
        }
    }

    fn collect_expr(&mut self, expr: ast::Expr<'_>) -> ExprId {
        self.maybe_collect_expr(expr).unwrap_or_else(|| self.missing_expr())
    }

    pub(in crate::expr_store) fn collect_expr_opt(
        &mut self,
        expr: Option<ast::Expr<'_>>,
    ) -> ExprId {
        match expr {
            Some(expr) => self.collect_expr(expr),
            None => self.missing_expr(),
        }
    }

    /// Returns `None` if and only if the expression is `#[cfg]`d out.
    fn maybe_collect_expr(&mut self, expr: ast::Expr<'_>) -> Option<ExprId> {
        // An expression is lowered within the one it is in: rustc's 5,204-arm `else if`.
        stdx::ensure_sufficient_stack(|| self.maybe_collect_expr_here(expr))
    }

    fn maybe_collect_expr_here(&mut self, expr: ast::Expr<'_>) -> Option<ExprId> {
        if !self.check_cfg(expr) {
            return None;
        }
        let syntax_ptr = ExprPtr::of(expr);

        // FIXME: Move some of these arms out into separate methods for clarity
        Some(match expr {
            ast::Expr::IfExpr(e) => {
                let then_branch = self.collect_block_opt(e.then_branch());

                let else_branch = e.else_branch().map(|b| match b {
                    ast::ElseBranch::Block(it) => self.collect_block(it),
                    ast::ElseBranch::IfExpr(elif) => self.collect_expr(ast::Expr::IfExpr(elif)),
                });

                let condition = self.collect_expr_opt(e.condition());

                self.alloc_expr(Expr::If { condition, then_branch, else_branch }, syntax_ptr)
            }
            ast::Expr::LetExpr(e) => {
                let pat = self.collect_pat_top(e.pat());
                let expr = self.collect_expr_opt(e.expr());
                self.alloc_expr(Expr::Let { pat, expr }, syntax_ptr)
            }
            ast::Expr::BlockExpr(e) => match e.modifier() {
                Some(ast::BlockModifier::Try { try_token: _, bikeshed_token: _, result_type }) => {
                    self.desugar_try_block(e, result_type)
                }
                Some(ast::BlockModifier::Unsafe(_)) => {
                    self.collect_block_(e, |_, id, statements, tail| Expr::Block {
                        id,
                        statements,
                        tail,
                        label: None,
                        unsafe_: Unsafe::Yes,
                    })
                }
                Some(ast::BlockModifier::Label(label)) => {
                    let label_hygiene = self.hygiene_id_for(label.syntax().text_range());
                    let label_id = self.collect_label(label);
                    self.with_labeled_rib(label_id, label_hygiene, |this| {
                        this.collect_block_(e, |_, id, statements, tail| Expr::Block {
                            id,
                            statements,
                            tail,
                            label: Some(label_id),
                            unsafe_: Unsafe::No,
                        })
                    })
                }
                Some(ast::BlockModifier::Async(_)) => {
                    let capture_by =
                        if e.move_token().is_some() { CaptureBy::Value } else { CaptureBy::Ref };
                    self.with_label_rib(RibKind::Closure, |this| {
                        this.with_binding_owner(|this| {
                            this.with_awaitable_block(Awaitable::Yes, |this| {
                                this.collect_block_(e, |this, id, statements, tail| {
                                    this.desugared_coroutine_expr(
                                        CoroutineKind::Async,
                                        CoroutineSource::Block,
                                        capture_by,
                                        id,
                                        statements,
                                        tail,
                                    )
                                })
                            })
                        })
                    })
                }
                Some(ast::BlockModifier::Gen(_)) => {
                    let capture_by =
                        if e.move_token().is_some() { CaptureBy::Value } else { CaptureBy::Ref };
                    self.with_label_rib(RibKind::Closure, |this| {
                        this.with_binding_owner(|this| {
                            this.with_awaitable_block(Awaitable::No("non-async gen block"), |this| {
                                this.collect_block_(e, |this, id, statements, tail| {
                                    this.desugared_coroutine_expr(
                                        CoroutineKind::Gen,
                                        CoroutineSource::Block,
                                        capture_by,
                                        id,
                                        statements,
                                        tail,
                                    )
                                })
                            })
                        })
                    })
                }
                Some(ast::BlockModifier::AsyncGen(_)) => {
                    let capture_by =
                        if e.move_token().is_some() { CaptureBy::Value } else { CaptureBy::Ref };
                    self.with_label_rib(RibKind::Closure, |this| {
                        this.with_binding_owner(|this| {
                            this.with_awaitable_block(Awaitable::Yes, |this| {
                                this.collect_block_(e, |this, id, statements, tail| {
                                    this.desugared_coroutine_expr(
                                        CoroutineKind::AsyncGen,
                                        CoroutineSource::Block,
                                        capture_by,
                                        id,
                                        statements,
                                        tail,
                                    )
                                })
                            })
                        })
                    })
                }
                Some(ast::BlockModifier::Const(_)) => {
                    self.with_label_rib(RibKind::Constant, |this| {
                        this.with_awaitable_block(Awaitable::No("constant block"), |this| {
                            this.with_binding_owner(|this| {
                                let inner_expr = this.collect_block(e);
                                this.alloc_expr(Expr::Const(inner_expr), syntax_ptr)
                            })
                        })
                    })
                }
                None => self.collect_block(e),
            },
            ast::Expr::LoopExpr(e) => {
                let label = e.label().map(|label| {
                    (self.hygiene_id_for(label.syntax().text_range()), self.collect_label(label))
                });
                let body = self.collect_labelled_block_opt(label, e.loop_body());
                self.alloc_expr(Expr::Loop { body, label: label.map(|it| it.1), source: LoopSource::Loop }, syntax_ptr)
            }
            ast::Expr::WhileExpr(e) => self.collect_while_loop(syntax_ptr, e),
            ast::Expr::ForExpr(e) => self.collect_for_loop(syntax_ptr, e),
            ast::Expr::CallExpr(e) => {
                let callee = self.collect_expr_opt(e.expr());
                let mark = self.collect_args(e.arg_list());
                self.with_lists(mark, |this, lists| {
                    this.alloc_expr(Expr::Call { callee, args: lists.exprs_since(mark) }, syntax_ptr)
                })
            }
            ast::Expr::MethodCallExpr(e) => {
                let receiver = self.collect_expr_opt(e.receiver());
                let mark = self.collect_args(e.arg_list());
                let method_name = e.name_ref().map(|nr| nr.as_name()).unwrap_or_else(Name::missing);
                let generic_args = e.generic_arg_list().and_then(|it| {
                    self.lower_generic_args(it, &mut ImplTraitLowering::Error)
                });
                let id = self.with_lists(mark, |this, lists| {
                    this.store.alloc_method_call(
                        receiver,
                        method_name,
                        lists.exprs_since(mark),
                        generic_args,
                    )
                });
                self.map_expr(id, syntax_ptr)
            }
            ast::Expr::MatchExpr(e) => {
                let expr = self.collect_expr_opt(e.expr());
                let mark = self.store.list_mark();
                for arm in e.match_arm_list().iter().flat_map(|it| it.arms()) {
                    if self.check_cfg(arm) {
                        let arm = MatchArm {
                            pat: self.collect_pat_top(arm.pat()),
                            expr: self.collect_expr_opt(arm.expr()),
                            guard: arm.guard().map(|guard| self.collect_expr_opt(guard.condition())),
                        };
                        self.store.push_list_arm(arm);
                    }
                }
                self.with_lists(mark, |this, lists| {
                    this.alloc_expr(Expr::Match { expr, arms: lists.arms_since(mark) }, syntax_ptr)
                })
            }
            ast::Expr::PathExpr(e) => {
                let (expr_id, hygiene) = match self.collect_expr_path(e) {
                    Some((path, hygiene)) => {
                        let id = self.store.alloc_path_expr(path);
                        (self.map_expr(id, syntax_ptr), hygiene)
                    }
                    None => (self.alloc_expr(Expr::Missing, syntax_ptr), HygieneId::ROOT),
                };
                if !hygiene.is_root() {
                    self.store.ident_hygiene.insert(expr_id.into(), hygiene);
                }
                expr_id
            }
            ast::Expr::ContinueExpr(e) => {
                let label = self.resolve_label(e.lifetime()).unwrap_or_else(|e| {
                    self.store.diagnostics.push(e);
                    None
                });
                self.alloc_expr(Expr::Continue { label }, syntax_ptr)
            }
            ast::Expr::BreakExpr(e) => {
                let label = self.resolve_label(e.lifetime()).unwrap_or_else(|e| {
                    self.store.diagnostics.push(e);
                    None
                });
                let expr = e.expr().map(|e| self.collect_expr(e));
                self.alloc_expr(Expr::Break { expr, label }, syntax_ptr)
            }
            ast::Expr::ParenExpr(e) => {
                // A macro puts its `$x:expr` in parentheses: with `#[cfg]` on what was handed in,
                // those go with it.
                let inner = match e.expr() {
                    Some(inner) => self.maybe_collect_expr(inner)?,
                    None => self.missing_expr(),
                };
                // make the paren expr point to the inner expression as well for IDE resolution
                if self.store.record {
                    let src = self.expander.in_file(syntax_ptr);
                    self.store.expr_map.push((src, inner.into()));
                    // rustc's `lower_expr_mut` of `ExprKind::Paren`: the inner expression's span
                    // is the parens' when theirs contains it (not one a macro expanded to).
                    let widen = self.store.expr_map_back.get(inner).is_some_and(|it| {
                        it.file_id == src.file_id
                            && src.value.text_range().contains_range(it.value.text_range())
                    });
                    if widen {
                        self.store.expr_map_back.insert(inner, src.map(AstPtr::wrap_left));
                    }
                }
                inner
            }
            ast::Expr::ReturnExpr(e) => {
                let expr = e.expr().map(|e| self.collect_expr(e));
                self.alloc_expr(Expr::Return { expr }, syntax_ptr)
            }
            ast::Expr::BecomeExpr(e) => {
                let expr =
                    e.expr().map(|e| self.collect_expr(e)).unwrap_or_else(|| self.missing_expr());
                self.alloc_expr(Expr::Become { expr }, syntax_ptr)
            }
            ast::Expr::YieldExpr(e) => {
                self.is_lowering_coroutine = true;
                let expr = e.expr().map(|e| self.collect_expr(e));
                self.alloc_expr(Expr::Yield { expr }, syntax_ptr)
            }
            ast::Expr::YeetExpr(e) => {
                let expr = e.expr().map(|e| self.collect_expr(e));
                self.alloc_expr(Expr::Yeet { expr }, syntax_ptr)
            }
            ast::Expr::RecordExpr(e) => {
                let path = e
                    .path()
                    .and_then(|path| self.lower_path_view(path, &mut ImplTraitLowering::Error));
                let Some(path) = path else {
                    return Some(self.missing_expr());
                };
                let mark = self.store.list_mark();
                let spread = if let Some(nfl) = e.record_expr_field_list() {
                    for field in nfl.fields() {
                        if !self.check_cfg(field) {
                            continue;
                        }

                        let Some(name) = field.field_name() else { continue };
                        let name = name.as_name();

                        let expr = match field.expr() {
                            Some(e) => self.collect_expr(e),
                            None => self.missing_expr(),
                        };
                        if self.store.record {
                            let src = self.expander.in_file(AstPtr::<tag::RecordExprField>::of(field));
                            self.store.field_map_back.push((expr, src));
                        }
                        self.store.push_list_record_field(RecordLitField { name, expr });
                    }
                    let spread_expr = nfl.spread().map(|s| self.collect_expr(s));
                    let has_spread_syntax = nfl.dotdot_token().is_some();
                    match (spread_expr, has_spread_syntax) {
                        (None, false) => RecordSpread::None,
                        (None, true) => RecordSpread::FieldDefaults,
                        (Some(expr), _) => RecordSpread::Expr(expr),
                    }
                } else {
                    RecordSpread::None
                };
                let id = self.with_lists(mark, |this, lists| {
                    this.store.alloc_record_lit(path, lists.record_fields_since(mark), spread)
                });
                self.map_expr(id, syntax_ptr)
            }
            ast::Expr::FieldExpr(e) => {
                let expr = self.collect_expr_opt(e.expr());
                let name = match e.field_access() {
                    Some(kind) => kind.as_name(),
                    _ => Name::missing(),
                };
                self.alloc_expr(Expr::Field { expr, name: &name }, syntax_ptr)
            }
            ast::Expr::AwaitExpr(e) => {
                let expr = self.collect_expr_opt(e.expr());
                if let Awaitable::No(location) = self.is_lowering_awaitable_block() {
                    self.store.diagnostics.push(ExpressionStoreDiagnostics::AwaitOutsideOfAsync {
                        node: self.expander.in_file(AstPtr::<tag::AwaitExpr>::of(e)),
                        // alloc: a diagnostic, the error path.
                        location: location.to_string(),
                    });
                    // rustc's lowering makes it `ExprKind::Err`: nothing more is reported of it.
                    return Some(self.alloc_expr(Expr::Missing, syntax_ptr));
                }
                self.alloc_expr(Expr::Await { expr }, syntax_ptr)
            }
            ast::Expr::TryExpr(e) => self.collect_try_operator(syntax_ptr, e),
            ast::Expr::CastExpr(e) => {
                let expr = self.collect_expr_opt(e.expr());
                let type_ref = self.lower_type_ref_opt_disallow_impl_trait(e.ty());
                self.alloc_expr(Expr::Cast { expr, type_ref }, syntax_ptr)
            }
            ast::Expr::RefExpr(e) => {
                let expr = self.collect_expr_opt(e.expr());
                let raw_tok = e.raw_token().is_some();
                let mutability = if raw_tok {
                    if e.mut_token().is_some() { Mutability::Mut } else { Mutability::Shared }
                } else {
                    Mutability::from_mutable(e.mut_token().is_some())
                };
                let rawness = Rawness::from_raw(raw_tok);
                self.alloc_expr(Expr::Ref { expr, rawness, mutability }, syntax_ptr)
            }
            ast::Expr::PrefixExpr(e) => {
                let expr = self.collect_expr_opt(e.expr());
                match e.op_kind() {
                    Some(op) => self.alloc_expr(Expr::UnaryOp { expr, op }, syntax_ptr),
                    None => self.alloc_expr(Expr::Missing, syntax_ptr),
                }
            }
            ast::Expr::ClosureExpr(e) => self.with_label_rib(RibKind::Closure, |this| {
                let mut is_coroutine_closure = false;
                // The params (pats) and their types (opt types) above `mark` stay on the stacks
                // until the coroutine closure's params are given their owner below.
                let mark = this.store.list_mark();
                let closure = this.with_binding_owner_and_return(|this| {
                    // For coroutine closures, the body, aka. the coroutine is the bindings owner, and not the closure.
                    if let Some(pl) = e.param_list() {
                        for param in pl.params() {
                            if !this.check_cfg(param) {
                                continue;
                            }

                            let pat = this.collect_pat_top(param.pat());
                            let type_ref =
                                param.ty().map(|it| this.lower_type_ref_disallow_impl_trait(it));
                            this.store.push_list_pat(pat);
                            this.store.push_list_opt_type(type_ref);
                        }
                    }
                    let ret_type = e
                        .ret_type()
                        .and_then(|r| r.ty())
                        .map(|it| this.lower_type_ref_disallow_impl_trait(it));

                    let prev_is_lowering_coroutine = mem::take(&mut this.is_lowering_coroutine);
                    let prev_try_block = this.current_try_block.take();

                    let awaitable = if e.async_token().is_some() {
                        Awaitable::Yes
                    } else {
                        Awaitable::No("non-async closure")
                    };
                    let mut body = this
                        .with_awaitable_block(awaitable, |this| this.collect_expr_opt(e.body()));
                    let kind = {
                        if e.async_token().is_some() && e.gen_token().is_some() {
                            Some(CoroutineKind::AsyncGen)
                        } else if e.async_token().is_some() {
                            Some(CoroutineKind::Async)
                        } else if e.gen_token().is_some() {
                            Some(CoroutineKind::Gen)
                        } else {
                            None
                        }
                    };

                    let closure_kind = if let Some(kind) = kind {
                        // It's important that this expr is allocated immediately before the closure.
                        // We rely on it for `coroutine_for_closure()`.
                        let mut params = mem::take(&mut this.stacks.closure_params);
                        let params_base = params.len();
                        params.extend(this.store.pats_since(mark).iter().copied().map(Param::new));
                        body = this.lower_coroutine_body_with_moved_arguments(
                            &mut None,
                            &mut params[params_base..],
                            body,
                            kind,
                            CoroutineSource::Closure,
                        );
                        let top = this.store.list_mark();
                        let mut lists = this.store.take_lists();
                        lists
                            .pats_since_mut(mark)
                            .iter_mut()
                            .set_from(params[params_base..].iter().map(|arg| arg.formal));
                        this.store.put_lists(lists, top);
                        params.truncate(params_base);
                        this.stacks.closure_params = params;
                        is_coroutine_closure = true;

                        ClosureKind::CoroutineClosure(kind)
                    } else if this.is_lowering_coroutine {
                        let movability = if e.static_token().is_some() {
                            Movability::Static
                        } else {
                            Movability::Movable
                        };
                        ClosureKind::OldCoroutine(movability)
                    } else {
                        ClosureKind::Closure
                    };
                    let capture_by =
                        if e.move_token().is_some() { CaptureBy::Value } else { CaptureBy::Ref };
                    this.is_lowering_coroutine = prev_is_lowering_coroutine;
                    this.current_try_block = prev_try_block;
                    let top = this.store.list_mark();
                    let closure = this.with_lists(top, |this, lists| {
                        this.alloc_expr(
                            Expr::Closure {
                                args: lists.pats_since(mark),
                                arg_types: lists.opt_types_since(mark),
                                ret_type,
                                body,
                                closure_kind,
                                capture_by,
                            },
                            syntax_ptr,
                        )
                    });
                    let inherits = |attr: ast::Attr<'_>| {
                        attr.simple_name().is_some_and(|it| it == "rustc_inherit_overflow_checks")
                    };
                    if ast::HasAttrs::attrs(&e).any(inherits) {
                        this.store.overflow_inheriting.insert(closure);
                    }

                    (if is_coroutine_closure { body } else { closure }, closure)
                });

                if is_coroutine_closure {
                    // The closure's args, still on the pat stack above `mark`.
                    for i in 0..this.store.pats_since(mark).len() {
                        let arg = this.store.pats_since(mark)[i];
                        let Pat::Bind { id, .. } = this.store.pat(arg) else {
                            unreachable!("`lower_coroutine_body_with_moved_arguments()` makes each argument of a coroutine closure a simple binding");
                        };
                        this.store.binding_owners.insert(id, closure);
                    }
                }
                this.store.truncate_to(mark);

                closure
            }),
            ast::Expr::BinExpr(e) => {
                let op = e.op_kind();
                if let Some(ast::BinaryOp::Assignment { op: None }) = op {
                    let target = self.collect_expr_as_pat_opt(e.lhs());
                    let value = self.collect_expr_opt(e.rhs());
                    self.alloc_expr(Expr::Assignment { target, value }, syntax_ptr)
                } else {
                    let lhs = self.collect_expr_opt(e.lhs());
                    let rhs = self.collect_expr_opt(e.rhs());
                    self.alloc_expr(Expr::BinaryOp { lhs, rhs, op }, syntax_ptr)
                }
            }
            ast::Expr::TupleExpr(e) => {
                let mark = self.collect_exprs(e.fields());
                // if there is a leading comma, the user is most likely to type out a leading expression
                // so we insert a missing expression at the beginning for IDE features
                let leading_missing = comma_follows_token(e.l_paren_token());
                if leading_missing {
                    let missing = self.missing_expr();
                    self.store.push_list_expr(missing);
                }

                self.with_lists(mark, |this, lists| {
                    if leading_missing {
                        lists.exprs_since_mut(mark).rotate_right(1);
                    }
                    this.alloc_expr(Expr::Tuple { exprs: lists.exprs_since(mark) }, syntax_ptr)
                })
            }
            ast::Expr::ArrayExpr(e) => {
                let kind = e.kind();

                match kind {
                    ArrayExprKind::ElementList(e) => {
                        let mark = self.collect_exprs(e);
                        self.with_lists(mark, |this, lists| {
                            this.alloc_expr(
                                Expr::Array(Array::ElementList {
                                    elements: lists.exprs_since(mark),
                                }),
                                syntax_ptr,
                            )
                        })
                    }
                    ArrayExprKind::Repeat { initializer, repeat } => {
                        let initializer = self.collect_expr_opt(initializer);
                        let repeat = self.with_label_rib(RibKind::Constant, |this| {
                            // An anonymous const: a body of its own, never `async`.
                            this.with_awaitable_block(Awaitable::No("constant"), |this| {
                                if let Some(repeat) = repeat {
                                    this.with_binding_owner(|this| this.collect_expr(repeat))
                                } else {
                                    this.missing_expr()
                                }
                            })
                        });
                        self.alloc_expr(
                            Expr::Array(Array::Repeat { initializer, repeat }),
                            syntax_ptr,
                        )
                    }
                }
            }

            ast::Expr::Literal(e) => {
                let literal = match self.module_path_literal(e) {
                    Some(it) => it,
                    None => self.lower_literal(e),
                };
                let id = self.store.alloc_literal_expr(literal);
                self.map_expr(id, syntax_ptr)
            }
            ast::Expr::IndexExpr(e) => {
                let base = self.collect_expr_opt(e.base());
                let index = self.collect_expr_opt(e.index());
                self.alloc_expr(Expr::Index { base, index }, syntax_ptr)
            }
            ast::Expr::RangeExpr(e) => self.collect_range_expr(e, syntax_ptr),
            ast::Expr::MacroExpr(e) => {
                let e = e.macro_call()?;
                let macro_ptr = AstPtr::<tag::MacroCall>::of(e);
                let id = self.collect_macro_call(e, macro_ptr, true, |this, expansion| {
                    expansion.map(|it| this.maybe_collect_expr(it))
                });
                match id {
                    Some(Some(id)) => {
                        // Make the macro-call point to its expanded expression so we can query
                        // semantics on syntax pointers to the macro
                        if self.store.record {
                            let src = self.expander.in_file(syntax_ptr);
                            self.store.expr_map.push((src, id.into()));
                        }
                        id
                    }
                    // Macro expanded into a disabled cfg (yes, there is such thing, see the weird_cfgs test).
                    Some(None) => return None,
                    None => self.alloc_expr(Expr::Missing, syntax_ptr),
                }
            }
            ast::Expr::UnderscoreExpr(_) => self.alloc_expr(Expr::Underscore, syntax_ptr),
            ast::Expr::AsmExpr(e) => self.lower_inline_asm(e, syntax_ptr),
            ast::Expr::OffsetOfExpr(e) => {
                let container = self.lower_type_ref_opt_disallow_impl_trait(e.ty());
                // A float token is two tuple indices: `0.1`.
                // alloc: the stored field list, once per `offset_of!`.
                let mut fields = Vec::new();
                for it in e.fields() {
                    match it.syntax().first_token() {
                        Some(token) if token.kind() == syntax::SyntaxKind::FLOAT_NUMBER => fields
                            .extend(
                                token
                                    .text()
                                    .split('.')
                                    .filter_map(|index| index.parse().ok())
                                    .map(Name::new_tuple_field),
                            ),
                        _ => fields.push(it.as_name()),
                    }
                }
                let fields = fields.into_boxed_slice();
                let id = self.store.alloc_offset_of_expr(OffsetOf { container, fields });
                self.map_expr(id, syntax_ptr)
            }
            ast::Expr::FormatArgsExpr(f) => self.collect_format_args(f, syntax_ptr),
            ast::Expr::IncludeBytesExpr(_) => self.alloc_expr(Expr::IncludeBytes, syntax_ptr),
            ast::Expr::UnsafeBinderCastExpr(e) => {
                let kind = match e.is_wrap() {
                    true => UnsafeBinderCastKind::Wrap,
                    false => UnsafeBinderCastKind::Unwrap,
                };
                let expr = self.collect_expr_opt(e.expr());
                let ty = e.ty().map(|ty| self.lower_type_ref_disallow_impl_trait(ty));
                self.alloc_expr(Expr::UnsafeBinderCast { kind, expr, ty }, syntax_ptr)
            }
            ast::Expr::CfgPredExpr(e) => {
                let enabled = Literal::Bool(self.check_cfg_pred(e.cfg_predicate()));
                let id = self.store.alloc_literal_expr(enabled);
                self.map_expr(id, syntax_ptr)
            },
        })
    }

    fn collect_range_expr(&mut self, e: ast::RangeExpr<'_>, syntax_ptr: ExprPtr) -> ExprId {
        let lhs = e.start().map(|lhs| self.collect_expr(lhs));
        let rhs = e.end().map(|rhs| self.collect_expr(rhs));
        let kind = e.op_kind().unwrap_or(ast::RangeOp::Exclusive);
        let new_range = self.features().new_range;
        let lang_items = self.lang_items();
        let lang_item = match (lhs, rhs, kind) {
            (None, None, _) => lang_items.RangeFull,
            (Some(..), None, ast::RangeOp::Exclusive) => {
                if new_range {
                    lang_items.RangeFromCopy
                } else {
                    lang_items.RangeFrom
                }
            }
            (None, Some(..), ast::RangeOp::Exclusive) => lang_items.RangeTo,
            (Some(..), Some(..), ast::RangeOp::Exclusive) => {
                if new_range {
                    lang_items.RangeCopy
                } else {
                    lang_items.Range
                }
            }
            (None, Some(..), ast::RangeOp::Inclusive) => {
                if new_range {
                    lang_items.RangeToInclusiveCopy
                } else {
                    lang_items.RangeToInclusive
                }
            }
            (Some(lhs), Some(rhs), ast::RangeOp::Inclusive) => {
                if new_range {
                    lang_items.RangeInclusiveCopy
                } else {
                    return self.collect_inclusive_range(syntax_ptr, lang_items, lhs, rhs);
                }
            }
            (Some(..), None, ast::RangeOp::Inclusive) => {
                if new_range {
                    lang_items.RangeFromCopy
                } else {
                    lang_items.RangeFrom
                }
            }
        };
        let Some(struct_path) = self.lang_path(lang_item) else {
            return self.alloc_expr(Expr::Missing, syntax_ptr);
        };
        let lhs = lhs.map(|lhs| (lhs, sym::start));
        let rhs = rhs.map(|rhs| {
            (
                rhs,
                if lang_item == lang_items.RangeInclusiveCopy
                    || lang_item == lang_items.RangeToInclusiveCopy
                {
                    sym::last
                } else {
                    sym::end
                },
            )
        });
        let mark = self.store.list_mark();
        for (expr, name) in std::iter::chain(lhs, rhs) {
            let field = RecordLitField { name: Name::new_symbol_root(name), expr };
            self.store.push_list_record_field(field);
        }
        let id = self.with_lists(mark, |this, lists| {
            this.store.alloc_record_lit(
                struct_path,
                lists.record_fields_since(mark),
                RecordSpread::None,
            )
        });
        self.map_expr(id, syntax_ptr)
    }

    fn collect_inclusive_range(
        &mut self,
        syntax_ptr: ExprPtr,
        lang_items: &LangItems,
        lhs: ExprId,
        rhs: ExprId,
    ) -> ExprId {
        let fn_path = self.lang_path(lang_items.RangeInclusiveNew);
        let fn_path = self.alloc_opt_path_expr(fn_path);
        self.alloc_expr(Expr::Call { callee: fn_path, args: &[lhs, rhs] }, syntax_ptr)
    }

    fn collect_expr_path(&mut self, e: ast::PathExpr<'_>) -> Option<(PathIdx, HygieneId)> {
        e.path().and_then(|path| {
            let path = self.lower_path_view(path, &mut ImplTraitLowering::Error)?;
            let lowered = self.store.path_at(path);
            // Need to enable `mod_path.len() < 1` for `self`. A longer path takes its hygiene
            // too, whatever it starts with, for the items a `macro` names where it is defined.
            let may_be_variable =
                matches!(lowered, Path::BarePath(mod_path) if mod_path.len() <= 1);
            let hygiene = if may_be_variable || lowered.mod_path().is_some() {
                self.hygiene_id_for(e.syntax().text_range())
            } else {
                HygieneId::ROOT
            };
            Some((path, hygiene))
        })
    }

    /// Whether this path should be lowered as destructuring assignment, or as a normal assignment.
    fn path_is_destructuring_assignment(&self, path: &ModPath) -> bool {
        // rustc has access to a full resolver here, including local variables and generic params, and it checks the following
        // criteria: a path not lowered as destructuring assignment if it can *fully resolve* to something that is *not*
        // a const, a unit struct or a variant.
        // We don't have access to a full resolver here. So we should do the same as rustc, but assuming that local variables
        // could be resolved to nothing (fortunately, there cannot be a local variable shadowing a unit struct/variant/const,
        // as that is an error). We don't need to consider const params as it's an error to refer to these in patterns.
        let (resolution, unresolved_idx, _) = self.def_map.resolve_path_locally(
            self.local_def_map,
            self.db,
            self.module,
            path,
            BuiltinShadowMode::Other,
        );
        match unresolved_idx {
            Some(_) => {
                // If `Some(_)`, path could be resolved to unit struct/variant/const with type information, i.e. an assoc type or const.
                // If `None`, path could be a local variable.
                resolution.take_types().is_some()
            }
            None => match resolution.take_values() {
                // We don't need to consider non-unit structs/variants, as those are not value types.
                Some(ModuleDefId::EnumVariantId(_))
                | Some(ModuleDefId::AdtId(_))
                | Some(ModuleDefId::ConstId(_)) => true,
                _ => false,
            },
        }
    }

    fn collect_expr_as_pat_opt(&mut self, expr: Option<ast::Expr<'_>>) -> PatId {
        match expr {
            Some(expr) => self.collect_expr_as_pat(expr),
            _ => self.missing_pat(),
        }
    }

    fn collect_expr_as_pat(&mut self, expr: ast::Expr<'_>) -> PatId {
        self.maybe_collect_expr_as_pat(expr).unwrap_or_else(|| {
            let src = self.expander.in_file(ExprPtr::of(expr).wrap_left());
            let expr = self.collect_expr(expr);
            // Do not use `alloc_pat_from_expr()` here, it will override the entry in `expr_map`.
            let id = self.store.alloc_pat(Pat::Expr(expr));
            if self.store.record {
                self.store.pat_map_back.insert(id, src);
            }
            id
        })
    }

    fn maybe_collect_expr_as_pat(&mut self, expr: ast::Expr<'_>) -> Option<PatId> {
        if !self.check_cfg(expr) {
            return None;
        }
        let syntax_ptr = ExprPtr::of(expr);

        let result = match expr {
            ast::Expr::UnderscoreExpr(_) => self.alloc_pat_from_expr(Pat::Wild, syntax_ptr),
            ast::Expr::ParenExpr(e) => {
                // We special-case `(..)` for consistency with patterns.
                if let Some(ast::Expr::RangeExpr(range)) = e.expr()
                    && range.is_range_full()
                {
                    return Some(self.alloc_pat_from_expr(
                        Pat::Tuple { args: &[], ellipsis: Some(0) },
                        syntax_ptr,
                    ));
                }
                return e.expr().and_then(|expr| self.maybe_collect_expr_as_pat(expr));
            }
            ast::Expr::TupleExpr(e) => {
                let (ellipsis, mark) = collect_tuple(self, e.fields());
                self.with_lists(mark, |this, lists| {
                    this.alloc_pat_from_expr(
                        Pat::Tuple { args: lists.pats_since(mark), ellipsis },
                        syntax_ptr,
                    )
                })
            }
            ast::Expr::ArrayExpr(e) => {
                if e.semicolon_token().is_some() {
                    return None;
                }

                let mark = self.store.list_mark();
                let mut elements = e.exprs();
                let mut rest_ptr = None;
                for elem in elements.by_ref() {
                    match collect_possibly_rest(self, elem) {
                        Either::Left(elem) => self.store.push_list_pat(elem),
                        Either::Right(()) => {
                            rest_ptr = Some(ExprPtr::of(elem));
                            break;
                        }
                    }
                }
                let prefix_len = self.store.pats_since(mark).len();
                for elem in elements {
                    let pat = self.collect_expr_as_pat(elem);
                    self.store.push_list_pat(pat);
                }
                let slice = rest_ptr.map(|ptr| self.alloc_pat_from_expr(Pat::Rest, ptr));
                self.with_lists(mark, |this, lists| {
                    let (prefix, suffix) = lists.pats_since(mark).split_at(prefix_len);
                    this.alloc_pat_from_expr(Pat::Slice { prefix, slice, suffix }, syntax_ptr)
                })
            }
            ast::Expr::CallExpr(e) => {
                let path = collect_path(self, e.expr()?)?;
                let path = path
                    .path()
                    .and_then(|path| self.lower_path_view(path, &mut ImplTraitLowering::Error));
                let Some(path) = path else {
                    return Some(self.missing_pat());
                };
                let arg_list = e.arg_list()?;
                let (ellipsis, mark) = collect_tuple(self, arg_list.args());
                let id = self.with_lists(mark, |this, lists| {
                    this.store.alloc_tuple_struct_pat(path, lists.pats_since(mark), ellipsis)
                });
                self.map_pat_from_expr(id, syntax_ptr)
            }
            ast::Expr::PathExpr(e) => {
                let (path, hygiene) = self.collect_expr_path(e)?;
                let mod_path =
                    self.store.path_at(path).mod_path().expect("should not lower to lang path");
                if self.path_is_destructuring_assignment(mod_path) {
                    let pat_id = self.store.alloc_path_pat(path);
                    let pat_id = self.map_pat_from_expr(pat_id, syntax_ptr);
                    if !hygiene.is_root() {
                        self.store.ident_hygiene.insert(pat_id.into(), hygiene);
                    }
                    pat_id
                } else {
                    return None;
                }
            }
            ast::Expr::MacroExpr(e) => {
                let e = e.macro_call()?;
                let macro_ptr = AstPtr::<tag::MacroCall>::of(e);
                let src = self.expander.in_file(ExprPtr::of(expr));
                let id = self.collect_macro_call(e, macro_ptr, true, |this, expansion| {
                    this.collect_expr_as_pat_opt(expansion)
                });
                if self.store.record {
                    self.store.expr_map.push((src, id.into()));
                }
                id
            }
            ast::Expr::RecordExpr(e) => {
                let path = e
                    .path()
                    .and_then(|path| self.lower_path_view(path, &mut ImplTraitLowering::Error));
                let Some(path) = path else {
                    return Some(self.missing_pat());
                };
                let record_field_list = e.record_expr_field_list()?;
                let ellipsis = record_field_list.dotdot_token().is_some();
                if let Some(spread) = record_field_list.spread() {
                    self.store.diagnostics.push(
                        ExpressionStoreDiagnostics::FruInDestructuringAssignment {
                            node: self.expander.in_file(ExprPtr::of(spread)),
                        },
                    );
                }
                let mark = self.store.list_mark();
                for f in record_field_list.fields() {
                    if !self.check_cfg(f) {
                        continue;
                    }
                    let Some(field_expr) = f.expr() else { continue };
                    let pat = self.collect_expr_as_pat(field_expr);
                    let Some(name) = f.field_name() else { continue };
                    let name = name.as_name();
                    if self.store.record {
                        let src =
                            self.expander.in_file(AstPtr::<tag::RecordExprField>::of(f).wrap_left());
                        self.store.pat_field_map_back.push((pat, src));
                    }
                    self.store.push_list_field_pat(RecordFieldPat { name, pat });
                }
                let id = self.with_lists(mark, |this, lists| {
                    this.store.alloc_record_pat(path, lists.field_pats_since(mark), ellipsis)
                });
                self.map_pat_from_expr(id, syntax_ptr)
            }
            _ => return None,
        };
        return Some(result);

        /// The path is of `expr`'s tree or of an expansion's, so it lives as long as both (`'v`).
        fn collect_path<'db: 'v, 't: 'v, 'v>(
            this: &mut ExprCollector<'db>,
            expr: ast::Expr<'t>,
        ) -> Option<ast::PathExpr<'v>> {
            match expr {
                ast::Expr::PathExpr(e) => Some(e),
                ast::Expr::MacroExpr(mac) => {
                    let call = mac.macro_call()?;
                    {
                        let macro_ptr = AstPtr::<tag::MacroCall>::of(call);
                        this.collect_macro_call(call, macro_ptr, true, |this, expanded_path| {
                            collect_path(this, expanded_path?)
                        })
                    }
                }
                _ => None,
            }
        }

        fn collect_possibly_rest(
            this: &mut ExprCollector<'_>,
            expr: ast::Expr<'_>,
        ) -> Either<PatId, ()> {
            match expr {
                ast::Expr::RangeExpr(e) if e.is_range_full() => Either::Right(()),
                ast::Expr::MacroExpr(mac) => match mac.macro_call() {
                    Some(call) => {
                        let macro_ptr = AstPtr::<tag::MacroCall>::of(call);
                        let pat = this.collect_macro_call(
                            call,
                            macro_ptr,
                            true,
                            |this, expanded_expr| match expanded_expr {
                                Some(expanded_pat) => collect_possibly_rest(this, expanded_pat),
                                None => Either::Left(this.missing_pat()),
                            },
                        );
                        if let Either::Left(pat) = pat
                            && this.store.record
                        {
                            let src = this.expander.in_file(ExprPtr::of(expr).wrap_left());
                            this.store.pat_map_back.insert(pat, src);
                        }
                        pat
                    }
                    None => {
                        let ptr = ExprPtr::of(expr);
                        Either::Left(this.alloc_pat_from_expr(Pat::Missing, ptr))
                    }
                },
                _ => Either::Left(this.collect_expr_as_pat(expr)),
            }
        }

        /// The tuple's `..` and the mark its other elements are pushed above on the pat stack.
        fn collect_tuple(
            this: &mut ExprCollector<'_>,
            fields: AstChildren<'_, ast::Expr<'_>>,
        ) -> (Option<u32>, ListMark) {
            let mark = this.store.list_mark();
            let mut ellipsis = None;
            for (idx, elem) in fields.enumerate() {
                match collect_possibly_rest(this, elem) {
                    Either::Left(pat) => this.store.push_list_pat(pat),
                    Either::Right(()) => {
                        if ellipsis.is_none() {
                            ellipsis = Some(idx as u32);
                        }
                        // FIXME: Report an error here otherwise.
                    }
                }
            }
            (ellipsis, mark)
        }
    }

    /// The callback should return two exprs: the first is the bindings owner, the second is the expr to return.
    fn with_binding_owner_and_return(
        &mut self,
        create_expr: impl FnOnce(&mut Self) -> (ExprId, ExprId),
    ) -> ExprId {
        let prev_unowned_bindings_len = self.stacks.unowned_bindings.len();
        let (bindings_owner, expr_to_return) = create_expr(self);
        self.associate_unowned_bindings_with(prev_unowned_bindings_len, bindings_owner);
        expr_to_return
    }

    fn associate_unowned_bindings_with(
        &mut self,
        prev_unowned_bindings_len: usize,
        bindings_owner: ExprId,
    ) {
        for binding in self.stacks.unowned_bindings.drain(prev_unowned_bindings_len..) {
            self.store.binding_owners.insert(binding, bindings_owner);
        }
    }

    fn with_binding_owner(&mut self, create_expr: impl FnOnce(&mut Self) -> ExprId) -> ExprId {
        self.with_binding_owner_and_return(move |this| {
            let expr = create_expr(this);
            (expr, expr)
        })
    }

    /// Desugar `try { <stmts>; <expr> }` into `'<new_label>: { <stmts>; ::std::ops::Try::from_output(<expr>) }`,
    /// `try { <stmts>; }` into `'<new_label>: { <stmts>; ::std::ops::Try::from_output(()) }`
    /// and save the `<new_label>` to use it as a break target for desugaring of the `?` operator.
    fn desugar_try_block(
        &mut self,
        e: BlockExpr<'_>,
        result_type: Option<ast::Type<'_>>,
    ) -> ExprId {
        let try_from_output = self.lang_path(self.lang_items().TryTraitFromOutput);
        let label = self.generate_new_name();
        let label = self.alloc_label_desugared(
            Label { name: label },
            AstPtr::<tag::BlockExpr>::of(e).wrap_right(),
        );
        let try_block_info = match result_type {
            Some(_) => TryBlock::Heterogeneous { label },
            None => TryBlock::Homogeneous { label },
        };
        let old_try_block = self.current_try_block.replace(try_block_info);

        let ptr: ExprPtr = AstPtr::<tag::BlockExpr>::of(e).upcast();
        let (btail, expr_id) = self.with_labeled_rib(label, HygieneId::ROOT, |this| {
            let mut btail = None;
            let block = this.collect_block_(e, |_, id, statements, tail| {
                btail = tail;
                Expr::Block { id, statements, tail, label: Some(label), unsafe_: Unsafe::No }
            });
            (btail, block)
        });

        let callee = self.alloc_opt_path_expr(try_from_output);
        let callee = self.map_expr_desugared(callee, ptr);
        let next_tail = match btail {
            Some(tail) => {
                self.alloc_expr_desugared_with_ptr(Expr::Call { callee, args: &[tail] }, ptr)
            }
            None => {
                let unit = self.alloc_expr_desugared_with_ptr(Expr::Tuple { exprs: &[] }, ptr);
                self.alloc_expr_desugared_with_ptr(Expr::Call { callee, args: &[unit] }, ptr)
            }
        };
        self.store.set_block_tail(expr_id, next_tail);
        self.current_try_block = old_try_block;
        match result_type {
            Some(ty) => {
                // `{ let <name>: <ty> = <expr>; <name> }`
                let name = self.generate_new_name();
                let type_ref = self.lower_type_ref_disallow_impl_trait(ty);
                let binding = self.alloc_binding(
                    name,
                    BindingAnnotation::Unannotated,
                    HygieneId::ROOT,
                );
                let pat = self.alloc_pat_desugared(Pat::Bind { id: binding, subpat: None });
                self.add_definition_to_binding(binding, pat);
                let tail_expr = self.store.alloc_name_path_expr(name);
                let tail_expr = self.map_expr_desugared(tail_expr, ptr);
                let statements = self.store.list_mark();
                self.store.push_list_statement(Statement::Let {
                    pat,
                    type_ref: Some(type_ref),
                    initializer: Some(expr_id),
                    else_branch: None,
                    is_super: false,
                });
                self.with_lists(statements, |this, lists| {
                    this.alloc_expr_desugared_with_ptr(
                        Expr::Block {
                            id: None,
                            statements: lists.statements_since(statements),
                            tail: Some(tail_expr),
                            label: None,
                            unsafe_: Unsafe::No,
                        },
                        ptr,
                    )
                })
            }
            None => expr_id,
        }
    }

    /// Desugar `ast::WhileExpr` from: `[opt_ident]: while <cond> <body>` into:
    /// ```ignore (pseudo-rust)
    /// [opt_ident]: loop {
    ///   if <cond> {
    ///     <body>
    ///   }
    ///   else {
    ///     break;
    ///   }
    /// }
    /// ```
    /// FIXME: Rustc wraps the condition in [`DropTemps`] -- a construct equivalent to
    /// `{ let _t = <cond>; _t }` -- to preserve drop semantics.
    /// We should probably do the same in future.
    ///
    /// [`DropTemps`]: https://doc.rust-lang.org/nightly/nightly-rustc/rustc_hir/hir/enum.ExprKind.html#variant.DropTemps
    fn collect_while_loop(&mut self, syntax_ptr: ExprPtr, e: ast::WhileExpr<'_>) -> ExprId {
        let label = e.label().map(|label| {
            (self.hygiene_id_for(label.syntax().text_range()), self.collect_label(label))
        });
        let body = self.collect_labelled_block_opt(label, e.loop_body());

        // Labels can also be used in the condition expression, like this:
        // ```
        // fn main() {
        //     let mut optional = Some(0);
        //     'my_label: while let Some(a) = match optional {
        //         None => break 'my_label,
        //         Some(val) => Some(val),
        //     } {
        //         println!("{}", a);
        //         optional = None;
        //     }
        // }
        // ```
        let condition = match label {
            Some((label_hygiene, label)) => self.with_labeled_rib(label, label_hygiene, |this| {
                this.collect_expr_opt(e.condition())
            }),
            None => self.collect_expr_opt(e.condition()),
        };

        let break_expr = self.alloc_expr(Expr::Break { expr: None, label: None }, syntax_ptr);
        let if_expr = self.alloc_expr(
            Expr::If { condition, then_branch: body, else_branch: Some(break_expr) },
            syntax_ptr,
        );
        self.alloc_expr(
            Expr::Loop { body: if_expr, label: label.map(|it| it.1), source: LoopSource::While },
            syntax_ptr,
        )
    }

    /// Desugar `ast::ForExpr` from: `[opt_ident]: for <pat> in <head> <body>` into:
    /// ```ignore (pseudo-rust)
    /// match IntoIterator::into_iter(<head>) {
    ///     mut iter => {
    ///         [opt_ident]: loop {
    ///             match Iterator::next(&mut iter) {
    ///                 None => break,
    ///                 Some(<pat>) => <body>,
    ///             };
    ///         }
    ///     }
    /// }
    /// ```
    fn collect_for_loop(&mut self, syntax_ptr: ExprPtr, e: ast::ForExpr<'_>) -> ExprId {
        let lang_items = self.lang_items();
        let (Some(into_iter_fn), Some(iter_next_fn), Some(option_some), Some(option_none)) = (
            self.lang_path(lang_items.IntoIterIntoIter),
            self.lang_path(lang_items.IteratorNext),
            self.lang_path(lang_items.OptionSome),
            self.lang_path(lang_items.OptionNone),
        ) else {
            return self.missing_expr();
        };
        let iterable = e.iterable();
        let syntax_ptr_iterable = iterable.map_or(syntax_ptr, ExprPtr::of);
        let loop_body = e.loop_body().map(ast::Expr::BlockExpr);
        let head = self.collect_expr_opt(iterable);
        let into_iter_fn_expr = self.store.alloc_path_expr(into_iter_fn);
        let into_iter_fn_expr = self.map_expr_desugared(into_iter_fn_expr, syntax_ptr_iterable);
        let iterator = self.alloc_expr_desugared_with_ptr(
            Expr::Call { callee: into_iter_fn_expr, args: &[head] },
            syntax_ptr_iterable,
        );
        let none_arm = MatchArm {
            pat: self.store.alloc_path_pat(option_none),
            guard: None,
            expr: self
                .alloc_expr_desugared_with_ptr(Expr::Break { expr: None, label: None }, syntax_ptr),
        };
        let some_pat_args = [self.collect_pat_top(e.pat())];
        let label = e.label().map(|label| {
            (self.hygiene_id_for(label.syntax().text_range()), self.collect_label(label))
        });
        let some_arm = MatchArm {
            pat: self.store.alloc_tuple_struct_pat(option_some, &some_pat_args, None),
            guard: None,
            expr: self.with_opt_labeled_rib(label, |this| this.collect_expr_opt(loop_body)),
        };
        let iter_name = self.generate_new_name();
        let iter_expr = self.store.alloc_name_path_expr(iter_name);
        let iter_expr = self.map_expr_desugared(iter_expr, syntax_ptr_iterable);
        let iter_expr_mut = self.alloc_expr_desugared_with_ptr(
            Expr::Ref { expr: iter_expr, rawness: Rawness::Ref, mutability: Mutability::Mut },
            syntax_ptr_iterable,
        );
        let iter_next_fn_expr = self.store.alloc_path_expr(iter_next_fn);
        let iter_next_fn_expr = self.map_expr_desugared(iter_next_fn_expr, syntax_ptr_iterable);
        let iter_next_expr = self.alloc_expr_desugared_with_ptr(
            Expr::Call { callee: iter_next_fn_expr, args: &[iter_expr_mut] },
            syntax_ptr_iterable,
        );
        let loop_inner = self.alloc_expr_desugared_with_ptr(
            Expr::Match { expr: iter_next_expr, arms: &[none_arm, some_arm] },
            syntax_ptr,
        );
        let loop_inner = self.alloc_expr_desugared_with_ptr(
            Expr::Block {
                id: None,
                statements: Statements::EMPTY,
                tail: Some(loop_inner),
                label: None,
                unsafe_: Unsafe::No,
            },
            syntax_ptr,
        );
        let loop_outer = self.alloc_expr_desugared_with_ptr(
            Expr::Loop {
                body: loop_inner,
                label: label.map(|it| it.1),
                source: LoopSource::ForLoop,
            },
            syntax_ptr,
        );
        let iter_binding =
            self.alloc_binding(iter_name, BindingAnnotation::Mutable, HygieneId::ROOT);
        let iter_pat = self.alloc_pat_desugared(Pat::Bind { id: iter_binding, subpat: None });
        self.add_definition_to_binding(iter_binding, iter_pat);
        self.alloc_expr(
            Expr::Match {
                expr: iterator,
                arms: &[MatchArm { pat: iter_pat, guard: None, expr: loop_outer }],
            },
            syntax_ptr,
        )
    }

    /// Desugar `ast::TryExpr` from: `<expr>?` into:
    /// ```ignore (pseudo-rust)
    /// match Try::branch(<expr>) {
    ///     ControlFlow::Continue(val) => val,
    ///     ControlFlow::Break(residual) =>
    ///         // If there is an enclosing `try {...}`:
    ///         break 'catch_target Residual::into_try_type(residual),
    ///         // If there is an enclosing `try bikeshed Ty {...}`:
    ///         break 'catch_target Try::from_residual(residual),
    ///         // Otherwise:
    ///         return Try::from_residual(residual),
    /// }
    /// ```
    fn collect_try_operator(&mut self, syntax_ptr: ExprPtr, e: ast::TryExpr<'_>) -> ExprId {
        let lang_items = self.lang_items();
        let (Some(try_branch), Some(cf_continue), Some(cf_break)) = (
            self.lang_path(lang_items.TryTraitBranch),
            self.lang_path(lang_items.ControlFlowContinue),
            self.lang_path(lang_items.ControlFlowBreak),
        ) else {
            return self.missing_expr();
        };
        let operand = self.collect_expr_opt(e.expr());
        let try_branch = self.store.alloc_path_expr(try_branch);
        let try_branch = self.map_expr(try_branch, syntax_ptr);
        let expr =
            self.alloc_expr(Expr::Call { callee: try_branch, args: &[operand] }, syntax_ptr);
        let continue_name = self.generate_new_name();
        let continue_binding = self.alloc_binding(
            continue_name,
            BindingAnnotation::Unannotated,
            HygieneId::ROOT,
        );
        let continue_bpat =
            self.alloc_pat_desugared(Pat::Bind { id: continue_binding, subpat: None });
        self.add_definition_to_binding(continue_binding, continue_bpat);
        let continue_arm = MatchArm {
            pat: self.store.alloc_tuple_struct_pat(cf_continue, &[continue_bpat], None),
            guard: None,
            expr: {
                let it = self.store.alloc_name_path_expr(continue_name);
                self.map_expr(it, syntax_ptr)
            },
        };
        let break_name = self.generate_new_name();
        let break_binding =
            self.alloc_binding(break_name, BindingAnnotation::Unannotated, HygieneId::ROOT);
        let break_bpat = self.alloc_pat_desugared(Pat::Bind { id: break_binding, subpat: None });
        self.add_definition_to_binding(break_binding, break_bpat);
        let break_arm = MatchArm {
            pat: self.store.alloc_tuple_struct_pat(cf_break, &[break_bpat], None),
            guard: None,
            expr: {
                let it = self.store.alloc_name_path_expr(break_name);
                let it = self.map_expr(it, syntax_ptr);
                let convert_fn = match self.current_try_block {
                    Some(TryBlock::Homogeneous { .. }) => {
                        self.lang_path(lang_items.ResidualIntoTryType)
                    }
                    Some(TryBlock::Heterogeneous { .. }) | None => {
                        self.lang_path(lang_items.TryTraitFromResidual)
                    }
                };
                let callee = self.alloc_opt_path_expr(convert_fn);
                let callee = self.map_expr(callee, syntax_ptr);
                let result = self.alloc_expr(Expr::Call { callee, args: &[it] }, syntax_ptr);
                self.alloc_expr(
                    match self.current_try_block {
                        Some(
                            TryBlock::Heterogeneous { label } | TryBlock::Homogeneous { label },
                        ) => Expr::Break { expr: Some(result), label: Some(label) },
                        None => Expr::Return { expr: Some(result) },
                    },
                    syntax_ptr,
                )
            },
        };
        let arms = [continue_arm, break_arm];
        self.alloc_expr(Expr::Match { expr, arms: &arms }, syntax_ptr)
    }

    /// The literal with the path of the module being lowered where `module_path!()` left its
    /// placeholder, when the literal has one: alone, or in what `concat!` made of it.
    fn module_path_literal(&self, literal: ast::Literal<'_>) -> Option<Literal> {
        self.expander.current_file_id().macro_file()?;
        let ast::LiteralKind::String(text) = literal.kind() else { return None };
        let text = text.value().ok()?;
        if !text.contains(MODULE_PATH_PLACEHOLDER) {
            return None;
        }
        let path = self.module_path()?;
        Some(Literal::String(Symbol::intern(&text.replace(MODULE_PATH_PLACEHOLDER, &path))))
    }

    /// The path of the module being lowered, as `module_path!()` gives it.
    fn module_path(&self) -> Option<String> {
        let mut module = self.module;
        let mut segments = SmallVec::<[&'static str; 8]>::new();
        while let Some(parent) = module.containing_module(self.db) {
            if !module.is_block_module(self.db) {
                segments.push(module.name(self.db)?.symbol().as_str());
            }
            module = parent.to_static();
        }
        let krate = self.krate.extra_data(self.db).display_name.as_ref()?;
        segments.push(krate.crate_name().symbol().as_str());
        segments.reverse();
        Some(segments.join("::"))
    }

    /// Expands `mcall` and hands `collector` the expansion's root as a `T`, a view of the
    /// expansion's tree, which the database keeps (`'db`).
    fn collect_macro_call<T, U>(
        &mut self,
        mcall: ast::MacroCall<'_>,
        syntax_ptr: MacroCallPtr,
        record_diagnostics: bool,
        collector: impl FnOnce(&mut Self, Option<T>) -> U,
    ) -> U
    where
        T: ast::AstNode<'db>,
    {
        let macro_call_ptr = self.expander.in_file(syntax_ptr);

        let block_call = self.def_map.module(self.db, self.module).scope(self.db).macro_invoc(
            self.expander.in_file(self.expander.ast_id_map().ast_id_for_ptr(syntax_ptr)),
        );
        let res = match block_call {
            // fast path, macro call is in a block module
            Some(call) => Ok(self.expander.enter_expand_id(self.db, call)),
            None => {
                let resolver = |path: &ModPath, ctx: SyntaxContext| {
                    // The latest `macro_rules!` of that name before the call, which is not
                    // the last one of the block when a later statement defines it again.
                    let shadowing = path.as_ident().and_then(|name| {
                        let seen = *self.current_block_legacy_macro_defs_count.get(name)?;
                        let defs = self
                            .def_map
                            .module(self.db, self.def_map.root)
                            .legacy_macro_in_scope(self.db, name)?;
                        defs.get(seen.checked_sub(1)?).copied()
                    });
                    if shadowing.is_some() {
                        return shadowing;
                    }
                    self.def_map
                        .resolve_path(
                            self.local_def_map,
                            self.db,
                            self.module,
                            path,
                            crate::item_scope::BuiltinShadowMode::Other,
                            Some(MacroSubNs::Bang),
                        )
                        .0
                        .take_macros()
                        .or_else(|| {
                            let module = macro_def_site(self.db, HygieneId::new(ctx))?;
                            let resolver = module.resolver(self.db);
                            let found = resolver.resolve_path_as_macro(
                                self.db,
                                path,
                                Some(MacroSubNs::Bang),
                            )?;
                            Some(found.def)
                        })
                };
                // alloc: none unless the call is eager and its input holds macro calls
                let mut eager_calls = Vec::new();
                let res = self.expander.enter_expand(
                    self.db,
                    mcall,
                    self.krate,
                    resolver,
                    &mut eager_calls,
                );
                for (ptr, call) in eager_calls {
                    self.store.macro_calls.push(call);
                    if self.store.record {
                        self.store.expansions.push((ptr.map(|(it, _)| it), call));
                    }
                }
                res
            }
        };

        let res = match res {
            Ok(res) => res,
            Err(UnresolvedMacro { path }) => {
                if record_diagnostics {
                    self.store.diagnostics.push(ExpressionStoreDiagnostics::UnresolvedMacroCall {
                        node: self.expander.in_file(syntax_ptr),
                        path,
                    });
                }
                return collector(self, None);
            }
        };
        // No need to push macro and parsing errors as they'll be recreated from `macro_calls()`.

        match res.value {
            Some((mark, expansion)) => {
                // Keep collecting even with expansion errors so we can provide completions and
                // other services in incomplete macro expressions.
                if let Some(macro_file) = self.expander.current_file_id().macro_file() {
                    self.store.macro_calls.push(macro_file);
                    if self.store.record {
                        self.store.expansions.push((macro_call_ptr, macro_file));
                    }
                }

                let id = collector(self, expansion);
                self.expander.exit(mark);
                id
            }
            None => collector(self, None),
        }
    }

    /// `in_expr_place` is whether rustc parses a call there in parentheses or brackets as an
    /// expression (`parse_stmt_mac`): with no `;` after it. The last call of a block is one
    /// ([`hir_expand::ExpandTo::of_call_site_view`]) and expands to an expression.
    fn collect_macro_as_stmt(
        &mut self,
        mac: ast::MacroExpr<'_>,
        in_expr_place: bool,
    ) -> Option<ExprId> {
        let mac_call = mac.macro_call()?;
        if hir_expand::ExpandTo::of_call_site_view(mac_call) == Ok(hir_expand::ExpandTo::Expr) {
            return self.maybe_collect_expr(ast::Expr::MacroExpr(mac));
        }
        // `MacroExpr` alone has no `HasAttrs`.
        if !self.check_cfg(ast::Expr::MacroExpr(mac)) {
            return None;
        }

        let as_expr =
            in_expr_place && mac_call.token_tree().is_some_and(|it| it.l_curly_token().is_none());
        let syntax_ptr = ExprPtr::of(ast::Expr::MacroExpr(mac));
        let macro_ptr = MacroCallPtr::of(mac_call);
        let expansion = self.collect_macro_call(
            mac_call,
            macro_ptr,
            false,
            |this, expansion: Option<ast::MacroStmts<'_>>| match expansion {
                Some(expansion)
                    if this
                        .expander
                        .current_file_id()
                        .macro_file()
                        .is_some_and(|it| it.is_include_macro(this.db))
                        && let Some(ast::Stmt::ExprStmt(first)) = expansion.statements().next()
                        && let Some(expr) = first.expr() =>
                {
                    // rustc reads one expression of an included file wherever the call is
                    // (`ExpandInclude::make_expr`), and lints what follows it
                    // (`incomplete_include`): `include!("f.rs");` of `1 2` is `1;`.
                    this.maybe_collect_expr(expr)
                }
                Some(expansion) => {
                    // rustc parses the expansion of such a call as one expression, and takes
                    // one `;` after it (`ParserAnyMacro::make`, the lint
                    // `semicolon_in_expressions_from_macros`): `e;` is the value `e`.
                    if as_expr
                        && expansion.expr().is_none()
                        && let mut written = expansion.statements()
                        && let (Some(ast::Stmt::ExprStmt(only)), None) =
                            (written.next(), written.next())
                        && only.semicolon_token().is_some()
                        && let Some(expr) = only.expr()
                    {
                        return this.maybe_collect_expr(expr);
                    }
                    expansion.statements().for_each(|stmt| this.collect_stmt(stmt));
                    expansion.expr().and_then(|expr| match expr {
                        ast::Expr::MacroExpr(mac) => {
                            this.collect_macro_as_stmt(mac, as_expr)
                        }
                        expr => this.maybe_collect_expr(expr),
                    })
                }
                // What does not expand is not nothing: a block that ends in it has its type,
                // not `()`.
                None => Some(this.alloc_expr(Expr::Missing, syntax_ptr)),
            },
        );
        expansion.inspect(|&tail| {
            // Make the macro-call point to its expanded expression so we can query
            // semantics on syntax pointers to the macro
            if self.store.record {
                let src = self.expander.in_file(syntax_ptr);
                self.store.expr_map.push((src, tail.into()));
            }
        })
    }

    fn collect_stmt(&mut self, s: ast::Stmt<'_>) {
        match s {
            ast::Stmt::LetStmt(stmt) => {
                if !self.check_cfg(stmt) {
                    return;
                }
                let pat = self.collect_pat_top(stmt.pat());
                let type_ref = stmt.ty().map(|it| self.lower_type_ref_disallow_impl_trait(it));
                let initializer = stmt.initializer().map(|e| self.collect_expr(e));
                let else_branch = stmt
                    .let_else()
                    .and_then(|let_else| let_else.block_expr())
                    .map(|block| self.collect_block(block));
                let is_super = stmt.super_token().is_some();
                self.store.push_list_statement(Statement::Let {
                    pat,
                    type_ref,
                    initializer,
                    else_branch,
                    is_super,
                });
            }
            ast::Stmt::ExprStmt(stmt) => {
                let expr = stmt.expr();
                let has_semi = stmt.semicolon_token().is_some();
                // Note that macro could be expanded to multiple statements
                if let Some(ast::Expr::MacroExpr(mac)) = expr {
                    if let Some(expr) = self.collect_macro_as_stmt(mac, !has_semi) {
                        self.store.push_list_statement(Statement::Expr { expr, has_semi })
                    }
                } else if let Some(expr) = expr.and_then(|expr| self.maybe_collect_expr(expr)) {
                    self.store.push_list_statement(Statement::Expr { expr, has_semi });
                }
            }
            ast::Stmt::Item(ast::Item::MacroDef(macro_)) => {
                if !self.check_cfg(macro_) {
                    return;
                }
                let Some(name) = macro_.name() else {
                    self.store.push_list_statement(Statement::Item(Item::Other));
                    return;
                };
                let name = name.as_name();
                let macro_id = self
                    .def_map
                    .module(self.db, self.def_map.root)
                    .name_in_scope(self.db, &name)
                    .take_macros();
                self.collect_macro_def(macro_id);
            }
            ast::Stmt::Item(ast::Item::MacroRules(macro_)) => {
                if !self.check_cfg(macro_) {
                    return;
                }
                let Some(name) = macro_.name() else {
                    self.store.push_list_statement(Statement::Item(Item::Other));
                    return;
                };
                let name = name.as_name();
                let macro_defs_count =
                    self.current_block_legacy_macro_defs_count.entry(name).or_insert(0);
                let macro_id = self
                    .def_map
                    .module(self.db, self.def_map.root)
                    .legacy_macro_in_scope(self.db, &name)
                    .and_then(|it| it.get(*macro_defs_count))
                    .copied();
                *macro_defs_count += 1;
                self.collect_macro_def(macro_id);
            }
            ast::Stmt::Item(_item) => self.store.push_list_statement(Statement::Item(Item::Other)),
        }
    }

    fn collect_macro_def(&mut self, macro_id: Option<MacroId>) {
        let Some(macro_id) = macro_id else {
            unreachable!("the block's def map defines each macro definition its statements hold");
        };
        let macro_id = macro_id.definition(self.db);
        self.store.push_list_statement(Statement::Item(Item::MacroDef(&macro_id)));
        self.stacks.label_ribs.push(LabelRib::new(RibKind::MacroDef(macro_id)));
    }

    fn collect_block(&mut self, block: ast::BlockExpr<'_>) -> ExprId {
        self.collect_block_(block, |_, id, statements, tail| Expr::Block {
            id,
            statements,
            tail,
            label: None,
            unsafe_: Unsafe::No,
        })
    }

    fn collect_block_(
        &mut self,
        block: ast::BlockExpr<'_>,
        mk_block: impl for<'s> FnOnce(
            &mut Self,
            Option<BlockId>,
            Statements<'s>,
            Option<ExprId>,
        ) -> Expr<'s>,
    ) -> ExprId {
        let block_id = (|| {
            let token = self.lowering_mode.allow_tracked_structs()?;
            let file_local_id = self.expander.ast_id_map().ast_id_for_block_view(block)?;
            let ast_id = self.expander.in_file(file_local_id);
            if let Some(given) = &mut self.given_blocks {
                let block =
                    *given.next().expect("the body made more blocks than its first lowering");
                assert_eq!(
                    block.ast_id(self.db),
                    ast_id,
                    "the body's blocks are not in the order of its first lowering"
                );
                return Some(block);
            }
            // The id goes only into this lowering query's result (`block_scopes`, the block's
            // `Expr::Block`) and the source map of the same body, invalidated with it.
            Some(BlockIdLt::new(self.db, ast_id, self.module, self.block_owner, token).to_static())
        })();

        let (module, def_map) =
            match block_id.map(|block_id| (block_def_map(self.db, block_id), block_id)) {
                Some((def_map, block_id)) => {
                    self.store.block_scopes.push(block_id);
                    (def_map.root_module_id(), def_map)
                }
                None => (self.module, self.def_map),
            };
        let prev_def_map = mem::replace(&mut self.def_map, def_map);
        let prev_local_module = mem::replace(&mut self.module, module);
        // A block without items of its own is still in the scope the count is of.
        let prev_legacy_macros_count = match block_id {
            Some(_) => mem::take(&mut self.current_block_legacy_macro_defs_count),
            // clone: empty, so free, unless a block-local `macro_rules!` is in scope.
            None => self.current_block_legacy_macro_defs_count.clone(),
        };

        let statements = self.store.list_mark();
        block.statements().for_each(|s| self.collect_stmt(s));
        let tail = block.tail_expr().and_then(|e| match e {
            ast::Expr::MacroExpr(mac) => self.collect_macro_as_stmt(mac, true),
            expr => self.maybe_collect_expr(expr),
        });
        let tail = tail.or_else(|| self.store.pop_list_tail_expr(statements));

        let written_in = block
            .stmt_list()
            .and_then(|it| it.l_curly_token())
            .map(|it| self.expander.ctx_for_range(it.text_range()))
            .filter(|it| !it.is_root());
        let syntax_node_ptr = ExprPtr::of(ast::Expr::BlockExpr(block));
        let expr_id = self.with_lists(statements, |this, lists| {
            let expr = mk_block(this, block_id, lists.statements_since(statements), tail);
            this.alloc_expr(expr, syntax_node_ptr)
        });
        if let Some(written_in) = written_in {
            self.store.block_editions.insert(expr_id, written_in.edition(self.db));
        }

        self.def_map = prev_def_map;
        self.module = prev_local_module;
        self.current_block_legacy_macro_defs_count = prev_legacy_macros_count;
        expr_id
    }

    fn collect_block_opt(&mut self, expr: Option<ast::BlockExpr<'_>>) -> ExprId {
        match expr {
            Some(block) => self.collect_block(block),
            None => self.missing_expr(),
        }
    }

    fn collect_labelled_block_opt(
        &mut self,
        label: Option<(HygieneId, LabelId)>,
        expr: Option<ast::BlockExpr<'_>>,
    ) -> ExprId {
        match label {
            Some((hygiene, label)) => {
                self.with_labeled_rib(label, hygiene, |this| this.collect_block_opt(expr))
            }
            None => self.collect_block_opt(expr),
        }
    }

    fn collect_param_as_ident(&mut self, pat: ast::Pat<'_>) -> PatId {
        // parameters of functions in `extern` blocks and associated trait functions without a body
        // can only be simple identifiers and wildcards.
        // Furthermore, the identifiers in their parameters are always interpreted as bindings, even
        // if in a normal function they won't be, because they would refer to a path pattern.

        match pat {
            ast::Pat::IdentPat(bp) if bp.is_simple_ident() => {
                let name = bp.name().map(|nr| nr.as_name()).unwrap_or_else(Name::missing);
                let hygiene = bp
                    .name()
                    .map(|name| self.hygiene_id_for(name.syntax().text_range()))
                    .unwrap_or(HygieneId::ROOT);
                let binding = self.alloc_binding(name, BindingAnnotation::Unannotated, hygiene);
                let pat = self.alloc_pat(Pat::Bind { id: binding, subpat: None }, PatPtr::of(pat));
                self.add_definition_to_binding(binding, pat);
                pat
            }
            ast::Pat::WildcardPat(_) => self.alloc_pat(Pat::Wild, PatPtr::of(pat)),
            ast::Pat::MacroPat(mac) => {
                self.collect_macro_pat_with(mac, |this, pat| this.collect_param_as_ident(pat))
            }
            _ => {
                self.store.diagnostics.push(ExpressionStoreDiagnostics::PatternArgInExternFn {
                    node: self.expander.in_file(PatPtr::of(pat)),
                });
                self.missing_pat()
            }
        }
    }

    // region: patterns

    fn collect_pat_top(&mut self, pat: Option<ast::Pat<'_>>) -> PatId {
        match pat {
            Some(pat) => self.collect_pat(pat, &mut BindingList::default()),
            None => self.missing_pat(),
        }
    }

    fn collect_pat(&mut self, pat: ast::Pat<'_>, binding_list: &mut BindingList) -> PatId {
        let ptr = PatPtr::of(pat);
        let pattern = match pat {
            ast::Pat::IdentPat(bp) => {
                let name = bp.name().map(|nr| nr.as_name()).unwrap_or_else(Name::missing);
                let hygiene = bp
                    .name()
                    .map(|name| self.hygiene_id_for(name.syntax().text_range()))
                    .unwrap_or(HygieneId::ROOT);

                let annotation =
                    BindingAnnotation::new(bp.mut_token().is_some(), bp.ref_token().is_some());
                let subpat = bp.pat().map(|subpat| self.collect_pat(subpat, binding_list));

                let is_simple_ident_pat =
                    annotation == BindingAnnotation::Unannotated && subpat.is_none();
                let is_path = is_simple_ident_pat && {
                    // This could also be a single-segment path pattern. To
                    // decide that, we need to try resolving the name.
                    let (resolved, _) = self.def_map.resolve_path(
                        self.local_def_map,
                        self.db,
                        self.module,
                        &name.into(),
                        BuiltinShadowMode::Other,
                        None,
                    );
                    // Funnily enough, record structs/variants *can* be shadowed
                    // by pattern bindings (but unit or tuple structs/variants
                    // can't).
                    // Shadowing statics is an error as well, so we just ignore that case here.
                    matches!(
                        resolved.take_values(),
                        Some(
                            ModuleDefId::ConstId(_)
                                | ModuleDefId::EnumVariantId(_)
                                | ModuleDefId::AdtId(AdtId::StructId(_))
                        )
                    )
                };
                if is_path {
                    let id = self.store.alloc_name_path_pat(name);
                    return self.map_pat(id, ptr);
                }

                let id = binding_list.find(self, name, hygiene, annotation);
                let pat = self.alloc_pat(Pat::Bind { id, subpat }, ptr);
                self.add_definition_to_binding(id, pat);
                return pat;
            }
            ast::Pat::TupleStructPat(p) => {
                let path = p
                    .path()
                    .and_then(|path| self.lower_path_view(path, &mut ImplTraitLowering::Error));
                let Some(path) = path else {
                    return self.missing_pat();
                };
                let (mark, ellipsis) = self.collect_tuple_pat(
                    p.fields(),
                    comma_follows_token(p.l_paren_token()),
                    binding_list,
                );
                let id = self.with_lists(mark, |this, lists| {
                    this.store.alloc_tuple_struct_pat(path, lists.pats_since(mark), ellipsis)
                });
                return self.map_pat(id, ptr);
            }
            ast::Pat::RefPat(p) => {
                let pat = self.collect_pat_opt(p.pat(), binding_list);
                let mutability = Mutability::from_mutable(p.mut_token().is_some());
                Pat::Ref { pat, mutability }
            }
            ast::Pat::PathPat(p) => {
                let path = p
                    .path()
                    .and_then(|path| self.lower_path_view(path, &mut ImplTraitLowering::Error));
                let id = match path {
                    Some(path) => self.store.alloc_path_pat(path),
                    None => self.store.alloc_pat(Pat::Missing),
                };
                return self.map_pat(id, ptr);
            }
            ast::Pat::OrPat(p) => {
                let prev_is_used = mem::take(&mut binding_list.is_used);
                let prev_reject_new = mem::take(&mut binding_list.reject_new);
                let mut it = p.pats();
                let Some(first) = it.next() else {
                    return self.alloc_pat(Pat::Or(&[]), ptr);
                };
                let mark = self.store.list_mark();
                binding_list.push(PatBoundCtx::Or);
                binding_list.push(PatBoundCtx::Product);
                let pat = self.collect_pat(first, binding_list);
                self.store.push_list_pat(pat);
                binding_list.pop();
                binding_list.reject_new = true;
                for rest in it {
                    for it in binding_list.is_used.values_mut() {
                        *it = false;
                    }
                    binding_list.push(PatBoundCtx::Product);
                    let pat = self.collect_pat(rest, binding_list);
                    self.store.push_list_pat(pat);
                    binding_list.pop();
                    for (&id, &is_used) in binding_list.is_used.iter() {
                        if !is_used {
                            self.store.bindings[id].problems =
                                Some(BindingProblems::NotBoundAcrossAll);
                        }
                    }
                }
                binding_list.pop();
                binding_list.reject_new = prev_reject_new;
                let current_is_used = mem::replace(&mut binding_list.is_used, prev_is_used);
                for (id, _) in current_is_used.into_iter() {
                    binding_list.check_is_used(self, id);
                }
                if let &[pat] = self.store.pats_since(mark) {
                    // Leading pipe without real OR pattern. Leaving an one-item OR pattern may confuse later stages.
                    self.store.truncate_to(mark);
                    return pat;
                }
                return self.with_lists(mark, |this, lists| {
                    this.alloc_pat(Pat::Or(lists.pats_since(mark)), ptr)
                });
            }
            ast::Pat::ParenPat(p) => return self.collect_pat_opt(p.pat(), binding_list),
            ast::Pat::TuplePat(p) => {
                let (mark, ellipsis) = self.collect_tuple_pat(
                    p.fields(),
                    comma_follows_token(p.l_paren_token()),
                    binding_list,
                );
                return self.with_lists(mark, |this, lists| {
                    this.alloc_pat(Pat::Tuple { args: lists.pats_since(mark), ellipsis }, ptr)
                });
            }
            ast::Pat::WildcardPat(_) => Pat::Wild,
            ast::Pat::RecordPat(p) => {
                let path = p
                    .path()
                    .and_then(|path| self.lower_path_view(path, &mut ImplTraitLowering::Error));
                let Some(path) = path else {
                    return self.missing_pat();
                };
                let record_pat_field_list =
                    p.record_pat_field_list().expect("every struct should have a field list");
                let mark = self.store.list_mark();
                for f in record_pat_field_list.fields() {
                    if !self.check_cfg(f) {
                        continue;
                    }
                    let Some(ast_pat) = f.pat() else { continue };
                    let pat = self.collect_pat(ast_pat, binding_list);
                    let Some(name) = f.field_name() else { continue };
                    let name = name.as_name();
                    if self.store.record {
                        let src = self
                            .expander
                            .in_file(AstPtr::<tag::RecordPatField>::of(f).wrap_right());
                        self.store.pat_field_map_back.push((pat, src));
                    }
                    self.store.push_list_field_pat(RecordFieldPat { name, pat });
                }

                let ellipsis = record_pat_field_list.rest_pat().is_some();

                let id = self.with_lists(mark, |this, lists| {
                    this.store.alloc_record_pat(path, lists.field_pats_since(mark), ellipsis)
                });
                return self.map_pat(id, ptr);
            }
            ast::Pat::SlicePat(p) => {
                let SlicePatComponents { prefix, slice, suffix } = p.components();

                let mark = self.store.list_mark();
                for p in prefix {
                    let pat = self.collect_pat(p, binding_list);
                    self.store.push_list_pat(pat);
                }
                let prefix_len = self.store.pats_since(mark).len();
                let slice = slice.map(|p| self.collect_pat(p, binding_list));
                for p in suffix {
                    let pat = self.collect_pat(p, binding_list);
                    self.store.push_list_pat(pat);
                }
                return self.with_lists(mark, |this, lists| {
                    let (prefix, suffix) = lists.pats_since(mark).split_at(prefix_len);
                    this.alloc_pat(Pat::Slice { prefix, slice, suffix }, ptr)
                });
            }
            ast::Pat::LiteralPat(lit) => 'b: {
                let Some((hir_lit, ast_lit)) = self.pat_literal_to_hir(lit) else {
                    break 'b Pat::Missing;
                };
                let expr_ptr = ExprPtr::of(ast::Expr::Literal(ast_lit));
                let expr_id = self.store.alloc_literal_expr(hir_lit);
                Pat::Lit(self.map_expr(expr_id, expr_ptr))
            }
            ast::Pat::RestPat(_) => Pat::Rest,
            ast::Pat::BoxPat(boxpat) => {
                let inner = self.collect_pat_opt(boxpat.pat(), binding_list);
                Pat::Box { inner }
            }
            ast::Pat::DerefPat(inner) => {
                let inner = self.collect_pat_opt(inner.pat(), binding_list);
                Pat::Deref { inner }
            }
            ast::Pat::NotNull(_) => Pat::NotNull,
            ast::Pat::ConstBlockPat(_) | ast::Pat::GuardPat(_) => Pat::Missing,
            ast::Pat::CfgPredPat(p) => {
                let enabled = Literal::Bool(self.check_cfg_pred(p.cfg_predicate()));
                let expr_id = self.store.alloc_literal_expr(enabled);
                Pat::Lit(self.map_expr_from_pat(expr_id, ptr))
            }
            ast::Pat::MacroPat(mac) => {
                return self.collect_macro_pat_with(mac, |this, expanded_pat| {
                    this.collect_pat(expanded_pat, binding_list)
                });
            }
            ast::Pat::RangePat(p) => {
                let mut range_part_lower = |p: Option<ast::Pat<'_>>| -> Option<ExprId> {
                    p.and_then(|it| {
                        let ptr = PatPtr::of(it);
                        match it {
                            ast::Pat::LiteralPat(it) => {
                                let (literal, _) = self.pat_literal_to_hir(it)?;
                                let id = self.store.alloc_literal_expr(literal);
                                Some(self.map_expr_from_pat(id, ptr))
                            }
                            ast::Pat::IdentPat(ident) if ident.is_simple_ident() => {
                                ident.name().map(|name| {
                                    let id = self.store.alloc_name_path_expr(name.as_name());
                                    self.map_expr_from_pat(id, ptr)
                                })
                            }
                            ast::Pat::PathPat(p) => p
                                .path()
                                .and_then(|path| {
                                    self.lower_path_view(path, &mut ImplTraitLowering::Error)
                                })
                                .map(|parsed| {
                                    let id = self.store.alloc_path_expr(parsed);
                                    self.map_expr_from_pat(id, ptr)
                                }),
                            // We only need to handle literal, ident (if bare) and path patterns here,
                            // as any other pattern as a range pattern operand is semantically invalid.
                            _ => None,
                        }
                    })
                };
                let start = range_part_lower(p.start());
                let end = range_part_lower(p.end());
                match p.op_kind() {
                    Some(range_type) => Pat::Range { start, end, range_type },
                    None => Pat::Missing,
                }
            }
        };
        self.alloc_pat(pattern, ptr)
    }

    fn collect_macro_pat_with(
        &mut self,
        mac: ast::MacroPat<'_>,
        callback: impl FnOnce(&mut Self, ast::Pat<'db>) -> PatId,
    ) -> PatId {
        match mac.macro_call() {
            Some(call) => {
                let macro_ptr = MacroCallPtr::of(call);
                let src = self.expander.in_file(PatPtr::of(ast::Pat::MacroPat(mac)));
                let pat = self.collect_macro_call(call, macro_ptr, true, |this, expanded_pat| {
                    match expanded_pat {
                        Some(pat) => callback(this, pat),
                        None => this.missing_pat(),
                    }
                });
                if self.store.record {
                    self.store.pat_map.push((src, pat.into()));
                }
                pat
            }
            None => self.missing_pat(),
        }
    }

    fn collect_pat_opt(
        &mut self,
        pat: Option<ast::Pat<'_>>,
        binding_list: &mut BindingList,
    ) -> PatId {
        match pat {
            Some(pat) => self.collect_pat(pat, binding_list),
            None => self.missing_pat(),
        }
    }

    fn collect_tuple_pat(
        &mut self,
        args: AstChildren<'_, ast::Pat<'_>>,
        has_leading_comma: bool,
        binding_list: &mut BindingList,
    ) -> (ListMark, Option<u32>) {
        let mark = self.store.list_mark();
        // The location of the `..`, if there is one. Note that we do not
        // consider the possibility of there being multiple `..` here.
        let mut ellipsis = None;
        for (idx, p) in args.enumerate() {
            match self.collect_pat_possibly_rest(p, binding_list) {
                Either::Left(pat) => self.store.push_list_pat(pat),
                // We want to skip the `..` pattern here, since we account for it above.
                Either::Right(()) => {
                    if ellipsis.is_none() {
                        ellipsis = Some(idx as u32);
                    }
                }
            }
        }
        // if there is a leading comma, the user is most likely to type out a leading pattern
        // so we insert a missing pattern at the beginning for IDE features
        if has_leading_comma {
            let missing = self.missing_pat();
            self.store.push_list_pat(missing);
            let top = self.store.list_mark();
            let mut lists = self.store.take_lists();
            lists.pats_since_mut(mark).rotate_right(1);
            self.store.put_lists(lists, top);
        }

        (mark, ellipsis)
    }

    // `collect_pat` rejects `ast::Pat::RestPat`, but it should be handled in some cases that
    // it is the macro expansion result of an arg sub-pattern in a slice or tuple pattern.
    fn collect_pat_possibly_rest(
        &mut self,
        pat: ast::Pat<'_>,
        binding_list: &mut BindingList,
    ) -> Either<PatId, ()> {
        match pat {
            ast::Pat::RestPat(_) => Either::Right(()),
            ast::Pat::MacroPat(mac) => match mac.macro_call() {
                Some(call) => {
                    let macro_ptr = MacroCallPtr::of(call);
                    let src = self.expander.in_file(PatPtr::of(pat));
                    let pat =
                        self.collect_macro_call(call, macro_ptr, true, |this, expanded_pat| {
                            if let Some(expanded_pat) = expanded_pat {
                                this.collect_pat_possibly_rest(expanded_pat, binding_list)
                            } else {
                                Either::Left(this.missing_pat())
                            }
                        });
                    if let Some(pat) = pat.left()
                        && self.store.record
                    {
                        self.store.pat_map.push((src, pat.into()));
                    }
                    pat
                }
                None => {
                    let ptr = PatPtr::of(pat);
                    Either::Left(self.alloc_pat(Pat::Missing, ptr))
                }
            },
            _ => Either::Left(self.collect_pat(pat, binding_list)),
        }
    }

    fn collect_ty_pat_opt(&mut self, pat: Option<ast::Pat<'_>>) -> PatId {
        match pat {
            Some(pat) => self.collect_ty_pat(pat),
            None => self.missing_pat(),
        }
    }

    fn collect_ty_pat(&mut self, pat: ast::Pat<'_>) -> PatId {
        let ptr = PatPtr::of(pat);
        match pat {
            ast::Pat::NotNull(_) => self.alloc_pat(Pat::NotNull, ptr),
            ast::Pat::OrPat(pat) => {
                let mark = self.store.list_mark();
                for pat in pat.pats() {
                    let pat = self.collect_ty_pat(pat);
                    self.store.push_list_pat(pat);
                }
                self.with_lists(mark, |this, lists| {
                    this.alloc_pat(Pat::Or(lists.pats_since(mark)), ptr)
                })
            }
            ast::Pat::RangePat(range_pat) => {
                let start = range_pat
                    .start()
                    .map(|pat| {
                        self.with_fresh_binding_expr_root(|this| this.lower_ty_pat_range_side(pat))
                    })
                    .unwrap_or_else(|| self.lower_ty_pat_range_end(self.lang_items().RangeMin));
                let end = range_pat
                    .end()
                    .map(|pat| match range_pat.op_kind() {
                        Some(ast::RangeOp::Inclusive) | None => self
                            .with_fresh_binding_expr_root(|this| this.lower_ty_pat_range_side(pat)),
                        Some(ast::RangeOp::Exclusive) => self.lower_excluded_range_end(pat),
                    })
                    .unwrap_or_else(|| self.lower_ty_pat_range_end(self.lang_items().RangeMax));
                self.alloc_pat(
                    Pat::Range {
                        start: Some(start),
                        end: Some(end),
                        range_type: ast::RangeOp::Inclusive,
                    },
                    ptr,
                )
            }
            ast::Pat::MacroPat(pat) => {
                self.collect_macro_pat_with(pat, |this, pat| this.collect_ty_pat(pat))
            }
            // rustc's "pattern not supported in pattern types": the type lowering reports
            // anything but a range, an or-pattern and `!null`; `Missing` is a parse error's.
            _ => self.alloc_pat(Pat::Wild, ptr),
        }
    }

    fn lower_ty_pat_range_side(&mut self, pat: ast::Pat<'_>) -> ExprId {
        let ptr = PatPtr::of(pat);
        match pat {
            ast::Pat::LiteralPat(it) => {
                let Some((literal, _)) = self.pat_literal_to_hir(it) else {
                    return self.missing_expr();
                };
                let id = self.store.alloc_literal_expr(literal);
                self.map_expr_from_pat(id, ptr)
            }
            ast::Pat::PathPat(it) => {
                let path = it
                    .path()
                    .and_then(|path| self.lower_path_view(path, &mut ImplTraitLowering::Error));
                let id = self.alloc_opt_path_expr(path);
                self.map_expr_from_pat(id, ptr)
            }
            ast::Pat::IdentPat(it) if it.is_simple_ident() => {
                let name = it.name().map(|nr| nr.as_name()).unwrap_or_else(Name::missing);
                let id = self.store.alloc_name_path_expr(name);
                self.map_expr_from_pat(id, ptr)
            }
            _ => self.missing_expr(), // FIXME: Emit an error.
        }
    }

    /// When a range has no end specified (`1..` or `1..=`) or no start specified (`..5` or `..=5`),
    /// we instead use a constant of the MAX/MIN of the type.
    /// This way the type system does not have to handle the lack of a start/end.
    fn lower_ty_pat_range_end(&mut self, lang_item: Option<ConstId>) -> ExprId {
        self.with_fresh_binding_expr_root(|this| {
            let path = this.lang_path(lang_item);
            this.alloc_opt_path_expr(path)
        })
    }

    /// Lowers the range end of an exclusive range (`2..5`) to an inclusive range 2..=(5 - 1).
    /// This way the type system doesn't have to handle the distinction between inclusive/exclusive ranges.
    fn lower_excluded_range_end(&mut self, pat: ast::Pat<'_>) -> ExprId {
        self.with_fresh_binding_expr_root(|this| {
            let excluded_end = this.lower_ty_pat_range_side(pat);
            let range_sub_path = this.lang_path(this.lang_items().RangeSub);
            let range_sub_path = this.alloc_opt_path_expr(range_sub_path);
            this.alloc_expr_desugared(Expr::Call { callee: range_sub_path, args: &[excluded_end] })
        })
    }

    // endregion: patterns

    /// Returns `false` (and emits diagnostics) when `owner` if `#[cfg]`d out, and `true` when
    /// not.
    #[inline]
    fn check_cfg<'t>(&mut self, owner: impl ast::HasAttrs<'t>) -> bool {
        if !may_have_attrs(&owner) {
            return true;
        }
        self.check_cfg_of(ast::AnyHasAttrs::new(owner))
    }

    /// [`Self::check_cfg`] of an owner that may have attributes: one instance for every kind
    /// of owner.
    fn check_cfg_of(&mut self, owner: ast::AnyHasAttrs<'_>) -> bool {
        let enabled = self.expander.is_cfg_enabled(owner, self.cfg_options);
        match enabled {
            Ok(()) => true,
            Err(cfg) => {
                self.store.diagnostics.push(ExpressionStoreDiagnostics::InactiveCode {
                    node: self.expander.in_file(ast::ptr(owner.syntax())),
                    cfg,
                });
                false
            }
        }
    }

    /// Used for `cfg!()`, conducting imprecise checks
    fn check_cfg_pred(&self, pred: Option<ast::CfgPredicate<'_>>) -> bool {
        let expr = pred.map_or(cfg::CfgExpr::Invalid, cfg::CfgExpr::parse_from_view);
        self.cfg_options.check(&expr) != Some(false)
    }

    fn add_definition_to_binding(&mut self, binding_id: BindingId, pat_id: PatId) {
        if self.store.record {
            self.store.binding_definitions.push((binding_id, pat_id));
        }
    }

    // region: labels

    fn collect_label(&mut self, ast_label: ast::Label<'_>) -> LabelId {
        let label = Label {
            name: ast_label
                .lifetime()
                .map_or_else(Name::missing, |lt| Name::new_lifetime(lt.text())),
        };
        self.alloc_label(label, AstPtr::<tag::Label>::of(ast_label))
    }

    fn resolve_label(
        &self,
        lifetime: Option<ast::Lifetime<'_>>,
    ) -> Result<Option<LabelId>, ExpressionStoreDiagnostics> {
        let Some(lifetime) = lifetime else { return Ok(None) };
        let mut hygiene_id =
            self.expander.hygiene_for_range(self.db, lifetime.syntax().text_range());
        let mut hygiene_info = if hygiene_id.is_root() {
            None
        } else {
            hygiene_id.syntax_context().outer_expn(self.db).map(|expansion| {
                let expansion = hir_expand::MacroCallId::from(expansion).loc(self.db);
                (hygiene_id.syntax_context().parent(self.db), expansion.def)
            })
        };
        let name = Name::new_lifetime(lifetime.text());

        for (rib_idx, rib) in self.stacks.label_ribs.iter().enumerate().rev() {
            match &rib.kind {
                RibKind::Normal(label_name, id, label_hygiene)
                    if *label_name == name && *label_hygiene == hygiene_id =>
                {
                    return if self.is_label_valid_from_rib(rib_idx) {
                        Ok(Some(*id))
                    } else {
                        Err(ExpressionStoreDiagnostics::UnreachableLabel {
                            name,
                            node: self.expander.in_file(LifetimePtr::of(lifetime)),
                        })
                    };
                }
                RibKind::MacroDef(macro_id) => {
                    if let Some((parent_ctx, label_macro_id)) = hygiene_info
                        && label_macro_id == *macro_id
                    {
                        // A macro is allowed to refer to labels from before its declaration.
                        // Therefore, if we got to the rib of its declaration, give up its hygiene
                        // and use its parent expansion.

                        hygiene_id = HygieneId::new(parent_ctx.opaque_and_semiopaque(self.db));
                        hygiene_info = parent_ctx.outer_expn(self.db).map(|expansion| {
                            let expansion = hir_expand::MacroCallId::from(expansion).loc(self.db);
                            (parent_ctx.parent(self.db), expansion.def)
                        });
                    }
                }
                _ => {}
            }
        }

        Err(ExpressionStoreDiagnostics::UndeclaredLabel {
            name,
            node: self.expander.in_file(LifetimePtr::of(lifetime)),
        })
    }

    fn is_label_valid_from_rib(&self, rib_index: usize) -> bool {
        !self.stacks.label_ribs[rib_index + 1..].iter().any(|rib| rib.kind.is_label_barrier())
    }

    fn pop_label_rib(&mut self) {
        // We need to pop all macro defs, plus one rib.
        while let Some(LabelRib { kind: RibKind::MacroDef(_) }) = self.stacks.label_ribs.pop() {
            // Do nothing.
        }
    }

    fn with_label_rib<T>(&mut self, kind: RibKind, f: impl FnOnce(&mut Self) -> T) -> T {
        self.stacks.label_ribs.push(LabelRib::new(kind));
        let res = f(self);
        self.pop_label_rib();
        res
    }

    fn with_labeled_rib<T>(
        &mut self,
        label: LabelId,
        hygiene: HygieneId,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        self.stacks.label_ribs.push(LabelRib::new(RibKind::Normal(
            self.store.labels[label].name,
            label,
            hygiene,
        )));
        let res = f(self);
        self.pop_label_rib();
        res
    }

    fn with_opt_labeled_rib<T>(
        &mut self,
        label: Option<(HygieneId, LabelId)>,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        match label {
            None => f(self),
            Some((hygiene, label)) => self.with_labeled_rib(label, hygiene, f),
        }
    }
    // endregion: labels

    /// While `expr` is a macro call, repeatedly expand it. If the end result is a string literal,
    /// return that, along with a boolean for whether it was a direct string literal, i.e. no macro
    /// calls were involved. In all other cases, return `None`.
    /// The string is of `expr`'s tree or of an expansion's, so it lives as long as both (`'v`).
    fn expand_macros_to_string<'t: 'v, 'v>(
        &mut self,
        expr: ast::Expr<'t>,
    ) -> Option<(ast::String<'v>, bool)>
    where
        'db: 'v,
    {
        let m = match expr {
            ast::Expr::MacroExpr(m) => m,
            // An expression a macro was given is in parentheses where the macro puts it.
            ast::Expr::ParenExpr(inner) => {
                let (text, _) = self.expand_macros_to_string(inner.expr()?)?;
                return Some((text, false));
            }
            ast::Expr::Literal(l) => {
                return match l.kind() {
                    ast::LiteralKind::String(s) => Some((s, true)),
                    _ => None,
                };
            }
            _ => return None,
        };
        let e = m.macro_call()?;
        let macro_ptr = MacroCallPtr::of(e);
        let (exp, _) =
            self.collect_macro_call(e, macro_ptr, true, |this, expansion: Option<ast::Expr<'db>>| {
                expansion.and_then(|it| this.expand_macros_to_string(it))
            })?;
        Some((exp, false))
    }

    fn lang_path(&mut self, lang: Option<impl Into<LangItemTarget>>) -> Option<PathIdx> {
        Some(self.store.push_lang_path(lang?.into(), None))
    }

    fn ty_rel_lang_path(
        &mut self,
        lang: Option<impl Into<LangItemTarget>>,
        relative_name: Symbol,
    ) -> Option<PathIdx> {
        Some(self.store.push_lang_path(lang?.into(), Some(Name::new_symbol_root(relative_name))))
    }
}

impl<'db> ExprCollector<'db> {
    fn pat_literal_to_hir<'t>(
        &mut self,
        lit: ast::LiteralPat<'t>,
    ) -> Option<(Literal, ast::Literal<'t>)> {
        let ast_lit = lit.literal()?;
        let mut hir_lit = self.lower_literal(ast_lit);
        if lit.minus_token().is_some() {
            hir_lit = hir_lit.negate()?;
        }
        Some((hir_lit, ast_lit))
    }

    /// `lit` as a value, reporting an integer whose digits do not fit `u128` (rustc's
    /// `LitError::IntTooLarge` from `lower_lit`; [`Literal::from`] gives it `0`).
    fn lower_literal(&mut self, lit: ast::Literal<'_>) -> Literal {
        let kind = lit.kind();
        if let ast::LiteralKind::IntNumber(int) = kind
            && int.suffix().and_then(crate::builtin_type::BuiltinFloat::from_suffix).is_none()
            && int.value().is_err_and(|it| *it.kind() == std::num::IntErrorKind::PosOverflow)
        {
            self.store.diagnostics.push(ExpressionStoreDiagnostics::IntLiteralTooLarge {
                node: self.expander.in_file(AstPtr::<tag::Literal>::of(lit)),
            });
        }
        kind.into()
    }
}

impl<'db> ExprCollector<'db> {
    fn features(&self) -> &'db UnstableFeatures {
        self.def_map.features()
    }

    /// An anonymous const's root (a const argument, an array type's length): a body of its own,
    /// never `async`.
    fn with_fresh_binding_expr_root(&mut self, f: impl FnOnce(&mut Self) -> ExprId) -> ExprId {
        self.with_expr_root(|this| {
            this.with_awaitable_block(Awaitable::No("constant"), |this| this.with_binding_owner(f))
        })
    }

    fn with_expr_root(&mut self, f: impl FnOnce(&mut Self) -> ExprId) -> ExprId {
        let inference_roots = self.store.inference_roots.take();
        let root = f(self);
        self.store.inference_roots = inference_roots;

        let exprs_end = end(self.store.expr_count());
        let pats_end = end(self.store.pat_count());
        let bindings_end = end(self.store.bindings.len());
        if let Some(inference_roots) = &mut self.store.inference_roots {
            inference_roots.push(ExprRoot { root, exprs_end, pats_end, bindings_end });
        }

        return root;

        fn end<T>(len: usize) -> la_arena::Idx<T> {
            la_arena::Idx::from_raw(la_arena::RawIdx::from_u32(len as u32))
        }
    }

    /// Runs `f` with the builder's list stacks moved out of it, so `f` can allocate a node whose
    /// lists are the ones above `mark` (`lists.exprs_since(mark)`, ...), then puts the stacks back
    /// truncated to `mark`. `f` may allocate nodes but must not push onto the stacks: what it
    /// pushed would be lost.
    fn with_lists<R>(&mut self, mark: ListMark, f: impl FnOnce(&mut Self, &mut Lists) -> R) -> R {
        let mut lists = self.store.take_lists();
        let result = f(self, &mut lists);
        self.store.put_lists(lists, mark);
        result
    }

    /// Lowers a call's arguments onto the expr stack, leaving out those `#[cfg]`d out.
    fn collect_args(&mut self, arg_list: Option<ast::ArgList<'_>>) -> ListMark {
        let mark = self.store.list_mark();
        for arg in arg_list.iter().flat_map(|it| it.args()) {
            if let Some(arg) = self.maybe_collect_expr(arg) {
                self.store.push_list_expr(arg);
            }
        }
        mark
    }

    /// Lowers `exprs` onto the expr stack, leaving out those `#[cfg]`d out.
    fn collect_exprs<'t>(&mut self, exprs: impl Iterator<Item = ast::Expr<'t>>) -> ListMark {
        let mark = self.store.list_mark();
        for expr in exprs {
            if let Some(expr) = self.maybe_collect_expr(expr) {
                self.store.push_list_expr(expr);
            }
        }
        mark
    }

    /// `Expr::Path` of the path `path` lowering pushed, or `Expr::Missing` without a path; with
    /// no source map.
    fn alloc_opt_path_expr(&mut self, path: Option<PathIdx>) -> ExprId {
        match path {
            Some(path) => self.store.alloc_path_expr(path),
            None => self.store.alloc_expr(Expr::Missing),
        }
    }

    fn alloc_expr(&mut self, expr: Expr<'_>, ptr: ExprPtr) -> ExprId {
        let id = self.store.alloc_expr(expr);
        self.map_expr(id, ptr)
    }
    /// The source maps of an expression just allocated for `ptr`.
    fn map_expr(&mut self, id: ExprId, ptr: ExprPtr) -> ExprId {
        if self.store.record {
            let src = self.expander.in_file(ptr);
            self.store.expr_map_back.insert(id, src.map(AstPtr::wrap_left));
            self.store.expr_map.push((src, id.into()));
        }
        id
    }
    // FIXME: desugared exprs don't have ptr, that's wrong and should be fixed.
    // Migrate to alloc_expr_desugared_with_ptr and then rename back
    fn alloc_expr_desugared(&mut self, expr: Expr<'_>) -> ExprId {
        self.store.alloc_expr(expr)
    }
    fn alloc_expr_desugared_with_ptr(&mut self, expr: Expr<'_>, ptr: ExprPtr) -> ExprId {
        let id = self.store.alloc_expr(expr);
        self.map_expr_desugared(id, ptr)
    }
    /// The back map of a desugared expression just allocated for `ptr`.
    fn map_expr_desugared(&mut self, id: ExprId, ptr: ExprPtr) -> ExprId {
        if self.store.record {
            let src = self.expander.in_file(ptr);
            self.store.expr_map_back.insert(id, src.map(AstPtr::wrap_left));
            // We intentionally don't fill this as it could overwrite a non-desugared entry
            // self.store.expr_map.push((src, id));
        }
        id
    }
    fn missing_expr(&mut self) -> ExprId {
        self.alloc_expr_desugared(Expr::Missing)
    }

    fn alloc_binding(
        &mut self,
        name: Name,
        mode: BindingAnnotation,
        hygiene: HygieneId,
    ) -> BindingId {
        let binding = self.store.bindings.alloc(Binding { name, mode, problems: None, hygiene });
        self.stacks.unowned_bindings.push(binding);
        binding
    }

    fn alloc_pat_from_expr(&mut self, pat: Pat<'_>, ptr: ExprPtr) -> PatId {
        let id = self.store.alloc_pat(pat);
        self.map_pat_from_expr(id, ptr)
    }
    /// The source maps of a pattern just allocated for the expression `ptr`.
    fn map_pat_from_expr(&mut self, id: PatId, ptr: ExprPtr) -> PatId {
        if self.store.record {
            let src = self.expander.in_file(ptr);
            self.store.expr_map.push((src, id.into()));
            self.store.pat_map_back.insert(id, src.map(AstPtr::wrap_left));
        }
        id
    }

    /// The source maps of an expression just allocated for the pattern `ptr`.
    fn map_expr_from_pat(&mut self, id: ExprId, ptr: PatPtr) -> ExprId {
        if self.store.record {
            let src = self.expander.in_file(ptr);
            self.store.pat_map.push((src, id.into()));
            self.store.expr_map_back.insert(id, src.map(AstPtr::wrap_right));
        }
        id
    }

    fn alloc_pat(&mut self, pat: Pat<'_>, ptr: PatPtr) -> PatId {
        let id = self.store.alloc_pat(pat);
        self.map_pat(id, ptr)
    }
    /// The source maps of a pattern just allocated for `ptr`.
    fn map_pat(&mut self, id: PatId, ptr: PatPtr) -> PatId {
        if self.store.record {
            let src = self.expander.in_file(ptr);
            self.store.pat_map_back.insert(id, src.map(AstPtr::wrap_right));
            self.store.pat_map.push((src, id.into()));
        }
        id
    }
    // FIXME: desugared pats don't have ptr, that's wrong and should be fixed somehow.
    fn alloc_pat_desugared(&mut self, pat: Pat<'_>) -> PatId {
        self.store.alloc_pat(pat)
    }
    fn missing_pat(&mut self) -> PatId {
        self.store.alloc_pat(Pat::Missing)
    }

    fn alloc_label(&mut self, label: Label, ptr: AstPtr<tag::Label>) -> LabelId {
        self.alloc_label_desugared(label, ptr.wrap_left())
    }

    fn alloc_label_desugared(&mut self, label: Label, ptr: LabelPtr) -> LabelId {
        let id = self.store.labels.alloc(label);
        if self.store.record {
            let src = self.expander.in_file(ptr);
            self.store.label_map_back.insert(id, src);
            self.store.label_map.push((src, id));
        }
        id
    }

    fn is_lowering_awaitable_block(&self) -> &Awaitable {
        self.awaitable_context.as_ref().unwrap_or(&Awaitable::No("unknown"))
    }

    fn with_awaitable_block<T>(
        &mut self,
        awaitable: Awaitable,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let orig = self.awaitable_context.replace(awaitable);
        let res = f(self);
        self.awaitable_context = orig;
        res
    }

    fn hygiene_id_for(&self, range: TextRange) -> HygieneId {
        self.expander.hygiene_for_range(self.db, range)
    }

    fn with_lifetime_bound_scope<T>(
        &mut self,
        bound_scope: LifetimeBoundScope,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let old = self.named_lifetime_store.lifetime_bound_scope.replace(bound_scope);
        let res = f(self);
        self.named_lifetime_store.lifetime_bound_scope = old;
        res
    }

    fn for_path_type_projection<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        if self.is_argument_lt_bound_scope() {
            let old = self.named_lifetime_store.lifetime_bound_scope.take();
            let res = f(self);
            self.named_lifetime_store.lifetime_bound_scope = old;
            res
        } else {
            f(self)
        }
    }

    fn push_named_target_lifetime(&mut self, id: LifetimeRefId) {
        if let LifetimeRef::Named(name) = &self.store.lifetimes[id] {
            self.named_lifetime_store.push_named_lifetime(*name);
        }
    }

    fn is_argument_lt_bound_scope(&mut self) -> bool {
        matches!(self.named_lifetime_store.lifetime_bound_scope, Some(LifetimeBoundScope::Argument))
    }

    fn get_constrained_lifetimes_if_type_alias<'a>(
        &mut self,
        mod_path: &'a ModPath,
        lifetime_args: Option<&'a [LifetimeRefId]>,
    ) -> Option<impl Iterator<Item = LifetimeRefId> + use<'a, 'db>> {
        let lifetime_args = lifetime_args?;
        let r_path = self.def_map.resolve_path(
            self.local_def_map,
            self.db,
            self.module,
            mod_path,
            BuiltinShadowMode::Module,
            None,
        );
        let ModuleDefId::TypeAliasId(id) = r_path.0.take_types()? else { return None };

        let constrained_lt_indices = get_constrained_lifetimes(self.db, id);
        let res = constrained_lt_indices
            .iter()
            .filter_map(|&idx| lifetime_args.get(idx as usize).copied());
        return Some(res);

        #[blaze_query::tracked(returns(deref), cycle_result = get_constrained_lifetimes_cycle_result)]
        fn get_constrained_lifetimes(
            db: &Db,
            type_alias_id: TypeAliasId,
        ) -> Box<[u32]> {
            let TypeAliasSignature { generic_params, store, ty, .. } =
                TypeAliasSignature::of(db, type_alias_id);
            let &Some(ty) = ty else { return Default::default() };

            let mut visitor = Visitor {
                store,
                generic_params,
                parent: type_alias_id,
                // alloc: the query's result, once per type alias.
                constrained_lt_indices: Vec::new(),
            };
            store.visit_type_ref_children(ty, &mut visitor);

            return visitor.constrained_lt_indices.into_boxed_slice();

            struct Visitor<'a> {
                store: &'a ExpressionStore,
                generic_params: &'a GenericParams,
                parent: TypeAliasId,
                constrained_lt_indices: Vec<u32>,
            }

            impl StoreVisitor for Visitor<'_> {
                fn on_lifetime(&mut self, lifetime: LifetimeRefId) {
                    if let LifetimeRef::Named(lifetime_name) = &self.store[lifetime]
                        && let Some(param_id) = self
                            .generic_params
                            .find_lifetime_by_name(lifetime_name, self.parent.into())
                    {
                        self.constrained_lt_indices.push(param_id.local_id.into_raw().into_u32());
                    }
                }

                fn on_generic_args(&mut self, args: GenericArgs<'_>) {
                    if !args.has_self_type {
                        crate::expr_store::visit_generic_args(self, args);
                    }
                }
            }
        }

        fn get_constrained_lifetimes_cycle_result(
            _db: &Db,
            _: blaze_query::db::Id,
            _id: TypeAliasId,
        ) -> Box<[u32]> {
            Default::default()
        }
    }
}

/// Whether a type path is a plain path with no anchor: the kind a `macro`'s hygiene is kept for.
fn is_plain_path(path: Path<'_>) -> bool {
    path.type_anchor().is_none() && path.mod_path().is_some_and(|it| it.kind == PathKind::Plain)
}

/// Whether `owner` may have an attribute of its own, outer or inner: false only when it surely
/// has none. Every `AnyAttr*` in `rust.ungram` leads its rule (after at most a `{` token), so a
/// node whose first child node is not an attribute has none, and so for the node holding its
/// inner attributes. One child read in place of a walk over every child, for the nodes almost
/// all of a body is.
#[inline]
pub(in crate::expr_store) fn may_have_attrs<'t>(owner: &impl ast::HasAttrs<'t>) -> bool {
    let leads =
        |node: ast::Node<'t>| node.first_child().is_some_and(|it| ast::AnyAttr::can_cast(it.kind()));
    leads(owner.syntax()) || owner.inner_attributes_node().is_some_and(leads)
}

/// The tree of the root of a handle's tree, for [`view_of`]: what the handle callers outside the
/// crate keep alive while their node is lowered as a view.
fn top_of<N: syntax::AstNode>(node: &N) -> Arc<FlatTree> {
    let root = node.syntax().ancestors().last().expect("a node is among its ancestors");
    // clone: one count of the tree per handle call, on the cold edge of a caller not on views.
    Arc::clone(root.flat().expect("every node is in a flat tree").0)
}

/// The view of `node` under `top`, its root's tree ([`top_of`]): the same node, found by its
/// pointer.
fn view_of<'t, N: syntax::AstNode + Typed>(node: &N, top: &'t Arc<FlatTree>) -> N::Of<'t> {
    AstPtr::new(node).to_view(ast::Node::root(top))
}

/// Whether the first token after `t` that is not trivia is a `,`.
pub(in crate::expr_store) fn comma_follows_token(t: Option<ast::Token<'_>>) -> bool {
    let mut next = t.and_then(|t| t.next_token());
    while let Some(token) = next {
        if !token.kind().is_trivia() {
            return token.kind() == syntax::T![,];
        }
        next = token.next_token();
    }
    false
}

/// The syntax of the HIR's `AssociatedTypeBinding` number `binding_idx` of a segment's arguments.
pub fn hir_assoc_type_binding_to_ast_view<'t>(
    segment_args: ast::GenericArgList<'t>,
    binding_idx: u32,
) -> Option<ast::AssocTypeArg<'t>> {
    segment_args
        .generic_args()
        .filter_map(|arg| match arg {
            ast::GenericArg::AssocTypeArg(it) => Some(it),
            _ => None,
        })
        .filter(|binding| binding.param_list().is_none() && binding.name_ref().is_some())
        .nth(binding_idx as usize)
}

/// The syntax of the HIR's generic argument number `arg_idx`. Does not support the `Self`
/// argument.
pub fn hir_generic_arg_to_ast_view<'t>(
    args: ast::GenericArgList<'t>,
    arg_idx: u32,
    has_self_arg: bool,
) -> Option<ast::GenericArg<'t>> {
    args.generic_args()
        .filter(|arg| match arg {
            ast::GenericArg::AssocTypeArg(_) => false,
            ast::GenericArg::LifetimeArg(arg) => arg.lifetime().is_some(),
            ast::GenericArg::ConstArg(_) | ast::GenericArg::TypeArg(_) => true,
        })
        .nth(arg_idx as usize - has_self_arg as usize)
}
