//! The lints as queries of the front end's database: one per body, over the tree of the anchor
//! the body is written in ([`body_syntax`]), and one per file for what is not in a body. A
//! value holds what was found with ranges of the query's tree and no file names, so an edit
//! that moves a body without changing it backdates its value, and the driver
//! ([`crate::check`]) moves and names what it found.
//!
//! [`body_syntax`]: hir_def::expr_store::body::body_syntax

#[cfg(test)]
mod tests;

use base_db::{Db, EditionedFileId};
use blaze_hash::HashSet;
use hir::HasVisibility as _;
use hir_def::{
    AdtId, AssocItemId, DefWithBodyId, ImplId, ItemContainerId, Lookup as _, ModuleDefId,
    nameres::crate_def_map, visibility::Visibility,
};
use syntax::{TextRange, TextSize};

use crate::{
    blaze, check,
    conf::Conf,
    diag::Diag,
    famous::{self, Famous},
    lints,
};

/// The lints a query runs, which may be above `allow` where it looks, and the configuration
/// they read.
#[blaze_query::interned(debug, unsafe(no_lifetime))]
pub(crate) struct LintSet {
    #[returns(copy)]
    pub(crate) on: lints::On,
    #[returns(ref)]
    pub(crate) conf: Conf,
}

/// What one query found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Lints {
    pub(crate) findings: Vec<Finding>,
    /// The `#[expect]`s of the lints run, on what the query walked of the file.
    pub(crate) expects: Vec<Expect>,
}

pub(crate) type BodyLints = Lints;
pub(crate) type ItemLints = Lints;

/// A lint the query found, before its level is known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Finding {
    pub(crate) lint: Found,
    /// Where its level is asked for: the node it was found at as written in the query's tree,
    /// or for a node a macro made up, the call of the macro there.
    pub(crate) at: TextRange,
    /// Where rustc's walk over the crate comes to it.
    pub(crate) place: Place,
    /// Its spans are of the query's tree, but for those of other files.
    pub(crate) diagnostic: Diag,
}

/// Which lint a finding is of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Found {
    Clippy(lints::Implemented),
    Blaze(blaze::Lint),
}

/// Where in rustc's walk a lint is emitted, as an order key of the query's tree: outer nodes
/// before the nodes inside them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    /// As the walk comes to a node written at this range.
    Node(TextRange),
    /// At `at`, before or after everything else there by `then`.
    At { at: u32, then: u32 },
}

/// An `#[expect]` of a lint run, as the walk met it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Expect {
    /// The lint's name in the attribute.
    pub(crate) name: TextRange,
    pub(crate) reason: Option<String>,
    /// The item, statement or expression the attribute is on.
    pub(crate) owner: TextRange,
}

/// The lints of `set` over the body of `def`, from the tree of its anchor, or of the macro
/// expansion that writes it. Typed lints infer the body the first time they ask.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn body_lints(db: &Db, def: DefWithBodyId, set: LintSet) -> BodyLints {
    check::lint_body(db, def, set.on(db), set.conf(db))
}

/// The lints of `set` over what of `file` is in no body, as `krate` reads it: its items and
/// what attribute macros and macro calls make of them. A body is never walked into.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn item_lints(
    db: &Db,
    file: EditionedFileId,
    krate: base_db::Crate,
    set: LintSet,
) -> ItemLints {
    check::lint_items(db, file, krate, set.on(db), set.conf(db))
}

/// A lint name written in a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Named {
    pub(crate) at: TextRange,
    /// What the attribute it is written in is on; the name itself when it is in none.
    pub(crate) owner: TextRange,
    pub(crate) lints: lints::On,
}

/// The lint names a file's text writes, with every lint they name.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct FileNames {
    pub(crate) names: Box<[Named]>,
    /// The union of the names' lints.
    pub(crate) on: lints::On,
}

/// The lint names `file` writes: one scan of its text, again only when the text changes.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn lint_names(db: &Db, file: EditionedFileId) -> FileNames {
    check::names_of(db, file)
}

/// A file of a crate's modules, as the driver reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ModuleFile {
    pub(crate) file: EditionedFileId,
    /// The file and range of the `mod` item declaring its module, for all but the root.
    pub(crate) declared: Option<(EditionedFileId, TextRange)>,
}

/// A body the driver lints, and where the tree of its query is in its file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BodyAt {
    pub(crate) def: DefWithBodyId,
    pub(crate) file: EditionedFileId,
    /// Where the body's anchor starts in the file; 0 for a body a macro writes.
    pub(crate) offset: TextSize,
    /// Its item as written in the file, or the outermost macro call writing it.
    pub(crate) item: TextRange,
}

/// What the driver walks of a crate: the files of its modules, each once, in the order of its
/// modules, and its bodies in the order they are linted.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    pub(crate) files: Box<[ModuleFile]>,
    pub(crate) bodies: Box<[BodyAt]>,
}

