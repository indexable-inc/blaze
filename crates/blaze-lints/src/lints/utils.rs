//! What several lints ask of the code, as clippy's `clippy_utils` answers it.

use hir::PathResolution;
use syntax::{NodeOrToken, SyntaxKind, TextRange, TextSize, view as ast};

use ast::{AstNode, HasArgList as _, HasName as _};

use crate::{check::Cx, sugg::Sugg};

/// The most arguments a [`MethodCall`] has: no lint asks of a call with more.
pub(crate) const MAX_ARGS: usize = 2;

/// A method call: `receiver.name(args)`.
#[derive(Clone, Copy)]
pub(crate) struct MethodCall<'t> {
    pub(crate) name: &'t str,
    /// The method's name, where it is written.
    pub(crate) name_ref: ast::NameRef<'t>,
    pub(crate) receiver: ast::Expr<'t>,
    /// The arguments, then `receiver` in the places past them.
    args: [ast::Expr<'t>; MAX_ARGS],
    len: usize,
}

impl<'t> MethodCall<'t> {
    /// The arguments, in order.
    #[inline]
    pub(crate) fn args(&self) -> &[ast::Expr<'t>] {
        &self.args[..self.len]
    }
}

/// The name of the method `expr` calls, if it is a method call: what a lint of one method
/// asks before [`method_call`].
#[inline]
pub(crate) fn method_name(expr: ast::Expr<'_>) -> Option<&str> {
    let ast::Expr::MethodCallExpr(call) = expr else { return None };
    Some(call.name_ref()?.text())
}

/// clippy's `method_call`: `expr` as a method call of at most [`MAX_ARGS`] arguments whose
/// receiver and arguments are all written in the file, none of them made by a macro.
pub(crate) fn method_call<'t>(cx: &Cx<'_, 't>, expr: ast::Expr<'t>) -> Option<MethodCall<'t>> {
    let ast::Expr::MethodCallExpr(call) = expr else { return None };
    let receiver = call.receiver()?;
    if !cx.is_written(receiver.syntax()) {
        return None;
    }
    let mut args = [receiver; MAX_ARGS];
    let mut len = 0;
    for arg in call.arg_list()?.args() {
        if len == MAX_ARGS || !cx.is_written(arg.syntax()) {
            return None;
        }
        args[len] = arg;
        len += 1;
    }
    let name_ref = call.name_ref()?;
    Some(MethodCall { name: name_ref.text(), name_ref, receiver, args, len })
}

/// `Option` or `Result`, when `ty`, its references peeled, is one of them.
pub(crate) fn option_or_result(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> Option<&'static str> {
    let adt = Some(ty.strip_references().as_adt()?);
    let famous = cx.famous();
    if famous.core_option_Option().map(hir::Adt::Enum) == adt {
        return Some("Option");
    }
    if famous.core_result_Result().map(hir::Adt::Enum) == adt {
        return Some("Result");
    }
    None
}

/// clippy's `is_none_expr`: `expr` is `None`, `Option`'s.
pub(crate) fn is_none(cx: &Cx<'_, '_>, expr: ast::Expr<'_>) -> bool {
    let ast::Expr::PathExpr(path) = expr else { return false };
    let Some(path) = path.path() else { return false };
    let Some(PathResolution::Def(hir::ModuleDef::EnumVariant(variant))) = cx.path(path) else {
        return false;
    };
    let db = cx.db();
    cx.famous().core_option_Option() == Some(variant.parent_enum(db))
        && variant.name(db).as_str() == "None"
}

/// `expr` is the literal `value`.
pub(crate) fn is_bool_literal(expr: ast::Expr<'_>, value: bool) -> bool {
    let ast::Expr::Literal(literal) = expr else { return false };
    matches!(literal.kind(), ast::LiteralKind::Bool(it) if it == value)
}

/// The locals the paths of `node` name, each once, added to `found`.
pub(crate) fn locals_in<'q, 't>(
    cx: &Cx<'q, 't>,
    node: ast::Node<'t>,
    found: &mut Vec<hir::Local<'q>>,
) {
    for path in node.descendants().filter_map(ast::PathExpr::cast) {
        if let Some(local) = local_of(cx, path)
            && !found.contains(&local)
        {
            found.push(local);
        }
    }
}

pub(crate) fn local_of<'q, 't>(cx: &Cx<'q, 't>, path: ast::PathExpr<'t>) -> Option<hir::Local<'q>> {
    match cx.path(path.path()?)? {
        PathResolution::Local(local) => Some(local),
        _ => None,
    }
}

/// The body `node` is in, as rustc's `hir_enclosing_body_owner` finds it: its closure, its
/// function, or the initializer of its constant or static.
pub(crate) fn enclosing_body(node: ast::Node<'_>) -> Option<ast::Node<'_>> {
    node.ancestors().skip(1).find_map(|it| {
        if let Some(closure) = ast::ClosureExpr::cast(it) {
            return closure.body().map(|it| it.syntax());
        }
        if let Some(function) = ast::Fn::cast(it) {
            return function.body().map(|it| it.syntax());
        }
        if let Some(konst) = ast::Const::cast(it) {
            return konst.body().map(|it| it.syntax());
        }
        ast::Static::cast(it).and_then(|it| it.body()).map(|it| it.syntax())
    })
}

/// `expr`, or what initialised the binding it names when that binding is not `mut`, as
/// clippy's `expr_or_init` follows it.
pub(crate) fn expr_or_init<'t>(cx: &Cx<'_, 't>, expr: ast::Expr<'t>) -> ast::Expr<'t> {
    let mut expr = expr;
    for _ in 0..16 {
        let ast::Expr::PathExpr(path) = expr else { break };
        let Some(local) = local_of(cx, path) else { break };
        let Some(init) = binding_init(cx, local, path) else { break };
        expr = init;
    }
    expr
}

