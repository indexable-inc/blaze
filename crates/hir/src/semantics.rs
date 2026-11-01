//! What the source means: [`view::Semantics`], and the scopes, resolutions and caches it shares
//! with the source analyzer.

mod child_by_source;
mod source_to_def;
pub mod view;

use std::{cell::RefCell, mem, ops};

use base_db::{Db, all_crates};
use blaze_hash::{HashMap, HashSet};
use either::Either;
use hir_def::{
    DefWithBodyId, ExpressionStoreOwnerId, GenericDefId, TraitId, VariantId,
    expr_store::{ExprOrPatSource, ExpressionStore, HygieneId, path::Path},
    hir::{ExprId, ExprOrPatId},
    resolver::{self, Resolver, TypeNs},
};
use hir_expand::{EditionedFileId, HirFileId, mod_path::ModPath};
use hir_ty::{
    InferBodyId, InferenceResult,
    db::{AnonConstId, HirDatabase as _},
    next_solver::AnyImplId,
};
use hir_ty_unsafeck::unsafe_operations;
use span::FileId;

use crate::{
    BuiltinAttr, ConstParam, Crate, DeriveHelper, ExpressionStoreOwner, Function, Impl, ItemInNs,
    Label, Local, Module, ModuleDef, Name, ScopeDef, ToolModule, Trait, Type, TypeAlias, TypeParam,
    semantics::source_to_def::{SourceToDefCache, SourceToDefCtx},
    source_analyzer::resolve_hir_path,
};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PathResolution<'db> {
    /// An item
    Def(ModuleDef),
    /// A local binding (only value namespace)
    Local(Local<'db>),
    /// A type parameter
    TypeParam(TypeParam),
    /// A const parameter
    ConstParam(ConstParam),
    SelfType(Impl),
    BuiltinAttr(BuiltinAttr),
    ToolModule(ToolModule),
    DeriveHelper(DeriveHelper),
}

impl<'db> PathResolution<'db> {
    pub(crate) fn in_type_ns(&self) -> Option<TypeNs> {
        match self {
            PathResolution::Def(ModuleDef::Adt(adt)) => Some(TypeNs::AdtId((*adt).into())),
            PathResolution::Def(ModuleDef::BuiltinType(builtin)) => {
                Some(TypeNs::BuiltinType((*builtin).into()))
            }
            PathResolution::Def(
                ModuleDef::Const(_)
                | ModuleDef::EnumVariant(_)
                | ModuleDef::Macro(_)
                | ModuleDef::Function(_)
                | ModuleDef::Module(_)
                | ModuleDef::Static(_)
                | ModuleDef::Trait(_),
            ) => None,
            PathResolution::Def(ModuleDef::TypeAlias(alias)) => {
                Some(TypeNs::TypeAliasId((*alias).into()))
            }
            PathResolution::BuiltinAttr(_)
            | PathResolution::ToolModule(_)
            | PathResolution::Local(_)
            | PathResolution::DeriveHelper(_)
            | PathResolution::ConstParam(_) => None,
            PathResolution::TypeParam(param) => Some(TypeNs::GenericParam((*param).into())),
            PathResolution::SelfType(impl_def) => match impl_def.id {
                AnyImplId::ImplId(id) => Some(TypeNs::SelfType(id)),
                AnyImplId::BuiltinDeriveImplId(_) => None,
            },
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct PathResolutionPerNs<'db> {
    pub type_ns: Option<PathResolution<'db>>,
    pub value_ns: Option<PathResolution<'db>>,
    pub macro_ns: Option<PathResolution<'db>>,
}

impl<'db> PathResolutionPerNs<'db> {
    pub fn new(
        type_ns: Option<PathResolution<'db>>,
        value_ns: Option<PathResolution<'db>>,
        macro_ns: Option<PathResolution<'db>>,
    ) -> Self {
        PathResolutionPerNs { type_ns, value_ns, macro_ns }
    }
    pub fn any(&self) -> Option<PathResolution<'db>> {
        self.type_ns.or(self.value_ns).or(self.macro_ns)
    }
}

#[derive(Debug)]
pub struct TypeInfo<'db> {
    /// The original type of the expression or pattern.
    pub original: Type<'db>,
    /// The adjusted type, if an adjustment happened.
    pub adjusted: Option<Type<'db>>,
}

