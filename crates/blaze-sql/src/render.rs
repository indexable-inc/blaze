//! A checked statement as an engine's text (docs/v2/plan/design/sql.md): SQLite's `?k` or
//! Postgres's `$k` placeholders numbered from 1 in the order parameters first appear, and the
//! [`Slot`] each one binds.
//!
//! The text is canonical, not the statement as written: every table and column is written by
//! its schema name in double quotes, keywords lower case, every operand of an operator that is
//! not a single term in parentheses, and every result column with `as` the name the
//! description gives it (`*` and `t.*` as their columns), so an engine names each column as
//! the description does. Where SQLite and Postgres would read one text differently, each gets
//! the text that means what the description says: Postgres's `sum`, `avg`, `count` and
//! `length` are cast to the types SQLite gives, its `like` has no escape character, its
//! `order by` puts nulls where SQLite does.

#![blaze::sans_io]

use alloc::vec::Vec;
use core::alloc::Allocator;
use core::fmt;

use crate::Checked;
use crate::ast::{
    self, CallArgs, CompareOp, Expr, ExprKind, JoinKind, Name, ResultColumn, Span, SqlType,
    Statement, TableRef, UnaryOp,
};
use crate::desc::{Backend, Param, Rendered, Slot, Storage};
use crate::funcs::{self, Postgres};
use crate::infer::{self, Cx, Entry, TypeError};
// The one bound on nesting, the parser's: rendering recurses once per level, and the checker
// refused anything deeper already.
use crate::parse::MAX_DEPTH;

/// SQLite's most placeholders in one statement (`SQLITE_MAX_VARIABLE_NUMBER`'s default).
pub const SQLITE_MAX_PLACEHOLDERS: u32 = 32766;

/// Postgres's most placeholders in one statement (the wire protocol's 16-bit count).
pub const POSTGRES_MAX_PLACEHOLDERS: u32 = 65535;

/// How many bytes of a parameter's name an error keeps.
const PARAM_NAME_BYTES: usize = 47;

/// A parameter's name kept in an error, which borrows nothing: its first
/// [`PARAM_NAME_BYTES`] bytes, cut at a character boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParamName {
    bytes: [u8; PARAM_NAME_BYTES],
    len: u8,
    cut: bool,
}

impl ParamName {
    fn new(name: &str) -> ParamName {
        let mut len = name.len().min(PARAM_NAME_BYTES);
        while !name.is_char_boundary(len) {
            len -= 1;
        }
        let mut bytes = [0; PARAM_NAME_BYTES];
        if let (Some(to), Some(from)) = (bytes.get_mut(..len), name.as_bytes().get(..len)) {
            to.copy_from_slice(from);
        }
        // `len` is at most PARAM_NAME_BYTES, which fits a u8.
        ParamName { bytes, len: len as u8, cut: len < name.len() }
    }

    /// The name kept, without its `$`.
    pub fn as_str(&self) -> &str {
        self.bytes
            .get(..usize::from(self.len))
            .and_then(|bytes| core::str::from_utf8(bytes).ok())
            .unwrap_or("")
    }
}

impl fmt::Display for ParamName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`${}{}`", self.as_str(), if self.cut { "…" } else { "" })
    }
}

/// Why a checked statement has no text for an engine.
#[derive(Debug)]
pub enum RenderError {
    /// `list_lens` has a length for each list parameter, and it had another count.
    ListLens { expected: u32, found: u32 },
    /// A list parameter used twice on SQLite, which writes one placeholder per entry.
    ListUsedTwice { param: ParamName, span: Span },
    /// The parameter whose placeholder would cross the engine's most.
    TooManyPlaceholders { backend: Backend, limit: u32, param: ParamName, span: Span },
    /// A parameter of the statement its description does not have: the `Checked` is not one
    /// statement and its description.
    NotInDescription { param: ParamName, span: Span },
    /// The statement does not resolve against its own schema here: a bug in the checker.
    Inconsistent { span: Span },
    /// An expression nests deeper than rendering recurses.
    TooDeep { span: Span },
    /// An `offset` with no `limit`, which the parser refuses: a tree built by hand.
    OffsetWithoutLimit { span: Span },
    /// Writing a number into the text failed.
    Format,
}

