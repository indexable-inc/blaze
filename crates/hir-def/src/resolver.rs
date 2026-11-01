//! Name resolution façade.
use std::{cell::RefCell, fmt, iter, mem, ops::ControlFlow};

use base_db::{Crate, Db};
use blaze_hash::HashSet;
use blaze_mem::scratch::Scratch;
use hir_expand::{
    MacroDefId,
    mod_path::{ModPath, PathKind},
    name::{AsName, Name},
};
use intern::{Symbol, sym};
use smallvec::SmallVec;
use span::SyntaxContext;
use syntax::{SyntaxKind, view::HasName};

use crate::{
    AdtId, AstIdLoc, ConstId, ConstParamId, DefWithBodyId, EnumId, EnumVariantId,
    ExpressionStoreOwnerId, ExternBlockId, ExternCrateId, FunctionId, FxIndexMap, GenericDefId,
    GenericParamId, HasModule, ImplId, ItemContainerId, LifetimeParamId, Lookup, Macro2Id, MacroId,
    MacroRulesId, ModuleDefId, ModuleId, ProcMacroId, StaticId, StructId, TraitId, TypeAliasId,
    TypeOrConstParamId, TypeParamId, UseId, VariantId,
    builtin_type::BuiltinType,
    expr_store::{
        ExpressionStore, HygieneId,
        path::Path,
        scope::{ExprScopes, ScopeId},
    },
    hir::{
        BindingId, ExprId, LabelId,
        generics::{GenericParams, TypeOrConstParamData},
    },
    item_scope::{BUILTIN_SCOPE, BuiltinShadowMode, ImportOrExternCrate, ItemScope},
    lang_item::LangItemTarget,
    nameres::{
        DefMap, LocalDefMap, MacroSubNs, ResolvePathResultPrefixInfo, block_def_map, uses::Use,
    },
    per_ns::{MacrosItem, Namespace, PerNs},
    signatures::ImplSignature,
    src::HasSource,
    type_ref::LifetimeRef,
    visibility::{RawVisibility, Visibility, VisibilityError},
};

/// What [`Resolver::names_in_scope`] returns: each name with its meanings, innermost first.
pub type NamesInScope = FxIndexMap<Name, SmallVec<[ScopeDef; 1]>>;

thread_local! {
    /// The `macro_use` prelude of [`Resolver::names_in_scope_into`], put in name order.
    static MACRO_USE_PRELUDE: RefCell<Scratch<(Name, MacroId)>> =
        const { RefCell::new(Scratch::new()) };
}

/// A stack of [`Scope`]s over the scope of a module. A body walk keeps one: entering an
/// expression pushes its scopes onto the same stack and leaving truncates it
/// ([`Resolver::update_to_inner_scope`], [`Resolver::reset_to_guard`]), so a walk allocates
/// only when the stack grows past its deepest point so far.
#[derive(Debug, Clone)]
pub struct Resolver<'db> {
    /// The stack of scopes, where the inner-most scope is the last item.
    ///
    /// When using, you generally want to process the scopes in reverse order,
    /// there's `scopes` *method* for that.
    scopes: SmallVec<[Scope<'db>; 8]>,
    /// The expression scopes that [`Scope::ExprScope`] and [`Scope::MacroDefScope`] index: one
    /// per expression store the stack has held, so one in practice, kept inline.
    expr_stores: SmallVec<[ExprStore<'db>; 1]>,
    module_scope: ModuleItemMap<'db>,
}

#[derive(Clone, Copy)]
struct ModuleItemMap<'db> {
    def_map: &'db DefMap,
    local_def_map: &'db LocalDefMap,
    module_id: ModuleId,
}

impl fmt::Debug for ModuleItemMap<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModuleItemMap").field("module_id", &self.module_id).finish()
    }
}

/// The expression scopes of one store owner.
#[derive(Clone, Copy)]
struct ExprStore<'db> {
    owner: ExpressionStoreOwnerId,
    scopes: &'db ExprScopes,
}

impl fmt::Debug for ExprStore<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExprStore").field("owner", &self.owner).finish()
    }
}

