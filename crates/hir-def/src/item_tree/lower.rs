//! AST -> `ItemTree` lowering code.

use std::{
    cell::{Cell, OnceCell},
    hash::Hash,
    mem,
};

use base_db::{Db, Crate, FxIndexSet};
use cfg::{CfgExpr, CfgOptions};
use hir_expand::{
    HirFileId,
    attrs::Attr,
    mod_path::PathKind,
    name::{AsName, Name},
    span_map::SpanMap,
};
use la_arena::Arena;
use span::{AstIdMap, FileAstId, RealSpanMap, SyntaxContext};
use syntax::{
    ast as tag,
    view::{self as ast, AstNode, HasModuleItem, HasName, HasVisibility, StructKind},
};

use crate::item_tree::{
    AliasId, AttrOwner, Const, Enum, ExternBlock, ExternCrate, Function, IdRange, Impl,
    ImportAlias, ItemKind, ItemTree, ItemTreeLens, Macro2, MacroCall, MacroRules, Mod, ModItemId,
    ModPath, NameId, PathId, RawVisibility, RawVisibilityId, Static, Struct, Trait, TypeAlias,
    Union, Use, UseTree, UseTreeId, VisibilityExplicitness, attrs::lower_attrs_into, items::Marks,
};

/// The index of the next element of an array `len` long, as a tree's `u32` ids count. The top
/// few `u32`s are the ids' sentinels ([`NameId::NONE`], [`ModItemId::TOP_OWNER`], ...).
fn id(len: usize) -> u32 {
    match u32::try_from(len) {
        Ok(id) if id < u32::MAX - 4 => id,
        _ => panic!("an item tree with {len} elements in one array: more than `u32` ids name"),
    }
}

fn push<T>(column: &mut Vec<T>, value: T) -> u32 {
    let at = id(column.len());
    column.push(value);
    at
}

thread_local! {
    /// The worker's builder, empty between trees and keeping its columns' capacity, so a
    /// worker grows no column again once it has lowered a tree as big. A tree lowered while
    /// another is (none is today) takes a fresh one.
    static SCRATCH: Cell<TreeBuilder> = Cell::new(TreeBuilder::default());
}

/// The most items a kept [`SCRATCH`] builder has room for: about 100 KiB of columns.
const SCRATCH_ITEMS: usize = 1 << 13;

/// An [`ItemTree`] being built: its columns, growing.
#[derive(Default)]
struct TreeBuilder {
    /// The items of the lists being lowered, innermost last: a list is lowered onto the end
    /// and moved to `children` when it is done ([`TreeBuilder::items_since`]); what is left at
    /// the end is the top level.
    pending_items: Vec<ModItemId>,
    /// The same for the lists of `use` trees.
    pending_use_trees: Vec<UseTreeId>,
    /// The visibilities a tuple struct's constructor is the least of, while its fields are read.
    ctor_scratch: Vec<RawVisibility>,

    kinds: Vec<ItemKind>,
    data: Vec<u32>,
    ast_ids: Vec<FileAstId<tag::Item>>,
    children: Vec<ModItemId>,

    consts: Vec<Const>,
    enums: Vec<Enum>,
    extern_blocks: Vec<ExternBlock>,
    extern_crates: Vec<ExternCrate>,
    functions: Vec<Function>,
    impls: Vec<Impl>,
    macro2s: Vec<Macro2>,
    macro_calls: Vec<MacroCall>,
    macro_rules: Vec<MacroRules>,
    mods: Vec<Mod>,
    statics: Vec<Static>,
    structs: Vec<Struct>,
    traits: Vec<Trait>,
    type_aliases: Vec<TypeAlias>,
    unions: Vec<Union>,
    uses: Vec<Use>,

    use_trees: Vec<UseTree>,
    use_tree_lists: Vec<UseTreeId>,
    ctor_vis: Vec<RawVisibilityId>,

    names: Scoped<Name>,
    paths: Scoped<ModPath>,
    vis: Scoped<RawVisibility>,

    attr_owners: Vec<AttrOwner>,
    /// The top level's owner, pushed after every item's at `finish`.
    top_attrs: Option<AttrOwner>,
    attrs: Vec<Attr>,
    cfgs: Vec<CfgExpr>,
}

/// A table of a tree whose values are deduplicated within each top-level item only, so the
/// tree of a file is its items' trees one after the other, each one's ids moved by where it
/// starts ([`super::items::assemble`]), and the same whether it is lowered whole or per item.
struct Scoped<T> {
    /// The values of the items sealed.
    done: Vec<T>,
    /// The values of the item being lowered, after `done`'s.
    scope: FxIndexSet<T>,
}

impl<T> Default for Scoped<T> {
    fn default() -> Self {
        // alloc: none, both empty; the worker's builder keeps their capacity
        Scoped { done: Vec::new(), scope: FxIndexSet::default() }
    }
}

impl<T: Hash + Eq> Scoped<T> {
    fn id(&mut self, value: T) -> u32 {
        let (at, _) = self.scope.insert_full(value);
        id(self.done.len() + at)
    }

    /// Ends the item being lowered: no later value is deduplicated against its.
    fn seal(&mut self) {
        match self.scope.len() {
            0 => {}
            // Draining clears the table, which writes every control byte of its capacity: after
            // one item of many names, far more than taking a few entries out one by one.
            len if len * 16 < self.scope.capacity() => {
                let from = self.done.len();
                while let Some(value) = self.scope.pop() {
                    self.done.push(value);
                }
                self.done[from..].reverse();
            }
            _ => self.done.extend(self.scope.drain(..)),
        }
    }