fn backend_name(backend: Backend) -> &'static str {
    match backend {
        Backend::Sqlite => "SQLite",
        Backend::Postgres => "Postgres",
    }
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenderError::ListLens { expected, found } => write!(
                f,
                "the statement has {expected} list parameters, and {found} list lengths were given; give one length per list parameter, in the description's order"
            ),
            RenderError::ListUsedTwice { param, span } => write!(
                f,
                "list parameter {param} is used twice; on SQLite a list is one placeholder per entry, so a list parameter may be used once (at bytes {}..{})",
                span.start, span.end
            ),
            RenderError::TooManyPlaceholders { backend, limit, param, span } => write!(
                f,
                "parameter {param} needs a placeholder past {}'s most of {limit} in one statement (at bytes {}..{})",
                backend_name(*backend),
                span.start,
                span.end
            ),
            RenderError::NotInDescription { param, span } => write!(
                f,
                "parameter {param} is not in the statement's description (at bytes {}..{}); the checked statement is not consistent",
                span.start, span.end
            ),
            RenderError::Inconsistent { span } => write!(
                f,
                "the checked statement does not resolve against its own schema at bytes {}..{}; this is a bug in the checker",
                span.start, span.end
            ),
            RenderError::TooDeep { span } => write!(
                f,
                "the expression at bytes {}..{} nests deeper than {MAX_DEPTH} levels",
                span.start, span.end
            ),
            RenderError::OffsetWithoutLimit { span } => write!(
                f,
                "`offset` at bytes {}..{} has no `limit` before it, which no parsed statement has; write `limit .. offset ..`",
                span.start, span.end
            ),
            RenderError::Format => f.write_str("writing a number into the statement's text failed"),
        }
    }
}

/// Writes `checked` as `backend`'s text. `list_lens` holds the length of each list parameter,
/// in the order the list parameters stand in the description.
pub fn render<A: Allocator + Copy, B: Allocator + Copy>(
    checked: &Checked<'_, A>,
    backend: Backend,
    list_lens: &[u32],
    alloc: B,
) -> Result<Rendered<B>, RenderError> {
    let params = &checked.description.params[..];
    let mut list_of = Vec::with_capacity_in(params.len(), alloc);
    let mut lists: u32 = 0;
    for param in params {
        if param.list {
            list_of.push(lists);
            lists = lists.saturating_add(1);
        } else {
            list_of.push(NOT_A_LIST);
        }
    }
    if usize::try_from(lists).ok() != Some(list_lens.len()) {
        return Err(RenderError::ListLens {
            expected: lists,
            found: u32::try_from(list_lens.len()).unwrap_or(u32::MAX),
        });
    }
    let mut numbers = Vec::with_capacity_in(params.len(), alloc);
    numbers.resize(params.len(), UNNUMBERED);
    let mut writer = Writer {
        backend,
        params,
        list_lens,
        list_of,
        numbers,
        text: Vec::new_in(alloc),
        slots: Vec::new_in(alloc),
        cx: Cx::new(&checked.schema, None, alloc),
        depth: 0,
    };
    writer.statement(&checked.statement)?;
    Ok(Rendered { text: writer.text, slots: writer.slots })
}

/// In `list_of`: the parameter is a scalar.
const NOT_A_LIST: u32 = u32::MAX;
/// In `numbers`: the parameter has no placeholder yet.
const UNNUMBERED: u32 = 0;
/// In `numbers`: a SQLite list parameter, written once.
const LIST_WRITTEN: u32 = u32::MAX;

fn inconsistent(error: TypeError<'_>) -> RenderError {
    RenderError::Inconsistent { span: error.span }
}

/// A `fmt::Write` into the text, for a real's digits.
struct Bytes<'a, B: Allocator>(&'a mut Vec<u8, B>);

impl<B: Allocator> fmt::Write for Bytes<'_, B> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

struct Writer<'c, 's, B: Allocator + Copy> {
    backend: Backend,
    params: &'c [Param<'s>],
    list_lens: &'c [u32],
    /// Per parameter: its index in `list_lens`, or [`NOT_A_LIST`].
    list_of: Vec<u32, B>,
    /// Per parameter: its placeholder number, [`UNNUMBERED`] or [`LIST_WRITTEN`].
    numbers: Vec<u32, B>,
    text: Vec<u8, B>,
    slots: Vec<Slot, B>,
    /// The typer, run again for the scope and a `sum`'s argument.
    cx: Cx<'s, B>,
    depth: u32,
}

