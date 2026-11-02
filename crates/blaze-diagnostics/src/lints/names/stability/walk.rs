//! The crate walked once as rustc's stability checks walk it, each use checked as the walk
//! comes to it: each item-like (rustc's HIR owner: an item, a trait item, an impl item, a
//! foreign item) in the crate's order with the module it is of, and in each the uses the
//! checks look at, as the node they are at.
//!
//! - paths resolved by name resolution: `Checker::visit_path`, `visit_use` (one check of a
//!   constructor imported with its struct), the parent segments (stability alone);
//! - type-relative paths, method calls, field accesses, the fields of struct expressions and
//!   patterns and of tuple-struct patterns, associated item constraints: typeck's and
//!   `hir_ty_lowering`'s `check_stability`;
//! - macro calls, derives and attribute macros: resolution's
//!   `check_stability_and_deprecation`;
//! - each `#[deprecated]` with its target, and each `#[macro_escape]` of a `mod`;
//! - each `#[unstable]` trait impl: `ineffective_unstable_trait_impl` when what its self type
//!   and trait name is all stable (`CheckTraitImplStable`, over the impl's header alone).
//!
//! The syntax is the crate walk every syntax pass shares (`cx.walk()`, [`Walk`]), which has
//! already left out what `cfg` removes and the items attribute macros replace. Each tree's
//! `kinds` column is scanned, and a node is read only when its kind is one this walk looks at
//! ([`looked_at`]); a node's attributes only when the walk's `attrs` column says it may have
//! one. Where rustc's order is not the trees' pre-order, the walk follows rustc:
//!
//! - what a macro call, an attribute macro, a derive or a `mod` file brings in is walked where
//!   it is reached (a derive's after its item);
//! - a method's or field's check comes after the expression it is of, and a `let`'s type and
//!   initializer before its pattern.
//!
//! A use lives on the stack: it is a probe of the [`Index`] and is gone. Only what is
//! reported is kept, with a `u64` key of where rustc's checks come to it, and sorted once at
//! the end. The parent segments of a path are resolved only when the path itself was allowed
//! and names another crate's definition: a definition of this crate is reached only through
//! this crate's modules, whose uses rustc doesn't check.
//!
//! Follows `compiler/rustc_passes/src/stability.rs` (`Checker`, `is_unstable_reexport`),
//! `compiler/rustc_hir/src/intravisit.rs` (`walk_use`), `compiler/rustc_middle/src/hir/map.rs`
//! (`hir_module_items`: items, trait items, impl items, foreign items),
//! `compiler/rustc_ast_lowering/src/item.rs` (`lower_use_tree`), `compiler/rustc_hir_typeck/
//! src/method/mod.rs` (`lookup_method`, `resolve_fully_qualified_call`), `expr.rs`
//! (`check_expr_field`, `check_expr_struct_fields`), `pat.rs` (`check_pat_tuple_struct`,
//! `check_struct_pat_fields`), `compiler/rustc_hir_analysis/src/hir_ty_lowering/mod.rs`
//! (`lower_type_relative_path`, `check_assoc_item`), `compiler/rustc_resolve/src/macros.rs`
//! (`check_stability_and_deprecation`) and `compiler/rustc_resolve/src/build_reduced_graph.rs`
//! (`contains_macro_use`) (MIT or Apache-2.0).

use std::ops::ControlFlow;

use cfg::CfgOptions;
use either::Either;
use hir::{Adt, HasVisibility as _, HirFileId, InFile, ModuleDef, PathResolution, view::ModuleSourceView};
use hir_def::attrs::stability::{DeprecatedSince, DeprecationEntry, Stability};
use hir_expand::name::AsName as _;
use intern::Symbol;
use smallvec::SmallVec;
use syntax::{
    NodeOrToken, SyntaxKind, SyntaxNodePtr, TextRange,
    view::{self as ast, AstNode as _, Node},
};

use super::{
    Site,
    attr,
    def::{Def, ItemLike, trait_item},
    lookup::Index,
    target::{self, Attribute, GenericParamKind, MethodKind, Target},
};
use crate::{
    cx::Cx,
    lints::{
        registry::LintId,
        syntax::{Tree, Walk},
    },
    lower::{Found, Phase},
    model::Diag,
};

/// What an item-like is in: rustc's `hir_module_items` lists a module's items, trait items,
/// impl items and foreign items, in that order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Container {
    Free,
    Trait,
    Impl { of_trait: bool },
    Foreign,
}

impl Container {
    /// Which of `hir_module_items`' lists an item-like written in `self` is in.
    fn list(self) -> u8 {
        match self {
            Container::Free => 0,
            Container::Trait => 1,
            Container::Impl { .. } => 2,
            Container::Foreign => 3,
        }
    }
}

/// The item-like of what is outside every item: the crate root.
const ROOT: u32 = u32::MAX;

/// How many item-likes the order keys hold: the item-like takes the low 29 bits of a key.
const OWNERS: usize = 1 << 29;

/// An item-like, rustc's HIR owner: what `hir_get_parent_item` of a use in it is.
#[derive(Clone, Copy)]
struct Owner {
    node: InFile<SyntaxNodePtr>,
    /// The module it is listed in, by where rustc's walk of modules comes to it.
    module: u32,
    /// Which of the module's lists it is in ([`Container::list`]).
    list: u8,
    /// A `use` item, which may be an unstable re-export.
    is_use: bool,
}

/// A use of a definition the checks look at, checked where it is found and not kept.
#[derive(Clone, Copy)]
struct Use {
    def: Def,
    /// The item-like it is in, or [`ROOT`].
    owner: u32,
    /// The node of the use, where the lint's level is decided.
    at: InFile<SyntaxNodePtr>,
    /// Where a deprecation is reported: `method_span.unwrap_or(span)`.
    span: TextRange,
    /// Where an unstable use is reported: `span`.
    whole: TextRange,
    phase: Phase,
    /// The use's node is an expression: a deprecation's suggestion is given.
    expr: bool,
    /// A parent segment of a path or the trait of a `<Type as Trait>`: checked for stability
    /// alone (`check_stability` with no `id`).
    stability_only: bool,
    /// In what a derive made (`Span::in_derive_expansion`): no deprecation is reported.
    derive: bool,
}

impl Use {
    fn site(&self, range: TextRange) -> Site {
        Site { at: self.at, span: InFile::new(self.at.file_id, range), phase: self.phase }
    }
}

#[derive(Clone, Copy)]
struct Ctx {
    owner: u32,
    module: u32,
    /// What the items written here are in.
    container: Container,
    /// Whether the tree walked is entered in a body: what [`in_body`] answers above its root.
    body: bool,
    derive: bool,
}

/// A node's `#[deprecated]`s, with the attribute each is written in.
type Deprecated<'t> = SmallVec<[(ast::Meta<'t>, ast::Attr<'t>); 1]>;

struct Walker<'a, 'b, 'db> {
    cx: &'a Cx<'b, 'db>,
    walk: &'a Walk<'db>,
    cfg: &'db CfgOptions,
    index: Index<'a, 'b, 'db>,
    owners: Vec<Owner>,
    modules: u32,
    /// The crate root, the item-like [`ROOT`] stands for.
    root: InFile<SyntaxNodePtr>,
    /// Whether the crate enables `extern_types`, read the first time a help needs it.
    extern_types: Option<bool>,
    reported: Reported,
}

/// What the checks found, by the pass that reports it.
#[derive(Default)]
struct Reported {
    /// The errors of what the walk reached and the crate walk has no tree of.
    missing: Vec<Found>,
    escapes: Vec<Found>,
    macro_unstable: Vec<Found>,
    macro_deprecated: Vec<Found>,
    /// `check_mod_attrs`' and `check_mod_unstable_api_usage`'s, by [`Walker::item_key`].
    items: Vec<(u64, Found)>,
    useless: Vec<Found>,
    /// Typeck's, by the item-like they are in.
    bodies: Vec<(u32, Found)>,
}

/// What `CheckTraitImplStable` has found so far of a trait impl's self type and trait.
struct Header {
    /// `fully_stable`: no use of an unstable definition or `extern` ABI yet.
    stable: bool,
    /// The definitions looked up, the facts of the lint when it fires.
    defs: SmallVec<[Def; 4]>,
}

