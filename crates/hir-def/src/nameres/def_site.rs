//! Def-site hygiene of `macro` items (macros 2.0, `decl_macro`): where the first segment of a
//! path a `macro`'s body writes is looked up.
//!
//! A port of rustc's rule (`rustc_resolve`'s `hygienic_lexical_parent` and `expn_def_scope`, with
//! bindings keyed by `normalize_to_macros_2_0`). A path whose first segment carries an opaque
//! mark of a `macro` expansion `E` (its context normalized to macros 2.0 has `E` as its outer
//! expansion) sees, in the blocks and the module around it, only the names `E` itself wrote.
//! When none of them has the name and that module is not one `E` wrote, the lookup goes on at the
//! module that defines the `macro`, with the context outside `E`, and so on for each opaque mark.
//! `self`, `super` and `crate` mean the def site's module the same way. A `macro_rules!` marks
//! its expansion semi-opaque, which normalizing drops, so its paths resolve where it is called,
//! as before; so do builtin macros' and proc macros', whose def-site spans this leaves as they
//! were.
//!
//! Where rustc compares each binding's own syntax context, this asks whether the binding was
//! written inside `E`'s expansion (its item, or the `use` that imports it, sits in a file `E`
//! made, directly or through macros it called). A name `E` was handed by its caller (`$name`)
//! and wrote into an item counts as written by `E` here, where rustc gives it the caller's
//! context; generic parameters are not filtered.

use base_db::Db;
use hir_expand::{
    HirFileId, MacroCallId, MacroDefKind,
    mod_path::{ModPath, PathKind},
    name::Name,
};
use span::SyntaxContext;

use crate::{
    AstIdLoc, Lookup, MacroId, ModuleDefId, ModuleId,
    item_scope::{BuiltinShadowMode, ImportOrExternCrate, ImportOrGlob},
    nameres::{
        BlockInfo, DefMap, LocalDefMap, MacroSubNs, ModuleOrigin, crate_def_map,
        crate_local_def_map, path_resolution::adjust_to_nearest_non_block_module,
    },
    per_ns::{Namespace, PerNs},
};

/// Where a path's first segment is looked up, after def-site hygiene.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hygienic {
    /// Where the path is written: its context carries no opaque mark of a `macro`, or the
    /// scope it is written in is one the `macro` wrote.
    Here,
    /// From this module: a block or module around the path that has a name the expansion
    /// wrote, or the module defining the `macro` (followed through each opaque mark).
    At(ModuleId),
    /// The module defining the `macro` is not known yet: its own crate's def map is being
    /// collected, and the macro is in no module around the path. The path is unresolved.
    Unknown,
}

/// One step of [`DefMap::hygienic_step`].
enum Step {
    Here,
    At(ModuleId),
    DefSite { module: ModuleId, ctxt: SyntaxContext },
    Unknown,
}

impl DefMap {
    /// Where the first segment of `path`, written in `module` of this def map with syntax
    /// context `ctxt`, is looked up: see the module docs. `ns` is the namespace the first
    /// segment is looked up in (`None`: any).
    pub(crate) fn hygienic_origin(
        &self,
        db: &Db,
        module: ModuleId,
        path: &ModPath,
        ns: Option<Namespace>,
        ctxt: SyntaxContext,
    ) -> Hygienic {
        let mut module = module;
        let mut ctxt = ctxt;
        let mut moved = false;
        // Each turn either ends or follows one opaque mark out to its def site with the context
        // outside it (`parent`), so it ends after as many turns as `ctxt` has opaque marks.
        loop {
            let map = self.map_for(db, module);
            match map.hygienic_step(db, module, path, ns, ctxt) {
                Step::Here if moved => return Hygienic::At(module),
                Step::Here => return Hygienic::Here,
                Step::At(it) => return Hygienic::At(it),
                Step::Unknown => return Hygienic::Unknown,
                Step::DefSite { module: def_site, ctxt: outside } => {
                    module = def_site;
                    ctxt = outside;
                    moved = true;
                }
            }
        }
    }

    /// [`DefMap::resolve_path`] from `origin`, what [`DefMap::hygienic_origin`] gave for the
    /// path written in `module`.
    pub(crate) fn resolve_path_from(
        &self,
        local_def_map: &LocalDefMap,
        db: &Db,
        module: ModuleId,
        path: &ModPath,
        shadow: BuiltinShadowMode,
        expected_macro_subns: Option<MacroSubNs>,
        origin: Hygienic,
    ) -> (PerNs, Option<usize>) {
        match origin {
            Hygienic::Here => {
                self.resolve_path(local_def_map, db, module, path, shadow, expected_macro_subns)
            }
            Hygienic::At(at) => self.map_for(db, at).resolve_path(
                self.local_for(db, at, local_def_map),
                db,
                at,
                path,
                shadow,
                expected_macro_subns,
            ),
            Hygienic::Unknown => (PerNs::none(), None),
        }
    }

