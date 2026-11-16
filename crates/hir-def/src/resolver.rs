//! Name resolution façade: the scopes of a position (generic parameters, a body's local
//! bindings and macro definitions, blocks) over the scope of its module, which is read from
//! the records one name at a time ([`crate::nameres::resolve_path`]; reconnect.md, "The
//! resolver"). A name whose module scope record the task lacks stops it with a `Need` naming
//! that scope's key. Which import brought a name is not in the records, so the resolver
//! answers no question about uses of imports (the unused-import lint; parked.md).
use std::{fmt, iter, ops::ControlFlow};

use base_db::{Crate, Db};
use blaze_hash::HashSet;
use hir_expand::{
    MacroDefId,
    mod_path::{ModPath, PathKind},
    name::{AsName, Name},
};
use intern::{Symbol, sym};
use smallvec::SmallVec;
use span::SyntaxContext;
use std::alloc::Global;
use syntax::view::HasName;

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
    item_scope::{BuiltinShadowMode, ImportOrExternCrate, ItemScope},
    lang_item::LangItemTarget,
    nameres::{
        DefMap, MacroSubNs, ResolvePathResultPrefixInfo, block_def_map,
        def_site::{Hygienic, hygienic_origin},
    },
    per_ns::{MacrosItem, Namespace, PerNs},
    signatures::ImplSignature,
    src::HasSource,
    type_ref::LifetimeRef,
    visibility::{RawVisibility, Visibility, VisibilityError},
};

/// What [`Resolver::names_in_scope`] returns: each name with its meanings, innermost first.
pub type NamesInScope = FxIndexMap<Name, SmallVec<[ScopeDef; 1]>>;

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

