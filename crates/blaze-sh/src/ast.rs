//! Abstract syntax tree for `blaze-sh` (`bstd::sh`).

use blaze_own::{ARc, AString, AVec};

use crate::span::Span;

/// A sequence of statements separated by `;` or newlines.
#[derive(Clone, Debug)]
pub struct Block<'a> {
    pub stmts: AVec<'a, Stmt<'a>>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum Stmt<'a> {
    /// `let x = expr` or `mut x = expr`.
    Let {
        name: ARc<'a, str>,
        mutable: bool,
        ty: Option<TypeAnn>,
        value: Expr<'a>,
        span: Span,
    },
    /// `$x = expr`, `$x += expr`, `$x.a.b = expr`.
    Assign {
        target: AssignTarget<'a>,
        op: AssignOp,
        value: Expr<'a>,
        span: Span,
    },
    /// `def name [params...] { body }`.
    Def {
        name: ARc<'a, str>,
        params: AVec<'a, ParamDef<'a>>,
        body: ARc<'a, Block<'a>>,
        span: Span,
    },
    /// `alias name = replacement_text`.
    Alias {
        name: ARc<'a, str>,
        expansion: ARc<'a, str>,
        span: Span,
    },
    /// `for x in iter { body }`.
    For {
        var: ARc<'a, str>,
        iter: Expr<'a>,
        body: Block<'a>,
        span: Span,
    },
    /// `while cond { body }`.
    While {
        cond: Expr<'a>,
        body: Block<'a>,
        span: Span,
    },
    /// `loop { body }`.
    Loop {
        body: Block<'a>,
        span: Span,
    },
    /// `break`, `continue`, `return [expr]`.
    Break(Span),
    Continue(Span),
    Return {
        value: Option<Expr<'a>>,
        span: Span,
    },
    /// Expression or pipeline evaluated for its value.
    Expr(Expr<'a>),
}

