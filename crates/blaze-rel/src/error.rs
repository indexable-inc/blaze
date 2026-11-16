//! Why a query did not answer: what, where (the statement and the byte), and the rule
//! (rule 19). Nothing is retried or answered another way (rule 3).

use core::alloc::Allocator;
use core::fmt;

use blaze_own::String;
use blaze_sql::parse::SyntaxError;

use crate::datum::Kind;
use crate::plan::{Loc, Source};

/// A refusal of one query.
#[derive(Debug)]
pub struct QueryError<'s, A: Allocator> {
    /// The statement it is about: the query's first line, or the view's name.
    pub statement: &'s str,
    /// Byte offsets in the statement's text (the query's, or the views' prelude's).
    pub at: Option<(u32, u32)>,
    pub kind: ErrorKind<'s, A>,
}

#[derive(Debug)]
pub enum ErrorKind<'s, A: Allocator> {
    /// The text is not of the query grammar.
    Syntax(SyntaxError<'s>),
    /// The query is not one statement, or the views' prelude holds something but views.
    NotOneQuery { found: usize },
    /// A form the engine does not run yet (a correlated subquery, `unnest`, a list type, ..).
    Unsupported(&'static str),
    UnknownTable(&'s str),
    UnknownColumn { table: Option<&'s str>, column: &'s str },
    AmbiguousColumn(&'s str),
    /// A column outside an aggregate that `group by` does not cover.
    NotGrouped(&'s str),
    UnknownFunction(&'s str),
    /// An aggregate or window function where none may stand (in `where`, inside another).
    AggregateNotAllowed(&'s str),
    Arity { function: &'s str, found: usize },
    /// A parameter `$name` the caller did not give.
    MissingParam(&'s str),
    /// Two inputs of a set operation, or a CTE and its column list, of different widths.
    Width { left: usize, right: usize },
    /// An operator over kinds it does not relate.
    Type { op: &'static str, left: Kind, right: Kind },
    /// Integer arithmetic past the 64-bit range, or a cast out of its type's range.
    Overflow(&'static str),
    /// A text that is not a number where one was cast from it.
    NotANumber,
    /// A scalar subquery of more than one row or column.
    NotScalar { rows: usize, columns: usize },
    /// `error(message)` was called.
    Raised(String<A>),
    /// A query past a bound of [`crate::Bounds`] (rule 18).
    Bound { what: &'static str, most: u64 },
    /// A recursive CTE past a bound (rule 18).
    Recursion { cte: &'s str, what: &'static str, most: u64 },
    /// A table could not hand over a chunk.
    Read { table: &'static str, chunk: usize, why: &'static str },
}

impl<A: Allocator> ErrorKind<'_, A> {
    /// The rule that refuses it.
    pub fn rule(&self) -> &'static str {
        match self {
            ErrorKind::Bound { .. } | ErrorKind::Recursion { .. } => "rule 18 (bounded-progress)",
            ErrorKind::Unsupported(_) => "rule 3 (no-fallback): not run another way",
            ErrorKind::Read { .. } => "rule 19 (errors-name-the-thing)",
            ErrorKind::Syntax(_)
            | ErrorKind::NotOneQuery { .. }
            | ErrorKind::UnknownTable(_)
            | ErrorKind::UnknownColumn { .. }
            | ErrorKind::AmbiguousColumn(_)
            | ErrorKind::NotGrouped(_)
            | ErrorKind::UnknownFunction(_)
            | ErrorKind::AggregateNotAllowed(_)
            | ErrorKind::Arity { .. }
            | ErrorKind::MissingParam(_)
            | ErrorKind::Width { .. }
            | ErrorKind::Type { .. }
            | ErrorKind::Overflow(_)
            | ErrorKind::NotANumber
            | ErrorKind::NotScalar { .. }
            | ErrorKind::Raised(_) => "SQL semantics (query.md)",
        }
    }
}

impl<A: Allocator> fmt::Display for ErrorKind<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorKind::Syntax(error) => write!(f, "{error}"),
            ErrorKind::NotOneQuery { found } => {
                write!(f, "the text holds {found} statements where one query was expected")
            }
            ErrorKind::Unsupported(what) => write!(f, "{what} is not run by blaze-rel yet"),
            ErrorKind::UnknownTable(name) => write!(f, "no table, view or CTE `{name}`"),
            ErrorKind::UnknownColumn { table: Some(table), column } => {
                write!(f, "no column `{table}.{column}`")
            }
            ErrorKind::UnknownColumn { table: None, column } => write!(f, "no column `{column}`"),
            ErrorKind::AmbiguousColumn(name) => {
                write!(f, "column `{name}` is in two tables in scope: qualify it")
            }
            ErrorKind::NotGrouped(name) => write!(
                f,
                "column `{name}` is neither grouped nor inside an aggregate: add it to `group by` \
                 or wrap it in `any_value(..)`"
            ),
            ErrorKind::UnknownFunction(name) => write!(f, "no function `{name}`"),
            ErrorKind::AggregateNotAllowed(name) => {
                write!(f, "aggregate `{name}` stands where no aggregate may")
            }
            ErrorKind::Arity { function, found } => {
                write!(f, "`{function}` does not take {found} arguments")
            }
            ErrorKind::MissingParam(name) => write!(f, "parameter `${name}` was not given"),
            ErrorKind::Width { left, right } => {
                write!(f, "{left} columns on one side and {right} on the other")
            }
            ErrorKind::Type { op, left, right } => {
                write!(f, "`{op}` of {} and {}", left.name(), right.name())
            }
            ErrorKind::Overflow(what) => write!(f, "{what} is outside the 64-bit integers"),
            ErrorKind::NotANumber => f.write_str("text that is not a number cast to a number"),
            ErrorKind::NotScalar { rows, columns } => write!(
                f,
                "a scalar subquery gave {rows} rows of {columns} columns: it gives at most one \
                 row of one column"
            ),
            ErrorKind::Raised(message) => write!(f, "error(): {}", message.as_str()),
            ErrorKind::Bound { what, most } => write!(f, "past the bound of {most} {what}"),
            ErrorKind::Recursion { cte, what, most } => {
                write!(f, "recursive `{cte}` went past the bound of {most} {what}")
            }
            ErrorKind::Read { table, chunk, why } => {
                write!(f, "table `{table}` could not read chunk {chunk}: {why}")
            }
        }
    }
}

impl<A: Allocator> fmt::Display for QueryError<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.kind)?;
        write!(f, " (in `{}`", self.statement)?;
        if let Some((start, end)) = self.at {
            write!(f, ", bytes {start}..{end}")?;
        }
        write!(f, "; {})", self.kind.rule())
    }
}

/// An error before its statement is named: where, by source, and what.
#[derive(Debug)]
pub(crate) struct Fail<'s, A: Allocator> {
    pub at: Option<Loc>,
    pub kind: ErrorKind<'s, A>,
}

pub(crate) type Run<'s, T, A> = Result<T, Fail<'s, A>>;

pub(crate) fn fail<'s, T, A: Allocator>(at: Loc, kind: ErrorKind<'s, A>) -> Run<'s, T, A> {
    Err(Fail { at: Some(at), kind })
}

impl<'s, A: Allocator> Fail<'s, A> {
    pub(crate) fn nowhere(kind: ErrorKind<'s, A>) -> Fail<'s, A> {
        Fail { at: None, kind }
    }

    /// Names the statement: `query` for the query's text, `views[i]` for view `i`.
    pub(crate) fn named(self, query: &'s str, views: &[&'s str]) -> QueryError<'s, A> {
        let statement = match self.at.map(|it| it.source) {
            Some(Source::View(at)) => views.get(at as usize).copied().unwrap_or(query),
            Some(Source::Query) | None => query,
        };
        QueryError { statement, at: self.at.map(|it| (it.start, it.end)), kind: self.kind }
    }
}
