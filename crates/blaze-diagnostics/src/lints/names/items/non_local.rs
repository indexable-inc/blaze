//! `non_local_definitions`: an `impl` in a body whose self type and trait are all from outside
//! it, and a `#[macro_export]` `macro_rules!` in a body.
//!
//! An item's parent is the definition rustc's def collector gives it: the innermost function,
//! constant, static, closure, `async` block or constant expression (array length, repeat
//! count, discriminant, braced const argument, inline `const`) around it, an `async fn`'s or
//! `async` closure's body being a closure of its own; then the item, trait, impl or module
//! around it. The bodies an item is in are counted up to its module: rustc's late pass runs
//! per module, so an item of a module in a body is in none.
//!
//! Not done: rustc's note naming an external crate's macro to update with `cargo update`, which
//! it gives only when run by cargo (`was_invoked_from_cargo`), which `after_abort` is not told;
//! and the doc-test help (`UNSTABLE_RUSTDOC_TEST_PATH`), as blaze runs no doc-tests.
//!
//! Follows `compiler/rustc_lint/src/non_local_def.rs`, `compiler/rustc_lint/src/lints.rs`
//! (`NonLocalDefinitionsDiag`), `compiler/rustc_resolve/src/def_collector.rs` (the parents)
//! and `compiler/rustc_middle/src/ty/util.rs` (`def_kind_descr`) (MIT or Apache-2.0).

use blaze_items::Fact;
use hir::{InFile, ModuleDef, PathResolution, view::HasSourceView};
use smallvec::SmallVec;
use syntax::{
    SyntaxKind, TextRange,
    view::{self as ast, AstNode, HasGenericArgs, HasName},
};

use super::walk::{self, Item};
use crate::{
    cx::{Cx, closure_decl_range_view, item_range_view},
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

/// A definition around an item, as rustc's `DefKind` of the item's parent says it.
#[derive(Clone, Copy, Debug)]
struct Owner<'db> {
    /// Where it is, and whether it is the closure an `async fn` or `async` closure's body is.
    node: InFile<ast::Node<'db>>,
    inner: bool,
    kind: Kind,
    /// Whether the item is in its body (the item is in it as one of rustc's `check_body`).
    body: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// A module: a module item or a file.
    Module,
    /// A constant named `_`.
    AnonConst,
    Body,
    /// An item, trait, impl, enum, variant or extern block that has no body.
    Other,
}

impl Owner<'_> {
    fn key(&self) -> (Option<hir::HirFileId>, TextRange, bool) {
        // Every file is the crate root to `peel`: modules are peeled up to it.
        match self.node.value.kind() {
            SyntaxKind::SOURCE_FILE => (None, TextRange::default(), false),
            _ => (Some(self.node.file_id), self.node.value.text_range(), self.inner),
        }
    }

    fn is_root(&self) -> bool {
        self.node.value.kind() == SyntaxKind::SOURCE_FILE
    }

    /// rustc's `def_kind_descr`, read only for a lint that fires, as are `name` and `span`.
    fn descr(&self) -> &'static str {
        let node = self.node.value;
        if self.inner {
            return match node.kind() {
                SyntaxKind::FN => "async fn",
                _ => "async closure",
            };
        }
        let in_assoc = || node.parent().and_then(|it| it.parent()).is_some_and(|it| matches!(it.kind(), SyntaxKind::IMPL | SyntaxKind::TRAIT));
        match node.kind() {
            SyntaxKind::FN if in_assoc() => {
                let method = ast::Fn::cast(node).and_then(|it| it.param_list()).is_some_and(|it| it.self_param().is_some());
                if method { "method" } else { "associated function" }
            }
            SyntaxKind::FN => "function",
            SyntaxKind::CONST if in_assoc() => "associated constant",
            SyntaxKind::CONST => "constant",
            SyntaxKind::STATIC => "static",
            SyntaxKind::CLOSURE_EXPR => "closure",
            SyntaxKind::BLOCK_EXPR => {
                let block = ast::BlockExpr::cast(node);
                let coroutine = block.filter(|it| it.const_token().is_none()).map(|it| (it.async_token().is_some(), it.gen_token().is_some()));
                match coroutine {
                    Some((true, true)) => "async gen block",
                    Some((false, true)) => "gen block",
                    Some((true, false)) => "async block",
                    _ => "constant expression",
                }
            }
            // A const argument, an array length or a repeat count.
            _ => "constant expression",
        }
    }

    /// rustc's `opt_item_name`, as the lint writes it.
    fn name(&self) -> String {
        let named = match self.node.value.kind() {
            _ if self.inner => None,
            SyntaxKind::FN => ast::Fn::cast(self.node.value).and_then(|it| it.name()),
            SyntaxKind::CONST => {
                let it = ast::Const::cast(self.node.value);
                if it.is_some_and(|it| it.underscore_token().is_some()) {
                    return "_".to_owned(); // alloc: the name in a lint that fires
                }
                it.and_then(|it| it.name())
            }
            SyntaxKind::STATIC => ast::Static::cast(self.node.value).and_then(|it| it.name()),
            _ => None,
        };
        named.map_or_else(|| "<unnameable>".to_owned(), |it| it.text().to_owned()) // alloc: the name in a lint that fires
    }

    /// rustc's `def_span`.
    fn span(&self) -> TextRange {
        let node = self.node.value;
        if self.inner {
            return match ast::Fn::cast(node).and_then(|it| it.body()) {
                Some(body) => body.syntax().text_range(),
                None => closure_decl_range_view(node),
            };
        }
        match node.kind() {
            SyntaxKind::FN | SyntaxKind::CONST | SyntaxKind::STATIC => super::def_span(node),
            SyntaxKind::CLOSURE_EXPR => closure_decl_range_view(node),
            SyntaxKind::BLOCK_EXPR => match ast::BlockExpr::cast(node) {
                Some(it) if it.const_token().is_some() => it.stmt_list().map_or(item_range_view(node), |it| it.syntax().text_range()),
                _ => item_range_view(node),
            },
            _ => node.text_range(),
        }
    }
}