/// One scope of a [`Resolver`]: `Copy`, 24 bytes at most (16 where rustc puts the tag in a
/// niche of `GenericDefId`), against 32 when it held a `MacroDefId` and an owner per scope.
/// Pushing one is a copy; nothing in it is dropped.
#[derive(Clone, Copy)]
enum Scope<'db> {
    /// All the items and imported names of a module of a block's def map. Its local def map is
    /// the crate's, the resolver's `module_scope.local_def_map`: a block is in the crate of the
    /// item whose body it is in.
    BlockScope { def_map: &'db DefMap, module_id: ModuleId },
    /// Brings the generic parameters of an item into scope as well as the `Self` type alias /
    /// generic for ADTs and impls.
    GenericParams { def: GenericDefId, params: &'db GenericParams },
    /// Local bindings: scope `scope_id` of `expr_stores[store]`.
    ExprScope { store: u32, scope_id: ScopeId },
    /// Macro definition inside bodies that affects all paths after it in the same block: the
    /// macro of scope `scope_id` of `expr_stores[store]`.
    MacroDefScope { store: u32, scope_id: ScopeId },
}

const _: () = assert!(size_of::<Scope<'static>>() <= 24);

impl fmt::Debug for Scope<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::BlockScope { module_id, .. } => {
                f.debug_struct("BlockScope").field("module_id", module_id).finish()
            }
            Scope::GenericParams { def, .. } => {
                f.debug_struct("GenericParams").field("def", def).finish()
            }
            Scope::ExprScope { store, scope_id } => f
                .debug_struct("ExprScope")
                .field("store", store)
                .field("scope_id", scope_id)
                .finish(),
            Scope::MacroDefScope { store, scope_id } => f
                .debug_struct("MacroDefScope")
                .field("store", store)
                .field("scope_id", scope_id)
                .finish(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeNs {
    SelfType(ImplId),
    GenericParam(TypeParamId),
    AdtId(AdtId),
    AdtSelfType(AdtId),
    // Yup, enum variants are added to the types ns, but any usage of variant as
    // type is an error.
    EnumVariantId(EnumVariantId),
    TypeAliasId(TypeAliasId),
    BuiltinType(BuiltinType),
    TraitId(TraitId),

    ModuleId(ModuleId),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ResolveValueResult {
    ValueNs(ValueNs),
    Partial(TypeNs, usize),
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum ValueNs {
    ImplSelf(ImplId),
    LocalBinding(BindingId),
    FunctionId(FunctionId),
    ConstId(ConstId),
    StaticId(StaticId),
    StructId(StructId),
    EnumVariantId(EnumVariantId),
    GenericParam(ConstParamId),
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum LifetimeNs {
    Static,
    LifetimeParam(LifetimeParamId),
}

impl<'db> Resolver<'db> {
    /// Resolve known trait from std, like `std::futures::Future`
    pub fn resolve_known_trait(&self, db: &Db, path: &ModPath) -> Option<TraitId> {
        let res = self.resolve_module_path(db, path, BuiltinShadowMode::Other).take_types()?;
        match res {
            ModuleDefId::TraitId(it) => Some(it),
            _ => None,
        }
    }

    /// Resolve known struct from std, like `std::boxed::Box`
    pub fn resolve_known_struct(
        &self,
        db: &Db,
        path: &ModPath,
    ) -> Option<StructId> {
        let res = self.resolve_module_path(db, path, BuiltinShadowMode::Other).take_types()?;
        match res {
            ModuleDefId::AdtId(AdtId::StructId(it)) => Some(it),
            _ => None,
        }
    }

    /// Resolve known enum from std, like `std::result::Result`
    pub fn resolve_known_enum(&self, db: &Db, path: &ModPath) -> Option<EnumId> {
        let res = self.resolve_module_path(db, path, BuiltinShadowMode::Other).take_types()?;
        match res {
            ModuleDefId::AdtId(AdtId::EnumId(it)) => Some(it),
            _ => None,
        }
    }

    pub fn resolve_module_path_in_items(&self, db: &Db, path: &ModPath) -> PerNs {
        self.resolve_module_path(db, path, BuiltinShadowMode::Module)
    }

    pub fn resolve_path_in_type_ns(
        &self,
        db: &Db,
        path: Path<'_>,
    ) -> Option<(TypeNs, Option<usize>, Option<ImportOrExternCrate>)> {
        self.resolve_path_in_type_ns_with_prefix_info(db, path).map(
            |(resolution, remaining_segments, import, _, _)| {
                (resolution, remaining_segments, import)
            },
        )
    }

    /// The scope where the `macro` that wrote a name with `hygiene` is defined, for the
    /// items it names there.
    pub fn def_site(db: &'db Db, hygiene: HygieneId) -> Option<Resolver<'db>> {
        Some(macro_def_site(db, hygiene)?.resolver(db))
    }

    pub fn resolve_path_in_type_ns_with_prefix_info(
        &self,
        db: &Db,
        path: Path<'_>,
    ) -> Option<(
        TypeNs,
        Option<usize>,
        Option<ImportOrExternCrate>,
        ResolvePathResultPrefixInfo,
        Visibility,
    )> {
        let path = match path {
            Path::BarePath(mod_path) => mod_path,
            Path::Normal(it) => it.mod_path,
            Path::LangItem(l, seg) => {
                let type_ns = match l {
                    LangItemTarget::UnionId(it) => TypeNs::AdtId(it.into()),
                    LangItemTarget::TypeAliasId(it) => TypeNs::TypeAliasId(it),
                    LangItemTarget::StructId(it) => TypeNs::AdtId(it.into()),
                    LangItemTarget::EnumVariantId(it) => TypeNs::EnumVariantId(it),
                    LangItemTarget::EnumId(it) => TypeNs::AdtId(it.into()),
                    LangItemTarget::TraitId(it) => TypeNs::TraitId(it),
                    LangItemTarget::FunctionId(_)
                    | LangItemTarget::ImplId(_)
                    | LangItemTarget::StaticId(_)
                    | LangItemTarget::ConstId(_)
                    | LangItemTarget::MacroId(_) => return None,
                };
                return Some((
                    type_ns,
                    seg.as_ref().map(|_| 1),
                    None,
                    ResolvePathResultPrefixInfo::default(),
                    Visibility::Public,
                ));
            }
        };
        let first_name = path.segments().first()?;
        let skip_to_mod = path.kind != PathKind::Plain;
        if skip_to_mod {
            return self.skip_to_mod(|scope| scope.resolve_path_in_type_ns(db, path));
        }

        let remaining_idx = || {
            if path.segments().len() == 1 { None } else { Some(1) }
        };

        for scope in self.scopes() {
            match scope {
                Scope::ExprScope { .. } | Scope::MacroDefScope { .. } => continue,
                Scope::GenericParams { params, def } => {
                    if let GenericDefId::ImplId(impl_) = def {
                        if *first_name == sym::Self_ {
                            return Some((
                                TypeNs::SelfType(impl_),
                                remaining_idx(),
                                None,
                                ResolvePathResultPrefixInfo::default(),
                                Visibility::Public,
                            ));
                        }
                    } else if let GenericDefId::AdtId(adt) = def
                        && *first_name == sym::Self_
                    {
                        return Some((
                            TypeNs::AdtSelfType(adt),
                            remaining_idx(),
                            None,
                            ResolvePathResultPrefixInfo::default(),
                            Visibility::Public,
                        ));
                    }
                    if let Some(id) = params.find_type_by_name(first_name, def) {
                        return Some((
                            TypeNs::GenericParam(id),
                            remaining_idx(),
                            None,
                            ResolvePathResultPrefixInfo::default(),
                            Visibility::Public,
                        ));
                    }
                }
                Scope::BlockScope { def_map, module_id } => {
                    let m = self.block_items(def_map, module_id);
                    if let Some(res) = m.resolve_path_in_type_ns(db, path) {
                        let res = match res.0 {
                            TypeNs::ModuleId(_) if res.1.is_none() => {
                                if let Some(ModuleDefId::BuiltinType(builtin)) = BUILTIN_SCOPE
                                    .get(first_name)
                                    .and_then(|builtin| builtin.take_types())
                                {
                                    (
                                        TypeNs::BuiltinType(builtin),
                                        remaining_idx(),
                                        None,
                                        ResolvePathResultPrefixInfo::default(),
                                        Visibility::Public,
                                    )
                                } else {
                                    res
                                }
                            }
                            _ => res,
                        };
                        return Some(res);
                    }
                }
            }
        }
        self.module_scope.resolve_path_in_type_ns(db, path)
    }

    pub fn resolve_path_in_type_ns_fully(
        &self,
        db: &Db,
        path: Path<'_>,
    ) -> Option<TypeNs> {
        let (res, unresolved, _) = self.resolve_path_in_type_ns(db, path)?;
        if unresolved.is_some() {
            return None;
        }
        Some(res)
    }

    pub fn resolve_visibility(
        &self,
        db: &Db,
        visibility: &RawVisibility,
    ) -> Result<Visibility, VisibilityError> {
        match visibility {
            RawVisibility::Module(_, _) => {
                let (item_map, item_local_map, module) = self.item_scope_();
                item_map.resolve_visibility(
                    item_local_map,
                    db,
                    module,
                    visibility,
                    self.scopes().any(|scope| {
                        matches!(scope, Scope::GenericParams { def: GenericDefId::ImplId(_), .. })
                    }),
                )
            }
            RawVisibility::PubSelf(explicitness) => {
                Ok(Visibility::Module(self.module(), *explicitness))
            }
            RawVisibility::PubCrate => Ok(Visibility::PubCrate(self.krate())),
            RawVisibility::Public => Ok(Visibility::Public),
        }
    }

    pub fn resolve_path_in_value_ns(
        &self,
        db: &Db,
        path: Path<'_>,
        hygiene_id: HygieneId,
    ) -> Option<ResolveValueResult> {
        self.resolve_path_in_value_ns_with_prefix_info(db, path, hygiene_id).map(|(it, _, _)| it)
    }

    fn skip_to_mod<T>(&self, mut f: impl FnMut(&ModuleItemMap<'db>) -> Option<T>) -> Option<T> {
        self.scopes()
            .find_map(|scope| match scope {
                Scope::BlockScope { def_map, module_id } => {
                    f(&self.block_items(def_map, module_id))
                }
                _ => None,
            })
            .or_else(|| f(&self.module_scope))
    }

    pub fn resolve_path_in_value_ns_with_prefix_info(
        &self,
        db: &Db,
        path: Path<'_>,
        mut hygiene_id: HygieneId,
    ) -> Option<(ResolveValueResult, ResolvePathResultPrefixInfo, Visibility)> {
        let path = match path {
            Path::BarePath(mod_path) => mod_path,
            Path::Normal(it) => it.mod_path,
            Path::LangItem(l, None) => {
                return Some((
                    ResolveValueResult::ValueNs(match l {
                        LangItemTarget::FunctionId(it) => ValueNs::FunctionId(it),
                        LangItemTarget::StaticId(it) => ValueNs::StaticId(it),
                        LangItemTarget::StructId(it) => ValueNs::StructId(it),
                        LangItemTarget::EnumVariantId(it) => ValueNs::EnumVariantId(it),
                        LangItemTarget::ConstId(it) => ValueNs::ConstId(it),
                        LangItemTarget::UnionId(_)
                        | LangItemTarget::ImplId(_)
                        | LangItemTarget::TypeAliasId(_)
                        | LangItemTarget::TraitId(_)
                        | LangItemTarget::EnumId(_)
                        | LangItemTarget::MacroId(_) => return None,
                    }),
                    ResolvePathResultPrefixInfo::default(),
                    Visibility::Public,
                ));
            }
            Path::LangItem(l, Some(_)) => {
                let type_ns = match l {
                    LangItemTarget::UnionId(it) => TypeNs::AdtId(it.into()),
                    LangItemTarget::TypeAliasId(it) => TypeNs::TypeAliasId(it),
                    LangItemTarget::StructId(it) => TypeNs::AdtId(it.into()),
                    LangItemTarget::EnumVariantId(it) => TypeNs::EnumVariantId(it),
                    LangItemTarget::EnumId(it) => TypeNs::AdtId(it.into()),
                    LangItemTarget::TraitId(it) => TypeNs::TraitId(it),
                    LangItemTarget::FunctionId(_)
                    | LangItemTarget::ImplId(_)
                    | LangItemTarget::StaticId(_)
                    | LangItemTarget::ConstId(_)
                    | LangItemTarget::MacroId(_) => return None,
                };
                // Remaining segments start from 0 because lang paths have no segments other than the remaining.
                return Some((
                    ResolveValueResult::Partial(type_ns, 0),
                    ResolvePathResultPrefixInfo::default(),
                    Visibility::Public,
                ));
            }
        };
        let n_segments = path.segments().len();
        let tmp = Name::new_symbol_root(sym::self_);
        let first_name = if path.is_self() { &tmp } else { path.segments().first()? };
        let skip_to_mod = path.kind != PathKind::Plain && !path.is_self();
        let path_hygiene = hygiene_id;
        if skip_to_mod {
            // A `macro` names the modules of where it is defined, private ones too: its
            // `crate::` path is looked up there when it is not visible where the macro is used,
            // as a plain path is below.
            return self.skip_to_mod(|scope| scope.resolve_path_in_value_ns(db, path)).or_else(
                || {
                    macro_def_site(db, path_hygiene).and_then(|module| {
                        module.resolver(db).module_scope.resolve_path_in_value_ns(db, path)
                    })
                },
            );
        }

        if n_segments <= 1 {
            // Read at the first macro scope the walk meets, before `hygiene_id` changes there.
            let mut macro_hygiene = None;
            for scope in self.scopes() {
                match scope {
                    Scope::ExprScope { store, scope_id } => {
                        // A name a pattern binds twice means its last binding.
                        let entry = self.expr_scopes(store).entries(scope_id).iter().rev().find(
                            |entry| entry.name() == first_name && entry.hygiene() == hygiene_id,
                        );

                        if let Some(e) = entry {
                            return Some((
                                ResolveValueResult::ValueNs(ValueNs::LocalBinding(e.binding())),
                                ResolvePathResultPrefixInfo::default(),
                                Visibility::Public,
                            ));
                        }
                    }
                    Scope::MacroDefScope { store, scope_id } => {
                        let info = macro_hygiene.get_or_insert_with(|| hygiene_info(db, hygiene_id));
                        handle_macro_def_scope(
                            db,
                            &mut hygiene_id,
                            info,
                            self.macro_def(store, scope_id),
                        )
                    }
                    Scope::GenericParams { params, def } => {
                        if let GenericDefId::ImplId(impl_) = def
                            && *first_name == sym::Self_
                        {
                            return Some((
                                ResolveValueResult::ValueNs(ValueNs::ImplSelf(impl_)),
                                ResolvePathResultPrefixInfo::default(),
                                Visibility::Public,
                            ));
                        }
                        if let Some(id) = params.find_const_by_name(first_name, def) {
                            let val = ValueNs::GenericParam(id);
                            return Some((
                                ResolveValueResult::ValueNs(val),
                                ResolvePathResultPrefixInfo::default(),
                                Visibility::Public,
                            ));
                        }
                    }
                    Scope::BlockScope { def_map, module_id } => {
                        let m = self.block_items(def_map, module_id);
                        if let Some(def) = m.resolve_path_in_value_ns(db, path) {
                            return Some(def);
                        }
                    }
                }
            }
        } else {
            for scope in self.scopes() {
                match scope {
                    Scope::ExprScope { .. } | Scope::MacroDefScope { .. } => continue,
                    Scope::GenericParams { params, def } => {
                        if let GenericDefId::ImplId(impl_) = def {
                            if *first_name == sym::Self_ {
                                return Some((
                                    ResolveValueResult::Partial(TypeNs::SelfType(impl_), 1),
                                    ResolvePathResultPrefixInfo::default(),
                                    Visibility::Public,
                                ));
                            }
                        } else if let GenericDefId::AdtId(adt) = def
                            && *first_name == sym::Self_
                        {
                            let ty = TypeNs::AdtSelfType(adt);
                            return Some((
                                ResolveValueResult::Partial(ty, 1),
                                ResolvePathResultPrefixInfo::default(),
                                Visibility::Public,
                            ));
                        }
                        if let Some(id) = params.find_type_by_name(first_name, def) {
                            let ty = TypeNs::GenericParam(id);
                            return Some((
                                ResolveValueResult::Partial(ty, 1),
                                ResolvePathResultPrefixInfo::default(),
                                Visibility::Public,
                            ));
                        }
                    }
                    Scope::BlockScope { def_map, module_id } => {
                        let m = self.block_items(def_map, module_id);
                        if let Some(def) = m.resolve_path_in_value_ns(db, path) {
                            return Some(def);
                        }
                    }
                }
            }
        }

        if let Some(res) = self.module_scope.resolve_path_in_value_ns(db, path) {
            return Some(res);
        }

        if let Some(res) = macro_def_site(db, path_hygiene)
            .and_then(|module| module.resolver(db).module_scope.resolve_path_in_value_ns(db, path))
        {
            return Some(res);
        }

        // If a path of the shape `u16::from_le_bytes` failed to resolve at all, then we fall back
        // to resolving to the primitive type, to allow this to still work in the presence of
        // `use core::u16;`.
        if path.kind == PathKind::Plain
            && n_segments > 1
            && let Some(builtin) = BuiltinType::by_name(first_name)
        {
            return Some((
                ResolveValueResult::Partial(TypeNs::BuiltinType(builtin), 1),
                ResolvePathResultPrefixInfo::default(),
                Visibility::Public,
            ));
        }

        None
    }

    pub fn resolve_path_in_value_ns_fully(
        &self,
        db: &Db,
        path: Path<'_>,
        hygiene: HygieneId,
    ) -> Option<ValueNs> {
        match self.resolve_path_in_value_ns(db, path, hygiene)? {
            ResolveValueResult::ValueNs(it) => Some(it),
            ResolveValueResult::Partial(..) => None,
        }
    }

    pub fn resolve_path_as_macro(
        &self,
        db: &Db,
        path: &ModPath,
        expected_macro_kind: Option<MacroSubNs>,
    ) -> Option<MacrosItem> {
        let (item_map, item_local_map, module) = self.item_scope_();
        item_map
            .resolve_path(
                item_local_map,
                db,
                module,
                path,
                BuiltinShadowMode::Other,
                expected_macro_kind,
            )
            .0
            .take_macros_full()
    }

    pub fn resolve_path_as_macro_def(
        &self,
        db: &Db,
        path: &ModPath,
        expected_macro_kind: Option<MacroSubNs>,
    ) -> Option<MacroDefId> {
        self.resolve_path_as_macro(db, path, expected_macro_kind).map(|it| it.def.definition(db))
    }

    /// Appends to `out` the imports and `extern crate`s of this crate that `path` resolves
    /// through in `ns` (types or values), in the item scope where
    /// [`Self::resolve_path_in_type_ns`] or [`Self::resolve_path_in_value_ns`] finds it, as
    /// [`crate::nameres::uses`] records uses: none for a path naming a local, a generic
    /// parameter or `Self`, or that does not resolve. Macro paths go through
    /// [`Self::record_macro_path_uses`].
    pub fn record_path_uses(
        &self,
        db: &Db,
        path: Path<'_>,
        ns: Namespace,
        hygiene: HygieneId,
        out: &mut Vec<Use>,
    ) {
        let mod_path = match path {
            Path::BarePath(it) => it,
            Path::Normal(it) => it.mod_path,
            Path::LangItem(..) => return,
        };
        let in_items = match ns {
            Namespace::Types => !matches!(
                self.resolve_path_in_type_ns(db, path),
                None | Some((
                    TypeNs::SelfType(_) | TypeNs::GenericParam(_) | TypeNs::AdtSelfType(_),
                    ..
                ))
            ),
            Namespace::Values => !matches!(
                self.resolve_path_in_value_ns(db, path, hygiene),
                None | Some(
                    ResolveValueResult::ValueNs(
                        ValueNs::LocalBinding(_) | ValueNs::GenericParam(_) | ValueNs::ImplSelf(_)
                    ) | ResolveValueResult::Partial(
                        TypeNs::SelfType(_) | TypeNs::GenericParam(_) | TypeNs::AdtSelfType(_),
                        _
                    )
                )
            ),
            Namespace::Macros => false,
        };
        if !in_items {
            return;
        }
        // The item scope the lookup answered from: the innermost block that has the path, the
        // module, or the module where the macro that wrote it is defined.
        let block = self.scopes().find_map(|scope| match scope {
            Scope::BlockScope { def_map, module_id } => {
                let items = self.block_items(def_map, module_id);
                items.answers(db, mod_path, ns).then_some(items)
            }
            _ => None,
        });
        if let Some(items) = block.or_else(|| {
            self.module_scope.answers(db, mod_path, ns).then_some(self.module_scope)
        }) {
            items.record_uses(db, mod_path, ns, out);
            return;
        }
        if ns == Namespace::Values
            && let Some(module) = macro_def_site(db, hygiene)
        {
            let items = module.resolver(db).module_scope;
            if items.answers(db, mod_path, ns) {
                items.record_uses(db, mod_path, ns, out);
            }
        }
    }

    /// [`Self::record_path_uses`] of a macro path, where [`Self::resolve_path_as_macro`]
    /// resolves it.
    pub fn record_macro_path_uses(
        &self,
        db: &Db,
        path: &ModPath,
        expected_macro_kind: Option<MacroSubNs>,
        out: &mut Vec<Use>,
    ) {
        let (item_map, item_local_map, module) = self.item_scope_();
        item_map.path_uses_outside_imports(
            item_local_map,
            db,
            module,
            path,
            Namespace::Macros,
            expected_macro_kind,
            true,
            out,
        );
    }

    /// Appends to `out` the imports of this crate that have `trait_` in scope here, for a
    /// method or associated item type inference found in it through the traits in scope (rustc's
    /// `TraitCandidate::import_ids`, which `used_trait_imports` marks used). The candidate is
    /// rustc's first: the impl's own trait, which no import brings; else the innermost block
    /// scope that has the trait, then the module's. A trait the std prelude gives records
    /// nothing: the prelude's glob is the injected import, which rustc never lints.
    pub fn record_trait_uses(&self, db: &Db, trait_: TraitId, out: &mut Vec<Use>) {
        let is_impl_trait = |scope: Scope<'_>| match scope {
            Scope::GenericParams { def: GenericDefId::ImplId(impl_), .. } => {
                let impl_data = ImplSignature::of(db, impl_);
                impl_data.target_trait.is_some_and(|target| {
                    self.resolve_path_in_type_ns_fully(db, impl_data.store.path(target.path))
                        == Some(TypeNs::TraitId(trait_))
                })
            }
            _ => false,
        };
        if self.scopes().any(is_impl_trait) {
            return;
        }
        let in_blocks = self.scopes().find_map(|scope| match scope {
            Scope::BlockScope { def_map, module_id } => {
                def_map.module(db, module_id).scope(db).trait_binding(trait_)
            }
            _ => None,
        });
        let binding = in_blocks.or_else(|| {
            let ModuleItemMap { def_map, module_id, .. } = self.module_scope;
            def_map.module(db, module_id).scope(db).trait_binding(trait_)
        });
        if let Some((name, Some(import))) = binding {
            crate::nameres::uses::trait_uses(db, self.krate(), trait_, (name, import), out);
        }
    }

    pub fn resolve_lifetime(&self, lifetime: &LifetimeRef) -> Option<LifetimeNs> {
        match lifetime {
            LifetimeRef::Static => Some(LifetimeNs::Static),
            LifetimeRef::Named(name) => self.scopes().find_map(|scope| match scope {
                Scope::GenericParams { def, params } => {
                    params.find_lifetime_by_name(name, def).map(LifetimeNs::LifetimeParam)
                }
                _ => None,
            }),
            LifetimeRef::Placeholder | LifetimeRef::Error => None,
            LifetimeRef::Param(lifetime_param_id) => {
                Some(LifetimeNs::LifetimeParam(*lifetime_param_id))
            }
        }
    }

    /// Returns a set of names available in the current scope.
    ///
    /// Note that this is a somewhat fuzzy concept -- internally, the compiler
    /// doesn't necessary follow a strict scoping discipline. Rather, it just
    /// tells for each ident what it resolves to.
    ///
    /// A good example is something like `str::from_utf8`. From scopes point of
    /// view, this code is erroneous -- both `str` module and `str` type occupy
    /// the same type namespace.
    ///
    /// We don't try to model that super-correctly -- this functionality is
    /// primarily exposed for completions.
    ///
    /// Note that in Rust one name can be bound to several items:
    ///
    /// ```
    /// # #![allow(non_camel_case_types)]
    /// macro_rules! t { () => (()) }
    /// type t = t!();
    /// const t: t = t!();
    /// ```
    ///
    /// That's why we return a multimap.
    ///
    /// The shadowing is accounted for: in
    ///
    /// ```ignore
    /// let it = 92;
    /// {
    ///     let it = 92;
    ///     $0
    /// }
    /// ```
    ///
    /// there will be only one entry for `it` in the result.
    ///
    /// The result is ordered *roughly* from the innermost scope to the
    /// outermost: when the name is introduced in two namespaces in two scopes,
    /// we use the position of the first scope.
    pub fn names_in_scope(&self, db: &Db) -> NamesInScope {
        let mut names = NamesInScope::default();
        self.names_in_scope_into(db, &mut names);
        names
    }

    /// [`Self::names_in_scope`] into `names`, which it empties first: a caller that keeps one map
    /// allocates only while the map grows, and for a name with more than one meaning (its list
    /// leaves its inline slot).
    pub fn names_in_scope_into(&self, db: &Db, names: &mut NamesInScope) {
        names.clear();
        let mut res = ScopeNames { map: names };
        for scope in self.scopes() {
            self.process_names(scope, &mut res, db);
        }
        let ModuleItemMap { def_map, module_id, local_def_map } = self.module_scope;
        // FIXME: should we provide `self` here?
        def_map.module(db, module_id).scope(db).entries().for_each(|(name, def)| {
            res.add_per_ns(name, def);
        });
        def_map.module(db, module_id).scope(db).legacy_macros().for_each(|(name, macs)| {
            macs.iter().for_each(|&mac| {
                res.add(name, ScopeDef::ModuleDef(ModuleDefId::MacroId(mac)));
            })
        });
        // The `macro_use` prelude is a map, so its names are unique and an unstable sort keeps
        // the order; `res.add` doesn't reenter the resolver, so the borrow is taken once.
        MACRO_USE_PRELUDE.with_borrow_mut(|sorted| {
            let sorted = sorted.take();
            sorted.extend(def_map.macro_use_prelude().iter().map(|(name, &(def, _))| (*name, def)));
            sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            for &(name, def) in sorted.iter() {
                res.add(&name, ScopeDef::ModuleDef(def.into()));
            }
        });
        local_def_map.extern_prelude().for_each(|(name, (def, _extern_crate))| {
            res.add(name, ScopeDef::ModuleDef(ModuleDefId::ModuleId(def)));
        });
        BUILTIN_SCOPE.iter().for_each(|(name, &def)| {
            res.add_per_ns(name, def);
        });
        if let Some((prelude, _use)) = def_map.prelude() {
            let prelude_def_map = prelude.def_map(db);
            for (name, def) in prelude_def_map.module(db, prelude).scope(db).entries() {
                res.add_per_ns(name, def)
            }
        }
    }

    /// Note: Not to be used directly within hir-def/hir-ty
    pub fn extern_crate_decls_in_scope<'a>(
        &'a self,
        db: &'a Db,
    ) -> impl Iterator<Item = Name> + 'a {
        self.module_scope
            .def_map
            .module(db, self.module_scope.module_id)
            .scope(db)
            .extern_crate_decls()
            .filter_map(|id| {
                let loc = id.lookup(db);
                let extern_crate = loc.source_view(db);
                // If there is a rename (`as x`), extract the renamed name, or remove the `extern crate`
                // if it is an underscore.
                extern_crate
                    .value
                    .rename()
                    .map(|a| a.name().map(|it| it.as_name()))
                    .unwrap_or_else(|| extern_crate.value.name_ref().map(|it| it.as_name()))
            })
    }

    pub fn extern_crates_in_scope(&self) -> impl Iterator<Item = (Name, ModuleId)> + '_ {
        self.module_scope
            .local_def_map
            .extern_prelude()
            .map(|(name, module_id)| (*name, module_id.0))
    }

    pub fn traits_in_scope(&self, db: &Db) -> HashSet<TraitId> {
        let mut traits = HashSet::default();
        self.traits_in_scope_into(db, &mut traits);
        traits
    }

    /// Adds the traits in scope to `traits`, for a caller that refills one set as it walks
    /// (clear it first for exactly [`Self::traits_in_scope`]), or that keeps it in its frame.
    /// The resolver's [`Self::module`] names which traits these are: two positions of one walk
    /// with the same module have the same traits in scope.
    pub fn traits_in_scope_into(&self, db: &Db, traits: &mut impl Extend<TraitId>) {
        self.for_each_trait_in_scope(db, |it| traits.extend(iter::once(it)));
    }

    /// Calls `f` with each trait in scope, innermost scope first. A trait in scope twice (from
    /// a block and from the module) comes twice.
    pub fn for_each_trait_in_scope(&self, db: &Db, mut f: impl FnMut(TraitId)) {
        let _ = self.try_for_each_trait_in_scope(db, &mut |it| {
            f(it);
            ControlFlow::<()>::Continue(())
        });
    }

    /// Whether `trait_` is in scope: [`Self::for_each_trait_in_scope`]'s walk, stopping at it.
    pub fn has_trait_in_scope(&self, db: &Db, trait_: TraitId) -> bool {
        self.try_for_each_trait_in_scope(db, &mut |it| match it == trait_ {
            true => ControlFlow::Break(()),
            false => ControlFlow::Continue(()),
        })
        .is_break()
    }

    fn try_for_each_trait_in_scope<B>(
        &self,
        db: &Db,
        f: &mut impl FnMut(TraitId) -> ControlFlow<B>,
    ) -> ControlFlow<B> {
        // FIXME(trait_alias): Trait alias brings aliased traits in scope! Note that supertraits of
        // aliased traits are NOT brought in scope (unless also aliased).
        for scope in self.scopes() {
            match scope {
                Scope::BlockScope { def_map, module_id } => {
                    for it in def_map.module(db, module_id).traits_in_scope(db) {
                        f(it)?;
                    }
                }
                Scope::GenericParams { def: GenericDefId::ImplId(impl_), .. } => {
                    let impl_data = ImplSignature::of(db, impl_);
                    if let Some(target_trait) = impl_data.target_trait
                        && let Some(TypeNs::TraitId(trait_)) = self
                            .resolve_path_in_type_ns_fully(db, impl_data.store.path(target_trait.path))
                    {
                        f(trait_)?;
                    }
                }
                _ => (),
            }
        }
        if let Some((prelude, _use)) = self.module_scope.def_map.prelude() {
            let prelude_def_map = prelude.def_map(db);
            for it in prelude_def_map.module(db, prelude).traits_in_scope(db) {
                f(it)?;
            }
        }
        let module = self.module_scope.def_map.module(db, self.module_scope.module_id);
        for it in module.traits_in_scope(db) {
            f(it)?;
        }
        ControlFlow::Continue(())
    }

    pub fn traits_in_scope_from_block_scopes<'a>(
        &'a self,
        db: &'a Db,
    ) -> impl Iterator<Item = TraitId> + 'a {
        self.scopes()
            .filter_map(move |scope| match scope {
                Scope::BlockScope { def_map, module_id } => {
                    Some(def_map.module(db, module_id).traits_in_scope(db))
                }
                _ => None,
            })
            .flatten()
    }

    pub fn module(&self) -> ModuleId {
        self.item_scope_().2
    }

    pub fn item_scope<'a>(&'a self, db: &'a Db) -> &'a ItemScope {
        let (def_map, _, local_id) = self.item_scope_();
        def_map.module(db, local_id).scope(db)
    }

    pub fn krate(&self) -> Crate {
        self.module_scope.def_map.krate()
    }

    pub fn def_map(&self) -> &DefMap {
        self.item_scope_().0
    }

    #[inline]
    pub fn top_level_def_map(&self) -> &'db DefMap {
        self.module_scope.def_map
    }

    #[inline]
    pub fn is_visible(&self, db: &Db, visibility: Visibility) -> bool {
        visibility.is_visible_from_def_map(
            db,
            self.module_scope.def_map,
            self.module_scope.module_id,
        )
    }

    pub fn generic_def(&self) -> Option<GenericDefId> {
        self.scopes().find_map(|scope| match scope {
            Scope::GenericParams { def, .. } => Some(def),
            _ => None,
        })
    }

    pub fn generic_params(&self) -> Option<&GenericParams> {
        self.scopes().find_map(|scope| match scope {
            Scope::GenericParams { params, .. } => Some(params),
            _ => None,
        })
    }

    pub fn all_generic_params(&self) -> impl Iterator<Item = (&GenericParams, GenericDefId)> {
        self.scopes().filter_map(|scope| match scope {
            Scope::GenericParams { params, def } => Some((params, def)),
            _ => None,
        })
    }

    pub fn expression_store_owner(&self) -> Option<ExpressionStoreOwnerId> {
        self.scopes().find_map(|scope| match scope {
            Scope::ExprScope { store, .. } => Some(self.expr_stores[store as usize].owner),
            _ => None,
        })
    }

    pub fn impl_def(&self) -> Option<ImplId> {
        self.scopes().find_map(|scope| match scope {
            Scope::GenericParams { def: GenericDefId::ImplId(def), .. } => Some(def),
            _ => None,
        })
    }

    /// Checks if we rename `renamed` (currently named `current_name`) to `new_name`, will the meaning of this reference
    /// (that contains `current_name` path) change from `renamed` to some another variable (returned as `Some`).
    pub fn rename_will_conflict_with_another_variable(
        &self,
        db: &Db,
        current_name: &Name,
        current_name_as_path: &ModPath,
        mut hygiene_id: HygieneId,
        new_name: &Symbol,
        to_be_renamed: BindingId,
    ) -> Option<BindingId> {
        let mut hygiene_info = hygiene_info(db, hygiene_id);
        let mut will_be_resolved_to = None;
        for scope in self.scopes() {
            match scope {
                Scope::ExprScope { store, scope_id } => {
                    for entry in self.expr_scopes(store).entries(scope_id).iter().rev() {
                        if entry.hygiene() == hygiene_id {
                            if entry.binding() == to_be_renamed {
                                // This currently resolves to our renamed variable, now `will_be_resolved_to`
                                // contains `Some` if the meaning will change or `None` if not.
                                return will_be_resolved_to;
                            } else if entry.name().symbol() == new_name {
                                will_be_resolved_to = Some(entry.binding());
                            }
                        }
                    }
                }
                Scope::MacroDefScope { store, scope_id } => handle_macro_def_scope(
                    db,
                    &mut hygiene_id,
                    &mut hygiene_info,
                    self.macro_def(store, scope_id),
                ),
                Scope::GenericParams { params, def } => {
                    if params.find_const_by_name(current_name, def).is_some() {
                        // It does not resolve to our renamed variable.
                        return None;
                    }
                }
                Scope::BlockScope { def_map, module_id } => {
                    let m = self.block_items(def_map, module_id);
                    if m.resolve_path_in_value_ns(db, current_name_as_path).is_some() {
                        // It does not resolve to our renamed variable.
                        return None;
                    }
                }
            }
        }
        // It does not resolve to our renamed variable.
        None
    }

    /// Checks if we rename `renamed` to `name`, will the meaning of this reference (that contains `name` path) change
    /// from some other variable (returned as `Some`) to `renamed`.
    pub fn rename_will_conflict_with_renamed(
        &self,
        db: &Db,
        name: &Name,
        name_as_path: &ModPath,
        mut hygiene_id: HygieneId,
        to_be_renamed: BindingId,
    ) -> Option<BindingId> {
        let mut hygiene_info = hygiene_info(db, hygiene_id);
        let mut will_resolve_to_renamed = false;
        for scope in self.scopes() {
            match scope {
                Scope::ExprScope { store, scope_id } => {
                    for entry in self.expr_scopes(store).entries(scope_id).iter().rev() {
                        if entry.binding() == to_be_renamed {
                            will_resolve_to_renamed = true;
                        } else if entry.hygiene() == hygiene_id && entry.name() == name {
                            if will_resolve_to_renamed {
                                // This will resolve to the renamed variable before it resolves to the original variable.
                                return Some(entry.binding());
                            } else {
                                // This will resolve to the original variable.
                                return None;
                            }
                        }
                    }
                }
                Scope::MacroDefScope { store, scope_id } => handle_macro_def_scope(
                    db,
                    &mut hygiene_id,
                    &mut hygiene_info,
                    self.macro_def(store, scope_id),
                ),
                Scope::GenericParams { params, def } => {
                    if params.find_const_by_name(name, def).is_some() {
                        // Here and below, it might actually resolve to our renamed variable - in which case it'll
                        // hide the generic parameter or some other thing (not a variable). We don't check for that
                        // because due to naming conventions, it is rare that variable will shadow a non-variable.
                        return None;
                    }
                }
                Scope::BlockScope { def_map, module_id } => {
                    let m = self.block_items(def_map, module_id);
                    if m.resolve_path_in_value_ns(db, name_as_path).is_some() {
                        return None;
                    }
                }
            }
        }
        None
    }

    /// `expr_id` is required to be an expression id that comes after the top level expression scope in the given resolver
    #[must_use]
    pub fn update_to_inner_scope(
        &mut self,
        db: &'db Db,
        owner: impl Into<ExpressionStoreOwnerId>,
        expr_id: ExprId,
    ) -> UpdateGuard {
        self.update_to_inner_scope_(db, owner.into(), expr_id)
    }

    fn update_to_inner_scope_(
        &mut self,
        db: &'db Db,
        owner: ExpressionStoreOwnerId,
        expr_id: ExprId,
    ) -> UpdateGuard {
        // Each scope is pushed as macro, locals, block and the range reversed at the end, so the
        // stack holds outer scopes first and, for each, its block, locals, macro.
        #[inline(always)]
        fn append_expr_scope<'db>(
            db: &'db Db,
            resolver: &mut Resolver<'db>,
            store: u32,
            expr_scopes: &'db ExprScopes,
            scope_id: ScopeId,
        ) {
            if expr_scopes.macro_def(scope_id).is_some() {
                resolver.scopes.push(Scope::MacroDefScope { store, scope_id });
            }
            resolver.scopes.push(Scope::ExprScope { store, scope_id });
            if let Some(block) = expr_scopes.block(scope_id) {
                let def_map = block_def_map(db, block);
                resolver.scopes.push(Scope::BlockScope { def_map, module_id: def_map.root });
            }
        }

        let start = self.scopes.len();
        let innermost_scope =
            self.scopes().find(|scope| !matches!(scope, Scope::MacroDefScope { .. }));
        match innermost_scope {
            Some(Scope::ExprScope { store, scope_id }) => {
                let expr_scopes = self.expr_scopes(store);
                let scope_chain = expr_scopes
                    .scope_chain(expr_scopes.scope_for(expr_id))
                    .take_while(|&it| it != scope_id);
                for scope_id in scope_chain {
                    append_expr_scope(db, self, store, expr_scopes, scope_id);
                }
            }
            _ => {
                let expr_scopes = ExprScopes::of(db, owner);
                let store = self.expr_store_index(owner, expr_scopes);
                let scope_chain = expr_scopes.scope_chain(expr_scopes.scope_for(expr_id));

                for scope_id in scope_chain {
                    append_expr_scope(db, self, store, expr_scopes, scope_id);
                }
            }
        }
        self.scopes[start..].reverse();
        UpdateGuard(start)
    }

    pub fn reset_to_guard(&mut self, UpdateGuard(start): UpdateGuard) {
        self.scopes.truncate(start);
    }
}