/// The stability checks of the crate, in rustc's order: what resolution reports of macros,
/// each module's `check_mod_attrs` then `check_mod_unstable_api_usage`, the attribute
/// parser's delayed lints, then typeck's.
pub(super) fn walk(cx: &Cx<'_, '_>) -> Vec<Found> {
    let db = cx.db;
    let walk = cx.walk();
    let file = cx.krate.root_module(db).definition_source_view(db).file_id;
    let tree = walk.tree(file).expect("the crate walk starts at the crate root's file");
    let mut walker = Walker {
        cx,
        walk,
        cfg: walk.cfg,
        index: Index::new(cx),
        owners: Vec::new(), // alloc: once per crate, one slot per item-like
        modules: 1,
        root: tree.ptr(tree.root),
        extern_types: None,
        reported: Reported::default(),
    };
    let ctx = Ctx { owner: ROOT, module: 0, container: Container::Free, body: false, derive: false };
    // With none, a `#![cfg]` removes the crate's contents and attributes.
    if let Some(&root) = tree.nodes.first() {
        if tree.attrs[0] {
            let deprecated = walker.deprecated(root);
            for (meta, written) in &deprecated {
                walker.deprecated_attr(Target::Crate, InFile::new(file, root), meta, written, ROOT, false);
            }
        }
        walker.tree(tree, ctx);
    }
    walker.finish()
}

impl<'a, 'b, 'db> Walker<'a, 'b, 'db> {
    fn finish(self) -> Vec<Found> {
        let Reported { missing, escapes, macro_unstable, macro_deprecated, mut items, useless, mut bodies } = self.reported;
        items.sort_by_key(|(key, _)| *key);
        bodies.sort_by_key(|(key, _)| *key);
        let count = missing.len()
            + escapes.len()
            + macro_unstable.len()
            + macro_deprecated.len()
            + items.len()
            + useless.len()
            + bodies.len();
        let mut found = Vec::with_capacity(count);
        found.extend(missing);
        found.extend(escapes);
        found.extend(macro_unstable);
        found.extend(macro_deprecated);
        found.extend(items.into_iter().map(|(_, it)| it));
        found.extend(useless);
        found.extend(bodies.into_iter().map(|(_, it)| it));
        found
    }

    /// Where rustc's checks of the module an item-like is in come to what is reported of it:
    /// the module, `check_mod_attrs` (`0`) before `check_mod_unstable_api_usage` (`1`), the
    /// item-like's list in `hir_module_items`, the item-like.
    fn item_key(&self, owner: u32, check: u64) -> u64 {
        if owner == ROOT {
            return check << 31;
        }
        let it = self.owners[owner as usize];
        (u64::from(it.module) << 32) | (check << 31) | (u64::from(it.list) << 29) | u64::from(owner + 1)
    }

    /// Keeps what the check of `used` found, by the pass that reports it.
    fn emit(&mut self, used: &Use, found: Found) {
        match used.phase {
            Phase::Bodies => self.reported.bodies.push((used.owner.wrapping_add(1), found)),
            _ => {
                let key = self.item_key(used.owner, 1);
                self.reported.items.push((key, found));
            }
        }
    }

