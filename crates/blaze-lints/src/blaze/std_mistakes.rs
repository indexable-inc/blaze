//! The std mistakes people and LLMs make most, each a lint of its own: a call of std's API that
//! compiles, passes a quick test and is wrong on the input nobody tried (a short write, a NaN, a
//! multi-byte character, a 4 GiB file, a thread nobody joins). Each is found by name
//! resolution and inferred types, never by text: the method or function a call resolves to is
//! std's, and the types are the ones inference gave.
//!
//! One module per lint under `std_mistakes/`; this one dispatches them and holds what they
//! share: which std item a call is, how a value's use is read from the tree, and how a finding
//! with its fix is reported. What each finds and leaves alone is in its module's comment and
//! in docs/lints.md.

mod clone_to_satisfy;
mod default_numeric_fallback_len;
mod detached_thread;
mod forget;
mod len_minus;
mod partial_cmp_unwrap;
mod partial_write;
mod path_to_str_unwrap;
mod refcell_borrow_across_call;
mod str_byte_slice;
#[cfg(test)]
mod tests;
mod transmute;
mod unbounded_read;
mod unchecked_index_in_loop;

use syntax::{
    SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, HasArgList as _},
};

use super::{Lint, test_code};
use crate::{check::Cx, diag::Applicability};

/// The lints of this module.
const LINTS: &[Lint] = &[
    Lint::PartialWrite,
    Lint::PartialCmpUnwrap,
    Lint::UnboundedRead,
    Lint::LenMinus,
    Lint::StrByteSlice,
    Lint::Forget,
    Lint::PathToStrUnwrap,
    Lint::DetachedThread,
    Lint::Transmute,
    Lint::CloneToSatisfy,
    Lint::UncheckedIndexInLoop,
    Lint::DefaultNumericFallbackLen,
    Lint::RefcellBorrowAcrossCall,
];

/// Every node of a file: each lint looks at the kinds it finds and returns at once otherwise.
/// Nothing a macro made is looked at, and nothing in test code but where a lint says so.
pub(super) fn check_node<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !matches!(
        node.kind(),
        SyntaxKind::METHOD_CALL_EXPR
            | SyntaxKind::CALL_EXPR
            | SyntaxKind::BIN_EXPR
            | SyntaxKind::INDEX_EXPR
            | SyntaxKind::REF_EXPR
            | SyntaxKind::FOR_EXPR
            | SyntaxKind::LET_STMT
    ) {
        return;
    }
    if !LINTS.iter().any(|it| cx.on_blaze(*it)) || cx.expansion().is_some() || !cx.is_written(node)
    {
        return;
    }
    if cx.analysis().is_none() {
        return;
    }
    partial_write::check(cx, node);
    partial_cmp_unwrap::check(cx, node);
    unbounded_read::check(cx, node);
    len_minus::check(cx, node);
    str_byte_slice::check(cx, node);
    forget::check(cx, node);
    path_to_str_unwrap::check(cx, node);
    detached_thread::check(cx, node);
    transmute::check(cx, node);
    clone_to_satisfy::check(cx, node);
    unchecked_index_in_loop::check(cx, node);
    default_numeric_fallback_len::check(cx, node);
    refcell_borrow_across_call::check(cx, node);
}

/// A fix of a finding: the parts it rewrites and how sure it is.
struct Fix {
    message: &'static str,
    parts: Vec<(TextRange, String)>,
    applicability: Applicability,
}

/// Reports `lint` at `range`, its level asked at `node`, with `note`, the lint's help and, when
/// there is one, `fix`.
fn report<'t>(
    cx: &mut Cx<'_, 't>,
    lint: Lint,
    node: ast::Node<'t>,
    range: TextRange,
    message: String,
    note: &str,
    fix: Option<Fix>,
) {
    let span = cx.span(range);
    // alloc: the fix's parts, one finding's
    let parts = fix.map(|it| {
        let parts: Vec<_> = it.parts.into_iter().map(|(at, text)| (cx.span(at), text)).collect();
        (it.message, parts, it.applicability)
    });
    let diagnostic = cx.blaze(lint, node, span, message);
    diagnostic.note(note);
    if let Some((message, parts, applicability)) = parts {
        diagnostic.multipart_suggestion(message, parts, applicability);
    }
    diagnostic.help(lint.help()).help(format!("for further information visit {}", lint.docs_url()));
}

/// Whether `node` is test code, which most of these lints leave alone: a test's input is the
/// test's own.
fn in_test<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> bool {
    test_code::is_test_code(cx, node)
}