impl<'db> TypeInfo<'db> {
    pub fn original(self) -> Type<'db> {
        self.original
    }

    pub fn has_adjustment(&self) -> bool {
        self.adjusted.is_some()
    }

    /// The adjusted type, or the original in case no adjustments occurred.
    pub fn adjusted(self) -> Type<'db> {
        self.adjusted.unwrap_or(self.original)
    }
}

type DefWithoutBodyWithAnonConsts = Either<GenericDefId, VariantId>;
type ExprToAnonConst<'db> = HashMap<ExprId, AnonConstId<'db>>;
type DefAnonConstsMap<'db> = HashMap<DefWithoutBodyWithAnonConsts, ExprToAnonConst<'db>>;

pub struct SemanticsImpl<'db> {
    pub db: &'db Db,
    s2d_cache: RefCell<SourceToDefCache<'db>>,
    /// All anon consts defined by a *signature* (not a body).
    signature_anon_consts_cache: RefCell<DefAnonConstsMap<'db>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LintAttr {
    #[default]
    Allow,
    Expect,
    Warn,
    Deny,
    Forbid,
}

impl Drop for SemanticsImpl<'_> {
    fn drop(&mut self) {
        mem::take(self.s2d_cache.get_mut()).give(self.db);
    }
}

impl<'db> SemanticsImpl<'db> {
    fn new(db: &'db Db) -> Self {
        SemanticsImpl {
            db,
            s2d_cache: RefCell::new(SourceToDefCache::take(db)),
            signature_anon_consts_cache: Default::default(),
        }
    }

    /// If not crate is found for the file, try to return the last crate in topological order.
    pub fn first_crate(&self, file: FileId) -> Option<Crate> {
        match self.file_to_module_defs(file).next() {
            Some(module) => Some(module.krate(self.db)),
            None => all_crates(self.db).last().copied().map(Into::into),
        }
    }

    pub fn attach_first_edition_opt(&self, file: FileId) -> Option<EditionedFileId> {
        let krate = self.file_to_module_defs(file).next()?.krate(self.db);
        Some(EditionedFileId::new(self.db, file, krate.edition(self.db)))
    }

    pub fn attach_first_edition(&self, file: FileId) -> EditionedFileId {
        self.attach_first_edition_opt(file)
            .unwrap_or_else(|| EditionedFileId::current_edition(self.db, file))
    }

    pub fn adjust_edition(&self, file_id: HirFileId) -> HirFileId {
        if let Some(editioned_file_id) = file_id.file_id() {
            self.attach_first_edition_opt(editioned_file_id.file_id(self.db))
                .map_or(file_id, Into::into)
        } else {
            file_id
        }
    }

    pub fn get_unsafe_ops(&self, def: ExpressionStoreOwner) -> HashSet<ExprOrPatSource> {
        let Ok(def) = ExpressionStoreOwnerId::try_from(def) else { return Default::default() };
        let (body, source_map) = ExpressionStore::with_source_map(self.db, def);
        let mut res = HashSet::default();
        self.with_all_infers_for_store(def, &mut |infer| {
            for root in body.expr_roots() {
                unsafe_operations(self.db, infer, def, body, root, &mut |node, _| {
                    if let Ok(node) = source_map.expr_or_pat_syntax(node) {
                        res.insert(node);
                    }
                });
            }
        });
        res
    }