#[inline]
fn handle_macro_def_scope(
    db: &Db,
    hygiene_id: &mut HygieneId,
    hygiene_info: &mut Option<(SyntaxContext, MacroDefId)>,
    macro_id: &MacroDefId,
) {
    if let Some((parent_ctx, label_macro_id)) = hygiene_info
        && label_macro_id == macro_id
    {
        // A macro is allowed to refer to variables from before its declaration.
        // Therefore, if we got to the rib of its declaration, give up its hygiene
        // and use its parent expansion.
        *hygiene_id = HygieneId::new(parent_ctx.opaque_and_semiopaque(db));
        *hygiene_info = parent_ctx.outer_expn(db).map(|expansion| {
            let expansion = hir_expand::MacroCallId::from(expansion).loc(db);
            (parent_ctx.parent(db), expansion.def)
        });
    }
}

/// The module of the `macro` whose expansion produced a name. Items named in such a macro are
/// the ones in scope where the macro is defined.
// FIXME: rustc looks there first; this is only reached when the name is unknown at the call site.
pub(crate) fn macro_def_site(db: &Db, hygiene_id: HygieneId) -> Option<ModuleId> {
    let (_, def) = hygiene_info(db, hygiene_id)?;
    // Only a `macro` is found below, and it is defined by a `MACRO_DEF` node: a `macro_rules!`
    // or a proc macro is turned away by one look at its node, not a walk over the crate's
    // macros.
    if def.kind.erased_ast_id().to_ptr(db).kind() != SyntaxKind::MACRO_DEF {
        return None;
    }
    crate::crate_def_map(db, def.krate).modules(db).find_map(|(_, data)| {
        data.scope(db).macros().find_map(|(_, it)| match it.def {
            MacroId::Macro2Id(id) if it.def.definition(db) == def => Some(id.lookup(db).container),
            _ => None,
        })
    })
}