/// What `let` initialised `local`, which `path` names, with, if it is bound by a plain `let`
/// written in the tree walked and not `mut`.
fn binding_init<'q, 't>(
    cx: &Cx<'q, 't>,
    local: hir::Local<'q>,
    path: ast::PathExpr<'t>,
) -> Option<ast::Expr<'t>> {
    let name = path.path()?.as_single_name_ref()?.text();
    // A binding is written before what names it; a path a macro made has no place to stop at.
    let before = cx.range(path.syntax()).map_or(TextSize::from(u32::MAX), |it| it.start());
    let pat = cx
        .root()
        .descendants()
        .take_while(|it| it.text_range().start() < before)
        .filter_map(ast::IdentPat::cast)
        .find(|it| it.name().is_some_and(|it| it.text() == name) && cx.local(*it) == Some(local))?;
    if pat.mut_token().is_some() || pat.ref_token().is_some() {
        return None;
    }
    let let_stmt = ast::LetStmt::cast(pat.syntax().parent()?)?;
    let_stmt.initializer()
}

/// clippy's `Sugg::hir_with_context` for `expr`, written in the file.
pub(crate) fn sugg<'t>(cx: &Cx<'_, 't>, expr: ast::Expr<'t>) -> Sugg {
    let snippet = |node: ast::Node<'t>| cx.source_of(node);
    let same_type = |prefix: ast::PrefixExpr<'t>| {
        let inner = prefix.expr();
        let outer = cx.ty(ast::Expr::PrefixExpr(prefix));
        inner.and_then(|it| cx.ty(it)).zip(outer).is_some_and(|(a, b)| a == b)
    };
    Sugg::of(expr, &snippet, &same_type)
}

/// clippy's `peel_blocks`: `expr` with the blocks around it that only hold an expression
/// taken off.
pub(crate) fn peel_blocks(expr: ast::Expr<'_>) -> ast::Expr<'_> {
    let mut expr = expr;
    while let ast::Expr::BlockExpr(block) = expr
        && block.modifier().is_none()
        && block.label().is_none()
        && block.statements().next().is_none()
        && let Some(tail) = block.tail_expr()
    {
        expr = tail;
    }
    expr
}

/// clippy's `peel_blocks_with_stmt`: as [`peel_blocks`], a block holding one expression
/// statement and nothing else taken off too.
pub(crate) fn peel_blocks_with_stmt(expr: ast::Expr<'_>) -> ast::Expr<'_> {
    let mut expr = expr;
    loop {
        let ast::Expr::BlockExpr(block) = expr else { return expr };
        if block.modifier().is_some() || block.label().is_some() {
            return expr;
        }
        let mut statements = block.statements();
        let (first, more) = (statements.next(), statements.next().is_some());
        expr = match (first, more, block.tail_expr()) {
            (None, _, Some(tail)) => tail,
            (Some(ast::Stmt::ExprStmt(stmt)), false, None) => match stmt.expr() {
                Some(inner) => inner,
                None => return expr,
            },
            _ => return expr,
        };
    }
}

/// clippy's `span_contains_comment`: whether a comment is written in `range` of `node`.
pub(crate) fn contains_comment(node: ast::Node<'_>, range: TextRange) -> bool {
    let is_comment = |kind| {
        matches!(
            kind,
            SyntaxKind::COMMENT | SyntaxKind::INNER_DOC_COMMENT | SyntaxKind::OUTER_DOC_COMMENT
        )
    };
    // Only the tokens of the element holding `range` can be in it.
    let within = match node.text_range().contains_range(range) {
        true => node.covering_element(range),
        false => NodeOrToken::Node(node),
    };
    let within = match within {
        NodeOrToken::Node(it) => it,
        NodeOrToken::Token(it) => {
            return is_comment(it.kind()) && range.contains_range(it.text_range());
        }
    };
    within.descendants_with_tokens().filter_map(|it| it.into_token()).any(|it| {
        is_comment(it.kind()) && range.contains_range(it.text_range())
    })
}

/// Calls `f` on each path expression of `root` that starts at or after `after`, and of what
/// the macro calls there expand to.
pub(crate) fn each_path<'t>(
    cx: &Cx<'_, 't>,
    root: ast::Node<'t>,
    after: Option<TextSize>,
    f: &mut impl FnMut(ast::PathExpr<'t>),
) {
    each_path_at(cx, root, after, 0, f);
}

fn each_path_at<'t>(
    cx: &Cx<'_, 't>,
    root: ast::Node<'t>,
    after: Option<TextSize>,
    depth: u32,
    f: &mut impl FnMut(ast::PathExpr<'t>),
) {
    const DEEPEST: u32 = 8;
    for node in root.descendants() {
        if after.is_some_and(|it| node.text_range().start() < it) {
            continue;
        }
        if let Some(path) = ast::PathExpr::cast(node) {
            f(path);
        } else if let Some(call) = ast::MacroCall::cast(node)
            && depth < DEEPEST
            && let Some(expansion) = cx.expand(call)
        {
            each_path_at(cx, expansion, None, depth + 1, f);
        }
    }
}

/// clippy's `indentation`: the whitespace starting the line `range` starts on.
pub(crate) fn indentation(text: &str, range: TextRange) -> &str {
    let start = usize::from(range.start());
    let line_start = text[..start].rfind('\n').map_or(0, |it| it + 1);
    let line = &text[line_start..];
    let end = line.find(|it: char| !it.is_whitespace() || it == '\n').unwrap_or(line.len());
    &line[..end]
}