    fn with_ctx<F: FnOnce(&mut SourceToDefCtx<'db, '_>) -> T, T>(&self, f: F) -> T {
        let mut ctx = SourceToDefCtx { db: self.db, cache: &mut self.s2d_cache.borrow_mut() };
        f(&mut ctx)
    }

    fn file_to_module_defs(&self, file: FileId) -> impl Iterator<Item = Module> {
        // alloc: copy out of the cache borrow, inline for the common single module
        self.with_ctx(|ctx| ctx.file_to_def(file).to_owned()).into_iter().map(Module::from)
    }
    fn populate_anon_const_cache_for<'a>(
        &self,
        cache: &'a mut DefAnonConstsMap<'db>,
        def: DefWithoutBodyWithAnonConsts,
    ) -> &'a ExprToAnonConst<'db> {
        cache.entry(def).or_insert_with(|| match def {
            Either::Left(def) => AnonConstId::all_from_signature(self.db, def)
                .map(|anon_const| (anon_const.loc(self.db).expr, anon_const))
                // alloc: cached once per signature
                .collect(),
            Either::Right(def) => {
                let all_anon_consts: &[AnonConstId<'db>] =
                    self.db.field_types_with_diagnostics(def).defined_anon_consts();
                all_anon_consts
                    .iter()
                    .map(|&anon_const| (anon_const.loc(self.db).expr, anon_const))
                    // alloc: cached once per signature
                    .collect()
            }
        })
    }

    fn find_anon_const_for_root_expr_in_signature(
        &self,
        def: DefWithoutBodyWithAnonConsts,
        root_expr: ExprId,
    ) -> Option<AnonConstId<'db>> {
        let mut cache = self.signature_anon_consts_cache.borrow_mut();
        let anon_consts_map = self.populate_anon_const_cache_for(&mut cache, def);
        anon_consts_map.get(&root_expr).copied()
    }

    pub(crate) fn infer_body_for_expr_or_pat(
        &self,
        def: ExpressionStoreOwnerId,
        store: &ExpressionStore,
        node: ExprOrPatId,
    ) -> Option<InferBodyId<'db>> {
        let handle_def_without_body = |def| {
            let root_expr = match node {
                ExprOrPatId::ExprId(expr) => store.find_root_for_expr(expr),
                ExprOrPatId::PatId(pat) => store.find_root_for_pat(pat),
            };
            let anon_const = self.find_anon_const_for_root_expr_in_signature(def, root_expr)?;
            Some(anon_const.into())
        };
        match def {
            ExpressionStoreOwnerId::Signature(def) => handle_def_without_body(Either::Left(def)),
            ExpressionStoreOwnerId::Body(def) => Some(def.into()),
            ExpressionStoreOwnerId::VariantFields(def) => {
                handle_def_without_body(Either::Right(def))
            }
        }
    }

    fn with_all_infers_for_store(
        &self,
        owner: ExpressionStoreOwnerId,
        callback: &mut impl FnMut(&'db InferenceResult<'db>),
    ) {
        let mut handle_def_without_body = |def| {
            let mut cache = self.signature_anon_consts_cache.borrow_mut();
            let map = self.populate_anon_const_cache_for(&mut cache, def);
            for &anon_const in map.values() {
                callback(InferenceResult::of(self.db, anon_const));
            }
        };
        match owner {
            ExpressionStoreOwnerId::Signature(def) => handle_def_without_body(Either::Left(def)),
            ExpressionStoreOwnerId::Body(def) => {
                callback(InferenceResult::of(self.db, def));
            }
            ExpressionStoreOwnerId::VariantFields(def) => {
                handle_def_without_body(Either::Right(def))
            }
        }
    }
}