/// `expr` without the parentheses around it.
fn strip(expr: ast::Expr<'_>) -> ast::Expr<'_> {
    expr.peel_parens()
}

/// The expression `node` is a part of, past the parentheses around it.
fn parent_expr(node: ast::Node<'_>) -> Option<ast::Node<'_>> {
    let mut at = node.parent()?;
    while at.kind() == SyntaxKind::PAREN_EXPR {
        at = at.parent()?;
    }
    Some(at)
}

/// Whether `f` is the trait at `path`'s own method or an impl's of it.
fn of_trait(cx: &Cx<'_, '_>, f: hir::Function, path: &[&str]) -> bool {
    let db = cx.db();
    let Some(trait_) = cx.std_trait(path) else { return false };
    hir::AsAssocItem::as_assoc_item(f, db).and_then(|it| it.container_or_implemented_trait(db))
        == Some(trait_)
}

/// The ADT whose inherent impl `f` is a method of.
fn inherent_adt(cx: &Cx<'_, '_>, f: hir::Function) -> Option<hir::Adt> {
    let db = cx.db();
    let hir::AssocItemContainer::Impl(imp) = hir::AsAssocItem::as_assoc_item(f, db)?.container(db)
    else {
        return None;
    };
    if imp.trait_(db).is_some() {
        return None;
    }
    imp.self_ty(db).as_adt()
}

/// Whether `f` is a method of the inherent impl of the std ADT at `path`.
fn inherent_of(cx: &Cx<'_, '_>, f: hir::Function, path: &[&str]) -> bool {
    match (inherent_adt(cx, f), cx.std_def(path)) {
        (Some(adt), Some(hir::ModuleDef::Adt(it))) => adt == it,
        _ => false,
    }
}

/// Whether `f` is the function at `path` of std.
fn is_std_fn(cx: &Cx<'_, '_>, f: hir::Function, path: &[&str]) -> bool {
    cx.std_def(path) == Some(hir::ModuleDef::Function(f))
}

/// The function a call `callee(..)` calls, by the path it is written with.
fn called<'t>(cx: &Cx<'_, 't>, call: ast::CallExpr<'t>) -> Option<hir::Function> {
    let ast::Expr::PathExpr(callee) = strip(call.expr()?) else { return None };
    match cx.path(callee.path()?)? {
        hir::PathResolution::Def(hir::ModuleDef::Function(it)) => Some(it),
        _ => None,
    }
}

/// The function `node`, a method call or a call by path, calls, with its receiver or first
/// argument and the rest of its arguments.
fn callee<'t>(
    cx: &Cx<'_, 't>,
    node: ast::Node<'t>,
) -> Option<(hir::Function, Option<ast::Expr<'t>>, Vec<ast::Expr<'t>>)> {
    if let Some(call) = ast::MethodCallExpr::cast(node) {
        let f = cx.method(call)?;
        // alloc: a call's few arguments
        let args: Vec<_> = call.arg_list()?.args().collect();
        return Some((f, call.receiver(), args));
    }
    let call = ast::CallExpr::cast(node)?;
    let f = called(cx, call)?;
    // alloc: a call's few arguments
    let mut args: Vec<_> = call.arg_list()?.args().collect();
    let first = (!args.is_empty()).then(|| args.remove(0));
    Some((f, first, args))
}

/// Which of `Option`'s or `Result`'s panicking unwraps `call` is: `unwrap` or `expect`, resolved
/// to the enum's own impl.
fn unwrap_of<'t>(cx: &Cx<'_, 't>, call: ast::MethodCallExpr<'t>) -> Option<&'static str> {
    let f = cx.method(call)?;
    let name = match f.name(cx.db()).as_str() {
        "unwrap" => "unwrap",
        "expect" => "expect",
        _ => return None,
    };
    let adt = inherent_adt(cx, f)?;
    let famous = cx.famous();
    let option = famous.core_option_Option().map(hir::Adt::Enum);
    let result = famous.core_result_Result().map(hir::Adt::Enum);
    (Some(adt) == option || Some(adt) == result).then_some(name)
}

