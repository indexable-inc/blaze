//! The old `ModCollector` and the collector's seeding and definition paths, rule for rule, over
//! a [`Sink`]: every mutation the old code made of its `DefCollector` is one `sink.apply(op)`,
//! in the order it made them; every read goes through `sink.def_map()`,
//! `sink.local_def_map()`, `sink.find_unresolved_named` and the module's [`SideTables`].
//!
//! [`module_decls`] is one module's round-0 collection, over [`DeclSink`]: the module's own row,
//! its children's rows (each scope a fact of the child's own `module_decls`), and every other
//! module read through [`decl_row`] (`nameres-core.md` §3, "Round 0 reads").

use std::alloc::Allocator;

use base_db::{BuiltDependency, Crate, CrateOrigin, Db, LangCrateOrigin};
use blaze_hash::HashMap;
use blaze_mem::{Frame, Tag};
use cfg::CfgExpr;
use either::Either;
use hir_expand::{
    EditionedFileId, ErasedAstId, HirFileId, InFile, MacroCallId, MacroDefId, MacroDefKind,
    builtin::{find_builtin_attr, find_builtin_derive, find_builtin_macro},
    mod_path::{ModPath, PathKind},
    name::{AsName, Name},
    proc_macro::{CustomProcMacroExpander, ProcMacros},
};
use intern::{Symbol, sym};
use span::{Edition, FileAstId, ROOT_ERASED_FILE_AST_ID};
use syntax::ast;
use triomphe::Arc;

use crate::{
    AdtId, AstId, AstIdWithPath, BuiltinDeriveImplId, BuiltinDeriveImplLoc, ConstLoc, EnumLoc,
    ExternBlockLoc, ExternCrateId, ExternCrateLoc, FunctionId, FunctionLoc, ImplLoc, Intern,
    ItemContainerId, Macro2Id, Macro2Loc, MacroExpander, MacroRulesId, MacroRulesLoc,
    MacroRulesLocFlags, ModuleDefId, ModuleId, ProcMacroId, ProcMacroLoc, StaticLoc, StructLoc,
    TraitLoc, TypeAliasLoc, UnionLoc, UseLoc, file_item_tree,
    item_scope::{ImportOrExternCrate, ItemScope, PerNsGlobImports},
    item_tree::{
        self, Attrs, AttrsOrCfgRef, ImportAlias, ImportKind, ItemTree, ItemTreeAstId, Macro2,
        MacroCall, MacroRules, Mod, ModItemId, ModItemKind, ModKind, StructValueNsCtor, TreeId,
    },
    macro_call_as_call_id,
    nameres::{
        BuiltinShadowMode, DefMap, DefMapCrateData, DiagnosticsByModule, LocalDefMap, MacroSubNs,
        ModuleData, ModuleOrigin, ModulesMap, ModulesSlot, ResolveMode, crate_def_map,
        diagnostics::{DefDiagnostic, PreludeError},
        enable_builtin_derive_fast_path,
        mod_resolution::ModDir,
        proc_macro::{ProcMacroDef, ProcMacroKind, parse_macro_name_and_helper_attrs},
        sub_namespace_match,
    },
    per_ns::{Item, PerNs},
    tt,
    visibility::{RawVisibility, Visibility, VisibilityExplicitness},
};

use super::{
    FrameMap, FrameVec,
    env::{Ancestor, ChildMode, ChildRequest, CollectCx, CollectEnv, SideTables, Sink},
    map_in,
    ops::{DeclOp, ImportOp, ImportSource, Inherited, MacroOp, MacroOpKind, OpLog},
    queries::{
        self, CrateEnvKey, Decl, DeclItems, DeclKey, EnvKey, ModuleDecls, Rows, RowsKey, Scope,
    },
};

// The constants of a collection.

/// The crate's dependencies by name, as the old `collect_defs` prologue made them: a later
/// dependency of a name replaces an earlier one's value in its place.
fn deps<'db, 'f>(
    db: &'db Db,
    frame: &'f Frame,
    krate: Crate,
) -> FrameVec<'f, (Name, &'db BuiltDependency)> {
    let data = krate.data(db);
    let mut deps = Vec::with_capacity_in(data.dependencies.len(), frame);
    for dep in &data.dependencies {
        tracing::debug!("crate dep {:?} -> {:?}", dep.name, dep.crate_id);
        let name = dep.as_name();
        match deps.iter_mut().find(|(it, _)| *it == name) {
            Some(it) => it.1 = dep,
            None => deps.push((name, dep)),
        }
    }
    deps
}

/// The constants of a collection of `krate` (`collect_defs`' prologue): deps, cfg, the crate's
/// own proc macros (read with the root file's syntax context), the fast-path flag. Its
/// temporaries go to `frame`, the calling query's.
pub(crate) fn cx<'db, 'f>(
    db: &'db Db,
    frame: &'f Frame,
    krate: Crate,
    in_block: bool,
) -> CollectCx<'db, 'f> {
    let krate_data = krate.data(db);
    // A block's map shares the crate map's data (`block_def_map`); a crate's own collection reads
    // the crate attributes only.
    let data = match in_block {
        true => Arc::clone(&crate_def_map(db, krate).data),
        false => Arc::clone(queries::crate_data(db, krate)),
    };
    let by_target = krate_data.target_kind.is_proc_macro();
    let proc_macros = match by_target {
        true => ProcMacros::get_for_crate(db, krate)
            .and_then(|proc_macros| {
                let root = HirFileId::from(krate.root_file_id(db));
                proc_macros.list(root.syntax_context(db, krate_data.edition))
            })
            .unwrap_or_default(),
        false => Default::default(),
    };
    // A block's collection never runs `seed_with_top_level`, which reads `#![crate_type]`.
    let is_proc_macro = by_target || (!in_block && data.proc_macro);
    CollectCx {
        db,
        frame,
        krate,
        data,
        deps: deps(db, frame, krate),
        cfg_options: krate.cfg_options(db),
        proc_macros,
        is_proc_macro,
        builtin_derive_fast_path: enable_builtin_derive_fast_path(db),
        in_block,
    }
}

/// The crate attributes of the root file: the first part of the old `seed_with_top_level`.
pub(crate) fn crate_data(
    db: &Db,
    krate: Crate,
) -> Arc<DefMapCrateData> {
    let _p = tracing::info_span!("crate_data").entered();
    let krate_data = krate.data(db);
    let file_id = krate.root_file_id(db);
    let item_tree = file_item_tree(db, file_id.into(), krate);
    let attrs = match item_tree.top_level_attrs() {
        Some(AttrsOrCfgRef::Enabled { attrs }) => attrs,
        None => Attrs::EMPTY,
        Some(AttrsOrCfgRef::CfgDisabled(_, attrs)) => attrs,
    };
    let mut crate_data = DefMapCrateData::new(krate_data.edition);
    let mut is_proc_macro = krate_data.target_kind.is_proc_macro();
    // Process other crate-level attributes.
    for attr in &*attrs {
        let Some(attr_name) = attr.path.as_ident() else { continue };

        match () {
            () if *attr_name == sym::recursion_limit => {
                if let Some(limit) = attr.string_value()
                    && let Ok(limit) = limit.parse()
                {
                    crate_data.recursion_limit = Some(limit);
                }
            }
            () if *attr_name == sym::crate_type => {
                if attr.string_value() == Some("proc-macro") {
                    is_proc_macro = true;
                }
            }
            () if *attr_name == sym::no_core => crate_data.no_core = true,
            () if *attr_name == sym::no_std => crate_data.no_std = true,
            () if *attr_name == sym::rustc_coherence_is_core => {
                crate_data.rustc_coherence_is_core = true;
            }
            () if *attr_name == sym::feature => {
                let features =
                    attr.parse_path_comma_token_tree(db).into_iter().flatten().filter_map(
                        |(feat, _, _)| match feat.segments() {
                            // clone: an interned symbol
                            [name] => Some(name.symbol().clone()),
                            _ => None,
                        },
                    );
                features.for_each(|feature| crate_data.unstable_features.enable(feature));
            }
            () if *attr_name == sym::register_tool => {
                if let Some(ident) = attr.single_ident_value() {
                    // clone: an interned symbol
                    crate_data.registered_tools.push(ident.sym.clone());
                    cov_mark::hit!(register_tool);
                }
            }
            () => (),
        }
    }
    crate_data.proc_macro = is_proc_macro;
    crate_data.shrink_to_fit();
    // alloc: one per crate, the value of `queries::crate_data`
    Arc::new(crate_data)
}

/// The crate root's round-0 key: the extern prelude of the crate's dependencies (the old
/// `seed_with_top_level`'s second part).
pub(crate) fn root_decl(
    db: &Db,
    frame: &Frame,
    krate: Crate,
) -> DeclKey {
    let crate_data = queries::crate_data(db, krate);
    let deps = deps(db, frame, krate);
    let mut prelude = Vec::with_capacity_in(deps.len(), frame);
    for &(ref name, dep) in &deps {
        if dep.is_prelude() {
            // `no_core` and `no_std` remove the sysroot dependence on `core` and `std`; the
            // crate graph has them in place no matter what. Reading a dependency's crate data
            // is done for sysroot crates only (the order of the `&&`), which keep their ids.
            let skip = dep.is_sysroot()
                && match dep.crate_id.data(db).origin {
                    CrateOrigin::Lang(LangCrateOrigin::Core) => crate_data.no_core,
                    CrateOrigin::Lang(LangCrateOrigin::Std) => {
                        crate_data.no_core || crate_data.no_std
                    }
                    _ => false,
                };
            if !skip {
                prelude.push((*name, dep.crate_id));
            }
        }
    }
    // The def maps are collected as one batch of the database's tasks, then put in the extern
    // prelude in the order of the dependencies.
    let prelude = &prelude[..];
    if prelude.len() > 1 {
        db.run_all(Tag::Nameres, prelude.len(), |db, at| {
            crate_def_map(db, prelude[at].1);
        });
    }
    let extern_prelude = prelude
        .iter()
        .map(|&(name, krate)| (name, crate_def_map(db, krate).root_module_id(), None))
        // alloc: once per crate, the root's crate env
        .collect();
    let krate_env = CrateEnvKey::new(
        db,
        super::env::CrateEnv { extern_prelude, macro_use: Box::default(), prelude: None },
    );
    let env = EnvKey::new(db, CollectEnv { krate: krate_env, ancestors: Box::default() });
    let root_file = krate.root_file_id(db);
    DeclKey::new(
        db,
        Decl {
            krate,
            module: queries::crate_root(db, krate),
            origin: ModuleOrigin::CrateRoot { definition: root_file },
            visibility: Visibility::Public,
            parent: None,
            tree: TreeId::new(root_file.into(), None),
            items: DeclItems::Top,
            mod_dir: ModDir::root(),
            macro_depth: 0,
            inherited: Box::default(),
            env,
        },
    )
}

/// The `LocalDefMap` paths read under a crate env: one per env, shared by every module whose
/// collection starts from it.
#[blaze_query::tracked(returns(ref))]
fn crate_env_local(
    db: &Db,
    env: CrateEnvKey,
) -> LocalDefMap {
    env.env(db).local_def_map()
}

/// What a `mod name;` reads of the file it names: whether the file's own `#![cfg]` is false,
/// and whether it is `#![macro_use]`. A query, so that the declaring module's collection does
/// not depend on the rest of the file.
#[derive(Debug, PartialEq, Eq)]
struct FileHead {
    cfg_disabled: Option<CfgExpr>,
    macro_use: bool,
}

#[blaze_query::tracked(returns(ref))]
fn mod_file_head(
    db: &Db,
    file: EditionedFileId,
    krate: Crate,
) -> FileHead {
    match file_item_tree(db, file.into(), krate).top_level_attrs() {
        // clone: the cfg of a file that is configured out, once per such file
        Some(AttrsOrCfgRef::CfgDisabled(cfg, _)) => FileHead { cfg_disabled: Some(cfg.clone()), macro_use: false },
        Some(AttrsOrCfgRef::Enabled { attrs }) => {
            FileHead { cfg_disabled: None, macro_use: attrs.by_key(sym::macro_use).exists() }
        }
        None => FileHead { cfg_disabled: None, macro_use: false },
    }
}