#[inline]
fn hygiene_info(
    db: &Db,
    hygiene_id: HygieneId,
) -> Option<(SyntaxContext, MacroDefId)> {
    if !hygiene_id.is_root() {
        let ctx = hygiene_id.syntax_context();
        ctx.outer_expn(db).map(|expansion| {
            let expansion = hir_expand::MacroCallId::from(expansion).loc(db);
            (ctx.parent(db), expansion.def)
        })
    } else {
        None
    }
}

pub struct UpdateGuard(usize);

impl<'db> Resolver<'db> {
    /// The scopes, innermost first, by value (each is a small `Copy`).
    fn scopes(&self) -> impl Iterator<Item = Scope<'db>> + '_ {
        self.scopes.iter().rev().copied()
    }

    fn expr_scopes(&self, store: u32) -> &'db ExprScopes {
        self.expr_stores[store as usize].scopes
    }

    /// The macro of a [`Scope::MacroDefScope`], which is only pushed for a scope that has one.
    fn macro_def(&self, store: u32, scope_id: ScopeId) -> &'db MacroDefId {
        self.expr_scopes(store).macro_def(scope_id).expect("a macro def scope has a macro")
    }

    /// The index in `expr_stores` of `owner`'s scopes, added when the stack meets it first.
    fn expr_store_index(&mut self, owner: ExpressionStoreOwnerId, scopes: &'db ExprScopes) -> u32 {
        let at = match self.expr_stores.iter().position(|it| it.owner == owner) {
            Some(at) => at,
            None => {
                self.expr_stores.push(ExprStore { owner, scopes });
                self.expr_stores.len() - 1
            }
        };
        at as u32
    }

    /// The items of a module of a block's def map, which share the crate's local def map.
    fn block_items(&self, def_map: &'db DefMap, module_id: ModuleId) -> ModuleItemMap<'db> {
        ModuleItemMap { def_map, local_def_map: self.module_scope.local_def_map, module_id }
    }

    fn resolve_module_path(
        &self,
        db: &Db,
        path: &ModPath,
        shadow: BuiltinShadowMode,
    ) -> PerNs {
        let (item_map, item_local_map, module) = self.item_scope_();
        // This method resolves `path` just like import paths, so no expected macro subns is given.
        let (module_res, segment_index) =
            item_map.resolve_path(item_local_map, db, module, path, shadow, None);
        if segment_index.is_some() {
            return PerNs::none();
        }
        module_res
    }

    /// The innermost block scope that contains items or the module scope that contains this resolver.
    fn item_scope_(&self) -> (&DefMap, &LocalDefMap, ModuleId) {
        self.scopes()
            .find_map(|scope| match scope {
                Scope::BlockScope { def_map, module_id } => {
                    Some((def_map, self.module_scope.local_def_map, module_id))
                }
                _ => None,
            })
            .unwrap_or((
                self.module_scope.def_map,
                self.module_scope.local_def_map,
                self.module_scope.module_id,
            ))
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ScopeDef {
    ModuleDef(ModuleDefId),
    Unknown,
    ImplSelfType(ImplId),
    AdtSelfType(AdtId),
    GenericParam(GenericParamId),
    Local(BindingId),
    Label(LabelId),
}

impl<'db> Resolver<'db> {
    fn process_names(&self, scope: Scope<'db>, acc: &mut ScopeNames<'_>, db: &Db) {
        match scope {
            Scope::BlockScope { def_map, module_id } => {
                def_map.module(db, module_id).scope(db).entries().for_each(|(name, def)| {
                    acc.add_per_ns(name, def);
                });
                def_map.module(db, module_id).scope(db).legacy_macros().for_each(|(name, macs)| {
                    macs.iter().for_each(|&mac| {
                        acc.add(name, ScopeDef::ModuleDef(ModuleDefId::MacroId(mac)));
                    })
                });
            }
            Scope::GenericParams { params, def: parent } => {
                if let GenericDefId::ImplId(impl_) = parent {
                    acc.add(&Name::new_symbol_root(sym::Self_), ScopeDef::ImplSelfType(impl_));
                } else if let GenericDefId::AdtId(adt) = parent {
                    acc.add(&Name::new_symbol_root(sym::Self_), ScopeDef::AdtSelfType(adt));
                }

                for (local_id, param) in params.iter_type_or_consts() {
                    if let Some(name) = &param.name() {
                        let id = TypeOrConstParamId { parent, local_id };
                        let data = &GenericParams::of(db, parent)[local_id];
                        acc.add(
                            name,
                            ScopeDef::GenericParam(match data {
                                TypeOrConstParamData::TypeParamData(_) => {
                                    GenericParamId::TypeParamId(TypeParamId::from_unchecked(id))
                                }
                                TypeOrConstParamData::ConstParamData(_) => {
                                    GenericParamId::ConstParamId(ConstParamId::from_unchecked(id))
                                }
                            }),
                        );
                    }
                }
                for (local_id, param) in params.iter_lt() {
                    let id = LifetimeParamId { parent, local_id };
                    acc.add(&param.name, ScopeDef::GenericParam(id.into()))
                }
            }
            Scope::ExprScope { store, scope_id } => {
                let ExprStore { owner, scopes } = self.expr_stores[store as usize];
                if let Some(label) = scopes.label(scope_id) {
                    acc.add(&ExpressionStore::of(db, owner)[label].name, ScopeDef::Label(label))
                }
                scopes.entries(scope_id).iter().for_each(|e| {
                    acc.add_local(e.name(), e.binding());
                });
            }
            Scope::MacroDefScope { .. } => {}
        }
    }
}

pub fn resolver_for_scope(
    db: &Db,
    owner: impl Into<ExpressionStoreOwnerId> + HasResolver,
    scope_id: Option<ScopeId>,
) -> Resolver<'_> {
    let store_owner = owner.into();
    let r = store_owner.resolver(db);
    let scopes = ExprScopes::of(db, store_owner);
    resolver_for_scope_(db, scopes, scope_id, r, store_owner)
}

fn resolver_for_scope_<'db>(
    db: &'db Db,
    scopes: &'db ExprScopes,
    scope_id: Option<ScopeId>,
    mut r: Resolver<'db>,
    owner: ExpressionStoreOwnerId,
) -> Resolver<'db> {
    let store = r.expr_store_index(owner, scopes);
    // A walk of parent links: no allocation, and the reservation makes the pushes below
    // allocate at most once.
    r.scopes.reserve(scopes.scope_chain(scope_id).count());
    let start = r.scopes.len();
    // The chain runs innermost first: each scope is pushed as locals, macro, block and the
    // range reversed, so the stack holds outer scopes first and, for each, its block, macro,
    // locals.
    for scope_id in scopes.scope_chain(scope_id) {
        r.scopes.push(Scope::ExprScope { store, scope_id });
        if scopes.macro_def(scope_id).is_some() {
            r.scopes.push(Scope::MacroDefScope { store, scope_id });
        }
        if let Some(block) = scopes.block(scope_id) {
            let def_map = block_def_map(db, block);
            // Using `DefMap::ROOT` is okay here since inside modules other than the root,
            // there can't directly be expressions.
            r.scopes.push(Scope::BlockScope { def_map, module_id: def_map.root });
        }
    }
    r.scopes[start..].reverse();
    r
}