/// `SemanticsScope` encapsulates the notion of a scope (the set of visible
/// names) at a particular program point.
///
/// It is a bit tricky, as scopes do not really exist inside the compiler.
/// Rather, the compiler directly computes for each reference the definition it
/// refers to. It might transiently compute the explicit scope map while doing
/// so, but, generally, this is not something left after the analysis.
///
/// However, we do very much need explicit scopes for IDE purposes --
/// completion, at its core, lists the contents of the current scope. The notion
/// of scope is also useful to answer questions like "what would be the meaning
/// of this piece of code if we inserted it into this position?".
///
/// So `SemanticsScope` is constructed from a specific program point (a syntax
/// node or just a raw offset) and provides access to the set of visible names
/// on a somewhat best-effort basis.
///
/// Note that if you are wondering "what does this specific existing name mean?",
/// you'd better use the `resolve_` family of methods.
#[derive(Debug)]
pub struct SemanticsScope<'db> {
    pub db: &'db Db,
    infer_body: Option<InferBodyId<'db>>,
    file_id: HirFileId,
    resolver: Resolver<'db>,
}

impl<'db> SemanticsScope<'db> {
    pub fn file_id(&self) -> HirFileId {
        self.file_id
    }

    pub fn module(&self) -> Module {
        Module { id: self.resolver.module() }
    }

    pub fn krate(&self) -> Crate {
        Crate { id: self.resolver.krate() }
    }

    // FIXME: This is a weird function, we shouldn't have this?
    pub fn containing_function(&self) -> Option<Function> {
        self.resolver.expression_store_owner().and_then(|owner| match owner {
            ExpressionStoreOwnerId::Body(DefWithBodyId::FunctionId(id)) => Some(id.into()),
            _ => None,
        })
    }

    pub fn expression_store_owner(&self) -> Option<ExpressionStoreOwner> {
        self.resolver.expression_store_owner().map(Into::into)
    }

    pub(crate) fn resolver(&self) -> &Resolver<'db> {
        &self.resolver
    }

    /// Note: `VisibleTraits` should be treated as an opaque type, passed into `Type
    pub fn visible_traits(&self) -> VisibleTraits {
        let resolver = &self.resolver;
        VisibleTraits(resolver.traits_in_scope(self.db))
    }

    /// Calls the passed closure `f` on all names in scope.
    pub fn process_all_names(&self, f: &mut impl FnMut(Name, ScopeDef<'db>)) {
        thread_local! {
            static NAMES_IN_SCOPE: RefCell<resolver::NamesInScope> =
                RefCell::new(resolver::NamesInScope::default());
        }
        // Taken rather than borrowed: `f` may ask another scope for its names.
        let mut scope = NAMES_IN_SCOPE.take();
        self.resolver.names_in_scope_into(self.db, &mut scope);
        for (name, entries) in scope.iter() {
            for &entry in entries {
                let def = match entry {
                    resolver::ScopeDef::ModuleDef(it) => ScopeDef::ModuleDef(it.into()),
                    resolver::ScopeDef::Unknown => ScopeDef::Unknown,
                    resolver::ScopeDef::ImplSelfType(it) => ScopeDef::ImplSelfType(it.into()),
                    resolver::ScopeDef::AdtSelfType(it) => ScopeDef::AdtSelfType(it.into()),
                    resolver::ScopeDef::GenericParam(id) => ScopeDef::GenericParam(id.into()),
                    resolver::ScopeDef::Local(binding_id) => {
                        match (self.resolver.expression_store_owner(), self.infer_body) {
                            (Some(parent), Some(parent_infer)) => {
                                ScopeDef::Local(Local { parent, parent_infer, binding_id })
                            }
                            _ => continue,
                        }
                    }
                    resolver::ScopeDef::Label(label_id) => {
                        match self.resolver.expression_store_owner() {
                            Some(parent) => ScopeDef::Label(Label { parent, label_id }),
                            None => continue,
                        }
                    }
                };
                f(*name, def)
            }
        }
        NAMES_IN_SCOPE.set(scope);
    }

    /// Checks if a trait is in scope, either because of an import or because we're in an impl of it.
    pub fn can_use_trait_methods(&self, t: Trait) -> bool {
        self.resolver.has_trait_in_scope(self.db, t.id)
    }

    /// What `path` would resolve to at this scope.
    pub fn speculative_resolve_mod_path(&self, path: &ModPath) -> Option<PathResolution<'db>> {
        resolve_hir_path(
            self.db,
            &self.resolver,
            self.infer_body,
            Path::BarePath(path),
            HygieneId::ROOT,
            None,
        )
    }

    pub fn resolve_mod_path(&self, path: &ModPath) -> impl Iterator<Item = ItemInNs> + use<> {
        let items = self.resolver.resolve_module_path_in_items(self.db, path);
        items.iter_items().map(|(item, _)| item.into())
    }

    /// Iterates over associated types that may be specified after the given path (using
    /// `Ty::Assoc` syntax).
    pub fn assoc_type_shorthand_candidates(
        &self,
        resolution: &PathResolution<'db>,
        mut cb: impl FnMut(TypeAlias),
    ) {
        let (Some(def), Some(resolution)) = (self.resolver.generic_def(), resolution.in_type_ns())
        else {
            return;
        };
        hir_ty::associated_type_shorthand_candidates(self.db, def, resolution, |_, id| {
            cb(id.into());
            false
        });
    }

    pub fn generic_def(&self) -> Option<crate::GenericDef> {
        self.resolver.generic_def().map(|id| id.into())
    }

    pub fn extern_crates(&self) -> impl Iterator<Item = (Name, Module)> + '_ {
        self.resolver.extern_crates_in_scope().map(|(name, id)| (name, Module { id }))
    }

    pub fn extern_crate_decls(&self) -> impl Iterator<Item = Name> + '_ {
        self.resolver.extern_crate_decls_in_scope(self.db)
    }

    pub fn has_same_self_type(&self, other: &SemanticsScope<'_>) -> bool {
        self.resolver.impl_def() == other.resolver.impl_def()
    }
}

