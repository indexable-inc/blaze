//! Typing one statement against a schema (docs/v2/plan/design/sql.md, "Typing rules").
//!
//! Every expression gets a storage, a Rust type and a nullability ([`Ty`]). A bare parameter or
//! `null` has none of its own: its position gives it one (the other side of a comparison, the
//! column it is assigned to, the type of a `cast`), and a position that gives none is an error
//! saying to `cast` it. Every parameter is then one [`Param`] of the [`Description`], in the order
//! it first appears in the text, and the statement returns rows of its result columns or a count.
//!
//! [`Cx`] is the one typer: [`infer`] runs it over a statement, and [`crate::render`] runs it
//! again over the same statement to resolve names and to learn the storage of a `sum`'s argument.

#![blaze::sans_io]

use alloc::vec::Vec;
use core::alloc::Allocator;
use core::fmt;
use core::iter;

use crate::ast::{
    self, CallArgs, CaseArm, ChainLink, ChainOp, CompareOp, Expr, ExprKind, JoinKind, Name,
    ResultColumn, Span, Statement, TableRef, UnaryOp,
};
use crate::desc::{Description, Param, ResultCol, Returns, Storage, Ty};
use crate::funcs::{self, Args, Function, Gives};
// The one bound on nesting, the parser's: typing recurses once per level, and a tree past it
// (built by hand; the parser makes none) is refused rather than recursed into.
use crate::parse::MAX_DEPTH;
use crate::same::same_expr;
use crate::schema::{Column, Schema, Table};

/// The subset's keywords as the checker's messages and the rendered text spell them: lower
/// case, each the word itself. A message names a clause or an operator by one, and
/// [`crate::render`] writes them, so each word is written here once.
pub(crate) mod keyword {
    /// The keyword `select`.
    pub(crate) const SELECT: &str = "select";
    /// The keyword `distinct`.
    pub(crate) const DISTINCT: &str = "distinct";
    /// The keyword `from`.
    pub(crate) const FROM: &str = "from";
    /// The keyword `join`.
    pub(crate) const JOIN: &str = "join";
    /// The keyword `on`.
    pub(crate) const ON: &str = "on";
    /// The keyword `where`.
    pub(crate) const WHERE: &str = "where";
    /// The keyword `having`.
    pub(crate) const HAVING: &str = "having";
    /// The keyword `desc`.
    pub(crate) const DESC: &str = "desc";
    /// The keyword `limit`.
    pub(crate) const LIMIT: &str = "limit";
    /// The keyword `offset`.
    pub(crate) const OFFSET: &str = "offset";
    /// The keyword `as`.
    pub(crate) const AS: &str = "as";
    /// The keyword `values`.
    pub(crate) const VALUES: &str = "values";
    /// The keyword `update`.
    pub(crate) const UPDATE: &str = "update";
    /// The keyword `set`.
    pub(crate) const SET: &str = "set";
    /// The keyword `returning`.
    pub(crate) const RETURNING: &str = "returning";
    /// The keyword `and`.
    pub(crate) const AND: &str = "and";
    /// The keyword `or`.
    pub(crate) const OR: &str = "or";
    /// The keyword `not`.
    pub(crate) const NOT: &str = "not";
    /// The keyword `is`.
    pub(crate) const IS: &str = "is";
    /// The keyword `null`.
    pub(crate) const NULL: &str = "null";
    /// The keyword `true`.
    pub(crate) const TRUE: &str = "true";
    /// The keyword `false`.
    pub(crate) const FALSE: &str = "false";
    /// The keyword `in`.
    pub(crate) const IN: &str = "in";
    /// The keyword `between`.
    pub(crate) const BETWEEN: &str = "between";
    /// The keyword `like`.
    pub(crate) const LIKE: &str = "like";
    /// The keyword `case`.
    pub(crate) const CASE: &str = "case";
    /// The keyword `when`.
    pub(crate) const WHEN: &str = "when";
    /// The keyword `then`.
    pub(crate) const THEN: &str = "then";
    /// The keyword `else`.
    pub(crate) const ELSE: &str = "else";
    /// The keyword `end`.
    pub(crate) const END: &str = "end";
}

/// Why a statement does not type against a schema: what, where, and the rule.
#[derive(Debug)]
pub struct TypeError<'s> {
    pub kind: TypeErrorKind<'s>,
    pub span: Span,
}

