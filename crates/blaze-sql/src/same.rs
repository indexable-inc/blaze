//! Tree equality that ignores spans: what a round trip through the printer and the parser
//! keeps.
//!
//! Two names are the same when both are quoted and their text is equal, or both are unquoted
//! and their text is equal without regard to ASCII case. Parameter names compare exactly.
//! Reals compare by their bits, blobs' hex digits without regard to case. A tree deeper than
//! [`MAX_DEPTH`] is the same as nothing: the parser makes no such tree, and the bound keeps
//! the walk off the end of the stack.

#![blaze::sans_io]

use core::alloc::Allocator;

use crate::ast::{CallArgs, Expr, ExprKind, From, Name, ResultColumn, Statement, TableRef};
use crate::parse::MAX_DEPTH;

/// Whether two trees are one statement: equal but for spans, with unquoted names compared
/// without regard to ASCII case.
pub fn same<A: Allocator, B: Allocator>(a: &Statement<'_, A>, b: &Statement<'_, B>) -> bool {
    match (a, b) {
        (Statement::Select(a), Statement::Select(b)) => {
            a.distinct == b.distinct
                && result_columns(&a.columns, &b.columns)
                && option(a.from.as_ref(), b.from.as_ref(), from)
                && option(a.filter.as_ref(), b.filter.as_ref(), same_expr)
                && list(&a.group_by, &b.group_by, same_expr)
                && option(a.having.as_ref(), b.having.as_ref(), same_expr)
                && list(&a.order_by, &b.order_by, |a, b| {
                    a.descending == b.descending && same_expr(&a.expr, &b.expr)
                })
                && option(a.limit.as_ref(), b.limit.as_ref(), same_expr)
                && option(a.offset.as_ref(), b.offset.as_ref(), same_expr)
        }
        (Statement::Insert(a), Statement::Insert(b)) => {
            name(&a.table, &b.table)
                && list(&a.columns, &b.columns, name)
                && list(&a.rows, &b.rows, |a, b| list(a, b, same_expr))
                && result_columns(&a.returning, &b.returning)
        }
        (Statement::Update(a), Statement::Update(b)) => {
            name(&a.table, &b.table)
                && list(&a.sets, &b.sets, |a, b| {
                    name(&a.column, &b.column) && same_expr(&a.value, &b.value)
                })
                && option(a.filter.as_ref(), b.filter.as_ref(), same_expr)
                && result_columns(&a.returning, &b.returning)
        }
        (Statement::Delete(a), Statement::Delete(b)) => {
            name(&a.table, &b.table)
                && option(a.filter.as_ref(), b.filter.as_ref(), same_expr)
                && result_columns(&a.returning, &b.returning)
        }
        (
            Statement::Select(_)
            | Statement::Insert(_)
            | Statement::Update(_)
            | Statement::Delete(_),
            _,
        ) => false,
    }
}

/// [`same`] for two expressions.
pub fn same_expr<A: Allocator, B: Allocator>(a: &Expr<'_, A>, b: &Expr<'_, B>) -> bool {
    expr(a, b, 1)
}

fn name(a: &Name<'_>, b: &Name<'_>) -> bool {
    a.quoted == b.quoted
        && if a.quoted { a.text == b.text } else { a.text.eq_ignore_ascii_case(b.text) }
}

fn param(a: &Name<'_>, b: &Name<'_>) -> bool {
    a.quoted == b.quoted && a.text == b.text
}

