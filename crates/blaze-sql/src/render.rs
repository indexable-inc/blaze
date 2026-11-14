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

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::alloc::Allocator;
use core::fmt;

use crate::Checked;
use crate::ast::{
    self, CallArgs, CompareOp, Expr, ExprKind, JoinKind, Name, ResultColumn, Span, Statement,
    TableRef, UnaryOp,
};
use crate::desc::{Backend, Param, Rendered, Slot, Storage};
use crate::funcs::{self, Postgres};
use crate::infer::{self, Cx, Entry, TypeError, keyword};
// The one bound on nesting, the parser's: rendering recurses once per level, and the checker
// refused anything deeper already.
use crate::parse::MAX_DEPTH;

/// SQLite's most placeholders in one statement (`SQLITE_MAX_VARIABLE_NUMBER`'s default).
pub const SQLITE_MAX_PLACEHOLDERS: u32 = 32766;

/// Postgres's most placeholders in one statement (the wire protocol's 16-bit count).
pub const POSTGRES_MAX_PLACEHOLDERS: u32 = 65535;

/// How many bytes of a parameter's name an error keeps.
const PARAM_NAME_BYTES: usize = 47;

/// The engines' names of the types a `cast` writes, each written once.
mod engine_type {
    /// Postgres's boolean.
    pub(super) const BOOLEAN: &str = "boolean";
    /// Postgres's 64-bit integer, the Integer storage.
    pub(super) const BIGINT: &str = "bigint";
    /// Postgres's byte string, the Blob storage.
    pub(super) const BYTEA: &str = "bytea";
    /// Text, on both engines.
    pub(super) const TEXT: &str = "text";
    /// SQLite's integer, which also holds a Boolean as 0 or 1.
    pub(super) const INTEGER: &str = "integer";
    /// SQLite's 64-bit float.
    pub(super) const REAL: &str = "real";
    /// SQLite's byte string.
    pub(super) const BLOB: &str = "blob";
}

/// What a `cast` opens with, before its operand.
const CAST_OPEN: &str = "cast(";

/// What a SQLite blob literal opens with, before its hex digits.
const SQLITE_BLOB_OPEN: &str = "x'";

/// SQLite's name for itself, which its variant's name (`Sqlite`) does not spell.
const SQLITE_NAME: &str = "SQLite";

/// Postgres's name for itself.
const POSTGRES_NAME: &str = "Postgres";

/// A parameter's name kept in an error, which borrows nothing: its first
/// [`PARAM_NAME_BYTES`] bytes, cut at a character boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParamName {
    bytes: [u8; PARAM_NAME_BYTES],
    /// How many of `bytes` are the name, at most [`PARAM_NAME_BYTES`].
    len: usize,
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
        ParamName { bytes, len, cut: len < name.len() }
    }

    /// The name kept, without its `$`. The bytes kept end at a character boundary, so their
    /// first run of valid UTF-8 is all of them.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn as_str(&self) -> &str {
        self.bytes
            .get(..self.len)
            .and_then(|bytes| bytes.utf8_chunks().next())
            .map_or("", |chunk| chunk.valid())
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
    ListLens { expected: usize, found: usize },
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
    Format(fmt::Error),
}

fn backend_name(backend: Backend) -> &'static str {
    match backend {
        Backend::Sqlite => SQLITE_NAME,
        Backend::Postgres => POSTGRES_NAME,
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
            RenderError::TooManyPlaceholders {
                backend: backend @ (Backend::Sqlite | Backend::Postgres),
                limit,
                param,
                span,
            } => write!(
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
            RenderError::Format(error) => {
                write!(f, "writing a number into the statement's text failed: {error}")
            }
        }
    }
}