    /// The `#[deprecated]`s of `node`, once `cfg_attr` is expanded.
    fn deprecated(&self, node: Node<'db>) -> Deprecated<'db> {
        let mut found = Deprecated::new();
        attr::each::<()>(node, self.cfg, |meta, written| {
            if attr::is(&meta, "deprecated") {
                found.push((meta, written));
            }
            ControlFlow::Continue(())
        });
        found
    }

    /// The tree of `file`, which `node` of `t` brings in. With none its uses can't be checked,
    /// and that is an error at `node`.
    fn tree_of(&mut self, file: HirFileId, t: &Tree<'db>, node: Node<'db>) -> Option<&'a Tree<'db>> {
        let walk = self.walk;
        let found = walk.tree(file);
        if found.is_none() {
            let span = self.cx.span(InFile::new(t.file, node.text_range()));
            let diag = Diag::error(
                None,
                "blaze can't check the stability of the uses in what this brings in: the crate's syntax walk does not have it",
                span,
            );
            self.reported.missing.push(Found::new(diag, Phase::Items));
        }
        found
    }

    /// Walks `t` from its root, entered with `ctx`.
    fn tree(&mut self, t: &'a Tree<'db>, ctx: Ctx) {
        self.run(t, 0, None, ctx, None);
    }

    /// Visits what the walk comes to in `t` from the node at `from` on while it is in `parent`
    /// (to the end of `t` with none), in pre-order: each node it looks at with what is in it,
    /// and each item an attribute macro replaces. A node of `list`'s kind is visited with
    /// `list`'s context. Returns the index of the first node looked at after.
    fn run(
        &mut self,
        t: &'a Tree<'db>,
        from: usize,
        parent: Option<Node<'db>>,
        ctx: Ctx,
        list: Option<(SyntaxKind, Ctx)>,
    ) -> usize {
        let mut replaced = replaced_in(t, parent);
        let outer = parent.map(|it| (it, it.text_range()));
        let mut j = from;
        loop {
            j = next(t, j);
            let here = t.nodes.get(j).copied().filter(|&it| outer.is_none_or(|(parent, range)| inside(parent, range, it)));
            while let Some((&item, rest)) = replaced.split_first()
                && here.is_none_or(|it| item.text_range().start() < it.text_range().start())
            {
                self.attr_macro(t, item, ctx);
                replaced = rest;
            }
            let Some(node) = here else { return j };
            let at = match list {
                Some((kind, list)) if t.kinds[j] == kind => list,
                _ => ctx,
            };
            j = self.visit(t, j, at);
            let end = node.text_range().end();
            while let Some((item, rest)) = replaced.split_first()
                && item.text_range().start() < end
            {
                replaced = rest;
            }
        }
    }

    fn children(&mut self, t: &'a Tree<'db>, j: usize, ctx: Ctx) -> usize {
        self.run(t, j + 1, Some(t.nodes[j]), ctx, None)
    }

    /// Visits the node at `j`, one the walk looks at, and what is in it. Returns the index of
    /// the first node looked at after.
    fn visit(&mut self, t: &'a Tree<'db>, j: usize, ctx: Ctx) -> usize {
        let (kind, node) = (t.kinds[j], t.nodes[j]);
        match kind {
            SyntaxKind::ATTR => return past(t, j + 1, node),
            SyntaxKind::MACRO_CALL => {
                if let Some(call) = ast::MacroCall::cast(node) {
                    self.macro_call(t, call, ctx);
                }
                return past(t, j + 1, node);
            }
            SyntaxKind::LET_STMT => return self.let_stmt(t, j, ctx),
            _ => {}
        }
        if ast::Item::can_cast(kind)
            && let Some(item) = ast::Item::cast(node)
        {
            return self.item(t, j, item, ctx);
        }
        if t.attrs[j]
            && let Some(target) = param_target(kind)
        {
            let deprecated = self.deprecated(node);
            for (meta, written) in &deprecated {
                self.deprecated_attr(target, InFile::new(t.file, node), meta, written, ctx.owner, false);
            }
        }
        match kind {
            // typeck checks a method or field after the expression it is of.
            SyntaxKind::METHOD_CALL_EXPR | SyntaxKind::FIELD_EXPR => {
                let after = self.children(t, j, ctx);
                self.uses_at(t.file, node, ctx);
                after
            }
            _ => {
                self.uses_at(t.file, node, ctx);
                self.children(t, j, ctx)
            }
        }
    }

    /// `check_decl_local`'s order: the type, the initializer, the pattern, then the rest. What
    /// is in each child of the `let` at `j` is a run of `t`, found in one pass over it. The
    /// type, the initializer and the pattern are `LetStmt::ty`, `initializer` and `pat`: the
    /// first child of their kind.
    fn let_stmt(&mut self, t: &'a Tree<'db>, j: usize, ctx: Ctx) -> usize {
        let node = t.nodes[j];
        let mut children: SmallVec<[(Node<'db>, usize); 8]> = SmallVec::new();
        // The indices in `children` of the type, the initializer and the pattern.
        let mut first: [Option<usize>; 3] = [None; 3];
        let mut after = j + 1;
        for child in node.children() {
            let kind = child.kind();
            let slot = if ast::Type::can_cast(kind) {
                Some(0)
            } else if ast::Expr::can_cast(kind) {
                Some(1)
            } else if ast::Pat::can_cast(kind) {
                Some(2)
            } else {
                None
            };
            if let Some(slot) = slot
                && first[slot].is_none()
            {
                first[slot] = Some(children.len());
            }
            let from = after;
            after = past(t, after, child);
            children.push((child, from));
        }
        for index in first.into_iter().flatten() {
            let (child, from) = children[index];
            self.run(t, from, Some(child), ctx, None);
        }
        for (index, &(child, from)) in children.iter().enumerate() {
            if !first.contains(&Some(index)) {
                self.run(t, from, Some(child), ctx, None);
            }
        }
        after
    }

    fn owner(&mut self, node: InFile<SyntaxNodePtr>, ctx: Ctx, is_use: bool) -> u32 {
        let id = self.owners.len();
        assert!(id < OWNERS, "more item-likes in one crate than the stability checks' order keys hold");
        self.owners.push(Owner { node, module: ctx.module, list: ctx.container.list(), is_use });
        id as u32
    }

    fn item(&mut self, t: &'a Tree<'db>, j: usize, item: ast::Item<'db>, ctx: Ctx) -> usize {
        let node = t.nodes[j];
        let owner = self.owner(t.ptr(node), ctx, matches!(item, ast::Item::Use(_)));
        let inner = Ctx { owner, container: Container::Free, body: false, ..ctx };
        let of_trait_impl = ctx.container == Container::Impl { of_trait: true };
        if t.attrs[j]
            && let Some(target) = item_target(item, ctx.container)
        {
            let deprecated = self.deprecated(node);
            for (meta, written) in &deprecated {
                self.deprecated_attr(target, InFile::new(t.file, node), meta, written, owner, of_trait_impl);
            }
        }
        let list = |kind: SyntaxKind, container: Container| Some((kind, Ctx { container, ..inner }));
        match item {
            ast::Item::Module(module) => self.module(t, j, module, inner),
            ast::Item::Impl(imp) => {
                let of_trait = imp.trait_().is_some();
                if of_trait && t.attrs[j] {
                    self.ineffective_unstable(t, j, imp, owner);
                }
                let list = list(SyntaxKind::ASSOC_ITEM_LIST, Container::Impl { of_trait });
                self.run(t, j + 1, Some(node), inner, list)
            }
            ast::Item::Trait(_) => {
                self.run(t, j + 1, Some(node), inner, list(SyntaxKind::ASSOC_ITEM_LIST, Container::Trait))
            }
            ast::Item::ExternBlock(_) => {
                self.run(t, j + 1, Some(node), inner, list(SyntaxKind::EXTERN_ITEM_LIST, Container::Foreign))
            }
            ast::Item::Struct(_) | ast::Item::Enum(_) | ast::Item::Union(_) => {
                let after = self.children(t, j, inner);
                self.derives(t, j, ctx);
                after
            }
            _ => self.children(t, j, inner),
        }
    }

    fn module(&mut self, t: &'a Tree<'db>, j: usize, module: ast::Module<'db>, inner: Ctx) -> usize {
        let cx = self.cx;
        let node = t.nodes[j];
        let contents = Ctx { module: self.modules, ..inner };
        self.modules += 1;
        let mut file_root: Option<&'a Tree<'db>> = None;
        let after = match module.item_list() {
            Some(_) => self.children(t, j, contents),
            None => {
                let after = self.children(t, j, inner);
                if let Some(def) = cx.vsema.to_def(module)
                    && let source = def.definition_source_view(cx.db)
                    && let ModuleSourceView::SourceFile(_) = source.value
                    && let Some(file) = self.tree_of(source.file_id, t, node)
                    && let Some(&root) = file.nodes.first()
                {
                    if file.attrs[0] {
                        let deprecated = self.deprecated(root);
                        for (meta, written) in &deprecated {
                            self.deprecated_attr(Target::Mod, InFile::new(file.file, root), meta, written, inner.owner, false);
                        }
                    }
                    self.tree(file, contents);
                    file_root = Some(file);
                }
                after
            }
        };
        let first = match t.attrs[j] {
            true => self.macro_use(node),
            false => None,
        }
        .or_else(|| file_root.filter(|it| it.attrs[0]).and_then(|it| self.macro_use(it.root)));
        if let Some((meta, written)) = first
            && attr::is(&meta, "macro_escape")
        {
            let file = cx.vsema.hir_file_for(written.syntax());
            let range = match written.meta() {
                Some(it) if it.syntax() == meta.syntax() => written.syntax().text_range(),
                _ => meta.syntax().text_range(),
            };
            let found = super::macro_escape(cx, InFile::new(file, range), written.kind().is_inner(), t.ptr(node));
            self.reported.escapes.push(found);
        }
        after
    }

    /// The first `#[macro_use]` or `#[macro_escape]` of `node`.
    fn macro_use(&self, node: Node<'db>) -> Option<(ast::Meta<'db>, ast::Attr<'db>)> {
        attr::each(node, self.cfg, |meta, written| {
            if attr::is(&meta, "macro_use") || attr::is(&meta, "macro_escape") {
                ControlFlow::Break((meta, written))
            } else {
                ControlFlow::Continue(())
            }
        })
    }

    /// A `#[deprecated]` `meta` written in `written` on `on`, a `target` in the item-like
    /// `owner`: the lints of the attribute checks and of the attribute parser's target check.
    fn deprecated_attr(
        &mut self,
        target: Target,
        on: InFile<Node<'_>>,
        meta: &ast::Meta<'_>,
        written: &ast::Attr<'_>,
        owner: u32,
        of_trait_impl: bool,
    ) {
        let useless = target::is_useless(target);
        if !useless && !target::may_have_no_effect(target, of_trait_impl) {
            return;
        }
        let Some(attr) = Attribute::new(target, on, meta, written) else { return };
        let cx = self.cx;
        if let Some(found) = target::no_effect(cx, &attr, of_trait_impl) {
            let key = self.item_key(owner, 0);
            self.reported.items.push((key, found));
        }
        if useless {
            let extern_types = *self.extern_types.get_or_insert_with(|| target::extern_types(cx));
            self.reported.useless.extend(target::useless(cx, &attr, extern_types));
        }
    }

    /// A macro call of `t`: the macro's use, then what it expands to, entered as the call is.
    fn macro_call(&mut self, t: &'a Tree<'db>, call: ast::MacroCall<'db>, ctx: Ctx) {
        let cx = self.cx;
        let node = call.syntax();
        if let Some(path) = call.path()
            && let Some(mac) = cx.vsema.resolve_macro_call(call)
        {
            let span = InFile::new(t.file, path.syntax().text_range());
            self.check_macro(mac, span, t.ptr(node), ctx.derive, || written(path.syntax()));
        }
        if let Some(expanded) = cx.vsema.expand_macro_call(call)
            && let Some(tree) = self.tree_of(expanded.file_id, t, node)
        {
            let body = in_body(node, ctx);
            self.tree(tree, Ctx { body, ..ctx });
        }
    }

    /// An item of `t` an attribute macro replaces: the macro's use, then what it expands to,
    /// entered as the item is.
    fn attr_macro(&mut self, t: &'a Tree<'db>, node: Node<'db>, ctx: Ctx) {
        let cx = self.cx;
        let Some(item) = ast::Item::cast(node) else { return };
        if let Some(mac) = cx.vsema.resolve_attr_macro_call(item) {
            let name = mac.name(cx.db);
            let name = name.as_str();
            let found = attr::each(node, self.cfg, |meta, _| {
                match meta
                    .path()
                    .filter(|it| it.segment().and_then(|it| it.name_ref()).is_some_and(|it| it.text() == name))
                {
                    Some(path) => ControlFlow::Break(path),
                    None => ControlFlow::Continue(()),
                }
            });
            if let Some(path) = found {
                let span = InFile::new(t.file, path.syntax().text_range());
                self.check_macro(mac, span, t.ptr(node), ctx.derive, || written(path.syntax()));
            }
        }
        if let Some(expanded) = cx.vsema.expand_attr_macro(item)
            && let Some(tree) = self.tree_of(expanded.value.file_id, t, node)
        {
            let body = in_body(node, ctx);
            self.tree(tree, Ctx { body, ..ctx });
        }
    }

    /// The derives of the ADT at `j`: each derive macro, then what each made.
    fn derives(&mut self, t: &'a Tree<'db>, j: usize, ctx: Ctx) {
        if !t.attrs[j] {
            return;
        }
        let node = t.nodes[j];
        let cfg = self.cfg;
        attr::each::<()>(node, cfg, |meta, written| {
            if attr::is(&meta, "derive") {
                self.derive(t, node, meta, written, ctx);
            }
            ControlFlow::Continue(())
        });
    }

    fn derive(&mut self, t: &'a Tree<'db>, node: Node<'db>, meta: ast::Meta<'db>, written: ast::Attr<'db>, ctx: Ctx) {
        let cx = self.cx;
        let macros = cx.vsema.resolve_derive_macro(meta).unwrap_or_default();
        if macros.iter().any(Option::is_some) {
            let ranges = match written.meta() {
                Some(it) if it.syntax() == meta.syntax() => derive_ranges(written),
                _ => SmallVec::new(),
            };
            let at = t.ptr(node);
            for (mac, range) in macros.into_iter().zip(ranges) {
                if let Some(mac) = mac {
                    self.check_macro(mac, InFile::new(t.file, range), at, ctx.derive, || derive_text(written, range));
                }
            }
        }
        let Some(derived) = cx.vsema.expand_derive_macro(meta) else { return };
        for root in derived.into_iter().flatten() {
            let file = cx.vsema.hir_file_for(root.value);
            if file.is_macro()
                && let Some(tree) = self.tree_of(file, t, node)
            {
                self.tree(tree, Ctx { derive: true, body: false, container: Container::Free, ..ctx });
            }
        }
    }

    /// `Checker::visit_item`'s `ineffective_unstable_trait_impl` of the trait impl `imp` at `j`
    /// of `t`, the item-like `owner`. In a crate with `staged_api`, an `#[unstable]` on it has
    /// no effect when everything its self type and trait name is stable
    /// (`CheckTraitImplStable`), unless an `#[unstable_feature_bound]` names the same feature,
    /// and is not linted when the trait does not resolve. Kept before what the walk of the
    /// impl finds: rustc lints it before it visits what is in the impl.
    fn ineffective_unstable(&mut self, t: &'a Tree<'db>, j: usize, imp: ast::Impl<'db>, owner: u32) {
        let cx = self.cx;
        let node = t.nodes[j];
        const INEFFECTIVE: LintId = LintId::named("ineffective_unstable_trait_impl");
        if !self.index.staged(cx.krate) || !cx.lint_may_fire(INEFFECTIVE) {
            return;
        }
        let Some(range) = attr::stability_range(node, self.cfg) else { return };
        let Some(stability) = ItemLike::at(cx, InFile::new(t.file, node)).and_then(|it| it.stability(cx.db)) else {
            return;
        };
        if !stability.is_unstable() || attr::unstable_feature_bound(node, self.cfg, &stability.feature) {
            return;
        }
        let (Some(ast::Type::PathType(trait_ty)), Some(self_ty)) = (imp.trait_(), imp.self_ty()) else { return };
        let Some(trait_path) = trait_ty.path() else { return };
        if !matches!(cx.vsema.resolve_path(trait_path), Some(PathResolution::Def(ModuleDef::Trait(_)))) {
            return;
        }
        let mut header = Header { stable: true, defs: SmallVec::new() };
        for part in [self_ty.syntax(), trait_ty.syntax()] {
            let range = part.text_range();
            let from = t.nodes.partition_point(|it| it.text_range().start() < range.start());
            self.header_nodes(t, from, Some(range), &mut header);
        }
        if !header.stable {
            return;
        }
        let key = self.item_key(owner, 1);
        let found = super::ineffective_unstable_impl(cx, InFile::new(t.file, range), t.ptr(node), &header.defs);
        self.reported.items.push((key, found));
    }

    /// `CheckTraitImplStable` over the nodes of `t` from `from` on that start in `range` (all of
    /// `t` with none), a type of an impl's header or what a macro there expands to. What rustc's
    /// HIR does not have of them is passed over: attributes, a macro call's own path, and the
    /// bodies of anon consts.
    fn header_nodes(&mut self, t: &'a Tree<'db>, from: usize, range: Option<TextRange>, header: &mut Header) {
        let walk = self.walk;
        let mut k = from;
        while header.stable && k < t.nodes.len() {
            let node = t.nodes[k];
            if range.is_some_and(|it| node.text_range().start() >= it.end()) {
                return;
            }
            match t.kinds[k] {
                SyntaxKind::ATTR => {
                    k = after(t, k);
                    continue;
                }
                SyntaxKind::CONST_ARG => {
                    self.const_arg(t, node, header);
                    k = after(t, k);
                    continue;
                }
                SyntaxKind::MACRO_CALL => {
                    if let Some(call) = ast::MacroCall::cast(node)
                        && let Some(expanded) = self.cx.vsema.expand_macro_call(call)
                    {
                        match walk.tree(expanded.file_id) {
                            Some(tree) => self.header_nodes(tree, 0, None, header),
                            // The walk of the impl reports that the crate walk has no tree of it.
                            None => header.stable = false,
                        }
                    }
                    k = after(t, k);
                    continue;
                }
                SyntaxKind::PATH if node.parent().is_none_or(|it| it.kind() != SyntaxKind::PATH) => {
                    if let Some(path) = ast::Path::cast(node) {
                        self.header_path(t, path, None, header);
                    }
                }
                SyntaxKind::FN_PTR_TYPE => {
                    if ast::FnPtrType::cast(node).is_some_and(unstable_abi) {
                        header.stable = false;
                    }
                }
                _ => {}
            }
            k += 1;
        }
    }

    /// `CheckTraitImplStable::visit_path` of `path`, an outermost path of `t`, resolving to `res`
    /// when that is known: the stability of what rustc's HIR path resolves to. A type-relative
    /// path's is that of the type before it; `Self` is the impl, whose stability is the
    /// `#[unstable]` itself; the trait of a `<Type as Trait>` is a segment, not looked up.
    fn header_path(&mut self, t: &Tree<'db>, path: ast::Path<'db>, res: Option<PathResolution<'db>>, header: &mut Header) {
        let cx = self.cx;
        let qualifier = path.qualifier();
        if qualifier.is_none()
            && path.segment().and_then(|it| it.kind()).is_some_and(|it| matches!(it, ast::PathSegmentKind::SelfTypeKw))
        {
            header.stable = false;
            return;
        }
        if is_anchor_trait(path) {
            return;
        }
        let Some(res) = res.or_else(|| cx.vsema.resolve_path(path)) else { return };
        if let Some(qualifier) = qualifier {
            let (relative, qualifier_res) = type_relative(cx, qualifier, &res);
            if relative {
                if !matches!(qualifier.segment().and_then(|it| it.kind()), Some(ast::PathSegmentKind::Type { .. }))
                    && !matches!(qualifier_res, Some(None))
                {
                    self.header_path(t, qualifier, qualifier_res.flatten(), header);
                }
                return;
            }
        }
        let PathResolution::Def(def) = res else { return };
        match def {
            ModuleDef::BuiltinType(_) => return,
            // A generic argument naming a value is a const argument (`lower_generic_arg`).
            ModuleDef::Const(_) | ModuleDef::Static(_) | ModuleDef::Function(_) => {
                return self.const_arg(t, path.syntax(), header);
            }
            _ => {}
        }
        let def = Def::Item(trait_item(cx.db, def));
        if self.index.stability(def).is_some_and(Stability::is_unstable) {
            header.stable = false;
            return;
        }
        header.defs.push(def);
    }

    /// A const argument at `node` of `t` in an impl's header. With no `gca_macroless_args`, a
    /// path in it is lowered to a path only when it names a const parameter (no stability), and
    /// otherwise to an anon const whose body `CheckTraitImplStable` does not enter: nothing in
    /// it counts. With that feature some are lowered to paths rustc then checks; blaze does not
    /// tell which, so a const argument with a path in it is an error.
    fn const_arg(&mut self, t: &Tree<'_>, node: Node<'_>, header: &mut Header) {
        let cx = self.cx;
        if !node.descendants().any(|it| it.kind() == SyntaxKind::PATH)
            || !cx.krate.is_unstable_feature_enabled(cx.db, &Symbol::intern("gca_macroless_args"))
        {
            return;
        }
        header.stable = false;
        let span = cx.span(InFile::new(t.file, node.text_range()));
        let diag = Diag::error(
            None,
            "blaze can't tell whether the `#[unstable]` of this impl has an effect: under `gca_macroless_args` rustc checks the stability of some paths in a const argument, and blaze does not",
            span,
        );
        self.reported.missing.push(Found::new(diag, Phase::Items));
    }

    /// `check_stability_and_deprecation` of the macro `mac` used at `span`: its own
    /// stability, checked in every crate, then its own deprecation, outside derives. `path`
    /// prints the path as written (`pprust::path_to_string`), only for what is reported.
    fn check_macro(
        &mut self,
        mac: hir::Macro,
        span: InFile<TextRange>,
        at: InFile<SyntaxNodePtr>,
        derive: bool,
        path: impl Fn() -> String,
    ) {
        let cx = self.cx;
        let site = Site { at, span, phase: Phase::Resolve };
        let mut text = None;
        if let Some(stability) = self.index.macro_stability(mac)
            && self.index.macro_denied(stability, span.file_id, span.value)
        {
            let path: &str = text.get_or_insert_with(&path);
            let def = Def::Item(ModuleDef::Macro(mac));
            let found = super::unstable(cx, def, Some(path), stability, site);
            self.reported.macro_unstable.extend(found);
        }
        if !derive && let Some(depr) = self.index.macro_deprecation(mac) {
            match super::in_effect(depr, self.index.current) {
                Some(in_effect) => {
                    let path: &str = text.get_or_insert_with(&path);
                    let found = super::deprecated_macro(cx, mac, depr, in_effect, path, site);
                    self.reported.macro_deprecated.push(found);
                }
                None => self.reported.missing.push(Found::new(super::unknown_current(cx, span), Phase::Resolve)),
            }
        }
    }

    fn uses_at(&mut self, file: HirFileId, value: Node<'db>, ctx: Ctx) {
        let cx = self.cx;
        let db = cx.db;
        match value.kind() {
            SyntaxKind::PATH => {
                if value.parent().is_some_and(|it| matches!(it.kind(), SyntaxKind::PATH | SyntaxKind::USE_TREE)) {
                    return;
                }
                if let Some(path) = ast::Path::cast(value) {
                    let anchor_trait = is_anchor_trait(path);
                    self.path(file, path, ctx, anchor_trait, None);
                }
            }
            SyntaxKind::USE_TREE => {
                if let Some(tree) = ast::UseTree::cast(value) {
                    self.use_tree(file, tree, ctx);
                }
            }
            SyntaxKind::METHOD_CALL_EXPR => {
                if let Some(call) = ast::MethodCallExpr::cast(value) {
                    self.method_call(file, call, ctx);
                }
            }
            SyntaxKind::FIELD_EXPR => {
                if let Some(field) = ast::FieldExpr::cast(value) {
                    self.field(file, field, ctx);
                }
            }
            SyntaxKind::RECORD_EXPR_FIELD => {
                let Some(field) = ast::RecordExprField::cast(value) else { return };
                let Some((def, _, _)) = cx.vsema.resolve_record_field(field) else { return };
                if matches!(def.parent_def(db), hir::Variant::EnumVariant(_)) {
                    return;
                }
                self.field_use(file, value, Def::Field(def), ctx);
            }
            SyntaxKind::RECORD_PAT_FIELD => {
                let Some(field) = ast::RecordPatField::cast(value) else { return };
                let Some((def, _)) = cx.vsema.resolve_record_pat_field(field) else { return };
                self.field_use(file, value, Def::Field(def), ctx);
            }
            SyntaxKind::TUPLE_STRUCT_PAT => {
                if let Some(pat) = ast::TupleStructPat::cast(value) {
                    self.tuple_struct_pat(file, pat, ctx);
                }
            }
            SyntaxKind::ASSOC_TYPE_ARG => {
                if let Some(arg) = ast::AssocTypeArg::cast(value) {
                    self.constraint(file, arg, ctx);
                }
            }
            _ => {}
        }
    }

    /// `check_stability_allow_unstable` of `used`: its deprecation (but in a definition
    /// deprecated by the same attribute), then its stability. Whether it was allowed.
    fn check(&mut self, used: &Use) -> bool {
        let cx = self.cx;
        if !used.stability_only
            && !used.derive
            && let Some(entry) = self.index.deprecation(used.def)
        {
            let mut report = matches!(entry.deprecation.since, DeprecatedSince::RustcVersion(_));
            if !report {
                let parent = self.owner_deprecation(used.owner);
                report = !parent.is_some_and(|it| entry.same_origin(&it));
            }
            if report {
                let suggestion = used.expr.then(|| InFile::new(used.at.file_id, used.span));
                let current = self.index.current;
                if let Some(found) =
                    super::deprecated(cx, used.def, entry.deprecation, current, used.site(used.span), suggestion)
                {
                    self.emit(used, found);
                }
            }
        }
        let Some(stability) = self.unstable(used) else { return true };
        if let Some(found) = super::unstable(cx, used.def, None, stability, used.site(used.whole)) {
            self.emit(used, found);
        }
        false
    }

    /// `lookup_deprecation_entry` of the item-like `owner`, as `hir_get_parent_item` of a use.
    fn owner_deprecation(&mut self, owner: u32) -> Option<DeprecationEntry<'db>> {
        let node = match owner {
            ROOT => self.root,
            it => self.owners[it as usize].node,
        };
        self.index.local_deprecation(node)
    }

    /// The stability of what `used` names when `eval_stability_allow_unstable` denies it: a
    /// public definition of a dependency compiled with `#![feature(staged_api)]`, unstable,
    /// its feature not enabled nor allowed where it is used, nor the `use` it is in an
    /// unstable re-export.
    fn unstable(&mut self, used: &Use) -> Option<&'db Stability> {
        let stability = self.index.foreign_stability(used.def)?;
        if !self.index.denied(stability, used.at.file_id, used.whole) || self.is_unstable_reexport(used.owner) {
            return None;
        }
        Some(stability)
    }

    /// `is_unstable_reexport`: the item-like is a `use` that is itself unstable.
    fn is_unstable_reexport(&mut self, owner: u32) -> bool {
        if owner == ROOT || !self.owners[owner as usize].is_use || !self.index.staged(self.cx.krate) {
            return false;
        }
        self.index.local_stability(self.owners[owner as usize].node).is_some_and(Stability::is_unstable)
    }

    /// Whether a use of `def` can be reported: it is deprecated or has a stability rustc
    /// checks.
    fn may_report(&mut self, def: Def) -> bool {
        self.index.deprecation(def).is_some() || self.index.foreign_stability(def).is_some()
    }

    /// A path [`Self::path`] could not resolve. Name resolution finding nothing is rustc's
    /// `Res::Err`: an error of its own (E0412, E0425, E0433), and `Checker::visit_path` has no
    /// definition to check either. Otherwise blaze knows what the path names but not here: no
    /// item or body holds it, or resolution finds it but not at this place. That is an error
    /// at the path, as its use goes unchecked.
    fn unresolved(&mut self, file: HirFileId, path: ast::Path<'db>) {
        let cx = self.cx;
        let why = match cx.vsema.scope(path.syntax()) {
            None => "blaze finds no item or body it is in",
            Some(scope) if scope.speculative_resolve_view(path).is_some() => {
                "name resolution finds it, but blaze can't resolve it at this place"
            }
            Some(_) => return,
        };
        let span = cx.span(InFile::new(file, path.syntax().text_range()));
        // alloc: on the error path, for the message
        let message = format!("blaze can't check the stability of `{}`: {why}", written(path.syntax()));
        self.reported.missing.push(Found::new(Diag::error(None, message, span), Phase::Items));
    }

    /// A path as rustc's HIR has it: resolved by name resolution (`Checker::visit_path`, the
    /// parent segments for stability alone), or type-relative (typeck's or `hir_ty_lowering`'s
    /// check of what it resolves to, the type before it a path of its own). `res` is what it
    /// resolves to, when that is known.
    fn path(
        &mut self,
        file: HirFileId,
        path: ast::Path<'db>,
        ctx: Ctx,
        stability_only: bool,
        res: Option<PathResolution<'db>>,
    ) {
        let cx = self.cx;
        let db = cx.db;
        let qualifier = path.qualifier();
        if qualifier.is_none()
            && path.segment().and_then(|it| it.kind()).is_some_and(|it| matches!(it, ast::PathSegmentKind::SelfTypeKw))
        {
            return;
        }
        let Some(res) = res.or_else(|| cx.vsema.resolve_path(path)) else {
            self.unresolved(file, path);
            return;
        };
        let (relative, qualifier_res) = match qualifier {
            Some(qualifier) => type_relative(cx, qualifier, &res),
            None => (false, None),
        };
        let parent = path.syntax().parent();
        let parent_kind = parent.map(|it| it.kind());
        let def = match res {
            PathResolution::Def(ModuleDef::BuiltinType(_)) => None,
            PathResolution::Def(def) => {
                let def = trait_item(db, def);
                let value_ns = matches!(
                    parent_kind,
                    Some(SyntaxKind::PATH_EXPR | SyntaxKind::PATH_PAT | SyntaxKind::TUPLE_STRUCT_PAT)
                );
                Some(if value_ns { Def::value(db, def) } else { Def::Item(def) })
            }
            _ => None,
        };
        let main = match def {
            Some(def) => {
                let whole = path.syntax().text_range();
                let last = path.segment().and_then(|it| it.name_ref()).map_or(whole, |it| it.syntax().text_range());
                let at = parent.unwrap_or(path.syntax());
                let used = Use {
                    def,
                    owner: ctx.owner,
                    at: InFile::new(file, ast::ptr(at)),
                    span: if relative && parent_kind == Some(SyntaxKind::PATH_TYPE) { whole } else { last },
                    whole,
                    phase: if relative && in_body(path.syntax(), ctx) { Phase::Bodies } else { Phase::Items },
                    expr: matches!(parent_kind, Some(SyntaxKind::PATH_EXPR | SyntaxKind::RECORD_EXPR)),
                    stability_only,
                    derive: ctx.derive,
                };
                Some((used, self.check(&used)))
            }
            None => None,
        };
        let Some(qualifier) = qualifier else { return };
        if relative {
            if !matches!(qualifier.segment().and_then(|it| it.kind()), Some(ast::PathSegmentKind::Type { .. }))
                && !matches!(qualifier_res, Some(None))
            {
                self.path(file, qualifier, ctx, stability_only, qualifier_res.flatten());
            }
            return;
        }
        let Some((main, true)) = main else { return };
        if self.index.is_local(main.def) {
            return;
        }
        let through = self.index.through(main.def);
        let mut resolved = qualifier_res;
        let mut segment = Some(qualifier);
        while let Some(qualifier) = segment {
            let res = resolved.take().unwrap_or_else(|| cx.vsema.resolve_path(qualifier));
            self.segment(qualifier, res, &main, through, ctx);
            segment = qualifier.qualifier();
        }
    }

    /// The check of the parent segment `qualifier`, resolving to `res`, of the path checked
    /// as `main`, which was allowed: an error when the segment is unstable, or for an item
    /// allowed through unstable modules (`through`), its deprecation with the item's message.
    fn segment(
        &mut self,
        qualifier: ast::Path<'db>,
        res: Option<PathResolution<'db>>,
        main: &Use,
        through: Option<&'db Symbol>,
        ctx: Ctx,
    ) {
        let Some(PathResolution::Def(def)) = res else { return };
        if matches!(def, ModuleDef::BuiltinType(_)) {
            return;
        }
        let segment = qualifier.segment();
        let name = segment.as_ref().and_then(|it| it.name_ref()).map(|it| it.syntax().text_range());
        let range = name.or(segment.map(|it| it.syntax().text_range())).unwrap_or(qualifier.syntax().text_range());
        let used = Use {
            def: Def::Item(def),
            owner: ctx.owner,
            at: main.at,
            span: range,
            whole: range,
            phase: Phase::Items,
            expr: false,
            stability_only: true,
            derive: ctx.derive,
        };
        let Some(stability) = self.unstable(&used) else { return };
        let cx = self.cx;
        let found = match through {
            Some(message) => super::deprecated_through(cx, used.def, message, self.index.current, main.site(main.span)),
            None => super::unstable(cx, used.def, None, stability, used.site(range)),
        };
        if let Some(found) = found {
            self.emit(&used, found);
        }
    }

    /// A use tree as `lower_use_tree` makes items of it: a single import checked in each
    /// namespace it imports in (a constructor and its struct once), a glob by what it imports
    /// from, a list by nothing unless it is empty.
    fn use_tree(&mut self, file: HirFileId, tree: ast::UseTree<'db>, ctx: Ctx) {
        let cx = self.cx;
        let db = cx.db;
        let Some(path) = tree.path() else { return };
        if let Some(list) = tree.use_tree_list() {
            let root_only = path.qualifier().is_none()
                && path.segment().is_some_and(|it| it.coloncolon_token().is_some() && it.name_ref().is_none());
            if list.use_trees().next().is_none() && !root_only {
                self.use_path(file, tree, path, None, ctx);
            }
            return;
        }
        if tree.star_token().is_some() {
            if let Some(PathResolution::Def(def)) = cx.vsema.resolve_path(path) {
                self.use_path(file, tree, path, Some(Def::Item(def)), ctx);
            }
            return;
        }
        let Some(per_ns) = cx.vsema.resolve_path_per_ns(path) else { return };
        let def_of = |it: Option<PathResolution<'_>>| match it {
            Some(PathResolution::Def(def)) if !matches!(def, ModuleDef::BuiltinType(_)) => Some(def),
            _ => None,
        };
        let (types, values, macros) = (def_of(per_ns.type_ns), def_of(per_ns.value_ns), def_of(per_ns.macro_ns));
        let mut defs: SmallVec<[Def; 3]> = SmallVec::new();
        match (types, values) {
            (Some(ty), Some(val)) if ty == val && matches!(Def::value(db, val), Def::Ctor(_)) => {
                defs.push(Def::value(db, val));
            }
            _ => {
                defs.extend(types.map(Def::Item));
                defs.extend(values.map(|it| Def::value(db, it)));
            }
        }
        defs.extend(macros.map(Def::Item));
        for def in defs {
            self.use_path(file, tree, path, Some(def), ctx);
        }
    }