fn option<T, U>(a: Option<&T>, b: Option<&U>, same: impl Fn(&T, &U) -> bool) -> bool {
    #[cfg(test)]
    crate::tests::stack::touch();
    match (a, b) {
        (Some(a), Some(b)) => same(a, b),
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

fn list<T, U>(a: &[T], b: &[U], same: impl Fn(&T, &U) -> bool) -> bool {
    #[cfg(test)]
    crate::tests::stack::touch();
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same(a, b))
}

fn table_ref(a: &TableRef<'_>, b: &TableRef<'_>) -> bool {
    name(&a.table, &b.table) && option(a.alias.as_ref(), b.alias.as_ref(), name)
}

fn from<A: Allocator, B: Allocator>(a: &From<'_, A>, b: &From<'_, B>) -> bool {
    table_ref(&a.first, &b.first)
        && list(&a.joins, &b.joins, |a, b| {
            a.kind == b.kind && table_ref(&a.table, &b.table) && same_expr(&a.on, &b.on)
        })
}

fn result_columns<A: Allocator, B: Allocator>(
    a: &[ResultColumn<'_, A>],
    b: &[ResultColumn<'_, B>],
) -> bool {
    list(a, b, |a, b| match (a, b) {
        (ResultColumn::Star { .. }, ResultColumn::Star { .. }) => true,
        (ResultColumn::TableStar { table: a }, ResultColumn::TableStar { table: b }) => name(a, b),
        (
            ResultColumn::Expr { expr: a, alias: a_alias },
            ResultColumn::Expr { expr: b, alias: b_alias },
        ) => same_expr(a, b) && option(a_alias.as_ref(), b_alias.as_ref(), name),
        (
            ResultColumn::Star { .. } | ResultColumn::TableStar { .. } | ResultColumn::Expr { .. },
            _,
        ) => false,
    })
}

/// `a` and `b`, `depth` levels down their expressions (the top is 1).
fn expr<A: Allocator, B: Allocator>(a: &Expr<'_, A>, b: &Expr<'_, B>, depth: u32) -> bool {
    #[cfg(test)]
    crate::tests::stack::touch();
    if depth > MAX_DEPTH {
        return false;
    }
    let below = depth + 1;
    let sub = |a: &Expr<'_, A>, b: &Expr<'_, B>| expr(a, b, below);
    match (&a.kind, &b.kind) {
        (ExprKind::Null, ExprKind::Null) => true,
        (ExprKind::Boolean(a), ExprKind::Boolean(b)) => a == b,
        (ExprKind::Integer(a), ExprKind::Integer(b)) => a == b,
        (ExprKind::Real(a), ExprKind::Real(b)) => a.to_bits() == b.to_bits(),
        (ExprKind::Text(a), ExprKind::Text(b)) => a == b,
        (ExprKind::Blob(a), ExprKind::Blob(b)) => a.eq_ignore_ascii_case(b),
        (
            ExprKind::Column { table: a_table, column: a_column },
            ExprKind::Column { table: b_table, column: b_column },
        ) => option(a_table.as_ref(), b_table.as_ref(), name) && name(a_column, b_column),
        (ExprKind::Param(a), ExprKind::Param(b)) => param(a, b),
        (ExprKind::Unary { op: a_op, operand: a }, ExprKind::Unary { op: b_op, operand: b }) => {
            a_op == b_op && sub(a, b)
        }
        (
            ExprKind::Chain { first: a_first, rest: a_rest },
            ExprKind::Chain { first: b_first, rest: b_rest },
        ) => {
            sub(a_first, b_first)
                && list(a_rest, b_rest, |a, b| a.op == b.op && sub(&a.operand, &b.operand))
        }
        (
            ExprKind::Compare { op: a_op, left: a_left, right: a_right },
            ExprKind::Compare { op: b_op, left: b_left, right: b_right },
        ) => a_op == b_op && sub(a_left, b_left) && sub(a_right, b_right),
        (
            ExprKind::IsNull { operand: a, negated: a_negated },
            ExprKind::IsNull { operand: b, negated: b_negated },
        ) => a_negated == b_negated && sub(a, b),
        (
            ExprKind::InList { operand: a, list: a_list, negated: a_negated },
            ExprKind::InList { operand: b, list: b_list, negated: b_negated },
        ) => a_negated == b_negated && sub(a, b) && list(a_list, b_list, sub),
        (
            ExprKind::InParam { operand: a, list: a_list, negated: a_negated },
            ExprKind::InParam { operand: b, list: b_list, negated: b_negated },
        ) => a_negated == b_negated && sub(a, b) && param(a_list, b_list),
        (
            ExprKind::Between { operand: a, low: a_low, high: a_high, negated: a_negated },
            ExprKind::Between { operand: b, low: b_low, high: b_high, negated: b_negated },
        ) => a_negated == b_negated && sub(a, b) && sub(a_low, b_low) && sub(a_high, b_high),
        (
            ExprKind::Like { operand: a, pattern: a_pattern, negated: a_negated },
            ExprKind::Like { operand: b, pattern: b_pattern, negated: b_negated },
        ) => a_negated == b_negated && sub(a, b) && sub(a_pattern, b_pattern),
        (
            ExprKind::Case { operand: a_operand, arms: a_arms, otherwise: a_otherwise },
            ExprKind::Case { operand: b_operand, arms: b_arms, otherwise: b_otherwise },
        ) => {
            option(a_operand.as_deref(), b_operand.as_deref(), sub)
                && list(a_arms, b_arms, |a, b| sub(&a.when, &b.when) && sub(&a.then, &b.then))
                && option(a_otherwise.as_deref(), b_otherwise.as_deref(), sub)
        }
        (ExprKind::Cast { operand: a, to: a_to }, ExprKind::Cast { operand: b, to: b_to }) => {
            a_to == b_to && sub(a, b)
        }
        (
            ExprKind::Call { function: a_function, args: a_args },
            ExprKind::Call { function: b_function, args: b_args },
        ) => {
            name(a_function, b_function)
                && match (a_args, b_args) {
                    (CallArgs::Star, CallArgs::Star) => true,
                    (
                        CallArgs::List { distinct: a_distinct, args: a_args },
                        CallArgs::List { distinct: b_distinct, args: b_args },
                    ) => a_distinct == b_distinct && list(a_args, b_args, sub),
                    (CallArgs::Star | CallArgs::List { .. }, _) => false,
                }
        }
        (
            ExprKind::Null
            | ExprKind::Boolean(_)
            | ExprKind::Integer(_)
            | ExprKind::Real(_)
            | ExprKind::Text(_)
            | ExprKind::Blob(_)
            | ExprKind::Column { .. }
            | ExprKind::Param(_)
            | ExprKind::Unary { .. }
            | ExprKind::Chain { .. }
            | ExprKind::Compare { .. }
            | ExprKind::IsNull { .. }
            | ExprKind::InList { .. }
            | ExprKind::InParam { .. }
            | ExprKind::Between { .. }
            | ExprKind::Like { .. }
            | ExprKind::Case { .. }
            | ExprKind::Cast { .. }
            | ExprKind::Call { .. },
            _,
        ) => false,
    }
}