#[derive(Debug)]
pub enum TypeErrorKind<'s> {
    /// An expression nests deeper than [`MAX_DEPTH`].
    TooDeep { limit: u32 },
    /// A table the schema does not have.
    NoSuchTable { table: &'s str },
    /// Two tables of one `from` under one name.
    TableTwice { name: &'s str },
    /// `t.c` or `t.*` with no `t` in scope.
    NotInScope { table: &'s str },
    /// `t.c` where `t` has no `c`; `table` is the name as written.
    NoSuchColumn { table: &'s str, column: &'s str },
    /// An unqualified column no table in scope has.
    UnknownColumn { column: &'s str },
    /// An unqualified column two tables in scope have.
    AmbiguousColumn { column: &'s str, first: &'s str, second: &'s str },
    /// A column where none is visible (`values`, `limit`, `offset`).
    ColumnNotAllowed { column: &'s str, place: Place },
    /// `*` in a `select` without `from`.
    StarWithoutFrom,
    /// An integer literal above `i64::MAX` that is not `-9223372036854775808`.
    IntegerTooLarge { magnitude: u64 },
    /// A real literal outside the finite 64-bit floats.
    RealNotFinite,
    /// An operand of the wrong storage.
    WrongStorage { what: What<'s>, place: Place, expected: Expected, found: Storage },
    /// Arithmetic on an Integer and a Real.
    MixedArithmetic { op: &'static str, left: Storage, right: Storage },
    /// `%` on Reals, which SQLite and Postgres compute differently.
    RealRemainder,
    /// A comparison of two storages.
    CompareStorage { place: Place, left: Storage, right: Storage },
    /// A comparison of two different Rust types, neither plain (`PersonId = OrderId`).
    CompareTypes { place: Place, left: &'s str, right: &'s str },
    /// Branches of a `case` or arguments of `coalesce` of two types.
    BranchTypes { place: Place, first: TyName<'s>, other: TyName<'s> },
    /// A parameter or `null` whose position gives it no type.
    NeedsCast { what: What<'s>, place: Place },
    /// Parameters on both sides of a comparison or an operator.
    ParamsBothSides { place: Place, left: &'s str, right: &'s str },
    /// One parameter given two types at two positions.
    ParamConflict { name: &'s str, first: TyName<'s>, second: TyName<'s> },
    /// A function not in [`funcs::FUNCTIONS`].
    UnknownFunction { name: &'s str },
    /// A call with the wrong number of arguments.
    Arity { function: &'static str, arity: funcs::Arity, found: usize },
    /// `f(distinct ..)` for an `f` that is not an aggregate.
    DistinctNotAggregate { function: &'static str },
    /// `f(*)` for an `f` that is not `count`.
    StarNotCount { function: &'static str },
    /// An aggregate in `where`, `on`, `group by`, inside another aggregate, or in a statement
    /// that is not a `select`.
    AggregateNotAllowed { function: &'static str, place: Place },
    /// A column outside an aggregate in a grouped `select` that `group by` does not cover.
    NotGrouped { column: &'s str },
    /// `having` with neither `group by` nor an aggregate.
    HavingWithoutGroup,
    /// An `order by` term of a `select distinct` that is not a result column.
    NotInDistinct { what: What<'s> },
    /// `order by 1` or `group by 1`.
    Positional { clause: &'static str, position: u64 },
    /// A result column that is not a plain column and has no `as`.
    Unnamed { what: What<'s> },
    /// Two result columns of one name, without regard to ASCII case.
    DuplicateResultName { name: &'s str },
    /// A column listed twice in an `insert` or set twice in an `update`.
    ColumnTwice { column: &'s str },
    /// An `insert` leaving out a column that is not nullable and is not the table's integer key.
    MissingColumn { table: &'s str, column: &'s str },
    /// A row of `values` not as wide as the column list. `row` counts from 1.
    RowWidth { row: usize, columns: usize, values: usize },
    /// `null` into a column that is not nullable.
    NullIntoNonNull { column: &'s str },
    /// A nullable value into a column that is not nullable.
    NullableIntoNonNull { column: &'s str, what: What<'s> },
    /// A value of another type than its column's.
    AssignType { column: &'s str, expected: TyName<'s>, found: TyName<'s> },
    /// A nullable value where one must not be null (`limit`, `offset`).
    NullableNotAllowed { what: What<'s>, place: Place },
    /// `cast(x as boolean)` of an `x` that is neither a Boolean nor a parameter.
    CastToBoolean { what: What<'s>, found: Storage },
}

/// Where an expression stands, for a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// In a clause: `where`, `on`, `having`, `limit`, `values`, ..
    Clause(&'static str),
    /// An operand of an operator: `+`, `=`, `like`, ..
    Operand(&'static str),
    /// An argument of a function.
    Argument(&'static str),
    /// A result column of a `select` or a `returning`.
    Result,
    /// The argument of an aggregate, where another aggregate may not be.
    InsideAggregate,
}

/// The expression an error is about, as written.
#[derive(Clone, Copy, Debug)]
pub enum What<'s> {
    Column(&'s str),
    Param(&'s str),
    Null,
    Boolean(bool),
    Integer(u64),
    Real(f64),
    Text(&'s str),
    Blob(&'s str),
    Call(&'s str),
    /// An operator's expression, by its operator.
    Operator(&'static str),
}

/// The storages a position takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expected {
    One(Storage),
    /// An Integer or a Real.
    Number,
    /// Anything but a Boolean.
    NotBoolean,
}

/// A type as a message names it: `Option<PersonId>` (integer), `[PersonId]` for a list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TyName<'s> {
    pub storage: Storage,
    pub rust: &'s str,
    pub nullable: bool,
    pub list: bool,
}

/// Whether a parameter is one value or a list of them (`in $list`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Form {
    Scalar,
    List,
}

impl Form {
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn is_list(self) -> bool {
        matches!(self, Form::List)
    }
}

impl<'s> TyName<'s> {
    fn of(ty: Ty<'s>, form: Form) -> TyName<'s> {
        let list = form.is_list();
        TyName { storage: ty.storage, rust: ty.rust.name, nullable: ty.nullable, list }
    }
}

/// A `fmt::Write` that writes ASCII upper case as lower: a storage's message name is its
/// variant's derived `Debug` name in lower case (`Integer` is `integer`), so a rename follows it.
struct Lower<'f, 'a>(&'f mut fmt::Formatter<'a>);

impl fmt::Write for Lower<'_, '_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            fmt::Write::write_char(&mut *self.0, c.to_ascii_lowercase())?;
        }
        Ok(())
    }
}

fn storage_with_article(storage: Storage) -> &'static str {
    match storage {
        Storage::Boolean => "a boolean",
        Storage::Integer => "an integer",
        Storage::Real => "a real",
        Storage::Text => "a text",
        Storage::Blob => "a blob",
    }
}

impl fmt::Display for TyName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.list {
            write!(f, "`[{}]`", self.rust)?;
        } else if self.nullable {
            write!(f, "`Option<{}>`", self.rust)?;
        } else {
            write!(f, "`{}`", self.rust)?;
        }
        f.write_str(" (")?;
        fmt::Write::write_fmt(&mut Lower(&mut *f), format_args!("{:?}", self.storage))?;
        f.write_str(")")
    }
}

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Place::Clause(clause) => write!(f, "in `{clause}`"),
            Place::Operand(op) => write!(f, "as an operand of `{op}`"),
            Place::Argument(function) => write!(f, "as an argument of `{function}`"),
            Place::Result => f.write_str("as a result column"),
            Place::InsideAggregate => f.write_str("inside another aggregate"),
        }
    }
}

impl fmt::Display for What<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            What::Column(column) => write!(f, "column `{column}`"),
            What::Param(name) => write!(f, "parameter `${name}`"),
            What::Null => f.write_str("`null`"),
            What::Boolean(value) => write!(f, "`{value}`"),
            What::Integer(value) => write!(f, "integer `{value}`"),
            What::Real(value) => write!(f, "real `{value:?}`"),
            What::Text(text) => write!(f, "text `'{text}'`"),
            What::Blob(hex) => write!(f, "blob `x'{hex}'`"),
            What::Call(function) => write!(f, "the call to `{function}`"),
            What::Operator(op) => write!(f, "the `{op}` expression"),
        }
    }
}

impl fmt::Display for Expected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expected::One(
                storage @ (Storage::Boolean
                | Storage::Integer
                | Storage::Real
                | Storage::Text
                | Storage::Blob),
            ) => f.write_str(storage_with_article(*storage)),
            Expected::Number => f.write_str("an integer or a real"),
            Expected::NotBoolean => f.write_str("anything but a boolean"),
        }
    }
}

impl fmt::Display for TypeErrorKind<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use Expected as E;
        use Place as P;
        use Storage as S;
        use TypeErrorKind as K;
        use What as W;
        match self {
            K::TooDeep { limit } => {
                write!(f, "the expression nests deeper than {limit} levels, the most the checker types")
            }
            K::NoSuchTable { table } => write!(f, "the schema has no table `{table}`"),
            K::TableTwice { name } => write!(
                f,
                "`{name}` names two tables of this `from`; give one an alias with `as`"
            ),
            K::NotInScope { table } => {
                write!(f, "`{table}` is not a table or an alias of this statement's `from`")
            }
            K::NoSuchColumn { table, column } => {
                write!(f, "table `{table}` has no column `{column}`")
            }
            K::UnknownColumn { column } => {
                write!(f, "no table of this statement has a column `{column}`")
            }
            K::AmbiguousColumn { column, first, second } => write!(
                f,
                "column `{column}` is in both `{first}` and `{second}`; qualify it, as `{first}.{column}`"
            ),
            K::ColumnNotAllowed {
                column,
                place:
                    place @ (P::Clause(_)
                    | P::Operand(_)
                    | P::Argument(_)
                    | P::Result
                    | P::InsideAggregate),
            } => {
                write!(f, "column `{column}` cannot be read {place}, which sees no columns")
            }
            K::StarWithoutFrom => f.write_str("`*` needs a `from` to take its columns from"),
            K::IntegerTooLarge { magnitude } => write!(
                f,
                "integer `{magnitude}` is above the largest 64-bit integer, 9223372036854775807"
            ),
            K::RealNotFinite => f.write_str("the real literal is outside the finite 64-bit floats"),
            K::WrongStorage {
                what:
                    what @ (W::Column(_)
                    | W::Param(_)
                    | W::Null
                    | W::Boolean(_)
                    | W::Integer(_)
                    | W::Real(_)
                    | W::Text(_)
                    | W::Blob(_)
                    | W::Call(_)
                    | W::Operator(_)),
                place:
                    place @ (P::Clause(_)
                    | P::Operand(_)
                    | P::Argument(_)
                    | P::Result
                    | P::InsideAggregate),
                expected: expected @ (E::One(_) | E::Number | E::NotBoolean),
                found: found @ (S::Boolean | S::Integer | S::Real | S::Text | S::Blob),
            } => write!(
                f,
                "{what} {place} must be {expected}, but it is {}",
                storage_with_article(*found)
            ),
            K::MixedArithmetic {
                op,
                left: left @ (S::Boolean | S::Integer | S::Real | S::Text | S::Blob),
                right: right @ (S::Boolean | S::Integer | S::Real | S::Text | S::Blob),
            } => write!(
                f,
                "`{op}` takes two integers or two reals, not {} and {}; `cast` one side",
                storage_with_article(*left),
                storage_with_article(*right)
            ),
            K::RealRemainder => f.write_str(
                "`%` takes integers, not reals (SQLite and Postgres disagree on a real remainder); `cast` both sides to integer",
            ),
            K::CompareStorage {
                place:
                    place @ (P::Clause(_)
                    | P::Operand(_)
                    | P::Argument(_)
                    | P::Result
                    | P::InsideAggregate),
                left: left @ (S::Boolean | S::Integer | S::Real | S::Text | S::Blob),
                right: right @ (S::Boolean | S::Integer | S::Real | S::Text | S::Blob),
            } => write!(
                f,
                "{} and {} are compared {place}; both sides must have one storage, so `cast` one",
                storage_with_article(*left),
                storage_with_article(*right)
            ),
            K::CompareTypes {
                place:
                    place @ (P::Clause(_)
                    | P::Operand(_)
                    | P::Argument(_)
                    | P::Result
                    | P::InsideAggregate),
                left,
                right,
            } => write!(
                f,
                "`{left}` and `{right}` are compared {place}, but they are different Rust types"
            ),
            K::BranchTypes {
                place:
                    place @ (P::Clause(_)
                    | P::Operand(_)
                    | P::Argument(_)
                    | P::Result
                    | P::InsideAggregate),
                first,
                other,
            } => write!(
                f,
                "the values {place} must have one type, but one is {first} and another {other}"
            ),
            K::NeedsCast {
                what:
                    what @ (W::Column(_)
                    | W::Param(_)
                    | W::Null
                    | W::Boolean(_)
                    | W::Integer(_)
                    | W::Real(_)
                    | W::Text(_)
                    | W::Blob(_)
                    | W::Call(_)
                    | W::Operator(_)),
                place:
                    place @ (P::Clause(_)
                    | P::Operand(_)
                    | P::Argument(_)
                    | P::Result
                    | P::InsideAggregate),
            } => write!(
                f,
                "{what} {place} gets no type from its position; give it one with `cast(.. as <type>)`"
            ),
            K::ParamsBothSides {
                place:
                    place @ (P::Clause(_)
                    | P::Operand(_)
                    | P::Argument(_)
                    | P::Result
                    | P::InsideAggregate),
                left,
                right,
            } => write!(
                f,
                "parameters `${left}` and `${right}` {place} give each other no type; `cast` one of them"
            ),
            K::ParamConflict { name, first, second } => write!(
                f,
                "parameter `${name}` is {first} at one place and {second} at another; one parameter has one type"
            ),
            K::UnknownFunction { name } => {
                write!(f, "no function `{name}`; the functions are {}", funcs::Known)
            }
            K::Arity {
                function,
                arity: arity @ (funcs::Arity::Exactly(_) | funcs::Arity::AtLeast(_)),
                found,
            } => {
                write!(f, "`{function}` takes {arity}, not {found}")
            }
            K::DistinctNotAggregate { function } => {
                write!(f, "`distinct` is only for aggregates, and `{function}` is not one")
            }
            K::StarNotCount { function } => {
                write!(f, "`*` is an argument only of `count`, not of `{function}`")
            }
            K::AggregateNotAllowed {
                function,
                place:
                    place @ (P::Clause(_)
                    | P::Operand(_)
                    | P::Argument(_)
                    | P::Result
                    | P::InsideAggregate),
            } => {
                write!(f, "aggregate `{function}` is not allowed {place}")
            }
            K::NotGrouped { column } => write!(
                f,
                "column `{column}` is neither in `group by` nor inside an aggregate; group by it or aggregate it"
            ),
            K::HavingWithoutGroup => f.write_str("`having` needs a `group by` or an aggregate"),
            K::NotInDistinct {
                what:
                    what @ (W::Column(_)
                    | W::Param(_)
                    | W::Null
                    | W::Boolean(_)
                    | W::Integer(_)
                    | W::Real(_)
                    | W::Text(_)
                    | W::Blob(_)
                    | W::Call(_)
                    | W::Operator(_)),
            } => write!(
                f,
                "{what} in `order by` of a `select distinct` must be one of its result columns"
            ),
            K::Positional { clause, position } => write!(
                f,
                "`{clause} {position}` is positional, which is not in the subset; name the expression"
            ),
            K::Unnamed {
                what:
                    what @ (W::Column(_)
                    | W::Param(_)
                    | W::Null
                    | W::Boolean(_)
                    | W::Integer(_)
                    | W::Real(_)
                    | W::Text(_)
                    | W::Blob(_)
                    | W::Call(_)
                    | W::Operator(_)),
            } => {
                write!(f, "{what} as a result column needs a name; add `as <name>`")
            }
            K::DuplicateResultName { name } => write!(
                f,
                "two result columns are named `{name}` (names compare without regard to case); rename one with `as`"
            ),
            K::ColumnTwice { column } => write!(f, "column `{column}` is given twice"),
            K::MissingColumn { table, column } => write!(
                f,
                "this `insert` into `{table}` leaves out `{column}`, which is not nullable and not the table's integer key"
            ),
            K::RowWidth { row, columns, values } => {
                write!(f, "row {row} of `values` has {values} values for {columns} columns")
            }
            K::NullIntoNonNull { column } => {
                write!(f, "`null` cannot go into column `{column}`, which is not nullable")
            }
            K::NullableIntoNonNull {
                column,
                what:
                    what @ (W::Column(_)
                    | W::Param(_)
                    | W::Null
                    | W::Boolean(_)
                    | W::Integer(_)
                    | W::Real(_)
                    | W::Text(_)
                    | W::Blob(_)
                    | W::Call(_)
                    | W::Operator(_)),
            } => {
                write!(f, "{what} may be null, but column `{column}` is not nullable")
            }
            K::AssignType { column, expected, found } => {
                write!(f, "column `{column}` is {expected}, but the value is {found}")
            }
            K::NullableNotAllowed {
                what:
                    what @ (W::Column(_)
                    | W::Param(_)
                    | W::Null
                    | W::Boolean(_)
                    | W::Integer(_)
                    | W::Real(_)
                    | W::Text(_)
                    | W::Blob(_)
                    | W::Call(_)
                    | W::Operator(_)),
                place:
                    place @ (P::Clause(_)
                    | P::Operand(_)
                    | P::Argument(_)
                    | P::Result
                    | P::InsideAggregate),
            } => {
                write!(f, "{what} {place} may be null, and it must not be")
            }
            K::CastToBoolean {
                what:
                    what @ (W::Column(_)
                    | W::Param(_)
                    | W::Null
                    | W::Boolean(_)
                    | W::Integer(_)
                    | W::Real(_)
                    | W::Text(_)
                    | W::Blob(_)
                    | W::Call(_)
                    | W::Operator(_)),
                found: found @ (S::Boolean | S::Integer | S::Real | S::Text | S::Blob),
            } => write!(
                f,
                "{what} is {}, and `cast(.. as boolean)` takes only a boolean or a parameter (SQLite has no boolean to cast to); write `x <> 0` instead",
                storage_with_article(*found)
            ),
        }
    }
}

impl fmt::Display for TypeError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at bytes {}..{})", self.kind, self.span.start, self.span.end)
    }
}

fn err<'s, T>(kind: TypeErrorKind<'s>, span: Span) -> Result<T, TypeError<'s>> {
    Err(TypeError { kind, span })
}

/// Types `statement` against `schema`.
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn infer<'s, A: Allocator + Copy>(
    schema: &Schema<'s>,
    statement: &Statement<'s, A>,
    alloc: A,
) -> Result<Description<'s, A>, TypeError<'s>> {
    run(schema, statement, None, alloc)
}

/// A bug to plant in inference, for a test's control.
#[cfg(feature = "plant")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plant {
    /// A `left join`'s right side keeps its columns non-null.
    LeftJoinKeepsNonNull,
}

/// Without the `plant` feature there is no bug to plant: `Option<Plant>` is always `None`.
#[cfg(not(feature = "plant"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {}

/// [`infer`] with `plant` planted.
#[cfg(feature = "plant")]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn infer_planted<'s, A: Allocator + Copy>(
    schema: &Schema<'s>,
    statement: &Statement<'s, A>,
    plant: Plant,
    alloc: A,
) -> Result<Description<'s, A>, TypeError<'s>> {
    run(schema, statement, Some(plant), alloc)
}

/// Whether a `left join` makes its right side nullable: always, but with the planted bug.
fn left_join_nullable(plant: Option<Plant>) -> bool {
    #[cfg(feature = "plant")]
    {
        plant != Some(Plant::LeftJoinKeepsNonNull)
    }
    #[cfg(not(feature = "plant"))]
    {
        match plant {
            None => true,
            Some(never) => match never {},
        }
    }
}

fn run<'s, A: Allocator + Copy>(
    schema: &Schema<'s>,
    statement: &Statement<'s, A>,
    plant: Option<Plant>,
    alloc: A,
) -> Result<Description<'s, A>, TypeError<'s>> {
    let mut cx = Cx::new(schema, plant, alloc);
    let returns = match statement {
        Statement::Select(select) => cx.select(select)?,
        Statement::Insert(insert) => cx.insert(insert)?,
        Statement::Update(update) => cx.update(update)?,
        Statement::Delete(delete) => cx.delete(delete)?,
    };
    Ok(cx.finish(returns))
}

/// A name as written against a name of the schema: an unquoted one without regard to ASCII
/// case (the schema's rule), a quoted one exactly.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn names_schema(name: &Name<'_>, schema_name: &str) -> bool {
    if name.quoted { name.text == schema_name } else { name.text.eq_ignore_ascii_case(schema_name) }
}

/// Two names as written, by Postgres's rule, which is SQLite's narrowed: an unquoted name folds
/// to lower case, a quoted one is exact.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn names_same(a: &Name<'_>, b: &Name<'_>) -> bool {
    match (a.quoted, b.quoted) {
        (false, false) => a.text.eq_ignore_ascii_case(b.text),
        (true, true) => a.text == b.text,
        (true, false) => Unquoted(b.text).folds_to(a.text),
        (false, true) => Unquoted(a.text).folds_to(b.text),
    }
}

/// The text of an unquoted name, which folds to lower case.
#[derive(Clone, Copy)]
struct Unquoted<'a>(&'a str);

