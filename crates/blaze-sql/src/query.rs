//! The query grammar's tree (docs/v2/plan/design/query.md): what [`crate::parse::parse_script`]
//! and [`crate::parse::parse_query`] make for blaze-rel, the profile's query engine. It is the
//! analytic SQL the profile's views are written in (DuckDB's dialect, as far as they use it):
//! `with [recursive]`, set operations, derived tables, every join form they use, `filter`,
//! windows, `qualify`, `group by all`, subqueries and `::`.
//!
//! Expressions are the subset's [`Expr`]; the forms only this grammar has are one more variant,
//! [`ExprKind::Analytic`](crate::ast::ExprKind::Analytic), holding an [`Analytic`]. The typed
//! subset's checker refuses that variant by name, so nothing here reaches an engine through
//! [`crate::check`]. When the typed surface takes subqueries, its `Select` folds into [`Core`].

#![blaze::sans_io]

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::alloc::Allocator;

use crate::ast::{CallArgs, Expr, Name, ResultColumn, Span};

/// One statement of a script.
#[derive(Debug)]
pub enum Statement<'s, A: Allocator> {
    /// A query.
    Query(Query<'s, A>),
    /// `create [or replace] view name as query`.
    CreateView { name: Name<'s>, replace: bool, query: Query<'s, A>, span: Span },
}

/// `[with ..] body [order by ..] [limit ..] [offset ..]`.
#[derive(Debug)]
pub struct Query<'s, A: Allocator> {
    pub with: Option<With<'s, A>>,
    pub body: SetExpr<'s, A>,
    pub order_by: Vec<Order<'s, A>, A>,
    pub limit: Option<Expr<'s, A>>,
    pub offset: Option<Expr<'s, A>>,
    pub span: Span,
}

/// `with [recursive] cte, ..`.
#[derive(Debug)]
pub struct With<'s, A: Allocator> {
    pub recursive: bool,
    pub ctes: Vec<Cte<'s, A>, A>,
}

/// `name [(column, ..)] as (query)`.
#[derive(Debug)]
pub struct Cte<'s, A: Allocator> {
    pub name: Name<'s>,
    pub columns: Vec<Name<'s>, A>,
    pub query: Query<'s, A>,
}