/// The method call `node` is the receiver of, past parentheses.
fn receiver_of(node: ast::Node<'_>) -> Option<ast::MethodCallExpr<'_>> {
    let parent = ast::MethodCallExpr::cast(parent_expr(node)?)?;
    let receiver = strip(parent.receiver()?);
    (receiver.syntax() == node).then_some(parent)
}

/// The body `node` is in: the innermost `fn`'s, `const`'s or `static`'s, or the closure's when
/// `closures`.
fn body_of(node: ast::Node<'_>, closures: bool) -> Option<ast::Node<'_>> {
    node.ancestors().find(|it| match it.kind() {
        SyntaxKind::FN | SyntaxKind::CONST | SyntaxKind::STATIC => true,
        SyntaxKind::CLOSURE_EXPR => closures,
        _ => false,
    })
}

/// The local `pat` binds, when it is a plain binding (`x`, `mut x`).
fn bound<'q, 't>(cx: &Cx<'q, 't>, pat: ast::Pat<'t>) -> Option<hir::Local<'q>> {
    let ast::Pat::IdentPat(pat) = pat else { return None };
    if pat.pat().is_some() || pat.ref_token().is_some() {
        return None;
    }
    cx.local(pat)
}

/// The paths in `scope` that name `local`.
fn uses_of<'q, 't>(
    cx: &Cx<'q, 't>,
    scope: ast::Node<'t>,
    local: hir::Local<'q>,
) -> Vec<ast::PathExpr<'t>> {
    // alloc: the uses of one local, a few
    let mut uses = Vec::new();
    for node in scope.descendants() {
        let Some(path) = ast::PathExpr::cast(node) else { continue };
        let Some(written) = path.path() else { continue };
        if written.qualifier().is_some() {
            continue;
        }
        if let Some(hir::PathResolution::Local(it)) = cx.path(written)
            && it == local
        {
            uses.push(path);
        }
    }
    uses
}

/// Whether the value of `node` is thrown away where it is made: the expression of a statement
/// (`f(x);`), the initializer of `let _ = ..` or `let _name = ..`, or the same after a `?`, an
/// `.unwrap()` or an `.expect(..)` of it.
fn discarded<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> bool {
    let mut at = node;
    for _ in 0..4 {
        let Some(parent) = parent_expr(at) else { return false };
        match parent.kind() {
            SyntaxKind::EXPR_STMT => return true,
            SyntaxKind::LET_STMT => {
                let Some(stmt) = ast::LetStmt::cast(parent) else { return false };
                return match stmt.pat() {
                    Some(ast::Pat::WildcardPat(_)) => true,
                    Some(ast::Pat::IdentPat(it)) => {
                        it.name().is_some_and(|name| name.text().starts_with('_'))
                    }
                    _ => false,
                };
            }
            SyntaxKind::TRY_EXPR => at = parent,
            SyntaxKind::METHOD_CALL_EXPR => {
                let Some(call) = ast::MethodCallExpr::cast(parent) else { return false };
                if receiver_of(at) != Some(call) || unwrap_of(cx, call).is_none() {
                    return false;
                }
                at = parent;
            }
            // A block's tail whose block is itself a statement: `{ f.write(b) };`, `unsafe { .. };`.
            SyntaxKind::STMT_LIST => {
                let Some(block) = parent.parent() else { return false };
                at = block;
            }
            _ => return false,
        }
    }
    false
}

/// Whether `expr` is a constant: a literal, a path to a `const` or `static`, or arithmetic of
/// them.
fn constant<'t>(cx: &Cx<'_, 't>, expr: ast::Expr<'t>) -> bool {
    match strip(expr) {
        ast::Expr::Literal(_) => true,
        ast::Expr::PathExpr(path) => matches!(
            path.path().and_then(|it| cx.path(it)),
            Some(hir::PathResolution::Def(hir::ModuleDef::Const(_) | hir::ModuleDef::Static(_)))
        ),
        ast::Expr::BinExpr(bin) => {
            bin.lhs().is_some_and(|it| constant(cx, it)) && bin.rhs().is_some_and(|it| constant(cx, it))
        }
        _ => false,
    }
}

/// Whether the function `node` is in returns an `Option`, so `?` on one hands a `None` on.
fn returns_option<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> bool {
    let Some(f) = node.ancestors().find_map(ast::Fn::cast) else { return false };
    let in_closure = node.ancestors().take_while(|it| *it != f.syntax()).any(|it| {
        it.kind() == SyntaxKind::CLOSURE_EXPR
            || ast::BlockExpr::cast(it).is_some_and(|block| block.async_token().is_some())
    });
    if in_closure {
        return false;
    }
    let Some(def) = cx.fn_def(f) else { return false };
    let ret = def.ret_type(cx.db());
    let option = cx.famous().core_option_Option().map(hir::Adt::Enum);
    option.is_some() && ret.as_adt() == option
}
