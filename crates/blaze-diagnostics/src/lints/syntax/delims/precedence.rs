//! When the delimiters around an expression are needed for it to parse as it does: rustc's
//! `UnusedDelimLint::is_expr_delims_necessary`, and the parser's classifications it reads, of
//! what each expression is once expanded.
//!
//! Follows rustc's `compiler/rustc_lint/src/unused.rs` (`is_expr_delims_necessary`),
//! `compiler/rustc_ast/src/util/classify.rs` (`expr_is_complete`,
//! `expr_requires_semi_to_be_stmt`, `expr_trailing_brace`, `type_trailing_braced_mac_call`) and
//! `compiler/rustc_ast/src/util/parser.rs` (`contains_exterior_struct_lit`) (MIT or
//! Apache-2.0).

use syntax::view::{self as ast, RangeItem as _};

use super::{Ctx, MACRO_DEPTH, expanded, expanded_ty};
use crate::cx::Cx;

/// Whether the delimiters around `inner` (expanded) are needed where it is: before the `else`
/// of a `let`-`else`, a range as a `let` scrutinee, `&raw`, a leftmost operand that would end
/// a statement, and before a block, what would run into it.
pub(super) fn delims_necessary<'db>(cx: &Cx<'_, 'db>, inner: ast::Expr<'db>, ctx: Ctx, followed_by_block: bool) -> bool {
    if ctx == Ctx::AssignedValueLetElse && (is_lazy(inner) || trailing_brace(cx, inner)) {
        return true;
    }
    if matches!(inner, ast::Expr::RangeExpr(_)) && ctx == Ctx::LetScrutineeExpr {
        return true;
    }
    // Kept around `&raw const x`: a method called on it needs them.
    if matches!(inner, ast::Expr::RefExpr(it) if it.raw_token().is_some()) {
        return true;
    }
    // `({ 0 } + 1)`: without them, `{ 0 }` is a statement.
    let mut innermost = inner;
    loop {
        let next = match innermost {
            ast::Expr::BinExpr(it) if is_binary(it) => it.lhs(),
            ast::Expr::CallExpr(it) => it.expr(),
            ast::Expr::CastExpr(it) => it.expr(),
            ast::Expr::IndexExpr(it) => it.base(),
            _ => break,
        };
        let Some(next) = next else { break };
        innermost = expanded(cx, next);
        if !requires_semi_to_be_stmt(innermost) {
            return true;
        }
    }
    if !followed_by_block {
        return false;
    }
    // `match &(S { f }) {}`.
    let mut innermost = inner;
    while let ast::Expr::RefExpr(it) = innermost
        && let Some(e) = it.expr()
    {
        innermost = expanded(cx, e);
    }
    if contains_exterior_struct_lit(cx, innermost) {
        return true;
    }
    // `if (() == return) {}`: what ends the expression would take the block.
    let mut innermost = inner;
    loop {
        let next = match innermost {
            ast::Expr::PrefixExpr(it) => it.expr(),
            ast::Expr::BinExpr(it) => it.rhs(),
            ast::Expr::ReturnExpr(_) | ast::Expr::YieldExpr(_) | ast::Expr::YeetExpr(_) => return true,
            // `if (break 'a i) {}`: `i {}` would be a struct literal.
            ast::Expr::BreakExpr(it) => {
                return it.expr().is_some_and(|it| {
                    let it = expanded(cx, it);
                    is_block(it) || matches!(it, ast::Expr::PathExpr(_))
                });
            }
            ast::Expr::RangeExpr(it) if it.end().is_some() => {
                return it.end().is_some_and(|it| is_block(expanded(cx, it)));
            }
            _ => return contains_exterior_struct_lit(cx, inner),
        };
        let Some(next) = next else { return contains_exterior_struct_lit(cx, inner) };
        innermost = expanded(cx, next);
    }
}

/// A binary operation that is not an assignment: rustc's `ExprKind::Binary`.
fn is_binary(e: ast::BinExpr<'_>) -> bool {
    e.op_kind().is_some_and(|it| !matches!(it, ast::BinaryOp::Assignment { .. }))
}

/// `a && b` or `a || b`.
pub(super) fn is_lazy(e: ast::Expr<'_>) -> bool {
    matches!(e, ast::Expr::BinExpr(it) if matches!(it.op_kind(), Some(ast::BinaryOp::LogicOp(_))))
}

/// `a = b`: rustc's `ExprKind::Assign`, not a compound assignment.
pub(super) fn is_assign(e: ast::Expr<'_>) -> bool {
    matches!(e, ast::Expr::BinExpr(it) if it.op_kind() == Some(ast::BinaryOp::Assignment { op: None }))
}