impl Unquoted<'_> {
    /// Whether it folds to the quoted name `quoted`.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn folds_to(self, quoted: &str) -> bool {
        self.0.len() == quoted.len()
            && self.0.bytes().zip(quoted.bytes()).all(|(u, q)| u.to_ascii_lowercase() == q)
    }
}

/// The table of `schema` that `name` names.
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn table_of<'s>(schema: &Schema<'s>, name: &Name<'_>) -> Option<&'s Table<'s>> {
    schema.tables.iter().copied().find(|table| names_schema(name, table.name))
}

/// The column of `table` that `name` names.
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn column_of<'s>(table: &'s Table<'s>, name: &Name<'_>) -> Option<&'s Column<'s>> {
    table.columns.iter().find(|column| names_schema(name, column.name))
}

/// The one primary key column of `table` when it is an Integer: the engine assigns it, so an
/// `insert` may leave it out.
fn integer_key<'s>(table: &'s Table<'s>) -> Option<&'s Column<'s>> {
    let mut keys = table.columns.iter().filter(|column| column.primary_key);
    match (keys.next(), keys.next()) {
        (Some(key), None) if key.storage == Storage::Integer => Some(key),
        _ => None,
    }
}

/// The type of `column` read through `entry`: nullable if the column is, or if the entry is
/// the right side of a `left join`.
fn column_ty<'s>(column: &'s Column<'s>, entry: &Entry<'s>) -> Ty<'s> {
    Ty { storage: column.storage, rust: column.rust, nullable: column.nullable || entry.nullable }
}

/// The plain Rust type of `storage`, non-null; `Ty { nullable, ..plain(storage) }` for one that
/// may be null.
fn plain(storage: Storage) -> Ty<'static> {
    Ty { storage, rust: storage.plain(), nullable: false }
}

fn is_number(storage: Storage) -> bool {
    matches!(storage, Storage::Integer | Storage::Real)
}

/// Whether `e` is a literal value (`true`, `1`, `1.5`, `'a'`, `x'00'`) or a negated number
/// literal (`-1`, `-1.5`), which goes into a column of its storage whatever the column's Rust
/// type.
fn is_literal<X: Allocator>(e: &Expr<'_, X>) -> bool {
    match &e.kind {
        ExprKind::Boolean(_)
        | ExprKind::Integer(_)
        | ExprKind::Real(_)
        | ExprKind::Text(_)
        | ExprKind::Blob(_) => true,
        ExprKind::Unary { op: UnaryOp::Neg, operand } => {
            matches!(operand.kind, ExprKind::Integer(_) | ExprKind::Real(_))
        }
        ExprKind::Null
        | ExprKind::Column { .. }
        | ExprKind::Param(_)
        | ExprKind::Unary { op: UnaryOp::Not, .. }
        | ExprKind::Chain { .. }
        | ExprKind::Compare { .. }
        | ExprKind::IsNull { .. }
        | ExprKind::InList { .. }
        | ExprKind::InParam { .. }
        | ExprKind::Between { .. }
        | ExprKind::Like { .. }
        | ExprKind::Case { .. }
        | ExprKind::Cast { .. }
        | ExprKind::Call { .. } => false,
    }
}

/// Whether `rust` is the plain Rust type of its storage, which compares with any type of it.
fn is_plain(ty: &Ty<'_>) -> bool {
    ty.rust == ty.storage.plain()
}

#[blaze::pure]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn chain_text(op: ChainOp) -> &'static str {
    match op {
        ChainOp::Or => keyword::OR,
        ChainOp::And => keyword::AND,
        ChainOp::Add => "+",
        ChainOp::Sub => "-",
        ChainOp::Mul => "*",
        ChainOp::Div => "/",
        ChainOp::Rem => "%",
        ChainOp::Concat => "||",
    }
}

#[blaze::pure]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn compare_text(op: CompareOp) -> &'static str {
    match op {
        CompareOp::Eq => "=",
        CompareOp::Ne => "<>",
        CompareOp::Lt => "<",
        CompareOp::Le => "<=",
        CompareOp::Gt => ">",
        CompareOp::Ge => ">=",
        CompareOp::Is => keyword::IS,
        CompareOp::IsNot => "is not",
    }
}