/// Computes the item tree of each of `calls`' expansions as one batch of the database's tasks
/// ([`Db::run_all`]). The results are left in the database, where the collection
/// reads them in the round's order.
pub(crate) fn expand_all(
    db: &Db,
    krate: Crate,
    calls: &[MacroCallId],
) {
    if calls.len() > 1 {
        db.run_all(Tag::Expand, calls.len(), |db, at| {
            file_item_tree(db, calls[at].into(), krate);
        });
    }
}

// Round 0.

#[cfg(test)]
thread_local! {
    /// The scope each `module_decls` execution's `DeclSink` held at its end, by key. The
    /// `TestDB` runs its queries on the calling thread.
    static HELD: std::cell::RefCell<HashMap<DeclKey, crate::nameres::BuildingScope>> =
        std::cell::RefCell::new(HashMap::default());
}

/// The scope the last execution of `module_decls(key)` on this thread held at its end.
#[cfg(test)]
pub(super) fn held_scope(key: DeclKey) -> crate::nameres::BuildingScope {
    HELD.with(|held| held.borrow().get(&key).cloned())
        .unwrap_or_else(|| panic!("module_decls({key:?}) did not run on this thread"))
}

/// One module's round-0 declarations: its collection over a [`DeclSink`].
pub(crate) fn module_decls(
    db: &Db,
    frame: &Frame,
    key: DeclKey,
) -> ModuleDecls {
    let _p = tracing::info_span!("module_decls").entered();
    let decl = key.decl(db);
    let cx = cx(db, frame, decl.krate, false);
    let mut sink = DeclSink::new(&cx, key, decl);
    let mut side = SideTables::default();
    sink.apply(DeclOp::Init {
        module: decl.module,
        origin: decl.origin,
        visibility: decl.visibility,
        parent: decl.parent,
        // clone: the log owns its module's textual scope, once per module
        inherited: decl.inherited.clone(),
    });
    let item_tree = decl.tree.item_tree(db, decl.krate);
    let items = match decl.items {
        DeclItems::Nothing => None,
        DeclItems::Top if decl.parent.is_none() => {
            inject_std_extern_crate(&cx, &mut sink);
            inject_prelude(&mut sink);
            root_items(db, &mut sink, decl, item_tree)
        }
        DeclItems::Top => Some(item_tree.top_level_items()),
        DeclItems::Inline(item) => Some(inline_items(item_tree, item, decl.module)),
    };
    if let Some(items) = items {
        collect_items(
            &cx,
            &mut sink,
            &mut side,
            decl.module,
            decl.tree,
            decl.macro_depth,
            decl.mod_dir,
            items,
            decl.module.into(),
        );
    }
    sink.finish(side)
}

/// The crate root's items: all of them, or, when the root file's `#![cfg]` is false, only the
/// module the compiler appended to it (rustc takes the crate's items away before it injects its
/// test harness, so a test crate of `#![cfg(target_arch = "wasm32")]` still has a `main` that
/// runs no tests); `None` when there is none.
fn root_items<'t, 'db, S: Sink<'db>>(
    db: &Db,
    sink: &mut S,
    decl: &Decl,
    item_tree: &'t ItemTree,
) -> Option<&'t [ModItemId]> {
    let items = item_tree.top_level_items();
    let Some(AttrsOrCfgRef::CfgDisabled(cfg_expr, _)) = item_tree.top_level_attrs() else {
        return Some(items);
    };
    sink.apply(DeclOp::Diagnostic(DefDiagnostic::unconfigured_code(
        decl.module,
        InFile::new(decl.tree.file_id(), ROOT_ERASED_FILE_AST_ID),
        // clone: the root's cfg, once per configured-out crate
        cfg_expr.clone(),
    )));
    let ModuleOrigin::CrateRoot { definition } = decl.origin else {
        panic!("nameres facts: the crate root {:?} has origin {:?}", decl.module, decl.origin)
    };
    let root = db.file_text(definition.file_id(db));
    let appended = root.appended_root_module(db);
    let item = appended.and_then(|name| {
        items.last().filter(|&&item| {
            matches!(item_tree.index(item),
                ModItemKind::Mod(_, module) if item_tree[module.name].as_str() == name)
        })
    })?;
    Some(std::slice::from_ref(item))
}

/// The items of the inline `mod` item `item` of `item_tree`, the module `module`.
fn inline_items(item_tree: &ItemTree, item: ModItemId, module: ModuleId) -> &[ModItemId] {
    match item_tree.index(item) {
        ModItemKind::Mod(_, m) => match item_tree.mod_kind(m) {
            ModKind::Inline { items } => items,
            ModKind::Outline => {
                panic!("nameres facts: the round-0 key of {module:?} names `mod` item {item:?}, which is not inline")
            }
        },
        _ => panic!("nameres facts: the round-0 key of {module:?} names item {item:?}, which is not a `mod`"),
    }
}

/// The files a declarative macro call's expansion reads its macro from: the file of the
/// macro's definition, and where that definition is itself in an expansion of a declarative
/// macro (`macro_rules! foo` made by `mk_foo!()`), the file of that macro's, up the chain. Each
/// may be another crate's, whose edit changes no binding when the macros keep their ids.
pub(in crate::nameres) fn definition_files(db: &Db, call: MacroCallId, mut each: impl FnMut(span::FileId)) {
    let mut call = call;
    while let MacroDefKind::Declarative(ast, _) = call.loc(db).def.kind {
        each(ast.file_id.original_file(db).file_id(db));
        match ast.file_id {
            HirFileId::MacroFile(maker) => call = maker,
            HirFileId::FileId(_) => return,
        }
    }
}

/// The round-0 sink of [`module_decls`]: the module's own row (scope `Building`) and the rows of
/// the children it has declared (each scope the child's `Scope::Decl` fact) in a view `DefMap`
/// that reads every other module through [`decl_row`]; the crate env; the log.
///
/// The view reads through `Rows::Decl` of the module's own key until it declares a child, then
/// of the key of the last child it declared: that child's env holds this module (as its nearest
/// ancestor, with the keys of the children before it) and this module's ancestors, and
/// [`decl_row`] reaches that child's own subtree through its `module_decls`, which is finished.
/// So no read of a collection ever reaches its own `module_decls`.
struct DeclSink<'a, 'db, 'f> {
    cx: &'a CollectCx<'db, 'f>,
    #[cfg(test)]
    key: DeclKey,
    decl: &'db Decl,
    /// The env the module is declared in: its ancestors.
    env: &'db CollectEnv,
    def_map: DefMap,
    /// The crate env as of now; changed only by the root's `extern crate`s and its prelude.
    crate_env: CrateEnvKey,
    local_def_map: &'db LocalDefMap,
    /// The log's ops so far; [`DeclSink::finish`] makes the [`OpLog`].
    log: FrameVec<'f, DeclOp>,
    children: FrameVec<'f, DeclKey>,
    child_keys: FrameVec<'f, (ModuleId, DeclKey)>,
    /// [`ModuleDecls::other_files`].
    other_files: FrameVec<'f, span::FileId>,
    from_glob_import: PerNsGlobImports<&'f Frame>,
    /// The module's plain imports by the name each binds: indices into `log`, in order.
    named: FrameMap<'f, Name, FrameVec<'f, u32>>,
}

impl<'a, 'db, 'f> DeclSink<'a, 'db, 'f> {
    fn new(cx: &'a CollectCx<'db, 'f>, key: DeclKey, decl: &'db Decl) -> Self {
        let db = cx.db;
        let env = decl.env.env(db);
        let rows = blaze_query::db::AsId::as_id(&RowsKey::new(db, Rows::Decl(key)));
        let mut def_map = DefMap {
            krate: decl.krate,
            block: None,
            root: queries::crate_root(db, decl.krate),
            modules: ModulesSlot::Unit { own: ModulesMap::new(), rows },
            prelude: None,
            macro_use_prelude: HashMap::default(),
            derive_helpers_in_scope: HashMap::default(),
            session_macros: false,
            diagnostics: Vec::new(), // alloc: empty, the view's diagnostics go to the log
            diagnostics_by_module: DiagnosticsByModule::default(),
            records: None,
            names: Default::default(),
            data: Arc::clone(&cx.data),
        };
        env.krate.env(db).install(&mut def_map);
        DeclSink {
            cx,
            #[cfg(test)]
            key,
            decl,
            env,
            def_map,
            crate_env: env.krate,
            local_def_map: crate_env_local(db, env.krate),
            log: Vec::new_in(cx.frame),
            children: Vec::new_in(cx.frame),
            child_keys: Vec::new_in(cx.frame),
            other_files: Vec::new_in(cx.frame),
            from_glob_import: PerNsGlobImports::new_in(cx.frame),
            named: map_in(cx.frame),
        }
    }

    fn own_row(&mut self) -> &mut ModuleData {
        let own = self.decl.module;
        &mut self.def_map.modules.building()[own]
    }

    /// Applies `op` to the held state, as the old `DefCollector` did to the module's row; other
    /// modules' ops (a `#[macro_export]` of a non-root module) wait for the root's `Splice`.
    fn apply_state(&mut self, op: &DeclOp) {
        let own = self.decl.module;
        match *op {
            DeclOp::Init { module, origin, visibility, parent, ref inherited } if module == own => {
                let mut data = ModuleData::new(origin, visibility, parent);
                for (name, macs) in inherited.iter() {
                    for &mac in macs.iter() {
                        data.scope_mut().define_legacy_macro(*name, mac);
                    }
                }
                self.def_map.modules.building().insert(own, data);
            }
            DeclOp::PushChild { parent, name, child } if parent == own => {
                self.own_row().children_mut().insert(name, child);
            }
            DeclOp::Declare { module, def } if module == own => {
                self.own_row().scope_mut().declare(def)
            }
            DeclOp::Update { module, ref resolutions, vis, import } if module == own => {
                self.update(resolutions, vis, import)
            }
            DeclOp::DefineImpl { module, imp, is_trait_impl, is_reservation } if module == own => {
                self.own_row().scope_mut().define_impl(imp, is_trait_impl, is_reservation)
            }
            DeclOp::DefineBuiltinDeriveImpl { module, imp } if module == own => {
                self.own_row().scope_mut().define_builtin_derive_impl(imp)
            }
            DeclOp::DefineExternBlock { module, id } if module == own => {
                self.own_row().scope_mut().define_extern_block(id)
            }
            DeclOp::DefineExternCrateDecl { module, id } if module == own => {
                self.own_row().scope_mut().define_extern_crate_decl(id)
            }
            DeclOp::DefineUseDecl { module, id } if module == own => {
                self.own_row().scope_mut().define_use_decl(id)
            }
            DeclOp::DefineUnnamedConst { module, id } if module == own => {
                self.own_row().scope_mut().define_unnamed_const(id)
            }
            DeclOp::DefineLegacyMacro { module, name, mac } if module == own => {
                self.own_row().scope_mut().define_legacy_macro(name, mac)
            }
            DeclOp::AddMacroInvoc { module, call, id } if module == own => {
                self.own_row().scope_mut().add_macro_invoc(call, id)
            }
            DeclOp::AddAttrMacroInvoc { module, item, id } if module == own => {
                self.own_row().scope_mut().add_attr_macro_invoc(item, id)
            }
            DeclOp::SetDeriveMacroInvoc { module, adt, call, attr, idx } if module == own => {
                self.own_row().scope_mut().set_derive_macro_invoc(adt, call, attr, idx)
            }
            DeclOp::InitDeriveAttribute { module, adt, attr, attr_call, ref derive_calls }
                if module == own =>
            {
                // Made by the macro steps; a round-0 collection never makes one.
                // clone: a derive attribute's call list, inline up to 4
                let calls = derive_calls.clone();
                self.own_row().scope_mut().init_derive_attribute(adt, attr, attr_call, calls)
            }
            DeclOp::ExternPrelude { .. } | DeclOp::MacroUsePrelude { .. } | DeclOp::Prelude { .. } => {
                self.env_after(op)
            }
            _ => {}
        }
    }

