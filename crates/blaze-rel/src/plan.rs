//! The plan: relational operators over relations of datum columns, and the scalar expressions
//! they evaluate, every name bound to a column index (docs/v2/plan/design/query.md, "The plan
//! IR"). The executor ([`crate::exec`]) runs a plan as written; the strategy of each operator
//! (hash or nested loop, which side to build) is chosen as it runs.

use core::alloc::Allocator;

use blaze_own::{Box, Vec};
use blaze_sql::query::TypeName;

use crate::datum::Datum;

/// Which text a position is in: the query, or a view of the prelude by its index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Query,
    View(u32),
}

/// A byte range of a source: where an expression or operator was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Loc {
    pub source: Source,
    pub start: u32,
    pub end: u32,
}

/// A scalar expression over one row.
#[derive(Debug)]
pub struct Expr<A: Allocator> {
    pub kind: ExprKind<A>,
    pub at: Loc,
}

pub type Exprs<A> = Vec<Expr<A>, A>;
pub type Boxed<A> = Box<Expr<A>, A>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arith {
    Add,
    Sub,
    Mul,
    /// Always a double's division (DuckDB's `/`).
    Div,
    Rem,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug)]
pub enum ExprKind<A: Allocator> {
    Lit(Datum),
    Col(u32),
    Neg(Boxed<A>),
    Not(Boxed<A>),
    /// Kleene's: false if any is false, else null if any is null, else true.
    And(Exprs<A>),
    Or(Exprs<A>),
    Arith { op: Arith, left: Boxed<A>, right: Boxed<A> },
    Concat { left: Boxed<A>, right: Boxed<A> },
    Compare { op: Cmp, left: Boxed<A>, right: Boxed<A> },
    /// `left is [not] distinct from right` (`is not` / `is`): never null.
    Distinct { left: Boxed<A>, right: Boxed<A>, negated: bool },
    IsNull { operand: Boxed<A>, negated: bool },
    InList { operand: Boxed<A>, list: Exprs<A>, negated: bool },
    /// `operand [not] in (query)`, the query uncorrelated: subquery `sub`'s one column.
    InSet { operand: Boxed<A>, sub: u32, negated: bool },
    Between { operand: Boxed<A>, low: Boxed<A>, high: Boxed<A>, negated: bool },
    Like { operand: Boxed<A>, pattern: Boxed<A>, negated: bool },
    Case { operand: Option<Boxed<A>>, arms: Vec<(Expr<A>, Expr<A>), A>, otherwise: Option<Boxed<A>> },
    Cast { operand: Boxed<A>, to: TypeName },
    Call { func: Func, args: Exprs<A> },
    /// An uncorrelated scalar subquery's value.
    Scalar(u32),
    /// `exists (query)` whose correlation is equalities: whether subquery `sub` has a row whose
    /// key columns equal `keys` evaluated on this row. Uncorrelated, `keys` is empty.
    Exists { sub: u32, keys: Exprs<A> },
}

/// A scalar function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Func {
    Coalesce,
    Nullif,
    Greatest,
    Least,
    Round,
    Abs,
    Floor,
    Ceil,
    StartsWith,
    EndsWith,
    Contains,
    SplitPart,
    Replace,
    Concat,
    Length,
    Lower,
    Upper,
    ArrayToString,
    Error,
}

impl Func {
    const NAMES: [(&'static str, Func); 21] = [
        ("coalesce", Func::Coalesce),
        ("nullif", Func::Nullif),
        ("greatest", Func::Greatest),
        ("least", Func::Least),
        ("round", Func::Round),
        ("abs", Func::Abs),
        ("floor", Func::Floor),
        ("ceil", Func::Ceil),
        ("ceiling", Func::Ceil),
        ("starts_with", Func::StartsWith),
        ("ends_with", Func::EndsWith),
        ("contains", Func::Contains),
        ("split_part", Func::SplitPart),
        ("replace", Func::Replace),
        ("concat", Func::Concat),
        ("length", Func::Length),
        ("lower", Func::Lower),
        ("upper", Func::Upper),
        ("array_to_string", Func::ArrayToString),
        ("list_aggr_string", Func::ArrayToString),
        ("error", Func::Error),
    ];

    pub fn named(name: &str) -> Option<Func> {
        Func::NAMES.iter().find(|(it, _)| it.eq_ignore_ascii_case(name)).map(|&(_, f)| f)
    }

    /// The least and most arguments it takes.
    pub fn arity(self) -> (usize, usize) {
        match self {
            Func::Coalesce | Func::Greatest | Func::Least | Func::Concat => (1, usize::MAX),
            Func::Nullif | Func::StartsWith | Func::EndsWith | Func::Contains => (2, 2),
            Func::ArrayToString => (2, 2),
            Func::Round => (1, 2),
            Func::Abs | Func::Floor | Func::Ceil | Func::Length | Func::Lower | Func::Upper => (1, 1),
            Func::Error => (1, 1),
            Func::SplitPart | Func::Replace => (3, 3),
        }
    }
}

/// An aggregate function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggFunc {
    /// `count(*)`.
    CountRows,
    Count,
    Sum,
    Min,
    Max,
    Avg,
    AnyValue,
    ArgMax,
    ArgMin,
    Median,
    StringAgg,
    BoolAnd,
    BoolOr,
}