fn what<'s, X: Allocator>(e: &Expr<'s, X>) -> What<'s> {
    // A chain without links (which the parser never makes) is its first operand; peeled in a
    // loop, not a recursion.
    let mut e = e;
    while let ExprKind::Chain { first, rest } = &e.kind
        && rest.is_empty()
    {
        e = first;
    }
    match &e.kind {
        ExprKind::Null => What::Null,
        ExprKind::Boolean(value) => What::Boolean(*value),
        ExprKind::Integer(value) => What::Integer(*value),
        ExprKind::Real(value) => What::Real(*value),
        ExprKind::Text(text) => What::Text(text),
        ExprKind::Blob(hex) => What::Blob(hex),
        ExprKind::Column { column, .. } => What::Column(column.text),
        ExprKind::Param(name) => What::Param(name.text),
        ExprKind::Unary { op: UnaryOp::Neg, .. } => What::Operator("-"),
        ExprKind::Unary { op: UnaryOp::Not, .. } => What::Operator(keyword::NOT),
        // The tree a chain means is left-nested: its outermost operator is the last link's.
        ExprKind::Chain { rest, .. } => match rest.last() {
            Some(link) => What::Operator(chain_text(link.op)),
            // Not reached: a chain without links was peeled above.
            None => What::Operator("chain"),
        },
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
            ..
        } => What::Operator(compare_text(*op)),
        ExprKind::IsNull { .. } => What::Operator("is null"),
        ExprKind::InList { .. } | ExprKind::InParam { .. } => What::Operator(keyword::IN),
        ExprKind::Between { .. } => What::Operator(keyword::BETWEEN),
        ExprKind::Like { .. } => What::Operator(keyword::LIKE),
        ExprKind::Case { .. } => What::Operator(keyword::CASE),
        ExprKind::Cast { .. } => What::Operator("cast"),
        ExprKind::Call { function, .. } => What::Call(function.text),
    }
}

/// The error for a group of operands none of which has a type: two parameters, or one
/// parameter or `null` alone.
fn no_anchor<'e, 's: 'e, X: Allocator + 'e>(
    items: impl Iterator<Item = &'e Expr<'s, X>>,
    place: Place,
    at: Span,
) -> TypeError<'s> {
    let mut params = items.filter_map(|item| match &item.kind {
        ExprKind::Param(name) => Some(name.text),
        ExprKind::Null
        | ExprKind::Boolean(_)
        | ExprKind::Integer(_)
        | ExprKind::Real(_)
        | ExprKind::Text(_)
        | ExprKind::Blob(_)
        | ExprKind::Column { .. }
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
        | ExprKind::Call { .. } => None,
    });
    let kind = match (params.next(), params.next()) {
        (Some(left), Some(right)) => TypeErrorKind::ParamsBothSides { place, left, right },
        (Some(name), None) => TypeErrorKind::NeedsCast { what: What::Param(name), place },
        (None, _) => TypeErrorKind::NeedsCast { what: What::Null, place },
    };
    TypeError { kind, span: at }
}

/// What an expression is before its position types it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Shape<'s> {
    Typed(Ty<'s>),
    /// A bare parameter: its position gives its type.
    Param(Name<'s>),
    /// `null`: its position gives its type, nullable.
    Null,
}

/// Whether a comparison is `=` and its kind (a parameter is non-null, the result nullable if a
/// side is) or `is` (a parameter is nullable, the result is not).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cmp {
    Plain,
    Is,
}

/// An operand of a chain's link, for its errors: the link's operand, or the chain so far.
#[derive(Clone, Copy, Debug)]
struct Side<'s> {
    what: What<'s>,
    span: Span,
}

/// A table of the statement's scope.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Entry<'s> {
    /// The name the statement refers to it by: its alias, or its name as written.
    pub(crate) name: Name<'s>,
    /// `name` is an alias, not the table's name.
    pub(crate) alias: bool,
    pub(crate) table: &'s Table<'s>,
    /// The right side of a `left join`: every column is nullable.
    pub(crate) nullable: bool,
}

/// A column reference resolved in the scope.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Resolved<'s> {
    /// Its table's index in the scope.
    pub(crate) entry: usize,
    pub(crate) column: &'s Column<'s>,
    pub(crate) ty: Ty<'s>,
}

/// A parameter as typed so far.
#[derive(Clone, Copy, Debug)]
struct Seen<'s> {
    name: &'s str,
    /// The byte offset of its first use, which orders the description's parameters.
    first: u32,
    ty: Ty<'s>,
    form: Form,
}

/// The values of a `case`'s branches or `coalesce`'s arguments, unified.
#[derive(Clone, Copy, Debug)]
struct Unified<'s> {
    /// Their storage and Rust type, nullable if any is.
    ty: Ty<'s>,
    /// Every one of them is nullable.
    all_nullable: bool,
}

/// The typer of one statement: its scope, the parameters typed so far, and what the current
/// position allows.
pub(crate) struct Cx<'s, A: Allocator + Copy> {
    schema: Schema<'s>,
    plant: Option<Plant>,
    alloc: A,
    scope: Vec<Entry<'s>, A>,
    params: Vec<Seen<'s>, A>,
    /// Where an aggregate here would stand, when it may not; `None` where it may.
    aggregates_refused: Option<Place>,
    /// Where a column here would stand, when none is visible.
    columns_refused: Option<Place>,
    /// An aggregate was typed where one may be: the `select` is grouped.
    saw_aggregate: bool,
    depth: u32,
}

impl<'s, A: Allocator + Copy> Cx<'s, A> {
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn new(schema: &Schema<'s>, plant: Option<Plant>, alloc: A) -> Self {
        Cx {
            schema: *schema,
            plant,
            alloc,
            scope: Vec::new_in(alloc),
            params: Vec::new_in(alloc),
            aggregates_refused: None,
            columns_refused: None,
            saw_aggregate: false,
            depth: 0,
        }
    }

