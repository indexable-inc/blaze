//! The items of a crate as rustc's HIR has them, in the order its passes visit them: module
//! by module (`crate::checks::modules`), each module's items in source order with the items of
//! their bodies, impls, traits and `extern` blocks after each, what macro calls expand to in
//! place of the calls, and none that `cfg` turns off. Each item has the nodes that are its own:
//! its syntax and what macros in it expand to, without the items in it.
//!
//! Follows `compiler/rustc_lint/src/late.rs` (`late_lint_mod_inner`, `visit_item`,
//! `visit_nested_item`: each module on its own, items in bodies walked with the body)
//! (MIT or Apache-2.0).

use std::{ops::ControlFlow, rc::Rc};

use blaze_hash::HashMap;
use blaze_ids::radix;
use blaze_mem::Frame;
use hir::{HirFileId, InFile, view::ModuleSourceView};
use smallvec::SmallVec;
use syntax::{
    SyntaxKind, SyntaxNodePtr, TextRange,
    view::{self as ast, AstNode, HasAttrs, HasModuleItem},
};

use crate::cx::Cx;

/// What an item is an item of.
#[derive(Clone, Copy, Debug)]
pub(super) enum Container<'db> {
    /// A module's or a block's own item.
    Module,
    Trait(InFile<ast::Trait<'db>>),
    Impl(InFile<ast::Impl<'db>>),
    Extern(InFile<ast::ExternBlock<'db>>),
}

/// An item, with where it is and the nodes that are its own.
#[derive(Debug)]
pub(super) struct Item<'db> {
    pub(super) node: InFile<ast::Item<'db>>,
    pub(super) container: Container<'db>,
    /// The module it is declared in, not counting blocks.
    pub(super) module: hir::Module,
    /// The index of the item whose body it is written in, if it is in one.
    pub(super) in_body: Option<usize>,
    /// One past the index of the last item in it (its associated, foreign and body items and
    /// theirs): those after it up to `end` are what it holds.
    pub(super) end: usize,
    /// The walk's one column of every item's own nodes, grouped by item.
    column: Rc<[InFile<ast::Node<'db>>]>,
    /// Where its own nodes are in `column`.
    own: (u32, u32),
}

impl<'db> Item<'db> {
    /// Its own nodes, in source order (preorder), itself first; in what the macros it calls
    /// expand to, too; without the items in it and what is in them.
    pub(super) fn nodes(&self) -> &[InFile<ast::Node<'db>>] {
        &self.column[self.own.0 as usize..self.own.1 as usize]
    }

    /// The node rustc emits an item's lints at.
    pub(super) fn ptr(&self) -> InFile<SyntaxNodePtr> {
        self.node.map(|it| ast::ptr(it.syntax()))
    }

    pub(super) fn syntax(&self) -> InFile<ast::Node<'db>> {
        self.node.map(|it| it.syntax())
    }

    /// Its own nodes that are `N`, each kind tested before it is cast.
    pub(super) fn own<N: AstNode<'db>>(&self) -> impl Iterator<Item = InFile<N>> + '_ {
        self.nodes()
            .iter()
            .filter(|it| N::can_cast(it.value.kind()))
            .filter_map(|it| N::cast(it.value).map(|n| it.with_value(n)))
    }

    /// The items it holds, among `items` (all of the walk's, it at `index`).
    pub(super) fn held<'i>(&self, items: &'i [Item<'db>], index: usize) -> &'i [Item<'db>] {
        &items[index + 1..self.end]
    }
}

/// A key that sorts as the AST has things: the offsets of the declarations of a module and the
/// modules it is in from the crate's root, then an offset in it. Inline up to three levels.
pub(super) type Order = SmallVec<[u32; 4]>;

/// Each module's place in the crate as the AST has it (the offsets of the declarations of it
/// and the modules it is in, from the crate's root), made once per module: one hash lookup
/// per key after that.
#[derive(Default)]
pub(super) struct Orders {
    of: HashMap<hir::Module, Order>,
}

impl Orders {
    /// The key that sorts what is at `lo` in `module` as the AST has it, each module's items in
    /// place of its `mod`: rustc's order of `DefIndex`es and of the early lint pass.
    pub(super) fn key(&mut self, cx: &Cx<'_, '_>, module: hir::Module, lo: u32) -> Order {
        let mut key = Order::from_slice(self.of.entry(module).or_insert_with(|| module_order(cx, module)));
        key.push(lo);
        key
    }
}

fn module_order(cx: &Cx<'_, '_>, module: hir::Module) -> Order {
    let mut order = Order::new();
    let mut at = Some(module);
    while let Some(module) = at {
        if let Some(range) = module.declaration_source_range(cx.db) {
            order.push(cx.span(range).lo);
        }
        at = module.parent(cx.db);
    }
    order.reverse();
    order
}

/// The fields of a struct, a union or an enum's variants, in the order they are written.
pub(super) fn fields<'db>(item: &Item<'db>) -> SmallVec<[InFile<ast::Node<'db>>; 8]> {
    let of = |kind: SyntaxKind| item.nodes().iter().filter(move |it| it.value.kind() == kind).copied();
    let mut fields: SmallVec<[InFile<ast::Node<'db>>; 8]> = SmallVec::new();
    fields.extend(of(SyntaxKind::RECORD_FIELD).chain(of(SyntaxKind::TUPLE_FIELD)));
    radix::sort_by_key(&mut fields, |it| {
        radix::pack_32(u32::from(it.file_id != item.node.file_id), it.value.text_range().start().into())
    });
    fields
}

/// The items of `cx.krate` in the order rustc visits them.
pub(super) fn items<'db>(cx: &Cx<'_, 'db>) -> Vec<Item<'db>> {
    let mut walk = Walk {
        cx,
        // alloc: the one walk of the check, held by `items::Walk` and read by every pass after it
        items: Vec::new(),
        own: Vec::new_in(cx.frame()),
        // alloc: one empty column for the whole walk, replaced by the real one at the end
        empty: Rc::from([]),
    };
    for module in crate::checks::modules(cx) {
        let source = module.definition_source_view(cx.db);
        let file = source.file_id;
        let root = cx.root(file);
        let list: Option<ast::AstChildren<'db, ast::Item<'db>>> = match source.value {
            ModuleSourceView::SourceFile(_) => ast::SourceFile::cast(root).map(|it| it.items()),
            ModuleSourceView::Module(it) => ast::Module::cast(ast::to_node(ast::ptr(it.syntax()), root))
                .and_then(|it| it.item_list())
                .map(|it| it.items()),
            ModuleSourceView::BlockExpr(_) => continue,
        };
        for item in list.into_iter().flatten() {
            walk.item(InFile::new(file, item), Container::Module, module, None);
        }
    }
    walk.finish()
}

struct Walk<'c, 'a, 'db> {
    cx: &'c Cx<'a, 'db>,
    items: Vec<Item<'db>>,
    /// Each item's own nodes as they are met, with the index of the item they are of. An
    /// item's nodes interleave with the items nested in it, so they are grouped at the end.
    own: Vec<(u32, InFile<ast::Node<'db>>), &'a Frame>,
    /// The column each item points at until the walk ends.
    empty: Rc<[InFile<ast::Node<'db>>]>,
}

impl<'db> Walk<'_, '_, 'db> {
    /// The items, each pointing at its own nodes in one column: the nodes grouped by item
    /// (stable, so each item's stay in preorder).
    fn finish(self) -> Vec<Item<'db>> {
        let Walk { mut items, mut own, .. } = self;
        radix::sort_by_key(&mut own[..], |it| it.0);
        let mut at = 0;
        for (index, item) in items.iter_mut().enumerate() {
            let start = at;
            while own.get(at).is_some_and(|it| it.0 as usize == index) {
                at += 1;
            }
            item.own = (start as u32, at as u32);
        }
        // alloc: the walk's one column of nodes, held by `items::Walk` for the check
        let column: Rc<[InFile<ast::Node<'db>>]> = own.into_iter().map(|(_, node)| node).collect();
        for item in &mut items {
            item.column = Rc::clone(&column);
        }
        items
    }

    /// Records `node` as one of the item at `owner`'s own nodes.
    fn keep(&mut self, owner: usize, node: InFile<ast::Node<'db>>) {
        self.own.push((owner as u32, node));
    }

    /// Visits `item`, written in `container`, and what is in it.
    fn item(&mut self, item: InFile<ast::Item<'db>>, container: Container<'db>, module: hir::Module, in_body: Option<usize>) {
        if !self.cx.cfg_enabled(item.value.syntax()) {
            return;
        }
        if let ast::Item::MacroCall(call) = item.value {
            if let Some(expansion) = self.expand(item.with_value(call)) {
                self.expansion(expansion, container, module, in_body);
            }
            return;
        }
        if self.cx.vsema.is_attr_macro_call(item) {
            if let Some(expansion) = self.cx.vsema.expand_attr_macro(item.value) {
                self.expansion(expansion.value, container, module, in_body);
            }
            return;
        }
        let index = self.items.len();
        self.items.push(Item {
            node: item,
            container,
            module,
            in_body,
            end: index + 1,
            column: Rc::clone(&self.empty),
            own: (0, 0),
        });
        self.keep(index, item.map(|it| it.syntax()));
        let file = item.file_id;
        let inner = match item.value {
            // Another module's items, visited with it.
            ast::Item::Module(_) | ast::Item::MacroRules(_) | ast::Item::MacroDef(_) => return,
            ast::Item::Impl(it) => Some(Container::Impl(item.with_value(it))),
            ast::Item::Trait(it) => Some(Container::Trait(item.with_value(it))),
            ast::Item::ExternBlock(it) => Some(Container::Extern(item.with_value(it))),
            _ => None,
        };
        for child in item.value.syntax().children() {
            let list = matches!(child.kind(), SyntaxKind::ASSOC_ITEM_LIST | SyntaxKind::EXTERN_ITEM_LIST);
            match inner {
                Some(container) if list => {
                    self.keep(index, InFile::new(file, child));
                    for it in child.children() {
                        self.node(file, it, index, Some(container), module);
                    }
                }
                _ => self.node(file, child, index, None, module),
            }
        }
        self.items[index].end = self.items.len();
    }

    /// What a macro call or an attribute macro of an item expands to, in place of it.
    fn expansion(&mut self, root: InFile<ast::Node<'db>>, container: Container<'db>, module: hir::Module, in_body: Option<usize>) {
        for child in root.value.children() {
            if is_item(child)
                && let Some(item) = ast::Item::cast(child)
            {
                self.item(root.with_value(item), container, module, in_body);
            }
        }
    }

    /// `node`, under the item `owner`: one of its own, or an item in it, which is of
    /// `direct` when it is one of the items of an impl, a trait or an `extern` block `owner`
    /// is, else of `owner`'s body.
    fn node(&mut self, file: HirFileId, node: ast::Node<'db>, owner: usize, direct: Option<Container<'db>>, module: hir::Module) {
        if !self.cx.cfg_enabled(node) {
            return;
        }
        let kind = node.kind();
        if is_item(node) {
            let item = ast::Item::cast(node).expect("an item");
            match direct {
                Some(container) => self.item(InFile::new(file, item), container, module, self.items[owner].in_body),
                None => self.item(InFile::new(file, item), Container::Module, module, Some(owner)),
            }
            return;
        }
        if kind == SyntaxKind::MACRO_CALL
            && let Some(call) = ast::MacroCall::cast(node)
        {
            self.keep(owner, InFile::new(file, node));
            let Some(expansion) = self.expand(InFile::new(file, call)) else { return };
            match expansion.value.kind() {
                SyntaxKind::MACRO_ITEMS | SyntaxKind::MACRO_STMTS => {
                    for child in expansion.value.children() {
                        self.node(expansion.file_id, child, owner, direct, module);
                    }
                }
                _ => self.node(expansion.file_id, expansion.value, owner, None, module),
            }
            return;
        }
        self.keep(owner, InFile::new(file, node));
        for child in node.children() {
            self.node(file, child, owner, None, module);
        }
    }

    fn expand(&self, call: InFile<ast::MacroCall<'db>>) -> Option<InFile<ast::Node<'db>>> {
        let id = self.cx.vsema.to_def2(call)?;
        let file: HirFileId = id.into();
        Some(InFile::new(file, self.cx.root(file)))
    }
}

/// Whether `node` is an item of HIR's: an item, an associated item or an item of an `extern`
/// block, not a macro call (which is what it expands to) nor an `asm!` in a body.
fn is_item(node: ast::Node<'_>) -> bool {
    match node.kind() {
        SyntaxKind::MACRO_CALL => false,
        SyntaxKind::ASM_EXPR => matches!(
            node.parent().map(|it| it.kind()),
            Some(SyntaxKind::SOURCE_FILE | SyntaxKind::ITEM_LIST | SyntaxKind::MACRO_ITEMS)
        ),
        _ => ast::Item::can_cast(node.kind()),
    }
}

/// Whether `f` accepts any attribute named `name` on `owner`, its `cfg_attr`s expanded,
/// stopping at the first it does.
pub(super) fn any_named<'t>(cx: &Cx<'_, '_>, owner: impl HasAttrs<'t>, name: &str, mut f: impl FnMut(ast::Meta<'t>) -> bool) -> bool {
    named(cx, owner, name, |meta, _| match f(meta) {
        true => ControlFlow::Break(()),
        false => ControlFlow::Continue(()),
    })
    .is_some()
}

/// The first attribute named `name` on `owner`, as [`named`] gives it.
pub(super) fn attr<'t>(cx: &Cx<'_, '_>, owner: impl HasAttrs<'t>, name: &str) -> Option<(ast::Meta<'t>, TextRange)> {
    named(cx, owner, name, |meta, range| ControlFlow::Break((meta, range)))
}

/// Calls `f` with each attribute named `name` on `owner`, its `cfg_attr`s expanded, until it
/// breaks: each with the span rustc gives it (the whole `#[..]`, or what is in a `cfg_attr`).
fn named<'t, B>(
    cx: &Cx<'_, '_>,
    owner: impl HasAttrs<'t>,
    name: &str,
    mut f: impl FnMut(ast::Meta<'t>, TextRange) -> ControlFlow<B>,
) -> Option<B> {
    if owner.attrs().next().is_none() {
        return None;
    }
    let cfg = cx.krate.cfg(cx.db);
    hir_expand::attrs::expand_cfg_attr_view(
        owner.attrs(),
        || cfg,
        |meta, attr| {
            if !meta.simple_name().is_some_and(|it| it == name) {
                return ControlFlow::Continue(());
            }
            let range = match meta.syntax().parent() == Some(attr.syntax()) {
                true => attr.syntax().text_range(),
                false => meta.syntax().text_range(),
            };
            f(meta, range)
        },
    )
}