    /// The crate env after a crate-level op, installed into the view.
    fn env_after(&mut self, op: &DeclOp) {
        let db = self.cx.db;
        let Some(next) = self.crate_env.env(db).after(op) else { return };
        self.crate_env = CrateEnvKey::new(db, next);
        let env = self.crate_env.env(db);
        env.install(&mut self.def_map);
        if matches!(op, DeclOp::ExternPrelude { .. }) {
            self.local_def_map = crate_env_local(db, self.crate_env);
        }
    }

    /// `DefCollector::update` of the module's own scope. A round-0 module has no glob importer
    /// (globs are resolved by the units), so nothing propagates, and no import of a round-0 op
    /// is a glob.
    fn update(
        &mut self,
        resolutions: &[(Option<Name>, PerNs)],
        vis: Visibility,
        import: Option<ImportOrExternCrate>,
    ) {
        let db = self.cx.db;
        let own = self.decl.module;
        if let Some(ImportOrExternCrate::Glob(glob)) = import {
            panic!("nameres facts: a round-0 collection of {own:?} applies glob import {glob:?}");
        }
        for &(name, res) in resolutions {
            match name {
                Some(name) => {
                    let mut defs = res;
                    let def_map = &self.def_map;
                    if let Some(def) = defs.types.as_mut() {
                        def.vis = def.vis.min(db, vis, def_map).unwrap_or(vis);
                    }
                    if let Some(def) = defs.values.as_mut() {
                        def.vis = def.vis.min(db, vis, def_map).unwrap_or(vis);
                    }
                    if let Some(def) = defs.macros.as_mut() {
                        def.vis = def.vis.min(db, vis, def_map).unwrap_or(vis);
                    }
                    let DeclSink { def_map, from_glob_import, .. } = &mut *self;
                    def_map.modules.building()[own].scope_mut().push_res_with_import(
                        from_glob_import,
                        (own, name),
                        defs,
                        import,
                    );
                }
                None => {
                    let tr = match res.take_types_full() {
                        Some(Item { def: ModuleDefId::TraitId(tr), vis: _, import: _ }) => tr,
                        Some(other) => {
                            tracing::debug!("non-trait `_` import of {:?}", other);
                            continue;
                        }
                        None => continue,
                    };
                    let old_vis = self.def_map.module(db, own).scope(db).unnamed_trait_vis(tr);
                    let should_update = match old_vis {
                        None => true,
                        Some(old_vis) => {
                            let max_vis = old_vis.max(db, vis, &self.def_map).unwrap_or_else(|| {
                                panic!("`Tr as _` imports with unrelated visibilities {old_vis:?} and {vis:?} (trait {tr:?})");
                            });
                            if max_vis == old_vis {
                                false
                            } else {
                                cov_mark::hit!(upgrade_underscore_visibility);
                                true
                            }
                        }
                    };
                    if should_update {
                        self.own_row().scope_mut().push_unnamed_trait_through(tr, vis, import);
                    }
                }
            }
        }
    }

    /// The env a child declared now is collected in: this module as its nearest ancestor (its
    /// module bindings, its children so far, and the keys of those collected), then this
    /// module's own ancestors.
    fn child_env(&self) -> EnvKey {
        let db = self.cx.db;
        let decl = self.decl;
        let row = self.def_map.module(db, decl.module);
        let scope = row.scope(db);
        let is_module = |it: &(&Name, Item<ModuleDefId, ImportOrExternCrate>)| {
            matches!(it.1.def, ModuleDefId::ModuleId(_))
        };
        let mut modules = Vec::with_capacity(scope.types().filter(is_module).count());
        modules.extend(
            scope
                .types()
                .filter(is_module)
                .map(|(name, it)| (*name, PerNs { types: Some(it), values: None, macros: None })),
        );
        // Exact: filled to its capacity, so the box takes the vector's allocation.
        let modules = modules.into_boxed_slice();
        // alloc: as above, the parent's children so far
        let children = row.children(db).iter().map(|(name, module)| (*name, *module)).collect();
        let mut ancestors = Vec::with_capacity(1 + self.env.ancestors.len());
        ancestors.push(Ancestor {
            module: decl.module,
            origin: decl.origin,
            visibility: decl.visibility,
            parent: decl.parent,
            modules,
            children,
            child_keys: self.child_keys.as_slice().into(),
        });
        // clone: the chain of ancestors a child's key holds, once per child module
        ancestors.extend(self.env.ancestors.iter().cloned());
        EnvKey::new(
            db,
            CollectEnv { krate: self.crate_env, ancestors: ancestors.into_boxed_slice() },
        )
    }

    fn finish(self, side: SideTables) -> ModuleDecls {
        let ops = OpLog::from_frame(self.log);
        // alloc: the module's round-0 children, part of the query's value, at exact size
        let children = Box::from(&self.children[..]);
        let db = self.cx.db;
        let row = self.def_map.module(db, self.decl.module);
        let legacy = textual_scope(row.scope(db));
        #[cfg(test)]
        {
            let crate::nameres::ScopeSlot::Building(held) = &row.scope else {
                panic!("the round-0 row of {:?} is not building", self.decl.module)
            };
            // clone: test capture of the held scope
            HELD.with(|it| it.borrow_mut().insert(self.key, (**held).clone()));
        }
        // alloc: the other files it read, at exact size; empty for most modules
        let other_files = Box::from(&self.other_files[..]);
        // alloc: one per module, the tables its first macro step starts from
        ModuleDecls { ops, children, legacy, side: Arc::new(side), other_files }
    }
}

impl<'a, 'db, 'f> Sink<'db> for DeclSink<'a, 'db, 'f> {
    fn db(&self) -> &'db Db {
        self.cx.db
    }

    fn def_map(&self) -> &DefMap {
        &self.def_map
    }

    fn local_def_map(&self) -> &LocalDefMap {
        self.local_def_map
    }

    fn apply(&mut self, op: DeclOp) {
        self.apply_state(&op);
        if let DeclOp::Import(it) = &op
            && it.module == self.decl.module
            && it.source.kind == ImportKind::Plain
            && let Some(name) = it.bound_name()
        {
            let at = u32::try_from(self.log.len()).expect("a module log of 2^32 ops");
            let frame = self.cx.frame;
            self.named.entry(name).or_insert_with(|| Vec::new_in(frame)).push(at);
        }
        self.log.push(op);
    }

    fn find_unresolved_named<T>(
        &self,
        module: ModuleId,
        name: Name,
        mut f: impl FnMut(&ModPath) -> Option<T>,
    ) -> Option<T> {
        if module != self.decl.module {
            return None;
        }
        self.named.get(&name)?.iter().find_map(|&at| match &self.log[at as usize] {
            DeclOp::Import(it) => f(&it.path),
            other => unreachable!("the import index names {other:?}"),
        })
    }

    fn configured_out_file(&mut self, file: EditionedFileId) {
        let file = file.file_id(self.cx.db);
        if !self.other_files.contains(&file) {
            self.other_files.push(file);
        }
    }

    fn collecting_expansion(&mut self, call: MacroCallId) {
        let db = self.cx.db;
        let own = HirFileId::from(call).original_file(db).file_id(db);
        let other_files = &mut self.other_files;
        definition_files(db, call, |file| {
            if file != own && !other_files.contains(&file) {
                other_files.push(file);
            }
        });
    }

    fn child(&mut self, request: ChildRequest<'_>) -> ChildMode {
        let db = self.cx.db;
        let own = self.decl.module;
        let ChildRequest {
            parent,
            child,
            name: _,
            origin,
            visibility,
            tree,
            items,
            mod_dir,
            macro_depth,
            inherited,
        } = request;
        assert_eq!(
            parent, own,
            "nameres facts: the round-0 collection of {own:?} declares {child:?} in {parent:?}"
        );
        let env = self.child_env();
        let key = DeclKey::new(
            db,
            Decl {
                krate: self.decl.krate,
                module: child,
                origin,
                visibility,
                parent: Some(parent),
                tree,
                items,
                mod_dir,
                macro_depth,
                // clone: a child's key owns the textual scope it starts with, once per child
                inherited: inherited.into(),
                env,
            },
        );
        self.children.push(key);
        self.child_keys.push((child, key));
        let row = ModuleData {
            origin,
            visibility,
            parent: Some(parent),
            scope: queries::fact_slot(db, Scope::Decl(key)),
        };
        self.def_map.modules.building().insert(child, row);
        if let ModulesSlot::Unit { rows, .. } = &mut self.def_map.modules {
            *rows = blaze_query::db::AsId::as_id(&RowsKey::new(db, Rows::Decl(key)));
        }
        self.log.push(DeclOp::Splice { child, key });
        if own == self.def_map.root {
            for op in queries::subtree_macro_exports(db, key).iter() {
                self.apply_state(op);
            }
        }
        ChildMode::Separate(key)
    }

    fn take_macros(&mut self, _into: &mut FrameVec<'_, MacroOp>) {}
}

/// The row of `module` in the round-0 view of `key` (`Rows::Decl`): an ancestor as the key's
/// env holds it (its module bindings and children, in a `Building` scope); a module declared
/// before the key's module under one of its ancestors, or under the key's module itself, with
/// its `Scope::Decl` fact; `None` for any other.
pub(crate) fn decl_row(
    db: &Db,
    key: DeclKey,
    module: ModuleId,
) -> Option<ModuleData> {
    let decl = key.decl(db);
    let env = decl.env.env(db);
    if let Some(ancestor) = env.ancestors.iter().find(|it| it.module == module) {
        return Some(ancestor_row(ancestor));
    }
    let found = find_key(db, key, decl, env, module)?;
    let found_decl = found.decl(db);
    Some(ModuleData {
        origin: found_decl.origin,
        visibility: found_decl.visibility,
        parent: found_decl.parent,
        scope: queries::fact_slot(db, Scope::Decl(found)),
    })
}

/// `scope`'s textual (`macro_rules!`) scope, every definition of each name, at exact size: a
/// module's `legacy` and the scope a child starts with.
fn textual_scope(scope: &ItemScope) -> Inherited {
    let mut out = Vec::with_capacity(scope.legacy_macros().count());
    // alloc: one exact box per name, the result's own
    out.extend(scope.legacy_macros().map(|(name, macs)| (*name, Box::from(macs))));
    out.into_boxed_slice()
}

/// An ancestor's row: its module bindings and children, as the chain toward the key's module
/// was declared in it.
fn ancestor_row(ancestor: &Ancestor) -> ModuleData {
    let mut scope = ItemScope::default();
    let mut globs = PerNsGlobImports::default();
    for &(name, def) in ancestor.modules.iter() {
        let import = def.types.and_then(|it| it.import);
        scope.push_res_with_import(&mut globs, (ancestor.module, name), def, import);
    }
    // alloc: a row of the view, made once per (view, ancestor) by `queries::row`
    let children = ancestor.children.iter().copied().collect();
    ModuleData::with_scope(ancestor.origin, ancestor.visibility, ancestor.parent, scope, children)
}

/// The round-0 key of `module`, found from its parent: a child of the key's module (its
/// `module_decls`), a collected child of an ancestor (the ancestor's `child_keys`), or a child of
/// a module found so.
fn find_key(
    db: &Db,
    key: DeclKey,
    decl: &Decl,
    env: &CollectEnv,
    module: ModuleId,
) -> Option<DeclKey> {
    let parent = module.containing_module_inside_def_map(db)?;
    let parent = parent.to_static();
    if parent == decl.module {
        return queries::child_key(db, key, module);
    }
    if let Some(ancestor) = env.ancestors.iter().find(|it| it.module == parent) {
        return ancestor.child_keys.iter().find(|it| it.0 == module).map(|it| it.1);
    }
    let parent_key = find_key(db, key, decl, env, parent)?;
    queries::child_key(db, parent_key, module)
}

// The definition paths every sink shares.

