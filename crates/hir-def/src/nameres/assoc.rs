//! Expansion of associated items

use std::mem;

use base_db::Db;
use cfg::CfgOptions;
use hir_expand::{
    AstId, AttrMacroAttrIds, ExpandTo, HirFileId, InFile, Intern, Lookup, MacroCallKind,
    MacroDefKind,
    mod_path::ModPath,
    name::{AsName, Name},
    span_map::SpanMap,
};
use span::AstIdMap;
use syntax::{
    ast,
    view::{self, AstNode, HasModuleItem, HasName},
};
use thin_vec::ThinVec;

use crate::{
    AssocItemId, AstIdWithPath, ConstLoc, FunctionId, FunctionLoc, ImplId, ItemContainerId,
    ItemLoc, MacroCallId, ModuleId, TraitId, TypeAliasId, TypeAliasLoc,
    item_tree::AttrsOrCfg,
    macro_call_as_call_id,
    nameres::{
        DefMap, LocalDefMap, MacroSubNs,
        attr_resolution::ResolvedAttr,
        diagnostics::{DefDiagnostic, DefDiagnostics},
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraitItems {
    pub items: Box<[(Name, AssocItemId)]>,
    // `ThinVec` as the vec is usually empty anyways
    pub macro_calls: ThinVec<(AstId<ast::Item>, MacroCallId)>,
}

#[blaze_query::tracked]
impl TraitItems {
    #[inline]
    pub(crate) fn query(db: &Db, tr: TraitId) -> &TraitItems {
        &Self::query_with_diagnostics(db, tr).0
    }

    #[blaze_query::tracked(returns(ref))]
    pub fn query_with_diagnostics(
        db: &Db,
        tr: TraitId,
    ) -> (TraitItems, DefDiagnostics) {
        let ItemLoc { container: module_id, id: ast_id } = *tr.lookup(db);
        let source = ast_id.to_view(db);
        if source.eq_token().is_some() {
            // FIXME(trait-alias) probably needs special handling here
            return (
                TraitItems { macro_calls: ThinVec::new(), items: Box::default() },
                // alloc: empty; allocates nothing
                DefDiagnostics::new(vec![]),
            );
        }

        let collector =
            AssocItemCollector::new(db, module_id, ItemContainerId::TraitId(tr), ast_id.file_id);
        let (items, macro_calls, diagnostics) = collector.collect(source.assoc_item_list());

        (TraitItems { macro_calls, items }, DefDiagnostics::new(diagnostics))
    }

    pub fn associated_types(&self) -> impl Iterator<Item = TypeAliasId> + '_ {
        self.items.iter().filter_map(|(_name, item)| match item {
            AssocItemId::TypeAliasId(t) => Some(*t),
            _ => None,
        })
    }

    pub fn associated_type_by_name(&self, name: &Name) -> Option<TypeAliasId> {
        self.items.iter().find_map(|(item_name, item)| match item {
            AssocItemId::TypeAliasId(t) if item_name == name => Some(*t),
            _ => None,
        })
    }

    pub fn method_by_name(&self, name: &Name) -> Option<FunctionId> {
        self.items.iter().find_map(|(item_name, item)| match item {
            AssocItemId::FunctionId(t) if item_name == name => Some(*t),
            _ => None,
        })
    }

    pub fn assoc_item_by_name(&self, name: &Name) -> Option<AssocItemId> {
        self.items.iter().find_map(|&(ref item_name, item)| match item {
            AssocItemId::FunctionId(_) if item_name == name => Some(item),
            AssocItemId::TypeAliasId(_) if item_name == name => Some(item),
            AssocItemId::ConstId(_) if item_name == name => Some(item),
            _ => None,
        })
    }

    pub fn macro_calls(&self) -> impl Iterator<Item = (AstId<ast::Item>, MacroCallId)> + '_ {
        self.macro_calls.iter().copied()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ImplItems {
    pub items: Box<[(Name, AssocItemId)]>,
    // `ThinVec` as the vec is usually empty anyways
    pub macro_calls: ThinVec<(AstId<ast::Item>, MacroCallId)>,
}

#[blaze_query::tracked]
impl ImplItems {
    #[blaze_query::tracked(returns(ref))]
    pub fn of(db: &Db, id: ImplId) -> (ImplItems, DefDiagnostics) {
        let _p = tracing::info_span!("impl_items_with_diagnostics_query").entered();
        let ItemLoc { container: module_id, id: ast_id } = *id.lookup(db);

        let collector =
            AssocItemCollector::new(db, module_id, ItemContainerId::ImplId(id), ast_id.file_id);
        let source = collector.ast_id_map.get(ast_id.value).to_view(ast_id.file_id.root(db));
        let (items, macro_calls, diagnostics) = collector.collect(source.assoc_item_list());

        (ImplItems { items, macro_calls }, DefDiagnostics::new(diagnostics))
    }
}

impl ImplItems {
    pub fn macro_calls(&self) -> impl Iterator<Item = (AstId<ast::Item>, MacroCallId)> + '_ {
        self.macro_calls.iter().copied()
    }
}

struct AssocItemCollector<'db> {
    db: &'db Db,
    module_id: ModuleId,
    def_map: &'db DefMap,
    local_def_map: &'db LocalDefMap,
    ast_id_map: &'db AstIdMap,
    span_map: SpanMap<'db>,
    cfg_options: &'db CfgOptions,
    file_id: HirFileId,
    diagnostics: Vec<DefDiagnostic>,
    container: ItemContainerId,

    macro_depth: u32,
    items: Vec<(Name, AssocItemId)>,
    macro_calls: ThinVec<(AstId<ast::Item>, MacroCallId)>,
}