/// Writes `checked` as `backend`'s text. `list_lens` holds the length of each list parameter,
/// in the order the list parameters stand in the description.
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn render<A: Allocator + Copy, B: Allocator + Copy>(
    checked: &Checked<'_, A>,
    backend: Backend,
    list_lens: &[u32],
    alloc: B,
) -> Result<Rendered<B>, RenderError> {
    let params = &checked.description.params[..];
    let mut list_of = Vec::with_capacity_in(params.len(), alloc);
    let mut lists: usize = 0;
    for param in params {
        if param.list {
            list_of.push(Some(lists));
            lists = lists.saturating_add(1);
        } else {
            list_of.push(None);
        }
    }
    if lists != list_lens.len() {
        return Err(RenderError::ListLens { expected: lists, found: list_lens.len() });
    }
    let mut numbers = Vec::with_capacity_in(params.len(), alloc);
    numbers.resize(params.len(), UNNUMBERED);
    let mut writer = Writer {
        backend,
        params,
        list_lens,
        list_of: list_of.into_boxed_slice(),
        numbers,
        text: Vec::new_in(alloc),
        slots: Vec::new_in(alloc),
        placeholders: 0,
        cx: Cx::new(&checked.schema, None, alloc),
        depth: 0,
    };
    writer.statement(&checked.statement)?;
    Ok(Rendered { text: writer.text, slots: writer.slots })
}

/// In `numbers`: the parameter has no placeholder yet.
const UNNUMBERED: u32 = 0;
/// In `numbers`: a SQLite list parameter, written once.
const LIST_WRITTEN: u32 = u32::MAX;

/// The base numbers are written in.
const RADIX: u64 = 10;

/// The most decimal digits a `u64` has: `u64::MAX` is 18446744073709551615, 20 digits.
const U64_DIGITS: usize = 20;

/// Whether a list parameter is `in $list` or `not in $list`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Membership {
    In,
    NotIn,
}

/// A parameter of the description: its index there, and that index as a [`Slot`] names it.
#[derive(Clone, Copy, Debug)]
struct ParamAt {
    index: usize,
    slot: u32,
}

/// A statement that does not type against its own schema while it is rendered: the checker
/// typed it, so this is a bug in the checker, at the error's span.
impl From<TypeError<'_>> for RenderError {
    fn from(error: TypeError<'_>) -> RenderError {
        RenderError::Inconsistent { span: error.span }
    }
}

/// A `fmt::Write` into the text, for a real's digits.
struct Bytes<'a, B: Allocator>(&'a mut Vec<u8, B>);

impl<B: Allocator> fmt::Write for Bytes<'_, B> {
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

struct Writer<'c, 's, B: Allocator + Copy> {
    backend: Backend,
    params: &'c [Param<'s>],
    list_lens: &'c [u32],
    /// Per parameter: its index in `list_lens`, or `None` for a scalar.
    list_of: Box<[Option<usize>], B>,
    /// Per parameter: its placeholder number, [`UNNUMBERED`] or [`LIST_WRITTEN`].
    numbers: Vec<u32, B>,
    text: Vec<u8, B>,
    slots: Vec<Slot, B>,
    /// How many placeholders `slots` holds, as their numbers count.
    placeholders: u32,
    /// The typer, run again for the scope and a `sum`'s argument.
    cx: Cx<'s, B>,
    depth: u32,
}

