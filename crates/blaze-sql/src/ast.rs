//! The syntax tree of one statement of the subset (docs/v2/plan/design/sql.md, "The SQL
//! subset"). Names and literals are slices of the source text; lists and boxes live in the
//! allocator the parser was handed. A tree holds no types: [`crate::infer`] adds them.
//!
//! Equality that ignores spans and the case of keywords is [`crate::same::same`]; the tree has
//! no `PartialEq`, because two trees of one statement differ in their spans.

#![blaze::sans_io]

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::alloc::Allocator;

/// Bytes `start..end` of the source text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

/// An identifier: a table, a column, an alias, a function or a parameter.
///
/// `text` is the name without its quotes (`"order"` gives `order`) or its `$`. An unquoted name
/// matches without regard to ASCII case; `quoted` says which it was.
#[derive(Clone, Copy, Debug)]
pub struct Name<'s> {
    pub text: &'s str,
    pub quoted: bool,
    pub span: Span,
}

#[derive(Debug)]
pub enum Statement<'s, A: Allocator> {
    Select(Select<'s, A>),
    Insert(Insert<'s, A>),
    Update(Update<'s, A>),
    Delete(Delete<'s, A>),
}

/// `select [distinct] columns [from ..] [where ..] [group by ..] [having ..] [order by ..]
/// [limit ..] [offset ..]`.
#[derive(Debug)]
pub struct Select<'s, A: Allocator> {
    pub distinct: bool,
    pub columns: Vec<ResultColumn<'s, A>, A>,
    pub from: Option<From<'s, A>>,
    pub filter: Option<Expr<'s, A>>,
    pub group_by: Vec<Expr<'s, A>, A>,
    pub having: Option<Expr<'s, A>>,
    pub order_by: Vec<OrderTerm<'s, A>, A>,
    pub limit: Option<Expr<'s, A>>,
    pub offset: Option<Expr<'s, A>>,
}

/// One entry of a `select` list or a `returning` list.
#[derive(Debug)]
pub enum ResultColumn<'s, A: Allocator> {
    /// `*`
    Star { span: Span },
    /// `t.*`
    TableStar { table: Name<'s> },
    /// `expr [as alias]`
    Expr { expr: Expr<'s, A>, alias: Option<Name<'s>> },
}

/// `from first [join ..]*`
#[derive(Debug)]
pub struct From<'s, A: Allocator> {
    pub first: TableRef<'s>,
    pub joins: Vec<Join<'s, A>, A>,
}

/// `table [as alias]`
#[derive(Clone, Copy, Debug)]
pub struct TableRef<'s> {
    pub table: Name<'s>,
    pub alias: Option<Name<'s>>,
}

/// `[inner] join table on expr` or `left [outer] join table on expr`.
#[derive(Debug)]
pub struct Join<'s, A: Allocator> {
    pub kind: JoinKind,
    pub table: TableRef<'s>,
    pub on: Expr<'s, A>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
}

/// `expr [asc | desc]`
#[derive(Debug)]
pub struct OrderTerm<'s, A: Allocator> {
    pub expr: Expr<'s, A>,
    pub descending: bool,
}

/// `insert into table (columns) values (row) [, (row)]* [returning ..]`. The column list is
/// required.
#[derive(Debug)]
pub struct Insert<'s, A: Allocator> {
    pub table: Name<'s>,
    pub columns: Vec<Name<'s>, A>,
    pub rows: Vec<Vec<Expr<'s, A>, A>, A>,
    pub returning: Vec<ResultColumn<'s, A>, A>,
}

/// `update table set column = expr [, ..]* [where ..] [returning ..]`
#[derive(Debug)]
pub struct Update<'s, A: Allocator> {
    pub table: Name<'s>,
    pub sets: Vec<Assignment<'s, A>, A>,
    pub filter: Option<Expr<'s, A>>,
    pub returning: Vec<ResultColumn<'s, A>, A>,
}

#[derive(Debug)]
pub struct Assignment<'s, A: Allocator> {
    pub column: Name<'s>,
    pub value: Expr<'s, A>,
}

/// `delete from table [where ..] [returning ..]`
#[derive(Debug)]
pub struct Delete<'s, A: Allocator> {
    pub table: Name<'s>,
    pub filter: Option<Expr<'s, A>>,
    pub returning: Vec<ResultColumn<'s, A>, A>,
}

#[derive(Debug)]
pub struct Expr<'s, A: Allocator> {
    pub kind: ExprKind<'s, A>,
    pub span: Span,
}