impl<'db> AssocItemCollector<'db> {
    fn new(
        db: &'db Db,
        module_id: ModuleId,
        container: ItemContainerId,
        file_id: HirFileId,
    ) -> Self {
        let (def_map, local_def_map) = module_id.local_def_map(db);
        Self {
            db,
            module_id,
            def_map,
            local_def_map,
            ast_id_map: file_id.ast_id_map(db),
            span_map: file_id.span_map(db),
            cfg_options: module_id.krate(db).cfg_options(db),
            file_id,
            container,
            // alloc: the query's result, once per trait or impl
            items: Vec::new(),

            macro_depth: file_id.macro_expansion_depth(db),
            macro_calls: ThinVec::new(),
            // alloc: the query's result, once per trait or impl
            diagnostics: Vec::new(),
        }
    }

    fn collect(
        mut self,
        item_list: Option<view::AssocItemList<'db>>,
    ) -> (Box<[(Name, AssocItemId)]>, ThinVec<(AstId<ast::Item>, MacroCallId)>, Vec<DefDiagnostic>)
    {
        if let Some(item_list) = item_list {
            for item in item_list.assoc_items() {
                self.collect_item(item);
            }
        }
        self.macro_calls.shrink_to_fit();
        (self.items.into_boxed_slice(), self.macro_calls, self.diagnostics)
    }

    fn collect_item(&mut self, item: view::AssocItem<'db>) {
        let ast_id = self.ast_id_map.ast_id_of_view::<ast::AssocItem>(item);
        let attrs = match AttrsOrCfg::lower(self.db, item, &|| self.cfg_options, self.span_map) {
            AttrsOrCfg::Enabled { attrs } => attrs,
            AttrsOrCfg::CfgDisabled(cfg) => {
                self.diagnostics.push(DefDiagnostic::unconfigured_code(
                    self.module_id,
                    InFile::new(self.file_id, ast_id.erase()),
                    cfg.0,
                ));
                return;
            }
        };
        let ast_id = InFile::new(self.file_id, ast_id.upcast());

        'attrs: for (attr_id, attr) in attrs.as_ref().iter() {
            let ast_id_with_path = AstIdWithPath { path: ModPath::clone(&attr.path), ast_id };

            match self.def_map.resolve_attr_macro(
                self.local_def_map,
                self.db,
                self.module_id,
                ast_id_with_path,
                attr,
                attr_id,
                self.macro_depth + 1,
            ) {
                Ok(ResolvedAttr::Macro(call_id)) => {
                    let loc = call_id.loc(self.db);
                    if let MacroDefKind::ProcMacro(_, exp, _) = loc.def.kind {
                        // If there's no expander for the proc macro (e.g. the
                        // proc macro is ignored, or building the proc macro
                        // crate failed), skip expansion like we would if it was
                        // disabled. This is analogous to the handling in
                        // `DefCollector::collect_macros`.
                        if let Some(err) = exp.as_expand_error(self.db, loc.def.krate) {
                            self.diagnostics.push(DefDiagnostic::macro_error(
                                self.module_id,
                                ast_id,
                                ModPath::clone(&attr.path),
                                err,
                            ));
                            continue 'attrs;
                        }
                    }

                    self.macro_calls.push((ast_id, call_id));
                    self.collect_macro_items(call_id);
                    return;
                }
                Ok(_) => (),
                Err(_) => {
                    self.diagnostics.push(DefDiagnostic::unresolved_macro_call(
                        self.module_id,
                        MacroCallKind::Attr {
                            ast_id,
                            attr_args: None,
                            censored_attr_ids: AttrMacroAttrIds::from_one(attr_id),
                        },
                        ModPath::clone(&attr.path),
                    ));
                }
            }
        }