    /// The check of a `use` path resolving to `def` (when it is checked), and of its parent
    /// segments, those of the trees it is nested in after its own.
    fn use_path(&mut self, file: HirFileId, tree: ast::UseTree<'db>, path: ast::Path<'db>, def: Option<Def>, ctx: Ctx) {
        let cx = self.cx;
        let def = match def {
            Some(def) => def,
            None => match cx.vsema.resolve_path(path) {
                Some(PathResolution::Def(def)) => Def::Item(def),
                _ => return,
            },
        };
        let whole = path.syntax().text_range();
        let segment = path.segment();
        let name = segment.as_ref().and_then(|it| it.name_ref()).map(|it| it.syntax().text_range());
        let last = name.or(segment.map(|it| it.syntax().text_range())).unwrap_or(whole);
        let main = Use {
            def: trait_item_def(cx, def),
            owner: ctx.owner,
            at: InFile::new(file, ast::ptr(tree.syntax())),
            span: last,
            whole,
            phase: Phase::Items,
            expr: false,
            stability_only: false,
            derive: ctx.derive,
        };
        if !self.check(&main) || self.index.is_local(main.def) {
            return;
        }
        let through = self.index.through(main.def);
        let outer = tree
            .syntax()
            .ancestors()
            .skip(1)
            .take_while(|it| it.kind() != SyntaxKind::USE)
            .filter(|it| it.kind() == SyntaxKind::USE_TREE)
            .filter_map(ast::UseTree::cast)
            .filter_map(|it| it.path());
        for start in path.qualifier().into_iter().chain(outer) {
            let mut segment = Some(start);
            while let Some(qualifier) = segment {
                let res = cx.vsema.resolve_path(qualifier);
                self.segment(qualifier, res, &main, through, ctx);
                segment = qualifier.qualifier();
            }
        }
    }