impl AggFunc {
    const NAMES: [(&'static str, AggFunc); 15] = [
        ("count", AggFunc::Count),
        ("sum", AggFunc::Sum),
        ("min", AggFunc::Min),
        ("max", AggFunc::Max),
        ("avg", AggFunc::Avg),
        ("mean", AggFunc::Avg),
        ("any_value", AggFunc::AnyValue),
        ("arg_max", AggFunc::ArgMax),
        ("max_by", AggFunc::ArgMax),
        ("arg_min", AggFunc::ArgMin),
        ("min_by", AggFunc::ArgMin),
        ("median", AggFunc::Median),
        ("string_agg", AggFunc::StringAgg),
        ("bool_and", AggFunc::BoolAnd),
        ("bool_or", AggFunc::BoolOr),
    ];

    pub fn named(name: &str) -> Option<AggFunc> {
        AggFunc::NAMES.iter().find(|(it, _)| it.eq_ignore_ascii_case(name)).map(|&(_, f)| f)
    }

    pub fn arity(self) -> (usize, usize) {
        match self {
            AggFunc::CountRows => (0, 0),
            AggFunc::ArgMax | AggFunc::ArgMin => (2, 2),
            AggFunc::StringAgg => (1, 2),
            AggFunc::Count
            | AggFunc::Sum
            | AggFunc::Min
            | AggFunc::Max
            | AggFunc::Avg
            | AggFunc::AnyValue
            | AggFunc::Median
            | AggFunc::BoolAnd
            | AggFunc::BoolOr => (1, 1),
        }
    }
}

/// One aggregate call of an [`Plan::Aggregate`] or a window.
#[derive(Debug)]
pub struct Agg<A: Allocator> {
    pub func: AggFunc,
    pub args: Exprs<A>,
    pub distinct: bool,
    pub filter: Option<Expr<A>>,
    pub order: Vec<SortKey<A>, A>,
    pub at: Loc,
}

#[derive(Debug)]
pub struct SortKey<A: Allocator> {
    pub expr: Expr<A>,
    pub descending: bool,
    pub nulls_first: bool,
}

/// What a window computes for each row.
#[derive(Debug)]
pub enum WinFunc<A: Allocator> {
    RowNumber,
    Rank,
    DenseRank,
    /// `lead(expr, offset, default)` (`lag` is a negative offset).
    Shift { expr: Expr<A>, offset: i64, default: Option<Expr<A>> },
    /// An aggregate over the frame.
    Agg(Agg<A>),
}

/// A frame's bound, in rows or in the order key's peers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameBound {
    Unbounded,
    /// `n preceding` (negative) or `n following`, in rows.
    Rows(i64),
    /// The current row, and in `range` frames its peers.
    Current,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    /// `rows` (true) or `range` (false): whether the current row's peers are in it.
    pub rows: bool,
    pub start: FrameBound,
    pub end: FrameBound,
}

#[derive(Debug)]
pub struct Window<A: Allocator> {
    pub func: WinFunc<A>,
    pub partition: Exprs<A>,
    pub order: Vec<SortKey<A>, A>,
    pub frame: Frame,
    pub at: Loc,
}