    /// Empties the table, keeping its capacity.
    fn clear(&mut self) {
        self.done.clear();
        self.scope.clear();
    }
}

impl<T> Scoped<T> {
    fn len(&self) -> u32 {
        id(self.done.len() + self.scope.len())
    }
}

impl TreeBuilder {
    /// Where the next rows of each column go: at a top-level item's start, where its rows
    /// begin in the tree; after the last, where they end.
    fn marks(&self) -> Marks {
        Marks {
            top_level: id(self.pending_items.len()),
            kinds: id(self.kinds.len()),
            children: id(self.children.len()),
            consts: id(self.consts.len()),
            enums: id(self.enums.len()),
            extern_blocks: id(self.extern_blocks.len()),
            extern_crates: id(self.extern_crates.len()),
            functions: id(self.functions.len()),
            impls: id(self.impls.len()),
            macro2s: id(self.macro2s.len()),
            macro_calls: id(self.macro_calls.len()),
            macro_rules: id(self.macro_rules.len()),
            mods: id(self.mods.len()),
            statics: id(self.statics.len()),
            structs: id(self.structs.len()),
            traits: id(self.traits.len()),
            type_aliases: id(self.type_aliases.len()),
            unions: id(self.unions.len()),
            uses: id(self.uses.len()),
            use_trees: id(self.use_trees.len()),
            use_tree_lists: id(self.use_tree_lists.len()),
            ctor_vis: id(self.ctor_vis.len()),
            names: self.names.len(),
            paths: self.paths.len(),
            vis: self.vis.len(),
            attr_owners: id(self.attr_owners.len()),
            attr_list: id(self.attrs.len()),
            cfgs: id(self.cfgs.len()),
        }
    }

    /// Ends the top-level item being lowered ([`Scoped`]).
    fn seal(&mut self) {
        self.names.seal();
        self.paths.seal();
        self.vis.seal();
    }

    /// The tree built, the pending items its top level, in one allocation (none for a tree with
    /// no values); leaves the builder empty, its capacity kept.
    fn finish(&mut self) -> ItemTree {
        debug_assert!(self.pending_use_trees.is_empty() && self.ctor_scratch.is_empty());
        self.attr_owners.extend(self.top_attrs.take());
        self.seal();
        let lens = ItemTreeLens {
            top_level: self.pending_items.len(),
            kinds: self.kinds.len(),
            data: self.data.len(),
            ast_ids: self.ast_ids.len(),
            children: self.children.len(),
            consts: self.consts.len(),
            enums: self.enums.len(),
            extern_blocks: self.extern_blocks.len(),
            extern_crates: self.extern_crates.len(),
            functions: self.functions.len(),
            impls: self.impls.len(),
            macro2s: self.macro2s.len(),
            macro_calls: self.macro_calls.len(),
            macro_rules: self.macro_rules.len(),
            mods: self.mods.len(),
            statics: self.statics.len(),
            structs: self.structs.len(),
            traits: self.traits.len(),
            type_aliases: self.type_aliases.len(),
            unions: self.unions.len(),
            uses: self.uses.len(),
            use_trees: self.use_trees.len(),
            use_tree_lists: self.use_tree_lists.len(),
            ctor_vis: self.ctor_vis.len(),
            names: self.names.done.len(),
            paths: self.paths.done.len(),
            vis: self.vis.done.len(),
            attr_owners: self.attr_owners.len(),
            attr_index: self.kinds.len().div_ceil(64),
            attr_list: self.attrs.len(),
            cfgs: self.cfgs.len(),
        };
        ItemTree::build(lens, |s| {
            s.attr_index.extend(super::attr_blocks(&self.attr_owners, self.kinds.len()));
            s.top_level.move_from(&mut self.pending_items);
            s.kinds.move_from(&mut self.kinds);
            s.data.move_from(&mut self.data);
            s.ast_ids.move_from(&mut self.ast_ids);
            s.children.move_from(&mut self.children);
            s.consts.move_from(&mut self.consts);
            s.enums.move_from(&mut self.enums);
            s.extern_blocks.move_from(&mut self.extern_blocks);
            s.extern_crates.move_from(&mut self.extern_crates);
            s.functions.move_from(&mut self.functions);
            s.impls.move_from(&mut self.impls);
            s.macro2s.move_from(&mut self.macro2s);
            s.macro_calls.move_from(&mut self.macro_calls);
            s.macro_rules.move_from(&mut self.macro_rules);
            s.mods.move_from(&mut self.mods);
            s.statics.move_from(&mut self.statics);
            s.structs.move_from(&mut self.structs);
            s.traits.move_from(&mut self.traits);
            s.type_aliases.move_from(&mut self.type_aliases);
            s.unions.move_from(&mut self.unions);
            s.uses.move_from(&mut self.uses);
            s.use_trees.move_from(&mut self.use_trees);
            s.use_tree_lists.move_from(&mut self.use_tree_lists);
            s.ctor_vis.move_from(&mut self.ctor_vis);
            s.names.move_from(&mut self.names.done);
            s.paths.move_from(&mut self.paths.done);
            s.vis.move_from(&mut self.vis.done);
            s.attr_owners.move_from(&mut self.attr_owners);
            s.attr_list.move_from(&mut self.attrs);
            s.cfgs.move_from(&mut self.cfgs);
        })
    }