    /// The def map of `module`: this one when it is this one's, so that a def map being
    /// collected reads itself and not its query.
    pub(crate) fn map_for<'a>(&'a self, db: &'a Db, module: ModuleId) -> &'a DefMap {
        let block = module.block(db).map(|it| it.to_static());
        if module.krate(db) == self.krate && block == self.block.map(|it| it.block) {
            self
        } else {
            module.def_map(db)
        }
    }

    /// The local def map of `module`'s crate: `own` (this crate's) when it is this crate.
    pub(crate) fn local_for<'a>(
        &self,
        db: &'a Db,
        module: ModuleId,
        own: &'a LocalDefMap,
    ) -> &'a LocalDefMap {
        let krate = module.krate(db);
        if krate == self.krate { own } else { crate_local_def_map(db, krate).local(db) }
    }

    fn hygienic_step(
        &self,
        db: &Db,
        module: ModuleId,
        path: &ModPath,
        ns: Option<Namespace>,
        ctxt: SyntaxContext,
    ) -> Step {
        let ctxt = ctxt.normalize_to_macros_2_0(db);
        let Some(expn) = ctxt.outer_expn(db) else { return Step::Here };
        let expn = MacroCallId::from(expn);
        if !matches!(expn.loc(db).def.kind, MacroDefKind::Declarative(..)) {
            return Step::Here;
        }
        let (map, nearest) = match path.kind {
            PathKind::Plain => {
                let Some(name) = path.segments().first() else { return Step::Here };
                // The blocks around the path, innermost first, then the module holding them:
                // rustc's ribs of anonymous modules, then the first proper module.
                let mut map = self;
                let mut at = module;
                loop {
                    if map.wrote(db, at, name, ns, expn) {
                        return Step::At(at);
                    }
                    match map.block {
                        Some(BlockInfo { parent, .. }) if at == map.root => {
                            at = parent;
                            map = self.map_for(db, parent);
                        }
                        _ => break,
                    }
                }
                (map, at)
            }
            PathKind::Super(_) | PathKind::Crate => {
                adjust_to_nearest_non_block_module(db, self, module)
            }
            PathKind::DollarCrate(_) | PathKind::Abs => return Step::Here,
        };
        if map.module_written_inside(db, nearest, expn) {
            // A module the expansion wrote: what is not in it are the preludes', as for a
            // module written anywhere.
            return match path.kind {
                PathKind::Plain => Step::At(nearest),
                _ => Step::Here,
            };
        }
        match self.macro_def_module(db, module, expn) {
            Some(def_site) => Step::DefSite { module: def_site, ctxt: ctxt.parent(db) },
            None => Step::Unknown,
        }
    }

    /// Whether `module`'s own scope has `name` (in `ns`, or any namespace) bound by an item or
    /// import that `expn` wrote.
    fn wrote(
        &self,
        db: &Db,
        module: ModuleId,
        name: &Name,
        ns: Option<Namespace>,
        expn: MacroCallId,
    ) -> bool {
        let wants = |it: Namespace| ns.is_none_or(|ns| ns == it);
        if wants(Namespace::Macros)
            && let Some(&mac) = self
                .module(db, module)
                .legacy_macro_in_scope(db, name)
                .and_then(<[_]>::last)
            && macro_file(db, mac).is_some_and(|file| written_inside(db, file, expn))
        {
            return true;
        }
        let res = self.name_in_module(db, module, name);
        let types = res.types.filter(|_| wants(Namespace::Types)).and_then(|it| {
            match it.import {
                Some(import) => import_file(db, import),
                None => self.def_file(db, it.def),
            }
        });
        let values = res.values.filter(|_| wants(Namespace::Values)).and_then(|it| {
            match it.import {
                Some(ImportOrGlob::Import(import)) => Some(use_file(db, import.use_)),
                Some(ImportOrGlob::Glob(glob)) => Some(use_file(db, glob.use_)),
                None => self.def_file(db, it.def),
            }
        });
        let macros = res.macros.filter(|_| wants(Namespace::Macros)).and_then(|it| {
            match it.import {
                Some(import) => import_file(db, import),
                None => macro_file(db, it.def),
            }
        });
        [types, values, macros].into_iter().flatten().any(|file| written_inside(db, file, expn))
    }

    /// Whether `module` is one `expn` wrote: an inline module or block in its expansion. A
    /// module of its own file never is, whoever declared it.
    fn module_written_inside(&self, db: &Db, module: ModuleId, expn: MacroCallId) -> bool {
        match self.module(db, module).origin {
            ModuleOrigin::Inline { definition_tree_id, .. } => {
                written_inside(db, definition_tree_id.file_id(), expn)
            }
            ModuleOrigin::BlockExpr { block, .. } => written_inside(db, block.file_id, expn),
            ModuleOrigin::CrateRoot { .. } | ModuleOrigin::File { .. } => false,
        }
    }

    /// The file an item is written in, as a binding of a module's scope.
    fn def_file(&self, db: &Db, def: ModuleDefId) -> Option<HirFileId> {
        Some(match def {
            ModuleDefId::ModuleId(it) => {
                return self.map_for(db, it).module(db, it).origin.declaration().map(|it| it.file_id);
            }
            ModuleDefId::FunctionId(it) => it.lookup(db).ast_id().file_id,
            ModuleDefId::AdtId(crate::AdtId::StructId(it)) => it.lookup(db).ast_id().file_id,
            ModuleDefId::AdtId(crate::AdtId::UnionId(it)) => it.lookup(db).ast_id().file_id,
            ModuleDefId::AdtId(crate::AdtId::EnumId(it)) => it.lookup(db).ast_id().file_id,
            ModuleDefId::EnumVariantId(it) => it.lookup(db).id.file_id,
            ModuleDefId::ConstId(it) => it.lookup(db).ast_id().file_id,
            ModuleDefId::StaticId(it) => it.lookup(db).ast_id().file_id,
            ModuleDefId::TraitId(it) => it.lookup(db).ast_id().file_id,
            ModuleDefId::TypeAliasId(it) => it.lookup(db).ast_id().file_id,
            ModuleDefId::MacroId(it) => return macro_file(db, it),
            ModuleDefId::BuiltinType(_) => return None,
        })
    }

    /// The module defining the `macro` whose expansion is `expn`: rustc's `expn_def_scope`.
    /// Looked for first in the modules around `module` (the def maps around a block, then the
    /// module's ancestors), which is all there is to read while this crate's own def map is
    /// collected; then in the crate's def map.
    fn macro_def_module(&self, db: &Db, module: ModuleId, expn: MacroCallId) -> Option<ModuleId> {
        let def = expn.loc(db).def;
        let MacroDefKind::Declarative(ast_id, _) = def.kind else { return None };
        let container = |map: &DefMap, at: ModuleId| {
            map.module(db, at).scope(db).macros().find_map(|(_, it)| match it.def {
                MacroId::Macro2Id(id) => {
                    let loc = id.lookup(db);
                    (loc.id.upcast() == ast_id).then_some(loc.container)
                }
                MacroId::MacroRulesId(id) => {
                    let loc = id.lookup(db);
                    (loc.id.upcast() == ast_id).then_some(loc.container)
                }
                MacroId::ProcMacroId(_) => None,
            })
        };
        let mut map = self;
        let mut at = Some(module);
        while let Some(here) = at {
            if let Some(found) = container(map, here) {
                return Some(found);
            }
            at = match map.module(db, here).parent {
                Some(parent) => Some(parent),
                None => map.block.map(|BlockInfo { parent, .. }| {
                    map = self.map_for(db, parent);
                    parent
                }),
            };
        }
        // The crate's def map is collected unless it is this one, still being collected.
        let collecting = self.block.is_none()
            && def.krate == self.krate
            && !matches!(self.modules, super::ModulesSlot::Published(_) | super::ModulesSlot::Collected(_));
        match collecting {
            true => None,
            false => macro2_def_module(db, expn),
        }
    }
}

