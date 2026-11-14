//! The printer: a tree as the canonical text of the subset.
//!
//! Keywords are lower case and tokens are one space apart, except that `(` and `.` take no
//! space after them, `)`, `,` and `.` none before them, and prefix `-` none after it
//! (`f(a, b)`, `t.c`, `-x`). A name is written as it was (quoted if it was quoted), a parameter
//! as `$name`, and every operand that is not an atom (a literal, a column, a parameter, a call,
//! a `cast` or a `case`) is parenthesized, so the text never depends on precedence. A tree
//! the parser could not have made (an unquoted name that is a keyword, a real that is
//! negative or not finite, a text literal with a lone `'`, a tree deeper than
//! [`MAX_DEPTH`]) is refused with [`fmt::Error`] rather than written as other text.

#![blaze::sans_io]

use core::alloc::Allocator;
use core::fmt;

use crate::ast::{
    CallArgs, ChainOp, CompareOp, Expr, ExprKind, JoinKind, Name, ResultColumn, SqlType, Statement,
    TableRef, UnaryOp,
};
use crate::lex::keyword;
use crate::parse::MAX_DEPTH;

/// Writes `statement` as text of the subset, which [`crate::parse::parse`] reads back to a tree
/// [`crate::same::same`] to it.
pub fn print<A: Allocator>(statement: &Statement<'_, A>, out: &mut impl fmt::Write) -> fmt::Result {
    Printer { out, parentheses: true }.statement(statement)
}

/// [`print`] with the parentheses around operands left out: a planted bug for the control of
/// the round-trip test, which must catch it.
#[cfg(test)]
pub(crate) fn print_without_parentheses<A: Allocator>(
    statement: &Statement<'_, A>,
    out: &mut impl fmt::Write,
) -> fmt::Result {
    Printer { out, parentheses: false }.statement(statement)
}

struct Printer<'o, W: fmt::Write> {
    out: &'o mut W,
    /// Off only in the planted printer.
    parentheses: bool,
}

/// Whether `expr` stands as an operand without parentheses.
fn atom<A: Allocator>(expr: &Expr<'_, A>) -> bool {
    match expr.kind {
        ExprKind::Null
        | ExprKind::Boolean(_)
        | ExprKind::Integer(_)
        | ExprKind::Real(_)
        | ExprKind::Text(_)
        | ExprKind::Blob(_)
        | ExprKind::Column { .. }
        | ExprKind::Param(_)
        | ExprKind::Call { .. }
        | ExprKind::Cast { .. }
        | ExprKind::Case { .. } => true,
        ExprKind::Unary { .. }
        | ExprKind::Chain { .. }
        | ExprKind::Compare { .. }
        | ExprKind::IsNull { .. }
        | ExprKind::InList { .. }
        | ExprKind::InParam { .. }
        | ExprKind::Between { .. }
        | ExprKind::Like { .. } => false,
    }
}

/// `[A-Za-z_][A-Za-z0-9_]*`.
fn is_identifier(text: &str) -> bool {
    let mut bytes = text.bytes();
    bytes.next().is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Whether every `'` of `text` is one of a doubled `''`.
fn quotes_doubled(text: &str) -> bool {
    let mut bytes = text.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'\'' && bytes.next() != Some(b'\'') {
            return false;
        }
    }
    true
}

fn chain_operator(op: ChainOp) -> &'static str {
    match op {
        ChainOp::Or => "or",
        ChainOp::And => "and",
        ChainOp::Add => "+",
        ChainOp::Sub => "-",
        ChainOp::Mul => "*",
        ChainOp::Div => "/",
        ChainOp::Rem => "%",
        ChainOp::Concat => "||",
    }
}

fn compare_operator(op: CompareOp) -> &'static str {
    match op {
        CompareOp::Eq => "=",
        CompareOp::Ne => "<>",
        CompareOp::Lt => "<",
        CompareOp::Le => "<=",
        CompareOp::Gt => ">",
        CompareOp::Ge => ">=",
        CompareOp::Is => "is",
        CompareOp::IsNot => "is not",
    }
}