    /// Empties what lowering a `use` tree alone fills ([`lower_use_tree`]), keeping its
    /// capacity: the rest of a builder between trees is empty already.
    fn clear_use_trees(&mut self) {
        self.pending_use_trees.clear();
        self.use_trees.clear();
        self.use_tree_lists.clear();
        self.names.clear();
        self.paths.clear();
    }

    /// A new item of `kind`, its payload at `data` of its column.
    fn item(&mut self, kind: ItemKind, ast_id: FileAstId<tag::Item>, data: u32) -> ModItemId {
        let index = id(self.kinds.len());
        self.kinds.push(kind);
        self.data.push(data);
        self.ast_ids.push(ast_id);
        ModItemId::new(index)
    }

    fn name(&mut self, name: Name) -> NameId {
        NameId { raw: self.names.id(name) }
    }

    fn path(&mut self, path: ModPath) -> PathId {
        PathId { raw: self.paths.id(path) }
    }

    fn alias(&mut self, alias: Option<ImportAlias>) -> AliasId {
        match alias {
            None => AliasId::NONE,
            Some(ImportAlias::Underscore) => AliasId::UNDERSCORE,
            Some(ImportAlias::Alias(name)) => AliasId { raw: self.name(name).raw },
        }
    }

    fn visibility(&mut self, vis: RawVisibility) -> RawVisibilityId {
        match &vis {
            RawVisibility::Public => RawVisibilityId::PUB,
            RawVisibility::PubSelf(VisibilityExplicitness::Explicit) => {
                RawVisibilityId::PRIV_EXPLICIT
            }
            RawVisibility::PubSelf(VisibilityExplicitness::Implicit) => {
                RawVisibilityId::PRIV_IMPLICIT
            }
            RawVisibility::PubCrate => RawVisibilityId::PUB_CRATE,
            _ => RawVisibilityId { raw: self.vis.id(vis) },
        }
    }

    /// The list of the items pending from `mark`, moved to `children`.
    fn items_since(&mut self, mark: usize) -> IdRange {
        let start = id(self.children.len());
        let len = id(self.pending_items.len() - mark);
        self.children.extend(self.pending_items.drain(mark..));
        IdRange { start, len }
    }

    fn use_tree(&mut self, tree: UseTree) -> UseTreeId {
        UseTreeId { raw: push(&mut self.use_trees, tree) }
    }

    /// The list of the `use` trees pending from `mark`, moved to `use_tree_lists`.
    fn use_tree_list_since(&mut self, mark: usize) -> IdRange {
        let start = id(self.use_tree_lists.len());
        let len = id(self.pending_use_trees.len() - mark);
        self.use_tree_lists.extend(self.pending_use_trees.drain(mark..));
        IdRange { start, len }
    }

    fn ctor_vis(&mut self, vis: impl IntoIterator<Item = RawVisibility>) -> IdRange {
        let start = self.ctor_vis.len();
        for vis in vis {
            let vis = self.visibility(vis);
            self.ctor_vis.push(vis);
        }
        IdRange { start: id(start), len: id(self.ctor_vis.len() - start) }
    }

    /// Gives `owner` the attributes from `start` of `attrs`, disabled by `cfg` if one is
    /// given. An owner with no attributes and no `cfg` is left out.
    fn add_attrs(&mut self, owner: ModItemId, start: usize, cfg: Option<CfgExpr>) {
        let len = self.attrs.len() - start;
        if len == 0 && cfg.is_none() {
            return;
        }
        let cfg = match cfg {
            Some(cfg) => push(&mut self.cfgs, cfg),
            None => AttrOwner::ENABLED,
        };
        let attrs = IdRange { start: id(start), len: id(len) };
        let row = AttrOwner { owner, attrs, cfg };
        if owner == ModItemId::TOP_OWNER {
            debug_assert!(self.top_attrs.is_none(), "the top level's attributes lowered twice");
            self.top_attrs = Some(row);
            return;
        }
        // `ItemTree::attrs` relies on the order; each item's attributes are lowered right
        // after its id is made, so this holds without a sort.
        assert!(
            self.attr_owners.last().is_none_or(|last| last.owner < owner),
            "attribute owners out of the order of their items"
        );
        self.attr_owners.push(row);
    }
}

pub(super) struct Ctx<'db> {
    pub(super) db: &'db Db,
    tree: TreeBuilder,
    ids: &'db AstIdMap,
    /// The file's span map, read when first wanted.
    span_map: OnceCell<SpanMap<'db>>,
    file: HirFileId,
    cfg_options: OnceCell<&'db CfgOptions>,
    krate: Crate,
}

impl<'db> Ctx<'db> {
    pub(super) fn new(db: &'db Db, file: HirFileId, krate: Crate) -> Self {
        Self {
            db,
            tree: SCRATCH.take(),
            ids: file.ast_id_map(db),
            file,
            cfg_options: OnceCell::new(),
            span_map: OnceCell::new(),
            krate,
        }
    }

    /// [`Ctx::new`] with the maps of a part of `file` parsed alone (a body's
    /// [`crate::expr_store::BodySyntax`]), whose ids and spans are the file's: what is lowered
    /// reads that part and not the whole file.
    pub(super) fn with_maps(
        db: &'db Db,
        file: HirFileId,
        krate: Crate,
        ids: &'db AstIdMap,
        span_map: &'db RealSpanMap,
    ) -> Self {
        Self {
            db,
            tree: SCRATCH.take(),
            ids,
            file,
            cfg_options: OnceCell::new(),
            span_map: OnceCell::from(SpanMap::RealSpanMap(span_map)),
            krate,
        }
    }