#[derive(Clone, Debug)]
pub struct AssignTarget<'a> {
    pub var: ARc<'a, str>,
    pub path: AVec<'a, PathStep<'a>>,
    pub span: Span,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AssignOp {
    Set,
    AddSet,
    SubSet,
    MulSet,
    DivSet,
    AppendSet,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TypeAnn {
    Any,
    Nothing,
    Bool,
    Int,
    Float,
    Number,
    String,
    Size,
    Duration,
    Date,
    Range,
    Binary,
    List,
    Record,
    Table,
    Closure,
}

impl TypeAnn {
    pub fn from_name(name: &str) -> Option<TypeAnn> {
        match name {
            "any" => Some(TypeAnn::Any),
            "nothing" => Some(TypeAnn::Nothing),
            "bool" => Some(TypeAnn::Bool),
            "int" => Some(TypeAnn::Int),
            "float" => Some(TypeAnn::Float),
            "number" => Some(TypeAnn::Number),
            "string" => Some(TypeAnn::String),
            "filesize" | "size" => Some(TypeAnn::Size),
            "duration" => Some(TypeAnn::Duration),
            "datetime" | "date" => Some(TypeAnn::Date),
            "range" => Some(TypeAnn::Range),
            "binary" => Some(TypeAnn::Binary),
            "list" => Some(TypeAnn::List),
            "record" => Some(TypeAnn::Record),
            "table" => Some(TypeAnn::Table),
            "closure" => Some(TypeAnn::Closure),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ParamDef<'a> {
    pub name: ARc<'a, str>,
    pub short: Option<char>,
    pub kind: ParamKind,
    pub ty: Option<TypeAnn>,
    pub default: Option<Expr<'a>>,
    pub span: Span,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParamKind {
    Positional,
    OptionalPositional,
    Rest,
    Flag,
    Named,
}

#[derive(Clone, Debug)]
pub struct Expr<'a> {
    pub kind: ExprKind<'a>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum ExprKind<'a> {
    Nothing,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(ARc<'a, str>),
    Size(i64),
    Duration(i64),
    Interpolated(AVec<'a, InterpPart<'a>>),
    List(AVec<'a, Expr<'a>>),
    Record(AVec<'a, (ARc<'a, str>, Expr<'a>)>),
    Table {
        headers: AVec<'a, ARc<'a, str>>,
        rows: AVec<'a, AVec<'a, Expr<'a>>>,
    },
    Closure {
        params: AVec<'a, ARc<'a, str>>,
        body: ARc<'a, Block<'a>>,
    },
    /// `$x` or `$in` or `$env` or `$it`.
    Var(ARc<'a, str>),
    /// Leading `.` implicit subject (`.`, `.package.version`, `.0.name`):
    /// resolves to the active row (`row_ctx`), piped input (`$in` when non-Nothing),
    /// or previous command output (`$_`).
    DotSubject,
    /// Cell-path projection: `base.field.0?.sub`.
    CellPath {
        base: blaze_own::ABox<'a, Expr<'a>>,
        steps: AVec<'a, PathStep<'a>>,
    },
    /// `base[]`: flatten one level and skip nulls (`Value::Nothing`).
    Flatten {
        base: blaze_own::ABox<'a, Expr<'a>>,
    },
    /// `base[idx]` / `base[-1]` / `base[a..b]` / `base["key"]`: index, negative index, or slice.
    IndexSlice {
        base: blaze_own::ABox<'a, Expr<'a>>,
        index: blaze_own::ABox<'a, Expr<'a>>,
    },
    /// `base[source == null]`: inline row/item predicate filter.
    Filter {
        base: blaze_own::ABox<'a, Expr<'a>>,
        pred: blaze_own::ABox<'a, Expr<'a>>,
    },
    /// `base.{name, ver: .version}`: inline record/table column projection & reshaping.
    Project {
        base: blaze_own::ABox<'a, Expr<'a>>,
        fields: AVec<'a, (ARc<'a, str>, Option<Expr<'a>>)>,
    },
    /// Bare cell-path literal (e.g. `size` inside `where size > 1mb` or `$.a.b`).
    BarePath(AVec<'a, PathStep<'a>>),
    Unary {
        op: UnaryOp,
        expr: blaze_own::ABox<'a, Expr<'a>>,
    },
    Binary {
        op: BinaryOp,
        left: blaze_own::ABox<'a, Expr<'a>>,
        right: blaze_own::ABox<'a, Expr<'a>>,
    },
    Range {
        start: Option<blaze_own::ABox<'a, Expr<'a>>>,
        step: Option<blaze_own::ABox<'a, Expr<'a>>>,
        end: Option<blaze_own::ABox<'a, Expr<'a>>>,
        inclusive: bool,
    },
    Pipeline(AVec<'a, Expr<'a>>),
    Call(CallExpr<'a>),
    If {
        cond: blaze_own::ABox<'a, Expr<'a>>,
        then_block: Block<'a>,
        else_branch: Option<blaze_own::ABox<'a, Expr<'a>>>,
    },
    Match {
        scrutinee: blaze_own::ABox<'a, Expr<'a>>,
        arms: AVec<'a, MatchArm<'a>>,
    },
    Try {
        try_block: Block<'a>,
        catch_var: Option<ARc<'a, str>>,
        catch_block: Option<Block<'a>>,
    },
    Block(Block<'a>),
    /// `rust { ... }` or `:rs ...` live blaze REPL cell.
    RustCell {
        code: AString<'a>,
    },
}

#[derive(Clone, Debug)]
pub enum InterpPart<'a> {
    Literal(ARc<'a, str>),
    Expr(Expr<'a>),
}

#[derive(Clone, Debug)]
pub struct PathStep<'a> {
    pub key: PathKey<'a>,
    pub optional: bool,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum PathKey<'a> {
    Field(ARc<'a, str>),
    Index(usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    Pow,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    RegexMatch,
    RegexNotMatch,
    In,
    NotIn,
    StartsWith,
    EndsWith,
    Append,
    And,
    Or,
    Xor,
}

#[derive(Clone, Debug)]
pub struct CallExpr<'a> {
    pub head: ARc<'a, str>,
    pub head_span: Span,
    /// `^cmd` forces external program dispatch even if a builtin or `def` shares the name.
    pub forced_external: bool,
    pub args: AVec<'a, CallArg<'a>>,
}

#[derive(Clone, Debug)]
pub enum CallArg<'a> {
    Positional(Expr<'a>),
    Flag {
        name: ARc<'a, str>,
        short: bool,
        value: Option<Expr<'a>>,
        span: Span,
    },
}

#[derive(Clone, Debug)]
pub struct MatchArm<'a> {
    pub pattern: Pattern<'a>,
    pub guard: Option<Expr<'a>>,
    pub body: Expr<'a>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum Pattern<'a> {
    Wildcard,
    Binding(ARc<'a, str>),
    Literal(Expr<'a>),
    Range {
        start: i64,
        end: i64,
        inclusive: bool,
    },
    Or(AVec<'a, Pattern<'a>>),
}