/// The definitions around an item, innermost first: a handful of levels, inline.
type Owners<'db> = SmallVec<[Owner<'db>; 8]>;

/// The definitions around `node`, innermost first, up to its crate's root or its file.
fn owners<'db>(cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>) -> Owners<'db> {
    let mut found = Owners::new();
    let mut prev = node.value;
    for ancestor in cx.vsema.ancestors_with_macros_file(node).skip(1) {
        let it = ancestor.value;
        // Out of an expansion `child` is its root, no child of `it`: no body of `it` is it.
        let child = std::mem::replace(&mut prev, it);
        let owner = |kind, body| Owner { node: ancestor, inner: false, kind, body };
        match it.kind() {
            SyntaxKind::SOURCE_FILE => {
                found.push(owner(Kind::Module, false));
                break;
            }
            SyntaxKind::MODULE => found.push(owner(Kind::Module, false)),
            SyntaxKind::FN => {
                let f = ast::Fn::cast(it);
                let body = f.and_then(|it| it.body()).is_some_and(|it| it.syntax() == child);
                if body && f.is_some_and(|it| it.async_token().is_some() || it.gen_token().is_some()) {
                    found.push(Owner { node: ancestor, inner: true, kind: Kind::Body, body: true });
                }
                found.push(owner(if body { Kind::Body } else { Kind::Other }, body));
            }
            SyntaxKind::CONST => {
                let c = ast::Const::cast(it);
                let body = c.and_then(|it| it.body()).is_some_and(|it| it.syntax() == child);
                let anon = c.is_some_and(|it| it.underscore_token().is_some());
                let kind = match (anon, body) {
                    (true, _) => Kind::AnonConst,
                    (false, true) => Kind::Body,
                    (false, false) => Kind::Other,
                };
                found.push(owner(kind, body));
            }
            SyntaxKind::STATIC => {
                let body = ast::Static::cast(it).and_then(|it| it.body()).is_some_and(|it| it.syntax() == child);
                found.push(owner(if body { Kind::Body } else { Kind::Other }, body));
            }
            SyntaxKind::CLOSURE_EXPR => {
                let closure = ast::ClosureExpr::cast(it);
                let body = closure.and_then(|it| it.body()).is_some_and(|it| it.syntax() == child);
                if body && closure.is_some_and(|it| it.async_token().is_some() || it.gen_token().is_some()) {
                    found.push(Owner { node: ancestor, inner: true, kind: Kind::Body, body: true });
                }
                found.push(owner(Kind::Body, body));
            }
            SyntaxKind::BLOCK_EXPR => {
                let block = ast::BlockExpr::cast(it);
                let def = block.is_some_and(|it| it.const_token().is_some() || it.async_token().is_some() || it.gen_token().is_some());
                if def {
                    found.push(owner(Kind::Body, true));
                }
            }
            SyntaxKind::CONST_ARG => found.push(owner(Kind::Body, true)),
            // A repeat count.
            SyntaxKind::ARRAY_EXPR if ast::ArrayExpr::cast(it).is_some_and(|it| it.semicolon_token().is_some()) => {
                let count = it.children().filter(|it| ast::Expr::can_cast(it.kind())).nth(1);
                if count == Some(child) {
                    found.push(Owner { node: ancestor.with_value(child), inner: false, kind: Kind::Body, body: true });
                }
            }
            SyntaxKind::IMPL
            | SyntaxKind::VARIANT
            | SyntaxKind::TRAIT
            | SyntaxKind::STRUCT
            | SyntaxKind::ENUM
            | SyntaxKind::UNION
            | SyntaxKind::TYPE_ALIAS
            | SyntaxKind::EXTERN_BLOCK => found.push(owner(Kind::Other, false)),
            _ => {}
        }
    }
    found
}