    pub(super) fn span_map(&self) -> SpanMap<'db> {
        *self.span_map.get_or_init(|| self.file.span_map(self.db))
    }

    /// Lowers `node`'s attributes into the tree's, and returns where they start and the `cfg`
    /// that disables the node, if one does. One instance for every kind of node.
    fn lower_attrs_of(&mut self, node: ast::AnyHasAttrs<'_>) -> (usize, Option<CfgExpr>) {
        let span_map = self.span_map();
        let (db, krate, cfg_options) = (self.db, self.krate, &self.cfg_options);
        let start = self.tree.attrs.len();
        let cfg = lower_attrs_into(
            db,
            node,
            &|| *cfg_options.get_or_init(|| krate.cfg_options(db)),
            span_map,
            &mut self.tree.attrs,
        );
        (start, cfg)
    }

    #[inline]
    fn lower_attrs<'t>(&mut self, owner: ModItemId, node: impl ast::HasAttrs<'t>) {
        let (start, cfg) = self.lower_attrs_of(ast::AnyHasAttrs::new(node));
        self.tree.add_attrs(owner, start, cfg);
    }

    /// Whether a `cfg` among `node`'s attributes is false; the attributes are not kept.
    #[inline]
    fn cfg_disabled<'t>(&mut self, node: impl ast::HasAttrs<'t>) -> bool {
        let (start, cfg) = self.lower_attrs_of(ast::AnyHasAttrs::new(node));
        self.tree.attrs.truncate(start);
        cfg.is_some()
    }

    /// Lowers the source file's inner attributes as the top level's, then those of
    /// `crate_attrs`, the files of the crate attributes rustc appends to them (for the crate
    /// whose root `file` is, [`base_db::applied_crate_attr_files`]), each with its own file's
    /// spans. A false `cfg` stops there: the attributes after it are not lowered.
    pub(super) fn lower_top_attrs(
        &mut self,
        file: ast::SourceFile<'_>,
        crate_attrs: &[base_db::EditionedFileId],
    ) {
        let start = self.tree.attrs.len();
        let mut cfg = self.lower_attrs_of(ast::AnyHasAttrs::new(file)).1;
        let (db, krate, cfg_options) = (self.db, self.krate, &self.cfg_options);
        for &attr_file in crate_attrs {
            if cfg.is_some() {
                break;
            }
            cfg = lower_attrs_into(
                db,
                ast::AnyHasAttrs::new(attr_file.parse(db).view()),
                &|| *cfg_options.get_or_init(|| krate.cfg_options(db)),
                HirFileId::from(attr_file).span_map(db),
                &mut self.tree.attrs,
            );
        }
        self.tree.add_attrs(ModItemId::TOP_OWNER, start, cfg);
    }

    /// The tree, its top level the items pending; the builder goes back to [`SCRATCH`] unless
    /// it grew past [`SCRATCH_ITEMS`] (a generated file), so a worker keeps a bounded amount.
    fn finish(mut self) -> ItemTree {
        let tree = self.tree.finish();
        if self.tree.kinds.capacity() <= SCRATCH_ITEMS {
            SCRATCH.set(mem::take(&mut self.tree));
        }
        tree
    }

    /// Lowers `item` onto the pending list being lowered, if it is an item.
    fn pend(&mut self, item: ast::Item<'_>) {
        if let Some(id) = self.lower_mod_item(item) {
            self.tree.pending_items.push(id);
        }
    }

    /// Lowers `item` as a top-level item: [`Self::pend`], then sealed ([`Scoped`]).
    fn pend_top(&mut self, item: ast::Item<'_>) {
        self.pend(item);
        self.tree.seal();
    }

    pub(super) fn lower_module_items<'t>(
        mut self,
        item_owner: impl HasModuleItem<'t>,
    ) -> ItemTree {
        for item in item_owner.items() {
            self.pend_top(item);
        }
        self.finish()
    }

    /// [`Self::lower_module_items`], pushing onto `marks` where each top-level item's rows
    /// begin, then where the last one's end: what [`super::items`] reuses rows of.
    pub(super) fn lower_module_items_marked<'t>(
        mut self,
        item_owner: impl HasModuleItem<'t>,
        marks: &mut Vec<Marks>,
    ) -> ItemTree {
        for item in item_owner.items() {
            marks.push(self.tree.marks());
            self.pend_top(item);
        }
        marks.push(self.tree.marks());
        self.finish()
    }

    /// The tree of one top-level item of a file: the file's tree is the trees of its top
    /// level's attributes ([`Self::lower_top`]) and of its items, assembled.
    pub(super) fn lower_item(mut self, item: ast::Item<'_>) -> ItemTree {
        self.pend_top(item);
        self.finish()
    }

    /// The tree of a source file's top level's attributes alone ([`Self::lower_top_attrs`]).
    pub(super) fn lower_top(
        mut self,
        file: ast::SourceFile<'_>,
        crate_attrs: &[base_db::EditionedFileId],
    ) -> ItemTree {
        self.lower_top_attrs(file, crate_attrs);
        self.finish()
    }

    pub(super) fn lower_macro_stmts(mut self, stmts: ast::MacroStmts<'_>) -> ItemTree {
        for stmt in stmts.statements() {
            let item = match stmt {
                ast::Stmt::Item(item) => item,
                // Macro calls can be both items and expressions. The syntax library always treats
                // them as expressions here, so we undo that.
                ast::Stmt::ExprStmt(es) => match es.expr() {
                    Some(ast::Expr::MacroExpr(expr)) => {
                        cov_mark::hit!(macro_call_in_macro_stmts_is_added_to_item_tree);
                        let Some(call) = expr.macro_call() else { continue };
                        call.into()
                    }
                    _ => continue,
                },
                _ => continue,
            };
            self.pend_top(item);
        }

        if let Some(ast::Expr::MacroExpr(tail_macro)) = stmts.expr()
            && let Some(call) = tail_macro.macro_call()
        {
            cov_mark::hit!(macro_stmt_with_trailing_macro_expr);
            self.pend_top(call.into());
        }

        self.finish()
    }

    pub(super) fn lower_block(mut self, block: ast::BlockExpr<'_>) -> ItemTree {
        self.lower_attrs(ModItemId::TOP_OWNER, block);
        for stmt in block.statements() {
            let id = match stmt {
                ast::Stmt::Item(item) => self.lower_mod_item(item),
                // Macro calls can be both items and expressions. The syntax library always treats
                // them as expressions here, so we undo that.
                ast::Stmt::ExprStmt(es) => match es.expr() {
                    Some(ast::Expr::MacroExpr(expr)) => self.lower_macro_stmt(expr),
                    _ => None,
                },
                _ => None,
            };
            if let Some(id) = id {
                self.tree.pending_items.push(id);
            }
            self.tree.seal();
        }
        if let Some(ast::Expr::MacroExpr(expr)) = block.tail_expr()
            && let Some(mod_item) = self.lower_macro_stmt(expr)
        {
            self.tree.pending_items.push(mod_item);
        }
        self.finish()
    }

    /// A macro call written as a statement is an item of the block when it expands to items.
    /// Its attributes are on the expression: one that is configured out is no item at all.
    fn lower_macro_stmt(&mut self, expr: ast::MacroExpr<'_>) -> Option<ModItemId> {
        let call = expr.macro_call()?;
        // Only an `Expr` has the attributes.
        if self.cfg_disabled(ast::Expr::MacroExpr(expr)) {
            return None;
        }
        self.lower_mod_item(call.into())
    }

    fn lower_mod_item(&mut self, item: ast::Item<'_>) -> Option<ModItemId> {
        let mod_item = match item {
            ast::Item::Struct(ast) => self.lower_struct(ast)?,
            ast::Item::Union(ast) => self.lower_union(ast)?,
            ast::Item::Enum(ast) => self.lower_enum(ast)?,
            ast::Item::Fn(ast) => self.lower_function(ast)?,
            ast::Item::TypeAlias(ast) => self.lower_type_alias(ast)?,
            ast::Item::Static(ast) => self.lower_static(ast)?,
            ast::Item::Const(ast) => self.lower_const(ast),
            ast::Item::Module(ast) => self.lower_module(ast)?,
            ast::Item::Trait(ast) => self.lower_trait(ast)?,
            ast::Item::Impl(ast) => self.lower_impl(ast),
            ast::Item::Use(ast) => self.lower_use(ast)?,
            ast::Item::ExternCrate(ast) => self.lower_extern_crate(ast)?,
            ast::Item::MacroCall(ast) => self.lower_macro_call(ast)?,
            ast::Item::MacroRules(ast) => self.lower_macro_rules(ast)?,
            ast::Item::MacroDef(ast) => self.lower_macro_def(ast)?,
            ast::Item::ExternBlock(ast) => self.lower_extern_block(ast),
            // FIXME: Handle `global_asm!()`.
            ast::Item::AsmExpr(_) => return None,
        };
        self.lower_attrs(mod_item, item);

        Some(mod_item)
    }

    fn lower_struct(&mut self, strukt: ast::Struct<'_>) -> Option<ModItemId> {
        let visibility = self.lower_visibility_from_ast(strukt);
        // clone: allocates nothing, a `ModPath` is an interned slice
        let visibility_id = self.tree.visibility(visibility.clone());
        let name = strukt.name()?.as_name();
        let ast_id = self.ids.ast_id_of_view::<tag::Struct>(strukt);
        let value_ns_ctor = match strukt.kind() {
            StructKind::Record(_) => IdRange::EMPTY,
            StructKind::Unit => self.tree.ctor_vis([visibility]),
            StructKind::Tuple(fields) => self.tuple_struct_ctor_vis(visibility, fields),
        };
        let res = Struct { name: self.tree.name(name), visibility: visibility_id, value_ns_ctor };
        let data = push(&mut self.tree.structs, res);
        Some(self.tree.item(ItemKind::Struct, ast_id.upcast(), data))
    }

    /// The visibilities a tuple struct's constructor is the least of, in `ctor_vis`.
    fn tuple_struct_ctor_vis(
        &mut self,
        struct_vis: RawVisibility,
        fields: ast::TupleFieldList<'_>,
    ) -> IdRange {
        if let RawVisibility::PubSelf(_) = struct_vis {
            // `pub(self)` <= anything.
            return self.tree.ctor_vis([struct_vis]);
        }

        // The least visibility so far: one, or several distinct `pub(in ..)`s (whose paths are
        // not ordered before name resolution), in the order of their fields.
        let mut least = mem::take(&mut self.tree.ctor_scratch);
        least.push(struct_vis);
        for field in fields.fields() {
            let field_vis = self.lower_visibility_from_ast(field);
            match (&field_vis, &least[0]) {
                (RawVisibility::PubSelf(_), _) => {
                    // If there is a field private, the minimal visibility is private.
                    least.clear();
                    least.push(field_vis);
                    break;
                }
                (_, RawVisibility::Public) => {
                    // anything <= `pub`.
                    least[0] = field_vis;
                }
                (RawVisibility::Module(..), RawVisibility::PubCrate) => {
                    // `pub(in module)` <= `pub(crate)`.
                    least[0] = field_vis;
                }
                (
                    RawVisibility::Public | RawVisibility::PubCrate,
                    RawVisibility::Module(..) | RawVisibility::PubCrate,
                ) => {
                    // `pub(in module) | pub(crate)` <= `pub(crate) | pub`.
                }
                (_, RawVisibility::PubSelf(_)) => {
                    unreachable!("we early-exit on `PubSelf`");
                }
                (RawVisibility::Module(..), RawVisibility::Module(..)) => {
                    if !least.contains(&field_vis) {
                        least.push(field_vis);
                    }
                }
            }
        }

        // A list of one is that visibility.
        let range = self.tree.ctor_vis(least.drain(..));
        self.tree.ctor_scratch = least;
        range
    }

    fn lower_union(&mut self, union: ast::Union<'_>) -> Option<ModItemId> {
        let visibility = self.lower_visibility(union);
        let name = union.name()?.as_name();
        let ast_id = self.ids.ast_id_of_view::<tag::Union>(union);
        let res = Union { name: self.tree.name(name), visibility };
        let data = push(&mut self.tree.unions, res);
        Some(self.tree.item(ItemKind::Union, ast_id.upcast(), data))
    }

    fn lower_enum(&mut self, enum_: ast::Enum<'_>) -> Option<ModItemId> {
        let visibility = self.lower_visibility(enum_);
        let name = enum_.name()?.as_name();
        let ast_id = self.ids.ast_id_of_view::<tag::Enum>(enum_);
        let res = Enum { name: self.tree.name(name), visibility };
        let data = push(&mut self.tree.enums, res);
        Some(self.tree.item(ItemKind::Enum, ast_id.upcast(), data))
    }

    fn lower_function(&mut self, func: ast::Fn<'_>) -> Option<ModItemId> {
        let visibility = self.lower_visibility(func);
        let name = func.name()?.as_name();

        let ast_id = self.ids.ast_id_of_view::<tag::Fn>(func);

        let res = Function { name: self.tree.name(name), visibility };
        let data = push(&mut self.tree.functions, res);
        Some(self.tree.item(ItemKind::Function, ast_id.upcast(), data))
    }

    fn lower_type_alias(&mut self, type_alias: ast::TypeAlias<'_>) -> Option<ModItemId> {
        let name = type_alias.name()?.as_name();
        let visibility = self.lower_visibility(type_alias);
        let ast_id = self.ids.ast_id_of_view::<tag::TypeAlias>(type_alias);
        let res = TypeAlias { name: self.tree.name(name), visibility };
        let data = push(&mut self.tree.type_aliases, res);
        Some(self.tree.item(ItemKind::TypeAlias, ast_id.upcast(), data))
    }

    fn lower_static(&mut self, static_: ast::Static<'_>) -> Option<ModItemId> {
        let name = static_.name()?.as_name();
        let visibility = self.lower_visibility(static_);
        let ast_id = self.ids.ast_id_of_view::<tag::Static>(static_);
        let res = Static { name: self.tree.name(name), visibility };
        let data = push(&mut self.tree.statics, res);
        Some(self.tree.item(ItemKind::Static, ast_id.upcast(), data))
    }

    fn lower_const(&mut self, konst: ast::Const<'_>) -> ModItemId {
        let name = match konst.name() {
            Some(name) => self.tree.name(name.as_name()),
            None => NameId::NONE,
        };
        let visibility = self.lower_visibility(konst);
        let ast_id = self.ids.ast_id_of_view::<tag::Const>(konst);
        let data = push(&mut self.tree.consts, Const { name, visibility });
        self.tree.item(ItemKind::Const, ast_id.upcast(), data)
    }

    fn lower_module(&mut self, module: ast::Module<'_>) -> Option<ModItemId> {
        let name = module.name()?.as_name();
        let visibility = self.lower_visibility(module);
        let items = if module.semicolon_token().is_some() {
            IdRange::NONE
        } else {
            let mark = self.tree.pending_items.len();
            match module.item_list() {
                Some(list) => {
                    for item in list.items() {
                        self.pend(item);
                    }
                }
                None => {
                    cov_mark::hit!(name_res_works_for_broken_modules);
                }
            }
            self.tree.items_since(mark)
        };
        let ast_id = self.ids.ast_id_of_view::<tag::Module>(module);
        let res = Mod { name: self.tree.name(name), visibility, items };
        let data = push(&mut self.tree.mods, res);
        Some(self.tree.item(ItemKind::Mod, ast_id.upcast(), data))
    }

    fn lower_trait(&mut self, trait_def: ast::Trait<'_>) -> Option<ModItemId> {
        let name = trait_def.name()?.as_name();
        let visibility = self.lower_visibility(trait_def);
        let ast_id = self.ids.ast_id_of_view::<tag::Trait>(trait_def);

        let def = Trait { name: self.tree.name(name), visibility };
        let data = push(&mut self.tree.traits, def);
        Some(self.tree.item(ItemKind::Trait, ast_id.upcast(), data))
    }

    fn lower_impl(&mut self, impl_def: ast::Impl<'_>) -> ModItemId {
        let ast_id = self.ids.ast_id_of_view::<tag::Impl>(impl_def);
        // Note that trait impls don't get implicit `Self` unlike traits, because here they are a
        // type alias rather than a type parameter, so this is handled by the resolver.
        let res = Impl { is_trait_impl: impl_def.trait_().is_some() };
        let data = push(&mut self.tree.impls, res);
        self.tree.item(ItemKind::Impl, ast_id.upcast(), data)
    }

    fn lower_use(&mut self, use_item: ast::Use<'_>) -> Option<ModItemId> {
        let visibility = self.lower_visibility(use_item);
        let ast_id = self.ids.ast_id_of_view::<tag::Use>(use_item);
        let span_map = self.span_map();
        let mut lowering = UseTreeLowering { db: self.db, mapping: None, tree: &mut self.tree };
        let use_tree = lowering.lower_use_tree(use_item.use_tree()?, &mut |range| {
            span_map.span_for_range(range).ctx
        })?;

        let data = push(&mut self.tree.uses, Use { visibility, use_tree });
        Some(self.tree.item(ItemKind::Use, ast_id.upcast(), data))
    }

    fn lower_extern_crate(&mut self, extern_crate: ast::ExternCrate<'_>) -> Option<ModItemId> {
        let name = extern_crate.name_ref()?.as_name();
        let alias = extern_crate.rename().map(|a| {
            a.name().map(|it| it.as_name()).map_or(ImportAlias::Underscore, ImportAlias::Alias)
        });
        let visibility = self.lower_visibility(extern_crate);
        let ast_id = self.ids.ast_id_of_view::<tag::ExternCrate>(extern_crate);

        let res =
            ExternCrate { name: self.tree.name(name), alias: self.tree.alias(alias), visibility };
        let data = push(&mut self.tree.extern_crates, res);
        Some(self.tree.item(ItemKind::ExternCrate, ast_id.upcast(), data))
    }

    fn lower_macro_call(&mut self, m: ast::MacroCall<'_>) -> Option<ModItemId> {
        let span_map = self.span_map();
        let path = m.path()?;
        let range = path.syntax().text_range();
        let path = ModPath::from_src_view(self.db, path, &mut |range| {
            span_map.span_for_range(range).ctx
        })?;
        let expand_to = match hir_expand::ExpandTo::of_call_site_view(m) {
            // A block's last call not in braces is an expression (rustc's `parse_stmt_mac`),
            // which defines no items: the body's lowering expands it.
            Ok(hir_expand::ExpandTo::Expr) => return None,
            Ok(it) => it,
            Err(parent) => {
                unreachable!("an item tree lowers the calls of item lists and statements, not of a `{parent:?}`")
            }
        };
        let ast_id = self.ids.ast_id_of_view::<tag::MacroCall>(m);
        let res = MacroCall {
            path: self.tree.path(path),
            expand_to,
            ctxt: span_map.span_for_range(range).ctx,
        };
        let data = push(&mut self.tree.macro_calls, res);
        Some(self.tree.item(ItemKind::MacroCall, ast_id.upcast(), data))
    }

    fn lower_macro_rules(&mut self, m: ast::MacroRules<'_>) -> Option<ModItemId> {
        let name = m.name()?;
        let ast_id = self.ids.ast_id_of_view::<tag::MacroRules>(m);

        let res = MacroRules { name: self.tree.name(name.as_name()) };
        let data = push(&mut self.tree.macro_rules, res);
        Some(self.tree.item(ItemKind::MacroRules, ast_id.upcast(), data))
    }

    fn lower_macro_def(&mut self, m: ast::MacroDef<'_>) -> Option<ModItemId> {
        let name = m.name()?;

        let ast_id = self.ids.ast_id_of_view::<tag::MacroDef>(m);
        let visibility = self.lower_visibility(m);

        let res = Macro2 { name: self.tree.name(name.as_name()), visibility };
        let data = push(&mut self.tree.macro2s, res);
        Some(self.tree.item(ItemKind::Macro2, ast_id.upcast(), data))
    }

    fn lower_extern_block(&mut self, block: ast::ExternBlock<'_>) -> ModItemId {
        let ast_id = self.ids.ast_id_of_view::<tag::ExternBlock>(block);
        let mark = self.tree.pending_items.len();
        for item in block.extern_item_list().into_iter().flat_map(|list| list.extern_items()) {
            // Note: All items in an `extern` block need to be lowered as if they're outside of one
            // (in other words, the knowledge that they're in an extern block must not be used).
            // This is because an extern block can contain macros whose ItemTree's top-level items
            // should be considered to be in an extern block too.
            let mod_item = match item {
                ast::ExternItem::Fn(ast) => self.lower_function(ast),
                ast::ExternItem::Static(ast) => self.lower_static(ast),
                ast::ExternItem::TypeAlias(ty) => self.lower_type_alias(ty),
                ast::ExternItem::MacroCall(call) => self.lower_macro_call(call),
            };
            let Some(mod_item) = mod_item else { continue };
            self.lower_attrs(mod_item, item);
            self.tree.pending_items.push(mod_item);
        }
        let children = self.tree.items_since(mark);

        let data = push(&mut self.tree.extern_blocks, ExternBlock { children });
        self.tree.item(ItemKind::ExternBlock, ast_id.upcast(), data)
    }

    fn lower_visibility<'t>(&mut self, item: impl HasVisibility<'t>) -> RawVisibilityId {
        let vis = self.lower_visibility_from_ast(item);
        self.tree.visibility(vis)
    }

    fn lower_visibility_from_ast<'t>(&self, item: impl HasVisibility<'t>) -> RawVisibility {
        visibility_from_ast(self.db, item.visibility(), &mut |range| {
            self.span_map().span_for_range(range).ctx
        })
    }
}