impl<'db> Resolver<'db> {
    fn push_scope(mut self, scope: Scope<'db>) -> Resolver<'db> {
        self.scopes.push(scope);
        self
    }

    fn push_generic_params_scope(
        self,
        db: &'db Db,
        def: GenericDefId,
    ) -> Resolver<'db> {
        let params = GenericParams::of(db, def);
        self.push_scope(Scope::GenericParams { def, params })
    }
}

impl<'db> ModuleItemMap<'db> {
    fn resolve_path_in_value_ns(
        &self,
        db: &'db Db,
        path: &ModPath,
    ) -> Option<(ResolveValueResult, ResolvePathResultPrefixInfo, Visibility)> {
        let (module_def, unresolved_idx, prefix_info) = self.def_map.resolve_path_locally(
            self.local_def_map,
            db,
            self.module_id,
            path,
            BuiltinShadowMode::Other,
        );
        match unresolved_idx {
            None => {
                let (value, vis) = to_value_ns(module_def, self.def_map)?;
                Some((ResolveValueResult::ValueNs(value), prefix_info, vis))
            }
            Some(unresolved_idx) => {
                let res = module_def.take_types_full()?;
                let ty = match res.def {
                    ModuleDefId::AdtId(it) => TypeNs::AdtId(it),
                    ModuleDefId::TraitId(it) => TypeNs::TraitId(it),
                    ModuleDefId::TypeAliasId(it) => TypeNs::TypeAliasId(it),
                    ModuleDefId::BuiltinType(it) => TypeNs::BuiltinType(it),

                    ModuleDefId::ModuleId(_)
                    | ModuleDefId::FunctionId(_)
                    | ModuleDefId::EnumVariantId(_)
                    | ModuleDefId::ConstId(_)
                    | ModuleDefId::MacroId(_)
                    | ModuleDefId::StaticId(_) => return None,
                };
                Some((ResolveValueResult::Partial(ty, unresolved_idx), prefix_info, res.vis))
            }
        }
    }

