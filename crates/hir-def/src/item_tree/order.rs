//! The items a body holds, in source order ([`body_items`]): what a pass that numbers items by
//! where they are written (dead code's, rustc's `ItemCollector` order) reads instead of a walk of
//! its own over the syntax. A block's item tree gives the order of the block's own items; this
//! gives their order among the items of the blocks nested in the block's statements
//! (`{ let _ = { fn a() {} }; fn b() {} }` holds `a`, then `b`), which no tree has.

use std::alloc::Global;

use base_db::Db;
use hir_expand::{ErasedAstId, HirFileId, InFile};
use span::AstIdMap;
use syntax::{
    SyntaxKind,
    view::{self, AstNode as _},
};

use crate::{
    BlockId, DefWithBodyId, HasModule,
    attrs::AttrFlags,
    expr_store::{
        BodySourceMap,
        body::{Body, body_syntax},
    },
};

/// An item written in a body: the block whose def map holds it and the ast id of the item, in
/// the file it is written in (the body's, or a macro expansion's). The block's scope maps it to
/// its id ([`crate::item_scope::ItemScope::items_by_ast`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BodyItem {
    pub block: BlockId,
    pub item: ErasedAstId,
}

/// The items written in the body of `owner`, in source order: the order a walk of the body's
/// syntax meets them, a macro call's expansion walked in place of the call, a node `cfg`'d out
/// not walked. Each is in the innermost block around it that has a def map. The items of those
/// items (an impl's, a nested function's body's) are not here: each body has its own list.
///
/// Read from the body's own syntax ([`body_syntax`]) and its unmoved source map, so it is
/// made again only when the body is, and is equal (no reader reruns) while its items are.
#[blaze_query::tracked(returns(deref))]
pub fn body_items(db: &Db, owner: DefWithBodyId) -> Box<[BodyItem]> {
    let _p = tracing::info_span!("body_items", ?owner).entered();
    let body = Body::of(db, owner);
    let blocks = body.store.block_scopes();
    if blocks.is_empty() {
        return Box::default();
    }
    let mut by_ast = blaze_hash::HashMap::new_in(Global);
    // alloc: the body's blocks by ast id, once per run of the query, dropped at its end
    by_ast.reserve(blocks.len());
    for &block in blocks {
        let ast = block.ast_id(db);
        by_ast.insert(InFile::new(ast.file_id, ast.value.erase()), block);
    }
    let krate = owner.krate(db);
    let mut walk = Walk {
        db,
        map: Body::unmoved_source_map(db, owner),
        cfg: krate.cfg_options(db),
        blocks: &by_ast,
        out: Vec::new(), // alloc: the query's result, at most once per body edit
    };
    match (Body::relative(db, owner), body_syntax(db, owner)) {
        // The unmoved map is the relative one: its pointers are into the body's own tree.
        (Some(_), Some(syntax)) => {
            let tree = syntax.tree(db);
            let item = view::to_node(syntax.item, view::Node::root(&tree));
            walk.body(owner, syntax.file_id, item, &syntax.ast_id_map);
        }
        // The unmoved map is the whole-file one: its pointers are into the file's tree.
        _ => {
            let InFile { file_id, value } = crate::expr_store::body::def_syntax(db, owner);
            walk.body(owner, file_id, value, file_id.ast_id_map(db));
        }
    }
    walk.out.into_boxed_slice()
}

struct Walk<'a> {
    db: &'a Db,
    map: &'a BodySourceMap,
    cfg: &'a cfg::CfgOptions,
    blocks: &'a blaze_hash::HashMap<ErasedAstId, BlockId, Global>,
    out: Vec<BodyItem>,
}

impl Walk<'_> {
    /// The body of the item `item` (a function's block, a const's or a static's initializer,
    /// a variant's discriminant): what its store is lowered from.
    fn body(&mut self, owner: DefWithBodyId, file: HirFileId, item: view::Node<'_>, ids: &AstIdMap) {
        let body = match owner {
            DefWithBodyId::FunctionId(_) => {
                view::Fn::cast(item).and_then(|it| it.body()).map(|it| it.syntax())
            }
            DefWithBodyId::ConstId(_) => {
                view::Const::cast(item).and_then(|it| it.body()).map(|it| it.syntax())
            }
            DefWithBodyId::StaticId(_) => {
                view::Static::cast(item).and_then(|it| it.body()).map(|it| it.syntax())
            }
            DefWithBodyId::VariantId(_) => (view::Variant::cast(item))
                .and_then(|it| it.const_arg())
                .and_then(|it| it.expr())
                .map(|it| it.syntax()),
        };
        if let Some(body) = body {
            self.node(file, body, ids, None);
        }
    }

    /// `node` of `file` (whose ast ids `ids` has), inside the block `block`.
    fn node(&mut self, file: HirFileId, node: view::Node<'_>, ids: &AstIdMap, block: Option<BlockId>) {
        if let Some(owner) = view::AnyHasAttrs::cast(node)
            && AttrFlags::is_cfg_enabled_in(owner, self.cfg).is_err()
        {
            return;
        }
        if let Some(call) = view::MacroCall::cast(node) {
            let Some(call) = self.map.expansion_view(InFile::new(file, call)) else { return };
            let expansion = HirFileId::from(call);
            let ids = expansion.ast_id_map(self.db);
            let root = expansion.root(self.db);
            match root.kind() {
                // Items or statements: each walked as if written where the call is.
                SyntaxKind::MACRO_ITEMS | SyntaxKind::MACRO_STMTS => {
                    for child in root.children() {
                        stdx::ensure_sufficient_stack(|| self.node(expansion, child, ids, block));
                    }
                }
                // An expression (a block among them) or a pattern or a type: the node itself.
                _ => stdx::ensure_sufficient_stack(|| self.node(expansion, root, ids, block)),
            }
            return;
        }
        // `view::Item` takes every `ASM_EXPR`, but only a `global_asm!` (`builtin # global_asm`)
        // is an item; an inline `asm!` or `naked_asm!` is an expression, which may sit in a block
        // with no def map. Its operands are walked as any expression's (they may hold blocks).
        if let Some(asm) = view::AsmExpr::cast(node)
            && asm.global_asm_token().is_none()
        {
            for child in node.children() {
                stdx::ensure_sufficient_stack(|| self.node(file, child, ids, block));
            }
            return;
        }
        if let Some(item) = view::Item::cast(node) {
            let Some(ast) = ids.try_ast_id_of_view::<syntax::ast::Item>(item) else {
                panic!("{file:?}: an item of a body has no ast id in its file's map")
            };
            match block {
                Some(block) => {
                    self.out.push(BodyItem { block, item: InFile::new(file, ast.erase()) })
                }
                None => panic!(
                    "{file:?}: item {ast:?} of a body is in no block of the body's store, which \
                     makes a block of every block holding an item"
                ),
            }
            return;
        }
        let block = match view::BlockExpr::cast(node).and_then(|it| ids.ast_id_for_block_view(it)) {
            Some(ast) => match self.blocks.get(&InFile::new(file, ast.erase())) {
                Some(&inner) => Some(inner),
                None => block,
            },
            None => block,
        };
        for child in node.children() {
            stdx::ensure_sufficient_stack(|| self.node(file, child, ids, block));
        }
    }
}