struct UseTreeLowering<'a, 't> {
    db: &'a Db,
    /// The lowered trees' syntax, in the order [`Use::expand`] numbers them: a tree before its
    /// children. Only the source map ([`lower_use_tree`]) wants it.
    mapping: Option<&'a mut Arena<ast::UseTree<'t>>>,
    tree: &'a mut TreeBuilder,
}

impl<'t> UseTreeLowering<'_, 't> {
    fn map(&mut self, tree: ast::UseTree<'t>) {
        if let Some(mapping) = &mut self.mapping {
            mapping.alloc(tree);
        }
    }

    /// Lowers `tree` into the tree's `use` trees, its children before it.
    fn lower_use_tree(
        &mut self,
        tree: ast::UseTree<'t>,
        span_for_range: &mut (impl FnMut(::tt::TextRange) -> SyntaxContext + ?Sized),
    ) -> Option<UseTreeId> {
        if let Some(use_tree_list) = tree.use_tree_list() {
            let prefix = match tree.path() {
                // E.g. use something::{{{inner}}};
                None => None,
                // E.g. `use something::{inner}` (prefix is `None`, path is `something`)
                // or `use something::{path::{inner::{innerer}}}` (prefix is `something::path`, path is `inner`)
                Some(path) => {
                    match ModPath::from_src_view(self.db, path, span_for_range) {
                        Some(it) => Some(it),
                        None => {
                            // FIXME: report errors somewhere
                            return None;
                        }
                    }
                }
            };

            self.map(tree);
            let mark = self.tree.pending_use_trees.len();
            for tree in use_tree_list.use_trees() {
                if let Some(id) = self.lower_use_tree(tree, span_for_range) {
                    self.tree.pending_use_trees.push(id);
                }
            }

            let path = match prefix {
                Some(prefix) => self.tree.path(prefix),
                None => PathId::NONE,
            };
            let list = self.tree.use_tree_list_since(mark);
            Some(self.tree.use_tree(UseTree {
                kind: UseTree::PREFIXED,
                path,
                alias: AliasId::NONE,
                list,
            }))
        } else {
            let is_glob = tree.star_token().is_some();
            let path = match tree.path() {
                Some(path) => Some(ModPath::from_src_view(self.db, path, span_for_range)?),
                // `use ::*;` globs the crate root.
                None if is_glob && tree.coloncolon_token().is_some() => {
                    Some(ModPath::from_kind(PathKind::Abs))
                }
                None => None,
            };
            let alias = tree.rename().map(|a| {
                a.name().map(|it| it.as_name()).map_or(ImportAlias::Underscore, ImportAlias::Alias)
            });
            if alias.is_some() && is_glob {
                return None;
            }

            match (path, alias, is_glob) {
                (path, None, true) => {
                    if path.is_none() {
                        cov_mark::hit!(glob_enum_group);
                    }
                    self.map(tree);
                    let path = match path {
                        Some(path) => self.tree.path(path),
                        None => PathId::NONE,
                    };
                    Some(self.tree.use_tree(UseTree {
                        kind: UseTree::GLOB,
                        path,
                        alias: AliasId::NONE,
                        list: IdRange::EMPTY,
                    }))
                }
                // Globs can't be renamed
                (_, Some(_), true) | (None, None, false) => None,
                // `bla::{ as Name}` is invalid
                (None, Some(_), false) => None,
                (Some(path), alias, false) => {
                    self.map(tree);
                    let path = self.tree.path(path);
                    let alias = self.tree.alias(alias);
                    Some(self.tree.use_tree(UseTree {
                        kind: UseTree::SINGLE,
                        path,
                        alias,
                        list: IdRange::EMPTY,
                    }))
                }
            }
        }
    }
}