    fn resolve_path_in_type_ns(
        &self,
        db: &Db,
        path: &ModPath,
    ) -> Option<(
        TypeNs,
        Option<usize>,
        Option<ImportOrExternCrate>,
        ResolvePathResultPrefixInfo,
        Visibility,
    )> {
        let (module_def, idx, prefix_info) = self.def_map.resolve_path_locally(
            self.local_def_map,
            db,
            self.module_id,
            path,
            BuiltinShadowMode::Other,
        );
        let (res, import, vis) = to_type_ns(module_def)?;
        Some((res, idx, import, prefix_info, vis))
    }

    /// Whether `path` resolves here in `ns` (types or values).
    fn answers(&self, db: &Db, path: &ModPath, ns: Namespace) -> bool {
        match ns {
            Namespace::Values => self.resolve_path_in_value_ns(db, path).is_some(),
            Namespace::Types | Namespace::Macros => self.resolve_path_in_type_ns(db, path).is_some(),
        }
    }

    fn record_uses(&self, db: &Db, path: &ModPath, ns: Namespace, out: &mut Vec<Use>) {
        self.def_map.path_uses_outside_imports(
            self.local_def_map,
            db,
            self.module_id,
            path,
            ns,
            None,
            false,
            out,
        );
    }
}

fn to_value_ns(per_ns: PerNs, def_map: &DefMap) -> Option<(ValueNs, Visibility)> {
    let (def, vis) = per_ns.take_values_full().map(|res| (res.def, res.vis)).or_else(|| {
        let Some(MacrosItem { def: MacroId::ProcMacroId(proc_macro), vis, .. }) =
            per_ns.take_macros_full()
        else {
            return None;
        };
        // If we cannot resolve to value ns, but we can resolve to a proc macro, and this is the crate
        // defining this proc macro - inside this crate, we should treat the macro as a function.
        let def = ModuleDefId::FunctionId(def_map.proc_macro_as_fn(proc_macro)?);
        Some((def, vis))
    })?;
    let res = match def {
        ModuleDefId::FunctionId(it) => ValueNs::FunctionId(it),
        ModuleDefId::AdtId(AdtId::StructId(it)) => ValueNs::StructId(it),
        ModuleDefId::EnumVariantId(it) => ValueNs::EnumVariantId(it),
        ModuleDefId::ConstId(it) => ValueNs::ConstId(it),
        ModuleDefId::StaticId(it) => ValueNs::StaticId(it),

        ModuleDefId::AdtId(AdtId::EnumId(_) | AdtId::UnionId(_))
        | ModuleDefId::TraitId(_)
        | ModuleDefId::TypeAliasId(_)
        | ModuleDefId::BuiltinType(_)
        | ModuleDefId::MacroId(_)
        | ModuleDefId::ModuleId(_) => return None,
    };
    Some((res, vis))
}