/// The item scope of one module (a block's, or the resolver's own module), read from its
/// records per name.
#[derive(Clone, Copy)]
struct ModuleItemMap<'db> {
    def_map: &'db DefMap,
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
    /// All the items and imported names of a block's module. No record holds them yet
    /// (reconnect.md, "What the new-layer crates lack"): a lookup in one stops the task.
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
    pub fn resolve_known_struct(&self, db: &Db, path: &ModPath) -> Option<StructId> {
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
        self.resolve_path_in_type_ns_hygienic(db, path, HygieneId::ROOT)
    }

    /// [`Self::resolve_path_in_type_ns_with_prefix_info`] of a path whose first segment has
    /// `hygiene`: a path a `macro` wrote is looked up in the item scopes its def-site hygiene
    /// says ([`hygienic_origin`]).
    pub fn resolve_path_in_type_ns_hygienic(
        &self,
        db: &Db,
        path: Path<'_>,
        hygiene: HygieneId,
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
        let hygienic = self.hygienic_items(db, path, Namespace::Types, hygiene);
        let skip_to_mod = path.kind != PathKind::Plain;
        // Generic parameters have macros 2.0 hygiene ([`crate::nameres::param_name`]): the
        // name they are looked up by, taken at the first generic parameters met.
        let mut keyed = None;
        let mut param_name = || {
            *keyed.get_or_insert_with(|| crate::nameres::path_param_name(db, self.ctx_table(), *first_name, hygiene))
        };
        if skip_to_mod {
            return match hygienic {
                Some(items) => {
                    let (items, path) = items?;
                    items.resolve_path_in_type_ns(db, &path)
                }
                None => self.skip_to_mod(|scope| scope.resolve_path_in_type_ns(db, path)),
            };
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
                    if let Some(id) = params.find_type_by_name(&param_name(), def) {
                        return Some((
                            TypeNs::GenericParam(id),
                            remaining_idx(),
                            None,
                            ResolvePathResultPrefixInfo::default(),
                            Visibility::Public,
                        ));
                    }
                }
                // The scope def-site hygiene says, below.
                Scope::BlockScope { .. } if hygienic.is_some() => continue,
                Scope::BlockScope { def_map, module_id } => {
                    let m = self.block_items(def_map, module_id);
                    if let Some(res) = m.resolve_path_in_type_ns(db, path) {
                        let res = match res.0 {
                            TypeNs::ModuleId(_) if res.1.is_none() => {
                                let builtin = match crate::nameres::builtin(first_name) {
                                    found if !found.is_none() => found,
                                    _ => def_map.arbitrary_int(db, first_name),
                                };
                                if let Some(ModuleDefId::BuiltinType(builtin)) =
                                    builtin.take_types()
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
        match hygienic {
            Some(items) => {
                let (items, path) = items?;
                items.resolve_path_in_type_ns(db, &path)
            }
            None => self.module_scope.resolve_path_in_type_ns(db, path),
        }
    }

    pub fn resolve_path_in_type_ns_fully(&self, db: &Db, path: Path<'_>) -> Option<TypeNs> {
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
            RawVisibility::Module(..) => {
                let (item_map, module) = self.item_scope_();
                item_map.resolve_visibility(
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
        // Generic parameters have macros 2.0 hygiene ([`crate::nameres::param_name`]): the
        // name they are looked up by, of the path's own hygiene (not `hygiene_id` as a macro
        // scope changes it), taken at the first generic parameters met.
        let path_hygiene = hygiene_id;
        let mut keyed = None;
        let mut param_name = || {
            *keyed.get_or_insert_with(|| {
                crate::nameres::path_param_name(db, self.ctx_table(), *first_name, path_hygiene)
            })
        };
        let skip_to_mod = path.kind != PathKind::Plain && !path.is_self();
        // Where the item scopes are for a path a `macro` wrote: its locals are told apart by
        // `hygiene_id` below, its items by def-site hygiene.
        let ns = if n_segments <= 1 { Namespace::Values } else { Namespace::Types };
        let hygienic = self.hygienic_items(db, path, ns, hygiene_id);
        if skip_to_mod {
            return match hygienic {
                Some(items) => {
                    let (items, path) = items?;
                    items.resolve_path_in_value_ns(db, &path)
                }
                None => self.skip_to_mod(|scope| scope.resolve_path_in_value_ns(db, path)),
            };
        }

        if n_segments <= 1 {
            // Read at the first macro scope the walk meets, before `hygiene_id` changes there.
            let mut macro_hygiene = None;
            for scope in self.scopes() {
                match scope {
                    Scope::ExprScope { store, scope_id } => {
                        // A name a pattern binds twice means its last binding.
                        let entry =
                            self.expr_scopes(store).entries(scope_id).iter().rev().find(|entry| {
                                entry.name() == first_name && entry.hygiene() == hygiene_id
                            });

                        if let Some(e) = entry {
                            return Some((
                                ResolveValueResult::ValueNs(ValueNs::LocalBinding(e.binding())),
                                ResolvePathResultPrefixInfo::default(),
                                Visibility::Public,
                            ));
                        }
                    }
                    Scope::MacroDefScope { store, scope_id } => {
                        let info =
                            macro_hygiene.get_or_insert_with(|| hygiene_info(db, self.ctx_table(), hygiene_id));
                        handle_macro_def_scope(db, self.ctx_table(), &mut hygiene_id,
                        info,
                        self.macro_def(store, scope_id),)
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
                        if let Some(id) = params.find_const_by_name(&param_name(), def) {
                            let val = ValueNs::GenericParam(id);
                            return Some((
                                ResolveValueResult::ValueNs(val),
                                ResolvePathResultPrefixInfo::default(),
                                Visibility::Public,
                            ));
                        }
                    }
                    // The scope def-site hygiene says, below.
                    Scope::BlockScope { .. } if hygienic.is_some() => continue,
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
                        if let Some(id) = params.find_type_by_name(&param_name(), def) {
                            let ty = TypeNs::GenericParam(id);
                            return Some((
                                ResolveValueResult::Partial(ty, 1),
                                ResolvePathResultPrefixInfo::default(),
                                Visibility::Public,
                            ));
                        }
                    }
                    // The scope def-site hygiene says, below.
                    Scope::BlockScope { .. } if hygienic.is_some() => continue,
                    Scope::BlockScope { def_map, module_id } => {
                        let m = self.block_items(def_map, module_id);
                        if let Some(def) = m.resolve_path_in_value_ns(db, path) {
                            return Some(def);
                        }
                    }
                }
            }
        }

        let items = match hygienic {
            Some(items) => items,
            None => Some((self.module_scope, *path)),
        };
        if let Some(res) = items.and_then(|(items, path)| items.resolve_path_in_value_ns(db, &path))
        {
            return Some(res);
        }

        // If a path of the shape `u16::from_le_bytes` failed to resolve at all, then we fall back
        // to resolving to the primitive type, to allow this to still work in the presence of
        // `use core::u16;`.
        if path.kind == PathKind::Plain
            && n_segments > 1
            && let Some(builtin) = BuiltinType::by_name(first_name).or_else(|| {
                let arbitrary = self.module_scope.def_map.arbitrary_int(db, first_name);
                match arbitrary.take_types() {
                    Some(ModuleDefId::BuiltinType(it)) => Some(it),
                    _ => None,
                }
            })
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
        let (item_map, module) = self.item_scope_();
        // A one-segment macro path is looked up in the `macro_rules!` textual scope at its
        // position first: the module's `TextualEntry` record, then the module's own definitions
        // and `#[macro_use]` modules above the position, then the `#[macro_use]` prelude. The
        // position is not known here: a typed stop (reconnect.md, "What the new-layer crates
        // lack").
        if path.kind == PathKind::Plain && path.segments().len() == 1 {
            let item = crate::nameres::scope_key(db, module);
            return base_db::OrNeed::or_need(Err(base_db::SurfaceError::NotBuilt(
                blaze_surface::NotBuilt { method: "the textual macro scope at a position", item },
            )));
        }
        item_map
            .resolve_path(db, module, path, BuiltinShadowMode::Other, expected_macro_kind)
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
        let ModuleItemMap { def_map, module_id } = self.module_scope;
        let scope = def_map.module(db, module_id).scope(db);
        scope.entries().for_each(|(name, def)| res.add_per_ns(name, def));
        for (name, macs) in scope.legacy_macros(db) {
            macs.iter().for_each(|&mac| res.add(name, ScopeDef::ModuleDef(ModuleDefId::MacroId(mac))));
        }
        for (name, (def, _)) in module_id.macro_use_prelude(db) {
            res.add(name, ScopeDef::ModuleDef(def.into()));
        }
        for (name, (def, _extern_crate)) in module_id.extern_prelude(db) {
            res.add(name, ScopeDef::ModuleDef(ModuleDefId::ModuleId(def)));
        }
        for (name, ty) in BuiltinType::all_builtin_types() {
            res.add_per_ns(&name, PerNs::types(ModuleDefId::BuiltinType(ty), Visibility::Public, None));
        }
        if let Some((prelude, _use)) = def_map.prelude(db) {
            for (name, def) in prelude.def_map(db).module(db, prelude).scope(db).entries() {
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

    pub fn extern_crates_in_scope<'a>(
        &'a self,
        db: &'a Db,
    ) -> impl Iterator<Item = (Name, ModuleId)> + 'a {
        self.module_scope.module_id.extern_prelude(db).map(|(name, (root, _))| (*name, root))
    }

    pub fn traits_in_scope(&self, db: &Db) -> HashSet<TraitId, Global> {
        let mut traits = HashSet::new_in(Global);
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
                        && let Some(TypeNs::TraitId(trait_)) = self.resolve_path_in_type_ns_fully(
                            db,
                            impl_data.store.path(target_trait.path),
                        )
                    {
                        f(trait_)?;
                    }
                }
                _ => (),
            }
        }
        if let Some((prelude, _use)) = self.module_scope.def_map.prelude(db) {
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
        self.item_scope_().1
    }

    pub fn item_scope<'a>(&'a self, db: &'a Db) -> &'a ItemScope {
        let (def_map, local_id) = self.item_scope_();
        def_map.module(db, local_id).scope(db)
    }

    pub fn krate(&self) -> Crate {
        self.module_scope.def_map.krate()
    }

    /// The arena whose syntax-context table the hygiene of what this resolver resolves is of:
    /// its crate's, where the expansions of the bodies it is in made their contexts
    /// ([`crate::nameres::crate_table`]).
    pub fn ctx_table(&self) -> blaze_memo::ArenaId {
        crate::nameres::crate_table(self.krate())
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

    /// The module the privacy of a field or associated item named here with `hygiene` is
    /// checked from, among the fields of an ADT or the items of an impl that `expn` made: the
    /// def site [`crate::nameres::def_site::privacy_scope`] gives; `None` for a name checked from
    /// here (rustc's `adjust_ident_and_get_scope`).
    pub fn privacy_scope(
        &self,
        db: &Db,
        hygiene: HygieneId,
        expn: Option<hir_expand::MacroCallId>,
    ) -> Option<ModuleId> {
        match hygiene.is_root() {
            true => None,
            false => crate::nameres::def_site::privacy_scope(
                db,
                self.ctx_table(),
                hygiene.syntax_context(),
                expn,
                self.module(),
            ),
        }
    }

    /// Whether `visibility` admits a name written here with `hygiene`: from
    /// [`Resolver::privacy_scope`], else as [`Resolver::is_visible`].
    pub fn is_visible_written(
        &self,
        db: &Db,
        visibility: Visibility,
        hygiene: HygieneId,
        expn: Option<hir_expand::MacroCallId>,
    ) -> bool {
        match self.privacy_scope(db, hygiene, expn) {
            Some(scope) => visibility.is_visible_from(db, scope),
            None => self.is_visible(db, visibility),
        }
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
        let mut hygiene_info = hygiene_info(db, self.ctx_table(), hygiene_id);
        let param_name = crate::nameres::path_param_name(db, self.ctx_table(), *current_name, hygiene_id);
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
                Scope::MacroDefScope { store, scope_id } => handle_macro_def_scope(db, self.ctx_table(), &mut hygiene_id,
                &mut hygiene_info,
                self.macro_def(store, scope_id),),
                Scope::GenericParams { params, def } => {
                    if params.find_const_by_name(&param_name, def).is_some() {
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
        let mut hygiene_info = hygiene_info(db, self.ctx_table(), hygiene_id);
        let param_name = crate::nameres::path_param_name(db, self.ctx_table(), *name, hygiene_id);
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
                Scope::MacroDefScope { store, scope_id } => handle_macro_def_scope(db, self.ctx_table(), &mut hygiene_id,
                &mut hygiene_info,
                self.macro_def(store, scope_id),),
                Scope::GenericParams { params, def } => {
                    if params.find_const_by_name(&param_name, def).is_some() {
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
                resolver.scopes.push(Scope::BlockScope { def_map, module_id: def_map.root_module_id() });
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
    table: blaze_memo::ArenaId,
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
        *hygiene_id = HygieneId::new(parent_ctx.opaque_and_semiopaque(db, table));
        *hygiene_info = parent_ctx.outer_expn(db, table).map(|expansion| {
            let expansion = hir_expand::MacroCallId::from(expansion).loc(db);
            (parent_ctx.parent(db, table), expansion.def)
        });
    }
}

#[inline]
fn hygiene_info(
    db: &Db,
    table: blaze_memo::ArenaId,
    hygiene_id: HygieneId,
) -> Option<(SyntaxContext, MacroDefId)> {
    if !hygiene_id.is_root() {
        let ctx = hygiene_id.syntax_context();
        ctx.outer_expn(db, table).map(|expansion| {
            let expansion = hir_expand::MacroCallId::from(expansion).loc(db);
            (ctx.parent(db, table), expansion.def)
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

    /// The items of a block's module.
    fn block_items(&self, def_map: &'db DefMap, module_id: ModuleId) -> ModuleItemMap<'db> {
        ModuleItemMap { def_map, module_id }
    }

    /// The item scope a path whose first segment has `hygiene` is looked up in, by a `macro`'s
    /// def-site hygiene ([`hygienic_origin`]): `None` where it is written (the blocks around
    /// it, then the module). A path a `macro` wrote with a def-site context stops there: the
    /// `Expansion` records that would say where are not built. The `Option<Option<..>>` shape
    /// is kept for the def-site answer, `Some(..)` once those records exist.
    fn hygienic_items<'a>(
        &'a self,
        db: &'a Db,
        _path: &ModPath,
        _ns: Namespace,
        hygiene: HygieneId,
    ) -> Option<Option<(ModuleItemMap<'a>, ModPath)>> {
        match hygienic_origin(db, self.ctx_table(), self.module(), hygiene) {
            Hygienic::Here => None,
        }
    }

    fn resolve_module_path(&self, db: &Db, path: &ModPath, shadow: BuiltinShadowMode) -> PerNs {
        let (item_map, module) = self.item_scope_();
        // This method resolves `path` just like import paths, so no expected macro subns is given.
        let (module_res, segment_index, _) = item_map.resolve_path(db, module, path, shadow, None);
        if segment_index.is_some() {
            return PerNs::none();
        }
        module_res
    }

    /// The innermost block scope that contains items or the module scope that contains this resolver.
    fn item_scope_(&self) -> (&'db DefMap, ModuleId) {
        self.scopes()
            .find_map(|scope| match scope {
                Scope::BlockScope { def_map, module_id } => Some((def_map, module_id)),
                _ => None,
            })
            .unwrap_or((self.module_scope.def_map, self.module_scope.module_id))
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
                let scope = def_map.module(db, module_id).scope(db);
                scope.entries().for_each(|(name, def)| acc.add_per_ns(name, def));
                for (name, macs) in scope.legacy_macros(db) {
                    macs.iter().for_each(|&mac| {
                        acc.add(name, ScopeDef::ModuleDef(ModuleDefId::MacroId(mac)));
                    })
                }
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
            r.scopes.push(Scope::BlockScope { def_map, module_id: def_map.root_module_id() });
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

    fn push_generic_params_scope(self, db: &'db Db, def: GenericDefId) -> Resolver<'db> {
        let params = GenericParams::of(db, def);
        self.push_scope(Scope::GenericParams { def, params })
    }
}

impl<'db> ModuleItemMap<'db> {
    /// `path` in this scope: one probe of the module's scope record per segment.
    fn resolve_locally(
        &self,
        db: &Db,
        path: &ModPath,
    ) -> (PerNs, Option<usize>, ResolvePathResultPrefixInfo) {
        self.def_map.resolve_path(db, self.module_id, path, BuiltinShadowMode::Other, None)
    }

    fn resolve_path_in_value_ns(
        &self,
        db: &'db Db,
        path: &ModPath,
    ) -> Option<(ResolveValueResult, ResolvePathResultPrefixInfo, Visibility)> {
        let (module_def, unresolved_idx, prefix_info) = self.resolve_locally(db, path);
        match unresolved_idx {
            None => {
                let (value, vis) = to_value_ns(db, module_def, self.def_map)?;
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
        let (module_def, idx, prefix_info) = self.resolve_locally(db, path);
        let (res, import, vis) = to_type_ns(module_def)?;
        Some((res, idx, import, prefix_info, vis))
    }

}

fn to_value_ns(db: &Db, per_ns: PerNs, def_map: &DefMap) -> Option<(ValueNs, Visibility)> {
    let (def, vis) = per_ns.take_values_full().map(|res| (res.def, res.vis)).or_else(|| {
        let Some(MacrosItem { def: MacroId::ProcMacroId(proc_macro), vis, .. }) =
            per_ns.take_macros_full()
        else {
            return None;
        };
        // If we cannot resolve to value ns, but we can resolve to a proc macro, and this is the crate
        // defining this proc macro - inside this crate, we should treat the macro as a function.
        let def = ModuleDefId::FunctionId(def_map.proc_macro_as_fn(db, proc_macro)?);
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
        let mut module_id = self;
        let mut scopes = SmallVec::new();
        // A block's module: its blocks, innermost first then reversed, so the stack holds
        // outer blocks first; the module scope is the first module with a key around them.
        while let Some(block) = module_id.block(db) {
            scopes.push(Scope::BlockScope { def_map: block_def_map(db, block), module_id });
            module_id = block.module(db);
        }
        scopes.reverse();
        let def_map = crate::nameres::crate_def_map(db, module_id.krate(db));
        Resolver {
            scopes,
            expr_stores: SmallVec::new(),
            module_scope: ModuleItemMap { def_map, module_id },
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
    lookup: impl Lookup<Data = impl AstIdLoc<Container = impl HasResolver>> + Copy,
) -> Resolver<'_> {
    lookup.lookup(db).container().resolver(db)
}