/// The syntax of `tree`'s `use` trees, in the order [`Use::expand`] numbers them.
pub(crate) fn lower_use_tree<'t>(
    db: &Db,
    tree: ast::UseTree<'t>,
    span_for_range: &mut (impl FnMut(::tt::TextRange) -> SyntaxContext + ?Sized),
) -> Option<Arena<ast::UseTree<'t>>> {
    let mut scratch = SCRATCH.take();
    let mut mapping = Arena::new();
    let mut lowering = UseTreeLowering { db, mapping: Some(&mut mapping), tree: &mut scratch };
    let lowered = lowering.lower_use_tree(tree, span_for_range);
    scratch.clear_use_trees();
    if scratch.kinds.capacity() <= SCRATCH_ITEMS {
        SCRATCH.set(scratch);
    }
    lowered?;
    Some(mapping)
}

fn private_vis() -> RawVisibility {
    RawVisibility::PubSelf(VisibilityExplicitness::Implicit)
}

pub(crate) fn visibility_from_ast(
    db: &Db,
    node: Option<ast::Visibility<'_>>,
    span_for_range: &mut (impl FnMut(::tt::TextRange) -> SyntaxContext + ?Sized),
) -> RawVisibility {
    let Some(node) = node else { return private_vis() };
    let path = match node.kind() {
        ast::VisibilityKind::In(path) => {
            let path = ModPath::from_src_view(db, path, span_for_range);
            match path {
                None => return private_vis(),
                Some(path) => path,
            }
        }
        ast::VisibilityKind::PubCrate => return RawVisibility::PubCrate,
        ast::VisibilityKind::PubSuper => ModPath::from_kind(PathKind::Super(1)),
        ast::VisibilityKind::PubSelf => {
            return RawVisibility::PubSelf(VisibilityExplicitness::Explicit);
        }
        ast::VisibilityKind::Pub => return RawVisibility::Public,
    };
    RawVisibility::Module(path, VisibilityExplicitness::Explicit)
}