fn type_name(to: SqlType) -> &'static str {
    match to {
        SqlType::Boolean => "boolean",
        SqlType::Integer => "integer",
        SqlType::Real => "real",
        SqlType::Text => "text",
        SqlType::Blob => "blob",
    }
}

impl<W: fmt::Write> Printer<'_, W> {
    fn str(&mut self, text: &str) -> fmt::Result {
        self.out.write_str(text)
    }

    fn name(&mut self, name: &Name<'_>) -> fmt::Result {
        if name.quoted {
            if name.text.is_empty() || name.text.contains('"') {
                return Err(fmt::Error);
            }
            self.str("\"")?;
            self.str(name.text)?;
            self.str("\"")
        } else {
            if !is_identifier(name.text) || keyword(name.text).is_some() {
                return Err(fmt::Error);
            }
            self.str(name.text)
        }
    }

    fn param(&mut self, name: &Name<'_>) -> fmt::Result {
        if name.quoted || !is_identifier(name.text) {
            return Err(fmt::Error);
        }
        self.str("$")?;
        self.str(name.text)
    }

    fn statement<A: Allocator>(&mut self, statement: &Statement<'_, A>) -> fmt::Result {
        match statement {
            Statement::Select(select) => {
                // `offset` reads back only after a `limit`.
                if select.offset.is_some() && select.limit.is_none() {
                    return Err(fmt::Error);
                }
                self.str("select ")?;
                if select.distinct {
                    self.str("distinct ")?;
                }
                self.result_columns(&select.columns)?;
                if let Some(from) = &select.from {
                    self.str(" from ")?;
                    self.table_ref(&from.first)?;
                    for join in &from.joins {
                        self.str(match join.kind {
                            JoinKind::Inner => " join ",
                            JoinKind::Left => " left join ",
                        })?;
                        self.table_ref(&join.table)?;
                        self.str(" on ")?;
                        self.top(&join.on)?;
                    }
                }
                self.clause(" where ", select.filter.as_ref())?;
                if !select.group_by.is_empty() {
                    self.str(" group by ")?;
                    self.list(&select.group_by)?;
                }
                self.clause(" having ", select.having.as_ref())?;
                for (at, term) in select.order_by.iter().enumerate() {
                    self.str(if at == 0 { " order by " } else { ", " })?;
                    self.top(&term.expr)?;
                    if term.descending {
                        self.str(" desc")?;
                    }
                }
                self.clause(" limit ", select.limit.as_ref())?;
                self.clause(" offset ", select.offset.as_ref())
            }
            Statement::Insert(insert) => {
                // An `insert` names one column or more and has one row or more, none empty.
                if insert.columns.is_empty()
                    || insert.rows.is_empty()
                    || insert.rows.iter().any(|row| row.is_empty())
                {
                    return Err(fmt::Error);
                }
                self.str("insert into ")?;
                self.name(&insert.table)?;
                self.str(" (")?;
                for (at, column) in insert.columns.iter().enumerate() {
                    if at > 0 {
                        self.str(", ")?;
                    }
                    self.name(column)?;
                }
                self.str(") values ")?;
                for (at, row) in insert.rows.iter().enumerate() {
                    if at > 0 {
                        self.str(", ")?;
                    }
                    self.str("(")?;
                    self.list(row)?;
                    self.str(")")?;
                }
                self.returning(&insert.returning)
            }
            Statement::Update(update) => {
                if update.sets.is_empty() {
                    return Err(fmt::Error);
                }
                self.str("update ")?;
                self.name(&update.table)?;
                self.str(" set ")?;
                for (at, set) in update.sets.iter().enumerate() {
                    if at > 0 {
                        self.str(", ")?;
                    }
                    self.name(&set.column)?;
                    self.str(" = ")?;
                    self.top(&set.value)?;
                }
                self.clause(" where ", update.filter.as_ref())?;
                self.returning(&update.returning)
            }
            Statement::Delete(delete) => {
                self.str("delete from ")?;
                self.name(&delete.table)?;
                self.clause(" where ", delete.filter.as_ref())?;
                self.returning(&delete.returning)
            }
        }
    }

    fn clause<A: Allocator>(&mut self, keyword: &str, expr: Option<&Expr<'_, A>>) -> fmt::Result {
        match expr {
            Some(expr) => {
                self.str(keyword)?;
                self.top(expr)
            }
            None => Ok(()),
        }
    }

    fn returning<A: Allocator>(&mut self, columns: &[ResultColumn<'_, A>]) -> fmt::Result {
        if columns.is_empty() {
            return Ok(());
        }
        self.str(" returning ")?;
        self.result_columns(columns)
    }

    fn result_columns<A: Allocator>(&mut self, columns: &[ResultColumn<'_, A>]) -> fmt::Result {
        // A `select` or `returning` list has at least one entry; an empty one has no text.
        if columns.is_empty() {
            return Err(fmt::Error);
        }
        for (at, column) in columns.iter().enumerate() {
            if at > 0 {
                self.str(", ")?;
            }
            match column {
                ResultColumn::Star { .. } => self.str("*")?,
                ResultColumn::TableStar { table } => {
                    self.name(table)?;
                    self.str(".*")?;
                }
                ResultColumn::Expr { expr, alias } => {
                    self.top(expr)?;
                    if let Some(alias) = alias {
                        self.str(" as ")?;
                        self.name(alias)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn table_ref(&mut self, table: &TableRef<'_>) -> fmt::Result {
        self.name(&table.table)?;
        if let Some(alias) = &table.alias {
            self.str(" as ")?;
            self.name(alias)?;
        }
        Ok(())
    }

    /// Whole expressions separated by `, `.
    fn list<A: Allocator>(&mut self, exprs: &[Expr<'_, A>]) -> fmt::Result {
        self.list_at(exprs, 1)
    }

    fn list_at<A: Allocator>(&mut self, exprs: &[Expr<'_, A>], depth: u32) -> fmt::Result {
        #[cfg(test)]
        crate::tests::stack::touch();
        for (at, expr) in exprs.iter().enumerate() {
            if at > 0 {
                self.str(", ")?;
            }
            self.expr(expr, depth)?;
        }
        Ok(())
    }

    /// An expression where a whole one stands.
    fn top<A: Allocator>(&mut self, expr: &Expr<'_, A>) -> fmt::Result {
        self.expr(expr, 1)
    }

    /// `expr` as an operand: parenthesized unless it is an atom.
    fn operand<A: Allocator>(&mut self, expr: &Expr<'_, A>, depth: u32) -> fmt::Result {
        #[cfg(test)]
        crate::tests::stack::touch();
        if self.parentheses && !atom(expr) {
            self.str("(")?;
            self.expr(expr, depth)?;
            self.str(")")
        } else {
            self.expr(expr, depth)
        }
    }

    fn not(&mut self, negated: bool) -> fmt::Result {
        if negated { self.str(" not") } else { Ok(()) }
    }

    /// `expr`, `depth` levels down its statement's expression (the top is 1).
    fn expr<A: Allocator>(&mut self, expr: &Expr<'_, A>, depth: u32) -> fmt::Result {
        #[cfg(test)]
        crate::tests::stack::touch();
        if depth > MAX_DEPTH {
            return Err(fmt::Error);
        }
        let below = depth + 1;
        match &expr.kind {
            ExprKind::Null => self.str("null"),
            ExprKind::Boolean(value) => self.str(if *value { "true" } else { "false" }),
            ExprKind::Integer(value) => write!(self.out, "{value}"),
            ExprKind::Real(value) => {
                if !value.is_finite() || value.is_sign_negative() {
                    return Err(fmt::Error);
                }
                write!(self.out, "{value:?}")
            }
            ExprKind::Text(text) => {
                if !quotes_doubled(text) {
                    return Err(fmt::Error);
                }
                self.str("'")?;
                self.str(text)?;
                self.str("'")
            }
            ExprKind::Blob(digits) => {
                if digits.len() % 2 != 0 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(fmt::Error);
                }
                self.str("x'")?;
                self.str(digits)?;
                self.str("'")
            }
            ExprKind::Column { table, column } => {
                if let Some(table) = table {
                    self.name(table)?;
                    self.str(".")?;
                }
                self.name(column)
            }
            ExprKind::Param(name) => self.param(name),
            ExprKind::Unary { op, operand } => {
                self.str(match op {
                    UnaryOp::Neg => "-",
                    UnaryOp::Not => "not ",
                })?;
                self.operand(operand, below)
            }
            ExprKind::Chain { first, rest } => {
                // A chain has at least one link, all of one level: anything else is no
                // text the parser reads back as this tree.
                let Some(head) = rest.first() else {
                    return Err(fmt::Error);
                };
                let level = head.op.level();
                if rest.iter().any(|link| link.op.level() != level) {
                    return Err(fmt::Error);
                }
                self.operand(first, below)?;
                for link in rest.iter() {
                    self.str(" ")?;
                    self.str(chain_operator(link.op))?;
                    self.str(" ")?;
                    self.operand(&link.operand, below)?;
                }
                Ok(())
            }
            ExprKind::Compare { op, left, right } => {
                self.operand(left, below)?;
                self.str(" ")?;
                self.str(compare_operator(*op))?;
                self.str(" ")?;
                // `a is (null)`: bare, `a is null` would read back as `IsNull`.
                let is = matches!(op, CompareOp::Is | CompareOp::IsNot);
                if is && matches!(right.kind, ExprKind::Null) {
                    self.str("(null)")
                } else {
                    self.operand(right, below)
                }
            }
            ExprKind::IsNull { operand, negated } => {
                self.operand(operand, below)?;
                self.str(if *negated { " is not null" } else { " is null" })
            }
            ExprKind::InList { operand, list, negated } => {
                // An `in` list has at least one entry; `in ()` is not text of the subset.
                if list.is_empty() {
                    return Err(fmt::Error);
                }
                self.operand(operand, below)?;
                self.not(*negated)?;
                self.str(" in (")?;
                self.list_at(list, below)?;
                self.str(")")
            }
            ExprKind::InParam { operand, list, negated } => {
                self.operand(operand, below)?;
                self.not(*negated)?;
                self.str(" in ")?;
                self.param(list)
            }
            ExprKind::Between { operand, low, high, negated } => {
                self.operand(operand, below)?;
                self.not(*negated)?;
                self.str(" between ")?;
                self.operand(low, below)?;
                self.str(" and ")?;
                self.operand(high, below)
            }
            ExprKind::Like { operand, pattern, negated } => {
                self.operand(operand, below)?;
                self.not(*negated)?;
                self.str(" like ")?;
                self.operand(pattern, below)
            }
            ExprKind::Case { operand, arms, otherwise } => {
                if arms.is_empty() {
                    return Err(fmt::Error);
                }
                self.str("case")?;
                if let Some(operand) = operand {
                    self.str(" ")?;
                    self.expr(operand, below)?;
                }
                for arm in arms {
                    self.str(" when ")?;
                    self.expr(&arm.when, below)?;
                    self.str(" then ")?;
                    self.expr(&arm.then, below)?;
                }
                if let Some(otherwise) = otherwise {
                    self.str(" else ")?;
                    self.expr(otherwise, below)?;
                }
                self.str(" end")
            }
            ExprKind::Cast { operand, to } => {
                self.str("cast(")?;
                self.expr(operand, below)?;
                self.str(" as ")?;
                self.str(type_name(*to))?;
                self.str(")")
            }
            ExprKind::Call { function, args } => {
                self.name(function)?;
                self.str("(")?;
                match args {
                    CallArgs::Star => self.str("*")?,
                    CallArgs::List { distinct, args } => {
                        if *distinct {
                            if args.is_empty() {
                                return Err(fmt::Error);
                            }
                            self.str("distinct ")?;
                        }
                        self.list_at(args, below)?;
                    }
                }
                self.str(")")
            }
        }
    }
}
