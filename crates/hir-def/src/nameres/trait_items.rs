//! The associated items of a trait as an import reads them: rustc's traits are modules to
//! `use`, so `use Trait::item` and `use Trait::*` bind the trait's functions, constants and
//! types (`import_trait_associated_functions`; without it, rustc's `finalize_imports` still
//! binds them and reports E0658).
//!
//! [`TraitItems`] expands the macros of a trait's body through the def map of the trait's
//! crate, so it cannot run while that def map is collected. A trait in the def map being
//! collected is read from its syntax instead, as [`TraitItems`] would list it: the same items,
//! the same ids. Where that needs a macro, the lookup stops with [`ItemsFromMacros`].

use std::sync::LazyLock;

use base_db::Db;
use hir_expand::{
    InFile, Intern, Lookup,
    name::{AsName, Name},
};
use intern::Symbol;
use syntax::{
    ast,
    view::{self, HasName},
};

use crate::{
    AssocItemId, ConstLoc, FunctionLoc, ItemContainerId, ItemLoc, ModuleDefId, TraitId,
    TypeAliasLoc,
    item_tree::{AttrsOrCfg, ImportKind},
    nameres::{DefMap, assoc::TraitItems, diagnostics::TraitItemImport},
    per_ns::PerNs,
    visibility::Visibility,
};

static FEATURE: LazyLock<Symbol> =
    LazyLock::new(|| Symbol::intern("import_trait_associated_functions"));

/// A trait in the def map being collected has an item that comes from a macro, or an attribute
/// that may be an attribute macro's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ItemsFromMacros;

impl DefMap {
    /// Calls `f` with each associated item of `trait_` an import from it binds, and its name,
    /// in declaration order, as this def map (being collected, or collected) can see them.
    /// Allocates nothing: a trait of this def map is read from its syntax as it goes, so on
    /// [`ItemsFromMacros`] `f` has seen the items before the one that stopped it. `collected`:
    /// this def map is complete (a dependency's a path reached), so its own traits are listed
    /// by [`TraitItems`] too.
    pub(crate) fn for_each_trait_item_for_import(
        &self,
        db: &Db,
        trait_: TraitId,
        collected: bool,
        mut f: impl FnMut(Name, AssocItemId),
    ) -> Result<(), ItemsFromMacros> {
        let ItemLoc { container: module, id } = *trait_.lookup(db);
        if collected || module.krate(db) != self.krate || module.block(db) != self.block_id() {
            for &(name, item) in &TraitItems::query(db, trait_).items {
                f(name, item);
            }
            return Ok(());
        }
        let source = id.to_view(db);
        // FIXME(trait-alias) as `TraitItems::query_with_diagnostics`: an alias lists no items.
        if source.eq_token().is_some() {
            return Ok(());
        }
        let Some(list) = source.assoc_item_list() else { return Ok(()) };
        let file_id = id.file_id;
        let ast_id_map = file_id.ast_id_map(db);
        let span_map = file_id.span_map(db);
        let cfg_options = module.krate(db).cfg_options(db);
        let container = ItemContainerId::TraitId(trait_);
        for item in list.assoc_items() {
            let attrs = match AttrsOrCfg::lower(db, item, &|| cfg_options, span_map) {
                AttrsOrCfg::Enabled { attrs } => attrs,
                AttrsOrCfg::CfgDisabled(_) => continue,
            };
            if attrs.as_ref().iter().any(|(_, attr)| !self.is_builtin_or_registered_attr(&attr.path))
            {
                return Err(ItemsFromMacros);
            }
            let item = match item {
                view::AssocItem::Fn(it) => it.name().map(|name| {
                    let id = InFile::new(file_id, ast_id_map.ast_id_of_view::<ast::Fn>(it));
                    (name, AssocItemId::from(FunctionLoc { container, id }.intern(db)))
                }),
                view::AssocItem::Const(it) => it.name().map(|name| {
                    let id = InFile::new(file_id, ast_id_map.ast_id_of_view::<ast::Const>(it));
                    (name, AssocItemId::from(ConstLoc { container, id }.intern(db)))
                }),
                view::AssocItem::TypeAlias(it) => it.name().map(|name| {
                    let id = InFile::new(file_id, ast_id_map.ast_id_of_view::<ast::TypeAlias>(it));
                    (name, AssocItemId::from(TypeAliasLoc { container, id }.intern(db)))
                }),
                view::AssocItem::MacroCall(_) => return Err(ItemsFromMacros),
            };
            if let Some((name, item)) = item {
                f(name.as_name(), item);
            }
        }
        Ok(())
    }

    /// What rustc's `finalize_imports` reports of an import of `kind` that resolved to `res`:
    /// without `import_trait_associated_functions`, a glob of a trait, or each namespace a
    /// single import binds a trait's item in (types, then values); and a glob of a trait whose
    /// items blaze cannot list. Calls `f` with each, in that order.
    pub(crate) fn trait_item_import_reports(
        &self,
        db: &Db,
        kind: ImportKind,
        res: PerNs,
        mut f: impl FnMut(TraitItemImport),
    ) {
        let enabled = self.imports_trait_items();
        let (types, values) = (res.types.map(|it| it.def), res.values.map(|it| it.def));
        if kind == ImportKind::Glob {
            if let Some(ModuleDefId::TraitId(it)) = types {
                match self.for_each_trait_item_for_import(db, it, false, |_, _| {}) {
                    Err(ItemsFromMacros) => f(TraitItemImport::ItemsFromMacros),
                    Ok(()) if !enabled => f(TraitItemImport::Unstable),
                    Ok(()) => {}
                }
            }
            return;
        }
        for def in [types, values].into_iter().flatten() {
            if !enabled && is_trait_item(db, def) {
                f(TraitItemImport::Unstable);
            }
        }
    }

    /// Whether the crate enables `import_trait_associated_functions`.
    pub(crate) fn imports_trait_items(&self) -> bool {
        self.data.unstable_features.is_enabled(&FEATURE)
    }
}

/// Whether `def` is an associated item of a trait: what an import binds only from a trait.
pub(crate) fn is_trait_item(db: &Db, def: ModuleDefId) -> bool {
    let container = match def {
        ModuleDefId::FunctionId(it) => it.lookup(db).container,
        ModuleDefId::ConstId(it) => it.lookup(db).container,
        ModuleDefId::TypeAliasId(it) => it.lookup(db).container,
        _ => return false,
    };
    matches!(container, ItemContainerId::TraitId(_))
}

/// What an import binds of `item`, with visibility `vis`: a function or constant in the values
/// namespace, a type in the types namespace.
pub(crate) fn trait_item_per_ns(item: AssocItemId, vis: Visibility) -> PerNs {
    match item {
        AssocItemId::FunctionId(it) => PerNs::values(it.into(), vis, None),
        AssocItemId::ConstId(it) => PerNs::values(it.into(), vis, None),
        AssocItemId::TypeAliasId(it) => PerNs::types(it.into(), vis, None),
    }
}