    /// Runs `f` with aggregates and columns allowed or refused as given, then restores them.
    fn within<T>(
        &mut self,
        aggregates: Option<Place>,
        columns: Option<Place>,
        f: impl FnOnce(&mut Self) -> Result<T, TypeError<'s>>,
    ) -> Result<T, TypeError<'s>> {
        #[cfg(test)]
        crate::tests::stack::touch();
        let saved = (self.aggregates_refused, self.columns_refused);
        self.aggregates_refused = aggregates;
        self.columns_refused = columns;
        let result = f(self);
        (self.aggregates_refused, self.columns_refused) = saved;
        result
    }

    /// The table `name` names.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn table(&self, name: &Name<'s>) -> Result<&'s Table<'s>, TypeError<'s>> {
        match table_of(&self.schema, name) {
            Some(table) => Ok(table),
            None => err(TypeErrorKind::NoSuchTable { table: name.text }, name.span),
        }
    }

    fn push_entry(&mut self, entry: Entry<'s>) -> Result<(), TypeError<'s>> {
        if self.scope.iter().any(|seen| names_same(&seen.name, &entry.name)) {
            return err(TypeErrorKind::TableTwice { name: entry.name.text }, entry.name.span);
        }
        self.scope.push(entry);
        Ok(())
    }

    /// The entry `table_ref` makes, its columns as nullable as the table's.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn table_entry(&self, table_ref: &TableRef<'s>) -> Result<Entry<'s>, TypeError<'s>> {
        let table = self.table(&table_ref.table)?;
        Ok(match table_ref.alias {
            Some(alias) => Entry { name: alias, alias: true, table, nullable: false },
            None => Entry { name: table_ref.table, alias: false, table, nullable: false },
        })
    }

    /// Puts the tables of `from` in scope, typing each join's `on` as it comes.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn enter_from<X: Allocator>(
        &mut self,
        from: &ast::From<'s, X>,
    ) -> Result<(), TypeError<'s>> {
        let first = self.table_entry(&from.first)?;
        self.push_entry(first)?;
        for join in from.joins.iter() {
            let nullable = match join.kind {
                JoinKind::Inner => false,
                JoinKind::Left => left_join_nullable(self.plant),
            };
            let entry = self.table_entry(&join.table)?;
            self.push_entry(Entry { nullable, ..entry })?;
            let on = Place::Clause(keyword::ON);
            self.within(Some(on), None, |cx| cx.boolean(&join.on, on).map(|_| ()))?;
        }
        Ok(())
    }

    /// Puts the one table of an `insert`, `update` or `delete` in scope.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn enter_table(&mut self, name: &Name<'s>) -> Result<&'s Table<'s>, TypeError<'s>> {
        let table = self.table(name)?;
        self.push_entry(Entry { name: *name, alias: false, table, nullable: false })?;
        Ok(table)
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn entry(&self, index: usize) -> Option<&Entry<'s>> {
        self.scope.get(index)
    }

    /// The index in scope of the table `name` names.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn find_entry(&self, name: &Name<'s>) -> Result<usize, TypeError<'s>> {
        match self.scope.iter().position(|entry| names_same(&entry.name, name)) {
            Some(index) => Ok(index),
            None => err(TypeErrorKind::NotInScope { table: name.text }, name.span),
        }
    }

    /// Resolves `table.column` or `column` in scope.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn resolve(
        &self,
        table: Option<&Name<'s>>,
        column: &Name<'s>,
    ) -> Result<Resolved<'s>, TypeError<'s>> {
        if let Some(place) = self.columns_refused {
            return err(
                TypeErrorKind::ColumnNotAllowed { column: column.text, place },
                column.span,
            );
        }
        match table {
            Some(table) => {
                let index = self.find_entry(table)?;
                let Some(entry) = self.scope.get(index) else {
                    return err(TypeErrorKind::NotInScope { table: table.text }, table.span);
                };
                match column_of(entry.table, column) {
                    Some(found) => {
                        Ok(Resolved { entry: index, column: found, ty: column_ty(found, entry) })
                    }
                    None => err(
                        TypeErrorKind::NoSuchColumn { table: table.text, column: column.text },
                        column.span,
                    ),
                }
            }
            None => {
                let mut found: Option<(Resolved<'s>, &'s str)> = None;
                for (index, entry) in self.scope.iter().enumerate() {
                    let Some(hit) = column_of(entry.table, column) else { continue };
                    if let Some((_, first)) = found {
                        return err(
                            TypeErrorKind::AmbiguousColumn {
                                column: column.text,
                                first,
                                second: entry.name.text,
                            },
                            column.span,
                        );
                    }
                    found = Some((
                        Resolved { entry: index, column: hit, ty: column_ty(hit, entry) },
                        entry.name.text,
                    ));
                }
                match found {
                    Some((resolved, _)) => Ok(resolved),
                    None => err(TypeErrorKind::UnknownColumn { column: column.text }, column.span),
                }
            }
        }
    }

    /// Records that parameter `name` has type `ty` (a list of it, for [`Form::List`]) at this
    /// use.
    fn bind(&mut self, name: Name<'s>, ty: Ty<'s>, form: Form) -> Result<(), TypeError<'s>> {
        for seen in self.params.iter_mut() {
            if seen.name == name.text {
                if seen.ty != ty || seen.form != form {
                    return err(
                        TypeErrorKind::ParamConflict {
                            name: name.text,
                            first: TyName::of(seen.ty, seen.form),
                            second: TyName::of(ty, form),
                        },
                        name.span,
                    );
                }
                seen.first = seen.first.min(name.span.start);
                return Ok(());
            }
        }
        self.params.push(Seen { name: name.text, first: name.span.start, ty, form });
        Ok(())
    }

    /// Types `e`, leaving a bare parameter or `null` for its position to type.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn synth<X: Allocator>(
        &mut self,
        e: &Expr<'s, X>,
    ) -> Result<Shape<'s>, TypeError<'s>> {
        #[cfg(test)]
        crate::tests::stack::touch();
        if self.depth >= MAX_DEPTH {
            return err(TypeErrorKind::TooDeep { limit: MAX_DEPTH }, e.span);
        }
        self.depth += 1;
        let shape = self.synth_kind(e);
        self.depth -= 1;
        shape
    }

    /// Types `e`, which must have a type of its own here.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn typed<X: Allocator>(
        &mut self,
        e: &Expr<'s, X>,
        place: Place,
    ) -> Result<Ty<'s>, TypeError<'s>> {
        #[cfg(test)]
        crate::tests::stack::touch();
        match self.synth(e)? {
            Shape::Typed(ty) => Ok(ty),
            Shape::Param(name) => {
                err(TypeErrorKind::NeedsCast { what: What::Param(name.text), place }, e.span)
            }
            Shape::Null => err(TypeErrorKind::NeedsCast { what: What::Null, place }, e.span),
        }
    }

    /// Types `e` at a position of one storage: a parameter takes it with the plain Rust type,
    /// non-null. Gives whether `e` is nullable.
    fn fixed<X: Allocator>(
        &mut self,
        e: &Expr<'s, X>,
        storage: Storage,
        place: Place,
    ) -> Result<bool, TypeError<'s>> {
        #[cfg(test)]
        crate::tests::stack::touch();
        let shape = self.synth(e)?;
        self.fixed_shape(shape, what(e), e.span, storage, place)
    }

    /// [`Self::fixed`] of an operand already typed as `shape`; `what` and `at` name it.
    fn fixed_shape(
        &mut self,
        shape: Shape<'s>,
        what: What<'s>,
        at: Span,
        storage: Storage,
        place: Place,
    ) -> Result<bool, TypeError<'s>> {
        match shape {
            Shape::Param(name) => {
                self.bind(name, plain(storage), Form::Scalar)?;
                Ok(false)
            }
            Shape::Null => Ok(true),
            Shape::Typed(ty) if ty.storage == storage => Ok(ty.nullable),
            Shape::Typed(ty) => err(
                TypeErrorKind::WrongStorage {
                    what,
                    place,
                    expected: Expected::One(storage),
                    found: ty.storage,
                },
                at,
            ),
        }
    }

    fn boolean<X: Allocator>(
        &mut self,
        e: &Expr<'s, X>,
        place: Place,
    ) -> Result<bool, TypeError<'s>> {
        #[cfg(test)]
        crate::tests::stack::touch();
        self.fixed(e, Storage::Boolean, place)
    }

    fn number<X: Allocator>(
        ty: &Ty<'s>,
        e: &Expr<'s, X>,
        place: Place,
    ) -> Result<(), TypeError<'s>> {
        Self::number_at(ty, what(e), e.span, place)
    }

    /// [`Self::number`] of an operand `what` at `at`.
    fn number_at(ty: &Ty<'s>, what: What<'s>, at: Span, place: Place) -> Result<(), TypeError<'s>> {
        if is_number(ty.storage) {
            Ok(())
        } else {
            err(
                TypeErrorKind::WrongStorage {
                    what,
                    place,
                    expected: Expected::Number,
                    found: ty.storage,
                },
                at,
            )
        }
    }

    fn synth_kind<X: Allocator>(&mut self, e: &Expr<'s, X>) -> Result<Shape<'s>, TypeError<'s>> {
        #[cfg(test)]
        crate::tests::stack::touch();
        let ty = match &e.kind {
            ExprKind::Null => return Ok(Shape::Null),
            ExprKind::Param(name) => return Ok(Shape::Param(*name)),
            ExprKind::Boolean(_) => plain(Storage::Boolean),
            ExprKind::Integer(magnitude) => {
                if *magnitude > i64::MAX.unsigned_abs() {
                    return err(TypeErrorKind::IntegerTooLarge { magnitude: *magnitude }, e.span);
                }
                plain(Storage::Integer)
            }
            ExprKind::Real(value) => {
                if !value.is_finite() {
                    return err(TypeErrorKind::RealNotFinite, e.span);
                }
                plain(Storage::Real)
            }
            ExprKind::Text(_) => plain(Storage::Text),
            ExprKind::Blob(_) => plain(Storage::Blob),
            ExprKind::Column { table, column } => self.resolve(table.as_ref(), column)?.ty,
            ExprKind::Unary { op: UnaryOp::Neg, operand } => {
                // `-9223372036854775808` is the one literal above `i64::MAX` that is an integer.
                if let ExprKind::Integer(magnitude) = operand.kind
                    && magnitude == i64::MIN.unsigned_abs()
                {
                    plain(Storage::Integer)
                } else {
                    let place = Place::Operand("-");
                    let ty = self.typed(operand, place)?;
                    Self::number(&ty, operand, place)?;
                    Ty { nullable: ty.nullable, ..plain(ty.storage) }
                }
            }
            ExprKind::Unary { op: UnaryOp::Not, operand } => {
                let nullable = self.boolean(operand, Place::Operand(keyword::NOT))?;
                Ty { nullable, ..plain(Storage::Boolean) }
            }
            ExprKind::Chain { first, rest } => return self.chain(first, rest),
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
                let place = Place::Operand(compare_text(*op));
                let cmp = match op {
                    CompareOp::Is | CompareOp::IsNot => Cmp::Is,
                    CompareOp::Eq
                    | CompareOp::Ne
                    | CompareOp::Lt
                    | CompareOp::Le
                    | CompareOp::Gt
                    | CompareOp::Ge => Cmp::Plain,
                };
                let nullable = self.compare(left, iter::once(&**right), place, cmp, e.span)?;
                Ty { nullable, ..plain(Storage::Boolean) }
            }
            ExprKind::IsNull { operand, negated } => {
                let place = Place::Operand(if *negated { "is not null" } else { "is null" });
                self.typed(operand, place)?;
                plain(Storage::Boolean)
            }
            ExprKind::InList { operand, list, .. } => {
                let place = Place::Operand(keyword::IN);
                let nullable = self.compare(operand, list.iter(), place, Cmp::Plain, e.span)?;
                Ty { nullable, ..plain(Storage::Boolean) }
            }
            ExprKind::InParam { operand, list, .. } => {
                let ty = self.typed(operand, Place::Operand(keyword::IN))?;
                let item = Ty { storage: ty.storage, rust: ty.rust, nullable: false };
                self.bind(*list, item, Form::List)?;
                Ty { nullable: ty.nullable, ..plain(Storage::Boolean) }
            }
            ExprKind::Between { operand, low, high, .. } => {
                let bounds = [&**low, &**high];
                let nullable = self.compare(
                    operand,
                    bounds.into_iter(),
                    Place::Operand(keyword::BETWEEN),
                    Cmp::Plain,
                    e.span,
                )?;
                Ty { nullable, ..plain(Storage::Boolean) }
            }
            ExprKind::Like { operand, pattern, .. } => {
                let place = Place::Operand(keyword::LIKE);
                let left = self.fixed(operand, Storage::Text, place)?;
                let right = self.fixed(pattern, Storage::Text, place)?;
                Ty { nullable: left || right, ..plain(Storage::Boolean) }
            }
            ExprKind::Case { operand, arms, otherwise } => {
                self.case(operand.as_deref(), arms, otherwise.as_deref(), e.span)?
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
                let storage = *to;
                match self.synth(operand)? {
                    // SQLite has no boolean to cast to (`cast(5 as integer)` is 5, not a 0 or 1)
                    // and Postgres refuses `cast(bigint as boolean)`: only a Boolean or a
                    // parameter is cast to one.
                    Shape::Typed(ty)
                        if storage == Storage::Boolean && ty.storage != Storage::Boolean =>
                    {
                        return err(
                            TypeErrorKind::CastToBoolean { what: what(operand), found: ty.storage },
                            e.span,
                        );
                    }
                    Shape::Null if storage == Storage::Boolean => {
                        return err(
                            TypeErrorKind::CastToBoolean {
                                what: What::Null,
                                found: Storage::Integer,
                            },
                            e.span,
                        );
                    }
                    Shape::Typed(ty) => Ty { nullable: ty.nullable, ..plain(storage) },
                    Shape::Param(name) => {
                        self.bind(name, plain(storage), Form::Scalar)?;
                        plain(storage)
                    }
                    Shape::Null => Ty { nullable: true, ..plain(storage) },
                }
            }
            ExprKind::Call { function, args: args @ (CallArgs::Star | CallArgs::List { .. }) } => {
                self.call(function, args, e.span)?
            }
        };
        Ok(Shape::Typed(ty))
    }

    /// A chain, `first op a op b ..`, typed as the left-nested tree it means: `first`, then for
    /// each link the rule of its operator over the running type and the link's operand. A loop,
    /// not a recursion per link, so a chain of any length takes one frame and one level.
    fn chain<X: Allocator>(
        &mut self,
        first: &Expr<'s, X>,
        rest: &[ChainLink<'s, X>],
    ) -> Result<Shape<'s>, TypeError<'s>> {
        #[cfg(test)]
        crate::tests::stack::touch();
        let mut running = self.synth(first)?;
        let mut left = Side { what: what(first), span: first.span };
        for link in rest {
            let right_shape = self.synth(&link.operand)?;
            let right = Side { what: what(&link.operand), span: link.operand.span };
            let at = Span { start: first.span.start, end: link.operand.span.end };
            let ty = self.link(link.op, (running, left), (right_shape, right), at)?;
            running = Shape::Typed(ty);
            left = Side { what: What::Operator(chain_text(link.op)), span: at };
        }
        Ok(running)
    }

    /// One link of a chain: `left op right`, both already typed.
    fn link(
        &mut self,
        op: ChainOp,
        left: (Shape<'s>, Side<'s>),
        right: (Shape<'s>, Side<'s>),
        at: Span,
    ) -> Result<Ty<'s>, TypeError<'s>> {
        let place = Place::Operand(chain_text(op));
        Ok(match op {
            ChainOp::Or | ChainOp::And => {
                let l =
                    self.fixed_shape(left.0, left.1.what, left.1.span, Storage::Boolean, place)?;
                let r =
                    self.fixed_shape(right.0, right.1.what, right.1.span, Storage::Boolean, place)?;
                Ty { nullable: l || r, ..plain(Storage::Boolean) }
            }
            ChainOp::Concat => {
                let l = self.fixed_shape(left.0, left.1.what, left.1.span, Storage::Text, place)?;
                let r =
                    self.fixed_shape(right.0, right.1.what, right.1.span, Storage::Text, place)?;
                Ty { nullable: l || r, ..plain(Storage::Text) }
            }
            ChainOp::Add | ChainOp::Sub | ChainOp::Mul | ChainOp::Div | ChainOp::Rem => {
                self.arithmetic(op, left, right, at)?
            }
        })
    }

    /// `+ - * / %`: two Integers or two Reals. A parameter takes the other side's storage with
    /// the plain Rust type, non-null; `null` makes the result null.
    fn arithmetic(
        &mut self,
        op: ChainOp,
        left: (Shape<'s>, Side<'s>),
        right: (Shape<'s>, Side<'s>),
        at: Span,
    ) -> Result<Ty<'s>, TypeError<'s>> {
        let symbol = chain_text(op);
        let place = Place::Operand(symbol);
        let anchor = match (left.0, right.0) {
            (Shape::Typed(ty), Shape::Typed(_) | Shape::Param(_) | Shape::Null)
            | (Shape::Param(_) | Shape::Null, Shape::Typed(ty)) => ty,
            (Shape::Param(l), Shape::Param(r)) => {
                return err(
                    TypeErrorKind::ParamsBothSides { place, left: l.text, right: r.text },
                    at,
                );
            }
            (Shape::Param(name), Shape::Null) | (Shape::Null, Shape::Param(name)) => {
                return err(TypeErrorKind::NeedsCast { what: What::Param(name.text), place }, at);
            }
            (Shape::Null, Shape::Null) => {
                return err(TypeErrorKind::NeedsCast { what: What::Null, place }, at);
            }
        };
        // `/` and `%` are null for a zero divisor on SQLite.
        let mut nullable = matches!(op, ChainOp::Div | ChainOp::Rem);
        for (shape, side) in [left, right] {
            match shape {
                Shape::Typed(ty) => {
                    Self::number_at(&ty, side.what, side.span, place)?;
                    if ty.storage != anchor.storage {
                        return err(
                            TypeErrorKind::MixedArithmetic {
                                op: symbol,
                                left: anchor.storage,
                                right: ty.storage,
                            },
                            at,
                        );
                    }
                    nullable |= ty.nullable;
                }
                Shape::Null => nullable = true,
                Shape::Param(name) => self.bind(name, plain(anchor.storage), Form::Scalar)?,
            }
        }
        if op == ChainOp::Rem && anchor.storage == Storage::Real {
            return err(TypeErrorKind::RealRemainder, at);
        }
        Ok(Ty { nullable, ..plain(anchor.storage) })
    }

    /// `operand` compared with each of `others` (`=`, `in (..)`, `between`, a `case`'s `when`s):
    /// one storage, and two Rust types that are not plain must be one. A parameter takes the
    /// storage and Rust type of the first typed operand, non-null (nullable for `is`). Gives
    /// whether the comparison is nullable.
    fn compare<'e, X: Allocator + 'e, I>(
        &mut self,
        operand: &'e Expr<'s, X>,
        others: I,
        place: Place,
        cmp: Cmp,
        at: Span,
    ) -> Result<bool, TypeError<'s>>
    where
        's: 'e,
        I: Iterator<Item = &'e Expr<'s, X>> + Clone,
    {
        #[cfg(test)]
        crate::tests::stack::touch();
        let mut nullable = false;
        let mut anchor: Option<Ty<'s>> = None;
        for item in iter::once(operand).chain(others.clone()) {
            match self.synth(item)? {
                Shape::Typed(ty) => {
                    nullable |= ty.nullable;
                    match anchor {
                        None => anchor = Some(ty),
                        Some(first) => Self::comparable(first, ty, place, at)?,
                    }
                }
                Shape::Null => nullable = true,
                Shape::Param(_) => {}
            }
        }
        let Some(anchor) = anchor else {
            return Err(no_anchor(iter::once(operand).chain(others), place, at));
        };
        let param = Ty { storage: anchor.storage, rust: anchor.rust, nullable: cmp == Cmp::Is };
        for item in iter::once(operand).chain(others) {
            if let ExprKind::Param(name) = &item.kind {
                self.bind(*name, param, Form::Scalar)?;
            }
        }
        Ok(match cmp {
            Cmp::Plain => nullable,
            Cmp::Is => false,
        })
    }

    fn comparable(
        first: Ty<'s>,
        other: Ty<'s>,
        place: Place,
        at: Span,
    ) -> Result<(), TypeError<'s>> {
        if first.storage != other.storage {
            return err(
                TypeErrorKind::CompareStorage { place, left: first.storage, right: other.storage },
                at,
            );
        }
        if !is_plain(&first) && !is_plain(&other) && first.rust != other.rust {
            return err(
                TypeErrorKind::CompareTypes {
                    place,
                    left: first.rust.name,
                    right: other.rust.name,
                },
                at,
            );
        }
        Ok(())
    }

    /// Values of one storage and one Rust type; a parameter among them takes it non-null, a
    /// `null` makes it nullable.
    fn unify<'e, X: Allocator + 'e, I>(
        &mut self,
        items: I,
        place: Place,
        at: Span,
    ) -> Result<Unified<'s>, TypeError<'s>>
    where
        's: 'e,
        I: Iterator<Item = &'e Expr<'s, X>> + Clone,
    {
        #[cfg(test)]
        crate::tests::stack::touch();
        let mut anchor: Option<Ty<'s>> = None;
        let mut any_nullable = false;
        let mut all_nullable = true;
        for item in items.clone() {
            match self.synth(item)? {
                Shape::Typed(ty) => {
                    if let Some(first) = anchor
                        && (first.storage != ty.storage || first.rust != ty.rust)
                    {
                        return err(
                            TypeErrorKind::BranchTypes {
                                place,
                                first: TyName::of(first, Form::Scalar),
                                other: TyName::of(ty, Form::Scalar),
                            },
                            item.span,
                        );
                    }
                    if anchor.is_none() {
                        anchor = Some(ty);
                    }
                    any_nullable |= ty.nullable;
                    all_nullable &= ty.nullable;
                }
                Shape::Null => any_nullable = true,
                Shape::Param(_) => all_nullable = false,
            }
        }
        let Some(anchor) = anchor else {
            return Err(no_anchor(items, place, at));
        };
        let param = Ty { storage: anchor.storage, rust: anchor.rust, nullable: false };
        for item in items {
            if let ExprKind::Param(name) = &item.kind {
                self.bind(*name, param, Form::Scalar)?;
            }
        }
        Ok(Unified {
            ty: Ty { storage: anchor.storage, rust: anchor.rust, nullable: any_nullable },
            all_nullable,
        })
    }

    fn case<X: Allocator>(
        &mut self,
        operand: Option<&Expr<'s, X>>,
        arms: &[CaseArm<'s, X>],
        otherwise: Option<&Expr<'s, X>>,
        at: Span,
    ) -> Result<Ty<'s>, TypeError<'s>> {
        #[cfg(test)]
        crate::tests::stack::touch();
        match operand {
            Some(operand) => {
                self.compare(
                    operand,
                    arms.iter().map(|arm| &arm.when),
                    Place::Operand(keyword::CASE),
                    Cmp::Plain,
                    at,
                )?;
            }
            None => {
                for arm in arms {
                    self.boolean(&arm.when, Place::Operand(keyword::WHEN))?;
                }
            }
        }
        let branches = arms.iter().map(|arm| &arm.then).chain(otherwise);
        let unified = self.unify(branches, Place::Operand(keyword::CASE), at)?;
        Ok(Ty {
            storage: unified.ty.storage,
            rust: unified.ty.rust,
            nullable: unified.ty.nullable || otherwise.is_none(),
        })
    }

    fn call<X: Allocator>(
        &mut self,
        name: &Name<'s>,
        args: &CallArgs<'s, X>,
        at: Span,
    ) -> Result<Ty<'s>, TypeError<'s>> {
        #[cfg(test)]
        crate::tests::stack::touch();
        let Some(function) = funcs::lookup(name) else {
            return err(TypeErrorKind::UnknownFunction { name: name.text }, name.span);
        };
        if function.aggregate {
            if let Some(place) = self.aggregates_refused {
                return err(
                    TypeErrorKind::AggregateNotAllowed { function: function.name, place },
                    at,
                );
            }
            self.saw_aggregate = true;
        }
        match args {
            CallArgs::Star if function.takes_star => Ok(plain(Storage::Integer)),
            CallArgs::Star => err(TypeErrorKind::StarNotCount { function: function.name }, at),
            CallArgs::List { distinct, args } => {
                if *distinct && !function.aggregate {
                    return err(
                        TypeErrorKind::DistinctNotAggregate { function: function.name },
                        at,
                    );
                }
                if !function.arity.admits(args.len()) {
                    return err(
                        TypeErrorKind::Arity {
                            function: function.name,
                            arity: function.arity,
                            found: args.len(),
                        },
                        at,
                    );
                }
                let aggregates = if function.aggregate {
                    Some(Place::InsideAggregate)
                } else {
                    self.aggregates_refused
                };
                let columns = self.columns_refused;
                self.within(aggregates, columns, |cx| cx.call_args(function, args, at))
            }
        }
    }

    fn call_args<X: Allocator>(
        &mut self,
        function: &'static Function,
        args: &[Expr<'s, X>],
        at: Span,
    ) -> Result<Ty<'s>, TypeError<'s>> {
        #[cfg(test)]
        crate::tests::stack::touch();
        let place = Place::Argument(function.name);
        let Some(first) = args.first() else {
            let found = 0;
            return err(
                TypeErrorKind::Arity { function: function.name, arity: function.arity, found },
                at,
            );
        };
        let (arg, all_nullable) = match function.args {
            Args::Any => {
                let ty = self.typed(first, place)?;
                (ty, ty.nullable)
            }
            Args::NotBoolean => {
                let ty = self.typed(first, place)?;
                if ty.storage == Storage::Boolean {
                    return err(
                        TypeErrorKind::WrongStorage {
                            what: what(first),
                            place,
                            expected: Expected::NotBoolean,
                            found: ty.storage,
                        },
                        first.span,
                    );
                }
                (ty, ty.nullable)
            }
            Args::Number => {
                let ty = self.typed(first, place)?;
                Self::number(&ty, first, place)?;
                (ty, ty.nullable)
            }
            Args::Text => {
                let nullable = self.fixed(first, Storage::Text, place)?;
                (Ty { nullable, ..plain(Storage::Text) }, nullable)
            }
            Args::OneType => {
                let unified = self.unify(args.iter(), place, at)?;
                (unified.ty, unified.all_nullable)
            }
        };
        Ok(match function.gives {
            Gives::Count => plain(Storage::Integer),
            Gives::StorageNullable => Ty { nullable: true, ..plain(arg.storage) },
            Gives::ArgumentNullable => Ty { storage: arg.storage, rust: arg.rust, nullable: true },
            Gives::RealNullable => Ty { nullable: true, ..plain(Storage::Real) },
            Gives::FirstNonNull => {
                Ty { storage: arg.storage, rust: arg.rust, nullable: all_nullable }
            }
            Gives::Text => Ty { nullable: arg.nullable, ..plain(Storage::Text) },
            Gives::Integer => Ty { nullable: arg.nullable, ..plain(Storage::Integer) },
            Gives::Storage => Ty { nullable: arg.nullable, ..plain(arg.storage) },
        })
    }

    /// The storage of `e`, which has a type of its own; for rendering a `sum`.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn storage_of<X: Allocator>(
        &mut self,
        e: &Expr<'s, X>,
    ) -> Result<Storage, TypeError<'s>> {
        let place = Place::Argument("sum");
        let aggregates = Some(Place::InsideAggregate);
        let columns = self.columns_refused;
        self.within(aggregates, columns, |cx| cx.typed(e, place).map(|ty| ty.storage))
    }

    /// `e` where an Integer that is never null goes (`limit`, `offset`); it sees no columns.
    fn count_clause<X: Allocator>(
        &mut self,
        e: &Expr<'s, X>,
        clause: &'static str,
    ) -> Result<(), TypeError<'s>> {
        let place = Place::Clause(clause);
        let nullable =
            self.within(Some(place), Some(place), |cx| cx.fixed(e, Storage::Integer, place))?;
        if nullable {
            return err(TypeErrorKind::NullableNotAllowed { what: what(e), place }, e.span);
        }
        Ok(())
    }

    /// `e` assigned to `column` (`values`, `set`): a parameter takes the column's type and
    /// nullability; `null` goes only into a nullable column; a literal (or a negated number
    /// literal) of the column's storage goes into it whatever its Rust type, as a comparison
    /// takes it; any other value has the column's storage and Rust type, and is non-null unless
    /// the column is nullable.
    fn assign<X: Allocator>(
        &mut self,
        e: &Expr<'s, X>,
        column: &'s Column<'s>,
    ) -> Result<(), TypeError<'s>> {
        let target = Ty { storage: column.storage, rust: column.rust, nullable: column.nullable };
        match self.synth(e)? {
            Shape::Param(name) => self.bind(name, target, Form::Scalar),
            Shape::Null if column.nullable => Ok(()),
            Shape::Null => err(TypeErrorKind::NullIntoNonNull { column: column.name }, e.span),
            Shape::Typed(ty) => {
                let rust_fits = ty.rust == column.rust || is_literal(e);
                if ty.storage != column.storage || !rust_fits {
                    return err(
                        TypeErrorKind::AssignType {
                            column: column.name,
                            expected: TyName::of(target, Form::Scalar),
                            found: TyName::of(ty, Form::Scalar),
                        },
                        e.span,
                    );
                }
                if ty.nullable && !column.nullable {
                    return err(
                        TypeErrorKind::NullableIntoNonNull { column: column.name, what: what(e) },
                        e.span,
                    );
                }
                Ok(())
            }
        }
    }

    /// The result columns of a `select` or a `returning`, in order.
    fn result_columns<X: Allocator>(
        &mut self,
        columns: &[ResultColumn<'s, X>],
        out: &mut Vec<ResultCol<'s>, A>,
    ) -> Result<(), TypeError<'s>> {
        for column in columns {
            match column {
                ResultColumn::Star { span } => {
                    if self.scope.is_empty() {
                        return err(TypeErrorKind::StarWithoutFrom, *span);
                    }
                    for entry in self.scope.iter() {
                        for field in entry.table.columns.iter() {
                            let col = ResultCol { name: field.name, ty: column_ty(field, entry) };
                            push_result(out, col, *span)?;
                        }
                    }
                }
                ResultColumn::TableStar { table } => {
                    let index = self.find_entry(table)?;
                    let Some(entry) = self.scope.get(index) else {
                        return err(TypeErrorKind::NotInScope { table: table.text }, table.span);
                    };
                    for field in entry.table.columns.iter() {
                        let col = ResultCol { name: field.name, ty: column_ty(field, entry) };
                        push_result(out, col, table.span)?;
                    }
                }
                ResultColumn::Expr { expr, alias } => {
                    let ty = self.typed(expr, Place::Result)?;
                    let name = match alias {
                        Some(alias) => alias.text,
                        None => match &expr.kind {
                            ExprKind::Column { table, column } => {
                                self.resolve(table.as_ref(), column)?.column.name
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
                                return err(TypeErrorKind::Unnamed { what: what(expr) }, expr.span);
                            }
                        },
                    };
                    push_result(out, ResultCol { name, ty }, expr.span)?;
                }
            }
        }
        Ok(())
    }

    fn returning<X: Allocator>(
        &mut self,
        columns: &[ResultColumn<'s, X>],
    ) -> Result<Returns<'s, A>, TypeError<'s>> {
        if columns.is_empty() {
            return Ok(Returns::Count);
        }
        let mut out = Vec::with_capacity_in(columns.len(), self.alloc);
        self.within(Some(Place::Clause(keyword::RETURNING)), None, |cx| {
            cx.result_columns(columns, &mut out)
        })?;
        Ok(Returns::Rows(out))
    }

    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn select<X: Allocator>(
        &mut self,
        select: &ast::Select<'s, X>,
    ) -> Result<Returns<'s, A>, TypeError<'s>> {
        if let Some(from) = &select.from {
            self.enter_from(from)?;
        }
        let mut out = Vec::with_capacity_in(select.columns.len(), self.alloc);
        self.within(None, None, |cx| cx.result_columns(&select.columns, &mut out))?;
        if let Some(filter) = &select.filter {
            let place = Place::Clause(keyword::WHERE);
            self.within(Some(place), None, |cx| cx.boolean(filter, place))?;
        }
        for group in select.group_by.iter() {
            positional(group, "group by")?;
            let place = Place::Clause("group by");
            self.within(Some(place), None, |cx| cx.typed(group, place))?;
        }
        if let Some(having) = &select.having {
            let place = Place::Clause(keyword::HAVING);
            self.within(None, None, |cx| cx.boolean(having, place))?;
        }
        for term in select.order_by.iter() {
            positional(&term.expr, "order by")?;
            if order_alias(&term.expr, &select.columns).is_some() {
                continue;
            }
            let place = Place::Clause("order by");
            self.within(None, None, |cx| cx.typed(&term.expr, place))?;
        }
        if let Some(limit) = &select.limit {
            self.count_clause(limit, keyword::LIMIT)?;
        }
        if let Some(offset) = &select.offset {
            self.count_clause(offset, keyword::OFFSET)?;
        }

        let grouped = !select.group_by.is_empty() || self.saw_aggregate;
        if let Some(having) = &select.having
            && !grouped
        {
            return err(TypeErrorKind::HavingWithoutGroup, having.span);
        }
        if grouped {
            for column in select.columns.iter() {
                match column {
                    ResultColumn::Star { span } => {
                        for index in 0..self.scope.len() {
                            self.entry_grouped(index, &select.group_by, *span)?;
                        }
                    }
                    ResultColumn::TableStar { table } => {
                        let index = self.find_entry(table)?;
                        self.entry_grouped(index, &select.group_by, table.span)?;
                    }
                    ResultColumn::Expr { expr, .. } => self.grouped(expr, &select.group_by, 0)?,
                }
            }
            if let Some(having) = &select.having {
                self.grouped(having, &select.group_by, 0)?;
            }
            for term in select.order_by.iter() {
                if order_alias(&term.expr, &select.columns).is_none() {
                    self.grouped(&term.expr, &select.group_by, 0)?;
                }
            }
        }
        if select.distinct {
            for term in select.order_by.iter() {
                if order_alias(&term.expr, &select.columns).is_none()
                    && !self.in_results(&term.expr, &select.columns)?
                {
                    return err(
                        TypeErrorKind::NotInDistinct { what: what(&term.expr) },
                        term.expr.span,
                    );
                }
            }
        }
        Ok(Returns::Rows(out))
    }

    /// Whether `group by` has a plain column that resolves to column `column` of entry `entry`.
    fn column_grouped<X: Allocator>(
        &self,
        entry: usize,
        column: &'s Column<'s>,
        group_by: &[Expr<'s, X>],
    ) -> bool {
        group_by.iter().any(|group| match &group.kind {
            ExprKind::Column { table, column: name } => {
                self.resolve(table.as_ref(), name).is_ok_and(|resolved| {
                    resolved.entry == entry && core::ptr::eq(resolved.column, column)
                })
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
            | ExprKind::Call { .. } => false,
        })
    }

    /// Every column of entry `index` is grouped (`*` in a grouped `select`).
    fn entry_grouped<X: Allocator>(
        &self,
        index: usize,
        group_by: &[Expr<'s, X>],
        at: Span,
    ) -> Result<(), TypeError<'s>> {
        let Some(entry) = self.scope.get(index) else { return Ok(()) };
        for column in entry.table.columns.iter() {
            if !self.column_grouped(index, column, group_by) {
                return err(TypeErrorKind::NotGrouped { column: column.name }, at);
            }
        }
        Ok(())
    }

    /// Every column of `e` outside an aggregate is covered by `group by`: `e` or the part of
    /// it is the same as a `group by` expression, or is a column a `group by` column resolves
    /// to.
    fn grouped<X: Allocator>(
        &self,
        e: &Expr<'s, X>,
        group_by: &[Expr<'s, X>],
        depth: u32,
    ) -> Result<(), TypeError<'s>> {
        #[cfg(test)]
        crate::tests::stack::touch();
        if depth >= MAX_DEPTH {
            return err(TypeErrorKind::TooDeep { limit: MAX_DEPTH }, e.span);
        }
        if group_by.iter().any(|group| same_expr(group, e)) {
            return Ok(());
        }
        match &e.kind {
            ExprKind::Column { table, column } => {
                let resolved = self.resolve(table.as_ref(), column)?;
                if self.column_grouped(resolved.entry, resolved.column, group_by) {
                    Ok(())
                } else {
                    err(TypeErrorKind::NotGrouped { column: column.text }, e.span)
                }
            }
            ExprKind::Call { function, .. }
                if funcs::lookup(function).is_some_and(|f| f.aggregate) =>
            {
                Ok(())
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
                for_each_child(e, &mut |child| self.grouped(child, group_by, depth + 1))
            }
        }
    }

    /// Whether `e` is one of the result columns `columns` (an `order by` term of a `select
    /// distinct`): the same expression, or a column `*` or `t.*` covers.
    fn in_results<X: Allocator>(
        &self,
        e: &Expr<'s, X>,
        columns: &[ResultColumn<'s, X>],
    ) -> Result<bool, TypeError<'s>> {
        let resolved = match &e.kind {
            ExprKind::Column { table, column } => Some(self.resolve(table.as_ref(), column)?),
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
            | ExprKind::Call { .. } => None,
        };
        Ok(columns.iter().any(|column| match column {
            ResultColumn::Expr { expr, .. } => {
                same_expr(expr, e)
                    || match (&expr.kind, resolved) {
                        (ExprKind::Column { table, column }, Some(want)) => {
                            self.resolve(table.as_ref(), column).is_ok_and(|got| {
                                got.entry == want.entry && core::ptr::eq(got.column, want.column)
                            })
                        }
                        _ => false,
                    }
            }
            ResultColumn::Star { .. } => resolved.is_some(),
            ResultColumn::TableStar { table } => match (self.find_entry(table), resolved) {
                (Ok(index), Some(want)) => index == want.entry,
                _ => false,
            },
        }))
    }

    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn insert<X: Allocator>(
        &mut self,
        insert: &ast::Insert<'s, X>,
    ) -> Result<Returns<'s, A>, TypeError<'s>> {
        let table = self.table(&insert.table)?;
        let mut listed: Vec<&'s Column<'s>, A> =
            Vec::with_capacity_in(insert.columns.len(), self.alloc);
        for name in insert.columns.iter() {
            let Some(column) = column_of(table, name) else {
                return err(
                    TypeErrorKind::NoSuchColumn { table: insert.table.text, column: name.text },
                    name.span,
                );
            };
            if listed.iter().any(|seen| core::ptr::eq(*seen, column)) {
                return err(TypeErrorKind::ColumnTwice { column: name.text }, name.span);
            }
            listed.push(column);
        }
        let key = integer_key(table);
        for column in table.columns.iter() {
            let given = listed.iter().any(|seen| core::ptr::eq(*seen, column));
            let assigned = key.is_some_and(|key| core::ptr::eq(key, column));
            if !column.nullable && !given && !assigned {
                return err(
                    TypeErrorKind::MissingColumn { table: table.name, column: column.name },
                    insert.table.span,
                );
            }
        }
        let place = Place::Clause(keyword::VALUES);
        for (index, row) in insert.rows.iter().enumerate() {
            if row.len() != listed.len() {
                let span = match (row.first(), row.last()) {
                    (Some(first), Some(last)) => {
                        Span { start: first.span.start, end: last.span.end }
                    }
                    _ => insert.table.span,
                };
                return err(
                    TypeErrorKind::RowWidth {
                        row: index.saturating_add(1),
                        columns: listed.len(),
                        values: row.len(),
                    },
                    span,
                );
            }
            for (value, column) in row.iter().zip(listed.iter()) {
                let column = *column;
                self.within(Some(place), Some(place), |cx| cx.assign(value, column))?;
            }
        }
        self.push_entry(Entry { name: insert.table, alias: false, table, nullable: false })?;
        self.returning(&insert.returning)
    }

    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn update<X: Allocator>(
        &mut self,
        update: &ast::Update<'s, X>,
    ) -> Result<Returns<'s, A>, TypeError<'s>> {
        let table = self.enter_table(&update.table)?;
        let mut set: Vec<&'s Column<'s>, A> = Vec::with_capacity_in(update.sets.len(), self.alloc);
        let place = Place::Clause(keyword::SET);
        for assignment in update.sets.iter() {
            let Some(column) = column_of(table, &assignment.column) else {
                return err(
                    TypeErrorKind::NoSuchColumn {
                        table: update.table.text,
                        column: assignment.column.text,
                    },
                    assignment.column.span,
                );
            };
            if set.iter().any(|seen| core::ptr::eq(*seen, column)) {
                return err(
                    TypeErrorKind::ColumnTwice { column: assignment.column.text },
                    assignment.column.span,
                );
            }
            set.push(column);
            self.within(Some(place), None, |cx| cx.assign(&assignment.value, column))?;
        }
        if let Some(filter) = &update.filter {
            let place = Place::Clause(keyword::WHERE);
            self.within(Some(place), None, |cx| cx.boolean(filter, place))?;
        }
        self.returning(&update.returning)
    }

    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn delete<X: Allocator>(
        &mut self,
        delete: &ast::Delete<'s, X>,
    ) -> Result<Returns<'s, A>, TypeError<'s>> {
        self.enter_table(&delete.table)?;
        if let Some(filter) = &delete.filter {
            let place = Place::Clause(keyword::WHERE);
            self.within(Some(place), None, |cx| cx.boolean(filter, place))?;
        }
        self.returning(&delete.returning)
    }

    /// The description: each parameter once, in the order it first appears in the text.
    fn finish(self, returns: Returns<'s, A>) -> Description<'s, A> {
        let mut seen = self.params;
        seen.sort_unstable_by_key(|param| param.first);
        let mut params = Vec::with_capacity_in(seen.len(), self.alloc);
        for param in seen.iter() {
            params.push(Param { name: param.name, ty: param.ty, list: param.form.is_list() });
        }
        Description { params, returns }
    }
}

/// Pushes a result column, refusing a second of one name without regard to ASCII case.
fn push_result<'s, A: Allocator>(
    out: &mut Vec<ResultCol<'s>, A>,
    column: ResultCol<'s>,
    at: Span,
) -> Result<(), TypeError<'s>> {
    if out.iter().any(|seen| seen.name.eq_ignore_ascii_case(column.name)) {
        return err(TypeErrorKind::DuplicateResultName { name: column.name }, at);
    }
    out.push(column);
    Ok(())
}

/// Refuses an integer literal as a `group by` or `order by` term: positional on the engines,
/// and not in the subset.
fn positional<'s, X: Allocator>(
    e: &Expr<'s, X>,
    clause: &'static str,
) -> Result<(), TypeError<'s>> {
    match e.kind {
        ExprKind::Integer(position) => err(TypeErrorKind::Positional { clause, position }, e.span),
        ExprKind::Null
        | ExprKind::Boolean(_)
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
        | ExprKind::Call { .. } => Ok(()),
    }
}