impl<'s, B: Allocator + Copy> Writer<'_, 's, B> {
    fn put(&mut self, s: &str) {
        self.text.extend_from_slice(s.as_bytes());
    }

    fn put_u64(&mut self, mut value: u64) {
        let mut digits = [0u8; 20];
        let mut at = digits.len();
        loop {
            at -= 1;
            if let Some(digit) = digits.get_mut(at) {
                // `value % 10` is below 10.
                *digit = b'0' + (value % 10) as u8;
            }
            value /= 10;
            if value == 0 || at == 0 {
                break;
            }
        }
        if let Some(digits) = digits.get(at..) {
            self.text.extend_from_slice(digits);
        }
    }

    /// A real's shortest digits that read back to it; Rust's `{:?}` always has a `.` or an
    /// exponent, so neither engine reads it as an integer.
    fn put_real(&mut self, value: f64) -> Result<(), RenderError> {
        fmt::write(&mut Bytes(&mut self.text), format_args!("{value:?}"))
            .map_err(|_| RenderError::Format)
    }

    /// A name of the schema, quoted.
    fn schema_name(&mut self, name: &str) {
        self.put("\"");
        for piece in name.split_inclusive('"') {
            self.put(piece);
            if piece.ends_with('"') {
                self.put("\"");
            }
        }
        self.put("\"");
    }

    /// A name from the source (an alias), quoted: its text is the source between its quotes,
    /// with any `""` still doubled.
    fn source_name(&mut self, name: &str) {
        self.put("\"");
        self.put(name);
        self.put("\"");
    }

    /// What a qualified column names its table by: its alias, or the table's schema name.
    fn entry_name(&mut self, entry: Entry<'s>) {
        if entry.alias {
            self.source_name(entry.name);
        } else {
            self.schema_name(entry.table.name);
        }
    }

    fn placeholder(&mut self, number: u32) {
        self.put(match self.backend {
            Backend::Sqlite => "?",
            Backend::Postgres => "$",
        });
        self.put_u64(u64::from(number));
    }

    fn limit(&self) -> u32 {
        match self.backend {
            Backend::Sqlite => SQLITE_MAX_PLACEHOLDERS,
            Backend::Postgres => POSTGRES_MAX_PLACEHOLDERS,
        }
    }

    /// Adds a placeholder binding `slot`, and gives its number.
    fn push_slot(&mut self, slot: Slot, name: &Name<'_>) -> Result<u32, RenderError> {
        let limit = self.limit();
        if u32::try_from(self.slots.len()).map_or(true, |count| count >= limit) {
            return Err(RenderError::TooManyPlaceholders {
                backend: self.backend,
                limit,
                param: ParamName::new(name.text),
                span: name.span,
            });
        }
        self.slots.push(slot);
        // At most `limit`, which fits a u32.
        Ok(self.slots.len() as u32)
    }

    fn param_index(&self, name: &Name<'_>) -> Result<usize, RenderError> {
        self.params.iter().position(|param| param.name == name.text).ok_or(
            RenderError::NotInDescription { param: ParamName::new(name.text), span: name.span },
        )
    }

    fn param(&mut self, name: &Name<'_>) -> Result<(), RenderError> {
        let index = self.param_index(name)?;
        if self.list_of.get(index) != Some(&NOT_A_LIST) {
            return Err(RenderError::Inconsistent { span: name.span });
        }
        let mut number = self.numbers.get(index).copied().unwrap_or(UNNUMBERED);
        if number == UNNUMBERED {
            // An index into the description, which has fewer parameters than the text has bytes.
            number = self.push_slot(Slot::Scalar { param: index as u32 }, name)?;
            if let Some(entry) = self.numbers.get_mut(index) {
                *entry = number;
            }
        }
        self.placeholder(number);
        Ok(())
    }

    /// ` in $list` after its operand.
    fn list_param(&mut self, name: &Name<'_>, negated: bool) -> Result<(), RenderError> {
        let index = self.param_index(name)?;
        let ord = match self.list_of.get(index) {
            Some(&ord) if ord != NOT_A_LIST => ord,
            _ => return Err(RenderError::Inconsistent { span: name.span }),
        };
        let param = index as u32;
        let number = self.numbers.get(index).copied().unwrap_or(UNNUMBERED);
        match self.backend {
            Backend::Sqlite => {
                if number != UNNUMBERED {
                    return Err(RenderError::ListUsedTwice {
                        param: ParamName::new(name.text),
                        span: name.span,
                    });
                }
                if let Some(entry) = self.numbers.get_mut(index) {
                    *entry = LIST_WRITTEN;
                }
                let Some(&len) = usize::try_from(ord).ok().and_then(|ord| self.list_lens.get(ord))
                else {
                    return Err(RenderError::Inconsistent { span: name.span });
                };
                self.put(if negated { " not in (" } else { " in (" });
                for item in 0..len {
                    if item > 0 {
                        self.put(", ");
                    }
                    let number = self.push_slot(Slot::ListItem { param, index: item }, name)?;
                    self.placeholder(number);
                }
                self.put(")");
            }
            Backend::Postgres => {
                self.put(if negated { " <> all(" } else { " = any(" });
                let number = if number == UNNUMBERED {
                    let number = self.push_slot(Slot::Array { param }, name)?;
                    if let Some(entry) = self.numbers.get_mut(index) {
                        *entry = number;
                    }
                    number
                } else {
                    number
                };
                self.placeholder(number);
                self.put(")");
            }
        }
        Ok(())
    }

    fn statement<X: Allocator>(&mut self, statement: &Statement<'s, X>) -> Result<(), RenderError> {
        match statement {
            Statement::Select(select) => self.select(select),
            Statement::Insert(insert) => self.insert(insert),
            Statement::Update(update) => self.update(update),
            Statement::Delete(delete) => self.delete(delete),
        }
    }

    fn select<X: Allocator>(&mut self, select: &ast::Select<'s, X>) -> Result<(), RenderError> {
        self.put("select ");
        if select.distinct {
            self.put("distinct ");
        }
        if let Some(from) = &select.from {
            self.cx.enter_from(from).map_err(inconsistent)?;
        }
        self.results(&select.columns)?;
        if let Some(from) = &select.from {
            self.put(" from ");
            self.table_ref(&from.first)?;
            for join in from.joins.iter() {
                self.put(match join.kind {
                    JoinKind::Inner => " join ",
                    JoinKind::Left => " left join ",
                });
                self.table_ref(&join.table)?;
                self.put(" on ");
                self.expr(&join.on)?;
            }
        }
        if let Some(filter) = &select.filter {
            self.put(" where ");
            self.expr(filter)?;
        }
        for (index, group) in select.group_by.iter().enumerate() {
            self.put(if index == 0 { " group by " } else { ", " });
            self.expr(group)?;
        }
        if let Some(having) = &select.having {
            self.put(" having ");
            self.expr(having)?;
        }
        for (index, term) in select.order_by.iter().enumerate() {
            self.put(if index == 0 { " order by " } else { ", " });
            match infer::order_alias(&term.expr, &select.columns) {
                Some(alias) => self.source_name(alias.text),
                None => self.expr(&term.expr)?,
            }
            if term.descending {
                self.put(" desc");
            }
            // SQLite puts nulls first in ascending order and last in descending; Postgres
            // the other way unless told.
            if self.backend == Backend::Postgres {
                self.put(if term.descending { " nulls last" } else { " nulls first" });
            }
        }
        match (&select.limit, &select.offset) {
            (Some(limit), offset) => {
                self.put(" limit ");
                self.expr(limit)?;
                if let Some(offset) = offset {
                    self.put(" offset ");
                    self.expr(offset)?;
                }
            }
            // The parser refuses `offset` without `limit`; only a tree built by hand has one.
            (None, Some(offset)) => {
                return Err(RenderError::OffsetWithoutLimit { span: offset.span });
            }
            (None, None) => {}
        }
        Ok(())
    }

    fn table_ref(&mut self, table_ref: &TableRef<'s>) -> Result<(), RenderError> {
        let table = self.cx.table(&table_ref.table).map_err(inconsistent)?;
        self.schema_name(table.name);
        if let Some(alias) = &table_ref.alias {
            self.put(" as ");
            self.source_name(alias.text);
        }
        Ok(())
    }

    /// Result columns, each `expr as "name"`; `*` and `t.*` as their columns.
    fn results<X: Allocator>(
        &mut self,
        columns: &[ResultColumn<'s, X>],
    ) -> Result<(), RenderError> {
        let mut first = true;
        for column in columns {
            match column {
                ResultColumn::Star { .. } => {
                    let mut index = 0;
                    while let Some(entry) = self.cx.entry(index).copied() {
                        self.entry_columns(entry, &mut first);
                        index += 1;
                    }
                }
                ResultColumn::TableStar { table } => {
                    let index = self.cx.find_entry(table).map_err(inconsistent)?;
                    let Some(entry) = self.cx.entry(index).copied() else {
                        return Err(RenderError::Inconsistent { span: table.span });
                    };
                    self.entry_columns(entry, &mut first);
                }
                ResultColumn::Expr { expr, alias } => {
                    if !first {
                        self.put(", ");
                    }
                    first = false;
                    self.expr(expr)?;
                    self.put(" as ");
                    match (alias, &expr.kind) {
                        (Some(alias), _) => self.source_name(alias.text),
                        (None, ExprKind::Column { table, column }) => {
                            let resolved =
                                self.cx.resolve(table.as_ref(), column).map_err(inconsistent)?;
                            self.schema_name(resolved.column.name);
                        }
                        (None, _) => return Err(RenderError::Inconsistent { span: expr.span }),
                    }
                }
            }
        }
        Ok(())
    }

    fn entry_columns(&mut self, entry: Entry<'s>, first: &mut bool) {
        for column in entry.table.columns.iter() {
            if !*first {
                self.put(", ");
            }
            *first = false;
            self.entry_name(entry);
            self.put(".");
            self.schema_name(column.name);
            self.put(" as ");
            self.schema_name(column.name);
        }
    }

    fn returning<X: Allocator>(
        &mut self,
        columns: &[ResultColumn<'s, X>],
    ) -> Result<(), RenderError> {
        if columns.is_empty() {
            return Ok(());
        }
        self.put(" returning ");
        self.results(columns)
    }

    fn insert<X: Allocator>(&mut self, insert: &ast::Insert<'s, X>) -> Result<(), RenderError> {
        let table = self.cx.table(&insert.table).map_err(inconsistent)?;
        self.put("insert into ");
        self.schema_name(table.name);
        self.put(" (");
        for (index, name) in insert.columns.iter().enumerate() {
            if index > 0 {
                self.put(", ");
            }
            let Some(column) = infer::column_of(table, name) else {
                return Err(RenderError::Inconsistent { span: name.span });
            };
            self.schema_name(column.name);
        }
        self.put(") values ");
        for (index, row) in insert.rows.iter().enumerate() {
            self.put(if index == 0 { "(" } else { ", (" });
            for (at, value) in row.iter().enumerate() {
                if at > 0 {
                    self.put(", ");
                }
                self.expr(value)?;
            }
            self.put(")");
        }
        if !insert.returning.is_empty() {
            self.cx.enter_table(&insert.table).map_err(inconsistent)?;
        }
        self.returning(&insert.returning)
    }

    fn update<X: Allocator>(&mut self, update: &ast::Update<'s, X>) -> Result<(), RenderError> {
        let table = self.cx.enter_table(&update.table).map_err(inconsistent)?;
        self.put("update ");
        self.schema_name(table.name);
        for (index, assignment) in update.sets.iter().enumerate() {
            self.put(if index == 0 { " set " } else { ", " });
            let Some(column) = infer::column_of(table, &assignment.column) else {
                return Err(RenderError::Inconsistent { span: assignment.column.span });
            };
            self.schema_name(column.name);
            self.put(" = ");
            self.expr(&assignment.value)?;
        }
        if let Some(filter) = &update.filter {
            self.put(" where ");
            self.expr(filter)?;
        }
        self.returning(&update.returning)
    }

    fn delete<X: Allocator>(&mut self, delete: &ast::Delete<'s, X>) -> Result<(), RenderError> {
        let table = self.cx.enter_table(&delete.table).map_err(inconsistent)?;
        self.put("delete from ");
        self.schema_name(table.name);
        if let Some(filter) = &delete.filter {
            self.put(" where ");
            self.expr(filter)?;
        }
        self.returning(&delete.returning)
    }

    /// An operand of an operator: in parentheses unless it is a single term.
    fn operand<X: Allocator>(&mut self, e: &Expr<'s, X>) -> Result<(), RenderError> {
        #[cfg(test)]
        crate::tests::stack::touch();
        let single = match &e.kind {
            ExprKind::Null
            | ExprKind::Boolean(_)
            | ExprKind::Integer(_)
            | ExprKind::Text(_)
            | ExprKind::Blob(_)
            | ExprKind::Column { .. }
            | ExprKind::Param(_)
            | ExprKind::Case { .. }
            | ExprKind::Cast { .. }
            | ExprKind::Call { .. } => true,
            ExprKind::Real(value) => !value.is_sign_negative(),
            ExprKind::Unary { .. }
            | ExprKind::Chain { .. }
            | ExprKind::Compare { .. }
            | ExprKind::IsNull { .. }
            | ExprKind::InList { .. }
            | ExprKind::InParam { .. }
            | ExprKind::Between { .. }
            | ExprKind::Like { .. } => false,
        };
        if single {
            self.expr(e)
        } else {
            self.put("(");
            self.expr(e)?;
            self.put(")");
            Ok(())
        }
    }

    fn expr<X: Allocator>(&mut self, e: &Expr<'s, X>) -> Result<(), RenderError> {
        #[cfg(test)]
        crate::tests::stack::touch();
        if self.depth >= MAX_DEPTH {
            return Err(RenderError::TooDeep { span: e.span });
        }
        self.depth += 1;
        let written = self.expr_kind(e);
        self.depth -= 1;
        written
    }

    fn expr_kind<X: Allocator>(&mut self, e: &Expr<'s, X>) -> Result<(), RenderError> {
        #[cfg(test)]
        crate::tests::stack::touch();
        let postgres = self.backend == Backend::Postgres;
        match &e.kind {
            ExprKind::Null => self.put("null"),
            ExprKind::Boolean(value) => self.put(if *value { "true" } else { "false" }),
            ExprKind::Integer(value) => {
                self.put_u64(*value);
                if postgres {
                    self.put("::bigint");
                }
            }
            ExprKind::Real(value) => {
                if !value.is_finite() {
                    return Err(RenderError::Inconsistent { span: e.span });
                }
                self.put_real(*value)?;
                if postgres {
                    self.put("::double precision");
                }
            }
            ExprKind::Text(text) => {
                self.put("'");
                self.put(text);
                self.put("'");
            }
            ExprKind::Blob(hex) => {
                if postgres {
                    self.put("'\\x");
                    self.put(hex);
                    self.put("'::bytea");
                } else {
                    self.put("x'");
                    self.put(hex);
                    self.put("'");
                }
            }
            ExprKind::Column { table, column } => {
                let resolved = self.cx.resolve(table.as_ref(), column).map_err(inconsistent)?;
                if table.is_some() {
                    let Some(entry) = self.cx.entry(resolved.entry).copied() else {
                        return Err(RenderError::Inconsistent { span: e.span });
                    };
                    self.entry_name(entry);
                    self.put(".");
                }
                self.schema_name(resolved.column.name);
            }
            ExprKind::Param(name) => self.param(name)?,
            ExprKind::Unary { op: UnaryOp::Neg, operand } => {
                if let ExprKind::Integer(magnitude) = operand.kind
                    && magnitude == i64::MIN.unsigned_abs()
                {
                    // The one integer whose magnitude is no `bigint`: Postgres negates it as a
                    // numeric, SQLite reads `-9223372036854775808` as an integer.
                    self.put(if postgres {
                        "(-9223372036854775808)::bigint"
                    } else {
                        "-9223372036854775808"
                    });
                } else {
                    self.put("-");
                    self.operand(operand)?;
                }
            }
            ExprKind::Unary { op: UnaryOp::Not, operand } => {
                self.put("not ");
                self.operand(operand)?;
            }
            // A chain is written flat, in a loop: the engines read `a op b op c` of one level
            // left to right, as the left-nested tree the chain means.
            ExprKind::Chain { first, rest } => {
                self.operand(first)?;
                for link in rest.iter() {
                    self.put(" ");
                    self.put(infer::chain_text(link.op));
                    self.put(" ");
                    self.operand(&link.operand)?;
                }
            }
            ExprKind::Compare { op, left, right } => {
                self.operand(left)?;
                self.put(match (op, postgres) {
                    (CompareOp::Is, true) => " is not distinct from ",
                    (CompareOp::IsNot, true) => " is distinct from ",
                    (CompareOp::Is, false) => " is ",
                    (CompareOp::IsNot, false) => " is not ",
                    (CompareOp::Eq, _) => " = ",
                    (CompareOp::Ne, _) => " <> ",
                    (CompareOp::Lt, _) => " < ",
                    (CompareOp::Le, _) => " <= ",
                    (CompareOp::Gt, _) => " > ",
                    (CompareOp::Ge, _) => " >= ",
                });
                self.operand(right)?;
            }
            ExprKind::IsNull { operand, negated } => {
                self.operand(operand)?;
                self.put(if *negated { " is not null" } else { " is null" });
            }
            ExprKind::InList { operand, list, negated } => {
                self.operand(operand)?;
                self.put(if *negated { " not in (" } else { " in (" });
                for (index, item) in list.iter().enumerate() {
                    if index > 0 {
                        self.put(", ");
                    }
                    self.expr(item)?;
                }
                self.put(")");
            }
            ExprKind::InParam { operand, list, negated } => {
                self.operand(operand)?;
                self.list_param(list, *negated)?;
            }
            ExprKind::Between { operand, low, high, negated } => {
                self.operand(operand)?;
                self.put(if *negated { " not between " } else { " between " });
                self.operand(low)?;
                self.put(" and ");
                self.operand(high)?;
            }
            ExprKind::Like { operand, pattern, negated } => {
                self.operand(operand)?;
                self.put(if *negated { " not like " } else { " like " });
                self.operand(pattern)?;
                // Postgres's `like` takes `\` as an escape by default; SQLite has none.
                if postgres {
                    self.put(" escape ''");
                }
            }
            ExprKind::Case { operand, arms, otherwise } => {
                self.put("case");
                if let Some(operand) = operand {
                    self.put(" ");
                    self.expr(operand)?;
                }
                for arm in arms.iter() {
                    self.put(" when ");
                    self.expr(&arm.when)?;
                    self.put(" then ");
                    self.expr(&arm.then)?;
                }
                if let Some(otherwise) = otherwise {
                    self.put(" else ");
                    self.expr(otherwise)?;
                }
                self.put(" end");
            }
            ExprKind::Cast { operand, to } => {
                self.put("cast(");
                self.expr(operand)?;
                self.put(" as ");
                self.put(match (to, postgres) {
                    (SqlType::Boolean, true) => "boolean",
                    (SqlType::Integer, true) => "bigint",
                    (SqlType::Real, true) => "double precision",
                    (SqlType::Text, true) => "text",
                    (SqlType::Blob, true) => "bytea",
                    (SqlType::Boolean | SqlType::Integer, false) => "integer",
                    (SqlType::Real, false) => "real",
                    (SqlType::Text, false) => "text",
                    (SqlType::Blob, false) => "blob",
                });
                self.put(")");
            }
            ExprKind::Call { function, args } => self.call(function, args, e.span)?,
        }
        Ok(())
    }

    fn call<X: Allocator>(
        &mut self,
        name: &Name<'s>,
        args: &CallArgs<'s, X>,
        at: Span,
    ) -> Result<(), RenderError> {
        #[cfg(test)]
        crate::tests::stack::touch();
        let Some(function) = funcs::lookup(name.text, name.quoted) else {
            return Err(RenderError::Inconsistent { span: at });
        };
        let cast = match (self.backend, function.postgres) {
            (Backend::Sqlite, _) | (Backend::Postgres, Postgres::AsIs) => None,
            (Backend::Postgres, Postgres::Bigint) => Some(" as bigint)"),
            (Backend::Postgres, Postgres::DoublePrecision) => Some(" as double precision)"),
            (Backend::Postgres, Postgres::BigintOfInteger) => match args {
                CallArgs::List { args, .. } => {
                    let Some(first) = args.first() else {
                        return Err(RenderError::Inconsistent { span: at });
                    };
                    match self.cx.storage_of(first).map_err(inconsistent)? {
                        Storage::Integer => Some(" as bigint)"),
                        _ => None,
                    }
                }
                CallArgs::Star => None,
            },
        };
        if cast.is_some() {
            self.put("cast(");
        }
        self.put(function.name);
        self.put("(");
        match args {
            CallArgs::Star => self.put("*"),
            CallArgs::List { distinct, args } => {
                if *distinct {
                    self.put("distinct ");
                }
                for (index, arg) in args.iter().enumerate() {
                    if index > 0 {
                        self.put(", ");
                    }
                    self.expr(arg)?;
                }
            }
        }
        self.put(")");
        if let Some(cast) = cast {
            self.put(cast);
        }
        Ok(())
    }
}