        self.record_item(item);
    }

    fn record_item(&mut self, item: view::AssocItem<'db>) {
        match item {
            view::AssocItem::Fn(function) => {
                let Some(name) = function.name() else { return };
                let ast_id = self.ast_id_map.ast_id_of_view::<ast::Fn>(function);
                let def = FunctionLoc {
                    container: self.container,
                    id: InFile::new(self.file_id, ast_id),
                }
                .intern(self.db);
                self.items.push((name.as_name(), def.into()));
            }
            view::AssocItem::TypeAlias(type_alias) => {
                let Some(name) = type_alias.name() else { return };
                let ast_id = self.ast_id_map.ast_id_of_view::<ast::TypeAlias>(type_alias);
                let def = TypeAliasLoc {
                    container: self.container,
                    id: InFile::new(self.file_id, ast_id),
                }
                .intern(self.db);
                self.items.push((name.as_name(), def.into()));
            }
            view::AssocItem::Const(konst) => {
                let Some(name) = konst.name() else { return };
                let ast_id = self.ast_id_map.ast_id_of_view::<ast::Const>(konst);
                let def =
                    ConstLoc { container: self.container, id: InFile::new(self.file_id, ast_id) }
                        .intern(self.db);
                self.items.push((name.as_name(), def.into()));
            }
            view::AssocItem::MacroCall(call) => {
                let ast_id = self.ast_id_map.ast_id_of_view::<ast::MacroCall>(call);
                let ast_id = InFile::new(self.file_id, ast_id);
                let Some(path) = call.path() else { return };
                let range = path.syntax().text_range();
                let Some(path) = ModPath::from_src_view(self.db, path, &mut |range| {
                    self.span_map.span_for_range(range).ctx
                }) else {
                    return;
                };
                let ctxt = self.span_map.span_for_range(range).ctx;

                let resolver = |path: &ModPath| {
                    let before = path.as_ident().and_then(|name| {
                        let call = ast_id.erase();
                        self.def_map.legacy_macro_before(self.db, self.module_id, name, call)
                    });
                    if let Some(before) = before {
                        return Some(before.definition(self.db));
                    }
                    self.def_map
                        .resolve_path(
                            self.local_def_map,
                            self.db,
                            self.module_id,
                            path,
                            crate::item_scope::BuiltinShadowMode::Other,
                            Some(MacroSubNs::Bang),
                        )
                        .0
                        .take_macros()
                        .map(|it| it.definition(self.db))
                };
                // alloc: none unless the call is eager and its input holds macro calls
                let mut eager_calls = Vec::new();
                let call_id = macro_call_as_call_id(
                    self.db,
                    ast_id,
                    &path,
                    ctxt,
                    ExpandTo::Items,
                    self.module_id.krate(self.db),
                    self.macro_depth + 1,
                    self.def_map.recursion_limit(),
                    resolver,
                    &mut eager_calls,
                );
                self.macro_calls.extend(
                    eager_calls
                        .into_iter()
                        .map(|(ptr, call_id)| (ptr.map(|(_, it)| it.upcast()), call_id)),
                );
                match call_id {
                    // FIXME: Expansion error?
                    Ok(call_id) => match call_id.value {
                        Some(call_id) => {
                            self.macro_calls.push((ast_id.upcast(), call_id));
                            self.collect_macro_items(call_id);
                        }
                        None => (),
                    },
                    Err(_) => {
                        self.diagnostics.push(DefDiagnostic::unresolved_macro_call(
                            self.module_id,
                            MacroCallKind::FnLike {
                                ast_id,
                                expand_to: ExpandTo::Items,
                                eager: None,
                            },
                            path,
                        ));
                    }
                }
            }
        }
    }

    fn collect_macro_items(&mut self, macro_call_id: MacroCallId) {
        if self.macro_depth > self.def_map.recursion_limit() {
            let diag = DefDiagnostic::recursion_overflow(self.db, self.module_id, macro_call_id);
            self.diagnostics.push(diag);
            return;
        }

        let (syntax, span_map) = &macro_call_id.parse_macro_expansion(self.db).value;
        let old_file_id = mem::replace(&mut self.file_id, macro_call_id.into());
        let old_ast_id_map = mem::replace(&mut self.ast_id_map, self.file_id.ast_id_map(self.db));
        let old_span_map = mem::replace(&mut self.span_map, SpanMap::ExpansionSpanMap(span_map));
        self.macro_depth += 1;

        let items = view::MacroItems::cast(syntax.root()).expect("not `MacroItems`");
        for item in items.items() {
            let item = match item {
                view::Item::Fn(it) => view::AssocItem::from(it),
                view::Item::Const(it) => it.into(),
                view::Item::TypeAlias(it) => it.into(),
                view::Item::MacroCall(it) => it.into(),
                // FIXME: Should error on disallowed item kinds.
                _ => continue,
            };
            self.collect_item(item);
        }

        self.macro_depth -= 1;
        self.file_id = old_file_id;
        self.ast_id_map = old_ast_id_map;
        self.span_map = old_span_map;
    }
}