    /// `lookup_method`'s check of the method picked, at its name.
    fn method_call(&mut self, file: HirFileId, call: ast::MethodCallExpr<'db>, ctx: Ctx) {
        let cx = self.cx;
        let Some(name) = call.name_ref() else { return };
        let Some(function) = cx.vsema.resolve_method_call(call) else { return };
        let range = name.syntax().text_range();
        let used = Use {
            def: Def::Item(trait_item(cx.db, ModuleDef::Function(function))),
            owner: ctx.owner,
            at: InFile::new(file, ast::ptr(call.syntax())),
            span: range,
            whole: range,
            phase: Phase::Bodies,
            expr: true,
            stability_only: false,
            derive: ctx.derive,
        };
        self.check(&used);
    }

    /// `check_expr_field`'s check of a field it can access, at the whole expression.
    fn field(&mut self, file: HirFileId, field: ast::FieldExpr<'db>, ctx: Ctx) {
        let cx = self.cx;
        let Some(Either::Left(def)) = cx.vsema.resolve_field(field) else { return };
        if !self.may_report(Def::Field(def)) {
            return;
        }
        let Some(scope) = cx.vsema.scope(field.syntax()) else { return };
        if !def.is_visible_from(cx.db, scope.module()) {
            return;
        }
        let range = field.syntax().text_range();
        let used = Use {
            def: Def::Field(def),
            owner: ctx.owner,
            at: InFile::new(file, ast::ptr(field.syntax())),
            span: range,
            whole: range,
            phase: Phase::Bodies,
            expr: true,
            stability_only: false,
            derive: ctx.derive,
        };
        self.check(&used);
    }