/// The std prelude `use std::prelude::rust_20xx::*` names (`inject_prelude`).
/// See compiler/rustc_builtin_macros/src/standard_library_imports.rs.
fn inject_prelude<'db, S: Sink<'db>>(sink: &mut S) {
    let db = sink.db();
    let def_map = sink.def_map();
    if def_map.data.no_core {
        // libcore does not get a prelude.
        return;
    }

    let krate = match def_map.data.no_std {
        true => Name::new_symbol_root(sym::core),
        false => Name::new_symbol_root(sym::std),
    };
    let root = def_map.root;
    if !sink.local_def_map().extern_prelude().any(|(name, _)| *name == krate) {
        let diagnostic = DefDiagnostic::prelude(root, krate, PreludeError::CrateNotFound);
        sink.apply(DeclOp::Diagnostic(diagnostic));
        return;
    }

    let edition = match def_map.data.edition {
        Edition::Edition2015 => Name::new_symbol_root(sym::rust_2015),
        Edition::Edition2018 => Name::new_symbol_root(sym::rust_2018),
        Edition::Edition2021 => Name::new_symbol_root(sym::rust_2021),
        Edition::Edition2024 => Name::new_symbol_root(sym::rust_2024),
    };

    let path_kind = match def_map.data.edition {
        Edition::Edition2015 => PathKind::Plain,
        _ => PathKind::Abs,
    };
    let path =
        ModPath::from_segments(path_kind, [krate, Name::new_symbol_root(sym::prelude), edition]);

    let (per_ns, _) = def_map.resolve_path(
        sink.local_def_map(),
        db,
        def_map.root_module_id(),
        &path,
        BuiltinShadowMode::Other,
        None,
    );

    match per_ns.types {
        Some(Item { def: ModuleDefId::ModuleId(m), import, .. }) => {
            sink.apply(DeclOp::Prelude {
                module: m,
                import: import.and_then(ImportOrExternCrate::use_),
            });
        }
        _ => {
            let diagnostic = DefDiagnostic::prelude(root, krate, PreludeError::NoPreludeModule);
            sink.apply(DeclOp::Diagnostic(diagnostic));
        }
    }
}

/// Defines the `extern crate std;` (`core` under `no_std`) that rustc inserts as the first
/// item of the crate root, as a private name in the root's type namespace.
///
/// rustc gives that item a call-site name in edition 2015 and a def-site one from 2018 on
/// (compiler/rustc_builtin_macros/src/standard_library_imports.rs). So in 2015 paths that look
/// the name up in the root module itself, such as `crate::std::...`, `super::std::...` or a
/// macro's `$crate::std::...` (lewton 0.10's own `try!`), find it, and in later editions they do
/// not. Plain paths reach the crate through the extern prelude in every edition: a dependency
/// cargo passes `noprelude` (build-std's `--extern noprelude:std`) is put there by this item,
/// as by any `extern crate` of the root, and the std prelude is injected after it. `core` is in
/// the extern prelude even when it is not the item's (rustc's `Resolver::new` adds `core` unless
/// `no_core`, then `std` unless `no_std`), so a 2018 crate's `use core::..` finds build-std's
/// `noprelude:core`.
fn inject_std_extern_crate<'db, S: Sink<'db>>(cx: &CollectCx<'db, '_>, sink: &mut S) {
    let data = &sink.def_map().data;
    if data.no_core {
        return;
    }
    let core = Name::new_symbol_root(sym::core);
    if !sink.local_def_map().extern_prelude.contains_key(&core)
        && let Some(dep) = cx.dep(&core)
    {
        let root = crate_def_map(cx.db, dep.crate_id).root_module_id();
        sink.apply(DeclOp::ExternPrelude { name: core, root, extern_crate: None });
    }
    let data = &sink.def_map().data;
    let edition = data.edition;
    let name = match data.no_std {
        true => Name::new_symbol_root(sym::core),
        false => Name::new_symbol_root(sym::std),
    };
    let krate_root = match sink.local_def_map().extern_prelude.get(&name) {
        Some(&(krate_root, _)) => krate_root,
        // Without the crate rustc stops with "can't find crate"; there is no item to point at
        // then, and `inject_prelude` reports it.
        None => {
            let Some(dep) = cx.dep(&name) else { return };
            let krate_root = crate_def_map(cx.db, dep.crate_id).root_module_id();
            sink.apply(DeclOp::ExternPrelude { name, root: krate_root, extern_crate: None });
            krate_root
        }
    };
    if edition != Edition::Edition2015 {
        return;
    }
    let root = sink.def_map().root;
    let vis = Visibility::Module(root, VisibilityExplicitness::Implicit);
    sink.apply(DeclOp::Update {
        module: root,
        // alloc: the log owns each update's resolutions
        resolutions: Box::new([(Some(name), PerNs::types(krate_root.into(), vis, None))]),
        vis,
        import: None,
    });
}

/// `ModCollector { .. }.collect(items, container)` through `sink`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_items<'db, S: Sink<'db>, A: Allocator + Clone>(
    cx: &CollectCx<'db, '_>,
    sink: &mut S,
    side: &mut SideTables<A>,
    module: ModuleId,
    tree: TreeId,
    macro_depth: u32,
    mod_dir: ModDir,
    items: &[ModItemId],
    container: ItemContainerId,
) {
    let mut path_buf = String::new(); // alloc: scratch for `mod` paths, grows once per collection
    ModCollector {
        cx,
        sink,
        side,
        macro_depth,
        module_id: module,
        tree_id: tree,
        item_tree: tree.item_tree(cx.db, cx.krate),
        mod_dir,
        path_buf: &mut path_buf,
    }
    .collect(items, container);
}

/// `collect_macro_expansion`: the items of `call`'s expansion, collected in `module`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_expansion<'db, S: Sink<'db>, A: Allocator + Clone>(
    cx: &CollectCx<'db, '_>,
    sink: &mut S,
    side: &mut SideTables<A>,
    module: ModuleId,
    call: MacroCallId,
    depth: u32,
    container: ItemContainerId,
    attr_macro_item: Option<AstId<ast::Item>>,
) {
    let _p = tracing::info_span!("nameres collect_expansion", depth).entered();
    let db = cx.db;
    if depth > sink.def_map().recursion_limit() {
        cov_mark::hit!(macro_expansion_overflow);
        sink.apply(DeclOp::Diagnostic(DefDiagnostic::recursion_overflow(db, module, call)));
        return;
    }
    sink.collecting_expansion(call);
    let def = call.loc(db).def;
    if let MacroDefKind::ProcMacro(_, exp, _) = def.kind
        && exp.requires_session(db, def.krate)
    {
        // Equal expansion output still has state changes to replay next revision.
        blaze_query::db::report_untracked_read(db);
        sink.apply(DeclOp::SessionMacros);
    }
    let file_id: HirFileId = call.into();

    let item_tree = file_item_tree(db, file_id, cx.krate);

    // Derive helpers that are in scope for an item are also in scope for attribute macro
    // expansions of that item (but not derive or fn like macros): the item an attribute macro
    // is on has its `#[helper]`s in the macro's input, and likely in its output. It ignores
    // items in blocks.
    if let Some(attr_macro_item) = attr_macro_item
        && let Some(derive_helpers) = side.derive_helpers.get(&attr_macro_item)
    {
        let mut copied = Vec::with_capacity_in(derive_helpers.len(), cx.frame);
        copied.extend_from_slice(derive_helpers);
        for item in item_tree.top_level_items() {
            side.derive_helpers_of(InFile::new(file_id, item.ast_id(item_tree)))
                .extend(copied.iter().copied());
        }
    }

    let mod_dir = if call.is_include_macro(db) { ModDir::root() } else { side.mod_dirs[&module] };

    collect_items(
        cx,
        sink,
        side,
        module,
        TreeId::new(file_id, None),
        depth,
        mod_dir,
        item_tree.top_level_items(),
        container,
    );
}

/// `seed_with_inner`: the items of a block, whose def map `sink` holds.
pub(crate) fn seed_block<'db, S: Sink<'db>, A: Allocator + Clone>(
    cx: &CollectCx<'db, '_>,
    sink: &mut S,
    side: &mut SideTables<A>,
    tree: TreeId,
) {
    let db = cx.db;
    let item_tree = tree.item_tree(db, cx.krate);
    let is_cfg_enabled =
        matches!(item_tree.top_level_attrs(), Some(AttrsOrCfgRef::Enabled { .. }) | None);
    if is_cfg_enabled {
        inject_prelude(sink);

        let module = sink.def_map().root;
        // A block `DefMap` is collected from scratch, but it may itself live inside a macro
        // expansion. Continue that chain's budget instead of restarting it, or a macro
        // expanding to an item that invokes the macro again never reaches the limit.
        let macro_depth = tree.file_id().macro_expansion_depth(db);
        collect_items(
            cx,
            sink,
            side,
            module,
            tree,
            macro_depth,
            ModDir::root(),
            item_tree.top_level_items(),
            module.into(),
        );
    }
}

/// Walks a single module, populating defs, imports and macros.
struct ModCollector<'a, 'db, 'f, S, A: Allocator + Clone> {
    cx: &'a CollectCx<'db, 'f>,
    sink: &'a mut S,
    side: &'a mut SideTables<A>,
    macro_depth: u32,
    module_id: ModuleId,
    tree_id: TreeId,
    item_tree: &'db ItemTree,
    mod_dir: ModDir,
    path_buf: &'a mut String,
}

impl<'db, 'f, S: Sink<'db>, A: Allocator + Clone> ModCollector<'_, 'db, 'f, S, A> {
    fn collect_in_top_module(&mut self, items: &[ModItemId]) {
        self.collect(items, self.module_id.into())
    }

    fn collect(&mut self, items: &[ModItemId], container: ItemContainerId) {
        let def_map = self.sink.def_map();
        let is_crate_root = self.module_id == def_map.root && def_map.block.is_none();

        // Not fresh for macros, and an `include!` collects at the root directory.
        if self.side.mod_dirs.insert(self.module_id, self.mod_dir) != Some(self.mod_dir) {
            self.sink.apply(DeclOp::SetModDir { module: self.module_id, dir: self.mod_dir });
        }

        let item_tree = self.item_tree;
        // extern crates should be processed eagerly instead of deferred to resolving.
        // `#[macro_use] extern crate` is hoisted to imports macros before collecting
        // any other items.
        if is_crate_root {
            for &item in items {
                if matches!(item_tree.index(item), ModItemKind::ExternCrate(..)) {
                    self.process_mod_item(item, container);
                }
            }
            for &item in items {
                if !matches!(item_tree.index(item), ModItemKind::ExternCrate(..)) {
                    self.process_mod_item(item, container);
                }
            }
        } else {
            for &item in items {
                self.process_mod_item(item, container);
            }
        }
    }

    fn resolve_vis(&self, visibility: &RawVisibility) -> Visibility {
        self.sink
            .def_map()
            .resolve_visibility(
                self.sink.local_def_map(),
                self.cx.db,
                self.module_id,
                visibility,
                false,
            )
            .unwrap_or(Visibility::Public)
    }

    /// Declares `def` in the module and binds `name` to it.
    fn update_def(
        &mut self,
        def: ModuleDefId,
        name: Name,
        vis: Visibility,
        value_ns_ctor_vis: Option<Visibility>,
    ) {
        let module = self.module_id;
        self.sink.apply(DeclOp::Declare { module, def });
        self.sink.apply(DeclOp::Update {
            module,
            // alloc: the log owns each update's resolutions
            resolutions: Box::new([(Some(name), PerNs::from_def(def, vis, value_ns_ctor_vis, None))]),
            vis,
            import: None,
        });
    }