fn to_type_ns(per_ns: PerNs) -> Option<(TypeNs, Option<ImportOrExternCrate>, Visibility)> {
    let def = per_ns.take_types_full()?;
    let res = match def.def {
        ModuleDefId::AdtId(it) => TypeNs::AdtId(it),
        ModuleDefId::EnumVariantId(it) => TypeNs::EnumVariantId(it),

        ModuleDefId::TypeAliasId(it) => TypeNs::TypeAliasId(it),
        ModuleDefId::BuiltinType(it) => TypeNs::BuiltinType(it),

        ModuleDefId::TraitId(it) => TypeNs::TraitId(it),

        ModuleDefId::ModuleId(it) => TypeNs::ModuleId(it),

        ModuleDefId::FunctionId(_)
        | ModuleDefId::ConstId(_)
        | ModuleDefId::MacroId(_)
        | ModuleDefId::StaticId(_) => return None,
    };
    Some((res, def.import, def.vis))
}

struct ScopeNames<'a> {
    map: &'a mut NamesInScope,
}

impl ScopeNames<'_> {
    fn add(&mut self, name: &Name, def: ScopeDef) {
        let set = self.map.entry(*name).or_default();
        if !set.contains(&def) {
            set.push(def)
        }
    }
    fn add_per_ns(&mut self, name: &Name, def: PerNs) {
        if let Some(ty) = &def.types {
            self.add(name, ScopeDef::ModuleDef(ty.def))
        }
        if let Some(def) = &def.values {
            self.add(name, ScopeDef::ModuleDef(def.def))
        }
        if let Some(mac) = &def.macros {
            self.add(name, ScopeDef::ModuleDef(ModuleDefId::MacroId(mac.def)))
        }
        if def.is_none() {
            self.add(name, ScopeDef::Unknown)
        }
    }
    fn add_local(&mut self, name: &Name, binding: BindingId) {
        let set = self.map.entry(*name).or_default();
        // XXX: hack, account for local (and only local) shadowing.
        //
        // This should be somewhat more principled and take namespaces into
        // accounts, but, alas, scoping rules are a hoax. `str` type and `str`
        // module can be both available in the same scope.
        if set.iter().any(|it| matches!(it, &ScopeDef::Local(_))) {
            cov_mark::hit!(shadowing_shows_single_completion);
            return;
        }
        set.push(ScopeDef::Local(binding))
    }
}