#[derive(Debug)]
pub struct VisibleTraits(pub HashSet<TraitId>);

impl ops::Deref for VisibleTraits {
    type Target = HashSet<TraitId>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// `Itertools::kmerge_by` of at most two iterators (what `token_at_offset` yields), without
/// its heap: the same order, the iterator that gave the last item staying first on a tie.
pub(crate) struct KMerge2<I: Iterator, F> {
    first: Option<(I::Item, I)>,
    second: Option<(I::Item, I)>,
    less: F,
}

pub(crate) fn kmerge2<I: Iterator, F: FnMut(&I::Item, &I::Item) -> bool>(
    mut iters: impl Iterator<Item = I>,
    mut less: F,
) -> KMerge2<I, F> {
    let head = |it: Option<I>| it.and_then(|mut it| Some((it.next()?, it)));
    let mut first = head(iters.next());
    let mut second = head(iters.next());
    debug_assert!(iters.next().is_none(), "`kmerge2` of more than two iterators");
    if first.is_none() {
        mem::swap(&mut first, &mut second);
    }
    if let (Some(a), Some(b)) = (&first, &second)
        && less(&b.0, &a.0)
    {
        mem::swap(&mut first, &mut second);
    }
    KMerge2 { first, second, less }
}

impl<I: Iterator, F: FnMut(&I::Item, &I::Item) -> bool> Iterator for KMerge2<I, F> {
    type Item = I::Item;

    fn next(&mut self) -> Option<I::Item> {
        let (head, tail) = self.first.as_mut()?;
        let Some(next) = tail.next() else {
            return mem::replace(&mut self.first, self.second.take()).map(|(item, _)| item);
        };
        let item = mem::replace(head, next);
        if let (Some(a), Some(b)) = (&self.first, &self.second)
            && (self.less)(&b.0, &a.0)
        {
            mem::swap(&mut self.first, &mut self.second);
        }
        Some(item)
    }
}