/// A `select`, a parenthesized query, or a set operation of two.
#[derive(Debug)]
pub enum SetExpr<'s, A: Allocator> {
    Core(Box<Core<'s, A>, A>),
    Nested(Box<Query<'s, A>, A>),
    SetOp {
        op: SetOp,
        /// `all`: keep duplicates.
        all: bool,
        /// DuckDB's `by name`: columns matched by name, not position.
        by_name: bool,
        left: Box<SetExpr<'s, A>, A>,
        right: Box<SetExpr<'s, A>, A>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOp {
    Union,
    Except,
    Intersect,
}

/// One `select .. from .. where .. group by .. having .. qualify ..`.
#[derive(Debug)]
pub struct Core<'s, A: Allocator> {
    pub distinct: bool,
    pub columns: Vec<ResultColumn<'s, A>, A>,
    /// The comma-separated `from` items; each may be a join tree. Empty without `from`.
    pub from: Vec<FromItem<'s, A>, A>,
    pub filter: Option<Expr<'s, A>>,
    pub group_by: GroupBy<'s, A>,
    pub having: Option<Expr<'s, A>>,
    pub qualify: Option<Expr<'s, A>>,
    pub span: Span,
}

#[derive(Debug)]
pub enum GroupBy<'s, A: Allocator> {
    /// No `group by`.
    None,
    Exprs(Vec<Expr<'s, A>, A>),
    /// DuckDB's `group by all`: every result column without an aggregate.
    All(Span),
}

/// What a `from` names.
#[derive(Debug)]
pub enum FromItem<'s, A: Allocator> {
    /// A table, view or CTE.
    Table { name: Name<'s>, alias: Option<Name<'s>> },
    /// A table function: `read_parquet(@t, ..)`, `range(..)`.
    Function { function: Name<'s>, args: Vec<Expr<'s, A>, A>, alias: Option<Name<'s>>, span: Span },
    /// `(query) [as] alias`.
    Subquery { query: Box<Query<'s, A>, A>, alias: Option<Name<'s>> },
    Join(Box<Join<'s, A>, A>),
}

#[derive(Debug)]
pub struct Join<'s, A: Allocator> {
    pub kind: JoinKind,
    pub left: FromItem<'s, A>,
    pub right: FromItem<'s, A>,
    pub constraint: Constraint<'s, A>,
    /// The join's keyword, for errors.
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
    /// DuckDB's `asof join`: each left row meets the one right row nearest under the
    /// constraint's one inequality.
    Asof,
    /// `asof left join`: a left row with no such right row stays, with nulls.
    AsofLeft,
}

#[derive(Debug)]
pub enum Constraint<'s, A: Allocator> {
    On(Expr<'s, A>),
    Using(Vec<Name<'s>, A>),
    /// A cross join's.
    None,
}

/// An `order by` term.
#[derive(Debug)]
pub struct Order<'s, A: Allocator> {
    pub expr: Expr<'s, A>,
    pub descending: bool,
    /// `nulls first` (`Some(true)`) or `nulls last`; `None` is the default, last (DuckDB's).
    pub nulls_first: Option<bool>,
}

/// The expression forms only the query grammar has.
#[derive(Debug)]
pub enum Analytic<'s, A: Allocator> {
    /// `(query)` as a value: one column, at most one row.
    Subquery(Query<'s, A>),
    /// `exists (query)`; `not exists` is `not` around it.
    Exists(Query<'s, A>),
    /// `operand [not] in (query)`.
    InQuery { operand: Expr<'s, A>, query: Query<'s, A>, negated: bool },
    /// A call with an inner `order by`, a `filter` or an `over`.
    Aggregate(Aggregate<'s, A>),
    /// `cast(operand as type)` or `operand::type` to a type of this grammar.
    Convert { operand: Expr<'s, A>, to: TypeName },
    /// `@name`: the files of table `name` (a `read_parquet` argument).
    Files(Name<'s>),
}

impl<A: Allocator> Analytic<'_, A> {
    /// What it is, for a message.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn what(&self) -> &'static str {
        match self {
            Analytic::Subquery(_) => "a subquery",
            Analytic::Exists(_) => "`exists`",
            Analytic::InQuery { .. } => "`in (select ..)`",
            Analytic::Aggregate(Aggregate { over: Some(_), .. }) => "a window function",
            Analytic::Aggregate(Aggregate { over: None, .. }) => {
                "an aggregate with `filter` or `order by`"
            }
            Analytic::Convert { .. } => "a cast to a query type",
            Analytic::Files(_) => "`@table`",
        }
    }
}

/// `function(args [order by ..]) [filter (where ..)] [over (..)]`.
#[derive(Debug)]
pub struct Aggregate<'s, A: Allocator> {
    pub function: Name<'s>,
    pub args: CallArgs<'s, A>,
    pub order_by: Vec<Order<'s, A>, A>,
    pub filter: Option<Expr<'s, A>>,
    pub over: Option<Window<'s, A>>,
}

/// `over ([partition by ..] [order by ..] [frame])`.
#[derive(Debug)]
pub struct Window<'s, A: Allocator> {
    pub partition_by: Vec<Expr<'s, A>, A>,
    pub order_by: Vec<Order<'s, A>, A>,
    pub frame: Option<Frame<'s, A>>,
}

/// `rows|range between start and end`, or `rows|range start` (whose end is the current row).
#[derive(Debug)]
pub struct Frame<'s, A: Allocator> {
    pub units: FrameUnits,
    pub start: Bound<'s, A>,
    pub end: Bound<'s, A>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameUnits {
    Rows,
    Range,
}

#[derive(Debug)]
pub enum Bound<'s, A: Allocator> {
    UnboundedPreceding,
    Preceding(Expr<'s, A>),
    CurrentRow,
    Following(Expr<'s, A>),
    UnboundedFollowing,
}

/// A type of `cast` or `::` in the query grammar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeName {
    Boolean,
    /// An integer of `bits` bits, signed or not: `bigint` is 64 signed, `ubigint` 64
    /// unsigned, `hugeint` 128 signed.
    Int { bits: u8, signed: bool },
    /// `double`, `real`, `float`: a 64-bit float.
    Float,
    /// `varchar`, `text`, `string`.
    Text,
}

impl TypeName {
    const NAMES: [(&'static str, TypeName); 19] = [
        ("boolean", TypeName::Boolean),
        ("bool", TypeName::Boolean),
        ("tinyint", TypeName::Int { bits: 8, signed: true }),
        ("smallint", TypeName::Int { bits: 16, signed: true }),
        ("integer", TypeName::Int { bits: 32, signed: true }),
        ("int", TypeName::Int { bits: 32, signed: true }),
        ("bigint", TypeName::Int { bits: 64, signed: true }),
        ("hugeint", TypeName::Int { bits: 128, signed: true }),
        ("utinyint", TypeName::Int { bits: 8, signed: false }),
        ("usmallint", TypeName::Int { bits: 16, signed: false }),
        ("uinteger", TypeName::Int { bits: 32, signed: false }),
        ("ubigint", TypeName::Int { bits: 64, signed: false }),
        ("double", TypeName::Float),
        ("real", TypeName::Float),
        ("float", TypeName::Float),
        ("float8", TypeName::Float),
        ("varchar", TypeName::Text),
        ("text", TypeName::Text),
        ("string", TypeName::Text),
    ];

    /// The type spelled `name`, without regard to ASCII case.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn named(name: &str) -> Option<TypeName> {
        TypeName::NAMES.iter().find(|(text, _)| text.eq_ignore_ascii_case(name)).map(|&(_, ty)| ty)
    }
}