impl<'s, B: Allocator + Copy> Writer<'_, 's, B> {
    fn put(&mut self, s: &str) {
        self.text.extend_from_slice(s.as_bytes());
    }

    /// `word` with a space on each side: a keyword between two parts of the text.
    fn spaced(&mut self, word: &str) {
        self.put(" ");
        self.put(word);
        self.put(" ");
    }

    fn put_u64(&mut self, mut value: u64) {
        let mut digits = [0u8; U64_DIGITS];
        let mut at = digits.len();
        loop {
            at -= 1;
            if let Some(digit) = digits.get_mut(at) {
                // `value % RADIX` is below 10, so its lowest byte is all of it.
                let [low, ..] = (value % RADIX).to_le_bytes();
                *digit = b'0' + low;
            }
            value /= RADIX;
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
            .map_err(RenderError::Format)
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
            self.source_name(entry.name.text);
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
        if self.placeholders >= limit {
            return Err(RenderError::TooManyPlaceholders {
                backend: self.backend,
                limit,
                param: ParamName::new(name.text),
                span: name.span,
            });
        }
        self.slots.push(slot);
        // Below `limit`, so one more fits a u32.
        self.placeholders += 1;
        Ok(self.placeholders)
    }

    fn param_index(&self, name: &Name<'_>) -> Result<ParamAt, RenderError> {
        self.params
            .iter()
            .enumerate()
            .zip(0u32..)
            .find(|((_, param), _)| param.name == name.text)
            .map(|((index, _), slot)| ParamAt { index, slot })
            .ok_or(RenderError::NotInDescription {
                param: ParamName::new(name.text),
                span: name.span,
            })
    }

    fn param(&mut self, name: &Name<'_>) -> Result<(), RenderError> {
        let ParamAt { index, slot } = self.param_index(name)?;
        if self.list_of.get(index) != Some(&None) {
            return Err(RenderError::Inconsistent { span: name.span });
        }
        let mut number = self.numbers.get(index).copied().unwrap_or(UNNUMBERED);
        if number == UNNUMBERED {
            number = self.push_slot(Slot::Scalar { param: slot }, name)?;
            if let Some(entry) = self.numbers.get_mut(index) {
                *entry = number;
            }
        }
        self.placeholder(number);
        Ok(())
    }

    /// ` in $list` after its operand.
    fn list_param(&mut self, name: &Name<'_>, membership: Membership) -> Result<(), RenderError> {
        let ParamAt { index, slot: param } = self.param_index(name)?;
        let Some(&Some(ord)) = self.list_of.get(index) else {
            return Err(RenderError::Inconsistent { span: name.span });
        };
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
                let Some(&len) = self.list_lens.get(ord) else {
                    return Err(RenderError::Inconsistent { span: name.span });
                };
                self.put(match membership {
                    Membership::In => " in (",
                    Membership::NotIn => " not in (",
                });
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
                self.put(match membership {
                    Membership::In => " = any(",
                    Membership::NotIn => " <> all(",
                });
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
        self.put(keyword::SELECT);
        self.put(" ");
        if select.distinct {
            self.put(keyword::DISTINCT);
            self.put(" ");
        }
        if let Some(from) = &select.from {
            self.cx.enter_from(from)?;
        }
        self.results(&select.columns)?;
        if let Some(from) = &select.from {
            self.spaced(keyword::FROM);
            self.table_ref(&from.first)?;
            for join in from.joins.iter() {
                match join.kind {
                    JoinKind::Inner => self.spaced(keyword::JOIN),
                    JoinKind::Left => self.put(" left join "),
                }
                self.table_ref(&join.table)?;
                self.spaced(keyword::ON);
                self.expr(&join.on)?;
            }
        }
        if let Some(filter) = &select.filter {
            self.spaced(keyword::WHERE);
            self.expr(filter)?;
        }
        for (index, group) in select.group_by.iter().enumerate() {
            self.put(if index == 0 { " group by " } else { ", " });
            self.expr(group)?;
        }
        if let Some(having) = &select.having {
            self.spaced(keyword::HAVING);
            self.expr(having)?;
        }
        for (index, term) in select.order_by.iter().enumerate() {
            self.put(if index == 0 { " order by " } else { ", " });
            match infer::order_alias(&term.expr, &select.columns) {
                Some(alias) => self.source_name(alias.text),
                None => self.expr(&term.expr)?,
            }
            if term.descending {
                self.put(" ");
                self.put(keyword::DESC);
            }
            // SQLite puts nulls first in ascending order and last in descending; Postgres
            // the other way unless told.
            if self.backend == Backend::Postgres {
                self.put(if term.descending { " nulls last" } else { " nulls first" });
            }
        }
        match (&select.limit, &select.offset) {
            (Some(limit), offset) => {
                self.spaced(keyword::LIMIT);
                self.expr(limit)?;
                if let Some(offset) = offset {
                    self.spaced(keyword::OFFSET);
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
        let table = self.cx.table(&table_ref.table)?;
        self.schema_name(table.name);
        if let Some(alias) = &table_ref.alias {
            self.spaced(keyword::AS);
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
                    let index = self.cx.find_entry(table)?;
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
                    self.spaced(keyword::AS);
                    match alias {
                        Some(alias) => self.source_name(alias.text),
                        None => match &expr.kind {
                            ExprKind::Column { table, column } => {
                                let resolved = self.cx.resolve(table.as_ref(), column)?;
                                self.schema_name(resolved.column.name);
                            }
                            ExprKind::Null
                            | ExprKind::Boolean(_)
                            | ExprKind::Integer(_)
                            | ExprKind::Real(_)
                            | ExprKind::Text(_)
                            | ExprKind::Blob(_)
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
                            | ExprKind::Call { .. } => {
                                return Err(RenderError::Inconsistent { span: expr.span });
                            }
                        },
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
            self.spaced(keyword::AS);
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
        self.spaced(keyword::RETURNING);
        self.results(columns)
    }

    fn insert<X: Allocator>(&mut self, insert: &ast::Insert<'s, X>) -> Result<(), RenderError> {
        let table = self.cx.table(&insert.table)?;
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
            self.cx.enter_table(&insert.table)?;
        }
        self.returning(&insert.returning)
    }

    fn update<X: Allocator>(&mut self, update: &ast::Update<'s, X>) -> Result<(), RenderError> {
        let table = self.cx.enter_table(&update.table)?;
        self.put(keyword::UPDATE);
        self.put(" ");
        self.schema_name(table.name);
        for (index, assignment) in update.sets.iter().enumerate() {
            if index == 0 {
                self.spaced(keyword::SET);
            } else {
                self.put(", ");
            }
            let Some(column) = infer::column_of(table, &assignment.column) else {
                return Err(RenderError::Inconsistent { span: assignment.column.span });
            };
            self.schema_name(column.name);
            self.put(" = ");
            self.expr(&assignment.value)?;
        }
        if let Some(filter) = &update.filter {
            self.spaced(keyword::WHERE);
            self.expr(filter)?;
        }
        self.returning(&update.returning)
    }

    fn delete<X: Allocator>(&mut self, delete: &ast::Delete<'s, X>) -> Result<(), RenderError> {
        let table = self.cx.enter_table(&delete.table)?;
        self.put("delete from ");
        self.schema_name(table.name);
        if let Some(filter) = &delete.filter {
            self.spaced(keyword::WHERE);
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
            ExprKind::Null => self.put(keyword::NULL),
            ExprKind::Boolean(value) => {
                self.put(if *value { keyword::TRUE } else { keyword::FALSE });
            }
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
                    self.put(SQLITE_BLOB_OPEN);
                    self.put(hex);
                    self.put("'");
                }
            }
            ExprKind::Column { table, column } => {
                let resolved = self.cx.resolve(table.as_ref(), column)?;
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
                self.put(keyword::NOT);
                self.put(" ");
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
                self.operand(left)?;
                match (op, postgres) {
                    // Postgres's `is` takes only `null`, `true` and `false`; its `is not
                    // distinct from` is SQLite's `is`.
                    (CompareOp::Is, true) => self.put(" is not distinct from "),
                    (CompareOp::IsNot, true) => self.put(" is distinct from "),
                    (CompareOp::Is | CompareOp::IsNot, false)
                    | (
                        CompareOp::Eq
                        | CompareOp::Ne
                        | CompareOp::Lt
                        | CompareOp::Le
                        | CompareOp::Gt
                        | CompareOp::Ge,
                        true | false,
                    ) => self.spaced(infer::compare_text(*op)),
                }
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
                let membership = if *negated { Membership::NotIn } else { Membership::In };
                self.list_param(list, membership)?;
            }
            ExprKind::Between { operand, low, high, negated } => {
                self.operand(operand)?;
                if *negated {
                    self.put(" ");
                    self.put(keyword::NOT);
                }
                self.spaced(keyword::BETWEEN);
                self.operand(low)?;
                self.spaced(keyword::AND);
                self.operand(high)?;
            }
            ExprKind::Like { operand, pattern, negated } => {
                self.operand(operand)?;
                if *negated {
                    self.put(" ");
                    self.put(keyword::NOT);
                }
                self.spaced(keyword::LIKE);
                self.operand(pattern)?;
                // Postgres's `like` takes `\` as an escape by default; SQLite has none.
                if postgres {
                    self.put(" escape ''");
                }
            }
            ExprKind::Case { operand, arms, otherwise } => {
                self.put(keyword::CASE);
                if let Some(operand) = operand {
                    self.put(" ");
                    self.expr(operand)?;
                }
                for arm in arms.iter() {
                    self.spaced(keyword::WHEN);
                    self.expr(&arm.when)?;
                    self.spaced(keyword::THEN);
                    self.expr(&arm.then)?;
                }
                if let Some(otherwise) = otherwise {
                    self.spaced(keyword::ELSE);
                    self.expr(otherwise)?;
                }
                self.put(" ");
                self.put(keyword::END);
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
                self.put(CAST_OPEN);
                self.expr(operand)?;
                self.spaced(keyword::AS);
                self.put(match (to, postgres) {
                    (Storage::Boolean, true) => engine_type::BOOLEAN,
                    (Storage::Integer, true) => engine_type::BIGINT,
                    (Storage::Real, true) => "double precision",
                    (Storage::Text, true) => engine_type::TEXT,
                    (Storage::Blob, true) => engine_type::BYTEA,
                    (Storage::Boolean | Storage::Integer, false) => engine_type::INTEGER,
                    (Storage::Real, false) => engine_type::REAL,
                    (Storage::Text, false) => engine_type::TEXT,
                    (Storage::Blob, false) => engine_type::BLOB,
                });
                self.put(")");
            }
            ExprKind::Call { function, args: args @ (CallArgs::Star | CallArgs::List { .. }) } => {
                self.call(function, args, e.span)?
            }
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
        let Some(function) = funcs::lookup(name) else {
            return Err(RenderError::Inconsistent { span: at });
        };
        let cast = match (self.backend, function.postgres) {
            (
                Backend::Sqlite,
                Postgres::AsIs
                | Postgres::Bigint
                | Postgres::DoublePrecision
                | Postgres::BigintOfInteger,
            )
            | (Backend::Postgres, Postgres::AsIs) => None,
            (Backend::Postgres, Postgres::Bigint) => Some(" as bigint)"),
            (Backend::Postgres, Postgres::DoublePrecision) => Some(" as double precision)"),
            (Backend::Postgres, Postgres::BigintOfInteger) => match args {
                CallArgs::List { args, .. } => {
                    let Some(first) = args.first() else {
                        return Err(RenderError::Inconsistent { span: at });
                    };
                    match self.cx.storage_of(first)? {
                        Storage::Integer => Some(" as bigint)"),
                        Storage::Boolean | Storage::Real | Storage::Text | Storage::Blob => None,
                    }
                }
                CallArgs::Star => None,
            },
        };
        if cast.is_some() {
            self.put(CAST_OPEN);
        }
        self.put(function.name);
        self.put("(");
        match args {
            CallArgs::Star => self.put("*"),
            CallArgs::List { distinct, args } => {
                if *distinct {
                    self.put(keyword::DISTINCT);
                    self.put(" ");
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