pub trait HasResolver: Copy {
    /// Builds a resolver for type references inside this def.
    fn resolver(self, db: &Db) -> Resolver<'_>;
}

impl HasResolver for ModuleId {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        let (mut def_map, local_def_map) = self.local_def_map(db);
        let mut module_id = self;

        let mut scopes = SmallVec::new();
        if self.block(db).is_some() {
            // Innermost block first, then reversed: the stack holds outer blocks first.
            while let Some(parent) = def_map.parent() {
                let block_def_map = mem::replace(&mut def_map, parent.def_map(db));
                let block_module_id = mem::replace(&mut module_id, parent);
                scopes.push(Scope::BlockScope { def_map: block_def_map, module_id: block_module_id });
                if parent.block(db).is_none() {
                    break;
                }
            }
            scopes.reverse();
        }
        Resolver {
            scopes,
            expr_stores: SmallVec::new(),
            module_scope: ModuleItemMap { def_map, local_def_map, module_id },
        }
    }
}

impl HasResolver for TraitId {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        lookup_resolver(db, self).push_generic_params_scope(db, self.into())
    }
}

impl<T: Into<AdtId> + Copy> HasResolver for T {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        let def = self.into();
        def.module(db).resolver(db).push_generic_params_scope(db, def.into())
    }
}

impl HasResolver for FunctionId {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        lookup_resolver(db, self).push_generic_params_scope(db, self.into())
    }
}

impl HasResolver for ConstId {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        // Consts can have generic params on nightly. Furthermore they're a `GenericDefId`,
        // so not pushing a generic params scope here complicates things (e.g. `TypeOwnerId` tracking).
        lookup_resolver(db, self).push_generic_params_scope(db, self.into())
    }
}

impl HasResolver for StaticId {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        lookup_resolver(db, self)
    }
}

impl HasResolver for TypeAliasId {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        lookup_resolver(db, self).push_generic_params_scope(db, self.into())
    }
}

impl HasResolver for ImplId {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        lookup_resolver(db, self).push_generic_params_scope(db, self.into())
    }
}

impl HasResolver for ExternBlockId {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        // Same as parent's
        lookup_resolver(db, self)
    }
}

impl HasResolver for ExternCrateId {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        lookup_resolver(db, self)
    }
}

impl HasResolver for UseId {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        lookup_resolver(db, self)
    }
}

impl HasResolver for DefWithBodyId {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        match self {
            DefWithBodyId::ConstId(c) => c.resolver(db),
            DefWithBodyId::FunctionId(f) => f.resolver(db),
            DefWithBodyId::StaticId(s) => s.resolver(db),
            DefWithBodyId::VariantId(v) => v.resolver(db),
        }
    }
}

impl HasResolver for ItemContainerId {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        match self {
            ItemContainerId::ModuleId(it) => it.resolver(db),
            ItemContainerId::TraitId(it) => it.resolver(db),
            ItemContainerId::ImplId(it) => it.resolver(db),
            ItemContainerId::ExternBlockId(it) => it.resolver(db),
        }
    }
}

impl HasResolver for GenericDefId {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        match self {
            GenericDefId::FunctionId(inner) => inner.resolver(db),
            GenericDefId::AdtId(adt) => adt.resolver(db),
            GenericDefId::TraitId(inner) => inner.resolver(db),
            GenericDefId::TypeAliasId(inner) => inner.resolver(db),
            GenericDefId::ImplId(inner) => inner.resolver(db),
            GenericDefId::ConstId(inner) => inner.resolver(db),
            GenericDefId::StaticId(inner) => inner.resolver(db),
        }
    }
}

impl HasResolver for ExpressionStoreOwnerId {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        match self {
            ExpressionStoreOwnerId::Signature(def) => def.resolver(db),
            ExpressionStoreOwnerId::Body(def) => def.resolver(db),
            ExpressionStoreOwnerId::VariantFields(variant_id) => variant_id.resolver(db),
        }
    }
}

impl HasResolver for EnumVariantId {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        lookup_resolver(db, self)
    }
}

impl HasResolver for VariantId {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        match self {
            VariantId::EnumVariantId(it) => it.resolver(db),
            VariantId::StructId(it) => it.resolver(db),
            VariantId::UnionId(it) => it.resolver(db),
        }
    }
}

impl HasResolver for MacroId {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        match self {
            MacroId::Macro2Id(it) => it.resolver(db),
            MacroId::MacroRulesId(it) => it.resolver(db),
            MacroId::ProcMacroId(it) => it.resolver(db),
        }
    }
}

impl HasResolver for Macro2Id {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        lookup_resolver(db, self)
    }
}

impl HasResolver for ProcMacroId {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        lookup_resolver(db, self)
    }
}

impl HasResolver for MacroRulesId {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        lookup_resolver(db, self)
    }
}

fn lookup_resolver(
    db: &Db,
    lookup: impl Lookup<Data = impl AstIdLoc<Container = impl HasResolver>>,
) -> Resolver<'_> {
    lookup.lookup(db).container().resolver(db)
}

#[cfg(test)]
mod tests;