    /// The check of a struct field named in an expression or a pattern, at `node`.
    fn field_use(&mut self, file: HirFileId, node: Node<'_>, def: Def, ctx: Ctx) {
        let range = node.text_range();
        let used = Use {
            def,
            owner: ctx.owner,
            at: InFile::new(file, ast::ptr(node)),
            span: range,
            whole: range,
            phase: Phase::Bodies,
            expr: false,
            stability_only: false,
            derive: ctx.derive,
        };
        self.check(&used);
    }

    /// `check_pat_tuple_struct`'s check of the field each subpattern is of
    /// (`enumerate_and_adjust` past a `..`), when their number fits.
    fn tuple_struct_pat(&mut self, file: HirFileId, pat: ast::TupleStructPat<'db>, ctx: Ctx) {
        let cx = self.cx;
        let Some(fields) = cx.vsema.resolve_tuple_struct_pat_fields(pat) else { return };
        // alloc: inline up to 8 subpatterns
        let subpats: SmallVec<[ast::Pat<'db>; 8]> = pat.fields().collect();
        let rest = subpats.iter().position(|it| matches!(it, ast::Pat::RestPat(_)));
        let count = subpats.len() - usize::from(rest.is_some());
        if count > fields.len() || (count < fields.len() && rest.is_none()) {
            return;
        }
        for (index, subpat) in subpats.iter().enumerate() {
            let field = match rest {
                Some(rest) if index == rest => continue,
                Some(rest) if index > rest => index - 1 + (fields.len() - count),
                _ => index,
            };
            self.field_use(file, subpat.syntax(), Def::Field(fields[field].0), ctx);
        }
    }

    /// `lower_assoc_item_constraint`'s check of the associated type a constraint names, at
    /// the whole constraint.
    fn constraint(&mut self, file: HirFileId, arg: ast::AssocTypeArg<'db>, ctx: Ctx) {
        let cx = self.cx;
        let db = cx.db;
        let Some(name) = arg.name_ref() else { return };
        let Some(path) =
            arg.syntax().ancestors().skip(1).find(|it| it.kind() == SyntaxKind::PATH).and_then(ast::Path::cast)
        else {
            return;
        };
        let Some(PathResolution::Def(ModuleDef::Trait(trait_))) = cx.vsema.resolve_path(path) else { return };
        let name = name.as_name();
        let found = trait_.items_with_supertraits(db).into_iter().find_map(|it| match it {
            hir::AssocItem::TypeAlias(it) if it.name(db) == name => Some(it),
            _ => None,
        });
        let Some(alias) = found else { return };
        let range = arg.syntax().text_range();
        let used = Use {
            def: Def::Item(ModuleDef::TypeAlias(alias)),
            owner: ctx.owner,
            at: InFile::new(file, ast::ptr(arg.syntax())),
            span: range,
            whole: range,
            phase: if in_body(arg.syntax(), ctx) { Phase::Bodies } else { Phase::Items },
            expr: false,
            stability_only: false,
            derive: ctx.derive,
        };
        self.check(&used);
    }
}

/// Whether the walk looks at a node of `kind`: an item-like or a list of them (what the nodes
/// in it are in), a node that holds a use, one a `#[deprecated]` is useless on, a `let` (whose
/// children typeck takes out of order), or what rustc's checks don't see in (an attribute, a
/// macro call's path).
fn looked_at(kind: SyntaxKind) -> bool {
    ast::Item::can_cast(kind)
        || matches!(
            kind,
            SyntaxKind::ATTR
                | SyntaxKind::ASSOC_ITEM_LIST
                | SyntaxKind::EXTERN_ITEM_LIST
                | SyntaxKind::LET_STMT
                | SyntaxKind::PATH
                | SyntaxKind::USE_TREE
                | SyntaxKind::METHOD_CALL_EXPR
                | SyntaxKind::FIELD_EXPR
                | SyntaxKind::RECORD_EXPR_FIELD
                | SyntaxKind::RECORD_PAT_FIELD
                | SyntaxKind::TUPLE_STRUCT_PAT
                | SyntaxKind::ASSOC_TYPE_ARG
                | SyntaxKind::PARAM
                | SyntaxKind::SELF_PARAM
                | SyntaxKind::TYPE_PARAM
                | SyntaxKind::LIFETIME_PARAM
                | SyntaxKind::CONST_PARAM
        )
}

/// The index of the first node of `t` from `from` on that the walk looks at, or `t`'s end.
fn next(t: &Tree<'_>, from: usize) -> usize {
    let found = t.kinds.get(from..).and_then(|it| it.iter().position(|&kind| looked_at(kind)));
    found.map_or(t.kinds.len(), |at| from + at)
}

/// The index of the first node of `t` after the one at `k` that is not in it.
fn after(t: &Tree<'_>, k: usize) -> usize {
    let end = t.nodes[k].text_range().end();
    let mut at = k + 1;
    while at < t.nodes.len() && t.nodes[at].text_range().start() < end {
        at += 1;
    }
    at
}

/// Whether `extern_abi_stability` refuses the ABI of the function pointer type `ty`. None
/// written is `Rust`, a bare `extern` is `C`, and one rustc does not know is an error of its
/// own and lowers to `Rust` (`lower_extern`, `lower_abi`): all stable.
fn unstable_abi(ty: ast::FnPtrType<'_>) -> bool {
    let Some(abi) = ty.abi().and_then(|it| it.abi_string()) else { return false };
    let Ok(name) = abi.value() else { return false };
    matches!(
        &*name,
        "llvm-intrinsic"
            | "vectorcall"
            | "vectorcall-unwind"
            | "rust-call"
            | "rust-cold"
            | "rust-preserve-none"
            | "tail"
            | "rust-invalid"
            | "gpu-kernel"
            | "ptx-kernel"
            | "msp430-interrupt"
            | "x86-interrupt"
            | "avr-interrupt"
            | "avr-non-blocking-interrupt"
            | "riscv-interrupt-m"
            | "riscv-interrupt-s"
            | "cmse-nonsecure-call"
            | "cmse-nonsecure-entry"
            | "Swift"
    )
}

/// The index of the first node looked at from `from` on that is not in `node`.
fn past(t: &Tree<'_>, from: usize, node: Node<'_>) -> usize {
    let range = node.text_range();
    let mut at = next(t, from);
    while at < t.nodes.len() && inside(node, range, t.nodes[at]) {
        at = next(t, at + 1);
    }
    at
}

/// Whether `node`, which is not before `parent` (whose range is `outer`) in pre-order, is in
/// it: it starts in it, or is empty where `parent` ends and has it above.
fn inside<'t>(parent: Node<'t>, outer: TextRange, node: Node<'t>) -> bool {
    let it = node.text_range();
    outer.start() <= it.start()
        && (it.start() < outer.end()
            || (it.is_empty() && it.start() == outer.end() && node.ancestors().any(|it| it == parent)))
}