/// A block, labelled or `unsafe` or not: rustc's `ExprKind::Block`, which `async`, `gen`,
/// `const` and `try` blocks are not.
pub(super) fn is_block(e: ast::Expr<'_>) -> bool {
    matches!(e, ast::Expr::BlockExpr(it) if is_block_kind(it))
}

fn is_block_kind(block: ast::BlockExpr<'_>) -> bool {
    block.async_token().is_none()
        && block.gen_token().is_none()
        && block.const_token().is_none()
        && block.try_block_modifier().is_none()
}

/// A block with no label that is not `unsafe`: rustc's `ExprKind::Block(_, None)` with
/// `BlockCheckMode::Default`.
pub(super) fn is_plain_block(block: ast::BlockExpr<'_>) -> bool {
    is_block_kind(block) && block.label().is_none() && block.unsafe_token().is_none()
}

/// `expr_is_complete`: an expression that ends a statement where it ends.
fn is_complete(e: ast::Expr<'_>) -> bool {
    match e {
        ast::Expr::IfExpr(_)
        | ast::Expr::MatchExpr(_)
        | ast::Expr::WhileExpr(_)
        | ast::Expr::LoopExpr(_)
        | ast::Expr::ForExpr(_) => true,
        // Blocks, `try` and `const` blocks; not `async` or `gen` ones.
        ast::Expr::BlockExpr(it) => it.async_token().is_none() && it.gen_token().is_none(),
        _ => false,
    }
}

/// `expr_requires_semi_to_be_stmt`.
fn requires_semi_to_be_stmt(e: ast::Expr<'_>) -> bool {
    match e {
        ast::Expr::MacroExpr(it) => !it.macro_call().is_some_and(braced),
        _ => !is_complete(e),
    }
}

/// Whether a macro is called with braces.
fn braced(call: ast::MacroCall<'_>) -> bool {
    call.token_tree().is_some_and(|it| it.l_curly_token().is_some())
}

/// `expr_trailing_brace(..).is_some()`: the expression ends in a `}`.
fn trailing_brace<'db>(cx: &Cx<'_, 'db>, e: ast::Expr<'db>) -> bool {
    let mut e = e;
    for _ in 0..MACRO_DEPTH {
        let next = match e {
            ast::Expr::RefExpr(it) => it.expr(),
            ast::Expr::BinExpr(it) => it.rhs(),
            ast::Expr::LetExpr(it) => it.expr(),
            ast::Expr::PrefixExpr(it) => it.expr(),
            ast::Expr::BecomeExpr(it) => it.expr(),
            ast::Expr::ClosureExpr(it) => it.body(),
            ast::Expr::BreakExpr(it) => it.expr(),
            ast::Expr::RangeExpr(it) => it.end(),
            ast::Expr::ReturnExpr(it) => it.expr(),
            ast::Expr::YeetExpr(it) => it.expr(),
            ast::Expr::YieldExpr(it) => it.expr(),
            ast::Expr::BlockExpr(_)
            | ast::Expr::ForExpr(_)
            | ast::Expr::IfExpr(_)
            | ast::Expr::LoopExpr(_)
            | ast::Expr::MatchExpr(_)
            | ast::Expr::RecordExpr(_)
            | ast::Expr::WhileExpr(_) => return true,
            ast::Expr::CastExpr(it) => return it.ty().is_some_and(|it| type_trailing_braced_mac_call(cx, it)),
            ast::Expr::MacroExpr(it) => return it.macro_call().is_some_and(braced),
            _ => return false,
        };
        let Some(next) = next else { return false };
        e = expanded(cx, next);
    }
    false
}

/// `type_trailing_braced_mac_call(..).is_some()`: a type that ends in a macro called with
/// braces.
fn type_trailing_braced_mac_call<'db>(cx: &Cx<'_, 'db>, ty: ast::Type<'db>) -> bool {
    let mut ty = expanded_ty(cx, ty);
    for _ in 0..MACRO_DEPTH {
        let next = match ty {
            ast::Type::MacroType(it) => return it.macro_call().is_some_and(braced),
            ast::Type::PtrType(it) => it.ty(),
            ast::Type::RefType(it) => it.ty(),
            ast::Type::ForType(it) => it.ty(),
            ast::Type::FnPtrType(it) => it.ret_type().and_then(|it| it.ty()),
            ast::Type::PathType(it) => it.path().and_then(path_return_type),
            ast::Type::DynTraitType(it) => it.type_bound_list().and_then(last_bound_return_type),
            ast::Type::ImplTraitType(it) => it.type_bound_list().and_then(last_bound_return_type),
            _ => return false,
        };
        let Some(next) = next else { return false };
        ty = expanded_ty(cx, next);
    }
    false
}