/// rustc's `peel_parent_while` of modules and anonymous constants from `owners[0]`.
fn peel<'o, 'db>(owners: &'o [Owner<'db>]) -> Option<&'o Owner<'db>> {
    owners.iter().find(|it| it.is_root() || !matches!(it.kind, Kind::Module | Kind::AnonConst))
}

/// How many bodies the item is in, as rustc's `body_depth` counts them in its module's pass.
fn depth(owners: &[Owner<'_>]) -> usize {
    owners.iter().take_while(|it| it.kind != Kind::Module).filter(|it| it.body).count()
}

/// The lints of `item` at rustc's `check_item`.
pub(super) fn item<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>) -> Option<Found> {
    item.in_body?;
    match item.node.value {
        ast::Item::Impl(imp) => impl_(cx, item, imp),
        ast::Item::MacroRules(rules) => macro_rules(cx, item, rules),
        _ => None,
    }
}

fn impl_<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, imp: ast::Impl<'db>) -> Option<Found> {
    let ours = owners(cx, item.syntax());
    let depth = depth(&ours);
    let parent = ours.first()?;
    if depth == 0 || (depth == 1 && parent.kind == Kind::AnonConst) {
        return None;
    }
    let self_ty = imp.self_ty()?;
    let outermost = peel(&ours).map(Owner::key);
    let mut paths: SmallVec<[(ast::Path<'db>, Option<PathResolution<'_>>); 4]> = SmallVec::new();
    let roots = std::iter::once(self_ty.syntax()).chain(imp.trait_().map(|it| it.syntax()));
    for root in roots {
        for path in root.descendants().filter(|it| it.kind() == SyntaxKind::PATH).filter_map(ast::Path::cast) {
            if path.syntax().parent().is_some_and(|it| ast::Path::can_cast(it.kind())) {
                continue;
            }
            let res = cx.vsema.resolve_path(path);
            let retained = matches!(
                res,
                Some(PathResolution::Def(def)) if !matches!(def, ModuleDef::BuiltinType(_))
            ) || matches!(res, Some(PathResolution::ConstParam(_)));
            if retained {
                paths.push((path, res));
            }
        }
    }
    let local = |res: &Option<PathResolution<'_>>| {
        let Some(PathResolution::Def(def)) = res else { return false };
        let Some(node) = source(cx, *def) else { return false };
        let theirs = owners(cx, node);
        let Some(theirs) = peel(&theirs) else { return false };
        theirs.key() == parent.key() || Some(theirs.key()) == outermost
    };
    if paths.iter().any(|(_, res)| local(res)) {
        return None;
    }

    let file = item.node.file_id;
    let span = |range: TextRange| cx.span(InFile::new(file, range));
    let start = item_range_view(imp.syntax()).start();
    let end = self_ty.syntax().text_range().end();
    let message = "non-local `impl` definition, `impl` blocks should be written at the same level as their item";
    let mut diag = Diag::lint("non_local_definitions", message, span(TextRange::new(start, end.max(start))));
    for (path, _) in &paths {
        diag.span_label(span(without_args(*path)), format!("`{}` is not local", last_name(*path))); // alloc: a lint that fires
    }
    let body = format!("{} `{}`", parent.descr(), parent.name()); // alloc: a lint that fires
    let label = match depth {
        1 => format!("move the `impl` block outside of this {body}"), // alloc: a lint that fires
        _ => format!("move the `impl` block outside of this {body} and up {depth} bodies"), // alloc: a lint that fires
    };
    diag.span_label(cx.span(parent.node.with_value(parent.span())), label);
    if let Some(call) = file.macro_file()
        && let Some(expansion) = cx.expansion_of(call)
    {
        // alloc: a lint that fires
        diag.note(format!("the {} `{}` defines the non-local `impl`, and may need to be changed", expansion.kind.descr(), expansion.name));
    }
    diag.note("an `impl` is never scoped, even when it is nested inside an item, as it may impact type checking outside of that item, which can be the case if neither the trait or the self type are at the same nesting level as the `impl`");
    let parent_node = parent.node.value;
    let const_or_static = !parent.inner && matches!(parent_node.kind(), SyntaxKind::CONST | SyntaxKind::STATIC);
    if const_or_static {
        diag.note("items in an anonymous const item (`const _: () = { ... }`) are treated as in the same scope as the anonymous const's declaration for the purpose of this lint");
        let named_unit = ast::Const::cast(parent_node).filter(|it| it.underscore_token().is_none()).and_then(|it| {
            let unit = matches!(it.ty(), Some(ast::Type::TupleType(t)) if t.fields().next().is_none());
            if unit { it.name() } else { None }
        });
        if let Some(name) = named_unit {
            let at = cx.span(parent.node.with_value(name.syntax().text_range()));
            diag.suggestion(at, "use a const-anon item to suppress this lint", "_", Applicability::MachineApplicable);
        }
    }
    if let Some((id, _)) = super::item_id(cx, item) {
        diag.fact(Fact::Signature(cx.key(id)), "the self type and trait of this `impl`");
    }
    Some(Found::lint(diag, Phase::Late, item.ptr()))
}