/// The items of `t` an attribute macro replaces that are in `parent` (all of them with none),
/// in pre-order.
fn replaced_in<'t, 'db>(t: &'t Tree<'db>, parent: Option<Node<'_>>) -> &'t [Node<'db>] {
    let all = &t.replaced[..];
    let Some(parent) = parent.filter(|_| !all.is_empty()) else { return all };
    let range = parent.text_range();
    let from = all.partition_point(|it| it.text_range().start() < range.start());
    let to = all.partition_point(|it| it.text_range().start() < range.end());
    &all[from..to]
}

/// Whether `node`, entered with `ctx`, is in a body (rustc's typeck lowers it): below an
/// expression or a pattern with no item between, or with no item above it in a tree entered
/// in a body.
fn in_body(node: Node<'_>, ctx: Ctx) -> bool {
    for it in node.ancestors().skip(1) {
        let kind = it.kind();
        if ast::Item::can_cast(kind) {
            return false;
        }
        if ast::Expr::can_cast(kind) || ast::Pat::can_cast(kind) {
            return true;
        }
    }
    ctx.body
}

/// `def`, an item of a trait impl taken as its trait's.
fn trait_item_def(cx: &Cx<'_, '_>, def: Def) -> Def {
    match def {
        Def::Item(it) => Def::Item(trait_item(cx.db, it)),
        it => it,
    }
}

/// Whether rustc resolves the path `qualifier` qualifies, resolving to `res`, in typeck
/// rather than in name resolution: `qualifier` is a type (`Type::item`, `<Type>::item`,
/// `Self::item`, `T::Assoc`, an enum's associated item), not a module, a trait or
/// `<Type as Trait>`. With what `qualifier` resolves to, when it was resolved to tell.
fn type_relative<'db>(
    cx: &Cx<'_, 'db>,
    qualifier: ast::Path<'db>,
    res: &PathResolution<'db>,
) -> (bool, Option<Option<PathResolution<'db>>>) {
    match qualifier.segment().and_then(|it| it.kind()) {
        Some(ast::PathSegmentKind::Type { trait_ref, .. }) => (trait_ref.is_none(), None),
        Some(ast::PathSegmentKind::SelfTypeKw) if qualifier.qualifier().is_none() => (true, None),
        _ => {
            let resolved = cx.vsema.resolve_path(qualifier);
            let relative = match resolved {
                Some(PathResolution::Def(ModuleDef::Module(_) | ModuleDef::Trait(_))) => false,
                Some(PathResolution::Def(ModuleDef::Adt(Adt::Enum(_)))) => {
                    !matches!(res, PathResolution::Def(ModuleDef::EnumVariant(_)))
                }
                Some(PathResolution::Def(
                    ModuleDef::Adt(_) | ModuleDef::TypeAlias(_) | ModuleDef::BuiltinType(_),
                ))
                | Some(PathResolution::SelfType(_) | PathResolution::TypeParam(_)) => true,
                _ => false,
            };
            (relative, Some(resolved))
        }
    }
}

