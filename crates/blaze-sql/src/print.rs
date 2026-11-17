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
    CallArgs, ChainOp, CompareOp, Expr, ExprKind, JoinKind, Name, ResultColumn, Statement,
    TableRef, UnaryOp,
};
use crate::desc::Storage;
use crate::lex::{BLOB_OPEN, Keyword, keyword, keyword_text};
use crate::parse::MAX_DEPTH;

/// Writes `statement` as text of the subset, which [`crate::parse::parse`] reads back to a tree
/// [`crate::same::same`] to it.
#[blaze::noalloc]
#[blaze::nonblocking]
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

impl<W: fmt::Write> Printer<'_, W> {
    fn str(&mut self, text: &str) -> fmt::Result {
        self.out.write_str(text)
    }

    /// `keyword`, in the spelling the lexer reads ([`keyword_text`]).
    fn word(&mut self, keyword: Keyword) -> fmt::Result {
        self.str(keyword_text(keyword).ok_or(fmt::Error)?)
    }

    /// `keyword` and the space after it.
    fn lead(&mut self, keyword: Keyword) -> fmt::Result {
        self.word(keyword)?;
        self.str(" ")
    }

    /// The space before `keyword`, and `keyword`.
    fn tail(&mut self, keyword: Keyword) -> fmt::Result {
        self.str(" ")?;
        self.word(keyword)
    }

    /// `keyword` with a space either side.
    fn pad(&mut self, keyword: Keyword) -> fmt::Result {
        self.tail(keyword)?;
        self.str(" ")
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
                self.lead(Keyword::Select)?;
                if select.distinct {
                    self.lead(Keyword::Distinct)?;
                }
                self.result_columns(&select.columns)?;
                if let Some(from) = &select.from {
                    self.pad(Keyword::From)?;
                    self.table_ref(&from.first)?;
                    for join in &from.joins {
                        match join.kind {
                            JoinKind::Inner => {}
                            JoinKind::Left => self.tail(Keyword::Left)?,
                        }
                        self.pad(Keyword::Join)?;
                        self.table_ref(&join.table)?;
                        self.pad(Keyword::On)?;
                        self.top(&join.on)?;
                    }
                }
                self.clause(Keyword::Where, select.filter.as_ref())?;
                if !select.group_by.is_empty() {
                    self.tail(Keyword::Group)?;
                    self.pad(Keyword::By)?;
                    self.list(&select.group_by)?;
                }
                self.clause(Keyword::Having, select.having.as_ref())?;
                for (at, term) in select.order_by.iter().enumerate() {
                    if at == 0 {
                        self.tail(Keyword::Order)?;
                        self.pad(Keyword::By)?;
                    } else {
                        self.str(", ")?;
                    }
                    self.top(&term.expr)?;
                    if term.descending {
                        self.tail(Keyword::Desc)?;
                    }
                }
                self.clause(Keyword::Limit, select.limit.as_ref())?;
                self.clause(Keyword::Offset, select.offset.as_ref())
            }
            Statement::Insert(insert) => {
                // An `insert` names one column or more and has one row or more, none empty.
                if insert.columns.is_empty()
                    || insert.rows.is_empty()
                    || insert.rows.iter().any(|row| row.is_empty())
                {
                    return Err(fmt::Error);
                }
                self.lead(Keyword::Insert)?;
                self.lead(Keyword::Into)?;
                self.name(&insert.table)?;
                self.str(" (")?;
                for (at, column) in insert.columns.iter().enumerate() {
                    if at > 0 {
                        self.str(", ")?;
                    }
                    self.name(column)?;
                }
                self.str(") ")?;
                self.lead(Keyword::Values)?;
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
                self.lead(Keyword::Update)?;
                self.name(&update.table)?;
                self.pad(Keyword::Set)?;
                for (at, set) in update.sets.iter().enumerate() {
                    if at > 0 {
                        self.str(", ")?;
                    }
                    self.name(&set.column)?;
                    self.str(" = ")?;
                    self.top(&set.value)?;
                }
                self.clause(Keyword::Where, update.filter.as_ref())?;
                self.returning(&update.returning)
            }
            Statement::Delete(delete) => {
                self.lead(Keyword::Delete)?;
                self.lead(Keyword::From)?;
                self.name(&delete.table)?;
                self.clause(Keyword::Where, delete.filter.as_ref())?;
                self.returning(&delete.returning)
            }
        }
    }

    /// ` keyword expr`, if there is an `expr`.
    fn clause<A: Allocator>(
        &mut self,
        keyword: Keyword,
        expr: Option<&Expr<'_, A>>,
    ) -> fmt::Result {
        match expr {
            Some(expr) => {
                self.pad(keyword)?;
                self.top(expr)
            }
            None => Ok(()),
        }
    }

    fn returning<A: Allocator>(&mut self, columns: &[ResultColumn<'_, A>]) -> fmt::Result {
        if columns.is_empty() {
            return Ok(());
        }
        self.pad(Keyword::Returning)?;
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
                        self.pad(Keyword::As)?;
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
            self.pad(Keyword::As)?;
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

    fn chain_op(&mut self, op: ChainOp) -> fmt::Result {
        match op {
            ChainOp::Or => self.word(Keyword::Or),
            ChainOp::And => self.word(Keyword::And),
            ChainOp::Add => self.str("+"),
            ChainOp::Sub => self.str("-"),
            ChainOp::Mul => self.str("*"),
            ChainOp::Div => self.str("/"),
            ChainOp::Rem => self.str("%"),
            ChainOp::Concat => self.str("||"),
        }
    }

    fn compare_op(&mut self, op: CompareOp) -> fmt::Result {
        match op {
            CompareOp::Eq => self.str("="),
            CompareOp::Ne => self.str("<>"),
            CompareOp::Lt => self.str("<"),
            CompareOp::Le => self.str("<="),
            CompareOp::Gt => self.str(">"),
            CompareOp::Ge => self.str(">="),
            CompareOp::Is => self.word(Keyword::Is),
            CompareOp::IsNot => {
                self.word(Keyword::Is)?;
                self.tail(Keyword::Not)
            }
        }
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
            ExprKind::Null => self.word(Keyword::Null),
            ExprKind::Boolean(value) => {
                self.word(if *value { Keyword::True } else { Keyword::False })
            }
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
                self.str(BLOB_OPEN)?;
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
            ExprKind::Unary { op: UnaryOp::Neg, operand } => {
                self.str("-")?;
                self.operand(operand, below)
            }
            ExprKind::Unary { op: UnaryOp::Not, operand } => {
                self.lead(Keyword::Not)?;
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
                    self.chain_op(link.op)?;
                    self.str(" ")?;
                    self.operand(&link.operand, below)?;
                }
                Ok(())
            }
            ExprKind::Compare {
                op:
                    op @ (CompareOp::Eq
                    | CompareOp::Ne
                    | CompareOp::Lt
                    | CompareOp::Le
                    | CompareOp::Gt
                    | CompareOp::Ge
                    | CompareOp::Is
                    | CompareOp::IsNot),
                left,
                right,
            } => {
                self.operand(left, below)?;
                self.str(" ")?;
                self.compare_op(*op)?;
                self.str(" ")?;
                // `a is (null)`: bare, `a is null` would read back as `IsNull`.
                let is = matches!(op, CompareOp::Is | CompareOp::IsNot);
                if is && matches!(right.kind, ExprKind::Null) {
                    self.str("(")?;
                    self.word(Keyword::Null)?;
                    self.str(")")
                } else {
                    self.operand(right, below)
                }
            }
            ExprKind::IsNull { operand, negated } => {
                self.operand(operand, below)?;
                self.tail(Keyword::Is)?;
                if *negated {
                    self.tail(Keyword::Not)?;
                }
                self.tail(Keyword::Null)
            }
            ExprKind::InList { operand, list, negated } => {
                // An `in` list has at least one entry; `in ()` is not text of the subset.
                if list.is_empty() {
                    return Err(fmt::Error);
                }
                self.operand(operand, below)?;
                if *negated {
                    self.tail(Keyword::Not)?;
                }
                self.pad(Keyword::In)?;
                self.str("(")?;
                self.list_at(list, below)?;
                self.str(")")
            }
            ExprKind::InParam { operand, list, negated } => {
                self.operand(operand, below)?;
                if *negated {
                    self.tail(Keyword::Not)?;
                }
                self.pad(Keyword::In)?;
                self.param(list)
            }
            ExprKind::Between { operand, low, high, negated } => {
                self.operand(operand, below)?;
                if *negated {
                    self.tail(Keyword::Not)?;
                }
                self.pad(Keyword::Between)?;
                self.operand(low, below)?;
                self.pad(Keyword::And)?;
                self.operand(high, below)
            }
            ExprKind::Like { operand, pattern, negated } => {
                self.operand(operand, below)?;
                if *negated {
                    self.tail(Keyword::Not)?;
                }
                self.pad(Keyword::Like)?;
                self.operand(pattern, below)
            }
            ExprKind::Case { operand, arms, otherwise } => {
                if arms.is_empty() {
                    return Err(fmt::Error);
                }
                self.word(Keyword::Case)?;
                if let Some(operand) = operand {
                    self.str(" ")?;
                    self.expr(operand, below)?;
                }
                for arm in arms {
                    self.pad(Keyword::When)?;
                    self.expr(&arm.when, below)?;
                    self.pad(Keyword::Then)?;
                    self.expr(&arm.then, below)?;
                }
                if let Some(otherwise) = otherwise {
                    self.pad(Keyword::Else)?;
                    self.expr(otherwise, below)?;
                }
                self.tail(Keyword::End)
            }
            ExprKind::Cast {
                operand,
                to:
                    to @ (Storage::Boolean
                    | Storage::Integer
                    | Storage::Real
                    | Storage::Text
                    | Storage::Blob),
            } => {
                self.word(Keyword::Cast)?;
                self.str("(")?;
                self.expr(operand, below)?;
                self.pad(Keyword::As)?;
                self.str(to.name().ok_or(fmt::Error)?)?;
                self.str(")")
            }
            ExprKind::Call { function, args: CallArgs::Star } => {
                self.name(function)?;
                self.str("(")?;
                self.str("*")?;
                self.str(")")
            }
            ExprKind::Call { function, args: CallArgs::List { distinct, args } } => {
                self.name(function)?;
                self.str("(")?;
                if *distinct {
                    if args.is_empty() {
                        return Err(fmt::Error);
                    }
                    self.lead(Keyword::Distinct)?;
                }
                self.list_at(args, below)?;
                self.str(")")
            }
        }
    }
}