/// The result column alias an `order by` term names: a bare unquoted or quoted name equal to
/// an alias, which both engines read as that result column before any table's column.
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn order_alias<'a, 's, X: Allocator>(
    e: &Expr<'s, X>,
    columns: &'a [ResultColumn<'s, X>],
) -> Option<&'a Name<'s>> {
    let ExprKind::Column { table: None, column } = &e.kind else { return None };
    columns.iter().find_map(|result| match result {
        ResultColumn::Expr { alias: Some(alias), .. } if names_same(alias, column) => Some(alias),
        ResultColumn::Expr { .. } | ResultColumn::Star { .. } | ResultColumn::TableStar { .. } => {
            None
        }
    })
}

/// Calls `f` on each direct child expression of `e`, stopping at the first error.
fn for_each_child<'s, X: Allocator, E>(
    e: &Expr<'s, X>,
    f: &mut impl FnMut(&Expr<'s, X>) -> Result<(), E>,
) -> Result<(), E> {
    #[cfg(test)]
    crate::tests::stack::touch();
    match &e.kind {
        ExprKind::Null
        | ExprKind::Boolean(_)
        | ExprKind::Integer(_)
        | ExprKind::Real(_)
        | ExprKind::Text(_)
        | ExprKind::Blob(_)
        | ExprKind::Column { .. }
        | ExprKind::Param(_) => Ok(()),
        ExprKind::Unary { operand, .. }
        | ExprKind::IsNull { operand, .. }
        | ExprKind::InParam { operand, .. }
        | ExprKind::Cast { operand, .. } => f(operand),
        ExprKind::Chain { first, rest } => {
            f(first)?;
            rest.iter().try_for_each(|link| f(&link.operand))
        }
        ExprKind::Compare { left, right, .. } => {
            f(left)?;
            f(right)
        }
        ExprKind::InList { operand, list, .. } => {
            f(operand)?;
            list.iter().try_for_each(|item| f(item))
        }
        ExprKind::Between { operand, low, high, .. } => {
            f(operand)?;
            f(low)?;
            f(high)
        }
        ExprKind::Like { operand, pattern, .. } => {
            f(operand)?;
            f(pattern)
        }
        ExprKind::Case { operand, arms, otherwise } => {
            if let Some(operand) = operand {
                f(operand)?;
            }
            for arm in arms.iter() {
                f(&arm.when)?;
                f(&arm.then)?;
            }
            match otherwise {
                Some(otherwise) => f(otherwise),
                None => Ok(()),
            }
        }
        ExprKind::Call { args: CallArgs::Star, .. } => Ok(()),
        ExprKind::Call { args: CallArgs::List { args, .. }, .. } => {
            args.iter().try_for_each(|arg| f(arg))
        }
    }
}