/// Whether `path` is the trait of a `<Type as Trait>` qualifier: a parent segment of the
/// path it qualifies.
fn is_anchor_trait(path: ast::Path<'_>) -> bool {
    let Some(ty) = path.syntax().parent().and_then(ast::PathType::cast) else { return false };
    let Some(segment) = ty.syntax().ancestors().skip(1).take(2).find_map(ast::PathSegment::cast) else {
        return false;
    };
    matches!(segment.kind(), Some(ast::PathSegmentKind::Type { trait_ref: Some(it), .. }) if it == ty)
}

/// A path as `pprust::path_to_string` prints it: its tokens with no space.
fn written(node: Node<'_>) -> String {
    // alloc: only for a use that is reported
    let mut text = String::new();
    for token in node.descendants_with_tokens().filter_map(NodeOrToken::into_token) {
        if !token.kind().is_trivia() {
            text.push_str(token.text());
        }
    }
    text
}

/// Where each path of a `#[derive(...)]` is: `crate::cx::derive_paths_view` without the text.
fn derive_ranges(attr: ast::Attr<'_>) -> SmallVec<[TextRange; 4]> {
    let mut ranges = SmallVec::new();
    let Some((_, tt)) = attr.as_simple_call() else { return ranges };
    let mut current: Option<TextRange> = None;
    for token in tt.syntax().children_with_tokens().filter_map(NodeOrToken::into_token) {
        match token.kind() {
            SyntaxKind::IDENT | SyntaxKind::COLON2 | SyntaxKind::COLON => {
                let range = token.text_range();
                current = Some(current.map_or(range, |it| it.cover(range)));
            }
            SyntaxKind::COMMA | SyntaxKind::R_PAREN => ranges.extend(current.take()),
            _ => {}
        }
    }
    ranges.extend(current);
    ranges
}

/// The path of the derive at `range` in `attr`, as `crate::cx::derive_paths_view` prints it.
fn derive_text(attr: ast::Attr<'_>, range: TextRange) -> String {
    // alloc: only for a use that is reported
    let mut text = String::new();
    let Some((_, tt)) = attr.as_simple_call() else { return text };
    for token in tt.syntax().children_with_tokens().filter_map(NodeOrToken::into_token) {
        if matches!(token.kind(), SyntaxKind::IDENT | SyntaxKind::COLON2 | SyntaxKind::COLON)
            && range.contains_range(token.text_range())
        {
            text.push_str(token.text());
        }
    }
    text
}

/// rustc's `Target` of an item written in `container`.
fn item_target(item: ast::Item<'_>, container: Container) -> Option<Target> {
    Some(match item {
        ast::Item::Const(_) => match container {
            Container::Free | Container::Foreign => Target::Const,
            _ => Target::AssocConst,
        },
        ast::Item::Enum(_) => Target::Enum,
        ast::Item::ExternBlock(_) => Target::ForeignMod,
        ast::Item::ExternCrate(_) => Target::ExternCrate,
        ast::Item::Fn(it) => match container {
            Container::Free => Target::Fn,
            Container::Foreign => Target::ForeignFn,
            Container::Impl { of_trait: false } => Target::Method(MethodKind::Inherent),
            Container::Impl { of_trait: true } => Target::Method(MethodKind::TraitImpl),
            Container::Trait => Target::Method(MethodKind::Trait { body: it.body().is_some() }),
        },
        ast::Item::Impl(it) => Target::Impl { of_trait: it.trait_().is_some() },
        ast::Item::MacroCall(_) => Target::MacroCall,
        ast::Item::MacroRules(_) | ast::Item::MacroDef(_) => Target::MacroDef,
        ast::Item::Module(_) => Target::Mod,
        ast::Item::Static(_) => match container {
            Container::Foreign => Target::ForeignStatic,
            _ => Target::Static,
        },
        ast::Item::Struct(_) => Target::Struct,
        ast::Item::Trait(_) => Target::Trait,
        ast::Item::TypeAlias(_) => match container {
            Container::Free => Target::TyAlias,
            Container::Foreign => Target::ForeignTy,
            _ => Target::AssocTy,
        },
        ast::Item::Union(_) => Target::Union,
        ast::Item::Use(_) => Target::Use,
        _ => return None,
    })
}

/// rustc's `Target` of a node of kind `kind` that is no item, where a `#[deprecated]` on it is
/// `useless_deprecated`: parameters, generic parameters, and the fields of struct patterns and
/// expressions. On a variant or a field it is allowed, on a where-predicate an error of the
/// attribute checks, and on a closure, an expression, a statement or an arm the
/// `unused_attributes` of `lints::syntax`.
fn param_target(kind: SyntaxKind) -> Option<Target> {
    Some(match kind {
        SyntaxKind::PARAM | SyntaxKind::SELF_PARAM => Target::Param,
        SyntaxKind::TYPE_PARAM => Target::GenericParam(GenericParamKind::Type),
        SyntaxKind::LIFETIME_PARAM => Target::GenericParam(GenericParamKind::Lifetime),
        SyntaxKind::CONST_PARAM => Target::GenericParam(GenericParamKind::Const),
        SyntaxKind::RECORD_PAT_FIELD => Target::PatField,
        SyntaxKind::RECORD_EXPR_FIELD => Target::ExprField,
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