    /// The builtin derives recorded for the ADT `id` before it was collected again: each
    /// becomes its impls, and the derive's helpers take the impl's id.
    fn consider_deferred_derives(&mut self, ast_id: FileAstId<ast::Adt>, id: AdtId) {
        let db = self.cx.db;
        let module = self.module_id;
        let adt: AstId<ast::Adt> = InFile::new(self.file_id(), ast_id);
        let item: AstId<ast::Item> = adt.map(|it| it.upcast());
        let Some(deferred_derives) = self.side.deferred_derives.remove(&item) else {
            return;
        };
        let ModCollector { sink, side, .. } = &mut *self;
        for deferred_derive in deferred_derives {
            crate::builtin_derive::with_derive_traits(deferred_derive.derive, |trait_| {
                let impl_id = BuiltinDeriveImplId::new(
                    db,
                    BuiltinDeriveImplLoc {
                        adt: id,
                        trait_,
                        derive_attr_id: deferred_derive.derive_attr_id,
                        derive_index: deferred_derive.derive_index,
                    },
                );
                sink.apply(DeclOp::DefineBuiltinDeriveImpl { module, imp: impl_id });
                sink.apply(DeclOp::SetDeriveMacroInvoc {
                    module,
                    adt,
                    call: Either::Right(impl_id),
                    attr: deferred_derive.derive_attr_id,
                    idx: deferred_derive.derive_index as usize,
                });
                // Change its helper attributes to the new id.
                if let Some(derive_helpers) = side.derive_helpers.get_mut(&item) {
                    for (_, _, call_id) in
                        &mut derive_helpers[deferred_derive.helpers_range.start..deferred_derive.helpers_range.end]
                    {
                        *call_id = Either::Right(impl_id);
                    }
                }
            });
        }
    }

    fn process_mod_item(&mut self, item_id: ModItemId, container: ItemContainerId) {
        let db = self.cx.db;
        let module_id = self.module_id;
        let item_tree = self.item_tree;
        let attrs = match item_tree.attrs(item_id) {
            Some(AttrsOrCfgRef::Enabled { attrs }) => attrs,
            None => Attrs::EMPTY,
            Some(AttrsOrCfgRef::CfgDisabled(cfg, _)) => {
                let ast_id = item_id.ast_id(item_tree).erase();
                self.emit_unconfigured_diagnostic(InFile::new(self.file_id(), ast_id), cfg);
                return;
            }
        };

        if let Err(()) = self.resolve_attributes(attrs, item_id, container) {
            // Do not process the item. It has at least one non-builtin attribute, so the
            // fixed-point algorithm is required to resolve the rest of them.
            return;
        }

        match item_tree.index(item_id) {
            ModItemKind::Mod(ast_id, module) => self.collect_module(item_id, ast_id, module, attrs),
            ModItemKind::Use(ast_id, use_) => {
                let id = UseLoc { container: module_id, id: InFile::new(self.file_id(), ast_id) }
                    .intern(db);
                self.sink.apply(DeclOp::DefineUseDecl { module: module_id, id });
                let is_prelude = attrs.by_key(sym::prelude_import).exists();
                let visibility = &item_tree[use_.visibility];
                let sink = &mut *self.sink;
                use_.expand(item_tree, |idx, path, kind, alias| {
                    sink.apply(DeclOp::Import(ImportOp {
                        module: module_id,
                        path,
                        alias,
                        // The visibility holds a path only for `pub(in path)`.
                        // clone: the log owns each use tree's visibility
                        visibility: visibility.clone(),
                        source: ImportSource { use_tree: idx, id, is_prelude, kind },
                    }));
                })
            }
            ModItemKind::ExternCrate(ast_id, item) => {
                let item_tree::ExternCrate { name, visibility, alias } = item;
                let name = item_tree[*name];
                let alias = item_tree.import_alias(*alias);

                let id = ExternCrateLoc {
                    container: module_id,
                    id: InFile::new(self.tree_id.file_id(), ast_id),
                }
                .intern(db);
                self.sink.apply(DeclOp::DefineExternCrateDecl { module: module_id, id });

                let is_self = name == sym::self_;
                let resolved = if is_self {
                    cov_mark::hit!(extern_crate_self_as);
                    Some(self.sink.def_map().crate_root(db))
                } else {
                    self.cx.dep(&name).map(|dep| crate_def_map(db, dep.crate_id).root_module_id())
                };

                let name = match &alias {
                    Some(ImportAlias::Alias(name)) => Some(*name),
                    Some(ImportAlias::Underscore) => None,
                    None => Some(name),
                };

                let def_map = self.sink.def_map();
                let is_crate_root = module_id == def_map.root && def_map.block.is_none();
                if let Some(resolved) = resolved {
                    let vis = self.resolve_vis(&item_tree[*visibility]);

                    if is_crate_root {
                        // extern crates in the crate root are special-cased to insert entries
                        // into the extern prelude: rust-lang/rust#54658
                        if let Some(name) = name {
                            // rustc's `introduced_by_item`: renamed, or naming a crate the
                            // dependencies do not put in the extern prelude.
                            let from_deps = matches!(
                                self.sink.local_def_map().extern_prelude.get(&name),
                                Some((_, None))
                            );
                            if alias.is_none() && from_deps {
                                self.sink.apply(DeclOp::NotIntroducing(id));
                            }
                            self.sink.apply(DeclOp::ExternPrelude {
                                name,
                                root: resolved,
                                extern_crate: Some(id),
                            });
                        }
                        // they also allow `#[macro_use]`
                        if !is_self {
                            self.process_macro_use_extern_crate(
                                id,
                                attrs.by_key(sym::macro_use).attrs(),
                                resolved.krate(db),
                            );
                        }
                    }

                    self.sink.apply(DeclOp::Update {
                        module: module_id,
                        // alloc: the log owns each update's resolutions
                        resolutions: Box::new([(name, PerNs::types(resolved.into(), vis, Some(ImportOrExternCrate::ExternCrate(id))))]),
                        vis,
                        import: Some(ImportOrExternCrate::ExternCrate(id)),
                    });
                } else {
                    if let Some(name) = name {
                        self.sink.apply(DeclOp::UnresolvedExternCrate { name });
                    }
                    self.sink.apply(DeclOp::Diagnostic(DefDiagnostic::unresolved_extern_crate(
                        module_id,
                        InFile::new(self.file_id(), ast_id),
                    )));
                }
            }
            ModItemKind::ExternBlock(ast_id, block) => {
                let extern_block_id = ExternBlockLoc {
                    container: module_id,
                    id: InFile::new(self.file_id(), ast_id),
                }
                .intern(db);
                self.sink.apply(DeclOp::DefineExternBlock { module: module_id, id: extern_block_id });
                self.collect(
                    item_tree.items(block.children),
                    ItemContainerId::ExternBlockId(extern_block_id),
                )
            }
            ModItemKind::MacroCall(ast_id, mac) => self.collect_macro_call(ast_id, mac, container),
            ModItemKind::MacroRules(ast_id, mac) => {
                self.collect_macro_rules(attrs, ast_id, mac, module_id)
            }
            ModItemKind::Macro2(ast_id, mac) => self.collect_macro_def(attrs, ast_id, mac, module_id),
            ModItemKind::Impl(ast_id, imp) => {
                let impl_id =
                    ImplLoc { container: module_id, id: InFile::new(self.file_id(), ast_id) }
                        .intern(db);
                let is_reservation = attrs.by_key(sym::rustc_reservation_impl).exists();
                self.sink.apply(DeclOp::DefineImpl {
                    module: module_id,
                    imp: impl_id,
                    is_trait_impl: imp.is_trait_impl,
                    is_reservation,
                })
            }
            ModItemKind::Function(ast_id, it) => {
                let fn_id =
                    FunctionLoc { container, id: InFile::new(self.tree_id.file_id(), ast_id) }
                        .intern(db);

                let vis = self.resolve_vis(&item_tree[it.visibility]);

                self.update_def(fn_id.into(), item_tree[it.name], vis, None);

                let def_map = self.sink.def_map();
                if def_map.block.is_none()
                    && self.cx.is_proc_macro
                    && module_id == def_map.root
                    && let Some(proc_macro) = attrs.parse_proc_macro_decl(&item_tree[it.name])
                {
                    // A proc macro is implemented as a function, which is a macro to other
                    // crates. In its own it is a function still, which can be called and
                    // which rustc's list of the crate's macros names from the root;
                    // `censor_non_proc_macros` keeps it to the crate.
                    self.export_proc_macro(proc_macro, InFile::new(self.file_id(), ast_id), fn_id);
                }
            }
            ModItemKind::Struct(ast_id, it) => {
                let vis = self.resolve_vis(&item_tree[it.visibility]);
                let value_ns_ctor_vis = match item_tree.value_ns_ctor(it) {
                    StructValueNsCtor::NoValueNsCtor => None,
                    StructValueNsCtor::ValueNsCtorWithVis(vis) => {
                        Some(self.resolve_vis(&item_tree[*vis]))
                    }
                    StructValueNsCtor::ValueNsCtorWithMinVis(multiple) => {
                        let def_map = self.sink.def_map();
                        let mut each =
                            multiple.iter().map(|&vis| self.resolve_vis(&item_tree[vis]));
                        let first = each.next();
                        each.fold(first, |a: Option<Visibility>, b| a?.min(db, b, def_map))
                    }
                };
                let interned = StructLoc {
                    container: module_id,
                    id: InFile::new(self.tree_id.file_id(), ast_id),
                }
                .intern(db);
                self.consider_deferred_derives(ast_id.upcast(), interned.into());
                self.update_def(interned.into(), item_tree[it.name], vis, value_ns_ctor_vis);
            }
            ModItemKind::Union(ast_id, it) => {
                let vis = self.resolve_vis(&item_tree[it.visibility]);
                let interned = UnionLoc {
                    container: module_id,
                    id: InFile::new(self.tree_id.file_id(), ast_id),
                }
                .intern(db);
                self.consider_deferred_derives(ast_id.upcast(), interned.into());
                self.update_def(interned.into(), item_tree[it.name], vis, None);
            }
            ModItemKind::Enum(ast_id, it) => {
                let enum_ = EnumLoc {
                    container: module_id,
                    id: InFile::new(self.tree_id.file_id(), ast_id),
                }
                .intern(db);

                self.consider_deferred_derives(ast_id.upcast(), enum_.into());
                let vis = self.resolve_vis(&item_tree[it.visibility]);
                self.update_def(enum_.into(), item_tree[it.name], vis, None);
            }
            ModItemKind::Const(ast_id, it) => {
                let const_id =
                    ConstLoc { container, id: InFile::new(self.tree_id.file_id(), ast_id) }
                        .intern(db);

                match item_tree.get_name(it.name) {
                    Some(&name) => {
                        let vis = self.resolve_vis(&item_tree[it.visibility]);
                        self.update_def(const_id.into(), name, vis, None);
                    }
                    None => {
                        // const _: T = ...;
                        self.sink.apply(DeclOp::DefineUnnamedConst { module: module_id, id: const_id });
                    }
                }
            }
            ModItemKind::Static(ast_id, it) => {
                let vis = self.resolve_vis(&item_tree[it.visibility]);
                let static_id =
                    StaticLoc { container, id: InFile::new(self.file_id(), ast_id) }.intern(db);
                self.update_def(static_id.into(), item_tree[it.name], vis, None);
            }
            ModItemKind::Trait(ast_id, it) => {
                let vis = self.resolve_vis(&item_tree[it.visibility]);
                let trait_id =
                    TraitLoc { container: module_id, id: InFile::new(self.file_id(), ast_id) }
                        .intern(db);
                self.update_def(trait_id.into(), item_tree[it.name], vis, None);
            }
            ModItemKind::TypeAlias(ast_id, it) => {
                let vis = self.resolve_vis(&item_tree[it.visibility]);
                let alias_id =
                    TypeAliasLoc { container, id: InFile::new(self.file_id(), ast_id) }.intern(db);
                self.update_def(alias_id.into(), item_tree[it.name], vis, None);
            }
        }
    }