fn macro_rules<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, rules: ast::MacroRules<'db>) -> Option<Found> {
    walk::attr(cx, rules, "macro_export")?;
    let owners = owners(cx, item.syntax());
    let depth = depth(&owners);
    let parent = owners.first()?;
    if depth == 0 || (depth == 1 && parent.kind == Kind::AnonConst) {
        return None;
    }
    let span = cx.span(item.node.with_value(item_range_view(rules.syntax())));
    let message = "non-local `macro_rules!` definition, `#[macro_export]` macro should be written at top level module";
    let mut diag = Diag::lint("non_local_definitions", message, span);
    let body = format!("{} `{}`", parent.descr(), parent.name()); // alloc: a lint that fires
    diag.help(match depth {
        // alloc: a lint that fires
        1 => format!("remove the `#[macro_export]` or move this `macro_rules!` outside the of the current {body}"),
        // alloc: a lint that fires
        _ => format!("remove the `#[macro_export]` or move this `macro_rules!` outside the of the current {body} and up {depth} bodies"),
    });
    diag.note("a `macro_rules!` definition is non-local if it is nested inside an item and has a `#[macro_export]` attribute");
    Some(Found::lint(diag, Phase::Late, item.ptr()))
}

/// The source of a definition a path resolves to, in this crate.
fn source<'db>(cx: &Cx<'_, 'db>, def: ModuleDef) -> Option<InFile<ast::Node<'db>>> {
    fn node<'db, D: HasSourceView>(cx: &Cx<'_, 'db>, def: D) -> Option<InFile<ast::Node<'db>>> {
        cx.vsema.source(def).map(|it| it.map(|it| it.syntax()))
    }
    if def.module(cx.db)?.krate(cx.db) != cx.krate {
        return None;
    }
    match def {
        ModuleDef::Adt(it) => node(cx, it),
        ModuleDef::Function(it) => node(cx, it),
        ModuleDef::EnumVariant(it) => node(cx, it),
        ModuleDef::Const(it) => node(cx, it),
        ModuleDef::Static(it) => node(cx, it),
        ModuleDef::Trait(it) => node(cx, it),
        ModuleDef::TypeAlias(it) => node(cx, it),
        ModuleDef::Macro(it) => node(cx, it),
        ModuleDef::Module(_) | ModuleDef::BuiltinType(_) => None,
    }
}

/// rustc's `path_span_without_args`.
fn without_args(path: ast::Path<'_>) -> TextRange {
    let range = path.syntax().text_range();
    let args = path.segment().and_then(|it| {
        it.generic_arg_list()
            .map(|it| it.syntax().text_range().start())
            .or_else(|| it.parenthesized_arg_list().map(|it| it.syntax().text_range().start()))
    });
    match args {
        Some(start) => TextRange::new(range.start(), start.max(range.start())),
        None => range,
    }
}

/// rustc's `path_name_to_string`: the last segment's name.
fn last_name(path: ast::Path<'_>) -> String {
    // alloc: the label of a lint that fires
    let Some(segment) = path.segment() else { return String::new() };
    match segment.name_ref() {
        Some(name) => name.text().to_owned(), // alloc: the label of a lint that fires
        None => segment.syntax().first_token().map_or_else(String::new, |it| it.text().to_owned()), // alloc: as above
    }
}