#[derive(Debug)]
pub enum ExprKind<'s, A: Allocator> {
    /// `null`
    Null,
    /// `true`, `false`
    Boolean(bool),
    /// An integer literal's magnitude; a sign is [`UnaryOp::Neg`] around it.
    Integer(u64),
    /// A literal with a `.` or an exponent.
    Real(f64),
    /// The source between the quotes of `'..'`, with `''` still doubled.
    Text(&'s str),
    /// The hex digits of `x'..'`.
    Blob(&'s str),
    /// `column` or `table.column`
    Column {
        table: Option<Name<'s>>,
        column: Name<'s>,
    },
    /// `$name`
    Param(Name<'s>),
    Unary {
        op: UnaryOp,
        operand: Box<Expr<'s, A>, A>,
    },
    /// `first op operand op operand ..`: a run of left-associative operators of one precedence
    /// level, flat, so a long run is one node and costs no nesting. `rest` has at least one
    /// link, and every link's operator is of one [`ChainOp::level`]. It means the left-nested
    /// tree, `((first op a) op b) ..`.
    Chain {
        first: Box<Expr<'s, A>, A>,
        rest: Vec<ChainLink<'s, A>, A>,
    },
    /// `left op right`: a comparison or `is`, which never chains.
    Compare {
        op: CompareOp,
        left: Box<Expr<'s, A>, A>,
        right: Box<Expr<'s, A>, A>,
    },
    /// `operand is [not] null`
    IsNull {
        operand: Box<Expr<'s, A>, A>,
        negated: bool,
    },
    /// `operand [not] in (list)`; the list has at least one entry.
    InList {
        operand: Box<Expr<'s, A>, A>,
        list: Vec<Expr<'s, A>, A>,
        negated: bool,
    },
    /// `operand [not] in $list`
    InParam {
        operand: Box<Expr<'s, A>, A>,
        list: Name<'s>,
        negated: bool,
    },
    /// `operand [not] between low and high`
    Between {
        operand: Box<Expr<'s, A>, A>,
        low: Box<Expr<'s, A>, A>,
        high: Box<Expr<'s, A>, A>,
        negated: bool,
    },
    /// `operand [not] like pattern`
    Like {
        operand: Box<Expr<'s, A>, A>,
        pattern: Box<Expr<'s, A>, A>,
        negated: bool,
    },
    /// `case [operand] when .. then .. [else ..] end`; at least one arm.
    Case {
        operand: Option<Box<Expr<'s, A>, A>>,
        arms: Vec<CaseArm<'s, A>, A>,
        otherwise: Option<Box<Expr<'s, A>, A>>,
    },
    /// `cast(operand as type)`
    Cast {
        operand: Box<Expr<'s, A>, A>,
        to: SqlType,
    },
    /// `function(args)`
    Call {
        function: Name<'s>,
        args: CallArgs<'s, A>,
    },
}

/// One `op operand` of a [`ExprKind::Chain`].
#[derive(Debug)]
pub struct ChainLink<'s, A: Allocator> {
    pub op: ChainOp,
    pub operand: Expr<'s, A>,
}

#[derive(Debug)]
pub struct CaseArm<'s, A: Allocator> {
    pub when: Expr<'s, A>,
    pub then: Expr<'s, A>,
}

#[derive(Debug)]
pub enum CallArgs<'s, A: Allocator> {
    /// `count(*)`
    Star,
    /// `f([distinct] a, b, ..)`
    List { distinct: bool, args: Vec<Expr<'s, A>, A> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    /// `-x`
    Neg,
    /// `not x`
    Not,
}

/// The left-associative operators, which chain. Levels, loosest first: `or`; `and`; `+ -`;
/// `* / %`; `||`. The comparisons bind between `and` and `+ -`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainOp {
    Or,
    And,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    /// `||`
    Concat,
}

impl ChainOp {
    /// The operator's precedence level, 0 the loosest. One chain holds one level.
    pub fn level(self) -> u8 {
        match self {
            ChainOp::Or => 0,
            ChainOp::And => 1,
            ChainOp::Add | ChainOp::Sub => 2,
            ChainOp::Mul | ChainOp::Div | ChainOp::Rem => 3,
            ChainOp::Concat => 4,
        }
    }
}

/// The comparisons and `is`. None chains: `a = b = c` is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareOp {
    /// `=`
    Eq,
    /// `<>` and `!=`
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// `a is b`: equality that holds for two nulls.
    Is,
    /// `a is not b`
    IsNot,
}

/// The types `cast` names: `boolean`, `integer`, `real`, `text`, `blob`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SqlType {
    Boolean,
    Integer,
    Real,
    Text,
    Blob,
}