    fn process_macro_use_extern_crate<'attr>(
        &mut self,
        extern_crate_id: ExternCrateId,
        macro_use_attrs: impl Iterator<Item = &'attr hir_expand::attrs::Attr>,
        target_crate: Crate,
    ) {
        cov_mark::hit!(macro_rules_from_other_crates_are_visible_with_macro_use);
        let mut single_imports = Vec::new_in(self.cx.frame);
        for attr in macro_use_attrs {
            let Some(paths) = attr.parse_path_comma_token_tree(self.cx.db) else {
                // `#[macro_use]` (without any paths) found, forget collected names and just import
                // all visible macros.
                self.import_macros_from_extern_crate(target_crate, None, Some(extern_crate_id));
                return;
            };
            for (path, _, _) in paths {
                if let Some(name) = path.as_ident() {
                    single_imports.push(*name);
                }
            }
        }

        self.import_macros_from_extern_crate(
            target_crate,
            Some(&single_imports),
            Some(extern_crate_id),
        );
    }

    /// Import exported macros from another crate. `names`, if `Some(_)`, specifies the name of
    /// macros to be imported. Otherwise this method imports all exported macros.
    ///
    /// Exported macros are just all macros in the root module scope.
    /// Note that it contains not only all `#[macro_export]` macros, but also all aliases
    /// created by `use` in the root module, ignoring the visibility of `use`.
    fn import_macros_from_extern_crate(
        &mut self,
        krate: Crate,
        names: Option<&[Name]>,
        extern_crate: Option<ExternCrateId>,
    ) {
        let db = self.cx.db;
        let def_map = crate_def_map(db, krate);
        // `#[macro_use]` brings macros into macro_use prelude. Yes, even non-`macro_rules!`
        // macros.
        let root_scope = def_map.module(db, def_map.root).scope(db);
        match names {
            Some(names) => {
                for &name in names {
                    // FIXME: Report diagnostic on 404.
                    if let Some(def) = root_scope.get(&name).take_macros() {
                        self.sink.apply(DeclOp::MacroUsePrelude { name, mac: def, extern_crate });
                    }
                }
            }
            None => {
                for (name, it) in root_scope.macros() {
                    self.sink.apply(DeclOp::MacroUsePrelude {
                        name: *name,
                        mac: it.def,
                        extern_crate,
                    });
                }
            }
        }
    }

    fn collect_module(
        &mut self,
        item_id: ModItemId,
        module_ast_id: ItemTreeAstId<Mod>,
        module: &Mod,
        attrs: Attrs<'_>,
    ) {
        let db = self.cx.db;
        let krate = self.cx.krate;
        let item_tree = self.item_tree;
        let name = item_tree[module.name];
        let path_attr = attrs.by_key(sym::path).string_value_unescape();
        let is_macro_use = attrs.by_key(sym::macro_use).exists();
        match item_tree.mod_kind(module) {
            // inline module, just recurse
            ModKind::Inline { items } => {
                let mod_dir =
                    self.mod_dir.descend_into_definition(&name, path_attr.as_deref(), self.path_buf);
                let (child_items, child_dir) = match mod_dir {
                    Some(dir) => (DeclItems::Inline(item_id), dir),
                    None => (DeclItems::Nothing, self.mod_dir),
                };
                let Some((module_id, mode)) = self.push_child_module(
                    name,
                    module_ast_id,
                    None,
                    &item_tree[module.visibility],
                    self.tree_id,
                    child_items,
                    child_dir,
                ) else {
                    return;
                };
                let Some(mod_dir) = mod_dir else { return };
                if let ChildMode::Inline = mode {
                    self.child(module_id, self.tree_id, item_tree, mod_dir).collect_in_top_module(items);
                }
                if is_macro_use {
                    self.import_all_legacy_macros(module_id, mode);
                }
            }
            // out of line module, resolve, parse and recurse
            ModKind::Outline => {
                let file_id = self.file_id();
                let ast_id = AstId::new(file_id, module_ast_id);
                match self.mod_dir.resolve_declaration(
                    db,
                    krate,
                    file_id,
                    &name,
                    path_attr.as_deref(),
                    self.path_buf,
                ) {
                    Ok((file_id, is_mod_rs, mod_dir)) => {
                        let head = mod_file_head(db, file_id, krate);
                        match &head.cfg_disabled {
                            Some(cfg) => {
                                self.sink.configured_out_file(file_id);
                                self.emit_unconfigured_diagnostic(
                                    InFile::new(self.file_id(), module_ast_id.erase()),
                                    cfg,
                                );
                            }
                            None => {
                                let tree = TreeId::new(file_id.into(), None);
                                let Some((module_id, mode)) = self.push_child_module(
                                    name,
                                    ast_id.value,
                                    Some((file_id, is_mod_rs)),
                                    &item_tree[module.visibility],
                                    tree,
                                    DeclItems::Top,
                                    mod_dir,
                                ) else {
                                    return;
                                };
                                if let ChildMode::Inline = mode {
                                    let child_tree = file_item_tree(db, file_id.into(), krate);
                                    self.child(module_id, tree, child_tree, mod_dir)
                                        .collect_in_top_module(child_tree.top_level_items());
                                }
                                if is_macro_use || head.macro_use {
                                    self.import_all_legacy_macros(module_id, mode);
                                }
                            }
                        }
                    }
                    Err(candidates) => {
                        self.push_child_module(
                            name,
                            ast_id.value,
                            None,
                            &item_tree[module.visibility],
                            self.tree_id,
                            DeclItems::Nothing,
                            self.mod_dir,
                        );
                        self.sink.apply(DeclOp::Diagnostic(DefDiagnostic::unresolved_module(
                            self.module_id,
                            ast_id,
                            candidates,
                        )));
                    }
                };
            }
        }
    }

    /// A collector of `module`'s items in `tree`, sharing this one's sink and tables.
    fn child<'c>(
        &'c mut self,
        module: ModuleId,
        tree: TreeId,
        item_tree: &'db ItemTree,
        mod_dir: ModDir,
    ) -> ModCollector<'c, 'db, 'f, S, A> {
        ModCollector {
            cx: self.cx,
            sink: &mut *self.sink,
            side: &mut *self.side,
            macro_depth: self.macro_depth,
            module_id: module,
            tree_id: tree,
            item_tree,
            mod_dir,
            path_buf: &mut *self.path_buf,
        }
    }

    /// Declares the child module `name`: its row comes from the sink (`ChildMode::Separate`, its
    /// own round-0 query) or from its `Init` (`ChildMode::Inline`, whose items the caller
    /// collects next).
    #[allow(clippy::too_many_arguments)]
    fn push_child_module(
        &mut self,
        name: Name,
        declaration: FileAstId<ast::Module>,
        definition: Option<(EditionedFileId, bool)>,
        visibility: &RawVisibility,
        tree: TreeId,
        items: DeclItems,
        mod_dir: ModDir,
    ) -> Option<(ModuleId, ChildMode)> {
        let db = self.cx.db;
        // A module's id is its parent and name, so a second `mod` of a name (E0428) declares
        // nothing: the first keeps the name, as in rustc's resolver.
        if self.sink.def_map().module(db, self.module_id).children(db).contains_key(&name) {
            return None;
        }
        let vis = self.resolve_vis(visibility);
        let origin = match definition {
            None => {
                ModuleOrigin::Inline { definition: declaration, definition_tree_id: self.tree_id }
            }
            Some((definition, is_mod_rs)) => ModuleOrigin::File {
                declaration,
                definition,
                is_mod_rs,
                declaration_tree_id: self.tree_id,
            },
        };

        // The creating query is keyed by content including the parent, so every revision in
        // which a value naming this id is live re-creates it and the query database keeps it
        // (nameres-core.md §3).
        let module = super::queries::child_module(
            db,
            self.cx.krate,
            self.sink.def_map().block_id(),
            self.module_id,
            name,
        );
        let inherited = textual_scope(self.sink.def_map().module(db, self.module_id).scope(db));
        let parent = self.module_id;
        self.sink.apply(DeclOp::PushChild { parent, name, child: module });

        let def = ModuleDefId::from(module);
        self.sink.apply(DeclOp::Declare { module: parent, def });
        self.sink.apply(DeclOp::Update {
            module: parent,
            // alloc: the log owns each update's resolutions
            resolutions: Box::new([(Some(name), PerNs::from_def(def, vis, None, None))]),
            vis,
            import: None,
        });
        let mode = self.sink.child(ChildRequest {
            parent,
            child: module,
            name,
            origin,
            visibility: vis,
            tree,
            items,
            mod_dir,
            macro_depth: self.macro_depth,
            inherited: &inherited,
        });
        if let ChildMode::Inline = mode {
            self.sink.apply(DeclOp::Init {
                module,
                origin,
                visibility: vis,
                parent: Some(parent),
                inherited,
            });
        }
        Some((module, mode))
    }

    /// Resolves attributes on an item.
    ///
    /// Returns `Err` when some attributes could not be resolved to builtins and have been
    /// registered as unresolved.
    ///
    /// The attributes up to and including the item's entry of `skip_attrs` are taken as
    /// resolved already.
    fn resolve_attributes(
        &mut self,
        attrs: Attrs<'_>,
        mod_item: ModItemId,
        container: ItemContainerId,
    ) -> Result<(), ()> {
        let skip_attrs = &self.side.skip_attrs;
        let ignore_up_to = match skip_attrs.is_empty() {
            true => None,
            false => skip_attrs.get(&InFile::new(self.file_id(), mod_item)).copied(),
        };
        for (attr_id, attr) in attrs.iter_after(ignore_up_to) {
            if self.sink.def_map().is_builtin_or_registered_attr(&attr.path) {
                continue;
            }
            tracing::debug!(
                "non-builtin attribute {}",
                attr.path.display(self.cx.db, Edition::LATEST)
            );

            let ast_id = AstIdWithPath::new(
                self.file_id(),
                mod_item.ast_id(self.item_tree),
                ModPath::clone(&attr.path), // clone: the directive owns the attribute's path
            );
            let shape = item_tree::deferred_item_shape(self.cx.db, ast_id.ast_id);
            self.sink.apply(DeclOp::Macro(MacroOp {
                module: self.module_id,
                depth: self.macro_depth + 1,
                kind: MacroOpKind::Attr {
                    ast_id,
                    attr_id,
                    // clone: the directive owns its attribute, once per unresolved attribute
                    attr: attr.clone(),
                    mod_item,
                    tree: self.tree_id,
                    shape,
                },
                container,
            }));

            return Err(());
        }

        Ok(())
    }

    /// Define a macro with `macro_rules`.
    ///
    /// It will define the macro in legacy textual scope, and if it has `#[macro_export]`,
    /// then it is also defined in the root module scope.
    /// You can `use` or invoke it by `crate::macro_name` anywhere, before or after the definition.
    ///
    /// It is surprising that the macro will never be in the current module scope.
    /// These code fails with "unresolved import/macro",
    /// ```rust,compile_fail
    /// mod m { macro_rules! foo { () => {} } }
    /// use m::foo as bar;
    /// ```
    ///
    /// ```rust,compile_fail
    /// macro_rules! foo { () => {} }
    /// self::foo!();
    /// crate::foo!();
    /// ```
    ///
    /// Well, this code compiles, because the plain path `foo` in `use` is searched
    /// in the legacy textual scope only.
    /// ```rust
    /// macro_rules! foo { () => {} }
    /// use foo as bar;
    /// ```
    fn define_macro_rules(
        &mut self,
        module_id: ModuleId,
        name: Name,
        macro_: MacroRulesId,
        export: bool,
    ) {
        // Textual scoping
        self.sink.apply(DeclOp::DefineLegacyMacro { module: module_id, name, mac: macro_.into() });

        // Module scoping
        // In Rust, `#[macro_export]` macros are unconditionally visible at the
        // crate root, even if the parent modules is **not** visible.
        if export {
            let root = self.sink.def_map().root;
            self.sink.apply(DeclOp::Declare { module: root, def: macro_.into() });
            self.sink.apply(DeclOp::Update {
                module: root,
                // alloc: the log owns each update's resolutions
                resolutions: Box::new([(Some(name), PerNs::macros(macro_.into(), Visibility::Public, None))]),
                vis: Visibility::Public,
                import: None,
            });
        }
    }

    /// Define a macro 2.0 macro
    ///
    /// The scoped of macro 2.0 macro is equal to normal function
    fn define_macro_def(
        &mut self,
        module_id: ModuleId,
        name: Name,
        macro_: Macro2Id,
        vis: &RawVisibility,
    ) {
        let vis = self.resolve_vis(vis);
        self.sink.apply(DeclOp::Declare { module: module_id, def: macro_.into() });
        self.sink.apply(DeclOp::Update {
            module: module_id,
            // alloc: the log owns each update's resolutions
            resolutions: Box::new([(Some(name), PerNs::macros(macro_.into(), Visibility::Public, None))]),
            vis,
            import: None,
        });
    }

    /// Define a proc macro
    ///
    /// A proc macro is similar to normal macro scope, but it would not visible in legacy textual scoped.
    /// And unconditionally exported.
    fn define_proc_macro(&mut self, name: Name, macro_: ProcMacroId) {
        let root = self.sink.def_map().root;
        self.sink.apply(DeclOp::Declare { module: root, def: macro_.into() });
        self.sink.apply(DeclOp::Update {
            module: root,
            // alloc: the log owns each update's resolutions
            resolutions: Box::new([(Some(name), PerNs::macros(macro_.into(), Visibility::Public, None))]),
            vis: Visibility::Public,
            import: None,
        });
    }

    /// Adds a definition of procedural macro `name` to the root module.
    ///
    /// The list of proc macros the crate exports is known before its names are resolved
    /// (`CollectCx::proc_macros`, from its built library). A macro not in it still resolves,
    /// to an expander that always errors.
    fn export_proc_macro(&mut self, def: ProcMacroDef, ast_id: AstId<ast::Fn>, fn_id: FunctionId) {
        let db = self.cx.db;
        let kind = def.kind.to_basedb_kind();
        let (expander, kind) = match self.cx.proc_macros.iter().find(|(n, _, _)| n == &def.name) {
            Some(&(_, _, true)) => (CustomProcMacroExpander::disabled(), kind),
            Some(&(_, expander, false)) => (expander, kind),
            None => (CustomProcMacroExpander::missing_expander(), kind),
        };

        let def_map = self.sink.def_map();
        let proc_macro_id = ProcMacroLoc {
            container: def_map.root_module_id(),
            id: ast_id,
            expander,
            kind,
            edition: def_map.data.edition,
        }
        .intern(db);
        self.define_proc_macro(def.name, proc_macro_id);
        if let ProcMacroKind::Derive { helpers } = def.kind {
            self.sink.apply(DeclOp::ExportedDerive { mac: proc_macro_id.into(), helpers });
        }
        self.sink.apply(DeclOp::ProcMacroFn { func: fn_id, mac: proc_macro_id });
    }

    fn collect_macro_rules(
        &mut self,
        attrs: Attrs<'_>,
        ast_id: ItemTreeAstId<MacroRules>,
        mac: &MacroRules,
        module: ModuleId,
    ) {
        let db = self.cx.db;
        let krate = self.cx.krate;
        let f_ast_id = InFile::new(self.file_id(), ast_id.upcast());

        let export_attr = || attrs.by_key(sym::macro_export);

        let is_export = export_attr().exists();
        let local_inner = if is_export {
            export_attr().tt_values().flat_map(|it| it.iter()).any(|it| match it {
                tt::TtElement::Leaf(tt::Leaf::Ident(ident)) => ident.sym == sym::local_inner_macros,
                _ => false,
            })
        } else {
            false
        };

        // Case 1: builtin macros
        let expander = if attrs.by_key(sym::rustc_builtin_macro).exists() {
            // `#[rustc_builtin_macro = "builtin_name"]` overrides the `macro_rules!` name.
            let name = match attrs.by_key(sym::rustc_builtin_macro).string_value_with_span() {
                Some((it, span)) => Name::new_symbol(Symbol::intern(it), span.ctx),
                None => {
                    let explicit_name =
                        attrs.by_key(sym::rustc_builtin_macro).tt_values().next().and_then(|tt| {
                            match tt.token_trees().iter().next() {
                                Some(tt::TtElement::Leaf(tt::Leaf::Ident(name))) => Some(name),
                                _ => None,
                            }
                        });
                    match explicit_name {
                        Some(ident) => ident.as_name(),
                        None => self.item_tree[mac.name],
                    }
                }
            };
            match find_builtin_macro(&name) {
                Some(Either::Left(it)) => MacroExpander::BuiltIn(it),
                Some(Either::Right(it)) => MacroExpander::BuiltInEager(it),
                None => {
                    self.sink.apply(DeclOp::Diagnostic(DefDiagnostic::unimplemented_builtin_macro(
                        self.module_id,
                        f_ast_id,
                    )));
                    MacroExpander::UnimplementedBuiltIn
                }
            }
        } else {
            // Case 2: normal `macro_rules!` macro
            let id = InFile::new(self.file_id(), ast_id);
            let decl_expander = id.upcast().decl_macro_expander(db, krate);
            let styles = decl_expander.mac.rule_styles();
            MacroExpander::Declarative { styles }
        };
        let allow_internal_unsafe = attrs.by_key(sym::allow_internal_unsafe).exists();

        let mut flags = MacroRulesLocFlags::empty();
        flags.set(MacroRulesLocFlags::LOCAL_INNER, local_inner);
        flags.set(MacroRulesLocFlags::ALLOW_INTERNAL_UNSAFE, allow_internal_unsafe);
        let edition_panic = attrs
            .by_key(sym::allow_internal_unstable)
            .tt_values()
            .flat_map(|it| it.iter())
            .any(|it| matches!(it, tt::TtElement::Leaf(tt::Leaf::Ident(ident)) if ident.sym == sym::edition_panic));
        flags.set(MacroRulesLocFlags::EDITION_PANIC, edition_panic);

        let macro_id = MacroRulesLoc {
            container: module,
            id: InFile::new(self.file_id(), ast_id),
            flags,
            expander,
            edition: self.sink.def_map().data.edition,
        }
        .intern(db);
        self.define_macro_rules(self.module_id, self.item_tree[mac.name], macro_id, is_export);
    }

    fn collect_macro_def(
        &mut self,
        attrs: Attrs<'_>,
        ast_id: ItemTreeAstId<Macro2>,
        mac: &Macro2,
        module: ModuleId,
    ) {
        let db = self.cx.db;
        let krate = self.cx.krate;
        let item_tree = self.item_tree;
        let mac_name = item_tree[mac.name];
        let f_ast_id = InFile::new(self.file_id(), ast_id.upcast());

        // Case 1: builtin macros
        let mut helpers_opt = None;
        let expander = if attrs.by_key(sym::rustc_builtin_macro).exists() {
            if let Some(expander) = find_builtin_macro(&mac_name) {
                match expander {
                    Either::Left(it) => MacroExpander::BuiltIn(it),
                    Either::Right(it) => MacroExpander::BuiltInEager(it),
                }
            } else if let Some(expander) = find_builtin_derive(&mac_name) {
                if let Some(attr) = attrs.by_key(sym::rustc_builtin_macro).tt_values().next() {
                    // NOTE: The item *may* have both `#[rustc_builtin_macro]` and `#[proc_macro_derive]`,
                    // in which case rustc ignores the helper attributes from the latter, but it
                    // "doesn't make sense in practice" (see rust-lang/rust#87027).
                    if let Some((name, helpers)) = parse_macro_name_and_helper_attrs(attr) {
                        // NOTE: rustc overrides the name if the macro name if it's different from the
                        // macro name, but we assume it isn't as there's no such case yet. FIXME if
                        // the following assertion fails.
                        stdx::always!(
                            name == mac_name,
                            "built-in macro {} has #[rustc_builtin_macro] which declares different name {}",
                            mac_name.display(db, Edition::LATEST),
                            name.display(db, Edition::LATEST),
                        );
                        helpers_opt = Some(helpers);
                    }
                }
                MacroExpander::BuiltInDerive(expander)
            } else if let Some(expander) = find_builtin_attr(&mac_name) {
                MacroExpander::BuiltInAttr(expander)
            } else {
                self.sink.apply(DeclOp::Diagnostic(DefDiagnostic::unimplemented_builtin_macro(
                    self.module_id,
                    f_ast_id,
                )));
                MacroExpander::UnimplementedBuiltIn
            }
        } else {
            // Case 2: normal `macro`
            let id = InFile::new(self.file_id(), ast_id);
            let decl_expander = id.upcast().decl_macro_expander(db, krate);
            let styles = decl_expander.mac.rule_styles();
            MacroExpander::Declarative { styles }
        };
        let allow_internal_unsafe = attrs.by_key(sym::allow_internal_unsafe).exists();

        let macro_id = Macro2Loc {
            container: module,
            id: InFile::new(self.file_id(), ast_id),
            expander,
            allow_internal_unsafe,
            edition: self.sink.def_map().data.edition,
        }
        .intern(db);
        self.define_macro_def(self.module_id, mac_name, macro_id, &item_tree[mac.visibility]);
        if let Some(helpers) = helpers_opt
            && self.sink.def_map().block.is_none()
        {
            self.sink.apply(DeclOp::ExportedDerive { mac: macro_id.into(), helpers });
        }
    }

    fn collect_macro_call(
        &mut self,
        ast_id: FileAstId<ast::MacroCall>,
        macro_call: &MacroCall,
        container: ItemContainerId,
    ) {
        let &MacroCall { path, expand_to, ctxt } = macro_call;
        let path = &self.item_tree[path];
        // clone: the call's directive owns its path
        let ast_id = AstIdWithPath::new(self.file_id(), ast_id, ModPath::clone(path));
        let db = self.cx.db;
        let module_id = self.module_id;

        // FIXME: Immediately expanding in "Case 1" is insufficient since "Case 2" may also define
        // new legacy macros that create textual scopes. We need a way to resolve names in textual
        // scopes without eager expansion.

        let mut eager_calls = Vec::new_in(self.cx.frame);
        let named = std::cell::RefCell::new(Vec::new_in(self.cx.frame));
        // Case 1: try to resolve macro calls with single-segment name, or with a path to a macro
        // of another crate, and expand them, so that the `macro_rules!` they define are in the
        // textual scope of what follows, child modules included.
        let this = &*self;
        let res = macro_call_as_call_id(
            db,
            ast_id.ast_id,
            &ast_id.path,
            ctxt,
            expand_to,
            self.cx.krate,
            self.macro_depth + 1,
            self.sink.def_map().recursion_limit(),
            |path| {
                let def = this.case_one_macro_def(path);
                if let Some(def) = def
                    && path != &ast_id.path
                {
                    // clone: an eager argument's path, kept for `NameRecords`
                    named.borrow_mut().push((ModPath::clone(path), def));
                }
                def
            },
            &mut eager_calls,
        );
        if let Ok(res) = res {
            for (ptr, call_id) in eager_calls {
                self.sink.apply(DeclOp::AddMacroInvoc {
                    module: module_id,
                    call: ptr.map(|(_, it)| it),
                    id: call_id,
                });
            }
            // FIXME: if there were errors, this might've been in the eager expansion from an
            // unresolved macro, so we need to push this into late macro resolution. see fixme above
            if res.err.is_none() {
                // Legacy macros need to be expanded immediately, so that any macros they produce
                // are in scope.
                if let Some(call_id) = res.value {
                    self.sink.apply(DeclOp::ResolvedMacroPath {
                        module: module_id,
                        // clone: the record owns the call's path
                        path: ModPath::clone(&ast_id.path),
                        bang: true,
                        def: call_id.loc(db).def,
                        call: Some(call_id),
                    });
                    for (path, def) in named.into_inner() {
                        self.sink.apply(DeclOp::ResolvedMacroPath {
                            module: module_id,
                            path,
                            bang: true,
                            def,
                            call: None,
                        });
                    }
                    self.sink.apply(DeclOp::AddMacroInvoc {
                        module: module_id,
                        call: ast_id.ast_id,
                        id: call_id,
                    });
                    self.sink.expanded_now(module_id, call_id);
                    collect_expansion(
                        self.cx,
                        &mut *self.sink,
                        &mut *self.side,
                        module_id,
                        call_id,
                        self.macro_depth + 1,
                        container,
                        None,
                    );
                }

                return;
            }
        }

        // Case 2: resolve in module scope, expand during name resolution.
        self.sink.apply(DeclOp::Macro(MacroOp {
            module: module_id,
            depth: self.macro_depth + 1,
            kind: MacroOpKind::FnLike { ast_id, expand_to, ctxt },
            container,
        }));
    }

    /// What Case 1 of [`Self::collect_macro_call`] resolves `path` to: the textual scope, the
    /// module's scope, the `#[macro_use]` prelude, a name a `use` of another crate imports, or
    /// the std prelude.
    fn case_one_macro_def(&self, path: &ModPath) -> Option<MacroDefId> {
        let db = self.cx.db;
        let module_id = self.module_id;
        let sink = &*self.sink;
        {
                let def_map = sink.def_map();
                // Where the path goes on in another crate, that crate is collected in full, so
                // what it names there it will still name once this crate is.
                let of_another_crate = |path: &ModPath| {
                    let resolved = def_map.resolve_path_fp_with_macro(
                        sink.local_def_map(),
                        db,
                        ResolveMode::Other,
                        module_id,
                        path,
                        BuiltinShadowMode::Module,
                        Some(MacroSubNs::Bang),
                    );
                    let whole =
                        resolved.segment_index.is_none() && resolved.prefix_info.differing_crate;
                    resolved
                        .resolved_def
                        .take_macros()
                        .filter(|&id| whole && sub_namespace_match(db, id, Some(MacroSubNs::Bang)))
                        .map(|it| it.definition(db))
                };
                let Some(name) = path.as_ident() else { return of_another_crate(path) };
                // A name a `use` of this module imports from another crate (`use cfg_if::cfg_if;`)
                // is not in its scope until imports are resolved, after this call would be:
                // lol_html's `cfg_if!` defines its `trace!` for the modules that follow.
                let imported = || sink.find_unresolved_named(module_id, *name, of_another_crate);
                // The std prelude is another crate's, collected in full. Its `include!` read at
                // item level puts the `macro_rules!` of the file in the textual scope of what
                // follows: syn 1's `#[macro_use] mod token` has `Token!` from
                // `include!("await.rs")`, and its `mod op` names it with nothing imported.
                let prelude = || {
                    // A block's def map is collected after the def maps around it, so what the
                    // name is there (the std prelude, or an item of the module holding the
                    // block) it stays. clang-sys's `load_manually` has a `mod build` in its
                    // body whose `include!`d `macro_rules!` its `mod dynamic` uses.
                    if def_map.block.is_some() {
                        return def_map
                            .resolve_path_fp_with_macro(
                                sink.local_def_map(),
                                db,
                                ResolveMode::Other,
                                module_id,
                                path,
                                BuiltinShadowMode::Module,
                                Some(MacroSubNs::Bang),
                            )
                            .resolved_def
                            .take_macros()
                            .filter(|&id| sub_namespace_match(db, id, Some(MacroSubNs::Bang)))
                            .map(|it| it.definition(db));
                    }
                    let (prelude, _) = def_map.prelude?;
                    if prelude.krate(db) == def_map.krate {
                        return None;
                    }
                    def_map
                        .resolve_in_prelude(db, name)
                        .take_macros()
                        .filter(|&id| sub_namespace_match(db, id, Some(MacroSubNs::Bang)))
                        .map(|it| it.definition(db))
                };
                def_map
                    .legacy_macro_in_textual_scope(db, module_id, name)
                    .or_else(|| def_map.module(db, module_id).scope(db).get(name).take_macros())
                    .or_else(|| Some(def_map.macro_use_prelude.get(name).copied()?.0))
                    .filter(|&id| sub_namespace_match(db, id, Some(MacroSubNs::Bang)))
                    .map(|it| it.definition(db))
                    .or_else(imported)
                    .or_else(prelude)
        }
    }

    /// `#[macro_use] mod`: the child's `macro_rules!` join this module's textual scope, the last
    /// definition of each name.
    fn import_all_legacy_macros(&mut self, child: ModuleId, mode: ChildMode) {
        let db = self.cx.db;
        let module = self.module_id;
        match mode {
            ChildMode::Separate(key) => {
                for &(name, mac) in queries::legacy_exports(db, key).iter() {
                    self.sink.apply(DeclOp::DefineLegacyMacro { module, name, mac });
                }
            }
            ChildMode::Inline => {
                let mut exports = Vec::new_in(self.cx.frame);
                exports.extend(
                    self.sink
                        .def_map()
                        .module(db, child)
                        .scope(db)
                        .legacy_macros()
                        .filter_map(|(name, macs)| Some((*name, *macs.last()?))),
                );
                for (name, mac) in exports {
                    self.sink.apply(DeclOp::DefineLegacyMacro { module, name, mac });
                }
            }
        }
    }

    fn emit_unconfigured_diagnostic(&mut self, ast_id: ErasedAstId, cfg: &CfgExpr) {
        self.sink.apply(DeclOp::Diagnostic(DefDiagnostic::unconfigured_code(
            self.module_id,
            ast_id,
            // clone: the diagnostic owns the cfg, once per configured-out item
            cfg.clone(),
        )));
    }

    #[inline]
    fn file_id(&self) -> HirFileId {
        self.tree_id.file_id()
    }
}