/// [`DefMap::macro_def_module`] in the def map of the `macro`'s crate, once collected: a scan
/// of its modules, kept per expansion.
#[blaze_query::tracked(returns(copy))]
fn macro2_def_module(db: &Db, expn: MacroCallId) -> Option<ModuleId> {
    let def = expn.loc(db).def;
    let MacroDefKind::Declarative(ast_id, _) = def.kind else { return None };
    crate_def_map(db, def.krate).modules(db).find_map(|(_, data)| {
        data.scope(db).macros().find_map(|(_, it)| match it.def {
            MacroId::Macro2Id(id) => {
                let loc = id.lookup(db);
                (loc.id.upcast() == ast_id).then_some(loc.container)
            }
            MacroId::MacroRulesId(id) => {
                let loc = id.lookup(db);
                (loc.id.upcast() == ast_id).then_some(loc.container)
            }
            MacroId::ProcMacroId(_) => None,
        })
    })
}

/// Whether `file` is `expn`'s expansion or the expansion of a macro called in it, at any depth.
fn written_inside(db: &Db, file: HirFileId, expn: MacroCallId) -> bool {
    let mut file = file;
    // Each turn moves to the file a macro was called in, ending at a real file.
    while let Some(call) = file.macro_file() {
        if call == expn {
            return true;
        }
        file = call.loc(db).kind.file_id();
    }
    false
}

fn macro_file(db: &Db, mac: MacroId) -> Option<HirFileId> {
    Some(match mac {
        MacroId::Macro2Id(it) => it.lookup(db).id.file_id,
        MacroId::MacroRulesId(it) => it.lookup(db).id.file_id,
        MacroId::ProcMacroId(it) => it.lookup(db).id.file_id,
    })
}

fn use_file(db: &Db, use_: crate::UseId) -> HirFileId {
    use_.lookup(db).id.file_id
}

fn import_file(db: &Db, import: ImportOrExternCrate) -> Option<HirFileId> {
    Some(match import {
        ImportOrExternCrate::Import(it) => use_file(db, it.use_),
        ImportOrExternCrate::Glob(it) => use_file(db, it.use_),
        ImportOrExternCrate::ExternCrate(it) => it.lookup(db).id.file_id,
    })
}