/// The return type of the last bound of a list, when it is a trait bound with one.
fn last_bound_return_type(list: ast::TypeBoundList<'_>) -> Option<ast::Type<'_>> {
    let bound = list.bounds().last()?;
    path_return_type(super::trait_bound_path(bound)?)
}

/// `path_return_type`: the `-> T` of a path's last segment, as in `FnOnce(&str) -> T`.
fn path_return_type(path: ast::Path<'_>) -> Option<ast::Type<'_>> {
    let segment = path.segment()?;
    segment.parenthesized_arg_list()?;
    segment.ret_type()?.ty()
}

/// `contains_exterior_struct_lit`: a struct literal not inside delimiters of its own, which
/// before a block would take the block as its fields.
fn contains_exterior_struct_lit<'db>(cx: &Cx<'_, 'db>, e: ast::Expr<'db>) -> bool {
    let sub = |it: Option<ast::Expr<'db>>| it.is_some_and(|it| contains_exterior_struct_lit(cx, expanded(cx, it)));
    match e {
        ast::Expr::RecordExpr(_) => true,
        ast::Expr::BinExpr(it) => sub(it.lhs()) || sub(it.rhs()),
        ast::Expr::AwaitExpr(it) => sub(it.expr()),
        ast::Expr::PrefixExpr(it) => sub(it.expr()),
        ast::Expr::CastExpr(it) => sub(it.expr()),
        ast::Expr::FieldExpr(it) => sub(it.expr()),
        ast::Expr::IndexExpr(it) => sub(it.base()),
        ast::Expr::MethodCallExpr(it) => sub(it.receiver()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use syntax::{Edition, SourceFile, view::AstNode as _};

    use super::*;

    /// `f` of the initializer of `let _ = text;`, viewed in its parse.
    fn expr<R>(text: &str, f: impl FnOnce(ast::Expr<'_>) -> R) -> R {
        let parse = SourceFile::parse(&format!("fn f() {{ let _ = {text}; }}"), Edition::CURRENT);
        let init = parse.root().descendants().find_map(ast::LetStmt::cast).and_then(|it| it.initializer());
        f(init.unwrap())
    }

    fn block(text: &str, f: impl FnOnce(ast::BlockExpr<'_>) -> bool) -> bool {
        expr(text, |it| {
            let ast::Expr::BlockExpr(it) = it else { panic!("`{text}` is a block") };
            f(it)
        })
    }

    #[test]
    fn blocks_end_a_statement_and_async_ones_do_not() {
        assert!(!expr("{ 0 }", requires_semi_to_be_stmt));
        assert!(!expr("unsafe { 0 }", requires_semi_to_be_stmt));
        assert!(!expr("const { 0 }", requires_semi_to_be_stmt));
        assert!(!expr("if a { 0 } else { 1 }", requires_semi_to_be_stmt));
        assert!(expr("async { 0 }", requires_semi_to_be_stmt));
        assert!(expr("a + 1", requires_semi_to_be_stmt));
    }

    #[test]
    fn a_block_kind_is_rustcs() {
        assert!(expr("'a: { 0 }", is_block));
        assert!(expr("unsafe { 0 }", is_block));
        assert!(!expr("async { 0 }", is_block));
        assert!(!expr("const { 0 }", is_block));
        assert!(!block("'a: { 0 }", is_plain_block));
        assert!(block("{ 0 }", is_plain_block));
    }

    #[test]
    fn lazy_and_plain_assignments_are_told_apart() {
        assert!(expr("a && b", is_lazy));
        assert!(expr("a || b", is_lazy));
        assert!(!expr("a & b", is_lazy));
        assert!(expr("a = b", is_assign));
        assert!(!expr("a += b", is_assign));
    }

    #[test]
    fn a_parenthesized_trait_path_has_a_return_type() {
        assert!(path("FnOnce(&str) -> u8", |it| path_return_type(it).is_some()));
        assert!(path("FnOnce(&str)", |it| path_return_type(it).is_none()));
    }

    fn path(text: &str, f: impl FnOnce(ast::Path<'_>) -> bool) -> bool {
        let parse = SourceFile::parse(&format!("type T = dyn {text};"), Edition::CURRENT);
        f(parse.root().descendants().find_map(ast::Path::cast).unwrap())
    }
}