#[cfg(test)]
mod tests {
    use hir_expand::fixture;
    use hir_expand::name::Name;
    use intern::Symbol;

    use crate::{
        ModuleId,
        nameres::facts::{
            ops::{DeclOp, MacroOpKind},
            queries::{self, DeclKey},
            unit,
        },
        test_db::TestDB,
    };

    fn name(text: &str) -> Name {
        Name::new_symbol_root(Symbol::intern(text))
    }

    /// Every round-0 key of the test crate, root first, in DFS preorder.
    fn keys(db: &TestDB) -> Vec<DeclKey> {
        let mut out = Vec::new();
        let mut stack = vec![queries::root_decl(db, fixture::test_crate(db))];
        while let Some(key) = stack.pop() {
            out.push(key);
            stack.extend(queries::module_decls(db, key).children.iter().rev().copied());
        }
        out
    }

    /// Replaying each module's round-0 log gives the scope its collection held at the end.
    fn assert_replay(fixture: &str) -> TestDB {
        let db = fixture::db(fixture).into_db();
        for key in keys(&db) {
            let decl = key.decl(&db);
            let decls = queries::module_decls(&db, key);
            let replayed = blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| {
                unit::local_scope(&db, frame, decl.module, &decl.inherited, &decls.ops)
            });
            assert_eq!(replayed, super::held_scope(key), "the round-0 scope of {:?}", decl.module);
        }
        db
    }

    fn root_key(db: &TestDB) -> DeclKey {
        queries::root_decl(db, fixture::test_crate(db))
    }

    fn child_key(db: &TestDB, parent: DeclKey, child: &str) -> DeclKey {
        let parent_module = parent.decl(db).module;
        *queries::module_decls(db, parent)
            .children
            .iter()
            .find(|key| key.decl(db).module.name(db) == Some(name(child)))
            .unwrap_or_else(|| panic!("{parent_module:?} has no round-0 child `{child}`"))
    }

    #[test]
    fn replay_equals_collection() {
        assert_replay(
            r#"
//- /lib.rs crate:main
extern crate self as this;
mod a;
pub mod b {
    pub struct S;
    pub(crate) fn f() {}
    pub(in crate::b) const C: u8 = 0;
    const _: () = ();
    pub mod inner {
        pub(super) struct T(u8);
        pub trait Tr {}
        impl Tr for T {}
    }
    use super::a::*;
    use inner::T as U;
    use inner::Tr as _;
}
macro_rules! m { () => {} }
mod c {
    m!();
    pub enum E { A }
    pub union Un { x: u8 }
    pub static ST: u8 = 0;
    pub type Ty = u8;
    extern "C" { fn ext(); }
}
#[cfg(any())]
mod off {}
//- /a.rs
pub struct A;
mod missing;
"#,
        );
    }

    #[test]
    fn macro_export_in_child_reaches_root() {
        let db = assert_replay(
            r#"
//- /lib.rs crate:main
mod a;
mod b {
    #[macro_export]
    macro_rules! exported { () => {} }
}
//- /a.rs
#[macro_export]
macro_rules! from_a { () => {} }
pub mod inner {
    #[macro_export]
    macro_rules! deep { () => {} }
}
"#,
        );
        let root = root_key(&db);
        let root_module = root.decl(&db).module;
        let held = super::held_scope(root);
        for it in ["exported", "from_a", "deep"] {
            assert!(held.scope.get(&name(it)).take_macros().is_some(), "`{it}` is not in the root");
        }
        let decls = queries::module_decls(&db, root);
        let splices = decls.ops.iter().filter(|op| matches!(op, DeclOp::Splice { .. })).count();
        assert_eq!(splices, 2, "one `Splice` per child of the root");
        let in_own_log = decls.ops.iter().any(|op| {
            matches!(op, DeclOp::Declare { module, .. } | DeclOp::Update { module, .. } if *module == root_module)
                && matches!(op, DeclOp::Update { resolutions, .. } if resolutions.iter().any(|it| it.0 == Some(name("from_a"))))
        });
        assert!(!in_own_log, "a child's export is in the root's own log");
        let a = child_key(&db, root, "a");
        let exports = queries::subtree_macro_exports(&db, a);
        assert!(exports.iter().any(|op| matches!(op, DeclOp::Update { resolutions, .. } if resolutions.iter().any(|it| it.0 == Some(name("deep"))))));
    }

    #[test]
    fn macro_use_mod_brings_child_macros() {
        let db = assert_replay(
            r#"
//- /lib.rs crate:main
#[macro_use]
mod m {
    macro_rules! inner_mac { () => {} }
}
#[macro_use]
mod f;
mod after {
    inner_mac!();
    file_mac!();
}
//- /f.rs
macro_rules! file_mac { () => {} }
"#,
        );
        let root = root_key(&db);
        let root_module: ModuleId = root.decl(&db).module;
        let held = super::held_scope(root);
        for it in ["inner_mac", "file_mac"] {
            assert!(held.scope.get_legacy_macro(&name(it)).is_some(), "`{it}` is not in the root's textual scope");
            assert!(
                queries::module_decls(&db, root).ops.iter().any(|op| matches!(op,
                    DeclOp::DefineLegacyMacro { module, name: n, .. } if *module == root_module && *n == name(it))),
                "no `DefineLegacyMacro` of `{it}` in the root's log"
            );
            assert!(queries::legacy_exports(&db, root).iter().any(|it2| it2.0 == name(it)));
        }
        // A later sibling inherits them, and its calls expand at once (case 1).
        let after = child_key(&db, root, "after");
        assert!(after.decl(&db).inherited.iter().any(|it| it.0 == name("inner_mac")));
        let ops = &queries::module_decls(&db, after).ops;
        assert!(ops.iter().all(|op| !matches!(op, DeclOp::Macro(_))), "a textual macro call was left for later");
        assert_eq!(ops.iter().filter(|op| matches!(op, DeclOp::AddMacroInvoc { .. })).count(), 2);
    }

    #[test]
    fn paths_into_declared_subtrees() {
        // The root reads its own child's subtree, and a later sibling an earlier one's, through
        // `decl_row`; neither resolves at round 0 (a path of this crate waits for case 2).
        let db = assert_replay(
            r#"
//- /lib.rs crate:main
mod a { pub mod b { #[macro_export] macro_rules! m { () => {} } } }
self::a::b::m!();
mod c {
    super::a::b::m!();
    pub(in crate::c) struct S;
}
"#,
        );
        let root = root_key(&db);
        for key in [root, child_key(&db, root, "c")] {
            let ops = &queries::module_decls(&db, key).ops;
            assert!(
                ops.iter().any(|op| matches!(op, DeclOp::Macro(it) if matches!(it.kind, MacroOpKind::FnLike { .. }))),
                "the call of {:?} is not a directive",
                key.decl(&db).module
            );
        }
    }
}