/// An equality key of a join: an expression over the left input and one over the right.
#[derive(Debug)]
pub struct Key<A: Allocator> {
    pub left: Expr<A>,
    pub right: Expr<A>,
    /// `is not distinct from`: two nulls match.
    pub null_equal: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Full,
    /// The left rows with a match; the left's columns only.
    Semi,
    /// The left rows with no match; the left's columns only.
    Anti,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetKind {
    Union,
    Except,
    Intersect,
}

/// A relational operator. Every plan's output is a relation of a known width.
#[derive(Debug)]
pub enum Plan<'s, A: Allocator> {
    /// Base table `table` of the catalog, its rows where `filter` is true; `prune` holds the
    /// conjuncts `column op literal` a chunk's statistics are checked against.
    Scan { table: u32, width: u32, filter: Option<Expr<A>>, prune: Vec<Prune, A> },
    /// One row of no columns: a `select` without `from`.
    Unit,
    Filter { input: Box<Plan<'s, A>, A>, predicate: Expr<A> },
    Project { input: Box<Plan<'s, A>, A>, exprs: Exprs<A> },
    /// The left's columns, then (but for semi and anti) the right's.
    Join {
        kind: JoinKind,
        left: Box<Plan<'s, A>, A>,
        right: Box<Plan<'s, A>, A>,
        keys: Vec<Key<A>, A>,
        /// Over the left's columns then the right's.
        residual: Option<Expr<A>>,
        /// The join's number in its query: the key its built side is kept under while a
        /// fixpoint runs, when that side reads no working table.
        id: u32,
        at: Loc,
    },
    /// DuckDB's asof join: each left row with the right row of equal `keys` nearest under
    /// `left op right` (`>=`: the greatest `right` at most `left`).
    Asof {
        outer: bool,
        left: Box<Plan<'s, A>, A>,
        right: Box<Plan<'s, A>, A>,
        keys: Vec<Key<A>, A>,
        op: Cmp,
        probe: Expr<A>,
        build: Expr<A>,
        id: u32,
        at: Loc,
    },
    /// The keys' columns, then the aggregates'. With no keys, one row even over no input.
    Aggregate { input: Box<Plan<'s, A>, A>, keys: Exprs<A>, aggs: Vec<Agg<A>, A> },
    /// The input's columns, then one a window.
    Window { input: Box<Plan<'s, A>, A>, windows: Vec<Window<A>, A> },
    Sort { input: Box<Plan<'s, A>, A>, keys: Vec<SortKey<A>, A> },
    Limit { input: Box<Plan<'s, A>, A>, limit: Option<u64>, offset: u64 },
    Distinct { input: Box<Plan<'s, A>, A> },
    Set { kind: SetKind, all: bool, left: Box<Plan<'s, A>, A>, right: Box<Plan<'s, A>, A> },
    /// A CTE or view, planned once and materialized once a query.
    Shared { id: u32 },
    /// The working table of fixpoint `id`: the rows its last round added.
    Work { id: u32 },
    /// `with recursive`: `base`, then `step` over the working table until a round adds none.
    Fixpoint {
        id: u32,
        name: &'s str,
        /// `union` (true) keeps only rows not already in the result; `union all` keeps all.
        distinct: bool,
        base: Box<Plan<'s, A>, A>,
        step: Box<Plan<'s, A>, A>,
        at: Loc,
    },
}

/// A conjunct `column op number` of a scan's filter, checked against a chunk's statistics:
/// a chunk where it holds for no row is skipped.
#[derive(Clone, Copy, Debug)]
pub struct Prune {
    pub column: u32,
    pub op: Cmp,
    pub value: crate::table::Bound,
}

impl<'s, A: Allocator> Plan<'s, A> {
    /// Whether the plan reads a fixpoint's working table: what is not is computed once per
    /// fixpoint, not once a round.
    pub fn reads_work(&self) -> bool {
        match self {
            Plan::Work { .. } => true,
            Plan::Scan { .. } | Plan::Unit | Plan::Shared { .. } => false,
            Plan::Filter { input, .. }
            | Plan::Project { input, .. }
            | Plan::Aggregate { input, .. }
            | Plan::Window { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. }
            | Plan::Distinct { input } => input.reads_work(),
            Plan::Join { left, right, .. }
            | Plan::Asof { left, right, .. }
            | Plan::Set { left, right, .. } => left.reads_work() || right.reads_work(),
            Plan::Fixpoint { base, step, .. } => base.reads_work() || step.reads_work(),
        }
    }
}