/// The [`Plan`] of `krate`: from its def map, the bodies' blocks and their anchors, so a run
/// with nothing changed is one hit here, not several per body.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn lint_plan(db: &Db, krate: base_db::Crate) -> Plan {
    check::plan_of(db, krate)
}

/// What `krate` resolves each path of [`famous::KNOWN`] to, by index: once per crate, so a
/// lint's lookup is a hash of its path and one load, not a walk of `std`'s scopes.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn famous_defs(db: &Db, krate: base_db::Crate) -> Box<[Option<hir::ModuleDef>]> {
    let famous = Famous::new(db, hir::Crate::from(krate));
    // alloc: the query's value, once per crate
    famous::KNOWN.iter().map(|path| famous.find(path)).collect()
}

/// What of a crate another crate can name: rustc's `effective_visibilities.is_exported`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Exported {
    /// The definitions named by a public path from the crate's root, through public modules,
    /// enums and traits and `pub use`s; impls and their items are read off their self type and
    /// trait ([`Exported::has_impl`]).
    defs: HashSet<ModuleDefId>,
}

impl Exported {
    /// Whether another crate can name `def`, a definition of the crate.
    pub(crate) fn has(&self, db: &Db, def: ModuleDefId) -> bool {
        if self.defs.contains(&def) {
            return true;
        }
        let container = match def {
            ModuleDefId::FunctionId(it) => it.lookup(db).container,
            ModuleDefId::ConstId(it) => it.lookup(db).container,
            ModuleDefId::TypeAliasId(it) => it.lookup(db).container,
            _ => return false,
        };
        let ItemContainerId::ImplId(imp) = container else { return false };
        // An inherent impl's item is at most its own visibility, a trait impl's the impl's.
        let public = hir::Impl::from(imp).trait_(db).is_some()
            || matches!(hir::ModuleDef::from(def).visibility(db), Visibility::Public);
        public && self.has_impl(db, imp)
    }

    /// Whether another crate can name the impl `imp`: rustc's `EffectiveVisibility::of_impl`
    /// over the outermost definitions of its self type and trait, each exported or of another
    /// crate.
    pub(crate) fn has_impl(&self, db: &Db, imp: ImplId) -> bool {
        let krate = imp.lookup(db).container.krate(db);
        let imp = hir::Impl::from(imp);
        let self_ty = imp.self_ty(db);
        let named = [
            self_ty.as_adt().map(|it| ModuleDefId::from(AdtId::from(it))),
            self_ty.as_dyn_trait().map(|it| ModuleDefId::TraitId(it.into())),
            imp.trait_(db).map(|it| ModuleDefId::TraitId(it.into())),
        ];
        named.into_iter().flatten().all(|def| !is_of(db, def, krate) || self.defs.contains(&def))
    }
}

/// Whether `def` is a definition of `krate`.
fn is_of(db: &Db, def: ModuleDefId, krate: base_db::Crate) -> bool {
    hir::ModuleDef::from(def).module(db).is_some_and(|it| it.krate(db).base() == krate)
}

/// What of `krate` another crate can name: resolve's pass of rustc's effective visibilities at
/// the `Reexported` level, from the crate's root over each name public in a scope another crate
/// can name. It reads the crate's def map and no body, so an edit inside a body keeps it.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn exported(db: &Db, krate: base_db::Crate) -> Exported {
    let mut exported = Exported::default();
    let root = crate_def_map(db, krate).root_module_id();
    exported.defs.insert(ModuleDefId::ModuleId(root));
    // alloc: the query's value and its worklist, once per crate
    let mut modules = vec![root];
    while let Some(module) = modules.pop() {
        let scope = module.def_map(db).module(db, module).scope(db);
        for (_, per_ns) in scope.entries() {
            let types = per_ns.types.map(|it| (it.def, it.vis));
            let values = per_ns.values.map(|it| (it.def, it.vis));
            let macros = per_ns.macros.map(|it| (ModuleDefId::MacroId(it.def), it.vis));
            for (def, vis) in [types, values, macros].into_iter().flatten() {
                if !matches!(vis, Visibility::Public)
                    || !is_of(db, def, krate)
                    || !exported.defs.insert(def)
                {
                    continue;
                }
                match def {
                    ModuleDefId::ModuleId(it) => modules.push(it),
                    ModuleDefId::AdtId(AdtId::EnumId(it)) => {
                        for &(variant, _) in it.enum_variants(db).variants.values() {
                            exported.defs.insert(ModuleDefId::EnumVariantId(variant));
                        }
                    }
                    ModuleDefId::TraitId(it) => {
                        for &(_, item) in &it.trait_items(db).items {
                            exported.defs.insert(match item {
                                AssocItemId::FunctionId(it) => it.into(),
                                AssocItemId::ConstId(it) => it.into(),
                                AssocItemId::TypeAliasId(it) => it.into(),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    exported
}
